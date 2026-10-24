//! (perf-bench) What each animated frame costs, and where its time goes:
//! the reusable measurement of decisions.md perf-bench, not a gate; the
//! wall-clock gates set from its before and after numbers are
//! `animated_cpu_frames_hold_their_gates` below (decisions.md
//! perf-gates).
//!
//! Strand's own animated scenes, written in `.strand` and compiled and
//! instantiated as `strand run` does (one, a WGSL `shader` node, built as
//! a diff): a morph of a card's size and radius, a shared-element
//! `morph:`, a launcher opening, a box resizing with a shadow (and its
//! radius springing), a `backdrop: blur()` card and a `glass()` one
//! moving over content, a theme crossfade and a palette spring, a list
//! scrolled by touchpad, fling and wheel, a timed shader, goo, letters
//! waving, a staggered grid entering, a fading card (fade layers), FLIP
//! glides, a cover turning on a blurred card, and a cover masked by
//! `shape(cookie)` turning in a clipped, shadowed card of rows (a masked
//! group: its mask is computed on the CPU on both legs). Each is drawn on its
//! own surface: a launcher-sized panel (940 × 640 logical, the owner's
//! launcher at 1.5 is 1410 × 960 px) or a whole screen (1920 × 1200 px at
//! scale 1, the owner's panel, and 2560 × 1440 px at 1.5, their earlier
//! monitors), at scale 1 and 1.5, at 120 Hz presentation times.
//!
//! A scene runs a warm-up cycle, then cycles of its animation (a state
//! toggled and every frame painted until it settles, or a fixed run of
//! frames for what animates for ever), each repeated on a fresh renderer.
//! Per frame it records the render thread's work (the diffs it applied
//! and the paint) and its parts (`PaintTimes`: layout, flatten, the
//! damage diff, the CPU raster's paint-cache builds, offscreen groups,
//! encoding, vello's raster and the copy, a crossfade's blend; the GPU
//! leg's lowering), the damage and the pixels rasterised, the paint
//! cache's, offscreen groups' and fade layers' builds (misses), and the
//! render thread's allocations with those of the raster's helper
//! threads drawing its cells (not the text worker's or the GPU
//! thread's); per run, the caches' bytes and the process's PSS. Legs:
//!
//! - `cpu`: the CPU renderer into two `wl_shm`-style buffers in turn
//!   (age 2, as a compositor holding the last one gives).
//! - `gpu`: the surface presented on the GPU (`Backend::GpuPresent`):
//!   the render thread flattens and lowers each frame
//!   (`Renderer::paint_gpu`), the `strand-gpu` thread draws it, read back
//!   so it is timed to the end of the GPU's run (`read`, which a
//!   presented frame does not wait for), with its phases from
//!   `strand_gpu::take_frame_phases`. On lavapipe in the container (CI's
//!   device, `STRAND_GPU_SOFTWARE=1`) or a real GPU through
//!   `scripts/container/gpu.sh` (advisory). Skipped without a device.
//!
//! Prints a table (p50 / p95 / max of each repeat, the stages' means,
//! frames over the 8.33 ms budget of 120 Hz) and writes every frame as
//! JSON. Run optimised:
//!
//! ```sh
//! cargo test --profile timing -p strand-render --test anim_bench -- \
//!     --ignored --nocapture --test-threads 1
//! ```
//!
//! Env: `STRAND_ANIM_BENCH_LEGS` (`cpu,gpu`), `STRAND_ANIM_BENCH_SCENES`
//! (names, comma-separated; default all), `STRAND_ANIM_BENCH_FITS`
//! (`1x,1.5x` of each scene's sizes), `STRAND_ANIM_BENCH_REPEATS` (3),
//! `STRAND_ANIM_BENCH_CYCLES` (a scene's own count by default),
//! `STRAND_ANIM_BENCH_JSON` (where the JSON goes; default
//! `anim_bench.json` in Cargo's target tmpdir), `STRAND_ANIM_BENCH_DEBUG`
//! (prints the GPU hand-off's state).
//!
//! Timings on a hybrid CPU move by up to 2× between runs as the thread
//! lands on a performance or an efficiency core: pin the run to the
//! performance cores (`taskset -c 0-3` on the owner's laptop, inside the
//! container) and compare repeats, not single runs (docs/benchmarks.md,
//! "Animation frame cost").

#![allow(dead_code)]

mod common;
#[cfg(feature = "gpu")]
mod gpu_host;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::{Renderer, ScrollInput, ScrollKind, TextBackend};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);
/// 120 Hz: the owner's panel.
const HZ: u32 = 120;
/// A 120 Hz frame.
const BUDGET: Duration = Duration::from_nanos(8_333_333);

// ---------------------------------------------------------------------
// Allocations of render-thread work while the bench's own thread does it:
// that thread's, and those of the raster's helper threads
// (`strand-raster`, perf-cpu-raster: a large paint draws its cells on a
// few threads, started for the paint), so a frame's count is its whole
// paint's however many threads drew it. The text worker's and the GPU
// thread's are not counted.

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
/// A `counted` call is running: helper threads count theirs.
static COUNTING_HELPERS: AtomicBool = AtomicBool::new(false);

/// The name of the raster's helper threads (`raster::share`).
const HELPER: &str = "strand-raster";

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Whether this thread is a raster helper: 0 not yet asked, 1 being
    /// asked (an allocation while asking counts as not), 2 yes, 3 no.
    static HELPER_KIND: Cell<u8> = const { Cell::new(0) };
}

/// This thread is one of the raster's helpers (asked once per thread;
/// they are started by `std::thread::Builder` with their name, so
/// asking allocates nothing).
fn is_helper() -> bool {
    HELPER_KIND
        .try_with(|k| match k.get() {
            2 => true,
            0 => {
                k.set(1);
                let yes = std::thread::current().name() == Some(HELPER);
                k.set(if yes { 2 } else { 3 });
                yes
            }
            _ => false,
        })
        .unwrap_or(false)
}

fn note(size: usize) {
    let mine = COUNTING.try_with(Cell::get).unwrap_or(false);
    if mine || (COUNTING_HELPERS.load(Ordering::Relaxed) && is_helper()) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Counts this thread's allocations, and the raster helpers' (joined
/// before the paint returns), while `f` runs.
fn counted<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    let (a, b) = (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    );
    COUNTING.with(|c| c.set(true));
    COUNTING_HELPERS.store(true, Ordering::Relaxed);
    let out = f();
    COUNTING_HELPERS.store(false, Ordering::Relaxed);
    COUNTING.with(|c| c.set(false));
    (
        out,
        ALLOCS.load(Ordering::Relaxed) - a,
        ALLOC_BYTES.load(Ordering::Relaxed) - b,
    )
}

// ---------------------------------------------------------------------
// Scenes.

/// Where a scene is drawn: a launcher-sized panel (logical, at each
/// scale) or the whole screen (physical, 1920 × 1200 at 1 and 2560 ×
/// 1440 at 1.5).
#[derive(Clone, Copy, Debug)]
enum Fit {
    Panel(f32, f32),
    Screen,
}

/// One size a scene is run at.
#[derive(Clone, Copy, Debug)]
struct Config {
    logical: (f32, f32),
    scale: f32,
}

impl Config {
    fn of(fit: Fit) -> Vec<Config> {
        match fit {
            Fit::Panel(w, h) => vec![
                Config {
                    logical: (w, h),
                    scale: 1.0,
                },
                Config {
                    logical: (w, h),
                    scale: 1.5,
                },
            ],
            Fit::Screen => vec![
                Config {
                    logical: (1920.0, 1200.0),
                    scale: 1.0,
                },
                Config {
                    logical: (2560.0 / 1.5, 1440.0 / 1.5),
                    scale: 1.5,
                },
            ],
        }
    }

    fn px(&self) -> Size {
        let s = Scale::from_f64(f64::from(self.scale)).unwrap();
        s.physical_size(LogicalSize::new(self.logical.0, self.logical.1))
    }

    fn name(&self) -> String {
        let p = self.px();
        format!("{}x{}@{}", p.w, p.h, self.scale)
    }
}

/// How a scene is made.
enum Source {
    /// `.strand` source for a panel `w` × `h` logical.
    Strand(fn(f32, f32) -> String),
    /// A diff, for what a source cannot name offline (a WGSL file).
    Built(fn(&mut Builder, f32, f32)),
}

/// How a cycle ends: when the surface wants no frame (and at most this
/// many), or after exactly this many.
#[derive(Clone, Copy)]
enum Frames {
    Settle(u32),
    Fixed(u32),
}

struct Scene {
    name: &'static str,
    about: &'static str,
    fit: Fit,
    source: Source,
    /// Called before each frame of cycle `cycle` (`f`: the frame in the
    /// cycle, 0 first): starts the animation (a state toggled, a theme
    /// swapped, a scroll).
    drive: fn(&mut Bench, u32, u32),
    frames: Frames,
    cycles: u32,
    /// Drawn by the CPU leg (a `shader` node draws nothing there: the
    /// CPU leg has no device for its pass).
    cpu: bool,
}

/// The panel every source scene is drawn on: `w` × `h`, transparent,
/// its content centred.
fn panel(w: f32, h: f32, body: &str) -> String {
    format!(
        "panel Bench {{ anchor: top_left; layer: overlay; open: true; keyboard: none
  width: {w}; height: {h}; align: center; justify: center
{body}
}}
"
    )
}

