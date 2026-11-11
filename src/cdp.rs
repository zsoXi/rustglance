//! Raw Chrome DevTools Protocol client over one WebSocket.
//!
//! Commands are matched to answers by id. Every event goes to every
//! subscriber through an unbounded channel: a lost `Fetch.requestPaused`
//! would leave a request hanging in the page, so nothing is dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::util::lock;

#[derive(Debug, thiserror::Error)]
pub enum CdpError {
    #[error("cannot connect to the browser at {url}")]
    Connect {
        url: String,
        #[source]
        source: tungstenite::Error,
    },
    #[error("the browser connection is closed")]
    Closed,
    #[error("{method} got no answer within {ms} ms")]
    Timeout { method: String, ms: u64 },
    #[error("{method} failed: {message} (code {code})")]
    Protocol { method: String, code: i64, message: String },
}

#[derive(Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// Flat-mode session the event came from; `None` for the browser target.
    pub session: Option<String>,
}

type Reply = Result<Value, (i64, String)>;

struct Inner {
    out: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    subs: Mutex<Vec<mpsc::UnboundedSender<Arc<Event>>>>,
    next: AtomicU64,
    closed: AtomicBool,
    timeout: Duration,
}

#[derive(Clone)]
pub struct Cdp(Arc<Inner>);

impl Cdp {
    /// Connects to a browser WebSocket URL. `timeout` bounds every command.
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self, CdpError> {
        // Full-page screenshots can exceed the default 64 MiB message limit.
        let config = WebSocketConfig::default().max_message_size(None).max_frame_size(None);
        let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true)
            .await
            .map_err(|source| CdpError::Connect {
                url: url.to_string(),
                source,
            })?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Message>();
        let inner = Arc::new(Inner {
            out,
            pending: Mutex::new(HashMap::new()),
            subs: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            timeout,
        });
        // The writer ends when every `Cdp` handle is dropped.
        tokio::spawn(async move {
            while let Some(m) = outgoing.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let reader = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Some(inner) = reader.upgrade() else { return };
                inner.dispatch(text.as_str());
            }
            if let Some(inner) = reader.upgrade() {
                inner.closed.store(true, Ordering::Release);
                // Dropping the senders wakes every waiter with `Closed`.
                lock(&inner.pending).clear();
                lock(&inner.subs).clear();
            }
        });
        Ok(Self(inner))
    }

    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value, CdpError> {
        self.call_with(method, params, session, self.0.timeout).await
    }

    pub async fn call_with(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err(CdpError::Closed);
        }
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.0.pending).insert(id, tx);
        if self.0.out.send(Message::text(msg.to_string())).is_err() {
            lock(&self.0.pending).remove(&id);
            return Err(CdpError::Closed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err((code, message)))) => Err(CdpError::Protocol {
                method: method.to_string(),
                code,
                message,
            }),
            Ok(Err(_)) => Err(CdpError::Closed),
            Err(_) => {
                lock(&self.0.pending).remove(&id);
                let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                Err(CdpError::Timeout {
                    method: method.to_string(),
                    ms,
                })
            }
        }
    }

    /// Every event from now on. The channel ends when the connection closes.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Arc<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        if !self.0.closed.load(Ordering::Acquire) {
            lock(&self.0.subs).push(tx);
        }
        rx
    }

    pub fn timeout(&self) -> Duration {
        self.0.timeout
    }
}

impl Inner {
    fn dispatch(&self, text: &str) {
        let mut v: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("rustglance: unreadable CDP message: {e}");
                return;
            }
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            // No waiter means the caller already timed out.
            let Some(tx) = lock(&self.pending).remove(&id) else {
                return;
            };
            let reply = match v.get("error") {
                Some(e) => Err((
                    e.get("code").and_then(Value::as_i64).unwrap_or(0),
                    e.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                )),
                None => Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
            };
            let _ = tx.send(reply);
            return;
        }
        let Some(method) = v.get("method").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let event = Arc::new(Event {
            method,
            params: v.get_mut("params").map(Value::take).unwrap_or(Value::Null),
            session: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
        });
        lock(&self.subs).retain(|s| s.send(event.clone()).is_ok());
    }
}
