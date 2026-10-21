//! A bundle turned into a scenario (docs/eval.md, "Replaying a glitch
//! report", steps 2 to 5): the window, the output, every service field and
//! cell rewound to the window's start, and the steps on the recorded
//! timeline.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use strand_core::flight::{Entry, flags, gap, kind, modifiers, request};

use super::bundle::{Bundle, node_ref_at};
use crate::harness::NodeRef;

/// (replayfix) A leave followed this soon by an enter on another surface
/// is the pointer handed from one surface to the other.
pub const HANDOFF_NS: u64 = 50_000_000;

/// How long before the report the busiest output is counted over.
pub const BUSY_NS: u64 = 10_000_000_000;
/// (report review) The longest window replayed, before the report: the
/// input ring's motion covers about as much (design.md "Reporting a
/// glitch": the last minute or so).
pub const WINDOW_CAP_NS: u64 = 120_000_000_000;

/// Services the sandbox keeps itself: its one screen, the clock and the
/// calendar it computes from `wall`, and `auth`, which is never recorded.
pub const SANDBOX_OWNS: [&str; 4] = ["screens", "clock", "calendar", "auth"];

/// What the replay is asked for.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `--output NAME`.
    pub output: Option<String>,
    /// `--traced SECONDS` (`None`, the default: all of the window).
    pub traced: Option<f64>,
    /// (replay-owner) `--gpu`: a surface the live shell drew on the GPU
    /// is replayed on both legs (`backend = "both"`). Without it the
    /// replay draws on the CPU alone, as a plain process on a desktop
    /// does, and says so in its limits.
    pub gpu: bool,
}

/// The output replayed.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputPick {
    pub name: String,
    /// (replayfix) Who it is, as `bundle.json`'s outputs name it: its
    /// `screens.all` id and description (a bundle before replayfix has
    /// neither), make and model.
    pub id: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub description: Option<String>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub hz: u32,
    /// Why it was picked.
    pub why: String,
}

/// A press as the live shell recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LivePress {
    /// Since the window's start.
    pub t: u64,
    pub hit: Option<NodeRef>,
    /// The hit's `node_ref`, when `nodes.json` did not name it.
    pub unknown: Option<String>,
}

/// A surface root mapped or gone, as the live shell recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveMap {
    pub t: u64,
    pub origin: String,
    pub mapped: bool,
}

/// A cell write as the live shell recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveWrite {
    pub t: u64,
    pub path: String,
    pub value: Json,
}

/// One line of the event list `replay.txt` gives.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Since the window's start.
    pub t: u64,
    /// (replayfix) `input`, `change` (a service, a cell, a request, an
    /// action, a config write), `surface` or `late`: its mark on the
    /// viewer's timeline.
    pub kind: &'static str,
    pub text: String,
}

/// (replayfix) Where the live pointer was from `t` on: on the surface
/// whose root is at `origin` (under the config directory), at `x, y`
/// (logical px), or (`origin` `None`) on none of the replayed output's.
#[derive(Clone, Debug, PartialEq)]
pub struct PointerAt {
    /// Since the window's start.
    pub t: u64,
    pub origin: Option<String>,
    pub x: f32,
    pub y: f32,
}

/// A step on the timeline.
#[derive(Clone, Debug)]
struct Timed {
    t: u64,
    text: String,
    /// Ticks it runs (a time step), 0 for an instant one.
    ticks: u64,
    /// (report review) The first tick it may run at: a step on a
    /// surface comes after the tick the sandbox shows it, which is the
    /// tick after the step that opened it (the live shell's surface
    /// maps a roundtrip after the change, often within the same tick);
    /// (replayfix) two ticks after the live map, the comparison's
    /// tolerance.
    not_before: u64,
}

/// A step at `t` (since the window's start), running `ticks`.
fn push(timed: &mut Vec<Timed>, t: u64, text: String, ticks: u64) {
    push_on(timed, t, text, ticks, 0);
}

/// [`push`] for a step on a surface the sandbox shows from tick
/// `not_before` on.
fn push_on(timed: &mut Vec<Timed>, t: u64, text: String, ticks: u64, not_before: u64) {
    timed.push(Timed {
        t,
        text,
        ticks,
        not_before,
    });
}

/// A bundle's replay, planned.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The window, ns since the process epoch.
    pub start: u64,
    pub end: u64,
    /// What the window starts after.
    pub start_why: String,
    pub output: OutputPick,
    /// When the window starts at the shell's start: up to the first
    /// input (ns since the window's start), the live shell was still
    /// mapping what the sandbox's boot maps at once. 0 otherwise.
    pub boot_until: u64,
    pub warmup_ms: u64,
    /// The traced part's length, ns before the report.
    pub traced_ns: u64,
    pub backend_both: bool,
    pub compositor_poses: bool,
    pub icons: Option<PathBuf>,
    /// Every service field and cell at the window's start, as JSON.
    pub seeds: BTreeMap<String, Json>,
    /// The steps, one per line.
    pub steps: Vec<String>,
    /// The tick period, ns.
    pub period: u64,
    /// What it cannot reproduce.
    pub limits: Vec<String>,
    /// Fidelity notes (a lost value, an unknown surface).
    pub notes: Vec<String>,
    pub presses: Vec<LivePress>,
    pub maps: Vec<LiveMap>,
    pub writes: Vec<LiveWrite>,
    pub events: Vec<Event>,
    /// (replayfix) The live pointer over the window, one entry per
    /// recorded move, press or leave on the replayed output.
    pub pointer: Vec<PointerAt>,
}

/// The live origin of a node's file, under the config directory.
fn rel_origin(origin: &str, config_dir: Option<&Path>) -> String {
    if let Some(d) = config_dir {
        let prefix = format!("{}/", d.display());
        if let Some(rest) = origin.strip_prefix(&prefix) {
            return rest.to_string();
        }
    }
    origin.to_string()
}

/// (replayfix) `origin` under the live config directory, if it is an
/// origin a step can name (`@file.strand:LINE[:COL]`); `None` for
/// anything else, which names no surface or node.
fn origin_name(origin: &str, config_dir: Option<&Path>) -> Option<String> {
    let o = rel_origin(origin, config_dir);
    matches!(
        crate::select::Selector::parse(&format!("@{o}")),
        Ok(crate::select::Selector::Origin { nth: None, .. })
    )
    .then_some(o)
}

/// Compact JSON of `v`.
fn compact(v: &Json) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

