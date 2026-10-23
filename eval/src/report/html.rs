//! The viewer page (docs/eval.md, "The viewer"): one self-contained HTML
//! file, `eval/viewer/viewer.{html,css,js}` embedded, the runs' traces
//! and checks in `<script type="application/json" id="run">` and the
//! frames as PNG data URIs. Nothing is loaded from anywhere else.
//!
//! The page stays under [`BUDGET`]: when the images would not fit, the
//! frames around failing findings, `shot` steps and segment ends are kept
//! first and the others thinned evenly; the viewer draws a thinned frame
//! as a wireframe of its trace, which is always complete.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::explain::{AXES, MOTIONS};
use super::{Entry, Frames, RunInfo};
use crate::Error;
use crate::checks::Timeline;

const HTML: &str = include_str!("../../viewer/viewer.html");
const CSS: &str = include_str!("../../viewer/viewer.css");
const JS: &str = include_str!("../../viewer/viewer.js");
/// (replay-clip) A replay's clip, made by the same file
/// `strand-eval replay clip` runs.
const CLIP: &str = crate::replay::clip::CLIP_JS;

/// The largest page, bytes (under the 16 MB a page may take).
pub const BUDGET: usize = 15 * 1024 * 1024;

/// One run on a page.
#[derive(Debug)]
pub struct PageRun<'a> {
    /// `HEAD 2fab9622`, `mutant bug25-hidden-text`.
    pub label: String,
    /// The run's directory, as the commands the page copies name it.
    pub out: String,
    pub frames: Frames,
    pub entries: &'a [Entry],
    /// The approved references of the run beside it (wireframes), not a
    /// run of its own.
    pub reference: bool,
}

/// A page: one run, or two side by side (A/B).
#[derive(Debug)]
pub struct Page<'a> {
    pub title: String,
    pub runs: Vec<PageRun<'a>>,
    /// The A/B comparison (`ab::compare`), when there are two runs.
    pub ab: Option<Value>,
    /// (replay-owner) A glitch report's replay ([`RunInfo::replay`]).
    pub replay: Option<Value>,
    pub budget: usize,
}

