//! (check-settings) The settings files a config reads (`state monitors
//! from "monitors.toml" { … }`), read as the running shell reads them
//! (spec.md 10.3): `strand check` reports what would not load, where in
//! the file it is.
//!
//! Each file is resolved as instantiate's `Storage::resolve` resolves it
//! (`~/…` from `$HOME`, an absolute path as is, else under the config
//! directory) and read through any symlink. A file that is not there is
//! no problem: the shell starts from the defaults and makes it on the
//! first write. A file that cannot be read, or is not TOML, is an error:
//! the shell takes nothing from it (every field keeps its last good
//! value) and never writes it. A field whose value does not read as its
//! type (`persist::decode_item`, the shell's own decoder: a wrong type,
//! a key a record does not have, a dictionary key that does not read)
//! is a warning, as the shell keeps that field's last good value and
//! loads the rest; so is a key no declaration of the file has, which the
//! shell ignores. Every problem in a field is reported, not only the
//! first the decoder stops at, so one check finds them all. Each problem is labelled in the TOML file (the value,
//! key or header at fault), with the declaration as a second label.
//!
//! This touches the disk, so it is not part of [`crate::compile`]:
//! `strand check` runs it, like [`super::paths::check`]; the TOML files
//! join the source map so the report shows their lines.

use std::ops::Range;
use std::path::{Path, PathBuf};

use strand_core::settings::toml_edit;

use crate::SourceMap;
use crate::diagnostic::{Diagnostic, closest};
use crate::hir::{self, Node, StateInit};
use crate::source::FileId;
use crate::syntax::Span;
use crate::ty::{Ty, TypeTable};
use crate::vm::persist::{decode_item, key_text};

/// One `state x from "file.toml" { … }`.
struct Decl<'a> {
    /// The `.strand` file it is in.
    file: FileId,
    name: &'a str,
    path: &'a str,
    path_span: Span,
    fields: &'a [hir::SettingsField],
}

/// Read every settings file `program` declares, relative paths under
/// `config_dir` (not read without one), adding each file read to `map`,
/// and report what the running shell would not load from it.
pub fn check(
    program: &hir::Program,
    config_dir: Option<&Path>,
    map: &mut SourceMap,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (path, decls) in &declared(program, config_dir) {
        check_file(&program.types, path, decls, map, &mut out);
    }
    out
}

/// The path of every settings file `program` declares, as [`check`]
/// resolves it, whether or not it is there: the language server watches
/// them, so a file made after it was analysed (the shell's first write)
/// is read when it appears.
pub fn paths(program: &hir::Program, config_dir: Option<&Path>) -> Vec<PathBuf> {
    declared(program, config_dir)
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// Each settings file `program` declares, resolved, with its declarations
/// (one read per file, whichever declarations share it).
fn declared<'a>(
    program: &'a hir::Program,
    config_dir: Option<&Path>,
) -> Vec<(PathBuf, Vec<Decl<'a>>)> {
    let mut decls = Vec::new();
    for file in &program.files {
        for item in &file.items {
            match item {
                hir::Item::State(s) => decl(program, s, &mut decls),
                hir::Item::Component(c) => nodes(program, &c.body, &mut decls),
                hir::Item::Surface(s) => nodes(program, &s.element.children, &mut decls),
                _ => {}
            }
        }
    }
    let mut files: Vec<(PathBuf, Vec<Decl<'_>>)> = Vec::new();
    for d in decls {
        let Some(path) = resolve(d.path, config_dir) else {
            continue;
        };
        match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, ds)) => ds.push(d),
            None => files.push((path, vec![d])),
        }
    }
    files
}

/// A settings file's path as the shell resolves it.
fn resolve(path: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest));
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        return Some(p);
    }
    config_dir.map(|d| d.join(p))
}

