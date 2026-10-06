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
//! - [`ty`]: types; [`check`]: name resolution and type checking, which
//!   produce the typed [`hir`] the VM and the LSP consume.
//! - [`lower`]: the HIR to bytecode and a mountable tree ([`lower::Program`]).
//! - [`vm`]: the dynamic [`vm::Value`], the interpreter, builtins, the
//!   [`vm::ServiceHost`] trait with the schema-populated mock and the real
//!   clock, and `persist` storage.
//! - [`reconcile`]: live reload: identity across reloads, Merkle hashes,
//!   the loader (largest consistent set, last good tree and its cache).
//! - [`instantiate`]: mounts a program on a `strand-core` runtime and
//!   emits one `strand_scene::SceneDiff` per tick.
//! - [`fmt`]: the formatter behind `strand fmt` and LSP formatting.
//!
//! [`compile`] runs the front end over every file of a config.

pub mod check;
pub mod diagnostic;
pub mod fmt;
pub mod hir;
pub mod instantiate;
pub mod lower;
pub mod reconcile;
pub mod schema;
pub mod source;
pub mod syntax;
pub mod ty;
pub mod vm;

pub use diagnostic::{Diagnostic, Severity};
pub use source::{FileId, SourceMap};

/// The front end's result for a whole config.
#[derive(Debug)]
pub struct Compiled {
    /// One parse per file of the source map, in its order.
    pub parses: Vec<syntax::Parse>,
    pub program: hir::Program,
    /// Syntax diagnostics, then checker diagnostics, by file.
    pub diagnostics: Vec<Diagnostic>,
}

impl Compiled {
    pub fn errors(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }

    pub fn warnings(&self) -> usize {
        self.diagnostics.len() - self.errors()
    }
}

/// The module name of a file: its stem (`theme` for `a/theme.strand`).
pub fn module_name(file_name: &str) -> &str {
    let base = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    base.strip_suffix(".strand").unwrap_or(base)
}

/// Parses and checks every file in `map` as one program, against the
/// builtin schema.
pub fn compile(map: &SourceMap) -> Compiled {
    compile_with(map, schema::Schema::builtin())
}

/// [`compile`] against a given schema (the builtin one extended by
/// service crates).
pub fn compile_with(map: &SourceMap, schema: &schema::Schema) -> Compiled {
    let parses: Vec<syntax::Parse> = map
        .iter()
        .map(|(id, f)| syntax::parse(id, &f.text))
        .collect();
    let names: Vec<&str> = map.iter().map(|(_, f)| module_name(&f.name)).collect();
    let modules: Vec<check::Module<'_>> = parses
        .iter()
        .zip(&names)
        .map(|(p, n)| check::Module {
            file: p.file_id,
            name: n,
            ast: &p.file,
        })
        .collect();
    let checked = check::check(&modules, schema);
    let mut diagnostics: Vec<Diagnostic> = parses
        .iter()
        .flat_map(|p| p.diagnostics.iter().cloned())
        .collect();
    // `$fg-muted`: the parser warns about a subtraction; when the checker
    // names the token meant, its error replaces the warning.
    let kebab_fixed: Vec<(FileId, syntax::Span)> = checked
        .diagnostics
        .iter()
        .filter(|d| d.code == "check::unknown_token")
        .filter_map(|d| Some((d.file(), d.primary_span()?)))
        .collect();
    diagnostics.retain(|d| {
        d.code != "syntax::kebab_case"
            || !d.primary_span().is_some_and(|s| {
                kebab_fixed
                    .iter()
                    .any(|(f, k)| *f == d.file() && k.start <= s.start && s.end <= k.end)
            })
    });
    diagnostics.extend(checked.diagnostics);
    diagnostics.sort_by_key(|d| (d.file(), d.primary_span().map_or(0, |s| s.start)));
    Compiled {
        parses,
        program: checked.program,
        diagnostics,
    }
}
