//! (M5) Token provenance for the inspector: where a token's value comes
//! from, step by step (design.md, "Overrides": `bg ← surface.hi ← base ←
//! palette:wallpaper`).
//!
//! [`TokenTable::provenance`] walks a token's definition the way
//! [`TokenScope`] evaluates it: the nearest table of the scope that
//! defines the path, an override's right-hand side in its parent scope, a
//! global derived token in the scope of whoever asks. Each definition is
//! a step; the walk follows the first token the definition reads (a
//! method's receiver, `oklch(from …)`'s base, an arithmetic's left side)
//! and lists the others in [`ProvenanceStep::reads`]. A runtime overlay's
//! token ([`OVERLAY_ORIGIN`]) is an overlay step, and a declared text
//! token gets a contrast-guard step after its definition. Read only:
//! nothing here is on the render path.

use std::borrow::Cow;

use crate::protocol::PropValue;
use crate::tokens::{MAX_TOKEN_DEPTH, OVERLAY_ORIGIN, TokenExpr, TokenScope, TokenTable};

/// What defined a [`ProvenanceStep`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// A `set { }` override on an ancestor: the override level, 1 for the
    /// outermost (the scope's index; 0 is the global table).
    Set(u32),
    /// The global table: a palette, a token set, a component's `tokens {
    /// }` (`$Toast.radius`) or the base tokens; the step's `origin` says
    /// which (`palette:wallpaper`, `tokens base`, `component Toast`).
    Global,
    /// A runtime overlay (`strand set '$accent' …`): the global table
    /// records [`OVERLAY_ORIGIN`] for it while the overlay holds.
    Overlay,
    /// The contrast guard: the token is a declared text colour, its
    /// lightness solved against the backgrounds in `reads`. The step's
    /// value is the guarded colour; the step before holds the colour as
    /// defined, so a difference is the guard's tint.
    Contrast,
}

impl StepKind {
    /// The kind as the inspector's dump spells it.
    pub fn name(self) -> &'static str {
        match self {
            StepKind::Set(_) => "set",
            StepKind::Global => "global",
            StepKind::Overlay => "overlay",
            StepKind::Contrast => "contrast",
        }
    }
}

/// One step of a token's provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ProvenanceStep {
    /// The token path, without the `$`.
    pub path: String,
    pub kind: StepKind,
    /// [`TokenTable::origin`] of the table that defined it (`None` for an
    /// override, whose table records none, and for a contrast step).
    pub origin: Option<String>,
    /// The value at this step, evaluated in the step's scope; `None` if it
    /// does not resolve.
    pub value: Option<PropValue>,
    /// The other tokens the definition reads (the chain follows the first
    /// it reads, which is not listed); a contrast step's backgrounds.
    pub reads: Vec<String>,
}

impl TokenTable {
    /// Where the token at `path` gets its value, nearest step first, with
    /// `self` as the global table and `scope` the `set { }` overrides of
    /// the node's ancestors, outermost first (the levels of a
    /// [`TokenScope`] after the global one). Empty if no table defines
    /// `path`. A cycle or a chain deeper than [`MAX_TOKEN_DEPTH`] stops
    /// the walk.
    pub fn provenance(&self, path: &str, scope: &[&TokenTable]) -> Vec<ProvenanceStep> {
        let mut levels: Vec<&TokenTable> = Vec::with_capacity(scope.len() + 1);
        levels.push(self);
        levels.extend_from_slice(scope);
        let mut out = Vec::new();
        let mut seen: Vec<(String, usize)> = Vec::new();
        let mut path = path.to_string();
        let mut len = levels.len();
        while out.len() < MAX_TOKEN_DEPTH as usize {
            let cur = &levels[..len];
            let Some(i) = cur
                .iter()
                .rposition(|t| t.tokens.contains_key(&path) || t.derived.contains_key(&path))
            else {
                break;
            };
            if seen.iter().any(|(p, l)| *p == path && *l == len) {
                break;
            }
            seen.push((path.clone(), len));
            let table = cur[i];
            // Overrides see their parent scope; global derived tokens see
            // the scope of whoever asks (as `TokenScope` evaluates them).
            let def = if i == 0 { cur } else { &cur[..i] };
            let at = TokenScope::new(def);
            let (value, mut reads) = match table.tokens.get(&path) {
                Some(v) => (at.resolve(v).map(Cow::into_owned), value_reads(v)),
                None => match table.derived.get(&path) {
                    Some(e) => (at.eval(e), expr_reads(e)),
                    None => break,
                },
            };
            let origin = table.origin(&path).map(str::to_string);
            let kind = if i > 0 {
                StepKind::Set(i as u32)
            } else if origin.as_deref() == Some(OVERLAY_ORIGIN) {
                StepKind::Overlay
            } else {
                StepKind::Global
            };
            let next = (!reads.is_empty()).then(|| reads.remove(0));
            let colour = matches!(value, Some(PropValue::Color(_)));
            out.push(ProvenanceStep {
                path: path.clone(),
                kind,
                origin,
                value,
                reads,
            });
            if colour && let Some(bgs) = levels[0].contrast.get(&path) {
                out.push(ProvenanceStep {
                    path: path.clone(),
                    kind: StepKind::Contrast,
                    origin: None,
                    value: TokenScope::new(cur).lookup(&path),
                    reads: bgs.clone(),
                });
            }
            match next {
                Some(n) => {
                    path = n;
                    len = def.len();
                }
                None => break,
            }
        }
        out
    }
}