fn decl<'a>(program: &'a hir::Program, s: &'a hir::StateDecl, out: &mut Vec<Decl<'a>>) {
    let StateInit::File {
        path,
        path_span,
        fields,
    } = &s.init
    else {
        return;
    };
    let def = program.def(s.def);
    out.push(Decl {
        file: def.file,
        name: &def.name,
        path,
        path_span: *path_span,
        fields,
    });
}

/// The settings declared in a component's or surface's tree.
fn nodes<'a>(program: &'a hir::Program, ns: &'a [Node], out: &mut Vec<Decl<'a>>) {
    for n in ns {
        match n {
            Node::State(s) => decl(program, s, out),
            Node::Element(e) => nodes(program, &e.children, out),
            Node::If(i) => {
                nodes(program, &i.then, out);
                nodes(program, &i.else_, out);
            }
            Node::For(l) => nodes(program, &l.body, out),
            Node::Match(m) => {
                for (_, arm) in &m.arms {
                    nodes(program, arm, out);
                }
            }
            _ => {}
        }
    }
}

fn check_file(
    types: &TypeTable,
    path: &Path,
    decls: &[Decl<'_>],
    map: &mut SourceMap,
    out: &mut Vec<Diagnostic>,
) {
    let Some(first) = decls.first() else {
        return;
    };
    let shown = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // The shell starts from the defaults and writes it when a value
        // is first set.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            out.push(
                Diagnostic::error(
                    "check::settings_unreadable",
                    format!(
                        "settings file `{shown}` cannot be read ({e}): the shell keeps every \
                         last good value and never writes it"
                    ),
                )
                .with_label_in(first.file, first.path_span, "read here"),
            );
            return;
        }
    };
    let toml = map.add(shown.clone(), text.clone());
    let doc = match toml_edit::Document::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            out.push(
                Diagnostic::error(
                    "check::settings_syntax",
                    format!(
                        "settings file `{shown}` is not valid TOML: {}; the shell keeps every \
                         last good value and never writes it",
                        e.message().trim_end()
                    ),
                )
                .with_label_in(toml, span(e.span()), "not TOML")
                .with_secondary_in(first.file, first.path_span, "read here"),
            );
            return;
        }
    };
    let root = doc.as_table();
    let mut said: Vec<(String, String)> = Vec::new();
    for d in decls {
        for f in d.fields {
            let Some((key, item)) = root.get_key_value(&f.name) else {
                continue;
            };
            let mut found = Vec::new();
            faults(types, &f.ty, item, "", &mut found);
            for (at, why) in found {
                // Two declarations of one file, one report.
                if said.iter().any(|s| s.0 == f.name && s.1 == why) {
                    continue;
                }
                said.push((f.name.clone(), why.clone()));
                let at = at.or_else(|| key.span()).or_else(|| item.span());
                out.push(
                    Diagnostic::warning(
                        "check::settings_value",
                        format!(
                            "`{shown}`: `{}`: {why}; the shell keeps its last good value \
                             (its default until one has loaded)",
                            f.name
                        ),
                    )
                    .with_label_in(toml, span(at), "does not load")
                    .with_secondary_in(
                        d.file,
                        f.span,
                        format!("`{}.{}` is a `{}`", d.name, f.name, types.show(&f.ty)),
                    ),
                );
            }
        }
    }
    // A key no declaration of the file has: the shell never reads it.
    let names: Vec<&str> = decls
        .iter()
        .flat_map(|d| d.fields.iter().map(|f| f.name.as_str()))
        .collect();
    for (k, _) in root.iter() {
        if names.contains(&k) {
            continue;
        }
        let at = root
            .key(k)
            .and_then(toml_edit::Key::span)
            .or_else(|| root.get(k).and_then(toml_edit::Item::span));
        let mut d = Diagnostic::warning(
            "check::settings_unknown_key",
            format!(
                "`{shown}`: `{k}` is not a setting of `{}`: the shell ignores it",
                first.name
            ),
        )
        .with_label_in(toml, span(at), "not declared")
        .with_secondary_in(first.file, first.path_span, "read here");
        d.help = Some(match closest(k, names.iter().copied()) {
            Some(n) => format!("did you mean `{n}`?"),
            None => format!(
                "declare it in `{}`'s block, or remove it (its settings: {})",
                first.name,
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            ),
        });
        out.push(d);
    }
}

