//! The theme swap budget (design.md: a swap "springs the colours in
//! OKLab in under 5 ms of work"): design.md's `theme.strand` and the
//! hello bar compiled and instantiated, a renderer showing the bar, and
//! `theme.look` written for light↔dark, auto→mocha, mocha→wallpaper and
//! wallpaper→auto. A swap's work is logic's re-resolve (the write, the
//! flush and the `SetTokens` it sends), the render thread's swap work
//! (planning with its contrast play-through, the roots of every frame:
//! `Renderer::take_swap_work`) and the token graph evaluated in every
//! frame until it settles (every token path looked up, the contrast
//! guard included). The median of each swap must stay under 5 ms on an
//! optimised build (CI: `cargo test --release -p strand-render --test
//! theme_swap_bench`; about 2 ms here). A debug build does the same work
//! about four times slower, so there the gate is four times the budget:
//! it still fails on a regression of the work's shape (a check per
//! node, a palette played through without end).

mod common;

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
/// design.md's budget, for optimised builds.
const BUDGET: Duration = Duration::from_millis(5);

/// The gate this build is held to (see the module doc).
fn gate() -> Duration {
    if cfg!(debug_assertions) {
        BUDGET * 4
    } else {
        BUDGET
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../strand-compiler/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("strand-swap-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 320×180 wallpaper, mostly `major`.
fn wallpaper(path: &std::path::Path, major: [u8; 3], minor: [u8; 3]) {
    let (w, h) = (320u32, 180u32);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(if x < 240 { &major } else { &minor });
        }
    }
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
}

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    r: Renderer,
    buf: Buffer,
    /// Frames painted so far (the presentation clock).
    k: u32,
}

impl Shell {
    fn boot(config: &std::path::Path, storage: Storage) -> Shell {
        let theme = fixture("theme.strand").replace("~/.config/strand/prefs.toml", "prefs.toml");
        let mut map = SourceMap::new();
        map.add("theme.strand".to_string(), theme);
        map.add("hello_bar.strand".to_string(), fixture("hello_bar.strand"));
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        let screen = host.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1 | Display")),
                ("name", Value::text("DP-1")),
            ],
        );
        host.set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let _ = config;
        let inst = Instance::new(&rt, program, host.clone(), storage);
        let mut r = renderer();
        let u = inst.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        assert!(r.apply(u.diff).is_empty());
        let root = r.tree().surface_nodes().next().unwrap();
        r.attach_surface(S, root);
        let mut buf = Buffer::new(480, 32, Scale::ONE);
        buf.paint_at(&mut r, S, 0, Duration::from_secs(1));
        Shell {
            rt,
            host,
            inst,
            r,
            buf,
            k: 0,
        }
    }

    fn time(&self) -> Duration {
        Duration::from_secs(1) + Duration::from_micros(16_667 * self.k as u64)
    }

    /// Paints frames until the bar is idle; returns the time spent
    /// evaluating the whole token graph once per frame.
    fn settle(&mut self) -> Duration {
        let mut eval = Duration::ZERO;
        let mut frames = 0;
        while self.r.wants_frame(S) {
            self.k += 1;
            let t = self.time();
            self.buf.paint_at(&mut self.r, S, 1, t);
            let tokens = &self.r.tree().tokens;
            let started = Instant::now();
            for path in tokens.tokens.keys().chain(tokens.derived.keys()) {
                std::hint::black_box(tokens.lookup(path));
            }
            eval += started.elapsed();
            frames += 1;
            assert!(frames < 600, "never settled");
        }
        eval
    }

    /// Writes `theme.look`; returns logic's time (the write and the
    /// flush that sends the new table) and applies the diff.
    /// `must`: the look changes the table.
    fn look(&mut self, v: &str, must: bool) -> Duration {
        let look = self.host.variant("Look", v);
        let started = Instant::now();
        self.inst.set("theme.look", look).unwrap();
        let u = self.inst.flush();
        let logic = started.elapsed();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        let swapped = u
            .diff
            .ops
            .iter()
            .any(|op| matches!(op, SceneOp::SetTokens { .. }));
        assert!(swapped || !must, "{v}: no SetTokens");
        assert!(self.r.apply(u.diff).is_empty());
        logic
    }
}

#[test]
fn a_theme_swap_is_under_five_milliseconds_of_work() {
    let dir = temp_dir("bench");
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let wall = config.join("wall.png");
    wallpaper(&wall, [30, 90, 200], [240, 200, 40]);
    std::fs::write(
        config.join("prefs.toml"),
        format!("wallpaper = \"{}\"\n", wall.display()),
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &config);
    let mut shell = Shell::boot(&config, storage.clone());
    // The wallpaper's seed is cached, as at any later swap back to it.
    shell.look("wallpaper", true);
    let theme = shell.inst.theme().unwrap();
    assert!(theme.wait_images(&shell.rt, Duration::from_secs(30)));
    let u = shell.inst.flush();
    assert!(shell.r.apply(u.diff).is_empty());
    shell.settle();

    let mut report = Vec::new();
    for (from, to) in [
        ("light", "dark"),
        ("dark", "light"),
        ("auto", "mocha"),
        ("mocha", "wallpaper"),
        ("wallpaper", "auto"),
    ] {
        let mut totals = Vec::new();
        let mut parts = Vec::new();
        for _ in 0..15 {
            shell.look(from, false);
            shell.settle();
            shell.r.take_swap_work();
            let logic = shell.look(to, true);
            assert!(shell.r.swapping(), "{from} → {to}: nothing springs");
            let eval = shell.settle();
            let render = shell.r.take_swap_work();
            totals.push(logic + render + eval);
            parts.push((logic, render, eval));
        }
        let mut sorted = totals.clone();
        sorted.sort();
        let median = sorted[sorted.len() / 2];
        let i = totals.iter().position(|t| *t == median).unwrap();
        let (logic, render, eval) = parts[i];
        eprintln!(
            "theme swap {from} → {to}: median {median:?} (logic {logic:?}, render swap {render:?}, \
             token graph {eval:?} over the frames)"
        );
        report.push((median, from, to));
    }
    let gate = gate();
    for (median, from, to) in report {
        assert!(
            median < gate,
            "{from} → {to}: {median:?} of work, over {gate:?} (design.md: {BUDGET:?} optimised)"
        );
    }
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}
