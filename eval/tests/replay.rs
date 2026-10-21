//! (report) `strand-eval replay`: a glitch report's bundle as a scenario,
//! replayed and held to what the live shell recorded (docs/eval.md,
//! "Replaying a glitch report"; features.md "Glitch reports").
//!
//! The bundles are written here from scratch, as `strand report` writes
//! them: both rings through `strand_core::flight::Ring`, `bundle.json`,
//! `services.json`, `cells.json`, `nodes.json` and a copy of the fixture
//! shell (`eval/fixtures/report/shell.strand`). The node origins they
//! name are the sandbox's own, read from a run of the same shell.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value as Json, json};
use strand_core::flight::{self, Rec, Ring, gap, kind, request};
use strand_eval::harness::{self, RunOptions};
use strand_eval::replay::{self, Bundle, Options};
use strand_eval::scenario::Scenario;
use strand_eval::trace::BuildInfo;

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const LIVE: &str = "/live/config";

fn shell() -> String {
    std::fs::read_to_string(strand_eval::scenario::eval_root().join("fixtures/report/shell.strand"))
        .unwrap()
}

/// The sandbox's origins of the fixture's nodes: the bar's root and
/// each `id:`.
#[derive(Debug)]
struct Origins {
    bar: String,
    button: String,
    lamp: String,
    spot: String,
}

fn origins() -> &'static Origins {
    static O: OnceLock<Origins> = OnceLock::new();
    O.get_or_init(|| {
        let dir = tmp("probe");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("shell.strand"), shell()).unwrap();
        let scn = dir.join("scenarios/local/probe.scn");
        std::fs::create_dir_all(scn.parent().unwrap()).unwrap();
        std::fs::write(
            &scn,
            format!(
                "id = \"probe\"\nscreen = {{ width = 1280, height = 720, scale = 1.0 }}\nconfig_dir = \"{}\"\n\n=== steps\nrest\n",
                config.display()
            ),
        )
        .unwrap();
        let sc = Scenario::load(&scn).unwrap();
        let t = harness::run(&sc, &RunOptions::default()).unwrap().trace;
        let of = |id: &str| {
            t.nodes
                .values()
                .find(|m| m.id.as_deref() == Some(id))
                .and_then(|m| m.origin.clone())
                .unwrap_or_else(|| panic!("no #{id}"))
        };
        let bar = t.nodes[&t.surfaces[0].root].origin.clone().unwrap();
        Origins {
            bar,
            button: of("button"),
            lamp: of("lamp"),
            spot: of("spot"),
        }
    })
}