/// A byte range of the TOML file as a label's span (the file's start when
/// toml_edit kept none).
fn span(r: Option<Range<usize>>) -> Span {
    let r = r.unwrap_or(0..0);
    let to = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    Span {
        start: to(r.start),
        end: to(r.end.max(r.start)),
    }
}

/// Every place in `item`, which does not read as a `ty`, the shell's
/// decoder would stop at, walking the item as `decode_item` does: each
/// failing value, key a record does not have, and dictionary key that
/// does not read, with its reason worded as the shell's notice words it
/// (`"a + b": [1]: scale: expected float`). `decode_item` stops at the
/// first, so a field with several problems would otherwise be fixed one
/// `strand check` at a time. A range is `None` when toml_edit kept no
/// span for it.
fn faults(
    types: &TypeTable,
    ty: &Ty,
    item: &toml_edit::Item,
    at: &str,
    out: &mut Vec<(Option<Range<usize>>, String)>,
) {
    let Err(why) = decode_item(types, ty, item) else {
        return;
    };
    let before = out.len();
    match ty.non_null() {
        Ty::Record(r) => {
            if let Some(t) = item.as_table_like() {
                let def = types.record(*r);
                for (k, child) in t.iter() {
                    let key = t.get_key_value(k).and_then(|(key, _)| key.span());
                    match def.field(k) {
                        // A key the record does not have.
                        None => out.push((
                            key.or_else(|| child.span()),
                            joined(at, &format!("unknown field {k:?}")),
                        )),
                        Some(f) => {
                            let n = out.len();
                            faults(types, &f.ty, child, &joined(at, &f.name), out);
                            if out.len() > n && out[n].0.is_none() {
                                out[n].0 = key;
                            }
                        }
                    }
                }
            }
        }
        Ty::Dict(k, v) => {
            if let Some(t) = item.as_table_like() {
                for (text, child) in t.iter() {
                    let key = t.get_key_value(text).and_then(|(key, _)| key.span());
                    let reads = matches!(
                        key_text(types, k, text, &mut Default::default()),
                        Some(Some(_))
                    );
                    if reads {
                        let n = out.len();
                        faults(types, v, child, &joined(at, &format!("{text:?}: ")), out);
                        if out.len() > n && out[n].0.is_none() {
                            out[n].0 = key;
                        }
                        continue;
                    }
                    // The shell's own words for this key: the entry alone,
                    // read as the dictionary.
                    let mut one = toml_edit::Table::new();
                    one.insert(text, child.clone());
                    if let Err(m) = decode_item(types, ty, &toml_edit::Item::Table(one)) {
                        out.push((key.or_else(|| child.span()), joined(at, &m)));
                    }
                }
            }
        }
        Ty::List(e, _) => {
            let elems: Vec<toml_edit::Item> = match item {
                toml_edit::Item::ArrayOfTables(a) => a
                    .iter()
                    .map(|t| toml_edit::Item::Table(t.clone()))
                    .collect(),
                _ => match item.as_array() {
                    Some(a) => a
                        .iter()
                        .map(|v| toml_edit::Item::Value(v.clone()))
                        .collect(),
                    None => Vec::new(),
                },
            };
            for (i, el) in elems.iter().enumerate() {
                let n = out.len();
                faults(types, e, el, &joined(at, &format!("[{i}]")), out);
                if out.len() > n && out[n].0.is_none() {
                    out[n].0 = el.span();
                }
            }
        }
        _ => {}
    }
    // A plain value of the wrong type, or what fails only as a whole
    // (two dictionary keys that read as one): the decoder's reason here.
    if out.len() == before {
        out.push((item.span(), joined(at, &why)));
    }
}

