//! Whole-number `state`s that are visibly written a fraction, found from
//! the syntax before checking, so the usual case is checked in one pass
//! (see [`super::check`]).
//!
//! `state level = 0` is an `int` until a fraction is written to it; then
//! the program is checked again with it pinned to `float`. Most such
//! writes can be read off the source: `level = 0.5`, `level /= 2`,
//! `level += 0.1`, `level = level + dy * 0.05` (a fraction literal, or a
//! handler parameter or local that is a `float`, anywhere in the
//! arithmetic), or `value: <-> level` on a builtin element whose prop is
//! a `float` (a slider). Those states are pinned up front. A pin is only
//! taken when the write certainly reaches that state: the state is the
//! only declaration of its name in its file (counting every name an
//! element, `id:`, loop, parameter or local brings into scope), the write
//! is in the same file, and the target is written bare. Anything else (a
//! fraction that arrives through another state, a call or a lambda) is
//! still found by the checker and costs a further pass.

use std::collections::{HashMap, HashSet};

use super::Module;
use crate::schema::Schema;
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast::{self, ArmBody, AssignOp, BinaryOp, Else, ExprKind, ItemKind, StmtKind};
use crate::ty::Ty;

/// The `(file, name span)` of each whole-number `state` the source
/// visibly writes a fraction to.
pub(super) fn pre_pins(modules: &[Module<'_>], schema: &Schema) -> HashSet<(FileId, Span)> {
    let mut out = HashSet::new();
    for m in modules {
        let mut w = Walk {
            schema,
            declared: HashMap::new(),
            whole_states: HashMap::new(),
            written: Vec::new(),
            floats: HashSet::new(),
        };
        w.items(&m.ast.items, None);
        let unique = |n: &str| w.declared.get(n) == Some(&1);
        for (name, reads) in &w.written {
            // Every name the fraction is computed with must be a plain
            // number: a whole-number state (an `int` or a `float`).
            let plain = reads
                .iter()
                .all(|r| unique(r) && w.whole_states.contains_key(r));
            if plain
                && unique(name)
                && let Some(&span) = w.whole_states.get(name)
            {
                out.insert((m.file, span));
            }
        }
    }
    out
}

struct Walk<'a> {
    schema: &'a Schema,
    /// How many times each name is declared in the file (states, lets,
    /// parameters, loop and event bindings, `id:` names, the names an
    /// element brings into scope).
    declared: HashMap<&'a str, u32>,
    /// Untyped states initialised with a whole number.
    whole_states: HashMap<&'a str, Span>,
    /// Names written a fraction, with the other names the fraction is
    /// computed from (each must turn out to be a whole-number state).
    written: Vec<(&'a str, Vec<&'a str>)>,
    /// In the handler being walked: its parameters and locals that are
    /// certainly `float`s (`on scroll(dy)`, `let d: float = …`).
    floats: HashSet<&'a str>,
}

impl<'a> Walk<'a> {
    fn declare(&mut self, name: &'a str) {
        *self.declared.entry(name).or_default() += 1;
    }

    fn params(&mut self, params: Option<&'a Vec<ast::Param>>) {
        for p in params.into_iter().flatten() {
            self.declare(&p.name.name);
        }
    }

    /// Declares the names builtin element `kind` brings into scope
    /// (`letters`'s `index`, `bar`'s `screen`).
    fn element_scope(&mut self, kind: &str) {
        if let Some(e) = self.schema.element(kind) {
            for (n, _) in &e.scope {
                self.declare(n);
            }
        }
    }

    /// `element`: the builtin element kind the items are props of.
    fn items(&mut self, items: &'a [ast::Item], element: Option<&'a str>) {
        for i in items {
            self.item(i, element);
        }
    }

