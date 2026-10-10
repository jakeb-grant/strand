//! The `lock` surface's check-time rules (docs/architecture.md, "The
//! lock"; decisions.md, m4-lock-w1). The runtime's side (only `auth`
//! unlocks, `open` held while locked) is `instantiate/lock.rs`.
//!
//! - `check::lock_twice`: a config has one `lock`. The session has one
//!   lock and one password field; a second `lock` is an error naming the
//!   first.
//! - `check::lock_prop`: the surface props that a lock cannot honour are
//!   warnings rather than silently dropped: a lock covers every monitor
//!   (`screens`), sits above everything on `ext-session-lock` (`layer`),
//!   fills its monitor (`anchor`) and always has the keyboard
//!   (`keyboard`).
//! - `check::lock_no_auth`: something in the `lock` calls `auth.submit`,
//!   in one of its handlers or timers or in a component it shows (a `fn`
//!   is pure, so it cannot). Only an `auth` success unlocks, and a lock that
//!   draws and never faults shows no built-in field, so a lock with no
//!   call would lock the session with no way out (decisions.md,
//!   m4-audit).

use std::collections::{HashMap, HashSet};

use crate::diagnostic::Diagnostic;
use crate::hir::{self, Callee, DefId, ElementKind, ExprKind, FileHir, Node, Stmt, StmtKind};
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast::{self, ItemKind};

use super::Module;

/// The surface props a `lock` ignores, and why.
const IGNORED: &[(&str, &str)] = &[
    ("screens", "a lock covers every monitor"),
    ("layer", "a lock is above everything, on `ext-session-lock`"),
    ("anchor", "a lock surface fills its monitor"),
    ("keyboard", "a lock always takes the keyboard"),
];

/// The diagnostics of every `lock` across the config's files.
pub(super) fn check(modules: &[Module<'_>]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut first: Option<(FileId, Span)> = None;
    for m in modules {
        for item in &m.ast.items {
            let ItemKind::Surface(s) = &item.kind else {
                continue;
            };
            if s.kind.name != "lock" {
                continue;
            }
            match first {
                None => first = Some((m.file, s.kind.span)),
                Some((file, span)) => out.push(
                    Diagnostic::error("check::lock_twice", "a config has one `lock`")
                        .with_label_in(m.file, s.kind.span, "a second `lock`")
                        .with_secondary_in(file, span, "the first `lock` is here")
                        .with_help(
                            "the session has one lock screen: put its content in one `lock`",
                        ),
                ),
            }
            props(m.file, &s.body, &mut out);
        }
    }
    out
}

/// `check::lock_prop` for the lock's own props (not its children's).
fn props(file: FileId, body: &ast::Block<ast::Item>, out: &mut Vec<Diagnostic>) {
    for item in &body.items {
        let ItemKind::Prop(p) = &item.kind else {
            continue;
        };
        if let Some((name, why)) = IGNORED.iter().find(|(n, _)| *n == p.name.name) {
            out.push(
                Diagnostic::warning(
                    "check::lock_prop",
                    format!("`{name}` has no effect on a `lock`"),
                )
                .with_label_in(file, p.name.span, *why)
                .with_help(format!("remove `{name}`")),
            );
        }
    }
}

/// `check::lock_no_auth`: the config's `lock`, when nothing it can run
/// calls `auth.submit`.
pub(super) fn way_out(files: &[FileHir]) -> Option<Diagnostic> {
    let mut bodies = Bodies::default();
    let mut lock = None;
    for f in files {
        for item in &f.items {
            match item {
                hir::Item::Component(c) => {
                    bodies.components.insert(c.def, &c.body);
                }
                hir::Item::Surface(s)
                    if lock.is_none() && s.element.kind == ElementKind::Builtin("lock".into()) =>
                {
                    lock = Some((f.file, &s.element));
                }
                _ => {}
            }
        }
    }
    let (file, el) = lock?;
    let mut walk = Walk {
        bodies: &bodies,
        seen: HashSet::new(),
    };
    if walk.element(el) {
        return None;
    }
    let head = Span::new(
        el.span.start,
        el.span.start.saturating_add(4).min(el.span.end),
    );
    Some(
        Diagnostic::error(
            "check::lock_no_auth",
            "this `lock` has no way out: nothing in it calls `auth.submit`",
        )
        .with_label_in(file, head, "only `auth.submit` can unlock the session")
        .with_help(
            "add a password field: `input { type: password; text: <-> secret; \
             on activate { if secret != \"\" { auth.submit(secret) }; secret = \"\" } }` \
             (an empty Return is not sent: with pam_faillock it would count as a failed login)",
        ),
    )
}

/// The components a lock can show, by name.
#[derive(Default)]
struct Bodies<'a> {
    components: HashMap<DefId, &'a [Node]>,
}

