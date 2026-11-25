//! Fake Herdr server test harness.
//!
//! `FakeHerdr` binds a `UnixListener` on `<tempdir>/herdr.sock` and speaks the
//! Herdr NDJSON socket protocol: one `{"id","method","params"}` request per
//! line, answered with `{"id","result":{...}}` or `{"id","error":{...}}`.
//!
//! Point a [`herdr_protocol::HerdrClient`] at it either explicitly via
//! [`FakeHerdr::socket_path`] or through the `HERDR_SOCKET_PATH` env override:
//!
//! ```no_run
//! # fn example() -> herdr_testkit::Result<()> {
//! let server = herdr_testkit::FakeHerdr::start()?;
//! std::env::set_var("HERDR_SOCKET_PATH", server.socket_path());
//! let client = herdr_protocol::HerdrClient::new().unwrap();
//! let pong = client.ping().unwrap();
//! assert_eq!(pong.version, "0.9.5");
//! # Ok(())
//! # }
//! ```
//!
//! Script responses per method with [`FakeHerdr::on`] / [`FakeHerdr::fail_next`],
//! inspect what the client sent with [`FakeHerdr::requests`] /
//! [`FakeHerdr::requests_for`], and drive long-lived calls
//! (`events.subscribe`, `events.wait`, `agent.wait`, `agent.prompt` with `wait`,
//! `pane.wait_for_output`) with [`FakeHerdr::push_event`],
//! [`FakeHerdr::push_subscription_event`], [`FakeHerdr::push_response`] and
//! [`FakeHerdr::push_error`].
//!
// Lint policy: no `unwrap()`/`expect()` in library code; mutex locking goes
// through [`lock`], which tolerates poisoning. Inline `#[cfg(test)]` modules
// are exempted per-module below.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod canned;

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use canned::canned_result;

const SOCKET_FILE_NAME: &str = "herdr.sock";

/// Lock a mutex, tolerating poisoning.
///
/// A panicked holder (e.g. a responder closure that panicked mid-request)
/// leaves the shared state inspectable; for a test harness, recovering the
/// guard and continuing is more useful than propagating the panic into
/// unrelated assertions.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Errors returned by the fake server harness itself (not API errors, which
/// are delivered to the client as `{"error": ...}` frames).
#[derive(Debug, thiserror::Error)]
pub enum FakeHerdrError {
    /// Filesystem/socket error while binding or serving.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The fake server transport is only implemented for unix sockets.
    #[error("herdr-testkit fake server is only supported on unix platforms")]
    Unsupported,
}

/// `Result` alias for harness operations.
pub type Result<T> = std::result::Result<T, FakeHerdrError>;

/// One request line received by the fake server.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The request `id` as a string (what `push_response`/`push_error` match on).
    pub id: String,
    /// The method name, e.g. `"agent.prompt"`.
    pub method: String,
    /// The decoded `params` object.
    pub params: Value,
    /// The raw NDJSON line as received.
    pub raw: String,
}

/// Context handed to closure responders registered via [`MethodStub::then`].
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The request `id` as a string.
    pub id: String,
    /// The method name.
    pub method: String,
    /// The decoded `params` object.
    pub params: Value,
}

/// What a closure responder (or scripted responder) should do with a request.
#[derive(Debug)]
pub enum HandlerReply {
    /// Send `{"id","result": <value>}` and close the connection.
    Result(Value),
    /// Send `{"id","error":{"code","message"}}` and close the connection.
    Error { code: String, message: String },
    /// Keep the connection open with no response; resolve later via
    /// [`FakeHerdr::push_response`] / [`FakeHerdr::push_error`] using the
    /// request id. Used for `agent.wait`-style calls.
    Hold,
    /// Send `{"id","result": <value>}` (e.g. a `subscription_started` ack) and
    /// keep the connection open for pushed frames. Used for `events.subscribe`.
    RespondAndHold(Value),
}

/// A scripted responder queued for a method. Consumed FIFO; when the queue is
/// empty the canned default for the method applies.
#[derive(Clone)]
enum Responder {
    Result(Value),
    Error { code: String, message: String },
    Hold,
    RespondAndHold(Value),
    Closure(Arc<dyn Fn(&RequestContext) -> HandlerReply + Send + Sync>),
}

