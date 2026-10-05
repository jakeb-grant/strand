//! Hover, go-to-definition and rename: what a name at an offset refers to.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
use strand_compiler::FileId;
use strand_compiler::hir::{
    Callee, DefId, ElementKind, ExprKind, LocalId, LocalKind, Program, Target,
};
use strand_compiler::schema::{DocKey, Schema};
use strand_compiler::syntax::Span;
use strand_compiler::syntax::ast;
use strand_compiler::ty::{Origin, Ty};

use crate::describe;
use crate::walk;
use crate::workspace::Analysis;

/// What sits at an offset.
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    /// A resolved name.
    Ref(Target),
    /// A component parameter written as a prop at a call site
    /// (`Toast n { dense: true }`).
    CallProp(LocalId),
    /// A builtin prop of an element kind.
    Prop {
        element: String,
        name: String,
        ty: Ty,
    },
}

/// The name at `offset` and its span.
pub fn find(an: &Analysis, file: FileId, offset: u32) -> Option<(Span, Found)> {
    let p = &an.compiled.program;
    if let Some(r) = p.reference_at(file, offset) {
        return Some((r.span, Found::Ref(r.target.clone())));
    }
    // A token's key where a token set defines it.
    if let Some((_, d)) = walk::token_defs(p)
        .into_iter()
        .find(|(f, d)| *f == file && d.span.start <= offset && offset <= d.span.end)
    {
        return Some((d.span, Found::Ref(Target::Token(d.path.clone()))));
    }
    for (f, owner, props) in walk::prop_lists(p) {
        if f != file {
            continue;
        }
        let Some(prop) = props
            .iter()
            .find(|q| q.span.start <= offset && offset <= q.span.end)
        else {
            continue;
        };
        let Some(owner) = owner else { continue };
        match &owner.kind {
            ElementKind::Component(d) => {
                let c = walk::component(p, *d)?;
                let l = c
                    .params
                    .iter()
                    .find(|q| p.local(q.local).name == prop.name)?;
                return Some((prop.span, Found::CallProp(l.local)));
            }
            ElementKind::Builtin(k) => {
                let ty = an
                    .schema
                    .element(k)
                    .and_then(|e| e.prop(&prop.name))
                    .map_or(prop.value.ty.clone(), |s| s.ty.clone());
                return Some((
                    prop.span,
                    Found::Prop {
                        element: k.clone(),
                        name: prop.name.clone(),
                        ty,
                    },
                ));
            }
            ElementKind::Unknown(_) => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Hover

pub fn hover(an: &Analysis, file: FileId, offset: u32) -> Option<Hover> {
    let p = &an.compiled.program;
    let schema = &*an.schema;
    let types = &p.types;
    let (span, value) = match find(an, file, offset) {
        Some((span, found)) => (span, describe_found(an, &found)),
        None => {
            // A method name, else the narrowest expression's type.
            let mut best: Option<(Span, String)> = None;
            for (f, e) in walk::exprs(p) {
                if f != file || !(e.span.start <= offset && offset <= e.span.end) {
                    continue;
                }
                if let ExprKind::Call {
                    callee:
                        Callee::Method {
                            receiver,
                            name,
                            overload,
                        },
                    ..
                } = &e.kind
                    && let Some(at) = method_span(an.text(file), receiver.span.end, name)
                    && at.start <= offset
                    && offset <= at.end
                {
                    let text = method_text(schema, types, &receiver.ty, name, *overload);
                    best = Some((at, text));
                    break;
                }
                if best.as_ref().is_none_or(|(s, _)| e.span.len() < s.len()) {
                    let code = format!(
                        "{}: {}",
                        e.span.text(an.text(file)).lines().next().unwrap_or(""),
                        describe::ty(types, &e.ty)
                    );
                    best = Some((e.span, describe::markdown(&code, None)));
                }
            }
            best?
        }
    };
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: Some(an.range(file, span)),
    })
}

/// Where a method name is written: after `receiver_end`, past `.`/`?.`.
fn method_span(src: &str, receiver_end: u32, name: &str) -> Option<Span> {
    let rest = src.get(receiver_end as usize..)?;
    let skip = rest.len()
        - rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == '?' || c == '.')
            .len();
    let start = receiver_end + skip as u32;
    src.get(start as usize..)?
        .starts_with(name)
        .then(|| Span::new(start, start + name.len() as u32))
}

