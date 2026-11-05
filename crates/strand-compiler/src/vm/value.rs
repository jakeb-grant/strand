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

/// (dictionary) A dictionary's entries, always in key order ([`key_cmp`]):
/// ints numerically, texts and paths by Unicode scalar, enums in
/// declaration order, `false` before `true`. There is no insertion or
/// recency history, so the derived equality, the language's `==` and the
/// graph's equality cut-off agree, and two dictionaries with the same
/// entries are equal however they were written (decisions.md
/// dictionary). Each key is held as [`dict_key`] makes it: a whole number
/// is an `int`, whatever arithmetic computed it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(Value, Value)>,
}

impl Dict {
    /// A dictionary of `entries` in any order: sorted, and of two entries
    /// with one key the later wins (a literal's computed keys, a store
    /// edited by hand).
    pub fn from_entries(entries: impl IntoIterator<Item = (Value, Value)>) -> Self {
        let mut d = Dict::default();
        for (k, v) in entries {
            d.insert(k, v);
        }
        d
    }

    /// The entries, in key order.
    pub fn entries(&self) -> &[(Value, Value)] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn find(&self, key: &Value) -> Result<usize, usize> {
        self.entries.binary_search_by(|(k, _)| key_cmp(k, key))
    }

    /// The value for `key`, if it has one.
    pub fn get(&self, key: &Value) -> Option<&Value> {
        self.find(key).ok().map(|i| &self.entries[i].1)
    }

    pub fn contains(&self, key: &Value) -> bool {
        self.find(key).is_ok()
    }

    /// Inserts `key` at its place in key order, or replaces its value.
    pub fn insert(&mut self, key: Value, value: Value) {
        let key = dict_key(key);
        match self.find(&key) {
            Ok(i) => self.entries[i].1 = value,
            Err(i) => self.entries.insert(i, (key, value)),
        }
    }

    /// Removes `key`'s entry; `None` (and no change) when there is none.
    pub fn remove(&mut self, key: &Value) -> Option<Value> {
        let i = self.find(key).ok()?;
        Some(self.entries.remove(i).1)
    }

    /// The entries `keep` is true for, in key order.
    pub fn retain(&mut self, keep: impl FnMut(&(Value, Value)) -> bool) {
        self.entries.retain(keep);
    }
}

/// (dictionary) A dictionary key as it is held: a whole plain number is an
/// `int` (`3.0` is the key `3`, as [`ValueKey`] has it), anything else as
/// it is.
pub fn dict_key(k: Value) -> Value {
    match k {
        Value::Num(n, Num::Float) if n.is_finite() && n.fract() == 0.0 => {
            Value::Num(n + 0.0, Num::Int)
        }
        Value::Num(n, Num::Int) => Value::Num(n + 0.0, Num::Int),
        k => k,
    }
}

/// (dictionary) The order of dictionary keys: numbers by value (`int` and
/// `float` one plain number, as [`ValueKey`] has it), texts by Unicode
/// scalar (UTF-8's byte order is the scalars' order), enums by declaration
/// order, `false` before `true`. Values of other kinds, which no key type
/// holds, order by kind.
pub fn key_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Num(..) => 2,
            Value::Text(_) => 3,
            Value::Enum(..) => 4,
            _ => 5,
        }
    }
    match (a, b) {
        (Value::Num(x, u), Value::Num(y, v)) => {
            let n = |f: f64| if f.is_nan() { f64::NAN.abs() } else { f + 0.0 };
            n(*x)
                .total_cmp(&n(*y))
                .then_with(|| key_unit(*u).rank().cmp(&key_unit(*v).rank()))
        }
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Enum(e, x), Value::Enum(f, y)) => e.0.cmp(&f.0).then(x.cmp(y)),
        _ if rank(a) != rank(b) => rank(a).cmp(&rank(b)),
        _ if key_eq(a, b) => Ordering::Equal,
        // Two values of a kind no key type holds: by their debug form, a
        // total order that never says two different ones are one key.
        _ => format!("{a:?}").cmp(&format!("{b:?}")),
    }
}

