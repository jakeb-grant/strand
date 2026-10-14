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

/// What a position accepts (spec.md 4.2): the rules choose among the
/// declarations of these kinds only, and with none of them a name falls
/// through to the builtins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Want {
    /// A type annotation: a `type` or an `enum`.
    Type,
    /// An element: a component.
    Component,
    /// A call: a `fn` or a `type`'s constructor.
    Call,
    /// A pattern's or a value's `Kind.v`: an enum.
    Enum,
    /// A bare name read as a value: any kind (what is not a value is
    /// reported as such).
    Value,
}

impl Want {
    fn accepts(self, kind: &DefKind) -> bool {
        match self {
            Want::Type => matches!(kind, DefKind::Type(_) | DefKind::Enum(_)),
            Want::Component => *kind == DefKind::Component,
            Want::Call => matches!(kind, DefKind::Fn | DefKind::Type(_)),
            Want::Enum => matches!(kind, DefKind::Enum(_)),
            Want::Value => true,
        }
    }
}

/// What a global name means in one file.
#[derive(Debug)]
pub(super) enum Global {
    Missing,
    One(DefId),
    /// Declared in two or more other files, not in this one (as a kind
    /// the position accepts). From [`Checker::global`], already reported.
    Ambiguous(Vec<DefId>),
}

impl Global {
    pub(super) fn one(&self) -> Option<DefId> {
        match self {
            Global::One(d) => Some(*d),
            _ => None,
        }
    }
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
pub fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The stems `file.Name` can name among `stems`: identifiers no other
/// of them shares (two `row.strand`s in two directories cannot be told
/// apart).
pub fn nameable<'s>(stems: &[&'s str]) -> Vec<&'s str> {
    stems
        .iter()
        .copied()
        .filter(|s| is_identifier(s) && stems.iter().filter(|o| *o == s).count() == 1)
        .collect()
}

impl<'a> Checker<'a> {
    /// What global `name` means in module `module` at a position that
    /// accepts `want` (rules 1 to 3). A name declared once means that
    /// declaration whatever its kind, as before (the position reports a
    /// kind it cannot use).
    pub(super) fn global_in(&self, module: usize, name: &str, want: Want) -> Global {
        let decls = match self.global_decls.get(name).map(Vec::as_slice) {
            None | Some([]) => return Global::Missing,
            Some([one]) => return Global::One(*one),
            Some(many) => many,
        };
        let accepted = |d: &DefId| want.accepts(&self.defs[d.0 as usize].kind);
        if let Some(own) = self.module_globals.get(module).and_then(|g| g.get(name))
            && accepted(own)
        {
            return Global::One(*own);
        }
        let file = self.modules.get(module).map(|m| m.file);
        let others: Vec<DefId> = decls
            .iter()
            .filter(|d| accepted(d) && Some(self.defs[d.0 as usize].file) != file)
            .copied()
            .collect();
        match others.as_slice() {
            [] => Global::Missing,
            [one] => Global::One(*one),
            _ => Global::Ambiguous(others),
        }
    }

    /// Global `name` used at `span` in the current file, at a position
    /// that accepts `want`. Ambiguous, the error is reported (once per
    /// use, even when an overloaded call checks it again) and the caller
    /// types the use as an error: no declaration stands in.
    pub(super) fn global(&mut self, name: &str, span: Span, want: Want) -> Global {
        let g = self.global_in(self.module, name, want);
        if let Global::Ambiguous(many) = &g {
            let many = many.clone();
            self.ambiguous(name, span, &many);
        }
        g
    }

    /// Global `name` in module `module` without reporting anything: `None`
    /// when missing or ambiguous. `written` may be qualified
    /// (`tray.Row`).
    pub(super) fn global_named(&self, module: usize, written: &str, want: Want) -> Option<DefId> {
        if let Some((file, name)) = written.split_once('.') {
            let m = *self.file_index.get(file)?;
            return self
                .module_globals
                .get(m)?
                .get(name)
                .copied()
                .filter(|d| shareable(&self.defs[d.0 as usize].kind));
        }
        self.global_in(module, written, want).one()
    }

