//! (history) The history a `graph` keeps of its value (design.md,
//! "Generative, data-driven and media"; decisions.md history).
//!
//! `graph X { history: D }` shows the last D of X as soon as it mounts.
//! When X exists while no graph shows it (a service field, or an
//! expression over service fields, file-level `let`s, `state`s and
//! settings, and constants, written in the graph), the instance keeps
//! its history here, once per series (its expression), not per graph:
//! every graph of the loaded config, in its files' trees and the
//! components they use, showing the same expression shares one store,
//! whose span is the longest `history` any of them asks for (60 s when
//! unset; a `history` that is not a constant, a literal or a file-level
//! `let` of one, keeps nothing). A value tied to a list item, a
//! component's parameter or `let`, an element's state or the animation
//! clock (`wave`, `noise`) exists only while mounted (and a `when` cannot
//! replace a graph's value, which is positional; were one to, it would
//! not be one expression): such a graph keeps its history from its
//! mount, as before.
//!
//! Each series has a recorder, an effect at file scope that evaluates
//! the expression whenever what it reads changes and appends the value,
//! timed on the boot clock (`strand_scene::backlog::Clocks`), to its
//! ring ([`Ring`]): nothing is written to disk. A value whose time is
//! exact (the input that changed is shown, or reports its changes as
//! they happen) steps; one a hidden service polled ramps from the point
//! before over that service's polling period
//! ([`ServiceHost::sampled`]). Time the machine slept (the boot clock ran
//! ahead of the awake one) is a gap in every ring. The services the
//! series read are kept ([`ServiceHost::keep`]): while nothing shows
//! them, a polled one samples at the hidden period ([`hidden_period`]).
//! A reload recomputes the series: a store whose expression no graph
//! shows any more is freed and its services let go; a config with no
//! such graph keeps nothing and wakes nothing.
//!
//! A mounted graph of a kept series gets its backlog as
//! [`SceneProp::Backlog`] (one [`Backlog`] per series version and
//! `history`, shared by the graphs asking the same, trimmed to it), again
//! each time the ring gains a point while its scope is live (a frozen
//! hidden panel's binding catches up when it is shown), so render fills
//! the columns it did not sample itself.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use strand_core::{NodeId as CoreId, Runtime, Scope, Signal};
use strand_scene::backlog::{ClockFn, Clocks, DEFAULT_HISTORY};
use strand_scene::{Backlog, BacklogPoint, NodeKind, Prop as SceneProp, PropValue};

use crate::hir::{DefId, DefKind};
use crate::lower::{ChunkId, Element, ElementKind, Node, Op, Program};
use crate::ty::Ty;
use crate::vm::{Env, ServiceHost, Value, Vm};

/// The shortest hidden period.
pub const HIDDEN_FLOOR: Duration = Duration::from_secs(2);

/// How many hidden samples span the longest history at least.
pub const HIDDEN_POINTS: u32 = 20;

/// The most points a store keeps (8 KiB of them): values closer together
/// than its span over a little less than this are one point (the
/// latest), which leaves room for the newest point, the one before the
/// span and a gap.
pub const MAX_POINTS: u32 = 512;

/// The closest two points a store keeps, whatever its span: a value
/// changing faster is kept at most this often.
pub const MIN_SPACING: Duration = Duration::from_millis(100);

/// The least the boot clock must run ahead of the awake clock between two
/// readings to be time the machine slept (not their drift).
pub const SLEPT: Duration = Duration::from_secs(2);

/// How often a polled service a series reads samples while nothing shows
/// it, for a store spanning `span`: a twentieth of it, at least 2 s (and
/// never faster than the service samples while shown, which it decides).
pub fn hidden_period(span: Duration) -> Duration {
    (span / HIDDEN_POINTS).max(HIDDEN_FLOOR)
}

/// One kept value: 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Point {
    /// When it was taken: boot clock seconds.
    t: f64,
    /// The value as the graph draws it; not finite: a gap starts here.
    v: f32,
    /// Seconds over which it moved here from the point before (0: a
    /// step, its time exact).
    ramp: f32,
}

impl Point {
    fn gap(&self) -> bool {
        !self.v.is_finite()
    }
}

/// One series' kept values, oldest first. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct Ring {
    points: VecDeque<Point>,
    span: Duration,
}

