//! The dynamic value the VM computes with.
//!
//! Every `.strand` value at run time is a [`Value`]: numbers carry their
//! unit, records their [`RecordId`], enums their [`EnumId`]. Token-bound
//! values stay symbolic ([`Value::Token`] holds a [`TokenExpr`]), so
//! `$surface.alpha(0.72)` reaches the render thread unresolved and keeps
//! springing with the palette. Values are cheap to clone (shared parts are
//! reference-counted) and compare structurally, which is what the reactive
//! graph's equality cut-off needs.

use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use strand_core::{KeyedMemo, KeyedSignal, KeyedVec, Memo, Signal};
use strand_scene::{Color, TokenExpr};

use crate::hir::{DefId, LocalId, NodeIdx};
use crate::ty::{EnumId, RecordId, TypeTable};

/// The unit a number carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Num {
    /// A whole number (`int`).
    Int,
    /// A plain number.
    Float,
    /// Logical pixels (`4px`; plain numbers in length positions too).
    Px,
    /// `40%`, kept as written (40, not 0.4).
    Percent,
    /// `4ch`.
    Ch,
    /// Degrees.
    Deg,
    /// Milliseconds (durations).
    Ms,
}

/// A record value: a service item, a user `type`, a settings file, a
/// `Date`. Fields are in the declaration order of [`TypeTable`].
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub ty: RecordId,
    pub fields: Vec<Value>,
}

/// A palette: Material 3 system roles under Strand's names.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub roles: Vec<(String, Color)>,
}

impl Palette {
    pub fn get(&self, role: &str) -> Option<Color> {
        self.roles.iter().find(|(r, _)| r == role).map(|(_, c)| *c)
    }
}

/// A call-shaped builtin value: `linear(45deg, $accent, $tertiary)`,
/// `spring(700, 0.9)`, `grow(6)`, `blur(16)`. The emitter turns it into a
/// paint, a transition or a `PropValue::Call` by name.
#[derive(Clone, Debug, PartialEq)]
pub struct CallValue {
    pub name: String,
    /// Positional arguments, in parameter order (defaults filled in).
    pub args: Vec<Value>,
}

/// A future an `Async` value is waiting on (a `sleep`, a load started in a
/// handler). `await` polls it.
pub type PendingFuture = Pin<Box<dyn Future<Output = Result<Value, String>>>>;

/// The pending part of an [`AsyncValue`]: what `await` waits on. Shared
/// by every copy of the value, so several handlers can await it: the
/// first to see it finish keeps the result and wakes the others, which
/// get the same result.
pub struct PendingOp {
    pub fut: std::cell::RefCell<Option<PendingFuture>>,
    /// The result, once the future finished.
    pub result: std::cell::RefCell<Option<Result<Value, String>>>,
    /// Awaiters waiting while another one polls the future.
    pub waiters: std::cell::RefCell<Vec<std::task::Waker>>,
}

impl PendingOp {
    pub fn new(fut: PendingFuture) -> Self {
        Self {
            fut: std::cell::RefCell::new(Some(fut)),
            result: std::cell::RefCell::default(),
            waiters: std::cell::RefCell::default(),
        }
    }
}

impl fmt::Debug for PendingOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingOp")
    }
}

/// `Async<T>`: keeps its last value, and says whether a newer one is
/// pending or failed.
#[derive(Clone, Debug)]
pub struct AsyncValue {
    pub value: Option<Value>,
    pub pending: bool,
    pub error: Option<Rc<str>>,
    /// What `await` waits on, if anything.
    pub op: Option<Rc<PendingOp>>,
}

impl PartialEq for AsyncValue {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
            && self.pending == other.pending
            && self.error == other.error
            && match (&self.op, &other.op) {
                (None, None) => true,
                (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                _ => false,
            }
    }
}

impl AsyncValue {
    pub fn ready(v: Value) -> Self {
        Self {
            value: Some(v),
            pending: false,
            error: None,
            op: None,
        }
    }

    pub fn failed(msg: impl Into<Rc<str>>) -> Self {
        Self {
            value: None,
            pending: false,
            error: Some(msg.into()),
            op: None,
        }
    }

    /// `??` takes the value only when it is settled and good.
    pub fn usable(&self) -> Option<&Value> {
        if self.pending || self.error.is_some() {
            None
        } else {
            self.value.as_ref()
        }
    }
}

