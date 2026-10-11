//! The theming hub's palette half (decisions.md m5-tools; m5-plan
//! S-palette): the 13 hue roles, filled from the source or harmonised
//! toward the accent; the extras and identity every importer keeps; the
//! persisted file's `name` and `extra` lines.

use std::path::PathBuf;

use material_colors::color::Rgb;
use material_colors::hct::Hct;
use strand_scene::{Color, Oklch, PropValue, TokenTable};
use strand_theme::contrast::{self, MIN_CONTRAST};
use strand_theme::import::CATPPUCCIN;
use strand_theme::import::CATPPUCCIN_NAMES;
use strand_theme::{ImportError, Options, Palette, Partial, Role, Variant, from_seed, import};

fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap()
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "strand-palette-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// All 62 roles in gamut and opaque, the declared pairs readable.
fn whole(p: &Palette) {
    assert_eq!(p.iter().count(), 62);
    for (r, c) in p.iter() {
        assert!(c.in_gamut(1e-4) && c.a == 1.0, "{r}: {c:?}");
    }
    assert!(contrast::worst(p) >= MIN_CONTRAST);
}

fn hct_hue(c: Color) -> f64 {
    let [r, g, b, _] = c.to_rgba8();
    Hct::new(Rgb::new(r, g, b)).get_hue()
}

/// The signed difference `to − from` in degrees, in (−180, 180].
fn turn(from: f64, to: f64) -> f64 {
    let d = (to - from).rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}

const CANONICAL: [(Role, &str); 7] = [
    (Role::Red, "#ff0000"),
    (Role::Orange, "#ff8000"),
    (Role::Yellow, "#ffff00"),
    (Role::Green, "#00ff00"),
    (Role::Cyan, "#00ffff"),
    (Role::Blue, "#0000ff"),
    (Role::Magenta, "#ff00ff"),
];

const BRIGHTS: [(Role, Role); 6] = [
    (Role::BrightRed, Role::Red),
    (Role::BrightYellow, Role::Yellow),
    (Role::BrightGreen, Role::Green),
    (Role::BrightCyan, Role::Cyan),
    (Role::BrightBlue, Role::Blue),
    (Role::BrightMagenta, Role::Magenta),
];

/// Every derived base hue is its canonical hue turned toward the accent
/// by half the difference, at most 15°, at tone 80 (dark) or 40 (light);
/// every derived bright is its base 0.08 OKLCH L lighter (dark) or
/// darker (light).
fn harmonised(p: &Palette) {
    let accent = hct_hue(p.get(Role::Accent));
    for (r, canonical) in CANONICAL {
        let h0 = hct_hue(hex(canonical));
        let d = turn(h0, accent);
        let want = h0 + d.signum() * (d.abs() * 0.5).min(15.0);
        let c = p.get(r);
        let got = hct_hue(c);
        assert!(turn(h0, got).abs() <= 15.0 + 1.5, "{r}: {got} from {h0}");
        assert!(turn(want, got).abs() <= 1.5, "{r}: {got}, want {want}");
        let [x, y, z, _] = c.to_rgba8();
        let tone = Hct::new(Rgb::new(x, y, z)).get_tone();
        let want_tone = if p.is_dark() { 80.0 } else { 40.0 };
        assert!((tone - want_tone).abs() < 1.0, "{r}: tone {tone}");
    }
    brights(p);
}

/// Every bright is its base with OKLCH L +0.08 (dark) or −0.08 (light),
/// brought into sRGB.
fn brights(p: &Palette) {
    for (bright, base) in BRIGHTS {
        let lch = p.get(base).to_oklch();
        let step = if p.is_dark() { 0.08 } else { -0.08 };
        let want = Color::from_oklch(Oklch {
            l: (lch.l + step).clamp(0.0, 1.0),
            ..lch
        })
        .gamut_mapped();
        assert_eq!(p.get(bright).to_rgba8(), want.to_rgba8(), "{bright}");
    }
}

