//! `material(seed:, variant:, dark:, contrast:)`: a Material 3 dynamic
//! scheme from a seed colour, by the `material-colors` crate.
//!
//! The Material spec version is pinned to [`SPEC`] (2021, the version
//! material-color-utilities, matugen and Android 12–14 schemes use), so a
//! `material-colors` upgrade that changes its default cannot recolour a
//! theme silently.

use material_colors::color::Rgb;
use material_colors::dynamic_color::{DynamicScheme, Platform, SpecVersion, Variant as McVariant};
use material_colors::hct::Hct;
use strand_scene::Color;

use crate::contrast;
use crate::palette::Palette;
use crate::role::Role;

/// The pinned Material colour spec.
pub const SPEC: SpecVersion = SpecVersion::Spec2021;
/// The spec's name, as recorded in caches and `docs/decisions.md`.
pub const SPEC_NAME: &str = "material-colors 0.5 / spec 2021";

/// The scheme variants (the schema's `Variant` enum, in its order).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Variant {
    #[default]
    TonalSpot,
    Vibrant,
    Expressive,
    Neutral,
    Monochrome,
    Fidelity,
    Content,
    Rainbow,
    FruitSalad,
}

impl Variant {
    pub const ALL: &'static [Variant] = &[
        Variant::TonalSpot,
        Variant::Vibrant,
        Variant::Expressive,
        Variant::Neutral,
        Variant::Monochrome,
        Variant::Fidelity,
        Variant::Content,
        Variant::Rainbow,
        Variant::FruitSalad,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Variant::TonalSpot => "tonal_spot",
            Variant::Vibrant => "vibrant",
            Variant::Expressive => "expressive",
            Variant::Neutral => "neutral",
            Variant::Monochrome => "monochrome",
            Variant::Fidelity => "fidelity",
            Variant::Content => "content",
            Variant::Rainbow => "rainbow",
            Variant::FruitSalad => "fruit_salad",
        }
    }

    pub fn from_name(name: &str) -> Option<Variant> {
        Variant::ALL.iter().copied().find(|v| v.name() == name)
    }

    fn mc(self) -> McVariant {
        match self {
            Variant::TonalSpot => McVariant::TonalSpot,
            Variant::Vibrant => McVariant::Vibrant,
            Variant::Expressive => McVariant::Expressive,
            Variant::Neutral => McVariant::Neutral,
            Variant::Monochrome => McVariant::Monochrome,
            Variant::Fidelity => McVariant::Fidelity,
            Variant::Content => McVariant::Content,
            Variant::Rainbow => McVariant::Rainbow,
            Variant::FruitSalad => McVariant::FruitSalad,
        }
    }
}

/// What `material(…)` was asked for, besides its source.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Options {
    pub variant: Variant,
    pub dark: bool,
    /// −1 (least) to 1 (most); 0 is the standard contrast.
    pub contrast: f64,
}

pub(crate) fn to_rgb(c: Color) -> Rgb {
    let [r, g, b, _] = c.to_rgba8();
    Rgb::new(r, g, b)
}

pub(crate) fn from_rgb(c: Rgb) -> Color {
    Color::from_rgba8(c.red, c.green, c.blue, 255)
}

