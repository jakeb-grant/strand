//! The first passes: declare every name, then resolve the signatures other
//! files need (record fields, component parameters, fn signatures, token
//! sets), then the per-file walk and the checks that need the whole
//! program.

use super::{Binding, Checker, CompParam, CompSig, Ctx, DoneHir, LazyAst, LazyState, Pending};
use crate::diagnostic::suggest;
use crate::hir::{self, Def, DefId, DefKind, Target};
use crate::syntax::Span;
use crate::syntax::ast::{self, ItemKind};
use crate::ty::{EnumDef, FieldDef, FnSig, Origin, ParamSig, RecordDef, Ty};

impl<'a> Checker<'a> {
    pub(super) fn collect(&mut self) {
        for (i, m) in self.modules.iter().enumerate() {
            self.file_index.entry(m.name.to_string()).or_insert(i);
        }
        for i in 0..self.modules.len() {
            self.module = i;
            let items = &self.modules[i].ast.items;
            for item in items {
                self.collect_item(item);
            }
        }
    }

    /// `@reset` is the only attribute, and only on `state` and `let`.
    pub(super) fn reset_attr(&mut self, item: &ast::Item) -> bool {
        let mut reset = false;
        for a in &item.attrs {
            if a.name.name == "reset" {
                if matches!(item.kind, ItemKind::State(_) | ItemKind::Let(_)) {
                    reset = true;
                } else {
                    self.error(
                        "check::attribute",
                        "`@reset` goes on a `state` or `let`",
                        a.span,
                        "nothing here to reset",
                    );
                }
            } else {
                let help =
                    suggest(&a.name.name, ["reset"]).map(|s| format!("did you mean `@{s}`?"));
                self.error(
                    "check::attribute",
                    format!("unknown attribute `@{}`", a.name.name),
                    a.span,
                    "not an attribute",
                )
                .help = help;
            }
        }
        reset
    }

    fn collect_item(&mut self, item: &'a ast::Item) {
        let reset = self.reset_attr(item);
        let file = self.file();
        let user = |span| Origin::User(file, span);
        match &item.kind {
            ItemKind::Component(c) => {
                let id =
                    self.declare_global(&c.name, DefKind::Component, Ty::Unit, LazyState::Ready);
                if let Some(t) = &c.tokens {
                    let entries = self.tokens.add_component(id, self.module, &c.name.name, t);
                    self.comp_tokens.insert(id, entries);
                }
            }
            ItemKind::Surface(s) => {
                if let Some(n) = &s.name {
                    self.declare_global(
                        n,
                        DefKind::Surface(s.kind.name.clone()),
                        Ty::Unit,
                        LazyState::Ready,
                    );
                }
            }
            ItemKind::State(s) => {
                let kind = match s.init {
                    ast::StateInit::File { .. } => DefKind::Settings,
                    ast::StateInit::Value { .. } => DefKind::State,
                };
                let pending = self.pending(LazyAst::State(s, reset));
                self.declare_file(&s.name, kind, s.export.is_some(), pending);
            }
            ItemKind::Let(l) => {
                let pending = self.pending(LazyAst::Let(l, reset));
                self.declare_file(&l.name, DefKind::Let, l.export.is_some(), pending);
            }
            ItemKind::Enum(e) => {
                let mut seen: Vec<&str> = Vec::new();
                for v in &e.variants {
                    if seen.contains(&v.name.as_str()) {
                        self.error(
                            "check::redeclared",
                            format!("variant `{}` is listed twice", v.name),
                            v.span,
                            "listed again here",
                        );
                    }
                    seen.push(&v.name);
                }
                let id = self.types.add_enum(EnumDef {
                    name: e.name.name.clone(),
                    variants: e.variants.iter().map(|v| v.name.clone()).collect(),
                    origin: user(e.name.span),
                });
                self.declare_global(
                    &e.name,
                    DefKind::Enum(id),
                    Ty::EnumType(id),
                    LazyState::Ready,
                );
            }
            ItemKind::Type(t) => {
                let id = self
                    .types
                    .add_record(RecordDef::new(t.name.name.clone(), user(t.name.span)));
                self.declare_global(&t.name, DefKind::Type(id), Ty::Unit, LazyState::Ready);
            }
            ItemKind::Fn(f) => {
                let pending = self.pending(LazyAst::Fn(f));
                self.declare_global(&f.name, DefKind::Fn, Ty::Error, pending);
            }
            ItemKind::Tokens(t) => {
                let id = self.declare_global(
                    &t.name,
                    DefKind::Tokens,
                    Ty::opaque("TokenSet"),
                    LazyState::Ready,
                );
                self.tokens.add_set(id, self.module, t);
            }
            ItemKind::Keyframes(k) => {
                self.declare_global(&k.name, DefKind::Keyframes, Ty::Unit, LazyState::Ready);
            }
            ItemKind::Service(s) => {
                let id = self
                    .types
                    .add_record(RecordDef::new(s.name.name.clone(), user(s.name.span)));
                self.declare_global(
                    &s.name,
                    DefKind::Service(id),
                    Ty::Record(id),
                    LazyState::Ready,
                );
            }
            ItemKind::Permit(p) if p.capability.name == "exec" => {
                self.permits.push(permit_programs(&p.args));
            }
            _ => {}
        }
    }

