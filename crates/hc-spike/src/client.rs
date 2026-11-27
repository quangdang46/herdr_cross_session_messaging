//! Synchronous JSON-RPC client over a persistent `hc connect --json`
//! subprocess.
//!
//! Wire format (verified live against hc 0.1.17):
//! - Requests:  `{"jsonrpc":"2.0","id":N,"method":"...","params":{...}}\n`
//! - Responses: `{"id":N,"jsonrpc":"2.0","result":{...}}` or
//!   `{"id":N,"jsonrpc":"2.0","error":{"code":..,"message":..,"data":..}}`
//! - Notifications (no id): `{"jsonrpc":"2.0","method":"event","params":{...}}`
//!
//! One reader thread demuxes stdout: frames with `id` go to the pending
//! request map, `method:"event"` frames fan out to event subscribers.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{HcError, Result};

/// Default per-request timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Max stdout line size accepted from the core.
const MAX_LINE: u64 = 8 * 1024 * 1024;

type PendingMap = Mutex<HashMap<u64, mpsc::Sender<Value>>>;

/// Event notification payload (`params` of `{"method":"event"}` frames).
pub type EventFrame = Value;

/// A live connection to a local or remote (`ssh host hc connect`) core.
pub struct HcClient {
    child: Mutex<Option<Child>>,
    stdin: Mutex<ChildStdin>,
    pending: Arc<PendingMap>,
    events_tx: Arc<Mutex<Vec<mpsc::Sender<EventFrame>>>>,
    next_id: AtomicU64,
    request_timeout: Duration,
}

impl HcClient {
    /// Spawn `hc connect --json` locally.
    pub fn local() -> Result<Self> {
        Self::connect({
            let mut c = Command::new("hc");
            c.args(["connect", "--json"]);
            c
        })
    }

