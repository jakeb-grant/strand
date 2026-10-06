//! The theme swap budget (design.md: a swap "springs the colours in
//! OKLab in under 5 ms of work"): design.md's `theme.strand` and the
//! hello bar compiled and instantiated, a renderer showing the bar, and
//! `theme.look` written for light↔dark, auto→mocha, mocha→wallpaper and
//! wallpaper→auto. A swap's work is logic's re-resolve (the write, the
//! flush and the `SetTokens` it sends), the render thread applying the
//! table (planning with its contrast play-through, evaluating the new
//! token graph once) and the swap's work in every frame until it
//! settles (the roots sampled and the frame's token graph evaluated
//! from them, which the frame's nodes then read:
//! `Renderer::take_swap_work`). The median of each swap must stay under
//! 5 ms on an optimised build (CI: `cargo test --release -p
//! strand-render --test theme_swap_bench`). A debug build does the same
//! work about four times slower, so there the gate is four times the
//! budget: it still fails on a regression of the work's shape (a check
//! per node, a palette played through without end).
//!
//! A swap no spring keeps readable crossfades instead; its work (the
//! same, plus taking a snapshot of each surface once) is held to the
//! same 5 ms, on two 2560×36 bars and a launcher-sized 1280×960 panel,
//! painting into one buffer (age 1: the snapshot is the buffer's copy)
//! and into two in turn (age 2: the copy, with the clock tick it missed
//! drawn again). Blending each crossfade frame with its snapshot is a
//! cost of painting that frame, held to [`BLEND_BUDGET`] per frame for
//! all three surfaces in optimised builds (a per-byte loop unoptimised
//! is some twenty times slower, so a debug build only reports it).
//!
//! `set { }` subtrees (0, 8 and 32 distinct scopes) and a slow
//! `$motion.effects` (`spring(120, 1)`, about a second) hold the swap's
//! once-per-swap work to the same 5 ms and each frame's to a twentieth
//! of it, and along the design's `spring(1600, 1)` the whole swap
//! (logic's re-resolve on design.md's theme, the apply and every
//! frame's work) to the 5 ms (decisions.md, wave3-theme fixer rounds 2
//! and 3).

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

/// What blending one crossfade frame of every surface may cost (a
/// quarter of a 60 Hz frame), optimised.
const BLEND_BUDGET: Duration = Duration::from_micros(4_000);

/// The gate this build is held to (see the module doc).
fn gate() -> Duration {
    if cfg!(debug_assertions) {
        BUDGET * 4
    } else {
        BUDGET
    }
}

fn median(v: &[Duration]) -> Duration {
    let mut sorted = v.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
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

    /// Paints frames until the bar is idle; returns the swap's work in
    /// them (`Renderer::take_swap_work`).
    fn settle(&mut self) -> Duration {
        let mut frames = 0;
        self.r.take_swap_work();
        while self.r.wants_frame(S) {
            self.k += 1;
            let t = self.time();
            self.buf.paint_at(&mut self.r, S, 1, t);
            frames += 1;
            assert!(frames < 600, "never settled");
        }
        self.r.take_swap_work()
    }

    /// Writes `theme.look`; returns logic's time (the write and the
    /// flush that sends the new table) and the render thread's applying
    /// it. `must`: the look changes the table.
    fn look(&mut self, v: &str, must: bool) -> (Duration, Duration) {
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
        let started = Instant::now();
        assert!(self.r.apply(u.diff).is_empty());
        let apply = started.elapsed();
        // Already counted in `apply`.
        self.r.take_swap_work();
        (logic, apply)
    }
}

/// A shell on design.md's theme and the hello bar, its wallpaper's seed
/// cached and every frame painted: (shell, its temp dir, storage).
fn bench_shell(name: &str) -> (Shell, std::path::PathBuf, Storage) {
    let dir = temp_dir(name);
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
    (shell, dir, storage)
}

