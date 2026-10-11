//! Events as text for people (`strand watch` without `--json`, the
//! log).

use serde_json::Value as Json;

/// An event as text for people.
pub fn describe(ev: &Json) -> String {
    let s = |v: &Json| v.as_str().unwrap_or("").to_string();
    let list = |key: &str| -> Vec<String> {
        ev.get(key)
            .and_then(Json::as_array)
            .map(|a| a.iter().map(s).collect())
            .unwrap_or_default()
    };
    let mut out = String::new();
    match ev.get("event").and_then(Json::as_str) {
        Some("reload") => {
            let ms = ev
                .pointer("/timing/total_ms")
                .and_then(Json::as_f64)
                .unwrap_or(0.0);
            let classes = list("classes");
            let committed = list("committed");
            let held = list("held");
            out.push_str(&format!(
                "reload ({ms:.1} ms): {}\n",
                if classes.is_empty() {
                    "no change".to_string()
                } else {
                    classes.join(", ")
                }
            ));
            if !committed.is_empty() {
                out.push_str(&format!("  committed: {}\n", committed.join(", ")));
            }
            if !held.is_empty() {
                out.push_str(&format!("  held back: {}\n", held.join(", ")));
            }
            let kept = list("kept");
            if !kept.is_empty() {
                out.push_str(&format!("  kept: {}\n", kept.join(", ")));
            }
            if let Some(reset) = ev.get("reset").and_then(Json::as_array) {
                for r in reset {
                    out.push_str(&format!("  reset: {} ({})\n", s(&r["cell"]), s(&r["why"])));
                }
            }
            for n in list("notices") {
                out.push_str(&format!("  {n}\n"));
            }
            if let Some(ds) = ev.get("diagnostics").and_then(Json::as_array) {
                for d in ds {
                    out.push_str(&format!("  {}\n", s(&d["short"])));
                }
            }
        }
        // Outside a reload: cells kept over a changed default (a
        // persisted cell at boot, a parked bar back), lowering's notices.
        Some("notices") => {
            for n in list("notices") {
                out.push_str(&format!("{n}\n"));
            }
            // The running config's warnings, told to a new watcher.
            if let Some(ds) = ev.get("diagnostics").and_then(Json::as_array) {
                for d in ds {
                    out.push_str(&format!("{}\n", s(&d["short"])));
                }
            }
        }
        Some("fault") => {
            out.push_str(&format!("fault: {}\n", s(&ev["message"])));
            if let Some(at) = ev.get("at").and_then(Json::as_str) {
                out.push_str(&format!("  at {at}\n"));
            }
        }
        _ => out.push_str(&format!("{ev}\n")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn events_read_as_text() {
        let ev = json!({
            "event": "reload",
            "classes": ["prop", "node-added"],
            "committed": ["bar.strand"],
            "held": [],
            "kept": ["Clock.open"],
            "reset": [{"cell": "t.b", "why": "renamed"}],
            "notices": ["launcher.query: kept \"fir\" (default changed) [reset]"],
            "timing": {"total_ms": 12.25},
            "diagnostics": [{"short": "bar.strand:3:1: error[x]: y"}],
        });
        assert_eq!(
            describe(&ev),
            "reload (12.2 ms): prop, node-added\n  committed: bar.strand\n  kept: Clock.open\n  reset: t.b (renamed)\n  launcher.query: kept \"fir\" (default changed) [reset]\n  bar.strand:3:1: error[x]: y\n"
        );
    }
}
