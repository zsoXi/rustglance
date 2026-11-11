//! Tools that move files or data: upload, download, screenshot, logs, get.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::{
    LOGIN_NEXT, args, click, logged_out, opt_bool, opt_ref, opt_str, opt_u64, req_str, resolve, target, web_url,
};
use crate::cdp::CdpError;
use crate::ctx::{Ctx, Handle};
use crate::js;
use crate::mcp::{OpFuture, Output};
use crate::refusal::{Class, Refusal};
use crate::util::{safe_name, sha256_file, sha256_hex, stamp};

/// Retry-After values above this are not waited out; the call is refused.
const LONGEST_RETRY_WAIT: Duration = Duration::from_secs(60);

async fn hash_file(path: PathBuf) -> Result<(u64, String), Refusal> {
    let shown = path.display().to_string();
    tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|e| Refusal::new(Class::Io, format!("hashing {shown} stopped: {e}"), "repeat the call"))?
        .map_err(|e| Refusal::io(format!("reading {shown}"), &e))
}

/// The file inside the upload folder; anything else is refused.
fn upload_path(dir: Option<&Path>, file: &str) -> Result<PathBuf, Refusal> {
    let Some(dir) = dir else {
        return Err(Refusal::new(
            Class::OutsideFolder,
            "uploads are off: the config has no upload_dir",
            "ask a human to set upload_dir and put the file there",
        ));
    };
    // `dir` is canonical (checked at config load), so a prefix test after
    // canonicalizing catches `..` and symlinks that lead out.
    let p = dir.join(file).canonicalize().map_err(|e| {
        Refusal::new(
            Class::NotFound,
            format!("{file:?} is not in the upload folder: {e}"),
            "give the name of a file a human put there",
        )
    })?;
    if !p.starts_with(dir) {
        return Err(Refusal::new(
            Class::OutsideFolder,
            format!("{file:?} points outside the upload folder"),
            "give a file name inside the upload folder",
        ));
    }
    if !p.is_file() {
        return Err(Refusal::new(
            Class::NotFound,
            format!("{file:?} is not a file"),
            "give a file name, not a folder",
        ));
    }
    if p.to_str().is_none() {
        return Err(Refusal::bad_args(format!(
            "{file:?} resolves to a path that is not UTF-8"
        )));
    }
    Ok(p)
}

pub fn upload(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["file", "ref"])?;
        let file = req_str(&a, "file")?;
        let r = opt_ref(&a, "ref")?;
        let path = upload_path(ctx.cfg.upload_dir.as_deref(), file)?;
        let start = Instant::now();
        let id = match ctx.eval_handle(js::FILE_INPUT, &json!({ "ref": r })).await? {
            Handle::Object(id) => id,
            Handle::Value(v) => {
                return Err(match v.as_str() {
                    Some("stale") => Refusal::stale(r.as_deref().unwrap_or("")),
                    Some("many") => Refusal::bad_args(
                        "the page has several file inputs; give ref of the upload button or its input",
                    ),
                    _ => Refusal::new(
                        Class::NotFound,
                        "no file input found",
                        "give ref of the upload button, or open the upload dialog first",
                    ),
                });
            }
        };
        let set = ctx
            .page_call(
                "DOM.setFileInputFiles",
                json!({ "files": [path.to_string_lossy()], "objectId": id }),
            )
            .await;
        ctx.release(&id).await;
        set?;
        let settled = ctx.settle(start).await?;
        let (size, sha256) = hash_file(path).await?;
        Ok(json!({ "file": file, "size": size, "sha256": sha256, "settled": settled }).into())
    })
}