    fn pending(&self, what: LazyAst<'a>) -> LazyState<'a> {
        LazyState::Pending(Pending {
            what,
            module: self.module,
            scope_depth: self.scopes.len(),
            node_depth: self.nodes.len(),
            ctx: self.ctx,
        })
    }

    fn redeclared(&mut self, name: &ast::Ident, prev: DefId, scope: &str) {
        let p = &self.defs[prev.0 as usize];
        let (pf, ps) = (p.file, p.span);
        let other_file = pf != self.file();
        let help = if other_file {
            format!("rename one: {scope} names are shared by every file of the config")
        } else {
            "rename one of them".to_string()
        };
        self.error(
            "check::redeclared",
            format!("`{}` is declared twice", name.name),
            name.span,
            "declared again here",
        )
        .add_secondary(pf, ps, "first declared here")
        .help = Some(help);
    }

    pub(super) fn declare_global(
        &mut self,
        name: &ast::Ident,
        kind: DefKind,
        ty: Ty,
        lazy: LazyState<'a>,
    ) -> DefId {
        let builtin = match &kind {
            DefKind::Component | DefKind::Surface(_) => {
                self.schema.element(&name.name).map(|_| "a builtin element")
            }
            DefKind::Type(_) | DefKind::Enum(_) => {
                self.schema.named_type(&name.name).map(|_| "a builtin type")
            }
            DefKind::Service(_) => self.schema.service(&name.name).map(|_| "a builtin service"),
            DefKind::Fn => self
                .schema
                .functions
                .contains_key(&name.name)
                .then_some("a builtin function"),
            _ => None,
        };
        if let Some(what) = builtin {
            self.error(
                "check::redeclared",
                format!("`{}` is {what}", name.name),
                name.span,
                "already taken",
            )
            .help = Some("choose another name".into());
        }
        let def = Def {
            name: name.name.clone(),
            kind,
            file: self.file(),
            span: name.span,
            ty,
            exported: false,
            owner: None,
        };
        let id = self.add_def(def, lazy);
        self.item_defs.insert((self.module, name.span), id);
        self.add_ref(name.span, Target::Def(id));
        match self.globals.get(&name.name).copied() {
            Some(prev) => self.redeclared(name, prev, "component, type and other global"),
            None => {
                self.globals.insert(name.name.clone(), id);
            }
        }
        id
    }

    fn declare_file(
        &mut self,
        name: &ast::Ident,
        kind: DefKind,
        exported: bool,
        lazy: LazyState<'a>,
    ) -> DefId {
        let def = Def {
            name: name.name.clone(),
            kind,
            file: self.file(),
            span: name.span,
            ty: Ty::Error,
            exported,
            owner: None,
        };
        let id = self.add_def(def, lazy);
        self.item_defs.insert((self.module, name.span), id);
        self.add_ref(name.span, Target::Def(id));
        match self.file_scopes[self.module].get(&name.name).copied() {
            Some(prev) => self.redeclared(name, prev, "file"),
            None => {
                self.file_scopes[self.module].insert(name.name.clone(), id);
            }
        }
        id
    }

    /// Declares the `state`s and `let`s of a tree block in a new scope, to
    /// be checked when reached or first read.
    pub(super) fn declare_block(&mut self, items: &'a [ast::Item]) {
        self.push_scope();
        for item in items {
            let reset = self.reset_attr_quiet(item);
            let (name, kind, what) = match &item.kind {
                ItemKind::State(s) => (
                    &s.name,
                    match s.init {
                        ast::StateInit::File { .. } => DefKind::Settings,
                        ast::StateInit::Value { .. } => DefKind::State,
                    },
                    LazyAst::State(s, reset),
                ),
                ItemKind::Let(l) => (&l.name, DefKind::Let, LazyAst::Let(l, reset)),
                _ => continue,
            };
            if let Some(prev) = self
                .scopes
                .last()
                .and_then(|s| s.iter().find(|(n, _)| *n == name.name).map(|(_, b)| *b))
            {
                if let Binding::Def(prev) = prev {
                    self.redeclared(name, prev, "block");
                }
                continue;
            }
            let def = Def {
                name: name.name.clone(),
                kind,
                file: self.file(),
                span: name.span,
                ty: Ty::Error,
                exported: false,
                owner: self.ctx.owner,
            };
            let lazy = LazyState::Pending(Pending {
                what,
                module: self.module,
                scope_depth: self.scopes.len(),
                node_depth: self.nodes.len(),
                ctx: Ctx {
                    handler: false,
                    pure_fn: false,
                    prop: false,
                    token_entry: None,
                    ..self.ctx
                },
            });
            let id = self.add_def(def, lazy);
            self.item_defs.insert((self.module, name.span), id);
            self.add_ref(name.span, Target::Def(id));
            self.bind(&name.name, Binding::Def(id));
        }
    }

    /// Like [`Checker::reset_attr`] but without reporting (the item is
    /// reported when reached).
    fn reset_attr_quiet(&self, item: &ast::Item) -> bool {
        item.attrs.iter().any(|a| a.name.name == "reset")
    }

    // -----------------------------------------------------------------------
    // Signatures

    pub(super) fn signatures(&mut self) {
        for i in 0..self.modules.len() {
            self.module = i;
            let items = &self.modules[i].ast.items;
            for item in items {
                match &item.kind {
                    ItemKind::Type(t) => self.type_fields(t),
                    ItemKind::Service(s) => self.service_fields(s),
                    _ => {}
                }
            }
        }
        // Components and fns may name the types above.
        for i in 0..self.modules.len() {
            self.module = i;
            let items = &self.modules[i].ast.items;
            for item in items {
                match &item.kind {
                    ItemKind::Component(c) => self.component_sig(c),
                    ItemKind::Fn(f) => self.fn_sig(f),
                    _ => {}
                }
            }
        }
        self.resolve_token_sets();
    }

    fn type_fields(&mut self, t: &'a ast::TypeDecl) {
        let Some(&id) = self.item_defs.get(&(self.module, t.name.span)) else {
            return;
        };
        let DefKind::Type(rid) = self.defs[id.0 as usize].kind else {
            return;
        };
        let mut fields: Vec<FieldDef> = Vec::new();
        for f in &t.fields.items {
            if fields.iter().any(|g| g.name == f.name.name) {
                self.error(
                    "check::redeclared",
                    format!("field `{}` is declared twice", f.name.name),
                    f.name.span,
                    "declared again here",
                );
                continue;
            }
            let ty = self.resolve_type(&f.ty);
            if let Some(rw) = f.rw {
                self.error(
                    "check::misplaced",
                    "`rw` marks writable service fields",
                    rw,
                    "not in a `type`",
                )
                .help = Some("fields of a record held in `state` are writable already".into());
            }
            if let Some(d) = &f.default {
                self.error(
                    "check::misplaced",
                    "record fields have no defaults",
                    d.span,
                    "not allowed in a `type`",
                )
                .help =
                    Some("give every field when building one: `Pin(app: a, label: \"x\")`".into());
            }
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                rw: false,
            });
        }
        self.types.record_mut(rid).fields = fields;
    }

    fn service_fields(&mut self, s: &'a ast::Service) {
        let Some(&id) = self.item_defs.get(&(self.module, s.name.span)) else {
            return;
        };
        let DefKind::Service(rid) = self.defs[id.0 as usize].kind else {
            return;
        };
        let mut fields: Vec<FieldDef> = Vec::new();
        for item in &s.body.items {
            let ItemKind::Field(f) = &item.kind else {
                continue;
            };
            if fields.iter().any(|g| g.name == f.name.name) {
                self.error(
                    "check::redeclared",
                    format!("field `{}` is declared twice", f.name.name),
                    f.name.span,
                    "declared again here",
                );
                continue;
            }
            let ty = self.resolve_type(&f.ty);
            fields.push(FieldDef {
                name: f.name.name.clone(),
                ty,
                rw: f.rw.is_some(),
            });
        }
        self.types.record_mut(rid).fields = fields;
    }

    fn component_sig(&mut self, c: &'a ast::Component) {
        let Some(&id) = self.item_defs.get(&(self.module, c.name.span)) else {
            return;
        };
        let mut params: Vec<CompParam> = Vec::new();
        for (i, p) in c.params.iter().flatten().enumerate() {
            if params.iter().any(|q| q.name == p.name.name) {
                self.error(
                    "check::redeclared",
                    format!("parameter `{}` is declared twice", p.name.name),
                    p.name.span,
                    "declared again here",
                );
            }
            let declared = p.ty.as_ref().map(|t| self.resolve_type(t));
            let default = p.default.as_ref().map(|d| {
                let e = self.expr(d, declared.as_ref());
                if let Some(t) = &declared {
                    self.require(&e, t, "the default");
                }
                e
            });
            let ty = match (declared, &default) {
                (Some(t), _) => t,
                (None, Some(d)) => d.ty.clone(),
                (None, None) => Ty::Error,
            };
            if let Some(d) = default {
                self.param_defaults.insert((id, i), d);
            }
            params.push(CompParam {
                name: p.name.name.clone(),
                ty,
                has_default: p.default.is_some(),
            });
        }
        let has_slot = has_slot(&c.body.items);
        self.comp_sigs.insert(id, CompSig { params, has_slot });
    }

    fn fn_sig(&mut self, f: &'a ast::FnDecl) {
        let Some(&id) = self.item_defs.get(&(self.module, f.name.span)) else {
            return;
        };
        let mut params = Vec::new();
        for p in &f.params {
            let ty = match &p.ty {
                Some(t) => self.resolve_type(t),
                None => {
                    self.error(
                        "check::needs_type",
                        format!("parameter `{}` needs a type", p.name.name),
                        p.name.span,
                        "add `: T`",
                    )
                    .help = Some(format!("`{}: float`", p.name.name));
                    Ty::Error
                }
            };
            if let Some(d) = &p.default {
                self.error(
                    "check::misplaced",
                    "fn parameters take no defaults",
                    d.span,
                    "every argument is passed",
                );
            }
            params.push(ParamSig {
                name: p.name.name.clone(),
                ty,
                has_default: false,
                variadic: false,
            });
        }
        if let Some(ret) = &f.ret {
            let ret = self.resolve_type(ret);
            self.defs[id.0 as usize].ty = Ty::Fn(std::sync::Arc::new(FnSig::new(params, ret)));
            // The body is checked when the file is walked.
            self.lazy[id.0 as usize] = LazyState::Ready;
        } else {
            // The return type comes from the body, checked on first call.
            self.fn_params.insert(id, params);
        }
    }

    // -----------------------------------------------------------------------
    // The file walk

    pub(super) fn file_items(&mut self, items: &'a [ast::Item]) -> Vec<hir::Item> {
        let mut out = Vec::new();
        for item in items {
            let def =
                |c: &Self, name: &ast::Ident| c.item_defs.get(&(c.module, name.span)).copied();
            match &item.kind {
                ItemKind::Component(c) => {
                    if let Some(id) = def(self, &c.name) {
                        out.push(hir::Item::Component(self.component(id, c)));
                    }
                }
                ItemKind::Surface(s) => {
                    let id = s.name.as_ref().and_then(|n| def(self, n));
                    out.push(hir::Item::Surface(self.surface(id, s, item.span)));
                }
                ItemKind::State(s) => {
                    if let Some(id) = def(self, &s.name)
                        && let Some(DoneHir::State(st)) = self.take_done(id)
                    {
                        out.push(hir::Item::State(st));
                    }
                }
                ItemKind::Let(l) => {
                    if let Some(id) = def(self, &l.name)
                        && let Some(DoneHir::Let(lt)) = self.take_done(id)
                    {
                        out.push(hir::Item::Let(lt));
                    }
                }
                ItemKind::Enum(e) => {
                    if let Some(id) = def(self, &e.name) {
                        out.push(hir::Item::Enum(id));
                    }
                }
                ItemKind::Type(t) => {
                    if let Some(id) = def(self, &t.name) {
                        out.push(hir::Item::Type(id));
                    }
                }
                ItemKind::Fn(f) => {
                    if let Some(id) = def(self, &f.name) {
                        let decl = match self.lazy[id.0 as usize] {
                            LazyState::Ready => {
                                self.lazy[id.0 as usize] = LazyState::Done;
                                Some(self.fn_decl(id, f))
                            }
                            _ => match self.take_done(id) {
                                Some(DoneHir::Fn(d)) => Some(d),
                                _ => None,
                            },
                        };
                        out.extend(decl.map(hir::Item::Fn));
                    }
                }
                ItemKind::Tokens(t) => {
                    if let Some(id) = def(self, &t.name) {
                        out.push(hir::Item::Tokens(self.token_set(id)));
                    }
                }
                ItemKind::Use(u) => out.push(hir::Item::Use(self.use_decl(u, item.span))),
                ItemKind::Service(s) => {
                    if let Some(id) = def(self, &s.name) {
                        out.push(hir::Item::Service(self.service_decl(id, s)));
                    }
                }
                ItemKind::Keyframes(k) => {
                    if let Some(id) = def(self, &k.name) {
                        out.push(hir::Item::Keyframes(self.keyframes(id, k)));
                    }
                }
                ItemKind::Permit(p) => out.push(hir::Item::Permit(hir::Permit {
                    capability: p.capability.name.clone(),
                    programs: permit_programs(&p.args).unwrap_or_default(),
                })),
                ItemKind::On(o) => {
                    if let Some(h) = self.handler(o, item.span) {
                        out.push(hir::Item::Handler(h));
                    }
                }
                ItemKind::Timer(t) => out.push(hir::Item::Timer(self.timer(t, item.span))),
                // Tree items at the top level were reported by the parser.
                _ => {}
            }
        }
        out
    }

    /// Checks that need every file.
    pub(super) fn after(&mut self, _files: &[hir::FileHir]) {
        // `use tokens` and `use palette` choose one theme for the config.
        let mut seen: Vec<(&'static str, usize, Span)> = Vec::new();
        for (module, span, kind) in std::mem::take(&mut self.uses) {
            if let Some(&(_, pm, ps)) = seen.iter().find(|(k, _, _)| *k == kind) {
                let pf = self.modules[pm].file;
                let saved = std::mem::replace(&mut self.module, module);
                self.error(
                    "check::redeclared",
                    format!("`use {kind}` appears twice"),
                    span,
                    "chosen again here",
                )
                .add_secondary(pf, ps, "first chosen here")
                .help = Some(format!(
                    "one `use {kind}` chooses for the whole config; pick between sets with an expression (`use {kind} a ? x : y`)"
                ));
                self.module = saved;
            } else {
                seen.push((kind, module, span));
            }
        }
        // Two files with one stem would give two exports one path.
        let mut stems: std::collections::HashMap<&str, usize> = Default::default();
        for (i, m) in self.modules.iter().enumerate() {
            let has_exports = self.defs.iter().any(|d| d.exported && d.file == m.file);
            if !has_exports {
                continue;
            }
            if let Some(&prev) = stems.get(m.name) {
                let pf = self.modules[prev].file;
                let span = self
                    .defs
                    .iter()
                    .find(|d| d.exported && d.file == m.file)
                    .map_or(Span::default(), |d| d.span);
                let saved = std::mem::replace(&mut self.module, i);
                self.error(
                    "check::redeclared",
                    format!("two files are named `{}`, so their exports clash", m.name),
                    span,
                    format!("exported as `{}.…`", m.name),
                )
                .add_secondary(pf, Span::default(), "the other file")
                .help = Some("rename one of the files".into());
                self.module = saved;
            } else {
                stems.insert(m.name, i);
            }
        }
    }
}

