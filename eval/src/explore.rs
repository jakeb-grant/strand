//! A starter scenario for a config (docs/eval.md, "Local configs and
//! fonts"; decisions.md eval-local): `strand-eval new <id> --config F
//! --explore`, which `strand-eval local new` runs. It knows nothing of
//! any particular shell. It reads what the config declares and where its
//! nodes are once it has booted:
//!
//! - each top-level `state` that is a `bool` is toggled and toggled back,
//!   and each that is an `enum` is set to each of its values, with a rest
//!   after each (others are listed as comments to fill in);
//! - each element something reads the `hover` or `pressed` of, and the
//!   element a `trigger:` popup hangs from, is hovered: the pointer
//!   glides to it and the run rests;
//! - each element with an `on click`, `on secondary`, `on middle` or `on
//!   scroll` handler gets that input twice (most of them toggle back),
//!   with a rest after each.
//!
//! The steps name each surface by its root (`on @file:line:col`) and
//! each place by the centre of the element's box after boot. A starter is
//! a draft: the owner reads it, keeps what matters and says what a
//! correct shell does (`expect`, `intend`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use strand_compiler::SourceMap;
use strand_compiler::hir::{DefKind, NodeIdx};
use strand_compiler::lower::code::Op;
use strand_compiler::lower::{ElementKind, Event, Node, Program};
use strand_compiler::ty::{Prim, Ty};
use strand_scene::NodeKind;

use crate::Error;
use crate::checks::Timeline;
use crate::harness::{self, RunOptions};
use crate::scenario::{Place, Scenario};
use crate::trace::{Row, RunTrace};

/// The most elements of each kind a starter exercises (a large shell's
/// starter stays readable; the rest are counted in a comment).
pub const MAX_TARGETS: usize = 40;

/// The inputs an element's handlers take.
const INPUTS: [&str; 4] = ["click", "secondary", "middle", "scroll"];

/// What the config declares that a starter can exercise.
#[derive(Debug, Default, PartialEq)]
pub struct Declared {
    /// Top-level `bool` states, by the path `toggle` takes.
    pub toggles: Vec<String>,
    /// Top-level enum states: the path and the enum's values.
    pub enums: Vec<(String, Vec<String>)>,
    /// Other top-level states: the path and the type, for a comment.
    pub others: Vec<(String, String)>,
    /// Elements something reads the hover or pressed of, or that a
    /// `trigger:` popup hangs from, by origin (`file:line:col`).
    pub hovers: BTreeSet<String>,
    /// Elements with input handlers: origin to the inputs (`click`, …).
    pub inputs: BTreeMap<String, BTreeSet<String>>,
    /// Of `hovers`, the elements a `trigger:` popup hangs from.
    pub anchors: BTreeSet<String>,
}

#[derive(Default)]
struct Walk {
    /// Each element's origin.
    origin: BTreeMap<NodeIdx, String>,
    hovers: BTreeSet<NodeIdx>,
    anchors: BTreeSet<NodeIdx>,
    inputs: BTreeMap<NodeIdx, BTreeSet<String>>,
}

fn origin(map: &SourceMap, file: strand_compiler::FileId, start: u32) -> Option<String> {
    let f = map.get(file)?;
    let (l, c) = crate::labels::line_col(&f.text, start as usize);
    Some(format!("{}:{l}:{c}", f.name))
}

fn walk(prog: &Program, map: &SourceMap, nodes: &[Node], parent: Option<NodeIdx>, w: &mut Walk) {
    for n in nodes {
        match n {
            Node::Element(e) => element(prog, map, e, parent, w),
            Node::Surface(s) => {
                element(prog, map, &s.element, parent, w);
                walk(prog, map, &s.body.nodes, Some(s.element.node), w);
            }
            Node::If { then, else_, .. } => {
                walk(prog, map, then, parent, w);
                walk(prog, map, else_, parent, w);
            }
            Node::For(f) => walk(prog, map, &f.body.nodes, parent, w),
            Node::Match { arms, .. } => {
                for a in arms {
                    walk(prog, map, a, parent, w);
                }
            }
            _ => {}
        }
    }
}

