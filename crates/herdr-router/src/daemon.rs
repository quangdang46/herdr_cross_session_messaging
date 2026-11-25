//! Daemon lifecycle and main loop.
//!
//! ## Architecture (sync, no tokio)
//!
//! [`Daemon::run`] / [`Daemon::spawn`] start two threads:
//!
//! * **event thread** — owns the single [`RegistrySync`] (single writer to
//!   the registries). Loop: `session.snapshot` → `events.subscribe` →
//!   dispatch frames → on socket error, reconnect with exponential
//!   backoff and re-snapshot (events are not replayable). The 60s
//!   `agent.list` safety reconcile piggybacks on the subscribe read
//!   timeout (the client sets `SO_RCVTIMEO`, so `next_frame` wakes
//!   periodically even when no events arrive).
//! * **delivery thread** — [`DeliveryWorker`] drains every pool's outbox
//!   on a short poll, woken early by [`Notify`] on status transitions.
//!
//! ## Graceful degradation
//!
//! * No Herdr socket / server down: the event thread idles with
//!   exponential backoff (1s → [`MAX_CONNECT_BACKOFF`]) and the delivery
//!   worker keeps retrying pending mail on its own schedule.
//! * Shutdown: [`Daemon::request_shutdown`] / [`DaemonHandle::shutdown`]
//!   set an [`AtomicBool`] and ring the notify condvar; the stop sentinel
//!   file (`<state>/routerd.shutdown`) written by `herdr-routerd stop` is
//!   also polled, so `stop` works even if the SIGTERM is missed. In-flight
//!   work finishes (bounded by the subscribe read timeout and one outbox
//!   sweep) before the threads exit.
//!
//! ## Single instance
//!
//! [`InstanceGuard`] takes `<state>/routerd.lock/` via atomic
//! `create_dir`; the pid is recorded inside. A stale lock (dead pid) is
//! reclaimed. The pid is mirrored at `<state>/routerd.pid` for
//! `status`/`stop`. Pure-std signal handling is not possible, so `stop`
//! writes the shutdown sentinel *and* best-effort SIGTERMs the pid; the
//! sentinel is the authoritative graceful path and stale locks are
//! recovered on next `start`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use herdr_plugin::paths::state_dir as plugin_state_dir;
use herdr_protocol::{HerdrClient, HerdrError, SubscriptionFrame};

use crate::delivery::{DeliveryWorker, DEFAULT_POLL_INTERVAL};
use crate::error::{Result, RouterError};
use crate::events::subscriptions_for;
use crate::pools::Pools;
use crate::registry_sync::{Notify, RegistrySync};

/// Safety-reconcile period (`agent.list` diff) — bead spec: 60s.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
/// First reconnect delay after a subscribe/transport failure.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Cap for reconnect backoff.
pub const MAX_CONNECT_BACKOFF: Duration = Duration::from_secs(30);
/// How often the event thread wakes to check shutdown/reconcile when the
/// subscribe stream is silent. Also the `SO_RCVTIMEO` on the subscribe
/// connection — keep it small enough for responsive `stop`.
pub const EVENT_WAKEUP: Duration = Duration::from_secs(5);
/// pidfile name under the state dir.
pub const PID_FILE_NAME: &str = "routerd.pid";
/// instance-lock directory name under the state dir.
pub const LOCK_DIR_NAME: &str = "routerd.lock";
/// shutdown sentinel written by `herdr-routerd stop`.
pub const SHUTDOWN_FILE_NAME: &str = "routerd.shutdown";
/// daemon log file name (under `<state>/logs/`).
pub const LOG_FILE_NAME: &str = "routerd.log";

/// Daemon configuration. Tests construct explicit values; production uses
/// [`DaemonConfig::from_env`].
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Plugin/pool state dir (pidfile, lock, pools, index).
    pub state_dir: PathBuf,
    /// Herdr socket path override; `None` → `HerdrClient::new()` env
    /// resolution (`HERDR_SOCKET_PATH` etc.).
    pub socket_path: Option<PathBuf>,
    /// `agent.list` reconcile period.
    pub reconcile_interval: Duration,
    /// Outbox poll period for the delivery worker.
    pub delivery_poll: Duration,
    /// Subscribe read timeout / event-loop wakeup period.
    pub event_wakeup: Duration,
}