/// A word a step line reads back as `text` (quoted, `"` and `\` escaped).
fn quoted(text: &str) -> String {
    let mut s = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// A value as a `set` step's tail: a text quoted, a record or list as
/// JSON, a scalar as written.
fn set_value(v: &Json) -> String {
    match v {
        Json::String(s) => quoted(s),
        Json::Object(_) | Json::Array(_) => compact(v),
        o => compact(o),
    }
}

/// (report review) `v` with each held-back text (`{"~text": N}`) as N
/// `x`s.
fn unheld(v: &Json) -> Json {
    match v {
        Json::Object(o) if o.len() == 1 && o.get("~text").is_some_and(Json::is_u64) => {
            Json::String("x".repeat(o["~text"].as_u64().unwrap_or(0) as usize))
        }
        Json::Object(o) => Json::Object(o.iter().map(|(k, v)| (k.clone(), unheld(v))).collect()),
        Json::Array(a) => Json::Array(a.iter().map(unheld).collect()),
        o => o.clone(),
    }
}

/// A value as an `emit` argument: one word.
fn arg_word(v: &Json) -> String {
    match v {
        Json::String(s) => quoted(s),
        Json::Object(_) | Json::Array(_) => quoted(&compact(v)),
        o => compact(o),
    }
}

/// A TOML basic string.
pub fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Plays keyed diffs (`{"d": …}`) forward on `list`.
pub fn apply_keyed(list: &mut Vec<Json>, d: &[Json]) {
    let idx = |v: &Json| v.as_u64().unwrap_or(0) as usize;
    for diff in d {
        if let Some(a) = diff.get("insert") {
            let i = idx(&a[0]).min(list.len());
            list.insert(i, a[2].clone());
        } else if let Some(a) = diff.get("update") {
            if let Some(slot) = list.get_mut(idx(&a[0])) {
                *slot = a[2].clone();
            }
        } else if let Some(a) = diff.get("remove") {
            let i = idx(&a[0]);
            if i < list.len() {
                list.remove(i);
            }
        } else if let Some(a) = diff.get("move") {
            let from = idx(&a[0]);
            if from < list.len() {
                let v = list.remove(from);
                let to = idx(&a[1]).min(list.len());
                list.insert(to, v);
            }
        } else if let Some(items) = diff.get("reset").and_then(Json::as_array) {
            *list = items.iter().map(|kv| kv[1].clone()).collect();
        }
    }
}

/// Undoes keyed diffs on `list` (the list after them), with `removed`
/// the items they removed or replaced, in the order they went: the list
/// before them. `None` when they do not fit it.
pub fn undo_keyed(list: &[Json], d: &[Json], removed: &[Json]) -> Option<Vec<Json>> {
    let mut list = list.to_vec();
    let mut removed = removed.to_vec();
    let idx = |v: &Json| v.as_u64().unwrap_or(0) as usize;
    for (n, diff) in d.iter().enumerate().rev() {
        if let Some(a) = diff.get("insert") {
            let i = idx(&a[0]);
            if i >= list.len() {
                return None;
            }
            list.remove(i);
        } else if let Some(a) = diff.get("update") {
            let i = idx(&a[0]);
            let v = removed.pop()?;
            *list.get_mut(i)? = v;
        } else if let Some(a) = diff.get("remove") {
            let i = idx(&a[0]).min(list.len());
            let v = removed.pop()?;
            list.insert(i, v);
        } else if let Some(a) = diff.get("move") {
            let to = idx(&a[1]);
            if to >= list.len() {
                return None;
            }
            let v = list.remove(to);
            let from = idx(&a[0]).min(list.len());
            list.insert(from, v);
        } else if diff.get("reset").is_some() {
            // The reset took every item before it: what is left of the
            // removed ones but those of the diffs before it.
            let before = d[..n]
                .iter()
                .filter(|x| x.get("update").is_some() || x.get("remove").is_some())
                .count();
            if removed.len() < before {
                return None;
            }
            list = removed.split_off(before);
        }
    }
    removed.is_empty().then_some(list)
}

/// A record's value as JSON (`None` when lost, truncated or not JSON).
fn value(e: &Entry) -> Option<Json> {
    serde_json::from_slice(e.value.as_deref()?).ok()
}

/// The services a config declares (`service NAME from …`).
pub fn declared_services(config: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![config.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "strand")
                && let Ok(text) = std::fs::read_to_string(&p)
            {
                for line in text.lines() {
                    let mut w = line.split_whitespace();
                    if let (Some("service"), Some(name), Some("from")) =
                        (w.next(), w.next(), w.next())
                    {
                        out.insert(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Whether the config uses `thumbnail` nodes.
fn uses_thumbnails(config: &Path) -> bool {
    let mut stack = vec![config.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "strand")
                && std::fs::read_to_string(&p).is_ok_and(|t| t.contains("thumbnail"))
            {
                return true;
            }
        }
    }
    false
}

fn secs(ns: i128) -> String {
    format!("{:+.2}s", ns as f64 / 1e9)
}

fn is_input(k: u8) -> bool {
    (kind::ENTER..=kind::DRAG).contains(&k)
}

/// What the input ring says of the surfaces and outputs.
#[derive(Debug, Default)]
struct Slots {
    /// Output slot: its name, logical size and scale.
    outputs: HashMap<u16, (String, u32, u32, f64)>,
    /// Surface slot: its root `node_ref`, output slot and kind byte.
    surfaces: HashMap<u16, (String, Option<u16>, u8)>,
    /// Surface slot: refresh period, ns.
    periods: HashMap<u16, u64>,
}

impl Slots {
    fn read(b: &Bundle) -> Slots {
        let mut s = Slots::default();
        for e in &b.inputs {
            match e.rec.kind {
                kind::OUTPUT => {
                    let name = e
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("output{}", e.rec.slot));
                    s.outputs.insert(
                        e.rec.slot,
                        (
                            name,
                            u32::from(e.rec.u16(2)),
                            u32::from(e.rec.u16(4)),
                            f64::from(e.rec.u16(6)) / 120.0,
                        ),
                    );
                }
                kind::MAPPED => {
                    let out = e.rec.u8(8);
                    s.surfaces.insert(
                        e.rec.slot,
                        (
                            node_ref_at(e),
                            (out != u8::MAX).then_some(u16::from(out)),
                            e.rec.u8(9),
                        ),
                    );
                }
                kind::REFRESH => {
                    s.periods.insert(e.rec.slot, u64::from(e.rec.u32(0)));
                }
                _ => {}
            }
        }
        // Surfaces mapped before the ring's oldest record, as
        // `bundle.json` lists them.
        for sr in &b.surfaces {
            s.surfaces.entry(sr.slot).or_insert_with(|| {
                let out = sr.output.as_ref().and_then(|n| {
                    s.outputs
                        .iter()
                        .find(|(_, o)| &o.0 == n)
                        .map(|(slot, _)| *slot)
                });
                (sr.root.clone(), out, 0)
            });
        }
        s
    }

    /// The output name of surface `slot`.
    fn output_of(&self, b: &Bundle, slot: u16) -> Option<String> {
        let (_, out, _) = self.surfaces.get(&slot)?;
        if let Some(o) = out.and_then(|o| self.outputs.get(&o)) {
            return Some(o.0.clone());
        }
        b.surfaces
            .iter()
            .find(|s| s.slot == slot)
            .and_then(|s| s.output.clone())
    }
}

/// Plans `b`'s replay.
pub fn plan(b: &Bundle, opts: &Options) -> Result<Plan, crate::Error> {
    if b.input.is_none() && b.changes.is_none() {
        return Err(crate::Error::Config(format!(
            "{}: the bundle holds no recording (`system.replay` was off): nothing to replay",
            b.dir.display()
        )));
    }
    let end = b.now_ns;
    let live_dir = b.live_config_dir();
    let slots = Slots::read(b);

    // 2. The window.
    let mut start = 0u64;
    let mut start_why = "the shell's start".to_string();
    let mut later = |t: u64, why: String| {
        if t > start && t <= end {
            start = t;
            start_why = why;
        }
    };
    if let Some(first) = b
        .inputs
        .first()
        .filter(|_| b.input.as_ref().is_some_and(|d| d.wrapped))
    {
        later(first.rec.t, "the input ring's oldest record".into());
    }
    if let Some(first) = b
        .change_entries
        .first()
        .filter(|_| b.changes.as_ref().is_some_and(|d| d.wrapped))
    {
        later(first.rec.t, "the changes ring's oldest record".into());
    }
    for e in b.inputs.iter().chain(&b.change_entries) {
        match e.rec.kind {
            kind::RELOAD => later(
                e.rec.t,
                format!("the reload at {}", secs(e.rec.t as i128 - end as i128)),
            ),
            kind::GAP_ENDS | kind::CHANGE_GAP_ENDS => {
                let why = match e.rec.u8(0) {
                    gap::LOCK => "a lock",
                    gap::OFF => "the recorder off",
                    gap::PICK => "an inspector pick",
                    _ => "a gap",
                };
                later(
                    e.rec.t,
                    format!(
                        "the end of {why} at {}",
                        secs(e.rec.t as i128 - end as i128)
                    ),
                );
            }
            _ => {}
        }
    }
    // Bounded by nothing, the window starts at the shell's first
    // surface, where the sandbox's boot is.
    if start == 0
        && let Some(first) = b.inputs.iter().find(|e| e.rec.kind == kind::MAPPED)
    {
        start = first.rec.t.min(end);
    }
    // (report review) Never longer than WINDOW_CAP_NS: a quiet shell's
    // rings may not wrap for hours, and every frame before the traced
    // part is still painted. Rewinding reaches any start inside the
    // rings.
    let capped = end.saturating_sub(start) > WINDOW_CAP_NS;
    if capped {
        start = end - WINDOW_CAP_NS;
        start_why = format!(
            "the replay's cap, {} s before the report",
            WINDOW_CAP_NS / 1_000_000_000
        );
    }
    let in_window = |t: u64| t >= start && t <= end;
    let rel = |t: u64| t.saturating_sub(start);

    // 3. The output.
    // (replayfix) The input on each output since `since`: the last 10 s
    // first, else the whole window (the owner's report came 20 s after
    // the hovering it was about, on another output than the newest
    // surface's).
    let busy_since = |since: u64| {
        let mut busy: BTreeMap<String, usize> = BTreeMap::new();
        for e in &b.inputs {
            if is_input(e.rec.kind)
                && e.rec.t >= since
                && in_window(e.rec.t)
                && let Some(o) = slots.output_of(b, e.rec.slot)
            {
                *busy.entry(o).or_default() += 1;
            }
        }
        busy
    };
    let busiest = |busy: &BTreeMap<String, usize>| {
        busy.iter()
            .max_by_key(|(n, c)| (**c, std::cmp::Reverse((*n).clone())))
            .map(|(n, c)| (n.clone(), *c))
    };
    let recent = busiest(&busy_since(end.saturating_sub(BUSY_NS)));
    let anywhere = busiest(&busy_since(start));
    let newest_mapped = b
        .inputs
        .iter()
        .rev()
        .find(|e| e.rec.kind == kind::MAPPED)
        .and_then(|e| slots.output_of(b, e.rec.slot))
        .or_else(|| b.surfaces.iter().find_map(|s| s.output.clone()));
    let (name, why) = match &opts.output {
        Some(n) => (n.clone(), "named by --output".to_string()),
        None => match (recent, anywhere) {
            (Some((n, c)), _) => (n, format!("the most input in the last 10 s ({c} records)")),
            (None, Some((n, c))) => (
                n,
                format!("the most input in the window, none in its last 10 s ({c} records)"),
            ),
            (None, None) => match newest_mapped {
                Some(n) => (n, "the newest mapped surface is on it".to_string()),
                None => ("DP-1".to_string(), "no output is recorded".to_string()),
            },
        },
    };
    let from_json = b.json["outputs"]
        .as_array()
        .and_then(|a| a.iter().find(|o| o["name"].as_str() == Some(&name)));
    let (mut width, mut height, mut scale) = (1920, 1080, 1.0);
    if let Some(o) = slots.outputs.values().find(|o| o.0 == name) {
        (width, height, scale) = (o.1, o.2, o.3);
    } else if let Some(o) = from_json {
        width = o["size"][0].as_u64().unwrap_or(1920) as u32;
        height = o["size"][1].as_u64().unwrap_or(1080) as u32;
        scale = o["scale"].as_f64().unwrap_or(1.0);
    } else if opts.output.is_some() {
        return Err(crate::Error::Usage(format!(
            "--output {name}: the bundle records no such output"
        )));
    }
    if width == 0 || height == 0 {
        (width, height) = (1920, 1080);
    }
    if !(scale > 0.0 && scale <= 4.0) {
        scale = 1.0;
    }
    let on_output = |slot: u16| slots.output_of(b, slot).is_none_or(|o| o == name);
    let mut periods: BTreeMap<u64, usize> = BTreeMap::new();
    for (slot, p) in &slots.periods {
        if on_output(*slot) && *p > 0 {
            *periods.entry(*p).or_default() += 1;
        }
    }
    let hz = periods
        .iter()
        .max_by_key(|(_, c)| **c)
        .map_or(60, |(p, _)| {
            ((1e9 / *p as f64).round() as u32).clamp(1, 1000)
        });
    let period = 1_000_000_000 / u64::from(hz);
    let text = |k: &str| from_json.and_then(|o| o[k].as_str()).map(str::to_string);
    let (mut id, mut description) = (text("id"), text("description"));
    let mut early_notes = Vec::new();
    if id.is_none()
        && let (Some(make), Some(model)) = (text("make"), text("model"))
        && recovered_id(b, &make, &model).is_none()
        && b.json["outputs"].as_array().is_some_and(|a| {
            a.iter()
                .filter(|o| {
                    o["make"].as_str() == Some(&make) && o["model"].as_str() == Some(&model)
                })
                .count()
                > 1
        })
    {
        early_notes.push(format!(
            "the screen's id is not known: the bundle is from before its outputs carried their \
             ids, and another output is a {make} {model} too, so the cells kept per screen \
             cannot tell which is this one's; they start at their defaults"
        ));
    }
    if id.is_none()
        && let (Some(make), Some(model)) = (text("make"), text("model"))
        && let Some((i, d)) = recovered_id(b, &make, &model)
    {
        early_notes.push(format!(
            "the screen's id `{i}` is read from the cells kept per screen: the bundle is from \
             before its outputs carried their ids"
        ));
        id = Some(i);
        description = description.or(Some(d));
    }
    let output = OutputPick {
        id,
        make: text("make"),
        model: text("model"),
        description,
        name: name.clone(),
        width,
        height,
        scale,
        hz,
        why,
    };

    // The surfaces' roots: their origins, from the changes ring's root
    // records and `nodes.json`.
    // (replayfix) A name that is no origin (the `?` a recorder wrote
    // for a surface with no place in the config, its own error panel)
    // names no surface: its root is not known.
    let mut origins: HashMap<String, String> = HashMap::new();
    for e in &b.change_entries {
        if e.rec.kind == kind::ROOT
            && let Some(o) = e
                .name
                .as_deref()
                .and_then(|o| origin_name(o, live_dir.as_deref()))
        {
            origins.insert(node_ref_at(e), o);
        }
    }
    let node_of = |r: &str| -> Option<NodeRef> {
        let n = b.nodes.get(r)?;
        Some(NodeRef {
            origin: origin_name(n["origin"].as_str()?, live_dir.as_deref())?,
            id: n["id"].as_str().map(str::to_string),
            key: n["key"].as_str().map(str::to_string),
        })
    };
    let origin_of_root = |r: &str| -> Option<String> {
        origins
            .get(r)
            .cloned()
            .or_else(|| node_of(r).map(|n| n.origin))
    };
    let sel_of = |slot: u16| -> Option<String> {
        let (root, _, _) = slots.surfaces.get(&slot)?;
        origin_of_root(root).map(|o| format!("@{o}"))
    };

    // 4. Rewinding.
    let declared = declared_services(&b.config_dir());
    let services_known = known_services();
    let known = |svc: &str| services_known.contains(svc) || declared.contains(svc);
    let mut notes = early_notes;
    let mut limits = Vec::new();
    let mut cur: BTreeMap<String, Json> = BTreeMap::new();
    let mut unmocked: BTreeMap<String, usize> = BTreeMap::new();
    if let Some(svcs) = b.services.as_object() {
        for (svc, s) in svcs {
            if SANDBOX_OWNS.contains(&svc.as_str()) {
                continue;
            }
            if !known(svc) {
                unmocked.entry(svc.clone()).or_default();
                continue;
            }
            if let Some(fields) = s["fields"].as_object() {
                for (f, v) in fields {
                    cur.insert(format!("{svc}.{f}"), v.clone());
                }
            }
        }
    }
    if let Some(cells) = b.cells.as_object() {
        for (p, v) in cells {
            cur.insert(p.clone(), v.clone());
        }
    }
    // Each field's records in the window that its replay plays forward
    // (those after the one it stopped rewinding at).
    let mut stopped: HashMap<String, u64> = HashMap::new();
    // (replayfix) A record whose value is gone (over the cap, or its
    // arena overwritten) no longer stops its field: the value after it
    // is known (the rewind has reached it), so the replay sets that
    // there (by the record's index) and rewinds on past it. The field's
    // older records still hold their own old values (a list's diffs are
    // undone from the list after the lost one, as if it had changed
    // nothing), so one change over the cap no longer throws away the
    // field's history before it.
    let mut bridged: HashMap<usize, Json> = HashMap::new();
    // Per field: its newest lost record's time, why, its kind, and how
    // many were lost.
    let mut lost_of: BTreeMap<String, (u64, &'static str, u8, usize)> = BTreeMap::new();
    for (i, e) in b.change_entries.iter().enumerate().rev() {
        if !in_window(e.rec.t) || !matches!(e.rec.kind, kind::FIELD | kind::KEYED | kind::CELL) {
            continue;
        }
        let Some(name) = e.name.clone() else {
            continue;
        };
        if stopped.contains_key(&name) {
            continue;
        }
        let svc = name.split('.').next().unwrap_or("");
        if e.rec.kind != kind::CELL && (SANDBOX_OWNS.contains(&svc) || !known(svc)) {
            continue;
        }
        if e.rec.flags & (flags::LOST | flags::TRUNCATED) != 0 {
            if let Some(after) = cur.get(&name) {
                bridged.insert(i, after.clone());
            }
            let what = if e.rec.flags & flags::TRUNCATED != 0 {
                "was over the value cap"
            } else {
                "was lost from the ring's arena"
            };
            lost_of
                .entry(name)
                .or_insert((e.rec.t, what, e.rec.kind, 0))
                .3 += 1;
            continue;
        }
        let v = value(e);
        let before = match (e.rec.kind, &v) {
            (kind::FIELD | kind::CELL, Some(Json::Array(pair))) if pair.len() == 2 => {
                Some(pair[0].clone())
            }
            (kind::KEYED, Some(j)) => {
                let now = cur
                    .get(&name)
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default();
                let d = j["d"].as_array().cloned().unwrap_or_default();
                let r = j["r"].as_array().cloned().unwrap_or_default();
                undo_keyed(&now, &d, &r).map(Json::Array)
            }
            _ => None,
        };
        match before {
            Some(old) => {
                cur.insert(name, old);
            }
            None => {
                notes.push(format!(
                    "`{name}`: its change at {} could not be undone (it does not fit the value \
                     after it); it starts at the oldest value known, the one after it",
                    secs(e.rec.t as i128 - end as i128)
                ));
                stopped.insert(name, e.rec.t);
            }
        }
    }
    for (name, (t, what, k, n)) in &lost_of {
        let more = if *n > 1 {
            format!(", and the values of {} older ones are gone too", n - 1)
        } else {
            String::new()
        };
        let how = match *k {
            kind::KEYED => {
                "the replay sets the list after it whole there, and undoes the changes before it \
                 from that list as if it had changed nothing"
            }
            kind::CELL => "the changes before it are undone as recorded",
            _ => {
                "the replay sets the value after it there, and undoes the changes before it as \
                 recorded"
            }
        };
        notes.push(format!(
            "`{name}`: its change at {} {what}{more}; {how}",
            secs(*t as i128 - end as i128)
        ));
    }
    let seeds = cur.clone();

    // 5. The steps.
    let mut timed: Vec<Timed> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    let mut presses = Vec::new();
    let mut maps = Vec::new();
    let mut writes = Vec::new();
    let mut track: Vec<PointerAt> = Vec::new();
    let mut other_output = 0usize;
    let mut unknown_surface = 0usize;
    let mut typed = 0u64;
    let mut password_spans = 0usize;
    let mut pastes = 0usize;
    let mut click_aways = 0usize;
    let mut sideways = 0usize;
    let mut drags = 0usize;
    let mut backend_both = b.surfaces.iter().any(|s| s.backend.starts_with("gpu"));
    let mut pointer: Option<(u16, f32, f32)> = None;
    let mut focus: Option<u16> = None;
    let mut last_press: Option<usize> = None;
    // (report review) Each surface slot's first tick shown in the
    // sandbox, after its live `MAPPED` in the window.
    let mut shown_from: HashMap<u16, u64> = HashMap::new();
    let ticks_of = |ms: u64| (ms * u64::from(hz)).div_ceil(1000).max(1);
    let button_name = |b: u16| match b {
        0 => Some("left"),
        1 => Some("right"),
        2 => Some("middle"),
        _ => None,
    };
    for (idx, e) in b.inputs.iter().enumerate() {
        let t = e.rec.t;
        match e.rec.kind {
            kind::MAPPED | kind::UNMAPPED if in_window(t) => {
                let root = match e.rec.kind {
                    kind::MAPPED => Some(node_ref_at(e)),
                    _ => slots.surfaces.get(&e.rec.slot).map(|s| s.0.clone()),
                };
                if e.rec.kind == kind::MAPPED {
                    // (replayfix) Up to the comparison's tolerance after
                    // the live map: a hover popup the replay opens a tick
                    // later than the live shell did is still in time for
                    // the pointer that entered it.
                    shown_from.insert(
                        e.rec.slot,
                        (rel(t) + period / 2) / period + super::fidelity::MAP_FRAMES,
                    );
                }
                if on_output(e.rec.slot)
                    && let Some(o) = root.as_deref().and_then(origin_of_root)
                {
                    let mapped = e.rec.kind == kind::MAPPED;
                    events.push(Event {
                        kind: "surface",
                        t: rel(t),
                        text: format!("surface {} @{o}", if mapped { "mapped" } else { "gone" }),
                    });
                    maps.push(LiveMap {
                        t: rel(t),
                        origin: o,
                        mapped,
                    });
                }
                continue;
            }
            kind::BACKEND if in_window(t) && e.rec.u8(0) == 1 => {
                backend_both = true;
                continue;
            }
            kind::LATE if in_window(t) => {
                if let Some(sel) = sel_of(e.rec.slot).filter(|_| on_output(e.rec.slot)) {
                    let missed = u64::from(e.rec.u16(0));
                    let at = rel(t).saturating_sub(missed * period);
                    let nb = shown_from.get(&e.rec.slot).copied().unwrap_or(0);
                    push_on(
                        &mut timed,
                        at,
                        format!("drop {missed} surface={sel}"),
                        0,
                        nb,
                    );
                    events.push(Event {
                        kind: "late",
                        t: rel(t),
                        text: format!("late frame on {sel}: {missed} missed"),
                    });
                }
                continue;
            }
            k if is_input(k) && in_window(t) => {}
            _ => continue,
        }
        // (replayfix) The pointer's place for the viewer: off the
        // replayed output's known surfaces when it enters another.
        let off = |track: &mut Vec<PointerAt>| {
            if matches!(e.rec.kind, kind::ENTER | kind::MOTION)
                && track.last().is_some_and(|p| p.origin.is_some())
            {
                track.push(PointerAt {
                    t: rel(t),
                    origin: None,
                    x: 0.0,
                    y: 0.0,
                });
            }
        };
        if !on_output(e.rec.slot) {
            off(&mut track);
            other_output += 1;
            continue;
        }
        let Some(sel) = sel_of(e.rec.slot) else {
            off(&mut track);
            unknown_surface += 1;
            continue;
        };
        let at = rel(t);
        let r = &e.rec;
        let nb = shown_from.get(&r.slot).copied().unwrap_or(0);
        let xy = |r: &strand_core::flight::Rec| (r.f32(0), r.f32(4));
        // The pointer moved to `x, y` on this surface, and keyboard
        // focus on it, each a step only when it changes.
        macro_rules! to_pointer {
            ($x:expr, $y:expr) => {{
                let (x, y): (f32, f32) = ($x, $y);
                track.push(PointerAt {
                    t: at,
                    origin: Some(sel[1..].to_string()),
                    x,
                    y,
                });
                if pointer != Some((r.slot, x, y)) {
                    push_on(
                        &mut timed,
                        at,
                        format!("pointer {x:.2},{y:.2} on {sel}"),
                        0,
                        nb,
                    );
                    pointer = Some((r.slot, x, y));
                }
            }};
        }
        macro_rules! to_focus {
            () => {{
                if focus != Some(r.slot) {
                    push_on(&mut timed, at, format!("focus {sel}"), 0, nb);
                    focus = Some(r.slot);
                }
            }};
        }
        match r.kind {
            kind::ENTER | kind::MOTION => {
                let (x, y) = xy(r);
                to_pointer!(x, y);
            }
            kind::LEAVE => {
                // (replayfix) A leave the compositor sent because a
                // surface mapped under the pointer (a hover popup over
                // its own trigger) is followed at once by an enter on
                // that surface. The replay moves the pointer there when
                // it shows, which leaves the trigger then, never before:
                // an early leave cancels the hover that opens the popup.
                let handed = b.inputs[idx + 1..]
                    .iter()
                    .find(|n| is_input(n.rec.kind))
                    .is_some_and(|n| {
                        n.rec.kind == kind::ENTER
                            && n.rec.slot != r.slot
                            && n.rec.t.saturating_sub(t) <= HANDOFF_NS
                            && in_window(n.rec.t)
                            && on_output(n.rec.slot)
                            && sel_of(n.rec.slot).is_some()
                    });
                if !handed {
                    push(&mut timed, at, "leave".into(), 0);
                }
                pointer = None;
                track.push(PointerAt {
                    t: at,
                    origin: None,
                    x: 0.0,
                    y: 0.0,
                });
            }
            kind::BUTTON => {
                let (x, y) = xy(r);
                let Some(bn) = button_name(r.u16(8)) else {
                    notes.push(format!("a press of button {} is not replayed", r.u16(8)));
                    continue;
                };
                to_pointer!(x, y);
                let pressed = r.u8(10) == 1;
                push(
                    &mut timed,
                    at,
                    format!("{} {bn}", if pressed { "press" } else { "release" }),
                    0,
                );
                events.push(Event {
                    kind: "input",
                    t: at,
                    text: format!(
                        "input {} {bn} at {x:.1},{y:.1} on {sel}",
                        if pressed { "press" } else { "release" }
                    ),
                });
                if pressed {
                    presses.push(LivePress {
                        t: at,
                        hit: None,
                        unknown: None,
                    });
                    last_press = Some(presses.len() - 1);
                }
            }
            kind::HIT => {
                if let Some(i) = last_press.take() {
                    let r = node_ref_at(e);
                    match node_of(&r) {
                        Some(n) => {
                            if let Some(ev) = events
                                .iter_mut()
                                .rev()
                                .find(|ev| ev.text.starts_with("input press"))
                            {
                                let _ = write!(ev.text, " hit {n}");
                            }
                            presses[i].hit = Some(n);
                        }
                        None => presses[i].unknown = Some(r),
                    }
                }
            }
            kind::AXIS => {
                let source = r.u8(8);
                let dy = r.f32(4);
                let discrete = r.i16(10);
                // (report review) The format scrolls vertically only.
                if r.f32(0) != 0.0 {
                    sideways += 1;
                }
                if pointer.map(|p| p.0) != Some(r.slot) {
                    // The pointer's place is the last recorded on it.
                    push_on(&mut timed, at, format!("pointer 1,1 on {sel}"), 0, nb);
                    pointer = Some((r.slot, 1.0, 1.0));
                }
                if source == 2 || source == 3 {
                    // A touchpad's frames, one swipe a frame.
                    push(
                        &mut timed,
                        at,
                        format!("swipe {dy:.2} over {}", 1000 / u64::from(hz)),
                        1,
                    );
                } else if discrete != 0 {
                    push(
                        &mut timed,
                        at,
                        format!("scroll {:.3}", f64::from(discrete) / 120.0),
                        0,
                    );
                } else if dy != 0.0 {
                    push(
                        &mut timed,
                        at,
                        format!(
                            "scroll {:.3}",
                            f64::from(dy) / crate::checks::teleport::NOTCH_PX
                        ),
                        0,
                    );
                }
                events.push(Event {
                    kind: "input",
                    t: at,
                    text: format!("input scroll {dy:.1} px on {sel}"),
                });
            }
            kind::KEYBOARD_ENTER => {
                push_on(&mut timed, at, format!("focus {sel}"), 0, nb);
                focus = Some(r.slot);
            }
            kind::KEYBOARD_LEAVE => {
                if focus == Some(r.slot) {
                    push(&mut timed, at, "unfocus".into(), 0);
                    focus = None;
                }
            }
            kind::KEY => {
                let Some(name) = e.name.clone() else {
                    continue;
                };
                to_focus!();
                let m = r.u16(2);
                let mut mods = String::new();
                for (bit, w) in [
                    (modifiers::CTRL, " +ctrl"),
                    (modifiers::SHIFT, " +shift"),
                    (modifiers::ALT, " +alt"),
                    (modifiers::SUPER, " +super"),
                ] {
                    if m & bit != 0 {
                        mods.push_str(w);
                    }
                }
                let phase = if r.u8(4) == 0 { "up" } else { "down" };
                push(&mut timed, at, format!("key {name}{mods} {phase}"), 0);
                events.push(Event {
                    kind: "input",
                    t: at,
                    text: format!("input key {name}{mods} {phase} on {sel}"),
                });
            }
            kind::TEXT => {
                to_focus!();
                let password = r.u8(8) == 1;
                let n = if password { 8 } else { u64::from(r.u32(0)) };
                if password {
                    password_spans += 1;
                } else {
                    typed += n;
                }
                let over = u64::from(r.u32(4));
                let ticks = if over * u64::from(hz) >= 1000 {
                    ticks_of(over)
                } else {
                    0
                };
                let text = if ticks > 0 {
                    format!("type {n} over {}", ticks * 1000 / u64::from(hz))
                } else {
                    format!("type {n}")
                };
                push(&mut timed, at, text, ticks);
                events.push(Event {
                    kind: "input",
                    t: at,
                    text: format!(
                        "input {} on {sel}",
                        if password {
                            "a password span".to_string()
                        } else {
                            format!("{n} keys typed")
                        }
                    ),
                });
            }
            kind::PASTE => {
                to_focus!();
                let password = r.u8(4) == 1;
                let n = if password { 8 } else { r.u32(0) };
                pastes += 1;
                push(&mut timed, at, format!("paste {n}"), 0);
                events.push(Event {
                    kind: "input",
                    t: at,
                    text: format!("input paste of {n} characters on {sel}"),
                });
            }
            kind::CLICK_AWAY => click_aways += 1,
            kind::DRAG => drags += 1,
            _ => {}
        }
    }

    // The changes, played forward.
    let mut lists: BTreeMap<String, Vec<Json>> = BTreeMap::new();
    // (replayfix) Each driven field's value as played so far, for a lost
    // record's value after it, set only where it changes something.
    let mut played: HashMap<String, Json> = HashMap::new();
    let mut answered_at: HashMap<String, u64> = HashMap::new();
    for (i, e) in b.change_entries.iter().enumerate() {
        let t = e.rec.t;
        if !in_window(t) {
            continue;
        }
        let at = rel(t);
        let Some(name) = e.name.clone() else {
            continue;
        };
        let svc = name.split('.').next().unwrap_or("").to_string();
        match e.rec.kind {
            kind::FIELD | kind::KEYED | kind::EVENT | kind::FETCH | kind::WRITE
                if SANDBOX_OWNS.contains(&svc.as_str()) =>
            {
                continue;
            }
            kind::FIELD | kind::KEYED | kind::EVENT | kind::FETCH if !known(&svc) => {
                *unmocked.entry(svc).or_default() += 1;
                continue;
            }
            _ => {}
        }
        if stopped.get(&name).is_some_and(|s| t <= *s) {
            continue;
        }
        if let Some(after) = bridged.get(&i)
            && matches!(e.rec.kind, kind::FIELD | kind::KEYED)
        {
            let now = played
                .get(&name)
                .or_else(|| seeds.get(&name))
                .cloned()
                .unwrap_or(Json::Null);
            if e.rec.kind == kind::KEYED {
                lists.insert(name.clone(), after.as_array().cloned().unwrap_or_default());
            }
            played.insert(name.clone(), after.clone());
            if now != *after {
                let shown = if e.rec.kind == kind::KEYED {
                    compact(after)
                } else {
                    set_value(after)
                };
                push(&mut timed, at, format!("set {name} {shown}"), 0);
                events.push(Event {
                    kind: "change",
                    t: at,
                    text: format!(
                        "service {name} = {} (its change was not recorded: the value after it)",
                        compact(after)
                    ),
                });
            }
            continue;
        }
        let v = value(e);
        match e.rec.kind {
            kind::FIELD => {
                if let Some(Json::Array(pair)) = &v
                    && pair.len() == 2
                {
                    played.insert(name.clone(), pair[1].clone());
                    push(
                        &mut timed,
                        at,
                        format!("set {name} {}", set_value(&pair[1])),
                        0,
                    );
                    events.push(Event {
                        kind: "change",
                        t: at,
                        text: format!("service {name} = {}", compact(&pair[1])),
                    });
                }
            }
            kind::KEYED => {
                if let Some(j) = &v {
                    let list = lists.entry(name.clone()).or_insert_with(|| {
                        seeds
                            .get(&name)
                            .and_then(Json::as_array)
                            .cloned()
                            .unwrap_or_default()
                    });
                    apply_keyed(list, j["d"].as_array().map_or(&[][..], Vec::as_slice));
                    let whole = Json::Array(list.clone());
                    played.insert(name.clone(), whole.clone());
                    push(&mut timed, at, format!("set {name} {}", compact(&whole)), 0);
                    events.push(Event {
                        kind: "change",
                        t: at,
                        text: format!("service {name}: {}", compact(&j["d"])),
                    });
                }
            }
            kind::EVENT => {
                if let Some(Json::Array(args)) = &v {
                    let words: Vec<String> = args.iter().map(arg_word).collect();
                    let tail = if words.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", words.join(" "))
                    };
                    push(&mut timed, at, format!("emit {name}{tail}"), 0);
                    events.push(Event {
                        kind: "change",
                        t: at,
                        text: format!("service event {name}{tail}"),
                    });
                }
            }
            kind::FETCH => {
                let step = match &v {
                    Some(j) if j.get("ok").is_some() => {
                        format!("answer {name} {}", compact(&j["ok"]))
                    }
                    Some(j) => match &j["err"] {
                        Json::String(s) => format!("answer {name} error {}", quoted(s)),
                        other => {
                            let n = other["~text"].as_u64().unwrap_or(1) as usize;
                            format!("answer {name} error {}", quoted(&"x".repeat(n)))
                        }
                    },
                    None => continue,
                };
                // Queued as early as it can be: after the method's last
                // answer (answers are given in the order queued).
                let when = answered_at.get(&name).copied().unwrap_or(0);
                answered_at.insert(name.clone(), at);
                push(&mut timed, when, step, 0);
                events.push(Event {
                    kind: "change",
                    t: at,
                    text: format!(
                        "service {name} answered{}",
                        if v.as_ref().is_some_and(|j| j.get("err").is_some()) {
                            " with an error"
                        } else {
                            ""
                        }
                    ),
                });
            }
            kind::CELL => {
                if let Some(Json::Array(pair)) = &v
                    && pair.len() == 2
                {
                    events.push(Event {
                        kind: "change",
                        t: at,
                        text: format!("cell {name} = {} (live)", compact(&pair[1])),
                    });
                    writes.push(LiveWrite {
                        t: at,
                        path: name,
                        value: pair[1].clone(),
                    });
                }
            }
            kind::REQUEST => {
                // (report review) `strand set`, `toggle` or `reset` from
                // outside the shell (a keybinding): replayed at its time,
                // a held-back text as `x`s as long as it was.
                let step = match (e.rec.u8(0), &v) {
                    (request::TOGGLE, _) => format!("toggle {name}"),
                    (_, Some(j)) => format!("set {name} {}", set_value(&unheld(j))),
                    (_, None) => continue,
                };
                push(&mut timed, at, step.clone(), 0);
                events.push(Event {
                    kind: "change",
                    t: at,
                    text: format!("request {step}"),
                });
            }
            kind::ACTION => events.push(Event {
                kind: "change",
                t: at,
                text: format!(
                    "action {name} {}",
                    if e.rec.u8(5) == 1 {
                        "started"
                    } else if e.rec.i32(0) == 0 {
                        "succeeded"
                    } else {
                        "failed"
                    }
                ),
            }),
            kind::WRITE => events.push(Event {
                kind: "change",
                t: at,
                text: format!(
                    "config wrote {name} = {}",
                    v.as_ref().map_or("?".into(), compact)
                ),
            }),
            kind::ROOT | kind::ROOT_GONE => {}
            _ => {}
        }
    }

    // Limits.
    if typed > 0 {
        limits.push(format!("typed text: {typed} keys typed, each typed as `x`"));
    }
    if password_spans > 0 {
        limits.push(format!(
            "typed text: {password_spans} password spans, each typed as eight `x`s"
        ));
    }
    if pastes > 0 {
        limits.push(format!("pastes: {pastes}, each of `x`s, as long as it was"));
    }
    for (svc, n) in &unmocked {
        limits.push(format!(
            "service `{svc}` has no stand-in in the sandbox ({n} changes not replayed)"
        ));
    }
    limits.push(
        "other programs' windows: `thumbnail` nodes draw nothing, and the compositor's blur and its own animations are not modelled"
            .into(),
    );
    if uses_thumbnails(&b.config_dir()) {
        limits.push("the config draws `thumbnail` nodes, which draw nothing here".into());
    }
    if other_output > 0 {
        limits.push(format!(
            "other outputs: {other_output} input records on outputs other than {name} not replayed"
        ));
    }
    let outputs: BTreeSet<&str> = slots.outputs.values().map(|o| o.0.as_str()).collect();
    for o in outputs {
        if o != name {
            limits.push(format!(
                "output {o} is not replayed (the sandbox has one screen)"
            ));
        }
    }
    if unknown_surface > 0 {
        limits.push(format!(
            "{unknown_surface} input records on surfaces whose root is not known are not replayed \
             (a surface Strand makes itself, such as its error panel, has no place in the config)"
        ));
    }
    if sideways > 0 {
        limits.push(format!(
            "{sideways} horizontal scrolls are not replayed (a scenario scrolls vertically only)"
        ));
    }
    if click_aways > 0 {
        limits.push(format!(
            "{click_aways} clicks outside the shell's surfaces are not replayed"
        ));
    }
    if drags > 0 {
        limits.push(format!(
            "{drags} drag records from other programs are not replayed"
        ));
    }
    for (p, bytes, why) in b.missing() {
        limits.push(format!("missing file {p} ({bytes} bytes: {why})"));
    }
    if capped {
        limits.push(format!(
            "the window starts at {start_why}: what happened earlier is not replayed"
        ));
    } else if start_why != "the shell's start" {
        limits.push(format!("the window starts after {start_why}"));
    }

    let boot_until = if start_why == "the shell's start" {
        b.inputs
            .iter()
            .filter(|e| is_input(e.rec.kind) && in_window(e.rec.t))
            .map(|e| rel(e.rec.t))
            .next()
            .unwrap_or(end - start)
    } else {
        0
    };

    // The traced part and the marks.
    let len = end.saturating_sub(start);
    let traced_ns = match opts.traced {
        None => len,
        Some(s) => ((s * 1e9) as u64).min(len),
    };
    let warmup_ms = (len - traced_ns) / 1_000_000;
    let traced_at = len - traced_ns;
    push(&mut timed, traced_at, "mark traced".into(), 0);
    for (label, before) in [("-5s", 5_000_000_000u64), ("-2s", 2_000_000_000)] {
        if len >= before && len - before > traced_at {
            push(&mut timed, len - before, format!("mark {label}"), 0);
        }
    }

    // The timeline, quantised to ticks.
    // Stable: steps at the same time keep the order they were planned in.
    timed.sort_by_key(|s| s.t);
    let mut steps = Vec::new();
    let mut tick = 0u64;
    for s in timed {
        let want = ((s.t + period / 2) / period).max(s.not_before);
        if want > tick {
            steps.push(format!("frames {}", want - tick));
            tick = want;
        }
        steps.push(s.text);
        tick += s.ticks;
    }
    let end_tick = (len + period / 2) / period;
    if end_tick > tick {
        steps.push(format!("frames {}", end_tick - tick));
    }
    steps.push("mark report".into());
    steps.push("shot report".into());
    steps.push("frames 1".into());
    events.sort_by_key(|e| e.t);

    let compositor_poses = {
        let g = b.diagnostics["compositor"]["globals"].as_array();
        let has = |n: &str| g.is_some_and(|g| g.iter().any(|x| x["interface"] == n));
        has("wp_alpha_modifier_v1") && has("wp_viewporter")
    };
    let icons = Some(b.config_dir().join("icons")).filter(|p| p.is_dir());
    Ok(Plan {
        start,
        end,
        start_why,
        output,
        boot_until,
        warmup_ms,
        traced_ns,
        backend_both,
        compositor_poses,
        icons,
        seeds,
        steps,
        period,
        limits,
        notes,
        presses,
        maps,
        writes,
        events,
        pointer: track,
    })
}

/// (replayfix) A screen's id for a bundle from before its outputs
/// carried their ids (format 1 before replayfix): an id is `make |
/// model | description` (strand-surface's monitor identity), and a cell
/// kept per screen names it (`Notch[<id>].grown`). The one such id that
/// starts with this output's make and model, when no other output has
/// both, and its description.
pub fn recovered_id(b: &Bundle, make: &str, model: &str) -> Option<(String, String)> {
    let twins = b.json["outputs"].as_array().map_or(0, |a| {
        a.iter()
            .filter(|o| o["make"].as_str() == Some(make) && o["model"].as_str() == Some(model))
            .count()
    });
    if twins > 1 {
        return None;
    }
    let prefix = format!("{make} | {model} | ");
    let mut found = BTreeSet::new();
    let names = b
        .cells
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys().cloned())
        .chain(
            b.change_entries
                .iter()
                .filter_map(|e| (e.rec.kind == kind::CELL).then(|| e.name.clone()).flatten()),
        );
    for n in names {
        let Some(open) = n.find('[') else {
            continue;
        };
        let Some(close) = n[open..].find("].").map(|c| open + c) else {
            continue;
        };
        let inner = &n[open + 1..close];
        if inner.starts_with(&prefix) {
            found.insert(inner.to_string());
        }
    }
    let mut it = found.into_iter();
    let one = it.next()?;
    if it.next().is_some() {
        return None;
    }
    let description = one[prefix.len()..].to_string();
    Some((one, description))
}

/// The services the sandbox's schema has.
fn known_services() -> BTreeSet<String> {
    let services = strand_services_schema::schemas();
    match strand_compiler::schema::Schema::builtin_with(&services) {
        Ok(s) => s.services.keys().cloned().collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// A scenario id from a bundle's name.
pub fn scenario_id(name: &str) -> String {
    let mut id: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    id = id.trim_start_matches('.').to_string();
    format!("report-{id}")
}

impl Plan {
    /// (replayfix) The traced part in words: `the whole window`, or `the
    /// last 20.0 s` (`--traced 20`).
    pub fn traced_words(&self) -> String {
        if self.traced_ns >= self.end - self.start {
            "the whole window".into()
        } else {
            format!("the last {:.1} s", self.traced_ns as f64 / 1e9)
        }
    }

    /// The window start's wall time, RFC 3339 at the bundle's offset.
    pub fn wall(&self, b: &Bundle) -> String {
        let unix = b.epoch_unix_ns as i128 + self.start as i128;
        let offset = chrono::DateTime::parse_from_rfc3339(&b.wall)
            .map(|d| d.offset().local_minus_utc())
            .unwrap_or(0);
        let off =
            chrono::FixedOffset::east_opt(offset).or_else(|| chrono::FixedOffset::east_opt(0));
        let secs = unix.div_euclid(1_000_000_000) as i64;
        let nanos = unix.rem_euclid(1_000_000_000) as u32;
        match (chrono::DateTime::from_timestamp(secs, nanos), off) {
            (Some(t), Some(off)) => t
                .with_timezone(&off)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            _ => "1970-01-01T00:00:00.000+00:00".into(),
        }
    }

    /// The scenario, as text.
    pub fn scenario(&self, b: &Bundle, id: &str) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "id = {}", toml_str(id));
        let about = format!(
            "The replay of the glitch report {:?} ({}): {} s up to the report, traced: {}; written by `strand-eval replay`.",
            b.words,
            b.dir.display(),
            (self.end - self.start) / 1_000_000_000,
            self.traced_words()
        );
        let _ = writeln!(s, "about = {}", toml_str(&about));
        // (replayfix) The screen is the live output: per-screen data
        // (`workspaces.slots(screen)`) and the cells kept per screen
        // name it.
        let o = &self.output;
        let mut who = format!(", name = {}", toml_str(&o.name));
        for (k, v) in [
            ("id", &o.id),
            ("make", &o.make),
            ("model", &o.model),
            ("description", &o.description),
        ] {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                let _ = write!(who, ", {k} = {}", toml_str(v));
            }
        }
        let _ = writeln!(
            s,
            "screen = {{ width = {}, height = {}, scale = {}{who} }}",
            o.width, o.height, o.scale
        );
        let _ = writeln!(s, "hz = {}", self.output.hz);
        let _ = writeln!(
            s,
            "config_dir = {}",
            toml_str(&b.config_dir().display().to_string())
        );
        let _ = writeln!(s, "fonts = \"local\"");
        let _ = writeln!(s, "wall = {}", toml_str(&self.wall(b)));
        let _ = writeln!(s, "warmup = {}", self.warmup_ms);
        if self.backend_both {
            let _ = writeln!(s, "backend = \"both\"");
        }
        if self.compositor_poses {
            let _ = writeln!(s, "compositor_poses = true");
        }
        if let Some(i) = &self.icons {
            let _ = writeln!(s, "icons = {}", toml_str(&i.display().to_string()));
        }
        if !self.seeds.is_empty() {
            let _ = writeln!(s, "\n[seed]");
            for (p, v) in &self.seeds {
                let _ = writeln!(s, "{} = {}", toml_str(p), toml_str(&compact(v)));
            }
        }
        let _ = writeln!(s, "\n=== steps");
        for l in &self.steps {
            let _ = writeln!(s, "{l}");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keyed_diffs_undo_to_the_list_before_them() {
        let before = vec![json!("a"), json!("b"), json!("c")];
        let diffs = vec![
            json!({"insert": [1, "x", "x"]}),
            json!({"update": [0, "a", "A"]}),
            json!({"remove": [3, "c"]}),
            json!({"move": [0, 2, "A"]}),
        ];
        let mut after = before.clone();
        apply_keyed(&mut after, &diffs);
        assert_eq!(after, vec![json!("x"), json!("b"), json!("A")]);
        let removed = vec![json!("a"), json!("c")];
        assert_eq!(undo_keyed(&after, &diffs, &removed), Some(before.clone()));
        // A reset takes every item before it.
        let reset = vec![
            json!({"reset": [["q", "q"]]}),
            json!({"insert": [1, "z", "z"]}),
        ];
        let mut after = before.clone();
        apply_keyed(&mut after, &reset);
        assert_eq!(after, vec![json!("q"), json!("z")]);
        assert_eq!(undo_keyed(&after, &reset, &before), Some(before));
        // Removed items that do not fit: none.
        assert_eq!(
            undo_keyed(&[json!("a")], &[json!({"update": [0, "a", "b"]})], &[]),
            None
        );
    }

    #[test]
    fn values_are_written_as_steps_read_them() {
        assert_eq!(set_value(&json!("a \"b\"")), "\"a \\\"b\\\"\"");
        assert_eq!(set_value(&json!({"~text": 3})), "{\"~text\":3}");
        assert_eq!(set_value(&json!(1.5)), "1.5");
        assert_eq!(
            arg_word(&json!({"a": "b c"})),
            "\"{\\\"a\\\":\\\"b c\\\"}\""
        );
        assert_eq!(toml_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(
            unheld(&json!({"a": {"~text": 3}, "b": [{"~text": 1}]})),
            json!({"a": "xxx", "b": ["x"]})
        );
        // A newline is written as the step parser reads it back.
        assert_eq!(set_value(&json!("a\nb")), "\"a\\nb\"");
        assert_eq!(
            scenario_id("2026-10-20T14-03-07 The Bar"),
            "report-2026-10-20t14-03-07-the-bar"
        );
    }
}
