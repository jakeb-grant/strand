//! The effect matrix (docs/eval.md, "Matrix"): every effect of
//! `eval/matrix/effects.toml` alone and every unordered pair, each on
//! `eval/matrix/fixture.strand.in` with the standard steps, run and
//! checked like any scenario (a cell is materialised as scenario text and
//! parsed, so `promote` writes it out as it ran). A cell fails the matrix
//! when it fails a check `eval/matrix/known.toml` does not list for it,
//! when a listed check passes (stale: remove the line), when it cannot
//! run, or when it is CHANGED: it left its approved reference recording
//! (`eval/refs/matrix-<hz>.ref`), or has none.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use crate::checks::{Finding, Sev, Status};
use crate::harness::RunOptions;
use crate::report::{self, Entry, Frames};
use crate::scenario::{CheckId, Place, Scenario};
use crate::select::Selector;
use crate::{Error, check_with};

/// The props slots of the fixture; every other slot takes text.
pub const PROP_SLOTS: [&str; 3] = ["row", "subject", "card"];
/// The text slots of the fixture.
pub const TEXT_SLOTS: [&str; 9] = [
    "subject_body",
    "with",
    "extra",
    "below",
    "card_width",
    "card_body",
    "decls",
    "pop_size",
    "card_height",
];
/// Slots where two effects' text is joined rather than a conflict.
const JOINED: [&str; 1] = ["decls"];
/// Props written as statements (`play shake`), not `prop: value`.
const STATEMENTS: [&str; 1] = ["play"];

/// A prop's value: an expression, or a block of them (`enter { … }`).
#[derive(Clone, Debug, PartialEq)]
pub enum Prop {
    Value(String),
    Block(BTreeMap<String, String>),
}

/// What an effect puts in a slot.
#[derive(Clone, Debug, PartialEq)]
pub enum Slot {
    Props(BTreeMap<String, Prop>),
    Text(String),
}

/// One effect of effects.toml.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effect {
    pub name: String,
    pub about: String,
    pub slots: BTreeMap<String, Slot>,
    pub rules: Vec<String>,
    pub screen_scale: Option<f64>,
    pub hz: Option<u32>,
    pub reduce_motion: Option<bool>,
    /// The scenario's `backend` (`both`: the cell runs on the GPU leg too
    /// and adds `cpu-gpu`).
    pub backend: Option<String>,
    pub pre: Vec<String>,
    pub mid: Vec<String>,
    pub post: Vec<String>,
    pub intend: Vec<String>,
    pub excludes: Vec<String>,
}

/// effects.toml.
#[derive(Clone, Debug, Default)]
pub struct Effects {
    pub defaults: BTreeMap<String, String>,
    pub effects: Vec<Effect>,
}

/// A known failure: the checks a cell may fail, on which nodes, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Known {
    pub checks: Vec<String>,
    /// Selectors of the nodes the failures are on: a finding of a listed
    /// check on any other node still fails the cell. `*` is any node,
    /// and the only way to accept a finding on no node (a surface's
    /// pixels).
    pub nodes: Vec<String>,
    pub note: String,
    pub since: String,
}

impl Known {
    /// Whether this entry accepts `f`, a failure in `trace`.
    pub fn covers(&self, f: &Finding, trace: &crate::trace::RunTrace) -> bool {
        if !self.checks.contains(&f.check) {
            return false;
        }
        if self.nodes.iter().any(|n| n == "*") {
            return true;
        }
        let Some(key) = f.key.as_deref() else {
            return false;
        };
        self.nodes.iter().any(|n| {
            Selector::parse(n)
                .map(|sel| {
                    crate::checks::select_seen(trace, &sel)
                        .iter()
                        .any(|k| k == key)
                })
                .unwrap_or(false)
        })
    }
}

/// The matrix's directory.
pub fn dir() -> PathBuf {
    crate::scenario::eval_root().join("matrix")
}

fn bad(file: &Path, msg: String) -> Error {
    Error::Config(format!("{}: {msg}", file.display()))
}

fn strings(v: Option<&toml::Value>, what: &str, file: &Path) -> Result<Vec<String>, Error> {
    match v {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .ok_or_else(|| bad(file, format!("{what}: a list of strings")))
            })
            .collect(),
        Some(_) => Err(bad(file, format!("{what}: a list of strings"))),
    }
}

impl Effects {
    pub fn load(path: &Path) -> Result<Effects, Error> {
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
        Effects::parse(&text, path)
    }

