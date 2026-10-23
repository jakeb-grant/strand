//! (replay-clip) `strand-eval replay clip` and the replay page's clip
//! (docs/eval.md, "A clip"): a selection spec (report, slice, elements,
//! note) made into the text an agent is handed, its JSON and a .zip of
//! the slice's frames cropped around the elements, by the one
//! eval/viewer/clip.js the page runs too. Held here: the spec read and
//! refused as typed errors, nodes named by key, `#id` and `file:line`,
//! the output stable, the page and the command line alike (text, JSON,
//! the .zip and its pixels against Rust's own decoding), the page with
//! no URL, and a replayed session's clip against its trace.

mod fx;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fx::*;
use serde_json::Value as Json;
use strand_eval::replay::clip::{self, ClipError, Replay, Spec};
use strand_eval::report;
use strand_eval::trace::{Row, RunTrace};

/// The scripted session replayed into its own scratch tree: the
/// replay's report directory (`out/`).
fn replayed(tag: &str) -> PathBuf {
    let dir = Fx::session().write(tag);
    let (_, out) = replay_of(&dir, &defaults());
    out
}

fn spec(from: &str, to: Option<&str>, nodes: &[&str], note: &str) -> Spec {
    Spec {
        from: from.into(),
        to: to.map(str::to_string),
        nodes: nodes.iter().map(|s| s.to_string()).collect(),
        note: note.into(),
    }
}

fn window(out: &Path) -> f64 {
    let f: Json =
        serde_json::from_str(&std::fs::read_to_string(out.join("fidelity.json")).unwrap()).unwrap();
    f["window"]["seconds"].as_f64().unwrap()
}

/// The key of the node with `id`.
fn key_of(t: &RunTrace, id: &str) -> String {
    t.nodes
        .iter()
        .find(|(_, m)| m.id.as_deref() == Some(id))
        .map(|(k, _)| k.clone())
        .unwrap_or_else(|| panic!("no #{id}"))
}

fn trace_of(out: &Path) -> (RunTrace, report::Entry) {
    let r = Replay::find(out).unwrap();
    let e = report::read_entry(&r.entry).unwrap();
    (e.trace.clone(), e)
}

/// The click lands 8.1 s into the session (the report at 30 s): the
/// badge mounts and pushes the lamp and the spot 40 px along.
fn around_the_click(out: &Path) -> (String, String) {
    let w = window(out);
    let at = |s: f64| format!("{:.3}s", s - 30.0);
    assert!(w > 23.0, "the window reaches back past the click: {w}");
    (at(7.5), at(9.0))
}

#[test]
fn a_clip_names_nodes_by_key_id_and_config_line() {
    let out = replayed("clip-names");
    let (t, _) = trace_of(&out);
    let lamp = key_of(&t, "lamp");
    let origin = t.nodes[&lamp].origin.clone().unwrap();
    // `file:line` and `file:line:col`, the file by its name alone too.
    let mut parts = origin.rsplitn(3, ':');
    let (_col, line, file) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let base = Path::new(file).file_name().unwrap().to_string_lossy();
    let (from, to) = around_the_click(&out);
    let r = Replay::find(&out).unwrap();
    let elements = |sel: &str| -> Vec<String> {
        let m = clip::make(&r, &spec(&from, Some(&to), &[sel], ""), false).unwrap();
        let j: Json = serde_json::from_str(&m.json).unwrap();
        j["elements"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap().to_string())
            .collect()
    };
    for sel in [
        lamp.clone(),
        "#lamp".to_string(),
        origin.clone(),
        format!("{base}:{line}"),
    ] {
        assert_eq!(elements(&sel), std::slice::from_ref(&lamp), "{sel}");
    }
    // A name twice is one element.
    let m = clip::make(&r, &spec(&from, Some(&to), &["#lamp", &lamp], ""), false).unwrap();
    let j: Json = serde_json::from_str(&m.json).unwrap();
    assert_eq!(j["elements"].as_array().unwrap().len(), 1);
}