    /// Spawn any command that speaks the hc protocol on stdio — e.g.
    /// `ssh user@host hc connect` for a remote core.
    pub fn connect(mut cmd: Command) -> Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(HcError::Transport)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HcError::Protocol("hc connect: no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HcError::Protocol("hc connect: no stdout".into()))?;

        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let events_tx = Arc::new(Mutex::new(Vec::<mpsc::Sender<EventFrame>>::new()));

        // Reader thread: demux responses by id, fan out event notifications.
        {
            let pending = Arc::clone(&pending);
            let events_tx = Arc::clone(&events_tx);
            std::thread::Builder::new()
                .name("hc-rpc-reader".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout.take(MAX_LINE));
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break, // EOF / error: core gone
                            Ok(_) => {}
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let frame: Value = match serde_json::from_str(trimmed) {
                            Ok(v) => v,
                            Err(_) => continue, // tolerate non-JSON noise
                        };
                        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                            if let Some(tx) = pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&id)
                            {
                                let _ = tx.send(frame);
                            }
                        } else if frame.get("method").and_then(Value::as_str) == Some("event") {
                            for tx in events_tx.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                                let _ = tx.send(frame.clone());
                            }
                        }
                    }
                    // Wake all pending requests with Closed on EOF.
                    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
                    for (_, tx) in map.drain() {
                        let _ = tx.send(json!({"id": null, "error": {
                            "code": -32000, "message": "connection closed"}}));
                    }
                })
                .map_err(HcError::Transport)?;
        }

        Ok(Self {
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(stdin),
            pending,
            events_tx,
            next_id: AtomicU64::new(1),
            request_timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Override the per-request timeout (tests).
    pub fn with_request_timeout(mut self, t: Duration) -> Self {
        self.request_timeout = t;
        self
    }

    /// Raw JSON-RPC call. Returns the `result` value or an `HcError::Api`.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line =
            serde_json::to_string(&frame).map_err(|e| HcError::Protocol(e.to_string()))?;
        line.push('\n');
        {
            let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
            stdin
                .write_all(line.as_bytes())
                .map_err(HcError::Transport)?;
            stdin.flush().map_err(HcError::Transport)?;
        }

        let frame = rx
            .recv_timeout(self.request_timeout)
            .map_err(|_| HcError::Timeout {
                method: method.to_string(),
            })?;
        // The reader may have already removed our pending entry; a stray
        // close-sentinel has id:null — treat as Closed.
        if frame.get("id").is_none() || frame.get("id") == Some(&Value::Null) {
            return Err(HcError::Closed);
        }
        if let Some(err) = frame.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32000);
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            return Err(HcError::Api { code, message });
        }
        Ok(frame.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Subscribe to `{"method":"event"}` notifications. Returns a receiver
    /// that yields raw event `params` frames until the client drops.
    ///
    /// NOTE: notifications only flow after `events.subscribe` has been
    /// called (and the `hello` handshake completed) — the core does not
    /// stream unsolicited events on a bare `connect`.
    pub fn subscribe_events(&self) -> mpsc::Receiver<EventFrame> {
        let (tx, rx) = mpsc::channel();
        self.events_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    // -- Typed helpers -----------------------------------------------------

    /// `hello` handshake — returns core id, version, method/feature lists.
    pub fn hello(&self) -> Result<Value> {
        self.call("hello", json!({}))
    }

    /// `terminal.list` — all live terminals (finished hidden unless `all`).
    pub fn terminal_list(&self) -> Result<Value> {
        self.call("terminal.list", json!({}))
    }

    /// `terminal.list` filtered by labels.
    pub fn terminal_list_labeled(&self, labels: &[(&str, &str)]) -> Result<Value> {
        let map: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        self.call("terminal.list", json!({"labels": map}))
    }

    /// `terminal.run` — spawn a program in a new terminal.
    /// Returns the Terminal object (has `address` = `t7`).
    pub fn terminal_run(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        labels: &[(&str, &str)],
    ) -> Result<Value> {
        let map: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        let mut params = json!({"argv": argv, "labels": map});
        if let Some(c) = cwd {
            params["cwd"] = Value::String(c.to_string());
        }
        self.call("terminal.run", params)
    }

    /// `terminal.send` — type text + enter into a terminal.
    pub fn terminal_send(&self, terminal: &str, text: &str) -> Result<Value> {
        self.call("terminal.send", json!({"terminal": terminal, "text": text}))
    }

    /// `terminal.read` — screen + history text.
    pub fn terminal_read(&self, terminal: &str, lines: u64) -> Result<Value> {
        self.call(
            "terminal.read",
            json!({"terminal": terminal, "lines": lines}),
        )
    }

    /// `terminal.wait` — block until status/exit/output/timeout.
    /// Params mirror the API: `terminal`, optional `output`, `timeout_ms`,
    /// `children`, `if`.
    pub fn terminal_wait(&self, params: Value) -> Result<Value> {
        self.call("terminal.wait", params)
    }

    /// `terminal.keys` — press keys (`enter`, `esc`, `ctrl+c`, ...).
    pub fn terminal_keys(&self, terminal: &str, keys: &[&str]) -> Result<Value> {
        self.call("terminal.keys", json!({"terminal": terminal, "keys": keys}))
    }

    /// `terminal.kill` then `terminal.rm` — full cleanup of a terminal.
    pub fn terminal_kill_rm(&self, terminal: &str) -> Result<()> {
        let _ = self.call("terminal.kill", json!({"terminal": terminal}));
        let _ = self.call("terminal.rm", json!({"terminal": terminal}));
        Ok(())
    }

    /// `events.subscribe` — returns `{seq, terminals[]}` snapshot; live
    /// events then arrive on the receiver from [`Self::subscribe_events`].
    pub fn events_subscribe(&self, labels: &[(&str, &str)]) -> Result<Value> {
        let map: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect();
        self.call("events.subscribe", json!({"labels": map}))
    }
}

impl Drop for HcClient {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
