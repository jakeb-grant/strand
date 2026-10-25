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
    // The frame held at the slice's start, drawn before it, is a row of
    // its own: no frame of the slice joins it.
    assert!(
        !rows[0].split_whitespace().next().unwrap().contains(".."),
        "{lamp}"
    );
    // No two rows of the slice alike one after the other: a row only
    // where a value changed.
    let cells = |r: &str| r.split_whitespace().skip(2).collect::<Vec<_>>().join(" ");
    for w in rows[1..].windows(2) {
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

/// The rows of an element's table in a clip's text: its `[n] ` block.
fn rows_of(text: &str, n: usize) -> Vec<String> {
    text.split(&format!("[{n}] "))
        .nth(1)
        .unwrap()
        .split(&format!("[{}] ", n + 1))
        .next()
        .unwrap()
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with('k') && l[1..].starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

/// A row's first and last tick: `k12..k30` → (12, 30), `k7 (held)` → (7, 7).
fn row_ticks(row: &str) -> (u64, u64) {
    let ks: Vec<u64> = row
        .split_whitespace()
        .take_while(|w| w.starts_with('k') || w.starts_with('('))
        .collect::<String>()
        .split("..")
        .map(|k| {
            k.trim_start_matches('k')
                .split('(')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    (ks[0], *ks.last().unwrap())
}

/// (replay-clip) A slice's ends are exact: given as ticks (`--from kA
/// --to kB`) its header, its JSON and its rows start at kA and end at kB;
/// given as times to the ms (what the page writes for a time between two
/// frames, or the owner types), a tick within the ms is in it, at either
/// end, whatever digits the window's length has. The hints are those in
/// the slice's ticks, never the frame held at its start; the .zip names
/// each crop by the tick its surface painted, and says which frames it
/// lacks.
#[test]
fn a_clips_ends_are_its_ticks() {
    let out = replayed("clip-ticks");
    let r = Replay::find(&out).unwrap();
    let (t, _) = trace_of(&out);
    let w = window(&out);
    let sid = t.surfaces[0].id;
    let period = (1e9_f64 / 60.0).floor();
    let secs = |k: u64| k as f64 * period / 1e9 - w;
    // The bar's paints around the click (the lamp gliding).
    let (lo, hi) = (7.5 - 30.0 + w, 9.5 - 30.0 + w);
    let paints: Vec<u64> = t
        .frames
        .iter()
        .filter(|f| f.painted.contains(&sid))
        .map(|f| u64::from(f.k))
        .filter(|k| {
            let s = *k as f64 * period / 1e9;
            s >= lo && s <= hi
        })
        .collect();
    assert!(paints.len() > 8, "the glide paints: {paints:?}");
    let (ka, kb) = (paints[2], paints[paints.len() - 3]);
    let m = clip::make(
        &r,
        &spec(&format!("k{ka}"), Some(&format!("k{kb}")), &["#lamp"], ""),
        false,
    )
    .unwrap();
    assert!(
        m.text.contains(&format!("ticks k{ka} to k{kb} (")),
        "{}",
        m.text
    );
    let j: Json = serde_json::from_str(&m.json).unwrap();
    assert_eq!(j["slice"]["from_k"], ka);
    assert_eq!(j["slice"]["to_k"], kb);
    assert_eq!(j["slice"]["ticks"], kb - ka + 1);
    assert_eq!(j["spec"]["from_tick"], ka);
    assert_eq!(j["spec"]["to_tick"], kb);
    let steps: Vec<u64> = j["slice"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap())
        .collect();
    let held = j["slice"]["held_from"].as_u64();
    let inside: Vec<u64> = steps
        .iter()
        .copied()
        .skip(usize::from(held.is_some()))
        .collect();
    assert_eq!((inside[0], *inside.last().unwrap()), (ka, kb), "{steps:?}");
    // The rows: the held frame's apart, then from kA to kB.
    let rows = rows_of(&m.text, 1);
    assert!(rows[0].contains("(held)"), "{rows:?}");
    assert_eq!(row_ticks(&rows[1]).0, ka, "{rows:?}");
    assert_eq!(row_ticks(rows.last().unwrap()).1, kb, "{rows:?}");
    assert!(
        m.text
            .contains(&format!(" --from k{ka} --to k{kb} --node ")),
        "a tick given is a tick again: {}",
        m.text
    );
    // The same ends as times to the ms, each tick of the glide: the tick
    // is the slice's end, at either end.
    for &k in &paints {
        let at = format!("{:.3}s", secs(k));
        let m = clip::make(&r, &spec(&at, None, &[], ""), false).unwrap();
        let j: Json = serde_json::from_str(&m.json).unwrap();
        assert_eq!(j["slice"]["from_k"], k, "--from {at}");
        let m = clip::make(&r, &spec(&format!("{:.3}s", -w), Some(&at), &[], ""), false).unwrap();
        let j: Json = serde_json::from_str(&m.json).unwrap();
        assert_eq!(j["slice"]["to_k"], k, "--to {at}");
        let steps = j["slice"]["steps"].as_array().unwrap();
        assert_eq!(steps.last().unwrap(), k, "--to {at}: {steps:?}");
    }
    // Hints: one on the lamp at the frame held at the slice's start (drawn
    // before it), one at kA; only kA's is the clip's.
    let lamp = key_of(&t, "lamp");
    let held = held.expect("a frame held at the slice's start");
    let checks = r.entry.join("checks.json");
    let mut c: Json = std::fs::read_to_string(&checks)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"results": []}));
    let finding =
        |k: u64, msg: &str| serde_json::json!({"sev": "warn", "key": lamp, "k": k, "msg": msg});
    c["results"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "check": "probe",
            "findings": [finding(held, "before the slice"), finding(ka, "in the slice")],
        }));
    std::fs::write(&checks, serde_json::to_vec(&c).unwrap()).unwrap();
    // A frame's pixels gone: the .zip says it lacks it.
    let s = spec(&format!("k{ka}"), Some(&format!("k{kb}")), &["#lamp"], "");
    let full = clip::make(&r, &s, true).unwrap();
    let j: Json = serde_json::from_str(&full.json).unwrap();
    assert!(
        full.text.contains(&format!("probe k{ka} ")),
        "{}",
        full.text
    );
    assert!(!full.text.contains("before the slice"), "{}", full.text);
    let paints_of = |k: u64| {
        t.frames
            .iter()
            .any(|f| u64::from(f.k) == k && f.painted.contains(&sid))
    };
    let crop = &j["crop"][0]["frames"];
    for f in crop.as_array().unwrap() {
        let k = f["k"].as_u64().unwrap();
        assert!(
            paints_of(k),
            "a crop named by k{k}, where its surface did not paint"
        );
        assert_eq!(f["file"], format!("frames/k{k}-s{sid}.png"));
    }
    assert_eq!(j["missing"], serde_json::json!([]));
    let image = crop[1]["image"].as_str().unwrap().to_string();
    let file = crop[1]["file"].as_str().unwrap().to_string();
    for p in [
        r.entry.join(&image),
        r.entry.join(report::pack::patch_file(&image)),
    ] {
        let _ = std::fs::remove_file(p);
    }
    let thin = clip::make(&r, &s, true).unwrap();
    assert!(thin.missing.contains(&file), "{:?}", thin.missing);
    let files = unzip(thin.zip.as_ref().unwrap());
    assert!(!files.contains_key(&file));
    let txt = String::from_utf8(files["clip.txt"].clone()).unwrap();
    assert!(
        txt.contains("frames left out of this .zip") && txt.contains(&file),
        "{txt}"
    );
    let zj: Json = serde_json::from_slice(&files["clip.json"]).unwrap();
    assert!(
        zj["missing"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == file.as_str()),
        "{}",
        zj["missing"]
    );
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// (replay-clip) `--traced N`: the window's start is a warm-up, drawn
/// and not traced. A slice wholly in it is refused saying so (not "the
/// replay's first frame comes later"); one that crosses into the trace
/// says it holds nothing of the part before.
#[test]
fn a_clip_says_what_the_trace_does_not_hold() {
    let dir = Fx::session().write("clip-traced");
    let (_, out) = replay_of(&dir, &traced_20());
    let r = Replay::find(&out).unwrap();
    let w = window(&out);
    assert!(w > 21.0, "{w}");
    let at = |s: f64| format!("{s:.3}s");
    let e = clip::make(&r, &spec(&at(-w + 0.5), Some(&at(-21.0)), &[], ""), false).unwrap_err();
    assert!(
        matches!(&e, ClipError::EmptySlice(m) if m.contains("--traced 20") && m.contains("before the trace starts")),
        "{e:?}"
    );
    let m = clip::make(&r, &spec(&at(-w + 0.5), Some("-5s"), &["#lamp"], ""), false).unwrap();
    assert!(m.text.contains("untraced before -20.000 s"), "{}", m.text);
    let j: Json = serde_json::from_str(&m.json).unwrap();
    assert_eq!(j["fidelity"]["untraced_before"].as_f64(), Some(-20.0));
    let m = clip::make(&r, &spec("-15s", Some("-5s"), &["#lamp"], ""), false).unwrap();
    assert!(!m.text.contains("untraced"), "{}", m.text);
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
    let k = r.verdict.ticks[0].k;
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
    // The lamp moving at the report live (2 px a frame), where the replay
    // drew it: held within its travel, its box left out of the pixels,
    // and the clip says so rather than "within 1 px".
    let mut moving = bar.clone();
    moving
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("lamp"))
        .unwrap()
        .travel = 2.0;
    write_drawn(&dir, &moving);
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    assert_eq!(r.verdict.in_motion, 1);
    assert_eq!(r.verdict.ticks[0].in_motion, 1);
    let rp = Replay::find(&out).unwrap();
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(
        l[0].contains("VERIFIED, every node of it at rest within 1 px")
            && l[0].contains(
                "1 node moving at the report, held within 2 frames of its travel, its box left out of the pixel comparison"
            ),
        "{}",
        end.text
    );
    let j: Json = serde_json::from_str(&end.json).unwrap();
    assert_eq!(j["fidelity"]["surfaces"][0]["in_motion"], 1);
    // 3 px off and moving 0.5 px a frame: past its bound, moving out of
    // step, which the verdict counts as no difference; the clip names it
    // on its own line, never as a difference.
    let mut step = bar.clone();
    let lamp = step
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("lamp"))
        .unwrap();
    lamp.rect[0] += 3.0;
    lamp.travel = 0.5;
    write_drawn(&dir, &step);
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    assert_eq!(r.verdict.moving.len(), 1, "{}", r.verdict.line());
    let rp = Replay::find(&out).unwrap();
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(
        l[0].contains("VERIFIED, every node of it at rest") && !l[0].contains("differs"),
        "{}",
        end.text
    );
    assert!(
        l[1].starts_with("    moving out of step with the live shell (1;"),
        "{}",
        end.text
    );
    assert!(
        l[2].contains("lamp") && l[2].contains("3 px off"),
        "{}",
        end.text
    );
    let j: Json = serde_json::from_str(&end.json).unwrap();
    assert_eq!(
        j["fidelity"]["surfaces"][0]["differences"],
        serde_json::json!([])
    );
    assert_eq!(
        j["fidelity"]["surfaces"][0]["moving_out_of_step"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The verdict's frame is placed by the replayed surface: a tick of
    // another surface is no comparison of this one, whatever its root.
    let fid = out.join("fidelity.json");
    let mut f: Json = serde_json::from_str(&std::fs::read_to_string(&fid).unwrap()).unwrap();
    let sid = f["verdict"]["ticks"][0]["sid"].as_u64().unwrap();
    f["verdict"]["ticks"][0]["sid"] = serde_json::json!(sid + 98);
    std::fs::write(&fid, serde_json::to_vec_pretty(&f).unwrap()).unwrap();
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(!l.iter().any(|l| l.contains("VERIFIED")), "{}", end.text);
    assert!(
        l[0].contains("(the verdict compared no frame of it)"),
        "{}",
        end.text
    );
    // A fidelity.json from before the replayed surface was recorded:
    // placed by its root while no other surface has that root.
    for t in f["verdict"]["ticks"].as_array_mut().unwrap() {
        t.as_object_mut().unwrap().remove("sid");
    }
    std::fs::write(&fid, serde_json::to_vec_pretty(&f).unwrap()).unwrap();
    let end = clip::make(&rp, &spec("-1s", None, &["#lamp"], ""), false).unwrap();
    let l = surface_lines(&end.text);
    assert!(l[0].contains("VERIFIED"), "{}", end.text);
    let _ = std::fs::remove_dir_all(out.parent().unwrap());
}

/// (replay-clip) A replay of Strand's own making, written by hand: a bar
/// (surface 1) and two popups rooted at one place in the config
/// (surfaces 2 and 3, `a.strand:5:1`), the second going at k30; the
/// verdict compared surface 2 at k55, and could not place one live
/// surface.
fn synthetic(tag: &str, verdict: Json) -> Replay {
    let dir = tmp(tag);
    let entry = dir.join("report-synthetic");
    std::fs::create_dir_all(&entry).unwrap();
    let cols = [
        "key", "s", "vx", "vy", "vw", "vh", "lx", "ly", "lw", "lh", "op", "sx", "sy", "hid", "mot",
        "emot",
    ];
    let row = |key: &str, s: u32, x: f64, op: f64, hid: Option<&str>| {
        serde_json::json!([
            key, s, x, 0.0, 10.0, 10.0, x, 0.0, 10.0, 10.0, op, 1.0, 1.0, hid, 0, 0
        ])
    };
    let (bar, p0, p1) = (
        "bar|a.strand:1:1|#0",
        "p|a.strand:5:1|#0",
        "p|a.strand:5:1|#1",
    );
    let (mark, dot) = ("mark|a.strand:7:3|#0", "dot|a.strand:9:3|#0");
    let all = |x: f64| {
        vec![
            row(bar, 1, 0.0, 1.0, None),
            row(p0, 2, 0.0, 1.0, None),
            row(mark, 2, 20.0, 1.0, None),
            row(p1, 3, 0.0, 1.0, None),
            row(dot, 3, x, 1.0, None),
        ]
    };
    let frame = |k: u32, painted: &[u32], sizes: &[u32], n: Vec<Json>| {
        let sz: serde_json::Map<String, Json> = sizes
            .iter()
            .map(|s| (s.to_string(), serde_json::json!([100.0, 40.0])))
            .collect();
        serde_json::json!({"k": k, "painted": painted, "sizes": sz, "n": n, "png": {}})
    };
    let trace = serde_json::json!({
        "cols": cols,
        "scenario": {"hz": 60, "scale": 1.0},
        "nodes": {
            bar: {"label": "bar Top", "origin": "a.strand:1:1"},
            p0: {"label": "popup#p", "origin": "a.strand:5:1"},
            p1: {"label": "popup#p", "origin": "a.strand:5:1"},
            mark: {"label": "box#mark", "origin": "a.strand:7:3", "id": "mark"},
            dot: {"label": "box#dot", "origin": "a.strand:9:3", "id": "dot"},
        },
        "surfaces": [
            {"id": 1, "root": bar, "label": "bar Top", "detached": null},
            {"id": 2, "root": p0, "label": "popup#p", "detached": null},
            {"id": 3, "root": p1, "label": "popup#p", "detached": 30},
        ],
        "frames": [
            frame(10, &[1, 2, 3], &[1, 2, 3], all(30.0)),
            frame(20, &[2, 3], &[1, 2, 3], all(30.0)),
            frame(25, &[3], &[1, 2, 3], all(34.0)),
            frame(40, &[2], &[1, 2], all(34.0)[..3].to_vec()),
            frame(55, &[1, 2], &[1, 2], all(34.0)[..3].to_vec()),
        ],
    });
    std::fs::write(entry.join("trace.json"), trace.to_string()).unwrap();
    let fid = serde_json::json!({
        "bundle": "/reports/synthetic",
        "words": "the dot",
        "window": {"seconds": 1.0, "traced_seconds": 1.0},
        "verdict": verdict,
        "frames": [],
        "divergences": [],
        "limits": [],
    });
    std::fs::write(dir.join("fidelity.json"), fid.to_string()).unwrap();
    Replay { dir, entry }
}

fn verdict_of(ticks: Json, drifted: Json) -> Json {
    serde_json::json!({
        "verdict": "VERIFIED", "line": "VERIFIED: the replay drew all 2 nodes on 1 surface where the live shell did",
        "tolerance": {"px": 1.0, "opacity": 0.02, "moving_frames": 2.0},
        "ticks": ticks, "in_motion": 0, "drifted": drifted, "live_only": [], "replay_only": [],
        "moving_out_of_step": [], "pixels": [], "replay_only_surfaces": [],
        "missing": ["9.1 (its root is not in nodes.json)"],
    })
}

/// The fidelity lines of a clip's text: each `@surface` line and those
/// under it.
fn fidelity_lines(t: &str) -> Vec<String> {
    t.lines()
        .skip_while(|l| *l != "fidelity:")
        .take_while(|l| !l.starts_with("elements ("))
        .filter(|l| l.starts_with("  @") || l.starts_with("    "))
        .map(str::to_string)
        .collect()
}

/// (replay-clip) Two surfaces rooted at one place: the verdict's frame
/// is placed on the one it names (`sid`), never on the other; without
/// `sid` (an older fidelity.json) on neither; what it found on a root two
/// compared surfaces share is said to be on one of them. A surface not
/// compared is said why, as the trace and fidelity.json hold it: gone
/// before the report, or "the verdict compared no frame of it" with the
/// live surface it could not place. The held frame is a row of its own,
/// and a row from a surface's going is `(gone)`.
#[test]
fn a_clip_places_the_verdict_on_the_surface_it_compared() {
    let tick = |sid: u32, k: u32| serde_json::json!({"surface": "a.strand:5:1", "k": k, "sid": sid, "in_motion": 0});
    let spec = spec("k15", None, &["#mark", "#dot", "bar|a.strand:1:1|#0"], "");
    let r = synthetic(
        "clip-place",
        verdict_of(serde_json::json!([tick(2, 55)]), serde_json::json!([])),
    );
    let mut made = vec![r.dir.clone()];
    let m = clip::make(&r, &spec, false).unwrap();
    let l = fidelity_lines(&m.text);
    let on = |at: &str| {
        l.iter()
            .find(|x| x.starts_with(&format!("  @{at}")))
            .cloned()
            .unwrap_or_default()
    };
    let j: Json = serde_json::from_str(&m.json).unwrap();
    let surf = |sid: u64| {
        j["fidelity"]["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["surface"] == sid)
            .unwrap()
            .clone()
    };
    let lines: Vec<&String> = l
        .iter()
        .filter(|x| x.starts_with("  @a.strand:5:1"))
        .collect();
    assert_eq!(lines.len(), 2, "{}", m.text);
    assert!(
        lines[0].contains("k55 was held to the live shell's last frame: VERIFIED"),
        "{}",
        m.text
    );
    assert!(
        lines[1].contains("(it went at k30 (")
            && lines[1].contains("), before the report)")
            && !lines[1].contains("VERIFIED"),
        "{}",
        m.text
    );
    assert_eq!(surf(2)["compared_in_slice"], serde_json::json!([55]));
    assert_eq!(surf(3)["compared_in_slice"], serde_json::json!([]));
    assert!(
        on("a.strand:1:1").contains(
            "(the verdict compared no frame of it; it could not place 1 live surface on a replayed one: 9.1 (its root is not in nodes.json))"
        ),
        "{}",
        m.text
    );
    // Its frames counted apart from the one held at its start.
    assert!(
        lines[0].contains(
            "3 frames painted in this slice, plus k10, drawn before it and held at its start"
        ),
        "{}",
        m.text
    );
    // The rows: the held frame's apart; the dot's from its surface going.
    let mark = rows_of(&m.text, 1);
    assert!(mark[0].starts_with("k10 (held) "), "{mark:?}");
    assert!(mark[1].starts_with("k20..k55 "), "{mark:?}");
    let dot = rows_of(&m.text, 2);
    assert!(
        dot.iter()
            .any(|r| r.starts_with("k30 (gone)..k55 ") && r.contains("not drawn: its surface went")),
        "{dot:?}"
    );
    // An older fidelity.json, no `sid`: two surfaces have that root, so
    // neither is said compared.
    let mut old = tick(2, 55);
    old.as_object_mut().unwrap().remove("sid");
    let r = synthetic(
        "clip-place-old",
        verdict_of(serde_json::json!([old]), serde_json::json!([])),
    );
    made.push(r.dir.clone());
    let m = clip::make(&r, &spec, false).unwrap();
    let l = fidelity_lines(&m.text);
    assert!(!l.iter().any(|x| x.contains("VERIFIED")), "{}", m.text);
    assert!(
        l.iter()
            .filter(|x| x.contains("which 2 surfaces of this replay are"))
            .count()
            == 2,
        "{}",
        m.text
    );
    // Two compared surfaces on one root, and a drift named by the root:
    // on one of them, never VERIFIED.
    let drift = serde_json::json!([{
        "surface": "a.strand:5:1", "node": "box#mark",
        "live": {"box": [20.0, 0.0, 10.0, 10.0], "opacity": 1.0},
        "replay": {"box": [24.0, 0.0, 10.0, 10.0], "opacity": 1.0}, "px": 4.0,
    }]);
    let r = synthetic(
        "clip-place-peers",
        verdict_of(serde_json::json!([tick(2, 55), tick(3, 25)]), drift),
    );
    let m = clip::make(&r, &spec, false).unwrap();
    let l = fidelity_lines(&m.text);
    assert!(!l.iter().any(|x| x.contains("VERIFIED")), "{}", m.text);
    assert_eq!(
        l.iter()
            .filter(|x| x.contains("it or another surface rooted at @a.strand:5:1 differs (1; the verdict compared 2 surfaces"))
            .count(),
        2,
        "{}",
        m.text
    );
    for d in made.iter().chain([&r.dir]) {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// (replay-clip) Two elements of one label and config line (the two
/// popups of `a.strand:5:1`, the second going at k30) are told apart in
/// the text by their key's end and by when each goes; the slice's header
/// counts frames any surface painted, and says over how many.
#[test]
fn a_clip_tells_apart_two_elements_of_one_line() {
    let r = synthetic(
        "clip-twins",
        verdict_of(serde_json::json!([]), serde_json::json!([])),
    );
    let s = spec("k15", None, &["p|a.strand:5:1|#0", "p|a.strand:5:1|#1"], "");
    let m = clip::make(&r, &s, false).unwrap();
    let t = &m.text;
    // k20, k25, k40 and k55 painted, over surfaces 1, 2 and 3.
    assert!(
        t.contains(", 4 frames painted across 3 surfaces; k10, drawn "),
        "{t}"
    );
    assert!(
        t.contains("\n[1] popup#p #0 · a.strand:5 · on popup#p (surface 2)\n"),
        "{t}"
    );
    assert!(
        t.contains("\n[2] popup#p #1 (gone from k30) · a.strand:5 · on popup#p (surface 3)\n"),
        "{t}"
    );
    // One element of a line says nothing more.
    let s = spec("k15", None, &["#mark"], "");
    let m = clip::make(&r, &s, false).unwrap();
    assert!(
        m.text
            .contains("\n[1] box#mark · a.strand:7 · on popup#p (surface 2)\n"),
        "{}",
        m.text
    );
    let _ = std::fs::remove_dir_all(&r.dir);
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
    // Its whole window traced: nothing of it is said to be untraced.
    assert!(!m.text.contains("untraced"), "{}", m.text);
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

/// (window) A clip covers a window as it covers a bar: its elements
/// named by `#id`, on the surface the replay labels `window Settings`,
/// at the size the compositor tiled it.
#[test]
fn a_clip_covers_a_window() {
    let (f, _) = window_session();
    let dir = f.write("clip-window");
    let (_, out) = replay_of(&dir, &defaults());
    let r = Replay::find(&out).unwrap();
    // 6 s to 8 s into the session: tiled, before the close.
    let m = clip::make(
        &r,
        &spec("-24.000s", Some("-22.000s"), &["#wbox"], ""),
        false,
    )
    .unwrap();
    let j: Json = serde_json::from_str(&m.json).unwrap();
    let els = j["elements"].as_array().unwrap();
    assert_eq!(els.len(), 1, "{j}");
    let surfaces = els[0]["surfaces"].as_array().unwrap();
    assert!(
        surfaces.iter().any(|s| s["label"] == "window Settings"),
        "{surfaces:?}"
    );
    assert!(m.text.contains("window Settings"), "{}", m.text);
}
