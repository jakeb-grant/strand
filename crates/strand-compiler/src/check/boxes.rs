//! (M6) `id.box` may drive only what cannot move it back (spec.md 6.3,
//! decisions.md m6-w2-surface, the owner's ruling of 2026-10-14).
//!
//! An element's laid-out box reaches logic a frame after layout put it
//! there. A binding on it that changed layout around it again would chase
//! its own tail a frame at a time (a width that follows the box of the
//! row it widens), so the checker allows a value that depends on a `.box`
//! only where laying it out cannot move that box:
//!
//! - paint props, which layout never reads (`x`, `y`, `opacity`,
//!   colours, transforms, `open`), on any element but a surface;
//! - any prop of an element on another surface than the box (a popup's
//!   island sized from the bar item it points at): one surface's layout
//!   never moves another's content. A popup's own box is where it sits on
//!   the surface it opens from, so it counts as on both;
//! - the layout props of an element with a constant `place: absolute`
//!   that the box is not inside (and is not): an absolute element is laid
//!   out after its parent and moves nothing else.
//!
//! An `if`, `for` or `match` that depends on a `.box` of its own surface
//! mounts and unmounts elements there, so it is refused too. The
//! dependence is followed through `let`s, `state`s (what handlers assign
//! them, and under which conditions), `fn`s, component parameters (from
//! each call) and `for` bindings. A `.box` the checker cannot place (a
//! node passed as a value) counts as on every surface and inside
//! everything: it may drive only paint props.

use std::collections::{BTreeSet, HashMap};

use super::Checker;
use crate::hir::{
    self, Callee, DefId, ElementKind, Event, Expr, ExprKind, LocalId, LocalKind, Node, NodeIdx,
    Stmt, StmtKind,
};
use crate::syntax::Span;
use crate::ty::Ty;
use strand_scene::{NodeKind, Prop as SceneProp};

/// Whose box a value depends on.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Src {
    Node(NodeIdx),
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
    absolute: bool,
    /// Its subtree holds a `slot` or a component call: what else is
    /// inside it is not known here.
    opaque: bool,
    name: Option<String>,
}

/// A binding to check once the dependences are known.
struct Use<'h> {
    module: usize,
    /// The element it is a prop of (`None`: an `if`, `for` or `match`).
    element: Option<NodeIdx>,
    /// Where the structure sits (for an `if`, `for` or `match`).
    at: Option<(Body, Option<NodeIdx>)>,
    what: String,
    span: Span,
    exprs: Vec<&'h Expr>,
}

