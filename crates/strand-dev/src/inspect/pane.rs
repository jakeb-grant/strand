//! The node pane as text: identity, layout, props, the provenance chain
//! of each token the node reads, and the kept-state badges of its cells.

use std::fmt::Write as _;

use serde_json::Value as Json;

/// `answer` (an `inspect.node` answer: `node`, `files`, `layout`) as the
/// terminal shows it.
pub fn pane(answer: &Json) -> String {
    let n = &answer["node"];
    let mut out = String::new();
    let s = |v: &Json| v.as_str().unwrap_or("").to_string();
    let origin = &n["origin"];
    let at = match origin["file"].as_str() {
        Some(f) => format!(
            "{f}:{}:{}",
            origin["line"].as_u64().unwrap_or(0),
            origin["col"].as_u64().unwrap_or(0)
        ),
        None => "(no source)".into(),
    };
    let _ = writeln!(out, "{} {}  {at}", s(&n["kind"]), s(&n["id"]));
    let id = &n["identity"];
    let mut who = Vec::new();
    if let Some(p) = id["instance_path"].as_str().filter(|p| !p.is_empty()) {
        who.push(p.to_string());
    }
    if let Some(i) = id["id"].as_str() {
        who.push(format!("id: {i}"));
    }
    who.push(format!("#{}", id["sibling_index"].as_u64().unwrap_or(0)));
    who.push(format!("in {}", s(&id["scope"])));
    let _ = writeln!(out, "  identity  {}", who.join("  "));
    match answer["layout"].as_array().map(Vec::as_slice) {
        Some([]) | None => {
            let _ = writeln!(out, "  layout    not drawn");
        }
        Some(l) => {
            for (i, b) in l.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "  {}surface {}: {},{} {}×{}",
                    if i == 0 { "layout    " } else { "          " },
                    b["surface"],
                    num(&b["x"]),
                    num(&b["y"]),
                    num(&b["width"]),
                    num(&b["height"]),
                );
            }
        }
    }
    if let Some(props) = n["props"].as_object().filter(|p| !p.is_empty()) {
        let _ = writeln!(out, "  props");
        for (k, v) in props {
            let _ = writeln!(out, "    {k}: {}", v.as_str().unwrap_or(&v.to_string()));
        }
    }
    if let Some(tokens) = n["tokens"].as_array().filter(|t| !t.is_empty()) {
        let _ = writeln!(out, "  tokens");
        for t in tokens {
            let chain: Vec<String> = t["chain"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|step| {
                    let mut x = format!("${}", s(&step["path"]));
                    if let Some(o) = step["origin"].as_str() {
                        let _ = write!(x, " ({o})");
                    }
                    if let Some(v) = step["value"].as_str() {
                        let _ = write!(x, " = {v}");
                    }
                    x
                })
                .collect();
            let _ = writeln!(out, "    {} ← {}", s(&t["prop"]), chain.join(" ← "));
        }
    }
    if let Some(cells) = n["cells"].as_array().filter(|c| !c.is_empty()) {
        let _ = writeln!(out, "  state");
        for c in cells {
            let mut badges = Vec::new();
            if c["kept"] == true {
                badges.push("[kept]".to_string());
            }
            if c["persisted"] == true {
                badges.push("[persisted]".to_string());
            }
            if let Some(l) = c["layer"].as_str() {
                badges.push(format!("[{l}]"));
            }
            let _ = writeln!(out, "    {} {}", s(&c["path"]), badges.join(" "));
        }
    }
    out
}

fn num(v: &Json) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 => format!("{f:.0}"),
        Some(f) => format!("{f:.1}"),
        None => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_pane_shows_identity_layout_chain_and_badges() {
        let answer = json!({
            "ok": true,
            "node": {
                "id": "4.0", "kind": "box", "parent": "1.0",
                "origin": {"span": [10, 40], "file": "/c/bar.strand", "line": 3, "col": 5},
                "identity": {"instance_path": "[DP-2]", "key": "DP-2", "id": "target",
                             "sibling_index": 0, "scope": "bar"},
                "props": {"bg": "#f38ba8", "width": "200"},
                "cells": [{"path": "bar.clicks", "kept": true, "persisted": false, "layer": null}],
                "tokens": [{"prop": "bg", "path": "probe.c", "chain": [
                    {"path": "probe.c", "kind": "global", "level": null, "origin": "base",
                     "value": "#f38ba8", "reads": []}]}],
            },
            "files": {},
            "layout": [{"surface": 2, "x": 0.0, "y": 0.0, "width": 200.0, "height": 40.0}],
        });
        let p = pane(&answer);
        assert!(p.starts_with("box 4.0  /c/bar.strand:3:5\n"), "{p}");
        assert!(p.contains("[DP-2]  id: target  #0  in bar"), "{p}");
        assert!(p.contains("layout    surface 2: 0,0 200×40"), "{p}");
        assert!(p.contains("bg ← $probe.c (base) = #f38ba8"), "{p}");
        assert!(p.contains("bar.clicks [kept]"), "{p}");
    }
}
