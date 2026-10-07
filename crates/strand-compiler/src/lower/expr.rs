//! Expressions and statements to bytecode.

use std::collections::BTreeSet;

use crate::hir::{
    self, AssignOp, BinaryOp, CallArg, Callee, DefId, DefKind, ExprKind, LambdaBody, LocalKind,
    Pattern as HPat, StmtKind, UnaryOp, Unit,
};
use crate::syntax::Span;
use crate::ty::Ty;
use crate::vm::value::Num;

use super::Lowerer;
use super::code::{
    ArgMap, Chunk, ChunkId, Const, KeyedQuery, KeyedRoot, Lambda, Op, Pattern, Place, PlaceRoot,
    PlaceSeg,
};

/// List methods that change the collection they are called on.
pub(crate) const MUTATIONS: &[&str] = &[
    "push",
    "insert",
    "remove",
    "clear",
    "remove_key",
    "move",
    "update",
];

impl Lowerer<'_> {
    /// A chunk computing `e`.
    pub(crate) fn expr_chunk(&mut self, e: &hir::Expr) -> ChunkId {
        let mut c = Chunk::new(self.file);
        self.expr(&mut c, e);
        self.add_chunk(c)
    }

    /// A handler or timer body.
    pub(crate) fn stmts_chunk(&mut self, body: &[hir::Stmt], _span: Span) -> ChunkId {
        let mut c = Chunk::new(self.file);
        self.frame += 1;
        for s in body {
            self.stmt(&mut c, s, false);
        }
        self.frame -= 1;
        self.add_chunk(c)
    }

    /// A `fn` body or block lambda: its value is its last expression.
    pub(crate) fn fn_chunk(&mut self, body: &[hir::Stmt]) -> ChunkId {
        let mut c = Chunk::new(self.file);
        self.frame += 1;
        self.block_value(&mut c, body);
        self.frame -= 1;
        self.add_chunk(c)
    }

    fn block_value(&mut self, c: &mut Chunk, body: &[hir::Stmt]) {
        let n = body.len();
        for (i, s) in body.iter().enumerate() {
            self.stmt(c, s, i + 1 == n);
        }
        if !matches!(body.last().map(|s| &s.kind), Some(StmtKind::Expr(_))) {
            let span = body.last().map_or(Span::default(), |s| s.span);
            c.emit(Op::Unit, span);
        }
    }

    fn stmt(&mut self, c: &mut Chunk, s: &hir::Stmt, keep: bool) {
        let span = s.span;
        match &s.kind {
            StmtKind::Let { local, value } => {
                self.expr(c, value);
                c.emit(Op::SetLocal(*local), span);
            }
            StmtKind::Assign { target, op, value } => {
                let place = self.place(c, target);
                self.expr(c, value);
                let place = add_place(c, place);
                c.emit(Op::Store { place, op: *op }, span);
            }
            StmtKind::Expr(e) => {
                self.expr(c, e);
                if !keep {
                    c.emit(Op::Pop, span);
                }
            }
            StmtKind::If { cond, then, else_ } => {
                self.expr(c, cond);
                let to_else = c.emit(Op::JumpIfFalse(0), span);
                self.block(c, then, span);
                let to_end = c.emit(Op::Jump(0), span);
                c.patch(to_else);
                self.block(c, else_, span);
                c.patch(to_end);
            }
            StmtKind::For {
                binding,
                iter,
                body,
            } => {
                self.expr(c, iter);
                let zero = c.constant(Const::Num(0.0, Num::Int));
                c.emit(Op::Const(zero), span);
                // Each iteration drops the last one's locals, so a long
                // loop keeps a frame of constant size.
                c.emit(Op::ScopeEnter, span);
                let top = c.here();
                let next = c.emit(
                    Op::IterNext {
                        binding: *binding,
                        end: 0,
                    },
                    span,
                );
                for s in body {
                    self.stmt(c, s, false);
                }
                c.emit(Op::Jump(top), span);
                c.patch(next);
                c.emit(Op::ScopeExit, span);
            }
            StmtKind::Match { scrutinee, arms } => {
                self.expr(c, scrutinee);
                let mut ends = Vec::new();
                for (p, body) in arms {
                    c.emit(Op::Dup, span);
                    let pat = self.pattern(c, p);
                    c.emit(Op::Match(pat), span);
                    let next = c.emit(Op::JumpIfFalse(0), span);
                    c.emit(Op::Pop, span);
                    self.block(c, body, span);
                    ends.push(c.emit(Op::Jump(0), span));
                    c.patch(next);
                }
                // No arm: nothing to do (statement `match`).
                c.emit(Op::Pop, span);
                for e in ends {
                    c.patch(e);
                }
            }
            StmtKind::Play(e) => {
                self.expr(c, e);
                c.emit(Op::Play, span);
            }
            StmtKind::Error => {}
        }
    }

    /// A statement block; one that binds locals is a scope, so they are
    /// dropped at its end.
    fn block(&mut self, c: &mut Chunk, body: &[hir::Stmt], span: Span) {
        let scoped = body.iter().any(|s| matches!(s.kind, StmtKind::Let { .. }));
        if scoped {
            c.emit(Op::ScopeEnter, span);
        }
        for s in body {
            self.stmt(c, s, false);
        }
        if scoped {
            c.emit(Op::ScopeExit, span);
        }
    }

    /// An item of a service's keyed list (a schema record with a key):
    /// a write below it goes to the service by the item's key.
    pub(crate) fn is_service_item(&self, ty: &Ty) -> bool {
        match ty.non_null() {
            Ty::Record(r) => {
                let def = self.hir.types.record(*r);
                def.key.is_some() && !def.handle && matches!(def.origin, crate::ty::Origin::Schema)
            }
            _ => false,
        }
    }

    /// The root of `target`'s place, its segments (leaf last), the index
    /// expressions above the root and, for [`PlaceRoot::Item`], the item
    /// expression: a path of fields from a service is that service's
    /// field (`audio.sink.volume`); otherwise the base nearest the leaf
    /// that is an item of a service's keyed list, else the
    /// `state`/settings or service it starts at. `None`: no writable root
    /// (the checker reported it).
    fn place_parts<'e>(&self, target: &'e hir::Expr) -> Option<PlaceParts<'e>> {
        // `audio.sink.volume`: fields all the way from the service is a
        // write of the service's field, whatever records it crosses.
        let mut cur = target;
        let fields_from_service = loop {
            match &cur.kind {
                ExprKind::Field { base, .. } => cur = base,
                ExprKind::Service(_) => break true,
                _ => break false,
            }
        };
        let mut segs = Vec::new();
        let mut cur = target;
        let mut indices = Vec::new();
        let mut item = None;
        let root = loop {
            if !fields_from_service && !std::ptr::eq(cur, target) && self.is_service_item(&cur.ty) {
                item = Some(cur);
                break PlaceRoot::Item;
            }
            match &cur.kind {
                ExprKind::Field { base, name, .. } => {
                    segs.push(PlaceSeg::Field(name.clone()));
                    cur = base;
                }
                ExprKind::Index { base, index } => {
                    segs.push(PlaceSeg::Index);
                    indices.push(index.as_ref());
                    cur = base;
                }
                ExprKind::Def(d) => break PlaceRoot::Def(*d),
                ExprKind::Service(s) => break PlaceRoot::Service(s.clone()),
                _ => return None,
            }
        };
        segs.reverse();
        indices.reverse();
        Some(PlaceParts {
            root,
            segs,
            indices,
            item,
        })
    }

    /// Pushes the item (for [`PlaceRoot::Item`]) and the index values of
    /// `target`'s place and returns the place.
    fn place(&mut self, c: &mut Chunk, target: &hir::Expr) -> Place {
        let Some(PlaceParts {
            root,
            segs,
            indices,
            item,
        }) = self.place_parts(target)
        else {
            // Rejected by the checker (`check::read_only`): nothing
            // reaches a program that writes here.
            return Place {
                root: PlaceRoot::Service(String::new()),
                segs: Vec::new(),
            };
        };
        if let PlaceRoot::Service(s) = &root {
            self.note_service(s);
        }
        if let Some(e) = item {
            self.expr(c, e);
        }
        for i in indices {
            self.expr(c, i);
        }
        Place { root, segs }
    }

    /// The place of a two-way target, with one chunk per index.
    pub(crate) fn two_way_place(
        &mut self,
        target: &hir::Expr,
        indices: &mut Vec<ChunkId>,
    ) -> Option<Place> {
        let PlaceParts {
            root,
            segs,
            indices: idx,
            item,
        } = self.place_parts(target)?;
        for e in item.into_iter().chain(idx) {
            let id = self.expr_chunk(e);
            indices.push(id);
        }
        Some(Place { root, segs })
    }

    /// A keyed collection read in place: a keyed `state` (`pins`) or a
    /// service's keyed field (`notifications.popups`). Notes the service.
    pub(crate) fn keyed_root(&mut self, c: &mut Chunk, e: &hir::Expr) -> Option<KeyedRoot> {
        if !matches!(e.ty, Ty::List(_, true)) {
            return None;
        }
        match &e.kind {
            ExprKind::Def(d) if self.hir.def(*d).kind == DefKind::State || self.is_view(*d) => {
                Some(KeyedRoot::Def(*d))
            }
            ExprKind::Field {
                base,
                name,
                optional: false,
            } => match &base.kind {
                ExprKind::Service(s) => {
                    self.note_field(s, name);
                    Some(KeyedRoot::Service {
                        service: c.name(s),
                        field: c.name(name),
                    })
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn note_service(&mut self, s: &str) {
        if let Some(top) = self.services.last_mut() {
            top.insert((s.to_string(), None));
        }
    }

    /// A direct read of `service.field` (it also holds the service).
    fn note_field(&mut self, s: &str, field: &str) {
        if let Some(top) = self.services.last_mut() {
            top.insert((s.to_string(), None));
            top.insert((s.to_string(), Some(field.to_string())));
        }
    }

    fn pattern(&mut self, c: &mut Chunk, p: &HPat) -> u32 {
        let pat = match p {
            HPat::Wildcard => Pattern::Wildcard,
            HPat::Variant(e, v) => Pattern::Variant(*e, *v),
            HPat::Literal(e) => match &e.kind {
                ExprKind::Number { value, unit } => {
                    Pattern::Literal(c.constant(num_const(*value, *unit, &e.ty)))
                }
                ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr,
                } => match &expr.kind {
                    ExprKind::Number { value, unit } => {
                        Pattern::Literal(c.constant(num_const(-*value, *unit, &e.ty)))
                    }
                    _ => Pattern::Error,
                },
                ExprKind::Text(t) => Pattern::Literal(c.constant(Const::Text(t.clone()))),
                ExprKind::Color(rgba) => Pattern::Literal(c.constant(Const::Color(*rgba))),
                ExprKind::Bool(b) => Pattern::Bool(*b),
                ExprKind::Null => Pattern::Null,
                ExprKind::Variant(e, v) => Pattern::Variant(*e, *v),
                _ => Pattern::Error,
            },
            HPat::Error => Pattern::Error,
        };
        c.patterns.push(pat);
        c.patterns.len() as u32 - 1
    }

    /// A chunk giving the index of the first arm of a tree `match` whose
    /// pattern the scrutinee matches (`null` when none does).
    pub(crate) fn match_selector(&mut self, scrutinee: &hir::Expr, pats: &[&HPat]) -> ChunkId {
        let mut c = Chunk::new(self.file);
        let span = scrutinee.span;
        self.expr(&mut c, scrutinee);
        let mut ends = Vec::new();
        for (i, p) in pats.iter().enumerate() {
            c.emit(Op::Dup, span);
            let pat = self.pattern(&mut c, p);
            c.emit(Op::Match(pat), span);
            let next = c.emit(Op::JumpIfFalse(0), span);
            c.emit(Op::Pop, span);
            let k = c.constant(Const::Num(i as f64, Num::Int));
            c.emit(Op::Const(k), span);
            ends.push(c.emit(Op::Jump(0), span));
            c.patch(next);
        }
        c.emit(Op::Pop, span);
        c.emit(Op::Null, span);
        for e in ends {
            c.patch(e);
        }
        self.add_chunk(c)
    }

    pub(crate) fn expr(&mut self, c: &mut Chunk, e: &hir::Expr) {
        // Deep expressions are bounded by the parser (256 levels); grow
        // the stack on small worker threads anyway.
        stacker::maybe_grow(64 * 1024, 1024 * 1024, || self.expr_inner(c, e));
    }

    fn expr_inner(&mut self, c: &mut Chunk, e: &hir::Expr) {
        let span = e.span;
        match &e.kind {
            ExprKind::Number { value, unit } => {
                let k = c.constant(num_const(*value, *unit, &e.ty));
                c.emit(Op::Const(k), span);
            }
            ExprKind::Text(t) => {
                let k = c.constant(Const::Text(t.clone()));
                c.emit(Op::Const(k), span);
            }
            ExprKind::Color(rgba) => {
                let k = c.constant(Const::Color(*rgba));
                c.emit(Op::Const(k), span);
            }
            ExprKind::Bool(b) => {
                c.emit(Op::Bool(*b), span);
            }
            ExprKind::Null | ExprKind::Error => {
                c.emit(Op::Null, span);
            }
            ExprKind::Local(l) => {
                let local = self.hir.local(*l);
                if local.kind == LocalKind::Channel {
                    let k = c.constant(Const::Channel(local.name.clone()));
                    c.emit(Op::Const(k), span);
                } else if let LocalKind::NodeId(n) = local.kind {
                    c.emit(Op::Node(n), span);
                } else {
                    c.emit(Op::Local(*l), span);
                }
            }
            ExprKind::Def(d) => {
                c.emit(Op::Def(*d), span);
            }
            ExprKind::Service(s) => {
                self.note_service(s);
                let n = c.name(s);
                c.emit(Op::Service(n), span);
            }
            ExprKind::Value(v) => {
                self.note_time(v, span);
                let n = c.name(v);
                c.emit(Op::Value(n), span);
            }
            ExprKind::Node(n) => {
                c.emit(Op::Node(*n), span);
            }
            ExprKind::Variant(en, v) => {
                c.emit(Op::Variant(*en, *v), span);
            }
            ExprKind::EnumType(en) => {
                c.emit(Op::EnumType(*en), span);
            }
            ExprKind::Token(p) => {
                let n = c.name(p);
                c.emit(Op::Token(n), span);
            }
            ExprKind::Field {
                base,
                name,
                optional: false,
            } if matches!(name.as_str(), "len" | "first" | "last")
                && self.keyed_root(c, base).is_some() =>
            {
                let root = self.keyed_root(c, base).unwrap_or(KeyedRoot::Def(DefId(0)));
                let query = match name.as_str() {
                    "len" => KeyedQuery::Len,
                    "first" => KeyedQuery::First,
                    _ => KeyedQuery::Last,
                };
                c.emit(Op::Keyed { root, query }, span);
            }
            ExprKind::Index { base, index } if self.keyed_root(c, base).is_some() => {
                let root = self.keyed_root(c, base).unwrap_or(KeyedRoot::Def(DefId(0)));
                self.expr(c, index);
                c.emit(
                    Op::Keyed {
                        root,
                        query: KeyedQuery::Index,
                    },
                    span,
                );
            }
            ExprKind::Call {
                callee: Callee::Method { receiver, name, .. },
                args,
            } if name == "contains"
                && args.len() == 1
                && self.keyed_root(c, receiver).is_some() =>
            {
                let root = self
                    .keyed_root(c, receiver)
                    .unwrap_or(KeyedRoot::Def(DefId(0)));
                self.expr(c, &args[0].value);
                c.emit(
                    Op::Keyed {
                        root,
                        query: KeyedQuery::Contains,
                    },
                    span,
                );
            }
            ExprKind::Field {
                base,
                name,
                optional,
            } => {
                if let ExprKind::Service(svc) = &base.kind {
                    self.note_field(svc, name);
                }
                self.expr(c, base);
                let skip = optional.then(|| c.emit(Op::NullJump(0), span));
                let n = c.name(name);
                c.emit(Op::Field(n), span);
                if let Some(s) = skip {
                    c.patch(s);
                }
            }
            ExprKind::Index { base, index } => {
                self.expr(c, base);
                self.expr(c, index);
                c.emit(Op::Index, span);
            }
            ExprKind::Unary { op, expr } => {
                self.expr(c, expr);
                if *op == UnaryOp::Await {
                    c.emit(Op::Await, span);
                } else {
                    c.emit(Op::Unary(*op), span);
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.expr(c, lhs);
                let jump = match op {
                    BinaryOp::And => Some(Op::AndJump(0)),
                    BinaryOp::Or => Some(Op::OrJump(0)),
                    BinaryOp::Coalesce => Some(Op::CoalesceJump(0)),
                    _ => None,
                };
                match jump {
                    Some(j) => {
                        let at = c.emit(j, span);
                        self.expr(c, rhs);
                        c.patch(at);
                    }
                    None => {
                        self.expr(c, rhs);
                        c.emit(Op::Binary(*op), span);
                    }
                }
            }
            ExprKind::Ternary { cond, then, else_ } => {
                self.expr(c, cond);
                let to_else = c.emit(Op::JumpIfFalse(0), span);
                self.expr(c, then);
                let to_end = c.emit(Op::Jump(0), span);
                c.patch(to_else);
                self.expr(c, else_);
                c.patch(to_end);
            }
            ExprKind::Lambda { params, body } => {
                let mut inner = Chunk::new(self.file);
                self.frame += 1;
                match body {
                    LambdaBody::Expr(b) => self.expr(&mut inner, b),
                    LambdaBody::Block(stmts) => self.block_value(&mut inner, stmts),
                }
                self.frame -= 1;
                let free = free_locals(&inner, params);
                let chunk = self.add_chunk(inner);
                c.lambdas.push(Lambda {
                    params: params.clone(),
                    chunk,
                    free,
                });
                let i = c.lambdas.len() as u32 - 1;
                c.emit(Op::Closure(i), span);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(c, scrutinee);
                let mut ends = Vec::new();
                for (p, value) in arms {
                    c.emit(Op::Dup, span);
                    let pat = self.pattern(c, p);
                    c.emit(Op::Match(pat), span);
                    let next = c.emit(Op::JumpIfFalse(0), span);
                    c.emit(Op::Pop, span);
                    self.expr(c, value);
                    ends.push(c.emit(Op::Jump(0), span));
                    c.patch(next);
                }
                c.emit(Op::Pop, span);
                let msg = c.name("no `match` arm for this value");
                c.emit(Op::Fail(msg), span);
                for e in ends {
                    c.patch(e);
                }
            }
            ExprKind::List(items) => {
                for i in items {
                    self.expr(c, i);
                }
                c.emit(Op::List(items.len() as u32), span);
            }
            ExprKind::Commas(items) => {
                for i in items {
                    self.expr(c, i);
                }
                c.emit(Op::Commas(items.len() as u32), span);
            }
            ExprKind::Spaced(items) => {
                for i in items {
                    self.expr(c, i);
                }
                c.emit(Op::Spaced(items.len() as u32), span);
            }
            ExprKind::Call { callee, args } => {
                let service_async = matches!(e.ty, Ty::Async(_))
                    && match callee {
                        Callee::Method {
                            receiver,
                            name,
                            overload,
                        } => {
                            matches!(receiver.kind, ExprKind::Service(_))
                                && !self.method_sig(&receiver.ty, name, *overload).2
                        }
                        _ => false,
                    };
                // Only the site's own call is lowered as a plain call: its
                // arguments' async calls are sites of their own.
                let whole = std::mem::take(&mut self.whole_async_call);
                if service_async && self.frame == 0 && !whole {
                    // In a binding: the scope's load of this call (its
                    // own chunk, as a `let` of it would be).
                    self.whole_async_call = true;
                    let site = self.expr_chunk(e);
                    c.emit(Op::AsyncSite(site), span);
                } else {
                    self.call(c, callee, args, span, service_async && self.frame > 0);
                }
            }
        }
    }

    /// `fetch`: an async service method called in a handler, `fn` or
    /// lambda ([`Op::FetchMethod`]).
    fn call(&mut self, c: &mut Chunk, callee: &Callee, args: &[CallArg], span: Span, fetch: bool) {
        match callee {
            Callee::Fn(d) => {
                let arity = self.out_fn_arity(*d);
                let map = self.push_args(c, args, arity, None);
                c.emit(Op::CallFn { def: *d, args: map }, span);
            }
            Callee::Builtin { name, overload } => {
                self.note_time(name, span);
                let sig = self
                    .schema
                    .functions
                    .get(name)
                    .and_then(|o| o.get(*overload))
                    .cloned();
                let (arity, variadic) = sig.as_ref().map_or((args.len(), None), |s| {
                    (s.params.len(), s.params.iter().position(|p| p.variadic))
                });
                let map = self.push_args(c, args, arity, variadic);
                let n = c.name(name);
                c.emit(
                    Op::CallBuiltin {
                        name: n,
                        overload: *overload as u16,
                        args: map,
                    },
                    span,
                );
            }
            Callee::Method {
                receiver,
                name,
                overload,
            } => {
                let list_recv = matches!(receiver.ty.non_null(), Ty::List(..));
                if list_recv && MUTATIONS.contains(&name.as_str()) {
                    let place = self.place(c, receiver);
                    let arity = mutation_arity(name);
                    let map = self.push_args(c, args, arity, None);
                    let place = add_place(c, place);
                    let method = c.name(name);
                    c.emit(
                        Op::Mutate {
                            place,
                            method,
                            args: map,
                        },
                        span,
                    );
                    return;
                }
                self.expr(c, receiver);
                let (arity, variadic, action) = self.method_sig(&receiver.ty, name, *overload);
                if action {
                    for s in self.action_services(&receiver.ty) {
                        if !c.actions.contains(&s) {
                            c.actions.push(s);
                        }
                    }
                }
                let arity = arity.unwrap_or(args.len());
                let map = self.push_args(c, args, arity, variadic.map(usize::from));
                let n = c.name(name);
                if fetch {
                    c.emit(Op::FetchMethod { name: n, args: map }, span);
                    return;
                }
                c.emit(
                    Op::CallMethod {
                        name: n,
                        args: map,
                        action,
                    },
                    span,
                );
            }
            Callee::Record(r) => {
                let arity = self.hir.types.record(*r).fields.len();
                let map = self.push_args(c, args, arity, None);
                c.emit(Op::MakeRecord { ty: *r, args: map }, span);
            }
            Callee::Value(f) => {
                self.expr(c, f);
                let map = self.push_args(c, args, args.len(), None);
                c.emit(Op::CallValue { args: map }, span);
            }
            Callee::Error => {
                c.emit(Op::Null, span);
            }
        }
    }

    fn out_fn_arity(&self, d: crate::hir::DefId) -> usize {
        match &self.hir.def(d).ty {
            Ty::Fn(sig) => sig.params.len(),
            _ => 0,
        }
    }

    /// Arity, variadic parameter and whether it is an action, for a method.
    /// The services an action on a value of type `recv` can change: the
    /// service itself (`notifications.clear()`), or every service whose
    /// fields reach the record (`n.expire()` on a `Notification`). A
    /// superset is fine: these become declared write edges.
    fn action_services(&self, recv: &Ty) -> Vec<String> {
        fn inner(t: &Ty) -> Option<&Ty> {
            match t {
                Ty::List(t, _) | Ty::Optional(t) | Ty::Async(t) => Some(t),
                _ => None,
            }
        }
        let mut t = recv;
        while let Some(i) = inner(t) {
            t = i;
        }
        let Ty::Record(target) = t else {
            return Vec::new();
        };
        let types = &self.hir.types;
        let mut out = Vec::new();
        for (name, &root) in &self.schema.services {
            let mut seen = BTreeSet::new();
            let mut stack = vec![root];
            while let Some(r) = stack.pop() {
                if !seen.insert(r) {
                    continue;
                }
                if r == *target {
                    out.push(name.clone());
                    break;
                }
                for f in &types.record(r).fields {
                    let mut ft = &f.ty;
                    while let Some(i) = inner(ft) {
                        ft = i;
                    }
                    if let Ty::Record(x) = ft {
                        stack.push(*x);
                    }
                }
            }
        }
        out
    }

    fn method_sig(
        &self,
        recv: &Ty,
        name: &str,
        overload: usize,
    ) -> (Option<usize>, Option<u16>, bool) {
        let from_methods = |ms: &[crate::ty::MethodDef]| {
            ms.iter().find(|m| m.name == name).and_then(|m| {
                m.sigs.get(overload).map(|s| {
                    (
                        Some(s.params.len()),
                        s.params.iter().position(|p| p.variadic).map(|i| i as u16),
                        s.action,
                    )
                })
            })
        };
        let t = match recv.non_null() {
            Ty::Async(inner) => inner.non_null().clone(),
            t => t.clone(),
        };
        let found = match &t {
            Ty::Record(r) => from_methods(&self.hir.types.record(*r).methods),
            Ty::Prim(crate::ty::Prim::Int) => from_methods(self.schema.methods_of("int"))
                .or_else(|| from_methods(self.schema.methods_of("float"))),
            Ty::Prim(p) => from_methods(self.schema.methods_of(p.name())),
            Ty::Opaque(n) => from_methods(self.schema.methods_of(n)),
            _ => None,
        };
        found.unwrap_or((None, None, false))
    }

    /// Pushes `args` in source order and records which parameter each
    /// fills.
    fn push_args(
        &mut self,
        c: &mut Chunk,
        args: &[CallArg],
        arity: usize,
        variadic: Option<usize>,
    ) -> u32 {
        let mut params = Vec::with_capacity(args.len());
        let mut next = 0usize;
        for a in args {
            self.expr(c, &a.value);
            let p = a.param.unwrap_or(next);
            next = p + 1;
            params.push(p as u16);
        }
        let arity = params
            .iter()
            .map(|&p| p as usize + 1)
            .max()
            .unwrap_or(0)
            .max(arity);
        c.args.push(ArgMap {
            params,
            arity: arity as u16,
            variadic: variadic.map(|v| v as u16),
        });
        c.args.len() as u32 - 1
    }
}

fn add_place(c: &mut Chunk, p: Place) -> u32 {
    c.places.push(p);
    c.places.len() as u32 - 1
}

fn mutation_arity(name: &str) -> usize {
    match name {
        "clear" => 0,
        "insert" | "move" | "update" => 2,
        _ => 1,
    }
}

/// A number literal as a constant, its unit from the literal or, for a
/// plain number, from the type the checker gave it.
fn num_const(value: f64, unit: Option<Unit>, ty: &Ty) -> Const {
    use crate::ty::Prim;
    let (v, u) = match unit {
        Some(Unit::Px) => (value, Num::Px),
        Some(Unit::Percent) => (value, Num::Percent),
        Some(Unit::Deg) => (value, Num::Deg),
        Some(Unit::Ch) => (value, Num::Ch),
        Some(Unit::S) => (value * 1000.0, Num::Ms),
        Some(Unit::Ms) => (value, Num::Ms),
        None => match ty.non_null() {
            Ty::Prim(Prim::Int) => (value, Num::Int),
            Ty::Prim(Prim::Length) => (value, Num::Px),
            Ty::Prim(Prim::Duration) => (value * 1000.0, Num::Ms),
            Ty::Prim(Prim::Angle) => (value, Num::Deg),
            _ => (value, Num::Float),
        },
    };
    Const::Num(v, u)
}

impl AssignOp {
    /// The binary operator a compound assignment applies.
    pub fn binary(self) -> Option<BinaryOp> {
        match self {
            AssignOp::Set => None,
            AssignOp::Add => Some(BinaryOp::Add),
            AssignOp::Sub => Some(BinaryOp::Sub),
            AssignOp::Mul => Some(BinaryOp::Mul),
            AssignOp::Div => Some(BinaryOp::Div),
        }
    }
}

/// The locals a lambda body reads that it does not bind itself: its own
/// `Op::Local`s and its inner lambdas' free locals, less its parameters
/// and `let`s.
fn free_locals(inner: &Chunk, params: &[hir::LocalId]) -> Vec<hir::LocalId> {
    let mut bound: BTreeSet<hir::LocalId> = params.iter().copied().collect();
    let mut used = BTreeSet::new();
    for op in &inner.ops {
        match op {
            Op::Local(l) => {
                used.insert(*l);
            }
            Op::SetLocal(l) | Op::IterNext { binding: l, .. } => {
                bound.insert(*l);
            }
            _ => {}
        }
    }
    for l in &inner.lambdas {
        used.extend(l.free.iter().copied());
    }
    used.difference(&bound).copied().collect()
}

/// A writable place taken apart: see [`Lowerer::place_parts`].
struct PlaceParts<'e> {
    root: PlaceRoot,
    /// Its segments, leaf last.
    segs: Vec<PlaceSeg>,
    /// The index expressions above the root.
    indices: Vec<&'e hir::Expr>,
    /// The item expression of a [`PlaceRoot::Item`] place.
    item: Option<&'e hir::Expr>,
}
