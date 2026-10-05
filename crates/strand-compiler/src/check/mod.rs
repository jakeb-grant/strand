//! Name resolution and type checking.
//!
//! [`check`] takes every parsed file of a config (no imports: they form one
//! program) and returns the typed [`Program`](crate::hir::Program) plus
//! diagnostics. It never panics and always returns a program: names it
//! cannot resolve and expressions that do not type-check become
//! [`Ty::Error`], which is accepted everywhere, so one mistake gives one
//! diagnostic.
//!
//! Scoping (see `docs/decisions.md`, wave2-check):
//! - components, surfaces, enums, types, fns, token sets, keyframes and
//!   custom services are global across the config's files; declaring one
//!   twice is an error naming both places;
//! - top-level `state` and `let` belong to their file; `export` makes them
//!   reachable from other files (and the CLI) as `file.name`;
//! - `state`/`let` in a component, surface or list item belong to it;
//!   handler and fn `let`s are local.
//!
//! The passes: collect declarations ([`collect`]), resolve signatures
//! (types, component parameters, fn signatures, token sets), then check
//! each file in order. `let`s, `state`s and fns without a return type are
//! checked on first use, which is also how static cycles are found.

mod collect;
mod expr;
mod prepin;
mod stmt;
mod tokens;
mod tree;

use std::collections::{HashMap, HashSet};

use crate::diagnostic::{Diagnostic, suggest};
use crate::hir::{
    self, Def, DefId, DefKind, Local, LocalId, LocalKind, NodeIdx, Program, Reference, Target,
};
use crate::schema::{ElementSchema, Schema};
use crate::source::FileId;
use crate::syntax::Span;
use crate::syntax::ast;
use crate::ty::{Ty, TypeTable};

/// Runs `f`, first moving to a fresh heap-allocated stack segment if
/// little of the current one is left. Every lazily checked entry point
/// (declarations, token entries, component bodies) goes through here, so
/// how deep a chain of definitions nests is bounded by memory, not by the
/// calling thread's stack.
pub(crate) fn grow<T>(f: impl FnOnce() -> T) -> T {
    const RED_ZONE: usize = 256 * 1024;
    const SEGMENT: usize = 4 * 1024 * 1024;
    stacker::maybe_grow(RED_ZONE, SEGMENT, f)
}

/// One file of the program.
#[derive(Clone, Copy, Debug)]
pub struct Module<'a> {
    pub file: FileId,
    /// The file stem: `theme` for `theme.strand` (the `theme` of
    /// `theme.look`).
    pub name: &'a str,
    pub ast: &'a ast::File,
}

/// What [`check`] returns.
#[derive(Debug, Default)]
pub struct Checked {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    /// How many times the program was checked: 1, plus one per round of
    /// whole-number states found fractional (see [`check`]).
    pub passes: usize,
}