/// A lambda or `fn` value: a chunk with its captured locals and scope.
pub struct Closure {
    pub params: Vec<LocalId>,
    pub chunk: u32,
    /// Locals of the frame that made it (handler `let`s, outer lambda
    /// parameters), by value: they never change after binding.
    pub captured: Vec<(LocalId, Value)>,
    pub env: Rc<super::Env>,
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Closure(chunk {})", self.chunk)
    }
}

/// The live state of one element instance: `hover`, `pressed`, `focused`,
/// `selected` and its laid-out size, written by input from the render
/// thread and read by `when hover { … }` and `vol.hover`.
#[derive(Debug)]
pub struct NodeState {
    pub idx: NodeIdx,
    pub scene: std::cell::Cell<Option<strand_scene::NodeId>>,
    pub hover: Signal<bool>,
    pub pressed: Signal<bool>,
    pub focused: Signal<bool>,
    pub selected: Signal<bool>,
    pub width: Signal<Value>,
    pub height: Signal<Value>,
    /// Which laid-out sizes bindings read: [`NodeState::WATCH_SIZE`],
    /// [`NodeState::WATCH_QUERY`] (sticky bits).
    pub watch: std::cell::Cell<u8>,
    /// Render has reported its size at least once: until then `width`
    /// and `height` are boot values (0), which a container query's
    /// hysteresis must not latch onto.
    pub laid_out: std::cell::Cell<bool>,
}

impl NodeState {
    /// A binding read `width` or `height`.
    pub const WATCH_SIZE: u8 = 1;
    /// A container query (`when self.width < N`) read one.
    pub const WATCH_QUERY: u8 = 2;
}

/// A value of the language.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    /// What actions and handlers return.
    Unit,
    Bool(bool),
    Num(f64, Num),
    Text(Rc<str>),
    Color(Color),
    Enum(EnumId, u32),
    /// An enum as a value (`options: Look`).
    EnumType(EnumId),
    Record(Rc<Record>),
    List(Rc<Vec<Value>>),
    /// `8, 8, 0`: a comma shorthand.
    Commas(Rc<Vec<Value>>),
    /// `0 2px 8px $shadow`: a space-separated value (shadows, fonts).
    Spaced(Rc<Vec<Value>>),
    Async(Rc<AsyncValue>),
    Fn(Rc<Closure>),
    /// An element instance (`self`, `vol`, the node of a bare `hover`).
    Node(Rc<NodeState>),
    /// A token reference or expression, resolved by the render thread.
    Token(Rc<TokenExpr>),
    Call(Rc<CallValue>),
    /// `enter { … }` / `exit { … }` props.
    Pose(Rc<Vec<(String, Value)>>),
    Palette(Rc<Palette>),
    /// A `tokens` set (`use tokens compact`).
    TokenSet(DefId),
    /// A service used as a value (`audio`, the receiver of
    /// `notifications.clear()`).
    Service(Rc<str>),
}

impl PartialEq for Closure {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl PartialEq for NodeState {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.into())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s.into())
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Num(n, Num::Float)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Num(n as f64, Num::Int)
    }
}

impl From<Color> for Value {
    fn from(c: Color) -> Self {
        Value::Color(c)
    }
}

impl From<Duration> for Value {
    fn from(d: Duration) -> Self {
        Value::Num(d.as_secs_f64() * 1000.0, Num::Ms)
    }
}

impl Value {
    pub fn int(n: i64) -> Self {
        Value::Num(n as f64, Num::Int)
    }

    pub fn float(n: f64) -> Self {
        Value::Num(n, Num::Float)
    }

    pub fn text(s: impl Into<Rc<str>>) -> Self {
        Value::Text(s.into())
    }

    pub fn list(items: Vec<Value>) -> Self {
        Value::List(Rc::new(items))
    }

    pub fn record(ty: RecordId, fields: Vec<Value>) -> Self {
        Value::Record(Rc::new(Record { ty, fields }))
    }

    pub fn token(e: TokenExpr) -> Self {
        Value::Token(Rc::new(e))
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Truthiness for conditions: `bool`s; anything else is an error the
    /// checker already ruled out, read as false.
    pub fn truthy(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n, _) => Some(*n),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }

    /// A duration: `6s`, `200ms`; plain numbers are seconds. `None` for
    /// a negative, non-finite or too large value (past `Duration::MAX`,
    /// about 584 billion years): config input never panics.
    pub fn as_duration(&self) -> Option<Duration> {
        let ms = match self {
            Value::Num(n, Num::Ms) => *n,
            Value::Num(n, _) => *n * 1000.0,
            _ => return None,
        };
        duration_ms(ms)
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            Value::Async(a) => match &a.value {
                Some(Value::List(items)) => Some(items),
                _ => Some(&[]),
            },
            _ => None,
        }
    }

    pub fn as_record(&self) -> Option<&Record> {
        match self {
            Value::Record(r) => Some(r),
            _ => None,
        }
    }

    /// True if a token reference sits anywhere inside.
    pub fn has_tokens(&self) -> bool {
        match self {
            Value::Token(_) => true,
            Value::List(v) | Value::Commas(v) | Value::Spaced(v) => v.iter().any(Value::has_tokens),
            Value::Call(c) => c.args.iter().any(Value::has_tokens),
            Value::Pose(p) => p.iter().any(|(_, v)| v.has_tokens()),
            _ => false,
        }
    }

    /// A record's field by name.
    pub fn field<'a>(&'a self, types: &TypeTable, name: &str) -> Option<&'a Value> {
        let r = self.as_record()?;
        let def = types.record(r.ty);
        let i = def.fields.iter().position(|f| f.name == name)?;
        r.fields.get(i)
    }

    /// The key of a keyed record (`id`, `app.id`), following the path.
    pub fn key_path<'a>(&'a self, types: &TypeTable, path: &[String]) -> Option<&'a Value> {
        let mut v = self;
        for seg in path {
            v = v.field(types, seg)?;
        }
        Some(v)
    }

    /// The identity of an item: its record key, else itself.
    pub fn identity(&self, types: &TypeTable) -> Value {
        if let Value::Record(r) = self
            && let Some(path) = &types.record(r.ty).key
            && let Some(k) = self.key_path(types, path)
        {
            return k.clone();
        }
        self.clone()
    }

    /// Shown as text: what `join` and `text` positions do with a value.
    pub fn show(&self, types: &TypeTable) -> String {
        match self {
            Value::Null | Value::Unit => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n, unit) => {
                let s = number(*n);
                match unit {
                    Num::Px => format!("{s}px"),
                    Num::Percent => format!("{s}%"),
                    Num::Ch => format!("{s}ch"),
                    Num::Deg => format!("{s}deg"),
                    Num::Ms => format!("{s}ms"),
                    Num::Int | Num::Float => s,
                }
            }
            Value::Text(t) => t.to_string(),
            Value::Color(c) => hex(*c),
            Value::Enum(e, v) => types
                .enums
                .get(e.0 as usize)
                .and_then(|d| d.variants.get(*v as usize))
                .cloned()
                .unwrap_or_default(),
            Value::EnumType(e) => types
                .enums
                .get(e.0 as usize)
                .map(|d| d.name.clone())
                .unwrap_or_default(),
            Value::List(items) | Value::Commas(items) => items
                .iter()
                .map(|v| v.show(types))
                .collect::<Vec<_>>()
                .join(", "),
            Value::Spaced(items) => items
                .iter()
                .map(|v| v.show(types))
                .collect::<Vec<_>>()
                .join(" "),
            Value::Async(a) => a.value.as_ref().map(|v| v.show(types)).unwrap_or_default(),
            Value::Record(r) => {
                let def = types.record(r.ty);
                let fields: Vec<String> = def
                    .fields
                    .iter()
                    .zip(&r.fields)
                    .map(|(f, v)| format!("{}: {}", f.name, v.show(types)))
                    .collect();
                format!("{} {{ {} }}", def.name, fields.join(", "))
            }
            Value::Fn(_) => "fn".into(),
            Value::Node(_) => "node".into(),
            Value::Token(t) => format!("{t:?}"),
            Value::Call(c) => format!(
                "{}({})",
                c.name,
                c.args
                    .iter()
                    .map(|v| v.show(types))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Pose(_) => "pose".into(),
            Value::Palette(_) => "palette".into(),
            Value::TokenSet(_) => "tokens".into(),
            Value::Service(s) => s.to_string(),
        }
    }
}

