//! Lowering: the typed HIR becomes a [`Program`] the VM runs.
//!
//! Every expression a binding, `let`, `fn`, handler or timer evaluates is
//! compiled to a [`Chunk`] of bytecode ([`code`]); the tree around them is
//! kept as a small structure ([`Node`]) the instantiator mounts: elements
//! with their props (scene props already resolved), `when` blocks, poses,
//! handlers, timers, `if`/`match`/`for` and component calls. A program is
//! plain data (`Send`), so the compiler worker can build it off the logic
//! thread.

pub mod code;
mod expr;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub use code::{ArgMap, Chunk, ChunkId, Const, Lambda, Op, Pattern, Place, PlaceRoot, PlaceSeg};
use strand_scene::{NodeKind, Prop as SceneProp};

use crate::hir::{self, DefId, DefKind, LocalId, LocalKind, NodeIdx, PoseKind, TimerKind};
use crate::schema::Schema;
use crate::source::FileId;
use crate::syntax::Span;
use crate::ty::{RecordId, Ty, TypeTable};

/// A compiled program.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub types: TypeTable,
    /// Every token path the program can read, with its type.
    pub tokens: BTreeMap<String, Ty>,
    pub chunks: Vec<Chunk>,
    pub defs: Vec<DefInfo>,
    pub locals: Vec<LocalInfo>,
    pub files: Vec<FileProgram>,
    pub components: BTreeMap<DefId, Component>,
    pub fns: BTreeMap<DefId, FnProgram>,
    pub token_sets: BTreeMap<DefId, TokenSet>,
    /// `use tokens …, palette …`.
    pub use_tokens: Option<ChunkId>,
    pub use_palette: Option<ChunkId>,
    /// Custom services (`service ppd from dbus …`): name and record.
    pub services: BTreeMap<DefId, (String, RecordId)>,
    /// Keyframes by declaration.
    pub keyframes: BTreeMap<DefId, String>,
    /// The `key` of keyed `state` collections (`state pins: [Pin] key
    /// app`), by declaration.
    pub state_keys: BTreeMap<DefId, Vec<String>>,
}

/// What the VM needs to know about a declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct DefInfo {
    pub name: String,
    pub kind: DefKind,
    /// The module (file stem) it is declared in.
    pub module: String,
    pub ty: Ty,
    pub exported: bool,
    /// The component or surface a nested `state`/`let` belongs to.
    pub owner: Option<DefId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalInfo {
    pub name: String,
    pub kind: LocalKind,
    pub ty: Ty,
}

/// One file's top-level runtime items, in source order.
#[derive(Clone, Debug, Default)]
pub struct FileProgram {
    pub file: FileId,
    pub module: String,
    pub items: Vec<Node>,
}

/// A component declaration.
#[derive(Clone, Debug)]
pub struct Component {
    pub def: DefId,
    pub name: String,
    /// Parameters with their defaults (evaluated in the component's scope
    /// when the caller passes nothing).
    pub params: Vec<(LocalId, Option<ChunkId>)>,
    pub body: Body,
    /// `tokens { radius: … }` as `Name.radius` entries.
    pub tokens: Vec<TokenDef>,
}

/// A `fn`.
#[derive(Clone, Debug)]
pub struct FnProgram {
    pub params: Vec<LocalId>,
    pub body: ChunkId,
}

/// A `tokens` set, groups flattened.
#[derive(Clone, Debug)]
pub struct TokenSet {
    pub name: String,
    pub extends: Option<DefId>,
    pub entries: Vec<TokenDef>,
}

#[derive(Clone, Debug)]
pub struct TokenDef {
    /// Without the `$`: `space.2`, `Toast.radius`.
    pub path: String,
    pub ty: Ty,
    pub value: ChunkId,
}

/// The items of a component, surface, `for` item or branch, with the
/// elements it owns (whose `hover`, `id:` names and size live in its
/// scope) and the services it reads.
#[derive(Clone, Debug, Default)]
pub struct Body {
    pub nodes: Arc<Vec<Node>>,
    /// Elements in this body (not in nested `for` items or components).
    pub owned: Arc<BTreeSet<NodeIdx>>,
    /// Services read anywhere in this body (acquired on mount).
    pub services: Arc<BTreeSet<String>>,
}