#[test]
fn hues_are_harmonised_toward_the_accent_in_light_and_dark() {
    assert_eq!(Role::COUNT, 62);
    let seeds = [
        "#7aa2f7", "#6750a4", "#b3261e", "#1e8e3e", "#f9ab00", "#00897b", "#808080",
    ];
    for seed in seeds {
        for dark in [false, true] {
            for variant in [Variant::TonalSpot, Variant::Vibrant, Variant::Monochrome] {
                let p = from_seed(
                    hex(seed),
                    Options {
                        variant,
                        dark,
                        contrast: 0.0,
                    },
                );
                whole(&p);
                harmonised(&p);
            }
        }
    }
    // The derivation table does the same for a source with no hues.
    for dark in [false, true] {
        let p = Partial {
            dark: Some(dark),
            ..Partial::default()
        }
        .fill();
        whole(&p);
        harmonised(&p);
        let p = Partial::new()
            .with(Role::Accent, hex("#e91e63"))
            .with(Role::Surface, hex(if dark { "#101010" } else { "#fafafa" }))
            .fill();
        whole(&p);
        harmonised(&p);
    }
}

#[test]
fn given_hues_are_never_changed() {
    // Catppuccin maps its own colours as its terminal ports do, in every
    // flavour; the contrast guard leaves them alone.
    for (flavour, table) in CATPPUCCIN {
        let p = import(&format!("catppuccin:{flavour}"), None).unwrap();
        whole(&p);
        let c = |name: &str| {
            let i = CATPPUCCIN_NAMES.iter().position(|n| *n == name).unwrap();
            hex(table[i])
        };
        for (role, name) in [
            (Role::Red, "red"),
            (Role::Orange, "peach"),
            (Role::Yellow, "yellow"),
            (Role::Green, "green"),
            (Role::Cyan, "teal"),
            (Role::Blue, "blue"),
            (Role::Magenta, "pink"),
        ] {
            assert_eq!(p.get(role), c(name), "{flavour} {role}");
        }
        // Brights are derived from the given bases.
        brights(&p);
    }
    // A hue that fails every contrast pair stays as given.
    let p = Partial::new()
        .with(Role::Surface, hex("#000000"))
        .with(Role::Green, hex("#010201"))
        .fill();
    assert_eq!(p.get(Role::Green), hex("#010201"));
}

#[test]
fn catppuccin_keeps_its_26_colours_and_its_name() {
    let p = import("catppuccin:mocha", None).unwrap();
    let names: Vec<&str> = p.extras().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, CATPPUCCIN_NAMES);
    assert_eq!(p.extra("mauve"), Some(hex("#cba6f7")));
    assert_eq!(p.extra("crust"), Some(hex("#11111b")));
    assert_eq!(p.name(), Some("Catppuccin Mocha"));
    assert_eq!(p.source(), Some("catppuccin:mocha"));
    // Changing the accent changes the role, never the extra.
    let blue = import("catppuccin:mocha:blue", None).unwrap();
    assert_eq!(blue.get(Role::Accent), hex("#89b4fa"));
    assert_eq!(blue.extra("mauve"), Some(hex("#cba6f7")));
    assert_eq!(
        import("catppuccin:frappe", None).unwrap().name(),
        Some("Catppuccin Frappé")
    );
}

/// A base24 scheme in tinted-theming's canonical form (lowercase
/// `"#rrggbb"`), its slots all distinct.
const BASE24: &str = include_str!("export/scheme.yaml");

#[test]
fn base16_and_base24_keep_every_slot_and_map_their_hues() {
    let dir = temp("base24");
    std::fs::write(dir.join("s.yaml"), BASE24).unwrap();
    let p = import("base24:s.yaml", Some(&dir)).unwrap();
    whole(&p);
    let names: Vec<&str> = p.extras().iter().map(|(n, _)| n.as_str()).collect();
    let want: Vec<String> = (0..24).map(|i| format!("base{i:02x}")).collect();
    assert_eq!(names, want);
    assert_eq!(p.extra("base0d"), Some(hex("#61afef")));
    assert_eq!(p.name(), Some("Strand Test Dark"));
    for (role, slot) in [
        (Role::Red, "base08"),
        (Role::Orange, "base09"),
        (Role::Yellow, "base0a"),
        (Role::Green, "base0b"),
        (Role::Cyan, "base0c"),
        (Role::Blue, "base0d"),
        (Role::Magenta, "base0e"),
        (Role::BrightRed, "base12"),
        (Role::BrightYellow, "base13"),
        (Role::BrightGreen, "base14"),
        (Role::BrightCyan, "base15"),
        (Role::BrightBlue, "base16"),
        (Role::BrightMagenta, "base17"),
    ] {
        assert_eq!(Some(p.get(role)), p.extra(slot), "{role}");
    }
    // base16: sixteen slots, the brights derived from its bases; the
    // legacy `scheme:` names it.
    let base16: String = BASE24
        .lines()
        .filter(|l| !l.trim_start().starts_with("base1") && !l.starts_with("name"))
        .chain(["scheme: \"Legacy\""])
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(dir.join("b.yaml"), base16).unwrap();
    let p = import("base16:b.yaml", Some(&dir)).unwrap();
    whole(&p);
    assert_eq!(p.extras().len(), 16);
    assert_eq!(p.name(), Some("Legacy"));
    assert_eq!(p.get(Role::Red), hex("#e06c75"));
    brights(&p);
    let _ = std::fs::remove_dir_all(dir);
}