/// `at`'s place and `m` as the decoder chains them: `mons[1]: scale`, a
/// record's field and a list's index joined by `: ` unless the rest is an
/// index, a dictionary's key always (its place ends in `: `).
fn joined(at: &str, m: &str) -> String {
    if at.is_empty() || at.ends_with(": ") || m.starts_with('[') {
        format!("{at}{m}")
    } else {
        format!("{at}: {m}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config and the settings file beside it, checked as `strand
    /// check` checks them; the diagnostics' codes, and the line each
    /// one's TOML label is on (1-based; 0 for a label elsewhere).
    fn checked(src: &str, toml: Option<&str>) -> Vec<(&'static str, usize, String)> {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("strand-settings-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(t) = toml {
            std::fs::write(dir.join("s.toml"), t).unwrap();
        }
        let mut map = SourceMap::new();
        map.add("a.strand", src.to_string());
        let c = crate::compile(&map);
        assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
        let diags = check(&c.program, Some(&dir), &mut map);
        let _ = std::fs::remove_dir_all(&dir);
        diags
            .iter()
            .map(|d| {
                let line = d
                    .primary()
                    .and_then(|l| {
                        let f = map.get(l.file)?;
                        f.name
                            .ends_with("s.toml")
                            .then(|| f.text[..l.span.start as usize].matches('\n').count() + 1)
                    })
                    .unwrap_or(0);
                (d.code, line, d.message.clone())
            })
            .collect()
    }

    const PROFILES: &str = "type Mon { name: text; scale: float; on: bool }\n\
        state mons from \"s.toml\" {\n  \
          profiles: [text: [Mon]] = []\n  \
          compact: bool = false\n\
        }\n";

    /// A file that loads whole, in the dictionary-of-records form the
    /// monitor-profiles page writes (`[profiles."a + b"]` with
    /// `[[profiles."a + b".mons]]`… here a dictionary of lists), gives
    /// nothing; so does a missing one.
    #[test]
    fn a_good_or_missing_file_is_quiet() {
        let good = "compact = true\n\n\
            [[profiles.\"a + b\"]]\nname = \"a\"\nscale = 1.5\n\n\
            [[profiles.\"a + b\"]]\nname = \"b\"\non = false\n";
        assert_eq!(checked(PROFILES, Some(good)), []);
        assert_eq!(checked(PROFILES, None), []);
    }

    /// A typo'd key inside a record is a warning on that key's line,
    /// naming where it is as the shell's notice does.
    #[test]
    fn a_typod_key_is_a_warning_on_its_line() {
        let toml = "compact = true\n\n\
            [[profiles.\"a + b\"]]\nname = \"a\"\n\n\
            [[profiles.\"a + b\"]]\nname = \"b\"\nscael = 2.0\n";
        let got = checked(PROFILES, Some(toml));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "check::settings_value");
        assert_eq!(got[0].1, 8, "{got:?}");
        assert!(
            got[0]
                .2
                .contains(r#"`profiles`: "a + b": [1]: unknown field "scael""#),
            "{got:?}"
        );
        assert!(got[0].2.contains("keeps its last good value"), "{got:?}");
    }

    /// A value of the wrong type, deep or at the top, is a warning on its
    /// own line; another field still loads, and is not reported.
    #[test]
    fn a_wrong_type_is_a_warning_on_its_line() {
        let toml = "compact = \"yes\"\n\n\
            [[profiles.\"a + b\"]]\nname = \"a\"\nscale = \"big\"\n";
        let got = checked(PROFILES, Some(toml));
        let lines: Vec<_> = got.iter().map(|g| (g.0, g.1)).collect();
        assert_eq!(
            lines,
            [("check::settings_value", 5), ("check::settings_value", 1)],
            "{got:?}"
        );
        assert!(got[0].2.contains("scale: expected float"), "{got:?}");
        assert!(got[1].2.contains("`compact`: expected bool"), "{got:?}");
    }

    /// A dictionary of records written as sections, `[profiles."a + b"]`
    /// with `[[profiles."a + b".mons]]` tables, as the monitor-profiles
    /// page has it: good, then a wrong type in its second monitor.
    #[test]
    fn a_dictionary_of_records_with_tables_is_read() {
        let src = "type Mon { name: text; scale: float }\n\
            type Profile { gaps: int; mons: [Mon] }\n\
            state mons from \"s.toml\" { profiles: [text: Profile] = [] }\n";
        let good = "[profiles.\"a + b\"]\ngaps = 4\n\n\
            [[profiles.\"a + b\".mons]]\nname = \"a\"\n\n\
            [[profiles.\"a + b\".mons]]\nname = \"b\"\nscale = 2\n";
        assert_eq!(checked(src, Some(good)), []);
        let bad = good.replace("scale = 2", "scale = true");
        let got = checked(src, Some(&bad));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!((got[0].0, got[0].1), ("check::settings_value", 9));
        assert!(
            got[0]
                .2
                .contains(r#""a + b": mons[1]: scale: expected float"#),
            "{got:?}"
        );
        // A dictionary key that does not read as the key type.
        let src = src.replace("[text: Profile]", "[int: Profile]");
        let got = checked(&src, Some(good));
        assert_eq!(
            (got[0].0, got[0].1),
            ("check::settings_value", 1),
            "{got:?}"
        );
    }

    /// Several problems in one field, two in one record and one in
    /// another entry of the dictionary, are each reported on its own
    /// line, not only the first the decoder stops at; two keys that read
    /// as one fail only as a whole and are reported at the dictionary.
    #[test]
    fn every_problem_in_a_field_is_reported() {
        let src = "type Mon { name: text; scale: float; on: bool }\n\
            type Profile { gaps: int; mons: [Mon] }\n\
            state mons from \"s.toml\" { profiles: [text: Profile] = [] }\n";
        let toml = "[profiles.\"a + b\"]\ngaps = 4\n\n\
            [[profiles.\"a + b\".mons]]\nname = \"a\"\non = \"yes\"\nwidht = 3\n\n\
            [profiles.c]\ngaps = \"wide\"\n";
        let got = checked(src, Some(toml));
        let lines: Vec<_> = got.iter().map(|g| (g.0, g.1)).collect();
        assert_eq!(
            lines,
            [
                ("check::settings_value", 6),
                ("check::settings_value", 7),
                ("check::settings_value", 10),
            ],
            "{got:?}"
        );
        assert!(
            got[0].2.contains(r#""a + b": mons[0]: on: expected bool"#),
            "{got:?}"
        );
        assert!(
            got[1]
                .2
                .contains(r#""a + b": mons[0]: unknown field "widht""#),
            "{got:?}"
        );
        assert!(got[2].2.contains(r#""c": gaps: expected int"#), "{got:?}");
        // `3` and `03` are one `int` key: the dictionary fails as a whole.
        let src = "state s from \"s.toml\" { sizes: [int: int] = [] }\n";
        let got = checked(src, Some("[sizes]\n3 = 1\n03 = 2\n"));
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].2.contains(r#"key "03": the same key as "3""#),
            "{got:?}"
        );
    }

    /// Not TOML: an error at the parser's place; a key no declaration
    /// has: a warning with a did-you-mean.
    #[test]
    fn syntax_errors_and_unknown_keys() {
        let got = checked(PROFILES, Some("compact = true\nprofiles = [\n"));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "check::settings_syntax");
        assert!(got[0].2.contains("is not valid TOML"), "{got:?}");
        assert!(got[0].1 >= 2, "{got:?}");
        let got = checked(PROFILES, Some("compat = true\n"));
        assert_eq!(
            got.iter().map(|g| (g.0, g.1)).collect::<Vec<_>>(),
            [("check::settings_unknown_key", 1)]
        );
        assert!(got[0].2.contains("`compat` is not a setting of `mons`"));
    }
}
