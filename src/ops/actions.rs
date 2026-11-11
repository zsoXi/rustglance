//! Page tools: open, tree, tap, type, check.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::{args, logged_out, opt_bool, opt_ref, opt_str, opt_u64, req_str, web_url};
use crate::ctx::Ctx;
use crate::js;
use crate::mcp::{OpFuture, Output};
use crate::refusal::{Class, Refusal};

/// The longest wait `check` accepts.
const LONGEST_CHECK: Duration = Duration::from_secs(120);

/// What to click: a ref from tree, or a visible label.
pub struct Target {
    pub r: Option<String>,
    pub text: Option<String>,
}

/// Reads `ref` or `text` (exactly one) from the arguments.
pub fn target(a: &Map<String, Value>) -> Result<Target, Refusal> {
    let r = opt_ref(a, "ref")?;
    let text = opt_str(a, "text")?.map(str::to_string);
    match (&r, &text) {
        (None, None) => Err(Refusal::bad_args("give ref (from tree) or text (the visible label)")),
        (Some(_), Some(_)) => Err(Refusal::bad_args("give ref or text, not both")),
        _ => Ok(Target { r, text }),
    }
}

pub struct Hit {
    pub r: String,
    pub desc: String,
}

/// Scrolls the target into view, refuses if it is covered or has no size,
/// and clicks its center with real mouse events.
pub async fn click(ctx: &Ctx, t: &Target) -> Result<Hit, Refusal> {
    let p = ctx
        .eval(js::CLICK_POINT, &json!({ "ref": t.r, "text": t.text }))
        .await?;
    let label = t.r.as_deref().or(t.text.as_deref()).unwrap_or("");
    if p["stale"] == true {
        return Err(Refusal::stale(label));
    }
    if let Some(many) = p["many"].as_array() {
        let list: Vec<&str> = many.iter().filter_map(Value::as_str).collect();
        return Err(if list.is_empty() {
            Refusal::new(
                Class::NotFound,
                format!("nothing clickable is labelled {label:?}"),
                "call tree and tap by ref",
            )
        } else {
            Refusal::new(
                Class::BadArgs,
                format!("{} elements match {label:?}: {}", list.len(), list.join("; ")),
                "tap one of the listed refs",
            )
        });
    }
    let r = p["ref"].as_str().unwrap_or(label).to_string();
    if p["disabled"] == true {
        return Err(Refusal::new(
            Class::NotFound,
            format!("{r} is disabled; a click would do nothing"),
            "wait until the page enables it (check, then tree), or finish what it waits for",
        ));
    }
    if p["zero"] == true {
        return Err(Refusal::new(
            Class::NotFound,
            format!("{r} has no size on screen (hidden or collapsed)"),
            "open the menu or section that holds it, then call tree",
        ));
    }
    let cover = &p["covered"];
    if cover.is_object() {
        let cref = cover["ref"].as_str().unwrap_or("?");
        let cdesc = cover["desc"].as_str().unwrap_or("");
        return Err(Refusal::new(
            Class::Covered,
            format!("{r} is covered by {cref} {cdesc}; a click would land on {cref}"),
            format!("close or move {cref} first (Escape, its close button, or scroll), then tap {r} again"),
        ));
    }
    let (Some(x), Some(y)) = (p["x"].as_f64(), p["y"].as_f64()) else {
        return Err(Refusal::new(
            Class::Browser,
            format!("no click point for {r}"),
            "call tree and try again",
        ));
    };
    ctx.click_at(x, y).await?;
    Ok(Hit {
        r,
        desc: p["desc"].as_str().unwrap_or("").to_string(),
    })
}

/// The URL as the browser parses it, resolved against the page (else
/// `start_url`); `host.tld/x` gets https://. Only http, https and
/// `about:blank` come out; the scheme is checked on the parsed form.
pub async fn resolve(ctx: &Ctx, url: &str) -> Result<String, Refusal> {
    let url = url.trim();
    web_url(url)?;
    if url == "about:blank" {
        return Ok(url.to_string());
    }
    let first = url.split('/').next().unwrap_or("");
    let url = if !url.contains("://") && !url.starts_with(['/', '.', '?', '#']) && first.contains('.') {
        format!("https://{url}")
    } else {
        url.to_string()
    };
    let current = ctx.current_url().await?;
    let base = if current.starts_with("http") {
        current
    } else {
        ctx.cfg.start_url.clone()
    };
    let v = ctx.eval(js::RESOLVE, &json!({ "rel": url, "base": base })).await?;
    let Some(resolved) = v.as_str() else {
        return Err(Refusal::bad_args(format!("cannot resolve {url:?}; give a full URL")));
    };
    web_url(resolved)?;
    Ok(resolved.to_string())
}

