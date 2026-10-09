//! Design tokens as the render thread sees them: palette roots (values that
//! spring) and derived tokens (expressions re-evaluated every frame), plus
//! token references inside prop values.
//!
//! The logic thread resolves the token graph's *structure* (which theme,
//! which overrides) and sends a [`TokenTable`]; the render thread evaluates
//! [`TokenExpr`]s against it at flatten time, so a palette spring keeps
//! every derived token exact mid-animation without logic re-sending props
//! (design: "Each frame the render thread re-evaluates the small token
//! graph").

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use crate::color::{Color, MIN_CONTRAST, Oklch};
use crate::protocol::{Length, Paint, Prop, PropClass, PropValue, Transition};

/// Deepest chain of token references followed before giving up (a cycle
/// is a load error on the logic side; this only bounds a bad table).
pub const MAX_TOKEN_DEPTH: u32 = 32;

/// A colour method: a `$` path segment followed by `(` (`$fg.alpha(0.65)`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TokenMethod {
    /// `.alpha(a)`: the same colour with alpha `a`.
    Alpha,
    /// `.mix(other, t)`: interpolates toward `other` by `t` (a fraction or
    /// a percentage) in premultiplied OKLab.
    Mix,
    /// `.lighten(d)`: raises OKLCH lightness by `d`.
    Lighten,
    /// `.darken(d)`: lowers OKLCH lightness by `d`.
    Darken,
}

impl TokenMethod {
    pub const ALL: &'static [TokenMethod] = &[
        TokenMethod::Alpha,
        TokenMethod::Mix,
        TokenMethod::Lighten,
        TokenMethod::Darken,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            TokenMethod::Alpha => "alpha",
            TokenMethod::Mix => "mix",
            TokenMethod::Lighten => "lighten",
            TokenMethod::Darken => "darken",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|m| m.name() == name)
    }
}

/// A channel of the base colour inside `oklch(from $c, l: l + 0.12)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Channel {
    L,
    C,
    H,
    Alpha,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// A token expression, evaluated by the render thread against the current
/// [`TokenTable`].
#[derive(Clone, Debug, PartialEq)]
pub enum TokenExpr {
    /// `$accent`, `$space.2`: a token by path, without the `$`.
    Ref(String),
    /// A literal value.
    Value(Box<PropValue>),
    /// `$surface.mix($fg, 8%)`.
    Method {
        receiver: Box<TokenExpr>,
        method: TokenMethod,
        args: Vec<TokenExpr>,
    },
    /// `oklch(from $surface, l: l + 0.12)`: channels left `None` keep the
    /// base colour's value; inside the channel expressions
    /// [`TokenExpr::Channel`] reads the base colour.
    OklchFrom {
        base: Box<TokenExpr>,
        l: Option<Box<TokenExpr>>,
        c: Option<Box<TokenExpr>>,
        h: Option<Box<TokenExpr>>,
        alpha: Option<Box<TokenExpr>>,
    },
    /// A channel of the enclosing `oklch(from …)` base colour.
    Channel(Channel),
    /// Arithmetic on numbers (channel expressions).
    Binary {
        op: BinOp,
        lhs: Box<TokenExpr>,
        rhs: Box<TokenExpr>,
    },
    /// A composite value whose colours come from token expressions:
    /// `border: 1, $border` or `linear(45deg, $accent, $tertiary)`.
    /// (Comma shorthands of numbers, `pad: 0, $space.3`, need no template:
    /// they are a `PropValue::List` holding `PropValue::Token` items, which
    /// [`TokenScope::resolve`] resolves in place.) The
    /// `n`-th entry of `colors` replaces the `n`-th colour of `value` in
    /// [`PropValue::colors_mut`] order; `None` keeps the literal colour.
    /// (M4) Likewise the `n`-th entry of `numbers` replaces the `n`-th
    /// number in [`PropValue::numbers_mut`] order, so `glow: 10 *
    /// wave(2s), $accent.alpha(0.4)` and `conic(from: t * 40deg, …)`
    /// travel as one value.
    Template {
        value: Box<PropValue>,
        colors: Vec<Option<TokenExpr>>,
        numbers: Vec<Option<TokenExpr>>,
    },
    /// (M4) `t`: seconds since the node appeared ([`TimeContext::t`]).
    Time,
    /// (M4) `wave(period, phase: 0)`: swings from 0 to 1 and back once
    /// per `period`, `0.5 − 0.5·cos(2π(t / period + phase))`, so it reads
    /// 0 at `t = 0`; `phase` is in periods (`phase: index * 0.1`).
    Wave {
        period: std::time::Duration,
        phase: Box<TokenExpr>,
    },
    /// (M4) `noise(x)`: smooth 1-D gradient noise in `-1..=1`, 0 at every
    /// whole `x`. A time signal only when `x` reads `t`.
    Noise(Box<TokenExpr>),
    /// (M4) A `letters` letter's `index` ([`TimeContext::index`]).
    Index,
    /// (M4) The number of letters in a `letters` block
    /// ([`TimeContext::count`]).
    Count,
}

/// (M4) What a node's time leaves read: [`TokenExpr::Time`],
/// [`TokenExpr::Index`] and [`TokenExpr::Count`]. Render builds one per
/// node per frame ([`TokenScope::with_time`]); without one, and under
/// `reduced_motion`, every time leaf reads 0.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct TimeContext {
    /// Seconds since the node appeared.
    pub t: f32,
    /// A `letters` letter's position, 0 elsewhere.
    pub index: u32,
    /// How many letters its `letters` block has, 0 elsewhere.
    pub count: u32,
}

impl TimeContext {
    /// `t` seconds into a node that is not a letter.
    pub const fn at(t: f32) -> Self {
        Self {
            t,
            index: 0,
            count: 0,
        }
    }
}

