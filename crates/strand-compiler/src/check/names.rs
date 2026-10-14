//! (M6, the owner's ruling of 2026-10-14; decisions.md m6-w2-language)
//! Components, types, enums and `fn`s declared under one name in more
//! than one file (spec.md 4.2):
//!
//! 1. a file that declares the name and uses it gets its own declaration;
//! 2. a file without its own gets the single declaration elsewhere;
//! 3. with two or more elsewhere, a bare use is the load error
//!    `check::ambiguous_name` at the use, naming every declaring file and
//!    offering `file.Name`.
//!
//! `file.Name` (`tray.Row e { … }`, `x: tray.Item`, `tray.fmt(x)`,
//! `tray.Kind.dark`) names the declaration of that file wherever a bare
//! name can stand. Surfaces, services, actions, token sets and keyframes
//! stay unique across the config. [`notes`] is the `check::same_name`
//! note `strand check` and the language server show at each declaration
//! of a name declared in more than one file; a load never reports it.

use std::collections::BTreeMap;

use super::Checker;
use crate::diagnostic::{Diagnostic, suggest};
use crate::hir::{self, DefId, DefKind, Target};
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast;

/// Whether a declaration of `kind` may share its name with declarations
/// in other files.
pub fn shareable(kind: &DefKind) -> bool {
    matches!(
        kind,
        DefKind::Component | DefKind::Type(_) | DefKind::Enum(_) | DefKind::Fn
    )
}

/// What a global name means in one file.
pub(super) enum Global {
    Missing,
    One(DefId),
    /// Declared in two or more other files, not in this one.
    Ambiguous(Vec<DefId>),
}

/// What `file.Name` names.
pub(super) enum Qualified {
    /// `file` is no file of the config.
    NotAFile,
    Found(DefId),
    /// The file declares no such name; the error is reported.
    Reported,
}

