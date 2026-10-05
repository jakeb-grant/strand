//! Whole-number `state`s that are visibly written a fraction, found from
//! the syntax before checking, so the usual case is checked in one pass
//! (see [`super::check`]).
//!
//! `state level = 0` is an `int` until a fraction is written to it; then
//! the program is checked again with it pinned to `float`. Most such
//! writes can be read off the source: `level = 0.5`, `level /= 2`,
//! `level += 0.1`, or `value: <-> level` on a builtin element whose prop
//! is a `float` (a slider). Those states are pinned up front. A pin is
//! only taken when the write certainly reaches that state: the state is
//! the only declaration of its name in its file, the write is in the same
//! file, and the target is written bare. Anything else (a fraction that
//! arrives through another state, a call or a lambda) is still found by
//! the checker and costs a further pass.

use std::collections::{HashMap, HashSet};

use super::Module;
use crate::schema::Schema;
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast::{self, ArmBody, AssignOp, Else, ExprKind, ItemKind, StmtKind};
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
            written: HashSet::new(),
        };
        w.items(&m.ast.items, None);
        for name in &w.written {
            if w.declared.get(name) == Some(&1)
                && let Some(&span) = w.whole_states.get(name)
            {
                out.insert((m.file, span));
            }
        }
    }
    out
}

struct Walk<'s, 'a> {
    schema: &'s Schema,
    /// How many times each name is declared in the file (states, lets,
    /// parameters, loop and event bindings).
    declared: HashMap<&'a str, u32>,
    /// Untyped states initialised with a whole number.
    whole_states: HashMap<&'a str, Span>,
    /// Names written a fraction.
    written: HashSet<&'a str>,
}

impl<'a> Walk<'_, 'a> {
    fn declare(&mut self, name: &'a ast::Ident) {
        *self.declared.entry(name.name.as_str()).or_default() += 1;
    }

    fn params(&mut self, params: Option<&'a Vec<ast::Param>>) {
        for p in params.into_iter().flatten() {
            self.declare(&p.name);
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
                self.declare(&c.name);
                self.params(c.params.as_ref());
                self.items(&c.body.items, None);
            }
            ItemKind::Surface(s) => {
                if let Some(n) = &s.name {
                    self.declare(n);
                }
                self.items(&s.body.items, Some(s.kind.name.as_str()));
            }
            ItemKind::State(s) => {
                self.declare(&s.name);
                if let ast::StateInit::Value {
                    ty: None, value, ..
                } = &s.init
                    && super::is_whole_literal(value)
                {
                    self.whole_states.insert(s.name.name.as_str(), s.name.span);
                }
            }
            ItemKind::Let(l) => self.declare(&l.name),
            ItemKind::Fn(f) => self.declare(&f.name),
            ItemKind::Prop(p) => {
                if p.two_way.is_some()
                    && let ExprKind::Name(target) = &p.value.kind
                    && element.is_some_and(|k| self.float_prop(k, &p.name.name))
                {
                    self.written.insert(target.name.as_str());
                }
                if let Some(b) = &p.block {
                    self.items(&b.items, element);
                }
            }
            ItemKind::Element(e) => {
                let kind = e.kind.name.as_str();
                let builtin = self.schema.element(kind).is_some().then_some(kind);
                if let Some(b) = &e.block {
                    self.items(&b.items, builtin);
                }
            }
            ItemKind::When(w) => self.items(&w.body.items, element),
            ItemKind::If(f) => self.if_items(f, element),
            ItemKind::For(f) => {
                self.declare(&f.binding);
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
                if let ast::Event::Named { params, .. } = &o.event {
                    self.params(params.as_ref());
                }
                self.stmts(&o.body.items);
            }
            ItemKind::Timer(t) => self.stmts(&t.body.items),
            ItemKind::Pose(p) => self.items(&p.body.items, element),
            _ => {}
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
            StmtKind::Let(l) => self.declare(&l.name),
            StmtKind::Assign { target, op, value } => {
                let ExprKind::Name(t) = &target.kind else {
                    return;
                };
                let fraction = match op {
                    AssignOp::Div => true,
                    AssignOp::Set | AssignOp::Add | AssignOp::Sub | AssignOp::Mul => {
                        fraction_literal(value)
                    }
                };
                if fraction {
                    self.written.insert(t.name.as_str());
                }
            }
            StmtKind::If(f) => self.if_stmts(f),
            StmtKind::For(f) => {
                self.declare(&f.binding);
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

    /// A builtin prop that reads and writes a `float` (a slider's
    /// `value`).
    fn float_prop(&self, kind: &str, prop: &str) -> bool {
        self.schema
            .element(kind)
            .and_then(|e| e.prop(prop))
            .is_some_and(|p| p.two_way && p.ty == Ty::FLOAT)
    }
}

/// `0.5`, `-0.25`, `(1.5)`: a plain number written with a fraction.
fn fraction_literal(e: &ast::Expr) -> bool {
    match &e.kind {
        ExprKind::Number(n) => n.fraction && n.unit.is_none(),
        ExprKind::Paren(inner) => fraction_literal(inner),
        ExprKind::Unary {
            op: ast::UnaryOp::Neg,
            expr,
        } => fraction_literal(expr),
        _ => false,
    }
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
}
