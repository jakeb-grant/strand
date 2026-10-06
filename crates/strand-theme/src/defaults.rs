//! The built-in theme: what a config without a theme file (the hello
//! bar) is themed with. Its base tokens are design.md's `tokens base`
//! from `theme.strand`, word for word; its palette is
//! `material(seed: system.accent ?? #7aa2f7, dark: system.dark,
//! contrast: system.contrast)`, which the compiler evaluates.
//!
//! The instantiator writes these under every token set, so a theme that
//! defines only some base tokens still has the rest. (A family that is
//! not installed falls back to the generic sans in `strand-text`.)

use strand_scene::{BinOp, Channel, Color};
use strand_scene::{Font, PropValue, Shadow, TokenExpr, TokenMethod, TokenTable, Transition};

fn n(v: f32) -> PropValue {
    PropValue::Number(v)
}

fn font(family: &str, size: f32, weight: u16) -> PropValue {
    PropValue::Font(Font {
        family: family.into(),
        size,
        weight,
    })
}

fn spring(stiffness: f32, damping: f32) -> PropValue {
    PropValue::Transition(Transition::Spring { stiffness, damping })
}

fn alpha(path: &str, a: f32) -> TokenExpr {
    TokenExpr::path(path).call(TokenMethod::Alpha, vec![TokenExpr::value(n(a))])
}

fn mix(path: &str, other: &str, t: f32) -> TokenExpr {
    TokenExpr::path(path).call(
        TokenMethod::Mix,
        vec![TokenExpr::path(other), TokenExpr::value(n(t))],
    )
}

/// `0 2px 8px $shadow.alpha(0.25)` and friends: shadow lists whose
/// colours are token expressions.
fn shadows(layers: &[(f32, f32, f32)]) -> TokenExpr {
    let value = PropValue::Shadow(
        layers
            .iter()
            .map(|&(y, blur, _)| Shadow {
                x: 0.0,
                y,
                blur,
                spread: 0.0,
                color: Color::BLACK,
            })
            .collect(),
    );
    TokenExpr::Template {
        value: Box::new(value),
        colors: layers
            .iter()
            .map(|&(_, _, a)| Some(alpha("shadow", a)))
            .collect(),
    }
}

/// design.md's `tokens base` (scales, fonts, springs, elevations and the
/// derived roles).
pub fn base_tokens() -> TokenTable {
    let mut t = TokenTable::default();
    for (k, v) in [("1", 4.0), ("2", 8.0), ("3", 12.0), ("4", 16.0)] {
        t.insert(format!("space.{k}"), n(v));
    }
    for (k, v) in [
        ("sm", 6.0),
        ("md", 10.0),
        ("lg", 14.0),
        ("xl", 20.0),
        ("full", 999.0),
    ] {
        t.insert(format!("radius.{k}"), n(v));
    }
    t.insert("font.ui", font("Inter", 13.0, 500));
    t.insert("font.title", font("Inter", 18.0, 600));
    t.insert("font.caption", font("Inter", 11.0, 400));
    t.insert("font.mono", font("JetBrains Mono", 12.0, 400));
    t.insert("motion.spatial", spring(700.0, 0.9));
    t.insert("motion.effects", spring(1600.0, 1.0));
    t.insert("motion.bouncy", spring(380.0, 0.75));
    t.insert_derived("elevation.md", shadows(&[(2.0, 8.0, 0.25)]));
    t.insert_derived(
        "elevation.lg",
        shadows(&[(8.0, 24.0, 0.3), (1.0, 2.0, 0.2)]),
    );
    t.insert_derived("elevation.xl", shadows(&[(16.0, 48.0, 0.4)]));
    t.insert_derived("surface.hi", mix("surface", "fg", 0.08));
    t.insert_derived("fg.muted", alpha("fg", 0.65));
    t.insert_derived("fg.faint", alpha("fg", 0.35));
    t.insert_derived("accent.hover", mix("accent", "on_accent", 0.08));
    t.insert_derived("accent.container", alpha("accent", 0.22));
    t.insert_derived(
        "border",
        TokenExpr::OklchFrom {
            base: Box::new(TokenExpr::path("surface")),
            l: Some(Box::new(TokenExpr::Binary {
                op: BinOp::Add,
                lhs: Box::new(TokenExpr::Channel(Channel::L)),
                rhs: Box::new(TokenExpr::value(n(0.12))),
            })),
            c: None,
            h: None,
            alpha: None,
        },
    );
    let paths: Vec<String> = t.tokens.keys().chain(t.derived.keys()).cloned().collect();
    for p in paths {
        t.set_origin(p, "base");
    }
    t
}

/// The seed of the built-in palette when the desktop sets no accent.
pub const DEFAULT_SEED: &str = crate::palette::DEFAULT_ACCENT;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::{Options, from_seed};

    #[test]
    fn the_built_in_theme_resolves_every_base_token() {
        let mut t = base_tokens();
        from_seed(Color::from_hex(DEFAULT_SEED).unwrap(), Options::default()).insert_into(&mut t);
        for path in [
            "space.2",
            "radius.lg",
            "font.ui",
            "motion.spatial",
            "elevation.lg",
            "surface.hi",
            "fg.muted",
            "fg.faint",
            "accent.hover",
            "accent.container",
            "border",
        ] {
            assert!(t.lookup(path).is_some(), "{path}");
        }
        let Some(PropValue::Shadow(s)) = t.lookup("elevation.lg") else {
            panic!()
        };
        assert_eq!(s.len(), 2);
        assert!((s[0].color.a - 0.3).abs() < 1e-6);
    }
}
