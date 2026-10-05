//! Bytecode: what bindings, `let`s, `fn`s and handlers compile to.
//!
//! A [`Chunk`] is a flat list of [`Op`]s for a stack machine, with its
//! constants and side tables. Control flow (`&&`, `??`, `?.`, ternaries,
//! `match`, `if` and `for` statements) is jumps, so the VM runs a chunk
//! in one loop and a handler can suspend at any `await` and resume where
//! it stopped. Chunks hold only plain data, so a compiled program can be
//! built on the compiler worker and handed to the logic thread.

use crate::hir::{AssignOp, BinaryOp, DefId, LocalId, NodeIdx, UnaryOp};
use crate::source::FileId;
use crate::syntax::Span;
use crate::ty::{EnumId, RecordId};
use crate::vm::value::Num;

/// Index of a chunk in [`super::Program::chunks`].
pub type ChunkId = u32;

/// A constant of a chunk.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Num(f64, Num),
    Text(String),
    Color([u8; 4]),
    /// A channel of the `oklch(from …)` base colour: `l`, `c`, `h` or
    /// `alpha`.
    Channel(String),
}

/// How the arguments pushed for a call map onto the callee's parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgMap {
    /// For each pushed argument, in push order, the parameter it fills.
    pub params: Vec<u16>,
    /// The callee's parameter count (variadic parameters count once).
    pub arity: u16,
    /// The variadic parameter, if any: every argument mapped to it is
    /// collected into a list.
    pub variadic: Option<u16>,
}

/// One step of a path into a writable place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceSeg {
    Field(String),
    /// The index is on the stack (statements) or computed by the place's
    /// index chunks (two-way bindings).
    Index,
}

/// Where a writable place starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceRoot {
    /// A `state` or settings file.
    Def(DefId),
    /// A service (`audio` in `audio.sink.volume`).
    Service(String),
}

/// A writable place: `level`, `prefs.compact`, `audio.sink.volume`,
/// `pins[0].label`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub root: PlaceRoot,
    pub segs: Vec<PlaceSeg>,
}

/// A `match` arm's pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Wildcard,
    Variant(EnumId, u32),
    /// A literal, as a constant index.
    Literal(u32),
    Null,
    Bool(bool),
    /// Never matches (already reported).
    Error,
}

/// A keyed collection read in place ([`Op::Keyed`]): a keyed `state`
/// or a service's keyed field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyedRoot {
    Def(DefId),
    /// Service and field, as name indices.
    Service {
        service: u32,
        field: u32,
    },
}

/// What [`Op::Keyed`] asks of a keyed collection, through core's
/// accessors instead of a copy of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyedQuery {
    Len,
    First,
    Last,
    /// `xs[i]`: the index is on the stack.
    Index,
    /// `xs.contains(x)`: the value is on the stack (found by its key).
    Contains,
}

/// A lambda made by [`Op::Closure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lambda {
    pub params: Vec<LocalId>,
    pub chunk: ChunkId,
    /// The locals its body (and the lambdas in it) reads: all a closure
    /// captures from the frame that makes it.
    pub free: Vec<LocalId>,
}

