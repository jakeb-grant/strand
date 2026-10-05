//! The one compiler crate behind the runtime, `strand check` and the LSP.
//!
//! Parses and type-checks `.strand` files, lowers bindings to bytecode, and
//! reconciles a new tree against the live one by identity (source span, then
//! `key`/`id`, then position). A broken save keeps the last good tree.
//!
//! See `docs/design.md`, "The proposed API" and "Live reload and real-time
//! config changes". The syntax is specified in `docs/grammar.md`.
//!
//! - [`source`]: file identity ([`FileId`], [`SourceMap`]) and which files
//!   a config directory loads ([`source::find_files`]).
//! - [`syntax`]: lexer, parser and syntax tree with spans (M1).
//! - [`diagnostic`]: errors with labels and did-you-mean fixes, rendered
//!   with miette.
//! - [`schema`]: the builtin elements, services, functions and tokens, as
//!   data that service crates extend.
//! - [`ty`]: the types of the language.

pub mod diagnostic;
pub mod schema;
pub mod source;
pub mod syntax;
pub mod ty;

pub use diagnostic::{Diagnostic, Severity};
pub use source::{FileId, SourceMap};
