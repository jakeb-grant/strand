//! Handler and fn bodies: statements, assignment, handlers and timers.

use super::Checker;
use crate::hir::{self, Event, LocalKind, StmtKind, Target};
use crate::syntax::Span;
use crate::syntax::ast::{self, AssignOp};
use crate::ty::{ParamSig, Ty};

impl<'a> Checker<'a> {
    pub(crate) fn stmts(&mut self, items: &'a [ast::Stmt]) -> Vec<hir::Stmt> {
        self.push_scope();
        let out = items.iter().map(|s| self.stmt(s)).collect();
        self.pop_scope();
        out
    }

    /// A fn body: like [`Checker::stmts`], but the last expression, the
    /// fn's value, is checked against the declared return type (so a `1`
    /// there is an `int` when the fn returns one).
    pub(crate) fn fn_body(&mut self, items: &'a [ast::Stmt], ret: Option<&Ty>) -> Vec<hir::Stmt> {
        self.push_scope();
        let mut out: Vec<hir::Stmt> = Vec::with_capacity(items.len());
        for (i, s) in items.iter().enumerate() {
            let last = i + 1 == items.len();
            out.push(match (&s.kind, ret) {
                (ast::StmtKind::Expr(e), Some(r)) if last => hir::Stmt {
                    kind: StmtKind::Expr(self.expr(e, Some(r))),
                    span: s.span,
                },
                _ => self.stmt(s),
            });
        }
        self.pop_scope();
        out
    }

    #[inline(never)]
    fn stmt(&mut self, s: &'a ast::Stmt) -> hir::Stmt {
        let span = s.span;
        let kind = match &s.kind {
            ast::StmtKind::Let(l) => self.stmt_let(l),
            ast::StmtKind::Assign { target, op, value } => self.assign(target, *op, value),
            ast::StmtKind::Expr(e) => StmtKind::Expr(self.expr(e, None)),
            ast::StmtKind::If(i) => self.stmt_if(i),
            ast::StmtKind::For(f) => self.stmt_for(f),
            ast::StmtKind::Match(m) => self.stmt_match(m),
            ast::StmtKind::Play(e) => StmtKind::Play(self.play_target(e)),
            ast::StmtKind::Error => StmtKind::Error,
        };
        hir::Stmt { kind, span }
    }

    #[inline(never)]
    fn stmt_let(&mut self, l: &'a ast::Let) -> StmtKind {
        {
            {
                let declared = l.ty.as_ref().map(|t| self.resolve_type(t));
                let value = match &declared {
                    Some(t) => self.expect(&l.value, t, &format!("`{}`", l.name.name)),
                    None => self.expr(&l.value, None),
                };
                if l.export.is_some() {
                    self.error(
                        "check::misplaced",
                        "only top-level `state` and `let` can be exported",
                        l.name.span,
                        "a local",
                    );
                }
                let untyped = declared.is_none();
                let ty = declared.unwrap_or_else(|| value.ty.clone());
                let local = self.bind_local(&l.name.name, ty, l.name.span, LocalKind::Let);
                if untyped {
                    // `let t = a; b = t` hands `a` to `b` (see `check`).
                    let found = self.whole_sources(&value);
                    if !found.is_empty() {
                        self.local_sources.insert(local, found);
                    }
                }
                self.add_ref(l.name.span, Target::Local(local));
                StmtKind::Let { local, value }
            }
        }
    }

    #[inline(never)]
    fn stmt_for(&mut self, f: &'a ast::For<ast::Stmt>) -> StmtKind {
        {
            {
                let iter = self.expr(&f.iter, None);
                let elem = self.iter_elem(&iter, &f.iter);
                self.push_scope();
                let binding =
                    self.bind_local(&f.binding.name, elem, f.binding.span, LocalKind::ForBinding);
                if let Some(k) = &f.key {
                    self.expr(k, None);
                }
                let body = self.stmts(&f.body.items);
                self.pop_scope();
                StmtKind::For {
                    binding,
                    iter,
                    body,
                }
            }
        }
    }

    #[inline(never)]
    fn stmt_match(&mut self, m: &'a ast::Match<ast::ArmBody<ast::Stmt>>) -> StmtKind {
        {
            {
                let scrutinee = self.expr(&m.scrutinee, None);
                let mut arms = Vec::new();
                let mut pats = Vec::new();
                for arm in &m.arms {
                    let p = self.pattern(&arm.pattern, &scrutinee.ty);
                    pats.push((p.clone(), arm.pattern.span));
                    let body = match &arm.body {
                        ast::ArmBody::Block(b) => self.stmts(&b.items),
                        ast::ArmBody::Single(s) => vec![self.stmt(s)],
                    };
                    arms.push((p, body));
                }
                self.exhaustive(&scrutinee.ty, &pats, m.arms_span);
                StmtKind::Match { scrutinee, arms }
            }
        }
    }

