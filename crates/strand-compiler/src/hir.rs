//! The typed, resolved program: what the checker hands to VM lowering, the
//! reconciler and the LSP.
//!
//! Every name is resolved (to a [`Def`], a [`Local`], a service, a builtin,
//! an enum variant or a token path) and every expression carries its
//! [`Ty`] and its [`Span`]. Spans are file-local; the file is the one of
//! the enclosing [`FileHir`]. The tree mirrors the syntax tree (see
//! `docs/architecture.md`, "strand-compiler"), with three differences:
//!
//! - an element's props are split from its children, and `when`, poses and
//!   selectors hold props only;
//! - each element has a program-unique [`NodeIdx`]; `hover`, `self` and an
//!   `id:` name all read [`ExprKind::Node`] of the element they mean;
//! - comma shorthands, space-separated values and component arguments keep
//!   their parts, typed against the prop or parameter they fill.
//!
//! [`Program::refs`] lists every resolved name use with its target, for
//! go-to-definition, rename and hover.

use std::collections::BTreeMap;

use crate::source::FileId;
use crate::syntax::Span;
pub use crate::syntax::ast::{AssignOp, BinaryOp, PoseKind, TimerKind, UnaryOp, Unit};
use crate::ty::{EnumId, RecordId, Ty, TypeTable};

/// A named declaration: component, surface, state, let, fn, type, enum,
/// token set, keyframes or service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);

/// A local name: a parameter, lambda or handler parameter, `for` binding,
/// handler `let`, or an element `id:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// An element of the program, unique across files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeIdx(pub u32);

/// The checked program.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub files: Vec<FileHir>,
    pub defs: Vec<Def>,
    pub locals: Vec<Local>,
    /// Schema and user records and enums.
    pub types: TypeTable,
    /// Every token path the program can read, with its type.
    pub tokens: BTreeMap<String, Ty>,
    /// Resolved name uses, in source order per file.
    pub refs: Vec<Reference>,
}

impl Program {
    pub fn def(&self, id: DefId) -> &Def {
        &self.defs[id.0 as usize]
    }

    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id.0 as usize]
    }

    /// The narrowest reference covering `offset` in `file`.
    pub fn reference_at(&self, file: FileId, offset: u32) -> Option<&Reference> {
        self.refs
            .iter()
            .filter(|r| r.file == file && r.span.start <= offset && offset <= r.span.end)
            .min_by_key(|r| r.span.len())
    }

    /// Top-level `export`s as `file.name` paths (the CLI's names).
    pub fn exports(&self) -> impl Iterator<Item = (String, DefId)> + '_ {
        self.defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.exported)
            .map(|(i, d)| {
                let file = self
                    .files
                    .iter()
                    .find(|f| f.file == d.file)
                    .map_or("", |f| f.name.as_str());
                (format!("{file}.{}", d.name), DefId(i as u32))
            })
    }
}

/// One `.strand` file.
#[derive(Clone, Debug)]
pub struct FileHir {
    pub file: FileId,
    /// The file stem: `theme` for `theme.strand`, the `theme` of
    /// `theme.look`.
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub name: String,
    pub kind: DefKind,
    pub file: FileId,
    /// The declared name.
    pub span: Span,
    /// A value's type (state, let, fn, settings record, service record);
    /// `Ty::Unit` for declarations that are not values.
    pub ty: Ty,
    pub exported: bool,
    /// The component or surface a nested `state`/`let` belongs to.
    pub owner: Option<DefId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefKind {
    Component,
    /// `bar`, `panel`, `osd`, `lock` (the element kind).
    Surface(String),
    State,
    /// `state x from "file.toml" { … }`.
    Settings,
    Let,
    Fn,
    Enum(EnumId),
    Type(RecordId),
    Tokens,
    Keyframes,
    /// `service x from …`.
    Service(RecordId),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub name: String,
    pub ty: Ty,
    pub file: FileId,
    pub span: Span,
    pub kind: LocalKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// A component or fn parameter.
    Param,
    LambdaParam,
    /// An event's parameter (`dy` in `on scroll(dy)`).
    EventParam,
    /// `for x in …`.
    ForBinding,
    /// `let` inside a handler or fn body.
    Let,
    /// A name an element brings into scope (`screen` in a bar, `index` in
    /// `letters`).
    ElementScope,
    /// A colour channel inside `oklch(from …)`.
    Channel,
    /// An element named with `id:`.
    NodeId(NodeIdx),
}

/// A resolved name use.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference {
    pub file: FileId,
    pub span: Span,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Def(DefId),
    Local(LocalId),
    /// A schema service such as `battery`.
    Service(String),
    /// A builtin function or value.
    Builtin(String),
    Variant(EnumId, u32),
    Token(String),
    Field(RecordId, String),
    Element(String),
    /// A file's namespace (`theme` in `theme.look`).
    File(FileId),
}

// ---------------------------------------------------------------------------
// Declarations

#[derive(Clone, Debug)]
pub enum Item {
    Component(Component),
    Surface(Surface),
    State(StateDecl),
    Let(LetDecl),
    Fn(FnDecl),
    Enum(DefId),
    Type(DefId),
    Tokens(TokenSet),
    Use(Use),
    Service(ServiceDecl),
    Keyframes(Keyframes),
    Handler(Handler),
    Timer(Timer),
    Permit(Permit),
}