impl<'a> Page<'a> {
    pub fn run(info: &RunInfo, entries: &'a [Entry]) -> Page<'a> {
        let rev = entries
            .first()
            .map(|e| {
                format!(
                    "{}{}",
                    e.trace.build.rev,
                    if e.trace.build.dirty { "+" } else { "" }
                )
            })
            .unwrap_or_default();
        let title = match entries {
            [one] => format!("strand-eval {}", one.id),
            _ => format!("strand-eval {} ({} scenarios)", info.command, entries.len()),
        };
        Page {
            title,
            runs: vec![PageRun {
                label: rev,
                out: info.out.display().to_string(),
                frames: info.frames,
                entries,
                reference: false,
            }],
            ab: None,
            replay: info.replay.clone(),
            budget: BUDGET,
        }
    }
}

/// The frame images an entry offers under `frames`: each with whether it
/// is a key frame (kept first when thinning).
fn offered(e: &Entry, frames: Frames) -> Vec<(String, bool)> {
    if !e.keeps_frames(frames) {
        return Vec::new();
    }
    let tl = Timeline::new(&e.trace);
    let mut key_ticks: BTreeSet<u32> = BTreeSet::new();
    for (_, f) in e.failing() {
        if let Some(k) = f.k
            && let Some(i) = tl.at(k)
        {
            for j in i.saturating_sub(3)..=(i + 3).min(tl.ks.len().saturating_sub(1)) {
                key_ticks.insert(tl.ks[j]);
            }
        }
    }
    for s in &e.trace.segments {
        key_ticks.insert(s.e);
        if let Some(s) = s.s {
            key_ticks.insert(s);
        }
    }
    for ev in &e.trace.events {
        if ev.text.starts_with("shot") {
            key_ticks.insert(ev.k);
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for f in &e.trace.frames {
        for name in f.png.values() {
            if e.images.contains_key(name) && seen.insert(name.clone()) {
                out.push((name.clone(), key_ticks.contains(&f.k)));
            }
        }
    }
    out
}

fn uri_len(png: usize) -> usize {
    "data:image/png;base64,".len() + png.div_ceil(3) * 4 + 8
}

/// What embedding a frame's stored file costs: its data URI, (replayfix)
/// the patch's when packed (an empty patch embeds nothing).
fn stored_len(e: &Entry, name: &str) -> usize {
    match &e.packed {
        Some(p) => p
            .files
            .get(name)
            .filter(|b| !b.is_empty())
            .map_or(0, |b| uri_len(b.len())),
        None => e.images.get(name).map_or(0, |im| uri_len(im.png.len())),
    }
}

/// The images to embed per `(run, entry)`, within `room` bytes.
/// (replayfix) A packed entry's frame takes its bases with it, down to
/// a whole one: a patch alone draws nothing.
fn choose(p: &Page, room: usize) -> BTreeMap<(usize, usize), BTreeSet<String>> {
    // Every offered frame, by index: where it is, its name, whether it
    // is a key frame, and the indices it needs (itself first).
    let mut all: Vec<((usize, usize), String, bool)> = Vec::new();
    let mut len: Vec<usize> = Vec::new();
    let mut needs: Vec<Vec<usize>> = Vec::new();
    for (r, run) in p.runs.iter().enumerate() {
        for (i, e) in run.entries.iter().enumerate() {
            let from = all.len();
            let offered = offered(e, run.frames);
            let index: BTreeMap<&str, usize> = offered
                .iter()
                .enumerate()
                .map(|(j, (n, _))| (n.as_str(), from + j))
                .collect();
            for (name, key) in &offered {
                len.push(stored_len(e, name));
                let chain = match &e.packed {
                    Some(pk) => pk
                        .chain(name)
                        .iter()
                        .filter_map(|c| index.get(c.as_str()).copied())
                        .collect(),
                    None => vec![index[name.as_str()]],
                };
                needs.push(chain);
                all.push(((r, i), name.clone(), *key));
            }
        }
    }
    let mut mark = vec![false; all.len()];
    let mut fits = |pick: &dyn Fn(usize, bool) -> bool| -> bool {
        mark.iter_mut().for_each(|m| *m = false);
        let mut total = 0;
        for (n, (_, _, key)) in all.iter().enumerate() {
            if pick(n, *key) {
                for &c in &needs[n] {
                    if !mark[c] {
                        mark[c] = true;
                        total += len[c];
                    }
                }
            }
        }
        total <= room
    };
    // Every image; else every key frame and every n-th other; else every
    // n-th key frame.
    let mut chosen: Option<Box<dyn Fn(usize, bool) -> bool>> = None;
    if fits(&|_, _| true) {
        chosen = Some(Box::new(|_, _| true));
    } else {
        for stride in 2..=64usize {
            let pick = move |n: usize, key: bool| key || n.is_multiple_of(stride);
            if fits(&pick) {
                chosen = Some(Box::new(pick));
                break;
            }
        }
        if chosen.is_none() {
            for stride in 1..=4096usize {
                let pick = move |n: usize, key: bool| key && n.is_multiple_of(stride);
                if fits(&pick) {
                    chosen = Some(Box::new(pick));
                    break;
                }
            }
        }
    }
    let mut out: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
    if let Some(pick) = chosen {
        for (n, (at, _, key)) in all.iter().enumerate() {
            if pick(n, *key) {
                for &c in &needs[n] {
                    out.entry(*at).or_default().insert(all[c].1.clone());
                }
            }
        }
    }
    out
}

/// (replay-owner) A replay's page shows the checks' statuses that would
/// read as verdicts (`fail`, `changed`, `xpass`) as `hint`: its findings
/// are hints (docs/eval.md, "Replaying a glitch report").
fn as_hint(status: &str) -> &str {
    match status {
        "fail" | "changed" | "xpass" => "hint",
        s => s,
    }
}

fn entry_json(
    e: &Entry,
    images: Map<String, Value>,
    thinned: bool,
    hints: bool,
) -> Result<Value, Error> {
    let sheets: Vec<Value> = e
        .failing()
        .into_iter()
        .filter_map(|(at, f)| {
            Some(json!({
                "check": f.check, "k": f.k, "key": f.key, "path": e.sheets.get(&at)?,
            }))
        })
        .collect();
    let mut checks = json!(e.report);
    if hints && let Some(rs) = checks["results"].as_array_mut() {
        for r in rs {
            if let Some(st) = r["status"]["status"].as_str() {
                let h = as_hint(st).to_string();
                r["status"]["status"] = Value::String(h);
            }
        }
    }
    let mut v = json!({
        "id": e.id,
        "file": e.file,
        "status": if hints { as_hint(e.status()) } else { e.status() },
        "error": e.error,
        "notes": e.notes,
        "trace": if e.packed.is_some() { Value::Null } else { json!(e.trace) },
        "checks": checks,
        "refs": e.refs,
        "thinned": thinned,
        "images": images,
        "sheets": sheets,
    });
    // (replayfix) A packed entry's page holds its packed trace, of each
    // frame the fields the viewer draws (`pack::VIEW_FIELDS`; the viewer
    // unpacks it), and its frames' patches: each frame's image is its
    // stored file, a patched one drawn over its base.
    if let Some(p) = &e.packed {
        let trace: Value = serde_json::from_str(&p.page_trace)
            .map_err(|x| Error::Run(format!("page: packed trace: {x}")))?;
        let patches: Map<String, Value> = p
            .patches
            .iter()
            .map(|(n, q)| (n.clone(), q.json()))
            .collect();
        v["trace"] = trace;
        v["patches"] = Value::Object(patches);
    }
    Ok(v)
}

/// JSON text safe inside a `<script>` element.
fn script_safe(json: &str) -> String {
    json.replace("</", "<\\/").replace("<!--", "<\\u0021--")
}

fn escape(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The page's HTML.
pub fn page(p: &Page) -> Result<String, Error> {
    let shell_len = HTML.len() + CSS.len() + JS.len() + CLIP.len() + p.title.len();
    // The page without images, to know the room left for them.
    let build = |chosen: &BTreeMap<(usize, usize), BTreeSet<String>>| -> Result<Value, Error> {
        let runs: Vec<Value> = p
            .runs
            .iter()
            .enumerate()
            .map(|(r, run)| {
                let scenarios = run
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let offered = offered(e, run.frames);
                        let keep = chosen.get(&(r, i));
                        let mut images = Map::new();
                        let mut shown = 0;
                        for (name, _) in &offered {
                            if !keep.is_some_and(|k| k.contains(name)) {
                                continue;
                            }
                            shown += 1;
                            let bytes = match &e.packed {
                                Some(pk) => pk.files.get(name).filter(|b| !b.is_empty()),
                                None => e.images.get(name).map(|im| &im.png),
                            };
                            if let Some(bytes) = bytes {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                                images.insert(
                                    name.clone(),
                                    Value::String(format!("data:image/png;base64,{b64}")),
                                );
                            }
                        }
                        let thinned = shown < offered.len();
                        entry_json(e, images, thinned, p.replay.is_some())
                    })
                    .collect::<Result<Vec<Value>, Error>>()?;
                Ok(json!({
                    "label": run.label,
                    "out": run.out,
                    "reference": run.reference,
                    "frames": match run.frames { Frames::All => "all", Frames::Failing => "failing", Frames::None => "none" },
                    "scenarios": scenarios,
                }))
            })
            .collect::<Result<Vec<Value>, Error>>()?;
        Ok(json!({
            "schema": 1,
            "title": p.title,
            "motions": MOTIONS,
            "axes": AXES,
            "runs": runs,
            "ab": p.ab,
            "replay": p.replay,
        }))
    };
    let bare = serde_json::to_string(&build(&BTreeMap::new())?)
        .map_err(|e| Error::Run(format!("page: {e}")))?;
    let room = p.budget.saturating_sub(shell_len + bare.len() + 1024);
    let chosen = choose(p, room);
    let data =
        serde_json::to_string(&build(&chosen)?).map_err(|e| Error::Run(format!("page: {e}")))?;
    let page = HTML
        .replace("{{TITLE}}", &escape(&p.title))
        .replace("{{CSS}}", CSS)
        .replace("{{CLIP}}", &CLIP.replace("</script", "<\\/script"))
        .replace("{{JS}}", &JS.replace("</script", "<\\/script"))
        .replace("{{DATA}}", &script_safe(&data));
    Ok(page)
}

/// The JSON embedded in a page (for tests and tools).
pub fn embedded(page: &str) -> Option<Value> {
    let start = page.find("<script type=\"application/json\" id=\"run\">")?;
    let body = &page[start..];
    let open = body.find('>')? + 1;
    let close = body.find("</script>")?;
    serde_json::from_str(&body[open..close]).ok()
}