#[test]
fn a_clip_refuses_what_it_cannot_clip_as_typed_errors() {
    let out = replayed("clip-refuse");
    let r = Replay::find(&out).unwrap();
    let make = |s: Spec| clip::make(&r, &s, false);
    let (from, to) = around_the_click(&out);
    assert!(
        matches!(make(spec(&from, Some(&to), &["#nothing"], "")), Err(ClipError::UnknownNode(m)) if m.contains("#nothing")),
        "an unknown node"
    );
    assert!(
        matches!(
            make(spec(&from, Some(&to), &["shell.strand:999"], "")),
            Err(ClipError::UnknownNode(_))
        ),
        "a line no node is on"
    );
    assert!(
        matches!(
            make(spec(&to, Some(&from), &["#lamp"], "")),
            Err(ClipError::EmptySlice(_))
        ),
        "--from after --to"
    );
    assert!(
        matches!(
            make(spec(&from, Some(&from), &["#lamp"], "")),
            Err(ClipError::EmptySlice(_))
        ),
        "an empty slice"
    );
    assert!(
        matches!(make(spec("soon", None, &[], "")), Err(ClipError::Spec(m)) if m.contains("soon")),
        "a time that does not read"
    );
    assert!(
        matches!(
            make(spec("-1s", Some("2s"), &[], "")),
            Err(ClipError::Spec(_))
        ),
        "after the report"
    );
    assert!(
        matches!(make(spec("-9999s", None, &[], "")), Err(ClipError::Spec(_))),
        "before the window"
    );
    // A report not replayed yet, and a directory that is neither.
    let fresh = Fx::session().write("clip-fresh");
    assert!(
        matches!(Replay::find(&fresh), Err(ClipError::NoReplay(m)) if m.contains("strand replay"))
    );
    let empty = tmp("clip-empty");
    assert!(matches!(Replay::find(&empty), Err(ClipError::NoReplay(_))));
    // Through the command line: usage errors, the message the error's.
    let cli = |args: &[&str]| {
        let mut a = vec!["replay".to_string(), "clip".to_string()];
        a.extend(args.iter().map(|s| s.to_string()));
        strand_eval::main_with(&a)
    };
    let o = out.display().to_string();
    let e = cli(&[&o, "--node", "#lamp"]).unwrap_err().to_string();
    assert!(e.contains("--from is needed"), "{e}");
    let e = cli(&[&o, "--from", &from, "--node", "#nothing"])
        .unwrap_err()
        .to_string();
    assert!(e.contains("#nothing"), "{e}");
    assert!(cli(&["--from", &from]).is_err(), "no report named");
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
    let _ = std::fs::remove_dir_all(&empty);
}

