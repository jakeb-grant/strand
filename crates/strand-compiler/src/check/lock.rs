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
//!   m4-audit). A call inside a `popup` or `tooltip` does not count.
//! - `check::lock_popup`: a `popup` or `tooltip` in the `lock` is a
//!   warning: strand-surface never opens one on a lock surface.

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

/// `check::lock_popup` for each `popup` or `tooltip` the config's `lock`
/// shows and, on a program with no error (`clean`), `check::lock_no_auth`
/// when nothing the lock can run calls `auth.submit`. A popup never opens
/// on a lock surface (strand-surface refuses it), so a call inside one is
/// no way out.
pub(super) fn way_out(files: &[FileHir], clean: bool) -> Vec<Diagnostic> {
    let mut bodies = HashMap::new();
    let mut lock = None;
    for f in files {
        for item in &f.items {
            match item {
                hir::Item::Component(c) => {
                    bodies.insert(c.def, (f.file, &c.body[..]));
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
    let Some((file, el)) = lock else {
        return Vec::new();
    };
    let mut walk = Walk {
        bodies: &bodies,
        seen: HashSet::new(),
        file,
        submits: false,
        popups: Vec::new(),
    };
    walk.element(el);
    let mut out: Vec<Diagnostic> = walk
        .popups
        .into_iter()
        .map(|(file, span, kind)| {
            Diagnostic::warning(
                "check::lock_popup",
                format!("a `{kind}` does not open on a lock screen"),
            )
            .with_label_in(file, span, "nothing in it is ever shown or run")
            .with_help("put its content in the lock itself")
        })
        .collect();
    if clean && !walk.submits {
        let head = Span::new(
            el.span.start,
            el.span.start.saturating_add(4).min(el.span.end),
        );
        out.push(
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
        );
    }
    out
}

/// A walk of everything the lock shows: whether anything calls
/// `auth.submit`, and the popups and tooltips it holds (not entered: they
/// never open on a lock). Each component is entered once, so recursion
/// ends.
struct Walk<'a> {
    bodies: &'a HashMap<DefId, (FileId, &'a [Node])>,
    seen: HashSet<DefId>,
    /// The file of the body being walked.
    file: FileId,
    submits: bool,
    popups: Vec<(FileId, Span, String)>,
}

impl Walk<'_> {
    fn element(&mut self, el: &hir::Element) {
        match &el.kind {
            ElementKind::Builtin(b) if b == "popup" || b == "tooltip" => {
                let end = el.span.start.saturating_add(b.len() as u32);
                let head = Span::new(el.span.start, end.min(el.span.end));
                self.popups.push((self.file, head, b.clone()));
                return;
            }
            ElementKind::Component(def) if self.seen.insert(*def) => {
                if let Some(&(file, body)) = self.bodies.get(def) {
                    let saved = std::mem::replace(&mut self.file, file);
                    self.nodes(body);
                    self.file = saved;
                }
            }
            _ => {}
        }
        el.props.iter().for_each(|p| self.prop(p));
        if let Some(e) = &el.arg {
            self.expr(e);
        }
        self.nodes(&el.children);
    }

    fn prop(&mut self, p: &hir::Prop) {
        self.expr(&p.value);
        p.sub.iter().for_each(|s| self.prop(s));
    }

    fn expr(&mut self, e: &hir::Expr) {
        self.submits = self.submits || calls(e);
    }

    fn nodes(&mut self, nodes: &[Node]) {
        for n in nodes {
            match n {
                Node::Element(e) => self.element(e),
                Node::If(i) => {
                    self.nodes(&i.then);
                    self.nodes(&i.else_);
                }
                Node::For(f) => self.nodes(&f.body),
                Node::Match(m) => m.arms.iter().for_each(|(_, body)| self.nodes(body)),
                Node::Handler(h) => self.submits = self.submits || calls_in(&h.body),
                Node::Timer(t) => self.submits = self.submits || calls_in(&t.body),
                Node::When(w) => w.props.iter().for_each(|p| self.prop(p)),
                Node::Pose(p) => p.props.iter().for_each(|p| self.prop(p)),
                Node::Selector(s) => s.props.iter().for_each(|p| self.prop(p)),
                Node::Let(l) => self.expr(&l.value),
                Node::State(_) | Node::Slot(_) | Node::Set(..) | Node::Play(_) => {}
            }
        }
    }
}

/// Whether statements (which hold no elements) call `auth.submit`.
fn calls_in(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match &s.kind {
        StmtKind::Let { value, .. } | StmtKind::Expr(value) => calls(value),
        StmtKind::Assign { target, value, .. } => calls(target) || calls(value),
        StmtKind::If { cond, then, else_ } => calls(cond) || calls_in(then) || calls_in(else_),
        StmtKind::For { iter, body, .. } => calls(iter) || calls_in(body),
        StmtKind::Match { scrutinee, arms } => {
            calls(scrutinee) || arms.iter().any(|(_, body)| calls_in(body))
        }
        StmtKind::Play(_) | StmtKind::Error => false,
    })
}

/// Whether an expression calls `auth.submit`.
fn calls(e: &hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Call { callee, args } => {
            let hit = match callee {
                Callee::Method { receiver, name, .. } => {
                    (name == "submit"
                        && matches!(&receiver.kind, ExprKind::Service(s) if s == "auth"))
                        || calls(receiver)
                }
                Callee::Value(v) => calls(v),
                // A `fn` is pure (`check::impure`): it cannot submit.
                Callee::Fn(_) | Callee::Builtin { .. } | Callee::Record(_) | Callee::Error => false,
            };
            hit || args.iter().any(|a| calls(&a.value))
        }
        ExprKind::Field { base, .. } => calls(base),
        ExprKind::Index { base, index } => calls(base) || calls(index),
        ExprKind::Unary { expr, .. } => calls(expr),
        ExprKind::Binary { lhs, rhs, .. } => calls(lhs) || calls(rhs),
        ExprKind::Ternary { cond, then, else_ } => calls(cond) || calls(then) || calls(else_),
        ExprKind::Lambda { body, .. } => match body {
            hir::LambdaBody::Expr(e) => calls(e),
            hir::LambdaBody::Block(b) => calls_in(b),
        },
        ExprKind::Match { scrutinee, arms } => {
            calls(scrutinee) || arms.iter().any(|(_, e)| calls(e))
        }
        ExprKind::List(v) | ExprKind::Commas(v) | ExprKind::Spaced(v) => v.iter().any(calls),
        _ => false,
    }
}
