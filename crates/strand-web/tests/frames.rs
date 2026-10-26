//! The docs examples run natively through strand-web, frame by frame
//! against the frames eval rendered for the site (site/media/*.png and
//! their .json, written by scripts/site/media.sh from
//! eval/scenarios/docs/): the web host must draw what eval draws.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use strand_web::{Options, Screen, Shell};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A docs scenario's config, `[services]` and screen, read the simple
/// way these two files are written.
struct Scn {
    config: String,
    services: Vec<(String, String)>,
    screen: Screen,
}

fn scn(name: &str) -> Scn {
    let text =
        std::fs::read_to_string(root().join(format!("eval/scenarios/docs/{name}.scn"))).unwrap();
    let (head, rest) = text.split_once("=== shell.strand\n").unwrap();
    let config = rest.split("=== steps").next().unwrap().to_string();
    let mut services = Vec::new();
    let mut screen = None;
    let mut in_services = false;
    for line in head.lines() {
        if line.starts_with('[') {
            in_services = line == "[services]";
            continue;
        }
        if let Some(s) = line.strip_prefix("screen") {
            let num = |k: &str| -> f64 {
                let at = s.find(k).unwrap() + k.len();
                let v: String = s[at..]
                    .trim_start_matches([' ', '='])
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                v.parse().unwrap()
            };
            screen = Some(Screen {
                width: num("width") as u32,
                height: num("height") as u32,
                scale: num("scale"),
            });
        }
        if in_services && let Some((k, v)) = line.split_once(" = ") {
            let k = k.trim().trim_matches('"').to_string();
            let v = v.trim();
            let v = v
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
                .unwrap_or(v);
            services.push((k, v.to_string()));
        }
    }
    Scn {
        config,
        services,
        screen: screen.unwrap(),
    }
}

fn fonts() -> Vec<Arc<Vec<u8>>> {
    vec![
        Arc::new(std::fs::read(strand_text::test_font_path()).unwrap()),
        Arc::new(std::fs::read(strand_text::test_bold_font_path()).unwrap()),
    ]
}

fn boot(s: &Scn) -> Shell {
    Shell::load(
        &[("shell.strand".to_string(), s.config.clone())],
        &s.services,
        Options {
            screen: s.screen,
            hz: 60,
            fonts: fonts(),
        },
    )
    .unwrap()
}

/// The site's frames of example `name` (light), and which one each
/// tick shows.
fn site_frames(name: &str) -> (u32, u32, Vec<Vec<u8>>, Vec<usize>) {
    let media = root().join("site/media");
    let j: serde_json::Value =
        serde_json::from_slice(&std::fs::read(media.join(format!("{name}.json"))).unwrap())
            .unwrap();
    let ticks: Vec<usize> = j["modes"]["light"]["ticks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let count = j["modes"]["light"]["count"].as_u64().unwrap() as u32;
    let dec = png::Decoder::new(std::io::BufReader::new(
        std::fs::File::open(media.join(format!("{name}-light.png"))).unwrap(),
    ));
    let mut rd = dec.read_info().unwrap();
    let mut buf = vec![0; rd.output_buffer_size().unwrap()];
    let info = rd.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb);
    let (w, h) = (info.width, info.height / count);
    let frames = (0..count as usize)
        .map(|i| {
            let px = (w * h) as usize;
            buf[i * px * 3..(i + 1) * px * 3]
                .chunks_exact(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect()
        })
        .collect();
    (w, h, frames, ticks)
}

/// The top-left `w × h` of the screen now, straight RGBA.
fn crop(shell: &Shell, w: u32, h: u32) -> Vec<u8> {
    let (sw, _, px) = shell.surfaces().composite();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let at = ((y * sw) * 4) as usize;
        out.extend_from_slice(&px[at..at + (w * 4) as usize]);
    }
    out
}

/// Pixels that differ, and the largest channel difference.
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    assert_eq!(a.len(), b.len());
    let mut n = 0;
    let mut max = 0;
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let d = p
            .iter()
            .zip(q)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        if d > 0 {
            n += 1;
            max = max.max(d);
        }
    }
    (n, max)
}

#[test]
fn hello_bar_draws_what_eval_drew() {
    let s = scn("hello");
    let mut shell = boot(&s);
    let (w, h, frames, ticks) = site_frames("hello");
    shell.rest(600).unwrap();
    assert_eq!(shell.surfaces().shown.len(), 1, "one bar");
    let got = crop(&shell, w, h);
    assert_eq!(
        diff(&got, &frames[ticks[0]]),
        (0, 0),
        "the hello bar at rest"
    );
}

#[test]
fn springs_draw_what_eval_drew_frame_by_frame() {
    let s = scn("springs");
    let mut shell = boot(&s);
    let (w, h, frames, ticks) = site_frames("springs");
    let mut got = Vec::new();
    // The scenario's steps: rest; set on true; rest; set on false; rest.
    let run = |shell: &mut Shell, got: &mut Vec<Vec<u8>>| loop {
        shell.tick().unwrap();
        got.push(crop(shell, w, h));
        if shell.at_rest() || got.len() > 600 {
            break;
        }
    };
    run(&mut shell, &mut got);
    shell.set("on", "true").unwrap();
    run(&mut shell, &mut got);
    shell.set("on", "false").unwrap();
    run(&mut shell, &mut got);
    assert_eq!(got.len(), ticks.len(), "as many ticks as eval ran");
    let bad: Vec<(usize, (usize, u8))> = got
        .iter()
        .enumerate()
        .map(|(k, f)| (k, diff(f, &frames[ticks[k]])))
        .filter(|(_, d)| d.0 > 0)
        .collect();
    assert!(bad.is_empty(), "ticks that differ (pixels, max): {bad:?}");
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
    let mut shell = Shell::load(
        &[("shell.strand".into(), config.into())],
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
    .unwrap();
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
    let mut shell = Shell::load(
        &[("shell.strand".into(), config.into())],
        &[],
        Options {
            screen: Screen {
                width: 400,
                height: 300,
                scale: 1.0,
            },
            hz: 60,
            fonts: fonts(),
        },
    )
    .unwrap();
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