impl DaemonConfig {
    /// Config rooted at an explicit state dir (socket from env).
    pub fn for_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            socket_path: None,
            reconcile_interval: RECONCILE_INTERVAL,
            delivery_poll: DEFAULT_POLL_INTERVAL,
            event_wakeup: EVENT_WAKEUP,
        }
    }

    /// Config from the process environment (`HERDR_PLUGIN_STATE_DIR` →
    /// `HERDR_POOL_STATE_DIR` → XDG → home), socket from env.
    pub fn from_env() -> Result<Self> {
        Ok(Self::for_state_dir(plugin_state_dir()?))
    }
}

/// Handle to a spawned [`Daemon`]: request shutdown, then [`Self::join`].
pub struct DaemonHandle {
    shutdown: Arc<AtomicBool>,
    notify: Notify,
    state_dir: PathBuf,
    event: std::thread::JoinHandle<()>,
    delivery: std::thread::JoinHandle<()>,
}

impl DaemonHandle {
    /// Ask the daemon to shut down gracefully (flag + notify + sentinel).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify();
        let _ = write_stop_sentinel(&self.state_dir);
    }

    /// Whether shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Block until both daemon threads exit (after [`Self::shutdown`]).
    pub fn join(self) {
        let _ = self.event.join();
        let _ = self.delivery.join();
    }
}

/// The router daemon.
///
/// Construct via [`Daemon::new`], then either [`Daemon::spawn`] (returns a
/// [`DaemonHandle`]; requires `Arc<Daemon>`) or [`Daemon::run`] (blocking,
/// scoped threads — used by `herdr-routerd run`).
pub struct Daemon {
    config: DaemonConfig,
    client: HerdrClient,
    /// Subscribe-stream client: same socket, bounded read timeout so the
    /// event loop wakes periodically for shutdown/reconcile checks.
    event_client: HerdrClient,
    pools: Arc<Pools>,
    shutdown: Arc<AtomicBool>,
    notify: Notify,
}