/// A tree item.
#[derive(Clone, Debug)]
pub enum Node {
    Element(Element),
    /// A surface at the top of a file (a `bar` is instantiated per
    /// monitor).
    Surface(Surface),
    If {
        cond: ChunkId,
        then: Arc<Vec<Node>>,
        else_: Arc<Vec<Node>>,
    },
    For(For),
    /// A tree `match`: `selector` gives the index of the arm to mount
    /// (`null` for none).
    Match {
        selector: ChunkId,
        arms: Vec<Arc<Vec<Node>>>,
    },
    Slot,
    State(State),
    Let {
        def: DefId,
        value: ChunkId,
    },
    Handler(Handler),
    Timer(Timer),
    /// `when cond { props }` on the enclosing element.
    When {
        cond: ChunkId,
        props: Vec<Prop>,
    },
    Pose {
        kind: PoseKind,
        props: Vec<Prop>,
    },
    /// `set { $x: … }`: token overrides for the enclosing subtree.
    Set(Vec<TokenDef>),
    /// `play shake` in the tree (not a handler).
    Play(ChunkId),
}

/// A surface declaration.
#[derive(Clone, Debug)]
pub struct Surface {
    pub def: Option<DefId>,
    /// `Top` in `bar Top`.
    pub name: Option<String>,
    pub kind: NodeKind,
    /// `screen` in a `bar`: one instance per monitor.
    pub screen: Option<LocalId>,
    pub element: Element,
    pub body: Body,
}

/// What an element is.
#[derive(Clone, Debug, PartialEq)]
pub enum ElementKind {
    /// A scene node kind.
    Builtin(NodeKind),
    Component(DefId),
    /// An element the scene does not know (already reported).
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct Element {
    pub node: NodeIdx,
    pub kind: ElementKind,
    /// The positional argument, as the prop it fills (`text x` → `text`)
    /// or a component's first parameter.
    pub arg: Option<Prop>,
    /// Props (for a component: named arguments).
    pub props: Vec<Prop>,
    pub children: Arc<Vec<Node>>,
    /// The `id:` name, if any.
    pub id: Option<LocalId>,
    /// The `screen`/`index` names the element brings into scope.
    pub scope: Vec<LocalId>,
    pub span: Span,
    pub file: FileId,
}

/// A prop binding.
#[derive(Clone, Debug)]
pub struct Prop {
    pub name: String,
    /// The scene prop; `None` for compiler-only props (`id`) and
    /// component arguments.
    pub prop: Option<SceneProp>,
    /// The type the prop expects (for converting values).
    pub ty: Ty,
    pub value: ChunkId,
    /// `<->`: where widget writes go.
    pub two_way: Option<TwoWay>,
    /// `~ …`.
    pub transition: Option<ChunkId>,
    pub sub: Vec<Prop>,
}

/// The target of a two-way binding.
#[derive(Clone, Debug)]
pub struct TwoWay {
    pub place: Place,
    /// One chunk per `Index` segment of the place.
    pub indices: Vec<ChunkId>,
    /// The type of the target (for converting written values).
    pub ty: Ty,
}

#[derive(Clone, Debug)]
pub struct For {
    pub binding: LocalId,
    pub iter: ChunkId,
    pub key: ForKey,
    pub body: Body,
}

/// How a `for` item is identified.
#[derive(Clone, Debug)]
pub enum ForKey {
    /// `key e`, evaluated with the binding in scope.
    Expr(ChunkId),
    /// The item record's own key field path.
    Path(Vec<String>),
    /// The value itself.
    Value,
}

#[derive(Clone, Debug)]
pub struct State {
    pub def: DefId,
    pub init: StateInit,
    pub persist: bool,
}

#[derive(Clone, Debug)]
pub enum StateInit {
    Value(ChunkId),
    /// A settings file: its record and each field's default.
    Settings {
        path: String,
        record: RecordId,
        fields: Vec<Option<ChunkId>>,
    },
}

#[derive(Clone, Debug)]
pub struct Handler {
    pub event: Event,
    pub params: Vec<LocalId>,
    pub body: ChunkId,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// `on click` and friends, on the enclosing element.
    Element(String),
    /// `on notifications.received(n)`.
    Service { service: String, event: String },
    /// `on change a, b [after T]`: each target's value chunk and, for a
    /// field of a keyed item (`audio.sink.volume`), the chunk giving the
    /// item whose identity re-baselines the handler.
    Change {
        targets: Vec<(ChunkId, Option<ChunkId>)>,
        debounce: Option<ChunkId>,
    },
}

#[derive(Clone, Debug)]
pub struct Timer {
    pub kind: TimerKind,
    pub duration: ChunkId,
    pub while_: Option<ChunkId>,
    pub body: ChunkId,
}

impl Program {
    pub fn chunk(&self, id: ChunkId) -> &Chunk {
        &self.chunks[id as usize]
    }

