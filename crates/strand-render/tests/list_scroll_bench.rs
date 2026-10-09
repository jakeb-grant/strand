//! Smooth 2,000-row scrolling, timed (the M4 exit; docs/m4-plan.md,
//! "Lists"): a launcher-sized panel (600 × 460) holding a virtualised
//! `list` of 2,000 rows (an icon and two lines of text each, as
//! design.md's launcher draws them), scrolled at injected 60 and 144 Hz
//! presentation times by a touchpad (the offset follows the fingers, a
//! fast swipe several rows a frame), then a fling, then wheel steps
//! (springs), while a stand-in for logic answers every window the list
//! asks for (`take_list_windows`) with the diff `set_list_window` sends
//! (rows unmounted and mounted with `window: true`, `row_first`).
//!
//! A frame's work is what the render thread does for it: applying the
//! window diff that arrived before it, if any, and painting it (scroll
//! motion, the rows laid out as they come into view, flatten and
//! raster). The 95th percentile of each run must stay within
//! [`BUDGET`] (3.5 ms, this plan's number, not design.md's: decisions.md
//! m4-lists-w1) on an optimised build (CI: `cargo test --profile timing
//! -p strand-render --test list_scroll_bench`). Unoptimised, the CPU
//! raster alone is some forty times slower, so a debug build (the
//! workspace tests) scrolls a quarter as far and only reports its
//! numbers, keeping the functional checks. The gates are
//! wall-clock: a miss is reported once at the end, after the functional
//! checks (no frame shows a gap, every frame draws, the view gets where
//! it was sent), with the `GATE_MISS` marker the container suite reads
//! (`scripts/container/gate-misses.sh`).

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_render::{Renderer, ScrollInput, ScrollKind};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
/// This plan's per-frame budget, for optimised builds.
const BUDGET: Duration = Duration::from_micros(3_500);
/// Rows in the list.
const ROWS: u32 = 2000;
/// The rows logic mounts before render first asks (`DEFAULT_LIST_WINDOW`).
const FIRST_WINDOW: u32 = 32;

/// The start of every wall-clock gate's failure message: the laptop's
/// container suite warns, instead of failing, only on failures that all
/// carry it (`scripts/container/gate-misses.sh`). Functional assertions
/// never carry it.
const GATE_MISS: &str = "timing gate missed";

/// True when this build's frame times are gated (optimised builds).
const GATED: bool = !cfg!(debug_assertions);

/// Swipe frames in a run: as given optimised, a quarter in debug.
fn swipe_frames(optimised: u32) -> u32 {
    if GATED { optimised } else { optimised / 4 }
}

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

/// The scene, and logic's side of the list: which row index each
/// mounted node shows.
struct Launcher {
    r: Renderer,
    buf: Buffer,
    list: NodeId,
    /// The mounted rows, in order: (global index, node).
    mounted: Vec<(u32, NodeId)>,
    next_id: u32,
    /// Window diffs applied, and rows created by them.
    windows: usize,
    created: usize,
}

impl Launcher {
    fn new() -> Self {
        let mut b = Builder::default();
        let panel = b.node(
            NodeKind::Panel,
            None,
            vec![
                (Prop::Width, num(600.0)),
                (Prop::Height, num(460.0)),
                (Prop::Bg, color("#eff1f5")),
                (Prop::Color, color("#4c4f69")),
                (Prop::Font, PropValue::Font(font(14.0))),
            ],
        );
        let col = b.node(
            NodeKind::Col,
            Some(panel),
            vec![(Prop::Pad, num(12.0)), (Prop::Gap, num(8.0))],
        );
        let list = b.node(
            NodeKind::List,
            Some(col),
            vec![
                (Prop::MaxHeight, num(420.0)),
                (Prop::RowCount, num(ROWS as f32)),
                (Prop::RowFirst, num(0.0)),
            ],
        );
        let mut me = Launcher {
            r: renderer(),
            buf: Buffer::new(600, 460, Scale::ONE),
            list,
            mounted: Vec::new(),
            // After the panel, its column and the list (0, 1, 2).
            next_id: 2,
            windows: 0,
            created: 0,
        };
        for i in 0..FIRST_WINDOW {
            let row = me.row(&mut b.diff, i, u32::MAX, false);
            me.mounted.push((i, row));
        }
        let errors = me.r.apply(b.diff);
        assert!(errors.is_empty(), "{errors:?}");
        me.r.attach_surface(S, panel);
        me
    }

