//! The tree-sitter grammar for `.strand` files, for editors.
//!
//! `grammar.js` writes docs/grammar.md for tree-sitter; the generated
//! parser (`src/`, committed and held to `grammar.js` by
//! `scripts/check-tree-sitter.sh`) and the external scanner
//! (`src/scanner.c`) are compiled by `build.rs`. strand-dev's tests use
//! it to hold the grammar to the compiler's parser; nothing in the
//! runtime or the language server links it (docs/architecture.md,
//! "tree-sitter layout").
//!
//! ```
//! let mut parser = tree_sitter::Parser::new();
//! parser.set_language(&tree_sitter_strand::LANGUAGE.into()).unwrap();
//! let tree = parser.parse("bar Top { edge: top }\n", None).unwrap();
//! assert!(!tree.root_node().has_error());
//! ```

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_strand() -> *const ();
}

/// The language, for `tree_sitter::Parser::set_language` (through
/// `tree_sitter::Language::new` or `.into()`).
// SAFETY: `tree_sitter_strand` is the generated parser's constructor; it
// takes nothing and returns a pointer to a static `TSLanguage`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_strand) };

/// `node-types.json`: every node kind and its fields.
pub const NODE_TYPES: &str = include_str!("../../src/node-types.json");

/// Highlights (`queries/highlights.scm`).
pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");

/// Scopes, definitions and references (`queries/locals.scm`).
pub const LOCALS_QUERY: &str = include_str!("../../queries/locals.scm");

/// Indentation (`queries/indents.scm`, nvim-treesitter's captures).
pub const INDENTS_QUERY: &str = include_str!("../../queries/indents.scm");

/// Injections (`queries/injections.scm`).
pub const INJECTIONS_QUERY: &str = include_str!("../../queries/injections.scm");

#[cfg(test)]
mod tests {
    #[test]
    fn the_language_loads() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::LANGUAGE.into())
            .expect("the generated parser's ABI is one the tree-sitter crate reads");
    }
}