fn tmp(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("strand-replay-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn bits(index: u32, gen_: u32) -> u64 {
    u64::from(index) | (u64::from(gen_) << 32)
}

/// A bundle being written.
struct Fx {
    input: Ring,
    changes: Ring,
    now: u64,
    services: Json,
    cells: Json,
    nodes: serde_json::Map<String, Json>,
    surfaces: Vec<Json>,
    outputs: Vec<Json>,
    missing: Vec<Json>,
    config: String,
    format: u64,
}

impl Fx {
    /// The scripted session: the bar mapped on DP-1, the pointer in, a
    /// hover on the spot, a click on the button (its cell written), and
    /// `mood.level` set to 7; the report at 30 s.
    fn session() -> Fx {
        let o = origins();
        let mut f = Fx {
            input: Ring::new(flight::INPUT, flight::ring::INPUT),
            changes: Ring::new(flight::CHANGES, flight::ring::CHANGES),
            now: 30 * S,
            services: json!({"mood": {"running": true, "fields": {"level": 7}}}),
            cells: json!({"shell.clicks": 1}),
            nodes: serde_json::Map::new(),
            surfaces: Vec::new(),
            outputs: Vec::new(),
            missing: Vec::new(),
            config: shell(),
            format: 1,
        };
        f.output(0, "DP-1", 1280, 720, 120, 900 * MS);
        f.node(bits(3, 1), &o.bar, None);
        f.node(bits(7, 1), &o.button, Some("button"));
        f.node(bits(9, 1), &o.lamp, Some("lamp"));
        f.node(bits(11, 1), &o.spot, Some("spot"));
        f.surface(1, 0, bits(3, 1), &o.bar, S, 16_666_667);
        f.pointer(1, kind::ENTER, 40.0, 20.0, 5 * S);
        f.pointer(1, kind::MOTION, 200.0, 20.0, 6 * S);
        f.pointer(1, kind::MOTION, 40.0, 20.0, 7 * S);
        f.press(1, 40.0, 20.0, 8 * S, Some(bits(7, 1)));
        // `on click` runs on the release.
        f.cell(8 * S + 100 * MS, "shell.clicks", "[0,1]");
        f.field(12 * S, "mood.level", "[1,7]");
        f
    }

    fn output(&mut self, slot: u16, name: &str, w: u16, h: u16, scale120: u16, t: u64) {
        let i = self.input.name(name);
        self.input.push(
            Rec::new(t, kind::OUTPUT, slot)
                .u16_at(0, i)
                .u16_at(2, w)
                .u16_at(4, h)
                .u16_at(6, scale120),
        );
        self.outputs.push(json!({
            "name": name, "make": "Fixture", "model": "Panel",
            "scale": f64::from(scale120) / 120.0, "size": [w, h], "position": [0, 0]
        }));
    }

    fn node(&mut self, b: u64, origin: &str, id: Option<&str>) {
        self.nodes.insert(
            format!("{}.{}", b as u32, (b >> 32) as u32),
            json!({"origin": format!("{LIVE}/{origin}"), "id": id, "key": null}),
        );
    }

    /// Surface `slot` mapped at `t` on output slot `out` with root `root`.
    fn surface(&mut self, slot: u16, out: u8, root: u64, origin: &str, t: u64, period: u32) {
        let name = self.changes.name(&format!("{LIVE}/{origin}"));
        let mut rec = Rec::new(t - MS, kind::ROOT, name).u64_at(0, root);
        rec.payload[8] = 0;
        self.changes.push(rec);
        self.input
            .push(Rec::new(t, kind::REFRESH, slot).u32_at(0, period));
        self.input.push(
            Rec::new(t, kind::MAPPED, slot)
                .u64_at(0, root)
                .u8_at(8, out)
                .u8_at(9, 0),
        );
        let output = self
            .outputs
            .get(usize::from(out))
            .map(|o| o["name"].clone());
        self.surfaces.push(json!({
            "slot": slot, "namespace": "strand-bar", "kind": "bar",
            "root": format!("{}.{}", root as u32, (root >> 32) as u32),
            "output": output, "backend": "cpu", "size": [1280, 40], "buffer": [1280, 40],
            "scale": 1.0, "frame": null, "repainted": 0
        }));
    }

    fn pointer(&mut self, slot: u16, k: u8, x: f32, y: f32, t: u64) {
        self.input
            .push(Rec::new(t, k, slot).f32_at(0, x).f32_at(4, y));
    }

    fn press(&mut self, slot: u16, x: f32, y: f32, t: u64, hit: Option<u64>) {
        let b = |t: u64, pressed: u8| {
            Rec::new(t, kind::BUTTON, slot)
                .f32_at(0, x)
                .f32_at(4, y)
                .u16_at(8, 0)
                .u8_at(10, pressed)
        };
        self.input.push(b(t, 1));
        if let Some(h) = hit {
            self.input.push(Rec::new(t, kind::HIT, slot).u64_at(0, h));
        }
        self.input.push(b(t + 100 * MS, 0));
    }

    fn change(&mut self, t: u64, k: u8, name: &str, inline: &[u8], value: &str) {
        let v = value.to_string();
        self.changes.push_named(
            t,
            k,
            name,
            inline,
            0,
            Some(&mut |w: &mut flight::ArenaWriter<'_>| w.put(v.as_bytes())),
        );
    }

    fn field(&mut self, t: u64, name: &str, value: &str) {
        self.change(t, kind::FIELD, name, &[], value);
    }

    fn cell(&mut self, t: u64, name: &str, value: &str) {
        self.change(t, kind::CELL, name, &[], value);
    }

    /// Writes the bundle into a new directory and returns it.
    fn write(&self, tag: &str) -> PathBuf {
        let dir = tmp(tag).join("reports/2026-10-20_14-03-07");
        self.write_to(&dir);
        dir
    }

    fn write_to(&self, dir: &Path) {
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        std::fs::write(dir.join("config/shell.strand"), &self.config).unwrap();
        std::fs::write(dir.join("input.flight"), self.input.dump(self.now)).unwrap();
        std::fs::write(dir.join("changes.flight"), self.changes.dump(self.now)).unwrap();
        let bundle = json!({
            "format": self.format,
            "words": "the bar dipped",
            "wall": "2026-10-20T14:03:07.000+02:00",
            "now_ns": self.now,
            // 2026-10-20T12:03:07Z, less the time since the epoch.
            "epoch_unix_ns": 1_792_497_787_000_000_000i64 - self.now as i64,
            "strand": {"version": "0.0.0", "commit": "fixture"},
            "config_dir": LIVE,
            "outputs": self.outputs,
            "surfaces": self.surfaces,
            "replay": true,
            "files": [],
            "missing": self.missing,
        });
        let put = |name: &str, v: &Json| {
            std::fs::write(dir.join(name), serde_json::to_vec_pretty(v).unwrap()).unwrap();
        };
        put("bundle.json", &bundle);
        put("services.json", &self.services);
        put("cells.json", &self.cells);
        put("nodes.json", &Json::Object(self.nodes.clone()));
        put("diagnostics.json", &json!({"compositor": {"globals": []}}));
        std::fs::write(dir.join("words.txt"), "the bar dipped\n").unwrap();
    }
}

fn plan_of(dir: &Path, opts: &Options) -> (Bundle, replay::Plan, String) {
    let b = Bundle::read(dir).unwrap();
    let p = replay::plan::plan(&b, opts).unwrap();
    let text = p.scenario(&b, "report-fixture");
    (b, p, text)
}

fn defaults() -> Options {
    Options {
        output: None,
        traced: Some(replay::plan::TRACED_S),
        gpu: false,
    }
}

/// Replays `dir` into a scratch tree: the scenario under its own
/// `scenarios/local/`, the report under `out/`.
fn replay_of(dir: &Path, opts: &Options) -> (replay::Replayed, PathBuf) {
    let root = dir.parent().unwrap().parent().unwrap().to_path_buf();
    let to = root.join("scenarios/local/report-fixture.scn");
    let out = root.join("out");
    let r = replay::replay(dir, opts, Some(&to), &out, None, BuildInfo::default()).unwrap();
    (r, out)
}

fn steps(text: &str) -> Vec<&str> {
    text.split("=== steps\n").nth(1).unwrap().lines().collect()
}

#[test]
fn a_bundle_converts_to_a_scenario_that_parses() {
    let dir = Fx::session().write("parse");
    let (_, p, text) = plan_of(&dir, &defaults());
    let root = dir.parent().unwrap().parent().unwrap();
    let scn = root.join("scenarios/local/report-fixture.scn");
    std::fs::create_dir_all(scn.parent().unwrap()).unwrap();
    std::fs::write(&scn, &text).unwrap();
    let sc = Scenario::load(&scn).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(sc.hz, 60);
    assert_eq!(p.start, S, "the window starts at the shell's first surface");
    assert_eq!(sc.warmup, 9_000, "all but the last 20 s are the warm-up");
    assert!(sc.wall.is_some());
    assert_eq!(p.output.name, "DP-1");
    assert!(text.contains("config_dir = "), "{text}");
    assert!(text.contains("fonts = \"local\""));
    assert!(
        text.contains("wall = \"2026-10-20T14:02:38.000+02:00\""),
        "the window's start, 29 s before: {text}"
    );
    let o = origins();
    let st = steps(&text);
    for want in [
        format!("pointer 40.00,20.00 on @{}", o.bar),
        format!("pointer 200.00,20.00 on @{}", o.bar),
        "press left".into(),
        "release left".into(),
        "set mood.level 7".into(),
        "mark traced".into(),
        "mark -5s".into(),
        "mark -2s".into(),
        "mark report".into(),
        "shot report".into(),
    ] {
        assert!(st.contains(&want.as_str()), "no `{want}` in\n{text}");
    }
    // The press is at 8 s, 7 s into the window: 420 ticks.
    let mut tick = 0u64;
    let mut press_at = None;
    for l in &st {
        if let Some(n) = l.strip_prefix("frames ") {
            tick += n.parse::<u64>().unwrap();
        }
        if *l == "press left" {
            press_at = Some(tick);
        }
    }
    assert_eq!(press_at, Some(420));
    assert_eq!(
        tick,
        1740 + 1,
        "the window's 29 s, then a frame after the shot"
    );
    // A newer format is refused by its number.
    let mut newer = Fx::session();
    newer.format = 2;
    let e = Bundle::read(&newer.write("newer")).unwrap_err().to_string();
    assert!(e.contains("format 2 is newer"), "{e}");
}

#[test]
fn the_window_starts_after_the_last_reload_or_gap() {
    let mut f = Fx::session();
    f.change(3 * S, kind::RELOAD, "reload", &[1, 0, 0, 0, 0, 0], "{}");
    let (_, p, _) = plan_of(&f.write("reload"), &defaults());
    assert_eq!(p.start, 3 * S);
    assert!(p.start_why.contains("reload"), "{}", p.start_why);
    assert!(
        p.limits
            .iter()
            .any(|l| l.contains("starts after the reload")),
        "{:?}",
        p.limits
    );
    // A lock's end, later, wins.
    f.input
        .push(Rec::new(4 * S, kind::GAP_BEGINS, 0).u8_at(0, gap::LOCK));
    f.input
        .push(Rec::new(5 * S - MS, kind::GAP_ENDS, 0).u8_at(0, gap::LOCK));
    let (_, p, text) = plan_of(&f.write("gap"), &defaults());
    assert_eq!(p.start, 5 * S - MS);
    assert!(p.start_why.contains("a lock"), "{}", p.start_why);
    // The surface mapped before the window is still replayed (from
    // `bundle.json`), and its input is on its timeline from the start.
    assert!(
        steps(&text)
            .iter()
            .any(|l| l.starts_with("pointer 40.00,20.00")),
        "{text}"
    );
}

#[test]
fn the_busiest_output_is_replayed() {
    let o = origins();
    let mut f = Fx::session();
    f.output(1, "HDMI-A-1", 2560, 1440, 180, 200 * MS);
    f.node(bits(5, 1), &o.bar, None);
    f.surface(2, 1, bits(5, 1), &o.bar, S + 100 * MS, 6_944_444);
    for i in 0..5 {
        f.pointer(
            2,
            kind::MOTION,
            100.0 + i as f32,
            10.0,
            25 * S + i * 100 * MS,
        );
    }
    let (_, p, text) = plan_of(&f.write("busy"), &defaults());
    assert_eq!(p.output.name, "HDMI-A-1", "{}", p.output.why);
    assert_eq!(
        (p.output.width, p.output.height, p.output.scale, p.output.hz),
        (2560, 1440, 1.5, 144)
    );
    assert!(
        text.contains("screen = { width = 2560, height = 1440, scale = 1.5 }"),
        "{text}"
    );
    assert!(
        p.limits.iter().any(|l| l.starts_with("other outputs:")),
        "{:?}",
        p.limits
    );
    assert!(
        p.limits
            .iter()
            .any(|l| l.contains("output DP-1 is not replayed"))
    );
    // `--output` names the other.
    let (_, p, _) = plan_of(
        &f.write("busy2"),
        &Options {
            output: Some("DP-1".into()),
            ..defaults()
        },
    );
    assert_eq!((p.output.name.as_str(), p.output.hz), ("DP-1", 60));
    let b = Bundle::read(&f.write("busy3")).unwrap();
    let e = replay::plan::plan(
        &b,
        &Options {
            output: Some("VGA-9".into()),
            ..defaults()
        },
    )
    .unwrap_err();
    assert!(e.to_string().contains("no such output"), "{e}");
}

#[test]
fn rewinding_reaches_each_value_at_the_start() {
    let mut f = Fx::session();
    f.field(20 * S, "mood.level", "[7,3]");
    f.services["mood"]["fields"]["level"] = json!(3);
    f.cell(21 * S, "shell.clicks", "[1,5]");
    f.cells["shell.clicks"] = json!(5);
    let (_, p, text) = plan_of(&f.write("rewind"), &defaults());
    assert_eq!(p.seeds["mood.level"], json!(1), "back through 3 and 7 to 1");
    assert_eq!(p.seeds["shell.clicks"], json!(0));
    assert!(text.contains("\"mood.level\" = \"1\""), "{text}");
    let st = steps(&text);
    let i7 = st.iter().position(|l| *l == "set mood.level 7").unwrap();
    let i3 = st.iter().position(|l| *l == "set mood.level 3").unwrap();
    assert!(i7 < i3);
    assert!(
        !st.iter().any(|l| l.starts_with("set shell.")),
        "cells are seeded, never driven"
    );
}

#[test]
fn a_lost_value_stops_its_field_and_is_noted() {
    let mut f = Fx::session();
    let big = format!("[7,\"{}\"]", "x".repeat(flight::VALUE_CAP + 10));
    f.field(20 * S, "mood.level", &big);
    let (_, p, text) = plan_of(&f.write("lost"), &defaults());
    assert_eq!(
        p.seeds["mood.level"],
        json!(7),
        "it starts at the oldest value known"
    );
    assert!(
        p.notes
            .iter()
            .any(|n| n.contains("`mood.level`") && n.contains("over the value cap")),
        "{:?}",
        p.notes
    );
    assert!(
        !steps(&text).iter().any(|l| l.starts_with("set mood.level")),
        "{text}"
    );
    // The cell rewinds as before.
    assert_eq!(p.seeds["shell.clicks"], json!(0));
}

#[test]
fn keyed_updates_replay_by_key() {
    let mut f = Fx::session();
    f.services["mood"]["fields"]["items"] = json!(["a", "x", "c", "d"]);
    f.change(
        14 * S,
        kind::KEYED,
        "mood.items",
        &[],
        r#"{"d":[{"update":[1,"b","x"]}],"r":["b"]}"#,
    );
    f.change(
        16 * S,
        kind::KEYED,
        "mood.items",
        &[],
        r#"{"d":[{"insert":[3,"d","d"]}],"r":[]}"#,
    );
    // The shell lists them, one box a key.
    f.config = f
        .config
        .replacen("{ level: int }", "{ level: int; items: [text] }", 1)
        .replacen(
            "  row {\n",
            "  row {\n    for it in mood.items key it { box { id: item; width: 10; height: 10; bg: #00ffff } }\n",
            1,
        );
    let dir = f.write("keyed");
    let (_, p, text) = plan_of(&dir, &defaults());
    assert_eq!(p.seeds["mood.items"], json!(["a", "b", "c"]));
    let st = steps(&text);
    let a = st
        .iter()
        .position(|l| *l == r#"set mood.items ["a","x","c"]"#);
    let b = st
        .iter()
        .position(|l| *l == r#"set mood.items ["a","x","c","d"]"#);
    assert!(a.is_some() && b.is_some() && a < b, "{text}");
    // Replayed, the list starts at three items and the one added mounts.
    let (r, out) = replay_of(
        &dir,
        &Options {
            output: None,
            traced: None,
            gpu: false,
        },
    );
    let trace: strand_eval::trace::RunTrace =
        serde_json::from_slice(&std::fs::read(out.join(&r.id).join("trace.json")).unwrap())
            .unwrap();
    let items: Vec<&String> = trace
        .nodes
        .iter()
        .filter(|(_, m)| m.id.as_deref() == Some("item"))
        .map(|(k, _)| k)
        .collect();
    let shown =
        |f: &strand_eval::trace::Frame| f.n.iter().filter(|row| items.contains(&&row.key)).count();
    assert_eq!(shown(&trace.frames[0]), 3, "the seed's three");
    assert_eq!(shown(trace.frames.last().unwrap()), 4, "and the one added");
}

#[test]
fn theme_icons_are_one_flat_folder_with_the_configs() {
    let f = Fx::session();
    let dir = f.write("icons");
    std::fs::create_dir_all(dir.join("config/icons")).unwrap();
    std::fs::write(
        dir.join("config/icons/mark.svg"),
        "<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .unwrap();
    let theme = dir.join("files/usr/share/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&theme).unwrap();
    std::fs::write(
        theme.join("app.svg"),
        "<svg xmlns='http://www.w3.org/2000/svg'/>",
    )
    .unwrap();
    let to = dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scenarios/local/report-icons.scn");
    let b = Bundle::read(&dir).unwrap();
    let (p, path) = replay::write_scenario(&b, &defaults(), Some(&to)).unwrap();
    let icons = p.icons.unwrap();
    assert!(icons.join("mark.svg").is_file() && icons.join("app.svg").is_file());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!("icons = \"{}\"", icons.display())),
        "{text}"
    );
    Scenario::load(&path).unwrap();
}

#[test]
fn recorded_answers_come_before_their_calls() {
    let mut f = Fx::session();
    f.change(
        10 * S,
        kind::FETCH,
        "apps.search",
        &1u32.to_le_bytes(),
        r#"{"ok":[]}"#,
    );
    f.change(
        15 * S,
        kind::FETCH,
        "apps.search",
        &2u32.to_le_bytes(),
        r#"{"err":"boom"}"#,
    );
    let (_, p, text) = plan_of(&f.write("answers"), &defaults());
    let st = steps(&text);
    let first = st
        .iter()
        .position(|l| *l == "answer apps.search []")
        .unwrap();
    let second = st
        .iter()
        .position(|l| *l == "answer apps.search error \"boom\"")
        .unwrap();
    let tick_at = |i: usize| -> u64 {
        st[..i]
            .iter()
            .filter_map(|l| l.strip_prefix("frames ").map(|n| n.parse::<u64>().unwrap()))
            .sum()
    };
    assert_eq!(
        tick_at(first),
        0,
        "the first is queued at the window's start"
    );
    assert_eq!(
        tick_at(second),
        540,
        "the second as the first is taken, 9 s in"
    );
    assert!(
        p.events
            .iter()
            .any(|e| e.text.contains("apps.search answered with an error"))
    );
}

#[test]
fn every_limit_is_named() {
    let mut f = Fx::session();
    f.input.push(Rec::new(9 * S, kind::KEYBOARD_ENTER, 1));
    f.input.push(
        Rec::new(9 * S + MS, kind::TEXT, 1)
            .u32_at(0, 5)
            .u32_at(4, 400),
    );
    f.input.push(
        Rec::new(9 * S + 2 * MS, kind::TEXT, 1)
            .u32_at(0, 3)
            .u32_at(4, 0)
            .u8_at(8, 1),
    );
    f.input
        .push(Rec::new(9 * S + 3 * MS, kind::PASTE, 1).u32_at(0, 11));
    f.input.push(Rec::new(9 * S + 4 * MS, kind::CLICK_AWAY, 1));
    f.input.push(Rec::new(9 * S + 5 * MS, kind::DRAG, 1));
    // (report review) A tilt of the wheel: sideways, no vertical notch.
    f.input.push(
        Rec::new(9 * S + 6 * MS, kind::AXIS, 1)
            .f32_at(0, 15.0)
            .u8_at(8, 4),
    );
    f.field(13 * S, "weather.temp", "[1,2]");
    f.output(1, "HDMI-A-1", 1920, 1080, 120, 200 * MS);
    f.missing
        .push(json!({"path": "/home/u/big.png", "bytes": 3_000_000, "why": "over 2 MiB"}));
    f.change(2 * S, kind::RELOAD, "reload", &[1, 0, 0, 0, 0, 0], "{}");
    let (_, p, text) = plan_of(&f.write("limits"), &defaults());
    let has = |what: &str| {
        assert!(
            p.limits.iter().any(|l| l.contains(what)),
            "no limit `{what}` in {:#?}",
            p.limits
        );
    };
    has("typed text: 5 keys typed");
    has("1 password spans, each typed as eight");
    has("pastes: 1");
    has("service `weather` has no stand-in");
    has("other programs' windows");
    has("output HDMI-A-1 is not replayed");
    has("missing file /home/u/big.png");
    has("the window starts after the reload");
    has("clicks outside the shell's surfaces");
    has("drag records");
    has("1 horizontal scrolls are not replayed");
    let st = steps(&text);
    for want in ["type 5 over 400", "type 8", "paste 11"] {
        assert!(st.contains(&want), "no `{want}` in\n{text}");
    }
    assert!(st.contains(&format!("focus @{}", origins().bar).as_str()));
}

#[test]
fn replay_txt_lists_inputs_changes_and_motion_by_source_location() {
    let dir = Fx::session().write("txt");
    // All of it traced: the click at 8 s and its motion are in the trace.
    let (r, out) = replay_of(
        &dir,
        &Options {
            output: None,
            traced: None,
            gpu: false,
        },
    );
    let text = std::fs::read_to_string(out.join("replay.txt")).unwrap();
    let o = origins();
    for want in [
        "glitch report: \"the bar dipped\"".to_string(),
        "window: 29.0 s up to the report, from the shell's start; traced: the last 29.0 s".into(),
        "output: DP-1 1280x720 scale 1 at 60 Hz".into(),
        format!(
            "-22.00s input press left at 40.0,20.0 on @{} hit @{} #button",
            o.bar, o.button
        ),
        format!("replay press hit @{} #button", o.button),
        "-18.00s service mood.level = 7".into(),
        "cell shell.clicks = 1 (live)".into(),
        "cell shell.clicks = 1 (replay)".into(),
        "motion (one line per run".into(),
        "hints (the checks may be wrong on a real shell):".into(),
    ] {
        assert!(text.contains(&want), "no `{want}` in\n{text}");
    }
    // The badge pushes the lamp along as the button is clicked: the run
    // names the lamp by its source location.
    assert!(
        text.lines()
            .any(|l| l.contains(&format!("@{}", o.lamp)) && l.contains(" x ")),
        "no motion line of the lamp in\n{text}"
    );
    for f in ["fidelity.json", "index.html", "summary.txt", "replay.txt"] {
        assert!(out.join(f).is_file(), "{f} not written");
    }
    assert_eq!(r.id, "report-2026-10-20_14-03-07");
    for f in ["trace.json", "checks.json", "scenario.scn"] {
        assert!(
            out.join(&r.id).join(f).is_file(),
            "{}/{f} not written",
            r.id
        );
    }
    assert!(r.scenario.is_file());
}

#[test]
fn a_faithful_replay_reports_no_divergence() {
    for traced in [Some(replay::plan::TRACED_S), None] {
        let opts = Options {
            output: None,
            traced,
            gpu: false,
        };
        let dir = Fx::session().write("faithful");
        let (r, out) = replay_of(&dir, &opts);
        assert!(
            r.fidelity.divergences.is_empty(),
            "{:#?}\n{}",
            r.fidelity.divergences,
            r.text
        );
        assert_eq!(r.fidelity.presses, (1, 1));
        assert_eq!(r.fidelity.writes, (1, 1));
        let j: Json =
            serde_json::from_slice(&std::fs::read(out.join("fidelity.json")).unwrap()).unwrap();
        assert_eq!(j["faithful"], json!(true));
        assert!(
            r.text
                .contains("fidelity: no divergence from the live shell")
        );
    }
}

#[test]
fn a_moved_target_is_the_first_divergence() {
    // The live button was 80 px wide; the bundle's copy of the config
    // makes it 30, so the replay's press at x 40 lands on the lamp.
    let mut f = Fx::session();
    f.config = f
        .config
        .replacen("id: button; width: 80", "id: button; width: 30", 1);
    let dir = f.write("moved");
    let (r, _) = replay_of(&dir, &defaults());
    let first = r.fidelity.divergences.first().expect("a divergence");
    assert_eq!(first.what, "press", "{:#?}", r.fidelity.divergences);
    let o = origins();
    assert!(
        first
            .text
            .contains(&format!("live hit @{} #button", o.button))
            && first
                .text
                .contains(&format!("the replay hit @{} #lamp", o.lamp)),
        "{}",
        first.text
    );
    assert!(r.text.contains("fidelity: "), "{}", r.text);
    // The click never counted: the live write has none in the replay.
    assert!(r.fidelity.divergences.iter().any(|d| d.what == "cell"));
}

#[test]
fn a_surface_mapped_apart_diverges() {
    // The live bar went at 25 s (its output gone) and came back at 26 s;
    // the replay's stayed.
    let mut f = Fx::session();
    f.input.push(Rec::new(25 * S, kind::UNMAPPED, 1));
    f.input.push(
        Rec::new(26 * S, kind::MAPPED, 1)
            .u64_at(0, bits(3, 1))
            .u8_at(8, 0),
    );
    let (r, _) = replay_of(&f.write("apart"), &defaults());
    let o = origins();
    let d: Vec<&str> = r
        .fidelity
        .divergences
        .iter()
        .filter(|d| d.what == "surface")
        .map(|d| d.text.as_str())
        .collect();
    assert_eq!(
        d,
        [
            format!(
                "surface @{} went live; the replay's did not within 2 frames",
                o.bar
            ),
            format!(
                "surface @{} mapped live; the replay's did not within 2 frames",
                o.bar
            ),
        ],
        "{:#?}",
        r.fidelity.divergences
    );
}

#[test]
fn the_final_frame_is_compared_with_the_png() {
    // The replay's own frame at the report, as the live one.
    let dir = Fx::session().write("frame0");
    let (r, out) = replay_of(&dir, &defaults());
    let trace: strand_eval::trace::RunTrace =
        serde_json::from_slice(&std::fs::read(out.join(&r.id).join("trace.json")).unwrap())
            .unwrap();
    let sid = trace.surfaces[0].id;
    let png_name = trace
        .frames
        .iter()
        .rev()
        .find_map(|fr| fr.png.get(&sid).cloned())
        .unwrap();
    let png = std::fs::read(out.join(&r.id).join(&png_name)).unwrap();
    let (w, h, px) = strand_eval::report::png::decode(&png).unwrap();
    assert!(
        r.fidelity.frames.is_empty(),
        "no PNG in the bundle, none compared"
    );

    let with_frame = |px: &[u8], tag: &str| {
        let mut f = Fx::session();
        f.surfaces[0]["frame"] = json!("frames/1-Top.png");
        let dir = f.write(tag);
        std::fs::write(
            dir.join("frames/1-Top.png"),
            strand_eval::report::png::encode(w, h, px).unwrap(),
        )
        .unwrap();
        replay_of(&dir, &defaults()).0
    };
    let same = with_frame(&px, "frame1");
    assert_eq!(same.fidelity.frames.len(), 1);
    let d = &same.fidelity.frames[0];
    assert_eq!((d.over, d.why.as_deref()), (0, None), "{d:?}");
    assert_eq!(d.total, u64::from(w) * u64::from(h));
    assert!(
        same.fidelity.divergences.is_empty(),
        "{:#?}",
        same.fidelity.divergences
    );

    // A 10x10 block painted over at 300,5: counted, boxed, under the share.
    let mut dot = px.clone();
    for y in 5..15 {
        for x in 300..310 {
            let i = ((y * w + x) * 4) as usize;
            dot[i..i + 4].copy_from_slice(&[255, 0, 255, 255]);
        }
    }
    let r = with_frame(&dot, "frame2");
    let d = &r.fidelity.frames[0];
    assert_eq!(d.over, 100);
    assert_eq!(d.bbox, Some([300, 5, 10, 10]));
    assert!(!d.diverges());
    assert!(r.text.contains("frames/1-Top.png @"), "{}", r.text);
    // Every pixel inverted: the frame diverges.
    let inv: Vec<u8> = px
        .chunks_exact(4)
        .flat_map(|p| [255 - p[0], 255 - p[1], 255 - p[2], 255])
        .collect();
    let r = with_frame(&inv, "frame3");
    assert!(r.fidelity.frames[0].diverges());
    assert!(
        r.fidelity
            .divergences
            .iter()
            .any(|d| d.what == "frame" && d.text.contains("of the pixels differ")),
        "{:#?}",
        r.fidelity.divergences
    );
}

/// The fixture bundles `scripts/eval/test-report-view.sh` replays: with
/// `REPORT_VIEW_FIXTURE=DIR`, an older and a newer bundle under
/// `DIR/reports/` (the newer is the default `report-view.sh` picks);
/// without it, the same into a scratch directory, read back.
#[test]
fn the_report_view_fixture_bundles_are_written() {
    let root = std::env::var_os("REPORT_VIEW_FIXTURE").map_or_else(|| tmp("view"), PathBuf::from);
    let f = Fx::session();
    for name in ["2026-10-20_10-00-00", "2026-10-20_14-03-07"] {
        let dir = root.join("reports").join(name);
        f.write_to(&dir);
        Bundle::read(&dir).unwrap();
    }
}

/// (report review) A quiet shell's rings reach back to its start: the
/// window is held to the last 120 s, and the limit says so.
#[test]
fn the_window_is_capped_before_the_report() {
    let mut f = Fx::session();
    // The report an hour after the bar mapped, with a click near it.
    f.now = 3600 * S;
    f.press(1, 40.0, 20.0, 3590 * S, Some(bits(7, 1)));
    f.cell(3590 * S + 100 * MS, "shell.clicks", "[1,2]");
    let (_, p, text) = plan_of(&f.write("cap"), &defaults());
    assert_eq!(p.end - p.start, replay::plan::WINDOW_CAP_NS);
    assert!(p.start_why.contains("cap"), "{}", p.start_why);
    assert!(
        p.limits
            .iter()
            .any(|l| l.contains("the replay's cap, 120 s before the report")),
        "{:#?}",
        p.limits
    );
    // The press inside the window is replayed, the hour before is not.
    assert!(
        steps(&text)
            .iter()
            .filter(|l| l.starts_with("press"))
            .count()
            == 1,
        "{text}"
    );
}

/// (report review) A cell written from outside the shell (`strand set`
/// from a keybinding) is replayed at its time, and its write is then the
/// live one.
#[test]
fn a_request_is_replayed_as_a_set() {
    let mut f = Fx::session();
    f.change(20 * S, kind::REQUEST, "shell.clicks", &[request::SET], "5");
    f.cell(20 * S + 17 * MS, "shell.clicks", "[1,5]");
    let dir = f.write("request");
    let (_, _, text) = plan_of(&dir, &defaults());
    assert!(steps(&text).contains(&"set shell.clicks 5"), "{text}");
    let (r, _) = replay_of(&dir, &defaults());
    assert!(
        r.fidelity.divergences.is_empty(),
        "{:#?}\n{}",
        r.fidelity.divergences,
        r.text
    );
    assert_eq!(r.fidelity.writes, (2, 2));
    // A held-back text is set as `x`s as long as it was.
    let mut g = Fx::session();
    g.change(
        20 * S,
        kind::REQUEST,
        "shell.note",
        &[request::SET],
        "{\"~text\":3}",
    );
    let (_, _, text) = plan_of(&g.write("request-held"), &defaults());
    assert!(steps(&text).contains(&"set shell.note \"xxx\""), "{text}");
}

/// (report review) A step the sandbox cannot take (input on a surface it
/// never shows) is a divergence, and the replay still ends with its
/// report.
#[test]
fn a_step_the_replay_cannot_take_is_a_divergence() {
    let mut f = Fx::session();
    // A panel the live shell showed (opened by something the bundle did
    // not keep), at a place the replay's config has no node.
    let panel = bits(40, 1);
    f.node(panel, "shell.strand:99:1", None);
    f.input.push(
        Rec::new(20 * S, kind::MAPPED, 2)
            .u64_at(0, panel)
            .u8_at(8, 0)
            .u8_at(9, 1),
    );
    f.pointer(2, kind::ENTER, 10.0, 10.0, 21 * S);
    let (r, out) = replay_of(&f.write("cannot"), &defaults());
    let step = r
        .fidelity
        .divergences
        .iter()
        .find(|d| d.what == "step")
        .unwrap_or_else(|| panic!("no step divergence: {:#?}", r.fidelity.divergences));
    assert!(step.text.contains("could not take line"), "{}", step.text);
    assert!(out.join("replay.txt").is_file());
    assert!(out.join("fidelity.json").is_file());
}

/// (report review) A press whose hit the bundle cannot name is no
/// evidence either way: a note, not a divergence.
#[test]
fn an_unnamed_hit_is_a_note() {
    let mut f = Fx::session();
    f.nodes.remove("7.1");
    let (r, _) = replay_of(&f.write("unnamed"), &defaults());
    assert!(
        !r.fidelity.divergences.iter().any(|d| d.what == "press"),
        "{:#?}",
        r.fidelity.divergences
    );
    assert!(
        r.fidelity
            .notes
            .iter()
            .any(|n| n.contains("is not in nodes.json, so it is not compared")),
        "{:#?}",
        r.fidelity.notes
    );
    assert!(r.text.contains("is not in nodes.json"), "{}", r.text);
}

/// (report review) A surface opened by a step (a request, here) maps in
/// the live shell a roundtrip later, often within the same tick; the
/// sandbox shows it the tick after. A step on it waits for that tick.
#[test]
fn a_step_on_a_surface_waits_for_the_tick_it_shows() {
    let mut f = Fx::session();
    let panel = bits(40, 1);
    f.node(panel, "shell.strand:99:1", None);
    f.change(20 * S, kind::REQUEST, "shell.clicks", &[request::SET], "5");
    f.input.push(
        Rec::new(20 * S + 2 * MS, kind::MAPPED, 2)
            .u64_at(0, panel)
            .u8_at(8, 0)
            .u8_at(9, 1),
    );
    f.input
        .push(Rec::new(20 * S + 3 * MS, kind::KEYBOARD_ENTER, 2));
    let (_, _, text) = plan_of(&f.write("shows"), &defaults());
    let st = steps(&text);
    let set = st.iter().position(|l| *l == "set shell.clicks 5").unwrap();
    let focus = st
        .iter()
        .position(|l| l.starts_with("focus @") && l.contains("99:1"))
        .unwrap_or_else(|| panic!("no focus in\n{text}"));
    assert!(
        st[set..focus].iter().any(|l| l.starts_with("frames ")),
        "the focus runs in the request's tick:\n{text}"
    );
}

/// (replay-owner) `drawn.json` as the live shell writes it, from boxes:
/// the bar (slot 1, root `3.1`) and its nodes.
fn write_drawn(dir: &Path, nodes: &[replay::drawn::Drawn]) {
    let nodes: Vec<Json> = nodes
        .iter()
        .map(|n| {
            json!({
                "ref": "0.0",
                "origin": format!("{LIVE}/{}", n.origin),
                "id": n.id, "key": n.key, "box": n.rect, "opacity": n.opacity,
            })
        })
        .collect();
    let drawn = json!({"format": 1, "surfaces": [{"slot": 1, "root": "3.1", "nodes": nodes}]});
    std::fs::write(
        dir.join("drawn.json"),
        serde_json::to_vec_pretty(&drawn).unwrap(),
    )
    .unwrap();
}

/// (replay-owner) The verdict: a bundle with no `drawn.json` is not
/// checked; one whose live boxes are where the replay drew its nodes is
/// VERIFIED; one node moved 3 px, or faded, is DRIFTED with that node
/// and how far; a node only one side drew is named. The verdict is the
/// first line of `replay.txt`, the first key of `fidelity.json`, and in
/// the viewer's page.
#[test]
fn the_live_boxes_are_held_to_the_replay_node_by_node() {
    let dir = Fx::session().write("verdict");
    let (r, out) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "NOT CHECKED", "{}", r.verdict.line());
    let o = origins();
    let (_, bar) = r
        .verdict
        .replayed
        .iter()
        .find(|(s, _)| strand_eval::replay::fidelity::same_origin(s, &o.bar))
        .expect("the bar was drawn at the report");
    assert!(bar.iter().any(|n| n.id.as_deref() == Some("button")));
    // As drawn: verified.
    write_drawn(&dir, bar);
    let (r, out2) = replay_of(&dir, &defaults());
    assert_eq!(
        r.verdict.word(),
        "VERIFIED",
        "{}",
        r.verdict.lines().join("\n")
    );
    assert_eq!(r.verdict.nodes, bar.len());
    assert!(
        r.text
            .starts_with("verdict: VERIFIED: the replay drew all "),
        "{}",
        r.text
    );
    let raw = std::fs::read_to_string(out2.join("fidelity.json")).unwrap();
    assert!(raw.starts_with("{\n  \"verdict\": {"), "{raw}");
    let j: Json = serde_json::from_str(&raw).unwrap();
    assert_eq!(j["verdict"]["verdict"], json!("VERIFIED"));
    let page = std::fs::read_to_string(out2.join("index.html")).unwrap();
    let data = strand_eval::report::html::embedded(&page).unwrap();
    assert_eq!(data["replay"]["verdict"], json!("VERIFIED"));
    // The button 3 px to the right live, the lamp faded: drifted.
    let mut moved = bar.clone();
    let b = moved
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("button"))
        .unwrap();
    b.rect[0] += 3.0;
    let lamp = moved
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("lamp"))
        .unwrap();
    lamp.opacity = (lamp.opacity - 0.5).max(0.0);
    write_drawn(&dir, &moved);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "DRIFTED");
    let lines = r.verdict.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("@{} #button", o.button)) && l.contains("3.0 px off")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("#lamp") && l.contains("opacity 0.50 off")),
        "{lines:#?}"
    );
    assert!(r.text.starts_with("verdict: DRIFTED: 2 of "), "{}", r.text);
    // One node drawn live only, one in the replay only.
    let mut extra = bar.clone();
    let gone = extra.remove(
        extra
            .iter()
            .position(|n| n.id.as_deref() == Some("spot"))
            .unwrap(),
    );
    extra.push(replay::drawn::Drawn {
        origin: "shell.strand:99:1".into(),
        id: Some("ghost".into()),
        ..gone.clone()
    });
    write_drawn(&dir, &extra);
    let (r, _) = replay_of(&dir, &defaults());
    let lines = r.verdict.lines().join("\n");
    assert!(
        lines.contains("#ghost (surface @") && lines.contains("not in the replay"),
        "{lines}"
    );
    assert!(
        lines.contains("#spot (surface @") && lines.contains("not live"),
        "{lines}"
    );
    let _ = out;
}