impl Daemon {
    /// Build a daemon: resolve the socket path, open `Pools`.
    pub fn new(config: DaemonConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.state_dir)?;
        let pools = Arc::new(Pools::open(&config.state_dir)?);
        let client = match &config.socket_path {
            Some(path) => HerdrClient::from_socket_path(path),
            None => HerdrClient::new()?,
        };
        let event_client = client
            .clone()
            .with_request_timeout(config.event_wakeup);
        Ok(Self {
            config,
            client,
            event_client,
            pools,
            shutdown: Arc::new(AtomicBool::new(false)),
            notify: Notify::default(),
        })
    }

    /// Spawn the event + delivery threads on an `Arc<Daemon>`; returns a
    /// handle. Threads are `'static` — the `Arc` keeps the daemon alive.
    pub fn spawn(self: &Arc<Self>) -> DaemonHandle {
        let event_daemon = Arc::clone(self);
        let event = std::thread::Builder::new()
            .name("herdr-routerd-events".into())
            .spawn(move || event_daemon.event_loop())
            .expect("spawn event thread");
        let delivery_daemon = Arc::clone(self);
        let delivery = std::thread::Builder::new()
            .name("herdr-routerd-delivery".into())
            .spawn(move || delivery_daemon.delivery_loop())
            .expect("spawn delivery thread");
        DaemonHandle {
            shutdown: Arc::clone(&self.shutdown),
            notify: self.notify.clone(),
            state_dir: self.config.state_dir.clone(),
            event,
            delivery,
        }
    }

    /// Blocking run: spawn scoped threads, return when both exit.
    /// Used by `herdr-routerd run` (foreground/debug).
    pub fn run(&self) -> Result<()> {
        std::thread::scope(|scope| {
            scope.spawn(|| self.event_loop());
            scope.spawn(|| self.delivery_loop());
        });
        Ok(())
    }

    /// Ask this daemon instance to stop (same as `DaemonHandle::shutdown`).
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify();
        let _ = write_stop_sentinel(&self.config.state_dir);
    }

    /// Whether shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// The shared pool registry (for tests / diagnostics).
    pub fn pools(&self) -> &Pools {
        &self.pools
    }

    /// The resolved Herdr client.
    pub fn client(&self) -> &HerdrClient {
        &self.client
    }

    // ------------------------------------------------------------------
    // Threads
    // ------------------------------------------------------------------

    /// Event-loop thread: snapshot → subscribe → dispatch → reconnect.
    fn event_loop(&self) {
        let mut sync = RegistrySync::new(&self.event_client, self.pools.as_ref());
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if self.should_stop() {
                tracing::info!("event loop: shutdown requested; exiting");
                return;
            }
            match self.event_session(&mut sync) {
                Ok(()) => {
                    // Clean stream end or resubscribe cycle completed —
                    // loop back and (re)subscribe.
                    tracing::debug!("event loop: session ended; continuing");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "event loop: session failed; backing off");
                }
            }
            if self.should_stop() {
                return;
            }
            // Interruptible backoff: the condvar wakes on shutdown/notify.
            let pair = self.notify.pair();
            let (lock, cvar) = &*pair;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cvar.wait_timeout(guard, backoff);
            drop(guard);
            backoff = (backoff * 2).min(MAX_CONNECT_BACKOFF);
        }
    }

    /// One subscribe session: snapshot sync → subscribe → frame loop.
    ///
    /// `Ok(())` means "re-subscribe immediately" (watched-pane set changed
    /// or shutdown requested); `Err` means transport/protocol failure —
    /// the caller backs off, then calls again, which re-snapshots
    /// (Resync → re-snapshot, since events are not replayable).
    fn event_session(&self, sync: &mut RegistrySync<'_>) -> Result<()> {
        // Bootstrap/resync: refresh the whole registry from the snapshot
        // before (re)subscribing so no events are missed.
        let synced = sync.sync_snapshot()?;
        tracing::info!(agents = synced, "event loop: snapshot synced");

        let mut last_reconcile = Instant::now();
        loop {
            if self.should_stop() {
                return Ok(());
            }
            let subs = subscriptions_for(sync.watched_panes());
            let mut stream = self.event_client.events_subscribe(subs)?;
            sync.acknowledge_subscriptions();
            tracing::debug!(count = subs.len(), "event loop: subscribed");

            loop {
                if self.should_stop() {
                    return Ok(());
                }
                match stream.next_frame() {
                    Ok(frame) => {
                        self.dispatch_frame(sync, &frame);
                        if sync.subscriptions_changed() {
                            // Pane set changed: drop this stream and
                            // re-issue events.subscribe with the new set.
                            tracing::debug!("event loop: watched panes changed; resubscribing");
                            return Ok(());
                        }
                    }
                    Err(HerdrError::Transport(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        // Read-timeout wakeup: reconcile timer + stop check.
                        if self.should_stop() {
                            return Ok(());
                        }
                        if last_reconcile.elapsed() >= self.config.reconcile_interval {
                            match sync.reconcile() {
                                Ok(n) => tracing::debug!(agents = n, "reconcile pass complete"),
                                Err(err) => tracing::warn!(error = %err, "reconcile failed"),
                            }
                            last_reconcile = Instant::now();
                        }
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }
    }

    /// Route one frame through `RegistrySync` and nudge the delivery
    /// worker when warranted.
    fn dispatch_frame(&self, sync: &mut RegistrySync<'_>, frame: &SubscriptionFrame) {
        let kind = frame_kind(frame);
        let span = tracing::debug_span!("dispatch", frame = kind);
        let _enter = span.enter();
        if let Err(err) = sync.handle_frame(frame) {
            tracing::warn!(error = %err, "frame dispatch failed");
        }
        if sync.take_delivery_nudge() {
            self.notify.notify();
        }
    }

    /// Delivery-worker thread: outbox sweeps until shutdown.
    fn delivery_loop(&self) {
        let worker = DeliveryWorker::new(
            self.client.clone(),
            Arc::clone(&self.pools),
            Arc::clone(&self.shutdown),
            self.notify.clone(),
            self.config.delivery_poll,
        );
        let stop_file = self.config.state_dir.join(SHUTDOWN_FILE_NAME);
        worker.run(Some(&stop_file));
    }

    /// Shutdown check: flag or stop sentinel file. (A bare SIGTERM without
    /// the sentinel kills the process; the next `start` reclaims the stale
    /// lock — documented trade-off of dependency-free signal handling.)
    fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst) || stop_requested(&self.config.state_dir)
    }
}

fn frame_kind(frame: &SubscriptionFrame) -> &'static str {
    match frame {
        SubscriptionFrame::Event(env) => env.event.dot_name().unwrap_or("unknown"),
        SubscriptionFrame::SubscriptionEvent(env) => match env.event {
            herdr_protocol::SubscriptionEventKind::PaneOutputMatched => "pane.output_matched",
            herdr_protocol::SubscriptionEventKind::PaneAgentStatusChanged => {
                "pane.agent_status_changed"
            }
            herdr_protocol::SubscriptionEventKind::ScrollChanged => "pane.scroll_changed",
            herdr_protocol::SubscriptionEventKind::Unknown => "subscription.unknown",
        },
    }
}

