//! Live view: the tab as an MJPEG stream on 127.0.0.1, with takeover.
//!
//! "Take over" makes every agent call refuse with `human_control` and sends
//! the viewer's clicks, wheel and keys to the page. "Release" hands control
//! back. Only requests addressed to 127.0.0.1 or localhost on this port are
//! served, so a web page cannot reach the view through DNS rebinding.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::ctx::Ctx;

/// Bounds for one request from the viewer page; its requests are tiny.
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 64 * 1024;

type Frame = Option<Arc<Vec<u8>>>;

const PAGE: &str = r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><title>rustglance live view</title>
<style>
body { margin: 0; font: 14px system-ui, sans-serif; background: #111; color: #eee; }
header { padding: 8px; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
button { font: inherit; padding: 4px 12px; }
img { max-width: 100%; display: block; cursor: crosshair; outline: none; }
</style>
<header><button id="take">Take over</button><button id="release">Release</button><span id="state"></span></header>
<img id="v" src="/stream" tabindex="0" alt="live view of the browser tab">
<script>
const v = document.getElementById('v'), st = document.getElementById('state');
let human = false;
const post = (p, b) => fetch(p, { method: 'POST', headers: { 'content-type': 'application/json' }, body: b ? JSON.stringify(b) : '' });
const show = () => { st.textContent = human ? 'You have control; agent calls are refused.' : 'The agent has control; view only.'; };
document.getElementById('take').onclick = async () => { await post('/take'); human = true; show(); v.focus(); };
document.getElementById('release').onclick = async () => { await post('/release'); human = false; show(); };
// Frame pixels map to CSS pixels at device scale 1, as in headless Chromium.
const at = (e) => ({ x: e.offsetX * v.naturalWidth / v.clientWidth, y: e.offsetY * v.naturalHeight / v.clientHeight });
const mouse = (event, e, extra) => { if (human) post('/input', Object.assign({ type: 'mouse', event }, at(e), extra)); };
v.addEventListener('mousedown', (e) => { e.preventDefault(); v.focus(); mouse('mousePressed', e, { button: 'left', clickCount: 1 }); });
v.addEventListener('mouseup', (e) => mouse('mouseReleased', e, { button: 'left', clickCount: 1 }));
v.addEventListener('wheel', (e) => { if (human) { e.preventDefault(); mouse('mouseWheel', e, { deltaX: e.deltaX, deltaY: e.deltaY }); } }, { passive: false });
v.addEventListener('keydown', (e) => { if (human) { e.preventDefault(); post('/input', { type: 'key', key: e.key }); } });
fetch('/state').then((r) => r.json()).then((s) => { human = s.human; show(); });
</script>
</html>
"#;

/// Starts the screencast and the HTTP server; both run until the process ends.
pub async fn start(ctx: Arc<Ctx>, port: u16) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let (frames, rx) = watch::channel::<Frame>(None);
    let mut events = ctx.cdp.subscribe();
    // Right after start the tab may still be swapping to start_url, and Chromium
    // refuses a screencast until the new page is active; try for about 5 s.
    let mut tries = 0;
    while let Err(e) = ctx.start_screencast(&ctx.session()).await {
        tries += 1;
        if tries == 10 {
            return Err(io::Error::other(e));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    ctx.screencast.store(true, Ordering::Release);
    let weak = Arc::downgrade(&ctx);
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if ev.method != "Page.screencastFrame" {
                continue;
            }
            let Some(ctx) = weak.upgrade() else { return };
            // Chromium sends the next frame only after this ack.
            let ack = json!({ "sessionId": ev.params["sessionId"] });
            if let Err(e) = ctx
                .cdp
                .call("Page.screencastFrameAck", ack, ev.session.as_deref())
                .await
            {
                eprintln!("rustglance: screencast ack: {e}");
            }
            match STANDARD.decode(ev.params["data"].as_str().unwrap_or("")) {
                Ok(jpeg) => {
                    frames.send_replace(Some(Arc::new(jpeg)));
                }
                Err(e) => eprintln!("rustglance: unreadable screencast frame: {e}"),
            }
        }
    });
    eprintln!("rustglance: live view at http://127.0.0.1:{port}/");
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let (ctx, rx) = (ctx.clone(), rx.clone());
                    tokio::spawn(async move {
                        // A viewer closing the tab ends its connection; nothing to report.
                        let _ = serve(stream, &ctx, rx, port).await;
                    });
                }
                Err(e) => {
                    eprintln!("rustglance: live view: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map_or("", |(_, v)| v.as_str())
    }
}

fn invalid(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

async fn read_request(s: &mut TcpStream) -> io::Result<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("request head too large"));
        }
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| invalid("request head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("");
    let path = target.split('?').next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err(invalid("request body too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request {
        method,
        path,
        headers,
        body,
    })
}

async fn respond(s: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body).await?;
    s.shutdown().await
}

/// Why a request is not for this view: Host must name it, and a POST's Origin too.
fn wrong_address(req: &Request, port: u16) -> Option<&'static str> {
    let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !local.iter().any(|h| h == req.header("host")) {
        return Some("wrong host");
    }
    if req.method == "POST" && !local.iter().any(|h| req.header("origin") == format!("http://{h}")) {
        return Some("wrong origin");
    }
    None
}

