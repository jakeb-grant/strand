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
use std::collections::BTreeMap;

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
    Template {
        value: Box<PropValue>,
        colors: Vec<Option<TokenExpr>>,
    },
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
        self.derived.remove(&path);
        self.tokens.insert(path, value);
    }

    /// Sets a derived token at `path`, replacing a plain one there.
    pub fn insert_derived(&mut self, path: impl Into<String>, expr: TokenExpr) {
        let path = path.into();
        self.tokens.remove(&path);
        self.derived.insert(path, expr);
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.derived.is_empty()
    }

    /// Declare that `text` is drawn on `bgs` (see [`TokenTable::contrast`]).
    pub fn insert_contrast(&mut self, text: impl Into<String>, bgs: Vec<String>) {
        self.contrast.insert(text.into(), bgs);
    }

    /// Evaluates the token at `path`, plain or derived.
    pub fn lookup(&self, path: &str) -> Option<PropValue> {
        TokenScope::new(&[self]).lookup(path)
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
        Self { levels }
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
            v => v.clone(),
        }))
    }

    fn eval_ref(&self, path: &str, depth: u32, budget: &Budget) -> Option<PropValue> {
        if depth > MAX_TOKEN_DEPTH || !budget.step() {
            return None;
        }
        for (i, table) in self.levels.iter().enumerate().rev() {
            // Overrides see their parent scope; global derived tokens see
            // the scope of whoever asks.
            let scope = if i == 0 {
                *self
            } else {
                TokenScope::new(&self.levels[..i])
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
            TokenExpr::Template { value, colors } => {
                let mut v = (**value).clone();
                for (slot, expr) in v.colors_mut().into_iter().zip(colors) {
                    if let Some(e) = expr {
                        *slot = col(e)?;
                    }
                }
                Some(v)
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
    use crate::protocol::{Border, Shadow};

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
        });
        let PropValue::Shadow(s) = t.resolve(&shadows).unwrap().into_owned() else {
            panic!()
        };
        assert_eq!((s[0].color, s[1].color), (Color::TRANSPARENT, Color::WHITE));
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
        // No $motion.effects token, fonts snap, unknown tokens snap.
        assert_eq!(
            s.transition(&Transition::Default, Prop::Bg),
            Transition::Instant
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
}
