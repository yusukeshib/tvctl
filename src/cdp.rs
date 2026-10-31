//! Minimal synchronous Chrome DevTools Protocol client.
//!
//! One IO thread owns the WebSocket. Calls are matched to responses by id; events
//! are broadcast to every live subscriber. Sessions use flattened mode, so
//! page-level commands carry a `sessionId`.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

pub struct Cdp {
    outgoing: Sender<String>,
    pending: Pending,
    subscribers: Arc<Mutex<Vec<Sender<Value>>>>,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
}

/// Event delivered to subscribers when the connection closes.
pub const CLOSED_EVENT: &str = "tvctl.connectionClosed";

impl Cdp {
    pub fn connect(ws_url: &str) -> Result<Cdp> {
        let (ws, _) =
            tungstenite::connect(ws_url).with_context(|| format!("connecting to {ws_url}"))?;
        if let MaybeTlsStream::Plain(s) = ws.get_ref() {
            s.set_read_timeout(Some(Duration::from_millis(20)))?;
        }
        let (outgoing, rx) = mpsc::channel();
        let cdp = Cdp {
            outgoing,
            pending: Pending::default(),
            subscribers: Arc::default(),
            next_id: AtomicU64::new(1),
            closed: Arc::default(),
        };
        let pending = cdp.pending.clone();
        let subscribers = cdp.subscribers.clone();
        let closed = cdp.closed.clone();
        std::thread::spawn(move || io_loop(ws, rx, &pending, &subscribers, &closed));
        Ok(cdp)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Receives all events from now on (`{"method", "params", "sessionId"?}`).
    pub fn subscribe(&self) -> Receiver<Value> {
        let (tx, rx) = mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(anyhow!("Chrome connection is closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.outgoing
            .send(msg.to_string())
            .map_err(|_| anyhow!("Chrome connection is closed"))?;
        let result = rx.recv_timeout(timeout);
        self.pending.lock().unwrap().remove(&id);
        match result {
            Ok(r) => r.map_err(|e| anyhow!("{method}: {e}")),
            Err(_) => Err(anyhow!("{method}: timed out")),
        }
    }
}

fn io_loop(
    mut ws: WebSocket<MaybeTlsStream<TcpStream>>,
    outgoing: Receiver<String>,
    pending: &Pending,
    subscribers: &Mutex<Vec<Sender<Value>>>,
    closed: &AtomicBool,
) {
    'outer: loop {
        loop {
            match outgoing.try_recv() {
                Ok(text) => {
                    if ws.send(Message::text(text)).is_err() {
                        break 'outer;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'outer,
            }
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(msg) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let result = match msg.get("error") {
                        Some(e) => Err(e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let _ = tx.send(result);
                    }
                } else if msg.get("method").is_some() {
                    subscribers
                        .lock()
                        .unwrap()
                        .retain(|s| s.send(msg.clone()).is_ok());
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    closed.store(true, Ordering::SeqCst);
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err("Chrome connection closed".into()));
    }
    let event = json!({ "method": CLOSED_EVENT, "params": {} });
    subscribers
        .lock()
        .unwrap()
        .retain(|s| s.send(event.clone()).is_ok());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Starts a fake DevTools endpoint. For each request it replies according to
    /// the method: `Echo` returns the params, `Fail` returns an error, `Event`
    /// emits an event before replying, `Hang` never replies, `Close` closes.
    fn mock_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            loop {
                let Ok(Message::Text(text)) = ws.read() else {
                    break;
                };
                let msg: Value = serde_json::from_str(text.as_str()).unwrap();
                let id = msg["id"].clone();
                let reply = match msg["method"].as_str().unwrap() {
                    "Echo" => {
                        json!({ "id": id, "result": { "params": msg["params"], "session": msg["sessionId"] } })
                    }
                    "Fail" => json!({ "id": id, "error": { "code": -32000, "message": "boom" } }),
                    "Event" => {
                        let ev = json!({ "method": "Test.happened", "params": { "n": 1 } });
                        ws.send(Message::text(ev.to_string())).unwrap();
                        json!({ "id": id, "result": {} })
                    }
                    "Hang" => continue,
                    "Close" => {
                        let _ = ws.close(None);
                        let _ = ws.flush();
                        break;
                    }
                    other => panic!("unexpected {other}"),
                };
                ws.send(Message::text(reply.to_string())).unwrap();
            }
        });
        format!("ws://{addr}")
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn call_returns_result_and_passes_session() {
        let cdp = Cdp::connect(&mock_server()).unwrap();
        let r = cdp.call("Echo", json!({ "a": 1 }), None, T).unwrap();
        assert_eq!(r["params"], json!({ "a": 1 }));
        assert_eq!(r["session"], Value::Null);
        let r = cdp.call("Echo", json!({}), Some("S1"), T).unwrap();
        assert_eq!(r["session"], "S1");
    }

    #[test]
    fn errors_and_timeouts() {
        let cdp = Cdp::connect(&mock_server()).unwrap();
        let e = cdp
            .call("Fail", json!({}), None, T)
            .unwrap_err()
            .to_string();
        assert!(e.contains("Fail") && e.contains("boom"), "{e}");
        let e = cdp
            .call("Hang", json!({}), None, Duration::from_millis(200))
            .unwrap_err()
            .to_string();
        assert!(e.contains("timed out"), "{e}");
        // The connection still works after a timeout.
        assert!(cdp.call("Echo", json!({}), None, T).is_ok());
    }

    #[test]
    fn events_reach_every_subscriber() {
        let cdp = Cdp::connect(&mock_server()).unwrap();
        let a = cdp.subscribe();
        let b = cdp.subscribe();
        drop(cdp.subscribe()); // dropped subscribers are pruned, not an error
        cdp.call("Event", json!({}), None, T).unwrap();
        for rx in [a, b] {
            let ev = rx.recv_timeout(T).unwrap();
            assert_eq!(ev["method"], "Test.happened");
            assert_eq!(ev["params"]["n"], 1);
        }
    }

    #[test]
    fn close_is_reported() {
        let cdp = Cdp::connect(&mock_server()).unwrap();
        let events = cdp.subscribe();
        let _ = cdp.call("Close", json!({}), None, Duration::from_millis(500));
        let ev = events.recv_timeout(T).unwrap();
        assert_eq!(ev["method"], CLOSED_EVENT);
        assert!(cdp.is_closed());
        let e = cdp
            .call("Echo", json!({}), None, T)
            .unwrap_err()
            .to_string();
        assert!(e.contains("closed"), "{e}");
    }

    #[test]
    fn connect_failure() {
        assert!(Cdp::connect("ws://127.0.0.1:1/devtools").is_err());
    }
}
