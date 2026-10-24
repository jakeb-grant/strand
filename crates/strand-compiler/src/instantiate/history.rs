//! (history) The history a `graph` keeps of its value (design.md,
//! "Generative, data-driven and media"; decisions.md history).
//!
//! `graph X { history: D }` shows the last D of X as soon as it is shown.
//! When X exists while no graph shows it (a service field, or an
//! expression over service fields, file-level `let`s and `state`s and
//! constants, written in the graph), the instance keeps its history here,
//! once per series (its expression), not per graph: every graph of the
//! loaded config showing the same expression shares one store, whose span
//! is the longest `history` any of them asks for (60 s when unset; a
//! computed `history:` keeps nothing). A value tied to a list item, a
//! component's parameter or `let`, an element or time exists only while
//! mounted: such a graph keeps its history from its mount, as before.
//!
//! Each series has a recorder, an effect at file scope that evaluates
//! the expression whenever what it reads changes and appends the value
//! with the logic clock's time to its ring ([`Ring`]): nothing is written
//! to disk. A value taken while every service it reads has a visible
//! reader steps; one taken while one has none is a coarse sample, which
//! ramps from the point before over the hidden period. The services the
//! series read are kept ([`ServiceHost::keep`]): while nothing shows them
//! they sample at the hidden period ([`hidden_period`]), coarser than
//! while shown. A reload recomputes the series: a store whose expression
//! no graph shows any more is freed and its services let go (they stop
//! sampling while hidden, and stop 5 s after their last reader as
//! before); a config with no such graph keeps nothing and wakes nothing.
//!
//! A mounted graph of a kept series gets its backlog as
//! [`SceneProp::Backlog`] (a [`Backlog`] of ages before now), again each
//! time the ring gains a point while its scope is live (a frozen hidden
//! panel's binding catches up when it is shown), so render fills the
//! columns it did not sample itself.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_core::{NodeId as CoreId, Runtime, Scope, Signal};
use strand_scene::backlog::DEFAULT_HISTORY;
use strand_scene::{Backlog, BacklogPoint, NodeKind, Prop as SceneProp, PropValue};

use crate::hir::{DefId, DefKind};
use crate::lower::{ChunkId, Element, ElementKind, Node, Op, Program};
use crate::ty::Ty;
use crate::vm::{Env, ServiceHost, Vm};

/// The shortest hidden period.
pub const HIDDEN_FLOOR: Duration = Duration::from_secs(2);

/// How many hidden samples span the longest history at least.
pub const HIDDEN_POINTS: u32 = 20;

/// The most points a store keeps (8 KiB of them): values closer together
/// than its span over a little less than this are one point (the
/// latest), which leaves room for the newest point, the one before the
/// span and rounding.
pub const MAX_POINTS: u32 = 512;

/// The closest two points a store keeps, whatever its span: a value
/// changing faster is kept at most this often.
pub const MIN_SPACING: Duration = Duration::from_millis(100);

/// How often a service a series reads samples while nothing shows it,
/// for a store spanning `span`: a twentieth of it, at least 2 s (and
/// never faster than the service samples while shown, which it decides).
pub fn hidden_period(span: Duration) -> Duration {
    (span / HIDDEN_POINTS).max(HIDDEN_FLOOR)
}

/// One kept value: 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    /// The logic clock's time it was taken at, in seconds.
    t: f64,
    /// The value as the graph draws it.
    v: f32,
    /// Seconds over which it moved here from the point before (0: a
    /// step, taken while shown).
    ramp: f32,
}

/// One series' kept values, oldest first. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Ring {
    points: VecDeque<Point>,
    span: Duration,
    every: Duration,
}

impl Ring {
    /// An empty ring spanning `span`.
    pub(crate) fn new(span: Duration) -> Ring {
        Ring {
            points: VecDeque::new(),
            span,
            every: hidden_period(span),
        }
    }

    /// Span it anew (a reload's longest `history`).
    fn respan(&mut self, span: Duration, now: Duration) {
        self.span = span;
        self.every = hidden_period(span);
        self.trim(now);
        self.points.shrink_to_fit();
    }