fn method_text(
    schema: &Schema,
    types: &strand_compiler::ty::TypeTable,
    receiver: &Ty,
    name: &str,
    overload: usize,
) -> String {
    let mut t = receiver.non_null().clone();
    if let Ty::Async(inner) = t {
        t = *inner;
    }
    match &t {
        Ty::Record(r) => {
            let rec = types.record(*r);
            if let Some(m) = rec.method(name)
                && let Some(s) = m.sigs.get(overload).or(m.sigs.first())
            {
                return describe::markdown(
                    &format!("{}.{}", rec.name, describe::sig(types, name, s)),
                    schema.doc(&DocKey::Member(rec.name.clone(), name.into())),
                );
            }
        }
        Ty::Prim(p) => {
            let ty_name = match p {
                strand_compiler::ty::Prim::Path => "text",
                strand_compiler::ty::Prim::Int => "float",
                p => p.name(),
            };
            if let Some(m) = schema.methods_of(ty_name).iter().find(|m| m.name == name)
                && let Some(s) = m.sigs.get(overload).or(m.sigs.first())
            {
                return describe::markdown(
                    &format!("{ty_name}.{}", describe::sig(types, name, s)),
                    schema.doc(&DocKey::Method(ty_name.into(), name.into())),
                );
            }
        }
        _ => {}
    }
    describe::markdown(&format!("fn {name}"), None)
}

