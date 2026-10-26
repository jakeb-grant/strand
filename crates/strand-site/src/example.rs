//! Animated examples. An example is an eval scenario under
//! `eval/scenarios/docs/` (docs/eval.md, "Scenario format"): a config and
//! the steps that drive it. A page's ```` ```example springs ```` block
//! shows the config of `springs.scn`, as written there, beside its frames
//! as Strand's renderer drew them on eval's virtual clock, a scrubber, and
//! each springing node's value over time. The frames come from
//! `strand-site media` (scripts/site/media.sh), which keeps the scenario's
//! hash beside them: a scenario changed since its media were rendered
//! fails the build, so a picture is always of the code beside it.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;

use crate::highlight::Highlighter;
use crate::html::esc;

/// Where the examples' scenarios live.
pub const DIR: &str = "eval/scenarios/docs";
/// Where their media are written (committed, served at `media/`).
pub const MEDIA: &str = "site/media";

/// A scenario, parsed as far as the site needs.
#[derive(Debug)]
pub struct Scenario {
    pub name: String,
    /// From the repository's root.
    pub path: String,
    /// Its `.strand` files, in order.
    pub files: Vec<(String, String)>,
    /// The steps, as written.
    pub steps: String,
    /// blake3 of the whole file.
    pub hash: String,
}

/// `eval/scenarios/docs/<name>.scn`.
pub fn load(root: &Path, name: &str) -> Result<Scenario, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!(
            "example `{name}`: a name is letters, digits and `-`"
        ));
    }
    let path = format!("{DIR}/{name}.scn");
    let text = std::fs::read_to_string(root.join(&path))
        .map_err(|e| format!("example `{name}`: {path}: {e}"))?;
    let mut files = Vec::new();
    let mut steps = None;
    let mut current: Option<(String, String)> = None;
    let mut finish = |cur: Option<(String, String)>, steps: &mut Option<String>| {
        if let Some((head, body)) = cur {
            if head == "steps" {
                *steps = Some(body);
            } else {
                files.push((head, body));
            }
        }
    };
    for line in text.split_inclusive('\n') {
        if let Some(head) = line.strip_prefix("=== ") {
            finish(current.take(), &mut steps);
            current = Some((head.trim().to_string(), String::new()));
        } else if let Some((_, body)) = &mut current {
            body.push_str(line);
        }
    }
    finish(current, &mut steps);
    // The section ends at the next `=== ` line; the blank line before it
    // separates, it is not the file's.
    for (_, body) in &mut files {
        while body.ends_with("\n\n") {
            body.pop();
        }
    }
    let steps = steps.ok_or_else(|| format!("{path}: no `=== steps`"))?;
    if files.is_empty() {
        return Err(format!("{path}: no config file"));
    }
    Ok(Scenario {
        name: name.to_string(),
        path,
        files,
        steps,
        hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
    })
}

/// The media for `name`, as `strand-site media` wrote them.
pub fn media(root: &Path, s: &Scenario) -> Result<Value, String> {
    let path = format!("{MEDIA}/{}.json", s.name);
    let text = std::fs::read_to_string(root.join(&path)).map_err(|e| {
        format!(
            "example `{}`: {path}: {e}; render it with scripts/site/media.sh",
            s.name
        )
    })?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    if v["source"].as_str() != Some(s.hash.as_str()) {
        return Err(format!(
            "example `{}`: {} changed since its media were rendered; run scripts/site/media.sh",
            s.name, s.path
        ));
    }
    Ok(v)
}

fn nums(v: &Value) -> Vec<f64> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default()
}

