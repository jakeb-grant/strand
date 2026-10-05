//! The one compiler crate behind the runtime, `strand check` and the LSP.
//!
//! Parses and type-checks `.strand` files, lowers bindings to bytecode, and
//! reconciles a new tree against the live one by identity (source span, then
//! `key`/`id`, then position). A broken save keeps the last good tree.
//!
//! See `docs/design.md`, "The proposed API" and "Live reload and real-time
//! config changes". The syntax is specified in `docs/grammar.md`.
//!
//! - [`syntax`]: lexer, parser and syntax tree with spans (M1).
//! - [`diagnostic`]: errors with labels and did-you-mean fixes, rendered
//!   with miette.

pub mod diagnostic;
pub mod syntax;

pub use diagnostic::{Diagnostic, Severity};