    #[inline(never)]
    fn stmt_if(&mut self, i: &'a ast::If<ast::Stmt>) -> StmtKind {
        let cond = self.expect(&i.cond, &Ty::BOOL, "the condition");
        let then = self.stmts(&i.then.items);
        let else_ = match &i.else_ {
            None => Vec::new(),
            Some(ast::Else::Block(b)) => self.stmts(&b.items),
            Some(ast::Else::If(inner, span)) => vec![hir::Stmt {
                kind: self.stmt_if(inner),
                span: *span,
            }],
        };
        StmtKind::If { cond, then, else_ }
    }

    fn assign(&mut self, target: &'a ast::Expr, op: AssignOp, value: &'a ast::Expr) -> StmtKind {
        if let Some(prop) = self.prop_assignment(target) {
            // QML's `width = 30`: a prop of the element, not a name.
            self.not_writable(
                super::expr::NotWritable::BoundProp(prop),
                target.span,
                "assign",
            );
            let value = self.expr(value, None);
            return StmtKind::Assign {
                target: hir::Expr::error(target.span),
                op,
                value,
            };
        }
        let t = self.expr(target, None);
        if self.ctx.pure_fn {
            self.error(
                "check::impure",
                "fns are pure: they cannot assign",
                target.span,
                "an assignment",
            )
            .help = Some("return the new value instead, and assign it in a handler".into());
        } else if let Err(why) = self.writable(&t) {
            self.not_writable(why, target.span, "assign");
        }
        let v = match op {
            AssignOp::Set => {
                let v = self.expr(value, Some(&t.ty));
                self.record_flow(&t, &v);
                if !self.widen(&t, &v.ty) {
                    self.require(&v, &t.ty, "this assignment");
                }
                v
            }
            _ => {
                let v = self.expr(value, Some(&t.ty));
                self.record_flow(&t, &v);
                // `i += 0.5`, `i *= 0.5`, `i /= 2` make a whole-number
                // state fractional.
                let fraction = match op {
                    AssignOp::Div => Ty::FLOAT,
                    _ => v.ty.clone(),
                };
                let widened = self.widen(&t, &fraction);
                let ok = match (t.ty.prim(), v.ty.prim()) {
                    _ if widened || t.ty.is_lenient() || v.ty.is_lenient() => true,
                    (Some(a), Some(b)) if a.is_numeric() && b.is_numeric() => {
                        matches!(op, AssignOp::Mul | AssignOp::Div) && b.is_scalar()
                            || self.types.assignable(&v.ty, &t.ty)
                    }
                    _ => false,
                };
                if !ok {
                    let (a, b) = (self.show(&t.ty), self.show(&v.ty));
                    self.error(
                        "check::type_mismatch",
                        format!("cannot apply `{}` to `{a}` and `{b}`", op.as_str()),
                        v.span,
                        format!("this is `{b}`"),
                    );
                }
                v
            }
        };
        StmtKind::Assign {
            target: t,
            op,
            value: v,
        }
    }

    /// The prop an assignment target names when it is a prop of the
    /// enclosing element rather than a value: a bare `width` that resolves
    /// to nothing, or `self.opacity`.
    fn prop_assignment(&self, target: &ast::Expr) -> Option<String> {
        let name = match &target.kind {
            ast::ExprKind::Name(id) => {
                let n = id.name.as_str();
                let bound = self.lookup_scope(n).is_some()
                    || self.file_scopes[self.module].contains_key(n)
                    || self.globals.contains_key(n)
                    || self.schema.services.contains_key(n)
                    || self.schema.values.contains_key(n)
                    || super::NODE_BOOLS.contains(&n)
                    || n == "self";
                (!bound).then_some(n)
            }
            ast::ExprKind::Field { base, name, .. } => match &base.kind {
                ast::ExprKind::Name(b)
                    if b.name == "self" && self.lookup_scope("self").is_none() =>
                {
                    let node_field = self
                        .types
                        .find_record("Node")
                        .is_some_and(|r| self.types.record(r).field(&name.name).is_some());
                    (!node_field).then_some(name.name.as_str())
                }
                _ => None,
            },
            _ => None,
        }?;
        let schema = self.nodes.last()?.schema?;
        schema.prop(name).map(|_| name.to_string())
    }

