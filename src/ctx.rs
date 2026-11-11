//! The live browser shared by all tool calls: one page tab, its network
//! state, the console and network log, and the write check.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::process::Child;

use crate::browser::{self, StartError};
use crate::cdp::{Cdp, CdpError, Event};
use crate::config::Config;
use crate::js;
use crate::refusal::{Class, Refusal};
use crate::util::{glob, lock, now};

/// How network writes (requests other than GET, HEAD and OPTIONS) are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// Blocked unless the config's `writes`, `write_allow` or a call's permit allows them.
    Config,
    /// Every write blocked (replay without `--writes`).
    Block,
    /// Every write allowed (replay with `--writes`).
    Allow,
}

/// Requests running longer than this (long polls, streams) do not hold back settling.
const LONG_REQUEST: Duration = Duration::from_secs(5);
/// The network counts as quiet after this long without a request in flight.
const NET_QUIET: Duration = Duration::from_millis(300);

/// Keys `press` knows, with their Windows virtual key codes.
const KEYS: [(&str, i64); 13] = [
    ("Enter", 13),
    ("Tab", 9),
    ("Escape", 27),
    ("Backspace", 8),
    ("Delete", 46),
    ("ArrowUp", 38),
    ("ArrowDown", 40),
    ("ArrowLeft", 37),
    ("ArrowRight", 39),
    ("Home", 36),
    ("End", 35),
    ("PageUp", 33),
    ("PageDown", 34),
];

#[derive(Clone)]
struct Page {
    session: String,
    target: String,
}

#[derive(Default)]
struct Net {
    /// Requests in flight: request id to start time.
    inflight: HashMap<String, Instant>,
    urls: HashMap<String, String>,
    logs: Vec<Value>,
    /// Writes blocked since the last `take_blocked`.
    blocked: Vec<String>,
}

pub enum Handle {
    Object(String),
    Value(Value),
}

pub struct Ctx {
    pub cfg: Config,
    pub cdp: Cdp,
    writes: Writes,
    page: Mutex<Page>,
    net: Mutex<Net>,
    permits: Mutex<Vec<(u64, String)>>,
    permit_seq: AtomicU64,
    /// Set by the live view while a human drives the browser.
    pub human: AtomicBool,
    /// Set while the live view streams; a replacement tab restarts the stream.
    pub screencast: AtomicBool,
    child: tokio::sync::Mutex<Option<Child>>,
}

/// Allows writes to matching URLs until dropped.
pub struct Permit<'a> {
    ctx: &'a Ctx,
    id: u64,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        lock(&self.ctx.permits).retain(|(i, _)| *i != self.id);
    }
}

async fn attach_page(cdp: &Cdp, new_url: Option<&str>) -> Result<Page, CdpError> {
    let existing = match new_url {
        Some(_) => None,
        None => {
            let t = cdp.call("Target.getTargets", json!({}), None).await?;
            t["targetInfos"].as_array().and_then(|all| {
                all.iter()
                    .find(|i| i["type"] == "page" && !i["url"].as_str().unwrap_or("").starts_with("devtools://"))
                    .and_then(|i| i["targetId"].as_str().map(str::to_string))
            })
        }
    };
    let target = match existing {
        Some(id) => id,
        None => {
            let url = new_url.unwrap_or("about:blank");
            let t = cdp.call("Target.createTarget", json!({ "url": url }), None).await?;
            field(&t, "targetId", "Target.createTarget")?
        }
    };
    let s = cdp
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
            None,
        )
        .await?;
    let session = field(&s, "sessionId", "Target.attachToTarget")?;
    Ok(Page { session, target })
}

fn field(v: &Value, key: &str, method: &str) -> Result<String, CdpError> {
    v[key].as_str().map(str::to_string).ok_or_else(|| CdpError::Protocol {
        method: method.to_string(),
        code: 0,
        message: format!("answer has no {key}"),
    })
}

fn remote_text(v: &Value) -> String {
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => v["description"]
            .as_str()
            .or(v["type"].as_str())
            .unwrap_or("")
            .to_string(),
    }
}

