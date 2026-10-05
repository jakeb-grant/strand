//! The first passes: declare every name, then resolve the signatures other
//! files need (record fields, component parameters, fn signatures, token
//! sets), then the per-file walk and the checks that need the whole
//! program.

use std::collections::{HashMap, HashSet};

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
                self.shadows_builtin(&s.name, "`state`");
                self.declare_file(&s.name, kind, s.export.is_some(), pending);
            }
            ItemKind::Let(l) => {
                let pending = self.pending(LazyAst::Let(l, reset));
                self.shadows_builtin(&l.name, "`let`");
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
                self.builtin_type_name(&e.name);
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
                self.builtin_type_name(&t.name);
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

    /// A binding already in the innermost scope, declared again there.
    pub(super) fn redeclared_binding(&mut self, name: &ast::Ident, prev: Binding) {
        match prev {
            Binding::Def(d) => self.redeclared(name, d, "block"),
            Binding::Local(l) => {
                let local = &self.locals[l.0 as usize];
                let (lf, ls) = (local.file, local.span);
                self.error(
                    "check::redeclared",
                    format!("`{}` is declared twice", name.name),
                    name.span,
                    "declared again here",
                )
                .add_secondary(lf, ls, "first declared here")
                .help = Some("rename one of them".into());
            }
        }
    }

    /// The language's own type names (`color`, `length`, `Async`, …) are
    /// not a prelude: no crate adds them, and a `type color` would make
    /// every `color` annotation mean something else. A redeclaration
    /// (annotations keep the builtin, so it is the only diagnostic).
    fn builtin_type_name(&mut self, name: &ast::Ident) {
        let n = name.name.as_str();
        if crate::ty::Prim::from_name(n).is_none() && !matches!(n, "any" | "unit" | "Async") {
            return;
        }
        self.error(
            "check::redeclared",
            format!("`{n}` is a builtin type"),
            name.span,
            "already taken",
        )
        .help = Some("choose another name: the language's own types cannot be redeclared".into());
    }

    /// Builtin names are a prelude: a `state`, `let`, parameter, `fn`,
    /// type or component named like a builtin function, value or type
    /// shadows it in its scope (a call that then fails names the hidden
    /// builtin). Shadowing a builtin service gets a warning, since
    /// `let battery = 5` turns `battery.percent` far away into a confusing
    /// error; a warning, so a service crate that adds a service never
    /// breaks a config. `what` names the declaration (`let`, `state`,
    /// `parameter`).
    pub(super) fn shadows_builtin(&mut self, name: &ast::Ident, what: &str) {
        let n = name.name.as_str();
        if !self.schema.services.contains_key(n) {
            return;
        }
        self.warning(
            "check::shadows_builtin",
            format!("this {what} hides the builtin service `{n}`"),
            name.span,
            "shadows a service",
        )
        .help = Some(format!(
            "rename the {what}: in its scope `{n}` no longer reads the service"
        ));
    }

    pub(super) fn declare_global(
        &mut self,
        name: &ast::Ident,
        kind: DefKind,
        ty: Ty,
        lazy: LazyState<'a>,
    ) -> DefId {
        // Builtin elements are resolved before components in a tree, so a
        // component named like one could never be placed; two services
        // under one name would be ambiguous. Other builtin names are a
        // prelude the declaration shadows (see `shadows_builtin`).
        let builtin = match &kind {
            DefKind::Component | DefKind::Surface(_) => {
                self.schema.element(&name.name).map(|_| "a builtin element")
            }
            DefKind::Service(_) => self.schema.service(&name.name).map(|_| "a builtin service"),
            _ => None,
        };
        match &kind {
            DefKind::Fn | DefKind::Type(_) | DefKind::Enum(_) | DefKind::Component => {
                let what = match &kind {
                    DefKind::Fn => "`fn`",
                    DefKind::Component => "component",
                    _ => "type",
                };
                self.shadows_builtin(name, what);
            }
            _ => {}
        }
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
                self.redeclared_binding(name, prev);
                continue;
            }
            self.shadows_builtin(
                name,
                if matches!(kind, DefKind::Let) {
                    "`let`"
                } else {
                    "`state`"
                },
            );
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
                let e = self.named_value("a parameter default", declared.as_ref(), |c| {
                    c.expr(d, declared.as_ref())
                });
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
                infer: p.ty.is_none() && p.default.is_none(),
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
                default: None,
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
                    let Some(id) = def(self, &c.name) else {
                        continue;
                    };
                    let infers = self
                        .comp_sigs
                        .get(&id)
                        .is_some_and(|s| s.params.iter().any(|p| p.infer));
                    if infers {
                        // Checked once every call site has been seen.
                        self.deferred.push(super::Deferred {
                            module: self.module,
                            index: out.len(),
                            def: id,
                            ast: c,
                        });
                    } else {
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

    /// Checks the components that infer parameter types from their call
    /// sites, after everything else (so every call outside them has been
    /// seen), callers before callees: a deferred component that calls
    /// another is checked first, so its calls count too. In a cycle of
    /// such components the first declared goes first.
    pub(super) fn deferred_components(&mut self, files: &mut [hir::FileHir]) {
        let mut pending = std::mem::take(&mut self.deferred);
        let n = pending.len();
        let index: HashMap<&str, usize> = pending
            .iter()
            .enumerate()
            .map(|(i, d)| (d.ast.name.name.as_str(), i))
            .collect();
        // callees[j]: the pending components component j's body uses;
        // waiting[i]: how many of component i's pending callers are
        // unchecked. Each body is walked once.
        let mut callees: Vec<Vec<usize>> = Vec::with_capacity(n);
        let mut waiting = vec![0usize; n];
        for (j, d) in pending.iter().enumerate() {
            let mut used = HashSet::new();
            element_names(&d.ast.body.items, &mut used);
            let mut out: Vec<usize> = used
                .iter()
                .filter_map(|name| index.get(name).copied())
                .filter(|&i| i != j)
                .collect();
            out.sort_unstable();
            for &i in &out {
                waiting[i] += 1;
            }
            callees.push(out);
        }
        // Kahn's order: the first declared ready component next; in a
        // cycle (none ready), the first declared unchecked one.
        let mut ready: std::collections::BTreeSet<usize> =
            (0..n).filter(|&i| waiting[i] == 0).collect();
        let mut done = vec![false; n];
        let mut cursor = 0;
        let mut placed: Vec<(usize, usize, u32, hir::Item)> = Vec::new();
        for _ in 0..n {
            let i = match ready.pop_first() {
                Some(i) => i,
                None => {
                    while cursor < n && done[cursor] {
                        cursor += 1;
                    }
                    if cursor == n {
                        break;
                    }
                    cursor
                }
            };
            done[i] = true;
            for &k in &callees[i] {
                waiting[k] = waiting[k].saturating_sub(1);
                if waiting[k] == 0 && !done[k] {
                    ready.insert(k);
                }
            }
            let d = pending[i];
            self.module = d.module;
            self.infer_params(d.def);
            let item = hir::Item::Component(self.component(d.def, d.ast));
            placed.push((d.module, d.index, d.ast.name.span.start, item));
        }
        pending.clear();
        // Back into file order: the last first keeps the others' indices
        // valid (two at one index: the later in the file first).
        placed.sort_by(|a, b| (a.0, b.1, b.2).cmp(&(b.0, a.1, a.2)));
        for (module, index, _, item) in placed {
            let items = &mut files[module].items;
            items.insert(index.min(items.len()), item);
        }
    }

    /// Joins the argument types passed to each inferred parameter of
    /// component `def`; disagreeing callers are one error at the
    /// parameter.
    fn infer_params(&mut self, def: DefId) {
        let Some(mut sig) = self.comp_sigs.get(&def).cloned() else {
            return;
        };
        self.inferred.insert(def);
        let decl = self.defs[def.0 as usize].clone();
        for (i, p) in sig.params.iter_mut().enumerate() {
            if !p.infer {
                continue;
            }
            // A type an earlier pass found passed inside a cycle, after
            // the join (see `arg_value`).
            let pinned = self
                .param_span(def, i)
                .and_then(|s| self.param_pins.get(&(decl.file, s)).cloned());
            let passed = self.param_args.get(&(def, i));
            let had_args = passed.is_some_and(|v| !v.is_empty());
            let args: Vec<super::PassedArg> = passed
                .map(|v| {
                    v.iter()
                        .filter(|(_, _, t)| !t.is_error())
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let Some((_, _, first)) = args.first() else {
                if let Some(t) = pinned {
                    p.ty = t;
                } else if had_args {
                    // Every argument was already an error at its call
                    // (`Side cente`): not also "nothing passes it a value".
                    self.infer_failed.insert((def, i));
                }
                continue;
            };
            let mut joined = first.clone();
            if let Some(t) = &pinned {
                joined = self.types.join(&joined, t).unwrap_or(joined);
            }
            let mut clash = None;
            for (k, (_, _, t)) in args.iter().enumerate().skip(1) {
                match self.types.join(&joined, t) {
                    Some(j) => joined = j,
                    None => {
                        clash = Some(k);
                        break;
                    }
                }
            }
            match clash {
                None => p.ty = joined,
                Some(k) => {
                    self.infer_failed.insert((def, i));
                    let (af, aspan, aty) = args[0].clone();
                    let (bf, bspan, bty) = args[k].clone();
                    let (sa, sb) = (self.show(&aty), self.show(&bty));
                    let span = self.param_span(def, i).unwrap_or(decl.span);
                    let name = p.name.clone();
                    self.error(
                        "check::needs_type",
                        format!(
                            "parameter `{name}` of `{}` is passed `{sa}` and `{sb}`",
                            decl.name
                        ),
                        span,
                        "its type comes from its callers, which disagree",
                    )
                    .add_secondary(af, aspan, format!("`{sa}` here"))
                    .add_secondary(bf, bspan, format!("`{sb}` here"))
                    .help = Some(format!(
                        "write the type it takes (`{name}: T`), or pass the same type everywhere"
                    ));
                }
            }
        }
        self.comp_sigs.insert(def, sig);
    }

    /// The span of parameter `i` of component `def`.
    pub(super) fn param_span(&self, def: DefId, i: usize) -> Option<Span> {
        let d = self.deferred_ast(def)?;
        d.params.as_ref()?.get(i).map(|p| p.name.span)
    }

    fn deferred_ast(&self, def: DefId) -> Option<&'a ast::Component> {
        let d = &self.defs[def.0 as usize];
        let m = self.modules.iter().position(|m| m.file == d.file)?;
        self.modules[m]
            .ast
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ItemKind::Component(c) if c.name.span == d.span => Some(c),
                _ => None,
            })
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
        // A file named like a service or global name cannot be read as
        // `file.name`: the name means the service (or global) there.
        for (i, m) in self.modules.iter().enumerate() {
            let taken = if self.schema.services.contains_key(m.name) {
                Some("a builtin service")
            } else if self.schema.values.contains_key(m.name) {
                Some("a builtin value")
            } else if self.globals.contains_key(m.name) {
                Some("declared in this config")
            } else {
                None
            };
            let Some(taken) = taken else { continue };
            let first = self
                .defs
                .iter()
                .find(|d| d.exported && d.file == m.file)
                .map(|d| (d.span, d.name.clone()));
            if let Some((span, export)) = first {
                let saved = std::mem::replace(&mut self.module, i);
                self.error(
                    "check::redeclared",
                    format!(
                        "`{}.{export}` cannot be read: `{}` is {taken}",
                        m.name, m.name
                    ),
                    span,
                    "exported from a file named like it",
                )
                .help = Some(format!(
                    "rename `{}.strand`; its exports are read as `file.name`",
                    m.name
                ));
                self.module = saved;
            }
        }
        // A file's `state`/`let` named like a global (`state C` and
        // `component C`): `C` would be a value here and an element
        // everywhere, so it is a redeclaration.
        for m in 0..self.modules.len() {
            let mut clashes: Vec<(DefId, DefId)> = self.file_scopes[m]
                .iter()
                .filter_map(|(n, d)| self.globals.get(n).map(|g| (*d, *g)))
                .collect();
            clashes.sort_by_key(|(d, _)| self.defs[d.0 as usize].span.start);
            for (d, g) in clashes {
                let def = &self.defs[d.0 as usize];
                let (name, span) = (def.name.clone(), def.span);
                let what = if def.kind == DefKind::Let {
                    "let"
                } else {
                    "state"
                };
                let global = &self.defs[g.0 as usize];
                let (gf, gs) = (global.file, global.span);
                let saved = std::mem::replace(&mut self.module, m);
                self.error(
                    "check::redeclared",
                    format!("`{name}` is declared twice"),
                    span,
                    format!("a `{what}` of this file"),
                )
                .add_secondary(gf, gs, "a name the whole config shares")
                .help = Some(format!(
                    "rename the `{what}`: components, types and other global names cannot be reused by a file's `state` or `let`"
                ));
                self.module = saved;
            }
        }
        // `my-bar.strand`: `my-bar.x` reads as a subtraction, so an export
        // there can never be reached.
        for (i, m) in self.modules.iter().enumerate() {
            if is_identifier(m.name) {
                continue;
            }
            let first = self
                .defs
                .iter()
                .find(|d| d.exported && d.file == m.file)
                .map(|d| (d.span, d.name.clone()));
            if let Some((span, export)) = first {
                let fixed = snake_case(m.name);
                let saved = std::mem::replace(&mut self.module, i);
                self.error(
                    "check::file_name",
                    format!(
                        "`{export}` cannot be read from other files: `{}` is not a name",
                        m.name
                    ),
                    span,
                    "exported from a file whose name cannot be written",
                )
                .help = Some(format!(
                    "rename the file to snake_case (`{fixed}.strand`) and read it as `{fixed}.{export}`"
                ));
                self.module = saved;
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

/// Whether `s` can be written as a name (`theme`, `my_bar`).
fn is_identifier(s: &str) -> bool {
    let mut b = s.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// `My-Bar 2` → `my_bar_2`.
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    match out.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => out,
        Some(_) => format!("f_{out}"),
        None => "config".to_string(),
    }
}

/// The element kinds a tree uses (component calls among them).
fn element_names<'t>(items: &'t [ast::Item], out: &mut HashSet<&'t str>) {
    for i in items {
        match &i.kind {
            ItemKind::Element(e) => {
                out.insert(e.kind.name.as_str());
                if let Some(b) = &e.block {
                    element_names(&b.items, out);
                }
            }
            ItemKind::When(w) => element_names(&w.body.items, out),
            ItemKind::If(f) => if_element_names(f, out),
            ItemKind::For(f) => element_names(&f.body.items, out),
            ItemKind::Match(m) => {
                for a in &m.arms {
                    match &a.body {
                        ast::ArmBody::Block(b) => element_names(&b.items, out),
                        ast::ArmBody::Single(i) => element_names(std::slice::from_ref(&**i), out),
                    }
                }
            }
            _ => {}
        }
    }
}

fn if_element_names<'t>(f: &'t ast::If<ast::Item>, out: &mut HashSet<&'t str>) {
    element_names(&f.then.items, out);
    match &f.else_ {
        Some(ast::Else::If(i, _)) => if_element_names(i, out),
        Some(ast::Else::Block(b)) => element_names(&b.items, out),
        None => {}
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
