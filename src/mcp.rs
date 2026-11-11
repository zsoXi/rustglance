//! MCP over stdio (newline-delimited JSON-RPC 2.0) and the wrapper every
//! tool call goes through: human takeover, tab recovery, write report,
//! timing and the journal.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::ctx::Ctx;
use crate::journal::Journal;
use crate::refusal::{Class, Refusal};
use crate::util::now;

const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Browser tools. Start with tree: it lists interactive elements with refs \
(e12), headings and text. Refs stay valid until the page reloads. Every action waits for the page \
to settle. A refused call returns error, reason and next: do what next says. Writes to the site \
(non-GET requests) from the driven tab are blocked unless the operator allowed them; a blocked \
write shows up in blocked_writes. Use get to read site data as JSON.";

pub type OpFuture<'a> = Pin<Box<dyn Future<Output = Result<Output, Refusal>> + Send + 'a>>;

/// One tool.
pub struct OpDef {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema of the arguments.
    pub schema: fn() -> Value,
    /// Changes data on the site; `replay` skips it unless `--writes`.
    pub writes: bool,
    pub run: for<'a> fn(&'a Ctx, Value) -> OpFuture<'a>,
}

/// A tool result: a flat JSON object, an optional text block kept out of
/// the JSON so it reads as plain text, and an optional base64 PNG.
#[derive(Default)]
pub struct Output {
    pub data: Map<String, Value>,
    pub text: Option<String>,
    pub image: Option<String>,
}

impl From<Value> for Output {
    fn from(v: Value) -> Self {
        let data = match v {
            Value::Object(m) => m,
            other => Map::from_iter([("value".to_string(), other)]),
        };
        Self {
            data,
            ..Self::default()
        }
    }
}

/// Runs one tool call with everything around it.
pub async fn run_op(ctx: &Ctx, journal: Option<&Journal>, op: &OpDef, mut args: Value) -> Result<Output, Refusal> {
    let t = now();
    let start = Instant::now();
    let res = if ctx.human.load(Ordering::Acquire) {
        Err(Refusal::new(
            Class::HumanControl,
            "a human has taken over the browser in the live view",
            "wait, then repeat the call",
        ))
    } else {
        (op.run)(ctx, args.clone()).await
    };
    let res = match res {
        Err(r) if r.class == Class::Wedged => Err(match ctx.recover().await {
            Ok((url, true)) => Refusal::new(
                Class::Wedged,
                format!("{}; the tab was replaced with a new one at {url}", r.reason),
                "check with tree or get whether the call took effect; repeat it only if it did not",
            ),
            Ok((url, false)) => Refusal::new(
                Class::Wedged,
                format!("{}; the tab at {url} was replaced with a blank one", r.reason),
                format!("open {url} again, then repeat the call"),
            ),
            Err(e) => Refusal::new(
                Class::Browser,
                format!("{}; replacing the tab failed: {e}", r.reason),
                "restart rustglance",
            ),
        }),
        other => other,
    };
    // Includes writes blocked since the last call: pages often save after a click has settled.
    let blocked = ctx.take_blocked();
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if matches!(&res, Err(r) if r.class == Class::Password) {
        hide_typed(&mut args);
    }
    let mut entry = journal_entry(&t, op.name, args, ms, res.is_ok(), &blocked);
    let mut res = match res {
        Ok(mut out) => {
            out.data.insert("t".to_string(), json!(t));
            out.data.insert("ms".to_string(), json!(ms));
            if !blocked.is_empty() {
                out.data.insert("blocked_writes".to_string(), json!(blocked));
            }
            entry["result"] = Value::Object(out.data.clone());
            hide_secrets(&mut entry["result"]);
            if let Some(text) = &out.text {
                entry["text"] = json!(text);
            }
            Ok(out)
        }
        Err(mut r) => {
            if !blocked.is_empty() {
                r.reason = format!("{}; writes blocked meanwhile: {}", r.reason, blocked.join(", "));
            }
            entry["refusal"] = json!(r);
            Err(r)
        }
    };
    if let Some(j) = journal {
        if let Err(e) = j.write(&entry) {
            eprintln!("rustglance: cannot write journal {:?}: {e}", j.path());
            if let Ok(out) = &mut res {
                out.data.insert("journal_error".to_string(), json!(e.to_string()));
            }
        }
    }
    res
}

/// Blanks what an agent tried to type into a password field, so it stays out of the journal.
fn hide_typed(args: &mut Value) {
    for k in ["text", "key"] {
        if let Some(v) = args.get_mut(k) {
            *v = json!("<hidden>");
        }
    }
}

/// Parts of a key name that mark a secret in a site's response.
const SECRET_KEYS: [&str; 8] = [
    "csrf",
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
    "api_key",
    "apikey",
];

