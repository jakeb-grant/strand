//! A full palette (every [`Role`] has a colour) and the one derivation
//! table that fills the roles an importer lacks; the source's own
//! colours (its extras) and its name ride along for templates.

use std::collections::BTreeMap;
use std::sync::Arc;

use material_colors::blend::harmonize;
use material_colors::hct::Hct;

use strand_scene::{Color, Oklch, PropValue, TokenTable};

use crate::contrast;
use crate::material::{from_rgb, to_rgb};
use crate::role::Role;

/// Every palette role with a colour, and whether it is a dark palette.
/// Two palettes are equal when their colours, darkness, extras and name
/// are; the source label is not compared.
#[derive(Clone, Debug)]
pub struct Palette {
    colors: [Color; Role::COUNT],
    dark: bool,
    /// What made it (`material(seed)`, `wallpaper`, `catppuccin:mocha`,
    /// `built-in`), for the inspector's provenance.
    source: Option<Arc<str>>,
    /// The scheme's own name (`Catppuccin Mocha`), when the source has one.
    name: Option<Arc<str>>,
    /// The source's own colours, in source order, by snake_case name.
    extras: Arc<[(String, Color)]>,
}

impl PartialEq for Palette {
    fn eq(&self, other: &Self) -> bool {
        self.colors == other.colors
            && self.dark == other.dark
            && self.extras == other.extras
            && self.name == other.name
    }
}

/// `raw` as an extra's name segment: lower-cased, every character other
/// than an ASCII letter, digit or `_` made `_` (decisions.md m5-tools).
pub fn snake_case(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "_".into() } else { s }
}