/// Whether `v` is a value of type `ty` (writes from widgets and `strand
/// set` are checked with it, so a bool never lands in an `int` state).
/// Shapes the VM does not distinguish (fonts, shadows, opaque values,
/// functions) are accepted.
pub fn fits(types: &TypeTable, ty: &crate::ty::Ty, v: &Value) -> bool {
    use crate::ty::{Prim, Ty};
    match (ty, v) {
        (Ty::Error | Ty::Any, _) => true,
        (Ty::Optional(_), Value::Null) => true,
        (Ty::Optional(t), v) => fits(types, t, v),
        (Ty::Union(ts), v) => ts.iter().any(|t| fits(types, t, v)),
        (_, Value::Null) => matches!(ty, Ty::Null),
        (Ty::Unit, v) => matches!(v, Value::Unit),
        (Ty::Prim(p), v) => match (p, v) {
            (Prim::Bool, Value::Bool(_)) => true,
            (Prim::Bool, _) => false,
            (Prim::Int, Value::Num(n, u)) => {
                matches!(u, Num::Int | Num::Float) && n.fract() == 0.0 && n.is_finite()
            }
            (Prim::Float, Value::Num(_, u)) => matches!(u, Num::Int | Num::Float),
            (Prim::Length, Value::Num(_, u)) => {
                matches!(u, Num::Int | Num::Float | Num::Px | Num::Percent | Num::Ch)
            }
            (Prim::Percent, Value::Num(_, u)) => matches!(u, Num::Int | Num::Float | Num::Percent),
            (Prim::Angle, Value::Num(_, u)) => matches!(u, Num::Int | Num::Float | Num::Deg),
            (Prim::Duration, Value::Num(_, u)) => matches!(u, Num::Int | Num::Float | Num::Ms),
            (
                Prim::Int | Prim::Float | Prim::Length | Prim::Percent | Prim::Angle,
                Value::Token(_),
            ) => true,
            (
                Prim::Int
                | Prim::Float
                | Prim::Length
                | Prim::Percent
                | Prim::Angle
                | Prim::Duration,
                _,
            ) => false,
            (Prim::Text | Prim::Path, v) => matches!(v, Value::Text(_)),
            (Prim::Color, v) => matches!(v, Value::Color(_) | Value::Token(_)),
            (Prim::Paint, v) => matches!(v, Value::Color(_) | Value::Token(_) | Value::Call(_)),
            _ => true,
        },
        (Ty::Enum(e), v) => {
            matches!(v, Value::Enum(e2, i) if e2 == e && (*i as usize) < types.enum_(*e).variants.len())
        }
        (Ty::Record(r), v) => matches!(v, Value::Record(rec) if rec.ty == *r),
        (Ty::List(t, _), v) => match v {
            Value::List(items) => items.iter().all(|i| fits(types, t, i)),
            _ => false,
        },
        _ => true,
    }
}

/// An `f32` from a widget as the `f64` with the shortest decimal form
/// that round-trips it (a slider's 0.8 is 0.8, not 0.800000011920929).
pub fn f32_to_f64(n: f32) -> f64 {
    if !n.is_finite() {
        return n as f64;
    }
    n.to_string().parse().unwrap_or(n as f64)
}

/// `ms` milliseconds as a `Duration`: `None` when negative, not finite or
/// out of range.
pub fn duration_ms(ms: f64) -> Option<Duration> {
    if ms < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(ms / 1000.0).ok()
}

/// A number without a trailing `.0`.
pub fn number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn hex(c: Color) -> String {
    let [r, g, b, a] = c.to_rgba8();
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// A value used as a hash key (keyed lists, `on change` identities).
/// Floats compare by bits, so `NaN` keys are equal to themselves.
#[derive(Clone, Debug)]
pub struct ValueKey(pub Value);

impl PartialEq for ValueKey {
    fn eq(&self, other: &Self) -> bool {
        key_eq(&self.0, &other.0)
    }
}

impl Eq for ValueKey {}

impl Hash for ValueKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        key_hash(&self.0, state);
    }
}