/// A search for an `auth.submit` call; each component is entered once,
/// so recursion ends.
struct Walk<'a> {
    bodies: &'a Bodies<'a>,
    seen: HashSet<DefId>,
}

impl Walk<'_> {
    fn element(&mut self, el: &hir::Element) -> bool {
        if let ElementKind::Component(def) = &el.kind
            && self.seen.insert(*def)
            && let Some(body) = self.bodies.components.get(def)
            && self.nodes(body)
        {
            return true;
        }
        let props = el.props.iter().any(|p| self.prop(p));
        props || el.arg.as_ref().is_some_and(|e| self.expr(e)) || self.nodes(&el.children)
    }

    fn prop(&mut self, p: &hir::Prop) -> bool {
        self.expr(&p.value) || p.sub.iter().any(|s| self.prop(s))
    }

    fn nodes(&mut self, nodes: &[Node]) -> bool {
        nodes.iter().any(|n| match n {
            Node::Element(e) => self.element(e),
            Node::If(i) => self.nodes(&i.then) || self.nodes(&i.else_),
            Node::For(f) => self.nodes(&f.body),
            Node::Match(m) => m.arms.iter().any(|(_, body)| self.nodes(body)),
            Node::Handler(h) => self.stmts(&h.body),
            Node::Timer(t) => self.stmts(&t.body),
            Node::When(w) => w.props.iter().any(|p| self.prop(p)),
            Node::Pose(p) => p.props.iter().any(|p| self.prop(p)),
            Node::Selector(s) => s.props.iter().any(|p| self.prop(p)),
            Node::Let(l) => self.expr(&l.value),
            Node::State(_) | Node::Slot(_) | Node::Set(..) | Node::Play(_) => false,
        })
    }

    fn stmts(&mut self, stmts: &[Stmt]) -> bool {
        stmts.iter().any(|s| match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) => self.expr(value),
            StmtKind::Assign { target, value, .. } => self.expr(target) || self.expr(value),
            StmtKind::If { cond, then, else_ } => {
                self.expr(cond) || self.stmts(then) || self.stmts(else_)
            }
            StmtKind::For { iter, body, .. } => self.expr(iter) || self.stmts(body),
            StmtKind::Match { scrutinee, arms } => {
                self.expr(scrutinee) || arms.iter().any(|(_, body)| self.stmts(body))
            }
            StmtKind::Play(_) | StmtKind::Error => false,
        })
    }

    fn expr(&mut self, e: &hir::Expr) -> bool {
        match &e.kind {
            ExprKind::Call { callee, args } => {
                let hit = match callee {
                    Callee::Method { receiver, name, .. } => {
                        (name == "submit"
                            && matches!(&receiver.kind, ExprKind::Service(s) if s == "auth"))
                            || self.expr(receiver)
                    }
                    Callee::Value(v) => self.expr(v),
                    // A `fn` is pure (`check::impure`): it cannot submit.
                    Callee::Fn(_) | Callee::Builtin { .. } | Callee::Record(_) | Callee::Error => {
                        false
                    }
                };
                hit || args.iter().any(|a| self.expr(&a.value))
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Index { base, index } => self.expr(base) || self.expr(index),
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } => self.expr(lhs) || self.expr(rhs),
            ExprKind::Ternary { cond, then, else_ } => {
                self.expr(cond) || self.expr(then) || self.expr(else_)
            }
            ExprKind::Lambda { body, .. } => match body {
                hir::LambdaBody::Expr(e) => self.expr(e),
                hir::LambdaBody::Block(b) => self.stmts(b),
            },
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee) || arms.iter().any(|(_, e)| self.expr(e))
            }
            ExprKind::List(v) | ExprKind::Commas(v) | ExprKind::Spaced(v) => {
                v.iter().any(|e| self.expr(e))
            }
            _ => false,
        }
    }
}