    fn id(&mut self) -> NodeId {
        self.next_id += 1;
        NodeId::new(self.next_id, 0)
    }

    /// Creates row `i` at child `index` of the list in `d`.
    fn row(&mut self, d: &mut SceneDiff, i: u32, index: u32, window: bool) -> NodeId {
        let row = self.id();
        d.push(SceneOp::Create {
            id: row,
            kind: NodeKind::Row,
            parent: Some(self.list),
            index,
            window,
        });
        d.set(row, Prop::Pad, num(8.0))
            .set(row, Prop::Gap, num(12.0))
            .set(row, Prop::Radius, num(8.0));
        let icon = self.id();
        d.create(icon, NodeKind::Box, Some(row), 0)
            .set(icon, Prop::Size, num(32.0))
            .set(icon, Prop::Radius, num(8.0))
            .set(
                icon,
                Prop::Bg,
                color(["#1e66f5", "#40a02b", "#df8e1d"][i as usize % 3]),
            );
        let col = self.id();
        d.create(col, NodeKind::Col, Some(row), 1);
        let name = self.id();
        d.create(name, NodeKind::Text, Some(col), 0).set(
            name,
            Prop::Text,
            text(&format!("App {:04}", i + 1)),
        );
        let comment = self.id();
        d.create(comment, NodeKind::Text, Some(col), 1).set(
            comment,
            Prop::Text,
            text(["Browse the web", "Terminal emulator", "Manage files"][i as usize % 3]),
        );
        row
    }

    /// Logic's answer to the windows render asked for: the diff
    /// `set_list_window` sends. Returns it, or `None` if nothing was
    /// asked.
    fn answer(&mut self) -> Option<SceneDiff> {
        let asked = self.r.take_list_windows();
        let (_, want) = asked.into_iter().find(|(l, _)| *l == self.list)?;
        let start = want.start.min(ROWS - 1);
        let end = want.end.clamp(start + 1, ROWS);
        let mut d = SceneDiff::new();
        let mut kept = Vec::new();
        for (i, node) in std::mem::take(&mut self.mounted) {
            if (start..end).contains(&i) {
                kept.push((i, node));
            } else {
                d.push(SceneOp::Remove {
                    id: node,
                    window: true,
                });
            }
        }
        // Rows before the kept block go in front of it, in order; rows
        // after it at the end.
        let (lo, hi) = match (kept.first(), kept.last()) {
            (Some(a), Some(b)) => (a.0, b.0 + 1),
            _ => (end, end),
        };
        let mut front = Vec::new();
        for i in start..lo.min(end) {
            let row = self.row(&mut d, i, i - start, true);
            front.push((i, row));
        }
        let mut back = Vec::new();
        for i in hi.max(start)..end {
            let row = self.row(&mut d, i, u32::MAX, true);
            back.push((i, row));
        }
        self.created += front.len() + back.len();
        self.mounted = front.into_iter().chain(kept).chain(back).collect();
        d.set(self.list, Prop::RowFirst, num(start as f32));
        self.windows += 1;
        Some(d)
    }
}

/// Per-frame work of one run, sorted.
struct Frames(Vec<Duration>);

impl Frames {
    fn at(&self, q: f64) -> Duration {
        let i = ((self.0.len() as f64 - 1.0) * q).round() as usize;
        self.0[i]
    }
}