impl Num {
    /// A stable rank of the unit, for ordering keys of one number by unit.
    fn rank(self) -> u8 {
        match self {
            Num::Int => 0,
            Num::Float => 1,
            Num::Px => 2,
            Num::Percent => 3,
            Num::Ch => 4,
            Num::Deg => 5,
            Num::Ms => 6,
        }
    }
}

/// A palette: every Material 3 system role under Strand's names
/// (`strand-theme`).
pub use strand_theme::Palette;

/// A call-shaped builtin value: `linear(45deg, $accent, $tertiary)`,
/// `spring(3000, 0.75)`, `grow(6)`, `blur(16)`. The emitter turns it into a
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

    /// What `??` takes: the value whenever there is one, the kept one
    /// while a newer one loads included (strand-core's `Async::or`: a
    /// search box keeps its last results while typing, a theme keeps its
    /// old palette while a wallpaper is quantised). Only a value still
    /// loading the first time, or a failure with nothing kept, falls
    /// through to the right-hand side.
    pub fn usable(&self) -> Option<&Value> {
        self.value.as_ref()
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
    /// (M6) A surface's open state, `p.open`: a `trigger: hover`
    /// popup's is the source of its `open` (the router writes it through
    /// [`crate::instantiate::Instance::write`]); any other surface's
    /// follows its `open` (true without one); false on other elements.
    pub open: Signal<bool>,
    pub width: Signal<Value>,
    pub height: Signal<Value>,
    /// (M6) Its laid-out box, `id.box`: a `Rect` record, or `Null` until
    /// render first reports it (it reads as all zeros then). Made on
    /// first use ([`NodeState::rect`]): most elements are never read so.
    pub rect_cell: std::cell::OnceCell<Signal<Value>>,
    /// (flex) The pointer over it, `self.pointer`: a `Point` in its
    /// laid-out box, or `Null` while the pointer is not over it. Made on
    /// first use ([`NodeState::pointer`]).
    pub pointer_cell: std::cell::OnceCell<Signal<Value>>,
    /// (window) A `window`'s compositor state, `w.maximized` and the
    /// rest. Made on first use ([`NodeState::window`]): only a window's
    /// root is ever told it.
    pub window_cell: std::cell::OnceCell<WindowSignals>,
    /// (oi-lock) A `thumbnail` with nothing to show, `t.blank`: the
    /// binary says so as it feeds it. Made on first use
    /// ([`NodeState::blank`]): only thumbnails are ever told it.
    pub blank_cell: std::cell::OnceCell<Signal<bool>>,
    /// (keyboard) The global index of a `list`'s selected row, `Null` with
    /// none: the router says so as it moves. Made on first use
    /// ([`NodeState::selected_index`]): only lists are ever told it.
    pub selected_index_cell: std::cell::OnceCell<Signal<Value>>,
    /// (keyboard) A `scroll`'s or `list`'s position, `Null` until render
    /// says so, which it does only once one is read ([`NodeState::WATCH_SCROLL`]).
    pub scroll_x_cell: std::cell::OnceCell<Signal<Value>>,
    pub scroll_y_cell: std::cell::OnceCell<Signal<Value>>,
    /// (M6) For a conditional id (`when c { id: name }`), the element it
    /// names now: `Value::Node`, or `Null` while no condition holds.
    /// Made on first use ([`NodeState::alias`]).
    pub alias_cell: std::cell::OnceCell<Signal<Value>>,
    /// (M6) The conditional ids this element may take (`when c { id:
    /// name }`), as their alias states: they let go of it when it
    /// unmounts.
    pub claims: std::cell::RefCell<Vec<std::rc::Rc<NodeState>>>,
    /// (M6) For a conditional id: the elements whose condition holds, in
    /// the order each started holding; the name is the last
    /// ([`NodeState::hold`]).
    pub holders: std::cell::RefCell<Vec<std::rc::Weak<NodeState>>>,
    /// (eval-finish) The component calls nested in the one at `idx` that
    /// this state was handed down through as each one's root (`Pill live
    /// { id: pill }` as `PlayButton`'s root, called `PlayButton playing
    /// { id: play_btn }`), outermost first: an `id:` on any of them names
    /// the element at the end of the chain too (decisions.md
    /// m6-w2-language, eval-finish).
    pub calls: std::cell::RefCell<Vec<NodeIdx>>,
    /// The core scope its signals belong to (that of the scope owning
    /// the element), for those made on first use.
    pub scope: Option<strand_core::NodeId>,
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
    /// (M6) A binding read `box`: render reports where it is, not only
    /// its size.
    pub const WATCH_BOX: u8 = 4;
    /// (flex) A binding or handler reads `pointer`: render reports the
    /// pointer over it, once a frame while it moves.
    pub const WATCH_POINTER: u8 = 8;
    /// (keyboard) A binding or handler reads `scroll_x` or `scroll_y`:
    /// render reports the position, once a frame while it moves.
    pub const WATCH_SCROLL: u8 = 16;

    /// (M6) `id.box`'s signal, made on first use in the element's scope.
    pub fn rect(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.rect_cell.get_or_init(|| self.make(rt))
    }

    /// (flex) `self.pointer`'s signal, made on first use.
    pub fn pointer(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.pointer_cell.get_or_init(|| self.make(rt))
    }

    /// (window) A window's compositor state signals, made on first use.
    pub fn window(&self, rt: &strand_core::Runtime) -> WindowSignals {
        *self.window_cell.get_or_init(|| {
            let make = |rt: &strand_core::Runtime| WindowSignals {
                maximized: rt.signal(false),
                fullscreen: rt.signal(false),
                tiled: rt.signal(false),
                decorated: rt.signal(false),
            };
            match self.scope {
                Some(o) => rt.with_owner(o, make).unwrap_or_else(|_| make(rt)),
                None => make(rt),
            }
        })
    }

    /// (oi-lock) `t.blank`'s signal, made on first use.
    pub fn blank(&self, rt: &strand_core::Runtime) -> Signal<bool> {
        *self.blank_cell.get_or_init(|| {
            let make = |rt: &strand_core::Runtime| rt.signal(false);
            match self.scope {
                Some(o) => rt.with_owner(o, make).unwrap_or_else(|_| make(rt)),
                None => make(rt),
            }
        })
    }

    /// (keyboard) `results.selected_index`'s signal, made on first use.
    pub fn selected_index(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.selected_index_cell.get_or_init(|| self.make(rt))
    }

    /// (keyboard) `s.scroll_x`'s signal, made on first use.
    pub fn scroll_x(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.scroll_x_cell.get_or_init(|| self.make(rt))
    }

    /// (keyboard) `s.scroll_y`'s signal, made on first use.
    pub fn scroll_y(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.scroll_y_cell.get_or_init(|| self.make(rt))
    }

    /// (M6) A conditional id's target signal (see `alias_cell`).
    pub fn alias(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        *self.alias_cell.get_or_init(|| self.make(rt))
    }

    /// (M6) For a conditional id: `member`'s condition started (`on`) or
    /// stopped holding. The name follows the element whose condition
    /// started holding last of those still holding, so which one it is
    /// never depends on the order effects run in, and when that one
    /// stops, the name goes back to the one before (`Null` once none
    /// holds). Nothing changes when `member` already is where `on` puts
    /// it.
    pub fn hold(
        &self,
        rt: &strand_core::Runtime,
        member: &std::rc::Rc<NodeState>,
        on: bool,
    ) -> Result<(), strand_core::Error> {
        let changed = {
            let mut h = self.holders.borrow_mut();
            h.retain(|w| w.strong_count() > 0);
            let at = h
                .iter()
                .position(|w| std::ptr::eq(w.as_ptr(), std::rc::Rc::as_ptr(member)));
            match (on, at) {
                (true, None) => {
                    h.push(std::rc::Rc::downgrade(member));
                    true
                }
                (false, Some(i)) => {
                    h.remove(i);
                    true
                }
                _ => false,
            }
        };
        if !changed {
            return Ok(());
        }
        let now = self
            .holders
            .borrow()
            .last()
            .and_then(std::rc::Weak::upgrade);
        let sig = self.alias(rt);
        let same = match (&now, sig.get_untracked(rt)?) {
            (Some(n), Value::Node(m)) => std::rc::Rc::ptr_eq(n, &m),
            (None, Value::Null) => true,
            _ => false,
        };
        if !same {
            sig.set(rt, now.map_or(Value::Null, Value::Node))?;
        }
        Ok(())
    }

    fn make(&self, rt: &strand_core::Runtime) -> Signal<Value> {
        let make = |rt: &strand_core::Runtime| rt.signal(Value::Null);
        match self.scope {
            Some(o) => rt.with_owner(o, make).unwrap_or_else(|_| make(rt)),
            None => make(rt),
        }
    }
}