/// spec.md 9.5: extras keep the source's order, so a base24 file that
/// lists its backgrounds (`00`–`07`, `10`–`17`) before its accents
/// (`08`–`0F`) gives them in that order, not slot order.
#[test]
fn base24_extras_follow_the_files_order_not_slot_order() {
    let dir = temp("base24-order");
    let lines: Vec<&str> = BASE24.lines().collect();
    let slot = |l: &&str| {
        l.trim_start()
            .strip_prefix("base")
            .map(|n| n[..2].to_string())
    };
    let (head, rest): (Vec<&str>, Vec<&str>) = lines.iter().partition(|l| slot(l).is_none());
    let (low, high): (Vec<&str>, Vec<&str>) = rest
        .iter()
        .partition(|l| matches!(slot(l).as_deref(), Some(n) if !("08".."10").contains(&n)));
    let text: String = head
        .iter()
        .chain(&low)
        .chain(&high)
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(dir.join("s.yaml"), text).unwrap();
    let p = import("base24:s.yaml", Some(&dir)).unwrap();
    let names: Vec<&str> = p.extras().iter().map(|(n, _)| n.as_str()).collect();
    let want: Vec<String> = (0..8)
        .chain(16..24)
        .chain(8..16)
        .map(|i| format!("base{i:02x}"))
        .collect();
    assert_eq!(names, want);
    assert_eq!(p.extra("base0d"), Some(hex("#61afef")));
    assert_eq!(p.get(Role::BrightBlue), hex("#79c0ff"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn matugen_and_w3c_keep_their_colours_in_source_order() {
    let dir = temp("order");
    // Keys deliberately out of alphabetical order: the extras follow the
    // file, not serde_json's sorted map.
    let doc = r##"{
  "mode": "dark",
  "colors": {
    "dark": {
      "source_color": "#7aa2f7",
      "primary": "#a4c9fe",
      "zeta": "#112233",
      "surface": "#111318",
      "alpha": "#445566",
      "green": "#3fb950",
      "on_surface": "#e1e2e8"
    }
  }
}"##;
    std::fs::write(dir.join("m.json"), doc).unwrap();
    let p = import("matugen:m.json", Some(&dir)).unwrap();
    whole(&p);
    // Every colour of the source is kept, a role's included (spec.md 9.5):
    // `primary` is both `$accent` and `$palette.primary`, and a custom
    // colour named for a hue fills that hue and is `$palette.green`.
    let extras: Vec<(&str, Color)> = p.extras().iter().map(|(n, c)| (n.as_str(), *c)).collect();
    assert_eq!(
        extras,
        [
            ("source_color", hex("#7aa2f7")),
            ("primary", hex("#a4c9fe")),
            ("zeta", hex("#112233")),
            ("surface", hex("#111318")),
            ("alpha", hex("#445566")),
            ("green", hex("#3fb950")),
            ("on_surface", hex("#e1e2e8")),
        ]
    );
    assert_eq!(p.get(Role::Accent), hex("#a4c9fe"));
    assert_eq!(p.get(Role::Green), hex("#3fb950"));
    assert_eq!(p.name(), Some("matugen:m.json"), "no name: the source");

    // matugen 3's `colors.<role>.<mode>` layout, `{ "color": … }` values:
    // the extras are the role keys, in the file's order, each with the
    // mode's colour.
    let doc = r##"{
  "mode": "dark",
  "colors": {
    "source_color": { "dark": { "color": "#7aa2f7" }, "light": { "color": "#7aa2f7" } },
    "zeta": { "light": { "color": "#000000" }, "dark": { "color": "#112233" } },
    "primary": { "light": { "color": "#3a5f8f" }, "dark": { "color": "#a4c9fe" } },
    "alpha": { "default": { "color": "#445566" } },
    "surface": { "light": { "color": "#f9f9ff" }, "dark": { "color": "#111318" } }
  }
}"##;
    std::fs::write(dir.join("v3.json"), doc).unwrap();
    let p = import("matugen:v3.json", Some(&dir)).unwrap();
    whole(&p);
    let extras: Vec<(&str, Color)> = p.extras().iter().map(|(n, c)| (n.as_str(), *c)).collect();
    assert_eq!(
        extras,
        [
            ("source_color", hex("#7aa2f7")),
            ("zeta", hex("#112233")),
            ("primary", hex("#a4c9fe")),
            ("alpha", hex("#445566")),
            ("surface", hex("#111318")),
        ]
    );
    assert_eq!(p.get(Role::Accent), hex("#a4c9fe"));

    let doc = r##"{
  "color": {
    "$type": "color",
    "surface": { "$value": "#101418" },
    "Brand-Primary": { "$value": "#ff5500" },
    "accent": { "$value": "{color.Brand-Primary}" },
    "on-surface": { "$value": "#e8e8f0" },
    "red": { "$value": "#d03030" }
  },
  "name": { "$type": "color", "$value": "#808080" },
  "size": { "$type": "dimension", "md": { "$value": "4px" } },
  "base": { "$type": "color", "night": { "$value": "#000010" } }
}"##;
    std::fs::write(dir.join("w.json"), doc).unwrap();
    let p = import("w3c:w.json", Some(&dir)).unwrap();
    whole(&p);
    let names: Vec<&str> = p.extras().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "color.surface",
            "color.brand_primary",
            "color.accent",
            "color.on_surface",
            "color.red",
            "name_",
            "base.night",
        ]
    );
    assert_eq!(p.extra("color.accent"), Some(hex("#ff5500")));
    assert_eq!(p.get(Role::Accent), hex("#ff5500"));
    // A W3C token naming a hue fills it.
    assert_eq!(p.get(Role::Red), hex("#d03030"));

    // Two colours that become one name are an error naming both.
    let doc = r##"{ "color": { "$type": "color",
        "accent": { "$value": "#ff0000" },
        "Brand": { "$value": "#00ff00" },
        "brand": { "$value": "#0000ff" } } }"##;
    std::fs::write(dir.join("dup.json"), doc).unwrap();
    let err = import("w3c:dup.json", Some(&dir)).unwrap_err();
    let ImportError::Parse { error, .. } = &err else {
        panic!("{err:?}")
    };
    assert!(
        error.contains("color.Brand") && error.contains("color.brand"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_seed_is_the_material_palettes_extra() {
    let p = from_seed(hex("#6750a4"), Options::default());
    let extras: Vec<(&str, Color)> = p.extras().iter().map(|(n, c)| (n.as_str(), *c)).collect();
    assert_eq!(extras, [("source_color", hex("#6750a4"))]);
    assert_eq!(p.name(), Some("material(seed)"));
}

#[test]
fn the_persisted_text_round_trips_extras_and_name() {
    for source in ["catppuccin:mocha", "catppuccin:latte"] {
        let p = import(source, None).unwrap();
        let text = p.to_text();
        assert!(text.contains("\nname Catppuccin "), "{text}");
        assert!(text.contains("\nextra mauve #"), "{text}");
        let back = Palette::from_text(&text).unwrap();
        for (r, c) in p.iter() {
            assert_eq!(back.get(r).to_rgba8(), c.to_rgba8(), "{source} {r}");
        }
        assert_eq!(back.extras(), p.extras());
        assert_eq!(back.name(), p.name());
        assert_eq!(back.source(), p.source());
        assert_eq!(back.is_dark(), p.is_dark());
    }
    // A translucent extra keeps its alpha.
    let p = from_seed(hex("#7aa2f7"), Options::default())
        .with_extras(vec![("glow".into(), Color::from_rgba8(255, 0, 0, 128))]);
    let back = Palette::from_text(&p.to_text()).unwrap();
    assert_eq!(back.extra("glow"), Some(Color::from_rgba8(255, 0, 0, 128)));

    // A file from before the hue roles: no hue, `name` or `extra` lines.
    // The hues are derived and the name is the source string.
    let old: String = import("catppuccin:mocha", None)
        .unwrap()
        .to_text()
        .lines()
        .filter(|l| {
            let k = l.split(' ').next().unwrap_or("");
            !matches!(k, "name" | "extra") && Role::from_name(k).is_none_or(|r| !r.is_hue())
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let back = Palette::from_text(&old).unwrap();
    whole(&back);
    harmonised(&back);
    assert_eq!(back.name(), Some("catppuccin:mocha"));
    assert!(back.extras().is_empty());
}

/// What `insert_into` hands the token table's palette entries, the only
/// thing `$palette.<name>` (templates, `strand export`) reads: the
/// identity first (`name` with its source-string fallback, `source` only
/// when there is one, `dark` a bool), then the extras in source order.
#[test]
fn insert_into_writes_the_identity_then_the_extras_in_order() {
    let text = |s: &str| PropValue::Text(s.into());
    let names =
        |t: &TokenTable| -> Vec<String> { t.palette.iter().map(|(n, _)| n.clone()).collect() };

    let p = import("catppuccin:mocha", None).unwrap();
    let mut t = TokenTable::default();
    p.insert_into(&mut t);
    assert_eq!(t.palette_value("name"), Some(&text("Catppuccin Mocha")));
    assert_eq!(t.palette_value("source"), Some(&text("catppuccin:mocha")));
    assert_eq!(t.palette_value("dark"), Some(&PropValue::Bool(true)));
    assert_eq!(
        t.palette_value("mauve"),
        Some(&PropValue::Color(hex("#cba6f7")))
    );
    let want: Vec<String> = ["name", "source", "dark"]
        .into_iter()
        .chain(CATPPUCCIN_NAMES.iter().copied())
        .map(String::from)
        .collect();
    assert_eq!(names(&t), want);
    let latte = import("catppuccin:latte", None).unwrap();
    latte.insert_into(&mut t);
    assert_eq!(t.palette_value("dark"), Some(&PropValue::Bool(false)));
    assert_eq!(t.palette_value("name"), Some(&text("Catppuccin Latte")));

    // No scheme name: `name` is the source string.
    let dir = temp("insert");
    std::fs::write(
        dir.join("m.json"),
        r##"{ "mode": "dark", "colors": { "dark": {
            "zeta": "#112233", "primary": "#a4c9fe", "alpha": "#445566" } } }"##,
    )
    .unwrap();
    let p = import("matugen:m.json", Some(&dir)).unwrap();
    let mut t = TokenTable::default();
    p.insert_into(&mut t);
    assert_eq!(t.palette_value("name"), Some(&text("matugen:m.json")));
    assert_eq!(
        names(&t),
        ["name", "source", "dark", "zeta", "primary", "alpha"]
    );
    let _ = std::fs::remove_dir_all(dir);

    // No source and no name: neither entry, `dark` alone, the extras after.
    let p = Partial::new()
        .with(Role::Accent, hex("#7aa2f7"))
        .with(Role::Surface, hex("#fafafa"))
        .fill()
        .with_extras(vec![("glow".into(), hex("#ff0000"))]);
    let mut t = TokenTable::default();
    p.insert_into(&mut t);
    assert_eq!(t.palette_value("name"), None);
    assert_eq!(t.palette_value("source"), None);
    assert_eq!(names(&t), ["dark", "glow"]);
    assert_eq!(t.palette_value("dark"), Some(&PropValue::Bool(p.is_dark())));
}

/// A first segment the identity uses is renamed whether it is a colour
/// or a W3C group (`$palette.dark` is a bool, so `$palette.dark.surface`
/// could not be read under it); later segments never are.
#[test]
fn identity_names_are_renamed_as_colours_and_as_groups() {
    use strand_theme::palette::extra_name;
    assert_eq!(extra_name(&["dark"]), "dark_");
    assert_eq!(extra_name(&["Source"]), "source_");
    assert_eq!(extra_name(&["dark", "surface"]), "dark_.surface");
    assert_eq!(extra_name(&["color", "name"]), "color.name");
    let dir = temp("groups");
    std::fs::write(
        dir.join("w.json"),
        r##"{ "dark": { "$type": "color", "surface": { "$value": "#111111" } },
              "color": { "$type": "color", "accent": { "$value": "#ff5500" },
                         "name": { "$value": "#222222" } } }"##,
    )
    .unwrap();
    let p = import("w3c:w.json", Some(&dir)).unwrap();
    let names: Vec<&str> = p.extras().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["dark_.surface", "color.accent", "color.name"]);
    let _ = std::fs::remove_dir_all(dir);
}