pub fn download(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["ref", "text", "url"])?;
        let url = opt_str(&a, "url")?;
        let t = match url {
            Some(_) if a.contains_key("ref") || a.contains_key("text") => {
                return Err(Refusal::bad_args("give url, or ref/text to click, not both"));
            }
            Some(u) => {
                web_url(u)?;
                None
            }
            None => Some(target(&a)?),
        };
        // Subscribe first: the download may begin before the click returns.
        let mut events = ctx.cdp.subscribe();
        match (&t, url) {
            (Some(t), _) => {
                click(ctx, t).await?;
            }
            (None, Some(u)) => {
                let u = resolve(ctx, u).await?;
                ctx.eval(js::DOWNLOAD_URL, &json!({ "url": u })).await?;
            }
            (None, None) => return Err(Refusal::bad_args("give ref, text or url")),
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ctx.cfg.download_ms);
        let mut begun: Option<(String, String, String)> = None;
        let (guid, name, src) = loop {
            let ev = match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(ev)) => ev,
                Ok(None) => return Err(CdpError::Closed.into()),
                Err(_) => {
                    let reason = if begun.is_none() {
                        "no download started"
                    } else {
                        "the download did not finish in time"
                    };
                    return Err(Refusal::new(
                        Class::Timeout,
                        reason,
                        "check logs; the link may open a page instead of a file",
                    ));
                }
            };
            let p = &ev.params;
            match ev.method.as_str() {
                "Browser.downloadWillBegin" if begun.is_none() => {
                    let field = |k: &str| p[k].as_str().unwrap_or("").to_string();
                    begun = Some((field("guid"), field("suggestedFilename"), field("url")));
                }
                "Browser.downloadProgress" => {
                    let Some((g, n, s)) = &begun else { continue };
                    if p["guid"].as_str() != Some(g.as_str()) {
                        continue;
                    }
                    match p["state"].as_str() {
                        Some("completed") => break (g.clone(), n.clone(), s.clone()),
                        Some("canceled") => {
                            return Err(Refusal::new(
                                Class::Browser,
                                "the browser canceled the download",
                                "check logs, then try again",
                            ));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        };
        let dir = ctx.cfg.evidence.join("downloads");
        let from = dir.join(safe_name(&guid));
        let to = dir.join(format!("{}-{}", stamp(), safe_name(&name)));
        tokio::fs::rename(&from, &to)
            .await
            .map_err(|e| Refusal::io(format!("moving {}", from.display()), &e))?;
        let file = to.to_string_lossy().into_owned();
        let (size, sha256) = hash_file(to).await?;
        Ok(json!({ "file": file, "name": name, "size": size, "sha256": sha256, "url": src }).into())
    })
}

pub fn screenshot(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["look", "full", "ref", "name"])?;
        let look = opt_bool(&a, "look")?.unwrap_or(false);
        let full = opt_bool(&a, "full")?.unwrap_or(false);
        let r = opt_ref(&a, "ref")?;
        let name = opt_str(&a, "name")?;
        let mut p = json!({ "format": "png" });
        if let Some(r) = &r {
            let rect = ctx.eval(js::RECT, &json!({ "ref": r })).await?;
            if rect.is_null() {
                return Err(Refusal::stale(r));
            }
            p["clip"] =
                json!({ "x": rect["x"], "y": rect["y"], "width": rect["width"], "height": rect["height"], "scale": 1 });
            p["captureBeyondViewport"] = json!(true);
        } else if full {
            let m = ctx.page_call("Page.getLayoutMetrics", json!({})).await?;
            let s = &m["cssContentSize"];
            p["clip"] = json!({ "x": 0, "y": 0, "width": s["width"], "height": s["height"], "scale": 1 });
            p["captureBeyondViewport"] = json!(true);
        }
        let shot = ctx.page_call("Page.captureScreenshot", p).await?;
        let data = shot["data"].as_str().unwrap_or("");
        let bytes = STANDARD.decode(data).map_err(|e| {
            Refusal::new(
                Class::Browser,
                format!("unreadable screenshot data: {e}"),
                "repeat the call",
            )
        })?;
        if bytes.is_empty() {
            return Err(Refusal::new(
                Class::Browser,
                "the screenshot came back empty",
                "repeat the call",
            ));
        }
        let tag = name.map(|n| format!("-{}", safe_name(n))).unwrap_or_default();
        let file = ctx.cfg.evidence.join("shots").join(format!("{}{tag}.png", stamp()));
        tokio::fs::write(&file, &bytes)
            .await
            .map_err(|e| Refusal::io(format!("writing {}", file.display()), &e))?;
        let mut out =
            Output::from(json!({ "file": file.to_string_lossy(), "size": bytes.len(), "sha256": sha256_hex(&bytes) }));
        if look {
            out.image = Some(data.to_string());
        }
        Ok(out)
    })
}