    /// `play shake`: a keyframes name.
    pub(crate) fn play_target(&mut self, e: &'a ast::Expr) -> hir::Expr {
        let keyframes = Ty::opaque("Keyframes");
        let h = self.expr(e, Some(&keyframes));
        if !h.ty.is_error() && h.ty != keyframes {
            let shown = self.show(&h.ty);
            self.error(
                "check::type_mismatch",
                "`play` takes the name of a `keyframes`",
                e.span,
                format!("this is `{shown}`"),
            );
        }
        h
    }

    /// The item type of what a `for` walks.
    pub(crate) fn iter_elem(&mut self, iter: &hir::Expr, ast: &ast::Expr) -> Ty {
        match &iter.ty {
            t if t.is_lenient() => Ty::Error,
            Ty::Optional(_) => {
                self.nullable_use(iter, ast, "operand");
                Ty::Error
            }
            t => match t.list_elem() {
                Some((e, _)) => e.clone(),
                None => {
                    let shown = self.show(t);
                    self.error(
                        "check::type_mismatch",
                        format!("`for` walks a list, not `{shown}`"),
                        iter.span,
                        format!("this is `{shown}`"),
                    );
                    Ty::Error
                }
            },
        }
    }

    // -----------------------------------------------------------------------
    // Handlers and timers