// ---------------------------------------------------------------------------
// Single-instance + pidfile lifecycle helpers (used by herdr-routerd)
// ---------------------------------------------------------------------------

/// Observed daemon state for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    /// No live daemon (no pidfile, stale pidfile, or dead pid).
    Stopped,
    /// A live daemon process holds the instance lock.
    Running { pid: u32 },
}

/// RAII guard for `<state>/routerd.lock/`. Removes the directory on drop
/// only if it still contains our pid (never steals another daemon's lock).
pub struct InstanceGuard {
    lock_dir: PathBuf,
    pid: u32,
}

impl InstanceGuard {
    /// Try to become the single daemon instance.
    ///
    /// * No lock → create it, write our pid.
    /// * Lock held by a live pid → [`RouterError::Daemon`] "already running".
    /// * Lock held by a dead pid → reclaim (remove + retry once).
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        let lock_dir = state_dir.join(LOCK_DIR_NAME);
        let pid = std::process::id();
        for attempt in 0..2 {
            match std::fs::create_dir(&lock_dir) {
                Ok(()) => {
                    std::fs::write(lock_dir.join("pid"), pid.to_string())?;
                    return Ok(Self {
                        lock_dir,
                        pid,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    match read_lock_pid(&lock_dir) {
                        Some(holder_pid) if pid_alive(holder_pid) => {
                            return Err(RouterError::Daemon(format!(
                                "herdr-routerd already running (pid {holder_pid})"
                            )));
                        }
                        _ => {
                            // Stale or unreadable lock: reclaim once.
                            if attempt == 0 {
                                let _ = std::fs::remove_dir_all(&lock_dir);
                                continue;
                            }
                            return Err(RouterError::Daemon(format!(
                                "cannot acquire instance lock {}",
                                lock_dir.display()
                            )));
                        }
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Only remove if the lock still belongs to us.
        if read_lock_pid(&self.lock_dir) == Some(self.pid) {
            let _ = std::fs::remove_dir_all(&self.lock_dir);
        }
    }
}

fn read_lock_pid(lock_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Write `<state>/routerd.pid` containing `pid` and start unix-time.
pub fn write_pid_file(state_dir: &Path, pid: u32) -> Result<()> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    std::fs::write(state_dir.join(PID_FILE_NAME), format!("{pid}\n{secs}\n"))?;
    Ok(())
}

/// Read `(pid, started_unix_secs)` from the pidfile.
pub fn read_pid_file(state_dir: &Path) -> Option<(u32, Option<u64>)> {
    let text = std::fs::read_to_string(state_dir.join(PID_FILE_NAME)).ok()?;
    let mut lines = text.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let started = lines.next().and_then(|l| l.trim().parse().ok());
    Some((pid, started))
}

/// Remove the pidfile (best effort).
pub fn remove_pid_file(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join(PID_FILE_NAME));
}

/// Write the shutdown sentinel polled by daemon threads.
pub fn write_stop_sentinel(state_dir: &Path) -> Result<()> {
    std::fs::write(state_dir.join(SHUTDOWN_FILE_NAME), b"stop\n")?;
    Ok(())
}

/// Remove the shutdown sentinel (called on daemon start and clean exit).
pub fn clear_stop_sentinel(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join(SHUTDOWN_FILE_NAME));
}

/// Whether the stop sentinel exists.
pub fn stop_requested(state_dir: &Path) -> bool {
    state_dir.join(SHUTDOWN_FILE_NAME).exists()
}

/// Inspect daemon liveness: pidfile pid alive AND lock held by same pid.
pub fn daemon_status(state_dir: &Path) -> DaemonStatus {
    let Some((pid, _started)) = read_pid_file(state_dir) else {
        return DaemonStatus::Stopped;
    };
    if pid_alive(pid) && read_lock_pid(&state_dir.join(LOCK_DIR_NAME)) == Some(pid) {
        DaemonStatus::Running { pid }
    } else {
        DaemonStatus::Stopped
    }
}

/// Best-effort SIGTERM to `pid` (unix only; no-op elsewhere).
pub fn sigterm(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Liveness probe: `kill -0` on unix; on other platforms reports dead
/// (documented MVP limitation — the daemon targets unix sockets anyway).
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}
