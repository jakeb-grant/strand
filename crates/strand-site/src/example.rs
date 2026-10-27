//! Animated examples. An example is an eval scenario under
//! `eval/scenarios/docs/` (docs/eval.md, "Scenario format"): a config and
//! the steps that drive it. A page's ```` ```example springs ```` block
//! shows the config of `springs.scn`, as written there, beside its frames
//! as Strand's renderer drew them on eval's virtual clock, a scrubber, and
//! each springing node's value over time. The frames come from
//! `strand-site media` (scripts/site/media.sh), which keeps beside them
//! the scenario's hash, the seal of the approved reference recording
//! each run was held to (eval/refs/, docs/eval.md "Reference
//! recordings"), and each file's hash. The build fails when a scenario
//! changed since its media were rendered, when its dark twin is not its
//! twin, when a reference was approved again since (the eval gate holds
//! every docs scenario to its reference, so a renderer change that alters
//! the pictures fails there and, once approved, here), and when a file
//! is not the one rendered: a picture is always of the code beside it,
//! as the renderer draws it now.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;

use crate::highlight::Highlighter;
use crate::html::esc;

/// Where the examples' scenarios live.
pub const DIR: &str = "eval/scenarios/docs";
/// Where their media are written (committed, served at `media/`).
pub const MEDIA: &str = "site/media";
/// Each scenario's dark twin, run for the dark media and gated as the
/// light one is.
pub const TWINS: &str = "eval/scenarios/docs/dark";
/// Where eval keeps approved reference recordings.
pub const REFS: &str = "eval/refs";
/// The live examples' manifest, in the site (docs/live-examples.md):
/// the build writes it saying there is no WebAssembly build, and
/// scripts/wasm/site.sh, which adds one to a built site, writes it
/// again naming its files. Every page loads it; a page offers live mode
/// only when it names a build.
pub const MANIFEST: &str = "wasm/manifest.js";

/// The manifest a site without the WebAssembly build has.
pub fn manifest_none() -> String {
    "// The live examples' manifest (docs/live-examples.md): this site was built without\n\
     // their WebAssembly build; scripts/wasm/site.sh adds it and names it here.\n\
     window.STRAND_WASM = null;\n"
        .to_string()
}

/// A scenario, parsed as far as the site needs.
#[derive(Debug)]
pub struct Scenario {
    pub name: String,
    /// Its eval id (`id = "docs-springs"`).
    pub id: String,
    /// From the repository's root.
    pub path: String,
    /// Its `.strand` files, in order.
    pub files: Vec<(String, String)>,
    /// The steps, as written.
    pub steps: String,
    /// The header before the first section (TOML: `id`, `screen`,
    /// `[services]`, …).
    pub header: String,
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
    let id = id_of(&text).ok_or_else(|| format!("{path}: no `id = \"…\"`"))?;
    if files.is_empty() {
        return Err(format!("{path}: no config file"));
    }
    let header = text
        .split_inclusive('\n')
        .take_while(|l| !l.starts_with("=== "))
        .collect();
    Ok(Scenario {
        name: name.to_string(),
        id,
        path,
        files,
        steps,
        header,
        hash: blake3::hash(text.as_bytes()).to_hex().to_string(),
    })
}

/// A scenario's `id`, from its header.
fn id_of(text: &str) -> Option<String> {
    text.lines()
        .take_while(|l| !l.starts_with("=== "))
        .find_map(|l| {
            let v = l.strip_prefix("id")?.trim_start().strip_prefix('=')?.trim();
            Some(v.strip_prefix('"')?.strip_suffix('"')?.to_string())
        })
}

/// The header keys a live example runs with (docs/live-examples.md):
/// any other (`wall`, `fonts`, `[seed]`, a second screen, …) would make
/// the browser's run another than eval's, so the build refuses it.
const LIVE_KEYS: &[&str] = &["id", "about", "screen", "hz", "services"];