    /// The stem of `file` (`tray` for `tray.strand`).
    pub(super) fn stem_of(&self, file: FileId) -> &'a str {
        self.modules
            .iter()
            .find(|m| m.file == file)
            .map_or("?", |m| m.name)
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
    /// Reported once per use: the mark is in `reported`, which an
    /// overloaded call's discarded attempts restore with their errors.
    fn ambiguous(&mut self, name: &str, span: Span, decls: &[DefId]) {
        if !self.reported.insert((self.file(), span)) {
            return;
        }
        let mut files: Vec<(&'a str, FileId, Span)> = decls
            .iter()
            .map(|d| {
                let def = &self.defs[d.0 as usize];
                (self.stem_of(def.file), def.file, def.span)
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        let stems: Vec<&str> = files.iter().map(|f| f.0).collect();
        let d = self.error(
            "check::ambiguous_name",
            format!(
                "`{name}` is declared in {} files, {}, and not in this one",
                files.len(),
                listed(&stems)
            ),
            span,
            "which one?",
        );
        for (stem, file, at) in &files {
            d.add_secondary(*file, *at, format!("declared in `{stem}.strand`"));
        }
        let (forms, help) = naming(&stems, |s| format!("{s}.{name}"));
        for f in forms {
            d.add_suggestion(span, f);
        }
        d.help = Some(format!(
            "{help} (or declare `{name}` in this file, which then uses its own)"
        ));
    }
}

/// `` `a.strand` and `b.strand` ``, or a comma list of three or more.
pub fn listed(stems: &[&str]) -> String {
    let shown: Vec<String> = stems.iter().map(|s| format!("`{s}.strand`")).collect();
    match shown.as_slice() {
        [a, b] => format!("{a} and {b}"),
        many => many.join(", "),
    }
}

/// How a use names one of the declarations in the files `stems`: the
/// forms it can write (`form(stem)`, `a.Row`), and a help saying so that
/// asks to rename each file whose name cannot be written (not an
/// identifier, or shared with another file) instead of offering a fix
/// that would not work.
pub fn naming(stems: &[&str], form: impl Fn(&str) -> String) -> (Vec<String>, String) {
    let ok = nameable(stems);
    let forms: Vec<String> = ok.iter().map(|s| form(s)).collect();
    let mut bad: Vec<&str> = stems.iter().copied().filter(|s| !ok.contains(s)).collect();
    bad.sort_unstable();
    bad.dedup();
    let quoted = |v: &[String]| v.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>();
    let mut help = match forms.as_slice() {
        [] => String::new(),
        many => format!("name the file it comes from: {}", quoted(many).join(" or ")),
    };
    if !bad.is_empty() {
        if !help.is_empty() {
            help.push_str("; ");
        }
        help.push_str(&format!(
            "rename {} to an identifier no other file has, to name {} so",
            bad.iter()
                .map(|s| format!("`{s}.strand`"))
                .collect::<Vec<_>>()
                .join(", "),
            if bad.len() == 1 { "it" } else { "them" }
        ));
    }
    (forms, help)
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
            let mut n = Diagnostic::note(
                "check::same_name",
                format!("`{name}` is also declared in {}", listed(&others)),
            )
            .with_label_in(
                d.file,
                d.span,
                format!("declared in {} files", files.len()),
            );
            for o in defs.iter().filter(|o| o.file != d.file) {
                n.add_secondary(o.file, o.span, format!("in `{}.strand`", stem(o.file)));
            }
            let mut stems: Vec<&str> = files.iter().map(|f| stem(*f)).collect();
            stems.sort_unstable();
            let (_, how) = naming(&stems, |s| format!("{s}.{name}"));
            n.help = Some(format!(
                "not a problem: a file that declares `{name}` uses its own; a file \
                 without one uses the only declaration elsewhere, and where two or more \
                 other files declare it, a bare `{name}` is an error there: {how}"
            ));

            out.push(n);
        }
    }
    out
}
