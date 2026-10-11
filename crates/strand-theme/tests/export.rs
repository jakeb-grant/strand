//! `strand export json` and `base24` (decisions.md m5-tools, "the CLI";
//! m5-plan S-palette): the json golden from `catppuccin:mocha`, every
//! value kind, base24's round trips and overlays reaching both.
//!
//! `STRAND_BLESS=1` rewrites the golden.

use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_scene::tokens::OVERLAY_ORIGIN;
use strand_scene::{Color, Insets, Length, PropValue, TokenTable};
use strand_theme::defaults::base_tokens;
use strand_theme::export::{BASE24_SLOTS, base24, json};
use strand_theme::{Options, Palette, Role, from_seed, import};

const SCHEME: &str = include_str!("export/scheme.yaml");

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/export")
        .join(name)
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "strand-export-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The table `strand run` builds for a config with no token set: the
/// built-in base tokens under the palette.
fn table(p: &Palette) -> TokenTable {
    let mut t = base_tokens();
    p.insert_into(&mut t);
    t
}

/// An overlay as S-ipc's layer writes one (`strand set '$fg' …`).
fn overlay(t: &mut TokenTable, path: &str, c: &str) {
    t.insert(path, PropValue::Color(Color::from_hex(c).unwrap()));
    t.set_origin(path, OVERLAY_ORIGIN);
}

/// The `palette:` lines of a base24 file.
fn slots(yaml: &str) -> Vec<&str> {
    yaml.lines().filter(|l| l.starts_with("  base")).collect()
}

#[test]
fn export_json_golden_from_catppuccin_mocha() {
    let p = import("catppuccin:mocha", None).unwrap();
    let mut t = table(&p);
    // One token of every other kind the writer knows, and one it writes
    // as `null`.
    t.insert("probe.percent", PropValue::Length(Length::Percent(40.0)));
    t.insert("probe.ch", PropValue::Length(Length::Ch(4.0)));
    t.insert("probe.auto", PropValue::Length(Length::Auto));
    t.insert("probe.px", PropValue::Length(Length::Px(8.0)));
    t.insert("probe.angle", PropValue::Angle(90.0));
    t.insert(
        "probe.duration",
        PropValue::Duration(Duration::from_millis(150)),
    );
    t.insert("probe.text", PropValue::Text("say \"hi\"".into()));
    t.insert("probe.keyword", PropValue::Keyword("center".into()));
    t.insert("probe.flag", PropValue::Bool(true));
    t.insert("probe.insets", PropValue::Insets(Insets::default()));
    let out = json(&p, &t);
    // It is JSON, and holds what the design lists.
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["name"], "Catppuccin Mocha");
    assert_eq!(v["source"], "catppuccin:mocha");
    assert_eq!(v["dark"], true);
    assert_eq!(v["roles"].as_object().unwrap().len(), 62);
    assert_eq!(v["roles"]["accent"], "#cba6f7");
    assert_eq!(v["roles"]["magenta"], "#f5c2e7");
    assert_eq!(v["extras"].as_object().unwrap().len(), 26);
    assert_eq!(v["extras"]["mauve"], "#cba6f7");
    assert_eq!(v["tokens"]["fg.muted"], "#cdd6f4a6");
    assert_eq!(v["tokens"]["space.2"], 8);
    assert_eq!(v["tokens"]["probe.px"], 8);
    assert_eq!(v["tokens"]["probe.percent"], "40%");
    assert_eq!(v["tokens"]["probe.duration"], 150);
    assert_eq!(v["tokens"]["probe.insets"], serde_json::Value::Null);
    assert_eq!(
        v["tokens"]["font.ui"],
        serde_json::json!({"family": "Inter", "size": 13, "weight": 500})
    );
    assert_eq!(
        v["tokens"]["motion.spatial"],
        serde_json::json!({"spring": {"stiffness": 700, "damping": 0.9}})
    );
    assert_eq!(v["tokens"]["elevation.md"][0]["blur"], 8);
    // Roles in schema order, extras in source order.
    let at = |k: &str| out.find(&format!("\"{k}\":")).unwrap();
    assert!(at("accent") < at("on_accent") && at("on_tertiary_fixed_variant") < at("red"));
    assert!(at("rosewater") < at("flamingo") && at("mantle") < at("crust"));
    // Deterministic.
    assert_eq!(json(&p, &table(&p)), json(&p, &table(&p)));
    let file = golden("catppuccin-mocha.json");
    if std::env::var_os("STRAND_BLESS").is_some() {
        std::fs::write(&file, &out).unwrap();
    }
    let want = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        out,
        want,
        "golden {} (STRAND_BLESS=1 rewrites it)",
        file.display()
    );
}