/// One scrolling run at `hz`: a swipe of `swipe` px a frame for `frames`
/// frames, a lift (a fling), then wheel steps; returns each frame's
/// work and how far the view went.
fn run(hz: u32, swipe: f32, frames: u32) -> (Frames, f32, Launcher) {
    let mut l = Launcher::new();
    let period = Duration::from_secs_f64(1.0 / hz as f64);
    let t0 = Duration::from_secs(1);
    let at = LogicalPoint::new(300.0, 200.0);
    // The first frame: everything laid out and shaped once (not a
    // scrolling frame).
    assert!(!l.buf.paint_at(&mut l.r, S, 0, t0).is_empty());
    if let Some(d) = l.answer() {
        assert!(l.r.apply(d).is_empty());
        l.buf.paint_at(&mut l.r, S, 1, t0 + period);
    }
    let mut work = Vec::new();
    let mut k = 2u32;
    let mut frame = |l: &mut Launcher, input: Option<ScrollInput>, k: u32| {
        let t = t0 + period * k;
        let start = Instant::now();
        if let Some(d) = l.answer() {
            assert!(l.r.apply(d).is_empty());
        }
        if let Some(input) = input {
            l.r.scroll_input(S, at, input);
        }
        let damage = l.buf.paint_at(&mut l.r, S, 1, t);
        work.push(start.elapsed());
        damage
    };
    let ms = |k: u32| ((t0 + period * k).as_millis() & 0xffff_ffff) as u32;
    // The swipe: the offset follows the fingers, a frame's worth each.
    for _ in 0..frames {
        let input = ScrollInput {
            dy: swipe,
            kind: ScrollKind::Touch,
            time: ms(k),
        };
        let damage = frame(&mut l, Some(input), k);
        assert!(!damage.is_empty(), "frame {k} drew nothing");
        k += 1;
    }
    // The lift: a fling at the swipe's speed, until it stops.
    let lift = ScrollInput {
        dy: 0.0,
        kind: ScrollKind::Lift,
        time: ms(k),
    };
    assert_eq!(l.r.scroll_input(S, at, lift), Some(l.list), "no fling");
    let fling_start = k;
    while l.r.wants_frame(S) {
        frame(&mut l, None, k);
        k += 1;
        assert!(k - fling_start < hz * 10, "the fling never stopped");
    }
    // Wheel steps, one every other frame, then settle.
    for step in 0..40 {
        let input = (step % 2 == 0).then_some(ScrollInput {
            dy: 15.0,
            kind: ScrollKind::Wheel,
            time: ms(k),
        });
        frame(&mut l, input, k);
        k += 1;
    }
    loop {
        if l.r.wants_frame(S) {
            frame(&mut l, None, k);
            k += 1;
            assert!(k < 100_000, "never settled");
            continue;
        }
        // At rest: a window still asked for is answered and drawn.
        let Some(d) = l.answer() else {
            break;
        };
        assert!(l.r.apply(d).is_empty());
    }
    work.sort();
    let shown = l.r.scroll_offset(l.list).unwrap_or(0.0);
    (Frames(work), shown, l)
}

/// The M4 exit's smooth scrolling, at 60 and 144 Hz.
#[test]
fn scrolling_2000_rows_stays_within_the_frame_budget() {
    let mut gates = Gates::default();
    for (hz, swipe, frames) in [(60, 96.0, 240), (144, 40.0, 576)] {
        let frames = swipe_frames(frames);
        let (work, shown, l) = run(hz, swipe, frames);
        // Functional: no frame showed a gap, logic was asked for many
        // windows and answered, the view went far down the list (at
        // least the swipe's own distance: optimised 23,040 px, 480 rows)
        // and its rows are drawn where they belong.
        let stats = l.r.list_frames();
        assert_eq!(stats.gaps, 0, "{hz} Hz: frames showed a gap");
        assert!(stats.frames as usize >= work.0.len(), "{hz} Hz: {stats:?}");
        assert!(l.windows >= 5, "{hz} Hz: {} windows", l.windows);
        let swiped = swipe * frames as f32;
        assert!(
            shown > swiped,
            "{hz} Hz: the view is at {shown}, swiped {swiped}"
        );
        let first_shown = (shown / 48.0) as u32;
        assert!(
            l.mounted
                .first()
                .is_some_and(|(i, _)| *i <= first_shown && first_shown < i + 48),
            "{hz} Hz: rows {:?} mounted for the view at row {first_shown}",
            l.mounted.first().zip(l.mounted.last())
        );
        let (median, p95, worst) = (work.at(0.5), work.at(0.95), work.at(1.0));
        eprintln!(
            "list scroll {hz} Hz: {} frames, median {median:?}, p95 {p95:?}, worst {worst:?}; {} windows, {} rows mounted by them; {}",
            work.0.len(),
            l.windows,
            l.created,
            if GATED {
                format!("gate {BUDGET:?} at p95")
            } else {
                "not gated (debug build)".to_string()
            }
        );
        gates.check(!GATED || p95 <= BUDGET, || {
            format!("{hz} Hz: p95 {p95:?} over {BUDGET:?} (median {median:?})")
        });
    }
    gates.hold();
}

/// The container suite's gate-miss check reads the marker these gates'
/// failures start with.
#[test]
fn the_gate_miss_marker_is_the_one_the_container_suite_reads() {
    let check = include_str!("../../../scripts/container/gate-misses.sh");
    assert!(check.contains(&format!("index(msg, \"{GATE_MISS}\") == 1")));
}