#[derive(Clone, Debug)]
pub struct Component {
    pub def: DefId,
    pub params: Vec<Param>,
    /// `tokens { radius: … }`, readable as `$Name.radius`.
    pub tokens: Vec<TokenDef>,
    pub body: Vec<Node>,
    pub has_slot: bool,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub local: LocalId,
    pub default: Option<Expr>,
}

/// A `bar`, `panel`, `osd` or `lock`: its root element.
#[derive(Clone, Debug)]
pub struct Surface {
    pub def: Option<DefId>,
    pub element: Element,
}

#[derive(Clone, Debug)]
pub struct StateDecl {
    pub def: DefId,
    pub init: StateInit,
    pub persist: bool,
    /// `@reset`.
    pub reset: bool,
}

#[derive(Clone, Debug)]
pub enum StateInit {
    Value {
        /// The key field path of a keyed collection (`key app`).
        key: Option<Vec<String>>,
        value: Expr,
    },
    File {
        path: String,
        fields: Vec<SettingsField>,
    },
}

#[derive(Clone, Debug)]
pub struct SettingsField {
    pub name: String,
    pub span: Span,
    pub ty: Ty,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct LetDecl {
    pub def: DefId,
    pub value: Expr,
    pub reset: bool,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub def: DefId,
    pub params: Vec<LocalId>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub struct TokenSet {
    pub def: DefId,
    pub extends: Option<DefId>,
    pub entries: Vec<TokenDef>,
}

/// One token definition, with groups flattened (`space { 1: 4px }` is
/// `space.1`).
#[derive(Clone, Debug)]
pub struct TokenDef {
    pub path: String,
    pub span: Span,
    pub override_: bool,
    pub value: Expr,
}

/// `use tokens a, palette b`.
#[derive(Clone, Debug)]
pub struct Use {
    pub tokens: Option<Expr>,
    pub palette: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct ServiceDecl {
    pub def: DefId,
    /// The file it is declared in, and its name's and source's spans
    /// (diagnostics found later: D-Bus introspection).
    pub file: FileId,
    pub name_span: Span,
    pub source_span: Span,
    /// `dbus`, `file`, `listen` or `poll`.
    pub source: String,
    pub args: Vec<Expr>,
    pub every: Option<Expr>,
    pub fields: Vec<ServiceField>,
    /// The source with its arguments evaluated (they must be constants);
    /// `None` when they are not (an error says why).
    pub spec: Option<SourceSpec>,
}

/// Where a no-code service reads from (design.md: `from dbus`, `from
/// file`, `from listen`, `from poll`), its arguments evaluated.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    /// Properties of `name`'s object at `path` (by default the name with
    /// `.` as `/`) on the system or session bus.
    Dbus {
        system: bool,
        name: String,
        path: Option<String>,
    },
    /// A file, read whenever it changes (`~/` is the home directory, a
    /// relative path the config directory).
    File { path: String },
    /// A long-running command: each line it prints is a document.
    Listen { command: Vec<String> },
    /// A command run (or a file read) every `every`, only while read.
    Poll {
        target: PollTarget,
        every: std::time::Duration,
    },
}

/// The shortest interval a `poll` of a command runs at: a shorter
/// `every` is warned about and runs at this (strand-services'
/// `custom::MIN_COMMAND_POLL`). A file poll forks nothing and has no floor.
pub const MIN_COMMAND_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// What a `poll` service runs or reads.
#[derive(Clone, Debug, PartialEq)]
pub enum PollTarget {
    /// The program and its arguments.
    Command(Vec<String>),
    /// A file (`"/sys/class/…"`: a path, no spaces, no arguments).
    File(String),
}

#[derive(Clone, Debug)]
pub struct ServiceField {
    pub name: String,
    pub span: Span,
    pub ty: Ty,
    pub rw: bool,
    /// The D-Bus property, JSON key or file key it reads.
    pub source_key: Option<String>,
    /// That key as a path (`= a.b` is `["a", "b"]`; a string is one
    /// segment); the field's own name when it names none.
    pub key: Vec<String>,
    /// Where the key is written (the field's name when it names none).
    pub key_span: Span,
}

#[derive(Clone, Debug)]
pub struct Keyframes {
    pub def: DefId,
    pub stops: Vec<KeyframeStop>,
    /// `duration: 300ms` and friends.
    pub props: Vec<Prop>,
}

#[derive(Clone, Debug)]
pub struct KeyframeStop {
    /// Percentages, as written (`25` for `25%`).
    pub at: Vec<f64>,
    pub props: Vec<Prop>,
}

#[derive(Clone, Debug)]
pub struct Permit {
    pub capability: String,
    pub programs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Trees

#[derive(Clone, Debug)]
pub enum Node {
    Element(Element),
    When(When),
    If(IfNode),
    For(ForNode),
    Match(MatchNode),
    Handler(Handler),
    Timer(Timer),
    Pose(Pose),
    Slot(Span),
    /// `set { $x: … }`.
    Set(Vec<TokenDef>, Span),
    Selector(Selector),
    Play(Expr),
    State(StateDecl),
    Let(LetDecl),
}

#[derive(Clone, Debug)]
pub struct Element {
    pub node: NodeIdx,
    pub kind: ElementKind,
    pub span: Span,
    /// The positional argument (for a component, its first parameter).
    pub arg: Option<Expr>,
    pub id: Option<LocalId>,
    pub props: Vec<Prop>,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ElementKind {
    /// A schema element (`text`, `row`, `bar`).
    Builtin(String),
    Component(DefId),
    /// Already reported.
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct Prop {
    pub name: String,
    pub span: Span,
    pub value: Expr,
    /// `<->`: `value` is a writable place.
    pub two_way: bool,
    pub transition: Option<Expr>,
    pub sub: Vec<Prop>,
    /// Reaches every descendant (`font`, `color`).
    pub inherited: bool,
}

#[derive(Clone, Debug)]
pub struct When {
    pub cond: Expr,
    pub props: Vec<Prop>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct IfNode {
    pub cond: Expr,
    pub then: Vec<Node>,
    pub else_: Vec<Node>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ForNode {
    pub binding: LocalId,
    pub iter: Expr,
    /// The explicit `key`; `None` when items bring their own.
    pub key: Option<Expr>,
    pub body: Vec<Node>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct MatchNode {
    pub scrutinee: Expr,
    pub arms: Vec<(Pattern, Vec<Node>)>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Handler {
    pub event: Event,
    pub params: Vec<LocalId>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// `on click` on the enclosing element (or surface).
    Element(String),
    /// `on notifications.received(n)`.
    Service { service: String, event: String },
    /// `on change a, b after T`.
    Change {
        targets: Vec<Expr>,
        debounce: Option<Expr>,
    },
}

#[derive(Clone, Debug)]
pub struct Timer {
    pub kind: TimerKind,
    pub duration: Expr,
    pub while_: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Pose {
    pub kind: PoseKind,
    pub props: Vec<Prop>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Selector {
    /// (M4) The `svg_part` node it lowers to.
    pub node: NodeIdx,
    pub name: String,
    pub props: Vec<Prop>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Wildcard,
    Variant(EnumId, u32),
    Literal(Expr),
    Error,
}

// ---------------------------------------------------------------------------
// Statements

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    Let {
        local: LocalId,
        value: Expr,
    },
    /// The target is a writable place: state, a `rw` service field or a
    /// settings field (or a field or index inside one).
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    Expr(Expr),
    If {
        cond: Expr,
        then: Vec<Stmt>,
        else_: Vec<Stmt>,
    },
    For {
        binding: LocalId,
        iter: Expr,
        body: Vec<Stmt>,
    },
    Match {
        scrutinee: Expr,
        arms: Vec<(Pattern, Vec<Stmt>)>,
    },
    Play(Expr),
    Error,
}

// ---------------------------------------------------------------------------
// Expressions

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

impl Expr {
    pub fn error(span: Span) -> Self {
        Self {
            kind: ExprKind::Error,
            ty: Ty::Error,
            span,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Number {
        value: f64,
        unit: Option<Unit>,
    },
    Text(String),
    Color([u8; 4]),
    Bool(bool),
    Null,
    Local(LocalId),
    /// A state, let, fn, settings file, custom service or token set.
    Def(DefId),
    /// A schema service.
    Service(String),
    /// A builtin value (`t`).
    Value(String),
    /// An element: `self`, an `id:` name, or the node a bare `hover`
    /// belongs to.
    Node(NodeIdx),
    Variant(EnumId, u32),
    /// An enum used as a value (`options: Look`).
    EnumType(EnumId),
    /// `$space.2`.
    Token(String),
    Field {
        base: Box<Expr>,
        name: String,
        optional: bool,
    },
    Call {
        callee: Callee,
        args: Vec<CallArg>,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        else_: Box<Expr>,
    },
    Lambda {
        params: Vec<LocalId>,
        body: LambdaBody,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
    },
    List(Vec<Expr>),
    /// `8, 8, 0` (prop and token values).
    Commas(Vec<Expr>),
    /// `0 2px 8px $shadow` (shadow and font values).
    Spaced(Vec<Expr>),
    Error,
}

#[derive(Clone, Debug)]
pub enum LambdaBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Clone, Debug)]
pub enum Callee {
    /// A user `fn`.
    Fn(DefId),
    /// A builtin function and the overload chosen.
    Builtin {
        name: String,
        overload: usize,
    },
    /// A method on a value (service, record, list, colour, text …).
    Method {
        receiver: Box<Expr>,
        name: String,
        overload: usize,
    },
    /// `Pin(app: a, label: "x")`: builds a record.
    Record(RecordId),
    /// Calling a function value (a lambda parameter).
    Value(Box<Expr>),
    Error,
}

#[derive(Clone, Debug)]
pub struct CallArg {
    /// The parameter this argument fills, by index into the signature
    /// (variadic arguments share the variadic parameter's index).
    pub param: Option<usize>,
    pub value: Expr,
    pub span: Span,
}
