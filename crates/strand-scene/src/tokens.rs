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
use std::collections::BTreeMap;

use crate::color::{Color, Oklch};
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
    /// `border: 1, $border` or `linear(45deg, $accent, $tertiary)`. The
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
}

impl TokenTable {
    /// The plain value stored at `path` (not evaluating derived tokens).
    pub fn get(&self, path: &str) -> Option<&PropValue> {
        self.tokens.get(path)
    }

    pub fn insert(&mut self, path: impl Into<String>, value: PropValue) {
        self.tokens.insert(path.into(), value);
    }

    pub fn insert_derived(&mut self, path: impl Into<String>, expr: TokenExpr) {
        self.derived.insert(path.into(), expr);
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.derived.is_empty()
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
#[derive(Copy, Clone, Debug)]
pub struct TokenScope<'a> {
    levels: &'a [&'a TokenTable],
}

impl<'a> TokenScope<'a> {
    /// `levels[0]` is the global table, the last entry the nearest
    /// override.
    pub fn new(levels: &'a [&'a TokenTable]) -> Self {
        Self { levels }
    }

    /// Evaluates the token at `path` in this scope.
    pub fn lookup(&self, path: &str) -> Option<PropValue> {
        self.eval_ref(path, 0)
    }

    /// Resolves a prop value in this scope (see [`TokenTable::resolve`]).
    pub fn resolve<'v>(&self, v: &'v PropValue) -> Option<Cow<'v, PropValue>> {
        match v {
            PropValue::Token(e) => self.eval(e).map(Cow::Owned),
            v => Some(Cow::Borrowed(v)),
        }
    }

    /// Evaluates an expression in this scope.
    pub fn eval(&self, e: &TokenExpr) -> Option<PropValue> {
        self.eval_in(e, 0, None)
    }

    /// The concrete curve for a prop set with `t`: `Default` takes the
    /// prop class's `$motion.spatial` / `$motion.effects` token (and snaps
    /// props that cannot interpolate), `Token` takes the named token.
    /// Missing or mistyped tokens fall back to [`Transition::Instant`].
    pub fn transition(&self, t: &Transition, prop: Prop) -> Transition {
        let path = match t {
            Transition::Default => match prop.class() {
                PropClass::Spatial => "motion.spatial",
                PropClass::Effects => "motion.effects",
                PropClass::Snap => return Transition::Instant,
            },
            Transition::Token(path) => path.as_str(),
            t => return t.clone(),
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

    fn eval_ref(&self, path: &str, depth: u32) -> Option<PropValue> {
        if depth > MAX_TOKEN_DEPTH {
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
            if let Some(v) = table.tokens.get(path) {
                return match v {
                    PropValue::Token(e) => scope.eval_in(e, depth + 1, None),
                    v => Some(v.clone()),
                };
            }
            if let Some(e) = table.derived.get(path) {
                return scope.eval_in(e, depth + 1, None);
            }
        }
        None
    }

    fn eval_in(&self, e: &TokenExpr, depth: u32, base: Option<Oklch>) -> Option<PropValue> {
        if depth > MAX_TOKEN_DEPTH {
            return None;
        }
        let num = |e: &TokenExpr| self.eval_in(e, depth + 1, base).as_ref().and_then(fraction);
        let col = |e: &TokenExpr| self.eval_in(e, depth + 1, base).as_ref().and_then(color);
        match e {
            TokenExpr::Ref(path) => self.eval_ref(path, depth + 1),
            TokenExpr::Value(v) => match v.as_ref() {
                PropValue::Token(inner) => self.eval_in(inner, depth + 1, base),
                v => Some(v.clone()),
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
                            .eval_in(e, depth + 1, Some(lch))
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

/// Brings a derived colour back into sRGB. M0 clips per channel; the
/// chroma-reducing gamut mapping design asks for lands with the M2 token
/// evaluator.
fn gamut_map(c: Color) -> Color {
    c.clamped()
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
}