fn handlers(nodes: &[Node]) -> BTreeSet<String> {
    nodes
        .iter()
        .filter_map(|n| match n {
            Node::Handler(h) => match &h.event {
                Event::Element(name) if INPUTS.contains(&name.as_str()) => Some(name.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn element(
    prog: &Program,
    map: &SourceMap,
    e: &strand_compiler::lower::Element,
    parent: Option<NodeIdx>,
    w: &mut Walk,
) {
    if let Some(o) = origin(map, e.file, e.span.start) {
        w.origin.insert(e.node, o);
    }
    let mut on = handlers(&e.children);
    on.extend(handlers(&e.call_handlers));
    if !on.is_empty() {
        // A component call's handlers are its root element's: the node
        // on screen is that one.
        let at = match e.kind {
            ElementKind::Component(def) => prog
                .components
                .get(&def)
                .and_then(|c| c.root)
                .unwrap_or(e.node),
            _ => e.node,
        };
        w.inputs.entry(at).or_default().extend(on);
    }
    if e.kind == ElementKind::Builtin(NodeKind::Popup)
        && e.props.iter().any(|p| p.name == "trigger")
        && let Some(anchor) = parent
    {
        w.hovers.insert(anchor);
        w.anchors.insert(anchor);
    }
    walk(prog, map, &e.children, Some(e.node), w);
}

/// What `prog` declares that a starter can exercise.
pub fn declared(prog: &Program, map: &SourceMap) -> Declared {
    let mut w = Walk::default();
    for f in &prog.files {
        walk(prog, map, &f.items, None, &mut w);
    }
    for c in prog.components.values() {
        walk(prog, map, &c.body.nodes, None, &mut w);
    }
    // `hover`, `pressed`, `vol.hover`: an element read, then that field.
    for c in &prog.chunks {
        for (i, op) in c.ops.iter().enumerate() {
            if let (Op::Node(n) | Op::NodeAlias(n), Some(Op::Field(f))) = (op, c.ops.get(i + 1))
                && matches!(
                    c.names.get(*f as usize).map(String::as_str),
                    Some("hover" | "pressed")
                )
            {
                w.hovers.insert(*n);
            }
        }
    }
    let mut d = Declared::default();
    let states: Vec<_> = prog
        .defs
        .iter()
        .filter(|x| x.owner.is_none() && x.kind == DefKind::State)
        .collect();
    for s in &states {
        let twins = states.iter().filter(|o| o.name == s.name).count();
        let path = if twins > 1 {
            format!("{}.{}", s.module, s.name)
        } else {
            s.name.clone()
        };
        match s.ty.non_null() {
            Ty::Prim(Prim::Bool) => d.toggles.push(path),
            Ty::Enum(e) => d.enums.push((path, prog.types.enum_(*e).variants.clone())),
            other => d.others.push((path, prog.types.show(other).to_string())),
        }
    }
    d.toggles.sort();
    d.toggles.dedup();
    d.enums.sort();
    d.enums.dedup();
    d.others.sort();
    d.others.dedup();
    d.hovers = w
        .hovers
        .iter()
        .filter_map(|n| w.origin.get(n).cloned())
        .collect();
    d.anchors = w
        .anchors
        .iter()
        .filter_map(|n| w.origin.get(n).cloned())
        .collect();
    for (n, on) in &w.inputs {
        if let Some(o) = w.origin.get(n) {
            d.inputs
                .entry(o.clone())
                .or_default()
                .extend(on.iter().cloned());
        }
    }
    d
}

/// Where an element is after boot: its surface's root selector and the
/// centre of its laid box there.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot {
    pub surface: String,
    pub at: [f32; 2],
    pub label: String,
}

/// The first shown node of each origin in the trace's last screen, with
/// its surface's root origin.
pub fn places(trace: &RunTrace) -> BTreeMap<String, Spot> {
    let tl = Timeline::new(trace);
    let mut out = BTreeMap::new();
    let Some(last) = tl.screens.last() else {
        return out;
    };
    let roots: BTreeMap<u32, String> = trace
        .surfaces
        .iter()
        .filter_map(|s| {
            let o = trace.nodes.get(&s.root)?.origin.clone()?;
            Some((s.id, format!("@{o}")))
        })
        .collect();
    let mut rows: Vec<&&Row> = last.values().collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    for r in rows {
        let shown = r.hidden.is_none() && r.op > 0.01 && r.laid[2] >= 1.0 && r.laid[3] >= 1.0;
        let Some(meta) = trace.nodes.get(&r.key) else {
            continue;
        };
        let (Some(o), Some(surface)) = (&meta.origin, roots.get(&r.surface)) else {
            continue;
        };
        if !shown || out.contains_key(o) {
            continue;
        }
        out.insert(
            o.clone(),
            Spot {
                surface: surface.clone(),
                at: [
                    (r.laid[0] + r.laid[2] / 2.0).round(),
                    (r.laid[1] + r.laid[3] / 2.0).round(),
                ],
                label: meta.label.clone(),
            },
        );
    }
    out
}

fn pt(p: [f32; 2]) -> String {
    format!("{},{}", p[0], p[1])
}

/// The step that gives `input` at `at`; `back` undoes a scroll (other
/// inputs are given again: most toggle back).
fn input_step(input: &str, at: &Spot, back: bool) -> String {
    match input {
        "click" => format!("click {} on {}", pt(at.at), at.surface),
        "secondary" => format!("click {} right on {}", pt(at.at), at.surface),
        "middle" => format!("click {} middle on {}", pt(at.at), at.surface),
        _ if back => "scroll -1".to_string(),
        _ => "scroll 1".to_string(),
    }
}

/// A segment name from a label: letters, digits and `-` only.
fn mark(prefix: &str, n: usize, label: &str) -> String {
    let mut s: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    let s: String = s.chars().take(32).collect();
    format!("{prefix}{n}-{s}")
}

/// Runs steps after the header; says where things are at their end.
pub type Runner<'r> = dyn FnMut(&[String]) -> Result<BTreeMap<String, Spot>, Error> + 'r;

/// The most re-runs one starter makes to see what a hover or an input
/// revealed (each replays the steps so far).
pub const MAX_RUNS: usize = 32;

/// How deep it follows what an input or a hover reveals (a popup's
/// contents, a popup in that popup).
pub const MAX_DEPTH: usize = 3;

/// The steps that exercise `d`. `run` runs the steps so far (after the
/// header) and says where things are at their end; it is called first
/// with the boot alone, then after each hover or input that may reveal
/// more (a popup's anchor, a handler), to exercise what it revealed
/// before going on.
pub fn steps(
    d: &Declared,
    start: &BTreeMap<String, String>,
    run: &mut Runner<'_>,
) -> Result<Vec<String>, Error> {
    let mut x = Explorer {
        d,
        out: vec!["rest".to_string()],
        done: BTreeSet::new(),
        last: None,
        runs: 0,
        screen: BTreeMap::new(),
        hovers: 0,
        inputs: 0,
        failed: Vec::new(),
    };
    for (s, ty) in &d.others {
        x.out.push(format!(
            "# set {s} <value>   (a {ty}: pick the values worth seeing)"
        ));
    }
    let boot = run(&x.out)?;
    x.screen = boot.clone();
    // Each state's other values, what each reveals exercised before the
    // next, then the value it started at: set, not toggled, since what
    // it revealed may have changed it (a scrim's click closing a panel).
    for s in &d.toggles {
        let moves = match start.get(s).map(String::as_str) {
            Some("true") => vec![format!("set {s} false"), format!("set {s} true")],
            Some(_) => vec![format!("set {s} true"), format!("set {s} false")],
            None => vec![format!("toggle {s}"), format!("toggle {s}")],
        };
        x.state(s, &moves, run);
    }
    for (s, values) in &d.enums {
        let first = start.get(s);
        let mut moves: Vec<String> = values
            .iter()
            .filter(|v| Some(*v) != first)
            .map(|v| format!("set {s} {v}"))
            .collect();
        if let Some(v) = first {
            moves.push(format!("set {s} {v}"));
        }
        x.state(s, &moves, run);
    }
    let boot = x.screen.clone();
    x.level(&boot, 0, run);
    let targets: BTreeSet<&String> = d.hovers.iter().chain(d.inputs.keys()).collect();
    let unseen = targets.iter().filter(|o| !x.done.contains(**o)).count();
    x.out.push(String::new());
    if !x.failed.is_empty() {
        x.out.push(format!(
            "# not exercised, the run failed there: {}",
            x.failed.join(", ")
        ));
    }
    if unseen > 0 {
        x.out.push(format!(
            "# {unseen} of {} elements were never on screen here (behind a state, a list or a \
             service's value), or past the first {MAX_TARGETS} of a kind",
            targets.len()
        ));
    }
    Ok(x.out)
}

struct Explorer<'a> {
    d: &'a Declared,
    out: Vec<String>,
    /// Origins exercised (or given up on).
    done: BTreeSet<String>,
    /// Where the pointer is, on which surface.
    last: Option<(String, [f32; 2])>,
    runs: usize,
    /// Where things were at the end of the last run: what a later run
    /// shows that this did not is what the steps since revealed.
    screen: BTreeMap<String, Spot>,
    hovers: usize,
    inputs: usize,
    failed: Vec<String>,
}

impl Explorer<'_> {
    /// `moves` on state `s`, a rest after each, what each but the last
    /// reveals exercised before the next (the last puts it back).
    fn state(&mut self, s: &str, moves: &[String], run: &mut Runner<'_>) {
        self.out.push(String::new());
        self.out.push(format!("mark state-{}", s.replace('.', "-")));
        for (i, m) in moves.iter().enumerate() {
            let keep = self.out.len();
            self.out.push(m.clone());
            self.out.push("rest".into());
            if i + 1 < moves.len() && !self.reveal(keep, s, 0, run) {
                return;
            }
        }
    }

    fn go(&mut self, p: &Spot) {
        if self.last.as_ref() == Some(&(p.surface.clone(), p.at)) {
            return;
        }
        let step = match &self.last {
            Some((s, from)) if *s == p.surface => format!(
                "glide {} -> {} over 200 on {}",
                pt(*from),
                pt(p.at),
                p.surface
            ),
            _ => format!("pointer {} on {}", pt(p.at), p.surface),
        };
        self.out.push(step);
        self.last = Some((p.surface.clone(), p.at));
    }

    /// Each target in `places` not exercised yet: hovers first, then
    /// inputs, each by origin.
    fn targets(&self, places: &BTreeMap<String, Spot>) -> Vec<(String, Spot)> {
        let hovers = self
            .d
            .hovers
            .iter()
            .filter(|o| !self.d.inputs.contains_key(*o));
        hovers
            .chain(self.d.inputs.keys())
            .filter(|o| !self.done.contains(*o))
            .filter_map(|o| Some((o.clone(), places.get(o)?.clone())))
            .collect()
    }

    /// What a run of the steps so far shows at its end, or `None` (the
    /// steps since `keep` dropped) when it fails or the runs are spent.
    fn look(
        &mut self,
        keep: usize,
        what: &str,
        run: &mut Runner<'_>,
    ) -> Option<Option<BTreeMap<String, Spot>>> {
        if self.runs >= MAX_RUNS {
            return Some(None);
        }
        self.runs += 1;
        match run(&self.out) {
            Ok(p) => Some(Some(p)),
            Err(_) => {
                self.out.truncate(keep);
                self.failed.push(what.to_string());
                None
            }
        }
    }

    fn level(&mut self, places: &BTreeMap<String, Spot>, depth: usize, run: &mut Runner<'_>) {
        for (o, _) in self.targets(places) {
            if self.done.contains(&o) {
                continue;
            }
            // Where it is now: a step since may have moved or hidden it.
            let Some(p) = self.screen.get(&o).cloned() else {
                continue;
            };
            self.done.insert(o.clone());
            let on = self.d.inputs.get(&o).cloned().unwrap_or_default();
            let keep = self.out.len();
            let last = self.last.clone();
            self.out.push(String::new());
            if on.is_empty() {
                if self.hovers >= MAX_TARGETS {
                    self.out.truncate(keep);
                    continue;
                }
                self.hovers += 1;
                self.out
                    .push(format!("mark {}", mark("hover", self.hovers, &p.label)));
                self.out.push(format!("# {o}"));
            } else {
                if self.inputs >= MAX_TARGETS {
                    self.out.truncate(keep);
                    continue;
                }
                self.inputs += 1;
                self.out
                    .push(format!("mark {}", mark("input", self.inputs, &p.label)));
                self.out.push(format!(
                    "# {o}: on {}",
                    on.iter().cloned().collect::<Vec<_>>().join(", on ")
                ));
            }
            self.go(&p);
            self.out.push("rest".into());
            // A popup's anchor reveals the popup; an input may open one.
            let reveals = self.d.anchors.contains(&o) || on.iter().any(|i| i != "scroll");
            let mut first = true;
            let mut at = p;
            for input in &on {
                self.out.push(input_step(input, &at, false));
                self.out.push("rest".into());
                if first && reveals && depth < MAX_DEPTH && input != "scroll" {
                    first = false;
                    if !self.reveal(keep, &o, depth, run) {
                        self.last = last.clone();
                        break;
                    }
                    // The input may have taken it away (a scrim that
                    // closes its surface): then there is no way back.
                    let Some(now) = self.screen.get(&o).cloned() else {
                        break;
                    };
                    at = now;
                    self.go(&at);
                }
                self.out.push(input_step(input, &at, true));
                self.out.push("rest".into());
            }
            if on.is_empty() && reveals && depth < MAX_DEPTH && !self.reveal(keep, &o, depth, run) {
                self.last = last;
            }
        }
    }

    /// Runs the steps so far and exercises what is newly on screen;
    /// false when the run failed (the target's steps are dropped).
    fn reveal(&mut self, keep: usize, o: &str, depth: usize, run: &mut Runner<'_>) -> bool {
        match self.look(keep, o, run) {
            None => false,
            Some(None) => true,
            Some(Some(now)) => {
                let fresh: BTreeMap<String, Spot> = now
                    .iter()
                    .filter(|(k, _)| !self.screen.contains_key(*k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                self.screen = now;
                if !self.targets(&fresh).is_empty() {
                    self.level(&fresh, depth + 1, run);
                }
                true
            }
        }
    }
}

/// The starter's steps for the config the header names: compiled for
/// what it declares, run for where its nodes are.
pub fn explore(header: &str, file: &Path) -> Result<Vec<String>, Error> {
    let scenario = |steps: &[String]| -> Result<Scenario, Error> {
        let text = format!("{header}\n=== steps\n{}\n", steps.join("\n"));
        Scenario::parse_in(&text, file, Place::Local)
    };
    let sc = scenario(&["rest".to_string()])?;
    let (prog, map) = harness::compile(&sc)?;
    let d = declared(&prog, &map);
    let paths: Vec<String> = d
        .toggles
        .iter()
        .chain(d.enums.iter().map(|(p, _)| p))
        .cloned()
        .collect();
    let start = harness::state_values(&sc, &paths)?;
    let mut run = |steps: &[String]| -> Result<BTreeMap<String, Spot>, Error> {
        let sc = scenario(steps)?;
        Ok(places(&harness::run(&sc, &RunOptions::default())?.trace))
    };
    let mut out = steps(&d, &start, &mut run)?;
    // The whole starter, once: a step that no longer finds its surface
    // (a popup a later input closed) is named, not left to surprise, and
    // so is a `rest` that never came (something on screen keeps waking:
    // a live readout, a running clock), which `settle` will report.
    match scenario(&out).and_then(|sc| harness::run(&sc, &RunOptions::default())) {
        Err(e) => out.push(format!("# this starter stops with: {e}")),
        Ok(r) => {
            let never: Vec<String> = r
                .trace
                .rests
                .iter()
                .filter(|x| !x.settled)
                .map(|x| x.line.to_string())
                .collect();
            if !never.is_empty() {
                out.push(format!(
                    "# the rests at lines {} never came within their max: something shown \
                     keeps waking (a live readout, a running clock); `settle` reports each",
                    never.join(", ")
                ));
            }
        }
    }
    Ok(out)
}