/// (window) What the compositor says of a `window` (its `xdg_toplevel`
/// states and decoration mode), as `w.maximized`, `w.fullscreen`,
/// `w.tiled` and `w.decorated` read it; `w.focused` is the node's own.
#[derive(Clone, Copy, Debug)]
pub struct WindowSignals {
    pub maximized: Signal<bool>,
    pub fullscreen: Signal<bool>,
    pub tiled: Signal<bool>,
    pub decorated: Signal<bool>,
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
    /// (dictionary) A dictionary `[K: V]`: its entries in key order.
    Dict(Rc<Dict>),
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
    /// (M4) A time-bound value (`t`, `wave(…)`, `noise(t)`, a `letters`
    /// letter's `index` and `count`, and arithmetic on them, tokens
    /// included): a [`TokenExpr`] over time leaves, evaluated by the
    /// render thread per node per frame (per letter). It only flows into props: reaching logic (a
    /// comparison, a condition, `match`, a store, a call that computes,
    /// text) is an error value ([`time_in_logic`]).
    Time(Rc<TokenExpr>),
    Call(Rc<CallValue>),
    /// `enter { … }` / `exit { … }` props.
    Pose(Rc<Vec<(String, Value)>>),
    Palette(Rc<Palette>),
    /// A `tokens` set (`use tokens compact`).
    TokenSet(DefId),
    /// (M4) A `keyframes` block named as a value (`play shake`).
    Keyframes(DefId),
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

