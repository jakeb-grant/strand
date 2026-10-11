//! Refactors offered as code actions: "extract component" (design.md,
//! "Developer experience"). Quick fixes live in `diag`.

mod extract;

pub use extract::{Extract, TITLE, extract, offer};