/// One instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Push a constant.
    Const(u32),
    Null,
    Unit,
    Bool(bool),
    /// Push a local: frame first, then the closure's captures, then the
    /// scope chain.
    Local(LocalId),
    /// Push a `state`, `let`, settings file, `fn`, token set or service
    /// declaration.
    Def(DefId),
    /// Push a schema service (`battery`) by name.
    Service(u32),
    /// Push a builtin value (`t`) by name.
    Value(u32),
    /// Push an element instance (`self`, `vol`, a bare `hover`'s node).
    Node(NodeIdx),
    /// Push `$path` (a name index).
    Token(u32),
    Variant(EnumId, u32),
    EnumType(EnumId),
    /// Pop a value, push its field (a name index).
    Field(u32),
    /// Pop index and base, push `base[index]`.
    Index,
    Unary(UnaryOp),
    /// Arithmetic and comparison; `&&`, `||` and `??` are jumps.
    Binary(BinaryOp),
    Jump(u32),
    /// Pop a condition; jump if false.
    JumpIfFalse(u32),
    /// `&&`: if the top is false jump (keeping it), else pop it.
    AndJump(u32),
    /// `||`: if the top is true jump (keeping it), else pop it.
    OrJump(u32),
    /// `??`: if the top is a usable value (not null, not a pending or
    /// failed `Async`) replace it by its value and jump; else pop it.
    CoalesceJump(u32),
    /// `?.`: if the top is null jump (keeping it).
    NullJump(u32),
    Pop,
    Dup,
    /// Call a builtin function (name index, overload, argument map).
    CallBuiltin {
        name: u32,
        overload: u16,
        args: u32,
    },
    /// Call a method; the receiver is below the arguments. `action` says
    /// a service or item action (`ws.focus()`), which runs through the
    /// service host.
    CallMethod {
        name: u32,
        args: u32,
        action: bool,
    },
    /// Call a config `fn`.
    CallFn {
        def: DefId,
        args: u32,
    },
    /// Call a function value; the callee is below the arguments.
    CallValue {
        args: u32,
    },
    /// Build a record (`Pin(app: a, label: "x")`).
    MakeRecord {
        ty: RecordId,
        args: u32,
    },
    /// A list mutation on a writable place (`pins.push(p)`): place index,
    /// method name, argument map. Index values of the place are below the
    /// arguments.
    Mutate {
        place: u32,
        method: u32,
        args: u32,
    },
    /// Pop n values, push a list / comma value / space-separated value.
    List(u32),
    Commas(u32),
    Spaced(u32),
    /// Make a closure (lambda index).
    Closure(u32),
    /// Pop the scrutinee, push whether it matches (pattern index).
    Match(u32),
    /// Pop a value into a frame local.
    SetLocal(LocalId),
    /// Open a block scope: remember the frame's length.
    ScopeEnter,
    /// Close the innermost block scope: drop the locals bound in it.
    ScopeExit,
    /// Pop the value (and the place's index values below it) and write
    /// it (place index).
    Store {
        place: u32,
        op: AssignOp,
    },
    /// `for x in xs` in a handler: the list and an index are on the
    /// stack; drop the previous iteration's locals (back to the loop's
    /// [`Op::ScopeEnter`]), then bind the next item or pop both and jump
    /// to the end (the loop's [`Op::ScopeExit`]).
    IterNext {
        binding: LocalId,
        end: u32,
    },
    /// Pop an `Async` and suspend until it settles; push its value.
    Await,
    /// `xs.len`, `xs.first`, `xs.last`, `xs[i]`, `xs.contains(x)` on a
    /// keyed collection: answered by core's accessors (`with`,
    /// `get_key`), never by copying the list; a plain list is read as
    /// usual.
    Keyed {
        root: KeyedRoot,
        query: KeyedQuery,
    },
    /// `play shake`: pop the keyframes value and play it on the node.
    Play,
    /// A runtime error with a message (a name index): a `match` with no
    /// arm for the value.
    Fail(u32),
}

/// A compiled expression, statement list or handler body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chunk {
    pub ops: Vec<Op>,
    pub consts: Vec<Const>,
    /// Field, method, service, value, token and builtin names.
    pub names: Vec<String>,
    pub args: Vec<ArgMap>,
    pub places: Vec<Place>,
    pub patterns: Vec<Pattern>,
    pub lambdas: Vec<Lambda>,
    /// The source span of each op, for runtime errors.
    pub spans: Vec<Span>,
    pub file: FileId,
    /// Holds an `await`: runs as a coroutine.
    pub awaits: bool,
    /// The services whose state the action calls in this chunk can change
    /// (`n.dismiss()` on a `Notification`: `notifications`), for the
    /// handler's declared write edges.
    pub actions: Vec<String>,
}

impl Chunk {
    pub fn new(file: FileId) -> Self {
        Self {
            file,
            ..Self::default()
        }
    }

    pub(crate) fn emit(&mut self, op: Op, span: Span) -> u32 {
        if matches!(op, Op::Await) {
            self.awaits = true;
        }
        self.ops.push(op);
        self.spans.push(span);
        self.ops.len() as u32 - 1
    }

    pub(crate) fn name(&mut self, n: &str) -> u32 {
        match self.names.iter().position(|x| x == n) {
            Some(i) => i as u32,
            None => {
                self.names.push(n.to_string());
                self.names.len() as u32 - 1
            }
        }
    }

    pub(crate) fn constant(&mut self, c: Const) -> u32 {
        match self.consts.iter().position(|x| *x == c) {
            Some(i) => i as u32,
            None => {
                self.consts.push(c);
                self.consts.len() as u32 - 1
            }
        }
    }

    /// Point the jump at `at` to the current end.
    pub(crate) fn patch(&mut self, at: u32) {
        let to = self.ops.len() as u32;
        match &mut self.ops[at as usize] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::AndJump(t)
            | Op::OrJump(t)
            | Op::CoalesceJump(t)
            | Op::NullJump(t) => *t = to,
            Op::IterNext { end, .. } => *end = to,
            _ => {}
        }
    }

    pub(crate) fn here(&self) -> u32 {
        self.ops.len() as u32
    }

    /// True if the chunk is one constant-free op sequence reading nothing
    /// reactive: literals, variants, tokens and pure calls on them.
    pub fn is_static(&self) -> bool {
        self.ops.iter().all(|op| {
            !matches!(
                op,
                Op::Local(_)
                    | Op::Def(_)
                    | Op::Service(_)
                    | Op::Value(_)
                    | Op::Node(_)
                    | Op::CallMethod { action: true, .. }
                    | Op::CallFn { .. }
                    | Op::CallValue { .. }
                    | Op::Await
                    | Op::Keyed { .. }
            )
        })
    }
}
