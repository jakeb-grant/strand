//! (M6) `id.box` never drives what moves it back (spec.md 6.3,
//! decisions.md m6-w2-surface, the owner's rulings of 2026-10-14).
//!
//! An element's laid-out box reaches logic a frame after layout put it
//! there. A binding on it that laid that box out again would chase its
//! own tail a frame at a time: a width that follows the box of the row it
//! widens, two absolute boxes each sized from the other, a bar item and a
//! popup's island each sized from the other's box. So the checker builds
//! the graph of the boxes each binding on a box can move, and refuses
//! every cycle in it (`check::box_layout_loop`), naming its path.
//!
//! What a binding moves follows from what it sets:
//!
//! - paint props (`x`, `y`, `opacity`, colours, transforms) move no box
//!   (a box is where layout put the element, before its paint offsets);
//!   on a surface neither do its effects (`bg`, `opacity`, `shadow`,
//!   `blur`, …), its corners or `open`;
//! - a layout prop of an element that is always `place: absolute` (no
//!   `when` or pose changes its `place`) moves its box and the boxes
//!   inside it; of any other element, every box on its surface, a popup's
//!   own box included (its size and its anchor place it);
//! - any other prop of a surface moves every box on it and the boxes of
//!   the popups opened from it;
//! - a `bar`'s size, `edge`, `margin`, `open` or `screens` change its
//!   exclusive zone, which places and sizes every layer surface (`bar`,
//!   `panel`, `osd`) on its output: every box on each of them (which
//!   output is not known here); so does whatever lays out a bar whose
//!   thickness is its content's;
//! - an `if`, `for` or `match`, `pages`' `current` and `options`, a `set
//!   { $token: … }` and the condition of a `when { id: … }` mount,
//!   restyle or rename on their surface: every box on it. A component's
//!   or a file's tokens reach anywhere: every box;
//! - and what moves a popup's own box moves every box on it, and so on
//!   through popups inside it: inherited values (`set`, `font`, `weight`,
//!   a surface's own props) reach into the popups nested in what they
//!   set, which render lays out again.
//!
//! What a value depends on is followed, until nothing changes, through
//! `let`s, `state`s and service fields (what handlers assign them, and
//! under which conditions), `fn`s, component parameters (from each
//! call), `for` bindings and conditional ids (the conditions that pick
//! the node, for every read of the name). `n.width` and `n.height` are
//! the same frame-late fact as `n.box`, but for an element's own size
//! read in its own props (a container query, which switches with
//! hysteresis); a cycle through them with no `n.box` on it loaded before
//! `.box` existed and is a warning, `check::size_loop` (spec.md 14). A box the checker cannot place (a node
//! passed as a value) may be any box: it may drive only what moves none.

use std::collections::{BTreeSet, HashMap, VecDeque};

use super::Checker;
use crate::hir::{
    self, Callee, DefId, ElementKind, Event, Expr, ExprKind, LocalId, LocalKind, Node, NodeIdx,
    Stmt, StmtKind,
};
use crate::syntax::Span;
use crate::ty::Ty;
use strand_scene::{NodeKind, Prop as SceneProp, PropClass};

/// A fact about a box a value depends on.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Src {
    /// `n.box`.
    Box(NodeIdx),
    /// `n.width` or `n.height`.
    Size(NodeIdx),
    /// A box the checker cannot place.
    Unknown,
}

type Taint = BTreeSet<Src>;

/// The body an element is written in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Body {
    Component(DefId),
    /// A top-level surface (or a file's own elements).
    Top(usize, usize),
}

/// Where an element sits, statically.
#[derive(Clone, Debug)]
struct Site {
    body: Body,
    /// Its elements around it in its body, outermost first.
    ancestors: Vec<NodeIdx>,
    /// The nearest surface element around it in its body.
    surface: Option<NodeIdx>,
    is_surface: bool,
    /// Always `place: absolute`.
    absolute: bool,
    /// A `slot` lies inside it: what a call passes is inside it too.
    slot: bool,
    /// A top-level `bar`, `panel` or `osd`: a layer surface, which a
    /// bar's exclusive zone moves.
    layer: bool,
    /// A `bar` whose thickness is its content's (no `width`, `height` or
    /// `size`): what lays it out changes its exclusive zone.
    auto_zone: bool,
    kind: String,
    name: Option<String>,
}

/// What a binding sets.
#[derive(Copy, Clone, Debug)]
enum At {
    /// A prop of the element that can move boxes.
    Prop(NodeIdx),
    /// A `bar`'s prop that changes its exclusive zone (its size, `edge`,
    /// `margin`, `open`, `screens`): it moves every layer surface.
    Zone(NodeIdx),
    /// An `if`, `for` or `match`: its body, the surface around it and the
    /// element it is in.
    Mount(Body, Option<NodeIdx>, Option<NodeIdx>),
    /// The condition of a `when { id: … }` on the element.
    Name(NodeIdx),
    /// Tokens any element may read.
    Everywhere,
}

/// What a binding reads: boxes (`n.box`) and other nodes' sizes
/// (`n.width`, `n.height`), and whether a box it cannot place.
#[derive(Default)]
struct Read {
    boxes: BTreeSet<NodeIdx>,
    sizes: BTreeSet<NodeIdx>,
    unknown: bool,
}

/// A binding to check once the dependences are known.
struct Use<'h> {
    module: usize,
    at: At,
    what: String,
    span: Span,
    exprs: Vec<&'h Expr>,
}

