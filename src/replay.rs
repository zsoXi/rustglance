//! Runs a script or journal without a model.
//!
//! Reads only by default: tools marked as writes are skipped and every
//! network write is blocked; `--writes` allows both. Silent while steps
//! pass. At the first failed step it stops and writes a report with a
//! screenshot and the browser log.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::cli::BoxError;
use crate::config::Config;
use crate::ctx::{Ctx, Writes};
use crate::journal::{self, Journal};
use crate::mcp::{OpDef, run_op};
use crate::script;
use crate::util::stamp;

struct Failure {
    step: usize,
    op: String,
    args: Value,
    reason: String,
}

/// Fills `{{name}}` placeholders in every string; names without a value are collected.
fn fill(v: &mut Value, params: &[(String, String)], missing: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            let mut done = String::new();
            while let Some(open) = rest.find("{{") {
                let Some(len) = rest[open + 2..].find("}}") else { break };
                let name = &rest[open + 2..open + 2 + len];
                done.push_str(&rest[..open]);
                match params.iter().find(|(k, _)| k == name) {
                    Some((_, value)) => done.push_str(value),
                    None => {
                        if !missing.iter().any(|m| m == name) {
                            missing.push(name.to_string());
                        }
                        done.push_str(&rest[open..open + 4 + len]);
                    }
                }
                rest = &rest[open + 4 + len..];
            }
            done.push_str(rest);
            *s = done;
        }
        Value::Array(a) => a.iter_mut().for_each(|x| fill(x, params, missing)),
        Value::Object(m) => m.values_mut().for_each(|x| fill(x, params, missing)),
        _ => {}
    }
}

pub async fn run(
    cfg: Config,
    ops: &[OpDef],
    file: &Path,
    writes: bool,
    params: &[(String, String)],
) -> Result<ExitCode, BoxError> {
    let mut steps: Vec<Value> = journal::read(file)?.iter().filter_map(script::step).collect();
    let mut missing = Vec::new();
    for s in &mut steps {
        fill(&mut s["args"], params, &mut missing);
    }
    if !missing.is_empty() {
        return Err(format!("the script needs --param for: {}", missing.join(", ")).into());
    }
    if let Some(s) = steps.iter().find(|s| !ops.iter().any(|o| s["op"] == o.name)) {
        return Err(format!("the script uses tool {} that this build does not have", s["op"]).into());
    }
    let ctx = Ctx::start(cfg, if writes { Writes::Allow } else { Writes::Block }).await?;
    let id = stamp();
    let jpath = ctx.cfg.evidence.join(format!("replay-{id}.jsonl"));
    let journal = Journal::open(&jpath).map_err(|e| format!("cannot open journal {}: {e}", jpath.display()))?;
    let mut skipped = 0;
    let mut failure = None;
    for (i, s) in steps.iter().enumerate() {
        let Some(op) = ops.iter().find(|o| s["op"] == o.name) else {
            continue;
        };
        if op.writes && !writes {
            skipped += 1;
            continue;
        }
        let fail = |reason: String| Failure {
            step: i + 1,
            op: op.name.to_string(),
            args: s["args"].clone(),
            reason,
        };
        match run_op(&ctx, Some(&journal), op, s["args"].clone()).await {
            Err(r) => {
                failure = Some(fail(format!("{:?}: {} (next: {})", r.class, r.reason, r.next)));
                break;
            }
            Ok(out) => {
                if out.data.get("pass") == Some(&json!(false)) {
                    failure = Some(fail(format!("check failed: {}", Value::Object(out.data.clone()))));
                    break;
                }
                let want = &s["expect"]["clicked"];
                let got = out.data.get("clicked").unwrap_or(&Value::Null);
                if !want.is_null() && want != got {
                    failure = Some(fail(format!(
                        "clicked {got} instead of {want}; the page differs from the recording"
                    )));
                    break;
                }
            }
        }
    }
    let code = match failure {
        None => {
            eprintln!(
                "replay ok: {} steps, {skipped} write steps skipped; journal {}",
                steps.len() - skipped,
                jpath.display()
            );
            ExitCode::SUCCESS
        }
        Some(f) => {
            let report = report(&ctx, ops, &id, file, &jpath, &f).await;
            eprintln!("replay failed at step {} ({}): {}", f.step, f.op, f.reason);
            match report {
                Ok(p) => eprintln!("report: {}", p.display()),
                Err(e) => eprintln!("rustglance: cannot write the failure report: {e}"),
            }
            ExitCode::FAILURE
        }
    };
    ctx.shutdown().await;
    Ok(code)
}

async fn report(
    ctx: &Ctx,
    ops: &[OpDef],
    id: &str,
    script: &Path,
    jpath: &Path,
    f: &Failure,
) -> std::io::Result<PathBuf> {
    let shot = match ops.iter().find(|o| o.name == "screenshot") {
        Some(op) => match run_op(ctx, None, op, json!({ "name": "replay-failure", "full": true })).await {
            Ok(out) => format!(
                "{} (sha256 {})",
                out.data.get("file").and_then(Value::as_str).unwrap_or("?"),
                out.data.get("sha256").and_then(Value::as_str).unwrap_or("?")
            ),
            Err(r) => format!("none ({})", r.reason),
        },
        None => "none (no screenshot tool)".to_string(),
    };
    let url = ctx.current_url().await.unwrap_or_else(|e| format!("unknown ({e})"));
    let (logs, _) = ctx.logs_since(0);
    let mut md = format!(
        "# Replay failed\n\n- script: {}\n- step: {} ({})\n- args: `{}`\n- reason: {}\n- page: {url}\n- screenshot: {shot}\n- journal: {}\n\n## Browser log\n\n",
        script.display(),
        f.step,
        f.op,
        f.args,
        f.reason,
        jpath.display()
    );
    if logs.is_empty() {
        md.push_str("(empty)\n");
    } else {
        md.push_str("```\n");
        for l in &logs {
            md.push_str(&l.to_string());
            md.push('\n');
        }
        md.push_str("```\n");
    }
    let path = ctx.cfg.evidence.join(format!("replay-{id}-failure.md"));
    tokio::fs::write(&path, md).await?;
    Ok(path)
}
