//! (replay-clip) The glitch-report fixtures eval/tests/replay.rs and
//! eval/tests/clip.rs share: a bundle written from scratch as `strand
//! report` writes one (`Fx`), the sandbox's origins of the fixture
//! shell's nodes, and the replay helpers. Each test file uses part.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value as Json, json};
use strand_core::flight::{self, Rec, Ring, kind};
use strand_eval::harness::{self, RunOptions};
use strand_eval::replay::{self, Bundle, Options};
use strand_eval::scenario::Scenario;
use strand_eval::trace::BuildInfo;

pub const S: u64 = 1_000_000_000;
pub const MS: u64 = 1_000_000;
pub const LIVE: &str = "/live/config";

pub fn shell() -> String {
    std::fs::read_to_string(strand_eval::scenario::eval_root().join("fixtures/report/shell.strand"))
        .unwrap()
}

/// The sandbox's origins of the fixture's nodes: the bar's root and
/// each `id:`.
#[derive(Debug)]
pub struct Origins {
    pub bar: String,
    pub button: String,
    pub lamp: String,
    pub spot: String,
}

pub fn origins() -> &'static Origins {
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

pub fn tmp(tag: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("strand-replay-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn bits(index: u32, gen_: u32) -> u64 {
    u64::from(index) | (u64::from(gen_) << 32)
}

/// A bundle being written.
pub struct Fx {
    pub input: Ring,
    pub changes: Ring,
    pub now: u64,
    pub services: Json,
    pub cells: Json,
    pub nodes: serde_json::Map<String, Json>,
    pub surfaces: Vec<Json>,
    pub outputs: Vec<Json>,
    pub missing: Vec<Json>,
    pub config: String,
    pub format: u64,
}

impl Fx {
    /// The scripted session: the bar mapped on DP-1, the pointer in, a
    /// hover on the spot, a click on the button (its cell written), and
    /// `mood.level` set to 7; the report at 30 s.
    pub fn session() -> Fx {
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

    pub fn output(&mut self, slot: u16, name: &str, w: u16, h: u16, scale120: u16, t: u64) {
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

    pub fn node(&mut self, b: u64, origin: &str, id: Option<&str>) {
        self.nodes.insert(
            format!("{}.{}", b as u32, (b >> 32) as u32),
            json!({"origin": format!("{LIVE}/{origin}"), "id": id, "key": null}),
        );
    }

    /// Surface `slot` mapped at `t` on output slot `out` with root `root`.
    pub fn surface(&mut self, slot: u16, out: u8, root: u64, origin: &str, t: u64, period: u32) {
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

    pub fn pointer(&mut self, slot: u16, k: u8, x: f32, y: f32, t: u64) {
        self.input
            .push(Rec::new(t, k, slot).f32_at(0, x).f32_at(4, y));
    }

    pub fn press(&mut self, slot: u16, x: f32, y: f32, t: u64, hit: Option<u64>) {
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

    pub fn change(&mut self, t: u64, k: u8, name: &str, inline: &[u8], value: &str) {
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

    pub fn field(&mut self, t: u64, name: &str, value: &str) {
        self.change(t, kind::FIELD, name, &[], value);
    }

    pub fn cell(&mut self, t: u64, name: &str, value: &str) {
        self.change(t, kind::CELL, name, &[], value);
    }

    /// Writes the bundle into a new directory and returns it.
    pub fn write(&self, tag: &str) -> PathBuf {
        let dir = tmp(tag).join("reports/2026-10-20_14-03-07");
        self.write_to(&dir);
        dir
    }

    pub fn write_to(&self, dir: &Path) {
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

pub fn plan_of(dir: &Path, opts: &Options) -> (Bundle, replay::Plan, String) {
    let b = Bundle::read(dir).unwrap();
    let p = replay::plan::plan(&b, opts).unwrap();
    let text = p.scenario(&b, "report-fixture");
    (b, p, text)
}

/// What `strand-eval replay` is asked for with no options: (replayfix)
/// the whole window traced.
pub fn defaults() -> Options {
    Options {
        output: None,
        traced: None,
        gpu: false,
    }
}

/// `--traced 20`: the last 20 s traced, the rest a warm-up.
pub fn traced_20() -> Options {
    Options {
        traced: Some(20.0),
        ..defaults()
    }
}

/// Replays `dir` into a scratch tree: the scenario under its own
/// `scenarios/local/`, the report under `out/`.
pub fn replay_of(dir: &Path, opts: &Options) -> (replay::Replayed, PathBuf) {
    let root = dir.parent().unwrap().parent().unwrap().to_path_buf();
    let to = root.join("scenarios/local/report-fixture.scn");
    let out = root.join("out");
    let r = replay::replay(dir, opts, Some(&to), &out, None, BuildInfo::default()).unwrap();
    (r, out)
}

pub fn steps(text: &str) -> Vec<&str> {
    text.split("=== steps\n").nth(1).unwrap().lines().collect()
}

/// `drawn.json`: the live boxes of the bar (slot 1), `nodes`.
pub fn write_drawn(dir: &Path, nodes: &[replay::drawn::Drawn]) {
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
    let drawn = json!({"format": 1, "surfaces": [{"slot": 1, "root": "3.1", "nodes": nodes}]});
    std::fs::write(
        dir.join("drawn.json"),
        serde_json::to_vec_pretty(&drawn).unwrap(),
    )
    .unwrap();
}