#[derive(Default)]
struct Flow<'h> {
    sites: HashMap<NodeIdx, Site>,
    /// A conditional id's alias index → the elements it may name.
    members: HashMap<NodeIdx, Vec<NodeIdx>>,
    /// A conditional id's alias index → the conditions that name it.
    alias_conds: HashMap<NodeIdx, Vec<&'h Expr>>,
    /// What those conditions depend on, settled.
    aliases: HashMap<NodeIdx, Taint>,
    /// Component → its call elements.
    calls: HashMap<DefId, Vec<NodeIdx>>,
    locals: HashMap<LocalId, Taint>,
    defs: HashMap<DefId, Taint>,
    /// A service field (`brightness.level`) → what handlers write it from.
    fields: HashMap<(String, String), Taint>,
    uses: Vec<Use<'h>>,
    /// The `fn` bodies and handler/timer bodies with their conditions, for
    /// the fixpoint: (statements, what triggers them).
    bodies: Vec<(&'h [Stmt], Vec<&'h Expr>)>,
    fns: Vec<&'h hir::FnDecl>,
    /// Component parameter flows: (param local, argument).
    args: Vec<(LocalId, &'h Expr)>,
    /// `let`, `state` values and `for` bindings: (target, value).
    def_values: Vec<(DefId, &'h Expr)>,
    local_values: Vec<(LocalId, &'h Expr)>,
    node_rec: Option<crate::ty::RecordId>,
}

impl Checker<'_> {
    /// Reports each binding on a box that can move that box again
    /// (`check::box_layout_loop`).
    pub(super) fn box_loops(&mut self, files: &[hir::FileHir]) {
        let mut flow = Flow {
            node_rec: self.types.find_schema_record("Node"),
            ..Flow::default()
        };
        let params: HashMap<DefId, Vec<LocalId>> = files
            .iter()
            .flat_map(|f| &f.items)
            .filter_map(|i| match i {
                hir::Item::Component(c) => {
                    Some((c.def, c.params.iter().map(|p| p.local).collect()))
                }
                _ => None,
            })
            .collect();
        for (m, f) in files.iter().enumerate() {
            for (k, item) in f.items.iter().enumerate() {
                match item {
                    hir::Item::Component(c) => {
                        for p in &c.params {
                            if let Some(d) = &p.default {
                                flow.local_values.push((p.local, d));
                            }
                        }
                        let name = &self.defs[c.def.0 as usize].name;
                        tokens(m, &format!("${name}."), &c.tokens, &mut flow);
                        let mut w = Walk::new(self, &params, m, Body::Component(c.def));
                        w.nodes(&c.body, &mut flow);
                    }
                    hir::Item::Surface(s) => {
                        let mut w = Walk::new(self, &params, m, Body::Top(m, k));
                        w.element(&s.element, &mut flow);
                    }
                    hir::Item::State(s) => state_values(s, &mut flow),
                    hir::Item::Let(l) => flow.def_values.push((l.def, &l.value)),
                    hir::Item::Fn(f) => flow.fns.push(f),
                    hir::Item::Handler(h) => handler(h, Vec::new(), &mut flow),
                    hir::Item::Timer(t) => timer(t, Vec::new(), &mut flow),
                    hir::Item::Tokens(t) => tokens(m, "$", &t.entries, &mut flow),
                    hir::Item::Use(u) => {
                        for e in u.tokens.iter().chain(&u.palette) {
                            flow.uses.push(Use {
                                module: m,
                                at: At::Everywhere,
                                what: "use".into(),
                                span: e.span,
                                exprs: vec![e],
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        // Most configs read no box: nothing to follow.
        if !flow.reads_any_box() {
            return;
        }
        flow.settle(self);
        self.box_report(&flow);
    }

    fn box_report(&mut self, flow: &Flow<'_>) {
        let on: HashMap<NodeIdx, Option<BTreeSet<NodeIdx>>> =
            flow.sites.keys().map(|n| (*n, flow.on(*n))).collect();
        // Each binding on a box: the boxes it reads, and those it moves.
        let mut reads: HashMap<usize, Read> = HashMap::new();
        let mut moves: HashMap<usize, BTreeSet<NodeIdx>> = HashMap::new();
        let mut readers: HashMap<NodeIdx, Vec<usize>> = HashMap::new();
        for (i, u) in flow.uses.iter().enumerate() {
            let mut taint = Taint::new();
            for e in &u.exprs {
                taint.extend(flow.taint(self, e));
            }
            if taint.is_empty() {
                continue;
            }
            let moved = flow.moves(u.at, &on);
            if moved.is_empty() {
                continue;
            }
            let own = match u.at {
                At::Prop(e) | At::Zone(e) | At::Name(e) => Some(e),
                At::Mount(_, _, around) => around,
                At::Everywhere => None,
            };
            let mut read = Read::default();
            for s in &taint {
                match *s {
                    Src::Box(n) => {
                        read.boxes.insert(n);
                    }
                    // An element's own size in its own props: a query.
                    Src::Size(n) if Some(n) != own => {
                        read.sizes.insert(n);
                    }
                    Src::Size(_) => {}
                    Src::Unknown => read.unknown = true,
                }
            }
            for n in read.boxes.union(&read.sizes) {
                readers.entry(*n).or_default().push(i);
            }
            reads.insert(i, read);
            moves.insert(i, moved);
        }
        let mut order: Vec<usize> = reads.keys().copied().collect();
        order.sort_unstable();
        let saved = self.module;
        for i in &order {
            let u = &flow.uses[*i];
            let unknown = reads.get(i).is_some_and(|r| r.unknown);
            let subject = match u.at {
                At::Mount(..) => format!("this `{}`", u.what),
                At::Name(_) => "this `when`'s condition".to_string(),
                _ => format!("`{}`", u.what),
            };
            let help = match u.at {
                At::Mount(..) | At::Name(_) => {
                    "choose with something layout does not change (a state, a service value), and move or fade what follows the box with paint props (`x`, `opacity`)"
                }
                _ => {
                    "`.box` is where layout put an element, a frame late: a value on it may set what moves no box it depends on (paint props such as `x`, `y`, `opacity` and colours, a surface's effects, the size of a `place: absolute` element it is not inside, or what is on another surface and reads nothing back)"
                }
            };
            self.module = u.module;
            if unknown {
                self.error(
                    "check::box_layout_loop",
                    format!(
                        "{subject} cannot follow the box of a node passed as a value: it may be any box, one this moves included"
                    ),
                    u.span,
                    "follows a box the checker cannot place",
                )
                .help = Some(format!(
                    "read the box by the node's `id:` where it is written; {help}"
                ));
                continue;
            }
            let Some((path, boxed)) = flow.cycle(*i, &reads, &moves, &readers) else {
                continue;
            };
            // Each box on the path, named by how the next binding reads it
            // (the last box is the first, read by `i`).
            let label = |j: usize| {
                let next = path.get(j + 1).map_or(path[1].1, |s| s.1);
                let n = path[j].0;
                if reads.get(&next).is_some_and(|r| r.boxes.contains(&n)) {
                    flow.box_label(n)
                } else {
                    flow.size_label(n)
                }
            };
            let mut text = vec![label(0)];
            for (j, (_, by)) in path.iter().enumerate().skip(1) {
                text.push(flow.use_label(*by));
                text.push(label(j));
            }
            let start = label(0);
            let others: Vec<(crate::source::FileId, Span)> = path
                .iter()
                .skip(1)
                .map(|(_, by)| *by)
                .filter(|by| by != i)
                .map(|by| (self.modules[flow.uses[by].module].file, flow.uses[by].span))
                .collect();
            let d = if boxed {
                let message = format!(
                    "{subject} depends on {start}, which it moves again: {}",
                    text.join(" → ")
                );
                let d = self.error("check::box_layout_loop", message, u.span, "a box loop");
                d.help = Some(help.into());
                d
            } else {
                // Only sizes on the loop: `n.width` loaded before `.box`
                // (spec.md 14), so it is a warning.
                let message = format!(
                    "{subject} depends on {start}, which it changes again: {}",
                    text.join(" → ")
                );
                let d = self.warning(
                    "check::size_loop",
                    message,
                    u.span,
                    "may change a frame at a time",
                );
                d.help = Some(
                    "another node's `width` and `height` arrive a frame after layout, as its `.box` does: a layout prop on one that lays it out again may settle late or keep changing; set paint props from it, or a size that moves nothing it reads"
                        .into(),
                );
                d
            };
            for (file, span) in others {
                d.add_secondary(file, span, "the loop goes on here");
            }
        }
        self.module = saved;
    }
}

fn state_values<'h>(s: &'h hir::StateDecl, flow: &mut Flow<'h>) {
    if let hir::StateInit::Value { value, .. } = &s.init {
        flow.def_values.push((s.def, value));
    }
}

fn handler<'h>(h: &'h hir::Handler, mut ctx: Vec<&'h Expr>, flow: &mut Flow<'h>) {
    if let Event::Change { targets, .. } = &h.event {
        ctx.extend(targets.iter());
    }
    flow.bodies.push((&h.body, ctx));
}

fn timer<'h>(t: &'h hir::Timer, mut ctx: Vec<&'h Expr>, flow: &mut Flow<'h>) {
    ctx.extend(t.while_.iter());
    flow.bodies.push((&t.body, ctx));
}

/// A component's or a file's tokens: any element may read them.
fn tokens<'h>(module: usize, prefix: &str, defs: &'h [hir::TokenDef], flow: &mut Flow<'h>) {
    for t in defs {
        flow.uses.push(Use {
            module,
            at: At::Everywhere,
            what: format!("{prefix}{}", t.path),
            span: t.span,
            exprs: vec![&t.value],
        });
    }
}

/// The walk over one body's elements.
struct Walk<'c, 'k> {
    checker: &'c Checker<'k>,
    params: &'c HashMap<DefId, Vec<LocalId>>,
    module: usize,
    body: Body,
    ancestors: Vec<NodeIdx>,
    surfaces: Vec<NodeIdx>,
}

impl<'c, 'k> Walk<'c, 'k> {
    fn new(
        checker: &'c Checker<'k>,
        params: &'c HashMap<DefId, Vec<LocalId>>,
        module: usize,
        body: Body,
    ) -> Self {
        Walk {
            checker,
            params,
            module,
            body,
            ancestors: Vec::new(),
            surfaces: Vec::new(),
        }
    }

    fn nodes<'h>(&mut self, nodes: &'h [Node], flow: &mut Flow<'h>) {
        for n in nodes {
            self.node(n, flow);
        }
    }

    fn structure<'h>(&mut self, what: &str, span: Span, exprs: Vec<&'h Expr>, flow: &mut Flow<'h>) {
        flow.uses.push(Use {
            module: self.module,
            at: At::Mount(
                self.body,
                self.surfaces.last().copied(),
                self.ancestors.last().copied(),
            ),
            what: what.into(),
            span,
            exprs,
        });
    }

    fn node<'h>(&mut self, n: &'h Node, flow: &mut Flow<'h>) {
        match n {
            Node::Element(e) => self.element(e, flow),
            Node::If(i) => {
                self.structure("if", i.span, vec![&i.cond], flow);
                self.nodes(&i.then, flow);
                self.nodes(&i.else_, flow);
            }
            Node::For(f) => {
                self.structure("for", f.span, vec![&f.iter], flow);
                flow.local_values.push((f.binding, &f.iter));
                self.nodes(&f.body, flow);
            }
            Node::Match(m) => {
                self.structure("match", m.span, vec![&m.scrutinee], flow);
                for (_, arm) in &m.arms {
                    self.nodes(arm, flow);
                }
            }
            Node::Slot(_) => {
                for a in &self.ancestors {
                    if let Some(s) = flow.sites.get_mut(a) {
                        s.slot = true;
                    }
                }
            }
            // `set { $token: … }` restyles what is inside its element.
            Node::Set(defs, _) => {
                for t in defs {
                    let what = format!("${}", t.path);
                    match self.ancestors.last() {
                        Some(e) => flow.uses.push(Use {
                            module: self.module,
                            at: At::Prop(*e),
                            what,
                            span: t.span,
                            exprs: vec![&t.value],
                        }),
                        None => self.structure(&what, t.span, vec![&t.value], flow),
                    }
                }
            }
            Node::Handler(h) => handler(h, Vec::new(), flow),
            Node::Timer(t) => timer(t, Vec::new(), flow),
            Node::State(s) => state_values(s, flow),
            Node::Let(l) => flow.def_values.push((l.def, &l.value)),
            _ => {}
        }
    }

    fn element<'h>(&mut self, e: &'h hir::Element, flow: &mut Flow<'h>) {
        let (kind, kind_name) = match &e.kind {
            ElementKind::Builtin(k) => (NodeKind::from_name(k), k.clone()),
            ElementKind::Component(d) => (None, self.checker.defs[d.0 as usize].name.clone()),
            ElementKind::Unknown(k) => (None, k.clone()),
        };
        let is_surface = kind.is_some_and(NodeKind::is_surface);
        let types = &self.checker.types;
        let is_absolute = |p: &hir::Prop| {
            matches!(p.value.kind, ExprKind::Variant(en, v)
                if types.enum_(en).variants.get(v as usize).is_some_and(|n| n == "absolute"))
        };
        // Its props, its `when`s' under their conditions and its poses'.
        let mut props: Vec<(&'h hir::Prop, Option<&'h Expr>)> =
            e.props.iter().map(|p| (p, None)).collect();
        for c in &e.children {
            match c {
                Node::When(w) => props.extend(w.props.iter().map(|p| (p, Some(&w.cond)))),
                Node::Pose(p) => props.extend(p.props.iter().map(|p| (p, None))),
                _ => {}
            }
        }
        // Absolute only if no `when` or pose changes `place` (a `set`
        // gives tokens, which `place: absolute` written as a constant
        // does not read).
        let places: Vec<&hir::Prop> = props
            .iter()
            .filter(|(p, _)| p.name == "place")
            .map(|(p, _)| *p)
            .collect();
        let absolute = !places.is_empty()
            && places.iter().all(|p| is_absolute(p))
            && e.props.iter().any(|p| p.name == "place");
        let name = e.id.map(|l| self.checker.locals[l.0 as usize].name.clone());
        let top = is_surface && self.surfaces.is_empty() && matches!(self.body, Body::Top(..));
        let layer = top && matches!(kind_name.as_str(), "bar" | "panel" | "osd");
        let auto_zone = top
            && kind_name == "bar"
            && !props
                .iter()
                .any(|(p, _)| matches!(p.name.as_str(), "width" | "height" | "size"));
        flow.sites.insert(
            e.node,
            Site {
                body: self.body,
                ancestors: self.ancestors.clone(),
                surface: self.surfaces.last().copied(),
                is_surface,
                absolute,
                slot: false,
                layer,
                auto_zone,
                kind: kind_name.clone(),
                name,
            },
        );
        for c in &e.children {
            let Node::When(w) = c else { continue };
            // A conditional id names this element while `cond` holds.
            if let Some(p) = w.props.iter().find(|p| p.name == "id")
                && let ExprKind::Local(l) = p.value.kind
                && let LocalKind::NodeAlias(a) = self.checker.locals[l.0 as usize].kind
            {
                flow.members.entry(a).or_default().push(e.node);
                flow.alias_conds.entry(a).or_default().push(&w.cond);
                let alias = self.checker.locals[l.0 as usize].name.clone();
                if let Some(s) = flow.sites.get_mut(&e.node) {
                    s.name.get_or_insert(alias.clone());
                }
                flow.uses.push(Use {
                    module: self.module,
                    at: At::Name(e.node),
                    what: alias,
                    span: w.cond.span,
                    exprs: vec![&w.cond],
                });
            }
        }
        match &e.kind {
            ElementKind::Component(d) => {
                flow.calls.entry(*d).or_default().push(e.node);
                let params = self.params.get(d).map(Vec::as_slice).unwrap_or_default();
                if let (Some(a), Some(first)) = (&e.arg, params.first()) {
                    flow.args.push((*first, a));
                }
                for (p, cond) in &props {
                    let Some(local) = params
                        .iter()
                        .find(|l| self.checker.locals[l.0 as usize].name == p.name)
                    else {
                        continue;
                    };
                    flow.args.push((*local, &p.value));
                    if let Some(c) = cond {
                        flow.args.push((*local, c));
                    }
                }
            }
            ElementKind::Builtin(k) => {
                let schema = self.checker.schema.element(k);
                let mut checked = Vec::new();
                if let (Some(a), Some(s)) = (&e.arg, schema)
                    && let Some(name) = &s.arg_prop
                {
                    checked.push((name.clone(), name.clone(), a.span, vec![a]));
                }
                for (p, cond) in &props {
                    if p.name == "id" {
                        continue;
                    }
                    let mut exprs = vec![&p.value];
                    exprs.extend(*cond);
                    checked.push((p.name.clone(), p.name.clone(), p.span, exprs.clone()));
                    for s in &p.sub {
                        let mut exprs = vec![&s.value];
                        exprs.extend(*cond);
                        checked.push((s.name.clone(), p.name.clone(), s.span, exprs));
                    }
                }
                // Each with the prop it is in (a sub-prop's parent).
                for (name, of, span, exprs) in checked {
                    let at = if layer && k == "bar" && zone_prop(&of) {
                        At::Zone(e.node)
                    } else if moves_boxes(&name, k, is_surface) {
                        At::Prop(e.node)
                    } else {
                        continue;
                    };
                    flow.uses.push(Use {
                        module: self.module,
                        at,
                        what: name,
                        span,
                        exprs,
                    });
                }
            }
            ElementKind::Unknown(_) => {}
        }
        self.ancestors.push(e.node);
        if is_surface {
            self.surfaces.push(e.node);
        }
        self.nodes(&e.children, flow);
        if is_surface {
            self.surfaces.pop();
        }
        self.ancestors.pop();
    }
}