    fn item(&mut self, item: &'a ast::Item, element: Option<&'a str>) {
        match &item.kind {
            ItemKind::Component(c) => {
                self.declare(&c.name.name);
                self.params(c.params.as_ref());
                self.items(&c.body.items, None);
            }
            ItemKind::Surface(s) => {
                if let Some(n) = &s.name {
                    self.declare(&n.name);
                }
                let kind = s.kind.name.as_str();
                self.element_scope(kind);
                self.items(&s.body.items, Some(kind));
            }
            ItemKind::State(s) => {
                self.declare(&s.name.name);
                if let ast::StateInit::Value {
                    ty: None, value, ..
                } = &s.init
                    && super::is_whole_literal(value)
                {
                    self.whole_states.insert(s.name.name.as_str(), s.name.span);
                }
            }
            ItemKind::Let(l) => self.declare(&l.name.name),
            ItemKind::Fn(f) => self.declare(&f.name.name),
            ItemKind::Prop(p) => {
                if p.name.name == "id"
                    && let ExprKind::Name(n) = &p.value.kind
                {
                    self.declare(&n.name);
                }
                if p.two_way.is_some()
                    && let ExprKind::Name(target) = &p.value.kind
                    && element.is_some_and(|k| self.float_prop(k, &p.name.name))
                {
                    self.written.push((target.name.as_str(), Vec::new()));
                }
                if let Some(b) = &p.block {
                    self.items(&b.items, element);
                }
            }
            ItemKind::Element(e) => {
                let kind = e.kind.name.as_str();
                let builtin = self.schema.element(kind).is_some().then_some(kind);
                if let Some(ast::HeadArg::Named(n, v)) = &e.arg
                    && n.name == "id"
                    && let ExprKind::Name(id) = &v.kind
                {
                    self.declare(&id.name);
                }
                self.element_scope(kind);
                if let Some(b) = &e.block {
                    self.items(&b.items, builtin);
                }
            }
            ItemKind::When(w) => self.items(&w.body.items, element),
            ItemKind::If(f) => self.if_items(f, element),
            ItemKind::For(f) => {
                self.declare(&f.binding.name);
                self.items(&f.body.items, element);
            }
            ItemKind::Match(m) => {
                for a in &m.arms {
                    match &a.body {
                        ArmBody::Block(b) => self.items(&b.items, element),
                        ArmBody::Single(i) => self.item(i, element),
                    }
                }
            }
            ItemKind::On(o) => {
                self.floats.clear();
                if let ast::Event::Named { path, params } = &o.event {
                    self.params(params.as_ref());
                    self.event_floats(path, params.as_deref(), element);
                }
                self.stmts(&o.body.items);
                self.floats.clear();
            }
            ItemKind::Timer(t) => {
                self.floats.clear();
                self.stmts(&t.body.items);
            }
            ItemKind::Pose(p) => self.items(&p.body.items, element),
            _ => {}
        }
    }

    /// The parameters of `on event(…)` that are `float`s: written so, or
    /// untyped where the element's event gives a `float` (`on scroll(dy)`).
    fn event_floats(
        &mut self,
        path: &[ast::Ident],
        params: Option<&'a [ast::Param]>,
        element: Option<&'a str>,
    ) {
        let sig = match path {
            [one] => element
                .and_then(|k| self.schema.element(k))
                .and_then(|e| e.event(&one.name)),
            _ => None,
        };
        for (i, p) in params.into_iter().flatten().enumerate() {
            let float = match &p.ty {
                Some(t) => is_float_type(t),
                None => sig
                    .and_then(|s| s.params.get(i))
                    .is_some_and(|s| s.ty == Ty::FLOAT),
            };
            if float {
                self.floats.insert(p.name.name.as_str());
            }
        }
    }

    fn if_items(&mut self, f: &'a ast::If<ast::Item>, element: Option<&'a str>) {
        self.items(&f.then.items, element);
        match &f.else_ {
            Some(Else::If(i, _)) => self.if_items(i, element),
            Some(Else::Block(b)) => self.items(&b.items, element),
            None => {}
        }
    }