/// Drops `shell` and its temp dir once its state is written.
fn finish(shell: Shell, dir: std::path::PathBuf, storage: Storage) {
    drop(shell);
    if let Some(p) = &storage.persist {
        assert!(p.sync(Duration::from_secs(5)));
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_theme_swap_is_under_five_milliseconds_of_work() {
    let (mut shell, dir, storage) = bench_shell("bench");
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
            let (logic, apply) = shell.look(to, true);
            assert!(shell.r.swapping(), "{from} → {to}: nothing springs");
            assert_eq!(shell.r.swap_crossfades(), 0, "{from} → {to} crossfaded");
            let frames = shell.settle();
            totals.push(logic + apply + frames);
            parts.push((logic, apply, frames));
        }
        let median = median(&totals);
        let i = totals.iter().position(|t| *t == median).unwrap();
        let (logic, apply, frames) = parts[i];
        eprintln!(
            "theme swap {from} → {to}: median {median:?} (logic {logic:?}, render apply {apply:?}, \
             roots and token graph over the frames {frames:?})"
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
    finish(shell, dir, storage);
}

/// A table for the crossfade: the built-in base tokens over a Material
/// palette, with `$fg` declared over `$surface` and `$surface.split`,
/// which head to black and white (`grey`: both grey).
fn split_table(grey: bool) -> TokenTable {
    let mut t = strand_theme::defaults::base_tokens();
    strand_theme::from_seed(
        hex(strand_theme::defaults::DEFAULT_SEED),
        strand_theme::Options::default(),
    )
    .insert_into(&mut t);
    t.insert("font.ui", PropValue::Font(font(14.0)));
    let mid = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let (a, b) = if grey {
        (mid, mid)
    } else {
        (Color::BLACK, Color::WHITE)
    };
    t.insert("surface", PropValue::Color(a));
    t.insert("surface.split", PropValue::Color(b));
    t.insert_contrast("fg", vec!["surface".into(), "surface.split".into()]);
    t
}

/// Each swap's work, its parts (apply, frames' work, frames), and each
/// frame's blending time.
type Rounds = (Vec<Duration>, Vec<(Duration, Duration, u32)>, Vec<Duration>);

/// One crossfade bench run: `rounds` crossfading swaps on two 2560×36
/// bars and a 1280×960 panel, with a clock tick painted between swaps.
/// `double`: each surface paints into two buffers in turn (age 2, as
/// under a compositor that holds the last buffer: a snapshot is then the
/// buffer's copy with the last frame's damage drawn again), else into
/// one (age 1). Returns each swap's work (apply with its planning, and
/// the swap's work in every frame, snapshots included), its parts, and
/// each frame's blending time.
fn crossfade_rounds(double: bool, rounds: u32) -> Rounds {
    let tok = |p: &str| PropValue::Token(TokenExpr::path(p));
    let mut b = Builder::default();
    b.diff.set_tokens(split_table(true), Transition::Instant);
    // Two bars and a launcher-sized panel, each with a card, text and a
    // clock.
    let mut roots = Vec::new();
    let mut clocks = Vec::new();
    for kind in [NodeKind::Bar, NodeKind::Bar, NodeKind::Panel] {
        let root = b.node(kind, None, vec![(Prop::Bg, tok("surface"))]);
        b.node(
            NodeKind::Box,
            Some(root),
            vec![
                (Prop::X, num(8.0)),
                (Prop::Y, num(4.0)),
                (Prop::Width, num(200.0)),
                (Prop::Height, num(28.0)),
                (Prop::Bg, tok("surface.split")),
                (Prop::Radius, tok("radius.md")),
            ],
        );
        for i in 0..6 {
            b.node(
                NodeKind::Text,
                Some(root),
                vec![
                    (Prop::X, num(240.0 + 120.0 * i as f32)),
                    (Prop::Y, num(8.0)),
                    (Prop::Text, text("Strand 12:59")),
                ],
            );
        }
        clocks.push(b.node(
            NodeKind::Text,
            Some(root),
            vec![
                (Prop::X, num(1000.0)),
                (Prop::Y, num(8.0)),
                (Prop::Text, text("12:00")),
            ],
        ));
        roots.push(root);
    }
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    let sizes = [(2560, 36), (2560, 36), (1280, 960)];
    // Per surface: its buffers, the one it paints next, and which hold
    // a frame.
    let mut bufs: Vec<(SurfaceId, [Buffer; 2], usize, [bool; 2])> = Vec::new();
    for (i, (root, (w, h))) in roots.iter().zip(sizes).enumerate() {
        let id = SurfaceId(10 + i as u32);
        r.attach_surface(id, *root);
        bufs.push((
            id,
            [Buffer::new(w, h, Scale::ONE), Buffer::new(w, h, Scale::ONE)],
            0,
            [false, false],
        ));
    }
    let ids: Vec<SurfaceId> = bufs.iter().map(|b| b.0).collect();
    let time = |k: u32| Duration::from_secs(1) + Duration::from_micros(16_667 * k as u64);
    let mut k = 0u32;
    // Paints every surface that wants a frame (`all`: every surface).
    let mut paint = |r: &mut Renderer, k: u32, all: bool| {
        for (id, b, next, held) in &mut bufs {
            if !all && !r.wants_frame(*id) {
                continue;
            }
            let i = *next;
            // Alternating, a buffer holds the frame before last.
            let age = match (held[i], double) {
                (false, _) => 0,
                (true, true) => 2,
                (true, false) => 1,
            };
            b[i].paint_at(r, *id, age, time(k));
            held[i] = true;
            if double {
                *next = 1 - i;
            }
        }
    };
    paint(&mut r, k, true);
    k += 1;
    paint(&mut r, k, true);
    let mut totals = Vec::new();
    let mut parts = Vec::new();
    let mut blends = Vec::new();
    for round in 0..rounds {
        // A clock tick on every surface before the swap: the buffer a
        // swap's first frame lands in misses it when two alternate.
        let mut d = SceneDiff::new();
        for c in &clocks {
            d.set(*c, Prop::Text, text(&format!("12:{:02}", round % 60)));
        }
        assert!(r.apply(d).is_empty());
        k += 1;
        paint(&mut r, k, true);
        let to = split_table(round % 2 == 1);
        let before = r.swap_crossfades();
        r.take_swap_work();
        let mut d = SceneDiff::new();
        d.set_tokens(to, Transition::Default);
        let started = Instant::now();
        assert!(r.apply(d).is_empty());
        let apply = started.elapsed();
        r.take_swap_work();
        assert_eq!(
            r.swap_crossfades(),
            before + 1,
            "round {round}: no crossfade"
        );
        r.take_fade_blend_work();
        let mut frames = 0;
        while ids.iter().any(|id| r.wants_frame(*id)) {
            k += 1;
            paint(&mut r, k, false);
            blends.push(r.take_fade_blend_work());
            frames += 1;
            assert!(frames < 600, "never settled");
        }
        let work = r.take_swap_work();
        totals.push(apply + work);
        parts.push((apply, work, frames));
    }
    (totals, parts, blends)
}

#[test]
fn a_crossfading_swap_is_under_five_milliseconds_of_work() {
    let gate = gate();
    for double in [false, true] {
        let (totals, parts, blends) = crossfade_rounds(double, 12);
        let m = median(&totals);
        let i = totals.iter().position(|t| *t == m).unwrap();
        let (apply, work, frames) = parts[i];
        let blend = median(&blends);
        let age = if double { 2 } else { 1 };
        eprintln!(
            "crossfading swap, buffers of age {age}: median {m:?}, worst {:?} (apply with \
             snapshots {apply:?}, frames {work:?} over {frames} frames); blend per frame {blend:?}",
            totals.iter().max().unwrap()
        );
        assert!(
            m < gate,
            "age {age}: {m:?} of work, over {gate:?} (design.md: {BUDGET:?} optimised)"
        );
        // A per-byte loop runs some twenty times slower unoptimised: the
        // blend is held to its budget in optimised builds only (CI).
        if !cfg!(debug_assertions) {
            assert!(
                blend < BLEND_BUDGET,
                "{blend:?} blending a frame, over {BLEND_BUDGET:?}"
            );
        }
    }
}

/// Light and dark tables of the default seed, both with
/// `$motion.effects` at `spring(stiffness, 1)`.
fn spring_tables(stiffness: f32) -> (TokenTable, TokenTable) {
    let t = |dark: bool| {
        let mut t = strand_theme::defaults::base_tokens();
        strand_theme::from_seed(
            hex(strand_theme::defaults::DEFAULT_SEED),
            strand_theme::Options {
                dark,
                ..strand_theme::Options::default()
            },
        )
        .insert_into(&mut t);
        t.insert("font.ui", PropValue::Font(font(14.0)));
        t.insert(
            "motion.effects",
            PropValue::Transition(Transition::of_spring(Spring::new(stiffness, 1.0).unwrap())),
        );
        t
    };
    (t(false), t(true))
}

/// The swap's work with `set { }` subtrees and slow colour springs: a
/// panel with `scopes` subtrees, each `set { $surface:
/// $surface.mix($accent, k) }` with its own `k` (each a scope the
/// contrast check plays through), and text in each; light↔dark with
/// `$motion.effects` at the design's `spring(1600, 1)` and at
/// `spring(120, 1)` (about a second). What happens once per swap
/// (logic's table aside: the render thread's apply, with its contrast
/// play-through and any snapshots) is held to the 5 ms budget whatever
/// the spring or the scopes; the work of each frame (the roots and the
/// frame's token graph) is held to a twentieth of it per frame, and
/// along the design's `spring(1600, 1)` the whole swap (logic's
/// re-resolve, measured on design.md's theme as in the first bench,
/// the apply and every frame's work) is held to the 5 ms; a slower
/// spring costs the same per frame, over more frames (decisions.md).
#[test]
fn set_scopes_and_slow_springs_stay_within_the_budget() {
    let tok = |p: &str| PropValue::Token(TokenExpr::path(p));
    // Logic's re-resolve of a light↔dark swap on design.md's theme.
    let logic = {
        let (mut shell, dir, storage) = bench_shell("logic");
        let mut logic = Vec::new();
        for i in 0..16 {
            let (l, _) = shell.look(if i % 2 == 0 { "dark" } else { "light" }, true);
            shell.settle();
            if i > 0 {
                logic.push(l);
            }
        }
        finish(shell, dir, storage);
        median(&logic)
    };
    let per_frame = BUDGET / 20;
    let gate_frame = if cfg!(debug_assertions) {
        per_frame * 4
    } else {
        per_frame
    };
    let mut report = Vec::new();
    for scopes in [0usize, 8, 32] {
        for stiffness in [1600.0f32, 120.0] {
            let (light, dark) = spring_tables(stiffness);
            let mut b = Builder::default();
            b.diff.set_tokens(light.clone(), Transition::Instant);
            let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("surface"))]);
            for i in 0..scopes {
                let mut set = TokenTable::default();
                let k = 0.04 + 0.5 * i as f32 / 32.0;
                set.insert(
                    "surface",
                    PropValue::Token(TokenExpr::path("surface").call(
                        TokenMethod::Mix,
                        vec![TokenExpr::path("accent"), TokenExpr::value(num(k))],
                    )),
                );
                let sub = b.node(
                    NodeKind::Box,
                    Some(root),
                    vec![
                        (Prop::X, num(8.0 + 40.0 * (i % 16) as f32)),
                        (Prop::Y, num(8.0 + 40.0 * (i / 16) as f32)),
                        (Prop::Width, num(36.0)),
                        (Prop::Height, num(36.0)),
                        (Prop::Tokens, PropValue::Tokens(Box::new(set))),
                        (Prop::Bg, tok("surface")),
                    ],
                );
                b.node(
                    NodeKind::Text,
                    Some(sub),
                    vec![(Prop::Text, text("12")), (Prop::Color, tok("fg"))],
                );
            }
            let mut r = renderer();
            assert!(r.apply(b.diff).is_empty());
            r.attach_surface(S, root);
            let mut buf = Buffer::new(680, 96, Scale::ONE);
            let mut k = 0u32;
            let time = |k: u32| Duration::from_secs(1) + Duration::from_micros(16_667 * k as u64);
            buf.paint_at(&mut r, S, 0, time(k));
            let mut applies = Vec::new();
            let mut frame_work = Vec::new();
            let mut frame_counts = Vec::new();
            let mut wholes = Vec::new();
            for round in 0..8 {
                let to = if round % 2 == 0 { &dark } else { &light };
                let mut d = SceneDiff::new();
                d.set_tokens(to.clone(), Transition::Default);
                r.take_swap_work();
                let started = Instant::now();
                assert!(r.apply(d).is_empty());
                applies.push(started.elapsed());
                r.take_swap_work();
                let mut frames = 0u32;
                while r.wants_frame(S) {
                    k += 1;
                    buf.paint_at(&mut r, S, 1, time(k));
                    frames += 1;
                    assert!(frames < 1200, "never settled");
                }
                let work = r.take_swap_work();
                frame_work.push(work / frames.max(1));
                frame_counts.push(frames);
                wholes.push(logic + applies[round] + work);
            }
            let apply = median(&applies);
            let each = median(&frame_work);
            let whole = median(&wholes);
            let frames = frame_counts[frame_counts.len() / 2];
            eprintln!(
                "{scopes} scopes, spring({stiffness}, 1): apply {apply:?} (worst {:?}), \
                 {each:?} per frame over {frames} frames, whole swap with logic's \
                 {logic:?}: {whole:?} (worst {:?}) ({} crossfades)",
                applies.iter().max().unwrap(),
                wholes.iter().max().unwrap(),
                r.swap_crossfades()
            );
            report.push((scopes, stiffness, apply, each, whole));
        }
    }
    // Unoptimised, evaluating tokens in `set { }` scopes is some six
    // times slower (the rest about four): a debug build is held to
    // eight times the budget here, still failing on a change of shape.
    let gate = if cfg!(debug_assertions) {
        BUDGET * 8
    } else {
        BUDGET
    };
    for (scopes, stiffness, apply, each, whole) in report {
        // Along the design's spring, the whole swap.
        if stiffness == 1600.0 {
            assert!(
                whole < gate,
                "{scopes} scopes, spring({stiffness}, 1): {whole:?} of work in all, over {gate:?}"
            );
        }
        assert!(
            apply < gate,
            "{scopes} scopes, spring({stiffness}, 1): apply {apply:?}, over {gate:?}"
        );
        assert!(
            each < gate_frame,
            "{scopes} scopes, spring({stiffness}, 1): {each:?} per frame, over {gate_frame:?}"
        );
    }
}