fn toggle(name: &'static str) -> impl Fn(&mut Bench, u32, u32) {
    move |b, cycle, f| {
        if f == 0 {
            b.set(name, Value::Bool(cycle % 2 == 0));
        }
    }
}

fn idle(_: &mut Bench, _: u32, _: u32) {}

const RESULTS: &str = r#"
    for a in ["Firefox", "Terminal", "Files", "Settings", "Music", "Calendar", "Mail", "Notes"] key a {
      row { height: 48; pad: 0, 12; gap: 12; radius: 12; align: center
            bg: a == "Firefox" ? #313244 : #00000000
        box { size: 32; radius: 9; bg: linear(135deg, #89b4fa, #cba6f7) }
        col { gap: 2; grow: 1
          text a { color: #cdd6f4; weight: 600 }
          text "Application launcher result" { color: #a6adc8; ellipsis: end }
        }
      }
    }"#;

fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "morph-card",
            about: "an island card's width, height and radius spring between a pill and a dashboard, with a shadow, a gradient and clipped text",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  col { width: big ? 760 : 240; height: big ? 480 : 48; radius: big ? 28 : 24
        pad: 12; gap: 10; bg: #1e1e2e; clip: true; shadow: 0 16px 48px #00000066
    row { gap: 10; align: center; height: 24
      box { size: 22; radius: 6; bg: linear(90deg, #f38ba8, #fab387) }
      text \"Now playing: a rather long song title\" { color: #cdd6f4; ellipsis: end; shrink: 1; min_width: 0 }
    }
    box { width: 720; height: 380; radius: 18; bg: linear(160deg, #313244, #45475a) }
  }",
                )
                .replace("panel Bench", "state big = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("big")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "shared-morph",
            about: "a pill and a card share a `morph:` name: each change morphs one into the other's box",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  stack { width: 800; height: 520
    if big {
      col { width: 760; height: 480; radius: 28; pad: 16; gap: 12; bg: #1e1e2e
            morph: \"media\"; shadow: 0 16px 48px #00000066
        box { width: 728; height: 360; radius: 18; bg: linear(160deg, #f38ba8, #89b4fa) }
        text \"A rather long song title\" { color: #cdd6f4; ellipsis: end }
      }
    } else {
      row { width: 220; height: 44; radius: 22; pad: 0, 12; gap: 8; bg: #313244; align: center
            morph: \"media\"
        box { size: 24; radius: 6; bg: linear(90deg, #f38ba8, #89b4fa) }
        text \"Song\" { color: #cdd6f4 }
      }
    }
  }",
                )
                .replace("panel Bench", "state big = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("big")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "panel-open",
            about: "a launcher card enters from `enter { opacity: 0; scale: 0.92; y: 24 }` and leaves by it: a search row and eight results with icons and text",
            fit: Fit::Screen,
            source: Source::Strand(|w, h| {
                let body = format!(
                    "  if open {{
    col {{ width: 640; pad: 16; gap: 8; radius: 24; bg: #1e1e2ef2
          shadow: 0 24px 64px #00000080; enter {{ opacity: 0; scale: 0.92; y: 24 }}
      row {{ height: 44; pad: 0, 14; radius: 14; bg: #313244; align: center
        text \"Search applications\" {{ color: #a6adc8 }}
      }}{RESULTS}
    }}
  }}"
                );
                panel(w, h, &body).replace("panel Bench", "state open = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("open")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "resize-shadow",
            about: "a launcher card (corners 18, 18, 32, 32) with a 0 16px 48px shadow resizes as its size springs",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  col { width: big ? 820 : 560; height: big ? 560 : 380; radius: 18, 18, 32, 32
        bg: #313244; shadow: 0 16px 48px #00000066; pad: 16; gap: 8
    text \"Launcher\" { color: #cdd6f4 }
  }",
                )
                .replace("panel Bench", "state big = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("big")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "resize-radius",
            about: "resize-shadow with its corners springing too (12 to 40)",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  col { width: big ? 820 : 560; height: big ? 560 : 380; radius: big ? 40 : 12
        bg: #313244; shadow: 0 16px 48px #00000066; pad: 16; gap: 8
    text \"Launcher\" { color: #cdd6f4 }
  }",
                )
                .replace("panel Bench", "state big = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("big")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "backdrop-blur",
            about: "a `backdrop: blur(24)` card with text glides across a screen of coloured cards",
            fit: Fit::Screen,
            source: Source::Strand(|w, h| {
                let body = "  stack { width: 1600; height: 900
    box { width: 1600; height: 900; radius: 32; bg: linear(120deg, #1e1e2e, #45475a) }
    box { place: absolute; x: 120; y: 140; width: 360; height: 260; radius: 40; bg: #f38ba8 }
    box { place: absolute; x: 560; y: 380; width: 420; height: 300; radius: 60; bg: #a6e3a1 }
    box { place: absolute; x: 1040; y: 120; width: 380; height: 380; radius: 190; bg: #89b4fa }
    box { place: absolute; x: 300; y: 560; width: 300; height: 200; radius: 30; bg: #f9e2af }
    col { place: absolute; x: moved ? 980 : 100; y: 220; width: 520; height: 380; radius: 28; pad: 20; gap: 10
          bg: #1e1e2e80; backdrop: blur(24)
      text \"Frosted card\" { color: #ffffff; weight: 600 }
      text \"Content behind it is blurred every frame it moves\" { color: #ffffffcc }
    }
  }";
                panel(w, h, body).replace("panel Bench", "state moved = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("moved")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "glass",
            about: "a `backdrop: glass()` card glides over coloured cards",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let body = "  stack { width: 900; height: 600
    box { width: 900; height: 600; radius: 28; bg: linear(120deg, #1e1e2e, #45475a) }
    box { place: absolute; x: 60; y: 80; width: 260; height: 200; radius: 40; bg: #f38ba8 }
    box { place: absolute; x: 380; y: 300; width: 300; height: 220; radius: 60; bg: #a6e3a1 }
    box { place: absolute; x: 640; y: 60; width: 220; height: 220; radius: 110; bg: #89b4fa }
    col { place: absolute; x: moved ? 480 : 40; y: 160; width: 380; height: 280; radius: 28; pad: 20
          bg: #ffffff14; backdrop: glass()
      text \"Glass\" { color: #ffffff; weight: 600 }
    }
  }";
                panel(w, h, body).replace("panel Bench", "state moved = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("moved")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "crossfade",
            about: "a theme swap no spring keeps readable crossfades the whole screen: a dashboard in $surface, $surface.hi and $fg",
            fit: Fit::Screen,
            source: Source::Strand(|w, h| panel(w, h, THEMED)),
            drive: |b, c, f| {
                if f == 0 {
                    let (x, y) = split_tables();
                    b.tokens(if c % 2 == 0 { y } else { x }, Transition::Default);
                }
            },
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "theme-spring",
            about: "a light to dark palette swap: every token-bound colour of the dashboard springs in OKLab",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| panel(w, h, THEMED)),
            drive: |b, c, f| {
                if f == 0 {
                    b.tokens(material(c % 2 == 0), Transition::Default);
                }
            },
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "list-scroll",
            about: "a virtualised list of 400 rows (icon and two texts) swiped by touchpad, flung, then wheel-stepped back",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let rows: Vec<String> = (0..400).map(|i| format!("\"App {i:03}\"")).collect();
                let body = format!(
                    "  col {{ width: 640; pad: 12; radius: 20; bg: #1e1e2e
    list {{ id: picks; height: 560; gap: 4
      for a in [{}] key a {{
        row {{ height: 52; pad: 0, 12; gap: 12; radius: 10; bg: #313244; align: center
          box {{ size: 32; radius: 8; bg: #89b4fa }}
          col {{ gap: 2
            text a {{ color: #cdd6f4 }}
            text \"Browse the web\" {{ color: #a6adc8 }}
          }}
        }}
      }}
    }}
  }}",
                    rows.join(", ")
                );
                panel(w, h, &body)
            }),
            drive: |b, c, f| {
                let at = LogicalPoint::new(b.logical.0 / 2.0, b.logical.1 / 2.0);
                let time = b.ms();
                let input = match c % 2 {
                    // A swipe down, a frame's worth each, then a fling.
                    0 if f < 24 => Some(ScrollInput {
                        dy: 30.0,
                        kind: ScrollKind::Touch,
                        time,
                    }),
                    0 if f == 24 => Some(ScrollInput {
                        dy: 0.0,
                        kind: ScrollKind::Lift,
                        time,
                    }),
                    // Wheel steps back up.
                    1 if f < 30 && f % 3 == 0 => Some(ScrollInput {
                        dy: -60.0,
                        kind: ScrollKind::Wheel,
                        time,
                    }),
                    _ => None,
                };
                if let Some(input) = input {
                    b.r.scroll_input(S, at, input);
                }
            },
            frames: Frames::Settle(360),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "shader",
            about: "a 760 × 480 WGSL `shader` node reading `strand.time` beside text (GPU: in the frame; CPU leg without a device: draws nothing)",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Built(shader_scene),
            drive: idle,
            frames: Frames::Fixed(60),
            cycles: 5,
            cpu: false,
        },
        Scene {
            name: "goo",
            about: "`merge 28`: five blobs swinging on `wave()` melt together",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  merge 28 { width: 800; height: 400
    box { place: absolute; x: 100 + 120 * wave(2s); y: 150; size: 100; radius: 50; bg: #89b4fa }
    box { place: absolute; x: 300 - 100 * wave(3s); y: 140; size: 120; radius: 60; bg: #89b4fa }
    box { place: absolute; x: 480 + 90 * wave(2.5s); y: 160; size: 90; radius: 45; bg: #89b4fa }
    box { place: absolute; x: 600; y: 120 + 80 * wave(1.5s); size: 110; radius: 55; bg: #89b4fa }
    box { place: absolute; x: 200; y: 220 + 60 * wave(1.8s); size: 80; radius: 40; bg: #89b4fa }
  }",
                )
            }),
            drive: idle,
            frames: Frames::Fixed(60),
            cycles: 5,
            cpu: true,
        },
        Scene {
            name: "letters",
            about: "two 40 px lines whose `letters` wave, turn and fade by index",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                panel(
                    w,
                    h,
                    "  col { gap: 20; font: \"DejaVu Sans\" 40px 600
    text \"Strand animates every letter\" { color: #f5c2e7
      letters { y: 8 * wave(1s, phase: index * 0.08); rotate: 6deg * wave(2s, phase: index * 0.05) }
    }
    text \"one at a time, at 120 Hz\" { color: #89dceb
      letters { y: 6 * wave(1.2s, phase: index * 0.1); opacity: 0.6 + 0.4 * wave(1s, phase: index * 0.1) }
    }
  }",
                )
            }),
            drive: idle,
            frames: Frames::Fixed(60),
            cycles: 5,
            cpu: true,
        },
        Scene {
            name: "stagger",
            about: "a 6 × 4 app grid's tiles enter 20 ms apart from `enter { opacity: 0; scale: 0.85 }` (and leave)",
            fit: Fit::Screen,
            source: Source::Strand(|w, h| {
                let tiles: Vec<String> = (0..24).map(|i| format!("\"T{i}\"")).collect();
                let body = format!(
                    "  if open {{
    col {{ width: 820; pad: 20; radius: 24; bg: #1a1b22e6
      grid {{ columns: 6; gap: 14; stagger: 20ms
        for a in [{}] key a {{
          col {{ height: 112; gap: 8; align: center; justify: center; radius: 16; bg: #313244
                enter {{ opacity: 0; scale: 0.85 }}
            box {{ size: 56; radius: 14; bg: linear(135deg, #89b4fa, #cba6f7) }}
            text a {{ color: #cdd6f4 }}
          }}
        }}
      }}
    }}
  }}",
                    tiles.join(", ")
                );
                panel(w, h, &body).replace("panel Bench", "state open = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("open")(b, c, f),
            frames: Frames::Settle(300),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "fade",
            about: "a card of text, icons and a gradient fades between opacity 1 and 0.15 (a fade layer)",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let body = format!(
                    "  col {{ width: 640; pad: 16; gap: 8; radius: 24; bg: #1e1e2e
        opacity: dim ? 0.15 : 1; shadow: 0 16px 48px #00000066{RESULTS}
  }}"
                );
                panel(w, h, &body).replace("panel Bench", "state dim = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("dim")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "flip",
            about: "a banner mounted at the top of a column of twelve rows: the rows glide down (FLIP), and back up when it goes",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let rows: Vec<String> = (0..12).map(|i| format!("\"Row {i}\"")).collect();
                let body = format!(
                    "  col {{ width: 640; pad: 12; gap: 6; radius: 20; bg: #1e1e2e
    if banner {{
      box {{ width: 616; height: 90; radius: 14; bg: linear(90deg, #f38ba8, #fab387)
            enter {{ opacity: 0; height: 0 }} }}
    }}
    for a in [{}] key a {{
      row {{ height: 34; pad: 0, 12; radius: 8; bg: #313244; align: center
        text a {{ color: #cdd6f4 }}
      }}
    }}
  }}",
                    rows.join(", ")
                );
                panel(w, h, &body).replace("panel Bench", "state banner = false\npanel Bench")
            }),
            drive: |b, c, f| toggle("banner")(b, c, f),
            frames: Frames::Settle(240),
            cycles: 6,
            cpu: true,
        },
        Scene {
            name: "cover-turn",
            about: "the launcher's case: an 80 px cover turning (`rotate: t * 40deg`) on a `backdrop: blur(24)` card over coloured content",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let art = cover_path();
                let body = format!(
                    "  stack {{ width: 900; height: 600
    box {{ width: 900; height: 600; radius: 28; bg: linear(120deg, #1e1e2e, #45475a) }}
    box {{ place: absolute; x: 60; y: 80; width: 260; height: 200; radius: 40; bg: #f38ba8 }}
    box {{ place: absolute; x: 520; y: 300; width: 300; height: 220; radius: 60; bg: #a6e3a1 }}
    col {{ place: absolute; x: 120; y: 120; width: 660; height: 360; radius: 28; pad: 24; gap: 12
          bg: #1e1e2e99; backdrop: blur(24)
      row {{ gap: 16; align: center
        image \"file://{art}\" {{ width: 80; height: 80; radius: 40; fit: cover; clip: true; rotate: t * 40deg }}
        col {{ gap: 4
          text \"Night Drive\" {{ color: #ffffff; weight: 600 }}
          text \"The Midnight\" {{ color: #ffffffb3 }}
        }}
      }}
    }}
  }}"
                );
                panel(w, h, &body)
            }),
            drive: idle,
            frames: Frames::Fixed(60),
            cycles: 5,
            cpu: true,
        },
        Scene {
            name: "cover-mask",
            about: "a 40 px cover masked by `shape(cookie)` turning (`rotate: t * 20deg`) in a clipped, translucent, shadowed card of result rows",
            fit: Fit::Panel(940.0, 640.0),
            source: Source::Strand(|w, h| {
                let art = cover_path();
                let body = format!(
                    "  col {{ width: 820; pad: 16; gap: 8; radius: 28; clip: true; bg: #1e1e2eeb
        shadow: 0 24px 64px #00000080
    row {{ height: 56; pad: 0, 12; gap: 12; radius: 14; bg: #313244; align: center
      image \"file://{art}\" {{ size: 40; fit: cover; mask: shape(cookie); rotate: t * 20deg }}
      col {{ gap: 2
        text \"Night Drive\" {{ color: #ffffff; weight: 600 }}
        text \"The Midnight\" {{ color: #ffffffb3 }}
      }}
    }}{RESULTS}
  }}"
                );
                panel(w, h, &body)
            }),
            drive: idle,
            frames: Frames::Fixed(60),
            cycles: 5,
            cpu: true,
        },
    ]
}

/// A dashboard bound to theme tokens (the theme scenes).
const THEMED: &str = "  col { width: 860; pad: 20; gap: 12; radius: 24; bg: $surface; color: $fg
    row { gap: 12
      col { width: 410; height: 200; radius: 16; pad: 16; gap: 8; bg: $surface.hi
        text \"Now playing\" { color: $fg; weight: 600 }
        text \"Night Drive, The Midnight\" { color: $fg }
      }
      col { width: 410; height: 200; radius: 16; pad: 16; gap: 8; bg: $surface.hi
        text \"Calendar\" { color: $fg; weight: 600 }
        text \"Stand-up at 10:00\" { color: $fg }
      }
    }
    row { gap: 12
      for a in [\"Wi-Fi\", \"Bluetooth\", \"Night light\", \"Do not disturb\"] key a {
        row { width: 200; height: 52; pad: 0, 12; radius: 14; align: center; bg: $surface.hi
          text a { color: $fg }
        }
      }
    }
    col { height: 160; radius: 16; pad: 16; gap: 6; bg: $surface.hi
      text \"Notifications\" { color: $fg; weight: 600 }
      text \"A message from a friend about the weekend\" { color: $fg }
      text \"Your download has finished\" { color: $fg }
    }
  }";

/// The crossfade's tables (as gpu.rs's): grey `$surface` and
/// `$surface.hi` with `$fg` over both, then black and white, which
/// no spring keeps readable.
fn split_tables() -> (TokenTable, TokenTable) {
    use strand_theme::{Options, from_seed};
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let palette = from_seed(
        hex(strand_theme::defaults::DEFAULT_SEED),
        Options {
            dark: false,
            ..Options::default()
        },
    );
    let mut a = strand_theme::defaults::base_tokens();
    palette.insert_into(&mut a);
    a.insert("font.ui", PropValue::Font(font(14.0)));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert("surface.hi", PropValue::Color(grey));
        t.insert_contrast("fg", vec!["surface".into(), "surface.hi".into()]);
    }
    a.insert("surface", PropValue::Color(grey));
    b.insert("surface", PropValue::Color(Color::BLACK));
    b.insert("surface.hi", PropValue::Color(Color::WHITE));
    (a, b)
}

/// The default seed's Material palette, light or dark, over the base
/// tokens.
fn material(dark: bool) -> TokenTable {
    use strand_theme::{Options, from_seed};
    let mut t = strand_theme::defaults::base_tokens();
    from_seed(
        hex(strand_theme::defaults::DEFAULT_SEED),
        Options {
            dark,
            ..Options::default()
        },
    )
    .insert_into(&mut t);
    t.insert("font.ui", PropValue::Font(font(14.0)));
    let c = if dark { hex("#2b2930") } else { hex("#ece6f0") };
    t.insert("surface.hi", PropValue::Color(c));
    t.insert_contrast("fg", vec!["surface".into(), "surface.hi".into()]);
    t
}

/// A 256 × 256 cover (four colours and a ring), written once.
fn cover_path() -> String {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("anim_bench");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("cover.png");
    if !p.exists() {
        let mut rgba = Vec::with_capacity(256 * 256 * 4);
        for y in 0..256i32 {
            for x in 0..256i32 {
                let d = ((x - 128).pow(2) + (y - 128).pow(2)) as f32;
                let ring = (d.sqrt() - 80.0).abs() < 10.0;
                let c = match (x < 128, y < 128, ring) {
                    (_, _, true) => [255, 255, 255, 255],
                    (true, true, _) => [0xf3, 0x8b, 0xa8, 255],
                    (false, true, _) => [0x89, 0xb4, 0xfa, 255],
                    (true, false, _) => [0xa6, 0xe3, 0xa1, 255],
                    (false, false, _) => [0xf9, 0xe2, 0xaf, 255],
                };
                rgba.extend_from_slice(&c);
            }
        }
        write_png(&p, 256, 256, &rgba);
    }
    p.display().to_string()
}

/// A panel holding a timed WGSL shader node and a caption.
fn shader_scene(b: &mut Builder, w: f32, h: f32) {
    use strand_scene::shader::{ShaderCode, UniformType};
    let code = Arc::new(ShaderCode {
        path: "bench.wgsl".into(),
        wgsl: "@group(1) @binding(0) var<uniform> u_tint: vec4<f32>;\n\
               @fragment\n\
               fn main(v: StrandVertex) -> @location(0) vec4<f32> {\n\
                   let k = 0.5 + 0.5 * sin(strand.time * 2.0 + v.uv.x * 9.0 + v.uv.y * 5.0);\n\
                   return vec4<f32>(u_tint.rgb * k, 1.0);\n\
               }\n"
        .into(),
        uniforms: ShaderCode::packed(vec![("u_tint".into(), UniformType::Vec4, 0)]),
    });
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Align, PropValue::Keyword("center".into())),
            (Prop::Justify, PropValue::Keyword("center".into())),
        ],
    );
    let card = b.node(
        NodeKind::Col,
        Some(root),
        vec![
            (Prop::Pad, num(16.0)),
            (Prop::Gap, num(10.0)),
            (Prop::Radius, num(24.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    b.node(
        NodeKind::Shader,
        Some(card),
        vec![
            (Prop::Width, num(760.0)),
            (Prop::Height, num(480.0)),
            (Prop::Radius, num(16.0)),
            (Prop::Shader, PropValue::Shader(code)),
            (
                Prop::Uniforms,
                PropValue::Uniforms(vec![("u_tint".into(), color("#89b4fa"))]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(card),
        vec![
            (Prop::Text, text("A shader, every frame")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(16.0))),
        ],
    );
}

// ---------------------------------------------------------------------
// The harness.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leg {
    Cpu,
    Gpu,
}

impl Leg {
    fn name(self) -> &'static str {
        match self {
            Leg::Cpu => "cpu",
            Leg::Gpu => "gpu",
        }
    }
}

/// One frame's measurements (durations in µs).
#[derive(Clone, Debug, Default)]
struct Sample {
    cycle: u32,
    /// The render thread's work: the diffs applied before it, and the
    /// paint (for the GPU leg, `paint_gpu`: flatten and lowering).
    work: f64,
    apply: f64,
    flatten: f64,
    layout: f64,
    text_wait: f64,
    diff: f64,
    lower: f64,
    raster: f64,
    prepare: f64,
    offscreen: f64,
    encode: f64,
    fine: f64,
    copy: f64,
    blend: f64,
    cells: u32,
    /// The GPU thread's span, its CPU part (up to submit) and the wait
    /// for the GPU's run and the readback (`read`); its named phases.
    gpu_total: f64,
    gpu_cpu: f64,
    gpu_read: f64,
    gpu_build: f64,
    gpu_render: f64,
    gpu_upload: f64,
    uploads: u64,
    upload_px: u64,
    damage_px: u64,
    raster_px: u64,
    allocs: u64,
    alloc_bytes: u64,
    /// Builds (misses) during this frame: paint cache entries, offscreen
    /// groups, fade layers built, fade layers drawn, raster nodes.
    cache_builds: u64,
    offscreen_builds: u64,
    fade_builds: u64,
    fade_draws: u64,
    raster_builds: u64,
    shadow_px: u64,
}

/// Counters read after each frame (their deltas are the frame's).
#[derive(Clone, Copy, Default)]
struct Counters {
    cache: u64,
    offscreen: u64,
    fade_builds: u64,
    fade_draws: u64,
    rasters: u64,
    shadow: u64,
}

impl Counters {
    fn read(r: &Renderer) -> Counters {
        let (fb, fd) = r.fade_layers();
        Counters {
            cache: r.paint_cache().1,
            offscreen: r.offscreen_cache().1,
            fade_builds: fb,
            fade_draws: fd,
            rasters: r.raster_nodes().0,
            shadow: r.blurred_shadow_px(),
        }
    }
}

#[cfg(feature = "gpu")]
struct GpuSide {
    gpu: strand_gpu::Gpu,
    pings: std::sync::mpsc::Receiver<()>,
}

/// One scene on one renderer and surface.
struct Bench {
    r: Renderer,
    inst: Option<Instance>,
    leg: Leg,
    logical: (f32, f32),
    size: Size,
    scale: Scale,
    /// Two buffers painted in turn (the CPU leg).
    bufs: [Buffer; 2],
    painted: [bool; 2],
    turn: usize,
    k: u32,
    /// Render-thread work done since the last frame (applying diffs).
    pending: Duration,
    pending_allocs: (u64, u64),
    counters: Counters,
    #[cfg(feature = "gpu")]
    gpu: Option<GpuSide>,
}

fn period() -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(HZ))
}

fn worker_text() -> TextBackend {
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    TextBackend::Worker(worker)
}

impl Bench {
    /// Boots `scene` at `cfg` for `leg`, with its warm-up done.
    fn boot(scene: &Scene, cfg: Config, leg: Leg) -> Option<Bench> {
        let mut r = Renderer::new(worker_text());
        r.set_busy_window(Duration::ZERO);
        r.set_query_wait(Duration::from_secs(30));
        let scale = Scale::from_f64(f64::from(cfg.scale)).unwrap();
        let (w, h) = cfg.logical;
        let (inst, root) = match &scene.source {
            Source::Strand(src) => {
                let mut map = SourceMap::new();
                map.add("shell.strand".to_string(), src(w, h));
                let compiled = strand_compiler::compile(&map);
                assert_eq!(
                    compiled.errors(),
                    0,
                    "{}: {:?}",
                    scene.name,
                    compiled.diagnostics
                );
                let program = Arc::new(lower::lower(
                    &compiled.program,
                    strand_compiler::schema::Schema::builtin(),
                ));
                let rt = Runtime::new();
                let host = Rc::new(SchemaHost::mock(&rt, &program.types));
                let screen = host.record(
                    "Screen",
                    &[
                        ("id", Value::text("Mock | DP-1")),
                        ("name", Value::text("DP-1")),
                        ("width", Value::float(f64::from(w))),
                        ("height", Value::float(f64::from(h))),
                        ("scale", Value::float(f64::from(cfg.scale))),
                    ],
                );
                host.set(&rt, "screens.all", Value::list(vec![screen]))
                    .unwrap();
                let inst = Instance::new(&rt, program, host.clone(), Storage::none());
                let diff = inst.flush().diff;
                let errors = r.apply(diff);
                assert!(errors.is_empty(), "{}: {errors:?}", scene.name);
                let root = r
                    .tree()
                    .surface_nodes()
                    .find(|n| r.tree().get(*n).is_some_and(|n| n.kind == NodeKind::Panel))
                    .expect("a panel");
                (Some(inst), root)
            }
            Source::Built(build) => {
                let mut b = Builder::default();
                build(&mut b, w, h);
                assert!(r.apply(b.diff).is_empty());
                (None, r.tree().roots()[0])
            }
        };
        if matches!(scene.name, "crossfade" | "theme-spring") {
            let mut d = SceneDiff::new();
            d.set_tokens(
                if scene.name == "crossfade" {
                    split_tables().0
                } else {
                    material(false)
                },
                Transition::Instant,
            );
            assert!(r.apply(d).is_empty());
        }
        let size = cfg.px();
        r.attach_surface(S, root);
        r.configure_surface(S, size, scale);
        r.set_surface_origin(
            S,
            Some((Some("DP-1".to_string()), LogicalPoint::new(0.0, 0.0))),
        );
        let mut b = Bench {
            r,
            inst,
            leg,
            logical: (w, h),
            size,
            scale,
            bufs: [
                Buffer::new(size.w, size.h, scale),
                Buffer::new(size.w, size.h, scale),
            ],
            painted: [false; 2],
            turn: 0,
            k: 0,
            pending: Duration::ZERO,
            pending_allocs: (0, 0),
            counters: Counters::default(),
            #[cfg(feature = "gpu")]
            gpu: None,
        };
        // The first frames on the CPU: text shaped, images decoded,
        // everything at rest.
        b.r.set_first_frame_wait(Duration::from_secs(10));
        b.settle(400);
        if leg == Leg::Gpu && !b.move_to_gpu() {
            return None;
        }
        b.counters = Counters::read(&b.r);
        Some(b)
    }

    /// Paints until nothing wants a frame (at most `cap` frames), text
    /// and images waited for.
    fn settle(&mut self, cap: u32) {
        for _ in 0..cap {
            self.r.update();
            if !self.r.wants_frame(S) {
                let more = self.r.wait_for_text(Duration::from_millis(200));
                self.r.update();
                if !more && !self.r.wants_frame(S) && !self.r.work_pending() {
                    break;
                }
                if !self.r.wants_frame(S) {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
            }
            self.frame(u32::MAX);
        }
        self.pending = Duration::ZERO;
    }

    /// The presentation time of the next frame, in ms (input times).
    fn ms(&self) -> u32 {
        ((T0 + period() * self.k).as_millis() & 0xffff_ffff) as u32
    }

    /// Applies a diff as the render thread does (its work counts toward
    /// the next frame), then logic's answers.
    fn apply(&mut self, diff: SceneDiff) {
        let start = Instant::now();
        let (errors, n, bytes) = counted(|| self.r.apply(diff));
        self.pending += start.elapsed();
        self.pending_allocs.0 += n;
        self.pending_allocs.1 += bytes;
        assert!(errors.is_empty(), "{errors:?}");
    }

    fn set(&mut self, name: &str, v: Value) {
        let Some(inst) = &self.inst else { return };
        inst.set_value("shell", name, v).unwrap();
        let diff = inst.flush().diff;
        self.apply(diff);
    }

    fn tokens(&mut self, t: TokenTable, how: Transition) {
        let mut d = SceneDiff::new();
        d.set_tokens(t, how);
        self.apply(d);
    }

    /// Logic's side after a frame: layout facts and list windows
    /// answered at once.
    fn answer(&mut self) {
        for _ in 0..4 {
            let Some(inst) = &self.inst else { return };
            let facts = self.r.take_layout_facts();
            let windows = self.r.take_list_windows();
            if facts.is_empty() && windows.is_empty() {
                return;
            }
            for (n, b) in facts {
                inst.set_box(n, b);
            }
            for (list, rows) in windows {
                inst.set_list_window(list, rows.start, rows.end.saturating_sub(rows.start));
            }
            let mut diff = inst.flush().diff;
            diff.layout_seen = Some(self.r.layout_seq());
            self.apply(diff);
        }
    }

    /// Paints the next frame; returns its measurements (`cycle`
    /// recorded), or `None` when nothing was drawn.
    fn frame(&mut self, cycle: u32) -> Option<Sample> {
        let t = T0 + period() * self.k;
        self.k += 1;
        let start = Instant::now();
        let mut s = Sample {
            cycle,
            ..Sample::default()
        };
        let drew = match self.leg {
            Leg::Cpu => {
                let i = self.turn;
                self.turn = 1 - self.turn;
                let age = if self.painted[i] { 2 } else { 0 };
                let (damage, n, bytes) = counted(|| self.bufs[i].paint_at(&mut self.r, S, age, t));
                self.painted[i] = true;
                s.allocs = n;
                s.alloc_bytes = bytes;
                s.damage_px = damage.area();
                s.raster_px = self.r.last_raster_pixels();
                !damage.is_empty()
            }
            Leg::Gpu => self.gpu_frame(t, &mut s),
        };
        let work = start.elapsed() + self.pending;
        s.allocs += self.pending_allocs.0;
        s.alloc_bytes += self.pending_allocs.1;
        let pt = self.r.last_paint_times();
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        s.work = us(work) - s.gpu_total;
        s.apply = us(self.pending);
        s.flatten = us(pt.flatten);
        s.layout = us(pt.layout);
        s.text_wait = us(pt.text_wait);
        s.diff = us(pt.diff);
        s.lower = us(pt.lower);
        s.raster = us(pt.raster);
        s.prepare = us(pt.raster_parts.prepare);
        s.offscreen = us(pt.raster_parts.offscreen);
        s.encode = us(pt.raster_parts.encode);
        s.fine = us(pt.raster_parts.fine);
        s.copy = us(pt.raster_parts.copy);
        s.cells = pt.raster_parts.cells;
        s.blend = us(pt.blend);
        self.pending = Duration::ZERO;
        self.pending_allocs = (0, 0);
        let c = Counters::read(&self.r);
        s.cache_builds = c.cache - self.counters.cache;
        s.offscreen_builds = c.offscreen - self.counters.offscreen;
        s.fade_builds = c.fade_builds - self.counters.fade_builds;
        s.fade_draws = c.fade_draws - self.counters.fade_draws;
        s.raster_builds = c.rasters - self.counters.rasters;
        s.shadow_px = c.shadow - self.counters.shadow;
        self.counters = c;
        self.answer();
        drew.then_some(s)
    }

    #[cfg(not(feature = "gpu"))]
    fn move_to_gpu(&mut self) -> bool {
        false
    }

    #[cfg(not(feature = "gpu"))]
    fn gpu_frame(&mut self, _: Duration, _: &mut Sample) -> bool {
        false
    }

    /// The surface presented on the GPU from now on, with a device of
    /// its own; false without one.
    #[cfg(feature = "gpu")]
    fn move_to_gpu(&mut self) -> bool {
        use strand_gpu::{Gpu, GpuOptions, GpuReply, GpuRequest};
        let opts = GpuOptions::from_env();
        let (ping, pings) = std::sync::mpsc::channel();
        let mut gpu = Gpu::spawn(
            Box::new(move || {
                let _ = ping.send(());
            }),
            opts,
        );
        self.r.warm_gpu();
        let deadline = Instant::now() + gpu_host::WAIT;
        loop {
            match gpu.try_recv() {
                Some(GpuReply::Ready(info)) => {
                    self.r.deliver_gpu(GpuReply::Ready(info));
                    break;
                }
                Some(GpuReply::Unavailable(e)) => {
                    eprintln!("anim_bench: no GPU device ({e}); the gpu leg is skipped");
                    return false;
                }
                _ => {}
            }
            assert!(Instant::now() < deadline, "the GPU thread did not answer");
            let _ = pings.recv_timeout(Duration::from_millis(50));
        }
        let _ = self.r.take_backend_changes();
        let _ = self.r.take_gpu_requests();
        self.r.promote_now(S);
        let _ = self.r.take_backend_changes();
        self.r.set_promotion(false);
        self.r.set_backend(S, Backend::GpuPresent);
        gpu.send(GpuRequest::Attach {
            surface: S,
            handles: None,
            size: self.size,
            scale: self.scale,
            opaque: false,
        });
        strand_gpu::record_frame_phases(true);
        self.gpu = Some(GpuSide { gpu, pings });
        if std::env::var_os("STRAND_ANIM_BENCH_DEBUG").is_some() {
            eprintln!(
                "move_to_gpu: backend {:?}, wants {}, status {:?}",
                self.r.backend(S),
                self.r.wants_frame(S),
                self.r.gpu_status()
            );
        }
        // The switch repaints in full; then the scene settles as before.
        self.settle(400);
        let _ = strand_gpu::take_frame_phases();
        true
    }

    /// A frame of the presented surface: lowered here, drawn and read
    /// back by the GPU thread.
    #[cfg(feature = "gpu")]
    fn gpu_frame(&mut self, t: Duration, s: &mut Sample) -> bool {
        use strand_gpu::{GpuReply, GpuRequest};
        let (frame, n, bytes) = counted(|| self.r.paint_gpu(S, t));
        s.allocs = n;
        s.alloc_bytes = bytes;
        s.damage_px = self.r.last_damage(S).map_or(0, |d| d.area());
        for q in self.r.take_gpu_requests() {
            if let Some(g) = &self.gpu {
                g.gpu.send(q);
            }
        }
        let Some(frame) = frame else {
            return false;
        };
        let id = frame.id;
        s.uploads = frame.uploads.len() as u64;
        s.upload_px = frame
            .uploads
            .iter()
            .map(|u| u64::from(u.pixmap.width()) * u64::from(u.pixmap.height()))
            .sum();
        let Some(g) = &mut self.gpu else {
            return false;
        };
        let sent = Instant::now();
        g.gpu.send(GpuRequest::Frame(frame));
        let deadline = sent + gpu_host::WAIT;
        let mut other = Vec::new();
        loop {
            match g.gpu.try_recv() {
                Some(GpuReply::Pixels { frame, .. }) if frame == id => break,
                Some(GpuReply::Failed { error, .. }) => panic!("GPU frame {id} failed: {error}"),
                Some(r) => other.push(r),
                None => {
                    assert!(Instant::now() < deadline, "no pixels for GPU frame {id}");
                    let _ = g.pings.recv_timeout(Duration::from_millis(20));
                }
            }
        }
        s.gpu_total = sent.elapsed().as_secs_f64() * 1e6;
        // Pass pixels go to render, as the host hands them (not the
        // readback attach's `Attached`: the surface stays presented).
        for r in other {
            if matches!(r, GpuReply::PassPixels { .. }) {
                self.r.deliver_gpu(r);
            }
        }
        // The thread keeps a frame's phases just after its reply.
        let mut phases = None;
        let until = Instant::now() + Duration::from_millis(200);
        while phases.is_none() && Instant::now() < until {
            phases = strand_gpu::take_frame_phases()
                .into_iter()
                .rev()
                .find(|p| p.frame == id);
            if phases.is_none() {
                std::thread::yield_now();
            }
        }
        if let Some(p) = phases {
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            let mut cpu = 0.0;
            for (what, d) in &p.phases {
                match *what {
                    "read" => s.gpu_read += us(*d),
                    other => {
                        cpu += us(*d);
                        match other {
                            "build" => s.gpu_build += us(*d),
                            "render" => s.gpu_render += us(*d),
                            "upload" => s.gpu_upload += us(*d),
                            _ => {}
                        }
                    }
                }
            }
            s.gpu_cpu = cpu;
        }
        true
    }

    /// The caches' bytes and the process's PSS, in kB.
    fn memory(&self) -> Vec<(&'static str, u64)> {
        let kb = |b: usize| b as u64 / 1024;
        vec![
            ("paint_cache_kb", kb(self.r.paint_cache().0)),
            ("offscreen_kb", kb(self.r.offscreen_cache().0)),
            ("raster_nodes_kb", kb(self.r.raster_nodes().1)),
            ("atlas_kb", kb(self.r.atlas_mirror_bytes(self.scale))),
            ("images_kb", kb(self.r.image_bytes())),
            ("pss_kb", pss_kb()),
        ]
    }
}

#[cfg(feature = "gpu")]
impl Drop for Bench {
    fn drop(&mut self) {
        if self.gpu.is_some() {
            strand_gpu::record_frame_phases(false);
        }
    }
}

fn pss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Pss:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

/// One run: its frames (warm-up excluded) and memory.
struct Run {
    samples: Vec<Sample>,
    memory: Vec<(&'static str, u64)>,
    /// The warm-up cycle's worst frame (µs): a first showing.
    cold_max: f64,
}

fn run(scene: &Scene, cfg: Config, leg: Leg, cycles: u32) -> Option<Run> {
    let mut b = Bench::boot(scene, cfg, leg)?;
    let mut samples = Vec::new();
    let mut cold_max: f64 = 0.0;
    // Cycle 0 warms up (not recorded).
    for cycle in 0..=cycles {
        let (cap, fixed) = match scene.frames {
            Frames::Settle(n) => (n, false),
            Frames::Fixed(n) => (n, true),
        };
        for f in 0..cap {
            (scene.drive)(&mut b, cycle, f);
            if !fixed && f > 0 && !b.r.wants_frame(S) {
                break;
            }
            if let Some(s) = b.frame(cycle) {
                let total = s.work + s.gpu_total;
                if cycle == 0 {
                    cold_max = cold_max.max(total);
                } else {
                    samples.push(s);
                }
            }
        }
        // Whatever was left (text arriving, an exit's end) settles
        // unrecorded.
        b.settle(60);
    }
    let memory = b.memory();
    Some(Run {
        samples,
        memory,
        cold_max,
    })
}

// ---------------------------------------------------------------------
// Statistics and output.

fn quantile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn mean(v: impl Iterator<Item = f64>) -> f64 {
    let (mut s, mut n) = (0.0, 0usize);
    for x in v {
        s += x;
        n += 1;
    }
    if n == 0 { 0.0 } else { s / n as f64 }
}

/// The figure a run is judged by: the render thread's work on the CPU
/// leg; on the GPU leg the longer of the two threads' (they overlap,
/// one frame apart), as `max`.
fn frame_cost(leg: Leg, s: &Sample) -> f64 {
    match leg {
        Leg::Cpu => s.work,
        Leg::Gpu => s.work.max(s.gpu_cpu + s.gpu_read),
    }
}

fn env_list(name: &str) -> Option<Vec<String>> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
}

fn json_sample(out: &mut String, s: &Sample) {
    let _ = write!(
        out,
        "{{\"cycle\":{},\"work\":{:.1},\"apply\":{:.1},\"flatten\":{:.1},\"layout\":{:.1},\"text_wait\":{:.1},\"diff\":{:.1},\"lower\":{:.1},\"raster\":{:.1},\"prepare\":{:.1},\"offscreen\":{:.1},\"encode\":{:.1},\"fine\":{:.1},\"copy\":{:.1},\"blend\":{:.1},\"cells\":{},\"gpu_total\":{:.1},\"gpu_cpu\":{:.1},\"gpu_read\":{:.1},\"gpu_build\":{:.1},\"gpu_render\":{:.1},\"gpu_upload\":{:.1},\"uploads\":{},\"upload_px\":{},\"damage_px\":{},\"raster_px\":{},\"allocs\":{},\"alloc_bytes\":{},\"cache_builds\":{},\"offscreen_builds\":{},\"fade_builds\":{},\"fade_draws\":{},\"raster_builds\":{},\"shadow_px\":{}}}",
        s.cycle,
        s.work,
        s.apply,
        s.flatten,
        s.layout,
        s.text_wait,
        s.diff,
        s.lower,
        s.raster,
        s.prepare,
        s.offscreen,
        s.encode,
        s.fine,
        s.copy,
        s.blend,
        s.cells,
        s.gpu_total,
        s.gpu_cpu,
        s.gpu_read,
        s.gpu_build,
        s.gpu_render,
        s.gpu_upload,
        s.uploads,
        s.upload_px,
        s.damage_px,
        s.raster_px,
        s.allocs,
        s.alloc_bytes,
        s.cache_builds,
        s.offscreen_builds,
        s.fade_builds,
        s.fade_draws,
        s.raster_builds,
        s.shadow_px,
    );
}

#[test]
#[ignore = "measurement, not a gate"]
fn animation_frame_cost() {
    let legs: Vec<Leg> = env_list("STRAND_ANIM_BENCH_LEGS")
        .unwrap_or_else(|| vec!["cpu".into(), "gpu".into()])
        .iter()
        .map(|l| match l.as_str() {
            "cpu" => Leg::Cpu,
            "gpu" => Leg::Gpu,
            other => panic!("unknown leg {other}"),
        })
        .collect();
    let picked = env_list("STRAND_ANIM_BENCH_SCENES");
    let fits = env_list("STRAND_ANIM_BENCH_FITS");
    let repeats: u32 = std::env::var("STRAND_ANIM_BENCH_REPEATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let cycles: Option<u32> = std::env::var("STRAND_ANIM_BENCH_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok());
    let json_path = std::env::var("STRAND_ANIM_BENCH_JSON")
        .unwrap_or_else(|_| format!("{}/anim_bench.json", env!("CARGO_TARGET_TMPDIR")));
    let device = if legs.contains(&Leg::Gpu) {
        gpu_device()
    } else {
        None
    };
    let mut json = String::from("{\"hz\":120,\"budget_us\":8333.3,\"device\":");
    let _ = write!(json, "{:?},\"runs\":[", device.as_deref().unwrap_or("none"));
    let mut first_run = true;
    let mut table = Vec::new();
    for scene in scenes() {
        if picked
            .as_ref()
            .is_some_and(|p| !p.iter().any(|n| n == scene.name))
        {
            continue;
        }
        for cfg in Config::of(scene.fit) {
            if fits.as_ref().is_some_and(|f| {
                !f.iter()
                    .any(|x| x.trim_end_matches('x') == format!("{}", cfg.scale))
            }) {
                continue;
            }
            for &leg in &legs {
                if (leg == Leg::Gpu && device.is_none()) || (leg == Leg::Cpu && !scene.cpu) {
                    continue;
                }
                let mut runs = Vec::new();
                for rep in 0..repeats {
                    let Some(r) = run(&scene, cfg, leg, cycles.unwrap_or(scene.cycles)) else {
                        break;
                    };
                    if !first_run {
                        json.push(',');
                    }
                    first_run = false;
                    let _ = write!(
                        json,
                        "{{\"scene\":{:?},\"config\":{:?},\"leg\":{:?},\"repeat\":{rep},\"cold_max_us\":{:.1},\"memory\":{{",
                        scene.name,
                        cfg.name(),
                        leg.name(),
                        r.cold_max
                    );
                    for (i, (k, v)) in r.memory.iter().enumerate() {
                        let _ = write!(json, "{}{k:?}:{v}", if i > 0 { "," } else { "" });
                    }
                    json.push_str("},\"frames\":[");
                    for (i, s) in r.samples.iter().enumerate() {
                        if i > 0 {
                            json.push(',');
                        }
                        json_sample(&mut json, s);
                    }
                    json.push_str("]}");
                    runs.push(r);
                }
                if runs.is_empty() {
                    continue;
                }
                table.push(row(&scene, cfg, leg, &runs));
                eprintln!("{}", table.last().unwrap());
            }
        }
    }
    json.push_str("]}");
    std::fs::write(&json_path, json).unwrap();
    println!(
        "\nanim_bench at {HZ} Hz (budget {:.2} ms), device {}:",
        BUDGET.as_secs_f64() * 1e3,
        device.as_deref().unwrap_or("none")
    );
    println!("{}", header());
    for line in &table {
        println!("{line}");
    }
    println!("JSON: {json_path}");
}

/// The GPU's name, or `None` without one.
#[cfg(feature = "gpu")]
fn gpu_device() -> Option<String> {
    use strand_gpu::{Gpu, GpuOptions, GpuReply};
    let (tx, rx) = std::sync::mpsc::channel();
    let mut g = Gpu::spawn(
        Box::new(move || {
            let _ = tx.send(());
        }),
        GpuOptions::from_env(),
    );
    let deadline = Instant::now() + gpu_host::WAIT;
    loop {
        match g.try_recv() {
            Some(GpuReply::Ready(info)) => return Some(format!("{info:?}")),
            Some(GpuReply::Unavailable(e)) => {
                eprintln!("anim_bench: no GPU device ({e})");
                return None;
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return None;
        }
        let _ = rx.recv_timeout(Duration::from_millis(50));
    }
}

#[cfg(not(feature = "gpu"))]
fn gpu_device() -> Option<String> {
    None
}

fn header() -> String {
    format!(
        "{:<14} {:<17} {:<4} {:>5} {:>22} {:>22} {:>9} {:>5}  {}",
        "scene",
        "surface",
        "leg",
        "frames",
        "p50 ms (each repeat)",
        "p95 ms (each repeat)",
        "max ms",
        "over",
        "means: stages ms; damage/raster Mpx; allocs; builds/frame"
    )
}

fn row(scene: &Scene, cfg: Config, leg: Leg, runs: &[Run]) -> String {
    let ms = |v: f64| v / 1e3;
    let mut p50 = Vec::new();
    let mut p95 = Vec::new();
    let mut max: f64 = 0.0;
    let mut over = 0usize;
    let mut frames = 0usize;
    for r in runs {
        let mut c: Vec<f64> = r.samples.iter().map(|s| frame_cost(leg, s)).collect();
        frames += c.len();
        over += c
            .iter()
            .filter(|v| **v > BUDGET.as_secs_f64() * 1e6)
            .count();
        p50.push(ms(quantile(&mut c, 0.5)));
        p95.push(ms(quantile(&mut c, 0.95)));
        max = max.max(ms(quantile(&mut c, 1.0)));
    }
    let all: Vec<&Sample> = runs.iter().flat_map(|r| r.samples.iter()).collect();
    let m = |f: fn(&Sample) -> f64| mean(all.iter().map(|s| f(s)));
    let stages = match leg {
        Leg::Cpu => format!(
            "apply {:.2} layout {:.2} flatten {:.2} diff {:.2} raster {:.2} [prep {:.2} offs {:.2} enc {:.2} fine {:.2} copy {:.2}] blend {:.2}",
            ms(m(|s| s.apply)),
            ms(m(|s| s.layout)),
            ms(m(|s| s.flatten - s.layout)),
            ms(m(|s| s.diff)),
            ms(m(|s| s.raster)),
            ms(m(|s| s.prepare)),
            ms(m(|s| s.offscreen)),
            ms(m(|s| s.encode)),
            ms(m(|s| s.fine)),
            ms(m(|s| s.copy)),
            ms(m(|s| s.blend)),
        ),
        Leg::Gpu => format!(
            "main {:.2} [layout {:.2} flatten {:.2} lower {:.2}] gpu-thread {:.2} [upload {:.2} build {:.2} render {:.2}] wait {:.2}; uploads {:.2} Mpx",
            ms(m(|s| s.work)),
            ms(m(|s| s.layout)),
            ms(m(|s| s.flatten - s.layout)),
            ms(m(|s| s.lower)),
            ms(m(|s| s.gpu_cpu)),
            ms(m(|s| s.gpu_upload)),
            ms(m(|s| s.gpu_build)),
            ms(m(|s| s.gpu_render)),
            ms(m(|s| s.gpu_read)),
            m(|s| s.upload_px as f64) / 1e6,
        ),
    };
    let f2 = |v: &[f64]| {
        v.iter()
            .map(|x| format!("{x:.2}"))
            .collect::<Vec<_>>()
            .join("/")
    };
    let cold = runs.iter().map(|r| r.cold_max).fold(0.0, f64::max);
    format!(
        "{:<14} {:<17} {:<4} {:>5} {:>22} {:>22} {:>9.2} {:>5}  {stages}; dmg {:.2} rast {:.2}; allocs {:.0}; builds c{:.2} o{:.2} f{:.2}; cold max {:.2} ms",
        scene.name,
        cfg.name(),
        leg.name(),
        frames,
        f2(&p50),
        f2(&p95),
        max,
        over,
        m(|s| s.damage_px as f64) / 1e6,
        m(|s| s.raster_px as f64) / 1e6,
        m(|s| s.allocs as f64),
        m(|s| s.cache_builds as f64),
        m(|s| s.offscreen_builds as f64),
        m(|s| s.fade_builds as f64),
        ms(cold),
    )
}

// ---------------------------------------------------------------------
// The wall-clock gates (perf-gates): the review's CPU wins, held in CI's
// `timing` job on GitHub's runners.

/// The start of every wall-clock gate's failure message: the laptop's
/// container suite warns, instead of failing, only on failures that all
/// carry it (`scripts/container/gate-misses.sh`). Functional assertions
/// never carry it.
const GATE_MISS: &str = "timing gate missed";

/// True when this build's frame times are gated (optimised builds).
const GATED: bool = !cfg!(debug_assertions);

/// A gated run's median frame: within one 120 Hz frame.
const GATE_MEDIAN: Duration = BUDGET;
/// A gated run's 95th percentile: within one 60 Hz frame.
const GATE_P95: Duration = Duration::from_nanos(16_666_667);
/// The encoding stage's mean where the win was the encoding (a mask
/// within its reach, a transformed gradient's parts from the cache;
/// decisions.md perf-cpu-raster).
const GATE_ENCODE: Duration = Duration::from_millis(1);

/// The CPU leg's gated runs: a scene, its scale, whether its whole
/// frames are gated (median and p95) and whether its encoding stage is.
/// Every gate is at least 3.5 times the laptop's figure (the container,
/// unpinned, as CI runs it), so GitHub's slower runners pass it; a gate
/// with less headroom is left out (shared-morph's frames at 1.5×,
/// 3.2–3.7 ms median and 9.4–10 ms p95 here). The code before the
/// review misses at least one gate of each run of cover-mask,
/// shared-morph and stagger; list-scroll and theme-spring catch a gross
/// regression only (2.4 and 4.5 ms median before). decisions.md
/// perf-gates.
const GATED_RUNS: [(&str, f32, bool, bool); 8] = [
    ("cover-mask", 1.0, true, true),
    ("cover-mask", 1.5, true, true),
    ("shared-morph", 1.0, true, true),
    ("shared-morph", 1.5, false, true),
    ("stagger", 1.0, true, true),
    ("stagger", 1.5, true, true),
    ("list-scroll", 1.0, true, false),
    ("theme-spring", 1.0, true, false),
];

/// Wall-clock gate verdicts, held once at the end (`Gates::hold`).
#[derive(Default)]
struct Gates(Vec<String>);

impl Gates {
    fn check(&mut self, met: bool, miss: impl FnOnce() -> String) {
        if !met {
            self.0.push(miss());
        }
    }

    fn hold(self) {
        if !self.0.is_empty() {
            panic!("{GATE_MISS}: {}", self.0.join("\n"));
        }
    }
}

/// The review's CPU wins as gates (CI: `cargo test --profile timing -p
/// strand-render --test anim_bench animated_cpu_frames_hold_their_gates`):
/// on the CPU leg, one run of each of [`GATED_RUNS`] at its scale, the
/// median frame within [`GATE_MEDIAN`] and the 95th percentile within
/// [`GATE_P95`], and where the win was the encoding, the encoding
/// stage's mean within [`GATE_ENCODE`]. Each run must draw its frames (functional, never a
/// gate miss). A debug build draws one cycle of each at scale 1 and
/// gates nothing.
#[test]
fn animated_cpu_frames_hold_their_gates() {
    let mut gates = Gates::default();
    let all = scenes();
    for (name, scale, frames_gated, encode_gated) in GATED_RUNS {
        if !GATED && scale != 1.0 {
            continue;
        }
        let scene = all
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no scene {name}"));
        let cfg = Config::of(scene.fit)
            .into_iter()
            .find(|c| c.scale == scale)
            .unwrap_or_else(|| panic!("{name}: no config at {scale}"));
        let cycles = if GATED { scene.cycles } else { 1 };
        let r = run(scene, cfg, Leg::Cpu, cycles).expect("the CPU leg boots");
        // Optimised, every cycle animates for many frames (cover-mask 60,
        // stagger about 50); in debug a cycle may spend its frames
        // waiting for the text worker and settle unrecorded.
        let least = if GATED { 4 * cycles as usize } else { 1 };
        assert!(
            r.samples.len() >= least,
            "{name} {}: {} frames drawn in {cycles} cycles",
            cfg.name(),
            r.samples.len()
        );
        let mut work: Vec<f64> = r.samples.iter().map(|s| s.work).collect();
        let (median, p95) = (quantile(&mut work, 0.5), quantile(&mut work, 0.95));
        let encode = mean(r.samples.iter().map(|s| s.encode));
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        let verdict = if GATED {
            let mut held = Vec::new();
            if frames_gated {
                held.push(format!("{GATE_MEDIAN:?} median, {GATE_P95:?} p95"));
            }
            if encode_gated {
                held.push(format!("{GATE_ENCODE:?} encoding"));
            }
            format!("gates {}", held.join(", "))
        } else {
            "not gated (debug build)".to_string()
        };
        eprintln!(
            "anim gate {name} {}: {} frames, median {:.2} ms, p95 {:.2} ms, encoding's mean {:.3} ms; {verdict}",
            cfg.name(),
            r.samples.len(),
            median / 1e3,
            p95 / 1e3,
            encode / 1e3,
        );
        if !GATED {
            continue;
        }
        gates.check(!frames_gated || median <= us(GATE_MEDIAN), || {
            format!(
                "{name} {}: median {:.2} ms over {GATE_MEDIAN:?}",
                cfg.name(),
                median / 1e3
            )
        });
        gates.check(!frames_gated || p95 <= us(GATE_P95), || {
            format!(
                "{name} {}: p95 {:.2} ms over {GATE_P95:?}",
                cfg.name(),
                p95 / 1e3
            )
        });
        gates.check(!encode_gated || encode <= us(GATE_ENCODE), || {
            format!(
                "{name} {}: encoding's mean {:.3} ms over {GATE_ENCODE:?}",
                cfg.name(),
                encode / 1e3
            )
        });
    }
    gates.hold();
}

/// The container suite's gate-miss check reads the marker these gates'
/// failures start with, and names this file among those that carry it.
#[test]
fn the_gate_miss_marker_is_the_one_the_container_suite_reads() {
    let check = include_str!("../../../scripts/container/gate-misses.sh");
    assert!(check.contains(&format!("index(msg, \"{GATE_MISS}\") == 1")));
    assert!(check.contains("strand-render/tests/anim_bench.rs"));
}

/// The bench's own checks, run in every test run (cheap): each scene
/// compiles, boots and animates (a cycle draws frames), on the CPU.
#[test]
fn every_scene_boots_and_animates() {
    for scene in scenes().into_iter().filter(|s| s.cpu) {
        let cfg = Config {
            logical: match scene.fit {
                Fit::Panel(w, h) => (w, h),
                Fit::Screen => (1280.0, 800.0),
            },
            scale: 1.0,
        };
        let mut b = Bench::boot(&scene, cfg, Leg::Cpu).expect("the CPU leg boots");
        let mut drawn = 0;
        let cap = match scene.frames {
            Frames::Settle(n) | Frames::Fixed(n) => n.min(6),
        };
        for f in 0..cap {
            (scene.drive)(&mut b, 0, f);
            drawn += usize::from(b.frame(0).is_some());
        }
        assert!(drawn > 1, "{}: {drawn} frames drawn", scene.name);
    }
}