/// Whether `prop` on a `bar` changes its exclusive zone, which moves the
/// other layer surfaces on its output.
fn zone_prop(prop: &str) -> bool {
    matches!(
        prop,
        "width" | "height" | "size" | "edge" | "margin" | "open" | "screens"
    )
}

/// Whether `prop`, set on an element of kind `kind`, can move a box: a
/// layout prop, `pages`' `current` (it mounts the page by value), and on
/// a surface everything but its effects, its corners and `open`.
fn moves_boxes(prop: &str, kind: &str, surface: bool) -> bool {
    let p = SceneProp::from_name(prop);
    if surface {
        return p.is_none_or(|p| {
            !(p.class() == PropClass::Effects
                || matches!(p, SceneProp::Open | SceneProp::Radius | SceneProp::Corners))
        });
    }
    (kind == "pages" && prop == "current") || p.is_some_and(SceneProp::affects_layout)
}

/// Calls `f` on `e` and every expression inside it.
fn each(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Field { base, .. } => each(base, f),
        ExprKind::Call { callee, args } => {
            match callee {
                Callee::Method { receiver, .. } => each(receiver, f),
                Callee::Value(v) => each(v, f),
                _ => {}
            }
            for a in args {
                each(&a.value, f);
            }
        }
        ExprKind::Index { base, index } => {
            each(base, f);
            each(index, f);
        }
        ExprKind::Unary { expr, .. } => each(expr, f),
        ExprKind::Binary { lhs, rhs, .. } => {
            each(lhs, f);
            each(rhs, f);
        }
        ExprKind::Ternary { cond, then, else_ } => {
            each(cond, f);
            each(then, f);
            each(else_, f);
        }
        ExprKind::Lambda { body, .. } => match body {
            hir::LambdaBody::Expr(x) => each(x, f),
            hir::LambdaBody::Block(stmts) => each_stmt(stmts, &mut Vec::new(), &mut |x, _| f(x)),
        },
        ExprKind::Match { scrutinee, arms } => {
            each(scrutinee, f);
            for (p, x) in arms {
                if let hir::Pattern::Literal(l) = p {
                    each(l, f);
                }
                each(x, f);
            }
        }
        ExprKind::List(xs) | ExprKind::Commas(xs) | ExprKind::Spaced(xs) => {
            for x in xs {
                each(x, f);
            }
        }
        _ => {}
    }
}