/// The extras' name for a source colour at `path` (one segment for a
/// flat source, a W3C group path for nested ones): each segment
/// [`snake_case`]d, joined by `.`, and a first segment the identity uses
/// (`name`, `source`, `dark`) given a trailing `_`.
pub fn extra_name<S: AsRef<str>>(path: &[S]) -> String {
    let mut out = String::new();
    for (i, seg) in path.iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        let seg = snake_case(seg.as_ref());
        out.push_str(&seg);
        if i == 0 && matches!(seg.as_str(), "name" | "source" | "dark") {
            out.push('_');
        }
    }
    out
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

    /// What made this palette (see [`Palette::with_source`]).
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The palette labelled as made by `source` (provenance only).
    pub fn with_source(mut self, source: &str) -> Palette {
        self.source = Some(source.into());
        self
    }

    /// Whether this is a dark palette (light text on dark surfaces).
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// The palette's name: the scheme's own (`Catppuccin Mocha`), else
    /// its source string.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref().or(self.source())
    }

    /// The palette named `name` (see [`Palette::name`]).
    pub fn with_name(mut self, name: &str) -> Palette {
        self.name = Some(name.into());
        self
    }

    /// The source's own colours, in source order (`$palette.<name>`).
    pub fn extras(&self) -> &[(String, Color)] {
        &self.extras
    }

    /// The extra named `name`.
    pub fn extra(&self, name: &str) -> Option<Color> {
        self.extras.iter().find(|(n, _)| n == name).map(|e| e.1)
    }

    /// The palette with `extras` as its source's colours.
    pub fn with_extras(mut self, extras: Vec<(String, Color)>) -> Palette {
        self.extras = extras.into();
        self
    }

    /// `(role, colour)` in schema order.
    pub fn iter(&self) -> impl Iterator<Item = (Role, Color)> + '_ {
        Role::ALL.iter().map(|&r| (r, self.get(r)))
    }

    /// Writes every role into `table` as a palette root (`$accent`), with
    /// the declared text/background pairs of the contrast guard, so the
    /// render thread keeps them readable while roots spring; and the
    /// identity and extras as the table's palette entries
    /// ([`TokenTable::palette`]: `name`, `source`, `dark`, then the
    /// extras in source order).
    pub fn insert_into(&self, table: &mut TokenTable) {
        let origin = format!("palette:{}", self.source().unwrap_or("?"));
        for (r, c) in self.iter() {
            table.insert(r.name(), PropValue::Color(c));
            table.set_origin(r.name(), origin.clone());
        }
        let mut entries = Vec::with_capacity(3 + self.extras.len());
        let text = |s: &str| PropValue::Text(s.to_string());
        if let Some(n) = self.name() {
            entries.push(("name".to_string(), text(n)));
        }
        if let Some(s) = self.source() {
            entries.push(("source".to_string(), text(s)));
        }
        entries.push(("dark".to_string(), PropValue::Bool(self.dark)));
        for (n, c) in self.extras.iter() {
            entries.push((n.clone(), PropValue::Color(*c)));
        }
        table.palette = entries;
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

    /// The palette as text, one `role #rrggbb` line per role after a
    /// `dark true|false` line, its `source` and `name` lines, then one
    /// `extra <name> #rrggbbaa` line per extra (what `strand run`
    /// persists as the last palette).
    pub fn to_text(&self) -> String {
        use std::fmt::Write;
        let mut out = format!("# strand palette\ndark {}\n", self.dark);
        let line = |s: &str| !s.contains(['\n', '\r']);
        if let Some(s) = self.source().filter(|s| line(s)) {
            let _ = writeln!(out, "source {s}");
        }
        if let Some(n) = self.name.as_deref().filter(|s| line(s)) {
            let _ = writeln!(out, "name {n}");
        }
        for (r, c) in self.iter() {
            let [x, y, z, _] = c.to_rgba8();
            let _ = writeln!(out, "{} #{x:02x}{y:02x}{z:02x}", r.name());
        }
        for (n, c) in self.extras.iter() {
            let [x, y, z, a] = c.to_rgba8();
            let _ = writeln!(out, "extra {n} #{x:02x}{y:02x}{z:02x}{a:02x}");
        }
        out
    }

    /// Reads [`Palette::to_text`]'s form. Roles it lacks (an older
    /// schema) are derived; `None` if it names no role at all.
    pub fn from_text(text: &str) -> Option<Palette> {
        let mut part = Partial::default();
        let mut source = None;
        for line in text.lines() {
            let Some((k, v)) = line.trim().split_once(' ') else {
                continue;
            };
            match (k, Role::from_name(k)) {
                ("dark", _) => part.dark = v.trim().parse().ok(),
                ("source", _) => source = Some(v.trim().to_string()),
                ("name", _) => part.name = Some(v.trim().to_string()),
                ("extra", _) => {
                    if let Some((n, c)) = v.trim().split_once(' ')
                        && let Some(c) = Color::from_hex(c.trim())
                    {
                        part.extras.push((n.to_string(), c));
                    }
                }
                (_, Some(r)) => {
                    if let Some(c) = Color::from_hex(v.trim()) {
                        part.set(r, c);
                    }
                }
                _ => {}
            }
        }
        let p = (!part.roles.is_empty()).then(|| part.fill())?;
        Some(match source {
            Some(s) => p.with_source(&s),
            None => p,
        })
    }

    /// Builds a palette from all roles, as given (no derivation, no
    /// contrast guard).
    pub fn from_fn(dark: bool, mut f: impl FnMut(Role) -> Color) -> Palette {
        let mut colors = [Color::BLACK; Role::COUNT];
        for &r in Role::ALL {
            colors[r.index()] = f(r);
        }
        Palette {
            colors,
            dark,
            source: None,
            name: None,
            extras: Arc::new([]),
        }
    }
}

/// Some roles of a palette, as an importer found them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partial {
    pub roles: BTreeMap<Role, Color>,
    /// Light or dark, when the source says; otherwise read from the
    /// surface's lightness.
    pub dark: Option<bool>,
    /// The scheme's own name, when the source has one.
    pub name: Option<String>,
    /// Every colour of the source, in source order, by its extras name
    /// ([`extra_name`]).
    pub extras: Vec<(String, Color)>,
}

/// The seven base hue roles and their canonical colours: the sRGB
/// primaries and secondaries, with orange between red and yellow.
const BASE_HUES: [(Role, &str); 7] = [
    (Role::Red, "#ff0000"),
    (Role::Orange, "#ff8000"),
    (Role::Yellow, "#ffff00"),
    (Role::Green, "#00ff00"),
    (Role::Cyan, "#00ffff"),
    (Role::Blue, "#0000ff"),
    (Role::Magenta, "#ff00ff"),
];

/// Each bright hue role and its base.
const BRIGHT_HUES: [(Role, Role); 6] = [
    (Role::BrightRed, Role::Red),
    (Role::BrightYellow, Role::Yellow),
    (Role::BrightGreen, Role::Green),
    (Role::BrightCyan, Role::Cyan),
    (Role::BrightBlue, Role::Blue),
    (Role::BrightMagenta, Role::Magenta),
];