/// Whether `s` can be read as a name (`my-bar` cannot).
fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl<'a> Checker<'a> {
    /// What global `name` means in module `module` (rules 1 to 3).
    pub(super) fn global_in(&self, module: usize, name: &str) -> Global {
        if let Some(&d) = self.module_globals.get(module).and_then(|g| g.get(name)) {
            return Global::One(d);
        }
        match self.global_decls.get(name).map(Vec::as_slice) {
            None | Some([]) => Global::Missing,
            Some([one]) => Global::One(*one),
            Some(many) => Global::Ambiguous(many.to_vec()),
        }
    }

    /// Global `name` used at `span` in the current file: its declaration,
    /// or `None` when there is none. Ambiguous, the error is reported and
    /// the first declaration stands in, so the rest still checks.
    pub(super) fn global(&mut self, name: &str, span: Span) -> Option<DefId> {
        match self.global_in(self.module, name) {
            Global::Missing => None,
            Global::One(d) => Some(d),
            Global::Ambiguous(many) => {
                self.ambiguous(name, span, &many);
                many.first().copied()
            }
        }
    }

    /// Global `name` in module `module` without reporting anything: `None`
    /// when missing or ambiguous. `written` may be qualified
    /// (`tray.Row`).
    pub(super) fn global_named(&self, module: usize, written: &str) -> Option<DefId> {
        if let Some((file, name)) = written.split_once('.') {
            let m = *self.file_index.get(file)?;
            return self
                .module_globals
                .get(m)?
                .get(name)
                .copied()
                .filter(|d| shareable(&self.defs[d.0 as usize].kind));
        }
        match self.global_in(module, written) {
            Global::One(d) => Some(d),
            _ => None,
        }
    }

    /// `file.Name`: the declaration of `name` in `file`, a component,
    /// type, enum or `fn`.
    pub(super) fn qualified(&mut self, file: &ast::Ident, name: &ast::Ident) -> Qualified {
        let Some(&m) = self.file_index.get(&file.name) else {
            return Qualified::NotAFile;
        };
        let found = self.module_globals[m]
            .get(&name.name)
            .copied()
            .filter(|d| shareable(&self.defs[d.0 as usize].kind));
        let file_id = self.modules[m].file;
        self.add_ref(file.span, Target::File(file_id));
        if let Some(d) = found {
            self.add_ref(name.span, Target::Def(d));
            return Qualified::Found(d);
        }
        let mut listed: Vec<String> = self.module_globals[m]
            .iter()
            .filter(|(_, d)| shareable(&self.defs[d.0 as usize].kind))
            .map(|(n, _)| n.clone())
            .collect();
        listed.sort_unstable();
        let fix = suggest(&name.name, listed.iter().map(String::as_str)).map(str::to_string);
        let d = self.error(
            "check::unknown_name",
            format!("`{}.strand` declares no `{}`", file.name, name.name),
            name.span,
            "not declared there",
        );
        match fix {
            Some(f) => {
                d.suggest(name.span, f);
            }
            None if !listed.is_empty() => {
                d.help = Some(format!(
                    "its components, types, enums and `fn`s: {}",
                    listed.join(", ")
                ));
            }
            None => {
                d.help = Some(format!(
                    "`{}.strand` declares no components, types, enums or `fn`s",
                    file.name
                ));
            }
        }
        Qualified::Reported
    }

    /// `name`, used at `span`, is declared in two or more other files.
    fn ambiguous(&mut self, name: &str, span: Span, decls: &[DefId]) {
        if !self.ambiguous_seen.insert((self.file(), span)) {
            return;
        }
        let mut files: Vec<(String, FileId, Span)> = decls
            .iter()
            .map(|d| {
                let def = &self.defs[d.0 as usize];
                let stem = self
                    .modules
                    .iter()
                    .find(|m| m.file == def.file)
                    .map_or("?", |m| m.name)
                    .to_string();
                (stem, def.file, def.span)
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let shown: Vec<String> = files
            .iter()
            .map(|(s, _, _)| format!("`{s}.strand`"))
            .collect();
        let listed = match shown.as_slice() {
            [a, b] => format!("{a} and {b}"),
            many => many.join(", "),
        };
        let d = self.error(
            "check::ambiguous_name",
            format!(
                "`{name}` is declared in {} files, {listed}, and not in this one",
                files.len()
            ),
            span,
            "which one?",
        );
        for (stem, file, at) in &files {
            d.add_secondary(*file, *at, format!("declared in `{stem}.strand`"));
        }
        let fixes: Vec<String> = files
            .iter()
            .filter(|(s, _, _)| is_identifier(s))
            .map(|(s, _, _)| format!("{s}.{name}"))
            .collect();
        for f in &fixes {
            d.add_suggestion(span, f.clone());
        }
        let unnamed: Vec<String> = files
            .iter()
            .filter(|(s, _, _)| !is_identifier(s))
            .map(|(s, _, _)| format!("`{s}.strand`"))
            .collect();
        let mut help = match fixes.as_slice() {
            [] => String::new(),
            [one] => format!("name the file it comes from: `{one}`"),
            many => format!(
                "name the file it comes from: {}",
                many.iter()
                    .map(|f| format!("`{f}`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        };
        if !unnamed.is_empty() {
            if !help.is_empty() {
                help.push_str("; ");
            }
            help.push_str(&format!(
                "rename {} to an identifier to name it so",
                unnamed.join(", ")
            ));
        }
        help.push_str(&format!(
            " (or declare `{name}` in this file, which then uses its own)"
        ));
        d.help = Some(help);
    }
}

/// The `check::same_name` note at each declaration of a component, type,
/// enum or `fn` whose name is declared in more than one file: which other
/// files declare it, and how a use is resolved. Never an error.
pub fn notes(program: &hir::Program) -> Vec<Diagnostic> {
    let stem = |f: FileId| {
        program
            .files
            .iter()
            .find(|h| h.file == f)
            .map_or("?", |h| h.name.as_str())
    };
    let mut by_name: BTreeMap<&str, Vec<&hir::Def>> = BTreeMap::new();
    for d in &program.defs {
        if d.owner.is_none() && shareable(&d.kind) {
            by_name.entry(d.name.as_str()).or_default().push(d);
        }
    }
    let mut out = Vec::new();
    for (name, defs) in by_name {
        let mut files: Vec<FileId> = defs.iter().map(|d| d.file).collect();
        files.sort_unstable();
        files.dedup();
        if files.len() < 2 {
            continue;
        }
        for d in &defs {
            let mut others: Vec<&str> = files
                .iter()
                .filter(|f| **f != d.file)
                .map(|f| stem(*f))
                .collect();
            others.sort_unstable();
            let listed = others
                .iter()
                .map(|s| format!("`{s}.strand`"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut n = Diagnostic::note(
                "check::same_name",
                format!("`{name}` is also declared in {listed}"),
            )
            .with_label_in(
                d.file,
                d.span,
                format!("declared in {} files", files.len()),
            );
            for o in defs.iter().filter(|o| o.file != d.file) {
                n.add_secondary(o.file, o.span, format!("in `{}.strand`", stem(o.file)));
            }
            let qualified = files
                .iter()
                .map(|f| format!("`{}.{name}`", stem(*f)))
                .collect::<Vec<_>>()
                .join(", ");
            n.help = Some(format!(
                "not a problem: a file that declares `{name}` uses its own; a file \
                 without one uses the only declaration elsewhere, and where two or more \
                 other files declare it, it must name one: {qualified}"
            ));
            out.push(n);
        }
    }
    out
}
