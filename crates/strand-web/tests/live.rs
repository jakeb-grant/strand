//! The docs examples run natively through strand-web as their figures'
//! live mode runs them (strand-site's `example::live`, docs/
//! live-examples.md), frame by frame against the frames eval drew for
//! the site: every frame painted, in both modes, has the time and the
//! pixel hash the example's media keep (site/media/<name>.json, written
//! by scripts/site/media.sh from runs held to their approved reference
//! recordings, eval/refs/). The icons come only from the files a page
//! supplies (`live::icons_drawn`, `live::install_icons`), as on the web.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use strand_web::live::{Example, icons_drawn, install_icons};
use strand_web::steps::Advance;
use strand_web::{Options, Screen, Shell};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The icon lookup is the process's: tests that boot shells run one at
/// a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn fonts() -> Vec<Arc<Vec<u8>>> {
    vec![
        Arc::new(std::fs::read(strand_text::test_font_path()).unwrap()),
        Arc::new(std::fs::read(strand_text::test_bold_font_path()).unwrap()),
    ]
}

fn examples() -> Vec<Example> {
    let v = strand_site::example::all_live(&root()).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| Example::from_json(&e.to_string()).unwrap())
        .collect()
}

/// What eval drew for `name` in `mode`: each frame's time (ms from the
/// first, to a tenth) and pixel hash.
fn eval_frames(name: &str, mode: &str) -> Vec<(f64, String)> {
    let path = root().join(format!("site/media/{name}.json"));
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let m = &j["modes"][mode];
    let hashes: Vec<&str> = m["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    let ticks = m["ticks"].as_array().unwrap();
    let t = j["t"].as_array().unwrap();
    assert_eq!(ticks.len(), t.len(), "{name}");
    t.iter()
        .zip(ticks)
        .map(|(t, i)| {
            (
                t.as_f64().unwrap(),
                hashes[i.as_u64().unwrap() as usize].to_string(),
            )
        })
        .collect()
}

/// The frames `ex` paints in live mode, its steps run to their end:
/// each one's time (as the media round it) and pixel hash.
fn live_frames(ex: &Example, dark: bool) -> Vec<(f64, String)> {
    let (mut shell, mut player) = ex.boot(None, dark, false, fonts()).unwrap();
    shell.record = true;
    while player.advance(&mut shell).unwrap() != Advance::Done {}
    assert!(shell.notes.is_empty(), "{}: {:?}", ex.name, shell.notes);
    let surfaces: Vec<_> = shell.paints.iter().map(|p| p.surface).collect();
    assert!(
        surfaces.windows(2).all(|w| w[0] == w[1]),
        "{}: an example draws one surface",
        ex.name
    );
    let t0 = shell.paints.first().map(|p| p.t).unwrap_or_default();
    shell
        .paints
        .iter()
        .map(|p| {
            let ms = (p.t - t0).as_secs_f64() * 1000.0;
            ((ms * 10.0).round() / 10.0, p.hash.clone())
        })
        .collect()
}

#[test]
fn every_docs_example_draws_every_frame_eval_drew() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let all = examples();
    let names: Vec<&str> = all.iter().map(|e| e.name.as_str()).collect();
    assert!(names.len() >= 5 && names.contains(&"springs"), "{names:?}");
    // The icons the examples draw, found on this machine's disk, then
    // supplied as the only icons there are, as a page supplies them.
    strand_icons::set_base_dirs(None);
    strand_icons::set_files(Default::default());
    let icons = icons_drawn(&all, &fonts()).unwrap();
    assert!(
        icons
            .iter()
            .any(|(r, _)| r.ends_with(".svg") || r.ends_with(".png")),
        "the OSD and the toasts draw icons: {icons:?}"
    );
    install_icons(
        icons
            .iter()
            .map(|(rel, abs)| (rel.clone(), std::fs::read(abs).unwrap()))
            .collect(),
    );
    let mut bad = Vec::new();
    for ex in &all {
        for (mode, dark) in [("light", false), ("dark", true)] {
            let want = eval_frames(&ex.name, mode);
            let got = live_frames(ex, dark);
            if got.len() != want.len() {
                bad.push(format!(
                    "{} {mode}: {} frames, eval drew {}",
                    ex.name,
                    got.len(),
                    want.len()
                ));
            }
            for (j, (g, w)) in got.iter().zip(&want).enumerate() {
                if g != w {
                    bad.push(format!(
                        "{} {mode}: frame {j}: at {} ms {}, eval's at {} ms {}",
                        ex.name,
                        g.0,
                        &g.1[..12],
                        w.0,
                        &w.1[..12]
                    ));
                    break;
                }
            }
        }
    }
    strand_icons::set_base_dirs(None);
    strand_icons::set_files(Default::default());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Without the supplied icons (a page that failed to fetch them), the
/// OSD draws no icon: the frames differ from eval's, so the test above
/// holds the icons too.
#[test]
fn the_osd_without_its_icon_is_not_what_eval_drew() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ex = examples().into_iter().find(|e| e.name == "osd").unwrap();
    install_icons(Vec::new());
    let got = live_frames(&ex, false);
    strand_icons::set_base_dirs(None);
    let want = eval_frames("osd", "light");
    assert_eq!(got.len(), want.len());
    assert!(
        got.iter().zip(&want).any(|(g, w)| g.1 != w.1),
        "the icon is drawn from nothing"
    );
}