/// Every token path `v` reads, in reading order (a method's receiver
/// before its arguments), duplicates kept out: the first is the one a
/// provenance chain follows.
pub fn value_reads(v: &PropValue) -> Vec<String> {
    let mut out = Vec::new();
    collect_value(v, &mut out);
    out
}

/// [`value_reads`] for an expression.
pub fn expr_reads(e: &TokenExpr) -> Vec<String> {
    let mut out = Vec::new();
    collect_expr(e, &mut out);
    out
}

fn collect_value(v: &PropValue, out: &mut Vec<String>) {
    if !v.has_tokens() {
        return;
    }
    match v {
        PropValue::Token(e) => collect_expr(e, out),
        PropValue::List(items) | PropValue::Call { args: items, .. } => {
            items.iter().for_each(|i| collect_value(i, out))
        }
        PropValue::Pose(props) => props.iter().for_each(|(_, v)| collect_value(v, out)),
        PropValue::Uniforms(entries) => entries.iter().for_each(|(_, v)| collect_value(v, out)),
        _ => {}
    }
}

fn collect_expr(e: &TokenExpr, out: &mut Vec<String>) {
    match e {
        TokenExpr::Ref(p) => {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
        TokenExpr::Value(v) => collect_value(v, out),
        TokenExpr::Method { receiver, args, .. } => {
            collect_expr(receiver, out);
            args.iter().for_each(|a| collect_expr(a, out));
        }
        TokenExpr::OklchFrom {
            base,
            l,
            c,
            h,
            alpha,
        } => {
            collect_expr(base, out);
            [l, c, h, alpha]
                .into_iter()
                .flatten()
                .for_each(|e| collect_expr(e, out));
        }
        TokenExpr::Binary { lhs, rhs, .. } => {
            collect_expr(lhs, out);
            collect_expr(rhs, out);
        }
        TokenExpr::Template {
            value,
            colors,
            numbers,
        } => {
            collect_value(value, out);
            colors
                .iter()
                .chain(numbers)
                .flatten()
                .for_each(|e| collect_expr(e, out));
        }
        TokenExpr::Wave { phase, .. } => collect_expr(phase, out),
        TokenExpr::Noise(x) => collect_expr(x, out),
        TokenExpr::Math { args, .. } => args.iter().for_each(|e| collect_expr(e, out)),
        TokenExpr::Time | TokenExpr::Index | TokenExpr::Count | TokenExpr::Channel(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::protocol::Length;
    use crate::tokens::TokenMethod;

    fn hex(s: &str) -> PropValue {
        PropValue::Color(Color::from_hex(s).unwrap())
    }

    /// design.md's tiers: a palette root, a base token over it, a set's
    /// token over that, and a component's `tokens { }` over the set's.
    fn global() -> TokenTable {
        let mut t = TokenTable::default();
        t.insert("surface", hex("#1e1e2e"));
        t.set_origin("surface", "palette:wallpaper");
        t.insert("fg", hex("#cdd6f4"));
        t.set_origin("fg", "palette:wallpaper");
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
        t.set_origin("surface.hi", "tokens base");
        t.insert_derived("card", TokenExpr::path("surface.hi"));
        t.set_origin("card", "tokens compact");
        t.insert_derived(
            "Toast.bg",
            TokenExpr::path("card").call(
                TokenMethod::Alpha,
                vec![TokenExpr::value(PropValue::Number(0.9))],
            ),
        );
        t.set_origin("Toast.bg", "component Toast");
        t
    }

    fn chain(steps: &[ProvenanceStep]) -> Vec<(&str, StepKind, Option<&str>)> {
        steps
            .iter()
            .map(|s| (s.path.as_str(), s.kind, s.origin.as_deref()))
            .collect()
    }

    #[test]
    fn a_four_step_chain_reaches_the_palette() {
        let t = global();
        let steps = t.provenance("Toast.bg", &[]);
        assert_eq!(
            chain(&steps),
            [
                ("Toast.bg", StepKind::Global, Some("component Toast")),
                ("card", StepKind::Global, Some("tokens compact")),
                ("surface.hi", StepKind::Global, Some("tokens base")),
                ("surface", StepKind::Global, Some("palette:wallpaper")),
            ]
        );
        // The mix's other operand is listed, not followed.
        assert_eq!(steps[2].reads, ["fg"]);
        // Each step's value is the token's.
        for s in &steps {
            assert_eq!(s.value, t.lookup(&s.path), "{}", s.path);
        }
        assert!(t.provenance("nope", &[]).is_empty());
    }

    #[test]
    fn a_set_step_reads_the_inherited_value() {
        let t = global();
        // `set { $surface: $surface.alpha(0.5) }` on an ancestor: the
        // right-hand `$surface` is the inherited one.
        let mut set = TokenTable::default();
        set.insert_derived(
            "surface",
            TokenExpr::path("surface").call(
                TokenMethod::Alpha,
                vec![TokenExpr::value(PropValue::Number(0.5))],
            ),
        );
        let steps = t.provenance("surface.hi", &[&set]);
        assert_eq!(
            chain(&steps),
            [
                ("surface.hi", StepKind::Global, Some("tokens base")),
                ("surface", StepKind::Set(1), None),
                ("surface", StepKind::Global, Some("palette:wallpaper")),
            ]
        );
        let PropValue::Color(c) = steps[1].value.clone().unwrap() else {
            panic!("{steps:?}")
        };
        assert_eq!(c.a, 0.5);
        // The derived token follows the override (it is evaluated in the
        // asker's scope), as render evaluates it.
        assert_eq!(
            steps[0].value,
            TokenScope::new(&[&t, &set]).lookup("surface.hi")
        );
        assert_ne!(steps[0].value, t.lookup("surface.hi"));
    }

    #[test]
    fn an_overlaid_token_is_an_overlay_step() {
        let mut t = global();
        t.insert("surface", hex("#ff0000"));
        t.set_origin("surface", OVERLAY_ORIGIN);
        let steps = t.provenance("surface.hi", &[]);
        assert_eq!(
            chain(&steps),
            [
                ("surface.hi", StepKind::Global, Some("tokens base")),
                ("surface", StepKind::Overlay, Some("overlay")),
            ]
        );
        assert_eq!(steps[1].value, Some(hex("#ff0000")));
    }

    #[test]
    fn a_guarded_text_token_gets_a_contrast_step() {
        let mut t = global();
        // Text too dark for its background: the guard lifts it.
        t.insert("fg", hex("#202030"));
        t.insert_contrast("fg", vec!["surface".into()]);
        let steps = t.provenance("fg", &[]);
        assert_eq!(
            chain(&steps),
            [
                ("fg", StepKind::Global, Some("palette:wallpaper")),
                ("fg", StepKind::Contrast, None),
            ]
        );
        assert_eq!(steps[0].value, Some(hex("#202030")), "as defined");
        assert_eq!(steps[1].value, t.lookup("fg"), "as guarded");
        assert_ne!(steps[0].value, steps[1].value, "the guard moved it");
        assert_eq!(steps[1].reads, ["surface"]);
    }

    #[test]
    fn a_cycle_stops_the_walk() {
        let mut t = TokenTable::default();
        t.insert_derived("a", TokenExpr::path("b"));
        t.insert_derived("b", TokenExpr::path("a"));
        let steps = t.provenance("a", &[]);
        assert_eq!(steps.len(), 2, "{steps:?}");
    }

    #[test]
    fn reads_follow_reading_order() {
        let e = TokenExpr::path("surface").call(
            TokenMethod::Mix,
            vec![TokenExpr::path("fg"), TokenExpr::path("surface")],
        );
        assert_eq!(expr_reads(&e), ["surface", "fg"]);
        let v = PropValue::List(vec![
            PropValue::Number(0.0),
            PropValue::Token(TokenExpr::path("space.3")),
        ]);
        assert_eq!(value_reads(&v), ["space.3"]);
    }
}
