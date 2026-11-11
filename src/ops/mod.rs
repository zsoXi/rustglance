//! The ten core tools and the argument helpers extensions can reuse.

mod actions;
mod files;

use serde_json::{Map, Value, json};

use crate::ctx::Ctx;
use crate::mcp::OpDef;
use crate::refusal::{Class, Refusal};

pub use actions::{Hit, Target, click, resolve, target};

/// Next step for every "logged out" refusal.
pub const LOGIN_NEXT: &str = "stop: a human must log in again (rustglance login); agents never log in";

pub fn core() -> Vec<OpDef> {
    vec![
        OpDef {
            name: "open",
            description: "Go to a URL and wait until the page settles. url is absolute, or a path like /tasks \
resolved against the current page (else the configured start_url). Optional wait_for waits for task-specific \
visible content stable for 200ms instead of network quiet. Returns final url/title and readiness; timeout \
does not imply navigation did not happen.",
            schema: || {
                schema(
                    json!({ "url": { "type": "string", "description": "https://... or /path" }, "wait_for": crate::ready::schema() }),
                    &["url"],
                )
            },
            writes: false,
            run: actions::open,
        },
        OpDef {
            name: "tree",
            description: "Outline of the page: interactive elements with refs like e12 (role, name, state, \
value, link target), headings, text, and groups (dialog, nav, aside, main, form, list, table). Refs stay \
valid until the page reloads. find keeps only lines containing that text; within (a ref) shows one element.",
            schema: || {
                schema(
                    json!({
                        "find": { "type": "string", "description": "keep lines containing this text" },
                        "within": { "type": "string", "description": "ref of the element to outline, e.g. e12" }
                    }),
                    &[],
                )
            },
            writes: false,
            run: actions::tree,
        },
        OpDef {
            name: "tap",
            description: "Click an element by ref (from tree) or by its visible text (button or link label). \
Scrolls it into view, refuses when another element covers it (and names that element), clicks like a \
mouse, then waits for the page to settle. Optional wait_for waits for destination-specific content \
instead of global network quiet; choose a condition not already true before clicking. A timeout \
does not imply the click did not happen: inspect before repeating.",
            schema: || {
                schema(
                    json!({
                        "ref": { "type": "string", "description": "e.g. e12, from tree" },
                        "text": { "type": "string", "description": "visible label, when there is no ref" },
                        "wait_for": crate::ready::schema()
                    }),
                    &[],
                )
            },
            writes: false,
            run: actions::tap,
        },
        OpDef {
            name: "type",
            description: "Type text into the field ref: replaces its content unless clear is false; for a \
select it picks the option with that text. submit:true presses Enter afterwards. Or press one key \
(Enter, Escape, Tab, arrows, Backspace...) on ref or on the focused element.",
            schema: || {
                schema(
                    json!({
                        "ref": { "type": "string", "description": "field from tree, e.g. e5" },
                        "text": { "type": "string" },
                        "key": { "type": "string", "description": "one key instead of text, e.g. Escape" },
                        "clear": { "type": "boolean", "description": "default true" },
                        "submit": { "type": "boolean", "description": "press Enter after typing" }
                    }),
                    &[],
                )
            },
            writes: false,
            run: actions::type_,
        },
        OpDef {
            name: "upload",
            description: "Put a file from the configured upload folder into a file input. file is a name \
inside that folder. ref is the file input or the upload button or label around it; it may be left out \
when the page has exactly one file input.",
            schema: || {
                schema(
                    json!({
                        "file": { "type": "string", "description": "file name inside the upload folder" },
                        "ref": { "type": "string" }
                    }),
                    &["file"],
                )
            },
            writes: false,
            run: files::upload,
        },
        OpDef {
            name: "download",
            description: "Download by clicking ref (or text), or from url. Waits until the file is complete \
and saves it in the evidence folder. Returns the file path, name, size and sha256.",
            schema: || {
                schema(
                    json!({
                        "ref": { "type": "string" },
                        "text": { "type": "string" },
                        "url": { "type": "string" }
                    }),
                    &[],
                )
            },
            writes: false,
            run: files::download,
        },
        OpDef {
            name: "screenshot",
            description: "Save a PNG of the viewport to the evidence folder (full:true for the whole page, \
ref for one element, name to tag the file). Returns the file path and sha256; look:true also returns \
the image itself.",
            schema: || {
                schema(
                    json!({
                        "look": { "type": "boolean", "description": "return the image too" },
                        "full": { "type": "boolean" },
                        "ref": { "type": "string" },
                        "name": { "type": "string" }
                    }),
                    &[],
                )
            },
            writes: false,
            run: files::screenshot,
        },
        OpDef {
            name: "logs",
            description: "Console messages, page errors, failed requests, HTTP errors (status 400 and up), \
writes (kind write, allowed true: sent; kind blocked, allowed false: stopped), from entry since on \
(default 0). Returns entries and next: pass next as since to get only newer entries.",
            schema: || schema(json!({ "since": { "type": "integer", "minimum": 0 } }), &[]),
            writes: false,
            run: files::logs,
        },
        OpDef {
            name: "get",
            description: "Read site data: HTTP GET path (like /api/tasks) with the browser's login, plus an \
optional query object. Returns status and the JSON body (text when not JSON). fields keeps only those \
keys in each item of the body's lists. A body too large to hand over keeps the first items of its \
lists: truncated gives kept and total per list, file holds the whole body. 401 means logged out; \
429 and 503 are retried.",
            schema: || {
                schema(
                    json!({
                        "path": { "type": "string", "description": "e.g. /api/tasks" },
                        "query": { "type": "object", "description": "e.g. {\"limit\": 20}" },
                        "fields": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "e.g. [\"id\", \"name\"]"
                        }
                    }),
                    &["path"],
                )
            },
            writes: false,
            run: files::get,
        },
        OpDef {
            name: "check",
            description: "Wait up to timeout_ms (default 5000, at most 120000) until every given condition holds: text \
visible on the page, gone (text no longer visible), url contains, ref still on the page. Returns pass \
true or false with each condition.",
            schema: || {
                schema(
                    json!({
                        "text": { "type": "string" },
                        "gone": { "type": "string" },
                        "url": { "type": "string" },
                        "ref": { "type": "string" },
                        "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 120_000 }
                    }),
                    &[],
                )
            },
            writes: false,
            run: actions::check,
        },
    ]
}

