//! Text for hovers and completion details: signatures, declarations and
//! the docs the schema and the config's comments carry.

use std::fmt::Write as _;

use strand_compiler::FileId;
use strand_compiler::hir::{Def, DefKind};
use strand_compiler::schema::{DocKey, Schema};
use strand_compiler::syntax::Span;
use strand_compiler::ty::{FnSig, Ty, TypeTable};

use crate::walk;
use crate::workspace::Analysis;

pub fn ty(types: &TypeTable, t: &Ty) -> String {
    types.show(t).to_string()
}

/// `fn name(a: T, b: U = 1) -> R`, `action name()`.
pub fn sig(types: &TypeTable, name: &str, s: &FnSig) -> String {
    let mut out = format!("{} {name}(", if s.action { "action" } else { "fn" });
    for (i, p) in s.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if p.variadic {
            out.push_str("...");
        }
        let _ = write!(out, "{}: {}", p.name, ty(types, &p.ty));
        if let Some(d) = &p.default {
            let _ = write!(out, " = {d}");
        }
    }
    out.push(')');
    if s.ret != Ty::Unit {
        let _ = write!(out, " -> {}", ty(types, &s.ret));
    }
    out
}

/// A fenced code block followed by docs.
pub fn markdown(code: &str, doc: Option<&str>) -> String {
    let mut out = format!("```strand\n{code}\n```");
    if let Some(d) = doc.filter(|d| !d.trim().is_empty()) {
        out.push_str("\n\n");
        out.push_str(d.trim());
    }
    out
}

/// The `//` comment lines directly above the line holding `span`, and
/// where the first of them starts.
pub fn leading_comments(src: &str, span: Span) -> Option<(usize, String)> {
    let start = (span.start as usize).min(src.len());
    let line_start = src[..start].rfind('\n').map_or(0, |i| i + 1);
    let mut lines = Vec::new();
    let mut end = line_start;
    while end > 0 {
        let prev_start = src[..end - 1].rfind('\n').map_or(0, |i| i + 1);
        let line = src[prev_start..end - 1].trim();
        match line.strip_prefix("//") {
            Some(c) => lines.push(c.trim().to_string()),
            None => break,
        }
        end = prev_start;
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some((end, lines.join("\n")))
}

/// The declaration of a config name, as hover shows it.
pub fn def(an: &Analysis, def: &Def) -> String {
    let p = &an.compiled.program;
    let types = &p.types;
    let export = if def.exported { "export " } else { "" };
    let src = an.text(def.file);
    match &def.kind {
        DefKind::Component => {
            let id = p
                .defs
                .iter()
                .position(|d| d == def)
                .map(|i| strand_compiler::hir::DefId(i as u32));
            let params = id
                .and_then(|id| walk::component(p, id))
                .map(|c| {
                    c.params
                        .iter()
                        .map(|q| {
                            let l = p.local(q.local);
                            let mut s = format!("{}: {}", l.name, ty(types, &l.ty));
                            if let Some(d) = &q.default {
                                let _ = write!(s, " = {}", d.span.text(src));
                            }
                            s
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if params.is_empty() {
                format!("component {}", def.name)
            } else {
                format!("component {}({})", def.name, params.join(", "))
            }
        }
        DefKind::Surface(kind) => format!("{kind} {}", def.name),
        DefKind::State => format!("{export}state {}: {}", def.name, ty(types, &def.ty)),
        DefKind::Settings => {
            let mut s = format!("state {} from settings", def.name);
            if let Ty::Record(r) = &def.ty {
                let rec = types.record(*r);
                s = format!("state {} from settings {{", def.name);
                for f in &rec.fields {
                    let _ = write!(s, "\n  {}: {}", f.name, ty(types, &f.ty));
                }
                s.push_str("\n}");
            }
            s
        }
        DefKind::Let => format!("{export}let {}: {}", def.name, ty(types, &def.ty)),
        DefKind::Fn => match &def.ty {
            Ty::Fn(s) => sig(types, &def.name, s),
            t => format!("fn {}: {}", def.name, ty(types, t)),
        },
        DefKind::Enum(e) => {
            let en = types.enum_(*e);
            format!("enum {} {{ {} }}", en.name, en.variants.join(", "))
        }
        DefKind::Type(r) | DefKind::Service(r) => {
            let rec = types.record(*r);
            let head = if matches!(def.kind, DefKind::Type(_)) {
                format!("type {}", def.name)
            } else {
                format!("service {}", def.name)
            };
            record_body(types, head, *r).unwrap_or_else(|| rec.name.clone())
        }
        DefKind::Tokens => format!("tokens {}", def.name),
        DefKind::Keyframes => format!("keyframes {}", def.name),
    }
}

fn record_body(
    types: &TypeTable,
    head: String,
    r: strand_compiler::ty::RecordId,
) -> Option<String> {
    let rec = types.record(r);
    let mut s = format!("{head} {{");
    for f in &rec.fields {
        let _ = write!(
            s,
            "\n  {}: {}{}",
            f.name,
            ty(types, &f.ty),
            if f.rw { " rw" } else { "" }
        );
    }
    s.push_str("\n}");
    Some(s)
}

/// A schema service's record, with its doc.
pub fn service(schema: &Schema, types: &TypeTable, name: &str) -> (String, Option<String>) {
    let code = match types.find_record(name).or_else(|| schema.service(name)) {
        Some(r) => format!("service {name}: {}", types.record(r).name),
        None => format!("service {name}"),
    };
    (
        code,
        schema
            .doc(&DocKey::Type(name.to_string()))
            .map(String::from),
    )
}

/// The doc comment of a config declaration, or of a schema entry.
///
/// A comment block that opens the file and starts with the file's own name
/// (`// launcher.strand. Bind a key to: …`) is the file's header, not the
/// doc of the declaration under it.
pub fn def_doc(an: &Analysis, file: FileId, span: Span) -> Option<String> {
    let (start, doc) = leading_comments(an.text(file), span)?;
    let name = an.map.get(file).map_or("", |f| f.name.as_str());
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let header = start == 0 && !base.is_empty() && doc.starts_with(base);
    (!header).then_some(doc)
}