/// Calls `f` on each expression of `stmts` with the conditions it runs
/// under (`ctx`); an assignment's target is passed too.
fn each_stmt<'h>(
    stmts: &'h [Stmt],
    ctx: &mut Vec<&'h Expr>,
    f: &mut dyn FnMut(&'h Expr, &[&'h Expr]),
) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) | StmtKind::Play(value) => {
                f(value, ctx)
            }
            StmtKind::Assign { target, value, .. } => {
                f(target, ctx);
                f(value, ctx);
            }
            StmtKind::If { cond, then, else_ } => {
                f(cond, ctx);
                ctx.push(cond);
                each_stmt(then, ctx, f);
                each_stmt(else_, ctx, f);
                ctx.pop();
            }
            StmtKind::For { iter, body, .. } => {
                f(iter, ctx);
                ctx.push(iter);
                each_stmt(body, ctx, f);
                ctx.pop();
            }
            StmtKind::Match { scrutinee, arms } => {
                f(scrutinee, ctx);
                ctx.push(scrutinee);
                for (_, arm) in arms {
                    each_stmt(arm, ctx, f);
                }
                ctx.pop();
            }
            StmtKind::Error => {}
        }
    }
}

/// The `state` (or settings file) an assignment target writes.
fn target_def(e: &Expr) -> Option<DefId> {
    match &e.kind {
        ExprKind::Def(d) => Some(*d),
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => target_def(base),
        _ => None,
    }
}

