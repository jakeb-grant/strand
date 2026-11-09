//! Handler and fn bodies: statements, assignment, handlers and timers.

use super::Checker;
use crate::hir::{self, Event, LocalKind, StmtKind, Target};
use crate::syntax::Span;
use crate::syntax::ast::{self, AssignOp};
use crate::ty::{ParamSig, Prim, Ty};
use strand_scene::{KEY_MODIFIERS, KeyPattern, KeyPatternError};

impl<'a> Checker<'a> {
    pub(crate) fn stmts(&mut self, items: &'a [ast::Stmt]) -> Vec<hir::Stmt> {
        self.push_scope();
        self.action_calls.push(Vec::new());
        let out = items.iter().map(|s| self.stmt(s)).collect();
        self.action_calls.pop();
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
            out.push(match &s.kind {
                // The fn's value.
                ast::StmtKind::Expr(e) if last => hir::Stmt {
                    kind: StmtKind::Expr(
                        self.named_value("a `fn`'s result", ret, |c| c.expr(e, ret)),
                    ),
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
            ast::StmtKind::Expr(e) => {
                let outer = self.stmt_expr.replace(e.span);
                let h = self.expr(e, None);
                self.stmt_expr = outer;
                if self.ctx.handler && matches!(h.ty, Ty::Async(_)) {
                    // `on click { sleep(1s) }` almost always meant to wait.
                    let shown = self.show(&h.ty);
                    self.error(
                        "check::async",
                        format!("this `{shown}` is dropped"),
                        h.span,
                        "never awaited or kept",
                    )
                    .help = Some("`await` it, or assign the result".into());
                }
                StmtKind::Expr(h)
            }
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
                    Some(t) => {
                        let h = self.let_value(&l.value, Some(t));
                        self.require(&h, t, &format!("`{}`", l.name.name));
                        h
                    }
                    // A whole-number literal is an `int`, as for a
                    // top-level `let` (a local is never written, so no
                    // fraction can widen it later).
                    None => {
                        let hint = super::whole_shape(&l.value);
                        self.let_value(&l.value, hint.as_ref())
                    }
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
                if let Some(prev) = self
                    .scopes
                    .last()
                    .and_then(|s| s.iter().find(|(n, _)| *n == l.name.name).map(|(_, b)| *b))
                {
                    self.redeclared_binding(&l.name, prev);
                }
                self.shadows_builtin(&l.name, "`let`");
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
                // A literal list of whole numbers is `[int]`, as an untyped
                // `let`'s is (decisions.md m6-w2-language, Gaps 12).
                let hint = super::whole_shape(&f.iter);
                let iter = self.expr(&f.iter, hint.as_ref());
                let elem = self.iter_elem(&iter, &f.iter);
                self.push_scope();
                let binding =
                    self.bind_local(&f.binding.name, elem, f.binding.span, LocalKind::ForBinding);
                if let Some(k) = &f.key {
                    self.expr(k, None);
                }
                let in_loop = self.ctx.in_loop;
                self.ctx.in_loop = self.ctx.handler;
                let body = self.stmts(&f.body.items);
                self.ctx.in_loop = in_loop;
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
        let mark = self.diags.len();
        let place = std::mem::replace(&mut self.ctx.place, true);
        let t = self.expr(target, None);
        self.ctx.place = place;
        if self.ctx.pure_fn {
            self.error(
                "check::impure",
                "fns are pure: they cannot assign",
                target.span,
                "an assignment",
            )
            .help = Some("return the new value instead, and assign it in a handler".into());
        } else if let Err(why) = self.writable(&t) {
            // One mistake, one diagnostic: `first.a = 3` through a
            // nullable `let first` is a write to a `let`; that the path
            // may be null no longer matters.
            let mut i = mark;
            while i < self.diags.len() {
                if self.diags[i].code == "check::nullable" {
                    self.diags.remove(i);
                } else {
                    i += 1;
                }
            }
            self.not_writable(why, target.span, "assign");
        }
        let entry = self.dict_entry(&t, target);
        let v = match op {
            AssignOp::Set => {
                let v = self.expr(value, Some(&t.ty));
                self.record_flow(&t, &v);
                // `xs = [0.5]`: the items' own type, not the target's.
                let written = self.written_ty(&v);
                if !self.widen(&t, &written)
                    && !self.require(&v, &t.ty, "this assignment")
                    && let Some((m, k)) = &entry
                {
                    if matches!(v.ty, Ty::Null) {
                        // `m[k] = null`: a dictionary's values are never
                        // null, and removing is its own word. A `V?` value
                        // (`m[k] = windows.focused?.id`) keeps the
                        // nullable help: it needs a fallback.
                        if let Some(d) = self.diags.last_mut() {
                            d.help = Some(format!(
                                "a dictionary's values are never null; to remove the entry, `{m}.remove({k})`"
                            ));
                        }
                    } else {
                        self.dict_widen_help(&t, &v);
                    }
                }
                v
            }
            _ if entry.is_some() => {
                let v = self.expr(value, None);
                let (m, k) = entry.unwrap_or_default();
                self.dict_compound(&t, op, target, value, &m, &k);
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
                    (Some(Prim::Text), Some(Prim::Text)) => op == AssignOp::Add,
                    // (flex) `xs += [x]` joins lists, as `+` does.
                    _ if matches!((&t.ty, &v.ty), (Ty::List(..), Ty::List(..))) => {
                        op == AssignOp::Add && self.types.assignable(&v.ty, &t.ty)
                    }
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

    /// `check::dict_compound`: `m[k] += v` may find no entry. The help
    /// and fix write the entry whole, starting a missing key from the
    /// value type's zero (`""` for text, `[]` for a list, `0s` for a
    /// duration, 1 for `*=` and `/=` on a number), with what was written
    /// on the right.
    fn dict_compound(
        &mut self,
        t: &hir::Expr,
        op: AssignOp,
        target: &ast::Expr,
        value: &'a ast::Expr,
        m: &str,
        k: &str,
    ) {
        let op_text = op.as_str();
        let bin = &op_text[..1];
        let start = match t.ty.non_null() {
            Ty::List(..) => "[]",
            Ty::Prim(Prim::Text | Prim::Path) => "\"\"",
            Ty::Prim(Prim::Duration) => "0s",
            Ty::Prim(Prim::Length) => "0px",
            Ty::Prim(Prim::Percent) => "0%",
            Ty::Prim(Prim::Angle) => "0deg",
            _ if bin == "*" || bin == "/" => "1",
            _ => "0",
        };
        let entry = self
            .text_of(target.span)
            .map_or_else(|| format!("{m}[{k}]"), str::to_string);
        // The right side as written, in parentheses unless it is one term.
        let rhs = self.text_of(value.span).map(|r| {
            if matches!(
                value.kind,
                ast::ExprKind::Binary { .. }
                    | ast::ExprKind::Ternary { .. }
                    | ast::ExprKind::Lambda { .. }
                    | ast::ExprKind::Match(_)
                    | ast::ExprKind::Commas(_)
                    | ast::ExprKind::Spaced(_)
            ) {
                format!("({r})")
            } else {
                r.to_string()
            }
        });
        let whole = format!(
            "{entry} = ({entry} ?? {start}) {bin} {}",
            rhs.as_deref().unwrap_or("…")
        );
        let d = self.error(
            "check::dict_compound",
            format!("`{op_text}` cannot change a dictionary entry, which may be missing"),
            t.span,
            "an entry of a dictionary",
        );
        d.help = Some(format!(
            "write the entry whole, with a value for a missing key: `{whole}`"
        ));
        if rhs.is_some() {
            d.add_suggestion(Span::new(target.span.start, value.span.end), whole);
        }
    }

    /// (dictionary) `m["b"] = 0.5` into an untyped `state m = ["a": 1]`:
    /// a dictionary keeps its literal's value type (it does not widen as
    /// a whole-number list does), so the help and fix declare it with
    /// `float` values rather than round the number written.
    fn dict_widen_help(&mut self, t: &hir::Expr, v: &hir::Expr) {
        let hir::ExprKind::Index { base, .. } = &t.kind else {
            return;
        };
        let hir::ExprKind::Def(d) = base.kind else {
            return;
        };
        let Ty::Dict(key, _) = &base.ty else {
            return;
        };
        if !self.untyped_dicts.contains(&d) || t.ty != Ty::INT || v.ty != Ty::FLOAT {
            return;
        }
        let def = &self.defs[d.0 as usize];
        let (name, file, at) = (def.name.clone(), def.file, def.span.end);
        let declared = format!("[{}: float]", self.show(key));
        if let Some(diag) = self.diags.last_mut() {
            diag.help = Some(format!(
                "a dictionary keeps its literal's value type, `int` here; \
                 to hold fractions, declare it: `state {name}: {declared} = …`"
            ));
            diag.suggestions.clear();
            if file == diag.file() {
                diag.suggestions.push(crate::diagnostic::Suggestion {
                    file,
                    span: Span::new(at, at),
                    replacement: format!(": {declared}"),
                    title: Some(format!("Declare `{name}: {declared}`")),
                });
            }
        }
    }

    /// The dictionary and key `t` writes an entry of, when `t` is `m[k]`,
    /// as `target` writes them (`last_win`, `3`), for messages.
    fn dict_entry(&self, t: &hir::Expr, target: &ast::Expr) -> Option<(String, String)> {
        let hir::ExprKind::Index { base, .. } = &t.kind else {
            return None;
        };
        if !matches!(base.ty.non_null(), Ty::Dict(..)) {
            return None;
        }
        let mut written = target;
        while let ast::ExprKind::Paren(inner) = &written.kind {
            written = inner;
        }
        let (m, k) = match &written.kind {
            ast::ExprKind::Index { base, index } => {
                (super::expr::written(base), super::expr::written(index))
            }
            _ => (None, None),
        };
        Some((
            m.unwrap_or_else(|| "m".into()),
            k.unwrap_or_else(|| "k".into()),
        ))
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
                        .find_schema_record("Node")
                        .is_some_and(|r| self.types.record(r).field(&name.name).is_some());
                    (!node_field).then_some(name.name.as_str())
                }
                // `vol.opacity = 1` with `id: vol`: a prop of that node,
                // one error rather than also "no field `opacity`".
                ast::ExprKind::Name(b) => {
                    let is_node = matches!(
                        self.lookup_scope(&b.name),
                        Some(super::Binding::Local(l))
                            if matches!(self.locals[l.0 as usize].kind, LocalKind::NodeId(_))
                    );
                    let n = name.name.as_str();
                    let node_field = self
                        .types
                        .find_schema_record("Node")
                        .is_some_and(|r| self.types.record(r).field(n).is_some());
                    if is_node
                        && !node_field
                        && self.schema.elements.values().any(|e| e.prop(n).is_some())
                    {
                        return Some(n.to_string());
                    }
                    None
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
            Ty::Dict(k, v) => {
                // One way to walk a dictionary: its entries, each a key
                // and a value (decisions.md, dictionary).
                let entry = self.entry_of(k, v);
                let shown = self.show(&iter.ty);
                let written = super::expr::path_text(ast);
                let at = Span::at(ast.span.end);
                let d = self.error(
                    "check::dict_for",
                    format!("`for` walks a list, and this is a dictionary, `{shown}`"),
                    iter.span,
                    "a dictionary",
                );
                d.help = Some(match &written {
                    Some(m) => format!(
                        "walk its entries, each a `key` and a `value`: `for e in {m}.entries`"
                    ),
                    None => "walk its entries, each a `key` and a `value`: `.entries`".into(),
                });
                d.add_suggestion(at, ".entries");
                entry
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
        let mut keys = Vec::new();
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
            ast::Event::Named {
                path,
                keys: written,
                params,
            } => {
                let (event, sig_params, known) = match self.event_params(path, span) {
                    Some((e, p)) => (e, p, true),
                    None => (
                        Event::Element(path.last().map_or(String::new(), |p| p.name.clone())),
                        Vec::new(),
                        false,
                    ),
                };
                keys = self.key_patterns(&event, written);
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
        self.ctx.in_loop = false;
        let calls = std::mem::take(&mut self.action_calls);
        let body = self.stmts(&o.body.items);
        self.action_calls = calls;
        self.pop_scope();
        self.ctx = saved;
        Some(hir::Handler {
            event,
            keys,
            when: None,
            params,
            body,
            span,
        })
    }

    /// (keyboard) The patterns `on key "ctrl+n", "down"` claims: each a
    /// string literal that reads as a key.
    fn key_patterns(&mut self, event: &Event, written: &'a [ast::Expr]) -> Vec<KeyPattern> {
        let Some(first) = written.first() else {
            return Vec::new();
        };
        if !matches!(event, Event::Element(e) if e == "key") {
            self.error(
                "check::key_pattern",
                "only `on key` takes key patterns",
                first.span,
                "not `on key`",
            );
            return Vec::new();
        }
        let mut keys = Vec::new();
        for e in written {
            let ast::ExprKind::String(s) = &e.kind else {
                self.expr(e, None);
                self.error(
                    "check::key_pattern",
                    "a key pattern is a string literal",
                    e.span,
                    "not a literal",
                )
                .help = Some(
                    "the router matches patterns without asking logic, so they are fixed: `on key \"ctrl+n\" { … }`; to turn one on and off, put it in a `when`"
                        .into(),
                );
                continue;
            };
            match KeyPattern::parse(&s.value) {
                Ok(k) => keys.push(k),
                Err(err) => self.key_pattern_error(e.span, &err),
            }
        }
        keys
    }

    /// A pattern that does not parse, with the closest it could have meant.
    fn key_pattern_error(&mut self, span: Span, err: &KeyPatternError) {
        let closest = match err {
            KeyPatternError::UnknownModifier(m) => {
                crate::diagnostic::suggest(&m.to_ascii_lowercase(), KEY_MODIFIERS.iter().copied())
                    .map(str::to_string)
            }
            KeyPatternError::UnknownKey(k) => {
                let names: Vec<String> = strand_scene::key_names().collect();
                Self::closest(&k.to_ascii_lowercase(), &names)
            }
            _ => None,
        };
        let help = match err {
            KeyPatternError::NoKey => "write modifiers before a key: `ctrl+n`".to_string(),
            KeyPatternError::UnknownModifier(_) => match closest {
                Some(c) => format!(
                    "did you mean `{c}`? the modifiers are `ctrl`, `shift`, `alt` and `super`"
                ),
                None => "the modifiers are `ctrl`, `shift`, `alt` and `super`".to_string(),
            },
            KeyPatternError::UnknownKey(_) => match closest {
                Some(c) => format!("did you mean `{c}`?"),
                None => "a key is one character or a key's name, such as `escape`, `pgdn` or `F5`"
                    .to_string(),
            },
            KeyPatternError::DuplicateModifier(_) => "name each modifier once".to_string(),
            KeyPatternError::StrayPlus => {
                "`+` joins modifiers to a key: `ctrl+n`; the `+` key itself is `\"+\"` or `\"ctrl++\"`"
                    .to_string()
            }
            KeyPatternError::ShiftOnCharacter(_) => {
                "write the character it types, with no `shift`: `\"!\"`, not `\"shift+1\"`"
                    .to_string()
            }
        };
        self.error(
            "check::key_pattern",
            err.to_string(),
            span,
            "not a key pattern",
        )
        .help = Some(help);
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
                        let fix = Self::closest(name, &candidates);
                        let d = self.error(
                            "check::unknown_event",
                            format!("`{kind}` has no event `{name}`"),
                            one.span,
                            "unknown event",
                        );
                        d.help = Some(format!(
                            "`{kind}` has {}",
                            candidates
                                .iter()
                                .map(|c| format!("`{c}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        d.suggest_opt(one.span, fix);
                        None
                    }
                }
            }
            [svc, ev] => {
                // (proc-run) `on backup.line(doc)`: an action's block.
                if let Some(&d) = self.globals.get(&svc.name)
                    && self.defs[d.0 as usize].kind == hir::DefKind::Action
                {
                    return self.action_event(d, svc, ev);
                }
                // (M6) A config's `service` first: it shadows the builtin
                // of its name (spec.md 4.2), events included.
                let rec = self
                    .globals
                    .get(&svc.name)
                    .and_then(|d| match self.defs[d.0 as usize].kind {
                        hir::DefKind::Service(r) => Some(r),
                        _ => None,
                    })
                    .or_else(|| self.schema.service(&svc.name));
                let Some(r) = rec else {
                    let candidates: Vec<String> = self.schema.services.keys().cloned().collect();
                    let fix = Self::closest(&svc.name, &candidates);
                    self.error(
                        "check::unknown_name",
                        format!("unknown service `{}`", svc.name),
                        svc.span,
                        "not a service",
                    )
                    .suggest_opt(svc.span, fix);
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
                        let fix = Self::closest(&ev.name, &candidates);
                        self.error(
                            "check::unknown_event",
                            format!("`{}` has no event `{}`", svc.name, ev.name),
                            ev.span,
                            "unknown event",
                        )
                        .suggest_opt(ev.span, fix);
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

    /// (proc-run) `on backup.line(doc)`: the `line` event of action `d`'s
    /// block, heard as the service serving it (decisions.md
    /// proc-interface). An action without a block has no events.
    fn action_event(
        &mut self,
        d: hir::DefId,
        svc: &ast::Ident,
        ev: &ast::Ident,
    ) -> Option<(Event, Vec<ParamSig>)> {
        self.add_ref(svc.span, Target::Def(d));
        let params = self
            .action_blocks
            .get(&d)
            .and_then(|r| self.types.record(*r).event(&ev.name))
            .map(|e| e.params.clone());
        if let Some(params) = params {
            return Some((
                Event::Service {
                    service: svc.name.clone(),
                    event: ev.name.clone(),
                },
                params,
            ));
        }
        let result =
            matches!(&self.defs[d.0 as usize].ty, Ty::Fn(sig) if matches!(sig.ret, Ty::Async(_)));
        let help = if self.action_blocks.contains_key(&d) {
            format!("`{}` has `line`", svc.name)
        } else if result {
            "an action with a result reads its output whole, as its `-> T`; its call's value \
             is what it printed"
                .to_string()
        } else {
            format!(
                "declare the fields its lines fill after its command: `action {}(…) [...] {{ \
                 line: text }}`; `{}.lines` holds what it printed without one",
                svc.name, svc.name
            )
        };
        let d = self.error(
            "check::unknown_event",
            format!("`{}` has no event `{}`", svc.name, ev.name),
            ev.span,
            "unknown event",
        );
        d.help = Some(help);
        None
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
        self.ctx.in_loop = false;
        let calls = std::mem::take(&mut self.action_calls);
        let body = self.stmts(&t.body.items);
        self.action_calls = calls;
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