/// (replay-owner) The checks' findings are hints: listed after the
/// verdict and the timeline under their label in `replay.txt`, as
/// `hints` in `fidelity.json` and labelled in the viewer; a replay whose
/// checks fail still ends well (`strand-eval replay` exits 0).
#[test]
fn the_checks_findings_are_hints() {
    // A finding: the badge the click mounts skips its enter pose (its
    // opacity snaps), which `vanish` calls a pop-in.
    let mut f = Fx::session();
    f.config = f.config.replacen(
        "id: badge;",
        "id: badge; enter { opacity: 0 }; opacity: 1 ~ instant;",
        1,
    );
    let dir = f.write("hints");
    let all = Options {
        traced: None,
        ..defaults()
    };
    let (r, out) = replay_of(&dir, &all);
    assert!(!r.ok, "the checks found nothing:\n{}", r.text);
    let text = &r.text;
    let v = text.find("verdict: ").unwrap();
    let ev = text.find("\nevents (seconds before the report):").unwrap();
    let mo = text.find("\nmotion (one line per run").unwrap();
    let h = text
        .find("\nhints (the checks may be wrong on a real shell):")
        .unwrap();
    assert!(v == 0 && v < ev && ev < mo && mo < h, "{text}");
    assert!(!text.contains("\nfindings:"), "{text}");
    let j: Json =
        serde_json::from_slice(&std::fs::read(out.join("fidelity.json")).unwrap()).unwrap();
    assert_eq!(
        j["hints"]["label"],
        json!("hints (the checks may be wrong on a real shell)")
    );
    assert!(j["hints"]["findings"].is_array());
    let page = std::fs::read_to_string(out.join("index.html")).unwrap();
    let data = strand_eval::report::html::embedded(&page).unwrap();
    assert_eq!(
        data["replay"]["hints"],
        json!("hints (the checks may be wrong on a real shell)")
    );
    // (replay-owner review) Nowhere does a finding read as a failure:
    // the page's statuses are hints (the viewer shows what the data
    // says, upper-cased), as are summary.md, summary.json, summary.txt
    // and replay.txt.
    let sc = &data["runs"][0]["scenarios"][0];
    assert_eq!(sc["status"], json!("hint"), "{}", sc["status"]);
    let results = sc["checks"]["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .any(|r| r["status"]["status"] == json!("hint")),
        "a check that found something reads as a hint"
    );
    assert!(
        results.iter().all(|r| !matches!(
            r["status"]["status"].as_str(),
            Some("fail" | "changed" | "xpass")
        )),
        "{results:#?}"
    );
    assert!(!page.contains("FAIL"), "the page says FAIL");
    for f in ["summary.md", "summary.json", "summary.txt", "replay.txt"] {
        let t = std::fs::read_to_string(out.join(f)).unwrap();
        assert!(!t.contains("FAIL"), "{f} says FAIL:\n{t}");
    }
    let md = std::fs::read_to_string(out.join("summary.md")).unwrap();
    assert!(md.contains(" hints: "), "{md}");
    assert!(
        md.contains("hints (the checks may be wrong on a real shell)"),
        "{md}"
    );
    let sj: Json =
        serde_json::from_slice(&std::fs::read(out.join("summary.json")).unwrap()).unwrap();
    assert_eq!(sj["ok"], json!(true), "the replay ran");
    assert!(sj["hints"].as_u64().is_some_and(|n| n > 0), "{sj}");
    assert_eq!(sj["scenarios"][0]["status"], json!("hints"));
    // The command that runs it ends well all the same.
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("replay")
        .arg(&dir)
        .args(["--no-open", "--traced", "all", "--to"])
        .arg(
            dir.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("scenarios/local/report-cli.scn"),
        )
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
}

