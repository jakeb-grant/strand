//! Completion: token paths after `$`, members after `.` and `?.`, writable
//! places after `<->`, element, component, keyword and prop names at the
//! start of a tree item, events after `on`, and enum variants as a prop's
//! value.

use std::collections::BTreeSet;

use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionTextEdit, Documentation, MarkupContent,
    MarkupKind, Range, TextEdit,
};
use strand_compiler::FileId;
use strand_compiler::hir::{Callee, DefKind, ExprKind, Program, Target};
use strand_compiler::schema::{DocKey, Schema};
use strand_compiler::ty::{Prim, Ty, TypeTable};

use crate::ctx::{self, Block, line_before, word_before, word_end};
use crate::describe;
use crate::walk;
use crate::workspace::Analysis;

/// Stands in for the name being typed after a `.`, so the text parses.
const PLACEHOLDER: &str = "__strand_complete__";

/// Keywords at the start of a top-level item.
const TOP_KEYWORDS: &[&str] = &[
    "component",
    "bar",
    "panel",
    "osd",
    "lock",
    "state",
    "let",
    "export",
    "enum",
    "type",
    "fn",
    "tokens",
    "use",
    "service",
    "permit",
    "keyframes",
    "on",
    "after",
    "every",
];

/// Keywords at the start of a tree item.
const TREE_KEYWORDS: &[&str] = &[
    "when", "if", "match", "for", "on", "after", "every", "enter", "exit", "state", "let", "slot",
    "set", "play",
];

/// Keywords at the start of a handler statement.
const STMT_KEYWORDS: &[&str] = &["let", "if", "match", "for", "play"];

/// Methods every list has (the checker's list, `check/expr.rs`).
const LIST_METHODS: &[&str] = &[
    "filter",
    "map",
    "sort_by",
    "take",
    "skip",
    "reverse",
    "join",
    "contains",
    "any",
    "all",
    "count",
    "find",
    "push",
    "insert",
    "remove",
    "clear",
    "remove_key",
    "move",
    "update",
];

pub fn complete(an: &Analysis, file: FileId, offset: u32) -> Vec<CompletionItem> {
    let schema = Schema::builtin();
    let src = an.text(file);
    let off = (offset as usize).min(src.len());
    if !src.is_char_boundary(off) {
        return Vec::new();
    }
    let mut c = Completer {
        an,
        file,
        schema,
        items: Vec::new(),
        seen: BTreeSet::new(),
    };
    // `$path`: scan back over the path.
    let b = src.as_bytes();
    let mut ps = off;
    while ps > 0 && (ctx::is_word(b[ps - 1]) || b[ps - 1] == b'.') {
        ps -= 1;
    }
    if ps > 0 && b[ps - 1] == b'$' {
        c.tokens(ps, off);
        return c.items;
    }
    let (ws, word) = word_before(src, off);
    let we = word_end(src, off);
    let before = &src[..ws];
    if before.ends_with('.') && !before.ends_with("..") {
        let (_, receiver) = word_before(src, ws - 1);
        if !receiver.is_empty() && receiver.bytes().all(|b| b.is_ascii_digit()) {
            // `0.`: a number being typed.
            return c.items;
        }
        let optional = before.ends_with("?.");
        c.members(ws, we, word.is_empty(), optional);
        return c.items;
    }
    let line = line_before(src, ws);
    let seg = line.rsplit([';', '{']).next().unwrap_or("").trim();
    let range = an.range(
        file,
        strand_compiler::syntax::Span::new(ws as u32, we as u32),
    );
    let parse = an.parse(file);
    let Some(parse) = parse else {
        return c.items;
    };
    let cx = ctx::context(&parse.file, src, ws as u32);
    if seg.is_empty() {
        c.item_start(&cx, range);
    } else if seg == "on" {
        c.events(&cx, range);
    } else if let Some(prop) = seg.strip_suffix("<->").map(str::trim_end)
        && let Some(prop) = prop.strip_suffix(':')
        && is_ident(prop.trim())
    {
        c.writable(&cx, range);
    } else if let Some(prop) = seg.strip_suffix(':')
        && is_ident(prop.trim())
    {
        c.variants(&cx, prop.trim(), range);
    }
    c.items
}