/// 1-D gradient noise (Perlin): smooth, in `-1..=1`, 0 at whole `x`.
pub fn noise(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let x0 = x.floor();
    let f = x - x0;
    // A gradient in -1..=1 per lattice point, from an integer hash.
    let grad = |i: f32| {
        let mut h = (i as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 31;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 29;
        (h >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };
    let a = grad(x0) * f;
    let b = grad(x0 + 1.0) * (f - 1.0);
    let s = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    // Two gradients each at most 0.5 away meet at most 0.5: scale to 1.
    ((a + (b - a) * s) * 2.0).clamp(-1.0, 1.0)
}

impl TokenExpr {
    /// `$path`.
    pub fn path(path: impl Into<String>) -> Self {
        TokenExpr::Ref(path.into())
    }

    /// A literal.
    pub fn value(v: PropValue) -> Self {
        TokenExpr::Value(Box::new(v))
    }

    /// (M4) True if the expression reads time: `t`, any `wave(…)`, or a
    /// `noise(x)` whose `x` does. A prop holding one is frame-driven:
    /// its node repaints each frame of its clock while visible, and only
    /// it. `index` and `count` alone are fixed per letter.
    pub fn reads_time(&self) -> bool {
        match self {
            TokenExpr::Time | TokenExpr::Wave { .. } => true,
            TokenExpr::Noise(x) => x.reads_time(),
            TokenExpr::Index | TokenExpr::Count | TokenExpr::Ref(_) | TokenExpr::Channel(_) => {
                false
            }
            TokenExpr::Value(v) => v.reads_time(),
            TokenExpr::Method { receiver, args, .. } => {
                receiver.reads_time() || args.iter().any(TokenExpr::reads_time)
            }
            TokenExpr::OklchFrom {
                base,
                l,
                c,
                h,
                alpha,
            } => {
                base.reads_time()
                    || [l, c, h, alpha]
                        .into_iter()
                        .flatten()
                        .any(|e| e.reads_time())
            }
            TokenExpr::Binary { lhs, rhs, .. } => lhs.reads_time() || rhs.reads_time(),
            TokenExpr::Template {
                value,
                colors,
                numbers,
            } => {
                value.reads_time()
                    || colors
                        .iter()
                        .chain(numbers)
                        .flatten()
                        .any(TokenExpr::reads_time)
            }
        }
    }

    /// `self.method(args…)`.
    pub fn call(self, method: TokenMethod, args: Vec<TokenExpr>) -> Self {
        TokenExpr::Method {
            receiver: Box::new(self),
            method,
            args,
        }
    }
}

/// The token table: palette roots and other plain values by path (without
/// the `$`: `"accent"`, `"space.2"`, `"motion.spatial"`), plus derived
/// tokens as expressions over them. Replaced wholesale by
/// [`crate::SceneOp::SetTokens`]; only the roots spring (M2), derived
/// tokens are re-evaluated from them every frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TokenTable {
    /// Plain values: palette roots, scales, fonts, springs.
    pub tokens: BTreeMap<String, PropValue>,
    /// Derived tokens: `fg.muted: $fg.alpha(0.65)`.
    pub derived: BTreeMap<String, TokenExpr>,
    /// Declared text/background pairs (the contrast guard): a text token
    /// by path, and the background tokens it is drawn on. Wherever the
    /// text token is evaluated (the global table's pairs apply in every
    /// scope, so a palette mid-spring or a `set { }` override is guarded
    /// too), its OKLCH lightness is solved to keep at least
    /// [`MIN_CONTRAST`] over each background
    /// ([`Color::with_contrast`]).
    pub contrast: BTreeMap<String, Vec<String>>,
    /// Where each path was defined, for the inspector's provenance
    /// (`bg ← surface.hi ← base ← palette:wallpaper`): a tier and its
    /// name (`palette:catppuccin:mocha`, `base`, `tokens compact`,
    /// `component Toast`). Informational: evaluation never reads it.
    pub origins: BTreeMap<String, String>,
    /// Every token evaluated once, as the global scope sees it
    /// ([`TokenTable::freeze`]).
    frozen: Frozen,
}

/// A table's tokens evaluated once in its own scope: what the render
/// thread works out once per frame (design.md, "each frame the render
/// thread re-evaluates the small token graph"), so the nodes of the
/// frame read their tokens instead of evaluating derived chains and
/// solving contrast per use. A clone of the table does not carry it,
/// and it never makes two tables differ.
#[derive(Default)]
struct Frozen(Option<HashMap<String, PropValue>>);

impl Clone for Frozen {
    fn clone(&self) -> Self {
        Frozen(None)
    }
}

impl PartialEq for Frozen {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for Frozen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(m) => write!(f, "Frozen({} tokens)", m.len()),
            None => f.write_str("Frozen(none)"),
        }
    }
}

impl TokenTable {
    /// Records that `path` was defined by `origin` (see
    /// [`TokenTable::origins`]).
    pub fn set_origin(&mut self, path: impl Into<String>, origin: impl Into<String>) {
        self.origins.insert(path.into(), origin.into());
    }

    /// Where `path` was defined, if recorded.
    pub fn origin(&self, path: &str) -> Option<&str> {
        self.origins.get(path).map(String::as_str)
    }

    /// The plain value stored at `path` (not evaluating derived tokens).
    pub fn get(&self, path: &str) -> Option<&PropValue> {
        self.tokens.get(path)
    }

    /// Sets a plain value at `path`, replacing a derived one there.
    pub fn insert(&mut self, path: impl Into<String>, value: PropValue) {
        let path = path.into();
        self.thaw();
        self.derived.remove(&path);
        self.tokens.insert(path, value);
    }

    /// Sets a derived token at `path`, replacing a plain one there.
    pub fn insert_derived(&mut self, path: impl Into<String>, expr: TokenExpr) {
        let path = path.into();
        self.thaw();
        self.tokens.remove(&path);
        self.derived.insert(path, expr);
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.derived.is_empty()
    }

    /// Declare that `text` is drawn on `bgs` (see [`TokenTable::contrast`]).
    pub fn insert_contrast(&mut self, text: impl Into<String>, bgs: Vec<String>) {
        self.thaw();
        self.contrast.insert(text.into(), bgs);
    }

    /// Evaluates the token at `path`, plain or derived.
    pub fn lookup(&self, path: &str) -> Option<PropValue> {
        TokenScope::new(&[self]).lookup(path)
    }

    /// Evaluates every token of the table once, in its own scope, and
    /// keeps the values: from then on a lookup in a scope whose global
    /// table this is (and an override's right-hand side that reads it)
    /// takes the kept value instead of evaluating the token again. The
    /// render thread freezes the tree's table once per frame while a
    /// palette springs, and once per `SetTokens` otherwise.
    ///
    /// Writing to [`TokenTable::tokens`], [`TokenTable::derived`] or
    /// [`TokenTable::contrast`] directly afterwards leaves stale values:
    /// freeze again (or [`TokenTable::thaw`]). The `insert` methods thaw.
    pub fn freeze(&mut self) {
        self.frozen = Frozen(None);
        let mut out = HashMap::with_capacity(self.tokens.len() + self.derived.len());
        for path in self.tokens.keys().chain(self.derived.keys()) {
            if let Some(v) = self.lookup(path) {
                out.insert(path.clone(), v);
            }
        }
        self.frozen = Frozen(Some(out));
    }

    /// Drops what [`TokenTable::freeze`] kept: lookups evaluate again.
    pub fn thaw(&mut self) {
        self.frozen = Frozen(None);
    }

    /// True while [`TokenTable::freeze`]'s values are kept.
    pub fn is_frozen(&self) -> bool {
        self.frozen.0.is_some()
    }

    /// Resolves a prop value: token references are evaluated, everything
    /// else is borrowed as is. `None` if a reference cannot be resolved
    /// (unknown token, wrong type); callers treat that as unset.
    pub fn resolve<'a>(&self, v: &'a PropValue) -> Option<Cow<'a, PropValue>> {
        TokenScope::new(&[self]).resolve(v)
    }

    /// Evaluates an expression.
    pub fn eval(&self, e: &TokenExpr) -> Option<PropValue> {
        TokenScope::new(&[self]).eval(e)
    }
}

/// Most solved pairs [`guarded`] remembers before it starts over.
const GUARD_MEMO: usize = 256;

/// Backgrounds a memo key holds (the palette's widest pair, `$fg` over
/// the eight surfaces); a pair with more is solved without the memo.
const GUARD_BGS: usize = 8;

/// A memo key: the text and up to [`GUARD_BGS`] backgrounds as f32 bits,
/// inline (no allocation on the per-frame path), then how many
/// backgrounds there are.
type GuardKey = ([u32; 4 * (GUARD_BGS + 1)], u8);

thread_local! {
    /// Solved text colours by (text, backgrounds): a frame's text nodes
    /// share their scope's few pairs, so each pair is solved once per
    /// frame (once per palette while nothing springs), not once per
    /// lookup.
    static GUARD: std::cell::RefCell<std::collections::HashMap<GuardKey, Color>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Solves done (memo misses), for cost tests.
    static GUARD_SOLVES: Cell<u64> = const { Cell::new(0) };
}

fn guard_key(text: Color, bgs: &[Color]) -> Option<GuardKey> {
    if bgs.len() > GUARD_BGS {
        return None;
    }
    let mut key = [0u32; 4 * (GUARD_BGS + 1)];
    for (i, c) in std::iter::once(&text).chain(bgs).enumerate() {
        key[i * 4..i * 4 + 4].copy_from_slice(&[c.r, c.g, c.b, c.a].map(f32::to_bits));
    }
    Some((key, bgs.len() as u8))
}

/// `text` solved over `bgs` ([`Color::with_contrast`]), memoised.
fn guarded(text: Color, bgs: &[Color]) -> Color {
    let key = guard_key(text, bgs);
    if let Some(k) = &key
        && let Some(c) = GUARD.with(|m| m.borrow().get(k).copied())
    {
        return c;
    }
    GUARD_SOLVES.with(|n| n.set(n.get() + 1));
    let solved = text.with_contrast(bgs, MIN_CONTRAST);
    if let Some(k) = key {
        GUARD.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= GUARD_MEMO {
                m.clear();
            }
            m.insert(k, solved);
        });
    }
    solved
}