fn boot_config(config: &str, w: u32, h: u32) -> Shell {
    Shell::load(
        &[("shell.strand".into(), config.into())],
        &[],
        Options {
            screen: Screen {
                width: w,
                height: h,
                scale: 1.0,
            },
            hz: 60,
            fonts: fonts(),
        },
    )
    .unwrap()
}

#[test]
fn a_config_with_an_error_gives_its_diagnostic() {
    let e = Shell::load(
        &[(
            "shell.strand".into(),
            "bar Top {\n  height: 32\n  text battery.nope\n}\n".into(),
        )],
        &[],
        Options {
            screen: Screen {
                width: 400,
                height: 100,
                scale: 1.0,
            },
            hz: 60,
            fonts: fonts(),
        },
    )
    .unwrap_err();
    assert!(e.short[0].starts_with("shell.strand:3:"), "{:?}", e.short);
    assert!(e.text.contains("nope"), "{}", e.text);
}

#[test]
fn a_click_reaches_logic() {
    let config = "state n = 0\n\nbar Top {\n  height: 40\n  box { width: 100; height: 40; on click { n = n + 1 } }\n}\n";
    let mut shell = boot_config(config, 400, 100);
    shell.rest(60).unwrap();
    shell.pointer_move([10.0, 10.0]).unwrap();
    shell.pointer_button(true).unwrap();
    shell.pointer_button(false).unwrap();
    shell.tick().unwrap();
    assert_eq!(shell.get("n").unwrap(), "1");
    assert!(shell.notes.is_empty(), "{:?}", shell.notes);
}

#[test]
fn a_click_opens_a_popup_below_its_bar() {
    let config = "state open = false\n\nbar Top {\n  edge: top; height: 32\n  text \"menu\" {\n    on click { open = !open }\n    popup { open: <-> open; box { width: 120; height: 80; bg: $surface } }\n  }\n}\n";
    let mut shell = boot_config(config, 400, 300);
    shell.rest(60).unwrap();
    assert_eq!(shell.surfaces().shown.len(), 1);
    shell.pointer_move([10.0, 16.0]).unwrap();
    shell.pointer_button(true).unwrap();
    shell.pointer_button(false).unwrap();
    shell.rest(600).unwrap();
    let shown = &shell.surfaces().shown;
    assert_eq!(
        shown.len(),
        2,
        "the bar and its popup; notes {:?}",
        shell.notes
    );
    let popup = &shown[1];
    assert_eq!(popup.kind, strand_scene::NodeKind::Popup);
    assert!(popup.origin[1] >= 32.0, "below the bar: {:?}", popup.origin);
    assert!(shell.surfaces().bottom() > 32.0 + 80.0 - 1.0);
}

/// A frame composited for a page is the surface's own pixels, as eval's
/// PNG holds them, when one surface fills the view; nothing is shown
/// before any surface is.
#[test]
fn the_view_is_the_one_surface_as_eval_draws_it() {
    let config = "bar Top {\n  height: 40\n  bg: $surface\n  box { width: 20; height: 20; radius: 4; bg: $accent }\n}\n";
    let mut shell = boot_config(config, 300, 200);
    shell.rest(60).unwrap();
    let s = shell.surfaces();
    let [x, y, w, h] = s.view().unwrap();
    assert_eq!([x, y, w, h], [0, 0, 300, 40]);
    let mut out = Vec::new();
    s.composite_into([x, y, w, h], &mut out);
    let sh = &s.shown[0];
    assert_eq!(out, strand_web::surfaces::to_rgba(&sh.pixels));
    // A larger rect: the rest of the screen is clear.
    s.composite_into([0, 0, 300, 60], &mut out);
    assert_eq!(&out[300 * 40 * 4..], &vec![0u8; 300 * 20 * 4][..]);
}