/// (replay-owner) `strand-eval replay <bundle>` as `strand replay` runs
/// it, a plain process: it writes `<bundle>/replay/`, prints the verdict
/// in one line and the two paths, opens nothing with `--no-open` and the
/// viewer with `xdg-open` without it (a stub here), and never touches the
/// session's Wayland display: `WAYLAND_DISPLAY` names a socket this test
/// listens on, which no one connects to.
#[test]
fn the_replay_command_runs_as_a_plain_process_beside_a_display() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Fx::session().write("cli");
    let root = dir.parent().unwrap().parent().unwrap().to_path_buf();
    let display = root.join("wayland-bogus");
    let listener = std::os::unix::net::UnixListener::bind(&display).unwrap();
    listener.set_nonblocking(true).unwrap();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let opened = root.join("opened");
    std::fs::write(
        bin.join("xdg-open"),
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", opened.display()),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("xdg-open"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |open: bool| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"));
        cmd.arg("replay").arg(&dir).arg("--to");
        cmd.arg(root.join("scenarios/local/report-cli.scn"));
        if !open {
            cmd.arg("--no-open");
        }
        cmd.env("WAYLAND_DISPLAY", &display)
            .env("XDG_RUNTIME_DIR", &root)
            .env("PATH", &path)
            .output()
            .unwrap()
    };
    let out = run(false);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(lines[0].starts_with("NOT CHECKED: "), "{stdout}");
    let rep = dir.join("replay");
    assert_eq!(
        lines[1],
        format!("replay: {}", rep.join("replay.txt").display())
    );
    assert_eq!(
        lines[2],
        format!("viewer: {}", rep.join("index.html").display())
    );
    for f in ["replay.txt", "index.html", "fidelity.json", "summary.txt"] {
        assert!(rep.join(f).is_file(), "no {f}");
    }
    assert_eq!(
        std::fs::metadata(&rep).unwrap().permissions().mode() & 0o077,
        0,
        "the replay is as private as the report"
    );
    assert!(!opened.exists(), "--no-open opened the viewer");
    // Opened: the stub is given the page.
    let out = run(true);
    assert!(out.status.success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !opened.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(&opened).unwrap().trim(),
        rep.join("index.html").display().to_string()
    );
    // No client came to the display.
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("something connected to WAYLAND_DISPLAY: {other:?}"),
    }
}

