//! The syntax tree, with byte spans.
//!
//! Spans live on the wrapper nodes: every [`Item`], [`Stmt`], [`Expr`],
//! [`Block`], [`Ident`], [`Type`], [`Pattern`], [`Arm`], [`Arg`], [`Param`]
//! and [`Field`] has one. The payload structs inside an item or statement
//! (`Component`, `Prop`, `When`, `On`, `Timer`, …) use the span of the
//! `Item`/`Stmt` that holds them. Keywords (`when`, `else`, `key`, `after`,
//! `while`, `extends`, …) are not stored; find them in
//! [`Parse::tokens`](super::Parse::tokens) by offset within the node's span.
//!
//! Spans are absolute offsets, so `PartialEq` on nodes is span-sensitive:
//! an edit above a node changes its spans. Comparing nodes for "did this
//! change" (reload identity, handler hashing) must use a span-insensitive
//! structural hash, such as one over the texts of the significant tokens
//! inside the node's span (`docs/architecture.md`, `strand-compiler`).
//!
//! The tree mirrors `docs/grammar.md` production by production. It is a
//! syntax tree, not a semantic one: names are unresolved, element kinds are
//! unchecked, and error nodes stand in for text that failed to parse.

use super::Span;

/// A name with its span.
#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// One parsed `.strand` file.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    pub items: Vec<Item>,
    pub span: Span,
}

/// `{ … }` holding items or statements.
#[derive(Clone, Debug, PartialEq)]
pub struct Block<T> {
    pub items: Vec<T>,
    /// From `{` to `}` inclusive (to the last item if `}` is missing).
    pub span: Span,
}

/// `@reset`.
#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: Ident,
    pub args: Vec<Arg>,
    pub span: Span,
}

/// A top-level declaration or a tree item.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub attrs: Vec<Attribute>,
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    // Declarations.
    Component(Component),
    Surface(Surface),
    State(State),
    Let(Let),
    Enum(EnumDecl),
    Type(TypeDecl),
    Fn(FnDecl),
    Tokens(TokensDecl),
    Use(Use),
    Service(Service),
    Permit(Permit),
    Keyframes(Keyframes),
    /// A typed field inside a `service` block.
    Field(Field),
    /// `25%, 75% { … }` inside `keyframes`.
    KeyframeStop(KeyframeStop),

    // Tree items.
    Prop(Prop),
    Element(Element),
    When(When),
    If(If<Item>),
    For(For<Item>),
    Match(Match<ArmBody<Item>>),
    On(On),
    Timer(Timer),
    Pose(Pose),
    Slot,
    Set(Block<TokenEntry>),
    Selector(Selector),
    Play(Expr),

    /// Text that could not be parsed as an item.
    Error,
}

// ---------------------------------------------------------------------------
// Declarations

/// `component Name(params) tokens { … } { body }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    pub name: Ident,
    pub params: Option<Vec<Param>>,
    pub tokens: Option<Block<TokenEntry>>,
    pub body: Block<Item>,
}

/// `bar Top { … }`, `panel`, `osd`, `lock`.
#[derive(Clone, Debug, PartialEq)]
pub struct Surface {
    pub kind: Ident,
    pub name: Option<Ident>,
    pub body: Block<Item>,
}

/// `state x: T key k = v persist` or `state x from "f.toml" { fields }`.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub export: Option<Span>,
    pub name: Ident,
    pub init: StateInit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StateInit {
    Value {
        ty: Option<Type>,
        key: Option<Expr>,
        value: Expr,
        persist: Option<Span>,
    },
    /// A typed two-way settings file.
    File {
        path: StringLit,
        fields: Block<Field>,
    },
}

/// `let x: T = e`.
#[derive(Clone, Debug, PartialEq)]
pub struct Let {
    pub export: Option<Span>,
    pub name: Ident,
    pub ty: Option<Type>,
    pub value: Expr,
}

/// `enum Kind { volume, brightness }`.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: Ident,
    pub variants: Vec<Ident>,
}

/// `type Pin { app: AppId; label: text }`.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDecl {
    pub name: Ident,
    pub fields: Block<Field>,
}

/// A typed field of a record, settings file or service:
/// `name: Type rw = default`.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: Ident,
    pub ty: Type,
    pub rw: Option<Span>,
    pub default: Option<Expr>,
    pub span: Span,
}