fn key_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x, u), Value::Num(y, v)) => u == v && norm(*x) == norm(*y),
        (Value::List(x), Value::List(y))
        | (Value::Commas(x), Value::Commas(y))
        | (Value::Spaced(x), Value::Spaced(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| key_eq(a, b))
        }
        (Value::Record(x), Value::Record(y)) => {
            x.ty == y.ty
                && x.fields.len() == y.fields.len()
                && x.fields.iter().zip(&y.fields).all(|(a, b)| key_eq(a, b))
        }
        _ => a == b,
    }
}

fn norm(x: f64) -> u64 {
    if x == 0.0 { 0 } else { x.to_bits() }
}

fn key_hash<H: Hasher>(v: &Value, state: &mut H) {
    std::mem::discriminant(v).hash(state);
    match v {
        Value::Bool(b) => b.hash(state),
        Value::Num(n, u) => {
            norm(*n).hash(state);
            u.hash(state);
        }
        Value::Text(t) => t.hash(state),
        Value::Color(c) => c.to_rgba8().hash(state),
        Value::Enum(e, i) => {
            e.hash(state);
            i.hash(state);
        }
        Value::EnumType(e) => e.hash(state),
        Value::Record(r) => {
            r.ty.hash(state);
            for f in &r.fields {
                key_hash(f, state);
            }
        }
        Value::List(items) | Value::Commas(items) | Value::Spaced(items) => {
            items.len().hash(state);
            for i in items.iter() {
                key_hash(i, state);
            }
        }
        Value::Service(s) => s.hash(state),
        Value::TokenSet(d) => d.hash(state),
        _ => {}
    }
}

/// Where a name's value lives at run time.
#[derive(Clone, Copy, Debug)]
pub enum Slot {
    /// `state`, settings fields, item bindings written by the VM.
    Signal(Signal<Value>),
    /// `let`s, component parameters, settings files read as a record.
    Memo(Memo<Value>),
    /// A keyed `state xs: [T] key f`: a core keyed collection, so a
    /// `push` is one `VecDiff` and a `for` over it follows the diffs;
    /// with the list as one value for every other read (built once per
    /// change, however many bindings read it).
    Keyed(KeyedSignal<ValueKey, Value>, Memo<Value>),
    /// A view `let` (`let shown = notifications.popups.filter(…).take(5)`):
    /// core's incremental view, shared by every `for` over it and by
    /// `shown.len`; with the list as one value for every other read.
    View(KeyedMemo<ValueKey, Value>, Memo<Value>),
}

impl Slot {
    /// Read and track.
    pub fn get(self, rt: &strand_core::Runtime) -> Result<Value, strand_core::Error> {
        match self {
            Slot::Signal(s) => s.get(rt),
            Slot::Memo(m) => m.get(rt),
            Slot::Keyed(_, list) | Slot::View(_, list) => list.get(rt),
        }
    }

    pub fn id(self) -> strand_core::NodeId {
        match self {
            Slot::Signal(s) => s.id(),
            Slot::Memo(m) => m.id(),
            Slot::Keyed(k, _) => k.id(),
            Slot::View(v, _) => v.id(),
        }
    }
}

/// A keyed collection's items as a list value.
pub fn list_of(v: &KeyedVec<ValueKey, Value>) -> Value {
    Value::list(v.items().iter().map(|(_, x)| x.clone()).collect())
}

/// A keyed view's items as a list value.
pub fn list_of_items(v: &[(ValueKey, Value)]) -> Value {
    Value::list(v.iter().map(|(_, x)| x.clone()).collect())
}

/// An empty keyed collection keyed by `path` (a record key, `state … key
/// f`'s path), or by the item's identity. `holder` keeps the type table
/// alive (`types` gets it out).
pub fn keyed_vec<H: 'static>(
    holder: H,
    types: fn(&H) -> &TypeTable,
    path: Option<Vec<String>>,
) -> KeyedVec<ValueKey, Value> {
    KeyedVec::new(move |v: &Value| {
        let t = types(&holder);
        ValueKey(match &path {
            Some(p) => v.key_path(t, p).cloned().unwrap_or(Value::Null),
            None => v.identity(t),
        })
    })
}