/// The text: the report, the slice in seconds and ticks, the note, the
/// fidelity statement, a table per element with a row per change, and
/// the command that makes it again; the same every time.
#[test]
fn a_clips_text_is_compact_and_stable() {
    let out = replayed("clip-text");
    let r = Replay::find(&out).unwrap();
    let (from, to) = around_the_click(&out);
    let s = spec(&from, Some(&to), &["#lamp", "#button"], "the lamp jumps");
    let a = clip::make(&r, &s, false).unwrap();
    let b = clip::make(&r, &s, false).unwrap();
    assert_eq!(a, b, "stable");
    let t = &a.text;
    assert!(
        t.starts_with("Strand replay clip: \"the bar dipped\""),
        "{t}"
    );
    assert!(
        t.contains(&format!(
            "slice: {} s to {} s before the report, ticks k",
            &from[..from.len() - 1],
            &to[..to.len() - 1]
        )),
        "{t}"
    );
    assert!(t.contains("note: the lamp jumps\n"), "{t}");
    assert!(t.contains("fidelity:\n"), "{t}");
    assert!(t.contains("[1] box#lamp · shell.strand:"), "{t}");
    assert!(t.contains("[2] box#button · shell.strand:"), "{t}");
    assert!(t.contains("regenerate: strand replay clip "), "{t}");
    assert!(
        t.contains(" --node '") && t.contains(" --note 'the lamp jumps'"),
        "{t}"
    );
    // The lamp: where it was, gliding 40 px along once the badge
    // mounts, and at rest there; a row per change, the frames it holds.
    let lamp = t
        .split("[1] ")
        .nth(1)
        .unwrap()
        .split("[2] ")
        .next()
        .unwrap();
    let rows: Vec<&str> = lamp
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with('k') && l[1..].starts_with(|c: char| c.is_ascii_digit()))
        .collect();
    assert!(
        rows[0].contains("(held)") && rows[0].contains("80, 0, 80, 40   same"),
        "{lamp}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("glide") && r.contains("120, 0, 80, 40")),
        "{lamp}"
    );
    let last = rows.last().unwrap();
    assert!(
        last.contains("120, 0, 80, 40  same") && last.ends_with('-'),
        "at rest: {lamp}"
    );
    assert!(
        last.split_whitespace().next().unwrap().contains(".."),
        "a row names its frames: {lamp}"
    );
    // No two rows alike one after the other: a row only where a value
    // changed.
    let cells = |r: &str| r.split_whitespace().skip(2).collect::<Vec<_>>().join(" ");
    for w in rows.windows(2) {
        assert_ne!(cells(w[0]), cells(w[1]), "{lamp}");
    }
    // The fixture's report has no live boxes: nothing compared the
    // frames of the slice, and the clip never says VERIFIED of them.
    let fid = t
        .split("fidelity:\n")
        .nth(1)
        .unwrap()
        .split("elements (")
        .next()
        .unwrap();
    assert!(!fid.contains("VERIFIED"), "{fid}");
    assert!(fid.contains("the replay's own"), "{fid}");
    // The JSON says the same.
    let j: Json = serde_json::from_str(&a.json).unwrap();
    assert_eq!(j["clip"], 1);
    assert_eq!(j["spec"]["note"], "the lamp jumps");
    assert_eq!(j["elements"].as_array().unwrap().len(), 2);
    assert!(j["slice"]["ticks"].as_u64().unwrap() > 0);
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// The frames the verdict held to the live boxes are said to be, and
/// only those: a slice that ends at the report holds the frame compared
/// (VERIFIED, or each difference named), one earlier holds frames
/// nothing compared, said to be the replay's own.
#[test]
fn a_clip_says_which_of_its_frames_were_compared() {
    let dir = Fx::session().write("clip-verified");
    let (r, _) = replay_of(&dir, &defaults());
    let o = origins();
    let (_, bar) = r
        .verdict
        .replayed
        .iter()
        .find(|(s, _)| strand_eval::replay::fidelity::same_origin(s, &o.bar))
        .expect("the bar was drawn at the report");
    let bar = bar.clone();
    write_drawn(&dir, &bar);
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "VERIFIED");
    let k = r.verdict.ticks[0].1;
    let rp = Replay::find(&out).unwrap();
    let surface_lines = |t: &str| -> Vec<String> {
        t.lines()
            .skip_while(|l| *l != "fidelity:")
            .take_while(|l| !l.starts_with("elements ("))
            .filter(|l| l.starts_with("  @") || l.starts_with("    "))
            .map(str::to_string)
            .collect()
    };
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(
        l[0].contains(&format!(
            "k{k} was held to the live shell's last frame: VERIFIED"
        )),
        "{}",
        end.text
    );
    let j: Json = serde_json::from_str(&end.json).unwrap();
    assert_eq!(
        j["fidelity"]["surfaces"][0]["compared_in_slice"],
        serde_json::json!([k])
    );
    // Before it: nothing compared, never VERIFIED.
    let (from, to) = around_the_click(&out);
    let early = clip::make(&rp, &spec(&from, Some(&to), &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&early.text);
    assert!(!l.iter().any(|l| l.contains("VERIFIED")), "{}", early.text);
    assert!(
        l[0].contains("was compared with what the live shell drew: they are the replay's own"),
        "{}",
        early.text
    );
    assert!(
        l[0].contains(&format!("the verdict held k{k}")),
        "{}",
        early.text
    );
    // The lamp 3 px off live: the clip at the report names it.
    let mut moved = bar.clone();
    moved
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("lamp"))
        .unwrap()
        .rect[0] += 3.0;
    write_drawn(&dir, &moved);
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "DRIFTED");
    let rp = Replay::find(&out).unwrap();
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(l[0].ends_with("it differs (1):"), "{}", end.text);
    assert!(
        l[1].contains("lamp") && l[1].contains("3 px off"),
        "{}",
        end.text
    );
    assert!(!l.iter().any(|l| l.contains("VERIFIED")), "{}", end.text);
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// A stored zip's files by name.
fn unzip(b: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
    let mut out = BTreeMap::new();
    let mut i = 0;
    while i + 30 <= b.len() && u32_at(i) == 0x0403_4b50 {
        assert_eq!(u16_at(i + 8), 0, "stored");
        let size = u32_at(i + 18);
        let (n, x) = (u16_at(i + 26), u16_at(i + 28));
        let name = String::from_utf8(b[i + 30..i + 30 + n].to_vec()).unwrap();
        let data = b[i + 30 + n + x..i + 30 + n + x + size].to_vec();
        assert_eq!(crc32(&data) as usize, u32_at(i + 14), "{name}: its CRC");
        out.insert(name, data);
        i += 30 + n + x + size;
    }
    assert_eq!(u32_at(i), 0x0201_4b50, "the central directory follows");
    out
}

