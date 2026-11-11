//! Markdown summary of a journal: time per tool, refusals, evidence files
//! and blocked writes.

use std::collections::BTreeMap;
use std::path::Path;

use crate::journal::{self, ReadError};

#[derive(Default)]
struct Stats {
    calls: u64,
    refused: u64,
    total_ms: u64,
    max_ms: u64,
}

pub fn render(path: &Path) -> Result<String, ReadError> {
    let entries = journal::read(path)?;
    let mut per_op: BTreeMap<String, Stats> = BTreeMap::new();
    let mut refusals = Vec::new();
    let mut evidence = Vec::new();
    let mut blocked = Vec::new();
    for e in &entries {
        let op = e["op"].as_str().unwrap_or("?").to_string();
        let ms = e["ms"].as_u64().unwrap_or(0);
        let s = per_op.entry(op.clone()).or_default();
        s.calls += 1;
        s.total_ms = s.total_ms.saturating_add(ms);
        s.max_ms = s.max_ms.max(ms);
        let t = e["t"].as_str().unwrap_or("");
        if let Some(r) = e.get("refusal") {
            s.refused += 1;
            refusals.push(format!(
                "| {t} | {op} | {} | {} |",
                r["error"].as_str().unwrap_or("?"),
                cell(r["reason"].as_str().unwrap_or(""))
            ));
        }
        let res = &e["result"];
        if let (Some(file), Some(sha)) = (res["file"].as_str(), res["sha256"].as_str()) {
            evidence.push(format!("| {t} | {op} | {file} | {sha} |"));
        }
        for w in e["blocked_writes"].as_array().into_iter().flatten() {
            blocked.push(format!("| {t} | {op} | {} |", cell(w.as_str().unwrap_or(""))));
        }
    }
    let mut md = format!("# Journal {}\n\n{} calls\n\n", path.display(), entries.len());
    md.push_str("## Tools\n\n| tool | calls | refused | total ms | avg ms | max ms |\n|---|---|---|---|---|---|\n");
    for (op, s) in &per_op {
        let avg = s.total_ms.checked_div(s.calls).unwrap_or(0);
        md.push_str(&format!(
            "| {op} | {} | {} | {} | {avg} | {} |\n",
            s.calls, s.refused, s.total_ms, s.max_ms
        ));
    }
    section(
        &mut md,
        "Refusals",
        "| time | tool | error | reason |\n|---|---|---|---|",
        &refusals,
    );
    section(
        &mut md,
        "Evidence files",
        "| time | tool | file | sha256 |\n|---|---|---|---|",
        &evidence,
    );
    section(
        &mut md,
        "Blocked writes",
        "| time | tool | request |\n|---|---|---|",
        &blocked,
    );
    Ok(md)
}

fn section(md: &mut String, title: &str, head: &str, rows: &[String]) {
    md.push_str(&format!("\n## {title}\n\n"));
    if rows.is_empty() {
        md.push_str("none\n");
        return;
    }
    md.push_str(head);
    md.push('\n');
    for r in rows {
        md.push_str(r);
        md.push('\n');
    }
}

/// Text safe inside a markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}