/// `v`, a value of a program with type table `from`, as a value of a
/// program with table `to` (a live reload keeping state): enums by name
/// and variant name, records by name and field name. `None` when the new
/// program has no such type, variant or field, or for values that only
/// mean something in the program that made them (closures, element
/// handles, pending loads, token sets). Returns the value and whether
/// anything changed.
pub fn translate(v: &Value, from: &TypeTable, to: &TypeTable) -> Option<(Value, bool)> {
    Some(match v {
        Value::Null
        | Value::Unit
        | Value::Bool(_)
        | Value::Num(..)
        | Value::Text(_)
        | Value::Color(_)
        | Value::Token(_)
        | Value::Palette(_)
        | Value::Service(_) => (v.clone(), false),
        Value::Enum(e, i) => {
            let def = from.enums.get(e.0 as usize)?;
            let variant = def.variants.get(*i as usize)?;
            let ne = to.find_enum(&def.name)?;
            let ni = to.enum_(ne).variants.iter().position(|x| x == variant)? as u32;
            (Value::Enum(ne, ni), ne != *e || ni != *i)
        }
        Value::EnumType(e) => {
            let def = from.enums.get(e.0 as usize)?;
            let ne = to.find_enum(&def.name)?;
            (Value::EnumType(ne), ne != *e)
        }
        Value::Record(r) => {
            let def = from.records.get(r.ty.0 as usize)?;
            let nr = to.find_record(&def.name)?;
            let ndef = to.record(nr);
            let mut changed = nr != r.ty || ndef.fields.len() != def.fields.len();
            let mut fields = Vec::with_capacity(ndef.fields.len());
            for f in &ndef.fields {
                let i = def.fields.iter().position(|x| x.name == f.name);
                let (fv, c) = match i.and_then(|i| r.fields.get(i)) {
                    Some(old) => translate(old, from, to)?,
                    // A new field: its type's default.
                    None => (crate::vm::schema_host::default_value(to, &f.ty), true),
                };
                if i.is_some_and(|i| i != fields.len()) {
                    changed = true;
                }
                changed |= c;
                fields.push(fv);
            }
            (Value::Record(Rc::new(Record { ty: nr, fields })), changed)
        }
        Value::List(xs) | Value::Commas(xs) | Value::Spaced(xs) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(xs.len());
            for x in xs.iter() {
                let (nx, c) = translate(x, from, to)?;
                changed |= c;
                out.push(nx);
            }
            let out = Rc::new(out);
            let nv = match v {
                Value::List(_) => Value::List(out),
                Value::Commas(_) => Value::Commas(out),
                _ => Value::Spaced(out),
            };
            (nv, changed)
        }
        Value::Pose(ps) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(ps.len());
            for (n, x) in ps.iter() {
                let (nx, c) = translate(x, from, to)?;
                changed |= c;
                out.push((n.clone(), nx));
            }
            (Value::Pose(Rc::new(out)), changed)
        }
        Value::Call(_) | Value::Async(_) | Value::Fn(_) | Value::Node(_) | Value::TokenSet(_) => {
            return None;
        }
    })
}

#[cfg(test)]
// `ValueKey` hashes and compares the value, never the pending future an
// `Async` may hold.
#[allow(clippy::mutable_key_type)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_hash_by_value() {
        let mut set = HashSet::new();
        assert!(set.insert(ValueKey(Value::int(1))));
        assert!(!set.insert(ValueKey(Value::int(1))));
        assert!(set.insert(ValueKey(Value::float(1.0))));
        assert!(set.insert(ValueKey(Value::float(f64::NAN))));
        assert!(!set.insert(ValueKey(Value::float(f64::NAN))));
        assert!(set.insert(ValueKey(Value::text("a"))));
        assert!(
            !set.insert(ValueKey(Value::float(-0.0))) || set.contains(&ValueKey(Value::float(0.0)))
        );
    }

    #[test]
    fn numbers_show_without_trailing_zeros() {
        assert_eq!(number(42.0), "42");
        assert_eq!(number(0.5), "0.5");
        assert_eq!(number(1.0 / 3.0), "0.3333");
    }

    #[test]
    fn async_is_usable_only_when_settled() {
        let a = AsyncValue::ready(Value::int(3));
        assert_eq!(a.usable(), Some(&Value::int(3)));
        let mut p = a.clone();
        p.pending = true;
        assert_eq!(p.usable(), None);
        assert_eq!(AsyncValue::failed("x").usable(), None);
    }
}