/// Hover text for what [`find`] found.
pub fn describe_found(an: &Analysis, found: &Found) -> String {
    let p = &an.compiled.program;
    let schema = &*an.schema;
    let types = &p.types;
    match found {
        Found::Ref(Target::Def(d)) => {
            let def = p.def(*d);
            describe::markdown(
                &describe::def(an, def),
                describe::def_doc(an, def.file, def.span).as_deref(),
            )
        }
        Found::Ref(Target::Local(l)) | Found::CallProp(l) => {
            let local = p.local(*l);
            let ty = describe::ty(types, &local.ty);
            let code = match local.kind {
                LocalKind::Param => format!("{}: {ty}  // parameter", local.name),
                LocalKind::LambdaParam => format!("{}: {ty}  // lambda parameter", local.name),
                LocalKind::EventParam => format!("{}: {ty}  // event parameter", local.name),
                LocalKind::ForBinding => format!("for {}: {ty}", local.name),
                LocalKind::Let => format!("let {}: {ty}", local.name),
                LocalKind::ElementScope => format!("{}: {ty}  // in scope here", local.name),
                LocalKind::Channel => format!("{}: {ty}  // colour channel", local.name),
                LocalKind::NodeId(_) => format!("id {}: {ty}", local.name),
            };
            describe::markdown(&code, None)
        }
        Found::Ref(Target::Service(name)) => {
            let (code, doc) = describe::service(schema, types, name);
            describe::markdown(&code, doc.as_deref())
        }
        Found::Ref(Target::Builtin(name)) => {
            if let Some(sigs) = schema.functions.get(name) {
                let code: Vec<String> =
                    sigs.iter().map(|s| describe::sig(types, name, s)).collect();
                describe::markdown(
                    &code.join("\n"),
                    schema.doc(&DocKey::Function(name.clone())),
                )
            } else {
                let ty = schema
                    .values
                    .get(name)
                    .map_or(String::new(), |t| describe::ty(types, t));
                describe::markdown(
                    &format!("value {name}: {ty}"),
                    schema.doc(&DocKey::Value(name.clone())),
                )
            }
        }
        Found::Ref(Target::Variant(e, v)) => {
            let en = types.enum_(*e);
            let variant = en.variants.get(*v as usize).map_or("", String::as_str);
            describe::markdown(&format!("{}.{variant}", en.name), None)
        }
        Found::Ref(Target::Token(path)) => {
            let ty = p
                .tokens
                .get(path)
                .cloned()
                .or_else(|| schema.tokens.get(path).map(|t| t.ty.clone()))
                .unwrap_or(Ty::Error);
            let mut code = format!("${path}: {}", describe::ty(types, &ty));
            for (f, d) in walk::token_defs(p) {
                if d.path == *path {
                    let value = d.value.span.text(an.text(f));
                    let set = token_set_of(p, f, d.span);
                    let over = if d.override_ { " (override)" } else { "" };
                    let _ = write!(code, "\n{set}{over}: {value}");
                }
            }
            describe::markdown(&code, schema.token_doc(path))
        }
        Found::Ref(Target::Field(r, name)) if types.record(*r).field(name).is_none() => {
            method_text(schema, types, &Ty::Record(*r), name, 0)
        }
        Found::Ref(Target::Field(r, name)) => {
            let rec = types.record(*r);
            let (ty, rw) = rec.field(name).map_or((String::new(), false), |f| {
                (describe::ty(types, &f.ty), f.rw)
            });
            describe::markdown(
                &format!("{}.{name}: {ty}{}", rec.name, if rw { " rw" } else { "" }),
                schema.doc(&DocKey::Member(rec.name.clone(), name.clone())),
            )
        }
        Found::Ref(Target::Element(kind)) => {
            let code = match schema.element(kind).and_then(|e| e.arg.as_ref()) {
                Some(t) => format!("element {kind}({})", describe::ty(types, t)),
                None => format!("element {kind}"),
            };
            describe::markdown(&code, schema.doc(&DocKey::Element(kind.clone())))
        }
        Found::Ref(Target::File(f)) => {
            let name = an.map.get(*f).map_or("", |s| s.name.as_str());
            describe::markdown(
                &format!("file {}", strand_compiler::module_name(name)),
                Some(name),
            )
        }
        Found::Prop { element, name, ty } => describe::markdown(
            &format!("{name}: {}", describe::ty(types, ty)),
            schema.doc(&DocKey::Prop(element.clone(), name.clone())),
        ),
    }
}

/// Where the token defined at `span` is: its token set's name, the
/// component whose `tokens` block holds it, or `set` for a subtree's.
fn token_set_of(p: &Program, file: FileId, span: Span) -> String {
    let holds = |entries: &[strand_compiler::hir::TokenDef]| entries.iter().any(|d| d.span == span);
    for item in p
        .files
        .iter()
        .filter(|h| h.file == file)
        .flat_map(|h| &h.items)
    {
        match item {
            strand_compiler::hir::Item::Tokens(t) if holds(&t.entries) => {
                return p.def(t.def).name.clone();
            }
            strand_compiler::hir::Item::Component(c) if holds(&c.tokens) => {
                return p.def(c.def).name.clone();
            }
            _ => {}
        }
    }
    "set".into()
}

// ---------------------------------------------------------------------------
// Go to definition

pub fn definition(an: &Analysis, file: FileId, offset: u32) -> Vec<(FileId, Span)> {
    let p = &an.compiled.program;
    let Some((_, found)) = find(an, file, offset) else {
        return Vec::new();
    };
    match found {
        Found::Ref(Target::Def(d)) => {
            let def = p.def(d);
            vec![(def.file, def.span)]
        }
        Found::Ref(Target::Local(l)) | Found::CallProp(l) => {
            let local = p.local(l);
            vec![(local.file, local.span)]
        }
        Found::Ref(Target::Token(path)) => walk::token_defs(p)
            .into_iter()
            .filter(|(_, d)| d.path == path)
            .map(|(f, d)| (f, d.span))
            .collect(),
        Found::Ref(Target::Field(r, name)) => match p.types.record(r).origin {
            Origin::User(f, decl) => user_field(an, f, decl, &name).into_iter().collect(),
            Origin::Schema => Vec::new(),
        },
        Found::Ref(Target::Variant(e, v)) => match p.types.enum_(e).origin {
            Origin::User(f, decl) => user_variant(an, f, decl, v).into_iter().collect(),
            Origin::Schema => Vec::new(),
        },
        Found::Ref(Target::File(f)) => vec![(f, Span::at(0))],
        _ => Vec::new(),
    }
}