/// An object schema that rejects unknown properties.
pub fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The call's arguments as an object, refusing names the tool does not take.
pub fn args(v: Value, known: &[&str]) -> Result<Map<String, Value>, Refusal> {
    let m = match v {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        _ => return Err(Refusal::bad_args("arguments must be an object")),
    };
    if let Some(k) = m.keys().find(|k| !known.contains(&k.as_str())) {
        let takes = if known.is_empty() {
            "nothing".to_string()
        } else {
            known.join(", ")
        };
        return Err(Refusal::bad_args(format!(
            "unknown argument {k:?}; this tool takes: {takes}"
        )));
    }
    Ok(m)
}

pub fn opt_str<'a>(a: &'a Map<String, Value>, k: &str) -> Result<Option<&'a str>, Refusal> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(Refusal::bad_args(format!("{k} must be a string"))),
    }
}

pub fn req_str<'a>(a: &'a Map<String, Value>, k: &str) -> Result<&'a str, Refusal> {
    opt_str(a, k)?.ok_or_else(|| Refusal::bad_args(format!("{k} is required")))
}

/// A boolean; the strings "true" and "false" count too.
pub fn opt_bool(a: &Map<String, Value>, k: &str) -> Result<Option<bool>, Refusal> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::String(s)) if s == "true" => Ok(Some(true)),
        Some(Value::String(s)) if s == "false" => Ok(Some(false)),
        Some(_) => Err(Refusal::bad_args(format!("{k} must be true or false"))),
    }
}

/// A whole number of zero or more; a string of digits counts too.
pub fn opt_u64(a: &Map<String, Value>, k: &str) -> Result<Option<u64>, Refusal> {
    let bad = || Refusal::bad_args(format!("{k} must be a whole number, 0 or more"));
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(bad),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| bad()),
        Some(_) => Err(bad()),
    }
}

/// A ref like `e12`. Also accepts `@e12` and `ref=e12`, which models write.
pub fn opt_ref(a: &Map<String, Value>, k: &str) -> Result<Option<String>, Refusal> {
    let Some(raw) = opt_str(a, k)? else { return Ok(None) };
    let r = raw.trim().trim_start_matches('[').trim_end_matches(']');
    let r = r.strip_prefix("ref=").unwrap_or(r);
    let r = r.strip_prefix('@').unwrap_or(r);
    let digits = r.strip_prefix('e').unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Refusal::bad_args(format!(
            "{k} {raw:?} is not a ref; refs look like e12 and come from tree"
        )));
    }
    Ok(Some(r.to_string()))
}

/// Refuses URLs whose scheme is not http or https (`about:blank` passes).
/// Blocks `file:` (local files), `javascript:` (agent code) and browser pages.
/// A relative URL has no scheme and passes. Browsers drop tabs and newlines
/// inside a URL (`java\tscript:`), so control characters are refused, and
/// callers check again on the URL as the browser parsed it.
pub fn web_url(u: &str) -> Result<(), Refusal> {
    let u = u.trim();
    if u.chars().any(char::is_control) {
        return Err(Refusal::bad_args("the URL contains control characters"));
    }
    if u == "about:blank" {
        return Ok(());
    }
    let scheme = match u.split_once(':') {
        Some((s, _))
            if !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            s
        }
        _ => return Ok(()),
    };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        Ok(())
    } else {
        Err(Refusal::bad_args(format!(
            "only http and https URLs can be opened, not {scheme}:"
        )))
    }
}

/// Refuses when the URL is the configured login page.
pub fn logged_out(ctx: &Ctx, url: &str) -> Result<(), Refusal> {
    match &ctx.cfg.logged_out_url {
        Some(p) if !p.is_empty() && url.contains(p.as_str()) => Err(Refusal::new(
            Class::LoggedOut,
            format!("the site sent the browser to its login page ({url})"),
            LOGIN_NEXT,
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_url_opens_only_web_pages() {
        for u in [
            "https://a.test/x",
            "http://a.test",
            "HTTPS://A.TEST/",
            "/api/items?limit=20",
            "about:blank",
        ] {
            assert!(web_url(u).is_ok(), "{u} should open");
        }
        for u in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<b>x</b>",
            "chrome://settings",
            "https://a.test/\nx",
        ] {
            assert_eq!(web_url(u).unwrap_err().class, Class::BadArgs, "{u} should be refused");
        }
    }
}