/// What the server does after a request is dispatched.
enum Outcome {
    Respond(Value),
    Fail { code: String, message: String },
    Hold,
    RespondAndHold(Value),
}

impl From<HandlerReply> for Outcome {
    fn from(reply: HandlerReply) -> Self {
        match reply {
            HandlerReply::Result(v) => Outcome::Respond(v),
            HandlerReply::Error { code, message } => Outcome::Fail { code, message },
            HandlerReply::Hold => Outcome::Hold,
            HandlerReply::RespondAndHold(v) => Outcome::RespondAndHold(v),
        }
    }
}

/// Kind of an open (held) connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnKind {
    /// Waiting for `push_response`/`push_error` (`agent.wait`, `events.wait`,
    /// `agent.prompt` with `wait`, `pane.wait_for_output`, `HandlerReply::Hold`).
    Waiting,
    /// Subscription stream opened by `events.subscribe`
    /// (`HandlerReply::RespondAndHold`).
    Subscription,
}

/// A live server-side connection that outlived its request dispatch.
struct ConnHandle {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Closes the underlying socket (both directions).
    shutdown: Box<dyn Fn() + Send>,
    /// For `ConnKind::Waiting`: the pending request id to echo in the final
    /// response.
    pending_request_id: Option<Value>,
    kind: ConnKind,
}

#[derive(Default)]
struct State {
    /// Scripted responders per method, consumed FIFO per request.
    responders: HashMap<String, VecDeque<Responder>>,
    /// Every request line received, in arrival order.
    requests: Vec<RecordedRequest>,
}

