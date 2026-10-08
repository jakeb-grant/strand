//! Developer tooling kept out of the runtime binary: the language server
//! (and, in M5, the inspector). It shares the parser, checker and
//! formatter with the runtime through `strand-compiler`.
//!
//! See `docs/design.md`, "Developer experience": diagnostics, completion
//! after `$`, `.` and `<->`, hovers generated from the service schemas,
//! go-to-definition, rename and quick fixes, and formatting.

mod completion;
mod ctx;
mod describe;
mod diag;
mod nav;
pub mod server;
pub mod text;
mod walk;
pub mod workspace;

pub use server::{capabilities, run_stdio, schema, serve, serve_with};