impl Checked {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// Resolves names and checks types across every file of a config.
pub fn check<'a>(modules: &'a [Module<'a>], schema: &'a Schema) -> Checked {
    // An untyped `state`/`let` holding a whole number (`state i = 0`) is
    // an `int` until a fraction is written to it (`i = 0.5`, `i += t`, a
    // slider's `value: <-> i`); then it is a `float` and the program is
    // checked again with it pinned. A pass also records which whole
    // declarations are assigned from which (`b = a`, `b = a * 2`, also
    // through an untyped `let`, handler local or fn value in between), and
    // the pins are closed over those hand-offs, so a chain of them costs
    // one extra pass, not one per link. The fraction writes the source
    // shows plainly (`level = 0.5`, `level += dy * 0.05`, a slider's
    // `<-> level`) are pinned before the first pass ([`prepin`]), so the
    // usual config is checked once.
    //
    // There is no pass cap: every pass but the last pins at least one more
    // declaration (or widens an inferred component parameter, see
    // [`Checker::param_pins`]), pins only grow and are bounded by the
    // declarations, so the loop ends. A hand-off the checker does not
    // follow (through a list, a record field) costs a pass, never a wrong
    // type or a false error.
    let mut pins: HashSet<DeclAt> = prepin::pre_pins(modules, schema);
    let mut param_pins: HashMap<DeclAt, Ty> = HashMap::new();
    let mut passes = 0;
    loop {
        let mut c = Checker::new(modules, schema);
        c.float_pins = pins.clone();
        c.param_pins = param_pins.clone();
        passes += 1;
        c.run();
        let before = pins.len();
        pins.extend(c.widened.iter().copied());
        close_flows(&mut pins, &c.flows);
        let mut params_moved = false;
        for (k, t) in c.new_param_pins.drain() {
            if param_pins.get(&k) != Some(&t) {
                param_pins.insert(k, t);
                params_moved = true;
            }
        }
        if pins.len() == before && !params_moved {
            let mut out = c.finish();
            out.passes = passes;
            return out;
        }
    }
}

/// A declaration by where its name is: stable across checker passes.
pub(crate) type DeclAt = (FileId, Span);

/// Adds to `pins` every whole-number declaration assigned (directly or
/// through other hand-offs) from a pinned one: `(target, source)` edges.
fn close_flows(pins: &mut HashSet<DeclAt>, flows: &[(DeclAt, DeclAt)]) {
    let mut from: HashMap<DeclAt, Vec<DeclAt>> = HashMap::new();
    for &(target, source) in flows {
        from.entry(source).or_default().push(target);
    }
    let mut queue: Vec<DeclAt> = pins.iter().copied().collect();
    while let Some(s) = queue.pop() {
        for &t in from.get(&s).into_iter().flatten() {
            if pins.insert(t) {
                queue.push(t);
            }
        }
    }
}

/// A whole-number literal with no unit (`3`, `-1`, `(0)`): an `int` where
/// nothing else fixes its type (an untyped `state`/`let`, an argument to
/// an inferred component parameter).
pub(crate) fn is_whole_literal(e: &ast::Expr) -> bool {
    match &e.kind {
        ast::ExprKind::Number(n) => !n.fraction && n.unit.is_none(),
        ast::ExprKind::Paren(inner) => is_whole_literal(inner),
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Neg,
            expr,
        } => is_whole_literal(expr),
        _ => false,
    }
}

/// Tree keywords, offered when an unknown element looks like one
/// (`whn hover { … }` → `when`).
pub(crate) const TREE_KEYWORDS: &[&str] = &[
    "when", "if", "else", "match", "for", "on", "after", "every", "enter", "exit", "state", "let",
    "slot", "set", "play",
];

/// Top-level keywords, offered the same way.
pub(crate) const TOP_KEYWORDS: &[&str] = &[
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
];

/// Unknown names per file that get a did-you-mean; see
/// [`Checker::may_suggest`].
pub(crate) const SUGGESTIONS_PER_FILE: u32 = 200;

/// The node booleans every element has.
pub(crate) const NODE_BOOLS: &[&str] = &["hover", "pressed", "focused", "selected"];

#[derive(Clone, Copy, Debug)]
pub(crate) enum Binding {
    Local(LocalId),
    Def(DefId),
}

/// Where the checker is.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ctx {
    /// In a handler body: assignment, actions and `await` are allowed.
    pub handler: bool,
    /// In a `fn` body: no assignment to outside names, no actions.
    pub pure_fn: bool,
    /// The component being checked.
    pub component: Option<DefId>,
    /// The component or surface nested `state`/`let` belong to.
    pub owner: Option<DefId>,
    /// In a prop value: raw colours are linted.
    pub prop: bool,
    /// Checking a token definition (an index into the token entries).
    pub token_entry: Option<usize>,
}