/// (replay-owner) A directory that is no report is refused, plainly.
#[test]
fn the_replay_command_refuses_a_directory_that_is_no_report() {
    let d = tmp("noreport");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("replay")
        .arg(&d)
        .arg("--no-open")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a glitch report"));
}

/// (replay-owner review) `drawn.json` for several surfaces: each slot's
/// root and its nodes.
fn write_drawn_surfaces(dir: &Path, surfaces: &[(u16, &str, &[replay::drawn::Drawn])]) {
    let surfaces: Vec<Json> = surfaces
        .iter()
        .map(|(slot, root, nodes)| {
            let nodes: Vec<Json> = nodes
                .iter()
                .map(|n| {
                    json!({
                        "ref": "0.0",
                        "origin": format!("{LIVE}/{}", n.origin),
                        "id": n.id, "key": n.key, "box": n.rect, "opacity": n.opacity,
                        "travel": n.travel, "fade": n.fade,
                    })
                })
                .collect();
            json!({"slot": slot, "root": root, "nodes": nodes})
        })
        .collect();
    std::fs::write(
        dir.join("drawn.json"),
        serde_json::to_vec_pretty(&json!({"format": 2, "surfaces": surfaces})).unwrap(),
    )
    .unwrap();
}

/// The replay's trace of `r`, read back from its report.
fn trace_of(r: &replay::Replayed, out: &Path) -> strand_eval::trace::RunTrace {
    serde_json::from_slice(&std::fs::read(out.join(&r.id).join("trace.json")).unwrap()).unwrap()
}

