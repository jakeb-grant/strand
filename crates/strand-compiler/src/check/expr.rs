//! Expressions: names, fields, calls, operators, lambdas, `match`.

use std::sync::Arc;

use super::{Binding, Checker, NODE_BOOLS};
use crate::diagnostic::suggest;
use crate::hir::{self, CallArg, Callee, DefKind, ExprKind, LocalKind, Target};
use crate::syntax::Span;
use crate::syntax::ast::{self, ArgKind, BinaryOp, UnaryOp};
use crate::ty::{FnSig, ParamSig, Prim, RecordId, Ty};

/// Why a place cannot be written.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NotWritable {
    Let(String),
    Param(String),
    BoundProp(String),
    ReadOnlyField {
        owner: String,
        field: String,
    },
    /// `prefs = …` for `state prefs from "….toml" { … }`.
    SettingsRecord {
        name: String,
        field: String,
    },
    Local(String),
    Other,
}

/// `a.b.c` for messages, if `e` is a plain path.
pub(crate) fn path_text(e: &ast::Expr) -> Option<String> {
    match &e.kind {
        ast::ExprKind::Name(i) => Some(i.name.clone()),
        ast::ExprKind::Field {
            base,
            name,
            optional,
        } => Some(format!(
            "{}{}{}",
            path_text(base)?,
            if *optional { "?." } else { "." },
            name.name
        )),
        ast::ExprKind::Token(k) => Some(format!(
            "${}",
            k.segments
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(".")
        )),
        ast::ExprKind::Call { callee, .. } => Some(format!("{}(…)", path_text(callee)?)),
        ast::ExprKind::Paren(inner) => path_text(inner),
        _ => None,
    }
}

fn quoted(e: &ast::Expr) -> String {
    path_text(e).map_or_else(|| "this value".to_string(), |p| format!("`{p}`"))
}