    fn stmts(&mut self, stmts: &'a [ast::Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &'a ast::Stmt) {
        match &s.kind {
            StmtKind::Let(l) => {
                let name = l.name.name.as_str();
                self.declare(name);
                let float = match &l.ty {
                    Some(t) => is_float_type(t),
                    // Only from fractions and handler floats alone.
                    None => self.fractional(&l.value).is_some_and(|r| r.is_empty()),
                };
                // A local shadows any earlier `float` of that name for the
                // rest of the handler (conservatively, past its block too).
                if float {
                    self.floats.insert(name);
                } else {
                    self.floats.remove(name);
                }
            }
            StmtKind::Assign { target, op, value } => {
                let ExprKind::Name(t) = &target.kind else {
                    return;
                };
                let reads = match op {
                    AssignOp::Div => Some(Vec::new()),
                    AssignOp::Set | AssignOp::Add | AssignOp::Sub | AssignOp::Mul => {
                        self.fractional(value)
                    }
                };
                if let Some(mut reads) = reads {
                    reads.retain(|r| *r != t.name.as_str());
                    self.written.push((t.name.as_str(), reads));
                }
            }
            StmtKind::If(f) => self.if_stmts(f),
            StmtKind::For(f) => {
                self.declare(&f.binding.name);
                self.floats.remove(f.binding.name.as_str());
                self.stmts(&f.body.items);
            }
            StmtKind::Match(m) => {
                for a in &m.arms {
                    match &a.body {
                        ArmBody::Block(b) => self.stmts(&b.items),
                        ArmBody::Single(st) => self.stmt(st),
                    }
                }
            }
            StmtKind::Expr(_) | StmtKind::Play(_) | StmtKind::Error => {}
        }
    }

    fn if_stmts(&mut self, f: &'a ast::If<ast::Stmt>) {
        self.stmts(&f.then.items);
        match &f.else_ {
            Some(Else::If(i, _)) => self.if_stmts(i),
            Some(Else::Block(b)) => self.stmts(&b.items),
            None => {}
        }
    }

    /// Whether a value is certainly a plain `float`: arithmetic with a
    /// fraction literal (`0.5`, `dy * 0.05`) or a handler `float`
    /// (`level + dy`) among its terms. `Some` lists the other names it
    /// reads, which must be plain numbers (whole-number states, checked
    /// once the file is walked); calls, fields and units are not looked
    /// into (`round(x * 0.5)` is an `int`, `2px * 0.5` a length).
    fn fractional(&self, e: &'a ast::Expr) -> Option<Vec<&'a str>> {
        let mut reads = Vec::new();
        self.terms(e, &mut reads)?.then_some(reads)
    }

    /// Walks the terms of arithmetic: `None` if one is not a plain number
    /// the walk understands, else whether one is fractional.
    fn terms(&self, e: &'a ast::Expr, reads: &mut Vec<&'a str>) -> Option<bool> {
        match &e.kind {
            ExprKind::Number(n) if n.unit.is_none() => Some(n.fraction),
            ExprKind::Name(n) if self.floats.contains(n.name.as_str()) => Some(true),
            ExprKind::Name(n) => {
                reads.push(n.name.as_str());
                Some(false)
            }
            ExprKind::Paren(inner)
            | ExprKind::Unary {
                op: ast::UnaryOp::Neg,
                expr: inner,
            } => self.terms(inner, reads),
            ExprKind::Binary {
                op:
                    op @ (BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem),
                lhs,
                rhs,
            } => {
                let l = self.terms(lhs, reads)?;
                let r = self.terms(rhs, reads)?;
                // Dividing plain numbers always gives a `float`.
                Some(l || r || *op == BinaryOp::Div)
            }
            _ => None,
        }
    }

    /// A builtin prop that reads and writes a `float` (a slider's
    /// `value`).
    fn float_prop(&self, kind: &str, prop: &str) -> bool {
        self.schema
            .element(kind)
            .and_then(|e| e.prop(prop))
            .is_some_and(|p| p.two_way && p.ty == Ty::FLOAT)
    }
}

/// The written type `float`.
fn is_float_type(t: &ast::Type) -> bool {
    matches!(&t.kind, ast::TypeKind::Named { path, args }
        if args.is_empty() && matches!(path.as_slice(), [one] if one.name == "float"))
}

#[cfg(test)]
mod tests {
    use super::super::{Module, check};
    use crate::schema::Schema;
    use crate::source::SourceMap;

    fn run(src: &str) -> super::super::Checked {
        let mut map = SourceMap::new();
        let id = map.add("a.strand", src);
        let parse = crate::syntax::parse(id, src);
        let modules = [Module {
            file: id,
            name: "a",
            ast: &parse.file,
        }];
        check(&modules, Schema::builtin())
    }