    pub fn parse(text: &str, file: &Path) -> Result<Effects, Error> {
        let v: toml::Table = toml::from_str(text).map_err(|e| bad(file, e.to_string()))?;
        let mut out = Effects::default();
        for (k, val) in v
            .get("defaults")
            .and_then(|d| d.as_table())
            .into_iter()
            .flatten()
        {
            let s = val
                .as_str()
                .ok_or_else(|| bad(file, format!("defaults.{k}: a string")))?;
            if !TEXT_SLOTS.contains(&k.as_str()) {
                return Err(bad(file, format!("defaults.{k}: not a text slot")));
            }
            out.defaults.insert(k.clone(), s.to_string());
        }
        for k in v.keys() {
            if k != "defaults" && k != "effect" {
                return Err(bad(file, format!("`{k}`: only [defaults] and [[effect]]")));
            }
        }
        let list = v
            .get("effect")
            .and_then(|e| e.as_array())
            .ok_or_else(|| bad(file, "no [[effect]]".into()))?;
        for e in list {
            let t = e
                .as_table()
                .ok_or_else(|| bad(file, "an [[effect]] is a table".into()))?;
            let name = t
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| bad(file, "an effect without a name".into()))?
                .to_string();
            let ok = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-.".contains(c));
            if !ok {
                return Err(bad(
                    file,
                    format!("effect `{name}`: lowercase letters, digits, `-` and `.`"),
                ));
            }
            let at = |m: String| bad(file, format!("effect {name}: {m}"));
            for k in t.keys() {
                let known = [
                    "name",
                    "about",
                    "slots",
                    "rules",
                    "screen_scale",
                    "hz",
                    "reduce_motion",
                    "backend",
                    "pre",
                    "mid",
                    "post",
                    "intend",
                    "excludes",
                ];
                if !known.contains(&k.as_str()) {
                    return Err(at(format!("unknown key `{k}`")));
                }
            }
            let mut slots = BTreeMap::new();
            for (slot, val) in t
                .get("slots")
                .and_then(|s| s.as_table())
                .into_iter()
                .flatten()
            {
                let s = if PROP_SLOTS.contains(&slot.as_str()) {
                    let tab = val
                        .as_table()
                        .ok_or_else(|| at(format!("slots.{slot} is a props slot: a table")))?;
                    let mut props = BTreeMap::new();
                    for (p, pv) in tab {
                        let prop = match pv {
                            toml::Value::String(s) => Prop::Value(s.clone()),
                            toml::Value::Table(b) => {
                                let mut blk = BTreeMap::new();
                                for (bk, bv) in b {
                                    let s = bv.as_str().ok_or_else(|| {
                                        at(format!("slots.{slot}.{p}.{bk}: a string"))
                                    })?;
                                    blk.insert(bk.clone(), s.to_string());
                                }
                                Prop::Block(blk)
                            }
                            _ => return Err(at(format!("slots.{slot}.{p}: a string or a table"))),
                        };
                        props.insert(p.clone(), prop);
                    }
                    Slot::Props(props)
                } else if TEXT_SLOTS.contains(&slot.as_str()) {
                    Slot::Text(
                        val.as_str()
                            .ok_or_else(|| at(format!("slots.{slot} is a text slot: a string")))?
                            .to_string(),
                    )
                } else {
                    return Err(at(format!("no slot `{slot}` in the fixture")));
                };
                slots.insert(slot.clone(), s);
            }
            out.effects.push(Effect {
                about: t
                    .get("about")
                    .and_then(|a| a.as_str())
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                slots,
                rules: strings(t.get("rules"), "rules", file)?,
                screen_scale: t
                    .get("screen_scale")
                    .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))),
                hz: t.get("hz").and_then(|v| v.as_integer()).map(|v| v as u32),
                reduce_motion: t.get("reduce_motion").and_then(|v| v.as_bool()),
                backend: match t.get("backend") {
                    None => None,
                    Some(v) => {
                        let b = v.as_str().ok_or_else(|| at("backend: a string".into()))?;
                        crate::scenario::BackendMode::parse(b).map_err(at)?;
                        Some(b.to_string())
                    }
                },
                pre: strings(t.get("pre"), "pre", file)?,
                mid: strings(t.get("mid"), "mid", file)?,
                post: strings(t.get("post"), "post", file)?,
                intend: strings(t.get("intend"), "intend", file)?,
                excludes: strings(t.get("excludes"), "excludes", file)?,
                name,
            });
        }
        let names: Vec<&str> = out.effects.iter().map(|e| e.name.as_str()).collect();
        for (i, n) in names.iter().enumerate() {
            if names[..i].contains(n) {
                return Err(bad(file, format!("effect `{n}` is written twice")));
            }
        }
        for e in &out.effects {
            for x in &e.excludes {
                if !names.contains(&x.as_str()) {
                    return Err(bad(
                        file,
                        format!("effect {}: excludes `{x}`, which is no effect", e.name),
                    ));
                }
            }
        }
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Option<&Effect> {
        self.effects.iter().find(|e| e.name == name)
    }
}

/// known.toml, by cell (`a+b` or `a+b@hz`).
pub fn load_known(path: &Path) -> Result<BTreeMap<String, Known>, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(Error::Io(path.display().to_string(), e)),
    };
    let v: toml::Table = toml::from_str(&text).map_err(|e| bad(path, e.to_string()))?;
    let mut out = BTreeMap::new();
    for (cell, entry) in v {
        let t = entry
            .as_table()
            .ok_or_else(|| bad(path, format!("[\"{cell}\"]: a table")))?;
        let checks = strings(t.get("checks"), "checks", path)?;
        for c in &checks {
            if CheckId::parse(c).is_none() {
                return Err(bad(path, format!("[\"{cell}\"]: `{c}` is not a check")));
            }
        }
        let nodes = strings(t.get("nodes"), "nodes", path)?;
        for n in &nodes {
            if n != "*"
                && let Err(e) = Selector::parse(n)
            {
                return Err(bad(path, format!("[\"{cell}\"]: nodes: `{n}`: {e}")));
            }
        }
        let note = t.get("note").and_then(|n| n.as_str()).unwrap_or("");
        let since = t.get("since").and_then(|n| n.as_str()).unwrap_or("");
        if checks.is_empty()
            || nodes.is_empty()
            || note.trim().is_empty()
            || since.trim().is_empty()
        {
            return Err(bad(
                path,
                format!("[\"{cell}\"]: checks, nodes, a note and a since date are required"),
            ));
        }
        out.insert(
            cell,
            Known {
                checks,
                nodes,
                note: note.to_string(),
                since: since.to_string(),
            },
        );
    }
    Ok(out)
}