    /// Keep `v`, taken at `t` while shown or not. An equal value is not a
    /// point; while the newest point is closer than the spacing to the
    /// one before, `v` replaces it (each spacing keeps its latest value).
    /// `true` when a point was added.
    pub(crate) fn push(&mut self, t: Duration, v: f32, shown: bool) -> bool {
        if !v.is_finite() || self.points.back().is_some_and(|p| p.v == v) {
            return false;
        }
        let t = t.as_secs_f64();
        let spacing = (self.span / (MAX_POINTS - 4))
            .max(MIN_SPACING)
            .as_secs_f64();
        let n = self.points.len();
        let replace = n >= 2 && self.points[n - 1].t - self.points[n - 2].t < spacing;
        if replace {
            self.points.pop_back();
        }
        let ramp = match (shown, self.points.back()) {
            (false, Some(prev)) => (t - prev.t).clamp(0.0, self.every.as_secs_f64()) as f32,
            _ => 0.0,
        };
        self.points.push_back(Point { t, v, ramp });
        self.trim_at(t);
        !replace
    }

    /// Drop what is older than the span, but the newest point at or
    /// before its start (what the value was then).
    fn trim(&mut self, now: Duration) {
        self.trim_at(now.as_secs_f64());
    }

    fn trim_at(&mut self, now: f64) {
        let start = now - self.span.as_secs_f64();
        if start <= 0.0 {
            // The clock has not run a span yet: nothing is that old.
            return;
        }
        while self.points.len() >= 2 && self.points[1].t <= start {
            self.points.pop_front();
        }
    }

    /// How many points it holds.
    pub(crate) fn len(&self) -> usize {
        self.points.len()
    }

    /// The age of the oldest point at `now`.
    pub(crate) fn oldest_age(&self, now: Duration) -> Option<Duration> {
        self.points
            .front()
            .map(|p| Duration::from_secs_f64((now.as_secs_f64() - p.t).max(0.0)))
    }

    /// The points as ages before `now`.
    pub(crate) fn backlog(&mut self, now: Duration) -> Backlog {
        self.trim(now);
        let now = now.as_secs_f64();
        Backlog {
            points: self
                .points
                .iter()
                .map(|p| BacklogPoint {
                    age: (now - p.t).max(0.0) as f32,
                    value: p.v,
                    ramp: p.ramp,
                })
                .collect(),
        }
    }
}

/// What the graphs of a program ask to keep: one entry per series.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Wanted {
    /// Expression chunk and its type, per series key.
    pub series: BTreeMap<Rc<str>, Want>,
    /// Each graph's value chunk, to its series key.
    pub by_chunk: HashMap<ChunkId, Rc<str>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Want {
    pub chunk: ChunkId,
    pub ty: Ty,
    /// The longest `history` of its graphs.
    pub span: Duration,
    /// The services it reads.
    pub services: BTreeSet<String>,
}

/// The builtins a kept expression may not call: time and handlers' own.
const NOT_KEPT: &[&str] = &["wave", "noise", "sleep", "propagate", "import"];

/// Every graph of `prog` whose value is kept, with its history evaluated
/// by `history` (a constant chunk's duration; `None`: not constant).
pub(crate) fn wanted(prog: &Program, history: &dyn Fn(ChunkId, &Ty) -> Option<Duration>) -> Wanted {
    let lets: HashMap<DefId, ChunkId> = prog
        .files
        .iter()
        .flat_map(|f| f.items.iter())
        .filter_map(|n| match n {
            Node::Let { def, value } => Some((*def, *value)),
            _ => None,
        })
        .collect();
    let mut out = Wanted {
        series: BTreeMap::new(),
        by_chunk: HashMap::new(),
    };
    let mut each = |e: &Element| {
        if !matches!(e.kind, ElementKind::Builtin(NodeKind::Graph)) {
            return;
        }
        let Some(value) = e
            .arg
            .iter()
            .chain(&e.props)
            .find(|p| p.prop == Some(SceneProp::Value))
        else {
            return;
        };
        let span = match e.props.iter().find(|p| p.prop == Some(SceneProp::History)) {
            None => DEFAULT_HISTORY,
            Some(h) => match prog.reads.get(h.value as usize) {
                Some(r) if *r == crate::lower::Reads::default() => match history(h.value, &h.ty) {
                    Some(d) if !d.is_zero() => d,
                    Some(_) => DEFAULT_HISTORY,
                    None => return,
                },
                _ => return,
            },
        };
        let mut services = BTreeSet::new();
        if !file_scope(prog, &lets, value.value, &mut services, 0) {
            return;
        }
        let key: Rc<str> = key(prog, value.value).into();
        let w = out.series.entry(key.clone()).or_insert_with(|| Want {
            chunk: value.value,
            ty: value.ty.clone(),
            span,
            services: services.clone(),
        });
        w.span = w.span.max(span);
        out.by_chunk.insert(value.value, key);
    };
    for f in &prog.files {
        walk(&f.items, &mut each);
    }
    for c in prog.components.values() {
        walk(&c.body.nodes, &mut each);
    }
    out
}

