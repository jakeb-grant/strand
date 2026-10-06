//! The contrast guard: declared text/background pairs keep at least
//! 3:1 (WCAG 2 contrast ratio) by solving the text's OKLCH lightness
//! (design.md, "How a swap animates", step 3).
//!
//! The pairs are Material 3's own: every `on_X` over its `X`, `fg` over
//! every surface, `fg_variant` over the surface and its variant, and the
//! inverse roles. [`guard`] applies them to a palette when it is made;
//! [`crate::Palette::insert_into`] also hands them to the render thread
//! (`TokenTable::contrast`), which solves them again wherever a text token
//! is evaluated, so a palette mid-spring and `set { }` overrides stay
//! readable too.

use strand_scene::Color;
pub use strand_scene::MIN_CONTRAST;

use crate::palette::Palette;
use crate::role::Role;

/// Text roles and the background roles each is drawn on.
pub const PAIRS: &[(Role, &[Role])] = &[
    (Role::OnAccent, &[Role::Accent]),
    (Role::OnAccentContainer, &[Role::AccentContainer]),
    (Role::OnSecondary, &[Role::Secondary]),
    (Role::OnSecondaryContainer, &[Role::SecondaryContainer]),
    (Role::OnTertiary, &[Role::Tertiary]),
    (Role::OnTertiaryContainer, &[Role::TertiaryContainer]),
    (Role::OnError, &[Role::Error]),
    (Role::OnErrorContainer, &[Role::ErrorContainer]),
    (Role::OnBg, &[Role::Bg]),
    (
        Role::Fg,
        &[
            Role::Surface,
            Role::SurfaceDim,
            Role::SurfaceBright,
            Role::SurfaceLowest,
            Role::SurfaceLow,
            Role::SurfaceContainer,
            Role::SurfaceHigh,
            Role::SurfaceHighest,
        ],
    ),
    (Role::FgVariant, &[Role::Surface, Role::SurfaceVariant]),
    (Role::InverseFg, &[Role::InverseSurface]),
    (Role::InverseAccent, &[Role::InverseSurface]),
];

/// The WCAG 2 contrast ratio of `text` over `bg`.
pub fn ratio(text: Color, bg: Color) -> f64 {
    text.contrast(bg)
}

/// `text`, its lightness solved to keep `min` over every one of `bgs`.
pub fn solve(text: Color, bgs: &[Color], min: f64) -> Color {
    text.with_contrast(bgs, min)
}

/// Applies every pair of [`PAIRS`] to `palette`. Only text roles change.
pub fn guard(palette: &mut Palette) {
    for (text, bgs) in PAIRS {
        let bgs: Vec<Color> = bgs.iter().map(|b| palette.get(*b)).collect();
        let solved = solve(palette.get(*text), &bgs, MIN_CONTRAST);
        palette.set(*text, solved);
    }
}

/// The lowest ratio over every pair of `palette` (≥ 3 after [`guard`]).
pub fn worst(palette: &Palette) -> f64 {
    PAIRS
        .iter()
        .flat_map(|(t, bgs)| bgs.iter().map(move |b| (*t, *b)))
        .map(|(t, b)| ratio(palette.get(t), palette.get(b)))
        .fold(f64::INFINITY, f64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::Partial;
    use proptest::prelude::*;

    #[test]
    fn no_role_is_both_text_and_background() {
        for (t, _) in PAIRS {
            assert!(PAIRS.iter().all(|(_, bgs)| !bgs.contains(t)), "{t}");
        }
    }

    #[test]
    fn an_unreadable_pair_is_fixed_by_moving_the_text() {
        let mut p = Partial::new()
            .with(Role::Surface, Color::from_hex("#202020").unwrap())
            .with(Role::Fg, Color::from_hex("#383838").unwrap())
            .fill();
        assert!(ratio(p.get(Role::Fg), p.get(Role::Surface)) >= MIN_CONTRAST);
        // The text got lighter (away from the dark surface), the surface
        // stayed.
        assert!(p.get(Role::Fg).to_oklch().l > 0.4);
        assert_eq!(p.get(Role::Surface), Color::from_hex("#202020").unwrap());
        guard(&mut p);
        assert!(worst(&p) >= MIN_CONTRAST);
    }

    fn color() -> impl Strategy<Value = Color> {
        (0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0).prop_map(|(r, g, b)| Color::rgb(r, g, b))
    }

    /// Whether some lightness of `text` (hue and chroma kept) reaches
    /// `min` over every one of `bgs`.
    fn feasible(text: Color, bgs: &[Color]) -> bool {
        let lch = text.to_oklch();
        (0..=400).any(|i| {
            let c = Color::from_oklch(strand_scene::Oklch {
                l: i as f64 / 400.0,
                ..lch
            })
            .gamut_mapped();
            bgs.iter().all(|b| c.contrast(*b) >= MIN_CONTRAST)
        })
    }

    proptest! {
        /// Random palettes, every role random, then guarded: a pair that
        /// can reach 3:1 at all does.
        #[test]
        fn every_pair_that_can_reach_three_to_one_does(colors in proptest::collection::vec(color(), Role::COUNT)) {
            let raw = Palette::from_fn(false, |r| colors[r.index()]);
            let mut p = raw.clone();
            guard(&mut p);
            for (t, bgs) in PAIRS {
                let bg: Vec<Color> = bgs.iter().map(|b| p.get(*b)).collect();
                if feasible(raw.get(*t), &bg) {
                    for b in &bg {
                        prop_assert!(ratio(p.get(*t), *b) >= MIN_CONTRAST, "{t}: {}", ratio(p.get(*t), *b));
                    }
                }
            }
            // A pair with one background always can.
            for (t, bgs) in PAIRS.iter().filter(|(_, b)| b.len() == 1) {
                prop_assert!(ratio(p.get(*t), p.get(bgs[0])) >= MIN_CONTRAST, "{t}");
            }
        }

        /// Random partial palettes, light or dark, filled by the
        /// derivation table: every pair is readable.
        #[test]
        fn derived_palettes_are_readable(
            dark in any::<bool>(),
            l in 0.0f64..0.38,
            c in 0.0f64..0.1,
            h in 0.0f64..360.0,
            fg in proptest::option::of(color()),
            accent in proptest::option::of(color()),
        ) {
            let l = if dark { 0.05 + l } else { 1.0 - l * 0.6 };
            let surface = Color::from_oklch(strand_scene::Oklch { l, c, h, alpha: 1.0 }).gamut_mapped();
            let mut part = Partial::new().with(Role::Surface, surface);
            if let Some(f) = fg { part.set(Role::Fg, f); }
            if let Some(a) = accent { part.set(Role::Accent, a); }
            let p = part.fill();
            prop_assert!(worst(&p) >= MIN_CONTRAST, "worst {}", worst(&p));
        }
    }
}