/// How many contrast solves this thread has done (the memo's misses).
#[doc(hidden)]
pub fn guard_solves() -> u64 {
    GUARD_SOLVES.with(Cell::get)
}

/// A chain of token tables: the global table sent by
/// [`crate::SceneOp::SetTokens`] first, then the [`crate::Prop::Tokens`]
/// overrides of each ancestor down to the node being drawn.
///
/// Lookup takes the nearest table that defines a path. An override's own
/// expression is evaluated in its *parent* scope, so in
/// `set { $surface: $surface.alpha(0.5) }` the right-hand `$surface` is the
/// inherited value (no cycle). Derived tokens of the global table are
/// evaluated in the scope of the node asking, so they stay derived inside
/// a subtree: with `$surface` overridden, `$surface.hi` follows it.
///
/// Every public entry point evaluates within [`MAX_TOKEN_STEPS`], so a
/// table with a large reference fan-out (user input) fails to resolve
/// instead of stalling the render thread.
#[derive(Copy, Clone, Debug)]
pub struct TokenScope<'a> {
    levels: &'a [&'a TokenTable],
    /// What time leaves read; `None` reads them all as 0.
    time: Option<TimeContext>,
}

/// Most evaluation steps (references followed plus expression nodes
/// visited) one `lookup`, `resolve`, `eval` or `transition` may take.
/// The design's whole token graph is about 100 operations.
pub const MAX_TOKEN_STEPS: u32 = 10_000;

/// Remaining work for one resolution, and whether the contrast guard
/// is on (it is off while it evaluates a pair's backgrounds: one level).
struct Budget {
    left: Cell<u32>,
    guarding: Cell<bool>,
}

impl Budget {
    fn new() -> Self {
        Self {
            left: Cell::new(MAX_TOKEN_STEPS),
            guarding: Cell::new(true),
        }
    }

    /// Takes one step; false once the budget is spent.
    fn step(&self) -> bool {
        let left = self.left.get();
        if left == 0 {
            return false;
        }
        self.left.set(left - 1);
        true
    }
}

thread_local! {
    /// Steps the last public entry point took (tests).
    static LAST_STEPS: Cell<u32> = const { Cell::new(0) };
}

impl Drop for Budget {
    fn drop(&mut self) {
        LAST_STEPS.with(|s| s.set(MAX_TOKEN_STEPS - self.left.get()));
    }
}

/// How many steps the last finished `lookup`/`resolve`/`eval` on this
/// thread took (cost tests).
#[doc(hidden)]
pub fn last_token_steps() -> u32 {
    LAST_STEPS.with(Cell::get)
}