/// Visit every element of `nodes`, nested ones included.
fn walk(nodes: &[Node], f: &mut dyn FnMut(&Element)) {
    for n in nodes {
        match n {
            Node::Element(e) => {
                f(e);
                walk(&e.children, f);
            }
            Node::Surface(s) => {
                f(&s.element);
                walk(&s.element.children, f);
                walk(&s.body.nodes, f);
            }
            Node::If { then, else_, .. } => {
                walk(then, f);
                walk(else_, f);
            }
            Node::For(x) => walk(&x.body.nodes, f),
            Node::Match { arms, .. } => {
                for a in arms {
                    walk(a, f);
                }
            }
            _ => {}
        }
    }
}

/// `chunk` reads only what exists at file scope (services, file-level
/// `let`s, `state`s and settings, constants), and something: its services
/// are added to `services`.
fn file_scope(
    prog: &Program,
    lets: &HashMap<DefId, ChunkId>,
    chunk: ChunkId,
    services: &mut BTreeSet<String>,
    depth: u32,
) -> bool {
    if depth > 16 {
        return false;
    }
    let c = prog.chunk(chunk);
    let mut reads = false;
    for op in &c.ops {
        match op {
            Op::Const(_)
            | Op::Null
            | Op::Unit
            | Op::Bool(_)
            | Op::Field(_)
            | Op::Index
            | Op::Unary(_)
            | Op::Binary(_)
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::AndJump(_)
            | Op::OrJump(_)
            | Op::CoalesceJump(_)
            | Op::NullJump(_)
            | Op::Pop
            | Op::Dup
            | Op::List(_)
            | Op::Commas(_)
            | Op::Spaced(_)
            | Op::Variant(..)
            | Op::EnumType(_)
            | Op::MakeRecord { .. }
            | Op::Match(_)
            | Op::Fail(_) => {}
            Op::CallMethod { action: false, .. } => {}
            Op::CallBuiltin { name, .. } => {
                if NOT_KEPT.contains(&c.names[*name as usize].as_str()) {
                    return false;
                }
            }
            Op::Service(n) => {
                reads = true;
                services.insert(c.names[*n as usize].clone());
            }
            Op::Def(d) => {
                let def = prog.def(*d);
                match &def.kind {
                    DefKind::Service(_) => {
                        reads = true;
                        services.insert(def.name.clone());
                    }
                    DefKind::State | DefKind::Settings if def.owner.is_none() => reads = true,
                    DefKind::Let if def.owner.is_none() => {
                        let Some(v) = lets.get(d) else { return false };
                        if !file_scope(prog, lets, *v, services, depth + 1) {
                            return false;
                        }
                        reads = true;
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    reads
}

/// A series' identity: its expression's code with every name spelled
/// out, so the same expression in two graphs (two files, two reloads) is
/// one series.
fn key(prog: &Program, chunk: ChunkId) -> String {
    use std::fmt::Write as _;
    let c = prog.chunk(chunk);
    let name = |i: u32| c.names.get(i as usize).map_or("?", String::as_str);
    let mut out = String::new();
    for op in &c.ops {
        let _ = match op {
            Op::Const(i) => write!(out, "c{:?} ", c.consts.get(*i as usize)),
            Op::Service(n) => write!(out, "s:{} ", name(*n)),
            Op::Def(d) => {
                let def = prog.def(*d);
                write!(out, "d:{}.{} ", def.module, def.name)
            }
            Op::Field(n) => write!(out, ".{} ", name(*n)),
            Op::CallBuiltin {
                name: n,
                overload,
                args,
            } => write!(
                out,
                "b:{}/{overload}/{:?} ",
                name(*n),
                c.args.get(*args as usize)
            ),
            Op::CallMethod { name: n, args, .. } => {
                write!(out, "m:{}/{:?} ", name(*n), c.args.get(*args as usize))
            }
            Op::Match(p) => write!(out, "p{:?} ", c.patterns.get(*p as usize)),
            Op::Fail(n) => write!(out, "f:{} ", name(*n)),
            other => write!(out, "{other:?} "),
        };
    }
    out
}

/// One kept series.
struct Series {
    want: Want,
    ring: Rc<RefCell<Ring>>,
    /// Bumped when the ring gains a point: mounted graphs' backlogs read
    /// it.
    version: Signal<u64>,
    recorder: Option<CoreId>,
}

/// Every kept series of an instance, across its reloads. See the module
/// docs.
#[derive(Default)]
pub(crate) struct Histories {
    /// Owns the series' cells and recorders (never a reload's root).
    scope: Cell<Option<Scope>>,
    series: RefCell<BTreeMap<Rc<str>, Series>>,
    by_chunk: RefCell<HashMap<ChunkId, Rc<str>>>,
    /// What each service was asked to keep (its hidden period).
    kept: RefCell<BTreeMap<String, Duration>>,
}

impl std::fmt::Debug for Histories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histories")
            .field("series", &self.series.borrow().keys().collect::<Vec<_>>())
            .field("kept", &self.kept.borrow())
            .finish()
    }
}

impl Histories {
    /// The series a graph with value chunk `chunk` (of the program last
    /// synced) shows, if kept.
    pub(crate) fn key_of(&self, chunk: ChunkId) -> Option<Rc<str>> {
        self.by_chunk.borrow().get(&chunk).cloned()
    }

    /// Follow the program `vm` runs: keep what its graphs want, free what
    /// they no longer do, and tell the services ([`ServiceHost::keep`]).
    /// Called by every boot and reload, once its file scope is declared.
    pub(crate) fn sync(self: &Rc<Self>, rt: &Runtime, ctx: &Rc<super::Ctx>, env: &Rc<Env>) {
        let vm = &ctx.vm;
        let scope = match self.scope.get() {
            Some(s) => s,
            None => {
                // Never under the instance tree: a reload disposes that.
                let s = rt.untrack(|rt| rt.scope(|_| ()).0);
                let _ = rt.reparent(s.id(), None);
                self.scope.set(Some(s));
                s
            }
        };
        let types = &vm.prog.types;
        let wanted = wanted(&vm.prog, &|chunk, ty| {
            let v = rt.untrack(|rt| vm.eval(rt, chunk, env)).ok()?;
            match super::convert::prop_value_for(types, SceneProp::History, ty, &v) {
                PropValue::Duration(d) => Some(d),
                _ => None,
            }
        });
        let now = rt.now();
        let mut series = self.series.borrow_mut();
        // Gone: freed, its recorder and cells disposed.
        let gone: Vec<Rc<str>> = series
            .keys()
            .filter(|k| !wanted.series.contains_key(*k))
            .cloned()
            .collect();
        for k in gone {
            if let Some(s) = series.remove(&k) {
                if let Some(r) = s.recorder {
                    rt.dispose(r);
                }
                s.version.dispose(rt);
            }
        }
        for (k, want) in &wanted.series {
            let s = match series.get_mut(k) {
                Some(s) => {
                    s.ring.borrow_mut().respan(want.span, now);
                    s.want = want.clone();
                    s
                }
                None => {
                    let version = rt
                        .with_owner(scope.id(), |rt| rt.signal(0u64))
                        .unwrap_or_else(|_| rt.signal(0u64));
                    rt.set_name(version.id(), format!("history {k}"));
                    series.entry(k.clone()).or_insert(Series {
                        want: want.clone(),
                        ring: Rc::new(RefCell::new(Ring::new(want.span))),
                        version,
                        recorder: None,
                    })
                }
            };
            // The recorder runs the new program's code.
            if let Some(r) = s.recorder.take() {
                rt.dispose(r);
            }
            s.recorder = recorder(rt, scope, vm, env, s);
            // What it reads, declared as a binding's is (a service's
            // cells are read untracked).
            if let Some(r) = s.recorder {
                ctx.declare_reads(rt, r, &[s.want.chunk], env, &[]);
            }
        }
        *self.by_chunk.borrow_mut() = wanted.by_chunk;
        // Each service sampled as often as its most demanding series asks.
        let mut every: BTreeMap<String, Duration> = BTreeMap::new();
        for s in series.values() {
            let p = hidden_period(s.want.span);
            for svc in &s.want.services {
                every
                    .entry(svc.clone())
                    .and_modify(|e| *e = (*e).min(p))
                    .or_insert(p);
            }
        }
        drop(series);
        let mut kept = self.kept.borrow_mut();
        let host = &vm.host;
        for (svc, _) in kept.iter().filter(|(s, _)| !every.contains_key(*s)) {
            host.keep(rt, svc, None);
        }
        // Every boot and reload says so again: a reload may route a name
        // to another host (a config `service` shadowing a builtin one).
        for (svc, p) in &every {
            host.keep(rt, svc, Some(*p));
        }
        *kept = every;
    }

    /// Free every series and let every service go (the instance shuts
    /// down).
    pub(crate) fn shutdown(&self, rt: &Runtime, host: &dyn ServiceHost) {
        self.series.borrow_mut().clear();
        self.by_chunk.borrow_mut().clear();
        if let Some(s) = self.scope.take() {
            s.dispose(rt);
        }
        let kept = std::mem::take(&mut *self.kept.borrow_mut());
        for svc in kept.keys() {
            host.keep(rt, svc, None);
        }
    }

    /// Series `key`'s backlog now, tracked: its binding runs again when
    /// the ring gains a point. `Unset` for a series not kept.
    pub(crate) fn backlog(&self, rt: &Runtime, key: &str) -> PropValue {
        let series = self.series.borrow();
        let Some(s) = series.get(key) else {
            return PropValue::Unset;
        };
        if s.version.get(rt).is_err() {
            return PropValue::Unset;
        }
        let b = s.ring.borrow_mut().backlog(rt.now());
        PropValue::Backlog(Arc::new(b))
    }

    /// Series `key`'s version cell (what its backlog bindings read).
    pub(crate) fn version(&self, key: &str) -> Option<CoreId> {
        self.series.borrow().get(key).map(|s| s.version.id())
    }

    /// What is kept now, by series ([`crate::instantiate::Instance::histories`]).
    pub(crate) fn infos(&self, rt: &Runtime) -> Vec<HistoryInfo> {
        let kept = self.kept.borrow();
        self.series
            .borrow()
            .iter()
            .map(|(k, s)| {
                let r = s.ring.borrow();
                HistoryInfo {
                    key: k.to_string(),
                    span: s.want.span,
                    services: s
                        .want
                        .services
                        .iter()
                        .map(|svc| (svc.clone(), kept.get(svc).copied()))
                        .collect(),
                    points: r.len(),
                    oldest: r.oldest_age(rt.now()),
                }
            })
            .collect()
    }
}

/// (history) One kept series, as [`crate::instantiate::Instance::histories`]
/// tells it.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryInfo {
    /// Its identity: the expression's code with names spelled out.
    pub key: String,
    /// The longest `history` of its graphs: what it keeps.
    pub span: Duration,
    /// The services it reads, each with the hidden period it was asked
    /// to sample at.
    pub services: Vec<(String, Option<Duration>)>,
    /// How many values it holds.
    pub points: usize,
    /// The age of the oldest.
    pub oldest: Option<Duration>,
}

/// The effect that appends series `s`'s value whenever what it reads
/// changes, owned by `scope`.
fn recorder(rt: &Runtime, scope: Scope, vm: &Rc<Vm>, env: &Rc<Env>, s: &Series) -> Option<CoreId> {
    let (vm2, env2) = (vm.clone(), env.clone());
    let ring = s.ring.clone();
    let version = s.version;
    let (chunk, ty) = (s.want.chunk, s.want.ty.clone());
    let services: Vec<String> = s.want.services.iter().cloned().collect();
    let host: Rc<dyn ServiceHost> = vm.host.clone();
    let effect = rt
        .with_owner(scope.id(), |rt| {
            rt.effect(move |rt| {
                // A value that fails or is not a number keeps nothing.
                let Ok(v) = vm2.eval(rt, chunk, &env2) else {
                    return Ok(());
                };
                let types = &vm2.prog.types;
                let Some(x) =
                    super::convert::prop_value_for(types, SceneProp::Value, &ty, &v).as_number()
                else {
                    return Ok(());
                };
                let shown = rt.untrack(|rt| services.iter().all(|s| host.shown(rt, s)));
                let added = ring.borrow_mut().push(rt.now(), x, shown);
                if added {
                    let n = version.get_untracked(rt).unwrap_or(0);
                    let _ = version.set(rt, n.wrapping_add(1));
                }
                Ok(())
            })
        })
        .ok()?;
    let _ = rt.writes_to(effect.id(), version.id());
    rt.set_name(effect.id(), "history recorder");
    Some(effect.id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: f64) -> Duration {
        Duration::from_secs_f64(n)
    }

    #[test]
    fn a_point_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Point>(), 16);
    }

    #[test]
    fn the_hidden_period_is_a_twentieth_of_the_span_at_least_2_s() {
        assert_eq!(hidden_period(s(60.0)), s(3.0));
        assert_eq!(hidden_period(s(10.0)), s(2.0));
        assert_eq!(hidden_period(s(600.0)), s(30.0));
    }

    /// The store keeps no more than its span: whatever it is fed, every
    /// point but one (what the value was as the span starts) is inside
    /// it, and it never holds more than its spacing allows.
    #[test]
    fn a_ring_keeps_no_more_than_its_span() {
        let mut r = Ring::new(s(60.0));
        // Ten minutes of a value changing every 20 ms, shown.
        let mut t = 0.0;
        let mut max = 0;
        while t < 600.0 {
            r.push(s(t), (t * 7.0).sin() as f32, true);
            max = max.max(r.len());
            t += 0.02;
        }
        let now = s(t - 0.02);
        let b = r.backlog(now);
        let ages: Vec<f32> = b.points.iter().map(|p| p.age).collect();
        assert!(ages[0] >= 60.0 && ages[0] < 60.2, "{}", ages[0]);
        assert!(ages[1..].iter().all(|a| *a <= 60.0), "within the span");
        // 60 s at the 118 ms spacing: never more than 512 points.
        assert!(max <= MAX_POINTS as usize, "{max}");
        assert!(b.points.len() > 450, "{}", b.points.len());
        // Hidden every 3 s for ten minutes: 21 points.
        let mut r = Ring::new(s(60.0));
        let mut t = 0.0;
        while t < 600.0 {
            r.push(s(t), ((t / 3.0) % 2.0) as f32, false);
            t += 3.0;
        }
        assert!(r.len() <= 22, "{}", r.len());
        assert!(r.oldest_age(s(t - 3.0)).unwrap() <= s(63.0));
    }

    #[test]
    fn shown_points_step_and_hidden_ones_ramp_over_the_period() {
        let mut r = Ring::new(s(60.0));
        assert!(r.push(s(0.0), 0.1, true));
        assert!(r.push(s(1.0), 0.2, true));
        assert!(!r.push(s(2.0), 0.2, true), "an equal value is no point");
        // Hidden: the next sample comes 3 s on, the one after 10 s on (it
        // was unchanged in between): ramps at most the period.
        assert!(r.push(s(4.0), 0.5, false));
        assert!(r.push(s(14.0), 0.9, false));
        let b = r.backlog(s(14.0));
        let ramps: Vec<f32> = b.points.iter().map(|p| p.ramp).collect();
        assert_eq!(ramps, [0.0, 0.0, 3.0, 3.0]);
        assert!((b.value_at(12.0).unwrap() - 0.3).abs() < 1e-6, "a third up");
        assert_eq!(b.value_at(5.0), Some(0.5), "held before the next ramp");
        assert_eq!(b.value_at(0.0), Some(0.9));
    }

    /// Before the clock has run a span, nothing is old enough to drop
    /// (values at time zero included).
    #[test]
    fn the_first_span_drops_nothing() {
        let mut r = Ring::new(s(60.0));
        assert!(r.push(s(0.0), 0.1, true));
        assert!(r.push(s(0.0), 0.2, true));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn a_respan_trims_at_once() {
        let mut r = Ring::new(s(60.0));
        for i in 0..60 {
            r.push(s(f64::from(i)), i as f32, true);
        }
        r.respan(s(10.0), s(59.0));
        assert_eq!(r.len(), 11);
        assert_eq!(r.oldest_age(s(59.0)), Some(s(10.0)));
    }
}