/// The service field an assignment target writes (`brightness.level`).
fn target_field(e: &Expr) -> Option<(String, String)> {
    match &e.kind {
        ExprKind::Field { base, name, .. } => match &base.kind {
            ExprKind::Service(s) => Some((s.clone(), name.clone())),
            _ => target_field(base),
        },
        ExprKind::Index { base, .. } => target_field(base),
        _ => None,
    }
}

/// Whether two sets of surfaces may meet (`None`: not known, so they may).
fn meet(a: Option<&BTreeSet<NodeIdx>>, b: Option<&BTreeSet<NodeIdx>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => !a.is_disjoint(b),
        _ => true,
    }
}

impl Flow<'_> {
    /// Whether `e` reads a laid-out fact of a node: `.box`, or `.width` or
    /// `.height` of a node.
    fn reads_fact(&self, c: &Checker<'_>, e: &Expr) -> bool {
        let ExprKind::Field { base, name, .. } = &e.kind else {
            return false;
        };
        name == "box"
            || (matches!(name.as_str(), "width" | "height")
                && match &base.kind {
                    ExprKind::Node(_) => true,
                    ExprKind::Local(l) => alias(c, *l).is_some(),
                    _ => matches!(&base.ty, Ty::Record(r) if Some(*r) == self.node_rec),
                })
    }

    /// Whether anything collected reads a box at all.
    fn reads_any_box(&self) -> bool {
        let mut found = false;
        let mut look = |e: &Expr| {
            each(e, &mut |x| {
                if let ExprKind::Field { name, .. } = &x.kind
                    && matches!(name.as_str(), "box" | "width" | "height")
                {
                    found = true;
                }
            })
        };
        for u in &self.uses {
            u.exprs.iter().for_each(|e| look(e));
        }
        for (_, e) in &self.def_values {
            look(e);
        }
        for (_, e) in self.local_values.iter().chain(&self.args) {
            look(e);
        }
        for (stmts, trig) in &self.bodies {
            trig.iter().for_each(|e| look(e));
            each_stmt(stmts, &mut Vec::new(), &mut |x, _| look(x));
        }
        for f in &self.fns {
            each_stmt(&f.body, &mut Vec::new(), &mut |x, _| look(x));
        }
        found
    }

    /// What `e` depends on, as far as the maps know.
    fn taint(&self, c: &Checker<'_>, e: &Expr) -> Taint {
        let mut out = Taint::new();
        each(e, &mut |x| match &x.kind {
            // A service field a handler writes from a box.
            ExprKind::Field { base, name, .. } if matches!(base.kind, ExprKind::Service(_)) => {
                if let ExprKind::Service(s) = &base.kind
                    && let Some(t) = self.fields.get(&(s.clone(), name.clone()))
                {
                    out.extend(t.iter().copied());
                }
            }
            ExprKind::Local(l) => {
                if let Some(t) = self.locals.get(l) {
                    out.extend(t.iter().copied());
                }
                // Which node a conditional id names follows its conditions.
                if let Some(t) = alias(c, *l).and_then(|a| self.aliases.get(&a)) {
                    out.extend(t.iter().copied());
                }
            }
            ExprKind::Def(d) => {
                if let Some(t) = self.defs.get(d) {
                    out.extend(t.iter().copied());
                }
            }
            ExprKind::Call {
                callee: Callee::Fn(d),
                ..
            } => {
                if let Some(t) = self.defs.get(d) {
                    out.extend(t.iter().copied());
                }
            }
            ExprKind::Field { base, name, .. } if self.reads_fact(c, x) => {
                let of = |n: NodeIdx| match name.as_str() {
                    "box" => Src::Box(n),
                    _ => Src::Size(n),
                };
                match &base.kind {
                    ExprKind::Node(n) => {
                        out.insert(of(*n));
                    }
                    ExprKind::Local(l) if alias(c, *l).is_some() => {
                        match alias(c, *l).and_then(|a| self.members.get(&a)) {
                            Some(m) => out.extend(m.iter().map(|n| of(*n))),
                            None if name == "box" => {
                                out.insert(Src::Unknown);
                            }
                            None => {}
                        }
                    }
                    // A node passed as a value: its box may be any box. Its
                    // size loaded before `.box` and is not followed (spec.md
                    // 14).
                    _ if name == "box"
                        && matches!(&base.ty, Ty::Record(r) if Some(*r) == self.node_rec) =>
                    {
                        out.insert(Src::Unknown);
                    }
                    _ => {}
                }
            }
            _ => {}
        });
        out
    }

    /// Follows dependences through `let`s, `state`s, `fn`s, parameters,
    /// bindings and conditional ids until nothing changes.
    fn settle(&mut self, c: &Checker<'_>) {
        // Each round adds at least one source somewhere or stops: the sets
        // only grow, and each is finite (the sources a program names), so
        // it ends, however long a chain is.
        loop {
            let mut changed = false;
            let mut add_local = |flow: &mut Self, l: LocalId, t: Taint| {
                if t.is_empty() {
                    return;
                }
                let have = flow.locals.entry(l).or_default();
                let before = have.len();
                have.extend(t);
                changed |= have.len() != before;
            };
            for i in 0..self.local_values.len() {
                let (l, e) = self.local_values[i];
                let t = self.taint(c, e);
                add_local(self, l, t);
            }
            for i in 0..self.args.len() {
                let (l, e) = self.args[i];
                let t = self.taint(c, e);
                add_local(self, l, t);
            }
            let mut def_adds: Vec<(DefId, Taint)> = Vec::new();
            for (d, e) in &self.def_values {
                def_adds.push((*d, self.taint(c, e)));
            }
            // Assignments in handlers, timers and fns, and `let`s in them.
            let mut local_adds: Vec<(LocalId, Taint)> = Vec::new();
            let mut field_adds: Vec<((String, String), Taint)> = Vec::new();
            for (stmts, trig) in &self.bodies {
                let mut base = Taint::new();
                for t in trig {
                    base.extend(self.taint(c, t));
                }
                self.stmt_flows(
                    c,
                    stmts,
                    &base,
                    &mut def_adds,
                    &mut local_adds,
                    &mut field_adds,
                );
            }
            for f in &self.fns {
                // A `fn`'s value depends on everything its body reads.
                let mut all = Taint::new();
                each_stmt(&f.body, &mut Vec::new(), &mut |x, _| {
                    all.extend(self.taint(c, x));
                });
                def_adds.push((f.def, all));
                self.stmt_flows(
                    c,
                    &f.body,
                    &Taint::new(),
                    &mut def_adds,
                    &mut local_adds,
                    &mut field_adds,
                );
            }
            // A `fn`'s parameters take what its callers pass.
            let fns: HashMap<DefId, &hir::FnDecl> = self.fns.iter().map(|f| (f.def, *f)).collect();
            let mut calls: Vec<(LocalId, Taint)> = Vec::new();
            let mut visit = |e: &Expr| {
                each(e, &mut |x| {
                    if let ExprKind::Call {
                        callee: Callee::Fn(d),
                        args,
                    } = &x.kind
                        && let Some(f) = fns.get(d)
                    {
                        for a in args {
                            if let Some(p) = a.param.and_then(|i| f.params.get(i)) {
                                calls.push((*p, self.taint(c, &a.value)));
                            }
                        }
                    }
                });
            };
            for u in &self.uses {
                u.exprs.iter().for_each(|e| visit(e));
            }
            for (_, e) in self.def_values.iter() {
                visit(e);
            }
            for (_, e) in self.local_values.iter().chain(self.args.iter()) {
                visit(e);
            }
            for (stmts, _) in &self.bodies {
                each_stmt(stmts, &mut Vec::new(), &mut |x, _| visit(x));
            }
            for f in &self.fns {
                each_stmt(&f.body, &mut Vec::new(), &mut |x, _| visit(x));
            }
            // A conditional id depends on its conditions.
            let mut alias_adds: Vec<(NodeIdx, Taint)> = Vec::new();
            for (a, conds) in &self.alias_conds {
                let mut t = Taint::new();
                for x in conds {
                    t.extend(self.taint(c, x));
                }
                alias_adds.push((*a, t));
            }
            for (l, t) in local_adds.into_iter().chain(calls) {
                add_local(self, l, t);
            }
            for (d, t) in def_adds {
                if t.is_empty() {
                    continue;
                }
                let have = self.defs.entry(d).or_default();
                let before = have.len();
                have.extend(t);
                changed |= have.len() != before;
            }
            for (k, t) in field_adds {
                if t.is_empty() {
                    continue;
                }
                let have = self.fields.entry(k).or_default();
                let before = have.len();
                have.extend(t);
                changed |= have.len() != before;
            }
            for (a, t) in alias_adds {
                if t.is_empty() {
                    continue;
                }
                let have = self.aliases.entry(a).or_default();
                let before = have.len();
                have.extend(t);
                changed |= have.len() != before;
            }
            if !changed {
                break;
            }
        }
    }

    /// What the assignments and `let`s of `stmts` take, under `base` and
    /// the conditions around them.
    fn stmt_flows(
        &self,
        c: &Checker<'_>,
        stmts: &[Stmt],
        base: &Taint,
        defs: &mut Vec<(DefId, Taint)>,
        locals: &mut Vec<(LocalId, Taint)>,
        fields: &mut Vec<((String, String), Taint)>,
    ) {
        let mut ctx = Vec::new();
        let mut walk = |stmts: &[Stmt], ctx: &mut Vec<&Expr>| {
            for s in stmts {
                let mut under = base.clone();
                for x in ctx.iter() {
                    under.extend(self.taint(c, x));
                }
                match &s.kind {
                    StmtKind::Assign { target, value, .. } => {
                        under.extend(self.taint(c, value));
                        if let Some(d) = target_def(target) {
                            defs.push((d, under));
                        } else if let Some(k) = target_field(target) {
                            fields.push((k, under));
                        }
                    }
                    StmtKind::Let { local, value } => {
                        under.extend(self.taint(c, value));
                        locals.push((*local, under));
                    }
                    StmtKind::For { binding, iter, .. } => {
                        under.extend(self.taint(c, iter));
                        locals.push((*binding, under));
                    }
                    _ => {}
                }
            }
        };
        nested(stmts, &mut ctx, &mut walk);
    }

    /// The surfaces element `n`'s box is on (`None`: not known): its
    /// surface, and for a surface itself also the surface it opens from
    /// (a popup's box is its place there).
    fn on(&self, n: NodeIdx) -> Option<BTreeSet<NodeIdx>> {
        let s = self.sites.get(&n)?;
        if !s.is_surface {
            return self.around(s.body, s.surface);
        }
        if s.surface.is_none() && matches!(s.body, Body::Top(..)) {
            return Some(BTreeSet::from([n]));
        }
        let mut out = self.around(s.body, s.surface)?;
        out.insert(n);
        Some(out)
    }

    /// The surfaces an element `e`'s props lay out.
    fn target_surfaces(&self, e: NodeIdx) -> Option<BTreeSet<NodeIdx>> {
        let s = self.sites.get(&e)?;
        if s.is_surface {
            return Some(BTreeSet::from([e]));
        }
        self.around(s.body, s.surface)
    }

    /// The surfaces a place in `body` under `surface` is on: that
    /// surface, or for a component's top, those of each place it is
    /// called from.
    fn around(&self, body: Body, surface: Option<NodeIdx>) -> Option<BTreeSet<NodeIdx>> {
        self.around_seen(body, surface, &mut Vec::new())
    }

    fn around_seen(
        &self,
        body: Body,
        surface: Option<NodeIdx>,
        seen: &mut Vec<DefId>,
    ) -> Option<BTreeSet<NodeIdx>> {
        if let Some(s) = surface {
            return Some(BTreeSet::from([s]));
        }
        let Body::Component(d) = body else {
            return None;
        };
        if seen.contains(&d) {
            return Some(BTreeSet::new());
        }
        seen.push(d);
        let mut out = BTreeSet::new();
        for call in self.calls.get(&d).into_iter().flatten() {
            let s = self.sites.get(call)?;
            out.extend(self.around_seen(s.body, s.surface, seen)?);
        }
        seen.pop();
        // Never called: its surfaces are not known.
        (!out.is_empty()).then_some(out)
    }

    /// Whether `b`'s box lies inside element `e` (or is it): in its
    /// subtree, in a component called there, or passed into a `slot` it
    /// holds. Not known counts as inside.
    fn inside(&self, b: NodeIdx, e: NodeIdx, seen: &mut Vec<DefId>) -> bool {
        if b == e {
            return true;
        }
        let (Some(bs), Some(es)) = (self.sites.get(&b), self.sites.get(&e)) else {
            return true;
        };
        if bs.body == es.body && bs.ancestors.contains(&e) {
            return true;
        }
        if let Body::Component(d) = bs.body
            && !seen.contains(&d)
        {
            seen.push(d);
            let called_inside = self
                .calls
                .get(&d)
                .into_iter()
                .flatten()
                .any(|c| self.inside(*c, e, seen));
            seen.pop();
            if called_inside {
                return true;
            }
        }
        if es.slot
            && let Body::Component(d) = es.body
            && let Some(calls) = self.calls.get(&d)
        {
            return bs.ancestors.iter().any(|a| calls.contains(a));
        }
        false
    }

    /// The boxes a binding at `at` can move.
    fn moves(&self, at: At, on: &HashMap<NodeIdx, Option<BTreeSet<NodeIdx>>>) -> BTreeSet<NodeIdx> {
        let boxes_on = |t: Option<&BTreeSet<NodeIdx>>| -> BTreeSet<NodeIdx> {
            on.iter()
                .filter(|(_, s)| meet(s.as_ref(), t))
                .map(|(n, _)| *n)
                .collect()
        };
        let layers: BTreeSet<NodeIdx> = self
            .sites
            .iter()
            .filter(|(_, s)| s.layer)
            .map(|(n, _)| *n)
            .collect();
        let mut out = match at {
            At::Prop(e) => {
                let Some(site) = self.sites.get(&e) else {
                    return on.keys().copied().collect();
                };
                let t = self.target_surfaces(e);
                if site.absolute && !site.is_surface {
                    // Itself and what is inside it, on its own surface (a
                    // popup inside it moves, and with it what it holds,
                    // below).
                    on.iter()
                        .filter(|(b, s)| {
                            meet(s.as_ref(), t.as_ref()) && self.inside(**b, e, &mut Vec::new())
                        })
                        .map(|(n, _)| *n)
                        .collect()
                } else {
                    boxes_on(t.as_ref())
                }
            }
            // A bar's exclusive zone places and sizes every layer surface
            // on its output (which output is not known here: all of them).
            At::Zone(_) => boxes_on(Some(&layers)),
            At::Name(e) => boxes_on(self.target_surfaces(e).as_ref()),
            At::Mount(body, surface, _) => boxes_on(self.around(body, surface).as_ref()),
            At::Everywhere => return on.keys().copied().collect(),
        };
        // What moves a surface's box moves what is on it: inherited values
        // (`set`, `font`, `weight`, a surface's own props) reach into the
        // popups nested in what they set, which render lays out again, and
        // a bar sized by its content changes its exclusive zone.
        loop {
            let moved: BTreeSet<NodeIdx> = out
                .iter()
                .filter(|n| self.sites.get(n).is_some_and(|s| s.is_surface))
                .copied()
                .collect();
            let zone = out
                .iter()
                .any(|n| self.sites.get(n).is_some_and(|s| s.auto_zone));
            let more: Vec<NodeIdx> = on
                .iter()
                .filter(|(b, s)| {
                    !out.contains(b)
                        && match s {
                            Some(s) => !s.is_disjoint(&moved) || (zone && !s.is_disjoint(&layers)),
                            None => !moved.is_empty(),
                        }
                })
                .map(|(n, _)| *n)
                .collect();
            if more.is_empty() {
                return out;
            }
            out.extend(more);
        }
    }

    /// A cycle through use `i`: from one of the boxes it reads, through
    /// `i` to a box it moves and on through other bindings back to the
    /// first. Each step is a box and the use that moved it there (the
    /// first box's use is `i`, unused). The flag: a box read (`n.box`)
    /// lies on it; a cycle of sizes only (`n.width`) is found only when
    /// there is none with a box.
    fn cycle(
        &self,
        i: usize,
        reads: &HashMap<usize, Read>,
        moves: &HashMap<usize, BTreeSet<NodeIdx>>,
        readers: &HashMap<NodeIdx, Vec<usize>>,
    ) -> Option<(Vec<(NodeIdx, usize)>, bool)> {
        let start = moves.get(&i)?;
        let read = reads.get(&i)?;
        // States: a box, and whether a box read lies on the way there
        // (after `i`'s own edge, which is known once the loop closes).
        type State = (NodeIdx, bool);
        let mut from: HashMap<State, Option<(State, usize)>> = HashMap::new();
        let mut queue = VecDeque::new();
        for b in start {
            from.insert((*b, false), None);
            queue.push_back((*b, false));
        }
        let path = |end: State, from: &HashMap<State, Option<(State, usize)>>| {
            let mut steps = Vec::new();
            let mut at = end;
            while let Some(Some((prev, by))) = from.get(&at) {
                steps.push((at.0, *by));
                at = *prev;
            }
            steps.push((at.0, i));
            steps.push((end.0, i));
            steps.reverse();
            steps
        };
        let mut sizes_only = None;
        while let Some((x, boxed)) = queue.pop_front() {
            if read.boxes.contains(&x) || (boxed && read.sizes.contains(&x)) {
                return Some((path((x, boxed), &from), true));
            }
            if read.sizes.contains(&x) && sizes_only.is_none() {
                sizes_only = Some(path((x, boxed), &from));
            }
            for v in readers.get(&x).into_iter().flatten() {
                let via_box = reads.get(v).is_some_and(|r| r.boxes.contains(&x));
                for m in moves.get(v).into_iter().flatten() {
                    let next = (*m, boxed || via_box);
                    if let std::collections::hash_map::Entry::Vacant(e) = from.entry(next) {
                        e.insert(Some(((x, boxed), *v)));
                        queue.push_back(next);
                    }
                }
            }
        }
        sizes_only.map(|p| (p, false))
    }

    /// How another node's size is named in a loop's path.
    fn size_label(&self, n: NodeIdx) -> String {
        match self.sites.get(&n) {
            Some(Site {
                name: Some(name), ..
            }) => format!("`{name}`'s size"),
            Some(s) => format!("a `{}`'s size", s.kind),
            None => "a size".into(),
        }
    }

    /// How a box is named in a loop's path.
    fn box_label(&self, n: NodeIdx) -> String {
        match self.sites.get(&n) {
            Some(Site {
                name: Some(name), ..
            }) => format!("`{name}.box`"),
            Some(s) => format!("a `{}`'s box", s.kind),
            None => "a box".into(),
        }
    }

    /// How a binding is named in a loop's path.
    fn use_label(&self, i: usize) -> String {
        let u = &self.uses[i];
        let of = |e: NodeIdx| match self.sites.get(&e) {
            Some(Site { name: Some(n), .. }) => format!("`{n}`"),
            Some(s) => format!("a `{}`", s.kind),
            None => "an element".into(),
        };
        match u.at {
            At::Prop(e) | At::Zone(e) => format!("`{}` of {}", u.what, of(e)),
            At::Mount(..) if u.what == "if" => "an `if`".into(),
            At::Mount(..) => format!("a `{}`", u.what),
            At::Name(_) => format!("the `when` naming `{}`", u.what),
            At::Everywhere => format!("`{}`", u.what),
        }
    }
}

/// Runs `walk` on `stmts` and every nested block, with the conditions
/// around each.
fn nested<'h>(
    stmts: &'h [Stmt],
    ctx: &mut Vec<&'h Expr>,
    walk: &mut dyn FnMut(&'h [Stmt], &mut Vec<&'h Expr>),
) {
    walk(stmts, ctx);
    for s in stmts {
        match &s.kind {
            StmtKind::If { cond, then, else_ } => {
                ctx.push(cond);
                nested(then, ctx, walk);
                nested(else_, ctx, walk);
                ctx.pop();
            }
            StmtKind::For { iter, body, .. } => {
                ctx.push(iter);
                nested(body, ctx, walk);
                ctx.pop();
            }
            StmtKind::Match { scrutinee, arms } => {
                ctx.push(scrutinee);
                for (_, arm) in arms {
                    nested(arm, ctx, walk);
                }
                ctx.pop();
            }
            _ => {}
        }
    }
}

/// The alias index of local `l`, if it is a conditional id.
fn alias(c: &Checker<'_>, l: LocalId) -> Option<NodeIdx> {
    match c.locals[l.0 as usize].kind {
        LocalKind::NodeAlias(a) => Some(a),
        _ => None,
    }
}