impl<'a> TokenScope<'a> {
    /// `levels[0]` is the global table, the last entry the nearest
    /// override.
    pub fn new(levels: &'a [&'a TokenTable]) -> Self {
        Self { levels, time: None }
    }

    /// (M4) The same scope reading `time` for its time leaves (a node's
    /// clock and, in a `letters` block, the letter's index and count).
    /// Render passes `None` under `reduced_motion`, which freezes every
    /// time signal at 0.
    pub fn with_time(self, time: Option<TimeContext>) -> Self {
        Self { time, ..self }
    }

    /// (M4) What this scope's time leaves read.
    pub fn time(&self) -> Option<TimeContext> {
        self.time
    }

    /// Evaluates the token at `path` in this scope.
    pub fn lookup(&self, path: &str) -> Option<PropValue> {
        self.eval_ref(path, 0, &Budget::new())
    }

    /// Resolves a prop value in this scope (see [`TokenTable::resolve`]).
    /// Token references nested in a `List`, `Pose` or `Call` (the comma
    /// shorthand `pad: 0, $space.3`, `radius: $radius.lg, $radius.lg, 0,
    /// 0`) are resolved in place; if any of them fails the whole value
    /// does.
    pub fn resolve<'v>(&self, v: &'v PropValue) -> Option<Cow<'v, PropValue>> {
        self.resolve_in(v, 0, &Budget::new())
    }

    /// Evaluates an expression in this scope.
    pub fn eval(&self, e: &TokenExpr) -> Option<PropValue> {
        self.eval_in(e, 0, None, &Budget::new())
    }

    /// The concrete curve for a prop set with `t`: `Default` takes the
    /// prop class's `$motion.spatial` / `$motion.effects` token, `Token`
    /// takes the named token. Props that cannot interpolate
    /// ([`PropClass::Snap`]: fonts, text, keywords) always snap, whatever
    /// was asked. Missing or mistyped tokens fall back to
    /// [`Transition::Instant`].
    pub fn transition(&self, t: &Transition, prop: Prop) -> Transition {
        let path = match (prop.class(), t) {
            (PropClass::Snap, _) => return Transition::Instant,
            (PropClass::Spatial, Transition::Default) => "motion.spatial",
            (PropClass::Effects, Transition::Default) => "motion.effects",
            (_, Transition::Token(path)) => path.as_str(),
            (_, t) => return t.clone(),
        };
        match self.lookup(path) {
            Some(PropValue::Transition(t))
                if !matches!(t, Transition::Default | Transition::Token(_)) =>
            {
                t
            }
            // A shell with no `motion` tokens still animates with the
            // design's springs (decisions.md, wave3-pixels).
            None => match path {
                "motion.spatial" => Transition::of_spring(crate::motion::SPATIAL),
                "motion.effects" => Transition::of_spring(crate::motion::EFFECTS),
                "motion.bouncy" => Transition::of_spring(crate::motion::BOUNCY),
                _ => Transition::Instant,
            },
            _ => Transition::Instant,
        }
    }

    fn resolve_in<'v>(
        &self,
        v: &'v PropValue,
        depth: u32,
        budget: &Budget,
    ) -> Option<Cow<'v, PropValue>> {
        if !v.has_tokens() {
            return Some(Cow::Borrowed(v));
        }
        let all = |items: &[PropValue]| -> Option<Vec<PropValue>> {
            items
                .iter()
                .map(|i| self.resolve_in(i, depth, budget).map(Cow::into_owned))
                .collect()
        };
        Some(Cow::Owned(match v {
            PropValue::Token(e) => self.eval_in(e, depth, None, budget)?,
            PropValue::List(items) => PropValue::List(all(items)?),
            PropValue::Call { name, args } => PropValue::Call {
                name: name.clone(),
                args: all(args)?,
            },
            PropValue::Pose(props) => PropValue::Pose(
                props
                    .iter()
                    .map(|(p, v)| {
                        self.resolve_in(v, depth, budget)
                            .map(|v| (*p, v.into_owned()))
                    })
                    .collect::<Option<_>>()?,
            ),
            PropValue::Uniforms(entries) => PropValue::Uniforms(
                entries
                    .iter()
                    .map(|(n, v)| {
                        self.resolve_in(v, depth, budget)
                            .map(|v| (n.clone(), v.into_owned()))
                    })
                    .collect::<Option<_>>()?,
            ),
            v => v.clone(),
        }))
    }

    fn eval_ref(&self, path: &str, depth: u32, budget: &Budget) -> Option<PropValue> {
        if depth > MAX_TOKEN_DEPTH || !budget.step() {
            return None;
        }
        // The global scope of a frozen table reads the kept values (not
        // while the guard evaluates a pair's backgrounds: those are read
        // unguarded, one level).
        if let [only] = self.levels
            && budget.guarding.get()
            && let Some(kept) = &only.frozen.0
        {
            return kept.get(path).cloned();
        }
        for (i, table) in self.levels.iter().enumerate().rev() {
            // Overrides see their parent scope; global derived tokens see
            // the scope of whoever asks.
            let scope = if i == 0 {
                *self
            } else {
                TokenScope::new(&self.levels[..i]).with_time(self.time)
            };
            let v = if let Some(v) = table.tokens.get(path) {
                scope.resolve_in(v, depth + 1, budget).map(Cow::into_owned)
            } else if let Some(e) = table.derived.get(path) {
                scope.eval_in(e, depth + 1, None, budget)
            } else {
                continue;
            };
            return self.guard(path, v, depth, budget);
        }
        None
    }

    /// The contrast guard: a declared text token keeps [`MIN_CONTRAST`]
    /// over its backgrounds, as evaluated in this scope.
    fn guard(
        &self,
        path: &str,
        v: Option<PropValue>,
        depth: u32,
        budget: &Budget,
    ) -> Option<PropValue> {
        let Some(PropValue::Color(text)) = v else {
            return v;
        };
        if !budget.guarding.get() {
            return v;
        }
        let Some(bgs) = self.levels.first().and_then(|t| t.contrast.get(path)) else {
            return v;
        };
        // The backgrounds as they are, unguarded: a background derived
        // from a guarded text token (`$bg.mix($fg, 4%)`) reads it once,
        // not through another guard per level.
        budget.guarding.set(false);
        let mut inline = [Color::BLACK; GUARD_BGS];
        let mut n = 0;
        let mut more: Vec<Color> = Vec::new();
        for b in bgs.iter().filter(|b| b.as_str() != path) {
            // A translucent background shows what is under it; only
            // opaque ones can be judged.
            if let Some(PropValue::Color(c)) = self.eval_ref(b, depth + 1, budget)
                && c.a >= 1.0
            {
                if n < GUARD_BGS && more.is_empty() {
                    inline[n] = c;
                    n += 1;
                } else {
                    if more.is_empty() {
                        more.extend_from_slice(&inline[..n]);
                    }
                    more.push(c);
                }
            }
        }
        budget.guarding.set(true);
        let bgs = if more.is_empty() {
            &inline[..n]
        } else {
            &more[..]
        };
        Some(PropValue::Color(guarded(text, bgs)))
    }

    fn eval_in(
        &self,
        e: &TokenExpr,
        depth: u32,
        base: Option<Oklch>,
        budget: &Budget,
    ) -> Option<PropValue> {
        if depth > MAX_TOKEN_DEPTH || !budget.step() {
            return None;
        }
        let sub = |e: &TokenExpr| self.eval_in(e, depth + 1, base, budget);
        let num = |e: &TokenExpr| sub(e).as_ref().and_then(fraction);
        let col = |e: &TokenExpr| sub(e).as_ref().and_then(color);
        match e {
            TokenExpr::Ref(path) => self.eval_ref(path, depth + 1, budget),
            TokenExpr::Value(v) => match v.as_ref() {
                PropValue::Token(inner) => sub(inner),
                v => self.resolve_in(v, depth + 1, budget).map(Cow::into_owned),
            },
            TokenExpr::Method {
                receiver,
                method,
                args,
            } => {
                let c = col(receiver)?;
                let out = match (method, args.as_slice()) {
                    (TokenMethod::Alpha, [a]) => c.with_alpha(num(a)?),
                    (TokenMethod::Mix, [other, t]) => c.lerp_oklab(col(other)?, num(t)?),
                    (TokenMethod::Mix, [other]) => c.lerp_oklab(col(other)?, 0.5),
                    (TokenMethod::Lighten, [d]) => shift_l(c, num(d)?),
                    (TokenMethod::Darken, [d]) => shift_l(c, -num(d)?),
                    _ => return None,
                };
                Some(PropValue::Color(gamut_map(out)))
            }
            TokenExpr::OklchFrom {
                base: b,
                l,
                c,
                h,
                alpha,
            } => {
                let lch = col(b)?.to_oklch();
                let ch = |slot: &Option<Box<TokenExpr>>, keep: f64| -> Option<f64> {
                    match slot {
                        None => Some(keep),
                        Some(e) => self
                            .eval_in(e, depth + 1, Some(lch), budget)
                            .as_ref()
                            .and_then(fraction)
                            .map(f64::from),
                    }
                };
                let out = Oklch {
                    l: ch(l, lch.l)?,
                    c: ch(c, lch.c)?.max(0.0),
                    h: ch(h, lch.h)?,
                    alpha: ch(alpha, lch.alpha)?,
                };
                Some(PropValue::Color(gamut_map(Color::from_oklch(out))))
            }
            TokenExpr::Channel(ch) => {
                let b = base?;
                let v = match ch {
                    Channel::L => b.l,
                    Channel::C => b.c,
                    Channel::H => b.h,
                    Channel::Alpha => b.alpha,
                };
                Some(PropValue::Number(v as f32))
            }
            TokenExpr::Binary { op, lhs, rhs } => {
                let (a, b) = (num(lhs)?, num(rhs)?);
                let v = match op {
                    BinOp::Add => a + b,
                    BinOp::Sub => a - b,
                    BinOp::Mul => a * b,
                    BinOp::Div => a / b,
                };
                v.is_finite().then_some(PropValue::Number(v))
            }
            TokenExpr::Template {
                value,
                colors,
                numbers,
            } => {
                let mut v = (**value).clone();
                for (slot, expr) in v.colors_mut().into_iter().zip(colors) {
                    if let Some(e) = expr {
                        *slot = col(e)?;
                    }
                }
                for (slot, expr) in v.numbers_mut().into_iter().zip(numbers) {
                    if let Some(e) = expr {
                        *slot = num(e)?;
                    }
                }
                Some(v)
            }
            TokenExpr::Time => Some(PropValue::Number(self.time.map_or(0.0, |c| c.t))),
            TokenExpr::Index => Some(PropValue::Number(self.time.map_or(0.0, |c| c.index as f32))),
            TokenExpr::Count => Some(PropValue::Number(self.time.map_or(0.0, |c| c.count as f32))),
            TokenExpr::Wave { period, phase } => {
                let Some(ctx) = self.time else {
                    return Some(PropValue::Number(0.0));
                };
                let phase = num(phase)?;
                let period = period.as_secs_f32();
                if period <= 0.0 {
                    return None;
                }
                let turns = ctx.t / period + phase;
                let v = 0.5 - 0.5 * (std::f32::consts::TAU * turns).cos();
                v.is_finite().then_some(PropValue::Number(v))
            }
            TokenExpr::Noise(x) => {
                if self.time.is_none() {
                    return Some(PropValue::Number(0.0));
                }
                Some(PropValue::Number(noise(num(x)?)))
            }
        }
    }
}