/// `fn name(params) -> T { body }`.
#[derive(Clone, Debug, PartialEq)]
pub struct FnDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
    pub body: Block<Stmt>,
}

/// `tokens name extends base { … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct TokensDecl {
    pub name: Ident,
    pub extends: Option<Ident>,
    pub entries: Block<TokenEntry>,
}

/// One entry of a tokens, `set` or component `tokens` block.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenEntry {
    pub override_: Option<Span>,
    pub key: TokenKey,
    pub body: TokenBody,
    pub span: Span,
}

/// `surface.hi`, `$surface`, `1`: segments of a token name.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenKey {
    /// Written with a leading `$`.
    pub dollar: bool,
    pub segments: Vec<Ident>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenBody {
    Value(Expr),
    Group(Block<TokenEntry>),
}

/// `use tokens a, palette b`.
#[derive(Clone, Debug, PartialEq)]
pub struct Use {
    pub clauses: Vec<UseClause>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UseClause {
    /// `tokens` or `palette`.
    pub kind: Ident,
    pub value: Expr,
    pub span: Span,
}

/// `service name from kind args every T { fields }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub name: Ident,
    pub source: ServiceSource,
    /// Fields and `permit`s.
    pub body: Block<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServiceSource {
    /// `dbus`, `file`, `listen` or `poll`.
    pub kind: Ident,
    pub args: Vec<Expr>,
    pub every: Option<Expr>,
    pub span: Span,
}

/// `permit exec "cmd", …`.
#[derive(Clone, Debug, PartialEq)]
pub struct Permit {
    pub capability: Ident,
    pub args: Vec<Expr>,
}

/// `keyframes name { 0% { … }; duration: 300ms }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Keyframes {
    pub name: Ident,
    pub body: Block<Item>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyframeStop {
    /// Percentages.
    pub stops: Vec<Expr>,
    pub body: Block<Item>,
}

// ---------------------------------------------------------------------------
// Tree items

/// `name: value ~ transition { sub-props }` or `name: <-> place`.
#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    pub name: Ident,
    /// The `<->` of a two-way binding.
    pub two_way: Option<Span>,
    pub value: Expr,
    pub transition: Option<Expr>,
    pub block: Option<Block<Item>>,
}

/// `kind [positional] { … }`: built-in elements and components alike.
#[derive(Clone, Debug, PartialEq)]
pub struct Element {
    pub kind: Ident,
    pub arg: Option<HeadArg>,
    pub block: Option<Block<Item>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HeadArg {
    Positional(Expr),
    /// `pages current: page { … }`: sugar for a first prop.
    Named(Ident, Expr),
}

/// `when cond { props }`.
#[derive(Clone, Debug, PartialEq)]
pub struct When {
    pub cond: Expr,
    pub body: Block<Item>,
}

/// `if c { … } else …`, in a tree (`T = Item`) or a handler (`T = Stmt`).
#[derive(Clone, Debug, PartialEq)]
pub struct If<T> {
    pub cond: Expr,
    pub then: Block<T>,
    pub else_: Option<Else<T>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Else<T> {
    If(Box<If<T>>, Span),
    Block(Block<T>),
}

/// `for x in xs key e { … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct For<T> {
    pub binding: Ident,
    pub iter: Expr,
    pub key: Option<Expr>,
    pub body: Block<T>,
}

/// `match x { pat => body, … }`; `B` is the arm body.
#[derive(Clone, Debug, PartialEq)]
pub struct Match<B> {
    pub scrutinee: Expr,
    pub arms: Vec<Arm<B>>,
    /// The braces holding the arms.
    pub arms_span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arm<B> {
    pub pattern: Pattern,
    pub body: B,
    pub span: Span,
}

/// An arm of a tree or statement `match`: a block or a single item.
#[derive(Clone, Debug, PartialEq)]
pub enum ArmBody<T> {
    Block(Block<T>),
    Single(Box<T>),
}

/// `on click { … }`, `on scroll(dy) { … }`, `on change a, b after T { … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct On {
    pub event: Event,
    pub body: Block<Stmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// `click`, `notifications.received(n)`, `drop(p: Pin, at: int)`.
    Named {
        path: Vec<Ident>,
        params: Option<Vec<Param>>,
    },
    /// `change a, b after T`.
    Change {
        targets: Vec<Expr>,
        debounce: Option<Expr>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    After,
    Every,
}

/// `after T while cond { … }`, `every T while cond { … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Timer {
    pub kind: TimerKind,
    pub duration: Expr,
    pub while_: Option<Expr>,
    pub body: Block<Stmt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoseKind {
    Enter,
    Exit,
}

/// `enter { … }`, `exit { … }`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pose {
    pub kind: PoseKind,
    pub body: Block<Item>,
}

/// `#needle { … }` inside an `svg`.
#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    /// Without the `#`.
    pub name: Ident,
    pub body: Block<Item>,
}