/// The hue `canonical` harmonised toward `accent` (its HCT hue turned
/// toward the accent's by half the difference, at most 15°, as
/// `blend::harmonize` does), at Material tone 80 (dark) or 40 (light).
pub fn harmonised(canonical: Color, accent: Color, dark: bool) -> Color {
    let mut h: Hct = harmonize(to_rgb(canonical), to_rgb(accent)).into();
    h.set_tone(if dark { 80.0 } else { 40.0 });
    from_rgb(h.into())
}

/// Fills the hue roles `p` lacks (design.md, "Hue roles"): a base hue is
/// its canonical colour [`harmonised`] toward `accent`; a bright is its
/// base with OKLCH lightness +0.08 (dark) or −0.08 (light). Given hues
/// are kept.
pub(crate) fn fill_hues(p: &mut BTreeMap<Role, Color>, accent: Color, dark: bool) {
    for (r, canonical) in BASE_HUES {
        // Not the entry API: `insert` is instantiated already, the entry
        // API's vacant insert would be another 3 KB.
        if p.get(&r).is_none() {
            p.insert(r, harmonised(hex(canonical), accent, dark));
        }
    }
    for (bright, base) in BRIGHT_HUES {
        if !p.contains_key(&bright) {
            let c = shifted(p[&base], if dark { 0.08 } else { -0.08 });
            p.insert(bright, c);
        }
    }
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
    ///   lightness 0.25 (dark) or white (light); `X_container` ← X mixed 60%
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
    /// - For each of accent, secondary, tertiary, in light and dark alike
    ///   (M3's fixed accents): `X_fixed` ← X at lightness 0.9,
    ///   `X_fixed_dim` 0.8, `on_X_fixed` 0.12, `on_X_fixed_variant` 0.32.
    /// - The hue roles ([`fill_hues`]): never contrast-guarded.
    ///
    /// Given roles are kept, gamut-mapped into sRGB and made opaque (a
    /// palette role is a colour things are drawn in, not a tint). The
    /// name and extras pass through, the extras gamut-mapped.
    pub fn fill(self) -> Palette {
        let mut p = self.roles;
        for c in p.values_mut() {
            *c = c.gamut_mapped().with_alpha(1.0);
        }
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
        // The fixed accents keep Material 3's tones in light and dark.
        let fixed = [
            (
                Role::Accent,
                Role::AccentFixed,
                Role::AccentFixedDim,
                Role::OnAccentFixed,
                Role::OnAccentFixedVariant,
            ),
            (
                Role::Secondary,
                Role::SecondaryFixed,
                Role::SecondaryFixedDim,
                Role::OnSecondaryFixed,
                Role::OnSecondaryFixedVariant,
            ),
            (
                Role::Tertiary,
                Role::TertiaryFixed,
                Role::TertiaryFixedDim,
                Role::OnTertiaryFixed,
                Role::OnTertiaryFixedVariant,
            ),
        ];
        for (x, f, dim, on, on_variant) in fixed {
            let c = p[&x];
            or(&mut p, f, || at_lightness(c, 0.9, 0.6));
            or(&mut p, dim, || at_lightness(c, 0.8, 0.8));
            or(&mut p, on, || at_lightness(c, 0.12, 0.6));
            or(&mut p, on_variant, || at_lightness(c, 0.32, 0.8));
        }
        fill_hues(&mut p, accent, dark);

        // Mixes of in-gamut colours can land a hair outside sRGB.
        let mut palette = Palette::from_fn(dark, |r| {
            p.get(&r).map_or(Color::BLACK, |c| c.gamut_mapped())
        });
        contrast::guard(&mut palette);
        palette.name = self.name.map(Into::into);
        palette.with_extras(
            self.extras
                .into_iter()
                .map(|(n, c)| (n, c.gamut_mapped()))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let p = crate::from_seed(Color::from_hex("#7aa2f7").unwrap(), Default::default());
        let back = Palette::from_text(&p.to_text()).unwrap();
        for (r, c) in p.iter() {
            assert_eq!(back.get(r).to_rgba8(), c.to_rgba8(), "{r}");
        }
        assert_eq!(back.is_dark(), p.is_dark());
        assert_eq!(Palette::from_text("nonsense\n"), None);
    }

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