impl Ring {
    /// An empty ring spanning `span`.
    pub(crate) fn new(span: Duration) -> Ring {
        Ring {
            points: VecDeque::new(),
            span,
        }
    }

    /// Span it anew (a reload's longest `history`).
    fn respan(&mut self, span: Duration, now: f64) {
        self.span = span;
        self.trim(now);
        self.points.shrink_to_fit();
    }

    /// Keep `v`, taken at `t` (boot clock seconds): exactly then
    /// (`polled` `None`), or by a hidden service polling every `polled`,
    /// so it could have changed any time since its last poll. An equal
    /// value is not a point; while the newest point is closer than the
    /// spacing to the one before, `v` replaces it (each spacing keeps its
    /// latest value). `true` when a point was added.
    pub(crate) fn push(&mut self, t: f64, v: f32, polled: Option<Duration>) -> bool {
        let newest = self.points.back().copied();
        if !v.is_finite() || newest.is_some_and(|p| p.v == v) {
            return false;
        }
        let spacing = (self.span / (MAX_POINTS - 4))
            .max(MIN_SPACING)
            .as_secs_f64();
        let n = self.points.len();
        let replace = n >= 2
            && !self.points[n - 1].gap()
            && self.points[n - 1].t - self.points[n - 2].t < spacing;
        if replace {
            self.points.pop_back();
        }
        let ramp = match (polled, self.points.back()) {
            (Some(p), Some(prev)) if !prev.gap() => (t - prev.t).clamp(0.0, p.as_secs_f64()) as f32,
            _ => 0.0,
        };
        self.points.push_back(Point { t, v, ramp });
        self.trim(t);
        !replace
    }

    /// Nothing is known from `t` on until the next value (the machine
    /// slept). `true` when that is news.
    pub(crate) fn gap(&mut self, t: f64) -> bool {
        match self.points.back() {
            Some(p) if !p.gap() => {
                let t = t.max(p.t);
                self.points.push_back(Point {
                    t,
                    v: f32::NAN,
                    ramp: 0.0,
                });
                self.trim(t);
                true
            }
            _ => false,
        }
    }