    pub fn def(&self, id: DefId) -> &DefInfo {
        &self.defs[id.0 as usize]
    }

    pub fn local(&self, id: LocalId) -> &LocalInfo {
        &self.locals[id.0 as usize]
    }

    /// `file.name` paths of exported values (the CLI's names).
    pub fn exports(&self) -> impl Iterator<Item = (String, DefId)> + '_ {
        self.defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.exported)
            .map(|(i, d)| (format!("{}.{}", d.module, d.name), DefId(i as u32)))
    }
}

/// Lowers a checked program against the schema it was checked with.
pub fn lower(program: &hir::Program, schema: &Schema) -> Program {
    let mut l = Lowerer {
        hir: program,
        schema,
        out: Program {
            types: program.types.clone(),
            tokens: program.tokens.clone(),
            ..Program::default()
        },
        file: FileId(0),
        owned: Vec::new(),
        services: Vec::new(),
    };
    for d in &program.defs {
        let module = program
            .files
            .iter()
            .find(|f| f.file == d.file)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        l.out.defs.push(DefInfo {
            name: d.name.clone(),
            kind: d.kind.clone(),
            module,
            ty: d.ty.clone(),
            exported: d.exported,
            owner: d.owner,
        });
    }
    for loc in &program.locals {
        l.out.locals.push(LocalInfo {
            name: loc.name.clone(),
            kind: loc.kind,
            ty: loc.ty.clone(),
        });
    }
    for f in &program.files {
        for item in &f.items {
            collect_keys(item, &mut l.out.state_keys);
        }
    }
    for f in &program.files {
        l.file = f.file;
        let mut fp = FileProgram {
            file: f.file,
            module: f.name.clone(),
            items: Vec::new(),
        };
        for item in &f.items {
            l.item(item, &mut fp.items);
        }
        l.out.files.push(fp);
    }
    l.out
}

pub(crate) struct Lowerer<'a> {
    pub(crate) hir: &'a hir::Program,
    pub(crate) schema: &'a Schema,
    pub(crate) out: Program,
    pub(crate) file: FileId,
    /// Elements of the bodies being lowered, innermost last.
    owned: Vec<BTreeSet<NodeIdx>>,
    /// Services read by the bodies being lowered, innermost last.
    pub(crate) services: Vec<BTreeSet<String>>,
}