/// What a figure's live mode runs (strand-web's `live::Example`): the
/// scenario's config files, its `[services]` with each value as eval
/// reads its text, its screen, its clock's rate and its steps, as JSON.
pub fn live(s: &Scenario) -> Result<Value, String> {
    use toml_edit::{DocumentMut, Item, Value as T};
    let at = |e: &str| format!("{}: {e}", s.path);
    let doc = s
        .header
        .parse::<DocumentMut>()
        .map_err(|e| at(&e.to_string()))?;
    for (k, _) in doc.iter() {
        if !LIVE_KEYS.contains(&k) {
            return Err(at(&format!(
                "`{k}` is not a header key the live example runs with ({})",
                LIVE_KEYS.join(", ")
            )));
        }
    }
    let screen = doc
        .get("screen")
        .and_then(Item::as_table_like)
        .ok_or_else(|| at("no `screen = { width, height, scale }`"))?;
    let num = |k: &str, default: Option<f64>| -> Result<f64, String> {
        match screen.get(k).and_then(Item::as_value) {
            Some(T::Integer(i)) => Ok(*i.value() as f64),
            Some(T::Float(f)) => Ok(*f.value()),
            None => default.ok_or_else(|| at(&format!("`screen` has no `{k}`"))),
            Some(_) => Err(at(&format!("`screen.{k}` is not a number"))),
        }
    };
    for (k, _) in screen.iter() {
        if !["width", "height", "scale"].contains(&k) {
            return Err(at(&format!(
                "`screen.{k}` is not one the live example runs with"
            )));
        }
    }
    let hz = match doc.get("hz").and_then(Item::as_integer) {
        Some(h) if (1..=1000).contains(&h) => h,
        Some(h) => return Err(at(&format!("`hz = {h}`"))),
        None => 60,
    };
    let mut services = Vec::new();
    if let Some(t) = doc.get("services") {
        let t = t
            .as_table_like()
            .ok_or_else(|| at("`[services]` is not a table"))?;
        for (k, v) in t.iter() {
            let text = match v.as_value() {
                Some(T::String(s)) => s.value().clone(),
                Some(v) => v.to_string().trim().to_string(),
                None => return Err(at(&format!("`[services]` `{k}` is not a value"))),
            };
            services.push(serde_json::json!([k, text]));
        }
    }
    Ok(serde_json::json!({
        "name": s.name,
        "id": s.id,
        "files": s.files.iter().map(|(f, b)| serde_json::json!([f, b])).collect::<Vec<_>>(),
        "services": services,
        "screen": {
            "width": num("width", None)?,
            "height": num("height", None)?,
            "scale": num("scale", Some(1.0))?,
        },
        "hz": hz,
        "steps": s.steps,
    }))
}

/// Every docs example (`eval/scenarios/docs/*.scn`, by name) as its
/// live mode runs it ([`live`]), as a JSON list.
pub fn all_live(root: &Path) -> Result<Value, String> {
    let dir = root.join(DIR);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{DIR}: {e}"))?
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "scn").then(|| p.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    let mut out = Vec::new();
    for n in names {
        out.push(live(&load(root, &n)?)?);
    }
    Ok(Value::Array(out))
}

/// JSON as the content of a `<script type="application/json">`: a `</`
/// would end the element, so it is written `<\/`.
pub fn script_json(v: &Value) -> String {
    v.to_string().replace("</", "<\\/")
}

/// The id of `id`'s dark twin.
pub fn dark_id(id: &str) -> String {
    format!("{id}-dark")
}

/// The dark twin of scenario `name`, whose text is `text`: the same
/// scenario under its own id, with the system in dark mode (set in its
/// `[services]` table, which is added when it has none).
pub fn dark_twin(name: &str, text: &str) -> Result<String, String> {
    let id = id_of(text).ok_or_else(|| format!("{DIR}/{name}.scn: no `id = \"…\"`"))?;
    let mut o = format!(
        "# Generated by scripts/site/media.sh from ../{name}.scn: the same scenario with the\n\
         # system in dark mode, for the site's dark pictures. Do not edit.\n"
    );
    let mut header = true;
    let mut services = false;
    for line in text.split_inclusive('\n') {
        if header && line.starts_with("=== ") {
            if !services {
                o.push_str("[services]\n\"system.dark\" = true\n\n");
            }
            header = false;
        }
        if header && line.trim_start().starts_with("id") && id_of(line).as_deref() == Some(&id) {
            o.push_str(&line.replacen(&format!("\"{id}\""), &format!("\"{}\"", dark_id(&id)), 1));
            continue;
        }
        o.push_str(line);
        if header && line.trim_end() == "[services]" {
            o.push_str("\"system.dark\" = true\n");
            services = true;
        }
    }
    Ok(o)
}