    #[test]
    fn plain_fraction_writes_are_pinned_before_the_first_pass() {
        let out = run("state level = 0\n\
                       state step = 0\n\
                       state half = 4\n\
                       state count = 0\n\
                       bar Top {\n\
                         slider { value: <-> level }\n\
                         box { on click { step = 0.5; half /= 2; count += 1 } }\n\
                       }\n");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.passes, 1);
        let ty = |name: &str| {
            let d = out
                .program
                .defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("no {name}"));
            out.program.types.show(&d.ty).to_string()
        };
        assert_eq!(ty("level"), "float");
        assert_eq!(ty("step"), "float");
        assert_eq!(ty("half"), "float");
        assert_eq!(ty("count"), "int");
    }

    /// A name declared twice in the file is left to the checker, which
    /// knows which declaration a write reaches.
    #[test]
    fn ambiguous_names_are_not_pinned_early() {
        let out = run("state level = 0\n\
                       component C(level: float) { text \"x\" }\n\
                       bar Top { box { on click { level = 0.5 } } }\n");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.passes, 2);
    }

    /// The usual scroll handler: a fraction computed from the event's
    /// `float` (`on scroll(dy)`, typed by the schema or written) or a
    /// fraction literal anywhere in the arithmetic is pinned up front.
    #[test]
    fn fraction_arithmetic_is_pinned_before_the_first_pass() {
        let out = run("state level = 0\n\
                       state bright = 0\n\
                       state ratio = 4\n\
                       state steps = 0\n\
                       bar Top {\n\
                         box {\n\
                           on scroll(dy) { level = level + dy * 0.05; bright += dy }\n\
                           on click { ratio = ratio / 3; steps = steps + 1 }\n\
                         }\n\
                         box { on scroll(d: float) { let half = d * 0.5; steps += 1; level -= half } }\n\
                       }\n");
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
        assert_eq!(out.passes, 1);
        let ty = |name: &str| {
            let d = out.program.defs.iter().find(|d| d.name == name).unwrap();
            out.program.types.show(&d.ty).to_string()
        };
        assert_eq!(ty("level"), "float");
        assert_eq!(ty("bright"), "float");
        assert_eq!(ty("ratio"), "float");
        assert_eq!(ty("steps"), "int");
    }

    /// A name an element brings into scope (`letters`'s `index`) is a
    /// declaration too: a write to it does not pin the file's state.
    #[test]
    fn element_scope_names_are_not_pinned_early() {
        let out = run("state index = 0\n\
                       bar Top {\n\
                         grid { columns: index }\n\
                         letters \"ab\" { on click { index = 0.5 } }\n\
                       }\n");
        assert!(
            !out.diagnostics
                .iter()
                .any(|d| d.message.contains("columns")),
            "{:?}",
            out.diagnostics
        );
        let d = out.program.defs.iter().find(|d| d.name == "index").unwrap();
        assert_eq!(out.program.types.show(&d.ty).to_string(), "int");
    }

    /// Hand-offs through handler locals and untyped fn values are
    /// followed: a long chain costs one extra pass, not one per link.
    #[test]
    fn chains_through_locals_and_fns_cost_one_extra_pass() {
        let links = 12;
        let states: String = (0..=links).map(|k| format!("state a{k} = 0\n")).collect();
        let locals: String = (1..=links)
            .map(|k| format!("; let t{k} = a{}; a{k} = t{k}", k - 1))
            .collect();
        let fns: String = (1..=links)
            .map(|k| format!("fn f{k}() {{ a{} }}\n", k - 1))
            .collect();
        let calls: String = (1..=links).map(|k| format!("; a{k} = f{k}()")).collect();
        for (decls, writes) in [(String::new(), locals), (fns, calls)] {
            let src =
                format!("{states}{decls}bar Top {{ box {{ on click {{ a0 = 0.5{writes} }} }} }}\n");
            let out = run(&src);
            assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
            assert!(out.passes <= 2, "{} passes", out.passes);
            let d = out
                .program
                .defs
                .iter()
                .find(|d| d.name == format!("a{links}"))
                .unwrap();
            assert_eq!(out.program.types.show(&d.ty).to_string(), "float");
        }
    }
}
