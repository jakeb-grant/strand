//! Tokens: token sets, `extends` and loud `override`s, token references,
//! `set { … }`, component tokens and `use`.
//!
//! Every token path the program can read is a schema token (palette roles
//! and base tiers), an entry of a `tokens` set, or a component token
//! (`$Toast.radius`). Entries are typed on first read, so a cycle among
//! derived tokens is found as it is walked and reported with its path.

use std::collections::{BTreeMap, HashMap};

use super::{Checker, Ctx};
use crate::diagnostic::Diagnostic;
use crate::hir::{self, DefId, DefKind, Target};
use crate::schema::Schema;
use crate::syntax::Span;
use crate::syntax::ast;
use crate::ty::Ty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Set(usize),
    Component(DefId),
}

#[derive(Clone, Debug)]
pub(crate) enum EntryState {
    Pending,
    Checking,
    Done(Ty),
}

#[derive(Clone, Debug)]
pub(crate) struct Entry<'a> {
    pub owner: Owner,
    pub path: String,
    pub span: Span,
    /// The key as written (`hi`, `surface.hi`), the tail of `path`.
    pub key: &'a ast::TokenKey,
    pub override_: bool,
    /// The `override group { … }` this entry came from: its prefix and
    /// key, so a misspelt group is reported once.
    pub group: Option<(String, &'a ast::TokenKey)>,
    pub value: &'a ast::Expr,
    pub module: usize,
    pub state: EntryState,
    pub hir: Option<hir::Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct SetInfo<'a> {
    pub def: DefId,
    pub module: usize,
    pub ast: &'a ast::TokensDecl,
    pub extends: Option<usize>,
    pub entries: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Tokens<'a> {
    pub sets: Vec<SetInfo<'a>>,
    pub set_of_def: HashMap<DefId, usize>,
    pub entries: Vec<Entry<'a>>,
    pub by_path: HashMap<String, Vec<usize>>,
    pub stack: Vec<usize>,
    /// Each set's place in the `extends` forest once cycles are broken:
    /// `(enter, leave, depth)` of a pre-order walk, so whether a set is on
    /// another's chain is two comparisons (see [`Tokens::index_chains`]).
    pub tour: Vec<(u32, u32, u32)>,
}

