//! Lossless lexer, recursive-descent parser and syntax tree.
//!
//! The grammar is specified in `docs/grammar.md`. Parsing never panics: a
//! broken file still yields a full [`ast::File`] (with error nodes) plus
//! diagnostics, so the LSP and reload pipeline always have a tree.

pub mod ast;
pub mod dump;
pub mod lexer;
mod parser;
mod span;

pub use parser::{Parse, parse};
pub use span::{LineIndex, Span};