/// The message (with stack) of a CDP `exceptionDetails`.
pub fn exception_text(ex: &Value) -> String {
    ex["exception"]["description"]
        .as_str()
        .or(ex["text"].as_str())
        .unwrap_or("unknown page error")
        .to_string()
}

fn page_error(ex: &Value) -> Refusal {
    let text = exception_text(ex);
    Refusal::new(Class::PageError, text, "check the arguments; call tree to refresh refs")
}

impl Ctx {
    pub async fn start(cfg: Config, writes: Writes) -> Result<Arc<Self>, StartError> {
        for dir in [
            cfg.evidence.clone(),
            cfg.evidence.join("shots"),
            cfg.evidence.join("downloads"),
            cfg.evidence.join("data"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|source| StartError::Dir {
                path: dir.clone(),
                source,
            })?;
        }
        let launched = browser::attach_or_launch(&cfg).await?;
        let page = attach_page(&launched.cdp, None).await?;
        let ctx = Arc::new(Self {
            cfg,
            cdp: launched.cdp,
            writes,
            page: Mutex::new(page.clone()),
            net: Mutex::default(),
            permits: Mutex::default(),
            permit_seq: AtomicU64::new(0),
            human: AtomicBool::new(false),
            screencast: AtomicBool::new(false),
            child: tokio::sync::Mutex::new(launched.child),
        });
        // Subscribe before enabling domains so no early event is missed.
        ctx.spawn_events();
        ctx.enable(&page.session).await?;
        let downloads = ctx.cfg.evidence.join("downloads").to_string_lossy().into_owned();
        ctx.cdp
            .call(
                "Browser.setDownloadBehavior",
                json!({ "behavior": "allowAndName", "downloadPath": downloads, "eventsEnabled": true }),
                None,
            )
            .await?;
        if !ctx.cfg.start_url.is_empty() && ctx.current_url().await? == "about:blank" {
            ctx.page_call("Page.navigate", json!({ "url": ctx.cfg.start_url }))
                .await?;
        }
        Ok(ctx)
    }