fn key_path(k: &ast::TokenKey) -> String {
    k.segments
        .iter()
        .map(|s| s.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// What to write in place of `key`, which spells the tail of token `path`,
/// so that it names token `meant` instead: `meant`'s own tail, when the
/// two share the part of the path written outside the key.
fn key_fix(key: &ast::TokenKey, path: &str, meant: &str) -> Option<String> {
    let segs: Vec<&str> = path.split('.').collect();
    let keep = segs.len().saturating_sub(key.segments.len().max(1));
    let tail = if keep == 0 {
        meant
    } else {
        meant.strip_prefix(&format!("{}.", segs[..keep].join(".")))?
    };
    Some(if key.dollar {
        format!("${tail}")
    } else {
        tail.to_string()
    })
}

/// Proposes the key that names token `meant` in place of `key` (which
/// spells the tail of `path`). When `meant` lies outside the group the key
/// is written in, no edit of the key reaches it: the help names the token
/// and its place instead of asking "did you mean …?", and there is no fix.
fn suggest_key(d: &mut Diagnostic, key: &ast::TokenKey, path: &str, meant: Option<String>) {
    let Some(m) = meant else { return };
    match key_fix(key, path, &m) {
        Some(f) => {
            d.suggest(key.span, f);
        }
        None => {
            let segs: Vec<&str> = path.split('.').collect();
            let keep = segs.len().saturating_sub(key.segments.len().max(1));
            let group = segs[..keep].join(".");
            d.help = Some(format!(
                "the closest token is `${m}`, which is outside group `{group}`"
            ));
        }
    }
}

impl<'a> Tokens<'a> {
    pub fn add_set(&mut self, def: DefId, module: usize, decl: &'a ast::TokensDecl) {
        let set = self.sets.len();
        let mut entries = Vec::new();
        self.flatten(
            Owner::Set(set),
            module,
            "",
            &decl.entries.items,
            None,
            false,
            &mut entries,
        );
        self.sets.push(SetInfo {
            def,
            module,
            ast: decl,
            extends: None,
            entries,
        });
        self.set_of_def.insert(def, set);
    }

    pub fn add_component(
        &mut self,
        def: DefId,
        module: usize,
        name: &str,
        block: &'a ast::Block<ast::TokenEntry>,
    ) -> Vec<usize> {
        let mut out = Vec::new();
        self.flatten(
            Owner::Component(def),
            module,
            name,
            &block.items,
            None,
            false,
            &mut out,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn flatten(
        &mut self,
        owner: Owner,
        module: usize,
        prefix: &str,
        items: &'a [ast::TokenEntry],
        group: Option<(String, &'a ast::TokenKey)>,
        override_: bool,
        out: &mut Vec<usize>,
    ) {
        for e in items {
            let key = key_path(&e.key);
            let path = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            let ov = override_ || e.override_.is_some();
            match &e.body {
                ast::TokenBody::Value(v) => {
                    let idx = self.entries.len();
                    self.entries.push(Entry {
                        owner,
                        path: path.clone(),
                        span: e.key.span,
                        key: &e.key,
                        override_: ov,
                        group: group.clone(),
                        value: v,
                        module,
                        state: EntryState::Pending,
                        hir: None,
                    });
                    self.by_path.entry(path).or_default().push(idx);
                    out.push(idx);
                }
                ast::TokenBody::Group(b) => {
                    let g = if e.override_.is_some() && group.is_none() {
                        Some((path.clone(), &e.key))
                    } else {
                        group.clone()
                    };
                    self.flatten(owner, module, &path, &b.items, g, ov, out);
                }
            }
        }
    }

    /// Sets from `set` up its `extends` chain, without repeats.
    fn chain(&self, set: Option<usize>) -> Vec<usize> {
        // A chain visits each set at most once, so more steps than there
        // are sets means a cycle (only before cycles are broken).
        let mut out = Vec::new();
        let mut cur = set;
        while let Some(s) = cur {
            if out.len() >= self.sets.len() || out.last() == Some(&s) {
                break;
            }
            out.push(s);
            cur = self.sets[s].extends;
        }
        out
    }

    /// Numbers the `extends` forest (see [`Tokens::tour`]); called once
    /// cycles are broken.
    pub fn index_chains(&mut self) {
        let n = self.sets.len();
        let mut children = vec![Vec::new(); n];
        let mut roots = Vec::new();
        for s in 0..n {
            match self.sets[s].extends {
                Some(p) => children[p].push(s),
                None => roots.push(s),
            }
        }
        let mut tour = vec![(u32::MAX, u32::MAX, 0); n];
        let mut clock = 0u32;
        for r in roots {
            tour[r] = (clock, 0, 0);
            clock += 1;
            let mut stack: Vec<(usize, usize)> = vec![(r, 0)];
            while let Some(top) = stack.last_mut() {
                let (node, next) = *top;
                if let Some(&c) = children[node].get(next) {
                    top.1 += 1;
                    tour[c] = (clock, 0, tour[node].2 + 1);
                    clock += 1;
                    stack.push((c, 0));
                } else {
                    tour[node].1 = clock;
                    stack.pop();
                }
            }
        }
        // Every set is reached once no cycle is left; otherwise lookups
        // walk the chain.
        if tour.iter().all(|t| t.0 != u32::MAX) {
            self.tour = tour;
        }
    }

    /// Whether set `a` is `s` or one of the sets `s` extends.
    fn on_chain(&self, a: usize, s: usize) -> bool {
        let (ta, ts) = (self.tour[a], self.tour[s]);
        ta.0 <= ts.0 && ts.0 < ta.1
    }

    /// The entry for `path` nearest `set` up its `extends` chain: of the
    /// entries with that path on the chain, the one in the deepest set.
    fn find_in_chain(&self, set: Option<usize>, path: &str) -> Option<usize> {
        if let Some(s) = set
            && self.tour.len() == self.sets.len()
        {
            let mut best: Option<(u32, usize)> = None;
            for &e in self.by_path.get(path)? {
                if let Owner::Set(a) = self.entries[e].owner
                    && self.on_chain(a, s)
                    && best.is_none_or(|(d, _)| self.tour[a].2 > d)
                {
                    best = Some((self.tour[a].2, e));
                }
            }
            return best.map(|(_, e)| e);
        }
        self.chain(set).into_iter().find_map(|s| {
            self.sets[s]
                .entries
                .iter()
                .copied()
                .find(|&e| self.entries[e].path == path)
        })
    }

    /// The component token (`Toast.radius` of `component Toast(…) tokens
    /// { radius: … }`) with this path: the knob a set may `override`.
    fn component_entry(&self, path: &str) -> Option<usize> {
        self.by_path
            .get(path)?
            .iter()
            .copied()
            .find(|&e| matches!(self.entries[e].owner, Owner::Component(_)))
    }

    /// Every readable token path and its type. A component token's type
    /// is its own (a set's `override` of it is checked against it).
    pub fn all_types(&self, schema: &Schema) -> BTreeMap<String, Ty> {
        let mut out: BTreeMap<String, Ty> = schema
            .tokens
            .iter()
            .map(|(k, t)| (k.clone(), t.ty.clone()))
            .collect();
        for e in &self.entries {
            if let EntryState::Done(t) = &e.state {
                if matches!(e.owner, Owner::Component(_)) {
                    out.insert(e.path.clone(), t.clone());
                } else {
                    out.entry(e.path.clone()).or_insert_with(|| t.clone());
                }
            }
        }
        out
    }

    fn known(&self, schema: &Schema, path: &str) -> bool {
        schema.tokens.contains_key(path) || self.by_path.contains_key(path)
    }

    fn all_paths(&self, schema: &Schema) -> Vec<String> {
        let mut v: Vec<String> = schema.tokens.keys().cloned().collect();
        v.extend(self.by_path.keys().cloned());
        v
    }
}

impl<'a> Checker<'a> {
    /// Resolves `extends` and checks each set's entries against the sets it
    /// extends: redefining needs `override`, and an `override` must
    /// override something.
    pub(super) fn resolve_token_sets(&mut self) {
        for s in 0..self.tokens.sets.len() {
            let set = &self.tokens.sets[s];
            let (module, ext) = (set.module, set.ast.extends.as_ref());
            let Some(ext) = ext else { continue };
            self.module = module;
            match self.globals.get(&ext.name).copied() {
                Some(d) if self.defs[d.0 as usize].kind == DefKind::Tokens => {
                    self.add_ref(ext.span, Target::Def(d));
                    self.tokens.sets[s].extends = self.tokens.set_of_def.get(&d).copied();
                }
                _ => {
                    let candidates: Vec<String> = self
                        .tokens
                        .sets
                        .iter()
                        .map(|t| self.defs[t.def.0 as usize].name.clone())
                        .collect();
                    let fix = Self::closest(&ext.name, &candidates);
                    self.error(
                        "check::unknown_name",
                        format!("unknown token set `{}`", ext.name),
                        ext.span,
                        "not a `tokens` set",
                    )
                    .suggest_opt(ext.span, fix);
                }
            }
        }
        // `a extends b extends a`: each set extends at most one, so a walk
        // from every unvisited set finds each cycle once (O(sets)). A
        // cycle is reported at its first declared member and broken there.
        let n = self.tokens.sets.len();
        // 0: unvisited, 1: on the current walk, 2: done.
        let mut mark = vec![0u8; n];
        let mut cycles: Vec<Vec<usize>> = Vec::new();
        for start in 0..n {
            let mut path = Vec::new();
            let mut cur = Some(start);
            while let Some(c) = cur {
                match mark[c] {
                    0 => {
                        mark[c] = 1;
                        path.push(c);
                        cur = self.tokens.sets[c].extends;
                    }
                    1 => {
                        let at = path.iter().position(|&p| p == c).unwrap_or(0);
                        cycles.push(path[at..].to_vec());
                        break;
                    }
                    _ => break,
                }
            }
            for p in path {
                mark[p] = 2;
            }
        }
        for cycle in cycles {
            let first = cycle
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| **s)
                .map_or(0, |(i, _)| i);
            let s = cycle[first];
            let names: Vec<String> = cycle[first..]
                .iter()
                .chain(&cycle[..first])
                .chain(std::iter::once(&s))
                .map(|i| format!("`{}`", self.defs[self.tokens.sets[*i].def.0 as usize].name))
                .collect();
            self.module = self.tokens.sets[s].module;
            let span = self.tokens.sets[s]
                .ast
                .extends
                .as_ref()
                .map_or(Span::default(), |e| e.span);
            self.error(
                "check::cycle",
                format!("token sets extend each other: {}", names.join(" → ")),
                span,
                "extends itself",
            );
            self.tokens.sets[s].extends = None;
        }
        self.tokens.index_chains();
        for s in 0..self.tokens.sets.len() {
            self.validate_set(s);
        }
    }

    fn validate_set(&mut self, s: usize) {
        self.module = self.tokens.sets[s].module;
        let parent = self.tokens.sets[s].extends;
        let set_name = self.defs[self.tokens.sets[s].def.0 as usize].name.clone();
        let entries = self.tokens.sets[s].entries.clone();
        let mut own: HashMap<String, usize> = HashMap::new();
        let mut reported_groups: Vec<String> = Vec::new();
        for &e in &entries {
            let entry = self.tokens.entries[e].clone();
            let path = entry.path.as_str();
            if let Some(&prev) = own.get(path) {
                let ps = self.tokens.entries[prev].span;
                let file = self.file();
                self.error(
                    "check::redeclared",
                    format!("`${path}` is defined twice in `{set_name}`"),
                    entry.span,
                    "defined again here",
                )
                .add_secondary(file, ps, "first defined here");
                continue;
            }
            own.insert(path.to_string(), e);
            // A component's token (`$Toast.radius`) is a knob sets
            // override like an inherited one.
            let inherited = self
                .tokens
                .find_in_chain(parent, path)
                .or_else(|| self.tokens.component_entry(path));
            let schema = self.schema.tokens.get(path);
            if !entry.override_ {
                if let Some(inh) = inherited {
                    let by = match self.tokens.entries[inh].owner {
                        Owner::Component(d) => {
                            format!("component `{}`", self.defs[d.0 as usize].name)
                        }
                        Owner::Set(p) => {
                            format!("`{}`", self.defs[self.tokens.sets[p].def.0 as usize].name)
                        }
                    };
                    let (f, sp) = (
                        self.modules[self.tokens.entries[inh].module].file,
                        self.tokens.entries[inh].span,
                    );
                    self.error(
                        "check::override_needed",
                        format!("`${path}` is already defined by {by}"),
                        entry.span,
                        "redefined without `override`",
                    )
                    .add_secondary(f, sp, "defined here")
                    .help = Some(format!("redefining a token is loud: `override {path}: …`"));
                } else if schema.is_some_and(|t| t.palette) {
                    self.error(
                        "check::override_needed",
                        format!("`${path}` is a palette role"),
                        entry.span,
                        "redefined without `override`",
                    )
                    .help = Some(format!(
                        "palette roles come from `use palette …`; to replace one here, write `override {path}: …`"
                    ));
                }
            } else if inherited.is_none() && schema.is_none() {
                // A misspelt override is an unknown name, never a new token.
                if let Some((prefix, gkey)) = &entry.group {
                    let dotted = format!("{prefix}.");
                    let any = self.schema.tokens.keys().any(|p| p.starts_with(&dotted))
                        || self.tokens.by_path.keys().any(|p| {
                            p.starts_with(&dotted)
                                && (self.tokens.find_in_chain(parent, p).is_some()
                                    || self.tokens.component_entry(p).is_some())
                        });
                    if !any {
                        if !reported_groups.contains(prefix) {
                            reported_groups.push(prefix.clone());
                            let groups = self.token_groups(parent);
                            let meant = Self::closest(prefix, &groups);
                            let d = self.error(
                                "check::unknown_token",
                                format!("`override {prefix}` overrides nothing"),
                                gkey.span,
                                "no such token group",
                            );
                            suggest_key(d, gkey, prefix, meant);
                            if d.help.is_none() {
                                d.help = Some("an override must name an existing token; drop `override` to add a new one".into());
                            }
                        }
                        continue;
                    }
                }
                let candidates = self.override_candidates(parent);
                let meant = Self::closest(path, &candidates);
                let d = self.error(
                    "check::unknown_token",
                    format!("`override {path}` overrides nothing"),
                    entry.span,
                    "no such token",
                );
                suggest_key(d, entry.key, path, meant);
                if d.help.is_none() {
                    d.help = Some(
                        "an override must name an existing token; drop `override` to add a new one"
                            .into(),
                    );
                }
            }
        }
    }

    fn override_candidates(&self, parent: Option<usize>) -> Vec<String> {
        let mut v: Vec<String> = self.schema.tokens.keys().cloned().collect();
        v.extend(
            self.tokens
                .entries
                .iter()
                .filter(|e| matches!(e.owner, Owner::Component(_)))
                .map(|e| e.path.clone()),
        );
        for s in self.tokens.chain(parent) {
            v.extend(
                self.tokens.sets[s]
                    .entries
                    .iter()
                    .map(|&e| self.tokens.entries[e].path.clone()),
            );
        }
        v
    }

    fn token_groups(&self, parent: Option<usize>) -> Vec<String> {
        let mut v: Vec<String> = self
            .override_candidates(parent)
            .iter()
            .filter_map(|p| p.split_once('.').map(|(g, _)| g.to_string()))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// A token set's entries, typed.
    pub(super) fn token_set(&mut self, def: DefId) -> hir::TokenSet {
        let Some(&s) = self.tokens.set_of_def.get(&def) else {
            return hir::TokenSet {
                def,
                extends: None,
                entries: Vec::new(),
            };
        };
        let entries = self.tokens.sets[s].entries.clone();
        let extends = self.tokens.sets[s].extends.map(|p| self.tokens.sets[p].def);
        hir::TokenSet {
            def,
            extends,
            entries: self.token_defs(&entries),
        }
    }

    pub(super) fn token_defs(&mut self, entries: &[usize]) -> Vec<hir::TokenDef> {
        entries
            .iter()
            .map(|&e| {
                let span = self.tokens.entries[e].span;
                self.entry_ty(e, span);
                let entry = &mut self.tokens.entries[e];
                hir::TokenDef {
                    path: entry.path.clone(),
                    span: entry.span,
                    override_: entry.override_,
                    value: entry
                        .hir
                        .take()
                        .unwrap_or_else(|| hir::Expr::error(entry.value.span)),
                }
            })
            .collect()
    }

    /// The type of entry `e`, checking it on first use.
    fn entry_ty(&mut self, e: usize, span: Span) -> Ty {
        // Token chains (`t0: $t1`, `t1: $t2`, …) nest like `let` chains.
        super::grow(|| self.entry_ty_now(e, span))
    }

    fn entry_ty_now(&mut self, e: usize, span: Span) -> Ty {
        match &self.tokens.entries[e].state {
            EntryState::Done(t) => return t.clone(),
            EntryState::Checking => {
                let pos = self.tokens.stack.iter().position(|x| *x == e).unwrap_or(0);
                let path: Vec<String> = self.tokens.stack[pos..]
                    .iter()
                    .chain(std::iter::once(&e))
                    .map(|i| format!("`${}`", self.tokens.entries[*i].path))
                    .collect();
                let p = self.tokens.entries[e].path.clone();
                self.error(
                    "check::cycle",
                    format!("static cycle: {}", path.join(" → ")),
                    span,
                    format!("`${p}` depends on itself"),
                )
                .help = Some("derived tokens must bottom out in the palette or a value".into());
                return Ty::Error;
            }
            EntryState::Pending => {}
        }
        self.tokens.entries[e].state = EntryState::Checking;
        let mark = (self.diags.len(), self.refs.len());
        self.tokens.stack.push(e);
        let entry = self.tokens.entries[e].clone();
        let mut want = self.schema.tokens.get(&entry.path).map(|t| t.ty.clone());
        // A set's entry for a component token is typed by the component's.
        if want.is_none()
            && matches!(entry.owner, Owner::Set(_))
            && let Some(c) = self.tokens.component_entry(&entry.path)
        {
            want = Some(self.entry_ty(c, entry.span)).filter(|t| *t != Ty::Error);
        }
        let ctx = Ctx {
            token_entry: Some(e),
            ..Ctx::default()
        };
        let h = self.with_place(entry.module, 0, 0, ctx, |c| {
            c.token_value(entry.value, want.as_ref(), &entry.path)
        });
        self.tokens.stack.pop();
        let ty = want.unwrap_or_else(|| h.ty.clone());
        self.tokens.entries[e].hir = Some(h);
        self.tokens.entries[e].state = EntryState::Done(ty.clone());
        self.keep_from(mark);
        ty
    }

    /// A token's value: like a prop value, but shadows and fonts are
    /// inferred for tokens the schema does not type.
    fn token_value(&mut self, value: &'a ast::Expr, want: Option<&Ty>, path: &str) -> hir::Expr {
        let what = format!("`${path}`");
        match want {
            Some(t) => self.prop_value(value, t, &what),
            None => match &value.kind {
                ast::ExprKind::Spaced(items) => {
                    let font = matches!(
                        items.first().map(|i| &i.kind),
                        Some(ast::ExprKind::String(_))
                    );
                    let t = if font { Ty::FONT } else { Ty::SHADOW };
                    self.prop_value(value, &t, &what)
                }
                ast::ExprKind::Commas(items)
                    if items
                        .iter()
                        .any(|i| matches!(i.kind, ast::ExprKind::Spaced(_))) =>
                {
                    self.prop_value(value, &Ty::SHADOW, &what)
                }
                _ => self.expr(value, None),
            },
        }
    }

    /// `$path`.
    pub(super) fn token_expr(&mut self, key: &ast::TokenKey) -> hir::Expr {
        let path = key_path(key);
        self.add_ref(key.span, Target::Token(path.clone()));
        let ty = self.token_ty(&path, key.span);
        hir::Expr {
            kind: hir::ExprKind::Token(path),
            ty,
            span: key.span,
        }
    }

    fn token_ty(&mut self, path: &str, span: Span) -> Ty {
        if let Some(cur) = self.ctx.token_entry
            && let Owner::Set(s) = self.tokens.entries[cur].owner
        {
            // `override x: $x.alpha(0.5)` reads the parent's `x`.
            let own = self.tokens.entries[cur].path == path;
            let start = if own {
                self.tokens.sets[s].extends
            } else {
                Some(s)
            };
            if let Some(e) = self.tokens.find_in_chain(start, path) {
                return self.entry_ty(e, span);
            }
        }
        if let Some(t) = self.schema.tokens.get(path) {
            return t.ty.clone();
        }
        // A component token reads as the component's own entry; a set's
        // `override` of it has been checked against that type.
        if let Some(e) = self.tokens.component_entry(path) {
            return self.entry_ty(e, span);
        }
        if let Some(&e) = self.tokens.by_path.get(path).and_then(|v| v.first()) {
            return self.entry_ty(e, span);
        }
        let prefix = format!("{path}.");
        let mut members: Vec<String> = self
            .tokens
            .all_paths(self.schema)
            .into_iter()
            .filter(|p| p.starts_with(&prefix))
            .collect();
        if !members.is_empty() {
            members.sort();
            members.dedup();
            let shown: Vec<String> = members.iter().take(4).map(|m| format!("`${m}`")).collect();
            let d = self.error(
                "check::unknown_token",
                format!("`${path}` is a group of tokens, not one token"),
                span,
                "a group",
            );
            d.help = Some(format!("pick one: {}", shown.join(", ")));
            // Every member is a fix, none preferred.
            for m in &members {
                d.add_suggestion(span, format!("${m}"));
            }
            return Ty::Error;
        }
        let candidates = self.tokens.all_paths(self.schema);
        let fix = suggest_path(path, &candidates).map(|m| format!("${m}"));
        self.error(
            "check::unknown_token",
            format!("unknown token `${path}`"),
            span,
            "not a token",
        )
        .suggest_opt(span, fix);
        Ty::Error
    }

    /// `$fg-muted`: the parser reads a subtraction (and warns); when the
    /// joined name is near a token, this reports the token instead of an
    /// unknown name `muted`.
    pub(super) fn kebab_token(
        &mut self,
        key: &ast::TokenKey,
        name: &ast::Ident,
        span: Span,
    ) -> Option<hir::Expr> {
        if key.span.end + 1 != name.span.start || self.lookup_scope(&name.name).is_some() {
            return None;
        }
        let path = key_path(key);
        let candidates = self.tokens.all_paths(self.schema);
        let exact = [
            format!("{path}.{}", name.name),
            format!("{path}_{}", name.name),
        ]
        .into_iter()
        .find(|p| candidates.contains(p));
        let meant = exact.or_else(|| {
            crate::diagnostic::suggest(
                &format!("{path}.{}", name.name),
                candidates.iter().map(String::as_str),
            )
            .map(str::to_string)
        })?;
        self.error(
            "check::unknown_token",
            format!("unknown token `${path}-{}`", name.name),
            span,
            format!("read as `${path} - {}`", name.name),
        )
        .suggest(span, format!("${meant}"));
        Some(hir::Expr::error(span))
    }

    /// `set { $x: … }`: overrides for a subtree. Every key must be a token;
    /// on the right, `$x` is the inherited value.
    pub(super) fn set_block(
        &mut self,
        block: &'a ast::Block<ast::TokenEntry>,
    ) -> Vec<hir::TokenDef> {
        let mut out = Vec::new();
        self.set_entries(&block.items, "", &mut out);
        out
    }

    fn set_entries(
        &mut self,
        items: &'a [ast::TokenEntry],
        prefix: &str,
        out: &mut Vec<hir::TokenDef>,
    ) {
        let saved = self.ctx;
        self.ctx.prop = false;
        for e in items {
            let key = key_path(&e.key);
            let path = if prefix.is_empty() {
                key
            } else {
                format!("{prefix}.{key}")
            };
            match &e.body {
                ast::TokenBody::Group(b) => self.set_entries(&b.items, &path, out),
                ast::TokenBody::Value(v) => {
                    let want = if self.tokens.known(self.schema, &path) {
                        self.add_ref(e.key.span, Target::Token(path.clone()));
                        Some(self.token_ty(&path, e.key.span))
                    } else {
                        let candidates = self.tokens.all_paths(self.schema);
                        let meant = suggest_path(&path, &candidates);
                        let d = self.error(
                            "check::unknown_token",
                            format!("`set` overrides `${path}`, which is not a token"),
                            e.key.span,
                            "not a token",
                        );
                        suggest_key(d, &e.key, &path, meant);
                        None
                    };
                    let value = self.token_value(v, want.as_ref(), &path);
                    out.push(hir::TokenDef {
                        path,
                        span: e.key.span,
                        override_: true,
                        value,
                    });
                }
            }
        }
        self.ctx = saved;
    }

    /// `use tokens a, palette b`.
    pub(super) fn use_decl(&mut self, u: &'a ast::Use, _span: Span) -> hir::Use {
        let mut out = hir::Use {
            tokens: None,
            palette: None,
        };
        for c in &u.clauses {
            match c.kind.name.as_str() {
                "tokens" => {
                    out.tokens =
                        Some(self.expect(&c.value, &Ty::opaque("TokenSet"), "`use tokens`"));
                    self.uses.push((self.module, c.span, "tokens"));
                }
                "palette" => {
                    out.palette =
                        Some(self.expect(&c.value, &Ty::opaque("Palette"), "`use palette`"));
                    self.uses.push((self.module, c.span, "palette"));
                }
                _ => {
                    self.expr(&c.value, None);
                }
            }
        }
        out
    }
}

/// Did-you-mean for token paths: the closest whole path.
fn suggest_path(path: &str, candidates: &[String]) -> Option<String> {
    crate::diagnostic::closest(path, candidates.iter().map(String::as_str))
}