impl Lowerer<'_> {
    fn item(&mut self, item: &hir::Item, out: &mut Vec<Node>) {
        match item {
            hir::Item::Component(c) => {
                let params = c
                    .params
                    .iter()
                    .map(|p| (p.local, p.default.as_ref().map(|d| self.expr_chunk(d))))
                    .collect();
                let tokens = c
                    .tokens
                    .iter()
                    .map(|t| self.token_def(t, &format!("{}.", self.hir.def(c.def).name)))
                    .collect();
                let body = self.body(&c.body);
                self.out.components.insert(
                    c.def,
                    Component {
                        def: c.def,
                        name: self.hir.def(c.def).name.clone(),
                        params,
                        body,
                        tokens,
                    },
                );
            }
            hir::Item::Surface(s) => {
                self.begin_body();
                let element = self.element(&s.element);
                let body = self.end_body(Vec::new());
                let kind = match &element.kind {
                    ElementKind::Builtin(k) => *k,
                    _ => NodeKind::Panel,
                };
                let screen = (kind == NodeKind::Bar)
                    .then(|| self.scope_local(s.element.span, "screen"))
                    .flatten();
                out.push(Node::Surface(Surface {
                    def: s.def,
                    name: s.def.map(|d| self.hir.def(d).name.clone()),
                    kind,
                    screen,
                    element,
                    body,
                }));
            }
            hir::Item::State(s) => out.push(Node::State(self.state(s))),
            hir::Item::Let(l) => out.push(Node::Let {
                def: l.def,
                value: self.expr_chunk(&l.value),
            }),
            hir::Item::Fn(f) => {
                let body = self.fn_chunk(&f.body);
                self.out.fns.insert(
                    f.def,
                    FnProgram {
                        params: f.params.clone(),
                        body,
                    },
                );
            }
            hir::Item::Tokens(t) => {
                let entries = t.entries.iter().map(|e| self.token_def(e, "")).collect();
                self.out.token_sets.insert(
                    t.def,
                    TokenSet {
                        name: self.hir.def(t.def).name.clone(),
                        extends: t.extends,
                        entries,
                    },
                );
            }
            hir::Item::Use(u) => {
                if let Some(t) = &u.tokens {
                    self.out.use_tokens = Some(self.expr_chunk(t));
                }
                if let Some(p) = &u.palette {
                    self.out.use_palette = Some(self.expr_chunk(p));
                }
            }
            hir::Item::Service(s) => {
                if let DefKind::Service(r) = self.hir.def(s.def).kind {
                    self.out
                        .services
                        .insert(s.def, (self.hir.def(s.def).name.clone(), r));
                }
            }
            hir::Item::Keyframes(k) => {
                self.out
                    .keyframes
                    .insert(k.def, self.hir.def(k.def).name.clone());
            }
            hir::Item::Handler(h) => out.push(Node::Handler(self.handler(h))),
            hir::Item::Timer(t) => out.push(Node::Timer(self.timer(t))),
            hir::Item::Enum(_) | hir::Item::Type(_) | hir::Item::Permit(_) => {}
        }
    }

    fn begin_body(&mut self) {
        self.owned.push(BTreeSet::new());
        self.services.push(BTreeSet::new());
    }

    fn end_body(&mut self, nodes: Vec<Node>) -> Body {
        let owned = self.owned.pop().unwrap_or_default();
        let services = self.services.pop().unwrap_or_default();
        // A body's services are also read by the bodies around it.
        if let Some(outer) = self.services.last_mut() {
            outer.extend(services.iter().cloned());
        }
        Body {
            nodes: Arc::new(nodes),
            owned: Arc::new(owned),
            services: Arc::new(services),
        }
    }

    fn body(&mut self, nodes: &[hir::Node]) -> Body {
        self.begin_body();
        let lowered = self.nodes(nodes);
        self.end_body(lowered)
    }

    /// The element-scope local `name` bound at the element spanning `span`.
    fn scope_local(&self, span: Span, name: &str) -> Option<LocalId> {
        self.hir
            .locals
            .iter()
            .position(|l| {
                l.kind == LocalKind::ElementScope
                    && l.file == self.file
                    && l.name == name
                    && span.start <= l.span.start
                    && l.span.end <= span.end
            })
            .map(|i| LocalId(i as u32))
    }

    fn nodes(&mut self, nodes: &[hir::Node]) -> Vec<Node> {
        let mut out = Vec::new();
        for n in nodes {
            if let Some(n) = self.node(n) {
                out.push(n);
            }
        }
        out
    }

    fn node(&mut self, n: &hir::Node) -> Option<Node> {
        Some(match n {
            hir::Node::Element(e) => Node::Element(self.element(e)),
            hir::Node::When(w) => Node::When {
                cond: self.expr_chunk(&w.cond),
                props: self.props(&w.props, None),
            },
            hir::Node::If(i) => Node::If {
                cond: self.expr_chunk(&i.cond),
                then: Arc::new(self.nodes(&i.then)),
                else_: Arc::new(self.nodes(&i.else_)),
            },
            hir::Node::For(f) => {
                let iter = self.expr_chunk(&f.iter);
                let key = match &f.key {
                    Some(k) => ForKey::Expr(self.expr_chunk(k)),
                    None => match self.root_key(&f.iter) {
                        Some(path) => ForKey::Path(path),
                        None => match f.iter.ty.list_elem().map(|(t, _)| t.non_null().clone()) {
                            Some(Ty::Record(r)) => match &self.hir.types.record(r).key {
                                Some(path) => ForKey::Path(path.clone()),
                                None => ForKey::Value,
                            },
                            _ => ForKey::Value,
                        },
                    },
                };
                Node::For(For {
                    binding: f.binding,
                    iter,
                    key,
                    body: self.body(&f.body),
                })
            }
            hir::Node::Match(m) => {
                let pats: Vec<&hir::Pattern> = m.arms.iter().map(|(p, _)| p).collect();
                let selector = self.match_selector(&m.scrutinee, &pats);
                let arms = m
                    .arms
                    .iter()
                    .map(|(_, body)| Arc::new(self.nodes(body)))
                    .collect();
                Node::Match { selector, arms }
            }
            hir::Node::Handler(h) => Node::Handler(self.handler(h)),
            hir::Node::Timer(t) => Node::Timer(self.timer(t)),
            hir::Node::Pose(p) => Node::Pose {
                kind: p.kind,
                props: self.props(&p.props, None),
            },
            hir::Node::Slot(_) => Node::Slot,
            hir::Node::Set(defs, _) => {
                Node::Set(defs.iter().map(|d| self.token_def(d, "")).collect())
            }
            // `#needle { … }` inside an `svg` lands with bindable SVG (M4).
            hir::Node::Selector(_) => return None,
            hir::Node::Play(e) => Node::Play(self.expr_chunk(e)),
            hir::Node::State(s) => Node::State(self.state(s)),
            hir::Node::Let(l) => Node::Let {
                def: l.def,
                value: self.expr_chunk(&l.value),
            },
        })
    }

    fn element(&mut self, e: &hir::Element) -> Element {
        if let Some(top) = self.owned.last_mut() {
            top.insert(e.node);
        }
        let (kind, schema) = match &e.kind {
            hir::ElementKind::Builtin(name) => match NodeKind::from_name(name) {
                Some(k) => (ElementKind::Builtin(k), self.schema.element(name)),
                None => (ElementKind::Unknown(name.clone()), None),
            },
            hir::ElementKind::Component(d) => (ElementKind::Component(*d), None),
            hir::ElementKind::Unknown(n) => (ElementKind::Unknown(n.clone()), None),
        };
        let arg = e.arg.as_ref().map(|a| {
            let (name, prop) = match &kind {
                ElementKind::Builtin(k) => {
                    let p = positional_prop(*k);
                    (p.name().to_string(), Some(p))
                }
                ElementKind::Component(d) => (self.first_param(*d), None),
                ElementKind::Unknown(_) => (String::new(), None),
            };
            let ty = schema.and_then(|s| s.arg.clone()).unwrap_or(a.ty.clone());
            Prop {
                name,
                prop,
                ty,
                value: self.expr_chunk(a),
                two_way: None,
                transition: None,
                sub: Vec::new(),
            }
        });
        let props = self.props(&e.props, schema);
        let scope = match schema {
            Some(s) => s
                .scope
                .iter()
                .filter_map(|(n, _)| self.scope_local(e.span, n))
                .collect(),
            None => Vec::new(),
        };
        let children = Arc::new(self.nodes(&e.children));
        Element {
            node: e.node,
            kind,
            arg,
            props,
            children,
            id: e.id,
            scope,
            span: e.span,
            file: self.file,
        }
    }

    fn first_param(&self, d: DefId) -> String {
        // Components are lowered in file order, a call may come first: read
        // the HIR's parameter.
        for f in &self.hir.files {
            for item in &f.items {
                if let hir::Item::Component(c) = item
                    && c.def == d
                {
                    return c
                        .params
                        .first()
                        .map(|p| self.hir.local(p.local).name.clone())
                        .unwrap_or_default();
                }
            }
        }
        String::new()
    }

    fn props(
        &mut self,
        props: &[hir::Prop],
        schema: Option<&crate::schema::ElementSchema>,
    ) -> Vec<Prop> {
        props
            .iter()
            .filter(|p| p.name != "id")
            .map(|p| {
                let ty = schema
                    .and_then(|s| s.prop(&p.name))
                    .map_or_else(|| p.value.ty.clone(), |s| s.ty.clone());
                self.prop(p, ty)
            })
            .collect()
    }

    fn prop(&mut self, p: &hir::Prop, ty: Ty) -> Prop {
        let two_way = if p.two_way {
            let mut indices = Vec::new();
            self.two_way_place(&p.value, &mut indices)
                .map(|place| TwoWay {
                    place,
                    indices,
                    ty: p.value.ty.clone(),
                })
        } else {
            None
        };
        let sub = p
            .sub
            .iter()
            .map(|s| {
                let t = s.value.ty.clone();
                self.prop(s, t)
            })
            .collect();
        Prop {
            name: p.name.clone(),
            prop: SceneProp::from_name(&p.name),
            ty,
            value: self.expr_chunk(&p.value),
            two_way,
            transition: p.transition.as_ref().map(|t| self.expr_chunk(t)),
            sub,
        }
    }

    fn state(&mut self, s: &hir::StateDecl) -> State {
        let init = match &s.init {
            hir::StateInit::Value { value, .. } => StateInit::Value(self.expr_chunk(value)),
            hir::StateInit::File { path, fields } => {
                let record = match &self.hir.def(s.def).ty {
                    Ty::Record(r) => *r,
                    _ => RecordId(0),
                };
                StateInit::Settings {
                    path: path.clone(),
                    record,
                    fields: fields
                        .iter()
                        .map(|f| f.default.as_ref().map(|d| self.expr_chunk(d)))
                        .collect(),
                }
            }
        };
        State {
            def: s.def,
            init,
            persist: s.persist,
        }
    }

    fn token_def(&mut self, t: &hir::TokenDef, prefix: &str) -> TokenDef {
        let path = format!("{prefix}{}", t.path);
        let ty = self
            .hir
            .tokens
            .get(&path)
            .cloned()
            .unwrap_or(t.value.ty.clone());
        TokenDef {
            path,
            ty,
            value: self.expr_chunk(&t.value),
        }
    }

    fn handler(&mut self, h: &hir::Handler) -> Handler {
        let event = match &h.event {
            hir::Event::Element(n) => Event::Element(n.clone()),
            hir::Event::Service { service, event } => {
                if let Some(s) = self.services.last_mut() {
                    s.insert(service.clone());
                }
                Event::Service {
                    service: service.clone(),
                    event: event.clone(),
                }
            }
            hir::Event::Change { targets, debounce } => Event::Change {
                targets: targets
                    .iter()
                    .map(|t| {
                        let key = match &t.kind {
                            hir::ExprKind::Field { base, .. }
                                if matches!(base.ty.non_null(), Ty::Record(r)
                                    if self.hir.types.record(*r).key.is_some()) =>
                            {
                                Some(self.expr_chunk(base))
                            }
                            _ => None,
                        };
                        (self.expr_chunk(t), key)
                    })
                    .collect(),
                debounce: debounce.as_ref().map(|d| self.expr_chunk(d)),
            },
        };
        Handler {
            event,
            params: h.params.clone(),
            body: self.stmts_chunk(&h.body, h.span),
            span: h.span,
        }
    }

    fn timer(&mut self, t: &hir::Timer) -> Timer {
        Timer {
            kind: t.kind,
            duration: self.expr_chunk(&t.duration),
            while_: t.while_.as_ref().map(|w| self.expr_chunk(w)),
            body: self.stmts_chunk(&t.body, t.span),
        }
    }

    /// The `key` of the keyed `state` a list expression derives from
    /// (`pins`, `pins.filter(…)`, `pins.take(3)`), if any.
    fn root_key(&self, e: &hir::Expr) -> Option<Vec<String>> {
        match &e.kind {
            hir::ExprKind::Def(d) => self.out.state_keys.get(d).cloned(),
            hir::ExprKind::Call {
                callee: hir::Callee::Method { receiver, name, .. },
                ..
            } if matches!(
                name.as_str(),
                "filter" | "take" | "skip" | "sort_by" | "reverse"
            ) =>
            {
                self.root_key(receiver)
            }
            _ => None,
        }
    }

    pub(crate) fn add_chunk(&mut self, c: Chunk) -> ChunkId {
        self.out.chunks.push(c);
        self.out.chunks.len() as ChunkId - 1
    }
}