impl<'a> Checker<'a> {
    /// Checks `e` and returns it typed. `expected` guides enum variants,
    /// lambda parameters, number literals and empty lists; mismatches are
    /// reported by [`Checker::require`].
    pub fn expr(&mut self, e: &'a ast::Expr, expected: Option<&Ty>) -> hir::Expr {
        // A thin dispatcher: deep expressions recurse through here, so each
        // case lives in its own function and this frame stays small.
        let span = e.span;
        match &e.kind {
            ast::ExprKind::Number(n) => self.number(n, expected, span),
            ast::ExprKind::String(s) => hir::Expr {
                kind: ExprKind::Text(s.value.clone()),
                ty: Ty::TEXT,
                span,
            },
            ast::ExprKind::Color(c) => self.color(c, span),
            ast::ExprKind::Bool(b) => hir::Expr {
                kind: ExprKind::Bool(*b),
                ty: Ty::BOOL,
                span,
            },
            ast::ExprKind::Null => hir::Expr {
                kind: ExprKind::Null,
                ty: Ty::Null,
                span,
            },
            ast::ExprKind::Name(id) => self.name_expr(id, expected),
            ast::ExprKind::Token(key) => self.token_expr(key),
            ast::ExprKind::Array(items) => self.array(items, expected, span),
            ast::ExprKind::Paren(inner) => {
                let mut h = self.expr(inner, expected);
                h.span = span;
                h
            }
            ast::ExprKind::Unary { op, expr } => self.unary(*op, expr, expected, span),
            ast::ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, expected, span),
            ast::ExprKind::Ternary { cond, then, else_ } => {
                self.ternary(cond, then, else_, expected, span)
            }
            ast::ExprKind::Field {
                base,
                name,
                optional,
            } => self.field_expr(base, name, *optional, span),
            ast::ExprKind::Call { callee, args } => self.call_expr(callee, args, span),
            ast::ExprKind::Index { base, index } => self.index(base, index, span),
            ast::ExprKind::Lambda { params, body } => self.lambda(params, body, expected, span),
            ast::ExprKind::Match(m) => self.match_expr(m, expected, span),
            ast::ExprKind::Commas(items) => self.loose_parts(items, true, span),
            ast::ExprKind::Spaced(items) => self.loose_parts(items, false, span),
            ast::ExprKind::Error => hir::Expr::error(span),
        }
    }

    #[inline(never)]
    fn number(&mut self, n: &ast::Number, expected: Option<&Ty>, span: Span) -> hir::Expr {
        let ty = match n.unit {
            Some(ast::Unit::Px | ast::Unit::Ch) => Ty::LENGTH,
            Some(ast::Unit::Percent) => Ty::PERCENT,
            Some(ast::Unit::Deg) => Ty::ANGLE,
            Some(ast::Unit::S | ast::Unit::Ms) => Ty::DURATION,
            None if !n.fraction && expected.is_some_and(accepts_int) => Ty::INT,
            None => Ty::FLOAT,
        };
        hir::Expr {
            kind: ExprKind::Number {
                value: n.value,
                unit: n.unit,
            },
            ty,
            span,
        }
    }

    #[inline(never)]
    fn color(&mut self, c: &ast::Color, span: Span) -> hir::Expr {
        if !self.ctx.prop && self.ctx.let_value {
            self.let_colours.push(span);
        }
        if self.ctx.prop {
            self.warning(
                "check::raw_color",
                "raw colour in a prop",
                span,
                "this colour ignores the theme",
            )
            .help = Some(
                "use a token such as `$accent` or `$fg.muted`, so theme swaps reach it".into(),
            );
        }
        hir::Expr {
            kind: ExprKind::Color(c.rgba),
            ty: Ty::COLOR,
            span,
        }
    }

    #[inline(never)]
    fn ternary(
        &mut self,
        cond: &'a ast::Expr,
        then: &'a ast::Expr,
        else_: &'a ast::Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        let c = self.expect(cond, &Ty::BOOL, "the condition");
        let t = self.expr(then, expected);
        let hint = expected.cloned().unwrap_or_else(|| t.ty.clone());
        let f = self.expr(else_, Some(&hint));
        let ty = self.join_branches(&t, &f, "`?:`");
        hir::Expr {
            kind: ExprKind::Ternary {
                cond: Box::new(c),
                then: Box::new(t),
                else_: Box::new(f),
            },
            ty,
            span,
        }
    }

    #[inline(never)]
    fn index(&mut self, base: &'a ast::Expr, index: &'a ast::Expr, span: Span) -> hir::Expr {
        let b = self.expr(base, None);
        let i = self.expect(index, &Ty::INT, "an index");
        let ty = match b.ty.clone() {
            Ty::Error | Ty::Any => Ty::Error,
            Ty::Optional(_) => {
                self.nullable_use(&b, base, "index");
                Ty::Error
            }
            t => match t.list_elem() {
                Some((elem, _)) => elem.clone(),
                None => {
                    let shown = self.show(&t);
                    self.error(
                        "check::type_mismatch",
                        format!("only lists can be indexed, not `{shown}`"),
                        base.span,
                        format!("this is `{shown}`"),
                    );
                    Ty::Error
                }
            },
        };
        hir::Expr {
            kind: ExprKind::Index {
                base: Box::new(b),
                index: Box::new(i),
            },
            ty,
            span,
        }
    }

    /// Commas or spaces outside a prop value (already reported by whoever
    /// placed them): checked for names only.
    #[inline(never)]
    fn loose_parts(&mut self, items: &'a [ast::Expr], commas: bool, span: Span) -> hir::Expr {
        let parts = items.iter().map(|i| self.expr(i, None)).collect();
        hir::Expr {
            kind: if commas {
                ExprKind::Commas(parts)
            } else {
                ExprKind::Spaced(parts)
            },
            ty: Ty::Error,
            span,
        }
    }

    /// Checks `e` against `ty`, reporting a mismatch.
    pub fn expect(&mut self, e: &'a ast::Expr, ty: &Ty, what: &str) -> hir::Expr {
        let h = self.expr(e, Some(ty));
        self.require(&h, ty, what);
        h
    }

    /// Reports `h` not fitting `ty`; true if it fits.
    pub fn require(&mut self, h: &hir::Expr, ty: &Ty, what: &str) -> bool {
        // `any` takes anything but a loading value (`join(",", hits)`):
        // the loading state would be forgotten.
        let async_to_any = matches!(h.ty, Ty::Async(_)) && matches!(ty.non_null(), Ty::Any);
        if self.types.assignable(&h.ty, ty) && !async_to_any {
            return true;
        }
        let found = self.show(&h.ty);
        let want = self.show(ty);
        match &h.ty {
            Ty::Optional(inner) if self.types.assignable(inner, ty) => {
                self.error(
                    "check::nullable",
                    format!("{what} expects `{want}`, but this may be null"),
                    h.span,
                    format!("this is `{found}`"),
                )
                .help = Some("give a fallback: `… ?? default`".into());
            }
            Ty::Async(inner) if async_to_any || self.types.assignable(inner, ty) => {
                let message = if async_to_any {
                    format!("{what} cannot take a value that may still be loading")
                } else {
                    format!("{what} expects `{want}`, but this may still be loading")
                };
                self.error(
                    "check::async",
                    message,
                    h.span,
                    format!("this is `{found}`"),
                )
                .help = Some("give a value for while it loads or fails: `… ?? fallback`".into());
            }
            Ty::Prim(p) if p.is_scalar() && matches!(ty, Ty::Prim(Prim::Duration)) => {
                self.error(
                    "check::type_mismatch",
                    format!("{what} expects a duration, which needs a unit"),
                    h.span,
                    "a plain number",
                )
                .help = Some("write `6s` or `200ms`".into());
            }
            _ => {
                let help = (accepts_int(ty) && h.ty == Ty::FLOAT).then(|| self.int_help(h));
                self.error(
                    "check::type_mismatch",
                    format!("{what} expects `{want}`, found `{found}`"),
                    h.span,
                    format!("this is `{found}`"),
                )
                .help = help;
            }
        }
        false
    }

    /// Help for a `float` where an `int` is expected.
    fn int_help(&self, h: &hir::Expr) -> String {
        if let ExprKind::Def(d) = h.kind {
            let def = &self.defs[d.0 as usize];
            let kw = match def.kind {
                DefKind::State => Some("state"),
                DefKind::Let => Some("let"),
                _ => None,
            };
            if let Some(kw) = kw {
                let n = &def.name;
                return if self.float_pins.contains(&(def.file, def.span)) {
                    format!(
                        "a fraction is written to `{n}`, so it is a `float`; round it here: `{n}.round`"
                    )
                } else {
                    format!("declare it `{kw} {n}: int = …`, or round it here: `{n}.round`")
                };
            }
        }
        "round it: `(…).round`, `.floor` or `.ceil`".into()
    }

    fn join_branches(&mut self, a: &hir::Expr, b: &hir::Expr, what: &str) -> Ty {
        match self.types.join(&a.ty, &b.ty) {
            Some(t) => t,
            None => {
                let (sa, sb) = (self.show(&a.ty), self.show(&b.ty));
                let file = self.file();
                self.error(
                    "check::type_mismatch",
                    format!("the branches of {what} differ: `{sa}` and `{sb}`"),
                    b.span,
                    format!("this is `{sb}`"),
                )
                .add_secondary(file, a.span, format!("this is `{sa}`"));
                Ty::Error
            }
        }
    }

    // -----------------------------------------------------------------------
    // Names

    /// The variant `name` of the enum `expected` asks for, if any.
    fn expected_variant(
        &self,
        name: &str,
        expected: Option<&Ty>,
    ) -> Option<(crate::ty::EnumId, u32)> {
        let mut found = None;
        let mut visit = |t: &Ty| {
            if let Ty::Enum(e) = t
                && found.is_none()
                && let Some(v) = self.types.enum_(*e).variant(name)
            {
                found = Some((*e, v));
            }
        };
        match expected {
            Some(Ty::Union(ts)) => ts.iter().for_each(|t| visit(t.non_null())),
            Some(t) => visit(t.non_null()),
            None => {}
        }
        found
    }

    /// The only user enum that has variant `name` (`state kind = volume`).
    /// Builtin enums' variants need a position that expects them.
    fn unique_variant(&self, name: &str) -> Result<Option<(crate::ty::EnumId, u32)>, Vec<String>> {
        let pool: Vec<_> = self
            .types
            .enums
            .iter()
            .enumerate()
            .skip(self.schema.types.enums.len())
            .filter(|(_, e)| matches!(e.origin, crate::ty::Origin::User(..)))
            .filter_map(|(i, e)| e.variant(name).map(|v| (crate::ty::EnumId(i as u32), v)))
            .collect();
        match pool.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(*one)),
            many => Err(many
                .iter()
                .map(|(e, _)| self.types.enum_(*e).name.clone())
                .collect()),
        }
    }

    pub fn name_expr(&mut self, id: &ast::Ident, expected: Option<&Ty>) -> hir::Expr {
        let span = id.span;
        let name = id.name.as_str();
        // Read before anything else is checked (a pending declaration
        // forced below has names of its own).
        let field_base = std::mem::take(&mut self.field_base);
        let mk = |kind, ty| hir::Expr { kind, ty, span };
        let variant = self.expected_variant(name, expected);
        let value = self.lookup_scope(name).or_else(|| {
            self.file_scopes[self.module]
                .get(name)
                .map(|&d| Binding::Def(d))
        });
        if let (Some((e, _)), Some(b)) = (variant, value) {
            // `state top = 5` and `edge: top`: never pick one silently.
            let (file, decl) = match b {
                Binding::Local(l) => (
                    self.locals[l.0 as usize].file,
                    self.locals[l.0 as usize].span,
                ),
                Binding::Def(d) => (self.defs[d.0 as usize].file, self.defs[d.0 as usize].span),
            };
            let en = self.types.enum_(e).name.clone();
            self.error(
                "check::ambiguous",
                format!("`{name}` is both a name in scope and a variant of `{en}`"),
                span,
                "which one?",
            )
            .add_secondary(file, decl, format!("the name `{name}`"))
            .help = Some(format!(
                "write `{en}.{name}` for the variant, or rename the `{name}` declared here"
            ));
            return hir::Expr::error(span);
        }
        if let Some(b) = self.lookup_scope(name) {
            return self.binding_expr(b, span);
        }
        if let Some((e, v)) = variant {
            self.add_ref(span, Target::Variant(e, v));
            return mk(ExprKind::Variant(e, v), Ty::Enum(e));
        }
        if NODE_BOOLS.contains(&name) || name == "self" {
            return self.node_expr(name, span);
        }
        if let Some(&d) = self.file_scopes[self.module].get(name) {
            return self.binding_expr(Binding::Def(d), span);
        }
        if let Some(&d) = self.globals.get(name) {
            let def = &self.defs[d.0 as usize];
            let what = match def.kind {
                DefKind::Component => Some("a component: use it as an element (`Name arg { … }`)"),
                DefKind::Surface(_) => Some("a surface, not a value"),
                DefKind::Type(_) => Some("a type: build one with `Name(field: value, …)`"),
                _ => None,
            };
            if let Some(what) = what {
                self.error(
                    "check::not_a_value",
                    format!("`{name}` is {what}"),
                    span,
                    "not a value",
                );
                return hir::Expr::error(span);
            }
            return self.binding_expr(Binding::Def(d), span);
        }
        if let Some(&rec) = self.schema.services.get(name) {
            self.add_ref(span, Target::Service(name.to_string()));
            return mk(ExprKind::Service(name.to_string()), Ty::Record(rec));
        }
        if let Some(t) = self.schema.values.get(name) {
            self.add_ref(span, Target::Builtin(name.to_string()));
            return mk(ExprKind::Value(name.to_string()), t.clone());
        }
        let fallback = if expected.is_none_or(|t| t.is_lenient()) {
            self.unique_variant(name)
        } else {
            Ok(None)
        };
        match fallback {
            Ok(Some((e, v))) => {
                self.add_ref(span, Target::Variant(e, v));
                return mk(ExprKind::Variant(e, v), Ty::Enum(e));
            }
            Ok(None) => {}
            Err(enums) => {
                self.error(
                    "check::ambiguous",
                    format!("`{name}` is a variant of several enums"),
                    span,
                    "which one?",
                )
                .help = Some(format!(
                    "write the enum: {}",
                    enums
                        .iter()
                        .map(|e| format!("`{e}.{name}`"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
                return hir::Expr::error(span);
            }
        }
        if let Some(&m) = self.file_index.get(name) {
            let file = self.modules[m].file;
            self.add_ref(span, Target::File(file));
            self.error(
                "check::not_a_value",
                format!("`{name}` is a file"),
                span,
                "not a value",
            )
            .help = Some(format!("read what it exports: `{name}.something`"));
            return hir::Expr::error(span);
        }
        self.unknown_name(id, expected, field_base);
        hir::Expr::error(span)
    }

    fn unknown_name(&mut self, id: &ast::Ident, expected: Option<&Ty>, field_base: bool) {
        let variant_help = self.builtin_variant_help(&id.name, expected);
        let label = match expected {
            Some(t @ Ty::Enum(_)) => format!("not a name, nor a variant of `{}`", self.show(t)),
            _ if variant_help.is_some() => "no enum expected here".to_string(),
            _ => "not found".to_string(),
        };
        let help = if variant_help.is_some() {
            variant_help
        } else if self.may_suggest() {
            self.name_suggestion(&id.name, expected, field_base)
        } else {
            None
        };
        self.error(
            "check::unknown_name",
            format!("unknown name `{}`", id.name),
            id.span,
            label,
        )
        .help = help;
    }

    /// A bare builtin variant where nothing expects its enum (`Side
    /// center` for an inferred parameter, `let a = center`): the enum
    /// must be written, or the position typed.
    fn builtin_variant_help(&self, name: &str, expected: Option<&Ty>) -> Option<String> {
        if expected.is_some_and(|t| !t.is_lenient()) {
            return None;
        }
        let enums: Vec<&str> = self.types.enums[..self.schema.types.enums.len()]
            .iter()
            .filter(|e| e.variant(name).is_some())
            .map(|e| e.name.as_str())
            .collect();
        let first = *enums.first()?;
        let written = enums
            .iter()
            .map(|e| format!("`{e}.{name}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        Some(match &self.infer_arg {
            Some(param) => format!(
                "`{name}` is a builtin variant, and the parameter it fills has no type to resolve it by: write {written}, or give the parameter a type (`{param}: {first}`)"
            ),
            None => format!(
                "`{name}` is a builtin variant, and nothing here expects its enum: write {written}"
            ),
        })
    }

    /// Did-you-mean for an unknown name: the variants of the enum the
    /// position expects come first (`edge: tp` → `top`), then every name
    /// in scope, then other files' exports (`dnd` → `toasts.dnd`).
    fn name_suggestion(
        &self,
        word: &str,
        expected: Option<&Ty>,
        field_base: bool,
    ) -> Option<String> {
        let mut variants: Vec<&str> = Vec::new();
        let mut add = |t: &Ty| {
            if let Ty::Enum(e) = t.non_null() {
                variants.extend(self.types.enum_(*e).variants.iter().map(String::as_str));
            }
        };
        match expected {
            Some(Ty::Union(ts)) => ts.iter().for_each(&mut add),
            Some(t) => add(t),
            None => {}
        }
        // `play shak`: the keyframes in the config.
        if expected.is_some_and(|t| *t == Ty::opaque("Keyframes")) {
            variants.extend(
                self.globals
                    .iter()
                    .filter(|(_, d)| self.defs[d.0 as usize].kind == DefKind::Keyframes)
                    .map(|(n, _)| n.as_str()),
            );
        }
        if let Some(v) = suggest(word, variants) {
            return Some(format!("did you mean `{v}`?"));
        }
        if let Some(v) = suggest(word, self.value_candidates(field_base)) {
            return Some(format!("did you mean `{v}`?"));
        }
        if field_base {
            return None;
        }
        self.export_suggestion(word)
    }

    pub fn binding_expr(&mut self, b: Binding, span: Span) -> hir::Expr {
        match b {
            Binding::Local(l) => {
                self.add_ref(span, Target::Local(l));
                let local = &self.locals[l.0 as usize];
                if self.untyped.contains(&l) && self.reported.insert((local.file, local.span)) {
                    let (name, lspan, lfile) = (local.name.clone(), local.span, local.file);
                    self.error(
                        "check::needs_type",
                        format!("parameter `{name}` needs a type"),
                        span,
                        "its type is needed here",
                    )
                    .add_secondary(lfile, lspan, "declared without a type")
                    .help = Some(format!(
                        "nothing passes it a value, so its type cannot come from a caller; write the type it takes, such as `{name}: Notification` or `{name}: float`"
                    ));
                }
                let local = &self.locals[l.0 as usize];
                let kind = match local.kind {
                    LocalKind::NodeId(n) => ExprKind::Node(n),
                    _ => ExprKind::Local(l),
                };
                hir::Expr {
                    kind,
                    ty: local.ty.clone(),
                    span,
                }
            }
            Binding::Def(d) => {
                self.add_ref(span, Target::Def(d));
                let ty = match self.defs[d.0 as usize].kind {
                    DefKind::Tokens => Ty::opaque("TokenSet"),
                    DefKind::Keyframes => Ty::opaque("Keyframes"),
                    _ => self.def_ty(d, span),
                };
                hir::Expr {
                    kind: ExprKind::Def(d),
                    ty,
                    span,
                }
            }
        }
    }

    fn node_record(&self) -> Ty {
        self.types.find_record("Node").map_or(Ty::Error, Ty::Record)
    }

    fn node_expr(&mut self, name: &str, span: Span) -> hir::Expr {
        let Some(node) = self.nodes.last() else {
            self.error(
                "check::no_node",
                format!("`{name}` belongs to an element, and there is none here"),
                span,
                "not inside an element",
            )
            .help = Some(format!(
                "use `{name}` inside the element it describes, or give that element an `id:` and read `id.{}`",
                if name == "self" { "width" } else { name }
            ));
            return hir::Expr::error(span);
        };
        let base = hir::Expr {
            kind: ExprKind::Node(node.idx),
            ty: self.node_record(),
            span,
        };
        if name == "self" {
            return base;
        }
        if let Ty::Record(r) = base.ty {
            self.add_ref(span, Target::Field(r, name.to_string()));
        }
        hir::Expr {
            kind: ExprKind::Field {
                base: Box::new(base),
                name: name.to_string(),
                optional: false,
            },
            ty: Ty::BOOL,
            span,
        }
    }

    // -----------------------------------------------------------------------
    // Fields

    fn field_expr(
        &mut self,
        base: &'a ast::Expr,
        name: &ast::Ident,
        optional: bool,
        span: Span,
    ) -> hir::Expr {
        if let ast::ExprKind::Name(b) = &base.kind
            && self.lookup_scope(&b.name).is_none()
            && !self.file_scopes[self.module].contains_key(&b.name)
        {
            // `Look.dark`.
            if let Some(&d) = self.globals.get(&b.name)
                && let DefKind::Enum(e) = self.defs[d.0 as usize].kind
            {
                self.add_ref(b.span, Target::Def(d));
                return self.variant_of(e, name, span);
            }
            if let Some(e) = self.schema.types.find_enum(&b.name)
                && !self.globals.contains_key(&b.name)
            {
                return self.variant_of(e, name, span);
            }
            // `theme.look`: another file's export.
            if !self.globals.contains_key(&b.name)
                && !self.schema.services.contains_key(&b.name)
                && !self.schema.values.contains_key(&b.name)
                && let Some(&m) = self.file_index.get(&b.name)
            {
                return self.export_expr(m, b, name, span);
            }
        }
        // Only `name_expr` reads (and clears) it.
        self.field_base = matches!(base.kind, ast::ExprKind::Name(_));
        let hb = self.expr(base, None);
        self.member(hb, base, name, optional, span)
    }

    fn variant_of(&mut self, e: crate::ty::EnumId, name: &ast::Ident, span: Span) -> hir::Expr {
        match self.types.enum_(e).variant(&name.name) {
            Some(v) => {
                self.add_ref(name.span, Target::Variant(e, v));
                hir::Expr {
                    kind: ExprKind::Variant(e, v),
                    ty: Ty::Enum(e),
                    span,
                }
            }
            None => {
                let def = self.types.enum_(e);
                let help = Self::did_you_mean(&name.name, &def.variants);
                let en = def.name.clone();
                self.error(
                    "check::unknown_field",
                    format!("`{en}` has no variant `{}`", name.name),
                    name.span,
                    "not a variant",
                )
                .help = help;
                hir::Expr::error(span)
            }
        }
    }

    fn export_expr(
        &mut self,
        m: usize,
        file: &ast::Ident,
        name: &ast::Ident,
        span: Span,
    ) -> hir::Expr {
        let fid = self.modules[m].file;
        self.add_ref(file.span, Target::File(fid));
        match self.file_scopes[m].get(&name.name).copied() {
            Some(d) if self.defs[d.0 as usize].exported || m == self.module => {
                self.add_ref(name.span, Target::Def(d));
                let ty = self.def_ty(d, name.span);
                hir::Expr {
                    kind: ExprKind::Def(d),
                    ty,
                    span,
                }
            }
            Some(d) => {
                let (df, ds) = (self.defs[d.0 as usize].file, self.defs[d.0 as usize].span);
                self.error(
                    "check::not_exported",
                    format!("`{}` is not exported from `{}`", name.name, file.name),
                    name.span,
                    "private to its file",
                )
                .add_secondary(df, ds, "declared here")
                .help = Some(format!(
                    "add `export` to its declaration: `export state {} = …`",
                    name.name
                ));
                hir::Expr::error(span)
            }
            None => {
                let candidates: Vec<String> = self.file_scopes[m]
                    .iter()
                    .filter(|(_, d)| self.defs[d.0 as usize].exported)
                    .map(|(n, _)| n.clone())
                    .collect();
                let help = Self::did_you_mean(&name.name, &candidates);
                self.error(
                    "check::unknown_field",
                    format!("`{}` exports no `{}`", file.name, name.name),
                    name.span,
                    "not exported there",
                )
                .help = help;
                hir::Expr::error(span)
            }
        }
    }

    pub(crate) fn nullable_use(&mut self, h: &hir::Expr, base: &ast::Expr, what: &str) {
        let shown = self.show(&h.ty);
        let help = match (what, path_text(base)) {
            ("field", Some(p)) => {
                format!("use `?.` to read through null: `{p}?.…`, then `?? default`")
            }
            _ => "give a fallback first: `(… ?? default)`".to_string(),
        };
        self.error(
            "check::nullable",
            format!("{} may be null", quoted(base)),
            base.span,
            format!("this is `{shown}`"),
        )
        .help = Some(help);
    }

    fn async_use(&mut self, h: &hir::Expr, base: &ast::Expr) {
        let shown = self.show(&h.ty);
        self.error(
            "check::async",
            format!("{} may still be loading", quoted(base)),
            base.span,
            format!("this is `{shown}`"),
        )
        .help =
            Some("read `.value` (null while loading) or give a fallback: `… ?? fallback`".into());
    }

    /// Checks a `let`'s value. A raw colour in it that the `let` hands on
    /// as a colour (`let c = #ff0000`, then `bg: c`) bypasses the theme
    /// as a prop's would, so it gets the same lint; one inside
    /// `material(seed: …)` does not.
    pub(crate) fn let_value(&mut self, value: &'a ast::Expr, expected: Option<&Ty>) -> hir::Expr {
        let mark = self.let_colours.len();
        let saved = self.ctx.let_value;
        self.ctx.let_value = true;
        let h = self.expr(value, expected);
        self.ctx.let_value = saved;
        let found = self.let_colours.split_off(mark.min(self.let_colours.len()));
        let ty = expected.unwrap_or(&h.ty);
        if colour_like(ty) {
            for span in found {
                self.warning(
                    "check::raw_color",
                    "raw colour in a `let`",
                    span,
                    "this colour ignores the theme",
                )
                .help = Some(
                    "use a token such as `$accent` or `$fg.muted`, so theme swaps reach it".into(),
                );
            }
        }
        h
    }

    /// `base.name` once `base` is checked.
    fn member(
        &mut self,
        base: hir::Expr,
        base_ast: &ast::Expr,
        name: &ast::Ident,
        optional: bool,
        span: Span,
    ) -> hir::Expr {
        let (ty, nullable) = match base.ty.clone() {
            Ty::Error | Ty::Any => (Ty::Error, false),
            Ty::Null => {
                self.nullable_use(&base, base_ast, "field");
                (Ty::Error, false)
            }
            Ty::Optional(inner) => {
                if !optional {
                    self.nullable_use(&base, base_ast, "field");
                }
                (self.field_of(&inner, base_ast, name, &base), optional)
            }
            Ty::Async(inner) => match name.name.as_str() {
                "pending" => (Ty::BOOL, false),
                "error" => (Ty::TEXT.optional(), false),
                "value" => ((*inner).clone().optional(), false),
                // `hits.len`: how many the last result had (0 before the
                // first). Element reads (`.first`, `.last`) would forget
                // the loading state, so they need `?? fallback` first.
                "len" if inner.list_elem().is_some() => {
                    (self.field_of(&inner, base_ast, name, &base), false)
                }
                _ => {
                    self.async_use(&base, base_ast);
                    (Ty::Error, false)
                }
            },
            t => (self.field_of(&t, base_ast, name, &base), false),
        };
        let ty = if nullable { ty.optional() } else { ty };
        hir::Expr {
            kind: ExprKind::Field {
                base: Box::new(base),
                name: name.name.clone(),
                optional,
            },
            ty,
            span,
        }
    }

    /// The type of field `name` of a value of type `t`.
    fn field_of(
        &mut self,
        t: &Ty,
        base_ast: &ast::Expr,
        name: &ast::Ident,
        base: &hir::Expr,
    ) -> Ty {
        let n = name.name.as_str();
        match t {
            Ty::Error | Ty::Any => Ty::Error,
            Ty::List(elem, _) => match n {
                "len" => Ty::INT,
                "first" | "last" => (**elem).clone().optional(),
                _ => {
                    let candidates = ["len", "first", "last"].map(String::from);
                    let is_method = LIST_METHODS.contains(&n);
                    self.no_field(t, base_ast, name, &candidates, is_method);
                    Ty::Error
                }
            },
            Ty::Prim(Prim::Text | Prim::Path) if n == "len" => Ty::INT,
            Ty::Record(r) => {
                let rec = self.types.record(*r);
                if let Some(f) = rec.field(n) {
                    let ty = f.ty.clone();
                    self.add_ref(name.span, Target::Field(*r, n.to_string()));
                    ty
                } else {
                    let is_method = rec.method(n).is_some();
                    let candidates: Vec<String> = rec.member_names().map(String::from).collect();
                    self.no_field(t, base_ast, name, &candidates, is_method);
                    Ty::Error
                }
            }
            _ => {
                let methods: Vec<String> =
                    self.methods_for(t).iter().map(|m| m.name.clone()).collect();
                let is_method = methods.iter().any(|m| m == n);
                let _ = base;
                self.no_field(t, base_ast, name, &methods, is_method);
                Ty::Error
            }
        }
    }

    fn no_field(
        &mut self,
        t: &Ty,
        base: &ast::Expr,
        name: &ast::Ident,
        candidates: &[String],
        is_method: bool,
    ) {
        let owner =
            path_text(base).map_or_else(|| format!("`{}`", self.show(t)), |p| format!("`{p}`"));
        if is_method {
            self.error(
                "check::not_a_value",
                format!("`{}` is a method", name.name),
                name.span,
                "call it",
            )
            .help = Some(format!("write `{}(…)`", name.name));
            return;
        }
        let help = Self::did_you_mean(&name.name, candidates);
        self.error(
            "check::unknown_field",
            format!("{owner} has no field `{}`", name.name),
            name.span,
            "unknown field",
        )
        .help = help;
    }

    /// Methods the schema gives a builtin type.
    fn methods_for(&self, t: &Ty) -> Vec<crate::ty::MethodDef> {
        match t {
            Ty::Prim(Prim::Int) => {
                let mut v = self.schema.methods_of("int").to_vec();
                v.extend(self.schema.methods_of("float").iter().cloned());
                v
            }
            Ty::Prim(p) => self.schema.methods_of(p.name()).to_vec(),
            Ty::Opaque(n) => self.schema.methods_of(n).to_vec(),
            _ => Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Calls

    fn call_expr(&mut self, callee: &'a ast::Expr, args: &'a [ast::Arg], span: Span) -> hir::Expr {
        match &callee.kind {
            ast::ExprKind::Name(id) => self.call_name(id, args, span),
            ast::ExprKind::Field {
                base,
                name,
                optional,
            } => {
                let recv = self.expr(base, None);
                self.method_call(recv, base, name, *optional, args, span)
            }
            _ => {
                let f = self.expr(callee, None);
                self.call_value(f, args, span)
            }
        }
    }

    fn call_value(&mut self, f: hir::Expr, args: &'a [ast::Arg], span: Span) -> hir::Expr {
        match f.ty.clone() {
            Ty::Fn(sig) => {
                let (args, ret) = self.call_args(&sig, args, "the function", span);
                hir::Expr {
                    kind: ExprKind::Call {
                        callee: Callee::Value(Box::new(f)),
                        args,
                    },
                    ty: ret,
                    span,
                }
            }
            Ty::Error | Ty::Any => {
                for a in args {
                    self.expr(&a.value, None);
                }
                hir::Expr::error(span)
            }
            t => {
                let shown = self.show(&t);
                self.error(
                    "check::type_mismatch",
                    format!("`{shown}` is not a function"),
                    f.span,
                    "cannot be called",
                );
                hir::Expr::error(span)
            }
        }
    }

    fn call_name(&mut self, id: &ast::Ident, args: &'a [ast::Arg], span: Span) -> hir::Expr {
        let name = id.name.as_str();
        if let Some(b) = self.lookup_scope(name) {
            let f = self.binding_expr(b, id.span);
            return self.call_value(f, args, span);
        }
        if let Some(&d) = self.globals.get(name) {
            match self.defs[d.0 as usize].kind.clone() {
                DefKind::Fn => {
                    self.add_ref(id.span, Target::Def(d));
                    let ty = self.def_ty(d, id.span);
                    if self.ctx.pure_fn && self.stack.contains(&d) && ty.is_error() {
                        return hir::Expr::error(span);
                    }
                    let Ty::Fn(sig) = ty else {
                        for a in args {
                            self.expr(&a.value, None);
                        }
                        return hir::Expr::error(span);
                    };
                    let (args, ret) = self.call_args(&sig, args, &format!("`{name}`"), span);
                    return hir::Expr {
                        kind: ExprKind::Call {
                            callee: Callee::Fn(d),
                            args,
                        },
                        ty: ret,
                        span,
                    };
                }
                DefKind::Type(r) => {
                    self.add_ref(id.span, Target::Def(d));
                    return self.construct(r, args, span);
                }
                DefKind::Component => {
                    self.error(
                        "check::not_a_value",
                        format!("`{name}` is a component, not a function"),
                        id.span,
                        "components are elements",
                    )
                    .help = Some(format!("write it as an element: `{name} arg {{ … }}`"));
                    return hir::Expr::error(span);
                }
                _ => {}
            }
        }
        if let Some(sigs) = self.schema.functions.get(name) {
            self.add_ref(id.span, Target::Builtin(name.to_string()));
            let sigs = sigs.clone();
            let reported = sigs.iter().any(|s| s.action) && self.action_check(name, id.span);
            // `material(seed: #7aa2f7)`: a seed is where a raw colour
            // belongs.
            let saved = self.ctx.let_value;
            if name == "material" {
                self.ctx.let_value = false;
            }
            let (overload, args, ret) = self.call_overloads(&sigs, args, name, span);
            self.ctx.let_value = saved;
            let ret = if reported { Ty::Error } else { ret };
            return hir::Expr {
                kind: ExprKind::Call {
                    callee: Callee::Builtin {
                        name: name.to_string(),
                        overload,
                    },
                    args,
                },
                ty: ret,
                span,
            };
        }
        if let Some(r) = self.schema.types.find_record(name)
            && !self.schema.services.contains_key(name)
        {
            return self.construct(r, args, span);
        }
        // Not a function: maybe a value of function type in file scope.
        if let Some(&d) = self.file_scopes[self.module].get(name) {
            let f = self.binding_expr(Binding::Def(d), id.span);
            return self.call_value(f, args, span);
        }
        let mut candidates: Vec<String> = self.schema.functions.keys().cloned().collect();
        candidates.extend(
            self.globals
                .iter()
                .filter(|(_, d)| {
                    matches!(self.defs[d.0 as usize].kind, DefKind::Fn | DefKind::Type(_))
                })
                .map(|(n, _)| n.clone()),
        );
        let help = Self::did_you_mean(name, &candidates);
        self.error(
            "check::unknown_name",
            format!("unknown function `{name}`"),
            id.span,
            "not found",
        )
        .help = help;
        for a in args {
            self.expr(&a.value, None);
        }
        hir::Expr::error(span)
    }

    /// `Pin(app: a, label: "x")`.
    fn construct(&mut self, r: RecordId, args: &'a [ast::Arg], span: Span) -> hir::Expr {
        let rec = self.types.record(r);
        let sig = FnSig::new(
            rec.fields
                .iter()
                .map(|f| ParamSig {
                    name: f.name.clone(),
                    ty: f.ty.clone(),
                    has_default: f.ty.is_optional(),
                    default: None,
                    variadic: false,
                })
                .collect(),
            Ty::Record(r),
        );
        let name = rec.name.clone();
        let (args, ty) = self.call_args(&sig, args, &format!("`{name}`"), span);
        hir::Expr {
            kind: ExprKind::Call {
                callee: Callee::Record(r),
                args,
            },
            ty,
            span,
        }
    }

    /// Actions change the world: handlers only.
    /// Reports an action called outside a handler; true if it did, so the
    /// caller types the call as already reported.
    fn action_check(&mut self, what: &str, span: Span) -> bool {
        if self.ctx.pure_fn {
            self.error(
                "check::impure",
                format!("`{what}` is an action, and fns are pure"),
                span,
                "changes something",
            )
            .help = Some("call it from a handler instead".into());
        } else if !self.ctx.handler {
            self.error(
                "check::action_in_binding",
                format!("`{what}` is an action, so it runs in a handler"),
                span,
                "changes something",
            )
            .help = Some(format!(
                "call it from a handler: `on click {{ {what}(…) }}`"
            ));
        } else {
            return false;
        }
        true
    }

    /// Picks an overload and checks the arguments against it.
    ///
    /// The overload is chosen by the call's shape first (named
    /// parameters, a `from` argument, how many positional arguments, which
    /// required parameters are filled), so `material(seed: …)` and
    /// `material(image: …)` check their arguments once. Only overloads the
    /// shape cannot tell apart (`radial(center, 40%)` against
    /// `radial(#000, #fff)`) are tried in turn; a declaration first read
    /// inside such an attempt keeps its own diagnostics (see
    /// [`Checker::force`]), and nested attempts are bounded
    /// ([`MAX_SPECULATION`]), so nested overloaded calls cost linear time.
    pub(crate) fn call_overloads(
        &mut self,
        sigs: &[Arc<FnSig>],
        args: &'a [ast::Arg],
        name: &str,
        span: Span,
    ) -> (usize, Vec<CallArg>, Ty) {
        let what = format!("`{name}`");
        let fits: Vec<usize> = (0..sigs.len())
            .filter(|&i| shape_fits(&sigs[i], args))
            .collect();
        // No overload fits the shape: the one with fewest errors explains
        // the mistake best.
        let tried: Vec<usize> = if fits.is_empty() {
            (0..sigs.len()).collect()
        } else {
            fits
        };
        if tried.len() == 1 || self.speculating >= MAX_SPECULATION {
            let i = tried[0];
            let (a, r) = self.call_args(&sigs[i], args, &what, span);
            return (i, a, r);
        }
        let mut best: Option<(usize, usize)> = None;
        for &i in &tried {
            let mark = (self.diags.len(), self.refs.len());
            let reported = self.reported.clone();
            self.speculating += 1;
            let (a, r) = self.call_args(&sigs[i], args, &what, span);
            self.speculating -= 1;
            let errors = self.diags[mark.0..].iter().filter(|d| d.is_error()).count();
            if errors == 0 {
                self.release_kept();
                return (i, a, r);
            }
            self.diags.truncate(mark.0);
            self.refs.truncate(mark.1);
            self.reported = reported;
            if best.is_none_or(|(_, e)| errors < e) {
                best = Some((i, errors));
            }
        }
        let i = best.map_or(tried[0], |(i, _)| i);
        let (a, r) = self.call_args(&sigs[i], args, &what, span);
        self.release_kept();
        (i, a, r)
    }

    /// Matches arguments to parameters and checks them. Returns the
    /// arguments and the call's type.
    pub(crate) fn call_args(
        &mut self,
        sig: &FnSig,
        args: &'a [ast::Arg],
        what: &str,
        span: Span,
    ) -> (Vec<CallArg>, Ty) {
        let mut filled = vec![false; sig.params.len()];
        let mut out = Vec::new();
        let mut lifted = false;
        // `oklch(from c, l: l + 0.1)`: the channels are names in the
        // other arguments.
        let channels = args.iter().any(|a| matches!(a.kind, ArgKind::From(_)));
        if channels {
            self.push_scope();
            for (n, t) in [
                ("l", Ty::FLOAT),
                ("c", Ty::FLOAT),
                ("h", Ty::ANGLE),
                ("alpha", Ty::FLOAT),
            ] {
                self.bind_local(n, t, span, LocalKind::Channel);
            }
        }
        let mut next_pos = 0usize;
        for a in args {
            let idx = match &a.kind {
                ArgKind::Named(n) => match sig
                    .params
                    .iter()
                    .position(|p| p.name == n.name && !p.variadic)
                {
                    Some(i) => Some(i),
                    None => {
                        let names: Vec<String> =
                            sig.params.iter().map(|p| p.name.clone()).collect();
                        let help = Self::did_you_mean(&n.name, &names).or_else(|| {
                            (!names.is_empty()).then(|| format!("it takes {}", list_names(&names)))
                        });
                        self.error(
                            "check::unknown_param",
                            format!("{what} has no parameter `{}`", n.name),
                            n.span,
                            "unknown parameter",
                        )
                        .help = help;
                        None
                    }
                },
                ArgKind::From(_) => match sig.params.iter().position(|p| p.name == "from") {
                    Some(i) => Some(i),
                    None => {
                        self.error(
                            "check::unknown_param",
                            format!("{what} takes no `from`"),
                            a.span,
                            "unknown parameter",
                        );
                        None
                    }
                },
                ArgKind::Positional => {
                    while next_pos < sig.params.len()
                        && (filled[next_pos] && !sig.params[next_pos].variadic)
                    {
                        next_pos += 1;
                    }
                    if next_pos < sig.params.len() {
                        Some(next_pos)
                    } else {
                        self.error(
                            "check::too_many_args",
                            format!(
                                "{what} takes {} argument{}",
                                sig.params.len(),
                                if sig.params.len() == 1 { "" } else { "s" }
                            ),
                            a.span,
                            "one too many",
                        );
                        None
                    }
                }
            };
            let Some(i) = idx else {
                let v = self.expr(&a.value, None);
                out.push(CallArg {
                    param: None,
                    value: v,
                    span: a.span,
                });
                continue;
            };
            let p = &sig.params[i];
            if filled[i] && !p.variadic {
                self.error(
                    "check::duplicate_arg",
                    format!("`{}` is given twice", p.name),
                    a.span,
                    "given again here",
                );
            }
            filled[i] = true;
            let want = p.ty.clone();
            let v = if channels && !matches!(a.kind, ArgKind::From(_)) || !channels {
                self.expr(&a.value, Some(&want))
            } else {
                // The `from` colour itself does not see the channels.
                let saved = self.scopes.pop();
                let v = self.expr(&a.value, Some(&want));
                if let Some(s) = saved {
                    self.scopes.push(s);
                }
                v
            };
            let arg_what = format!("`{}` of {what}", p.name);
            if sig.lift && v.ty.is_optional() && !want.is_optional() {
                lifted = true;
                let inner = hir::Expr {
                    ty: v.ty.non_null().clone(),
                    ..v.clone()
                };
                self.require(&inner, &want, &arg_what);
            } else {
                self.require(&v, &want, &arg_what);
            }
            out.push(CallArg {
                param: Some(i),
                value: v,
                span: a.span,
            });
        }
        if channels {
            self.pop_scope();
        }
        let missing: Vec<&str> = sig
            .params
            .iter()
            .zip(&filled)
            .filter(|(p, f)| !**f && !p.has_default && !p.variadic)
            .map(|(p, _)| p.name.as_str())
            .collect();
        if !missing.is_empty() {
            self.error(
                "check::missing_arg",
                format!(
                    "{what} needs {}",
                    list_names(&missing.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                ),
                span,
                "missing argument",
            );
        }
        let mut ret = sig.ret.clone();
        // `map(n => …)` returns what the lambda returns: the signature
        // says `any`, the argument knows.
        if lifted {
            ret = ret.optional();
        }
        (out, ret)
    }

    fn method_call(
        &mut self,
        recv: hir::Expr,
        recv_ast: &'a ast::Expr,
        name: &ast::Ident,
        optional: bool,
        args: &'a [ast::Arg],
        span: Span,
    ) -> hir::Expr {
        let (ty, nullable) = match recv.ty.clone() {
            Ty::Error | Ty::Any => {
                for a in args {
                    self.expr(&a.value, None);
                }
                return hir::Expr::error(span);
            }
            Ty::Optional(inner) => {
                if !optional {
                    self.nullable_use(&recv, recv_ast, "field");
                }
                ((*inner).clone(), optional)
            }
            // `hits.take(3)`, `hits.filter(…)`: a transform of a loading
            // list is still loading (`Async<[U]>`, `.pending` kept).
            Ty::Async(inner)
                if inner.list_elem().is_some()
                    && ASYNC_TRANSFORMS.contains(&name.name.as_str()) =>
            {
                let list = (*inner).clone();
                let Ty::List(elem, keyed) = &list else {
                    return hir::Expr::error(span);
                };
                let (args, ret) = self.list_method(&recv, recv_ast, elem, *keyed, name, args, span);
                let ret = if ret.is_error() {
                    ret
                } else {
                    Ty::Async(Box::new(ret))
                };
                return hir::Expr {
                    kind: ExprKind::Call {
                        callee: Callee::Method {
                            receiver: Box::new(recv),
                            name: name.name.clone(),
                            overload: 0,
                        },
                        args,
                    },
                    ty: ret,
                    span,
                };
            }
            Ty::Async(_) => {
                self.async_use(&recv, recv_ast);
                for a in args {
                    self.expr(&a.value, None);
                }
                return hir::Expr::error(span);
            }
            t => (t, false),
        };
        let n = name.name.as_str();
        let wrap = |kind, ty: Ty| hir::Expr {
            kind,
            ty: if nullable { ty.optional() } else { ty },
            span,
        };
        if let Ty::List(elem, keyed) = &ty {
            let (args, ret) = self.list_method(&recv, recv_ast, elem, *keyed, name, args, span);
            return wrap(
                ExprKind::Call {
                    callee: Callee::Method {
                        receiver: Box::new(recv),
                        name: n.to_string(),
                        overload: 0,
                    },
                    args,
                },
                ret,
            );
        }
        let methods = match &ty {
            Ty::Record(r) => {
                let rec = self.types.record(*r);
                if let Some(m) = rec.method(n) {
                    let sigs = m.sigs.clone();
                    self.add_ref(name.span, Target::Field(*r, n.to_string()));
                    Some(sigs)
                } else if let Some(Ty::Fn(sig)) = rec.field(n).map(|f| f.ty.clone()) {
                    Some(vec![sig])
                } else {
                    let candidates: Vec<String> = rec.member_names().map(String::from).collect();
                    let owner = path_text(recv_ast).unwrap_or_else(|| rec.name.clone());
                    let help = Self::did_you_mean(n, &candidates);
                    self.error(
                        "check::unknown_field",
                        format!("`{owner}` has no method `{n}`"),
                        name.span,
                        "unknown method",
                    )
                    .help = help;
                    None
                }
            }
            t => {
                let methods = self.methods_for(t);
                match methods.iter().find(|m| m.name == n) {
                    Some(m) => Some(m.sigs.clone()),
                    None => {
                        let candidates: Vec<String> =
                            methods.iter().map(|m| m.name.clone()).collect();
                        let shown = self.show(t);
                        let help = Self::did_you_mean(n, &candidates);
                        self.error(
                            "check::unknown_field",
                            format!("`{shown}` has no method `{n}`"),
                            name.span,
                            "unknown method",
                        )
                        .help = help;
                        None
                    }
                }
            }
        };
        let Some(sigs) = methods else {
            for a in args {
                self.expr(&a.value, None);
            }
            return hir::Expr::error(span);
        };
        let label = path_text(recv_ast).map_or_else(|| n.to_string(), |p| format!("{p}.{n}"));
        let reported = sigs.iter().any(|s| s.action) && self.action_check(&label, name.span);
        let (overload, args, ret) = self.call_overloads(&sigs, args, &label, span);
        let ret = if reported { Ty::Error } else { ret };
        wrap(
            ExprKind::Call {
                callee: Callee::Method {
                    receiver: Box::new(recv),
                    name: n.to_string(),
                    overload,
                },
                args,
            },
            ret,
        )
    }

    /// The key type of a keyed list value.
    fn key_ty(&mut self, recv: &hir::Expr, elem: &Ty) -> Option<Ty> {
        if let ExprKind::Def(d) = recv.kind
            && let Some(path) = self.state_keys.get(&d).cloned()
        {
            return match elem {
                Ty::Record(r) => self.types.field_path(*r, &path),
                _ => Some(elem.clone()),
            };
        }
        if let Ty::Record(r) = elem
            && let Some(path) = self.types.record(*r).key.clone()
        {
            return self.types.field_path(*r, &path);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn list_method(
        &mut self,
        recv: &hir::Expr,
        recv_ast: &ast::Expr,
        elem: &Ty,
        keyed: bool,
        name: &ast::Ident,
        args: &'a [ast::Arg],
        span: Span,
    ) -> (Vec<CallArg>, Ty) {
        let t = elem.clone();
        let list = Ty::List(Box::new(t.clone()), keyed);
        let p = |n: &str, ty: Ty| ParamSig {
            name: n.into(),
            ty,
            has_default: false,
            default: None,
            variadic: false,
        };
        let pred = Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], Ty::BOOL)));
        let any_fn = Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], Ty::Any)));
        let n = name.name.as_str();
        let mutation = matches!(
            n,
            "push" | "insert" | "remove" | "clear" | "remove_key" | "move" | "update"
        );
        let needs_key = matches!(n, "remove_key" | "move" | "update");
        let key = if needs_key {
            match self.key_ty(recv, elem) {
                Some(k) => k,
                None => {
                    let owner = quoted(recv_ast);
                    self.error(
                        "check::missing_key",
                        format!("`{n}` needs a keyed collection, and {owner} has no key"),
                        name.span,
                        "no key to find items by",
                    )
                    .help = Some("declare one: `state xs: [T] key field = []`".into());
                    Ty::Error
                }
            }
        } else {
            Ty::Error
        };
        let sig = match n {
            "filter" => FnSig::new(vec![p("keep", pred)], list.clone()),
            "map" => FnSig::new(vec![p("f", any_fn.clone())], list.clone()),
            "sort_by" => FnSig::new(vec![p("by", any_fn)], list.clone()),
            "take" | "skip" => FnSig::new(vec![p("count", Ty::INT)], list.clone()),
            "reverse" => FnSig::new(vec![], list.clone()),
            "join" => FnSig::new(vec![p("sep", Ty::TEXT)], Ty::TEXT),
            "contains" => FnSig::new(vec![p("item", t.clone())], Ty::BOOL),
            "any" | "all" => FnSig::new(vec![p("test", pred)], Ty::BOOL),
            "count" => FnSig::new(vec![p("test", pred)], Ty::INT),
            "find" => FnSig::new(vec![p("test", pred)], t.clone().optional()),
            "push" => FnSig::new(vec![p("item", t.clone())], Ty::Unit),
            "insert" => FnSig::new(vec![p("at", Ty::INT), p("item", t.clone())], Ty::Unit),
            "remove" => FnSig::new(vec![p("at", Ty::INT)], Ty::Unit),
            "clear" => FnSig::new(vec![], Ty::Unit),
            "remove_key" => FnSig::new(vec![p("key", key)], Ty::Unit),
            "move" => FnSig::new(vec![p("key", key), p("to", Ty::INT)], Ty::Unit),
            "update" => FnSig::new(
                vec![
                    p("key", key),
                    p(
                        "f",
                        Ty::Fn(Arc::new(FnSig::positional(vec![t.clone()], t.clone()))),
                    ),
                ],
                Ty::Unit,
            ),
            _ => {
                let candidates: Vec<String> = LIST_METHODS.iter().map(|s| s.to_string()).collect();
                let help = Self::did_you_mean(n, &candidates);
                self.error(
                    "check::unknown_field",
                    format!("lists have no method `{n}`"),
                    name.span,
                    "unknown method",
                )
                .help = help;
                for a in args {
                    self.expr(&a.value, None);
                }
                return (Vec::new(), Ty::Error);
            }
        };
        let label = path_text(recv_ast).map_or_else(|| n.to_string(), |p| format!("{p}.{n}"));
        if mutation
            && !self.action_check(&label, name.span)
            && let Err(why) = self.writable(recv)
        {
            self.not_writable(why, recv.span, "change");
        }
        if n == "join" && !self.types.assignable(&t, &Ty::TEXT) {
            let shown = self.show(&t);
            self.error(
                "check::type_mismatch",
                format!("`join` needs a list of `text`, found `[{shown}]`"),
                recv.span,
                "not text",
            )
            .help = Some("turn the items into text first: `.map(x => …)`".into());
        }
        let (out, ret) = self.call_args(&sig, args, &format!("`{label}`"), span);
        let ret = if n == "map" {
            let produced = out
                .first()
                .and_then(|a| match &a.value.ty {
                    Ty::Fn(s) => Some(s.ret.clone()),
                    _ => None,
                })
                .unwrap_or(Ty::Error);
            Ty::List(Box::new(produced), keyed)
        } else {
            ret
        };
        (out, ret)
    }

    // -----------------------------------------------------------------------
    // Operators

    fn unary(
        &mut self,
        op: UnaryOp,
        e: &'a ast::Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        let (inner, ty) = match op {
            UnaryOp::Not => {
                let h = self.expect(e, &Ty::BOOL, "`!`");
                (h, Ty::BOOL)
            }
            UnaryOp::Neg => {
                let h = self.expr(e, expected);
                let ty = match &h.ty {
                    t if t.is_numeric() || t.is_lenient() => t.clone(),
                    t => {
                        let shown = self.show(t);
                        self.error(
                            "check::type_mismatch",
                            format!("`-` needs a number, found `{shown}`"),
                            h.span,
                            format!("this is `{shown}`"),
                        );
                        Ty::Error
                    }
                };
                (h, ty)
            }
            UnaryOp::Await => {
                if !self.ctx.handler || self.ctx.pure_fn {
                    self.error(
                        "check::await_outside_handler",
                        "`await` only works in handlers",
                        span,
                        "not in a handler",
                    )
                    .help = Some(
                        "bindings never wait: bind the `Async` value and read `?? fallback`".into(),
                    );
                }
                let h = self.expr(e, None);
                let ty = match &h.ty {
                    Ty::Async(t) => (**t).clone(),
                    t if t.is_lenient() => Ty::Error,
                    t => {
                        let shown = self.show(t);
                        self.error(
                            "check::type_mismatch",
                            format!("`await` needs an `Async` value, found `{shown}`"),
                            h.span,
                            "nothing to wait for",
                        );
                        Ty::Error
                    }
                };
                (h, ty)
            }
        };
        hir::Expr {
            kind: ExprKind::Unary {
                op,
                expr: Box::new(inner),
            },
            ty,
            span,
        }
    }

    /// A bare name nothing declares (a variant waiting for its type).
    /// `my-bar.open` with neither `my` nor `bar` known: a name written
    /// in kebab case, read as a subtraction. One error for the whole
    /// name rather than one per half.
    fn kebab_name(&mut self, lhs: &ast::Expr, rhs: &ast::Expr, span: Span) -> Option<hir::Expr> {
        let ast::ExprKind::Name(a) = &lhs.kind else {
            return None;
        };
        let mut root = rhs;
        while let ast::ExprKind::Field { base, .. } = &root.kind {
            root = base;
        }
        let ast::ExprKind::Name(b) = &root.kind else {
            return None;
        };
        if a.span.end + 1 != b.span.start
            || !self.is_unbound_name(lhs)
            || !self.is_unbound_name(root)
        {
            return None;
        }
        let written = format!("{}-{}", a.name, b.name);
        let snake = format!("{}_{}", a.name, b.name);
        let help = if self.file_index.contains_key(&written) {
            format!("rename `{written}.strand` to `{snake}.strand` and read `{snake}.…`")
        } else {
            format!("names are snake_case: `{snake}`")
        };
        self.error(
            "check::unknown_name",
            format!("unknown name `{written}`"),
            Span::new(a.span.start, b.span.end),
            format!("read as `{} - {}`", a.name, b.name),
        )
        .help = Some(help);
        Some(hir::Expr::error(span))
    }

    fn is_unbound_name(&self, e: &ast::Expr) -> bool {
        match &e.kind {
            ast::ExprKind::Name(i) => {
                self.lookup_scope(&i.name).is_none()
                    && !self.file_scopes[self.module].contains_key(&i.name)
                    && !self.globals.contains_key(&i.name)
                    && !self.schema.services.contains_key(&i.name)
                    && !self.schema.values.contains_key(&i.name)
                    && !(NODE_BOOLS.contains(&i.name.as_str()) && !self.nodes.is_empty())
            }
            _ => false,
        }
    }

    fn operand_ok(&mut self, h: &hir::Expr, ast: &ast::Expr) -> bool {
        match &h.ty {
            Ty::Optional(_) | Ty::Null => {
                self.nullable_use(h, ast, "operand");
                false
            }
            Ty::Async(_) => {
                self.async_use(h, ast);
                false
            }
            _ => true,
        }
    }

    fn binary(
        &mut self,
        op: BinaryOp,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        if op == BinaryOp::Sub
            && let (ast::ExprKind::Token(key), ast::ExprKind::Name(name)) = (&lhs.kind, &rhs.kind)
            && let Some(e) = self.kebab_token(key, name, span)
        {
            return e;
        }
        if op == BinaryOp::Sub
            && let Some(e) = self.kebab_name(lhs, rhs, span)
        {
            return e;
        }
        let (l, r, ty) = match op {
            BinaryOp::Coalesce => self.coalesce(lhs, rhs, expected),
            BinaryOp::And | BinaryOp::Or => self.logic(op, lhs, rhs),
            BinaryOp::Eq | BinaryOp::Ne => self.equality(lhs, rhs, span),
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                self.comparison(op, lhs, rhs, span)
            }
            _ => self.arithmetic(op, lhs, rhs, expected, span),
        };
        hir::Expr {
            kind: ExprKind::Binary {
                op,
                lhs: Box::new(l),
                rhs: Box::new(r),
            },
            ty,
            span,
        }
    }

    #[inline(never)]
    fn coalesce(
        &mut self,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
        expected: Option<&Ty>,
    ) -> (hir::Expr, hir::Expr, Ty) {
        let l = self.expr(lhs, expected.map(|t| t.clone().optional()).as_ref());
        let inner = match &l.ty {
            Ty::Optional(t) | Ty::Async(t) => (**t).clone(),
            Ty::Null => expected.cloned().unwrap_or(Ty::Error),
            t => t.clone(),
        };
        let hint = if inner.is_lenient() {
            expected.cloned()
        } else {
            Some(inner.clone())
        };
        let r = self.expr(rhs, hint.as_ref());
        let ty = match self.types.join(&inner, &r.ty) {
            Some(t) => t,
            None => {
                let (a, b) = (self.show(&inner), self.show(&r.ty));
                self.error(
                    "check::type_mismatch",
                    format!("the fallback is `{b}`, but the value is `{a}`"),
                    r.span,
                    format!("this is `{b}`"),
                );
                Ty::Error
            }
        };
        (l, r, ty)
    }

    #[inline(never)]
    fn logic(
        &mut self,
        op: BinaryOp,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
    ) -> (hir::Expr, hir::Expr, Ty) {
        let what = format!("`{}`", op.as_str());
        let l = self.expect(lhs, &Ty::BOOL, &what);
        let r = self.expect(rhs, &Ty::BOOL, &what);
        (l, r, Ty::BOOL)
    }

    #[inline(never)]
    fn equality(
        &mut self,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
        span: Span,
    ) -> (hir::Expr, hir::Expr, Ty) {
        let (l, r) = if self.is_unbound_name(lhs) && !self.is_unbound_name(rhs) {
            let r = self.expr(rhs, None);
            let l = self.expr(lhs, Some(&r.ty.non_null().clone()));
            (l, r)
        } else {
            let l = self.expr(lhs, None);
            let r = self.expr(rhs, Some(&l.ty.non_null().clone()));
            (l, r)
        };
        let comparable = self.types.assignable(&l.ty, &r.ty)
            || self.types.assignable(&r.ty, &l.ty)
            || (l.ty.non_null().is_numeric() && r.ty.non_null().is_numeric());
        if !comparable {
            let (a, b) = (self.show(&l.ty), self.show(&r.ty));
            self.error(
                "check::type_mismatch",
                format!("cannot compare `{a}` with `{b}`"),
                span,
                "never equal",
            );
        }
        (l, r, Ty::BOOL)
    }

    #[inline(never)]
    fn comparison(
        &mut self,
        op: BinaryOp,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
        span: Span,
    ) -> (hir::Expr, hir::Expr, Ty) {
        let l = self.expr(lhs, None);
        let r = self.expr(rhs, Some(&l.ty.clone()));
        if self.operand_ok(&l, lhs) && self.operand_ok(&r, rhs) {
            self.arith(op, &l, &r, span);
        }
        (l, r, Ty::BOOL)
    }

    #[inline(never)]
    fn arithmetic(
        &mut self,
        op: BinaryOp,
        lhs: &'a ast::Expr,
        rhs: &'a ast::Expr,
        expected: Option<&Ty>,
        span: Span,
    ) -> (hir::Expr, hir::Expr, Ty) {
        let l = self.expr(lhs, expected);
        let hint = expected.cloned().unwrap_or_else(|| l.ty.clone());
        let r = self.expr(rhs, Some(&hint));
        let ty = if self.operand_ok(&l, lhs) && self.operand_ok(&r, rhs) {
            self.arith(op, &l, &r, span)
        } else {
            Ty::Error
        };
        (l, r, ty)
    }

    /// The type of arithmetic on numbers with units (comparisons give the
    /// operands' common type).
    fn arith(&mut self, op: BinaryOp, l: &hir::Expr, r: &hir::Expr, span: Span) -> Ty {
        use Prim::*;
        let (a, b) = match (l.ty.prim(), r.ty.prim()) {
            _ if l.ty.is_lenient() || r.ty.is_lenient() => return Ty::Error,
            (Some(a), Some(b)) if a.is_numeric() && b.is_numeric() => (a, b),
            _ => {
                let (sa, sb) = (self.show(&l.ty), self.show(&r.ty));
                let help = (op == BinaryOp::Add && l.ty == Ty::TEXT)
                    .then(|| "join text with `join(\"\", a, b)`".to_string());
                self.error(
                    "check::type_mismatch",
                    format!("`{}` needs numbers, found `{sa}` and `{sb}`", op.as_str()),
                    span,
                    "not numbers",
                )
                .help = help;
                return Ty::Error;
            }
        };
        let scalar = |p: Prim| p.is_scalar();
        let res = match op {
            BinaryOp::Mul => match (a, b) {
                (Int, Int) => Some(Int),
                (x, y) if scalar(x) && scalar(y) => Some(Float),
                (x, d) | (d, x) if scalar(x) => Some(d),
                _ => None,
            },
            BinaryOp::Div => match (a, b) {
                (x, y) if scalar(x) && scalar(y) => Some(Float),
                (d, x) if scalar(x) => Some(d),
                (x, y) if x == y => Some(Float),
                _ => None,
            },
            _ => match (a, b) {
                (Int, Int) => Some(Int),
                (x, y) if scalar(x) && scalar(y) => Some(Float),
                (x, y) if x == y => Some(x),
                // Plain numbers are pixels and degrees.
                (x @ (Length | Angle | Percent), y) | (y, x @ (Length | Angle | Percent))
                    if scalar(y) =>
                {
                    Some(x)
                }
                (Length, Percent) | (Percent, Length) => Some(Length),
                _ => None,
            },
        };
        match res {
            Some(p) => Ty::Prim(p),
            None => {
                let help = (a == Duration || b == Duration)
                    .then(|| "durations need units: `1s + 200ms`".to_string());
                self.error(
                    "check::type_mismatch",
                    format!(
                        "cannot apply `{}` to `{}` and `{}`",
                        op.as_str(),
                        a.name(),
                        b.name()
                    ),
                    span,
                    "units do not combine",
                )
                .help = help;
                Ty::Error
            }
        }
    }

    // -----------------------------------------------------------------------
    // Lists, lambdas, match

    fn array(&mut self, items: &'a [ast::Expr], expected: Option<&Ty>, span: Span) -> hir::Expr {
        let want = expected.and_then(|t| match t.non_null() {
            Ty::List(e, _) => Some((**e).clone()),
            _ => None,
        });
        let mut parts = Vec::new();
        let mut ty: Option<Ty> = want.clone();
        for i in items {
            let h = match &want {
                Some(w) => self.expect(i, w, "a list item"),
                None => self.expr(i, ty.as_ref()),
            };
            if want.is_none() {
                ty = match ty.take() {
                    None => Some(h.ty.clone()),
                    Some(prev) => Some(match self.types.join(&prev, &h.ty) {
                        Some(t) => t,
                        None => {
                            let (a, b) = (self.show(&prev), self.show(&h.ty));
                            self.error(
                                "check::type_mismatch",
                                format!("list items differ: `{a}` and `{b}`"),
                                h.span,
                                format!("this is `{b}`"),
                            );
                            Ty::Error
                        }
                    }),
                };
            }
            parts.push(h);
        }
        let elem = ty.unwrap_or(Ty::Error);
        hir::Expr {
            kind: ExprKind::List(parts),
            ty: Ty::list(elem),
            span,
        }
    }

    fn lambda(
        &mut self,
        params: &'a [ast::Param],
        body: &'a ast::LambdaBody,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        let sig = match expected.map(Ty::non_null) {
            Some(Ty::Fn(s)) => Some(s.clone()),
            _ => None,
        };
        self.push_scope();
        let mut locals = Vec::new();
        let mut tys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let from_sig = sig
                .as_ref()
                .and_then(|s| s.params.get(i))
                .map(|p| p.ty.clone());
            let ty = match (&p.ty, from_sig) {
                (Some(t), want) => {
                    let t = self.resolve_type(t);
                    if let Some(w) = want
                        && !self.types.assignable(&w, &t)
                    {
                        let (a, b) = (self.show(&w), self.show(&t));
                        self.error(
                            "check::type_mismatch",
                            format!("this parameter receives `{a}`, not `{b}`"),
                            p.span,
                            format!("declared `{b}`"),
                        );
                    }
                    t
                }
                (None, Some(w)) => w,
                (None, None) => {
                    self.error(
                        "check::needs_type",
                        format!("cannot tell the type of `{}`", p.name.name),
                        p.name.span,
                        "add `: T`",
                    )
                    .help = Some(format!("write `({}: T) => …`", p.name.name));
                    Ty::Error
                }
            };
            tys.push(ty.clone());
            locals.push(self.bind_local(&p.name.name, ty, p.name.span, LocalKind::LambdaParam));
        }
        if let Some(s) = &sig
            && params.len() > s.params.len()
        {
            self.error(
                "check::too_many_args",
                format!(
                    "this function receives {} argument{}",
                    s.params.len(),
                    if s.params.len() == 1 { "" } else { "s" }
                ),
                span,
                "too many parameters",
            );
        }
        let want_ret = sig.as_ref().map(|s| s.ret.clone());
        let (hbody, ret) = match body {
            ast::LambdaBody::Expr(e) => {
                let h = match &want_ret {
                    Some(r) if !matches!(r, Ty::Any | Ty::Unit) => {
                        self.expect(e, r, "the function's result")
                    }
                    _ => self.expr(e, None),
                };
                let ty = h.ty.clone();
                (hir::LambdaBody::Expr(Box::new(h)), ty)
            }
            ast::LambdaBody::Block(b) => {
                let stmts = self.stmts(&b.items);
                let ty = match stmts.last().map(|s| &s.kind) {
                    Some(hir::StmtKind::Expr(e)) => e.ty.clone(),
                    _ => Ty::Unit,
                };
                (hir::LambdaBody::Block(stmts), ty)
            }
        };
        self.pop_scope();
        hir::Expr {
            kind: ExprKind::Lambda {
                params: locals,
                body: hbody,
            },
            ty: Ty::Fn(Arc::new(FnSig::positional(tys, ret))),
            span,
        }
    }

    fn match_expr(
        &mut self,
        m: &'a ast::Match<ast::Expr>,
        expected: Option<&Ty>,
        span: Span,
    ) -> hir::Expr {
        let s = self.expr(&m.scrutinee, None);
        let mut arms = Vec::new();
        let mut ty: Option<Ty> = None;
        let mut patterns = Vec::new();
        for arm in &m.arms {
            let p = self.pattern(&arm.pattern, &s.ty);
            patterns.push((p.clone(), arm.pattern.span));
            let hint = expected.cloned().or_else(|| ty.clone());
            let b = self.expr(&arm.body, hint.as_ref());
            ty = Some(match ty {
                None => b.ty.clone(),
                Some(prev) => match self.types.join(&prev, &b.ty) {
                    Some(t) => t,
                    None => {
                        let (a, c) = (self.show(&prev), self.show(&b.ty));
                        self.error(
                            "check::type_mismatch",
                            format!("the arms of `match` differ: `{a}` and `{c}`"),
                            b.span,
                            format!("this is `{c}`"),
                        );
                        Ty::Error
                    }
                },
            });
            arms.push((p, b));
        }
        self.exhaustive(&s.ty, &patterns, m.arms_span);
        hir::Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(s),
                arms,
            },
            ty: ty.unwrap_or(Ty::Error),
            span,
        }
    }

    pub(crate) fn pattern(&mut self, p: &'a ast::Pattern, scrutinee: &Ty) -> hir::Pattern {
        match &p.kind {
            ast::PatternKind::Wildcard => hir::Pattern::Wildcard,
            ast::PatternKind::Error => hir::Pattern::Error,
            ast::PatternKind::Literal(e) => {
                let h = self.expect(e, scrutinee, "this pattern");
                hir::Pattern::Literal(h)
            }
            ast::PatternKind::Path(segs) => {
                let target = scrutinee.non_null();
                let (enum_id, vname) = match segs.as_slice() {
                    [one] => match target {
                        Ty::Enum(e) => (Some(*e), one),
                        t if t.is_lenient() => return hir::Pattern::Error,
                        t => {
                            let shown = self.show(t);
                            self.error(
                                "check::type_mismatch",
                                format!("`{}` is not a value of `{shown}`", one.name),
                                one.span,
                                "only enums match by name",
                            )
                            .help = Some("match a literal, or `_`".into());
                            return hir::Pattern::Error;
                        }
                    },
                    [en, v] => {
                        let e = self
                            .globals
                            .get(&en.name)
                            .and_then(|d| match self.defs[d.0 as usize].kind {
                                DefKind::Enum(e) => Some(e),
                                _ => None,
                            })
                            .or_else(|| self.schema.types.find_enum(&en.name));
                        if e.is_none() {
                            self.error(
                                "check::unknown_type",
                                format!("unknown enum `{}`", en.name),
                                en.span,
                                "not an enum",
                            );
                            return hir::Pattern::Error;
                        }
                        (e, v)
                    }
                    _ => {
                        self.error(
                            "check::unknown_name",
                            "a pattern is a variant (`dark`, `Look.dark`), a literal or `_`",
                            p.span,
                            "not a pattern",
                        );
                        return hir::Pattern::Error;
                    }
                };
                let Some(e) = enum_id else {
                    return hir::Pattern::Error;
                };
                if let Ty::Enum(want) = target
                    && *want != e
                {
                    let (a, b) = (self.types.enum_(e).name.clone(), self.show(target));
                    self.error(
                        "check::type_mismatch",
                        format!("this matches `{b}`, not `{a}`"),
                        p.span,
                        "wrong enum",
                    );
                    return hir::Pattern::Error;
                }
                match self.types.enum_(e).variant(&vname.name) {
                    Some(v) => {
                        self.add_ref(vname.span, Target::Variant(e, v));
                        hir::Pattern::Variant(e, v)
                    }
                    None => {
                        let def = self.types.enum_(e);
                        let help = Self::did_you_mean(&vname.name, &def.variants);
                        let en = def.name.clone();
                        self.error(
                            "check::unknown_name",
                            format!("`{en}` has no variant `{}`", vname.name),
                            vname.span,
                            "not a variant",
                        )
                        .help = help;
                        hir::Pattern::Error
                    }
                }
            }
        }
    }

    /// Reports the variants a `match` misses, and arms no value can reach
    /// (a variant or literal matched twice, anything after `_`).
    pub(crate) fn exhaustive(
        &mut self,
        scrutinee: &Ty,
        patterns: &[(hir::Pattern, Span)],
        span: Span,
    ) {
        self.unreachable_arms(patterns);
        if patterns
            .iter()
            .any(|(p, _)| matches!(p, hir::Pattern::Wildcard | hir::Pattern::Error))
        {
            return;
        }
        let missing: Vec<String> = match scrutinee {
            Ty::Enum(e) => self
                .types
                .enum_(*e)
                .variants
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    !patterns
                        .iter()
                        .any(|(p, _)| matches!(p, hir::Pattern::Variant(pe, v) if pe == e && *v as usize == *i))
                })
                .map(|(_, v)| v.clone())
                .collect(),
            Ty::Prim(Prim::Bool) => ["true", "false"]
                .into_iter()
                .filter(|b| {
                    !patterns.iter().any(|(p, _)| {
                        matches!(p, hir::Pattern::Literal(hir::Expr { kind: ExprKind::Bool(x), .. }) if x.to_string() == *b)
                    })
                })
                .map(String::from)
                .collect(),
            t if t.is_lenient() => Vec::new(),
            _ => vec!["_".to_string()],
        };
        if !missing.is_empty() {
            self.error(
                "check::non_exhaustive",
                format!(
                    "`match` misses {}",
                    missing
                        .iter()
                        .map(|m| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                span,
                "not every value has an arm",
            )
            .help = Some("add the missing arms, or `_ => …`".into());
        }
    }

    fn unreachable_arms(&mut self, patterns: &[(hir::Pattern, Span)]) {
        let key = |p: &hir::Pattern| -> Option<String> {
            match p {
                hir::Pattern::Variant(e, v) => Some(format!("{}.{v}", e.0)),
                hir::Pattern::Literal(h) => match &h.kind {
                    ExprKind::Bool(b) => Some(format!("b{b}")),
                    ExprKind::Text(t) => Some(format!("t{t}")),
                    ExprKind::Number { value, unit } => Some(format!("n{value}{unit:?}")),
                    _ => None,
                },
                hir::Pattern::Wildcard | hir::Pattern::Error => None,
            }
        };
        let mut seen: Vec<(String, Span)> = Vec::new();
        let mut wildcard: Option<Span> = None;
        let file = self.file();
        for (p, span) in patterns {
            if let Some(w) = wildcard {
                self.error(
                    "check::unreachable_arm",
                    "this arm can never match",
                    *span,
                    "after `_`",
                )
                .add_secondary(file, w, "`_` matches everything first")
                .help = Some("move `_` to the last arm, or remove this one".into());
                continue;
            }
            if matches!(p, hir::Pattern::Wildcard) {
                wildcard = Some(*span);
                continue;
            }
            let Some(k) = key(p) else { continue };
            if let Some((_, first)) = seen.iter().find(|(s, _)| *s == k) {
                let first = *first;
                self.error(
                    "check::unreachable_arm",
                    "this arm can never match",
                    *span,
                    "matched by an earlier arm",
                )
                .add_secondary(file, first, "already matched here")
                .help = Some("remove the repeated arm, or merge their bodies".into());
            } else {
                seen.push((k, *span));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Places

    /// Whether `e` can be assigned (or bound with `<->`).
    pub(crate) fn writable(&self, e: &hir::Expr) -> Result<(), NotWritable> {
        self.writable_place(e, true)
    }

    /// `whole`: `e` is the whole target, not the base of a field or index
    /// in it. A settings record is written field by field only: what a
    /// whole-record write would mean against its TOML file (overlay, file
    /// and defaults merge per field) is undefined.
    fn writable_place(&self, e: &hir::Expr, whole: bool) -> Result<(), NotWritable> {
        match &e.kind {
            ExprKind::Def(d) => {
                let def = &self.defs[d.0 as usize];
                match def.kind {
                    DefKind::State => Ok(()),
                    DefKind::Settings if whole => {
                        let field = match &def.ty {
                            Ty::Record(r) => self.types.record(*r).fields.first(),
                            _ => None,
                        };
                        Err(NotWritable::SettingsRecord {
                            name: def.name.clone(),
                            field: field.map_or_else(|| "field".into(), |f| f.name.clone()),
                        })
                    }
                    DefKind::Settings => Ok(()),
                    DefKind::Let => Err(NotWritable::Let(def.name.clone())),
                    _ => Err(NotWritable::Other),
                }
            }
            ExprKind::Local(l) => {
                let local = &self.locals[l.0 as usize];
                match local.kind {
                    LocalKind::Param => Err(NotWritable::Param(local.name.clone())),
                    LocalKind::Let => Err(NotWritable::Let(local.name.clone())),
                    _ => Err(NotWritable::Local(local.name.clone())),
                }
            }
            ExprKind::Field { base, name, .. } => {
                if base.ty.is_error() {
                    // Already reported.
                    return Ok(());
                }
                if let ExprKind::Node(_) = base.kind {
                    return Err(NotWritable::BoundProp(name.clone()));
                }
                let rec = match base.ty.non_null() {
                    Ty::Record(r) => Some(self.types.record(*r)),
                    _ => None,
                };
                match rec.and_then(|r| r.field(name).map(|f| (r, f))) {
                    Some((_, f)) if f.rw => Ok(()),
                    Some((r, _)) if matches!(r.origin, crate::ty::Origin::User(..)) => {
                        // A plain record: writable through the state
                        // holding it.
                        self.writable_place(base, false)
                    }
                    Some((r, _)) => Err(NotWritable::ReadOnlyField {
                        owner: r.name.clone(),
                        field: name.clone(),
                    }),
                    // An unknown field, already reported.
                    None if e.ty.is_error() => Ok(()),
                    None => Err(NotWritable::Other),
                }
            }
            ExprKind::Index { base, .. } => self.writable_place(base, false),
            ExprKind::Node(_) => Err(NotWritable::BoundProp("self".into())),
            ExprKind::Error => Ok(()),
            _ => Err(NotWritable::Other),
        }
    }

    /// Reports a write to something that cannot be written. `verb` is
    /// "assign" or "bind".
    pub(crate) fn not_writable(&mut self, why: NotWritable, span: Span, verb: &str) {
        let (code, msg, label, help) = match why {
            NotWritable::Let(n) => (
                "check::assign_to_let",
                format!("cannot {verb} `{n}`: it is a `let`"),
                "derived, read-only".to_string(),
                format!("a `let` follows its expression; to {verb} it, make it `state {n} = …`"),
            ),
            NotWritable::Param(n) => (
                "check::assign_to_prop",
                format!("cannot {verb} `{n}`: it is a parameter, bound by the caller"),
                "a bound prop".to_string(),
                format!(
                    "assigning would cut the binding; keep a `state` here and {verb} that, or change what the caller passes"
                ),
            ),
            NotWritable::BoundProp(n) if n == "self" => (
                "check::assign_to_prop",
                format!("cannot {verb} `self`: it is the element"),
                "the element itself".to_string(),
                "bind a prop to a `state` (`opacity: v`) and assign the state instead".to_string(),
            ),
            NotWritable::BoundProp(n) => (
                "check::assign_to_prop",
                format!("cannot {verb} `{n}`: props are bindings"),
                "a bound prop".to_string(),
                format!("bind the prop to a `state` (`{n}: v`) and assign the state instead"),
            ),
            NotWritable::SettingsRecord { name, field } => (
                "check::read_only",
                format!("cannot {verb} the whole settings record `{name}`"),
                "a settings file".to_string(),
                format!("{verb} a field: `{name}.{field} = …`"),
            ),
            NotWritable::ReadOnlyField { owner, field } => (
                "check::read_only",
                format!("cannot {verb} `{field}`: `{owner}` does not mark it `rw`"),
                "read-only".to_string(),
                "only `state`, settings fields and `rw` service fields can be written".to_string(),
            ),
            NotWritable::Local(n) => (
                "check::read_only",
                format!("cannot {verb} `{n}`"),
                "read-only".to_string(),
                "only `state`, settings fields and `rw` service fields can be written".to_string(),
            ),
            NotWritable::Other => (
                "check::read_only",
                format!("cannot {verb} this"),
                "not a writable place".to_string(),
                "only `state`, settings fields and `rw` service fields can be written".to_string(),
            ),
        };
        self.error(code, msg, span, label).help = Some(help);
    }

    /// Whether `e` reads anything that can change (for `on change`).
    pub(crate) fn is_reactive(&self, e: &hir::Expr) -> bool {
        use ExprKind as K;
        match &e.kind {
            K::Def(d) => match self.defs[d.0 as usize].kind {
                DefKind::State | DefKind::Settings | DefKind::Service(_) => true,
                // A `let` changes when what it reads does (checked
                // before it is read, so this does not recurse).
                DefKind::Let => !self.constant_lets.contains(d),
                DefKind::Component
                | DefKind::Surface(_)
                | DefKind::Fn
                | DefKind::Enum(_)
                | DefKind::Type(_)
                | DefKind::Tokens
                | DefKind::Keyframes => false,
            },
            K::Service(_) | K::Value(_) | K::Node(_) | K::Token(_) | K::Error => true,
            K::Local(l) => !matches!(self.locals[l.0 as usize].kind, LocalKind::Channel),
            K::Number { .. } | K::Text(_) | K::Color(_) | K::Bool(_) | K::Null => false,
            K::Variant(..) | K::EnumType(_) => false,
            K::Field { base, .. } => self.is_reactive(base),
            K::Index { base, index } => self.is_reactive(base) || self.is_reactive(index),
            K::Call { callee, args } => {
                let c = match callee {
                    Callee::Method { receiver, .. } => self.is_reactive(receiver),
                    Callee::Value(f) => self.is_reactive(f),
                    _ => false,
                };
                c || args.iter().any(|a| self.is_reactive(&a.value))
            }
            K::Unary { expr, .. } => self.is_reactive(expr),
            K::Binary { lhs, rhs, .. } => self.is_reactive(lhs) || self.is_reactive(rhs),
            K::Ternary { cond, then, else_ } => {
                self.is_reactive(cond) || self.is_reactive(then) || self.is_reactive(else_)
            }
            K::Match { scrutinee, arms } => {
                self.is_reactive(scrutinee) || arms.iter().any(|(_, a)| self.is_reactive(a))
            }
            K::List(items) | K::Commas(items) | K::Spaced(items) => {
                items.iter().any(|i| self.is_reactive(i))
            }
            K::Lambda { .. } => false,
        }
    }
}

/// Methods every list has.
const LIST_METHODS: &[&str] = &[
    "filter",
    "map",
    "sort_by",
    "take",
    "skip",
    "reverse",
    "join",
    "contains",
    "any",
    "all",
    "count",
    "find",
    "push",
    "insert",
    "remove",
    "clear",
    "remove_key",
    "move",
    "update",
];

/// A colour, paint, or a list or nullable of them.
fn colour_like(t: &Ty) -> bool {
    match t {
        Ty::Prim(Prim::Color | Prim::Paint) => true,
        Ty::Optional(t) | Ty::List(t, _) => colour_like(t),
        Ty::Tuple(ts) | Ty::Union(ts) => ts.iter().any(colour_like),
        _ => false,
    }
}

/// List methods that keep an `Async` list loading: they transform the
/// last result, and the transform is `Async` too.
const ASYNC_TRANSFORMS: &[&str] = &["filter", "map", "sort_by", "take", "skip", "reverse"];

/// How deep overload attempts nest before the first overload that fits
/// the call's shape is taken without trying the others.
pub(crate) const MAX_SPECULATION: u32 = 2;

/// Whether a call's arguments fit an overload's parameters by shape alone:
/// every named argument names a parameter, a `from` argument is given
/// exactly when the overload takes one, the positional arguments fit and
/// every required parameter is filled.
fn shape_fits(sig: &FnSig, args: &[ast::Arg]) -> bool {
    let mut filled = vec![false; sig.params.len()];
    let has_from = sig.params.iter().any(|p| p.name == "from");
    let mut from = false;
    let mut positional = 0usize;
    for a in args {
        match &a.kind {
            ArgKind::Named(n) => {
                match sig
                    .params
                    .iter()
                    .position(|p| p.name == n.name && !p.variadic)
                {
                    Some(i) if !filled[i] => filled[i] = true,
                    _ => return false,
                }
            }
            ArgKind::From(_) => from = true,
            ArgKind::Positional => positional += 1,
        }
    }
    if from != has_from {
        return false;
    }
    if from && let Some(i) = sig.params.iter().position(|p| p.name == "from") {
        filled[i] = true;
    }
    for (i, p) in sig.params.iter().enumerate() {
        if positional == 0 {
            break;
        }
        if p.variadic {
            positional = 0;
            filled[i] = true;
        } else if !filled[i] {
            filled[i] = true;
            positional -= 1;
        }
    }
    positional == 0
        && sig
            .params
            .iter()
            .zip(&filled)
            .all(|(p, f)| *f || p.has_default || p.variadic)
}

fn accepts_int(t: &Ty) -> bool {
    match t {
        Ty::Prim(Prim::Int) => true,
        Ty::Optional(t) => accepts_int(t),
        Ty::Union(ts) => ts.iter().any(accepts_int),
        _ => false,
    }
}

fn list_names(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}