#[derive(Default)]
struct Flow<'h> {
    sites: HashMap<NodeIdx, Site>,
    /// A conditional id's alias index → the elements it may name.
    members: HashMap<NodeIdx, Vec<NodeIdx>>,
    /// Component → its call elements.
    calls: HashMap<DefId, Vec<NodeIdx>>,
    locals: HashMap<LocalId, Taint>,
    defs: HashMap<DefId, Taint>,
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
    /// Reports each binding that would move the `.box` it depends on
    /// (`check::box_layout_loop`).
    pub(super) fn box_loops(&mut self, files: &[hir::FileHir]) {
        let mut flow = Flow {
            node_rec: self.types.find_record("Node"),
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
        let saved = self.module;
        for u in &flow.uses {
            let mut taint = Taint::new();
            for e in &u.exprs {
                taint.extend(flow.taint(self, e));
            }
            if taint.is_empty() {
                continue;
            }
            let bad = taint.iter().find(|r| match (u.element, u.at) {
                (Some(e), _) => !flow.may_drive(**r, e),
                (None, Some((body, surface))) => {
                    !flow.disjoint(flow.reader_surfaces(**r), flow.around(body, surface))
                }
                _ => false,
            });
            let Some(bad) = bad else { continue };
            let name = match bad {
                Src::Node(n) => flow.sites.get(n).and_then(|s| s.name.clone()),
                Src::Unknown => None,
            };
            let what_box = name.map_or_else(
                || "an element's `.box`".to_string(),
                |n| format!("`{n}.box`"),
            );
            self.module = u.module;
            let (message, label, help) = if u.element.is_some() {
                (
                    format!(
                        "`{}` cannot follow {what_box}: laying it out would move that box again",
                        u.what
                    ),
                    "a layout prop on that box",
                    "`.box` is where layout put an element, a frame late: it may drive paint props (`x`, `y`, `opacity`, colours, transforms) on any element, or the size of a `place: absolute` element it is not inside, or anything on another surface",
                )
            } else {
                (
                    format!(
                        "this `{}` cannot depend on {what_box}: what it mounts would move that box again",
                        u.what
                    ),
                    "depends on a box on its own surface",
                    "choose with something layout does not change (a state, a service value), and move or fade what follows the box with paint props (`x`, `opacity`)",
                )
            };
            self.error("check::box_layout_loop", message, u.span, label)
                .help = Some(help.into());
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

    fn opaque_above(&self, flow: &mut Flow<'_>) {
        for a in &self.ancestors {
            if let Some(s) = flow.sites.get_mut(a) {
                s.opaque = true;
            }
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
            element: None,
            at: Some((self.body, self.surfaces.last().copied())),
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
            Node::Slot(_) => self.opaque_above(flow),
            Node::Handler(h) => handler(h, Vec::new(), flow),
            Node::Timer(t) => timer(t, Vec::new(), flow),
            Node::State(s) => {
                state_values(s, flow);
            }
            Node::Let(l) => {
                flow.def_values.push((l.def, &l.value));
            }
            _ => {}
        }
    }

    fn element<'h>(&mut self, e: &'h hir::Element, flow: &mut Flow<'h>) {
        let kind = match &e.kind {
            ElementKind::Builtin(k) => NodeKind::from_name(k),
            _ => None,
        };
        let is_surface = kind.is_some_and(NodeKind::is_surface);
        let types = &self.checker.types;
        let absolute = e.props.iter().any(|p| {
            p.name == "place"
                && matches!(p.value.kind, ExprKind::Variant(en, v)
                    if types.enum_(en).variants.get(v as usize).is_some_and(|n| n == "absolute"))
        });
        let name = e.id.map(|l| self.checker.locals[l.0 as usize].name.clone());
        flow.sites.insert(
            e.node,
            Site {
                body: self.body,
                ancestors: self.ancestors.clone(),
                surface: self.surfaces.last().copied(),
                is_surface,
                absolute,
                opaque: false,
                name,
            },
        );
        let schema = match &e.kind {
            ElementKind::Builtin(k) => self.checker.schema.element(k),
            _ => None,
        };
        // Its props, and its `when`s' under their conditions.
        let mut props: Vec<(&'h hir::Prop, Option<&'h Expr>)> =
            e.props.iter().map(|p| (p, None)).collect();
        for c in &e.children {
            if let Node::When(w) = c {
                props.extend(w.props.iter().map(|p| (p, Some(&w.cond))));
                // A conditional id names this element.
                if let Some(p) = w.props.iter().find(|p| p.name == "id")
                    && let ExprKind::Local(l) = p.value.kind
                    && let LocalKind::NodeAlias(a) = self.checker.locals[l.0 as usize].kind
                {
                    flow.members.entry(a).or_default().push(e.node);
                    let alias = self.checker.locals[l.0 as usize].name.clone();
                    if let Some(s) = flow.sites.get_mut(&e.node) {
                        s.name.get_or_insert(alias);
                    }
                }
            }
        }
        match &e.kind {
            ElementKind::Component(d) => {
                self.opaque_above(flow);
                if let Some(s) = flow.sites.get_mut(&e.node) {
                    s.opaque = true;
                }
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
            ElementKind::Builtin(_) => {
                let mut checked = Vec::new();
                if let (Some(a), Some(s)) = (&e.arg, schema)
                    && let Some(name) = &s.arg_prop
                {
                    checked.push((name.clone(), a.span, vec![a]));
                }
                for (p, cond) in &props {
                    if p.name == "id" {
                        continue;
                    }
                    let mut exprs = vec![&p.value];
                    exprs.extend(*cond);
                    checked.push((p.name.clone(), p.span, exprs.clone()));
                    for s in &p.sub {
                        let mut exprs = vec![&s.value];
                        exprs.extend(*cond);
                        checked.push((s.name.clone(), s.span, exprs));
                    }
                }
                for (name, span, exprs) in checked {
                    if !moves_layout(&name, is_surface) {
                        continue;
                    }
                    flow.uses.push(Use {
                        module: self.module,
                        element: Some(e.node),
                        at: None,
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

/// Whether `prop`, set on an element, can move a box: its layout props,
/// and on a surface everything but `open` (where a popup sits follows its
/// size, offsets and anchor).
fn moves_layout(prop: &str, surface: bool) -> bool {
    if surface {
        return prop != "open";
    }
    SceneProp::from_name(prop).is_some_and(SceneProp::affects_layout)
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

impl Flow<'_> {
    /// Whether anything collected reads a `.box` at all.
    fn reads_any_box(&self) -> bool {
        let mut found = false;
        let mut look = |e: &Expr| {
            each(e, &mut |x| {
                if let ExprKind::Field { name, .. } = &x.kind
                    && name == "box"
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
            ExprKind::Local(l) => {
                if let Some(t) = self.locals.get(l) {
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
            ExprKind::Field { base, name, .. } if name == "box" => match &base.kind {
                ExprKind::Node(n) => {
                    out.insert(Src::Node(*n));
                }
                ExprKind::Local(l) if alias(c, *l).is_some() => {
                    match alias(c, *l).and_then(|a| self.members.get(&a)) {
                        Some(m) => out.extend(m.iter().map(|n| Src::Node(*n))),
                        None => {
                            out.insert(Src::Unknown);
                        }
                    }
                }
                _ if matches!(&base.ty, Ty::Record(r) if Some(*r) == self.node_rec) => {
                    out.insert(Src::Unknown);
                }
                _ => {}
            },
            _ => {}
        });
        out
    }

    /// Follows dependences through `let`s, `state`s, `fn`s, parameters
    /// and bindings until nothing changes.
    fn settle(&mut self, c: &Checker<'_>) {
        // Each round adds at least one source somewhere or stops; the
        // bound only guards against a mistake here.
        for _ in 0..64 {
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
            for (stmts, trig) in &self.bodies {
                let mut base = Taint::new();
                for t in trig {
                    base.extend(self.taint(c, t));
                }
                self.stmt_flows(c, stmts, &base, &mut def_adds, &mut local_adds);
            }
            for f in &self.fns {
                // A `fn`'s value depends on everything its body reads.
                let mut all = Taint::new();
                each_stmt(&f.body, &mut Vec::new(), &mut |x, _| {
                    all.extend(self.taint(c, x));
                });
                def_adds.push((f.def, all));
                self.stmt_flows(c, &f.body, &Taint::new(), &mut def_adds, &mut local_adds);
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
                        if let Some(d) = target_def(target) {
                            under.extend(self.taint(c, value));
                            defs.push((d, under));
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

    /// The surfaces whose layout can move `r`'s box (`None`: not known).
    fn reader_surfaces(&self, r: Src) -> Option<BTreeSet<NodeIdx>> {
        let Src::Node(n) = r else { return None };
        let s = self.sites.get(&n)?;
        let mut out = self.around(s.body, s.surface)?;
        if s.is_surface {
            out.insert(n);
        }
        Some(out)
    }

    /// The surfaces whose layout an element `e`'s props lay out.
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

    fn disjoint(&self, a: Option<BTreeSet<NodeIdx>>, b: Option<BTreeSet<NodeIdx>>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => a.is_disjoint(&b),
            _ => false,
        }
    }

    /// Whether `r`'s box may drive element `e`'s layout props.
    fn may_drive(&self, r: Src, e: NodeIdx) -> bool {
        if self.disjoint(self.reader_surfaces(r), self.target_surfaces(e)) {
            return true;
        }
        let (Src::Node(r), Some(site)) = (r, self.sites.get(&e)) else {
            return false;
        };
        if !site.absolute || site.is_surface || r == e {
            return false;
        }
        match self.sites.get(&r) {
            Some(rs) if rs.body == site.body => !rs.ancestors.contains(&e),
            Some(_) => !site.opaque,
            None => false,
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