fn collect_keys(item: &hir::Item, out: &mut BTreeMap<DefId, Vec<String>>) {
    fn state(s: &hir::StateDecl, out: &mut BTreeMap<DefId, Vec<String>>) {
        if let hir::StateInit::Value { key: Some(k), .. } = &s.init {
            out.insert(s.def, k.clone());
        }
    }
    fn nodes(ns: &[hir::Node], out: &mut BTreeMap<DefId, Vec<String>>) {
        for n in ns {
            match n {
                hir::Node::State(s) => state(s, out),
                hir::Node::Element(e) => nodes(&e.children, out),
                hir::Node::If(i) => {
                    nodes(&i.then, out);
                    nodes(&i.else_, out);
                }
                hir::Node::For(f) => nodes(&f.body, out),
                hir::Node::Match(m) => {
                    for (_, b) in &m.arms {
                        nodes(b, out);
                    }
                }
                _ => {}
            }
        }
    }
    match item {
        hir::Item::State(s) => state(s, out),
        hir::Item::Component(c) => nodes(&c.body, out),
        hir::Item::Surface(s) => nodes(&s.element.children, out),
        _ => {}
    }
}

/// The prop a builtin element's positional argument fills: `text x` is
/// its `text`, `icon x` and `image x` their `source`, `meter x` its
/// `value` (see `docs/decisions.md`, wave2-vm).
pub fn positional_prop(kind: NodeKind) -> SceneProp {
    match kind {
        NodeKind::Text | NodeKind::Button | NodeKind::Letters => SceneProp::Text,
        NodeKind::Meter | NodeKind::Graph | NodeKind::Merge => SceneProp::Value,
        NodeKind::Effect => SceneProp::Style,
        NodeKind::Page => SceneProp::Name,
        _ => SceneProp::Source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceMap;

    fn lower_src(src: &str) -> Program {
        let mut map = SourceMap::new();
        map.add("t.strand", src.to_string());
        let c = crate::compile(&map);
        assert_eq!(c.errors(), 0, "{:?}", c.diagnostics);
        lower(&c.program, Schema::builtin())
    }

    #[test]
    fn bindings_compile_to_bytecode() {
        let p = lower_src("state a = 1\nlet b = a + 2 > 3 && true\n");
        let FileProgram { items, .. } = &p.files[0];
        let Node::Let { value, .. } = &items[1] else {
            panic!("{items:?}")
        };
        let ops = &p.chunk(*value).ops;
        assert!(matches!(ops[0], Op::Def(_)), "{ops:?}");
        assert!(ops.iter().any(|o| matches!(o, Op::AndJump(_))), "{ops:?}");
    }

    #[test]
    fn a_bar_knows_its_screen_and_owned_nodes() {
        let p = lower_src("bar Top { text screen.name }\n");
        let Node::Surface(s) = &p.files[0].items[0] else {
            panic!()
        };
        assert_eq!(s.kind, NodeKind::Bar);
        assert_eq!(s.name.as_deref(), Some("Top"));
        assert!(s.screen.is_some());
        assert_eq!(s.body.owned.len(), 2);
    }

    #[test]
    fn handlers_with_await_are_coroutines() {
        let p = lower_src("state x = 0\nbar B { box { on click { await sleep(1s); x = 1 } } }\n");
        assert!(p.chunks.iter().any(|c| c.awaits));
    }
}