// ---------------------------------------------------------------------------
// Handler statements

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    Let(Let),
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    Expr(Expr),
    If(If<Stmt>),
    For(For<Stmt>),
    Match(Match<ArmBody<Stmt>>),
    Play(Expr),
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
}

impl AssignOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Set => "=",
            Self::Add => "+=",
            Self::Sub => "-=",
            Self::Mul => "*=",
            Self::Div => "/=",
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Number(Number),
    String(StringLit),
    Color(Color),
    Bool(bool),
    Null,
    Name(Ident),
    /// `$space.2`: a token path without method calls.
    Token(TokenKey),
    Array(Vec<Expr>),
    Paren(Box<Expr>),
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
    /// `a.b`, or `a?.b` when `optional`. A field named by digits (`x.0`)
    /// keeps its digits as the name.
    Field {
        base: Box<Expr>,
        name: Ident,
        optional: bool,
    },
    /// `f(args)`; a method call is a call whose callee is a `Field`.
    Call {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Lambda {
        params: Vec<Param>,
        body: LambdaBody,
    },
    Match(Box<Match<Expr>>),
    /// `8, 8, 0`: a comma shorthand (prop and token values only).
    Commas(Vec<Expr>),
    /// `0 2px 8px $shadow`: a space-separated group (prop and token values
    /// only).
    Spaced(Vec<Expr>),
    /// Text that could not be parsed as an expression.
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LambdaBody {
    Expr(Box<Expr>),
    Block(Block<Stmt>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
    Await,
}

impl UnaryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Not => "!",
            Self::Neg => "-",
            Self::Await => "await",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Coalesce,
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl BinaryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coalesce => "??",
            Self::Or => "||",
            Self::And => "&&",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Px,
    Percent,
    Deg,
    Ch,
    S,
    Ms,
}

impl Unit {
    pub fn from_suffix(s: &str) -> Option<Unit> {
        Some(match s {
            "px" => Self::Px,
            "%" => Self::Percent,
            "deg" => Self::Deg,
            "ch" => Self::Ch,
            "s" => Self::S,
            "ms" => Self::Ms,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Px => "px",
            Self::Percent => "%",
            Self::Deg => "deg",
            Self::Ch => "ch",
            Self::S => "s",
            Self::Ms => "ms",
        }
    }
}

/// `36`, `0.72`, `1.2s`, `40%`.
#[derive(Clone, Debug, PartialEq)]
pub struct Number {
    pub value: f64,
    /// Written with a fractional part.
    pub fraction: bool,
    /// `None` for a plain number, and for an unknown unit (already reported).
    pub unit: Option<Unit>,
}

/// A string literal with escapes resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct StringLit {
    pub value: String,
    pub span: Span,
}

/// `#rrggbb[aa]` or `#rgb[a]`, as straight-alpha sRGB bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub rgba: [u8; 4],
}

/// A call argument.
#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    pub kind: ArgKind,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArgKind {
    Positional,
    /// `months: -1`.
    Named(Ident),
    /// `from $surface` in `oklch(from $surface, l: l + 0.12)`.
    From(Span),
}

// ---------------------------------------------------------------------------
// Types, patterns, parameters

/// `name: Type = default`, for components, functions, lambdas and events.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: Ident,
    pub ty: Option<Type>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    /// `int`, `Notification`, `Async<[Hit]>`.
    Named {
        path: Vec<Ident>,
        args: Vec<Type>,
    },
    /// `[Pin]`.
    List(Box<Type>),
    /// `T?`.
    Optional(Box<Type>),
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatternKind {
    /// `_`.
    Wildcard,
    /// A variant or constant: `auto`, `Look.dark`.
    Path(Vec<Ident>),
    /// A literal expression: number (possibly negated), string, colour,
    /// boolean or null.
    Literal(Expr),
    Error,
}
