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

use crate::diagnostic::Diagnostic;
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