/// An element being checked.
#[derive(Clone, Debug)]
pub(crate) struct NodeCtx<'a> {
    pub idx: NodeIdx,
    pub kind: String,
    pub schema: Option<&'a ElementSchema>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LazyAst<'a> {
    Let(&'a ast::Let, bool),
    State(&'a ast::State, bool),
    Fn(&'a ast::FnDecl),
}

#[derive(Clone, Debug)]
pub(crate) struct Pending<'a> {
    pub what: LazyAst<'a>,
    pub module: usize,
    pub scope_depth: usize,
    pub node_depth: usize,
    pub ctx: Ctx,
}

#[derive(Clone, Debug)]
pub(crate) enum LazyState<'a> {
    /// Not checked lazily, or its type is known.
    Ready,
    Pending(Pending<'a>),
    Checking,
    Done,
}

#[derive(Clone, Debug)]
pub(crate) enum DoneHir {
    Let(hir::LetDecl),
    State(hir::StateDecl),
    Fn(hir::FnDecl),
}

/// A component's call signature.
#[derive(Clone, Debug)]
pub(crate) struct CompSig {
    pub params: Vec<CompParam>,
    pub has_slot: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CompParam {
    pub name: String,
    pub ty: Ty,
    pub has_default: bool,
    /// Written with neither a type nor a default (`Toast(n)`): its type
    /// is joined from the arguments at its call sites.
    pub infer: bool,
}

/// An argument passed to a parameter whose type is inferred: where, and
/// its type.
pub(crate) type PassedArg = (FileId, Span, Ty);

/// A component whose body waits for its call sites (it has a parameter
/// to infer).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deferred<'a> {
    pub module: usize,
    /// Where its item goes among the file's items.
    pub index: usize,
    pub def: DefId,
    pub ast: &'a ast::Component,
}

pub(crate) struct Checker<'a> {
    pub schema: &'a Schema,
    pub modules: &'a [Module<'a>],
    pub types: TypeTable,
    pub defs: Vec<Def>,
    pub lazy: Vec<LazyState<'a>>,
    pub done: HashMap<DefId, DoneHir>,
    pub locals: Vec<Local>,
    pub refs: Vec<Reference>,
    pub diags: Vec<Diagnostic>,
    /// Global names: components, surfaces, enums, types, fns, token sets,
    /// keyframes, custom services.
    pub globals: HashMap<String, DefId>,
    /// Each file's top-level `state`/`let`.
    pub file_scopes: Vec<HashMap<String, DefId>>,
    /// File stems, for `file.name`.
    pub file_index: HashMap<String, usize>,
    pub comp_sigs: HashMap<DefId, CompSig>,
    pub tokens: tokens::Tokens<'a>,
    /// Key paths of keyed `state` collections.
    pub state_keys: HashMap<DefId, Vec<String>>,
    /// Untyped component parameters (an error if used).
    pub untyped: HashSet<LocalId>,
    pub reported: HashSet<(FileId, Span)>,
    /// `id:` names, found before the tree is checked so they can be used
    /// before the element that declares them.
    pub ids: HashMap<(usize, Span), (LocalId, NodeIdx)>,
    /// Programs `permit exec` allows; `None` allows any.
    pub permits: Vec<Option<Vec<String>>>,
    pub uses: Vec<(usize, Span, &'static str)>,

    pub module: usize,
    pub scopes: Vec<Vec<(String, Binding)>>,
    pub nodes: Vec<NodeCtx<'a>>,
    pub ctx: Ctx,
    /// Lazily checked definitions in progress, for cycle reports.
    pub stack: Vec<DefId>,
    pub next_node: u32,
    /// The type `page` names take inside `pages` (its `current`).
    pub pages_current: Vec<Ty>,
    /// Declarations by the span of their name, per file.
    pub item_defs: HashMap<(usize, Span), DefId>,
    /// Checked defaults of component parameters.
    pub param_defaults: HashMap<(DefId, usize), hir::Expr>,
    pub out_files: Vec<hir::FileHir>,
    /// Component calls being checked.
    pub calls: Vec<tree::CallCtx>,
    /// Token entries of each component's `tokens { }`.
    pub comp_tokens: HashMap<DefId, Vec<usize>>,
    /// Parameters of fns whose return type comes from their body.
    pub fn_params: HashMap<DefId, Vec<crate::ty::ParamSig>>,
    /// Argument types at the call sites of parameters to infer, with
    /// where each was passed.
    pub param_args: HashMap<(DefId, usize), Vec<PassedArg>>,
    /// Inferred parameters whose callers disagree (already reported).
    pub infer_failed: HashSet<(DefId, usize)>,
    pub deferred: Vec<Deferred<'a>>,
    /// Did-you-mean suggestions each file may still compute.
    pub suggestions_left: Vec<u32>,
    /// The name being checked is the base of `name.field`.
    pub field_base: bool,
    /// `let`s whose value reads nothing that changes (`let c = 1`).
    pub constant_lets: HashSet<DefId>,
    /// Untyped `state`s and `let`s initialised with a whole number, typed
    /// `int` this pass (see [`check`]).
    pub whole: HashSet<DefId>,
    /// Of those, the ones a fraction was written to this pass.
    pub widened: HashSet<(FileId, Span)>,
    /// Whole-number declarations earlier passes found fractions written
    /// to: typed `float`.
    pub float_pins: HashSet<(FileId, Span)>,
    /// The inferred component parameter whose argument is being checked
    /// (for the help on a bare builtin variant: `Side center`).
    pub infer_arg: Option<String>,
    /// Hand-offs between whole-number declarations this pass: `(target,
    /// source)` for `target = …source…` (see [`check`]).
    pub flows: Vec<(DeclAt, DeclAt)>,
    /// The whole-number declarations the value of an untyped `let` or
    /// `state`, or of a fn without a return type, reads (through
    /// arithmetic, `?:`, `??` and other such values), so a hand-off
    /// through it is followed like a direct one.
    pub value_sources: HashMap<DefId, Vec<DefId>>,
    /// The same for handler and fn `let`s.
    pub local_sources: HashMap<LocalId, Vec<DefId>>,
    /// Inferred component parameters (by the parameter's name) widened by
    /// an earlier pass: a call inside a cycle of such components passed a
    /// type the joined one did not cover (`A 0.5` after `A 1`).
    pub param_pins: HashMap<DeclAt, Ty>,
    /// Those found this pass.
    pub new_param_pins: HashMap<DeclAt, Ty>,
    /// Components whose inferred parameters have been joined.
    pub inferred: HashSet<DefId>,
}

impl<'a> Checker<'a> {
    fn new(modules: &'a [Module<'a>], schema: &'a Schema) -> Self {
        Self {
            schema,
            modules,
            types: schema.types.clone(),
            defs: Vec::new(),
            lazy: Vec::new(),
            done: HashMap::new(),
            locals: Vec::new(),
            refs: Vec::new(),
            diags: Vec::new(),
            globals: HashMap::new(),
            file_scopes: vec![HashMap::new(); modules.len()],
            file_index: HashMap::new(),
            comp_sigs: HashMap::new(),
            tokens: tokens::Tokens::default(),
            state_keys: HashMap::new(),
            untyped: HashSet::new(),
            reported: HashSet::new(),
            ids: HashMap::new(),
            permits: Vec::new(),
            uses: Vec::new(),
            module: 0,
            scopes: Vec::new(),
            nodes: Vec::new(),
            ctx: Ctx::default(),
            stack: Vec::new(),
            next_node: 0,
            pages_current: Vec::new(),
            item_defs: HashMap::new(),
            param_defaults: HashMap::new(),
            out_files: Vec::new(),
            calls: Vec::new(),
            comp_tokens: HashMap::new(),
            fn_params: HashMap::new(),
            param_args: HashMap::new(),
            infer_failed: HashSet::new(),
            deferred: Vec::new(),
            suggestions_left: vec![SUGGESTIONS_PER_FILE; modules.len()],
            field_base: false,
            constant_lets: HashSet::new(),
            whole: HashSet::new(),
            widened: HashSet::new(),
            float_pins: HashSet::new(),
            infer_arg: None,
            flows: Vec::new(),
            value_sources: HashMap::new(),
            local_sources: HashMap::new(),
            param_pins: HashMap::new(),
            new_param_pins: HashMap::new(),
            inferred: HashSet::new(),
        }
    }

    fn run(&mut self) {
        self.collect();
        self.signatures();
        let mut files = Vec::new();
        for (i, m) in self.modules.iter().enumerate() {
            self.module = i;
            let items = self.file_items(&m.ast.items);
            files.push(hir::FileHir {
                file: m.file,
                name: m.name.to_string(),
                items,
            });
        }
        self.deferred_components(&mut files);
        self.after(&files);
        self.out_files = files;
    }

    fn finish(mut self) -> Checked {
        let files = std::mem::take(&mut self.out_files);
        // Stable order: by file, then position.
        self.diags.sort_by_key(|d| {
            (
                d.file(),
                d.primary_span().map_or(0, |s| s.start),
                d.primary_span().map_or(0, |s| s.end),
            )
        });
        self.diags.dedup();
        let tokens = self.tokens.all_types(self.schema);
        Checked {
            program: Program {
                files,
                defs: self.defs,
                locals: self.locals,
                types: self.types,
                tokens,
                refs: self.refs,
            },
            diagnostics: self.diags,
            passes: 1,
        }
    }

    // -----------------------------------------------------------------------
    // Small helpers

    pub fn file(&self) -> FileId {
        self.modules[self.module].file
    }

    pub fn show(&self, t: &Ty) -> String {
        self.types.show(t).to_string()
    }

    /// An error with its primary label in the current file.
    pub fn error(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) -> &mut Diagnostic {
        let file = self.file();
        self.diags
            .push(Diagnostic::error(code, message).with_label_in(file, span, label));
        self.diags.last_mut().expect("just pushed")
    }

    pub fn warning(
        &mut self,
        code: &'static str,
        message: impl Into<String>,
        span: Span,
        label: impl Into<String>,
    ) -> &mut Diagnostic {
        let file = self.file();
        self.diags
            .push(Diagnostic::warning(code, message).with_label_in(file, span, label));
        self.diags.last_mut().expect("just pushed")
    }

    pub fn add_ref(&mut self, span: Span, target: Target) {
        let file = self.file();
        self.refs.push(Reference { file, span, target });
    }

    pub fn add_def(&mut self, def: Def, lazy: LazyState<'a>) -> DefId {
        self.defs.push(def);
        self.lazy.push(lazy);
        DefId(self.defs.len() as u32 - 1)
    }

    pub fn add_local(&mut self, name: &str, ty: Ty, span: Span, kind: LocalKind) -> LocalId {
        self.locals.push(Local {
            name: name.to_string(),
            ty,
            file: self.file(),
            span,
            kind,
        });
        LocalId(self.locals.len() as u32 - 1)
    }

    pub fn new_node(&mut self) -> NodeIdx {
        self.next_node += 1;
        NodeIdx(self.next_node - 1)
    }

    pub fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub fn bind(&mut self, name: &str, b: Binding) {
        if let Some(s) = self.scopes.last_mut() {
            s.push((name.to_string(), b));
        }
    }

    pub fn bind_local(&mut self, name: &str, ty: Ty, span: Span, kind: LocalKind) -> LocalId {
        let id = self.add_local(name, ty, span, kind);
        self.bind(name, Binding::Local(id));
        id
    }

    pub fn lookup_scope(&self, name: &str) -> Option<Binding> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|(n, _)| n == name)
            .map(|(_, b)| *b)
    }

    /// Runs `f` with the context of another place (a lazily checked
    /// declaration), restoring the current one afterwards.
    pub fn with_place<T>(
        &mut self,
        module: usize,
        scope_depth: usize,
        node_depth: usize,
        ctx: Ctx,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let same = module == self.module;
        let saved_module = std::mem::replace(&mut self.module, module);
        let saved_scopes = std::mem::take(&mut self.scopes);
        let saved_nodes = std::mem::take(&mut self.nodes);
        if same {
            self.scopes = saved_scopes[..scope_depth.min(saved_scopes.len())].to_vec();
            self.nodes = saved_nodes[..node_depth.min(saved_nodes.len())].to_vec();
        }
        let saved_ctx = std::mem::replace(&mut self.ctx, ctx);
        let saved_pages = std::mem::take(&mut self.pages_current);
        let out = f(self);
        self.module = saved_module;
        self.scopes = saved_scopes;
        self.nodes = saved_nodes;
        self.ctx = saved_ctx;
        self.pages_current = saved_pages;
        out
    }

    /// The type of a definition, checking it first if it is still pending.
    /// `span` is where it is read (for a cycle report).
    pub fn def_ty(&mut self, id: DefId, span: Span) -> Ty {
        match &self.lazy[id.0 as usize] {
            LazyState::Ready | LazyState::Done => self.defs[id.0 as usize].ty.clone(),
            LazyState::Checking => {
                self.report_cycle(id, span);
                Ty::Error
            }
            LazyState::Pending(_) => {
                self.force(id);
                self.defs[id.0 as usize].ty.clone()
            }
        }
    }

    /// Checks a pending declaration now.
    ///
    /// A chain of declarations (`let a0 = a1 + 1`, `let a1 = a2 + 1`, …)
    /// is checked as nested first uses, one set of expression frames per
    /// link, so the stack grows onto the heap here: a long chain must not
    /// overflow a worker thread's stack (the LSP's, the reload compile's).
    pub fn force(&mut self, id: DefId) {
        grow(|| self.force_now(id));
    }

    fn force_now(&mut self, id: DefId) {
        let LazyState::Pending(p) =
            std::mem::replace(&mut self.lazy[id.0 as usize], LazyState::Checking)
        else {
            return;
        };
        self.stack.push(id);
        let done = self.with_place(p.module, p.scope_depth, p.node_depth, p.ctx, |c| {
            match p.what {
                LazyAst::Let(l, reset) => DoneHir::Let(c.let_decl(id, l, reset)),
                LazyAst::State(s, reset) => DoneHir::State(c.state_decl(id, s, reset)),
                LazyAst::Fn(f) => DoneHir::Fn(c.fn_decl(id, f)),
            }
        });
        self.stack.pop();
        self.done.insert(id, done);
        self.lazy[id.0 as usize] = LazyState::Done;
    }

    /// Takes the HIR of a declaration, checking it if nobody has yet.
    pub fn take_done(&mut self, id: DefId) -> Option<DoneHir> {
        self.force(id);
        self.done.remove(&id)
    }

    fn report_cycle(&mut self, id: DefId, span: Span) {
        let Some(pos) = self.stack.iter().position(|d| *d == id) else {
            return;
        };
        let path: Vec<String> = self.stack[pos..]
            .iter()
            .chain(std::iter::once(&id))
            .map(|d| format!("`{}`", self.defs[d.0 as usize].name))
            .collect();
        let def = &self.defs[id.0 as usize];
        let (def_file, def_span, name) = (def.file, def.span, def.name.clone());
        let what = match def.kind {
            DefKind::Fn => "function",
            DefKind::State | DefKind::Settings => "state",
            _ => "let",
        };
        let help = if def.kind == DefKind::Fn {
            "give the function a return type (`-> T`) or break the recursion".to_string()
        } else {
            "a value cannot depend on itself; break the loop with a `state`".to_string()
        };
        self.error(
            "check::cycle",
            format!("static cycle: {}", path.join(" → ")),
            span,
            format!("this reads `{name}` while computing it"),
        )
        .add_secondary(def_file, def_span, format!("the {what} `{name}`"))
        .help = Some(help);
    }

    /// Names for did-you-mean at a value position, borrowed (an unknown
    /// name must not copy every name in scope). File stems only when the
    /// name is read as `name.field` (`thme.look`).
    pub fn value_candidates(&self, field_base: bool) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for s in &self.scopes {
            out.extend(s.iter().map(|(n, _)| n.as_str()));
        }
        if !self.nodes.is_empty() {
            out.extend(NODE_BOOLS.iter().copied());
            out.push("self");
        }
        out.extend(self.file_scopes[self.module].keys().map(String::as_str));
        for (n, d) in &self.globals {
            if matches!(
                self.defs[d.0 as usize].kind,
                DefKind::Fn | DefKind::Tokens | DefKind::Service(_) | DefKind::Enum(_)
            ) {
                out.push(n);
            }
        }
        out.extend(self.schema.services.keys().map(String::as_str));
        out.extend(self.schema.values.keys().map(String::as_str));
        if field_base {
            out.extend(self.file_index.keys().map(String::as_str));
        }
        out
    }

    /// The expected type for the value of an untyped `state`/`let`: `int`
    /// for a whole-number literal (see [`check`]), unless an earlier pass
    /// pinned it to `float`.
    pub fn whole_hint(&mut self, id: DefId, value: &ast::Expr) -> Option<Ty> {
        let def = &self.defs[id.0 as usize];
        if !is_whole_literal(value) || self.float_pins.contains(&(def.file, def.span)) {
            return None;
        }
        self.whole.insert(id);
        Some(Ty::INT)
    }

    /// Writes a value of type `from` into `target`: if `target` is a
    /// whole-number declaration and `from` is fractional, records it for
    /// the next pass and returns true (the write is not an error).
    pub fn widen(&mut self, target: &hir::Expr, from: &Ty) -> bool {
        let hir::ExprKind::Def(d) = target.kind else {
            return false;
        };
        if !self.whole.contains(&d)
            || self.types.assignable(from, &Ty::INT)
            || !self.types.assignable(from, &Ty::FLOAT)
        {
            return false;
        }
        let def = &self.defs[d.0 as usize];
        self.widened.insert((def.file, def.span));
        true
    }

    /// The declarations `e`'s value comes from, for [`Checker::record_flow`]:
    /// through arithmetic, `?:` and `??` (which keep a `float` a `float`),
    /// and through untyped `let`s, handler locals and fn values, whose own
    /// sources were recorded when they were checked.
    pub fn flow_sources(&self, e: &hir::Expr, out: &mut Vec<DefId>) {
        match &e.kind {
            hir::ExprKind::Def(d) => {
                out.push(*d);
                out.extend(self.value_sources.get(d).into_iter().flatten());
            }
            hir::ExprKind::Local(l) => {
                out.extend(self.local_sources.get(l).into_iter().flatten());
            }
            hir::ExprKind::Call {
                callee: hir::Callee::Fn(d),
                ..
            } => out.extend(self.value_sources.get(d).into_iter().flatten()),
            hir::ExprKind::Unary {
                op: hir::UnaryOp::Neg,
                expr,
            } => self.flow_sources(expr, out),
            hir::ExprKind::Binary {
                op:
                    hir::BinaryOp::Add
                    | hir::BinaryOp::Sub
                    | hir::BinaryOp::Mul
                    | hir::BinaryOp::Div
                    | hir::BinaryOp::Rem
                    | hir::BinaryOp::Coalesce,
                lhs,
                rhs,
            } => {
                self.flow_sources(lhs, out);
                self.flow_sources(rhs, out);
            }
            hir::ExprKind::Ternary { then, else_, .. } => {
                self.flow_sources(then, out);
                self.flow_sources(else_, out);
            }
            _ => {}
        }
    }

    /// The whole-number declarations `e` reads, deduplicated (what
    /// [`Checker::value_sources`] and [`Checker::local_sources`] keep).
    pub fn whole_sources(&self, e: &hir::Expr) -> Vec<DefId> {
        let mut found = Vec::new();
        self.flow_sources(e, &mut found);
        found.retain(|d| self.whole.contains(d));
        found.sort_unstable_by_key(|d| d.0);
        found.dedup_by_key(|d| d.0);
        found
    }

    /// Records the sources of an untyped declaration's value (see
    /// [`Checker::value_sources`]).
    pub fn record_value_sources(&mut self, id: DefId, value: &hir::Expr) {
        let found = self.whole_sources(value);
        if !found.is_empty() {
            self.value_sources.insert(id, found);
        }
    }

    /// Records that `value` is written into `target`: if both sides are
    /// whole-number declarations, a fraction in the source makes the
    /// target fractional too (see [`check`]).
    pub fn record_flow(&mut self, target: &hir::Expr, value: &hir::Expr) {
        let hir::ExprKind::Def(t) = target.kind else {
            return;
        };
        if !self.whole.contains(&t) {
            return;
        }
        let td = &self.defs[t.0 as usize];
        let tk = (td.file, td.span);
        for s in self.whole_sources(value) {
            if s != t {
                let sd = &self.defs[s.0 as usize];
                self.flows.push((tk, (sd.file, sd.span)));
            }
        }
    }

    /// Whether a bare `name` resolves to anything at a value position
    /// (ignoring enum variants, which need an expected type).
    pub fn names_something(&self, name: &str) -> bool {
        self.lookup_scope(name).is_some()
            || NODE_BOOLS.contains(&name)
            || name == "self"
            || self.file_scopes[self.module].contains_key(name)
            || self.globals.contains_key(name)
            || self.schema.services.contains_key(name)
            || self.schema.values.contains_key(name)
            || self.file_index.contains_key(name)
    }

    /// Whether an unknown name in the current file may still compute a
    /// suggestion: each costs a pass over every name in scope, so a file
    /// full of broken names (a large paste) stops suggesting after
    /// [`SUGGESTIONS_PER_FILE`].
    pub fn may_suggest(&mut self) -> bool {
        let left = &mut self.suggestions_left[self.module];
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }

    /// `file.name` for an unknown bare name another file exports
    /// (`dnd` → `toasts.dnd`), exact matches first.
    pub fn export_suggestion(&self, word: &str) -> Option<String> {
        let mut exports: Vec<(&str, &str)> = Vec::new();
        for (m, scope) in self.file_scopes.iter().enumerate() {
            if m == self.module {
                continue;
            }
            for (n, d) in scope {
                if self.defs[d.0 as usize].exported {
                    exports.push((self.modules[m].name, n.as_str()));
                }
            }
        }
        exports.sort_unstable();
        let (file, name) = match exports.iter().find(|(_, n)| *n == word) {
            Some(&hit) => hit,
            None => {
                let name = suggest(word, exports.iter().map(|(_, n)| *n))?;
                *exports.iter().find(|(_, n)| *n == name)?
            }
        };
        Some(format!("did you mean `{file}.{name}`?"))
    }

    pub fn did_you_mean(word: &str, candidates: &[String]) -> Option<String> {
        suggest(word, candidates.iter().map(String::as_str)).map(|s| format!("did you mean `{s}`?"))
    }

    // -----------------------------------------------------------------------
    // Types written in source

    /// Resolves a written type.
    pub fn resolve_type(&mut self, t: &ast::Type) -> Ty {
        match &t.kind {
            ast::TypeKind::Error => Ty::Error,
            ast::TypeKind::List(inner) => {
                let elem = self.resolve_type(inner);
                Schema::list_of(&self.types, elem)
            }
            ast::TypeKind::Optional(inner) => self.resolve_type(inner).optional(),
            ast::TypeKind::Named { path, args } => {
                let name: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
                let span = t.span;
                if name == ["Async"] {
                    return match args.as_slice() {
                        [a] => Ty::Async(Box::new(self.resolve_type(a))),
                        _ => {
                            self.error(
                                "check::unknown_type",
                                "`Async` takes one type: `Async<T>`",
                                span,
                                "expected one type argument",
                            );
                            Ty::Error
                        }
                    };
                }
                if !args.is_empty() {
                    self.error(
                        "check::unknown_type",
                        format!("`{}` takes no type arguments", name.join(".")),
                        span,
                        "remove the `<…>`",
                    );
                }
                if let [one] = name.as_slice() {
                    if let Some(id) = self.globals.get(*one).copied() {
                        match self.defs[id.0 as usize].kind {
                            DefKind::Enum(e) => {
                                self.add_ref(path[0].span, Target::Def(id));
                                return Ty::Enum(e);
                            }
                            DefKind::Type(r) => {
                                self.add_ref(path[0].span, Target::Def(id));
                                return Ty::Record(r);
                            }
                            _ => {}
                        }
                    }
                    if let Some(ty) = self.schema.named_type(one) {
                        return ty;
                    }
                }
                let mut candidates: Vec<String> = [
                    "int", "float", "bool", "text", "color", "paint", "path", "length", "percent",
                    "angle", "duration", "font", "shadow", "Async",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                candidates.extend(self.types.records.iter().map(|r| r.name.clone()));
                candidates.extend(self.types.enums.iter().map(|e| e.name.clone()));
                candidates.extend(self.schema.aliases.keys().cloned());
                candidates.extend(self.schema.opaques.keys().cloned());
                let written = name.join(".");
                let help = Self::did_you_mean(&written, &candidates);
                let d = self.error(
                    "check::unknown_type",
                    format!("unknown type `{written}`"),
                    span,
                    "not a type",
                );
                d.help = help;
                Ty::Error
            }
        }
    }
}

impl<'a> Checker<'a> {
    /// The element schema for kind `name`.
    pub fn element_schema(&self, name: &str) -> Option<&'a ElementSchema> {
        self.schema.element(name)
    }
}