#[test]
fn a_base24_scheme_passes_through_byte_for_byte() {
    let dir = temp("pass");
    std::fs::write(dir.join("s.yaml"), SCHEME).unwrap();
    let p = import("base24:s.yaml", Some(&dir)).unwrap();
    let out = base24(&p, &table(&p));
    assert_eq!(slots(&out), slots(SCHEME));
    assert!(out.starts_with(
        "system: \"base24\"\nname: \"Strand Test Dark\"\nauthor: \"strand export\"\nvariant: \"dark\"\npalette:\n"
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn any_palette_round_trips_through_base24() {
    let dir = temp("round");
    let palettes = [
        from_seed(Color::from_hex("#7aa2f7").unwrap(), Options::default()).with_source("built-in"),
        from_seed(
            Color::from_hex("#7aa2f7").unwrap(),
            Options {
                dark: true,
                ..Options::default()
            },
        )
        .with_source("built-in"),
        import("catppuccin:mocha", None).unwrap(),
        import("catppuccin:latte", None).unwrap(),
        from_seed(Color::from_hex("#b3261e").unwrap(), Options::default()),
    ];
    let kept = [
        Role::Bg,
        Role::SurfaceLow,
        Role::SurfaceHigh,
        Role::Outline,
        Role::FgVariant,
        Role::Fg,
        Role::SurfaceDim,
        Role::SurfaceLowest,
    ];
    for (i, p) in palettes.iter().enumerate() {
        let out = base24(p, &table(p));
        let file = format!("p{i}.yaml");
        std::fs::write(dir.join(&file), &out).unwrap();
        let back = import(&format!("base24:{file}"), Some(&dir)).unwrap();
        assert_eq!(back.is_dark(), p.is_dark(), "{i}");
        for &r in Role::HUES.iter().chain(&kept) {
            // Latte's `fg_variant` (subtext0) is under 3:1 on its base02
            // (surface1), which the importer reads as `surface_variant`,
            // so the contrast guard moves it on the way back in: the
            // format's loss, as `accent` is (decisions.md m5-palette).
            if i == 3 && r == Role::FgVariant {
                assert_ne!(back.get(r).to_rgba8(), p.get(r).to_rgba8());
                continue;
            }
            assert_eq!(back.get(r).to_rgba8(), p.get(r).to_rgba8(), "{i}: {r}");
        }
        // The extras are what was written, slot for slot.
        for (slot, line) in slots(&out).iter().enumerate() {
            let hex = &line[line.len() - 8..line.len() - 1];
            let want = Color::from_hex(hex).unwrap();
            assert_eq!(
                back.extra(&format!("base{slot:02x}")),
                Some(want),
                "{i}: {line}"
            );
            assert_eq!(want.to_rgba8(), p.get(BASE24_SLOTS[slot]).to_rgba8());
        }
        // And exporting what came back writes the same scheme.
        assert_eq!(slots(&base24(&back, &table(&back))), slots(&out), "{i}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn overlays_reach_both_writers() {
    // `strand set $fg #ffffff` reaches base05 (and base06, also `fg`) of
    // a base16 scheme's base24 export, though the scheme passes through.
    let dir = temp("overlay");
    let base16: String = SCHEME
        .lines()
        .filter(|l| !l.trim_start().starts_with("base1"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(dir.join("b.yaml"), base16).unwrap();
    let p = import("base16:b.yaml", Some(&dir)).unwrap();
    let mut t = table(&p);
    let before = base24(&p, &t);
    assert!(before.contains("  base05: \"#d7dae0\"\n"), "{before}");
    assert!(before.contains("  base06: \"#e3e5ea\"\n"), "{before}");
    overlay(&mut t, "fg", "#ffffff");
    let after = base24(&p, &t);
    assert!(after.contains("  base05: \"#ffffff\"\n"), "{after}");
    assert!(after.contains("  base06: \"#ffffff\"\n"), "{after}");
    // Every slot not on `fg` is unchanged.
    for (a, b) in slots(&before).iter().zip(slots(&after)) {
        if !a.starts_with("  base05") && !a.starts_with("  base06") {
            assert_eq!(*a, b);
        }
    }
    let v: serde_json::Value = serde_json::from_str(&json(&p, &t)).unwrap();
    assert_eq!(v["roles"]["fg"], "#ffffff");
    assert_eq!(v["tokens"]["fg"], "#ffffff");
    assert_eq!(v["tokens"]["fg.muted"], "#ffffffa6");
    let _ = std::fs::remove_dir_all(dir);
}