pub fn logs(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["since"])?;
        let since = usize::try_from(opt_u64(&a, "since")?.unwrap_or(0)).unwrap_or(usize::MAX);
        let (entries, next) = ctx.logs_since(since);
        Ok(json!({ "count": entries.len(), "next": next, "entries": entries }).into())
    })
}

/// The lists in a body: the body itself, or the lists among its top-level values.
fn lists(body: &mut Value) -> Vec<(String, &mut Vec<Value>)> {
    match body {
        Value::Array(items) => vec![(String::new(), items)],
        Value::Object(m) => m
            .iter_mut()
            .filter_map(|(k, v)| match v {
                Value::Array(items) => Some((k.clone(), items)),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Keeps only `fields` in each object of the body's lists.
fn keep_fields(body: &mut Value, fields: &[String]) {
    for (_, items) in lists(body) {
        for item in items.iter_mut() {
            if let Value::Object(m) = item {
                m.retain(|k, _| fields.contains(k));
            }
        }
    }
}

fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(usize::MAX, |b| b.len())
}

/// Shortens the body's lists until its JSON fits in `max` bytes, and returns
/// how many items each shortened list kept and had ("" names a body that is a
/// list). A body that does not fit even with empty lists becomes null.
// Halving the longest list each round is simple, but up to half the room can
// stay unused; a search for the largest fitting length would fill it.
fn cut(body: &mut Value, max: usize) -> Map<String, Value> {
    let total: Vec<(String, usize)> = lists(body).into_iter().map(|(k, l)| (k, l.len())).collect();
    while json_len(body) > max {
        let shortened = match lists(body)
            .into_iter()
            .filter(|(_, l)| !l.is_empty())
            .max_by_key(|(_, l)| l.len())
        {
            Some((_, longest)) => {
                let half = longest.len() / 2;
                longest.truncate(half);
                true
            }
            None => false,
        };
        if !shortened {
            *body = Value::Null;
            break;
        }
    }
    let kept: Vec<(String, usize)> = lists(body).into_iter().map(|(k, l)| (k, l.len())).collect();
    let mut out = Map::new();
    for (name, had) in total {
        let now = kept.iter().find(|(k, _)| *k == name).map_or(0, |(_, n)| *n);
        if now < had {
            out.insert(name, json!({ "kept": now, "total": had }));
        }
    }
    out
}

/// The `fields` argument of get: a list of key names, or none.
fn opt_fields(a: &Map<String, Value>) -> Result<Option<Vec<String>>, Refusal> {
    let bad = || Refusal::bad_args("fields must be a list of key names like [\"id\", \"name\"]");
    match a.get("fields") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(fs)) => fs
            .iter()
            .map(|f| f.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .map(Some)
            .ok_or_else(bad),
        Some(_) => Err(bad()),
    }
}

/// The GET result for the agent: `fields` applied, then a body over
/// `get_max_bytes` cut to fit with the whole saved to evidence.
async fn hand_over(
    ctx: &Ctx,
    path: &str,
    status: u64,
    url: Value,
    mut body: Value,
    fields: Option<&[String]>,
) -> Result<Output, Refusal> {
    if let Some(f) = fields {
        keep_fields(&mut body, f);
    }
    let max = ctx.cfg.get_max_bytes;
    let bytes = json_len(&body);
    if max == 0 || bytes <= max {
        return Ok(json!({ "status": status, "url": url, "body": body }).into());
    }
    let whole = serde_json::to_vec_pretty(&body)
        .map_err(|e| Refusal::new(Class::Io, format!("saving the body of {path}: {e}"), "repeat the call"))?;
    let file =
        ctx.cfg
            .evidence
            .join("data")
            .join(format!("{}-{}.json", stamp(), safe_name(path.trim_start_matches('/'))));
    tokio::fs::write(&file, &whole)
        .await
        .map_err(|e| Refusal::io(format!("writing {}", file.display()), &e))?;
    let truncated = cut(&mut body, max);
    Ok(json!({
        "status": status,
        "url": url,
        "body": body,
        "truncated": truncated,
        "bytes": bytes,
        "file": file.to_string_lossy(),
    })
    .into())
}

pub fn get(ctx: &Ctx, a: Value) -> OpFuture<'_> {
    Box::pin(async move {
        let a = args(a, &["path", "query", "fields"])?;
        let path = req_str(&a, "path")?;
        web_url(path)?;
        let query = match a.get("query") {
            None | Some(Value::Null) => json!({}),
            Some(q @ Value::Object(_)) => q.clone(),
            Some(_) => return Err(Refusal::bad_args("query must be an object like {\"limit\": 20}")),
        };
        let fields = opt_fields(&a)?;
        // The fetch is aborted before the CDP command would time out, so a
        // slow site is not mistaken for a stuck page.
        let ms = ctx.cfg.cdp_timeout_ms.saturating_sub(1_000);
        let arg = json!({ "path": path, "query": query, "base": ctx.cfg.start_url, "ms": ms });
        let mut wait = Duration::from_secs(1);
        let mut attempt = 0;
        loop {
            let mut r = ctx.eval(js::GET, &arg).await?;
            if let Some(f) = r["failed"].as_str() {
                return Err(if f == "timeout" {
                    Refusal::new(
                        Class::Timeout,
                        format!("GET {path} got no answer within {ms} ms"),
                        "repeat later; the site is slow",
                    )
                } else {
                    Refusal::new(
                        Class::Browser,
                        format!("GET {path} failed: {f}"),
                        "check the path; open the site first",
                    )
                });
            }
            let status = r["status"].as_u64().unwrap_or(0);
            if status == 401 {
                return Err(Refusal::new(
                    Class::LoggedOut,
                    format!("GET {path} answered 401: the session is logged out"),
                    LOGIN_NEXT,
                ));
            }
            if status != 429 && status != 503 {
                logged_out(ctx, r["url"].as_str().unwrap_or(""))?;
                let body = r.get_mut("body").map(Value::take).unwrap_or_default();
                let url = r.get_mut("url").map(Value::take).unwrap_or_default();
                return hand_over(ctx, path, status, url, body, fields.as_deref()).await;
            }
            let asked = r["retry_after"]
                .as_str()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let pause = asked.unwrap_or(wait);
            if attempt >= ctx.cfg.retries || pause > LONGEST_RETRY_WAIT {
                return Err(Refusal::new(
                    Class::RateLimited,
                    format!(
                        "GET {path} answered {status} after {} tries (next wait would be {} s)",
                        attempt.saturating_add(1),
                        pause.as_secs()
                    ),
                    "wait, then repeat the call",
                ));
            }
            tokio::time::sleep(pause).await;
            wait = wait.saturating_mul(2);
            attempt += 1;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argument checks get runs before it touches the page.
    fn get_fields(a: Value) -> Result<Option<Vec<String>>, Refusal> {
        opt_fields(&args(a, &["path", "query", "fields"])?)
    }

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn get_takes_fields_as_a_list_of_names() {
        assert_eq!(get_fields(json!({ "path": "/x" })).unwrap(), None);
        assert_eq!(get_fields(json!({ "path": "/x", "fields": null })).unwrap(), None);
        assert_eq!(
            get_fields(json!({ "path": "/x", "fields": ["id", "name"] })).unwrap(),
            Some(names(&["id", "name"]))
        );
    }

    #[test]
    fn get_refuses_bad_arguments() {
        for bad in [
            json!({ "path": "/x", "fields": "id" }),
            json!({ "path": "/x", "fields": ["id", 1] }),
            json!({ "path": "/x", "field": ["id"] }),
            json!(["/x"]),
        ] {
            assert_eq!(get_fields(bad.clone()).unwrap_err().class, Class::BadArgs, "{bad}");
        }
    }

    #[test]
    fn keep_fields_trims_objects_in_every_list() {
        let mut body = json!({
            "items": [{ "id": 1, "name": "a", "secret": "x" }, 7],
            "other": [{ "id": 2, "note": "n" }],
            "total": 2,
        });
        keep_fields(&mut body, &names(&["id", "name"]));
        assert_eq!(
            body,
            json!({
                "items": [{ "id": 1, "name": "a" }, 7],
                "other": [{ "id": 2 }],
                "total": 2,
            })
        );
        let mut list = json!([{ "id": 1, "x": 2 }]);
        keep_fields(&mut list, &names(&["id"]));
        assert_eq!(list, json!([{ "id": 1 }]));
    }

    #[test]
    fn cut_leaves_a_body_that_fits() {
        let mut body = json!({ "items": [1, 2, 3] });
        let before = body.clone();
        assert!(cut(&mut body, json_len(&before)).is_empty());
        assert_eq!(body, before);
    }

    #[test]
    fn cut_halves_the_longest_list_and_keeps_its_head() {
        let mut body = json!({ "long": (0..100).collect::<Vec<_>>(), "short": [1, 2], "total": 102 });
        let truncated = cut(&mut body, 120);
        assert!(json_len(&body) <= 120);
        assert_eq!(
            Value::Object(truncated),
            json!({ "long": { "kept": 25, "total": 100 } })
        );
        assert_eq!(body["long"], json!((0..25).collect::<Vec<_>>()));
        assert_eq!(body["short"], json!([1, 2]));
        assert_eq!(body["total"], json!(102));
    }

    #[test]
    fn cut_names_a_list_body_with_an_empty_key() {
        let mut body = json!((0..50).collect::<Vec<_>>());
        let truncated = cut(&mut body, 20);
        assert!(json_len(&body) <= 20);
        assert_eq!(Value::Object(truncated), json!({ "": { "kept": 6, "total": 50 } }));
    }

    #[test]
    fn cut_nulls_a_body_that_never_fits() {
        let mut body = json!({ "name": "a long name that does not fit", "items": [1, 2] });
        let truncated = cut(&mut body, 10);
        assert_eq!(body, Value::Null);
        assert_eq!(Value::Object(truncated), json!({ "items": { "kept": 0, "total": 2 } }));

        let mut scalar = json!("a long text that does not fit");
        assert!(cut(&mut scalar, 10).is_empty());
        assert_eq!(scalar, Value::Null);
    }

    #[test]
    fn upload_path_stays_in_the_folder() {
        let root = std::env::temp_dir().join(format!("rustglance-test-{}-upload", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let up = root.join("up");
        std::fs::create_dir_all(up.join("sub")).unwrap();
        std::fs::write(up.join("a.txt"), "a").unwrap();
        let outside = root.join("outside.txt");
        std::fs::write(&outside, "o").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, up.join("link.txt")).unwrap();
        let dir = up.canonicalize().unwrap();
        let class = |f: &str| upload_path(Some(&dir), f).map(drop).map_err(|r| r.class);

        let inside = upload_path(Some(&dir), "a.txt").map_err(|r| r.class);
        let up_and_out = class("../outside.txt");
        let absolute = class(outside.to_str().unwrap());
        #[cfg(unix)]
        let link = class("link.txt");
        let missing = class("missing.txt");
        let folder = class("sub");
        let off = upload_path(None, "a.txt").map(drop).map_err(|r| r.class);
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(inside, Ok(dir.join("a.txt")));
        assert_eq!(up_and_out, Err(Class::OutsideFolder));
        assert_eq!(absolute, Err(Class::OutsideFolder));
        #[cfg(unix)]
        assert_eq!(link, Err(Class::OutsideFolder));
        assert_eq!(missing, Err(Class::NotFound));
        assert_eq!(folder, Err(Class::NotFound));
        assert_eq!(off, Err(Class::OutsideFolder));
    }
}
