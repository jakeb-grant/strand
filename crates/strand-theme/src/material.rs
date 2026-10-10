//! `material(seed:, variant:, dark:, contrast:)`: a Material 3 dynamic
//! scheme from a seed colour, by the `material-colors` crate.
//!
//! The Material spec version is pinned to [`SPEC`] (2021, the version
//! material-color-utilities, matugen and Android 12–14 schemes use), so a
//! `material-colors` upgrade that changes its default cannot recolour a
//! theme silently.

use material_colors::color::Rgb;
use material_colors::dynamic_color::{
    DynamicScheme, Platform, Role as McRole, SpecVersion, Variant as McVariant,
};
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

/// Palettes [`from_seed`] keeps (the oldest replaced first): a swap back
/// to a look shown before (light and dark, `auto` following the system)
/// reuses its palette instead of solving the scheme again. A fixed-size
/// cache of a pure function: a miss solves it as before.
const SEED_MEMO: usize = 8;

/// What a palette from [`from_seed`] depends on: the seed as Material
/// reads it (8-bit RGB), the variant, the mode and the clamped contrast.
type SeedKey = ([u8; 3], Variant, bool, u64);

thread_local! {
    /// The palettes kept, and the slot the next one replaces (oldest
    /// first).
    static SEEDS: std::cell::RefCell<(Vec<(SeedKey, Palette)>, usize)> =
        const { std::cell::RefCell::new((Vec::new(), 0)) };
}

