//! Palettes: `material(seed: …)` and `import("catppuccin:mocha")`.
//!
//! The real Material 3 algorithm (HCT tonal palettes, the scheme variants
//! and contrast levels) arrives with the `material-colors` crate in M2.
//! Until then `material(seed:)` is a deterministic stand-in with the same
//! shape: five tonal palettes in OKLCH (primary from the seed's hue and
//! chroma, secondary at a third of its chroma, tertiary 60° round the hue,
//! neutral and neutral-variant nearly grey, error at a fixed red), and
//! every role at its Material 3 tone, read as OKLCH lightness. It fills
//! the whole palette schema, light or dark, so a theme written against M3
//! roles runs and looks plausible now and exact later.

use strand_scene::{Color, Oklch};

use super::value::Palette;

/// A tone of a tonal palette (`hue`, `chroma`) at Material tone `t`
/// (0 black … 100 white), gamut-mapped by lowering chroma.
fn tone(hue: f64, chroma: f64, t: f64) -> Color {
    let l = (t / 100.0).clamp(0.0, 1.0);
    // Chroma falls off towards black and white, as tonal palettes do.
    let room = (1.0 - (2.0 * l - 1.0).abs()).max(0.0);
    let mut c = chroma * room.sqrt();
    for _ in 0..24 {
        let col = Color::from_oklch(Oklch {
            l,
            c,
            h: hue,
            alpha: 1.0,
        });
        if col.in_gamut(1e-4) {
            return col;
        }
        c *= 0.85;
    }
    Color::from_oklch(Oklch {
        l,
        c: 0.0,
        h: hue,
        alpha: 1.0,
    })
    .clamped()
}

/// Roles and their Material 3 tones: (role, palette, light tone, dark
/// tone). Palettes: 0 primary, 1 secondary, 2 tertiary, 3 error,
/// 4 neutral, 5 neutral variant.
const ROLES: &[(&str, usize, f64, f64)] = &[
    ("accent", 0, 40.0, 80.0),
    ("on_accent", 0, 100.0, 20.0),
    ("accent_container", 0, 90.0, 30.0),
    ("on_accent_container", 0, 10.0, 90.0),
    ("secondary", 1, 40.0, 80.0),
    ("on_secondary", 1, 100.0, 20.0),
    ("secondary_container", 1, 90.0, 30.0),
    ("on_secondary_container", 1, 10.0, 90.0),
    ("tertiary", 2, 40.0, 80.0),
    ("on_tertiary", 2, 100.0, 20.0),
    ("tertiary_container", 2, 90.0, 30.0),
    ("on_tertiary_container", 2, 10.0, 90.0),
    ("error", 3, 40.0, 80.0),
    ("on_error", 3, 100.0, 20.0),
    ("error_container", 3, 90.0, 30.0),
    ("on_error_container", 3, 10.0, 90.0),
    ("bg", 4, 98.0, 6.0),
    ("on_bg", 4, 10.0, 90.0),
    ("surface", 4, 98.0, 6.0),
    ("fg", 4, 10.0, 90.0),
    ("surface_variant", 5, 90.0, 30.0),
    ("fg_variant", 5, 30.0, 80.0),
    ("surface_dim", 4, 87.0, 6.0),
    ("surface_bright", 4, 98.0, 24.0),
    ("surface_lowest", 4, 100.0, 4.0),
    ("surface_low", 4, 96.0, 10.0),
    ("surface_container", 4, 94.0, 12.0),
    ("surface_high", 4, 92.0, 17.0),
    ("surface_highest", 4, 90.0, 22.0),
    ("inverse_surface", 4, 20.0, 90.0),
    ("inverse_fg", 4, 95.0, 20.0),
    ("inverse_accent", 0, 80.0, 40.0),
    ("outline", 5, 50.0, 60.0),
    ("outline_variant", 5, 80.0, 30.0),
    ("shadow", 4, 0.0, 0.0),
    ("scrim", 4, 0.0, 0.0),
    ("surface_tint", 0, 40.0, 80.0),
];

/// `material(seed:, variant:, dark:, contrast:)`.
pub fn material(seed: Color, variant: &str, dark: bool, contrast: f64) -> Palette {
    let s = seed.to_oklch();
    let base = s.c.max(0.04);
    let (primary_c, secondary_c, tertiary_c, neutral_c) = match variant {
        "monochrome" => (0.0, 0.0, 0.0, 0.0),
        "neutral" => (base * 0.4, base * 0.2, base * 0.3, 0.004),
        "vibrant" | "fruit_salad" | "rainbow" => (base.max(0.15), base * 0.5, base * 0.6, 0.012),
        "expressive" => (base * 0.8, base * 0.5, base.max(0.12), 0.016),
        "fidelity" | "content" => (base, base * 0.45, base * 0.6, 0.01),
        _ => (base.max(0.09), base / 3.0, base * 0.5, 0.008),
    };
    let palettes = [
        (s.h, primary_c),
        (s.h, secondary_c),
        ((s.h + 60.0) % 360.0, tertiary_c),
        (25.0, 0.17),
        (s.h, neutral_c),
        (s.h, neutral_c * 2.0),
    ];
    // Contrast pushes tones away from the middle.
    let k = contrast.clamp(-1.0, 1.0) * 0.25;
    let roles = ROLES
        .iter()
        .map(|&(role, p, light, darkt)| {
            let t = if dark { darkt } else { light };
            let t = (t + (t - 50.0) * k).clamp(0.0, 100.0);
            let (h, c) = palettes[p];
            (role.to_string(), tone(h, c, t))
        })
        .collect();
    Palette { roles }
}

fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap_or(Color::BLACK)
}

/// `import("catppuccin:mocha")` and the other bundled palettes.
pub fn import(source: &str) -> Option<Palette> {
    // Catppuccin flavours: base, mantle, crust, surface0-2, overlay0,
    // text, subtext0, accent (mauve), red, peach, green, sky.
    let flavour: [&str; 14] = match source {
        "catppuccin:mocha" => [
            "#1e1e2e", "#181825", "#11111b", "#313244", "#45475a", "#585b70", "#6c7086", "#cdd6f4",
            "#a6adc8", "#cba6f7", "#f38ba8", "#fab387", "#a6e3a1", "#89dceb",
        ],
        "catppuccin:macchiato" => [
            "#24273a", "#1e2030", "#181926", "#363a4f", "#494d64", "#5b6078", "#6e738d", "#cad3f5",
            "#a5adcb", "#c6a0f6", "#ed8796", "#f5a97f", "#a6da95", "#91d7e3",
        ],
        "catppuccin:frappe" => [
            "#303446", "#292c3c", "#232634", "#414559", "#51576d", "#626880", "#737994", "#c6d0f5",
            "#a5adce", "#ca9ee6", "#e78284", "#ef9f76", "#a6d189", "#99d1db",
        ],
        "catppuccin:latte" => [
            "#eff1f5", "#e6e9ef", "#dce0e8", "#ccd0da", "#bcc0cc", "#acb0be", "#9ca0b0", "#4c4f69",
            "#6c6f85", "#8839ef", "#d20f39", "#fe640b", "#40a02b", "#04a5e5",
        ],
        _ => return None,
    };
    let [
        base,
        mantle,
        crust,
        s0,
        s1,
        s2,
        overlay,
        text,
        subtext,
        mauve,
        red,
        peach,
        green,
        sky,
    ] = flavour.map(hex);
    let light = source.ends_with("latte");
    let on = |_: Color| if light { Color::WHITE } else { crust };
    let roles: Vec<(&str, Color)> = vec![
        ("accent", mauve),
        ("on_accent", on(base)),
        ("accent_container", mauve.lerp_oklab(base, 0.6)),
        ("on_accent_container", text),
        ("secondary", sky),
        ("on_secondary", on(base)),
        ("secondary_container", sky.lerp_oklab(base, 0.6)),
        ("on_secondary_container", text),
        ("tertiary", peach),
        ("on_tertiary", on(base)),
        ("tertiary_container", peach.lerp_oklab(base, 0.6)),
        ("on_tertiary_container", text),
        ("error", red),
        ("on_error", on(base)),
        ("error_container", red.lerp_oklab(base, 0.6)),
        ("on_error_container", text),
        ("bg", base),
        ("on_bg", text),
        ("surface", base),
        ("fg", text),
        ("surface_variant", s0),
        ("fg_variant", subtext),
        ("surface_dim", crust),
        ("surface_bright", s1),
        ("surface_lowest", crust),
        ("surface_low", mantle),
        ("surface_container", s0),
        ("surface_high", s1),
        ("surface_highest", s2),
        ("inverse_surface", text),
        ("inverse_fg", base),
        ("inverse_accent", mauve.lerp_oklab(text, 0.3)),
        ("outline", overlay),
        ("outline_variant", s1),
        ("shadow", Color::BLACK),
        ("scrim", Color::BLACK),
        ("surface_tint", mauve),
    ];
    let _ = green;
    Some(Palette {
        roles: roles.into_iter().map(|(r, c)| (r.to_string(), c)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;

    #[test]
    fn material_fills_every_palette_role() {
        let roles: Vec<&str> = Schema::builtin().palette_roles().collect();
        for dark in [false, true] {
            let p = material(Color::from_hex("#7aa2f7").unwrap(), "tonal_spot", dark, 0.0);
            for r in &roles {
                assert!(p.get(r).is_some(), "material lacks {r}");
            }
        }
        let p = import("catppuccin:mocha").unwrap();
        for r in &roles {
            assert!(p.get(r).is_some(), "mocha lacks {r}");
        }
    }

    #[test]
    fn dark_palettes_are_dark() {
        let seed = Color::from_hex("#7aa2f7").unwrap();
        let light = material(seed, "tonal_spot", false, 0.0);
        let dark = material(seed, "tonal_spot", true, 0.0);
        let l = |p: &Palette, r: &str| p.get(r).unwrap().to_oklch().l;
        assert!(l(&light, "surface") > 0.9);
        assert!(l(&dark, "surface") < 0.15);
        assert!(l(&dark, "fg") > l(&dark, "surface"));
        // Same seed, same palette.
        assert_eq!(material(seed, "tonal_spot", true, 0.0), dark);
    }
}