    /// `on …` in a tree or at the top level. Element events need an
    /// enclosing element.
    pub(crate) fn handler(&mut self, o: &'a ast::On, span: Span) -> Option<hir::Handler> {
        // Always `Some`; an unknown event is reported and its body still checked.
        let saved = self.ctx;
        let (event, params): (Event, Vec<hir::LocalId>) = match &o.event {
            ast::Event::Change { targets, debounce } => {
                let mut ts = Vec::new();
                for t in targets {
                    let h = self.expr(t, None);
                    if matches!(h.ty, Ty::Fn(_)) {
                        self.error(
                            "check::not_reactive",
                            "`on change` watches values, not functions",
                            t.span,
                            "a function",
                        );
                    } else if !self.is_reactive(&h) {
                        let label = match &h.kind {
                            hir::ExprKind::Def(d) => {
                                format!("`{}` never changes", self.defs[d.0 as usize].name)
                            }
                            _ => "constant".to_string(),
                        };
                        self.error(
                            "check::not_reactive",
                            "`on change` watches something that never changes",
                            t.span,
                            label,
                        )
                        .help = Some(
                            "watch a `state`, a `let` that reads one, or a service field, such as `audio.sink.volume`"
                                .into(),
                        );
                    }
                    ts.push(h);
                }
                let d = debounce
                    .as_ref()
                    .map(|d| self.expect(d, &Ty::DURATION, "`after`"));
                self.push_scope();
                (
                    Event::Change {
                        targets: ts,
                        debounce: d,
                    },
                    Vec::new(),
                )
            }
            ast::Event::Named { path, params } => {
                let (event, sig_params, known) = match self.event_params(path, span) {
                    Some((e, p)) => (e, p, true),
                    None => (
                        Event::Element(path.last().map_or(String::new(), |p| p.name.clone())),
                        Vec::new(),
                        false,
                    ),
                };
                self.push_scope();
                let mut locals = Vec::new();
                let given = params.as_deref().unwrap_or(&[]);
                if !known {
                    for p in given {
                        locals.push(self.bind_local(
                            &p.name.name,
                            Ty::Error,
                            p.name.span,
                            LocalKind::EventParam,
                        ));
                    }
                } else if given.len() > sig_params.len() {
                    let name = path.last().map_or("", |p| p.name.as_str());
                    self.error(
                        "check::too_many_args",
                        format!(
                            "`{name}` gives {} value{}",
                            sig_params.len(),
                            if sig_params.len() == 1 { "" } else { "s" }
                        ),
                        given[sig_params.len()].span,
                        "nothing to receive",
                    )
                    .help = (!sig_params.is_empty()).then(|| {
                        format!(
                            "it gives {}",
                            sig_params
                                .iter()
                                .map(|p| format!("`{}`", p.name))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    });
                }
                for (p, sp) in given.iter().zip(&sig_params) {
                    let ty = match &p.ty {
                        Some(t) => {
                            let t = self.resolve_type(t);
                            if !self.types.assignable(&sp.ty, &t) {
                                let (a, b) = (self.show(&sp.ty), self.show(&t));
                                self.error(
                                    "check::type_mismatch",
                                    format!("this event gives `{a}`, not `{b}`"),
                                    p.span,
                                    format!("declared `{b}`"),
                                );
                            }
                            t
                        }
                        None => sp.ty.clone(),
                    };
                    if let Some(d) = &p.default {
                        self.error(
                            "check::misplaced",
                            "event parameters take no defaults",
                            d.span,
                            "the event gives every value",
                        );
                    }
                    locals.push(self.bind_local(
                        &p.name.name,
                        ty,
                        p.name.span,
                        LocalKind::EventParam,
                    ));
                }
                (event, locals)
            }
        };
        self.ctx.handler = true;
        self.ctx.prop = false;
        let body = self.stmts(&o.body.items);
        self.pop_scope();
        self.ctx = saved;
        Some(hir::Handler {
            event,
            params,
            body,
            span,
        })
    }

    /// The event `path` names and the values it gives.
    fn event_params(&mut self, path: &[ast::Ident], span: Span) -> Option<(Event, Vec<ParamSig>)> {
        match path {
            [one] => {
                let name = one.name.as_str();
                let Some(node) = self.nodes.last() else {
                    self.error(
                        "check::no_node",
                        format!("`on {name}` belongs to an element, and there is none here"),
                        one.span,
                        "not inside an element",
                    )
                    .help = Some(format!(
                        "put it inside the element it is about: `box {{ on {name} {{ … }} }}`; service events are `on service.event`"
                    ));
                    return None;
                };
                let kind = node.kind.clone();
                let Some(schema) = node.schema else {
                    // A component call or an unknown element: reported.
                    return Some((Event::Element(name.to_string()), Vec::new()));
                };
                match schema.event(name) {
                    Some(ev) => {
                        self.add_ref(one.span, Target::Element(kind));
                        Some((Event::Element(name.to_string()), ev.params.clone()))
                    }
                    None => {
                        let candidates: Vec<String> =
                            schema.events.iter().map(|e| e.name.clone()).collect();
                        let help = Self::did_you_mean(name, &candidates).or_else(|| {
                            Some(format!(
                                "`{kind}` has {}",
                                candidates
                                    .iter()
                                    .map(|c| format!("`{c}`"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        });
                        self.error(
                            "check::unknown_event",
                            format!("`{kind}` has no event `{name}`"),
                            one.span,
                            "unknown event",
                        )
                        .help = help;
                        None
                    }
                }
            }
            [svc, ev] => {
                let rec = self.schema.service(&svc.name).or_else(|| {
                    self.globals
                        .get(&svc.name)
                        .and_then(|d| match self.defs[d.0 as usize].kind {
                            hir::DefKind::Service(r) => Some(r),
                            _ => None,
                        })
                });
                let Some(r) = rec else {
                    let candidates: Vec<String> = self.schema.services.keys().cloned().collect();
                    let help = Self::did_you_mean(&svc.name, &candidates);
                    self.error(
                        "check::unknown_name",
                        format!("unknown service `{}`", svc.name),
                        svc.span,
                        "not a service",
                    )
                    .help = help;
                    return None;
                };
                self.add_ref(svc.span, Target::Service(svc.name.clone()));
                let record = self.types.record(r);
                match record.event(&ev.name) {
                    Some(e) => Some((
                        Event::Service {
                            service: svc.name.clone(),
                            event: ev.name.clone(),
                        },
                        e.params.clone(),
                    )),
                    None => {
                        let candidates: Vec<String> =
                            record.events.iter().map(|e| e.name.clone()).collect();
                        let help = Self::did_you_mean(&ev.name, &candidates);
                        self.error(
                            "check::unknown_event",
                            format!("`{}` has no event `{}`", svc.name, ev.name),
                            ev.span,
                            "unknown event",
                        )
                        .help = help;
                        None
                    }
                }
            }
            _ => {
                self.error(
                    "check::unknown_event",
                    "an event is `name` or `service.event`",
                    span,
                    "not an event",
                );
                None
            }
        }
    }

    pub(crate) fn timer(&mut self, t: &'a ast::Timer, span: Span) -> hir::Timer {
        let saved = self.ctx;
        self.ctx.prop = false;
        let duration = self.expect(&t.duration, &Ty::DURATION, "the timer");
        let while_ = t
            .while_
            .as_ref()
            .map(|w| self.expect(w, &Ty::BOOL, "`while`"));
        self.ctx.handler = true;
        let body = self.stmts(&t.body.items);
        self.ctx = saved;
        hir::Timer {
            kind: t.kind,
            duration,
            while_,
            body,
            span,
        }
    }
}