/// The seal of the approved (`verdict good`) recording in
/// `eval/refs/<id>.ref`.
pub fn approved_seal(root: &Path, id: &str) -> Result<String, String> {
    let path = format!("{REFS}/{id}.ref");
    let text = std::fs::read_to_string(root.join(&path)).map_err(|_| {
        format!(
            "scenario `{id}` has no approved reference recording ({path}); check it with \
             `scripts/container/run.sh eval check --gate`, approve its run, then run scripts/site/media.sh"
        )
    })?;
    let mut block: Option<&str> = None;
    let mut good = false;
    for line in text.lines() {
        if let Some(r) = line.strip_prefix("ref ") {
            block = Some(r.trim().trim_matches('"'));
            good = false;
        } else if line.trim() == "verdict good" {
            good = true;
        } else if let Some(seal) = line.strip_prefix("seal ")
            && good
            && block == Some(id)
        {
            return Ok(seal.trim().to_string());
        }
    }
    Err(format!(
        "{path}: no approved (`verdict good`) recording of `{id}`"
    ))
}

/// The media for `name`, as `strand-site media` wrote them, checked
/// against the scenario, its twin, its references and the files.
pub fn media(root: &Path, s: &Scenario) -> Result<Value, String> {
    let path = format!("{MEDIA}/{}.json", s.name);
    let text = std::fs::read_to_string(root.join(&path)).map_err(|e| {
        format!(
            "example `{}`: {path}: {e}; render it with scripts/site/media.sh",
            s.name
        )
    })?;
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let again = format!("run scripts/site/media.sh {}", s.name);
    if v["source"].as_str() != Some(s.hash.as_str()) {
        return Err(format!(
            "example `{}`: {} changed since its media were rendered; {again}",
            s.name, s.path
        ));
    }
    let twin = format!("{TWINS}/{}.scn", s.name);
    let text =
        std::fs::read_to_string(root.join(&s.path)).map_err(|e| format!("{}: {e}", s.path))?;
    if std::fs::read_to_string(root.join(&twin)).ok() != Some(dark_twin(&s.name, &text)?) {
        return Err(format!(
            "example `{}`: {twin} is not {}'s dark twin; {again}",
            s.name, s.path
        ));
    }
    for (mode, id) in [("light", s.id.clone()), ("dark", dark_id(&s.id))] {
        let m = &v["modes"][mode];
        let seal = approved_seal(root, &id)?;
        if m["reference"]["id"].as_str() != Some(id.as_str())
            || m["reference"]["seal"].as_str() != Some(seal.as_str())
        {
            return Err(format!(
                "example `{}`: its {mode} media were not rendered from the recording \
                 {REFS}/{id}.ref approves now (a renderer change approved since?); {again}",
                s.name
            ));
        }
        let files = m["files"]
            .as_object()
            .filter(|f| !f.is_empty())
            .ok_or_else(|| {
                format!(
                    "example `{}`: {path} names no {mode} files; {again}",
                    s.name
                )
            })?;
        for key in ["base", "sprite"] {
            if let Some(f) = m[key].as_str()
                && !files.contains_key(f)
            {
                return Err(format!(
                    "example `{}`: {path}: `{f}` has no hash; {again}",
                    s.name
                ));
            }
        }
        for (file, hash) in files {
            let at = format!("{MEDIA}/{file}");
            let bytes = std::fs::read(root.join(&at)).map_err(|e| format!("{at}: {e}; {again}"))?;
            if hash.as_str() != Some(blake3::hash(&bytes).to_hex().as_str()) {
                return Err(format!(
                    "example `{}`: {at} is not the file its media rendered; {again}",
                    s.name
                ));
            }
        }
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
        // A long step (a whole JSON record) is cut short; all of it is
        // the label's title.
        let short: String = if text.chars().count() > MARK_CHARS {
            text.chars().take(MARK_CHARS - 1).collect::<String>() + "…"
        } else {
            text.to_string()
        };
        let _ = write!(
            o,
            "<line class=\"ex-mark\" x1=\"{xx:.1}\" x2=\"{xx:.1}\" y1=\"{pad_t}\" y2=\"{}\"/>\
             <text class=\"ex-axis ex-mark-text\" x=\"{:.1}\" y=\"{}\"><title>{}</title>{}</text>",
            base + 6.0,
            xx + 4.0,
            base + 20.0,
            esc(text),
            esc(&short),
        );
    }
    let _ = write!(
        o,
        "<text class=\"ex-axis\" x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>\
         <line class=\"ex-cursor\" x1=\"{pad_l}\" x2=\"{pad_l}\" y1=\"{pad_t}\" y2=\"{base}\" \
         data-x0=\"{pad_l}\" data-x1=\"{}\" data-tend=\"{t_end}\"/></svg>",
        w - pad_r,
        base + 20.0,
        seconds(t_end),
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

/// The longest step label under a curve, in characters.
const MARK_CHARS: usize = 26;

/// The tick a figure shows before it plays: the scene as its first step
/// left it, just before the second acts (the last tick when there is
/// one step or none), so a still view is never the blank before it all.
pub fn poster(media: &Value) -> usize {
    let ticks = media["t"].as_array().map_or(0, Vec::len);
    let acts: Vec<usize> = media["marks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["k"].as_u64())
        .map(|k| k as usize)
        .collect();
    match acts.get(1) {
        Some(&k) if k > 0 => (k - 1).min(ticks.saturating_sub(1)),
        _ => ticks.saturating_sub(1),
    }
}

/// The example's figure: the frames, their player, the value curve,
/// what its live mode runs ([`live`]) and, unless the page already shows
/// the same files (`code: false`), the config. `root` leads from the
/// page to the site's root.
pub fn figure(
    s: &Scenario,
    media: &Value,
    live: &Value,
    hl: &Highlighter,
    root: &str,
    code: bool,
) -> String {
    let mut o = String::new();
    let shown = poster(media);
    let size = nums(&media["size"]);
    let (lw, lh) = (
        size.first().copied().unwrap_or(1.0),
        size.get(1).copied().unwrap_or(1.0),
    );
    let ticks = media["t"].as_array().map_or(0, Vec::len);
    let _ = write!(
        o,
        "<figure class=\"example{still}\" id=\"example-{name}\">",
        name = s.name,
        still = if ticks > 1 { "" } else { " example-still" }
    );
    // A bar keeps its size on a narrow screen, in a strip that scrolls
    // sideways: shrunk to fit, its text would be too small to read.
    let keep = if lw >= 6.0 * lh {
        format!("; min-width: {lw}px")
    } else {
        String::new()
    };
    let _ = write!(
        o,
        "<div class=\"ex-view\"><div class=\"ex-frame\"><div class=\"ex-stage\" \
         style=\"aspect-ratio: {lw} / {lh}; max-width: {}px{keep}\">",
        lw * 1.5,
    );

    // Each mode's first frame, and over it the part that changes, from
    // a strip of every distinct frame's.
    for mode in ["light", "dark"] {
        let m = &media["modes"][mode];
        let Some(base) = m["base"].as_str() else {
            continue;
        };
        let _ = write!(
            o,
            "<div class=\"ex-sprite ex-{mode}\" style=\"background-image: url('{root}media/{base}')\">"
        );
        let frame = nums(&m["frame"]);
        let bx = nums(&m["box"]);
        if let (Some(sprite), Some(count), [fw, fh], [x, y, w, h]) = (
            m["sprite"].as_str(),
            m["count"].as_u64(),
            frame.as_slice(),
            bx.as_slice(),
        ) {
            let pc = |a: f64, b: f64| format!("{:.4}%", 100.0 * a / b);
            let at = nums(&m["ticks"]);
            // Without the script, the poster's frame is what shows.
            let i = at.get(shown).copied().unwrap_or(0.0);
            let y_at = if count > 1 {
                100.0 * i / (count - 1) as f64
            } else {
                0.0
            };
            let _ = write!(
                o,
                "<div class=\"ex-moving\" style=\"left: {}; top: {}; width: {}; height: {}; \
                 background-image: url('{root}media/{sprite}'); background-position: 0 {y_at:.4}%\" \
                 data-count=\"{count}\" data-ticks=\"{}\"></div>",
                pc(*x, *fw),
                pc(*y, *fh),
                pc(*w, *fw),
                pc(*h, *fh),
                at.iter()
                    .map(|f| (*f as u64).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        o.push_str("</div>");
    }
    o.push_str("</div></div>");
    let moving = ticks > 1;
    let t = nums(&media["t"]);
    if moving {
        let mut plot = String::new();
        curve(&mut plot, media);
        if !plot.is_empty() {
            let _ = write!(o, "<div class=\"ex-plot\">{plot}</div>");
        }
        let _ = write!(
            o,
            "<div class=\"ex-bar ex-controls\" data-t=\"{}\" data-poster=\"{shown}\">\
         <button type=\"button\" class=\"btn icon ex-play\" aria-label=\"Play\" aria-pressed=\"false\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path class=\"ex-icon-play\" d=\"M4.5 3v10l8.5-5z\"/>\
         <path class=\"ex-icon-pause\" d=\"M4.5 3h2.5v10H4.5zM9 3h2.5v10H9z\"/></svg></button>\
         <button type=\"button\" class=\"btn icon quiet ex-step\" data-step=\"-1\" aria-label=\"Previous frame\" title=\"Previous frame\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path d=\"M10.5 3.5 5 8l5.5 4.5z\"/></svg></button>\
         <button type=\"button\" class=\"btn icon quiet ex-step\" data-step=\"1\" aria-label=\"Next frame\" title=\"Next frame\">\
         <svg viewBox=\"0 0 16 16\" aria-hidden=\"true\"><path d=\"M5.5 3.5 11 8l-5.5 4.5z\"/></svg></button>\
         <input type=\"range\" class=\"ex-scrub\" id=\"ex-scrub-{name}\" min=\"0\" max=\"{}\" value=\"{shown}\" \
         aria-label=\"Frame\"><output class=\"ex-time\" for=\"ex-scrub-{name}\">{}</output></div>",
            t.iter().map(|v| fmt_num(*v)).collect::<Vec<_>>().join(","),
            ticks.saturating_sub(1),
            seconds(t.get(shown).copied().unwrap_or(0.0)),
            name = s.name
        );
    }
    o.push_str("</div>");
    // Under what it draws, the file it draws from.
    for (file, body) in s.files.iter().filter(|_| code) {
        let _ = write!(
            o,
            "<div class=\"code\"><div class=\"code-head\"><span class=\"code-file\">{}</span></div>\
             <pre><code class=\"language-strand\">{}</code></pre></div>",
            esc(file),
            hl.html(body)
        );
    }
    let _ = write!(
        o,
        "<figcaption>Drawn by Strand from {which}.</figcaption>\
         <script type=\"application/json\" class=\"ex-live-data\">{}</script></figure>",
        // What live mode runs (docs/live-examples.md), read by the
        // page's script only when the reader asks for it.
        script_json(live),
        which = if code {
            "this config"
        } else {
            "the config above"
        }
    );
    o
}

/// A time on the player, `ms` in seconds: "0.62 s".
fn seconds(ms: f64) -> String {
    format!("{:.2} s", ms / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poster_is_the_scene_its_first_step_left() {
        let m: Value = serde_json::json!({
            "t": (0..75).collect::<Vec<_>>(),
            "marks": [{"k": 1, "text": "set on true"}, {"k": 40, "text": "set on false"}],
        });
        assert_eq!(poster(&m), 39);
        let one: Value = serde_json::json!({"t": [0, 1, 2], "marks": [{"k": 1, "text": "x"}]});
        assert_eq!(poster(&one), 2);
        let still: Value = serde_json::json!({"t": [0]});
        assert_eq!(poster(&still), 0);
    }

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
        assert_eq!(s.id, "docs-t");
        assert_eq!(
            s.files,
            vec![("shell.strand".into(), "bar B { height: 30 }\n".into())]
        );
        assert_eq!(s.steps, "rest\n");
        assert!(load(&dir, "../t").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_example_is_its_scenario_and_refuses_what_the_web_cannot_run() {
        let dir = std::env::temp_dir().join(format!("strand-site-live-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        let scn = |header: &str| {
            format!(
                "{header}\n\n=== shell.strand\nbar B {{ height: 30 }}\n\n=== steps\nrest\nset x \"a</script>\"\n"
            )
        };
        std::fs::write(
            dir.join(DIR).join("t.scn"),
            scn("id = \"docs-t\"\nscreen = { width = 400, height = 100 }\n[services]\n\"battery.percent\" = 0.5\n\"x\" = 'hi'"),
        )
        .unwrap();
        let v = live(&load(&dir, "t").unwrap()).unwrap();
        assert_eq!(v["name"], "t");
        assert_eq!(v["id"], "docs-t");
        assert_eq!(v["screen"]["scale"], 1.0);
        assert_eq!(v["hz"], 60);
        assert_eq!(
            v["services"],
            serde_json::json!([["battery.percent", "0.5"], ["x", "hi"]])
        );
        // In a page, the JSON cannot end its script element early.
        assert!(!script_json(&v).contains("</script>"));
        // A header key the web host would not honour is refused by name.
        std::fs::write(
            dir.join(DIR).join("u.scn"),
            scn("id = \"docs-u\"\nscreen = { width = 400, height = 100 }\ngpu = true"),
        )
        .unwrap();
        let e = live(&load(&dir, "u").unwrap()).unwrap_err();
        assert!(e.contains("`gpu` is not a header key"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dark_twin_is_its_scenario_in_dark_mode_under_its_own_id() {
        let plain = "id = \"docs-t\"\nscreen = { width = 10 }\n\n=== shell.strand\nbar B { }\n\n=== steps\nrest\n";
        let twin = dark_twin("t", plain).unwrap();
        assert!(twin.starts_with("# Generated by scripts/site/media.sh"));
        assert!(twin.contains("id = \"docs-t-dark\"\n"), "{twin}");
        assert!(
            twin.contains("[services]\n\"system.dark\" = true\n\n=== shell.strand\nbar B { }\n"),
            "{twin}"
        );
        let with = "id = \"docs-u\"\n\n[services]\n\"battery.percent\" = 0.5\n\n=== shell.strand\nbar B { }\n";
        let twin = dark_twin("u", with).unwrap();
        assert!(
            twin.contains("[services]\n\"system.dark\" = true\n\"battery.percent\" = 0.5\n"),
            "{twin}"
        );
        assert_eq!(twin.matches("[services]").count(), 1);
        assert!(dark_twin("v", "about = 1\n=== steps\n").is_err());
    }

    #[test]
    fn the_approved_seal_is_the_good_blocks() {
        let dir = std::env::temp_dir().join(format!("strand-site-seal-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(REFS)).unwrap();
        std::fs::write(
            dir.join(REFS).join("docs-t.ref"),
            "# note\nref \"docs-t\"\nverdict bad\nseal 1111\nk0\nref \"docs-t\"\nverdict good\nseal 2222\nk0\n",
        )
        .unwrap();
        assert_eq!(approved_seal(&dir, "docs-t").unwrap(), "2222");
        assert!(
            approved_seal(&dir, "docs-u")
                .unwrap_err()
                .contains("no approved reference")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The committed `hello` example in a repository of its own, so each
    /// part the media are tied to can be changed and the check seen to
    /// fail.
    #[test]
    fn media_are_held_to_their_scenario_twin_references_and_files() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let d = std::env::temp_dir().join(format!("strand-site-media-tie-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let files = [
            format!("{DIR}/hello.scn"),
            format!("{TWINS}/hello.scn"),
            format!("{REFS}/docs-hello.ref"),
            format!("{REFS}/docs-hello-dark.ref"),
            format!("{MEDIA}/hello.json"),
            format!("{MEDIA}/hello-light.png"),
            format!("{MEDIA}/hello-dark.png"),
        ];
        let reset = || {
            for f in &files {
                std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
                std::fs::copy(repo.join(f), d.join(f)).unwrap();
            }
        };
        let check = || load(&d, "hello").and_then(|s| media(&d, &s));
        reset();
        check().unwrap();
        let edit = |f: &str, from: &str, to: &str| {
            let t = std::fs::read_to_string(d.join(f)).unwrap();
            assert!(t.contains(from), "{f}: {from}");
            std::fs::write(d.join(f), t.replacen(from, to, 1)).unwrap();
        };
        // The scenario changed since.
        edit(&files[0], "rest", "rest\nrest");
        assert!(
            check()
                .unwrap_err()
                .contains("changed since its media were rendered")
        );
        reset();
        // The twin is not the scenario's twin.
        edit(&files[1], "height: 32", "height: 33");
        assert!(
            check()
                .unwrap_err()
                .contains("is not eval/scenarios/docs/hello.scn's dark twin")
        );
        reset();
        // A recording approved since (its seal is another).
        edit(&files[3], "\nseal ", "\nseal 0");
        assert!(
            check()
                .unwrap_err()
                .contains("its dark media were not rendered from the recording")
        );
        reset();
        // A picture that is not the one rendered.
        std::fs::write(d.join(&files[5]), b"not the png").unwrap();
        assert!(
            check()
                .unwrap_err()
                .contains("hello-light.png is not the file its media rendered")
        );
        reset();
        // No approved reference at all.
        std::fs::remove_file(d.join(&files[2])).unwrap();
        assert!(
            check()
                .unwrap_err()
                .contains("no approved reference recording")
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