/// The fixture with a spinner that turns forever, moving at the report.
fn spinning() -> Fx {
    let mut f = Fx::session();
    // Declared below the bar, so the fixture's origins stay where they
    // are.
    f.config.push_str(
        "keyframes spin { 0% { rotate: 0 }; 100% { rotate: 360 }; duration: 900ms; repeat: 0 }\n",
    );
    f.config = f.config.replacen(
        "    box { id: spot;",
        "    box { id: spinner; width: 30; height: 10; bg: #89b4fa; play spin }\n    box { id: spot;",
        1,
    );
    f
}

/// (replay-owner review) The frame the verdict holds is the replay's last
/// one before the report, never the tick it paints after it, even when
/// the report falls just past a whole number of periods (the old pick,
/// any frame up to a period past the report, took the tick after it);
/// the same frame is the one its pixels are compared with. A node moving
/// at the report (a looping spinner) is held to two frames of its
/// travel, its box left out of the pixel comparison, and one further off
/// is named as moving out of step, not as a drift; a static node's
/// pixels differing makes the verdict DRIFTED.
#[test]
fn the_frame_held_is_the_last_before_the_report() {
    let mut f = spinning();
    // The window is the bar's first surface on (1 s) to the report: the
    // report 2 ms past a whole number of periods.
    let probe = f.write("tick-probe");
    let (_, p, _) = plan_of(&probe, &defaults());
    let whole = (29 * S) / p.period;
    f.now = S + whole * p.period + 2 * MS;
    let dir = f.write("tick");
    let (_, p, _) = plan_of(&dir, &defaults());
    let len = p.end - p.start;
    let end_tick = (len + p.period / 2) / p.period;
    assert!(end_tick * p.period <= len, "the report is past the tick");
    let (r, out) = replay_of(&dir, &defaults());
    let trace = trace_of(&r, &out);
    let mark = trace
        .events
        .iter()
        .find(|e| e.text == "mark report")
        .unwrap()
        .k;
    let sid = trace.surfaces[0].id;
    // The tick after the report painted the bar (its spinner turned), and
    // it lies within a period of the report: what the old pick took.
    let after = trace
        .frames
        .iter()
        .find(|fr| fr.k == mark && fr.painted.contains(&sid))
        .expect("the tick after the report painted the bar");
    assert!(after.t_ns <= 1_000_000_000 + len + p.period);
    let o = origins();
    let (_, bar) = r
        .verdict
        .replayed
        .iter()
        .find(|(s, _)| replay::fidelity::same_origin(s, &o.bar))
        .unwrap();
    let spinner = bar
        .iter()
        .find(|n| n.id.as_deref() == Some("spinner"))
        .unwrap()
        .clone();
    assert!(spinner.travel > 0.1, "{spinner:?}");

    // As drawn before the report: verified, at the tick before it.
    write_drawn(&dir, bar);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "VERIFIED", "{}", r.verdict.line());
    assert_eq!(r.verdict.ticks.len(), 1);
    assert_eq!(r.verdict.ticks[0].1, mark - 1, "{:?}", r.verdict.ticks);
    assert!(r.verdict.in_motion >= 1, "{}", r.verdict.line());
    assert!(
        r.verdict.line().contains("moving at the report"),
        "{}",
        r.verdict.line()
    );

    // Its pixels: the replay's own image of that frame as the live PNG.
    let fr = trace
        .frames
        .iter()
        .rev()
        .find(|fr| fr.k < mark && fr.png.contains_key(&sid))
        .unwrap();
    assert_eq!(fr.k, mark - 1);
    let png = std::fs::read(out.join(&r.id).join(&fr.png[&sid])).unwrap();
    let (w, h, px) = strand_eval::report::png::decode(&png).unwrap();
    let with_png = |px: &[u8], drawn: &[replay::drawn::Drawn], tag: &str| {
        let mut g = spinning();
        g.now = f.now;
        g.surfaces[0]["frame"] = json!("frames/1-Top.png");
        let d = g.write(tag);
        std::fs::write(
            d.join("frames/1-Top.png"),
            strand_eval::report::png::encode(w, h, px).unwrap(),
        )
        .unwrap();
        write_drawn(&d, drawn);
        replay_of(&d, &defaults()).0
    };
    let same = with_png(&px, bar, "tick-png");
    assert_eq!(same.verdict.word(), "VERIFIED", "{}", same.verdict.line());
    assert_eq!(same.verdict.pixels_held, 1);
    assert!(
        same.verdict
            .line()
            .contains("and the pixels of 1 as the live frame's"),
        "{}",
        same.verdict.line()
    );
    assert!(
        !r.verdict.line().contains("pixels"),
        "no PNG, no claim: {}",
        r.verdict.line()
    );
    assert_eq!(same.fidelity.frames[0].k, Some(mark - 1));
    assert_eq!(same.fidelity.frames[0].over, 0);
    let paint = |x0: u32, y0: u32| {
        let mut v = px.clone();
        for y in y0..y0 + 4 {
            for x in x0..x0 + 4 {
                let i = ((y * w + x) * 4) as usize;
                v[i..i + 4].copy_from_slice(&[255, 0, 255, 255]);
            }
        }
        v
    };
    // Over the spinner, which moves: not held pixel for pixel.
    let [sx, sy, sw, sh] = spinner.rect;
    let on_spinner = paint((sx + sw / 2.0) as u32 - 2, (sy + sh / 2.0) as u32 - 2);
    let r2 = with_png(&on_spinner, bar, "tick-spin");
    assert_eq!(r2.verdict.word(), "VERIFIED", "{}", r2.verdict.line());
    // Over the lamp, which is still: the pixels differ.
    let lamp = bar
        .iter()
        .find(|n| n.id.as_deref() == Some("lamp"))
        .unwrap();
    let on_lamp = paint(lamp.rect[0] as u32 + 10, lamp.rect[1] as u32 + 10);
    let r3 = with_png(&on_lamp, bar, "tick-lamp");
    assert_eq!(r3.verdict.word(), "DRIFTED", "{}", r3.verdict.line());
    assert!(
        r3.verdict
            .line()
            .contains(&format!("pixels differ on @{}", o.bar))
            && r3.verdict.line().contains("(16 px)"),
        "{}",
        r3.verdict.line()
    );
    assert!(
        r3.verdict
            .lines()
            .iter()
            .any(|l| l.contains("16 differ past 6 from the live frame")),
        "{:#?}",
        r3.verdict.lines()
    );

    // The spinner drawn live far from where the replay's turn puts it:
    // moving out of step, named on its own line, not a drift.
    let mut off = bar.clone();
    let s = off
        .iter_mut()
        .find(|n| n.id.as_deref() == Some("spinner"))
        .unwrap();
    s.rect[0] += 60.0;
    s.travel = 2.0;
    write_drawn(&dir, &off);
    let (r4, _) = replay_of(&dir, &defaults());
    assert_eq!(r4.verdict.word(), "VERIFIED", "{}", r4.verdict.line());
    assert_eq!(r4.verdict.moving.len(), 1, "{:#?}", r4.verdict.lines());
    assert!(
        r4.verdict
            .lines()
            .iter()
            .any(|l| l.starts_with("moving out of step: ")
                && l.contains("#spinner")
                && l.contains("moving at the report")),
        "{:#?}",
        r4.verdict.lines()
    );
    assert!(
        r4.verdict.line().contains("1 moving out of step"),
        "{}",
        r4.verdict.line()
    );
}