    /// A symbolic value: [`Value::Time`] when `e` reads time, else
    /// [`Value::Token`].
    pub fn symbolic(e: TokenExpr) -> Self {
        Value::symbolic_from(e, false)
    }

    /// A symbolic value built from operands of which one is time-bound
    /// (`time`): it stays [`Value::Time`] even if `e` reads no `t`, so a
    /// `letters` letter's `index * 0.1` stays render's to evaluate.
    pub fn symbolic_from(e: TokenExpr, time: bool) -> Self {
        if time || e.reads_time() {
            Value::Time(Rc::new(e))
        } else {
            Value::Token(Rc::new(e))
        }
    }

    /// (M4) True if a time-bound value sits anywhere inside.
    pub fn has_time(&self) -> bool {
        match self {
            Value::Time(_) => true,
            Value::List(v) | Value::Commas(v) | Value::Spaced(v) => v.iter().any(Value::has_time),
            Value::Call(c) => c.args.iter().any(Value::has_time),
            Value::Pose(p) => p.iter().any(|(_, v)| v.has_time()),
            Value::Record(r) => r.fields.iter().any(Value::has_time),
            Value::Dict(d) => d.entries().iter().any(|(_, v)| v.has_time()),
            _ => false,
        }
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
            Value::Token(_) | Value::Time(_) => true,
            Value::List(v) | Value::Commas(v) | Value::Spaced(v) => v.iter().any(Value::has_tokens),
            Value::Call(c) => c.args.iter().any(Value::has_tokens),
            Value::Pose(p) => p.iter().any(|(_, v)| v.has_tokens()),
            Value::Dict(d) => d.entries().iter().any(|(_, v)| v.has_tokens()),
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
            Value::Dict(d) => d
                .entries()
                .iter()
                .map(|(k, v)| format!("{}: {}", k.show(types), v.show(types)))
                .collect::<Vec<_>>()
                .join(", "),
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
            Value::Token(t) | Value::Time(t) => format!("{t:?}"),
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
            Value::Keyframes(_) => "keyframes".into(),
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
                Value::Token(_) | Value::Time(_),
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
            (Prim::Color, v) => matches!(v, Value::Color(_) | Value::Token(_) | Value::Time(_)),
            (Prim::Paint, v) => matches!(
                v,
                Value::Color(_) | Value::Token(_) | Value::Time(_) | Value::Call(_)
            ),
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
        (Ty::Dict(k, t), v) => match v {
            Value::Dict(d) => d
                .entries()
                .iter()
                .all(|(key, x)| fits(types, k, key) && fits(types, t, x)),
            _ => false,
        },
        _ => true,
    }
}