struct Inner {
    state: Mutex<State>,
    conns: Mutex<HashMap<u64, ConnHandle>>,
    next_conn: AtomicU64,
    shutdown_flag: AtomicBool,
    /// Keeps the socket directory alive for the server's lifetime.
    _tempdir: tempfile::TempDir,
    socket_path: PathBuf,
    #[cfg(unix)]
    accept_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Normalize a JSON request id to the string form used for matching and for
/// [`RecordedRequest::id`].
fn id_to_string(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn id_matches(id: &Value, request_id: &str) -> bool {
    id_to_string(id) == request_id
}

/// Write one NDJSON frame (JSON + `\n`) to a connection.
fn send_json(writer: &Arc<Mutex<Box<dyn Write + Send>>>, value: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    let mut w = lock(&**writer);
    w.write_all(&buf)?;
    w.flush()
}

fn send_response(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    id: &Value,
    result: Value,
) -> std::io::Result<()> {
    send_json(writer, &json!({ "id": id, "result": result }))
}

fn send_error(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    id: &Value,
    code: &str,
    message: &str,
) -> std::io::Result<()> {
    send_json(
        writer,
        &json!({ "id": id, "error": { "code": code, "message": message } }),
    )
}

impl Inner {
    fn push_frame(&self, frame: &Value, subscriptions_only: bool) {
        let conns = lock(&self.conns);
        for handle in conns.values() {
            if subscriptions_only && handle.kind != ConnKind::Subscription {
                continue;
            }
            // Best-effort: a dead connection is reaped by close_connection /
            // shutdown; individual write failures are ignored.
            let _ = send_json(&handle.writer, frame);
        }
    }

    /// Resolve a held (`Waiting`) connection by request id, send it the final
    /// envelope (`{"result": ...}` or `{"error": ...}` merged under `id`), and
    /// close the connection — mirroring the real server ending a wait.
    fn push_envelope(&self, request_id: &str, body: Value) {
        let handle = {
            let mut conns = lock(&self.conns);
            let key = conns.iter().find_map(|(k, h)| {
                h.pending_request_id
                    .as_ref()
                    .filter(|id| id_matches(id, request_id))
                    .map(|_| *k)
            });
            key.and_then(|k| conns.remove(&k))
        };
        if let Some(handle) = handle {
            if let Some(id) = &handle.pending_request_id {
                let mut envelope = json!({ "id": id });
                if let (Some(obj), Value::Object(fields)) = (envelope.as_object_mut(), &body) {
                    for (k, v) in fields {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                let _ = send_json(&handle.writer, &envelope);
            }
            (handle.shutdown)();
        }
    }

    fn close_connections(&self) {
        let handles: Vec<ConnHandle> = lock(&self.conns).drain().map(|(_, h)| h).collect();
        for handle in handles {
            (handle.shutdown)();
        }
    }

    fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.close_connections();
        #[cfg(unix)]
        {
            // Nudge the accept loop so `accept()` returns and observes the
            // shutdown flag, then wait for the thread to exit.
            let _ = std::os::unix::net::UnixStream::connect(&self.socket_path);
            if let Some(handle) = lock(&self.accept_thread).take() {
                let _ = handle.join();
            }
        }
    }
}

/// Scriptable fake Herdr server. See the crate-level docs for an overview.
pub struct FakeHerdr {
    inner: Arc<Inner>,
}

impl FakeHerdr {
    /// Start a fake server on `<tempdir>/herdr.sock`.
    ///
    /// The tempdir (and socket) live until the `FakeHerdr` is dropped or
    /// [`FakeHerdr::shutdown`] is called.
    pub fn start() -> Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let socket_path = tempdir.path().join(SOCKET_FILE_NAME);

        #[cfg(unix)]
        {
            let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
            let inner = Arc::new(Inner {
                state: Mutex::new(State::default()),
                conns: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(1),
                shutdown_flag: AtomicBool::new(false),
                _tempdir: tempdir,
                socket_path,
                accept_thread: Mutex::new(None),
            });
            let accept_inner = inner.clone();
            let accept_thread = std::thread::spawn(move || accept_loop(accept_inner, listener));
            *lock(&inner.accept_thread) = Some(accept_thread);
            Ok(Self { inner })
        }

        #[cfg(not(unix))]
        {
            let _ = (tempdir, socket_path);
            Err(FakeHerdrError::Unsupported)
        }
    }

    /// Path to the unix socket the fake server listens on.
    ///
    /// Use with `HerdrClient::from_socket_path`, or export it as
    /// `HERDR_SOCKET_PATH` for code that resolves the socket from the
    /// environment.
    pub fn socket_path(&self) -> &Path {
        &self.inner.socket_path
    }

    // ------------------------------------------------------------------
    // Scripting
    // ------------------------------------------------------------------

    /// Queue a scripted responder for `method`; each incoming request consumes
    /// one queued responder, falling back to the canned default when empty.
    ///
    /// Chainable: `server.on("agent.prompt").respond(json!({...}))`,
    /// `.error("agent_blocked", "...")`, `.hold()`, `.respond_and_hold(v)`,
    /// `.then(|req| ...)`.
    pub fn on(&self, method: &str) -> MethodStub {
        MethodStub {
            inner: self.inner.clone(),
            method: method.to_string(),
        }
    }

    /// Make the *next* request to `method` fail with an API error.
    ///
    /// `code` should be a Herdr API error code such as `agent_blocked`,
    /// `agent_not_ready`, `agent_not_found`, `agent_target_ambiguous`,
    /// `timeout`, or `invalid_params`.
    pub fn fail_next(&self, method: &str, code: &str, message: &str) {
        let mut state = lock(&self.inner.state);
        state
            .responders
            .entry(method.to_string())
            .or_default()
            .push_front(Responder::Error {
                code: code.to_string(),
                message: message.to_string(),
            });
    }

    // ------------------------------------------------------------------
    // Request recording
    // ------------------------------------------------------------------

    /// All requests received so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.inner.state).requests.clone()
    }

    /// Requests received for `method` only, in arrival order.
    pub fn requests_for(&self, method: &str) -> Vec<RecordedRequest> {
        lock(&self.inner.state)
            .requests
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    /// First recorded request for `method`, if any.
    pub fn last_request_for(&self, method: &str) -> Option<RecordedRequest> {
        self.requests_for(method).into_iter().next()
    }

    /// Wait until a request for `method` has been recorded (or `timeout`
    /// elapses). Useful between spawning a blocking client call and pushing a
    /// response for it.
    pub fn wait_for_request(&self, method: &str, timeout: Duration) -> Option<RecordedRequest> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(req) = self.last_request_for(method) {
                return Some(req);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Clear recorded requests and all scripted responders, returning the
    /// server to canned defaults.
    pub fn clear(&self) {
        let mut state = lock(&self.inner.state);
        state.requests.clear();
        state.responders.clear();
    }

    /// Clear only the recorded request log (scripted responders are kept).
    pub fn clear_requests(&self) {
        lock(&self.inner.state).requests.clear();
    }

    // ------------------------------------------------------------------
    // Pushing to open connections
    // ------------------------------------------------------------------

    /// Number of currently open held connections (subscription streams and
    /// pending waits).
    pub fn open_connections(&self) -> usize {
        lock(&self.inner.conns).len()
    }

    /// Wait until at least `min` connections are held open (or `timeout`).
    pub fn wait_for_open_connections(&self, min: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.open_connections() >= min {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Broadcast a raw frame (`{"event": ..., "data": ...}` or an error
    /// object) to every open held connection.
    ///
    /// For subscription streams prefer [`FakeHerdr::push_subscription_event`],
    /// which only targets `events.subscribe` connections.
    pub fn push_event(&self, frame: Value) {
        self.inner.push_frame(&frame, false);
    }

    /// Broadcast a subscription frame to all open `events.subscribe`
    /// connections only.
    pub fn push_subscription_event(&self, frame: Value) {
        self.inner.push_frame(&frame, true);
    }

    /// Send the final `{"id","result": <result>}` for a held request and close
    /// its connection. `request_id` is [`RecordedRequest::id`] of the pending
    /// call (e.g. from [`FakeHerdr::wait_for_request`]).
    pub fn push_response(&self, request_id: &str, result: Value) {
        self.inner
            .push_envelope(request_id, json!({ "result": result }));
    }

    /// Send the final `{"id","error":{"code","message"}}` for a held request
    /// and close its connection.
    pub fn push_error(&self, request_id: &str, code: &str, message: &str) {
        self.inner.push_envelope(
            request_id,
            json!({ "error": { "code": code, "message": message } }),
        );
    }

    /// Close all held connections. Pending client calls surface
    /// `HerdrError::Closed`.
    pub fn close_connection(&self) {
        self.inner.close_connections();
    }

    /// Stop the accept loop and close all connections. Idempotent; also runs
    /// on drop.
    pub fn shutdown(self) {
        self.inner.shutdown();
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

/// Builder returned by [`FakeHerdr::on`]. Each method queues one responder.
#[derive(Clone)]
pub struct MethodStub {
    inner: Arc<Inner>,
    method: String,
}

impl MethodStub {
    fn queue(&self, responder: Responder) -> Self {
        lock(&self.inner.state)
            .responders
            .entry(self.method.clone())
            .or_default()
            .push_back(responder);
        self.clone()
    }

    /// Respond with `{"id","result": <result>}` and close the connection.
    /// `result` is the inner result object, e.g. `json!({"type":"ok"})` or a
    /// payload from [`canned_result`].
    pub fn respond(self, result: impl Into<Value>) -> Self {
        self.queue(Responder::Result(result.into()))
    }

    /// Respond with an API error and close the connection.
    pub fn error(self, code: impl Into<String>, message: impl Into<String>) -> Self {
        self.queue(Responder::Error {
            code: code.into(),
            message: message.into(),
        })
    }

    /// Hold the connection open with no response; resolve later via
    /// [`FakeHerdr::push_response`] / [`FakeHerdr::push_error`].
    pub fn hold(self) -> Self {
        self.queue(Responder::Hold)
    }

    /// Send `result` immediately, then keep the connection open for pushed
    /// frames (the `events.subscribe` pattern).
    pub fn respond_and_hold(self, result: impl Into<Value>) -> Self {
        self.queue(Responder::RespondAndHold(result.into()))
    }

    /// Register a closure responder inspected per request. The closure may
    /// inspect [`RequestContext::params`] and return any [`HandlerReply`].
    pub fn then<F>(self, f: F) -> Self
    where
        F: Fn(&RequestContext) -> HandlerReply + Send + Sync + 'static,
    {
        self.queue(Responder::Closure(Arc::new(f)))
    }
}

// ---------------------------------------------------------------------------
// Unix socket server
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn accept_loop(inner: Arc<Inner>, listener: std::os::unix::net::UnixListener) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if inner.shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                let conn_inner = inner.clone();
                std::thread::spawn(move || handle_connection(conn_inner, stream));
            }
            Err(_) => {
                if inner.shutdown_flag.load(Ordering::SeqCst) {
                    break;
                }
                // Transient accept errors: keep serving.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Serve one connection: read request lines, dispatch responders, and either
/// answer-and-close or hold the connection open for later pushes.
#[cfg(unix)]
fn handle_connection(inner: Arc<Inner>, stream: std::os::unix::net::UnixStream) {
    use std::io::BufRead;
    use std::net::Shutdown;

    let Ok(writer_raw) = stream.try_clone() else {
        return;
    };
    let Ok(shutdown_raw) = writer_raw.try_clone() else {
        return;
    };
    let writer: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(Box::new(writer_raw)));
    let shutdown: Box<dyn Fn() + Send> = Box::new(move || {
        let _ = shutdown_raw.shutdown(Shutdown::Both);
    });

    let mut reader = std::io::BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let _ = send_json(
                    &writer,
                    &json!({
                        "id": Value::Null,
                        "error": { "code": "invalid_request", "message": format!("malformed request: {err}") },
                    }),
                );
                break;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

        lock(&inner.state).requests.push(RecordedRequest {
            id: id_to_string(&id),
            method: method.clone(),
            params: params.clone(),
            raw: trimmed.to_string(),
        });

        // Pop one scripted responder, else fall back to the canned default.
        let responder = {
            let mut state = lock(&inner.state);
            state
                .responders
                .get_mut(&method)
                .and_then(VecDeque::pop_front)
        }
        .unwrap_or_else(|| canned::default_for(&method, &params));

        let outcome = match responder {
            Responder::Result(v) => Outcome::Respond(v),
            Responder::Error { code, message } => Outcome::Fail { code, message },
            Responder::Hold => Outcome::Hold,
            Responder::RespondAndHold(v) => Outcome::RespondAndHold(v),
            Responder::Closure(f) => {
                let ctx = RequestContext {
                    id: id_to_string(&id),
                    method: method.clone(),
                    params: params.clone(),
                };
                f(&ctx).into()
            }
        };

        match outcome {
            Outcome::Respond(result) => {
                let _ = send_response(&writer, &id, result);
                break;
            }
            Outcome::Fail { code, message } => {
                let _ = send_error(&writer, &id, &code, &message);
                break;
            }
            Outcome::Hold => {
                register_conn(&inner, writer, shutdown, ConnKind::Waiting, Some(id));
                return;
            }
            Outcome::RespondAndHold(result) => {
                let _ = send_response(&writer, &id, result);
                register_conn(&inner, writer, shutdown, ConnKind::Subscription, None);
                return;
            }
        }
    }
    // One-shot path: both stream clones drop here, closing the socket.
}

/// Register a held connection so `push_*`/`close_connection` can reach it.
#[cfg(unix)]
fn register_conn(
    inner: &Arc<Inner>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    shutdown: Box<dyn Fn() + Send>,
    kind: ConnKind,
    pending_request_id: Option<Value>,
) {
    let id = inner.next_conn.fetch_add(1, Ordering::SeqCst);
    lock(&inner.conns).insert(
        id,
        ConnHandle {
            writer,
            shutdown,
            pending_request_id,
            kind,
        },
    );
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use herdr_protocol::{AgentStatus, HerdrClient, HerdrError, Subscription, SubscriptionFrame};
    use std::time::Duration;

    fn client_for(server: &FakeHerdr) -> HerdrClient {
        HerdrClient::from_socket_path(server.socket_path())
    }

    #[test]
    fn ping_and_agent_list_roundtrip() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let pong = client.ping().unwrap();
        assert_eq!(pong.version, "0.9.5");

        let agents = client.agent_list().unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name.as_deref(), Some("reviewer"));
        assert_eq!(agents[1].name.as_deref(), Some("builder"));

        let pings = server.requests_for("ping");
        assert_eq!(pings.len(), 1);
        assert!(pings[0].raw.contains("\"method\":\"ping\""));
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn env_var_override_is_honored() {
        let server = FakeHerdr::start().unwrap();
        // SAFETY-free in edition 2021; no other test resolves via env.
        std::env::set_var("HERDR_SOCKET_PATH", server.socket_path());
        let client = HerdrClient::new().unwrap();
        assert_eq!(client.socket_path(), server.socket_path());
        client.ping().unwrap();
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    #[test]
    fn canned_defaults_cover_core_methods() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        assert_eq!(client.session_snapshot().unwrap().panes.len(), 0);
        let agent = client.agent_get("reviewer").unwrap();
        assert_eq!(agent.pane_id, "w1:p1");
        client.agent_send_keys("reviewer", &["enter"]).unwrap();
        let read = client
            .agent_read(&herdr_protocol::AgentReadParams {
                target: "reviewer".to_string(),
                source: herdr_protocol::ReadSource::Recent,
                options: herdr_protocol::ReadOptions::default(),
            })
            .unwrap();
        assert_eq!(read.text, "hello world\n");
        assert_eq!(client.pane_list(None).unwrap().len(), 2);
        assert_eq!(client.pane_current(None).unwrap().pane_id, "w1:p2");
        client.pane_send_text("w1:p1", "ls -la\n").unwrap();
        client.pane_send_keys("w1:p1", &["ctrl+c"]).unwrap();
        assert_eq!(client.workspace_list().unwrap().len(), 2);
        client
            .notification_show(&herdr_protocol::NotificationShowParams {
                title: "hi".to_string(),
                body: None,
                position: None,
                sound: None,
            })
            .unwrap();
        client
            .integration_install(herdr_protocol::IntegrationTarget::Claude)
            .unwrap();
        // agent.prompt without `wait` resolves immediately.
        let prompted = client.agent_prompt("reviewer", "hello", None).unwrap();
        assert_eq!(prompted.name.as_deref(), Some("reviewer"));
    }

    #[test]
    fn scripted_respond_and_closure_handlers() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        server
            .on("agent.prompt")
            .respond(json!({ "type": "agent_prompted", "agent": { "pane_id": "w9:p9" } }));
        server.on("agent.send_keys").then(|req| {
            assert_eq!(req.method, "agent.send_keys");
            HandlerReply::Error {
                code: "agent_blocked".to_string(),
                message: "blocked".to_string(),
            }
        });

        let err = client.agent_send_keys("reviewer", &["enter"]).unwrap_err();
        assert!(matches!(
            err,
            HerdrError::Api { ref code, .. } if code == "agent_blocked"
        ));

        // agent.prompt had a queued canned respond; send_keys consumed its
        // closure once — a second send_keys falls back to the canned `ok`.
        client.agent_send_keys("reviewer", &["enter"]).unwrap();

        let reqs = server.requests_for("agent.send_keys");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].params["keys"][0], "enter");
    }

    #[test]
    fn fail_next_injects_api_error() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        server.fail_next("ping", "timeout", "timed out waiting for agent status");
        let err = client.ping().unwrap_err();
        assert!(matches!(
            err,
            HerdrError::Api { ref code, .. } if code == "timeout"
        ));
        // Only the next request failed; the one after uses the default.
        client.ping().unwrap();
    }

    #[test]
    fn unknown_method_gets_invalid_request() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);
        let err = client.call_raw("bogus.method", &json!({})).unwrap_err();
        assert!(matches!(
            err,
            HerdrError::Api { ref code, .. } if code == "invalid_request"
        ));
    }

    #[test]
    fn subscription_stream_receives_pushed_frames() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let mut stream = client
            .events_subscribe(vec![Subscription::PaneAgentDetected])
            .unwrap();
        assert!(server.wait_for_open_connections(1, Duration::from_secs(2)));

        server.push_subscription_event(json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "agent_status": "idle",
                "agent": "claude"
            }
        }));
        // Lifecycle (snake_case) frames also flow through push_event.
        server.push_event(json!({
            "event": "pane_agent_detected",
            "data": {
                "type": "pane_agent_detected",
                "pane_id": "w1:p2",
                "workspace_id": "w1",
                "agent": "codex",
                "final_status": "idle",
                "released": false
            }
        }));

        match stream.next_frame().unwrap() {
            SubscriptionFrame::SubscriptionEvent(env) => {
                assert_eq!(env.event_name, "pane.agent_status_changed");
            }
            other => panic!("expected subscription event, got {other:?}"),
        }
        match stream.next_frame().unwrap() {
            SubscriptionFrame::Event(env) => {
                assert_eq!(env.event, herdr_protocol::EventKind::PaneAgentDetected);
            }
            other => panic!("expected lifecycle event, got {other:?}"),
        }
    }

    #[test]
    fn held_wait_resolves_via_push_response() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let waiter = {
            let client = client.clone();
            std::thread::spawn(move || {
                client.agent_wait("builder", Some(vec![AgentStatus::Idle]), Some(10_000))
            })
        };

        let req = server
            .wait_for_request("agent.wait", Duration::from_secs(2))
            .expect("agent.wait request recorded");
        server.push_response(&req.id, canned_result("agent_info"));

        let agent = waiter.join().unwrap().unwrap();
        assert_eq!(agent.pane_id, "w1:p1");
        assert_eq!(server.open_connections(), 0);
    }

    #[test]
    fn agent_prompt_with_wait_holds_until_pushed() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let prompter = {
            let client = client.clone();
            std::thread::spawn(move || {
                client.agent_prompt(
                    "reviewer",
                    "please rebase",
                    Some(herdr_protocol::AgentPromptWait {
                        until: Some(vec![AgentStatus::Idle]),
                        timeout_ms: Some(10_000),
                    }),
                )
            })
        };

        let req = server
            .wait_for_request("agent.prompt", Duration::from_secs(2))
            .expect("agent.prompt request recorded");
        assert!(req.params.get("wait").is_some());
        server.push_response(&req.id, canned_result("agent_info"));

        let agent = prompter.join().unwrap().unwrap();
        assert_eq!(agent.pane_id, "w1:p1");
    }

    #[test]
    fn push_error_fails_held_request() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let waiter = {
            let client = client.clone();
            std::thread::spawn(move || {
                client.events_wait(
                    herdr_protocol::EventMatch::WorkspaceCreated { workspace_id: None },
                    Some(10_000),
                )
            })
        };

        let req = server
            .wait_for_request("events.wait", Duration::from_secs(2))
            .expect("events.wait request recorded");
        server.push_error(&req.id, "timeout", "timed out waiting for event");

        let err = waiter.join().unwrap().unwrap_err();
        assert!(matches!(
            err,
            HerdrError::Api { ref code, .. } if code == "timeout"
        ));
    }

    #[test]
    fn close_connection_drops_subscribers() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let mut stream = client
            .events_subscribe(vec![Subscription::WorkspaceCreated])
            .unwrap();
        assert!(server.wait_for_open_connections(1, Duration::from_secs(2)));

        server.close_connection();
        let err = stream.next_frame().unwrap_err();
        assert!(matches!(err, HerdrError::Closed | HerdrError::Protocol(_)));
    }

    #[test]
    fn clear_resets_recordings_and_scripts() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        client.ping().unwrap();
        server.fail_next("ping", "timeout", "boom");
        server.clear();

        assert!(server.requests().is_empty());
        client.ping().unwrap(); // fail_next queue was cleared too
        assert_eq!(server.requests_for("ping").len(), 1);
    }

    #[test]
    fn concurrent_connections_are_supported() {
        let server = FakeHerdr::start().unwrap();
        let client = client_for(&server);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                std::thread::spawn(move || client.ping().unwrap().version)
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), "0.9.5");
        }
        assert_eq!(server.requests_for("ping").len(), 4);
    }
}