/// The Material 3 palette for `seed`. Every role is filled straight
/// from the dynamic scheme (the 1:1 role table), then guarded.
pub fn from_seed(seed: Color, opts: Options) -> Palette {
    let contrast = if opts.contrast.is_finite() {
        opts.contrast.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let rgb = to_rgb(seed);
    let key: SeedKey = (
        [rgb.red, rgb.green, rgb.blue],
        opts.variant,
        opts.dark,
        contrast.to_bits(),
    );
    let kept = SEEDS.with(|m| {
        let m = m.borrow();
        m.0.iter().find(|(k, _)| *k == key).map(|(_, p)| p.clone())
    });
    if let Some(p) = kept {
        return p;
    }
    let p = solve_seed(rgb, opts.variant, opts.dark, contrast);
    SEEDS.with(|m| {
        let (kept, next) = &mut *m.borrow_mut();
        if kept.len() < SEED_MEMO {
            kept.push((key, p.clone()));
        } else {
            kept[*next] = (key, p.clone());
            *next = (*next + 1) % SEED_MEMO;
        }
    });
    p
}

/// Drops the palettes [`from_seed`] keeps on this thread, so the next
/// call for any seed solves the scheme again. For the timing benches
/// (theme_swap_bench, strand-compiler's theme swap): a gate measures a
/// swap to a seed not shown before (a new wallpaper, a changed seed),
/// the solve's worst case, not a palette kept from the swap before
/// (decisions.md, m4-owner-swap).
#[doc(hidden)]
pub fn forget_kept_palettes() {
    SEEDS.with(|m| {
        let (kept, next) = &mut *m.borrow_mut();
        kept.clear();
        *next = 0;
    });
}

/// [`from_seed`] without the memo.
fn solve_seed(rgb: Rgb, variant: Variant, dark: bool, contrast: f64) -> Palette {
    let source: Hct = rgb.into();
    let s = DynamicScheme::from_spec(
        source,
        variant.mc(),
        dark,
        Some(contrast),
        Platform::Phone,
        SPEC,
    );
    // One resolver for every role: the roles share its tone memo (each
    // scheme getter would solve the roles it depends on again), with
    // the same colours (`one_resolver_gives_every_getters_colour`).
    let resolver = s.resolver();
    let mut p = Palette::from_fn(dark, |r| from_rgb(resolver.rgb(mc_role(r).color())));
    contrast::guard(&mut p);
    p.with_source("material(seed)")
}

/// The `material-colors` role each palette role is filled from (the
/// 1:1 role table).
fn mc_role(r: Role) -> McRole {
    match r {
        Role::Accent => McRole::Primary,
        Role::OnAccent => McRole::OnPrimary,
        Role::AccentContainer => McRole::PrimaryContainer,
        Role::OnAccentContainer => McRole::OnPrimaryContainer,
        Role::Secondary => McRole::Secondary,
        Role::OnSecondary => McRole::OnSecondary,
        Role::SecondaryContainer => McRole::SecondaryContainer,
        Role::OnSecondaryContainer => McRole::OnSecondaryContainer,
        Role::Tertiary => McRole::Tertiary,
        Role::OnTertiary => McRole::OnTertiary,
        Role::TertiaryContainer => McRole::TertiaryContainer,
        Role::OnTertiaryContainer => McRole::OnTertiaryContainer,
        Role::Error => McRole::Error,
        Role::OnError => McRole::OnError,
        Role::ErrorContainer => McRole::ErrorContainer,
        Role::OnErrorContainer => McRole::OnErrorContainer,
        Role::Bg => McRole::Background,
        Role::OnBg => McRole::OnBackground,
        Role::Surface => McRole::Surface,
        Role::Fg => McRole::OnSurface,
        Role::SurfaceVariant => McRole::SurfaceVariant,
        Role::FgVariant => McRole::OnSurfaceVariant,
        Role::SurfaceDim => McRole::SurfaceDim,
        Role::SurfaceBright => McRole::SurfaceBright,
        Role::SurfaceLowest => McRole::SurfaceContainerLowest,
        Role::SurfaceLow => McRole::SurfaceContainerLow,
        Role::SurfaceContainer => McRole::SurfaceContainer,
        Role::SurfaceHigh => McRole::SurfaceContainerHigh,
        Role::SurfaceHighest => McRole::SurfaceContainerHighest,
        Role::InverseSurface => McRole::InverseSurface,
        Role::InverseFg => McRole::InverseOnSurface,
        Role::InverseAccent => McRole::InversePrimary,
        Role::Outline => McRole::Outline,
        Role::OutlineVariant => McRole::OutlineVariant,
        Role::Shadow => McRole::Shadow,
        Role::Scrim => McRole::Scrim,
        Role::SurfaceTint => McRole::SurfaceTint,
        Role::AccentFixed => McRole::PrimaryFixed,
        Role::AccentFixedDim => McRole::PrimaryFixedDim,
        Role::OnAccentFixed => McRole::OnPrimaryFixed,
        Role::OnAccentFixedVariant => McRole::OnPrimaryFixedVariant,
        Role::SecondaryFixed => McRole::SecondaryFixed,
        Role::SecondaryFixedDim => McRole::SecondaryFixedDim,
        Role::OnSecondaryFixed => McRole::OnSecondaryFixed,
        Role::OnSecondaryFixedVariant => McRole::OnSecondaryFixedVariant,
        Role::TertiaryFixed => McRole::TertiaryFixed,
        Role::TertiaryFixedDim => McRole::TertiaryFixedDim,
        Role::OnTertiaryFixed => McRole::OnTertiaryFixed,
        Role::OnTertiaryFixedVariant => McRole::OnTertiaryFixedVariant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Color {
        Color::from_hex(s).unwrap()
    }

    /// Each role by its scheme getter, as `from_seed` read them before it
    /// shared one resolver.
    fn getter(s: &DynamicScheme, r: Role) -> Rgb {
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

    #[test]
    fn one_resolver_gives_every_getters_colour() {
        // Seeds round the hue circle and greys, every variant, light and
        // dark, at three contrasts: each role the shared resolver gives is
        // the colour its own getter solves.
        let seeds = [
            "#7aa2f7", "#6750a4", "#b3261e", "#1e8e3e", "#f9ab00", "#00897b", "#808080", "#000000",
            "#ffffff", "#ff00ff",
        ];
        for seed in seeds {
            for variant in Variant::ALL {
                for dark in [false, true] {
                    for contrast in [-1.0, 0.0, 0.5] {
                        let s = DynamicScheme::from_spec(
                            to_rgb(hex(seed)).into(),
                            variant.mc(),
                            dark,
                            Some(contrast),
                            Platform::Phone,
                            SPEC,
                        );
                        let resolver = s.resolver();
                        for &r in Role::ALL {
                            assert_eq!(
                                resolver.rgb(mc_role(r).color()),
                                getter(&s, r),
                                "{seed} {variant:?} dark {dark} contrast {contrast}: {r}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_kept_palette_is_the_one_solved_again() {
        // Within the memo and past it (more seeds than it keeps, so the
        // first ones are solved again): every answer is the palette the
        // scheme gives, source included.
        let seeds: Vec<Color> = (0..super::SEED_MEMO as u32 * 2)
            .map(|i| {
                Color::from_oklch(strand_scene::Oklch {
                    l: 0.6,
                    c: 0.12,
                    h: i as f64 * 23.0,
                    alpha: 1.0,
                })
                .gamut_mapped()
            })
            .collect();
        for round in 0..3 {
            for (i, seed) in seeds.iter().enumerate() {
                let opts = Options {
                    dark: (i + round) % 2 == 0,
                    contrast: if round == 2 { 0.5 } else { 0.0 },
                    ..Options::default()
                };
                let got = from_seed(*seed, opts);
                let rgb = to_rgb(*seed);
                let want = solve_seed(rgb, opts.variant, opts.dark, opts.contrast);
                assert_eq!(got, want, "round {round}, seed {i}");
                assert_eq!(got.is_dark(), opts.dark);
                assert_eq!(got.source(), Some("material(seed)"));
                // Asked again at once: kept, the same.
                assert_eq!(from_seed(*seed, opts), want);
            }
        }
        // A seed differing only past 8 bits is the same seed to Material.
        let a = Color::new(0.5, 0.25, 0.75, 1.0);
        let b = Color::new(0.5 + 1e-5, 0.25, 0.75, 1.0);
        assert_eq!(
            from_seed(a, Options::default()),
            from_seed(b, Options::default())
        );
        assert_eq!(super::SEEDS.with(|m| m.borrow().0.len()), super::SEED_MEMO);
    }

    #[test]
    fn forgotten_palettes_are_solved_again_alike() {
        let kept = || super::SEEDS.with(|m| m.borrow().0.len());
        let seed = Color::new(0.2, 0.4, 0.8, 1.0);
        let want = from_seed(seed, Options::default());
        assert!(kept() >= 1);
        super::forget_kept_palettes();
        assert_eq!(kept(), 0);
        assert_eq!(super::SEEDS.with(|m| m.borrow().1), 0);
        // The next call is a miss: solved again, the same palette, and
        // kept once more.
        assert_eq!(from_seed(seed, Options::default()), want);
        assert_eq!(kept(), 1);
        // Forgetting is per thread: another thread's palettes are its own.
        std::thread::spawn(move || {
            super::forget_kept_palettes();
            assert_eq!(from_seed(seed, Options::default()), want);
        })
        .join()
        .unwrap();
        assert_eq!(kept(), 1);
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