/// CRC-32 (IEEE), bit by bit: the test's own.
fn crc32(d: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in d {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

#[test]
fn the_page_and_the_command_line_clip_alike() {
    let out = replayed("clip-parity");
    let r = Replay::find(&out).unwrap();
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    let (from, to) = around_the_click(&out);
    let (t, e) = trace_of(&out);
    let lamp = key_of(&t, "lamp");
    let button = key_of(&t, "button");
    for (s, zip) in [
        (
            spec(&from, Some(&to), &[&lamp, &button], "the lamp jumps"),
            true,
        ),
        (spec(&from, None, &[], ""), true),
        (
            spec("-2s", Some("-0.5s"), &[&lamp], "a quote ' in it"),
            false,
        ),
    ] {
        let cli = clip::make(&r, &s, zip).unwrap();
        // The page's spec as the viewer writes it.
        let pj = serde_json::json!({"from": s.from, "to": s.to, "nodes": s.nodes, "note": s.note})
            .to_string();
        let pg = clip::of_page(&page, &pj, zip).unwrap();
        assert_eq!(pg.text, cli.text, "the text");
        assert_eq!(pg.json, cli.json, "the JSON");
        assert_eq!(pg.zip, cli.zip, "the .zip");
        assert_eq!(pg.missing, cli.missing);
        assert!(cli.missing.is_empty(), "{:?}", cli.missing);
        let Some(z) = cli.zip else { continue };
        // The .zip: the text, the JSON and each frame cropped, pixel for
        // pixel the crop of the frame Rust rebuilds.
        let files = unzip(&z);
        assert_eq!(files["clip.txt"], cli.text.as_bytes());
        assert_eq!(files["clip.json"], cli.json.as_bytes());
        let j: Json = serde_json::from_str(&cli.json).unwrap();
        let mut n = 0;
        for c in j["crop"].as_array().unwrap() {
            let rect: Vec<u32> = c["rect"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap() as u32)
                .collect();
            for f in c["frames"].as_array().unwrap() {
                let img = &e.images[f["image"].as_str().unwrap()];
                let (w, _, px) = report::png::decode(&img.png).unwrap();
                let (cw, ch, cp) =
                    report::png::decode(&files[f["file"].as_str().unwrap()]).unwrap();
                assert_eq!((cw, ch), (rect[2], rect[3]));
                for y in 0..ch {
                    let at = (((rect[1] + y) * w + rect[0]) * 4) as usize;
                    let row = &px[at..at + (cw * 4) as usize];
                    assert_eq!(
                        &cp[(y * cw * 4) as usize..((y + 1) * cw * 4) as usize],
                        row,
                        "row {y}"
                    );
                }
                n += 1;
            }
        }
        assert!(n > 0, "a frame cropped");
        if !s.nodes.is_empty() {
            // The crop: the union of the selection's drawn boxes over the
            // slice, the margin around, clamped to the surface (1280 x 40).
            let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for e in j["elements"].as_array().unwrap() {
                for f in e["frames"].as_array().unwrap() {
                    let Some(d) = f["drawn"].as_array() else {
                        continue;
                    };
                    let d: Vec<f64> = d.iter().map(|v| v.as_f64().unwrap()).collect();
                    x0 = x0.min(d[0]);
                    y0 = y0.min(d[1]);
                    x1 = x1.max(d[0] + d[2]);
                    y1 = y1.max(d[1] + d[3]);
                }
            }
            let m = MARGIN as f64;
            let a = (x0 - m).floor().max(0.0);
            let b = (y0 - m).floor().max(0.0);
            let c = (x1 + m).ceil().min(1280.0);
            let d = (y1 + m).ceil().min(40.0);
            let want: Vec<u64> = [a, b, c - a, d - b].iter().map(|v| *v as u64).collect();
            let got: Vec<u64> = j["crop"][0]["rect"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_u64().unwrap())
                .collect();
            assert_eq!(got, want);
            assert!(x1 > 200.0, "the lamp's overshoot is in the crop: {x1}");
        }
    }
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// clip.js's margin around the selection, px.
const MARGIN: u64 = 8;

/// The replay's page holds no URL: everything it shows, the clip's
/// maker included, is in it.
#[test]
fn the_replay_page_names_no_url() {
    let out = replayed("clip-page");
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    for scheme in [
        "http://",
        "https://",
        "//cdn",
        "src=\"http",
        "url(http",
        "{{CLIP}}",
    ] {
        assert!(!page.contains(scheme), "the page names `{scheme}`");
    }
    assert!(
        page.contains("var StrandClip = (function () {"),
        "clip.js is in the page"
    );
    let data = report::html::embedded(&page).unwrap();
    assert!(
        data["replay"]["record"]["window"]["seconds"]
            .as_f64()
            .unwrap()
            > 0.0,
        "fidelity.json is in the page"
    );
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// The rows of `t` as the frame at tick `k` shows surface `s`: the last
/// frame by then that painted it.
fn rows_at(t: &RunTrace, k: u32, s: u32) -> Option<&Vec<Row>> {
    t.frames
        .iter()
        .take_while(|f| f.k <= k)
        .filter(|f| f.painted.contains(&s))
        .last()
        .map(|f| &f.n)
}

/// `STRAND_CLIP_REPLAY` (scripts/eval/test-report-view.sh --e2e: a
/// session `strand run` recorded on a headless sway, reported and
/// replayed): that replay, else the scripted fixture's. Its whole
/// window clipped, each element's drawn box, rest box, opacity and
/// scale at each frame are the trace's.
#[test]
fn a_replayed_session_clips_as_its_trace() {
    let (out, ours) = match std::env::var_os("STRAND_CLIP_REPLAY") {
        Some(p) => (PathBuf::from(p), false),
        None => (replayed("clip-e2e"), true),
    };
    let r = Replay::find(&out).unwrap();
    let (t, _) = trace_of(&out);
    let w = window(&r.dir);
    let mut keys: Vec<String> = t
        .nodes
        .iter()
        .filter(|(_, m)| m.id.is_some())
        .map(|(k, _)| k.clone())
        .take(6)
        .collect();
    if keys.is_empty() {
        keys = t.nodes.keys().take(6).cloned().collect();
    }
    let s = Spec {
        from: format!("{:.3}s", -w),
        to: None,
        nodes: keys.clone(),
        note: String::new(),
    };
    let m = clip::make(&r, &s, false).unwrap();
    let j: Json = serde_json::from_str(&m.json).unwrap();
    let mut checked = 0;
    for e in j["elements"].as_array().unwrap() {
        let key = e["key"].as_str().unwrap();
        for f in e["frames"].as_array().unwrap() {
            if f["drawn"].is_null() {
                continue;
            }
            let k = f["k"].as_u64().unwrap() as u32;
            let s = f["surface"].as_u64().unwrap() as u32;
            let row = rows_at(&t, k, s)
                .and_then(|rows| rows.iter().find(|r| r.key == key))
                .unwrap_or_else(|| panic!("{key} at k{k}: not in the trace"));
            let q = |v: f32| f64::from(strand_eval::trace::q(v));
            let near = |a: &Json, b: f64| (a.as_f64().unwrap() - b).abs() < 1e-3;
            let v = row.visual;
            for (i, want) in v.iter().enumerate() {
                assert!(
                    near(&f["drawn"][i], q(*want)),
                    "{key} k{k} drawn: {} vs {v:?}",
                    f["drawn"]
                );
            }
            let rest = [
                q(row.laid[0]) + q(row.inherited[0]) - q(row.flight[0]) + q(row.written[0]),
                q(row.laid[1]) + q(row.inherited[1]) - q(row.flight[1]) + q(row.written[1]),
                q(row.laid[2]),
                q(row.laid[3]),
            ];
            for (i, want) in rest.iter().enumerate() {
                assert!(
                    near(&f["rest"][i], *want),
                    "{key} k{k} rest: {} vs {rest:?}",
                    f["rest"]
                );
            }
            assert!(near(&f["opacity"], f64::from(row.op)), "{key} k{k} opacity");
            assert!(
                near(&f["scale"][0], f64::from(row.scale[0])),
                "{key} k{k} scale"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no frame of {keys:?} was drawn in the window");
    if ours {
        // The scripted session: the lamp 40 px along once the badge mounts.
        let lamp = key_of(&t, "lamp");
        let e = j["elements"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["key"] == lamp.as_str())
            .unwrap();
        let xs: Vec<f64> = e["frames"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| f["drawn"][0].as_f64())
            .collect();
        assert!(xs.contains(&80.0) && xs.contains(&120.0), "{xs:?}");
        let _ = std::fs::remove_dir_all(out.parent().unwrap());
    }
}

/// `strand replay clip` tells a clip option's value from the report as
/// `strand-eval replay clip` reads it: the same options take one.
#[test]
fn strand_replay_clip_knows_every_option_that_takes_a_value() {
    let src = std::fs::read_to_string(
        strand_eval::scenario::eval_root().join("../crates/strand/src/report/replay.rs"),
    )
    .unwrap();
    let line = src
        .lines()
        .find(|l| l.starts_with("const CLIP_VALUED"))
        .expect("CLIP_VALUED in strand's replay.rs");
    for o in clip::CLIP_VALUED {
        assert!(
            line.contains(&format!("\"{o}\"")),
            "{o} is not in strand's CLIP_VALUED: {line}"
        );
    }
    assert!(
        line.contains(&format!("; {}]", clip::CLIP_VALUED.len())),
        "{line}"
    );
}