    /// Drop what is older than the span, but the newest point at or
    /// before its start (what the value was then).
    fn trim(&mut self, now: f64) {
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
    pub(crate) fn oldest_age(&self, now: f64) -> Option<Duration> {
        self.points
            .front()
            .map(|p| Duration::from_secs_f64((now - p.t).max(0.0)))
    }

    /// The points of the last `span` before `now`, and the one in force
    /// as it starts.
    pub(crate) fn backlog(&self, span: Duration, now: f64) -> Backlog {
        let start = now - span.as_secs_f64();
        let first = self.points.partition_point(|p| p.t <= start);
        Backlog {
            points: self
                .points
                .iter()
                .skip(first.saturating_sub(1))
                .map(|p| BacklogPoint {
                    at: p.t,
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
    /// Each graph's value chunk, to its series key and its own `history`.
    pub by_chunk: HashMap<ChunkId, (Rc<str>, Duration)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Want {
    pub chunk: ChunkId,
    pub ty: Ty,
    /// The longest `history` of its graphs.
    pub span: Duration,
    /// The services it reads.
    pub services: BTreeSet<String>,
    /// The service fields it reads, through its `let`s too (`None`: a
    /// service read whole).
    pub inputs: BTreeSet<(String, Option<String>)>,
}

/// The builtins a kept expression may not call: time and handlers' own.
const NOT_KEPT: &[&str] = &["wave", "noise", "sleep", "propagate", "import"];

/// Every graph `prog` mounts (its files' trees and the components they
/// use) whose value is kept, with its history evaluated by `history` (a
/// constant chunk's duration; `None`: not a duration).
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
        // A `when` (or a pose) replacing the value: not one expression.
        let replaced = e.children.iter().any(|n| match n {
            Node::When { props, .. } | Node::Pose { props, .. } => {
                props.iter().any(|p| p.prop == Some(SceneProp::Value))
            }
            _ => false,
        });
        if replaced {
            return;
        }
        let span = match e.props.iter().find(|p| p.prop == Some(SceneProp::History)) {
            None => DEFAULT_HISTORY,
            Some(h) if constant(prog, &lets, h.value, 0) => match history(h.value, &h.ty) {
                Some(d) if !d.is_zero() => d,
                Some(_) => DEFAULT_HISTORY,
                None => return,
            },
            Some(_) => return,
        };
        let mut services = BTreeSet::new();
        let mut inputs = BTreeSet::new();
        if !file_scope(prog, &lets, value.value, &mut services, &mut inputs, 0) {
            return;
        }
        let key: Rc<str> = key(prog, &lets, value.value, 0).into();
        let w = out.series.entry(key.clone()).or_insert_with(|| Want {
            chunk: value.value,
            ty: value.ty.clone(),
            span,
            services,
            inputs,
        });
        w.span = w.span.max(span);
        out.by_chunk.insert(value.value, (key, span));
    };
    let mut seen = HashSet::new();
    for f in &prog.files {
        walk(prog, &f.items, &mut seen, &mut each);
    }
    out
}

/// Visit every element of `nodes`, nested ones included, entering each
/// component's body where it is used, once per component.
fn walk(prog: &Program, nodes: &[Node], seen: &mut HashSet<DefId>, f: &mut dyn FnMut(&Element)) {
    for n in nodes {
        match n {
            Node::Element(e) => {
                f(e);
                if let ElementKind::Component(def) = e.kind
                    && seen.insert(def)
                    && let Some(c) = prog.components.get(&def)
                {
                    walk(prog, &c.body.nodes, seen, f);
                }
                walk(prog, &e.children, seen, f);
            }
            Node::Surface(s) => {
                f(&s.element);
                walk(prog, &s.element.children, seen, f);
                walk(prog, &s.body.nodes, seen, f);
            }
            Node::If { then, else_, .. } => {
                walk(prog, then, seen, f);
                walk(prog, else_, seen, f);
            }
            Node::For(x) => walk(prog, &x.body.nodes, seen, f),
            Node::Match { arms, .. } => {
                for a in arms {
                    walk(prog, a, seen, f);
                }
            }
            _ => {}
        }
    }
}

/// `chunk` is constant: it reads nothing but file-level `let`s that are
/// constant themselves (`let span = 60s`).
fn constant(prog: &Program, lets: &HashMap<DefId, ChunkId>, chunk: ChunkId, depth: u32) -> bool {
    let r = prog.reads(chunk);
    if depth > 16 || !r.locals.is_empty() || !r.services.is_empty() || !r.nodes.is_empty() {
        return false;
    }
    r.defs
        .iter()
        .chain(r.fields.iter().map(|(d, _)| d))
        .all(|d| {
            matches!(prog.def(*d).kind, DefKind::Let)
                && prog.def(*d).owner.is_none()
                && lets
                    .get(d)
                    .is_some_and(|v| constant(prog, lets, *v, depth + 1))
        })
}

/// `chunk` reads only what exists at file scope (services, file-level
/// `let`s, `state`s and settings, constants), and something: its services
/// are added to `services`, the service fields it reads to `inputs`.
fn file_scope(
    prog: &Program,
    lets: &HashMap<DefId, ChunkId>,
    chunk: ChunkId,
    services: &mut BTreeSet<String>,
    inputs: &mut BTreeSet<(String, Option<String>)>,
    depth: u32,
) -> bool {
    if depth > 16 {
        return false;
    }
    inputs.extend(prog.reads(chunk).services.iter().cloned());
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
                        if !file_scope(prog, lets, *v, services, inputs, depth + 1) {
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

/// A config `service`'s declaration as a key part: a reload changing it
/// (a `from poll` command) is another series.
fn declared(prog: &Program, name: &str) -> Option<u64> {
    let decl = prog.services.values().find(|s| s.name == name)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{decl:?}").hash(&mut h);
    Some(h.finish())
}

/// A series' identity: its expression's code with every name spelled
/// out and every file-level `let` it reads written out in full, a config
/// service with its declaration, so the same expression in two graphs
/// (two files, two reloads) is one series and an edit to what it means is
/// another.
fn key(prog: &Program, lets: &HashMap<DefId, ChunkId>, chunk: ChunkId, depth: u32) -> String {
    use std::fmt::Write as _;
    let c = prog.chunk(chunk);
    let name = |i: u32| c.names.get(i as usize).map_or("?", String::as_str);
    let mut out = String::new();
    for op in &c.ops {
        let _ = match op {
            Op::Const(i) => write!(out, "c{:?} ", c.consts.get(*i as usize)),
            Op::Service(n) => match declared(prog, name(*n)) {
                Some(h) => write!(out, "s:{}#{h:x} ", name(*n)),
                None => write!(out, "s:{} ", name(*n)),
            },
            Op::Def(d) => {
                let def = prog.def(*d);
                match (&def.kind, lets.get(d)) {
                    (DefKind::Let, Some(v)) if def.owner.is_none() && depth < 16 => {
                        write!(out, "let({}) ", key(prog, lets, *v, depth + 1))
                    }
                    (DefKind::Service(_), _) => match declared(prog, &def.name) {
                        Some(h) => write!(out, "s:{}#{h:x} ", def.name),
                        None => write!(out, "s:{} ", def.name),
                    },
                    _ => write!(out, "d:{}.{} ", def.module, def.name),
                }
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
    /// The backlogs made of this version, by `history`: graphs asking
    /// the same share one.
    made: RefCell<Vec<(Duration, u64, Arc<Backlog>)>>,
}

/// Every kept series of an instance, across its reloads. See the module
/// docs.
pub(crate) struct Histories {
    /// Owns the series' cells and recorders (never a reload's root).
    scope: Cell<Option<Scope>>,
    series: RefCell<BTreeMap<Rc<str>, Series>>,
    by_chunk: RefCell<HashMap<ChunkId, (Rc<str>, Duration)>>,
    /// What each service was asked to keep (its hidden period).
    kept: RefCell<BTreeMap<String, Duration>>,
    /// The clocks points are timed on (a test passes its own).
    clock: Cell<ClockFn>,
    /// The clocks when last read: a reading by which the boot clock ran
    /// ahead of the awake one is time slept.
    last: Cell<Option<Clocks>>,
}

impl Default for Histories {
    fn default() -> Histories {
        Histories {
            scope: Cell::new(None),
            series: RefCell::default(),
            by_chunk: RefCell::default(),
            kept: RefCell::default(),
            clock: Cell::new(strand_scene::backlog::clocks),
            last: Cell::new(None),
        }
    }
}

impl std::fmt::Debug for Histories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Histories")
            .field("series", &self.series.borrow().keys().collect::<Vec<_>>())
            .field("kept", &self.kept.borrow())
            .finish()
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

impl Histories {
    /// Time points on `clock` from now on (tests).
    pub(crate) fn set_clock(&self, clock: ClockFn) {
        self.clock.set(clock);
        self.last.set(None);
    }

    /// Read the clocks: when the machine slept since the last reading,
    /// every ring gets a gap from then (and, with `rt`, its graphs are
    /// told). The boot clock's seconds now.
    pub(crate) fn observe(&self, rt: Option<&Runtime>) -> f64 {
        let now = (self.clock.get())();
        if let Some(last) = self.last.get() {
            let ran = now.boot.saturating_sub(last.boot);
            let awake = now.awake.saturating_sub(last.awake);
            if ran.saturating_sub(awake) >= SLEPT {
                // Asleep somewhere in between: nothing is known from the
                // last reading on.
                let from = secs(last.boot) + 1e-3;
                for s in self.series.borrow().values() {
                    if s.ring.borrow_mut().gap(from)
                        && let Some(rt) = rt
                    {
                        let n = s.version.get_untracked(rt).unwrap_or(0);
                        let _ = s.version.set(rt, n.wrapping_add(1));
                    }
                }
            }
        }
        self.last.set(Some(now));
        secs(now.boot)
    }

    /// Whether anything is kept.
    pub(crate) fn is_empty(&self) -> bool {
        self.series.borrow().is_empty()
    }

    /// The series a graph with value chunk `chunk` (of the program last
    /// synced) shows, if kept, and the graph's own `history`.
    pub(crate) fn key_of(&self, chunk: ChunkId) -> Option<(Rc<str>, Duration)> {
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
        let now = self.observe(None);
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
                    s.made.borrow_mut().clear();
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
                        made: RefCell::default(),
                    })
                }
            };
            // The recorder runs the new program's code.
            if let Some(r) = s.recorder.take() {
                rt.dispose(r);
            }
            s.recorder = recorder(rt, scope, vm, env, s, Rc::downgrade(self));
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

    /// Series `key`'s backlog of the last `span` now, tracked: its
    /// binding runs again when the ring gains a point. One per version
    /// and span, shared. `Unset` for a series not kept.
    pub(crate) fn backlog(&self, rt: &Runtime, key: &str, span: Duration) -> PropValue {
        let series = self.series.borrow();
        let Some(s) = series.get(key) else {
            return PropValue::Unset;
        };
        let Ok(version) = s.version.get(rt) else {
            return PropValue::Unset;
        };
        let mut made = s.made.borrow_mut();
        if let Some((_, _, b)) = made.iter().find(|(d, v, _)| *d == span && *v == version) {
            return PropValue::Backlog(b.clone());
        }
        made.retain(|(_, v, _)| *v == version);
        let now = secs((self.clock.get())().boot);
        let b = Arc::new(s.ring.borrow().backlog(span, now));
        made.push((span, version, b.clone()));
        PropValue::Backlog(b)
    }

    /// Series `key`'s version cell (what its backlog bindings read).
    pub(crate) fn version(&self, key: &str) -> Option<CoreId> {
        self.series.borrow().get(key).map(|s| s.version.id())
    }

    /// What is kept now, by series ([`crate::instantiate::Instance::histories`]).
    pub(crate) fn infos(&self) -> Vec<HistoryInfo> {
        let kept = self.kept.borrow();
        let now = secs((self.clock.get())().boot);
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
                    oldest: r.oldest_age(now),
                    ring: r
                        .points
                        .iter()
                        .map(|p| BacklogPoint {
                            at: p.t,
                            value: p.v,
                            ramp: p.ramp,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// Seed series `key`'s ring with `points`, oldest first (a glitch
    /// report's replay starting where the live shell was): they go
    /// before what it holds, those not older than its oldest dropped;
    /// `false` for a series not kept.
    pub(crate) fn seed(&self, rt: &Runtime, key: &str, points: &[BacklogPoint]) -> bool {
        let series = self.series.borrow();
        let Some(s) = series.get(key) else {
            return false;
        };
        {
            let mut ring = s.ring.borrow_mut();
            let first = ring.points.front().map_or(f64::INFINITY, |p| p.t);
            let mut seeded: std::collections::VecDeque<Point> = points
                .iter()
                .filter(|p| p.at < first && p.at.is_finite())
                .map(|p| Point {
                    t: p.at,
                    v: p.value,
                    ramp: p.ramp,
                })
                .collect();
            seeded.extend(ring.points.drain(..));
            ring.points = seeded;
            let now = secs((self.clock.get())().boot);
            ring.trim(now);
        }
        let n = s.version.get_untracked(rt).unwrap_or(0);
        let _ = s.version.set(rt, n.wrapping_add(1));
        true
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
    /// Its points, oldest first (a glitch report records them;
    /// [`crate::instantiate::Instance::seed_history`]).
    pub ring: Vec<BacklogPoint>,
}

/// The effect that appends series `s`'s value whenever what it reads
/// changes, owned by `scope`.
fn recorder(
    rt: &Runtime,
    scope: Scope,
    vm: &Rc<Vm>,
    env: &Rc<Env>,
    s: &Series,
    histories: Weak<Histories>,
) -> Option<CoreId> {
    let (vm2, env2) = (vm.clone(), env.clone());
    let ring = s.ring.clone();
    let version = s.version;
    let (chunk, ty) = (s.want.chunk, s.want.ty.clone());
    let inputs: Vec<(String, Option<String>)> = s.want.inputs.iter().cloned().collect();
    // What each input read last: the one that changed says how exact
    // the new value's time is.
    let last: RefCell<Vec<Option<Value>>> = RefCell::new(vec![None; inputs.len()]);
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
                let polled = rt.untrack(|rt| {
                    let mut last = last.borrow_mut();
                    let mut polled: Option<Duration> = None;
                    for (i, (svc, field)) in inputs.iter().enumerate() {
                        let now = field.as_ref().and_then(|f| host.read(rt, svc, f).ok());
                        let changed = now.is_none() || last[i] != now;
                        last[i] = now;
                        if changed && let Some(p) = host.sampled(rt, svc) {
                            polled = Some(polled.map_or(p, |q| q.max(p)));
                        }
                    }
                    polled
                });
                let Some(h) = histories.upgrade() else {
                    return Ok(());
                };
                let t = h.observe(None);
                let added = ring.borrow_mut().push(t, x, polled);
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

    /// Boot clock seconds of the tests: well past a span.
    const T0: f64 = 1000.0;

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
    /// it, and it never holds more than 512 points.
    #[test]
    fn a_ring_keeps_no_more_than_its_span() {
        let mut r = Ring::new(s(60.0));
        // Ten minutes of a value changing every 20 ms, shown.
        let mut t = 0.0;
        let mut max = 0;
        while t < 600.0 {
            r.push(T0 + t, (t * 7.0).sin() as f32, None);
            max = max.max(r.len());
            t += 0.02;
        }
        let now = T0 + t - 0.02;
        let b = r.backlog(s(60.0), now);
        let ages: Vec<f64> = b.points.iter().map(|p| now - p.at).collect();
        assert!(ages[0] >= 60.0 && ages[0] < 60.2, "{}", ages[0]);
        assert!(ages[1..].iter().all(|a| *a <= 60.0), "within the span");
        // 60 s at the 118 ms spacing: never more than 512 points.
        assert!(max <= MAX_POINTS as usize, "{max}");
        assert!(b.points.len() > 450, "{}", b.points.len());
        // A graph asking 10 s gets 10 s of it (and the point before).
        let short = r.backlog(s(10.0), now);
        assert!(now - short.points[0].at >= 10.0 && now - short.points[1].at < 10.0);
        // Hidden every 3 s for ten minutes: 21 points.
        let mut r = Ring::new(s(60.0));
        let mut t = 0.0;
        while t < 600.0 {
            r.push(T0 + t, ((t / 3.0) % 2.0) as f32, Some(s(3.0)));
            t += 3.0;
        }
        assert!(r.len() <= 22, "{}", r.len());
        assert!(r.oldest_age(T0 + t - 3.0).unwrap() <= s(63.0));
    }

    /// An exact value steps; a polled one ramps over its service's
    /// period, however long (a disk polled every 10 s, a slow poll), and
    /// never over more than the time since the point before.
    #[test]
    fn exact_points_step_and_polled_ones_ramp_over_their_period() {
        let mut r = Ring::new(s(60.0));
        assert!(r.push(T0, 0.1, None));
        assert!(r.push(T0 + 1.0, 0.2, None));
        assert!(!r.push(T0 + 2.0, 0.2, None), "an equal value is no point");
        assert!(r.push(T0 + 4.0, 0.5, Some(s(3.0))));
        assert!(r.push(T0 + 14.0, 0.9, Some(s(3.0))));
        assert!(r.push(T0 + 24.0, 0.3, Some(s(10.0))));
        assert!(r.push(T0 + 25.0, 0.4, Some(s(10.0))));
        let b = r.backlog(s(60.0), T0 + 25.0);
        let ramps: Vec<f32> = b.points.iter().map(|p| p.ramp).collect();
        assert_eq!(ramps, [0.0, 0.0, 3.0, 3.0, 10.0, 1.0]);
        assert!(
            (b.value_at(T0 + 2.0).unwrap() - 0.3).abs() < 1e-6,
            "a third up"
        );
        assert_eq!(b.value_at(T0 + 9.0), Some(0.5), "held before the next ramp");
        assert!(
            (b.value_at(T0 + 19.0).unwrap() - 0.6).abs() < 1e-6,
            "10 s ramp"
        );
    }

    /// Before the clock has run a span, nothing is old enough to drop
    /// (values at time zero included).
    #[test]
    fn the_first_span_drops_nothing() {
        let mut r = Ring::new(s(60.0));
        assert!(r.push(0.0, 0.1, None));
        assert!(r.push(0.0, 0.2, None));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn a_respan_trims_at_once() {
        let mut r = Ring::new(s(60.0));
        for i in 0..60 {
            r.push(T0 + f64::from(i), i as f32, None);
        }
        r.respan(s(10.0), T0 + 59.0);
        assert_eq!(r.len(), 11);
        assert_eq!(r.oldest_age(T0 + 59.0), Some(s(10.0)));
    }

    /// A gap is one point, the value after it steps, and a gap is never
    /// replaced by the spacing.
    #[test]
    fn a_gap_is_kept_and_the_next_value_steps() {
        let mut r = Ring::new(s(60.0));
        r.push(T0, 0.1, None);
        assert!(r.gap(T0 + 0.01));
        assert!(!r.gap(T0 + 0.02), "one gap at a time");
        assert!(r.push(T0 + 0.05, 0.4, Some(s(3.0))));
        let b = r.backlog(s(60.0), T0 + 1.0);
        assert_eq!(b.points.len(), 3);
        assert!(b.points[1].value.is_nan());
        assert_eq!(b.points[2].ramp, 0.0, "no ramp out of a gap");
    }
}