async fn serve(mut s: TcpStream, ctx: &Ctx, rx: watch::Receiver<Frame>, port: u16) -> io::Result<()> {
    let req = read_request(&mut s).await?;
    if let Some(why) = wrong_address(&req, port) {
        return respond(&mut s, "403 Forbidden", "text/plain", why.as_bytes()).await;
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await,
        ("GET", "/stream") => stream(s, rx).await,
        ("GET", "/state") => {
            let body = json!({ "human": ctx.human.load(Ordering::Acquire) }).to_string();
            respond(&mut s, "200 OK", "application/json", body.as_bytes()).await
        }
        ("POST", "/take") => {
            ctx.human.store(true, Ordering::Release);
            eprintln!("rustglance: a human took over the browser");
            respond(&mut s, "204 No Content", "text/plain", b"").await
        }
        ("POST", "/release") => {
            ctx.human.store(false, Ordering::Release);
            eprintln!("rustglance: the human released the browser");
            respond(&mut s, "204 No Content", "text/plain", b"").await
        }
        ("POST", "/input") => {
            if !ctx.human.load(Ordering::Acquire) {
                return respond(&mut s, "409 Conflict", "text/plain", b"take over first").await;
            }
            let res = match serde_json::from_slice::<Value>(&req.body) {
                Ok(v) => input(ctx, &v).await,
                Err(e) => Err(format!("body is not JSON: {e}")),
            };
            match res {
                Ok(()) => respond(&mut s, "204 No Content", "text/plain", b"").await,
                Err(e) => respond(&mut s, "400 Bad Request", "text/plain", e.as_bytes()).await,
            }
        }
        _ => respond(&mut s, "404 Not Found", "text/plain", b"not found").await,
    }
}

async fn stream(mut s: TcpStream, mut rx: watch::Receiver<Frame>) -> io::Result<()> {
    s.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )
    .await?;
    loop {
        let frame = rx.borrow_and_update().clone();
        if let Some(jpeg) = frame {
            let part = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                jpeg.len()
            );
            s.write_all(part.as_bytes()).await?;
            s.write_all(&jpeg).await?;
            s.write_all(b"\r\n").await?;
        }
        if rx.changed().await.is_err() {
            return Ok(());
        }
    }
}

/// Sends one viewer event to the page.
async fn input(ctx: &Ctx, v: &Value) -> Result<(), String> {
    match v["type"].as_str() {
        Some("mouse") => {
            let event = v["event"].as_str().unwrap_or("");
            if !matches!(event, "mousePressed" | "mouseReleased" | "mouseMoved" | "mouseWheel") {
                return Err(format!("unknown mouse event {event:?}"));
            }
            let (Some(x), Some(y)) = (v["x"].as_f64(), v["y"].as_f64()) else {
                return Err("x and y are required".to_string());
            };
            let mut p = json!({ "type": event, "x": x, "y": y });
            match event {
                "mousePressed" | "mouseReleased" => {
                    p["button"] = json!("left");
                    p["clickCount"] = json!(1);
                    p["buttons"] = json!(if event == "mousePressed" { 1 } else { 0 });
                }
                "mouseWheel" => {
                    p["deltaX"] = json!(v["deltaX"].as_f64().unwrap_or(0.0));
                    p["deltaY"] = json!(v["deltaY"].as_f64().unwrap_or(0.0));
                }
                _ => {}
            }
            ctx.page_call("Input.dispatchMouseEvent", p)
                .await
                .map(drop)
                .map_err(|e| e.to_string())
        }
        Some("key") => {
            let key = v["key"].as_str().unwrap_or("");
            if key.chars().count() == 1 {
                ctx.page_call("Input.insertText", json!({ "text": key }))
                    .await
                    .map(drop)
                    .map_err(|e| e.to_string())
            } else {
                ctx.press(key).await.map_err(|r| r.reason)
            }
        }
        _ => Err("type must be mouse or key".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, host: &str, origin: Option<&str>) -> Request {
        let mut headers = vec![("host".to_string(), host.to_string())];
        if let Some(o) = origin {
            headers.push(("origin".to_string(), o.to_string()));
        }
        Request {
            method: method.to_string(),
            path: "/".to_string(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn view_answers_only_local_addresses() {
        for host in ["127.0.0.1:9333", "localhost:9333"] {
            assert_eq!(wrong_address(&request("GET", host, None), 9333), None);
        }
        let origin = Some("http://localhost:9333");
        assert_eq!(wrong_address(&request("POST", "localhost:9333", origin), 9333), None);

        for host in ["evil.test:9333", "127.0.0.1:1", ""] {
            assert_eq!(wrong_address(&request("GET", host, None), 9333), Some("wrong host"));
        }
        for origin in [None, Some("http://evil.test"), Some("https://localhost:9333")] {
            assert_eq!(
                wrong_address(&request("POST", "localhost:9333", origin), 9333),
                Some("wrong origin")
            );
        }
    }
}