/// Blanks secrets in the journal's copy of a result. Numbers and booleans stay,
/// so counters such as `input_tokens` are kept.
fn hide_secrets(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                let key = k.to_ascii_lowercase();
                let carries_data = matches!(val, Value::String(_) | Value::Array(_) | Value::Object(_));
                if carries_data && SECRET_KEYS.iter().any(|s| key.contains(s)) {
                    *val = json!("<hidden>");
                } else {
                    hide_secrets(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(hide_secrets),
        _ => {}
    }
}

/// The journal line of one call before its result or refusal is added. Blocked
/// writes sit at the top, so `report` finds them for refused calls too.
fn journal_entry(t: &str, op: &str, args: Value, ms: u64, ok: bool, blocked: &[String]) -> Value {
    let mut entry = json!({ "t": t, "op": op, "args": args, "ms": ms, "ok": ok });
    if !blocked.is_empty() {
        entry["blocked_writes"] = json!(blocked);
    }
    entry
}

/// Serves MCP on stdin and stdout until stdin closes. Calls run one at a time.
pub async fn serve(ctx: &Ctx, ops: &[OpDef], journal: &Journal) -> std::io::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(ctx, ops, journal, msg).await,
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = reply {
            let mut s = r.to_string();
            s.push('\n');
            out.write_all(s.as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

pub fn tool_list(ops: &[OpDef]) -> Value {
    let tools: Vec<Value> = ops
        .iter()
        .map(|o| json!({ "name": o.name, "description": o.description, "inputSchema": (o.schema)() }))
        .collect();
    json!({ "tools": tools })
}

async fn handle(ctx: &Ctx, ops: &[OpDef], journal: &Journal, msg: Value) -> Option<Value> {
    let Value::Object(mut msg) = msg else {
        return Some(error(Value::Null, -32600, "expected a JSON-RPC object"));
    };
    // Notifications (no id) and answers to requests (no method) get no reply.
    let id = msg.remove("id")?;
    let method = msg.get("method").and_then(Value::as_str)?.to_string();
    let params = msg.remove("params").unwrap_or_else(|| json!({}));
    let result = match method.as_str() {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOLS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOLS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rustglance", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => tool_list(ops),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let Some(op) = ops.iter().find(|o| o.name == name) else {
                return Some(error(id, -32602, &format!("unknown tool {name:?}")));
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            tool_result(run_op(ctx, Some(journal), op, args).await)
        }
        _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tool_result(res: Result<Output, Refusal>) -> Value {
    match res {
        Ok(out) => {
            let mut content = vec![json!({ "type": "text", "text": Value::Object(out.data).to_string() })];
            if let Some(t) = out.text {
                content.push(json!({ "type": "text", "text": t }));
            }
            if let Some(png) = out.image {
                content.push(json!({ "type": "image", "data": png, "mimeType": "image/png" }));
            }
            json!({ "content": content, "isError": false })
        }
        Err(r) => json!({ "content": [{ "type": "text", "text": json!(r).to_string() }], "isError": true }),
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_typed_blanks_only_what_was_typed() {
        let mut args = json!({ "ref": "e1", "text": "secret", "key": "s", "submit": true });
        hide_typed(&mut args);
        assert_eq!(
            args,
            json!({ "ref": "e1", "text": "<hidden>", "key": "<hidden>", "submit": true })
        );
        let mut none = Value::Null;
        hide_typed(&mut none);
        assert_eq!(none, Value::Null);
    }

    #[test]
    fn report_lists_blocked_writes_of_kept_and_refused_calls() {
        let path = std::env::temp_dir().join(format!("rustglance-test-{}-journal.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let journal = Journal::open(&path).unwrap();
        let mut kept = journal_entry(
            "t1",
            "tap",
            json!({ "ref": "e1" }),
            5,
            true,
            &["POST https://a.test/save".into()],
        );
        kept["result"] = json!({ "settled": true });
        let mut refused = journal_entry("t2", "type", json!({}), 5, false, &["PUT https://a.test/item".into()]);
        refused["refusal"] = json!(Refusal::bad_args("text is missing"));
        let quiet = journal_entry("t3", "tree", json!({}), 1, true, &[]);
        for e in [&kept, &refused, &quiet] {
            journal.write(e).unwrap();
        }
        let md = crate::report::render(&path);
        std::fs::remove_file(&path).unwrap();
        let md = md.unwrap();
        assert!(md.contains("| t1 | tap | POST https://a.test/save |"), "{md}");
        assert!(md.contains("| t2 | type | PUT https://a.test/item |"), "{md}");
        assert!(quiet.get("blocked_writes").is_none());
    }
}
