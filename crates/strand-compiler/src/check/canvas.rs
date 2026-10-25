//! (flex) A time signal reaching a `canvas`'s `draw:` (the gap review's
//! drawing-widgets:animated-canvas). Logic runs `draw` when what it reads
//! changes and records its calls, never once per frame, so a value that
//! changes every frame (`t`, `wave(…)`, `noise` of `t`) has no number to
//! draw with there: the VM faults on it at run time. The checker finds
//! it first and says what moves a canvas instead: the canvas node's own
//! paint props, which render evaluates every frame, or a `shader`.

use std::collections::HashSet;

use super::Checker;
use crate::hir::{self, Callee, DefId, ExprKind, StmtKind};
use crate::syntax::Span;

/// How deep a `let` read inside another `let` is followed.
const MAX_DEPTH: u32 = 64;

/// What a time-bound value in an expression is: where, and how to name it.
struct Leaf {
    span: Span,
    what: String,
}

impl Checker<'_> {
    /// Notes `let` `id` when its value is time-bound, so a `draw:`
    /// reading it is reported.
    pub(super) fn note_time_let(&mut self, id: DefId, value: &hir::Expr) {
        if time_leaf(value, &self.time_lets, 0).is_some() {
            self.time_lets.insert(id);
        }
    }

    /// Reports the first time-bound value in canvas `draw` lambda
    /// `draw`.
    pub(super) fn time_in_draw(&mut self, draw: &hir::Expr) {
        let Some(leaf) = time_leaf(draw, &self.time_lets, 0) else {
            return;
        };
        self.error(
            "check::time_in_canvas",
            format!(
                "{} changes every frame, and a canvas is not drawn every frame",
                leaf.what
            ),
            leaf.span,
            "a time signal",
        )
        .help = Some(
            "`draw` runs when what it reads changes; animate the canvas node's own props \
             instead (`rotate: t * 90deg`, `opacity: wave(2s)`), or draw every frame with a \
             `shader`"
                .into(),
        );
    }
}

/// The first time-bound value in `e`: `t`, a `wave(…)`, or a `let`
/// in `time_lets` (`noise` of `t` holds `t`).
fn time_leaf(e: &hir::Expr, time_lets: &HashSet<DefId>, depth: u32) -> Option<Leaf> {
    if depth > MAX_DEPTH {
        return None;
    }
    let sub = |x: &hir::Expr| time_leaf(x, time_lets, depth + 1);
    match &e.kind {
        ExprKind::Value(n) if n == "t" => Some(Leaf {
            span: e.span,
            what: "`t`".into(),
        }),
        ExprKind::Def(d) if time_lets.contains(d) => Some(Leaf {
            span: e.span,
            what: "this value".into(),
        }),
        ExprKind::Call {
            callee: Callee::Builtin { name, .. },
            ..
        } if name == "wave" => Some(Leaf {
            span: e.span,
            what: "`wave(…)`".into(),
        }),
        ExprKind::Call { callee, args } => {
            let from_callee = match callee {
                Callee::Method { receiver, .. } => sub(receiver),
                Callee::Value(f) => sub(f),
                _ => None,
            };
            from_callee.or_else(|| args.iter().find_map(|a| sub(&a.value)))
        }
        ExprKind::Field { base, .. } => sub(base),
        ExprKind::Index { base, index } => sub(base).or_else(|| sub(index)),
        ExprKind::Unary { expr, .. } => sub(expr),
        ExprKind::Binary { lhs, rhs, .. } => sub(lhs).or_else(|| sub(rhs)),
        ExprKind::Ternary { cond, then, else_ } => {
            sub(cond).or_else(|| sub(then)).or_else(|| sub(else_))
        }
        ExprKind::Lambda { body, .. } => match body {
            hir::LambdaBody::Expr(x) => sub(x),
            hir::LambdaBody::Block(stmts) => stmts_leaf(stmts, time_lets, depth + 1),
        },
        ExprKind::Match { scrutinee, arms } => {
            sub(scrutinee).or_else(|| arms.iter().find_map(|(_, x)| sub(x)))
        }
        ExprKind::List(items) | ExprKind::Commas(items) | ExprKind::Spaced(items) => {
            items.iter().find_map(sub)
        }
        _ => None,
    }
}

fn stmts_leaf(stmts: &[hir::Stmt], time_lets: &HashSet<DefId>, depth: u32) -> Option<Leaf> {
    let e = |x: &hir::Expr| time_leaf(x, time_lets, depth + 1);
    let block = |b: &[hir::Stmt]| stmts_leaf(b, time_lets, depth + 1);
    stmts.iter().find_map(|s| match &s.kind {
        StmtKind::Let { value, .. } | StmtKind::Expr(value) | StmtKind::Play(value) => e(value),
        StmtKind::Assign { target, value, .. } => e(target).or_else(|| e(value)),
        StmtKind::If { cond, then, else_ } => {
            e(cond).or_else(|| block(then)).or_else(|| block(else_))
        }
        StmtKind::For { iter, body, .. } => e(iter).or_else(|| block(body)),
        StmtKind::Match { scrutinee, arms } => {
            e(scrutinee).or_else(|| arms.iter().find_map(|(_, b)| block(b)))
        }
        StmtKind::Error => None,
    })
}