fn is_ident(s: &str) -> bool {
    let mut b = s.bytes();
    b.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == b'_')
        && b.all(ctx::is_word)
}

struct Completer<'a> {
    an: &'a Analysis,
    file: FileId,
    schema: &'static Schema,
    items: Vec<CompletionItem>,
    /// Labels already offered.
    seen: BTreeSet<String>,
}

fn doc(text: Option<&str>) -> Option<Documentation> {
    text.filter(|t| !t.trim().is_empty()).map(|t| {
        Documentation::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: t.trim().to_string(),
        })
    })
}

impl<'a> Completer<'a> {
    fn program(&self) -> &'a Program {
        &self.an.compiled.program
    }

    fn types(&self) -> &'a TypeTable {
        &self.an.compiled.program.types
    }

    fn push(
        &mut self,
        label: impl Into<String>,
        kind: CompletionItemKind,
        detail: Option<String>,
        documentation: Option<&str>,
        edit: Option<(Range, String)>,
        sort: &str,
    ) {
        let label = label.into();
        if !self.seen.insert(label.clone()) {
            return;
        }
        self.items.push(CompletionItem {
            sort_text: Some(format!("{sort}{label}")),
            text_edit: edit
                .map(|(range, new_text)| CompletionTextEdit::Edit(TextEdit { range, new_text })),
            label,
            kind: Some(kind),
            detail,
            documentation: doc(documentation),
            ..CompletionItem::default()
        });
    }

    // -----------------------------------------------------------------------
    // `$tokens`

    fn tokens(&mut self, start: usize, end: usize) {
        let src = self.an.text(self.file);
        let end = src[end..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .map_or(src.len(), |i| end + i);
        let typed = &src[start..end];
        let range = self.an.range(
            self.file,
            strand_compiler::syntax::Span::new(start as u32, end as u32),
        );
        let p = self.program();
        let mut paths: Vec<(String, Ty)> = p
            .tokens
            .iter()
            .map(|(k, t)| (k.clone(), t.clone()))
            .collect();
        for (k, t) in &self.schema.tokens {
            if !p.tokens.contains_key(k) {
                paths.push((k.clone(), t.ty.clone()));
            }
        }
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, ty) in paths {
            let kind = if ty == Ty::COLOR {
                CompletionItemKind::COLOR
            } else {
                CompletionItemKind::CONSTANT
            };
            let detail = describe::ty(self.types(), &ty);
            let d = self.schema.doc(&DocKey::Token(path.clone()));
            self.push(
                path.clone(),
                kind,
                Some(detail),
                d,
                Some((range, path)),
                "0",
            );
        }
        // `$surface.al…`: methods of a colour token.
        if let Some((base, _)) = typed.rsplit_once('.')
            && let Some(t) = p
                .tokens
                .get(base)
                .cloned()
                .or_else(|| self.schema.tokens.get(base).map(|t| t.ty.clone()))
            && let Some(prim) = t.prim()
        {
            let seg_start = start + base.len() + 1;
            let range = self.an.range(
                self.file,
                strand_compiler::syntax::Span::new(seg_start as u32, end as u32),
            );
            for m in self.schema.methods_of(prim.name()) {
                for s in &m.sigs {
                    let label = describe::sig(self.types(), &m.name, s);
                    let d = self
                        .schema
                        .doc(&DocKey::Method(prim.name().into(), m.name.clone()));
                    let name = m.name.clone();
                    self.push_method(name, label, d, Some((range, m.name.clone())));
                }
            }
        }
    }

    fn push_method(
        &mut self,
        name: String,
        detail: String,
        d: Option<&str>,
        edit: Option<(Range, String)>,
    ) {
        if self.seen.contains(&name) {
            return;
        }
        self.push(name, CompletionItemKind::METHOD, Some(detail), d, edit, "1");
    }

    // -----------------------------------------------------------------------
    // `.` and `?.`

    /// Members after the dot ending at `name_start`; `empty` when no part
    /// of the name is typed yet.
    fn members(&mut self, name_start: usize, name_end: usize, empty: bool, optional: bool) {
        let src = self.an.text(self.file);
        let dot = name_start - if optional { 2 } else { 1 };
        // Inside `x: <-> place`: only what can be written.
        let line = line_before(src, dot);
        let seg = line.rsplit([';', '{']).next().unwrap_or("");
        let writable = seg.contains("<->");
        let owned;
        let (an, end): (&Analysis, usize) = if empty {
            let text = format!("{}{PLACEHOLDER}{}", &src[..name_start], &src[name_start..]);
            owned = self.an.with_text(self.file, &text);
            (&owned, name_start + PLACEHOLDER.len())
        } else {
            (self.an, name_end)
        };
        let p = &an.compiled.program;
        let text = an.text(self.file);
        // The expression the dot follows, typed.
        let mut base: Option<(Ty, bool)> = None;
        for (f, e) in walk::exprs(p) {
            if f != self.file {
                continue;
            }
            match &e.kind {
                ExprKind::Field { base: b, .. } if e.span.end as usize == end => {
                    base = Some((b.ty.clone(), root_is_state(p, b)));
                    break;
                }
                ExprKind::Call {
                    callee: Callee::Method { receiver, name, .. },
                    ..
                } if *name == text[name_start..end]
                    && text
                        .get(receiver.span.end as usize..name_start)
                        .is_some_and(|t| matches!(t.trim(), "." | "?.")) =>
                {
                    base = Some((receiver.ty.clone(), root_is_state(p, receiver)));
                    break;
                }
                _ => {}
            }
        }
        let range = self.an.range(
            self.file,
            strand_compiler::syntax::Span::new(name_start as u32, name_end as u32),
        );
        // What the word before the dot names, when there is no typed
        // expression (`Look.`, `theme.`).
        let (rs, receiver_word) = word_before(text, dot);
        let target = p
            .reference_at(self.file, rs as u32)
            .filter(|r| r.span.end as usize == dot)
            .map(|r| r.target.clone());
        match (&base, target) {
            (_, Some(Target::File(f))) => {
                for (path, d) in p.exports() {
                    let def = p.def(d);
                    if def.file == f {
                        let label = path.rsplit('.').next().unwrap_or(&path).to_string();
                        let detail = describe::def(an, def);
                        self.push(
                            label.clone(),
                            CompletionItemKind::VARIABLE,
                            Some(detail),
                            None,
                            Some((range, label)),
                            "0",
                        );
                    }
                }
                return;
            }
            (_, Some(Target::Def(d))) if matches!(p.def(d).kind, DefKind::Enum(_)) => {
                if let DefKind::Enum(e) = p.def(d).kind {
                    self.variants_of(e, range);
                }
                return;
            }
            (Some((Ty::Error, _)) | None, _) => {
                if let Some(e) = self.types().find_enum(receiver_word) {
                    self.variants_of(e, range);
                    return;
                }
            }
            _ => {}
        }
        if let Some((ty, state)) = base {
            self.members_of(&ty, range, writable, state);
        }
    }

    fn variants_of(&mut self, e: strand_compiler::ty::EnumId, range: Range) {
        let en = self.types().enum_(e).clone();
        for v in &en.variants {
            self.push(
                v.clone(),
                CompletionItemKind::ENUM_MEMBER,
                Some(en.name.clone()),
                None,
                Some((range, v.clone())),
                "0",
            );
        }
    }

    /// Fields and methods of a value of type `t`.
    fn members_of(&mut self, t: &Ty, range: Range, writable: bool, state: bool) {
        let types = self.types();
        let edit = |s: &str| Some((range, s.to_string()));
        match t {
            Ty::Optional(inner) => self.members_of(inner, range, writable, state),
            Ty::Async(inner) => {
                if !writable {
                    for (n, ty) in [
                        ("pending", Ty::BOOL),
                        ("error", Ty::TEXT.optional()),
                        ("value", (**inner).clone().optional()),
                    ] {
                        let detail = format!("{n}: {}", describe::ty(types, &ty));
                        self.push(
                            n,
                            CompletionItemKind::FIELD,
                            Some(detail),
                            None,
                            edit(n),
                            "0",
                        );
                    }
                }
                if inner.list_elem().is_some() {
                    self.members_of(inner, range, writable, state);
                }
            }
            Ty::List(elem, _) => {
                if writable {
                    return;
                }
                for (n, ty) in [
                    ("len", Ty::INT),
                    ("first", (**elem).clone().optional()),
                    ("last", (**elem).clone().optional()),
                ] {
                    let detail = format!("{n}: {}", describe::ty(types, &ty));
                    self.push(
                        n,
                        CompletionItemKind::FIELD,
                        Some(detail),
                        None,
                        edit(n),
                        "0",
                    );
                }
                for m in LIST_METHODS {
                    self.push(
                        *m,
                        CompletionItemKind::METHOD,
                        Some(format!("list method of [{}]", describe::ty(types, elem))),
                        None,
                        edit(m),
                        "1",
                    );
                }
            }
            Ty::Record(r) => {
                let rec = types.record(*r).clone();
                for f in &rec.fields {
                    if writable && !state && !f.rw && !leads_to_rw(types, &f.ty, 3) {
                        continue;
                    }
                    let detail = format!(
                        "{}: {}{}",
                        f.name,
                        describe::ty(types, &f.ty),
                        if f.rw { " rw" } else { "" }
                    );
                    let d = self
                        .schema
                        .doc(&DocKey::Member(rec.name.clone(), f.name.clone()));
                    self.push(
                        f.name.clone(),
                        CompletionItemKind::FIELD,
                        Some(detail),
                        d,
                        edit(&f.name),
                        "0",
                    );
                }
                if !writable {
                    for m in &rec.methods {
                        for s in &m.sigs {
                            let detail = describe::sig(types, &m.name, s);
                            let d = self
                                .schema
                                .doc(&DocKey::Member(rec.name.clone(), m.name.clone()));
                            self.push_method(m.name.clone(), detail, d, edit(&m.name));
                        }
                    }
                }
            }
            Ty::Prim(p) if !writable => {
                if matches!(p, Prim::Text | Prim::Path) {
                    self.push(
                        "len",
                        CompletionItemKind::FIELD,
                        Some("len: int".into()),
                        None,
                        edit("len"),
                        "0",
                    );
                }
                let ty_name = match p {
                    Prim::Path => "text",
                    Prim::Int => "float",
                    p => p.name(),
                };
                for m in self.schema.methods_of(ty_name) {
                    for s in &m.sigs {
                        let detail = describe::sig(types, &m.name, s);
                        let d = self
                            .schema
                            .doc(&DocKey::Method(ty_name.into(), m.name.clone()));
                        self.push_method(m.name.clone(), detail, d, edit(&m.name));
                    }
                }
            }
            Ty::EnumType(e) => self.variants_of(*e, range),
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Item starts

    fn item_start(&mut self, cx: &ctx::Context<'_>, range: Range) {
        let edit = |s: &str| Some((range, s.to_string()));
        match &cx.block {
            Block::TopLevel => {
                for k in TOP_KEYWORDS {
                    self.push(*k, CompletionItemKind::KEYWORD, None, None, edit(k), "2");
                }
            }
            Block::Statements => {
                for k in STMT_KEYWORDS {
                    self.push(*k, CompletionItemKind::KEYWORD, None, None, edit(k), "2");
                }
            }
            Block::Other => {}
            Block::Tree {
                element,
                props_only,
                sub_of,
            } => {
                if let Some(el) = element {
                    self.props(el, sub_of.as_deref(), range);
                }
                if *props_only {
                    return;
                }
                for k in TREE_KEYWORDS {
                    self.push(*k, CompletionItemKind::KEYWORD, None, None, edit(k), "2");
                }
                for (name, el) in &self.schema.elements {
                    if el.flags.surface {
                        continue;
                    }
                    if let Some(parent) = &el.flags.only_in
                        && element.as_deref() != Some(parent.as_str())
                    {
                        continue;
                    }
                    let detail = match &el.arg {
                        Some(t) => format!("{name}({})", describe::ty(self.types(), t)),
                        None => name.clone(),
                    };
                    let d = self.schema.doc(&DocKey::Element(name.clone()));
                    self.push(
                        name.clone(),
                        CompletionItemKind::CLASS,
                        Some(detail),
                        d,
                        edit(name),
                        "1",
                    );
                }
                let p = self.program();
                for d in &p.defs {
                    if d.kind == DefKind::Component {
                        let detail = describe::def(self.an, d);
                        let doc = describe::def_doc(self.an, d.file, d.span);
                        self.push(
                            d.name.clone(),
                            CompletionItemKind::MODULE,
                            Some(detail),
                            doc.as_deref(),
                            edit(&d.name),
                            "1",
                        );
                    }
                }
            }
        }
    }

    /// Props an element (or a component call) takes.
    fn props(&mut self, element: &str, sub_of: Option<&str>, range: Range) {
        let types = self.types();
        if let Some(el) = self.schema.element(element) {
            let props = match sub_of {
                Some(s) => el.prop(s).map_or(&[][..], |p| p.sub.as_slice()),
                None => el.props.as_slice(),
            };
            for prop in props {
                let detail = format!(
                    "{}: {}{}",
                    prop.name,
                    describe::ty(types, &prop.ty),
                    if prop.two_way { " <->" } else { "" }
                );
                let key = match sub_of {
                    Some(s) => format!("{s}.{}", prop.name),
                    None => prop.name.clone(),
                };
                let d = self.schema.doc(&DocKey::Prop(element.into(), key));
                self.push(
                    prop.name.clone(),
                    CompletionItemKind::PROPERTY,
                    Some(detail),
                    d,
                    Some((range, format!("{}: ", prop.name))),
                    "0",
                );
            }
            return;
        }
        let p = self.program();
        let Some(id) = p
            .defs
            .iter()
            .position(|d| d.kind == DefKind::Component && d.name == element)
        else {
            return;
        };
        if let Some(c) = walk::component(p, strand_compiler::hir::DefId(id as u32)) {
            for q in &c.params {
                let l = p.local(q.local);
                let detail = format!("{}: {}", l.name, describe::ty(types, &l.ty));
                self.push(
                    l.name.clone(),
                    CompletionItemKind::PROPERTY,
                    Some(detail),
                    None,
                    Some((range, format!("{}: ", l.name))),
                    "0",
                );
            }
        }
    }

    fn events(&mut self, cx: &ctx::Context<'_>, range: Range) {
        let edit = |s: &str| Some((range, s.to_string()));
        if let Block::Tree {
            element: Some(el), ..
        } = &cx.block
            && let Some(schema) = self.schema.element(el)
        {
            for e in &schema.events {
                let params: Vec<String> = e
                    .params
                    .iter()
                    .map(|p| format!("{}: {}", p.name, describe::ty(self.types(), &p.ty)))
                    .collect();
                let detail = if params.is_empty() {
                    format!("on {}", e.name)
                } else {
                    format!("on {}({})", e.name, params.join(", "))
                };
                let d = self
                    .schema
                    .doc(&DocKey::Prop(el.clone(), format!("on {}", e.name)));
                self.push(
                    e.name.clone(),
                    CompletionItemKind::EVENT,
                    Some(detail),
                    d,
                    edit(&e.name),
                    "0",
                );
            }
        }
        self.push(
            "change",
            CompletionItemKind::KEYWORD,
            Some("on change a, b after T".into()),
            None,
            edit("change"),
            "1",
        );
    }

    // -----------------------------------------------------------------------
    // `<->`

    /// Places a widget can write: state, settings fields, `rw` service
    /// fields.
    fn writable(&mut self, cx: &ctx::Context<'_>, range: Range) {
        let p = self.program();
        let types = self.types();
        let owner = cx.top.and_then(|it| {
            let name = match &it.kind {
                strand_compiler::syntax::ast::ItemKind::Component(c) => &c.name,
                strand_compiler::syntax::ast::ItemKind::Surface(s) => s.name.as_ref()?,
                _ => return None,
            };
            p.defs
                .iter()
                .position(|d| d.file == self.file && d.span == name.span)
                .map(|i| strand_compiler::hir::DefId(i as u32))
        });
        let file_name = |f: FileId| {
            p.files
                .iter()
                .find(|h| h.file == f)
                .map_or(String::new(), |h| h.name.clone())
        };
        let mut places: Vec<(String, Ty, &str)> = Vec::new();
        for d in &p.defs {
            let visible = d.file == self.file && (d.owner.is_none() || d.owner == owner);
            let path = if visible {
                d.name.clone()
            } else if d.exported && d.owner.is_none() {
                format!("{}.{}", file_name(d.file), d.name)
            } else {
                continue;
            };
            match d.kind {
                DefKind::State => places.push((path, d.ty.clone(), "state")),
                DefKind::Settings => {
                    if let Ty::Record(r) = d.ty {
                        for f in &types.record(r).fields {
                            places.push((format!("{path}.{}", f.name), f.ty.clone(), "setting"));
                        }
                    }
                }
                DefKind::Service(r) if visible || d.owner.is_none() => {
                    rw_paths(types, &d.name, r, 3, &mut |path, ty| {
                        places.push((path, ty, "service field"))
                    });
                }
                _ => {}
            }
        }
        for (name, r) in &self.schema.services {
            rw_paths(types, name, *r, 3, &mut |path, ty| {
                places.push((path, ty, "service field"))
            });
        }
        for (path, ty, what) in places {
            let detail = format!("{what}: {}", describe::ty(types, &ty));
            self.push(
                path.clone(),
                CompletionItemKind::VARIABLE,
                Some(detail),
                None,
                Some((range, path)),
                "0",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Prop values

    /// The variants of an enum-typed prop.
    fn variants(&mut self, cx: &ctx::Context<'_>, prop: &str, range: Range) {
        let Block::Tree {
            element: Some(el),
            sub_of,
            ..
        } = &cx.block
        else {
            return;
        };
        let Some(schema) = self.schema.element(el) else {
            return;
        };
        let p = match sub_of {
            Some(s) => schema
                .prop(s)
                .and_then(|p| p.sub.iter().find(|q| q.name == prop)),
            None => schema.prop(prop),
        };
        let Some(p) = p else {
            return;
        };
        let mut enums = Vec::new();
        collect_enums(&p.ty, &mut enums);
        for e in enums {
            self.variants_of(e, range);
        }
        if p.ty == Ty::BOOL {
            for v in ["true", "false"] {
                self.push(
                    v,
                    CompletionItemKind::KEYWORD,
                    None,
                    None,
                    Some((range, v.into())),
                    "0",
                );
            }
        }
    }
}

fn collect_enums(t: &Ty, out: &mut Vec<strand_compiler::ty::EnumId>) {
    match t {
        Ty::Enum(e) => out.push(*e),
        Ty::Optional(t) => collect_enums(t, out),
        Ty::Union(ts) => {
            for t in ts {
                collect_enums(t, out);
            }
        }
        _ => {}
    }
}

/// The base of a field chain is a `state` or settings record: everything
/// under it is writable.
fn root_is_state(p: &Program, e: &strand_compiler::hir::Expr) -> bool {
    match &e.kind {
        ExprKind::Field { base, .. } => root_is_state(p, base),
        ExprKind::Index { base, .. } => root_is_state(p, base),
        ExprKind::Def(d) => matches!(p.def(*d).kind, DefKind::State | DefKind::Settings),
        _ => false,
    }
}

/// A record reached through `t` has a writable field within `depth`.
fn leads_to_rw(types: &TypeTable, t: &Ty, depth: u32) -> bool {
    let Ty::Record(r) = t else {
        return false;
    };
    depth > 0
        && types
            .record(*r)
            .fields
            .iter()
            .any(|f| f.rw || leads_to_rw(types, &f.ty, depth - 1))
}

/// Paths to the `rw` fields of record `r` (named `base`), within `depth`.
fn rw_paths(
    types: &TypeTable,
    base: &str,
    r: strand_compiler::ty::RecordId,
    depth: u32,
    out: &mut impl FnMut(String, Ty),
) {
    if depth == 0 {
        return;
    }
    for f in &types.record(r).fields {
        let path = format!("{base}.{}", f.name);
        if f.rw {
            out(path.clone(), f.ty.clone());
        }
        if let Ty::Record(inner) = f.ty {
            rw_paths(types, &path, inner, depth - 1, out);
        }
    }
}