/// (replay-owner review) A surface the replay shows at the report that
/// the live shell did not (a popup its click opened in the replay only)
/// makes the verdict DRIFTED, named.
#[test]
fn a_surface_shown_in_the_replay_only_drifts() {
    let mut f = Fx::session();
    f.config = f.config.replacen(
        "on click { clicks = clicks + 1 } }",
        "on click { clicks = clicks + 1 }\n      popup { id: tip; open: clicks > 0; width: 60; height: 20; box { width: 20; height: 10; bg: #ffffff } } }",
        1,
    );
    let dir = f.write("onlyreplay");
    let (r, _) = replay_of(&dir, &defaults());
    let o = origins();
    let bar = r
        .verdict
        .replayed
        .iter()
        .find(|(s, _)| replay::fidelity::same_origin(s, &o.bar))
        .unwrap()
        .1
        .clone();
    assert_eq!(
        r.verdict.replayed.len(),
        2,
        "the bar and the popup: {:?}",
        r.verdict.replayed
    );
    write_drawn(&dir, &bar);
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(r.verdict.word(), "DRIFTED", "{}", r.verdict.line());
    assert_eq!(r.verdict.replay_only_surfaces.len(), 1, "{:?}", r.verdict);
    assert!(
        r.verdict
            .line()
            .contains("1 surface shown in the replay only"),
        "{}",
        r.verdict.line()
    );
    assert!(
        r.verdict.lines()[0].contains("shown in the replay at the report, not live"),
        "{:#?}",
        r.verdict.lines()
    );
}