/// The value curve: each springing node's tracked value over the run, the
/// steps as ticks on the time axis, and a cursor the player moves.
fn curve(o: &mut String, v: &Value) {
    let t = nums(&v["t"]);
    let tracks = v["tracks"].as_array().cloned().unwrap_or_default();
    if t.len() < 2 || tracks.is_empty() {
        return;
    }
    let (w, h, pad_l, pad_r, pad_t, pad_b) = (640.0, 168.0, 56.0, 12.0, 14.0, 34.0);
    let t_end = t[t.len() - 1].max(1.0);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for tr in &tracks {
        for p in nums(&tr["values"]) {
            lo = lo.min(p);
            hi = hi.max(p);
        }
    }
    if !lo.is_finite() || hi - lo < 1e-9 {
        return;
    }
    let span = hi - lo;
    let x = |ms: f64| pad_l + (w - pad_l - pad_r) * ms / t_end;
    let y = |val: f64| pad_t + (h - pad_t - pad_b) * (1.0 - (val - lo) / span);
    let _ = write!(
        o,
        "<svg class=\"ex-curve\" viewBox=\"0 0 {w} {h}\" role=\"img\" aria-label=\"Each knob's x over time\">"
    );
    // Gridlines at the values the tracks rest at: before each step that
    // acts, and at the end.
    let mut rests: Vec<f64> = Vec::new();
    let mut at: Vec<usize> = v["marks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["k"].as_u64())
        .filter(|k| *k > 0)
        .map(|k| k as usize - 1)
        .collect();
    at.push(t.len() - 1);
    for tr in &tracks {
        let vals = nums(&tr["values"]);
        for &k in &at {
            if let Some(val) = vals.get(k)
                && !rests.iter().any(|r| (r - val).abs() < 0.5)
            {
                rests.push(*val);
            }
        }
    }
    for val in rests {
        let _ = write!(
            o,
            "<line class=\"ex-grid\" x1=\"{pad_l}\" x2=\"{}\" y1=\"{yy:.1}\" y2=\"{yy:.1}\"/>\
             <text class=\"ex-axis\" x=\"{}\" y=\"{:.1}\" text-anchor=\"end\">{} px</text>",
            w - pad_r,
            pad_l - 8.0,
            y(val) + 4.0,
            fmt_num(val),
            yy = y(val)
        );
    }
    for (i, tr) in tracks.iter().enumerate() {
        let vals = nums(&tr["values"]);
        let mut pts = String::new();
        for (k, val) in vals.iter().enumerate() {
            if let Some(ms) = t.get(k) {
                let _ = write!(pts, "{:.1},{:.1} ", x(*ms), y(*val));
            }
        }
        let _ = write!(
            o,
            "<polyline class=\"ex-line ex-c{}\" points=\"{}\"/>",
            i % 4,
            pts.trim_end()
        );
    }
    // Each step that acts, on the time axis.
    let base = h - pad_b;
    let _ = write!(
        o,
        "<line class=\"ex-grid\" x1=\"{pad_l}\" x2=\"{}\" y1=\"{base}\" y2=\"{base}\"/>",
        w - pad_r
    );
    for m in v["marks"].as_array().cloned().unwrap_or_default() {
        let (Some(k), Some(text)) = (m["k"].as_u64(), m["text"].as_str()) else {
            continue;
        };
        let Some(ms) = t.get(k as usize) else {
            continue;
        };
        let xx = x(*ms);
        let _ = write!(
            o,
            "<line class=\"ex-mark\" x1=\"{xx:.1}\" x2=\"{xx:.1}\" y1=\"{pad_t}\" y2=\"{}\"/>\
             <text class=\"ex-axis ex-mark-text\" x=\"{:.1}\" y=\"{}\">{}</text>",
            base + 6.0,
            xx + 4.0,
            base + 20.0,
            esc(text),
        );
    }
    let _ = write!(
        o,
        "<text class=\"ex-axis\" x=\"{}\" y=\"{}\" text-anchor=\"end\">{} ms</text>\
         <line class=\"ex-cursor\" x1=\"{pad_l}\" x2=\"{pad_l}\" y1=\"{pad_t}\" y2=\"{base}\" \
         data-x0=\"{pad_l}\" data-x1=\"{}\" data-tend=\"{t_end}\"/></svg>",
        w - pad_r,
        base + 20.0,
        fmt_num(t_end),
        w - pad_r
    );
    o.push_str("<ul class=\"ex-legend\">");
    for (i, tr) in tracks.iter().enumerate() {
        let _ = write!(
            o,
            "<li><span class=\"ex-swatch ex-c{}\"></span><code>{}</code> {} on <code>{}</code></li>",
            i % 4,
            esc(tr["node"].as_str().unwrap_or("")),
            esc(tr["prop"].as_str().unwrap_or("")),
            esc(tr["curve"].as_str().unwrap_or(""))
        );
    }
    o.push_str("</ul>");
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-6 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}

/// The example's figure: the frames, their player, the value curve and
/// the config. `root` leads from the page to the site's root.
pub fn figure(s: &Scenario, media: &Value, hl: &Highlighter, root: &str) -> String {
    let mut o = String::new();
    let size = nums(&media["size"]);
    let (lw, lh) = (
        size.first().copied().unwrap_or(1.0),
        size.get(1).copied().unwrap_or(1.0),
    );
    let ticks = media["t"].as_array().map_or(0, Vec::len);
    let _ = write!(
        o,
        "<figure class=\"example{still}\" id=\"example-{name}\">\
         <div class=\"ex-view\"><div class=\"ex-stage\" style=\"aspect-ratio: {lw} / {lh}; max-width: {}px\">",
        lw * 1.5,
        name = s.name,
        still = if ticks > 1 { "" } else { " example-still" }
    );
    for mode in ["light", "dark"] {
        let m = &media["modes"][mode];
        let (Some(file), Some(count)) = (m["sprite"].as_str(), m["count"].as_u64()) else {
            continue;
        };
        let _ = write!(
            o,
            "<div class=\"ex-sprite ex-{mode}\" style=\"background-image: url('{root}media/{file}')\" \
             data-count=\"{count}\" data-ticks=\"{}\"></div>",
            nums(&m["ticks"])
                .iter()
                .map(|f| (*f as u64).to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    o.push_str("</div>");
    let moving = ticks > 1;
    let t: Vec<String> = nums(&media["t"]).iter().map(|v| fmt_num(*v)).collect();
    if moving {
        let _ = write!(
            o,
            "<div class=\"ex-controls\" data-t=\"{}\">\
         <button type=\"button\" class=\"ex-play\" aria-label=\"Play\" aria-pressed=\"false\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path class=\"ex-icon-play\" d=\"M4 2.5v11l9-5.5z\"/>\
         <path class=\"ex-icon-pause\" d=\"M4 2.5h3v11H4zM9 2.5h3v11H9z\"/></svg></button>\
         <button type=\"button\" class=\"ex-step\" data-step=\"-1\" aria-label=\"Previous frame\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path d=\"M10.5 3 5 8l5.5 5z\"/></svg></button>\
         <button type=\"button\" class=\"ex-step\" data-step=\"1\" aria-label=\"Next frame\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path d=\"m5.5 3 5.5 5-5.5 5z\"/></svg></button>\
         <input type=\"range\" class=\"ex-scrub\" id=\"ex-scrub-{name}\" min=\"0\" max=\"{}\" value=\"0\" \
         aria-label=\"Frame\"><output class=\"ex-time\" for=\"ex-scrub-{name}\">frame 0 · 0 ms</output></div>",
            t.join(","),
            ticks.saturating_sub(1),
            name = s.name
        );
        curve(&mut o, media);
    }
    o.push_str("</div>");
    for (file, body) in &s.files {
        let _ = write!(
            o,
            "<div class=\"code\"><div class=\"code-head\"><span class=\"code-file\">{}</span>\
             <a class=\"code-src\" href=\"{}\">{}</a></div><pre><code class=\"language-strand\">{}</code></pre></div>",
            esc(file),
            crate::GITHUB_BLOB.to_string() + &s.path,
            esc(&s.path),
            hl.html(body)
        );
    }
    let hz = media["hz"].as_u64().unwrap_or(60);
    let how = if moving {
        format!("one frame at a time, on a virtual {hz} Hz clock")
    } else {
        "with mock services".to_string()
    };
    let _ = write!(
        o,
        "<figcaption>Drawn by Strand's renderer {how}, from this config; \
         <a href=\"{}{}\">its scenario</a> holds the steps that drive it.</figcaption></figure>",
        crate::GITHUB_BLOB,
        s.path
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scenario_splits_into_files_and_steps() {
        let dir = std::env::temp_dir().join(format!("strand-site-scn-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        std::fs::write(
            dir.join(DIR).join("t.scn"),
            "id = \"docs-t\"\n\n=== shell.strand\nbar B { height: 30 }\n\n=== steps\nrest\n",
        )
        .unwrap();
        let s = load(&dir, "t").unwrap();
        assert_eq!(
            s.files,
            vec![("shell.strand".into(), "bar B { height: 30 }\n".into())]
        );
        assert_eq!(s.steps, "rest\n");
        assert!(load(&dir, "../t").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
