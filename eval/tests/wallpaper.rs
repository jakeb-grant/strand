//! (wallpaper-follow) A wallpaper palette on the virtual clock: the
//! quantiser runs off the logic thread in real time, and the driver
//! waits for it as the next tick starts (docs/eval.md, "Driver"), so a
//! palette nothing but `use palette` reads follows the setting, in
//! either mode, on the same ticks every run.

use strand_eval::harness::{RunOptions, run};
use strand_eval::scenario::{Scenario, eval_root};
use strand_eval::trace::RunTrace;
use strand_theme::{Options, Role, from_seed};

fn go(text: &str) -> RunTrace {
    let file = eval_root().join("scenarios/curated/wallpaper.scn");
    let sc = Scenario::parse(text, &file).unwrap_or_else(|e| panic!("{e}"));
    let out = run(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    assert!(out.notes.is_empty(), "{:?}", out.notes);
    out.trace
}

/// The accent of the palette `material(image:)` makes of a fixture, as
/// the trace writes a fill.
fn accent_of(image: &str, dark: bool) -> String {
    let bytes = std::fs::read(eval_root().join("fixtures/images").join(image)).unwrap();
    let seed = strand_theme::image::seed_from_bytes(&bytes).unwrap();
    let c = from_seed(
        seed,
        Options {
            dark,
            ..Options::default()
        },
    )
    .get(Role::Accent);
    strand_eval::paint::hex(c)
}

/// The fallback's accent.
fn seed_accent(dark: bool) -> String {
    let c = from_seed(
        strand_scene::Color::from_hex("#7aa2f7").unwrap(),
        Options {
            dark,
            ..Options::default()
        },
    )
    .get(Role::Accent);
    strand_eval::paint::hex(c)
}

/// #dot's fill in each painted frame, by tick.
fn dots(t: &RunTrace) -> Vec<(u32, String)> {
    let key = t
        .nodes
        .iter()
        .find(|(_, m)| m.id.as_deref() == Some("dot"))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("no #dot in {:?}", t.nodes.keys()));
    t.frames
        .iter()
        .filter_map(|f| {
            let r = f.n.iter().find(|r| r.key == key)?;
            let bg = r.paint.as_ref()?.bg.first()?.clone();
            Some((f.k, bg))
        })
        .collect()
}

/// The page's palette (site/how-to/wallpaper-theme.md), with nothing
/// reading the `Async` but `use palette`.
const CONFIG: &str = r##"
=== theme.strand
state prefs from "prefs.toml" { wallpaper: path = "/nope.png" }
let scheme: Palette = material(image: prefs.wallpaper, dark: system.dark)
                      ?? material(seed: #7aa2f7, dark: system.dark)
use palette scheme

=== bar.strand
bar Top { edge: top; height: 32; bg: $surface
  row { box { id: dot; size: 12; bg: $accent } }
}
"##;

/// A palette that only `use palette` reads follows the setting: before
/// the driver waited for the quantiser, a run's ticks went by in less
/// real time than a quantise takes, and the palette never arrived
/// unless something else (an element reading `.pending`) slowed the
/// run down.
#[test]
fn a_palette_read_only_by_use_palette_follows_the_wallpaper() {
    let text = format!(
        "id = \"wallpaper-follow\"\nscreen = {{ width = 200, height = 40, scale = 1.0 }}\n\
         {CONFIG}\n=== steps\nrest\n\
         set theme.prefs.wallpaper ../../fixtures/images/app.png\nwait 1500\n\
         mark second\n\
         set theme.prefs.wallpaper ../../fixtures/images/art.png\nwait 1500\n"
    );
    let t = go(&text);
    let d = dots(&t);
    let (app, art) = (accent_of("app.png", false), accent_of("art.png", false));
    assert_ne!(app, art);
    assert_eq!(d.first().map(|x| &x.1), Some(&seed_accent(false)), "{d:?}");
    let second = t
        .segments
        .iter()
        .find(|s| s.name == "second")
        .and_then(|s| s.s)
        .unwrap();
    let before = d.iter().rfind(|(k, _)| *k <= second).unwrap();
    assert_eq!(before.1, app, "the first picture's palette: {d:?}");
    assert_eq!(d.last().unwrap().1, art, "the second picture's: {d:?}");
    // On the same ticks every run.
    assert_eq!(go(&text).digest, t.digest);
}

/// The palette of a wallpaper set before boot arrives in the dark mode
/// as in the light, and whichever runs first: each run boots on the
/// fallback and springs to its own mode's palette within a few frames
/// of fixed time, not only when a `rest` happens to last long enough.
#[test]
fn a_seeded_wallpaper_arrives_in_both_modes_in_either_order() {
    let text = |dark: bool| {
        format!(
            "id = \"wallpaper-seeded\"\nscreen = {{ width = 200, height = 40, scale = 1.0 }}\n\
             [services]\n\"system.dark\" = {dark}\n\
             [seed]\n\"theme.prefs.wallpaper\" = '\"../../fixtures/images/app.png\"'\n\
             {CONFIG}\n=== steps\nframes 3\nwait 1500\n"
        )
    };
    for order in [[true, false], [false, true]] {
        for dark in order {
            let d = dots(&go(&text(dark)));
            assert_eq!(
                d.last().map(|x| &x.1),
                Some(&accent_of("app.png", dark)),
                "dark {dark}: {d:?}"
            );
        }
    }
}