pub fn open(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["url", "wait_for"])?;
        let ready = crate::ready::parse(a.get("wait_for"))?;
        crate::ready::validate_selector(ctx, ready.as_ref()).await?;
        let url = resolve(ctx, req_str(&a, "url")?).await?;
        let previous = if ready.is_some() {
            ctx.eval("() => performance.timeOrigin", &json!({})).await?.as_f64()
        } else {
            None
        };
        let start = Instant::now();
        let wait = Duration::from_millis(ctx.cfg.cdp_timeout_ms.saturating_add(ctx.cfg.settle_ms));
        let r = ctx.page_call_with("Page.navigate", json!({ "url": url }), wait).await?;
        if let Some(e) = r["errorText"].as_str().filter(|e| !e.is_empty()) {
            return Err(Refusal::new(
                Class::Browser,
                format!("cannot open {url}: {e}"),
                "check the URL; the site may be down",
            ));
        }
        let readiness = if let Some(ref condition) = ready {
            Some(
                crate::ready::wait(
                    ctx,
                    condition,
                    if r.get("loaderId").is_some() { previous } else { None },
                )
                .await?,
            )
        } else {
            None
        };
        let settled = if ready.is_none() {
            Some(ctx.settle(start).await?)
        } else {
            None
        };
        let (now, title) = ctx.page_info().await?;
        logged_out(ctx, &now)?;
        let mut result = json!({ "url": now, "title": title, "settled": settled });
        if let Some(readiness) = readiness {
            result["readiness"] = readiness;
        }
        Ok(result.into())
    })
}

pub fn tree(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["find", "within"])?;
        let find = opt_str(&a, "find")?;
        let within = opt_ref(&a, "within")?;
        let r = ctx.eval(js::TREE, &json!({ "find": find, "within": within })).await?;
        if r["stale"] == true {
            return Err(Refusal::stale(within.as_deref().unwrap_or("")));
        }
        let mut out = Output::from(json!({ "url": r["url"], "title": r["title"], "lines": r["count"] }));
        out.text = Some(r["tree"].as_str().unwrap_or("").to_string());
        Ok(out)
    })
}

pub fn tap(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["ref", "text", "wait_for"])?;
        let ready = crate::ready::parse(a.get("wait_for"))?;
        crate::ready::validate_selector(ctx, ready.as_ref()).await?;
        let t = target(&a)?;
        let start = Instant::now();
        let hit = click(ctx, &t).await?;
        let readiness = if let Some(ref condition) = ready {
            Some(crate::ready::wait(ctx, condition, None).await?)
        } else {
            None
        };
        let settled = if ready.is_none() {
            Some(ctx.settle(start).await?)
        } else {
            None
        };
        let url = ctx.current_url().await?;
        logged_out(ctx, &url)?;
        let mut result = json!({ "ref": hit.r, "clicked": hit.desc, "url": url, "settled": settled });
        if let Some(readiness) = readiness {
            result["readiness"] = readiness;
        }
        Ok(result.into())
    })
}