/// The Material 3 palette for `seed`. Every role is filled straight
/// from the dynamic scheme (the 1:1 role table), then guarded.
pub fn from_seed(seed: Color, opts: Options) -> Palette {
    let contrast = if opts.contrast.is_finite() {
        opts.contrast.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let source: Hct = to_rgb(seed).into();
    let s = DynamicScheme::from_spec(
        source,
        opts.variant.mc(),
        opts.dark,
        Some(contrast),
        Platform::Phone,
        SPEC,
    );
    let mut p = Palette::from_fn(opts.dark, |r| from_rgb(role(&s, r)));
    contrast::guard(&mut p);
    p.with_source("material(seed)")
}

fn role(s: &DynamicScheme, r: Role) -> Rgb {
    match r {
        Role::Accent => s.primary(),
        Role::OnAccent => s.on_primary(),
        Role::AccentContainer => s.primary_container(),
        Role::OnAccentContainer => s.on_primary_container(),
        Role::Secondary => s.secondary(),
        Role::OnSecondary => s.on_secondary(),
        Role::SecondaryContainer => s.secondary_container(),
        Role::OnSecondaryContainer => s.on_secondary_container(),
        Role::Tertiary => s.tertiary(),
        Role::OnTertiary => s.on_tertiary(),
        Role::TertiaryContainer => s.tertiary_container(),
        Role::OnTertiaryContainer => s.on_tertiary_container(),
        Role::Error => s.error(),
        Role::OnError => s.on_error(),
        Role::ErrorContainer => s.error_container(),
        Role::OnErrorContainer => s.on_error_container(),
        Role::Bg => s.background(),
        Role::OnBg => s.on_background(),
        Role::Surface => s.surface(),
        Role::Fg => s.on_surface(),
        Role::SurfaceVariant => s.surface_variant(),
        Role::FgVariant => s.on_surface_variant(),
        Role::SurfaceDim => s.surface_dim(),
        Role::SurfaceBright => s.surface_bright(),
        Role::SurfaceLowest => s.surface_container_lowest(),
        Role::SurfaceLow => s.surface_container_low(),
        Role::SurfaceContainer => s.surface_container(),
        Role::SurfaceHigh => s.surface_container_high(),
        Role::SurfaceHighest => s.surface_container_highest(),
        Role::InverseSurface => s.inverse_surface(),
        Role::InverseFg => s.inverse_on_surface(),
        Role::InverseAccent => s.inverse_primary(),
        Role::Outline => s.outline(),
        Role::OutlineVariant => s.outline_variant(),
        Role::Shadow => s.shadow(),
        Role::Scrim => s.scrim(),
        Role::SurfaceTint => s.surface_tint(),
        Role::AccentFixed => s.primary_fixed(),
        Role::AccentFixedDim => s.primary_fixed_dim(),
        Role::OnAccentFixed => s.on_primary_fixed(),
        Role::OnAccentFixedVariant => s.on_primary_fixed_variant(),
        Role::SecondaryFixed => s.secondary_fixed(),
        Role::SecondaryFixedDim => s.secondary_fixed_dim(),
        Role::OnSecondaryFixed => s.on_secondary_fixed(),
        Role::OnSecondaryFixedVariant => s.on_secondary_fixed_variant(),
        Role::TertiaryFixed => s.tertiary_fixed(),
        Role::TertiaryFixedDim => s.tertiary_fixed_dim(),
        Role::OnTertiaryFixed => s.on_tertiary_fixed(),
        Role::OnTertiaryFixedVariant => s.on_tertiary_fixed_variant(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Color {
        Color::from_hex(s).unwrap()
    }

    #[test]
    fn variant_names_match_the_schema_order() {
        let names: Vec<_> = Variant::ALL.iter().map(|v| v.name()).collect();
        assert_eq!(
            names,
            [
                "tonal_spot",
                "vibrant",
                "expressive",
                "neutral",
                "monochrome",
                "fidelity",
                "content",
                "rainbow",
                "fruit_salad"
            ]
        );
    }

    #[test]
    fn roles_sit_at_their_material_tones() {
        // HCT tone is CIE L*: primary 40 light / 80 dark, surface 98 / 6.
        let lstar = |c: Color| to_rgb(c).as_lstar();
        let seed = hex("#7aa2f7");
        let light = from_seed(seed, Options::default());
        let dark = from_seed(
            seed,
            Options {
                dark: true,
                ..Options::default()
            },
        );
        let near = |a: f64, b: f64| (a - b).abs() < 1.0;
        assert!(near(lstar(light.get(Role::Accent)), 40.0));
        assert!(near(lstar(dark.get(Role::Accent)), 80.0));
        assert!(near(lstar(light.get(Role::Surface)), 98.0));
        assert!(near(lstar(dark.get(Role::Surface)), 6.0));
        assert!(near(lstar(light.get(Role::OnAccent)), 100.0));
        assert!(near(lstar(dark.get(Role::OnAccent)), 20.0));
        assert!(dark.is_dark() && !light.is_dark());
        // The fixed accents keep their tones in both: 90, 80, 10, 30.
        for p in [&light, &dark] {
            assert!(near(lstar(p.get(Role::AccentFixed)), 90.0));
            assert!(near(lstar(p.get(Role::TertiaryFixedDim)), 80.0));
            assert!(near(lstar(p.get(Role::OnSecondaryFixed)), 10.0));
            assert!(near(lstar(p.get(Role::OnAccentFixedVariant)), 30.0));
        }
    }
}