    /// Closes the browser if this process started it.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let Some(c) = child.as_mut() else { return };
        match self.cdp.call("Browser.close", json!({}), None).await {
            Ok(_) | Err(CdpError::Closed) => {}
            Err(e) => eprintln!("rustglance: Browser.close: {e}"),
        }
        match tokio::time::timeout(Duration::from_secs(10), c.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("rustglance: waiting for Chromium: {e}"),
            Err(_) => {
                if let Err(e) = c.kill().await {
                    eprintln!("rustglance: cannot stop Chromium: {e}");
                }
            }
        }
        *child = None;
    }

    fn checks_writes(&self) -> bool {
        match self.writes {
            Writes::Allow => false,
            Writes::Block => true,
            Writes::Config => !self.cfg.writes,
        }
    }

    async fn enable(&self, session: &str) -> Result<(), CdpError> {
        for m in ["Page.enable", "Runtime.enable", "Network.enable"] {
            self.cdp.call(m, json!({}), Some(session)).await?;
        }
        // Without a window only the front tab draws frames. A tab behind another
        // keeps its CSS animations still and returns the last frame in screenshots.
        self.cdp.call("Page.bringToFront", json!({}), Some(session)).await?;
        self.cdp
            .call(
                "Emulation.setFocusEmulationEnabled",
                json!({ "enabled": true }),
                Some(session),
            )
            .await?;
        // A front tab draws every frame of a looping animation, and without a GPU
        // Chromium draws them on the CPU. Pages that honour reduced motion stop.
        let motion = json!({ "features": [{ "name": "prefers-reduced-motion", "value": "reduce" }] });
        self.cdp
            .call("Emulation.setEmulatedMedia", motion, Some(session))
            .await?;
        if self.checks_writes() {
            let globs: Vec<&str> = if self.cfg.write_block.is_empty() {
                vec!["*"]
            } else {
                self.cfg.write_block.iter().map(String::as_str).collect()
            };
            let patterns: Vec<Value> = globs
                .iter()
                .map(|g| json!({ "urlPattern": g, "requestStage": "Request" }))
                .collect();
            self.cdp
                .call("Fetch.enable", json!({ "patterns": patterns }), Some(session))
                .await?;
        }
        if self.screencast.load(Ordering::Acquire) {
            self.start_screencast(session).await?;
        }
        Ok(())
    }

    pub async fn start_screencast(&self, session: &str) -> Result<(), CdpError> {
        self.cdp
            .call(
                "Page.startScreencast",
                json!({ "format": "jpeg", "quality": 70, "everyNthFrame": 1 }),
                Some(session),
            )
            .await
            .map(drop)
    }

    pub fn session(&self) -> String {
        lock(&self.page).session.clone()
    }

    pub async fn page_call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        let s = self.session();
        self.cdp.call(method, params, Some(&s)).await
    }

    pub async fn page_call_with(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, CdpError> {
        let s = self.session();
        self.cdp.call_with(method, params, Some(&s), timeout).await
    }

    /// The tab's URL and title from the browser side. Works while the page
    /// navigates or hangs, unlike asking the page itself.
    pub async fn page_info(&self) -> Result<(String, String), CdpError> {
        let target = lock(&self.page).target.clone();
        let info = self
            .cdp
            .call("Target.getTargetInfo", json!({ "targetId": target }), None)
            .await?;
        let t = &info["targetInfo"];
        Ok((
            t["url"].as_str().unwrap_or("").to_string(),
            t["title"].as_str().unwrap_or("").to_string(),
        ))
    }

    pub async fn current_url(&self) -> Result<String, CdpError> {
        Ok(self.page_info().await?.0)
    }

    /// Replaces a tab that stopped answering with a new one and returns the
    /// URL the old tab had and whether the new tab was sent back there. Calls
    /// that run in the page (like `get`) need the site loaded, not a blank tab.
    pub async fn recover(&self) -> Result<(String, bool), CdpError> {
        let old = lock(&self.page).clone();
        let url = self.current_url().await?;
        let page = attach_page(&self.cdp, Some("about:blank")).await?;
        *lock(&self.page) = page.clone();
        {
            let mut net = lock(&self.net);
            net.inflight.clear();
            net.urls.clear();
        }
        self.enable(&page.session).await?;
        if let Err(e) = self
            .cdp
            .call("Target.closeTarget", json!({ "targetId": old.target }), None)
            .await
        {
            eprintln!("rustglance: cannot close the stuck tab: {e}");
        }
        let back = url.starts_with("http")
            && match self.page_call("Page.navigate", json!({ "url": url })).await {
                Ok(_) => true,
                Err(e) => {
                    eprintln!("rustglance: cannot reopen {url}: {e}");
                    false
                }
            };
        Ok((url, back))
    }

    fn spawn_events(self: &Arc<Self>) {
        let mut rx = self.cdp.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let Some(ctx) = weak.upgrade() else { return };
                ctx.on_event(&ev);
            }
        });
    }

    fn on_event(&self, ev: &Event) {
        if ev.method == "Fetch.requestPaused" {
            self.on_paused(ev);
            return;
        }
        if ev.session.as_deref() != Some(lock(&self.page).session.as_str()) {
            return;
        }
        let p = &ev.params;
        let id = p["requestId"].as_str().unwrap_or("").to_string();
        match ev.method.as_str() {
            "Network.requestWillBeSent" => {
                let kind = p["type"].as_str().unwrap_or("");
                let url = p["request"]["url"].as_str().unwrap_or("").to_string();
                let mut net = lock(&self.net);
                if kind != "EventSource" && kind != "WebSocket" {
                    net.inflight.insert(id.clone(), Instant::now());
                }
                net.urls.insert(id, url);
            }
            "Network.loadingFinished" => {
                let mut net = lock(&self.net);
                net.inflight.remove(&id);
                net.urls.remove(&id);
            }
            "Network.loadingFailed" => {
                let url = {
                    let mut net = lock(&self.net);
                    net.inflight.remove(&id);
                    net.urls.remove(&id).unwrap_or_default()
                };
                let error = p["errorText"].as_str().unwrap_or("");
                // Blocked writes are logged when blocked; cancels are noise.
                if !p["canceled"].as_bool().unwrap_or(false) && !error.contains("BLOCKED_BY_CLIENT") {
                    self.log("failed", json!({ "url": url, "text": error }));
                }
            }
            "Network.responseReceived" => {
                let status = p["response"]["status"].as_u64().unwrap_or(0);
                if status >= 400 {
                    self.log("http", json!({ "url": p["response"]["url"], "status": status }));
                }
                // A stream read through fetch stays open for good, like EventSource;
                // once it answers it no longer keeps the page from settling.
                if p["response"]["mimeType"] == "text/event-stream" {
                    lock(&self.net).inflight.remove(&id);
                }
            }
            "Runtime.consoleAPICalled" => {
                let text = p["args"]
                    .as_array()
                    .map(|a| a.iter().map(remote_text).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                self.log("console", json!({ "level": p["type"], "text": text }));
            }
            "Runtime.exceptionThrown" => {
                let ex = &p["exceptionDetails"];
                self.log("exception", json!({ "text": exception_text(ex), "url": ex["url"] }));
            }
            _ => {}
        }
    }

    fn on_paused(&self, ev: &Event) {
        let p = &ev.params;
        let id = p["requestId"].as_str().unwrap_or("").to_string();
        let method = p["request"]["method"].as_str().unwrap_or("GET");
        let url = p["request"]["url"].as_str().unwrap_or("");
        let read = matches!(method, "GET" | "HEAD" | "OPTIONS");
        let allowed = read || self.write_allowed(url);
        if !read {
            let what = format!("{method} {url}");
            if allowed {
                self.log("write", json!({ "text": what, "allowed": true }));
            } else {
                self.log("blocked", json!({ "text": what, "allowed": false }));
                lock(&self.net).blocked.push(what);
            }
        }
        let cdp = self.cdp.clone();
        let session = ev.session.clone();
        tokio::spawn(async move {
            let r = if allowed {
                cdp.call("Fetch.continueRequest", json!({ "requestId": id }), session.as_deref())
                    .await
            } else {
                let params = json!({ "requestId": id, "errorReason": "BlockedByClient" });
                cdp.call("Fetch.failRequest", params, session.as_deref()).await
            };
            if let Err(e) = r {
                eprintln!("rustglance: paused request {id} not released: {e}");
            }
        });
    }

    fn write_allowed(&self, url: &str) -> bool {
        match self.writes {
            Writes::Allow => true,
            Writes::Block => false,
            Writes::Config => {
                self.cfg.writes
                    || self.cfg.write_allow.iter().any(|g| glob(g, url))
                    || lock(&self.permits).iter().any(|(_, g)| glob(g, url))
            }
        }
    }

    /// Allows writes to URLs matching `pattern` while the permit lives.
    /// Only the `Config` mode honours permits; replay decides on its own.
    pub fn permit(&self, pattern: impl Into<String>) -> Permit<'_> {
        let id = self.permit_seq.fetch_add(1, Ordering::Relaxed);
        lock(&self.permits).push((id, pattern.into()));
        Permit { ctx: self, id }
    }

    pub fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.net).blocked)
    }

    pub fn log(&self, kind: &str, fields: Value) {
        let mut m = match fields {
            Value::Object(m) => m,
            other => Map::from_iter([("text".to_string(), other)]),
        };
        m.insert("t".to_string(), json!(now()));
        m.insert("kind".to_string(), json!(kind));
        let mut net = lock(&self.net);
        m.insert("i".to_string(), json!(net.logs.len()));
        net.logs.push(Value::Object(m));
    }

    /// Log entries from index `since` on, and the index to ask from next time.
    pub fn logs_since(&self, since: usize) -> (Vec<Value>, usize) {
        let net = lock(&self.net);
        (
            net.logs.get(since..).map(<[Value]>::to_vec).unwrap_or_default(),
            net.logs.len(),
        )
    }

    /// Runs `(func)(arg)` in the page and returns the JSON value it resolves to.
    /// `func` is a fixed function from `js` (or an extension's own constant);
    /// `arg` is serialized JSON, so tool arguments reach the page as data,
    /// never as code. No tool runs agent-supplied JavaScript.
    pub async fn eval(&self, func: &str, arg: &Value) -> Result<Value, Refusal> {
        let params = json!({
            "expression": format!("({func})({arg})"),
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        });
        let r = self.page_call("Runtime.evaluate", params).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(page_error(ex));
        }
        Ok(r["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    /// Like `eval`, but an object result comes back as a handle.
    /// Release it with `release`.
    pub async fn eval_handle(&self, func: &str, arg: &Value) -> Result<Handle, Refusal> {
        let params = json!({ "expression": format!("({func})({arg})"), "returnByValue": false, "awaitPromise": true });
        let r = self.page_call("Runtime.evaluate", params).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(page_error(ex));
        }
        Ok(match r["result"]["objectId"].as_str() {
            Some(id) => Handle::Object(id.to_string()),
            None => Handle::Value(r["result"].get("value").cloned().unwrap_or(Value::Null)),
        })
    }

    pub async fn release(&self, object_id: &str) {
        if let Err(e) = self
            .page_call("Runtime.releaseObject", json!({ "objectId": object_id }))
            .await
        {
            eprintln!("rustglance: Runtime.releaseObject: {e}");
        }
    }

    /// A real left click at viewport coordinates (CSS pixels).
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), Refusal> {
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let mut p = json!({ "type": kind, "x": x, "y": y, "buttons": buttons });
            if kind != "mouseMoved" {
                p["button"] = json!("left");
                p["clickCount"] = json!(1);
            }
            self.page_call("Input.dispatchMouseEvent", p).await?;
        }
        Ok(())
    }

    pub fn key_names() -> String {
        KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
    }

    /// Presses and releases a named key on the focused element.
    pub async fn press(&self, key: &str) -> Result<(), Refusal> {
        let Some(&(_, vk)) = KEYS.iter().find(|(k, _)| *k == key) else {
            return Err(Refusal::bad_args(format!(
                "unknown key {key:?}; known keys: {}",
                Self::key_names()
            )));
        };
        let mut down = json!({ "type": "rawKeyDown", "key": key, "code": key, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        if key == "Enter" {
            down["type"] = json!("keyDown");
            down["text"] = json!("\r");
            down["unmodifiedText"] = json!("\r");
        }
        self.page_call("Input.dispatchKeyEvent", down).await?;
        let up = json!({ "type": "keyUp", "key": key, "code": key, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
        self.page_call("Input.dispatchKeyEvent", up).await?;
        Ok(())
    }

    /// Waits until no recent request is in flight and the DOM stops
    /// changing, at most `settle_ms` after `since`. Returns whether the page
    /// went quiet in time; a busy page is not an error.
    pub async fn settle(&self, since: Instant) -> Result<bool, Refusal> {
        let deadline = since + Duration::from_millis(self.cfg.settle_ms);
        loop {
            if !self.network_quiet(deadline).await {
                return Ok(false);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let max = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
            match self.eval(js::DOM_QUIET, &json!({ "quiet": 200, "max": max })).await {
                Ok(v) => return Ok(v.as_bool().unwrap_or(false)),
                // A navigation destroyed the document mid-wait; wait for the next one.
                Err(r) if matches!(r.class, Class::PageError | Class::Browser) => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(r) => return Err(r),
            }
        }
    }

    async fn network_quiet(&self, deadline: Instant) -> bool {
        let mut idle: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let busy = lock(&self.net)
                .inflight
                .values()
                .any(|t| now.duration_since(*t) < LONG_REQUEST);
            if busy {
                idle = None;
            } else if now.duration_since(*idle.get_or_insert(now)) >= NET_QUIET {
                return true;
            }
            if now >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