pub fn type_(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["ref", "text", "key", "clear", "submit"])?;
        let r = opt_ref(&a, "ref")?;
        let text = opt_str(&a, "text")?;
        let key = opt_str(&a, "key")?;
        let clear = opt_bool(&a, "clear")?.unwrap_or(true);
        let submit = opt_bool(&a, "submit")?.unwrap_or(false);
        match (text, key) {
            (None, None) => return Err(Refusal::bad_args("give text (with ref) or key")),
            (Some(_), Some(_)) => {
                return Err(Refusal::bad_args(
                    "give text or key, not both; submit:true presses Enter after text",
                ));
            }
            (Some(_), None) if r.is_none() => {
                return Err(Refusal::bad_args("text needs ref: the field to type into, from tree"));
            }
            _ => {}
        }
        let start = Instant::now();
        let f = ctx
            .eval(
                js::FOCUS,
                &json!({ "ref": r, "text": text, "clear": clear && text.is_some() }),
            )
            .await?;
        let label = r.as_deref().unwrap_or("the focused element");
        if f["stale"] == true {
            return Err(Refusal::stale(label));
        }
        if f["password"] == true {
            return Err(Refusal::new(
                Class::Password,
                format!("{label} is a password field"),
                "stop; a human must log in",
            ));
        }
        if f["disabled"] == true || f["readonly"] == true {
            return Err(Refusal::new(
                Class::NotFound,
                format!("{label} or its selected option is disabled, inert, or read-only"),
                "choose an enabled editable control from tree; do not force the value",
            ));
        }
        if let Some(options) = f["options"].as_array() {
            let list: Vec<&str> = options.iter().filter_map(Value::as_str).collect();
            return Err(Refusal::new(
                Class::NotFound,
                format!(
                    "{label} has no option {:?}; options: {}",
                    text.unwrap_or(""),
                    list.join(" | ")
                ),
                "type one of the listed options",
            ));
        }
        if let Some(d) = f["unfocusable"].as_str() {
            return Err(Refusal::new(
                Class::NotFound,
                format!("{label} ({d}) cannot take keyboard focus"),
                "tap it first, or use the input inside it (call tree with within)",
            ));
        }
        let fref = f["ref"].as_str().map(str::to_string);
        if f.get("selected").is_none() {
            match (text, key) {
                // The field's content is selected, so a key press clears it.
                (Some(""), _) if clear => ctx.press("Backspace").await?,
                (Some(t), _) if !t.is_empty() => {
                    ctx.page_call("Input.insertText", json!({ "text": t })).await?;
                }
                (_, Some(k)) => ctx.press(k).await?,
                _ => {}
            }
        }
        if submit {
            ctx.press("Enter").await?;
        }
        let settled = ctx.settle(start).await?;
        // After submit the field may be gone; the value is then unknown, not an error.
        let value = match (&r, &fref) {
            (Some(_), Some(fr)) => ctx.eval(js::VALUE, &json!({ "ref": fr })).await.unwrap_or(Value::Null),
            _ => Value::Null,
        };
        let url = ctx.current_url().await?;
        logged_out(ctx, &url)?;
        Ok(json!({ "ref": fref, "value": value, "selected": f["selected"], "url": url, "settled": settled }).into())
    })
}

/// The wait of `check`. Calls run one at a time, so a long wait holds up the rest.
fn check_timeout(a: &Map<String, Value>) -> Result<Duration, Refusal> {
    let timeout = Duration::from_millis(opt_u64(a, "timeout_ms")?.unwrap_or(5_000));
    if timeout > LONGEST_CHECK {
        return Err(Refusal::bad_args(format!(
            "timeout_ms must be at most {}",
            LONGEST_CHECK.as_millis()
        )));
    }
    Ok(timeout)
}

pub fn check(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["text", "gone", "url", "ref", "timeout_ms"])?;
        let text = opt_str(&a, "text")?;
        let gone = opt_str(&a, "gone")?;
        let url = opt_str(&a, "url")?;
        let r = opt_ref(&a, "ref")?;
        if text.is_none() && gone.is_none() && url.is_none() && r.is_none() {
            return Err(Refusal::bad_args("give at least one of text, gone, url, ref"));
        }
        let timeout = check_timeout(&a)?;
        let arg = json!({ "text": text, "gone": gone, "url": url, "ref": r });
        let start = Instant::now();
        loop {
            match ctx.eval(js::CHECK, &arg).await {
                Ok(v) if v["pass"] == true || start.elapsed() >= timeout => {
                    let waited = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    return Ok(json!({
                        "pass": v["pass"],
                        "conditions": v["conditions"],
                        "url": v["url"],
                        "waited_ms": waited,
                    })
                    .into());
                }
                Ok(_) => {}
                // The document is being replaced; look again.
                Err(e) if matches!(e.class, Class::PageError | Class::Browser) => {
                    if start.elapsed() >= timeout {
                        return Err(Refusal::new(
                            Class::Timeout,
                            format!("the page kept reloading: {}", e.reason),
                            "call check again",
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout(a: Value) -> Result<Duration, Refusal> {
        check_timeout(&args(a, &["timeout_ms"])?)
    }

    #[test]
    fn check_limits_its_wait() {
        assert_eq!(timeout(json!({})).unwrap(), Duration::from_secs(5));
        assert_eq!(timeout(json!({ "timeout_ms": 0 })).unwrap(), Duration::ZERO);
        assert_eq!(timeout(json!({ "timeout_ms": 120_000 })).unwrap(), LONGEST_CHECK);
        assert_eq!(timeout(json!({ "timeout_ms": "5" })).unwrap(), Duration::from_millis(5));
        for bad in [
            json!({ "timeout_ms": 120_001 }),
            json!({ "timeout_ms": "120001" }),
            json!({ "timeout_ms": "soon" }),
            json!({ "timeout_ms": -1 }),
        ] {
            assert_eq!(timeout(bad.clone()).unwrap_err().class, Class::BadArgs, "{bad}");
        }
    }
}