/// The field `name` of the record declared at `decl` (a `type`, a settings
/// file or a custom service).
fn user_field(an: &Analysis, file: FileId, decl: Span, name: &str) -> Option<(FileId, Span)> {
    let parse = an.parse(file)?;
    let mut found = None;
    each_item(&parse.file.items, &mut |it| {
        let fields: Vec<&ast::Field> = match &it.kind {
            ast::ItemKind::Type(t) if t.name.span == decl => t.fields.items.iter().collect(),
            ast::ItemKind::State(s) if s.name.span == decl => match &s.init {
                ast::StateInit::File { fields, .. } => fields.items.iter().collect(),
                ast::StateInit::Value { .. } => Vec::new(),
            },
            ast::ItemKind::Service(s) if s.name.span == decl => s
                .body
                .items
                .iter()
                .filter_map(|i| match &i.kind {
                    ast::ItemKind::Field(f) => Some(f),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if let Some(f) = fields.iter().find(|f| f.name.name == name) {
            found = Some((file, f.name.span));
        }
    });
    found
}

fn user_variant(an: &Analysis, file: FileId, decl: Span, v: u32) -> Option<(FileId, Span)> {
    let parse = an.parse(file)?;
    let mut found = None;
    each_item(&parse.file.items, &mut |it| {
        if let ast::ItemKind::Enum(e) = &it.kind
            && e.name.span == decl
            && let Some(id) = e.variants.get(v as usize)
        {
            found = Some((file, id.span));
        }
    });
    found
}

/// Top-level items and the items of tree blocks, recursively.
fn each_item<'a>(items: &'a [ast::Item], f: &mut impl FnMut(&'a ast::Item)) {
    for it in items {
        f(it);
        let blocks: Vec<&'a [ast::Item]> = match &it.kind {
            ast::ItemKind::Component(c) => vec![&c.body.items],
            ast::ItemKind::Surface(s) => vec![&s.body.items],
            ast::ItemKind::Element(e) => e.block.iter().map(|b| b.items.as_slice()).collect(),
            ast::ItemKind::If(i) => vec![&i.then.items],
            ast::ItemKind::For(fr) => vec![&fr.body.items],
            _ => Vec::new(),
        };
        for b in blocks {
            each_item(b, f);
        }
    }
}

// ---------------------------------------------------------------------------
// Rename

/// Edits by file: (span, new text).
pub type Edits = BTreeMap<FileId, Vec<(Span, String)>>;

/// The span and current text of the renamable name at `offset`.
pub fn prepare_rename(an: &Analysis, file: FileId, offset: u32) -> Result<(Span, String), String> {
    let (span, found) = find(an, file, offset).ok_or("nothing to rename here")?;
    renamable(an, &found)?;
    let text = span.text(an.text(file));
    Ok((span, text.trim_start_matches('$').to_string()))
}

fn renamable(an: &Analysis, found: &Found) -> Result<(), String> {
    let p = &an.compiled.program;
    match found {
        Found::Ref(Target::Def(_)) | Found::CallProp(_) => Ok(()),
        Found::Ref(Target::Local(l)) => match p.local(*l).kind {
            LocalKind::ElementScope | LocalKind::Channel => {
                Err(format!("`{}` is built in", p.local(*l).name))
            }
            _ => Ok(()),
        },
        Found::Ref(Target::Token(path)) => {
            if an.schema.tokens.contains_key(path) {
                Err(format!(
                    "`${path}` is a palette role or base token the schema names; it cannot be renamed"
                ))
            } else {
                Ok(())
            }
        }
        Found::Ref(Target::Service(n) | Target::Builtin(n) | Target::Element(n)) => {
            Err(format!("`{n}` is built in"))
        }
        Found::Ref(Target::File(_)) => Err("rename the file to rename its namespace".into()),
        Found::Ref(Target::Variant(..) | Target::Field(..)) | Found::Prop { .. } => {
            Err("only states, lets, declarations, parameters and tokens can be renamed".into())
        }
    }
}

/// Renames the name at `offset` everywhere in the config.
pub fn rename(an: &Analysis, file: FileId, offset: u32, new_name: &str) -> Result<Edits, String> {
    let p = &an.compiled.program;
    let (_, found) = find(an, file, offset).ok_or("nothing to rename here")?;
    renamable(an, &found)?;
    let mut edits = Edits::new();
    let mut add = |f: FileId, s: Span, t: String| {
        let v = edits.entry(f).or_default();
        if !v.iter().any(|(x, _)| *x == s) {
            v.push((s, t));
        }
    };
    // The new name as references see it (a token's whole path).
    let mut new_path = new_name.trim_start_matches('$').to_string();
    match &found {
        Found::Ref(Target::Token(path)) => {
            let new = new_name.trim_start_matches('$');
            if !is_token_path(new) {
                return Err(format!("`{new_name}` is not a token name"));
            }
            let mut spots: Vec<(FileId, Span)> = p
                .refs
                .iter()
                .filter(|r| r.target == Target::Token(path.clone()))
                .map(|r| (r.file, r.span))
                .collect();
            spots.extend(
                walk::token_defs(p)
                    .into_iter()
                    .filter(|(_, d)| d.path == *path)
                    .map(|(f, d)| (f, d.span)),
            );
            // Renamed from its key inside a group (`soft` in `glow { soft:
            // … }`), a bare name is the new key in the same group.
            let group = spots.iter().find_map(|(f, s)| {
                let key = s.text(an.text(*f)).trim_start_matches('$');
                (key != path).then(|| path.strip_suffix(key)).flatten()
            });
            if let Some(prefix) = group.filter(|p| !p.is_empty())
                && !new.contains('.')
            {
                new_path = format!("{prefix}{new}");
            }
            let new = new_path.as_str();
            for (f, s) in spots {
                let text = s.text(an.text(f));
                let dollar = text.starts_with('$');
                let key = text.trim_start_matches('$');
                let replacement = if key == path {
                    new.to_string()
                } else {
                    // A key inside a group: `2` in `space { 2: 8px }`.
                    let prefix = path.strip_suffix(key).unwrap_or("");
                    match new.strip_prefix(prefix) {
                        Some(rest) if !prefix.is_empty() && !rest.is_empty() => rest.to_string(),
                        _ => {
                            return Err(format!(
                                "`${path}` is defined inside the `{}` group; its new name must stay in it (`${prefix}…`)",
                                prefix.trim_end_matches('.')
                            ));
                        }
                    }
                };
                add(
                    f,
                    s,
                    format!("{}{replacement}", if dollar { "$" } else { "" }),
                );
            }
        }
        Found::Ref(Target::Def(d)) => {
            if !is_ident(new_name) {
                return Err(format!("`{new_name}` is not a name"));
            }
            for r in p.refs.iter().filter(|r| r.target == Target::Def(*d)) {
                add(r.file, r.span, new_name.to_string());
            }
        }
        Found::Ref(Target::Local(l)) | Found::CallProp(l) => {
            if !is_ident(new_name) {
                return Err(format!("`{new_name}` is not a name"));
            }
            for r in p.refs.iter().filter(|r| r.target == Target::Local(*l)) {
                add(r.file, r.span, new_name.to_string());
            }
            let local = p.local(*l);
            add(local.file, local.span, new_name.to_string());
            // A component parameter is also written as a prop where the
            // component is called.
            if let Some(comp) = owning_component(p, *l) {
                for (f, owner, props) in walk::prop_lists(p) {
                    if owner.is_some_and(|o| o.kind == ElementKind::Component(comp)) {
                        for q in props.iter().filter(|q| q.name == local.name) {
                            add(f, q.span, new_name.to_string());
                        }
                    }
                }
            }
        }
        _ => return Err("cannot rename this".into()),
    }
    // A rename must not break the config.
    let before = an.compiled.errors();
    let after = an.with_texts(|f, text| match edits.get(&f) {
        Some(list) => apply(&text, list).into(),
        None => text,
    });
    if after.compiled.errors() > before {
        let first = after
            .compiled
            .diagnostics
            .iter()
            .find(|d| {
                d.is_error()
                    && !an
                        .compiled
                        .diagnostics
                        .iter()
                        .any(|o| o.message == d.message)
            })
            .or_else(|| after.compiled.diagnostics.iter().find(|d| d.is_error()))
            .map_or(String::new(), |d| d.message.clone());
        return Err(format!(
            "renaming to `{new_name}` would break the config: {first}"
        ));
    }
    // Nor change what any other name means: a new name that shadows
    // another (or is shadowed) moves references between declarations.
    let renamed = |t: &Target| match (&found, t) {
        (Found::Ref(Target::Def(d)), Target::Def(x)) => d == x,
        (Found::Ref(Target::Local(l)) | Found::CallProp(l), Target::Local(x)) => l == x,
        (Found::Ref(Target::Token(a)), Target::Token(b)) => a == b,
        _ => false,
    };
    let new_name = new_path.as_str();
    let was = ref_counts(p, |t| renamed(t).then_some(new_name));
    let now = ref_counts(&after.compiled.program, |_| None);
    if was != now {
        return Err(format!(
            "renaming to `{new_name}` would change what other names refer to"
        ));
    }
    Ok(edits)
}

/// How many references each declaration has, by a description that does
/// not depend on ids or positions; `rename` gives the new name of the
/// target being renamed.
fn ref_counts<'n>(
    p: &Program,
    rename: impl Fn(&Target) -> Option<&'n str>,
) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for r in &p.refs {
        let new = rename(&r.target);
        let key = match &r.target {
            Target::Def(d) => {
                let def = p.def(*d);
                format!(
                    "def {:?} {:?} {}",
                    def.file,
                    def.kind,
                    new.unwrap_or(&def.name)
                )
            }
            Target::Local(l) => {
                let local = p.local(*l);
                format!(
                    "local {:?} {:?} {}",
                    local.file,
                    std::mem::discriminant(&local.kind),
                    new.unwrap_or(&local.name)
                )
            }
            Target::Token(path) => format!("token {}", new.unwrap_or(path)),
            other => format!("{other:?}"),
        };
        *out.entry(key).or_insert(0) += 1;
    }
    out
}

/// The component whose parameter `l` is.
fn owning_component(p: &Program, l: LocalId) -> Option<DefId> {
    p.files.iter().flat_map(|f| &f.items).find_map(|i| match i {
        strand_compiler::hir::Item::Component(c) if c.params.iter().any(|q| q.local == l) => {
            Some(c.def)
        }
        _ => None,
    })
}

/// `text` with the edits applied (spans in the original text).
pub fn apply(text: &str, edits: &[(Span, String)]) -> String {
    let mut sorted: Vec<&(Span, String)> = edits.iter().collect();
    sorted.sort_by_key(|(s, _)| std::cmp::Reverse(s.start));
    let mut out = text.to_string();
    for (s, t) in sorted {
        if out.is_char_boundary(s.start as usize) && out.is_char_boundary(s.end as usize) {
            out.replace_range(s.range(), t);
        }
    }
    out
}

pub fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    b.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && s != "_"
}

fn is_token_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|seg| is_ident(seg) || !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
}