/// (replay-owner review) Two popups made in a `for` share their root's
/// origin and differ by row key: each live one is held to the replay's
/// with the same key, each replay surface taken once (the old match by
/// origin alone held both to the first).
#[test]
fn surfaces_from_one_place_are_held_each_to_its_own() {
    let config = |f: &mut Fx| {
        f.config = f.config.replacen(
            "    box { id: spot;",
            "    for n in [1, 2] key n { box { width: 20; height: 20; bg: #445566\n      popup { open: true; width: 40; height: 20; box { width: 10 * n; height: 10; bg: #112233 } } } }\n    box { id: spot;",
            1,
        );
    };
    let mut f = Fx::session();
    config(&mut f);
    let probe = f.write("pops-probe");
    let (r, _) = replay_of(&probe, &defaults());
    let o = origins();
    let pops: Vec<&(String, Vec<replay::drawn::Drawn>)> = r
        .verdict
        .replayed
        .iter()
        .filter(|(s, _)| !replay::fidelity::same_origin(s, &o.bar))
        .collect();
    assert_eq!(pops.len(), 2, "{:?}", r.verdict.replayed);
    assert_eq!(pops[0].0, pops[1].0, "one place");
    let (r0, r1) = (&pops[0].1[0], &pops[1].1[0]);
    assert_ne!(r0.key, r1.key, "{r0:?} {r1:?}");
    let bar = r
        .verdict
        .replayed
        .iter()
        .find(|(s, _)| replay::fidelity::same_origin(s, &o.bar))
        .unwrap()
        .1
        .clone();
    // The live shell had both, listed second first.
    let mut f = Fx::session();
    config(&mut f);
    for (slot, b, root) in [(2u16, bits(20, 1), r1), (3, bits(21, 1), r0)] {
        f.surface(slot, 0, b, &root.origin, S, 16_666_667);
        f.surfaces.last_mut().unwrap()["kind"] = json!("popup");
        f.nodes.insert(
            format!("{}.{}", b as u32, (b >> 32) as u32),
            json!({"origin": format!("{LIVE}/{}", root.origin), "id": null, "key": root.key}),
        );
    }
    let dir = f.write("pops");
    write_drawn_surfaces(
        &dir,
        &[
            (1, "3.1", &bar),
            (2, "20.1", &pops[1].1),
            (3, "21.1", &pops[0].1),
        ],
    );
    let (r, _) = replay_of(&dir, &defaults());
    assert_eq!(
        r.verdict.word(),
        "VERIFIED",
        "{}",
        r.verdict.lines().join("\n")
    );
    assert_eq!(r.verdict.surfaces, 3);
}

/// (replay-owner review) `strand-eval replay` reads the bundle before it
/// clears `<dir>/replay`: a directory with some other `bundle.json` keeps
/// its `replay/`.
#[test]
fn a_directory_that_is_no_report_keeps_its_replay_folder() {
    let d = tmp("keep");
    std::fs::write(d.join("bundle.json"), r#"{"format": 99}"#).unwrap();
    std::fs::create_dir_all(d.join("replay")).unwrap();
    std::fs::write(d.join("replay/notes.txt"), "mine").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("replay")
        .arg(&d)
        .arg("--no-open")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("is not a glitch report this strand-eval reads"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("replay/notes.txt")).unwrap(),
        "mine"
    );
    // A readable bundle.json but no config/: refused too, kept too.
    std::fs::write(d.join("bundle.json"), r#"{"format": 1}"#).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_strand-eval"))
        .arg("replay")
        .arg(&d)
        .arg("--no-open")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no config/"));
    assert_eq!(
        std::fs::read_to_string(d.join("replay/notes.txt")).unwrap(),
        "mine"
    );
}

/// (replay-owner review) `strand replay` tells an option's value from
/// the report directory by the same list `strand-eval replay` parses
/// with (`cli::REPLAY_VALUED`): each is in strand's `VALUED`.
#[test]
fn strand_replay_knows_every_option_that_takes_a_value() {
    let src = std::fs::read_to_string(
        strand_eval::scenario::eval_root().join("../crates/strand/src/report/replay.rs"),
    )
    .unwrap();
    let line = src
        .lines()
        .find(|l| l.starts_with("const VALUED"))
        .expect("VALUED in strand's replay.rs");
    for o in strand_eval::cli::REPLAY_VALUED {
        assert!(
            line.contains(&format!("\"{o}\"")),
            "{o} is not in strand's {line}"
        );
    }
    assert!(
        line.contains(&format!("; {}]", strand_eval::cli::REPLAY_VALUED.len())),
        "{line}"
    );
}
