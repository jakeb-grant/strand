//! A full palette (every [`Role`] has a colour) and the one derivation
//! table that fills the roles an importer lacks.

use std::collections::BTreeMap;

use strand_scene::{Color, Oklch, PropValue, TokenTable};

use crate::contrast;
use crate::role::Role;

/// Every palette role with a colour, and whether it is a dark palette.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    colors: [Color; Role::COUNT],
    dark: bool,
}

impl Palette {
    /// The colour of `role`.
    pub fn get(&self, role: Role) -> Color {
        self.colors[role.index()]
    }

    /// The colour of the role named `name` (Strand or Material 3 name).
    pub fn by_name(&self, name: &str) -> Option<Color> {
        Role::from_name(name).map(|r| self.get(r))
    }

    pub fn set(&mut self, role: Role, color: Color) {
        self.colors[role.index()] = color;
    }

    /// Whether this is a dark palette (light text on dark surfaces).
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// `(role, colour)` in schema order.
    pub fn iter(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        Role::ALL.iter().map(|&r| (r, self.get(r)))
    }

    /// Writes every role into `table` as a palette root (`$accent`), with
    /// the declared text/background pairs of the contrast guard, so the
    /// render thread keeps them readable while roots spring.
    pub fn insert_into(&self, table: &mut TokenTable) {
        for (r, c) in self.iter() {
            table.insert(r.name(), PropValue::Color(c));
        }
        for (text, bgs) in contrast::PAIRS {
            table.insert_contrast(
                text.name(),
                bgs.iter().map(|b| b.name().to_string()).collect(),
            );
        }
    }

    /// The palette `partial` fills, the rest derived ([`Partial::fill`]).
    pub fn from_partial(partial: Partial) -> Palette {
        partial.fill()
    }

    /// Builds a palette from all roles, as given (no derivation, no
    /// contrast guard).
    pub fn from_fn(dark: bool, mut f: impl FnMut(Role) -> Color) -> Palette {
        let mut colors = [Color::BLACK; Role::COUNT];
        for &r in Role::ALL {
            colors[r.index()] = f(r);
        }
        Palette { colors, dark }
    }
}

/// Some roles of a palette, as an importer found them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partial {
    pub roles: BTreeMap<Role, Color>,
    /// Light or dark, when the source says; otherwise read from the
    /// surface's lightness.
    pub dark: Option<bool>,
}

/// The accent a palette without one gets (design.md's default
/// `prefs.accent`).
pub const DEFAULT_ACCENT: &str = "#7aa2f7";

fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap_or(Color::BLACK)
}

/// `c` at OKLCH lightness `l`, chroma scaled by `chroma`, gamut-mapped.
fn at_lightness(c: Color, l: f64, chroma: f64) -> Color {
    let lch = c.to_oklch();
    Color::from_oklch(Oklch {
        l: l.clamp(0.0, 1.0),
        c: lch.c * chroma,
        h: lch.h,
        alpha: 1.0,
    })
    .gamut_mapped()
}

/// `c` with OKLCH lightness moved by `d`.
fn shifted(c: Color, d: f64) -> Color {
    let lch = c.to_oklch();
    at_lightness(c, lch.l + d, 1.0)
}

fn rotated(c: Color, degrees: f64, chroma: f64) -> Color {
    let lch = c.to_oklch();
    Color::from_oklch(Oklch {
        h: (lch.h + degrees).rem_euclid(360.0),
        c: lch.c * chroma,
        ..lch
    })
    .gamut_mapped()
}