/// (M4) The error a time-bound value gets where logic would need its
/// value now: it changes every frame on the render thread, so logic can
/// only pass it on to a prop (design.md, "Motion and time").
pub fn time_in_logic(what: &str) -> strand_core::Error {
    strand_core::Error::failed(format!(
        "a time-bound value (`t`, `wave(…)`, `noise(t)`) cannot be used {what}: it changes \
         every frame on the render thread, so only props can read it"
    ))
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
/// Floats compare by bits, so `NaN` keys are equal to themselves. A
/// number keeps its unit (`2px` and `2ch` are two keys), but `int` and
/// `float` are one plain number, not units: `3` and `3.0` are one key,
/// so a key a handler computes finds its item whatever tag the
/// arithmetic left it (decisions.md dictionary).
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
        (Value::Num(x, u), Value::Num(y, v)) => {
            key_unit(*u) == key_unit(*v) && norm(*x) == norm(*y)
        }
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
        (Value::Dict(x), Value::Dict(y)) => {
            x.len() == y.len()
                && x.entries()
                    .iter()
                    .zip(y.entries())
                    .all(|((a, v), (b, w))| key_eq(a, b) && key_eq(v, w))
        }
        _ => a == b,
    }
}

/// The unit a number keys by: `float` keys as `int`, both plain numbers.
fn key_unit(u: Num) -> Num {
    match u {
        Num::Float => Num::Int,
        u => u,
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
            key_unit(*u).hash(state);
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
        Value::Dict(d) => {
            d.len().hash(state);
            for (k, x) in d.entries() {
                key_hash(k, state);
                key_hash(x, state);
            }
        }
        Value::Service(s) => s.hash(state),
        Value::TokenSet(d) | Value::Keyframes(d) => d.hash(state),
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
/// handles, pending loads, token sets). A dictionary entry whose key no
/// longer translates is dropped, not the whole value
/// ([`translate_dropping`] counts them). Returns the value and whether
/// anything changed.
pub fn translate(v: &Value, from: &TypeTable, to: &TypeTable) -> Option<(Value, bool)> {
    translate_in(v, from, to, &mut Dropped::default())
}

/// (dictionary) [`translate`], with the dictionary entries it dropped
/// because their key no longer names anything in the new program (an
/// enum key whose variant is gone): a reload's overlay notes them once
/// per cell ([`Dropped::notice`]).
pub fn translate_dropping(
    v: &Value,
    from: &TypeTable,
    to: &TypeTable,
) -> Option<(Value, bool, Dropped)> {
    let mut dropped = Dropped::default();
    let (nv, changed) = translate_in(v, from, to, &mut dropped)?;
    Some((nv, changed, dropped))
}

/// (dictionary) Dictionary entries a value lost because their key no
/// longer names anything: an enum key whose variant a reload removed, or
/// one a stored value names that the program no longer declares
/// (`persist::decode_bytes_dropping`). Only a key drops its entry; a
/// value that no longer fits fails the whole value, as a list item does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// How many entries were dropped.
    pub entries: usize,
    /// The key type they no longer are (the first one met), by name.
    pub key: String,
}

impl Dropped {
    /// One entry dropped, its key of type `key`.
    pub fn add(&mut self, key: &str) {
        self.entries += 1;
        if self.key.is_empty() {
            self.key = key.to_string();
        }
    }

    /// Those `other` counted too.
    pub fn merge(&mut self, other: &Dropped) {
        if self.key.is_empty() {
            self.key.clone_from(&other.key);
        }
        self.entries += other.entries;
    }

    /// The overlay's line for cell `path`, when any entry was dropped:
    /// `island.used: dropped 2 entries (key no longer in Urgency)`.
    pub fn notice(&self, path: &str) -> Option<String> {
        if self.entries == 0 {
            return None;
        }
        Some(format!(
            "{path}: dropped {} entr{} (key no longer in {})",
            self.entries,
            if self.entries == 1 { "y" } else { "ies" },
            self.key
        ))
    }
}

fn translate_in(
    v: &Value,
    from: &TypeTable,
    to: &TypeTable,
    dropped: &mut Dropped,
) -> Option<(Value, bool)> {
    Some(match v {
        Value::Null
        | Value::Unit
        | Value::Bool(_)
        | Value::Num(..)
        | Value::Text(_)
        | Value::Color(_)
        | Value::Token(_)
        | Value::Time(_)
        | Value::Palette(_)
        | Value::Service(_) => (v.clone(), false),
        Value::Enum(e, i) => {
            let def = from.enums.get(e.0 as usize)?;
            let variant = def.variants.get(*i as usize)?;
            let ne = to.same_enum(def)?;
            let ni = to.enum_(ne).variants.iter().position(|x| x == variant)? as u32;
            (Value::Enum(ne, ni), ne != *e || ni != *i)
        }
        Value::EnumType(e) => {
            let def = from.enums.get(e.0 as usize)?;
            let ne = to.same_enum(def)?;
            (Value::EnumType(ne), ne != *e)
        }
        Value::Record(r) => {
            let def = from.records.get(r.ty.0 as usize)?;
            let nr = to.same_record(def)?;

            let ndef = to.record(nr);
            let mut changed = nr != r.ty || ndef.fields.len() != def.fields.len();
            let mut fields = Vec::with_capacity(ndef.fields.len());
            for f in &ndef.fields {
                let i = def.fields.iter().position(|x| x.name == f.name);
                let (fv, c) = match i.and_then(|i| r.fields.get(i)) {
                    Some(old) => translate_in(old, from, to, dropped)?,
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
                let (nx, c) = translate_in(x, from, to, dropped)?;
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
        // (dictionary) Keys and values each translated, then put back in
        // key order (an enum key's variants may have moved). A key that
        // no longer translates (its variant is gone) drops its entry,
        // counted in `dropped`; a value that no longer translates drops
        // the whole value, as a list item does.
        Value::Dict(d) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(d.len());
            for (k, x) in d.entries() {
                let Some((nk, ck)) = translate_in(k, from, to, dropped) else {
                    dropped.add(&key_type_name(from, k));
                    changed = true;
                    continue;
                };
                let (nx, cx) = translate_in(x, from, to, dropped)?;
                changed |= ck || cx;
                out.push((nk, nx));
            }
            (Value::Dict(Rc::new(Dict::from_entries(out))), changed)
        }
        Value::Pose(ps) => {
            let mut changed = false;
            let mut out = Vec::with_capacity(ps.len());
            for (n, x) in ps.iter() {
                let (nx, c) = translate_in(x, from, to, dropped)?;
                changed |= c;
                out.push((n.clone(), nx));
            }
            (Value::Pose(Rc::new(out)), changed)
        }
        Value::Call(_)
        | Value::Async(_)
        | Value::Fn(_)
        | Value::Node(_)
        | Value::TokenSet(_)
        | Value::Keyframes(_) => {
            return None;
        }
    })
}

/// (dictionary) The name of a dropped key's type in table `types`, for
/// [`Dropped::notice`]: an enum's name (only an enum key can stop naming
/// anything).
pub(crate) fn key_type_name(types: &TypeTable, k: &Value) -> String {
    match k {
        Value::Enum(e, _) => types
            .enums
            .get(e.0 as usize)
            .map_or_else(|| "enum".to_string(), |d| d.name.clone()),
        _ => "its type".to_string(),
    }
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
        // `int` and `float` are one plain number: `1.0` is the key `1`,
        // `1.5` another; a unit is a key of its own.
        assert!(!set.insert(ValueKey(Value::float(1.0))));
        assert!(set.contains(&ValueKey(Value::float(1.0))));
        assert!(set.insert(ValueKey(Value::float(1.5))));
        assert!(set.insert(ValueKey(Value::Num(1.0, Num::Px))));
        assert!(set.insert(ValueKey(Value::float(f64::NAN))));
        assert!(!set.insert(ValueKey(Value::float(f64::NAN))));
        assert!(set.insert(ValueKey(Value::text("a"))));
        assert!(
            !set.insert(ValueKey(Value::float(-0.0))) || set.contains(&ValueKey(Value::float(0.0)))
        );
    }

    /// (dictionary) Entries stay in key order whatever order they are
    /// written in; a whole number is one `int` key whatever its tag, so
    /// two dictionaries with the same entries are equal (the derived
    /// equality, which the graph's cut-off uses).
    #[test]
    fn dictionaries_keep_key_order_and_one_key_per_number() {
        let mut d = Dict::from_entries([
            (Value::int(3), Value::text("c")),
            (Value::int(1), Value::text("a")),
            (Value::float(2.0), Value::text("b")),
        ]);
        let keys: Vec<Value> = d.entries().iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(keys, [Value::int(1), Value::int(2), Value::int(3)]);
        d.insert(Value::int(2), Value::text("B"));
        assert_eq!(d.len(), 3);
        assert_eq!(d.get(&Value::float(2.0)), Some(&Value::text("B")));
        assert_eq!(d.get(&Value::float(2.5)), None);
        let e = Dict::from_entries([
            (Value::float(2.0), Value::text("B")),
            (Value::float(3.0), Value::text("c")),
            (Value::int(1), Value::text("a")),
        ]);
        assert_eq!(d, e);
        assert_eq!(d.remove(&Value::int(7)), None);
        assert_eq!(d.remove(&Value::float(1.0)), Some(Value::text("a")));
        assert_eq!(d.len(), 2);
        let t = Dict::from_entries([
            (Value::text("b"), Value::Null),
            (Value::text("\u{e9}"), Value::Null),
            (Value::text("B"), Value::Null),
            (Value::text("a"), Value::Null),
        ]);
        let shown: Vec<&str> = t
            .entries()
            .iter()
            .filter_map(|(k, _)| k.as_text())
            .collect();
        assert_eq!(shown, ["B", "a", "b", "\u{e9}"]);
        let flags = Dict::from_entries([
            (Value::Bool(true), Value::Null),
            (Value::Bool(false), Value::Null),
        ]);
        assert_eq!(flags.entries()[0].0, Value::Bool(false));
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
        assert_eq!(p.usable(), Some(&Value::int(3)), "kept while loading");
        p.value = None;
        assert_eq!(p.usable(), None, "loading the first time");
        assert_eq!(AsyncValue::failed("x").usable(), None);
    }
}