/// One cell: an effect alone or a pair.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// `a` or `a+b`.
    pub name: String,
    pub effects: Vec<String>,
    pub hz: u32,
    /// Why it does not run, if it does not.
    pub na: Option<String>,
    /// The scenario's text.
    pub text: String,
}

impl Cell {
    /// Its directory and scenario id: `a+b@60` and `a__b.60`.
    pub fn dir_name(&self) -> String {
        format!("{}@{}", self.name, self.hz)
    }

    pub fn id(&self) -> String {
        format!("m.{}.{}", self.name.replace('+', "__"), self.hz)
    }
}

/// The effects' fragments merged, or why they conflict.
fn merge(effects: &[&Effect]) -> Result<BTreeMap<String, Slot>, String> {
    let mut out: BTreeMap<String, Slot> = BTreeMap::new();
    for e in effects {
        for (slot, s) in &e.slots {
            match (out.get_mut(slot), s) {
                (None, s) => {
                    out.insert(slot.clone(), s.clone());
                }
                (Some(Slot::Text(a)), Slot::Text(b)) if JOINED.contains(&slot.as_str()) => {
                    a.push('\n');
                    a.push_str(b);
                }
                (Some(Slot::Text(_)), _) | (_, Slot::Text(_)) => {
                    return Err(format!("both set slot `{slot}`"));
                }
                (Some(Slot::Props(a)), Slot::Props(b)) => {
                    for (p, v) in b {
                        match (a.get_mut(p), v) {
                            (None, v) => {
                                a.insert(p.clone(), v.clone());
                            }
                            (Some(Prop::Block(x)), Prop::Block(y)) => {
                                for (k, val) in y {
                                    if x.contains_key(k) {
                                        return Err(format!("both set `{p} {{ {k} }}` on {slot}"));
                                    }
                                    x.insert(k.clone(), val.clone());
                                }
                            }
                            _ => return Err(format!("both set `{p}` on {slot}")),
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn render_props(props: &BTreeMap<String, Prop>) -> String {
    let mut s = String::new();
    for (p, v) in props {
        match v {
            Prop::Value(x) if STATEMENTS.contains(&p.as_str()) => s += &format!("; {p} {x}"),
            Prop::Value(x) => s += &format!("; {p}: {x}"),
            Prop::Block(b) => {
                let inner: Vec<String> = b.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                s += &format!("; {p} {{ {} }}", inner.join("; "));
            }
        }
    }
    s
}

/// The fixture with `slots` filled.
pub fn fill(
    fixture: &str,
    slots: &BTreeMap<String, Slot>,
    defaults: &BTreeMap<String, String>,
) -> String {
    let mut out = fixture.to_string();
    for slot in PROP_SLOTS {
        let text = match slots.get(slot) {
            Some(Slot::Props(p)) => render_props(p),
            _ => String::new(),
        };
        out = out.replace(&format!("{{{{{slot}}}}}"), &text);
    }
    for slot in TEXT_SLOTS {
        let text = match slots.get(slot) {
            Some(Slot::Text(t)) => t.clone(),
            _ => defaults.get(slot).cloned().unwrap_or_default(),
        };
        out = out.replace(&format!("{{{{{slot}}}}}"), &text);
    }
    out
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The scenario text of the cell of `names` at `hz`.
pub fn cell(set: &Effects, fixture: &str, names: &[&str], hz: u32) -> Cell {
    let name = names.join("+");
    let effects: Vec<&Effect> = names.iter().filter_map(|n| set.get(n)).collect();
    let mut na = None;
    for (i, a) in effects.iter().enumerate() {
        for b in &effects[i + 1..] {
            if a.excludes.contains(&b.name) || b.excludes.contains(&a.name) {
                na = Some(format!("excluded: {} and {} never pair", a.name, b.name));
            }
        }
    }
    let one = |f: &dyn Fn(&Effect) -> bool, what: &str| -> Option<String> {
        (effects.iter().filter(|e| f(e)).count() > 1).then(|| format!("conflict: both set {what}"))
    };
    na = na
        .or_else(|| one(&|e| e.screen_scale.is_some(), "the screen's scale"))
        .or_else(|| one(&|e| e.hz.is_some(), "the rate"))
        .or_else(|| one(&|e| e.backend.is_some(), "the backend"));
    let slots = match merge(&effects) {
        Ok(s) => s,
        Err(m) => {
            na = na.or(Some(format!("conflict: {m}")));
            BTreeMap::new()
        }
    };
    let hz = effects.iter().find_map(|e| e.hz).unwrap_or(hz);
    let scale = effects.iter().find_map(|e| e.screen_scale).unwrap_or(1.0);
    let reduce = effects.iter().any(|e| e.reduce_motion == Some(true));
    let c = Cell {
        name: name.clone(),
        effects: names.iter().map(|s| s.to_string()).collect(),
        hz,
        na,
        text: String::new(),
    };
    let about: Vec<String> = effects
        .iter()
        .map(|e| format!("{}: {}", e.name, e.about))
        .collect();
    let mut t = String::new();
    t += &format!("id = {}\n", toml_str(&c.id()));
    t += &format!(
        "about = {}\n",
        toml_str(&format!(
            "Effect matrix cell {name} at {hz} Hz (eval/matrix/fixture.strand.in). {}",
            about.join(" ")
        ))
    );
    t += &format!("screen = {{ width = 420, scale = {scale:?} }}\n");
    t += &format!("hz = {hz}\n");
    if reduce {
        t += "reduce_motion = true\n";
    }
    if let Some(b) = effects.iter().find_map(|e| e.backend.as_deref()) {
        t += &format!("backend = {}\n", toml_str(b));
    }
    t += "\n=== shell.strand\n";
    t += &fill(fixture, &slots, &set.defaults);
    if !t.ends_with('\n') {
        t.push('\n');
    }
    t += "\n=== steps\n";
    let intend: Vec<&String> = effects.iter().flat_map(|e| e.intend.iter()).collect();
    let lines = |list: &mut String, of: &dyn Fn(&Effect) -> &Vec<String>| {
        for e in &effects {
            for l in of(e) {
                list.push_str(l);
                list.push('\n');
            }
        }
    };
    lines(&mut t, &|e| &e.rules);
    t += "rest\n";
    lines(&mut t, &|e| &e.pre);
    for (mark, set_line, after) in [
        ("on", "set on true", true),
        ("wide", "set wide true", false),
        ("off", "set on false", false),
    ] {
        t += &format!("mark {mark}\n");
        for i in &intend {
            t += &format!("{i}\n");
        }
        t += &format!("{set_line}\nrest\n");
        if after {
            lines(&mut t, &|e| &e.mid);
        }
    }
    lines(&mut t, &|e| &e.post);
    Cell { text: t, ..c }
}

/// The cells of `names` (every one alone and, unless `singles`, every
/// unordered pair) at each rate.
pub fn cells(set: &Effects, fixture: &str, names: &[&str], hz: &[u32], singles: bool) -> Vec<Cell> {
    let mut out = Vec::new();
    for h in hz {
        for (i, a) in names.iter().enumerate() {
            out.push(cell(set, fixture, &[a], *h));
            if singles {
                continue;
            }
            for b in &names[..i] {
                out.push(cell(set, fixture, &[b, a], *h));
            }
        }
    }
    out
}

/// How a cell came out.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    /// Not run (conflict, excluded).
    Na,
    /// Failed only checks known.toml lists for it.
    Known,
    /// A check known.toml lists passes: remove it.
    Stale,
    /// It left its approved reference recording, or has none, and
    /// failed nothing else.
    Changed,
    Fail,
    /// It could not run.
    Error,
}

impl Outcome {
    pub fn gates(&self) -> bool {
        matches!(
            self,
            Outcome::Fail | Outcome::Stale | Outcome::Changed | Outcome::Error
        )
    }
}

/// A cell run.
#[derive(Debug)]
pub struct Done {
    pub cell: Cell,
    pub outcome: Outcome,
    /// The checks it failed.
    pub failed: Vec<String>,
    /// The failing findings, one line each.
    pub lines: Vec<String>,
    pub known: Option<Known>,
    pub entry: Option<Entry>,
}

/// What `run` is asked.
#[derive(Debug)]
pub struct Plan {
    pub effects: Option<Vec<String>>,
    pub hz: Vec<u32>,
    pub singles: bool,
    pub jobs: usize,
    pub build: crate::trace::BuildInfo,
    pub effects_file: PathBuf,
    pub known_file: PathBuf,
    pub fixture_file: PathBuf,
    /// The reference files (`eval/refs/`): `matrix-<hz>.ref`.
    pub refs_dir: PathBuf,
}

impl Plan {
    pub fn new(build: crate::trace::BuildInfo) -> Plan {
        Plan {
            effects: None,
            hz: vec![60],
            singles: false,
            jobs: 4,
            build,
            effects_file: dir().join("effects.toml"),
            known_file: dir().join("known.toml"),
            fixture_file: dir().join("fixture.strand.in"),
            refs_dir: crate::refs::dir(),
        }
    }
}

fn judge(cell: Cell, known: Option<&Known>, entry: Entry) -> Done {
    let report = entry.report.as_ref();
    let failed: Vec<String> = report
        .map(|r| {
            r.results
                .iter()
                .filter(|x| matches!(x.status, Status::Fail | Status::Xpass(_)))
                .map(|x| x.check.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut lines = Vec::new();
    let changed = report.and_then(|r| r.changed_line());
    if let Some(l) = &changed {
        lines.push(l.clone());
    }
    if let Some(r) = report {
        for res in &r.results {
            if res.status == Status::Changed {
                continue;
            }
            for f in res.findings.iter().filter(|f| f.sev == Sev::Fail) {
                lines.push(crate::checks::describe(f, &entry.trace));
            }
        }
    }
    // A failure the entry does not cover (another check, or a listed
    // check on a node it does not name) fails the cell.
    let uncovered: Vec<&Finding> = match (known, report) {
        (Some(k), Some(r)) => r
            .results
            .iter()
            .filter(|x| x.status != Status::Changed)
            .flat_map(|x| x.findings.iter())
            .filter(|f| f.sev == Sev::Fail && !k.covers(f, &entry.trace))
            .collect(),
        _ => Vec::new(),
    };
    let mut outcome = match known {
        None if failed.is_empty() => Outcome::Pass,
        None => Outcome::Fail,
        Some(k) => {
            if failed.iter().any(|c| !k.checks.contains(c)) {
                Outcome::Fail
            } else if !uncovered.is_empty() {
                lines.push(format!(
                    "known.toml accepts {} on {} only: {} failure(s) elsewhere",
                    k.checks.join(", "),
                    k.nodes.join(", "),
                    uncovered.len()
                ));
                Outcome::Fail
            } else if k.checks.iter().any(|c| !failed.contains(c)) {
                lines.push(format!(
                    "known.toml lists {} for this cell, and {} now pass: remove them",
                    k.checks.join(", "),
                    k.checks
                        .iter()
                        .filter(|c| !failed.contains(c))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                Outcome::Stale
            } else {
                Outcome::Known
            }
        }
    };
    if changed.is_some() && matches!(outcome, Outcome::Pass | Outcome::Known) {
        outcome = Outcome::Changed;
    }
    Done {
        cell,
        outcome,
        failed,
        lines,
        known: known.cloned(),
        entry: Some(entry),
    }
}

/// Runs one cell against `refs` (its approved and known-bad recordings).
pub fn run_cell(
    cell: Cell,
    known: Option<&Known>,
    build: &crate::trace::BuildInfo,
    refs: crate::refs::Refs,
) -> Done {
    if cell.na.is_some() {
        return Done {
            cell,
            outcome: Outcome::Na,
            failed: Vec::new(),
            lines: Vec::new(),
            known: None,
            entry: None,
        };
    }
    let file = dir().join("cells").join(format!("{}.scn", cell.dir_name()));
    let sc = match Scenario::parse_in(&cell.text, &file, Place::Committed) {
        Ok(s) => s,
        Err(e) => return errored(cell, e),
    };
    let opts = RunOptions {
        reduced_motion: false,
        images: true,
        build: build.clone(),
        backend: None,
        ..RunOptions::default()
    };
    match check_with(&sc, &opts, refs) {
        Ok(c) => {
            let (refs, reference) = report::refs_of(c.refs.as_ref(), &c.report, &c.run.trace);
            let entry = Entry {
                id: sc.id.clone(),
                file: format!("eval/matrix ({})", cell.name),
                source: sc.source.clone(),
                trace: c.run.trace,
                report: Some(c.report),
                images: c.run.images,
                notes: c.run.notes,
                refs,
                reference,
                ..Entry::default()
            };
            judge(cell, known, entry)
        }
        Err(e) => errored(cell, e),
    }
}

fn errored(cell: Cell, e: Error) -> Done {
    let entry = Entry {
        id: cell.id(),
        file: format!("eval/matrix ({})", cell.name),
        source: cell.text.clone(),
        error: Some(e.to_string()),
        ..Entry::default()
    };
    Done {
        lines: vec![format!("error: {e}")],
        cell,
        outcome: Outcome::Error,
        failed: Vec::new(),
        known: None,
        entry: Some(entry),
    }
}

/// The known.toml entry for `cell`: at its rate first, then any rate.
pub fn known_for<'a>(known: &'a BTreeMap<String, Known>, cell: &Cell) -> Option<&'a Known> {
    known
        .get(&cell.dir_name())
        .or_else(|| known.get(&cell.name))
}

/// Runs the plan's cells on `jobs` threads, each cell its own renderer
/// and logic.
pub fn run(plan: &Plan) -> Result<Vec<Done>, Error> {
    let set = Effects::load(&plan.effects_file)?;
    let fixture = std::fs::read_to_string(&plan.fixture_file)
        .map_err(|e| Error::Io(plan.fixture_file.display().to_string(), e))?;
    let known = load_known(&plan.known_file)?;
    let names: Vec<&str> = match &plan.effects {
        None => set.effects.iter().map(|e| e.name.as_str()).collect(),
        Some(list) => {
            for n in list {
                if set.get(n).is_none() {
                    return Err(Error::Usage(format!("no effect `{n}` in effects.toml")));
                }
            }
            // effects.toml's order, so cell names are stable.
            set.effects
                .iter()
                .map(|e| e.name.as_str())
                .filter(|n| list.iter().any(|x| x == n))
                .collect()
        }
    };
    for k in known.keys() {
        let cell = k.split('@').next().unwrap_or(k);
        if cell.split('+').any(|e| set.get(e).is_none()) {
            return Err(Error::Config(format!(
                "known.toml: [\"{k}\"] names an effect effects.toml does not have"
            )));
        }
    }
    let todo = cells(&set, &fixture, &names, &plan.hz, plan.singles);
    // Each rate's reference file, read once.
    let mut ref_files = BTreeMap::new();
    for c in &todo {
        ref_files.entry(c.hz).or_insert_with(|| {
            let file = plan.refs_dir.join(format!("matrix-{}.ref", c.hz));
            let read = crate::refs::RefFile::load(&file);
            (file, read)
        });
    }
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, Done)>> = Mutex::new(Vec::new());
    let jobs = plan.jobs.max(1).min(todo.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(c) = todo.get(i) else { break };
                    let refs = match ref_files.get(&c.hz) {
                        Some((file, read)) => crate::refs::Refs::of(file, read, &c.id(), true),
                        None => crate::refs::Refs {
                            required: true,
                            ..crate::refs::Refs::default()
                        },
                    };
                    let d = run_cell(c.clone(), known_for(&known, c), &plan.build, refs);
                    if let Ok(mut v) = done.lock() {
                        v.push((i, d));
                    }
                }
            });
        }
    });
    let mut v = done
        .into_inner()
        .map_err(|_| Error::Run("a matrix thread panicked".into()))?;
    if v.len() != todo.len() {
        return Err(Error::Run(format!(
            "{} of {} cells ran (a thread panicked)",
            v.len(),
            todo.len()
        )));
    }
    v.sort_by_key(|(i, _)| *i);
    Ok(v.into_iter().map(|(_, d)| d).collect())
}

/// Writes the matrix's report under `out`: each cell's run (its page
/// when it did not pass), `matrix.json`, `summary.md` and the grid
/// `index.html`.
pub fn write(out: &Path, plan: &Plan, done: &mut [Done]) -> Result<(), Error> {
    let set = Effects::load(&plan.effects_file)?;
    for d in done.iter_mut() {
        let Some(e) = d.entry.as_mut() else { continue };
        let cell_dir = out.join(d.cell.dir_name());
        report::write_entry(&cell_dir, e, Frames::Failing)?;
        if d.outcome != Outcome::Pass {
            let info = report::RunInfo {
                command: format!("matrix cell {}", d.cell.dir_name()),
                out: cell_dir.clone(),
                frames: Frames::Failing,
            };
            let page = report::page_of(&info, std::slice::from_ref(e))?;
            report::write(&cell_dir.join("index.html"), page.as_bytes())?;
        }
    }
    let cells: Vec<Value> = done
        .iter()
        .map(|d| {
            json!({
                "cell": d.cell.name,
                "hz": d.cell.hz,
                "effects": d.cell.effects,
                "dir": d.cell.dir_name(),
                "id": d.cell.id(),
                "outcome": d.outcome,
                "na": d.cell.na,
                "failed": d.failed,
                "findings": d.lines,
                "known": d.known.as_ref().map(|k| json!({"checks": k.checks, "nodes": k.nodes, "note": k.note, "since": k.since})),
                "reference": d.entry.as_ref().and_then(|e| e.refs.as_ref()).map(|r| r.status.clone()),
            })
        })
        .collect();
    let count = |o: Outcome| done.iter().filter(|d| d.outcome == o).count();
    let summary = json!({
        "pass": count(Outcome::Pass),
        "known": count(Outcome::Known),
        "na": count(Outcome::Na),
        "fail": count(Outcome::Fail),
        "stale": count(Outcome::Stale),
        "changed": count(Outcome::Changed),
        "error": count(Outcome::Error),
    });
    let rev = plan.build.rev.clone();
    let v = json!({
        "schema": 1,
        "rev": rev,
        "dirty": plan.build.dirty,
        "hz": plan.hz,
        "effects": set.effects.iter().map(|e| json!({"name": e.name, "about": e.about})).collect::<Vec<_>>(),
        "counts": summary,
        "cells": cells,
    });
    report::write(
        &out.join("matrix.json"),
        serde_json::to_string_pretty(&v)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    let stale = stale_refs(plan, done)?;
    report::write(
        &out.join("summary.md"),
        summary_md(out, done, &stale).as_bytes(),
    )?;
    report::write(&out.join("index.html"), grid(&set, plan, done).as_bytes())?;
    Ok(())
}

/// The blocks of each rate's reference file that name no cell, when
/// `plan` ran the whole matrix (every effect alone and every pair): an
/// effect removed or renamed. `approve --all-changed` prunes them.
pub fn stale_refs(plan: &Plan, done: &[Done]) -> Result<Vec<String>, Error> {
    if plan.effects.is_some() || plan.singles {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for hz in &plan.hz {
        let ids: BTreeSet<String> = done
            .iter()
            .filter(|d| d.cell.hz == *hz)
            .map(|d| d.cell.id())
            .collect();
        out.extend(crate::refs::stale_matrix(&plan.refs_dir, *hz, &ids)?);
    }
    Ok(out)
}

fn summary_md(out: &Path, done: &[Done], stale: &[String]) -> String {
    let mut md = vec![
        "# strand-eval matrix".to_string(),
        String::new(),
        format!("Report: `{}/index.html`.", out.display()),
        String::new(),
    ];
    let count = |o: Outcome| done.iter().filter(|d| d.outcome == o).count();
    md.push(format!(
        "{} cells: {} pass, {} known, {} n/a, {} fail, {} stale, {} changed, {} error.",
        done.len(),
        count(Outcome::Pass),
        count(Outcome::Known),
        count(Outcome::Na),
        count(Outcome::Fail),
        count(Outcome::Stale),
        count(Outcome::Changed),
        count(Outcome::Error)
    ));
    md.push(String::new());
    for id in stale {
        md.push(format!(
            "- **STALE REFERENCE** {id}: no such cell (`approve --all-changed` on a whole matrix run prunes it)"
        ));
    }
    for d in done.iter().filter(|d| d.outcome.gates()) {
        md.push(format!(
            "- **{}** {:?}: {}",
            d.cell.dir_name(),
            d.outcome,
            d.failed.join(", ")
        ));
        for l in d.lines.iter().take(6) {
            md.push(format!("  - {l}"));
        }
        if d.lines.len() > 6 {
            md.push(format!("  - … {} more", d.lines.len() - 6));
        }
        if d.outcome == Outcome::Changed {
            md.push(format!(
                "  - approve: `scripts/container/run.sh eval approve {}/{} --note \"why\"`",
                out.display(),
                d.cell.dir_name(),
            ));
        } else {
            md.push(format!(
                "  - promote: `scripts/container/run.sh eval promote {}/{} --as <name>`",
                out.display(),
                d.cell.dir_name(),
            ));
        }
    }
    if count(Outcome::Changed) > 0 {
        md.push(format!(
            "- approve every changed cell, once each change is seen to be right: `scripts/container/run.sh eval approve --all-changed {} --note \"why\"`",
            out.display()
        ));
    }
    for d in done.iter().filter(|d| d.outcome == Outcome::Known) {
        if let Some(k) = &d.known {
            md.push(format!(
                "- {} known ({} on {}): {}",
                d.cell.dir_name(),
                k.checks.join(", "),
                k.nodes.join(", "),
                k.note
            ));
        }
    }
    md.join("\n") + "\n"
}

fn esc(t: &str) -> String {
    t.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The triangular grid page: one row and one column per effect, each
/// cell coloured by its outcome, its title the failing checks, a link to
/// its page when it did not pass.
fn grid(set: &Effects, plan: &Plan, done: &[Done]) -> String {
    let by: BTreeMap<(String, u32), &Done> = done
        .iter()
        .map(|d| ((d.cell.name.clone(), d.cell.hz), d))
        .collect();
    let names: Vec<&str> = set
        .effects
        .iter()
        .map(|e| e.name.as_str())
        .filter(|n| done.iter().any(|d| d.cell.effects.iter().any(|x| x == n)))
        .collect();
    let hzs: BTreeSet<u32> = done.iter().map(|d| d.cell.hz).collect();
    let mut body = String::new();
    for hz in &hzs {
        body += &format!("<h2>{hz} Hz</h2><div class=\"wrap\"><table class=\"grid\"><tr><th></th>");
        for n in &names {
            body += &format!("<th class=\"col\"><span>{}</span></th>", esc(n));
        }
        body += "</tr>";
        for (i, a) in names.iter().enumerate() {
            body += &format!("<tr><th class=\"row\">{}</th>", esc(a));
            for (j, b) in names.iter().enumerate() {
                if j > i {
                    body += "<td class=\"off\"></td>";
                    continue;
                }
                let name = if i == j {
                    a.to_string()
                } else {
                    format!("{b}+{a}")
                };
                let Some(d) = by.get(&(name.clone(), *hz)) else {
                    body += "<td class=\"off\"></td>";
                    continue;
                };
                let (cls, mark) = match d.outcome {
                    Outcome::Pass => ("pass", ""),
                    Outcome::Na => ("na", "–"),
                    Outcome::Known => ("known", "k"),
                    Outcome::Stale => ("stale", "s"),
                    Outcome::Changed => ("changed", "c"),
                    Outcome::Fail => ("fail", "✕"),
                    Outcome::Error => ("error", "!"),
                };
                let mut title = format!("{name} @ {hz} Hz: {cls}");
                if let Some(na) = &d.cell.na {
                    title += &format!(" ({na})");
                }
                if !d.failed.is_empty() {
                    title += &format!("\nfailed: {}", d.failed.join(", "));
                }
                for l in d.lines.iter().take(4) {
                    title += &format!("\n{l}");
                }
                if let Some(k) = &d.known {
                    title += &format!("\nknown: {}", k.note);
                }
                let inner = if matches!(d.outcome, Outcome::Pass | Outcome::Na) {
                    mark.to_string()
                } else {
                    format!(
                        "<a href=\"{}/index.html\">{mark}</a>",
                        esc(&d.cell.dir_name())
                    )
                };
                body += &format!("<td class=\"{cls}\" title=\"{}\">{inner}</td>", esc(&title));
            }
            body += "</tr>";
        }
        body += "</table></div>";
        // The cells that did not pass, as text: a cell's title shows only
        // on hover, which a phone does not have.
        let mut list = String::new();
        for d in done
            .iter()
            .filter(|d| d.cell.hz == *hz && !matches!(d.outcome, Outcome::Pass | Outcome::Na))
        {
            let (cls, what) = match d.outcome {
                Outcome::Known => ("known", "known"),
                Outcome::Stale => ("stale", "stale known"),
                Outcome::Changed => ("changed", "changed"),
                Outcome::Fail => ("fail", "fail"),
                _ => ("error", "error"),
            };
            let mut why = d.failed.join(", ");
            if let Some(k) = &d.known {
                why = format!("{} (known: {})", k.checks.join(", "), k.note);
            }
            list += &format!(
                "<li><a href=\"{}/index.html\">{}</a> <span class=\"tag {cls}\">{what}</span> {}",
                esc(&d.cell.dir_name()),
                esc(&d.cell.name),
                esc(&why)
            );
            // A failing check's first finding, then the reference's line.
            let (changed, other): (Vec<&String>, Vec<&String>) =
                d.lines.iter().partition(|l| l.starts_with("CHANGED"));
            for l in other.into_iter().take(1).chain(changed) {
                list += &format!("<div class=\"line\">{}</div>", esc(l));
            }
            list += "</li>";
        }
        if !list.is_empty() {
            body += &format!("<ul class=\"cells\">{list}</ul>");
        }
    }
    let count = |o: Outcome| done.iter().filter(|d| d.outcome == o).count();
    let head = format!(
        "{} cells at {}: {} pass, {} known, {} n/a, {} fail, {} stale, {} changed, {} error · {}{}",
        done.len(),
        hzs.iter()
            .map(|h| format!("{h} Hz"))
            .collect::<Vec<_>>()
            .join(", "),
        count(Outcome::Pass),
        count(Outcome::Known),
        count(Outcome::Na),
        count(Outcome::Fail),
        count(Outcome::Stale),
        count(Outcome::Changed),
        count(Outcome::Error),
        plan.build.rev,
        if plan.build.dirty {
            " with changes"
        } else {
            ""
        }
    );
    GRID.replace("{{HEAD}}", &esc(&head))
        .replace("{{BODY}}", &body)
}

const GRID: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Effect matrix</title>
<style>
:root { --bg: #f4f4f6; --ink: #1d1d23; --muted: #62646f; --line: #d9dae0; --pass: #cfe9d6; --known: #e3dcf7; --na: #ececf0; --fail: #f6c4c4; --stale: #f4e3a8; --changed: #f7d9b0; --error: #f0a0a0; color-scheme: light; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) { --bg: #121216; --ink: #e7e7ee; --muted: #9b9dab; --line: #30313b; --pass: #1f4a2c; --known: #3a3060; --na: #22232a; --fail: #6b2323; --stale: #5a4a14; --changed: #6a4214; --error: #8a2a2a; color-scheme: dark; } }
:root[data-theme="dark"] { --bg: #121216; --ink: #e7e7ee; --muted: #9b9dab; --line: #30313b; --pass: #1f4a2c; --known: #3a3060; --na: #22232a; --fail: #6b2323; --stale: #5a4a14; --changed: #6a4214; --error: #8a2a2a; color-scheme: dark; }
* { box-sizing: border-box; }
body { margin: 0; padding: 12px 16px; background: var(--bg); color: var(--ink); font: 13px/1.4 system-ui, sans-serif; overflow-x: hidden; }
h1 { font-size: 16px; margin: 0 0 4px; } h2 { font-size: 14px; margin: 16px 0 6px; }
.meta { color: var(--muted); overflow-wrap: anywhere; }
.wrap { overflow-x: auto; max-width: 100%; }
.grid { border-collapse: collapse; }
.grid td, .grid th { border: 1px solid var(--line); width: 22px; height: 22px; padding: 0; text-align: center; font-size: 11px; }
.grid th.row { text-align: right; padding: 0 6px; white-space: nowrap; width: auto; font-weight: 500; }
.grid th.col { height: 120px; vertical-align: bottom; }
.grid th.col span { writing-mode: vertical-rl; transform: rotate(180deg); white-space: nowrap; font-weight: 500; padding: 4px 0; }
.grid td.off { border: 0; }
.grid td a { color: inherit; display: block; text-decoration: none; }
.pass { background: var(--pass); } .known { background: var(--known); } .na { background: var(--na); color: var(--muted); }
.fail { background: var(--fail); } .stale { background: var(--stale); } .changed { background: var(--changed); } .error { background: var(--error); }
.legend span { display: inline-block; padding: 1px 8px; margin: 2px 4px 2px 0; border-radius: 4px; }
.cells { list-style: none; padding: 0; margin: 8px 0 0; }
.cells li { padding: 3px 0; overflow-wrap: anywhere; }
.cells a { color: inherit; font-weight: 600; }
.cells .tag { padding: 0 6px; border-radius: 4px; font-size: 11px; }
.cells .line { color: var(--muted); font: 11px/1.4 ui-monospace, "DejaVu Sans Mono", monospace; overflow-wrap: anywhere; }
</style>
</head>
<body>
<h1>Effect matrix</h1>
<div class="meta">{{HEAD}}</div>
<div class="legend"><span class="pass">pass</span><span class="known">k known</span><span class="na">– n/a</span><span class="fail">✕ fail</span><span class="stale">s stale known</span><span class="changed">c changed from its reference</span><span class="error">! error</span></div>
<div class="meta">The diagonal is each effect alone, below it each pair. Hover a cell for its findings; a cell that did not pass links to its run and is listed under its grid.</div>
{{BODY}}
</body>
</html>
"#;