/// Whether a component body renders its children (`slot` anywhere in it).
fn has_slot(items: &[ast::Item]) -> bool {
    items.iter().any(|i| match &i.kind {
        ItemKind::Slot => true,
        ItemKind::Element(e) => e.block.as_ref().is_some_and(|b| has_slot(&b.items)),
        ItemKind::If(f) => if_has_slot(f),
        ItemKind::For(f) => has_slot(&f.body.items),
        ItemKind::Match(m) => m.arms.iter().any(|a| match &a.body {
            ast::ArmBody::Block(b) => has_slot(&b.items),
            ast::ArmBody::Single(i) => has_slot(std::slice::from_ref(&**i)),
        }),
        _ => false,
    })
}

fn if_has_slot(f: &ast::If<ast::Item>) -> bool {
    has_slot(&f.then.items)
        || match &f.else_ {
            Some(ast::Else::If(i, _)) => if_has_slot(i),
            Some(ast::Else::Block(b)) => has_slot(&b.items),
            None => false,
        }
}

/// The programs a `permit exec` lists (`None`: any).
fn permit_programs(args: &[ast::Expr]) -> Option<Vec<String>> {
    if args.is_empty() {
        return None;
    }
    Some(
        args.iter()
            .filter_map(|a| match &a.kind {
                ast::ExprKind::String(s) => Some(s.value.clone()),
                _ => None,
            })
            .collect(),
    )
}

pub(super) fn first_program(e: &ast::Expr) -> Option<&str> {
    match &e.kind {
        ast::ExprKind::String(s) => s.value.split_whitespace().next(),
        ast::ExprKind::Array(items) => items.first().and_then(first_program),
        _ => None,
    }
}