/// A number, with percentages as fractions (`8%` is `0.08`).
fn fraction(v: &PropValue) -> Option<f32> {
    let n = match v {
        PropValue::Number(n) | PropValue::Length(Length::Px(n)) => *n,
        PropValue::Length(Length::Percent(p)) => *p / 100.0,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

fn color(v: &PropValue) -> Option<Color> {
    match v {
        PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => Some(*c),
        _ => None,
    }
}

fn shift_l(c: Color, d: f32) -> Color {
    let mut lch = c.to_oklch();
    lch.l += d as f64;
    Color::from_oklch(lch)
}

/// Brings a derived colour back into sRGB by lowering OKLCH chroma
/// (CSS Color 4 gamut mapping, [`Color::gamut_mapped`]).
fn gamut_map(c: Color) -> Color {
    c.gamut_mapped()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Border, GradientStop, Shadow};

    fn table() -> TokenTable {
        let mut t = TokenTable::default();
        t.insert("fg", PropValue::Color(Color::WHITE));
        t.insert(
            "surface",
            PropValue::Color(Color::from_hex("#1e1e2e").unwrap()),
        );
        t.insert("space.2", PropValue::Number(8.0));
        t.insert_derived(
            "fg.muted",
            TokenExpr::path("fg").call(
                TokenMethod::Alpha,
                vec![TokenExpr::value(PropValue::Number(0.65))],
            ),
        );
        t.insert_derived(
            "surface.hi",
            TokenExpr::path("surface").call(
                TokenMethod::Mix,
                vec![
                    TokenExpr::path("fg"),
                    TokenExpr::value(PropValue::Length(Length::Percent(8.0))),
                ],
            ),
        );
        t.insert_derived(
            "border",
            TokenExpr::OklchFrom {
                base: Box::new(TokenExpr::path("surface")),
                l: Some(Box::new(TokenExpr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(TokenExpr::Channel(Channel::L)),
                    rhs: Box::new(TokenExpr::value(PropValue::Number(0.12))),
                })),
                c: None,
                h: None,
                alpha: None,
            },
        );
        t
    }

    #[test]
    fn derived_tokens_follow_their_roots() {
        let mut t = table();
        let muted = t.lookup("fg.muted").unwrap();
        assert_eq!(muted, PropValue::Color(Color::WHITE.with_alpha(0.65)));
        let border = t.lookup("border").unwrap();
        let PropValue::Color(b) = border else {
            panic!()
        };
        let surface = Color::from_hex("#1e1e2e").unwrap();
        assert!((b.to_oklch().l - surface.to_oklch().l - 0.12).abs() < 1e-4);
        let PropValue::Color(hi) = t.lookup("surface.hi").unwrap() else {
            panic!()
        };
        assert!(hi.to_oklch().l > surface.to_oklch().l);

        // Changing a root re-derives without touching the derived entries.
        t.insert("fg", PropValue::Color(Color::BLACK));
        assert_eq!(
            t.lookup("fg.muted").unwrap(),
            PropValue::Color(Color::BLACK.with_alpha(0.65))
        );
    }

    #[test]
    fn prop_values_resolve_references_and_templates() {
        let t = table();
        let v = PropValue::Token(TokenExpr::path("space.2"));
        assert_eq!(t.resolve(&v).unwrap().into_owned(), PropValue::Number(8.0));
        let lit = PropValue::Number(3.0);
        assert!(matches!(t.resolve(&lit), Some(Cow::Borrowed(_))));
        assert!(
            t.resolve(&PropValue::Token(TokenExpr::path("nope")))
                .is_none()
        );

        let border = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::Border(Border {
                width: 1.0,
                paint: Paint::Solid(Color::TRANSPARENT),
            })),
            colors: vec![Some(TokenExpr::path("fg.muted"))],
            numbers: vec![],
        });
        let PropValue::Border(b) = t.resolve(&border).unwrap().into_owned() else {
            panic!()
        };
        assert_eq!(b.paint, Paint::Solid(Color::WHITE.with_alpha(0.65)));

        let shadows = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::Shadow(vec![
                Shadow {
                    x: 0.0,
                    y: 2.0,
                    blur: 8.0,
                    spread: 0.0,
                    color: Color::TRANSPARENT,
                };
                2
            ])),
            colors: vec![None, Some(TokenExpr::path("fg"))],
            numbers: vec![],
        });
        let PropValue::Shadow(s) = t.resolve(&shadows).unwrap().into_owned() else {
            panic!()
        };
        assert_eq!((s[0].color, s[1].color), (Color::TRANSPARENT, Color::WHITE));
    }

    fn wave(period_ms: u64, phase: TokenExpr) -> TokenExpr {
        TokenExpr::Wave {
            period: std::time::Duration::from_millis(period_ms),
            phase: Box::new(phase),
        }
    }

    fn n(v: f32) -> TokenExpr {
        TokenExpr::value(PropValue::Number(v))
    }

    fn mul(a: TokenExpr, b: TokenExpr) -> TokenExpr {
        TokenExpr::Binary {
            op: BinOp::Mul,
            lhs: Box::new(a),
            rhs: Box::new(b),
        }
    }

    fn number(scope: TokenScope<'_>, e: &TokenExpr) -> f32 {
        match scope.eval(e) {
            Some(PropValue::Number(v)) => v,
            other => panic!("{e:?} gave {other:?}"),
        }
    }

    #[test]
    fn time_leaves_read_the_scope_time() {
        let t = table();
        let levels = [&t];
        let still = TokenScope::new(&levels);
        let at = |secs: f32| {
            still.with_time(Some(TimeContext {
                t: secs,
                index: 3,
                count: 8,
            }))
        };
        assert_eq!(at(1.5).time().map(|c| c.t), Some(1.5));
        assert_eq!(number(at(1.5), &TokenExpr::Time), 1.5);
        assert_eq!(number(at(1.5), &TokenExpr::Index), 3.0);
        assert_eq!(number(at(1.5), &TokenExpr::Count), 8.0);
        // `rotate: t * 20deg`.
        assert_eq!(number(at(2.0), &mul(TokenExpr::Time, n(20.0))), 40.0);

        // wave(2s): 0 at the start, 1 half way, 0 again a period on.
        let w = wave(2000, n(0.0));
        assert!(number(at(0.0), &w).abs() < 1e-6);
        assert!((number(at(1.0), &w) - 1.0).abs() < 1e-6);
        assert!((number(at(0.5), &w) - 0.5).abs() < 1e-6);
        assert!(number(at(2.0), &w).abs() < 1e-5);
        // phase: index * 0.1 is in periods.
        let phased = wave(1000, mul(TokenExpr::Index, n(0.1)));
        let shifted = TokenScope::new(&levels).with_time(Some(TimeContext {
            t: 0.2,
            index: 3,
            count: 8,
        }));
        let reference = TokenScope::new(&levels).with_time(Some(TimeContext::at(0.5)));
        assert!((number(shifted, &phased) - number(reference, &wave(1000, n(0.0)))).abs() < 1e-5);
        // A non-positive period does not resolve.
        assert!(at(1.0).eval(&wave(0, n(0.0))).is_none());

        // noise: 0 at whole x, smooth and bounded between.
        assert_eq!(number(at(0.0), &TokenExpr::Noise(Box::new(n(4.0)))), 0.0);
        let mut prev = noise(0.0);
        for i in 1..=400 {
            let v = noise(i as f32 * 0.01);
            assert!((-1.0..=1.0).contains(&v));
            assert!((v - prev).abs() < 0.05, "noise is smooth");
            prev = v;
        }
        assert!((0..40).any(|i| noise(i as f32 * 0.25 + 0.1).abs() > 0.05));
        assert_eq!(noise(f32::NAN), 0.0);
    }

    #[test]
    fn without_a_time_context_every_time_leaf_reads_zero() {
        let t = table();
        let levels = [&t];
        let scope = TokenScope::new(&levels);
        assert_eq!(scope.time(), None);
        for e in [
            TokenExpr::Time,
            TokenExpr::Index,
            TokenExpr::Count,
            wave(1600, n(0.25)),
            TokenExpr::Noise(Box::new(mul(TokenExpr::Time, n(3.3)))),
            TokenExpr::Noise(Box::new(n(0.5))),
        ] {
            assert_eq!(number(scope, &e), 0.0, "{e:?}");
            // `reduced_motion`: render passes `None` explicitly.
            assert_eq!(number(scope.with_time(None), &e), 0.0, "{e:?}");
        }
        assert_eq!(t.eval(&TokenExpr::Time), Some(PropValue::Number(0.0)));
    }

    #[test]
    fn reads_time_marks_frame_driven_values() {
        assert!(TokenExpr::Time.reads_time());
        assert!(wave(1000, n(0.0)).reads_time());
        assert!(TokenExpr::Noise(Box::new(TokenExpr::Time)).reads_time());
        assert!(!TokenExpr::Noise(Box::new(n(1.0))).reads_time());
        assert!(!TokenExpr::Index.reads_time());
        assert!(!TokenExpr::Count.reads_time());
        assert!(!TokenExpr::path("accent").reads_time());
        assert!(mul(n(10.0), wave(2000, n(0.0))).reads_time());
        let glow = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::List(vec![
                PropValue::Number(0.0),
                PropValue::Color(Color::TRANSPARENT),
            ])),
            colors: vec![Some(TokenExpr::path("accent"))],
            numbers: vec![Some(mul(n(10.0), wave(2000, n(0.0))))],
        });
        assert!(glow.reads_time());
        assert!(PropValue::List(vec![PropValue::Number(1.0), glow.clone()]).reads_time());
        assert!(
            PropValue::Uniforms(vec![("u_t".into(), PropValue::Token(TokenExpr::Time))])
                .reads_time()
        );
        assert!(!PropValue::Number(3.0).reads_time());
        assert!(!PropValue::Token(TokenExpr::path("fg")).reads_time());
    }

    #[test]
    fn templates_fill_numeric_slots_in_field_order() {
        let mut t = table();
        t.insert(
            "accent",
            PropValue::Color(Color::from_hex("#cba6f7").unwrap()),
        );
        let levels = [&t];
        // glow: 10 * wave(2s), $accent.alpha(0.4)
        let glow = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::List(vec![
                PropValue::Number(0.0),
                PropValue::Color(Color::TRANSPARENT),
            ])),
            colors: vec![Some(
                TokenExpr::path("accent").call(TokenMethod::Alpha, vec![n(0.4)]),
            )],
            numbers: vec![Some(mul(n(10.0), wave(2000, n(0.0))))],
        });
        let at_peak = TokenScope::new(&levels).with_time(Some(TimeContext::at(1.0)));
        let PropValue::List(items) = at_peak.resolve(&glow).unwrap().into_owned() else {
            panic!()
        };
        assert!((items[0].as_number().unwrap() - 10.0).abs() < 1e-4);
        assert!(matches!(items[1], PropValue::Color(c) if (c.a - 0.4).abs() < 1e-6));
        // Without time the radius reads 0 and the colour still resolves.
        let PropValue::List(items) = t.resolve(&glow).unwrap().into_owned() else {
            panic!()
        };
        assert_eq!(items[0], PropValue::Number(0.0));

        // border: 1.5, conic(from: t * 40deg, $accent, $accent): `from`
        // is slot 1 (the width is slot 0); `None` keeps the literal.
        let stop = |offset| GradientStop {
            offset,
            color: Color::TRANSPARENT,
        };
        let border = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::Border(Border {
                width: 1.5,
                paint: Paint::Conic {
                    from: 0.0,
                    stops: vec![stop(0.0), stop(1.0)],
                },
            })),
            colors: vec![
                Some(TokenExpr::path("accent")),
                Some(TokenExpr::path("accent")),
            ],
            numbers: vec![None, Some(mul(TokenExpr::Time, n(40.0)))],
        });
        let later = TokenScope::new(&levels).with_time(Some(TimeContext::at(2.0)));
        let PropValue::Border(b) = later.resolve(&border).unwrap().into_owned() else {
            panic!()
        };
        assert_eq!(b.width, 1.5);
        let Paint::Conic { from, stops } = b.paint else {
            panic!()
        };
        assert_eq!(from, 80.0);
        assert_eq!(stops[1].offset, 1.0);
        // A slot whose expression fails fails the whole value.
        let bad = PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::Number(1.0)),
            colors: vec![],
            numbers: vec![Some(TokenExpr::path("accent"))],
        });
        assert!(t.resolve(&bad).is_none());
    }

    #[test]
    fn uniforms_resolve_each_entry() {
        let t = table();
        let levels = [&t];
        let u = PropValue::Uniforms(vec![
            ("u_speed".into(), PropValue::Number(0.4)),
            ("u_tint".into(), PropValue::Token(TokenExpr::path("fg"))),
            ("u_time".into(), PropValue::Token(TokenExpr::Time)),
        ]);
        assert!(u.has_tokens());
        let scope = TokenScope::new(&levels).with_time(Some(TimeContext::at(3.0)));
        assert_eq!(
            scope.resolve(&u).unwrap().into_owned(),
            PropValue::Uniforms(vec![
                ("u_speed".into(), PropValue::Number(0.4)),
                ("u_tint".into(), PropValue::Color(Color::WHITE)),
                ("u_time".into(), PropValue::Number(3.0)),
            ])
        );
        let broken = PropValue::Uniforms(vec![(
            "u_x".into(),
            PropValue::Token(TokenExpr::path("nope")),
        )]);
        assert!(t.resolve(&broken).is_none());
    }

    #[test]
    fn overrides_keep_the_time_context() {
        // An override's right-hand side is evaluated in its parent scope,
        // which still reads the node's time.
        let t = table();
        let mut o = TokenTable::default();
        o.insert_derived("spin", mul(TokenExpr::Time, n(20.0)));
        let levels = [&t, &o];
        let scope = TokenScope::new(&levels).with_time(Some(TimeContext::at(0.5)));
        assert_eq!(scope.lookup("spin"), Some(PropValue::Number(10.0)));
    }

    #[test]
    fn cycles_and_bad_types_resolve_to_none() {
        let mut t = table();
        t.insert_derived("a", TokenExpr::path("b"));
        t.insert_derived("b", TokenExpr::path("a"));
        assert!(t.lookup("a").is_none());
        t.insert_derived(
            "bad",
            TokenExpr::path("space.2").call(TokenMethod::Alpha, vec![]),
        );
        assert!(t.lookup("bad").is_none());
        assert_eq!(TokenMethod::from_name("mix"), Some(TokenMethod::Mix));
    }

    #[test]
    fn scoped_overrides_shadow_inherit_and_rederive() {
        let global = table();
        let surface = Color::from_hex("#1e1e2e").unwrap();
        // set { $surface: $surface.alpha(0.5) }: the right-hand side is the
        // inherited value, so this is not a cycle.
        let mut set = TokenTable::default();
        set.insert(
            "surface",
            PropValue::Token(TokenExpr::path("surface").call(
                TokenMethod::Alpha,
                vec![TokenExpr::value(PropValue::Number(0.5))],
            )),
        );
        let levels = [&global, &set];
        let inner = TokenScope::new(&levels);
        assert_eq!(
            inner.lookup("surface"),
            Some(PropValue::Color(surface.with_alpha(0.5)))
        );
        // Global derived tokens follow the override inside the subtree...
        let PropValue::Color(b) = inner.lookup("border").unwrap() else {
            panic!()
        };
        assert_eq!(b.a, 0.5);
        // ...and not outside it.
        let PropValue::Color(b) = global.lookup("border").unwrap() else {
            panic!()
        };
        assert_eq!(b.a, 1.0);
        // Untouched tokens come from the global table.
        assert_eq!(inner.lookup("space.2"), Some(PropValue::Number(8.0)));

        // A nested override of a component token (`tokens { radius: … }`).
        let mut comp = TokenTable::default();
        comp.insert_derived("Toast.radius", TokenExpr::path("space.2"));
        comp.insert("surface", PropValue::Color(Color::BLACK));
        let levels = [&global, &set, &comp];
        let deeper = TokenScope::new(&levels);
        assert_eq!(deeper.lookup("Toast.radius"), Some(PropValue::Number(8.0)));
        assert_eq!(
            deeper.lookup("surface"),
            Some(PropValue::Color(Color::BLACK))
        );
        assert_eq!(inner.lookup("Toast.radius"), None);
    }

    #[test]
    fn transitions_resolve_through_motion_tokens() {
        let mut t = table();
        let spatial = Transition::Spring {
            stiffness: 700.0,
            damping: 0.9,
        };
        let bouncy = Transition::Spring {
            stiffness: 380.0,
            damping: 0.75,
        };
        t.insert("motion.spatial", PropValue::Transition(spatial.clone()));
        t.insert("motion.bouncy", PropValue::Transition(bouncy.clone()));
        let levels = [&t];
        let s = TokenScope::new(&levels);
        assert_eq!(s.transition(&Transition::Default, Prop::Width), spatial);
        // width: 24 ~ $motion.bouncy
        assert_eq!(
            s.transition(&Transition::Token("motion.bouncy".into()), Prop::Width),
            bouncy
        );
        // No $motion.effects token: the design's spring; fonts snap,
        // unknown tokens snap.
        assert_eq!(
            s.transition(&Transition::Default, Prop::Bg),
            Transition::of_spring(crate::motion::EFFECTS)
        );
        assert_eq!(
            s.transition(&Transition::Default, Prop::Font),
            Transition::Instant
        );
        assert_eq!(
            s.transition(&Transition::Token("motion.nope".into()), Prop::X),
            Transition::Instant
        );
        // A theme swap reaches props that named the token.
        let mut t2 = t.clone();
        t2.insert("motion.bouncy", PropValue::Transition(spatial.clone()));
        let levels = [&t2];
        assert_eq!(
            TokenScope::new(&levels)
                .transition(&Transition::Token("motion.bouncy".into()), Prop::X),
            spatial
        );
    }

    #[test]
    fn shorthand_lists_resolve_through_scoped_overrides() {
        use crate::protocol::{Corners, Insets};
        let mut global = table();
        global.insert("space.3", PropValue::Number(12.0));
        global.insert("radius.lg", PropValue::Number(14.0));
        // pad: 0, $space.3
        let pad = PropValue::List(vec![
            PropValue::Number(0.0),
            PropValue::Token(TokenExpr::path("space.3")),
        ]);
        // radius: $radius.lg, $radius.lg, 0, 0
        let radius = PropValue::List(vec![
            PropValue::Token(TokenExpr::path("radius.lg")),
            PropValue::Token(TokenExpr::path("radius.lg")),
            PropValue::Number(0.0),
            PropValue::Number(0.0),
        ]);
        let levels = [&global];
        let s = TokenScope::new(&levels);
        let insets = |s: &TokenScope<'_>| s.resolve(&pad).unwrap().insets().unwrap();
        assert_eq!(
            insets(&s),
            Insets {
                top: 0.0,
                right: 12.0,
                bottom: 0.0,
                left: 12.0
            }
        );
        // A compact theme or `set { $space.3: 4 }` reaches it unresolved.
        let mut set = TokenTable::default();
        set.insert("space.3", PropValue::Number(4.0));
        set.insert("radius.lg", PropValue::Number(6.0));
        let levels = [&global, &set];
        let inner = TokenScope::new(&levels);
        assert_eq!(insets(&inner).left, 4.0);
        let PropValue::List(r) = inner.resolve(&radius).unwrap().into_owned() else {
            panic!()
        };
        let v: Vec<f32> = r.iter().map(|v| v.as_number().unwrap()).collect();
        assert_eq!(
            Corners::from_values(&v),
            Some(Corners {
                top_left: 6.0,
                top_right: 6.0,
                bottom_right: 0.0,
                bottom_left: 0.0
            })
        );
        // margin: $space.2, $space.2, 0 is top 8, sides 8, bottom 0.
        let margin = PropValue::List(vec![
            PropValue::Token(TokenExpr::path("space.2")),
            PropValue::Token(TokenExpr::path("space.2")),
            PropValue::Number(0.0),
        ]);
        let m = s.resolve(&margin).unwrap().insets().unwrap();
        assert_eq!((m.top, m.right, m.bottom, m.left), (8.0, 8.0, 0.0, 8.0));
        // One unresolvable item fails the whole value; plain lists borrow.
        let bad = PropValue::List(vec![PropValue::Token(TokenExpr::path("nope"))]);
        assert!(s.resolve(&bad).is_none());
        let plain = PropValue::List(vec![PropValue::Number(1.0)]);
        assert!(matches!(s.resolve(&plain), Some(Cow::Borrowed(_))));
        // Calls and poses resolve their arguments too.
        let tint = PropValue::Call {
            name: "tint".into(),
            args: vec![PropValue::Token(TokenExpr::path("fg"))],
        };
        assert_eq!(
            s.resolve(&tint).unwrap().into_owned(),
            PropValue::Call {
                name: "tint".into(),
                args: vec![PropValue::Color(Color::WHITE)]
            }
        );
    }

    #[test]
    fn huge_fan_out_fails_fast() {
        // Four levels, each a 64-term sum of the level below: 64^4 leaf
        // evaluations without a work budget.
        let mut t = TokenTable::default();
        t.insert("n0", PropValue::Number(1.0));
        for level in 1..=4 {
            // A balanced sum, so depth stays far below MAX_TOKEN_DEPTH.
            fn sum(path: &str, n: usize) -> TokenExpr {
                if n == 1 {
                    return TokenExpr::path(path);
                }
                TokenExpr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(sum(path, n / 2)),
                    rhs: Box::new(sum(path, n / 2)),
                }
            }
            t.insert_derived(format!("n{level}"), sum(&format!("n{}", level - 1), 64));
        }
        let start = std::time::Instant::now();
        assert!(t.lookup("n4").is_none());
        assert!(
            t.resolve(&PropValue::Token(TokenExpr::path("n4")))
                .is_none()
        );
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
        // A small fan-out still resolves.
        assert_eq!(t.lookup("n1"), Some(PropValue::Number(64.0)));
    }

    /// Every method, exactly: `alpha` sets alpha, `mix` interpolates in
    /// premultiplied OKLab (a percentage is a fraction), `lighten` and
    /// `darken` move OKLCH lightness, `oklch(from …)` does channel
    /// arithmetic; results are gamut-mapped into sRGB.
    #[test]
    fn methods_evaluate_exactly() {
        let mut t = TokenTable::default();
        let accent = Color::from_hex("#7aa2f7").unwrap();
        t.insert("accent", PropValue::Color(accent));
        t.insert("fg", PropValue::Color(Color::WHITE));
        let call = |m, args| TokenExpr::path("accent").call(m, args);
        let n = |v: f32| TokenExpr::value(PropValue::Number(v));
        let eval = |e: TokenExpr| match t.eval(&e) {
            Some(PropValue::Color(c)) => c,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            eval(call(TokenMethod::Alpha, vec![n(0.22)])),
            accent.with_alpha(0.22)
        );
        let pct = TokenExpr::value(PropValue::Length(Length::Percent(8.0)));
        assert_eq!(
            eval(call(TokenMethod::Mix, vec![TokenExpr::path("fg"), pct])),
            accent.lerp_oklab(Color::WHITE, 0.08).gamut_mapped()
        );
        // Lightness lands within the gamut mapper's just-noticeable
        // difference (ΔEOK 0.02) of the asked value.
        let l = accent.to_oklch().l;
        let lighter = eval(call(TokenMethod::Lighten, vec![n(0.1)]));
        assert!(
            (lighter.to_oklch().l - (l + 0.1)).abs() < 0.02,
            "{lighter:?}"
        );
        let darker = eval(call(TokenMethod::Darken, vec![n(0.1)]));
        assert!((darker.to_oklch().l - (l - 0.1)).abs() < 0.02, "{darker:?}");
        assert!(lighter.in_gamut(1e-6) && darker.in_gamut(1e-6));
        // Lightening past white is white, not a clipped tint.
        assert_eq!(eval(call(TokenMethod::Lighten, vec![n(2.0)])), Color::WHITE);
        // oklch(from $accent, c: c * 4): far out of gamut, mapped back
        // keeping lightness and hue.
        let vivid = eval(TokenExpr::OklchFrom {
            base: Box::new(TokenExpr::path("accent")),
            l: None,
            c: Some(Box::new(TokenExpr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(TokenExpr::Channel(Channel::C)),
                rhs: Box::new(n(4.0)),
            })),
            h: None,
            alpha: None,
        });
        assert!(vivid.in_gamut(1e-6));
        let (a, v) = (accent.to_oklch(), vivid.to_oklch());
        assert!((v.l - a.l).abs() < 0.02 && (v.h - a.h).abs() < 3.0, "{v:?}");
        assert!(v.c > a.c);
    }

    #[test]
    fn declared_pairs_keep_their_contrast_in_every_scope() {
        let mut t = TokenTable::default();
        let surface = Color::from_hex("#1e1e2e").unwrap();
        t.insert("surface", PropValue::Color(surface));
        // Too dark to read on the surface.
        t.insert("fg", PropValue::Color(Color::from_hex("#303040").unwrap()));
        t.insert_contrast("fg", vec!["surface".into()]);
        let PropValue::Color(fg) = t.lookup("fg").unwrap() else {
            panic!()
        };
        assert!(
            fg.contrast(surface) >= MIN_CONTRAST,
            "{}",
            fg.contrast(surface)
        );
        // Derived tokens see the guarded value.
        t.insert_derived(
            "fg.muted",
            TokenExpr::path("fg").call(
                TokenMethod::Alpha,
                vec![TokenExpr::value(PropValue::Number(0.65))],
            ),
        );
        assert_eq!(
            t.lookup("fg.muted"),
            Some(PropValue::Color(fg.with_alpha(0.65)))
        );
        // A subtree that lightens the surface gets darker text.
        let mut set = TokenTable::default();
        set.insert(
            "surface",
            PropValue::Color(Color::from_hex("#c0c0d0").unwrap()),
        );
        let levels = [&t, &set];
        let PropValue::Color(inner) = TokenScope::new(&levels).lookup("fg").unwrap() else {
            panic!()
        };
        assert!(inner.contrast(Color::from_hex("#c0c0d0").unwrap()) >= MIN_CONTRAST);
        // A pair that already passes is untouched.
        t.insert("fg", PropValue::Color(Color::WHITE));
        assert_eq!(t.lookup("fg"), Some(PropValue::Color(Color::WHITE)));
    }

    /// Backgrounds derived from the guarded text token itself
    /// (`override surface: $bg.mix($fg, 4%)` on all eight surfaces) are
    /// evaluated with the guard off: one level, a bounded step count, and
    /// the answer still reaches the minimum over them.
    #[test]
    fn backgrounds_derived_from_guarded_text_are_one_level() {
        let hex = |h| Color::from_hex(h).unwrap();
        let surfaces = [
            "surface",
            "surface.dim",
            "surface.bright",
            "surface.lowest",
            "surface.low",
            "surface.container",
            "surface.high",
            "surface.highest",
        ];
        let mut t = TokenTable::default();
        t.insert("bg", PropValue::Color(hex("#2a2a3a")));
        // Too dark to read on the surfaces.
        t.insert("fg", PropValue::Color(hex("#3a3a4a")));
        for (i, s) in surfaces.iter().enumerate() {
            t.insert_derived(
                *s,
                TokenExpr::path("bg").call(
                    TokenMethod::Mix,
                    vec![
                        TokenExpr::path("fg"),
                        TokenExpr::value(PropValue::Number(0.04 + i as f32 * 0.01)),
                    ],
                ),
            );
        }
        t.insert_contrast("fg", surfaces.iter().map(|s| s.to_string()).collect());
        let PropValue::Color(fg) = t.lookup("fg").unwrap() else {
            panic!()
        };
        let steps = last_token_steps();
        assert!(steps < 100, "{steps} steps");
        // Each surface lookup reads `$fg` guarded (as a node would).
        for s in surfaces {
            let PropValue::Color(bg) = t.lookup(s).unwrap() else {
                panic!()
            };
            assert!(last_token_steps() < 200, "{s}: {}", last_token_steps());
            // Solved over the backgrounds as derived from the raw `$fg`;
            // the surface as a node sees it moves by at most a few
            // percent of the text's change, still far above the minimum.
            assert!(
                fg.contrast(bg) >= MIN_CONTRAST * 0.9,
                "{s}: {}",
                fg.contrast(bg)
            );
        }
    }

    /// A light↔dark swap, frame by frame: 50 text nodes evaluate `$fg`
    /// and `$fg.muted` against eight surfaces each frame. Each frame
    /// solves the pair once (the memo), whatever the node count, and a
    /// frame stays far inside design.md's 5 ms swap budget.
    #[test]
    fn a_swap_frame_solves_each_pair_once() {
        let hex = |h| Color::from_hex(h).unwrap();
        let (light_s, dark_s) = (hex("#fbf8ff"), hex("#121318"));
        let (light_f, dark_f) = (hex("#1a1b20"), hex("#e3e1e9"));
        let surfaces = [
            "surface",
            "surface.dim",
            "surface.bright",
            "surface.lowest",
            "surface.low",
            "surface.container",
            "surface.high",
            "surface.highest",
        ];
        let frames = 60;
        let mut worst = std::time::Duration::ZERO;
        for f in 0..=frames {
            let t = f as f32 / frames as f32;
            let mut table = TokenTable::default();
            for (i, s) in surfaces.iter().enumerate() {
                let c = light_s.lerp_oklab(dark_s, t);
                table.insert(*s, PropValue::Color(shift_l(c, i as f32 * 0.01)));
            }
            table.insert("fg", PropValue::Color(light_f.lerp_oklab(dark_f, t)));
            table.insert_derived(
                "fg.muted",
                TokenExpr::path("fg").call(
                    TokenMethod::Alpha,
                    vec![TokenExpr::value(PropValue::Number(0.65))],
                ),
            );
            table.insert_contrast("fg", surfaces.iter().map(|s| s.to_string()).collect());
            let before = guard_solves();
            let start = std::time::Instant::now();
            let levels = [&table];
            for _ in 0..50 {
                let scope = TokenScope::new(&levels);
                let Some(PropValue::Color(fg)) = scope.lookup("fg") else {
                    panic!()
                };
                assert!(
                    fg.contrast(table.get("surface").and_then(color).unwrap())
                        >= MIN_CONTRAST - 1e-6
                );
                assert!(scope.lookup("fg.muted").is_some());
            }
            worst = worst.max(start.elapsed());
            assert!(
                guard_solves() - before <= 1,
                "frame {f}: {} solves",
                guard_solves() - before
            );
        }
        eprintln!("worst swap frame: {worst:?}");
        if !cfg!(debug_assertions) {
            assert!(worst < std::time::Duration::from_millis(5), "{worst:?}");
        }
    }

    #[test]
    fn snap_props_never_animate() {
        let t = table();
        let levels = [&t];
        let s = TokenScope::new(&levels);
        let spring = Transition::Spring {
            stiffness: 300.0,
            damping: 0.8,
        };
        assert_eq!(s.transition(&spring, Prop::Font), Transition::Instant);
        assert_eq!(
            s.transition(&Transition::Token("motion.spatial".into()), Prop::Text),
            Transition::Instant
        );
        assert_eq!(s.transition(&spring, Prop::Width), spring);
    }

    /// A frozen table answers every lookup of its global scope with the
    /// values it evaluated once (one step each, the same values), an
    /// override's right-hand side reads them too while derived tokens
    /// inside the override's scope still follow it, a clone is not
    /// frozen, and an `insert` thaws.
    #[test]
    fn a_frozen_table_is_read_not_evaluated() {
        let mut t = table();
        t.insert_contrast("fg", vec!["surface".into(), "surface.hi".into()]);
        let plain: Vec<(String, Option<PropValue>)> = t
            .tokens
            .keys()
            .chain(t.derived.keys())
            .map(|p| (p.clone(), t.lookup(p)))
            .collect();
        t.freeze();
        assert!(t.is_frozen());
        for (p, v) in &plain {
            assert_eq!(&t.lookup(p), v, "{p}");
            assert_eq!(last_token_steps(), 1, "{p}: read, not evaluated");
        }
        assert_eq!(t.lookup("nope"), None);
        // An override: its own expression reads the frozen global value;
        // the global derived `surface.hi` is evaluated in its scope.
        let mut over = TokenTable::default();
        over.insert(
            "surface",
            PropValue::Token(TokenExpr::path("surface").call(
                TokenMethod::Mix,
                vec![
                    TokenExpr::path("fg"),
                    TokenExpr::value(PropValue::Number(0.5)),
                ],
            )),
        );
        let thawed = t.clone();
        assert!(!thawed.is_frozen());
        for path in ["surface", "surface.hi", "fg", "border"] {
            let frozen = TokenScope::new(&[&t, &over]).lookup(path);
            let fresh = TokenScope::new(&[&thawed, &over]).lookup(path);
            assert_eq!(frozen, fresh, "{path} under the override");
        }
        t.insert("surface", PropValue::Color(Color::WHITE));
        assert!(!t.is_frozen());
        assert_eq!(t.lookup("surface"), Some(PropValue::Color(Color::WHITE)));
    }
}