impl Partial {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, role: Role, color: Color) -> Self {
        self.roles.insert(role, color);
        self
    }

    pub fn set(&mut self, role: Role, color: Color) {
        self.roles.insert(role, color);
    }

    pub fn get(&self, role: Role) -> Option<Color> {
        self.roles.get(&role).copied()
    }

    /// Fills every missing role from the ones present, by one table
    /// (design.md: "One table derives any role they lack"), then applies
    /// the contrast guard. The table, in order (each line only where the
    /// role is missing; tones are Material 3's, read as OKLCH lightness):
    ///
    /// - `surface` ⇄ `bg`; with neither, M3's neutral surface (tone 6 dark,
    ///   98 light). Dark is the source's word, else `fg` lighter than
    ///   0.5, else `surface` darker than 0.5.
    /// - `fg` ⇄ `on_bg`; with neither, the surface's hue at lightness 0.92
    ///   (dark) or 0.2 (light).
    /// - `accent` ← `surface_tint` ← [`DEFAULT_ACCENT`] at tone 80 / 40;
    ///   `secondary` ← the accent at 35% chroma; `tertiary` ← the accent
    ///   turned 60°; `error` ← M3's error (`#ffb4ab` / `#ba1a1a`).
    /// - For each of accent, secondary, tertiary, error: `on_X` ← X at
    ///   tone 20 (dark) or white (light); `X_container` ← X mixed 60%
    ///   (dark) or 70% (light) into the surface; `on_X_container` ← X at
    ///   tone 92 / 20.
    /// - `surface_variant` ← surface mixed 18% / 10% towards fg;
    ///   `fg_variant` ← fg mixed 25% towards surface.
    /// - Surface containers by M3's tone steps from the surface: dark
    ///   lowest −0.02, low +0.04, container +0.06, high +0.11, highest
    ///   +0.16, dim ±0, bright +0.18; light lowest +0.02, low −0.02,
    ///   container −0.04, high −0.06, highest −0.08, dim −0.11, bright ±0.
    /// - `inverse_surface` ← fg, `inverse_fg` ← surface, `inverse_accent`
    ///   ← the accent at tone 40 (dark) or 80 (light).
    /// - `outline` ← fg mixed 45% towards surface, `outline_variant` 75%.
    /// - `shadow`, `scrim` ← black; `surface_tint` ← accent.
    pub fn fill(self) -> Palette {
        let mut p = self.roles;
        let get = |p: &BTreeMap<Role, Color>, r: Role| p.get(&r).copied();
        fn or<F: FnOnce() -> Color>(p: &mut BTreeMap<Role, Color>, r: Role, f: F) -> Color {
            if let Some(c) = p.get(&r) {
                return *c;
            }
            let c = f();
            p.insert(r, c);
            c
        }
        // Light or dark.
        let fg_given = get(&p, Role::Fg).or(get(&p, Role::OnBg));
        let surface_given = get(&p, Role::Surface).or(get(&p, Role::Bg));
        let dark = self
            .dark
            .unwrap_or_else(|| match (surface_given, fg_given) {
                (Some(s), _) => s.to_oklch().l < 0.5,
                (None, Some(f)) => f.to_oklch().l > 0.5,
                (None, None) => false,
            });
        let pick = |d: f64, l: f64| if dark { d } else { l };

        let surface =
            surface_given.unwrap_or_else(|| hex(if dark { "#141218" } else { "#fef7ff" }));
        or(&mut p, Role::Surface, || surface);
        or(&mut p, Role::Bg, || surface);
        let fg = fg_given.unwrap_or_else(|| at_lightness(surface, pick(0.92, 0.2), 0.5));
        or(&mut p, Role::Fg, || fg);
        or(&mut p, Role::OnBg, || fg);

        let tint = get(&p, Role::SurfaceTint);
        let accent = or(&mut p, Role::Accent, || {
            tint.unwrap_or_else(|| at_lightness(hex(DEFAULT_ACCENT), pick(0.8, 0.45), 1.0))
        });
        or(&mut p, Role::Secondary, || rotated(accent, 0.0, 0.35));
        or(&mut p, Role::Tertiary, || rotated(accent, 60.0, 1.0));
        or(&mut p, Role::Error, || {
            hex(if dark { "#ffb4ab" } else { "#ba1a1a" })
        });

        let families = [
            (
                Role::Accent,
                Role::OnAccent,
                Role::AccentContainer,
                Role::OnAccentContainer,
            ),
            (
                Role::Secondary,
                Role::OnSecondary,
                Role::SecondaryContainer,
                Role::OnSecondaryContainer,
            ),
            (
                Role::Tertiary,
                Role::OnTertiary,
                Role::TertiaryContainer,
                Role::OnTertiaryContainer,
            ),
            (
                Role::Error,
                Role::OnError,
                Role::ErrorContainer,
                Role::OnErrorContainer,
            ),
        ];
        for (x, on_x, container, on_container) in families {
            let c = p[&x];
            or(&mut p, on_x, || {
                if dark {
                    at_lightness(c, 0.25, 0.5)
                } else {
                    Color::WHITE
                }
            });
            or(&mut p, container, || {
                c.lerp_oklab(surface, pick(0.6, 0.7) as f32)
            });
            or(&mut p, on_container, || {
                at_lightness(c, pick(0.92, 0.2), 0.6)
            });
        }

        let (surface, fg) = (p[&Role::Surface], p[&Role::Fg]);
        or(&mut p, Role::SurfaceVariant, || {
            surface.lerp_oklab(fg, pick(0.18, 0.10) as f32)
        });
        or(&mut p, Role::FgVariant, || fg.lerp_oklab(surface, 0.25));
        let steps: [(Role, f64, f64); 7] = [
            (Role::SurfaceLowest, -0.02, 0.02),
            (Role::SurfaceLow, 0.04, -0.02),
            (Role::SurfaceContainer, 0.06, -0.04),
            (Role::SurfaceHigh, 0.11, -0.06),
            (Role::SurfaceHighest, 0.16, -0.08),
            (Role::SurfaceDim, 0.0, -0.11),
            (Role::SurfaceBright, 0.18, 0.0),
        ];
        for (r, d, l) in steps {
            or(&mut p, r, || shifted(surface, pick(d, l)));
        }
        or(&mut p, Role::InverseSurface, || fg);
        or(&mut p, Role::InverseFg, || surface);
        let accent = p[&Role::Accent];
        or(&mut p, Role::InverseAccent, || {
            at_lightness(accent, pick(0.4, 0.8), 1.0)
        });
        or(&mut p, Role::Outline, || fg.lerp_oklab(surface, 0.45));
        or(&mut p, Role::OutlineVariant, || {
            fg.lerp_oklab(surface, 0.75)
        });
        or(&mut p, Role::Shadow, || Color::BLACK);
        or(&mut p, Role::Scrim, || Color::BLACK);
        or(&mut p, Role::SurfaceTint, || accent);

        let mut palette = Palette::from_fn(dark, |r| p.get(&r).copied().unwrap_or(Color::BLACK));
        contrast::guard(&mut palette);
        palette
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_partial_is_a_whole_palette() {
        for dark in [false, true] {
            let p = Partial {
                dark: Some(dark),
                ..Partial::default()
            }
            .fill();
            assert_eq!(p.is_dark(), dark);
            let s = p.get(Role::Surface).to_oklch().l;
            assert_eq!(s < 0.5, dark);
            for (r, c) in p.iter() {
                assert!(c.in_gamut(1e-4), "{r} {c:?}");
            }
        }
    }

    #[test]
    fn given_roles_are_kept_and_dark_is_inferred() {
        let base = Color::from_hex("#1e1e2e").unwrap();
        let text = Color::from_hex("#cdd6f4").unwrap();
        let p = Partial::new()
            .with(Role::Surface, base)
            .with(Role::Fg, text)
            .fill();
        assert!(p.is_dark());
        assert_eq!(p.get(Role::Surface), base);
        assert_eq!(p.get(Role::Bg), base);
        assert_eq!(p.get(Role::Fg), text);
        assert_eq!(p.get(Role::InverseSurface), text);
        // Containers climb from the surface in a dark palette.
        let l = |r| p.get(r).to_oklch().l;
        assert!(l(Role::SurfaceLowest) < l(Role::Surface));
        assert!(l(Role::SurfaceLow) < l(Role::SurfaceContainer));
        assert!(l(Role::SurfaceContainer) < l(Role::SurfaceHigh));
        assert!(l(Role::SurfaceHigh) < l(Role::SurfaceHighest));
    }
}
