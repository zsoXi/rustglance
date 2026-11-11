//! Turns journals into replay scripts.
//!
//! A script is JSONL, one step per line: `{"op": .., "args": .., "expect": ..}`.
//! `expect.clicked` holds what a tap or download clicked in the recording;
//! replay stops when it clicks something else. With two runs of the same
//! task, string arguments that differ become `{{p1}}`, `{{p2}}`... and the
//! seen values are printed so a human can rename the parameters.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::cli::BoxError;
use crate::journal;

pub struct Built {
    pub text: String,
    pub notes: Vec<String>,
}

/// One script step from a journal entry or script line; `None` for an
/// entry the recording refused (it did nothing, so there is nothing to repeat).
pub fn step(entry: &Value) -> Option<Value> {
    if entry["ok"] == false {
        return None;
    }
    let op = entry["op"].as_str()?;
    let mut s = Map::new();
    s.insert("op".to_string(), json!(op));
    s.insert(
        "args".to_string(),
        entry.get("args").cloned().unwrap_or_else(|| json!({})),
    );
    let expect = match entry.get("expect") {
        Some(e) => Some(e.clone()),
        None => entry["result"].get("clicked").map(|c| json!({ "clicked": c })),
    };
    if let Some(e) = expect {
        s.insert("expect".to_string(), e);
    }
    Some(Value::Object(s))
}

fn is_ref(s: &str) -> bool {
    s.strip_prefix('e')
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Replaces differing strings in `a` with parameters. Refs are kept: replay
/// assigns them in the same order as the recording did.
fn diff(a: &mut Value, b: &Value, params: &mut Vec<(String, String)>) {
    match (a, b) {
        (Value::Object(ma), Value::Object(mb)) => {
            for (k, va) in ma.iter_mut() {
                if let Some(vb) = mb.get(k) {
                    diff(va, vb, params);
                }
            }
        }
        (Value::Array(aa), Value::Array(ab)) if aa.len() == ab.len() => {
            for (x, y) in aa.iter_mut().zip(ab) {
                diff(x, y, params);
            }
        }
        (Value::String(sa), Value::String(sb)) if sa != sb && !is_ref(sa) => {
            let i = match params.iter().position(|(x, y)| x == sa && y == sb) {
                Some(i) => i,
                None => {
                    params.push((sa.clone(), sb.clone()));
                    params.len() - 1
                }
            };
            *sa = format!("{{{{p{}}}}}", i + 1);
        }
        _ => {}
    }
}

pub fn build(files: &[PathBuf]) -> Result<Built, BoxError> {
    let mut runs = Vec::new();
    for f in files {
        let steps: Vec<Value> = journal::read(f)?.iter().filter_map(step).collect();
        runs.push(steps);
    }
    let Some(mut out) = runs.first().cloned() else {
        return Err("no journal given".into());
    };
    let mut notes = Vec::new();
    if let Some(second) = runs.get(1) {
        let first_diff = out.iter().zip(second).position(|(a, b)| a["op"] != b["op"]);
        if out.len() != second.len() || first_diff.is_some() {
            let at = first_diff.map_or(String::new(), |i| format!(", first different step {}", i + 1));
            return Err(format!(
                "the runs do not match ({} and {} steps{at}); record the same task twice",
                out.len(),
                second.len()
            )
            .into());
        }
        let mut params = Vec::new();
        for (a, b) in out.iter_mut().zip(second) {
            diff(&mut a["args"], &b["args"], &mut params);
            // A click target that changed between runs cannot be checked.
            if a.get("expect") != b.get("expect") {
                if let Value::Object(m) = a {
                    m.remove("expect");
                }
            }
        }
        for (i, (x, y)) in params.iter().enumerate() {
            notes.push(format!("p{}: run 1 {x:?}, run 2 {y:?}", i + 1));
        }
        if params.is_empty() {
            notes.push("no parameters: both runs used the same values".to_string());
        }
    }
    let mut text = String::new();
    for s in &out {
        text.push_str(&s.to_string());
        text.push('\n');
    }
    Ok(Built { text, notes })
}
