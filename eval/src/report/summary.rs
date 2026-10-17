//! The headless summaries (docs/eval.md, "Reports"): `summary.md`, one
//! line per failing finding with its check, node, origin, tick, axis,
//! value and bound and its contact sheet, and `summary.json`, the same
//! for tools.

use serde_json::{Value, json};

use super::{Entry, RunInfo};
use crate::checks::{Status, describe};

fn status_word(s: &Status) -> String {
    match s {
        Status::Pass => "pass".into(),
        Status::Fail => "FAIL".into(),
        Status::Xfail(_) => "XFAIL".into(),
        Status::Xpass(_) => "XPASS".into(),
        Status::Changed => "CHANGED".into(),
        Status::Skip(_) => "skip".into(),
    }
}

/// `summary.md`.
pub fn markdown(info: &RunInfo, entries: &[Entry]) -> String {
    let mut md = vec![
        format!("# strand-eval {}", info.command),
        String::new(),
        format!(
            "Report: `{}` (open `{}/index.html`).",
            info.out.display(),
            info.out.display()
        ),
        String::new(),
    ];
    if let Some(e) = entries.first() {
        let b = &e.trace.build;
        md.push(format!(
            "Build {}{} ({}).",
            b.rev,
            if b.dirty { " with changes" } else { "" },
            b.profile
        ));
        md.push(String::new());
    }
    let mut failed = Vec::new();
    for e in entries {
        let st = e.status();
        let frames = e.trace.frames.len();
        let head = match st {
            "error" => format!("- **{}** ERROR: {}", e.id, e.error.as_deref().unwrap_or("")),
            _ => format!("- **{}** {} ({frames} frames)", e.id, st.to_uppercase()),
        };
        md.push(head);
        if let Some(r) = &e.report {
            for res in &r.results {
                match &res.status {
                    Status::Xfail(why) => md.push(format!("  - {} XFAIL: {why}", res.check)),
                    Status::Xpass(why) => md.push(format!(
                        "  - {} XPASS: now passes, remove expect_fail ({why})",
                        res.check
                    )),
                    Status::Changed => md.push(format!("  - {} CHANGED", res.check)),
                    _ => {}
                }
            }
        }
        for (at, f) in e.failing() {
            let mut line = format!("  - {}", describe(f, &e.trace));
            if let Some(s) = e.sheets.get(&at) {
                line += &format!(" — sheet `{}/{s}`", info.out.display());
            }
            md.push(line);
        }
        for n in &e.notes {
            md.push(format!("  - note: {n}"));
        }
        if matches!(st, "fail" | "error") {
            failed.push(e.id.as_str());
        }
    }
    md.push(String::new());
    md.push(if failed.is_empty() {
        format!("{} scenarios: all pass", entries.len())
    } else {
        format!(
            "{} scenarios: {} failed ({})",
            entries.len(),
            failed.len(),
            failed.join(", ")
        )
    });
    md.join("\n") + "\n"
}

/// `summary.json`.
pub fn json(info: &RunInfo, entries: &[Entry]) -> String {
    let scenarios: Vec<Value> = entries
        .iter()
        .map(|e| {
            let checks: serde_json::Map<String, Value> = e
                .report
                .iter()
                .flat_map(|r| r.results.iter())
                .map(|r| (r.check.clone(), Value::String(status_word(&r.status))))
                .collect();
            let findings: Vec<Value> = e
                .failing()
                .into_iter()
                .map(|(at, f)| {
                    let meta = f.key.as_ref().and_then(|k| e.trace.nodes.get(k));
                    json!({
                        "check": f.check,
                        "segment": f.segment,
                        "k": f.k,
                        "key": f.key,
                        "label": meta.map(|m| m.label.clone()),
                        "origin": meta.and_then(|m| m.origin.clone()),
                        "axis": f.axis,
                        "got": f.got,
                        "bound": f.bound,
                        "more": f.more,
                        "msg": f.msg,
                        "sheet": e.sheets.get(&at),
                    })
                })
                .collect();
            json!({
                "id": e.id,
                "file": e.file,
                "status": e.status(),
                "error": e.error,
                "frames": e.trace.frames.len(),
                "digest": e.trace.digest,
                "dir": e.id,
                "checks": checks,
                "findings": findings,
                "notes": e.notes,
            })
        })
        .collect();
    let build = entries.first().map(|e| &e.trace.build);
    let v = json!({
        "schema": 1,
        "command": info.command,
        "out": info.out.display().to_string(),
        "rev": build.map(|b| b.rev.clone()),
        "dirty": build.map(|b| b.dirty),
        "ok": entries.iter().all(|e| !matches!(e.status(), "fail" | "error")),
        "scenarios": scenarios,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default()
}
