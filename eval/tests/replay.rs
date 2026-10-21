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
use strand_core::flight::{self, Rec, Ring, gap, kind};
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
        "findings:".into(),
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
