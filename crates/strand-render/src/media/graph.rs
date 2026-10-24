//! (M4) `graph value { history: 60s; fill: paint; smooth: 0.5 }`
//! (design.md: "graphs and sparklines with built-in history"; "trivial;
//! only the new column repaints").
//!
//! A graph is a CPU raster node. Each column of its box (one physical
//! pixel wide) holds one sample, so `history` spans the box: a column
//! lasts `history / columns`, and the node's clock ticks once per column
//! ([`crate::clock::Rate::Every`]). On a tick the kept pixels shift left
//! by the columns that passed and only the new ones are drawn; between
//! ticks a new value redraws only the newest column, which shows the
//! value now. A sample is the value at its tick, eased by `smooth` (0
//! follows the value, towards 1 it lags: each tick moves `1 - smooth` of
//! the way). Values are fractions, 0 at the bottom and 1 at the top,
//! clamped. The line is the node's `color`, under it `fill` (none by
//! default).
//!
//! Its time is the node's clock, so under `reduced_motion` (time frozen)
//! the history stops scrolling and the newest column still follows the
//! value.
//!
//! (history) A graph whose value logic keeps history of (a service
//! field, or an expression over file-scope values) also gets that
//! history as [`Prop::Backlog`] (`strand_scene::Backlog`): points timed
//! on the boot clock, which this module reads at each draw to work out
//! the time of each column, so a backlog made any time before lands in
//! the right columns. Columns the graph did not sample itself are drawn
//! from it: every column but the newest when it first draws (it shows
//! the last `history` as soon as it mounts), and the columns of a gap in
//! its ticks (it was hidden, or a frame came late), each value eased by
//! `smooth` in order as a tick would have. A column older than the
//! backlog's oldest point stays empty, as one older than the mount is
//! without a backlog; a column inside a gap of the backlog (the machine
//! slept) is drawn as none. Columns it samples while drawn are its own,
//! one per tick, as before.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_scene::{Backlog, Color, Paint, Prop, PropValue, TimeContext};
use vello_cpu::color::PremulRgba8;

use crate::clock::Rate;
use crate::offscreen::{RasterProps, RasterSource};

/// `history` when none is set.
pub const DEFAULT_HISTORY: Duration = strand_scene::backlog::DEFAULT_HISTORY;

/// The shortest column (a narrow graph over a short history).
const MIN_COLUMN: Duration = Duration::from_millis(16);

/// The line's width, logical pixels.
const LINE: f32 = 1.5;

/// A graph's samples and kept pixels.
#[derive(Debug, Default)]
struct Graph {
    /// The props as last read: the value now, its easing, history, and
    /// the line and fill colours (premultiplied).
    value: f32,
    smooth: f32,
    history: Duration,
    line: [f32; 4],
    fill: [f32; 4],
    /// One eased sample per tick, newest last (at most the columns).
    samples: VecDeque<f32>,
    /// The eased value as of the newest sample.
    eased: Option<f32>,
    /// The tick of the newest sample.
    tick: Option<u64>,
    /// The pixels drawn last: size, scale, the tick, the colours and the
    /// newest column's value they show.
    kept: Option<Kept>,
    /// Columns drawn so far (tests: a tick draws only the new ones).
    drawn_columns: u64,
    /// (history) The backlog as last read from the props.
    backlog: Option<Arc<Backlog>>,
    /// (history) The boot clock (`None`: the real one,
    /// [`strand_scene::backlog::boot_now`]; a test passes its own).
    clock: Option<fn() -> Duration>,
    /// (history) At this draw: the boot clock's seconds and the node's
    /// time, so tick `j` was taken `t - j * col` seconds before `boot`.
    now: (f64, f64),
}

/// (history) What the backlog says of a past tick.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Past {
    /// Its value then.
    Known(f32),
    /// Nothing is known then: the machine slept.
    Gap,
    /// Before the backlog's oldest point, or no backlog.
    Unknown,
}

#[derive(Debug)]
struct Kept {
    w: u32,
    h: u32,
    scale: u32,
    tick: u64,
    style: u64,
    pixels: Vec<PremulRgba8>,
}

/// A `graph` node's source.
#[derive(Debug, Default)]
pub struct GraphSource {
    inner: Mutex<Graph>,
    /// The width last drawn, physical pixels: the columns.
    columns: AtomicU32,
}

impl GraphSource {
    /// (history) A source reading `clock` as the boot clock (`None`: the
    /// real one).
    pub(crate) fn with_clock(clock: Option<fn() -> Duration>) -> GraphSource {
        GraphSource {
            inner: Mutex::new(Graph {
                clock,
                ..Graph::default()
            }),
            columns: AtomicU32::new(0),
        }
    }

    /// Columns drawn so far (tests: a tick draws only its new columns).
    pub fn drawn_columns(&self) -> u64 {
        self.inner.lock().map(|g| g.drawn_columns).unwrap_or(0)
    }

    /// How long one column lasts.
    fn column(&self, history: Duration) -> Duration {
        let cols = self.columns.load(Ordering::Relaxed).max(1);
        (history / cols).max(MIN_COLUMN)
    }
}

fn premul(c: Color) -> [f32; 4] {
    let a = c.a.clamp(0.0, 1.0);
    [
        c.r.clamp(0.0, 1.0) * a,
        c.g.clamp(0.0, 1.0) * a,
        c.b.clamp(0.0, 1.0) * a,
        a,
    ]
}

/// A paint as one colour: a gradient's stops averaged.
pub(crate) fn flat(p: &PropValue) -> Option<Color> {
    let stops = match p {
        PropValue::Color(c) => return Some(*c),
        PropValue::Paint(Paint::Solid(c)) => return Some(*c),
        PropValue::Paint(
            Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. },
        ) => stops,
        _ => return None,
    };
    let n = stops.len().max(1) as f32;
    let sum = stops.iter().fold([0.0f32; 4], |mut a, s| {
        a[0] += s.color.r;
        a[1] += s.color.g;
        a[2] += s.color.b;
        a[3] += s.color.a;
        a
    });
    Some(Color {
        r: sum[0] / n,
        g: sum[1] / n,
        b: sum[2] / n,
        a: sum[3] / n,
    })
}

pub(crate) fn number(v: Option<&PropValue>) -> Option<f32> {
    match v {
        Some(PropValue::Number(n)) if n.is_finite() => Some(*n),
        Some(PropValue::Length(strand_scene::Length::Px(n))) if n.is_finite() => Some(*n),
        _ => None,
    }
}

impl Graph {
    /// Brings the samples up to `tick` (`col`: a column's seconds): each
    /// tick passed eases a value in once more (at most a box's worth are
    /// kept), the newest the value now and those before it from the
    /// backlog where it has them (a gap in it stays a gap).
    fn advance(&mut self, tick: u64, columns: usize, col: f64) {
        let cols = columns.max(1);
        match self.tick {
            Some(t) if tick > t => {
                let n = ((tick - t) as usize).min(cols);
                for back in (0..n).rev() {
                    let j = tick as i64 - back as i64;
                    let v = match (back, self.past(j, col)) {
                        (0, _) | (_, Past::Unknown) => Some(self.value),
                        (_, Past::Known(v)) => Some(v),
                        (_, Past::Gap) => None,
                    };
                    self.ease_in(v);
                }
            }
            // Time went back (a resize changed the column, a remount): the
            // history starts again, from the backlog if there is one.
            Some(t) if tick < t => self.restart(tick, cols, col),
            Some(_) => {}
            None => self.restart(tick, cols, col),
        }
        self.tick = Some(tick);
        while self.samples.len() > cols {
            self.samples.pop_front();
        }
    }

    /// The samples afresh at `tick`: the columns before it from the
    /// backlog, from its oldest point on (none without one), each in its
    /// own column, the newest the value now.
    fn restart(&mut self, tick: u64, cols: usize, col: f64) {
        self.samples.clear();
        self.eased = None;
        for back in (1..cols).rev() {
            let j = tick as i64 - back as i64;
            match self.past(j, col) {
                // Older than anything kept: no column yet.
                Past::Unknown if self.samples.is_empty() => {}
                Past::Unknown => self.ease_in(Some(self.value)),
                Past::Known(v) => self.ease_in(Some(v)),
                Past::Gap => self.ease_in(None),
            }
        }
        self.ease_in(Some(self.value));
    }

    /// One more sample: `v` eased in by `smooth` (the first after a gap
    /// is `v`); `None` is a gap, drawn as no column.
    fn ease_in(&mut self, v: Option<f32>) {
        let Some(v) = v else {
            self.eased = None;
            self.samples.push_back(f32::NAN);
            return;
        };
        let e = match self.eased {
            Some(e) => e + (v - e) * (1.0 - self.smooth),
            None => v,
        };
        self.eased = Some(e);
        self.samples.push_back(e);
    }

    /// What the backlog holds for tick `j` (`col` seconds each): its time
    /// is worked out from the boot clock at this draw, so a backlog made
    /// any time before (the graph was hidden, logic was busy) lands in
    /// the right column. A tick after the newest point holds that point.
    fn past(&self, j: i64, col: f64) -> Past {
        let Some(b) = &self.backlog else {
            return Past::Unknown;
        };
        let (boot, t) = self.now;
        let age = (t - j as f64 * col).max(0.0);
        let at = boot - age;
        match b.oldest() {
            Some(o) if at >= o => {}
            _ => return Past::Unknown,
        }
        match b.value_at(at) {
            Some(v) => Past::Known(v.clamp(0.0, 1.0)),
            None => Past::Gap,
        }
    }

    /// The newest column's value: the newest sample eased toward the value
    /// now as its tick would.
    fn live(&self) -> f32 {
        let e = self.eased.unwrap_or(self.value);
        e + (self.value - e) * (1.0 - self.smooth)
    }

    fn style(&self) -> u64 {
        let mut h = DefaultHasher::new();
        for v in self.line.iter().chain(&self.fill) {
            v.to_bits().hash(&mut h);
        }
        h.finish()
    }
}

/// The value of column `x` of `w`: the samples right-aligned, the
/// newest column live.
fn column_value(samples: &VecDeque<f32>, live: f32, x: usize, w: usize) -> Option<f32> {
    if x + 1 == w {
        return Some(live);
    }
    // Columns before the newest show samples, newest last.
    let back = w - 1 - x;
    let n = samples.len();
    // A gap's columns (not finite) are drawn as none.
    (back <= n)
        .then(|| samples[n - back])
        .filter(|v| v.is_finite())
}

/// Draws column `x` of a `w × h` graph: the fill from its value down,
/// and the line joining its value to the previous column's.
#[allow(clippy::too_many_arguments)]
fn draw_column(
    px: &mut [PremulRgba8],
    w: usize,
    h: usize,
    x: usize,
    v: Option<f32>,
    prev: Option<f32>,
    line: [f32; 4],
    fill: [f32; 4],
    lw: f32,
) {
    for y in 0..h {
        px[y * w + x] = PremulRgba8::from_u8_array([0; 4]);
    }
    let Some(v) = v else { return };
    let hf = h as f32;
    let y_of = |v: f32| (1.0 - v.clamp(0.0, 1.0)) * (hf - lw) + lw / 2.0;
    let y = y_of(v);
    let yp = prev.map_or(y, y_of);
    let (top, bot) = (y.min(yp) - lw / 2.0, y.max(yp) + lw / 2.0);
    for row in 0..h {
        let (r0, r1) = (row as f32, row as f32 + 1.0);
        // How much of the pixel is under the value (fill), and how much
        // the line covers.
        let under = (r1 - y.max(r0)).clamp(0.0, 1.0);
        let on = (r1.min(bot) - r0.max(top)).clamp(0.0, 1.0);
        let mut c = [0.0f32; 4];
        for k in 0..4 {
            let f = fill[k] * under;
            c[k] = line[k] * on + f * (1.0 - line[3] * on);
        }
        px[row * w + x] = PremulRgba8::from_u8_array(c.map(|v| (v * 255.0).round() as u8));
    }
}

impl RasterSource for GraphSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, time: TimeContext) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        self.columns.store(w, Ordering::Relaxed);
        let (wu, hu) = (w as usize, h as usize);
        let col = self.column(g.history).as_secs_f64();
        let tick = (time.t as f64 / col).round().max(0.0) as u64;
        // Where this draw is on the boot clock, for the backlog's times.
        if g.backlog.is_some() {
            let boot = g.clock.unwrap_or(strand_scene::backlog::boot_now)();
            g.now = (boot.as_secs_f64(), f64::from(time.t));
        }
        g.advance(tick, wu, col);
        let live = g.live();
        let style = g.style();
        let (line, fill) = (g.line, g.fill);
        let lw = (LINE * scale).max(1.0);
        let scale_bits = scale.to_bits();
        // Kept pixels at this size and style shift by the ticks passed;
        // only the columns that changed are drawn.
        let mut kept = match g.kept.take() {
            Some(k) if k.w == w && k.h == h && k.scale == scale_bits && k.style == style => k,
            _ => Kept {
                w,
                h,
                scale: scale_bits,
                tick: u64::MAX,
                style,
                pixels: vec![PremulRgba8::from_u8_array([0; 4]); wu * hu],
            },
        };
        let first_new = if kept.tick == u64::MAX || tick < kept.tick {
            0
        } else {
            let shift = ((tick - kept.tick) as usize).min(wu);
            if shift > 0 {
                for row in kept.pixels.chunks_exact_mut(wu) {
                    row.copy_within(shift.., 0);
                }
            }
            // The previously newest column showed the live value; it now
            // shows its sample, so it is drawn again too.
            (wu - shift).saturating_sub(1)
        };
        let samples = &g.samples;
        for x in first_new..wu {
            let v = column_value(samples, live, x, wu);
            let prev = if x == 0 {
                None
            } else {
                column_value(samples, live, x - 1, wu)
            };
            draw_column(&mut kept.pixels, wu, hu, x, v, prev, line, fill, lw);
        }
        g.drawn_columns += (wu - first_new) as u64;
        kept.tick = tick;
        let n = pixels.len().min(kept.pixels.len());
        pixels[..n].copy_from_slice(&kept.pixels[..n]);
        g.kept = Some(kept);
    }

    fn rate(&self) -> Rate {
        // Until a frame has shown its width, the next frame comes at once
        // and learns it.
        if self.columns.load(Ordering::Relaxed) == 0 {
            return Rate::Refresh;
        }
        let history = self.inner.lock().map_or(DEFAULT_HISTORY, |g| {
            if g.history.is_zero() {
                DEFAULT_HISTORY
            } else {
                g.history
            }
        });
        Rate::Every(self.column(history))
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut g) = self.inner.lock() else {
            return 0;
        };
        g.value = number((props.get)(Prop::Value))
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        g.smooth = number((props.get)(Prop::Smooth))
            .unwrap_or(0.0)
            .clamp(0.0, 0.99);
        g.history = match (props.get)(Prop::History) {
            Some(PropValue::Duration(d)) if !d.is_zero() => *d,
            _ => DEFAULT_HISTORY,
        };
        g.line = premul(props.color);
        g.fill = (props.get)(Prop::Fill)
            .and_then(flat)
            .map_or([0.0; 4], premul);
        // Not part of the state: a backlog draws nothing by itself, only
        // the columns a later tick fills.
        g.backlog = match (props.get)(Prop::Backlog) {
            Some(PropValue::Backlog(b)) => Some(b.clone()),
            _ => None,
        };
        let mut h = DefaultHasher::new();
        g.live().to_bits().hash(&mut h);
        g.smooth.to_bits().hash(&mut h);
        g.history.hash(&mut h);
        g.style().hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_ease_and_scroll_by_ticks() {
        let mut g = Graph {
            value: 1.0,
            smooth: 0.5,
            ..Graph::default()
        };
        g.advance(0, 4, 0.1);
        assert_eq!(g.samples, [1.0]);
        g.value = 0.0;
        assert_eq!(g.live(), 0.5, "the newest column eases now");
        g.advance(2, 4, 0.1);
        assert_eq!(g.samples, [1.0, 0.5, 0.25]);
        g.advance(10, 4, 0.1);
        assert_eq!(g.samples.len(), 4, "at most a box's worth");
        g.advance(1, 4, 0.1);
        assert_eq!(g.samples, [0.0], "time went back: start again");
    }

    fn backlog(points: &[(f64, f32, f32)]) -> Option<Arc<Backlog>> {
        Some(Arc::new(Backlog {
            points: points
                .iter()
                .map(|&(at, value, ramp)| strand_scene::BacklogPoint { at, value, ramp })
                .collect(),
        }))
    }

    #[test]
    fn a_first_draw_fills_its_columns_from_the_backlog() {
        // One-second columns, drawn at 1000 s on the boot clock, 10 s into
        // the node; the backlog holds 0.2 from 997.5 s, then 0.6 from
        // 998.5 s, and the value now is 0.9.
        let mut g = Graph {
            value: 0.9,
            backlog: backlog(&[(997.5, 0.2, 0.0), (998.5, 0.6, 0.0)]),
            now: (1000.0, 10.0),
            ..Graph::default()
        };
        g.advance(10, 5, 1.0);
        // Ticks 6 and 7 are older than the backlog: no sample. Tick 8
        // (998 s) is 0.2, tick 9 (999 s) 0.6, tick 10 the value.
        assert_eq!(g.samples, [0.2, 0.6, 0.9]);
    }

    /// (review) A backlog whose newest point is older than the draw keeps
    /// every column at its own time: the columns after the newest point
    /// hold it, so the older ones do not slide toward now.
    #[test]
    fn a_restart_keeps_older_columns_in_place() {
        let mut g = Graph {
            value: 0.8,
            backlog: backlog(&[(985.0, 0.2, 0.0), (990.0, 0.8, 0.0)]),
            now: (1000.0, 20.0),
            ..Graph::default()
        };
        g.advance(20, 21, 1.0);
        let n = g.samples.len();
        let back = |k: usize| g.samples[n - 1 - k];
        assert_eq!(n, 16, "985 s to now");
        assert_eq!(back(15), 0.2, "985 s");
        assert_eq!(back(11), 0.2, "989 s");
        assert_eq!(back(10), 0.8, "990 s: the step where it was taken");
        assert_eq!(back(1), 0.8, "held to now");
    }

    /// A gap in the backlog (the machine slept) is drawn as no columns,
    /// and the value after it steps in without a ramp.
    #[test]
    fn a_gap_in_the_backlog_is_no_columns() {
        let mut g = Graph {
            value: 0.7,
            backlog: backlog(&[(990.0, 0.4, 0.0), (993.0, f32::NAN, 0.0), (997.0, 0.7, 3.0)]),
            now: (1000.0, 10.0),
            ..Graph::default()
        };
        g.advance(10, 12, 1.0);
        let shown: Vec<Option<f32>> = (0..12)
            .map(|x| column_value(&g.samples, g.live(), x, 12))
            .collect();
        let o = Some(0.4);
        let s = Some(0.7);
        assert_eq!(
            shown,
            [o, o, o, None, None, None, None, s, s, s, s, s],
            "990-992 s 0.4, 993-996 s nothing, then 0.7 (the last column live)"
        );
    }

    /// (review) A backlog whose newest point is long past (a graph hidden
    /// by opacity while its value did not change for minutes) fills a
    /// gap in its ticks at each column's own time, not shifted to the
    /// draw that first saw it.
    #[test]
    fn a_long_past_backlog_fills_each_column_at_its_own_time() {
        let mut g = Graph {
            value: 0.5,
            ..Graph::default()
        };
        // Minute columns; drawn at 1000 s, then hidden five ticks.
        g.now = (1000.0, 0.0);
        g.advance(0, 8, 60.0);
        g.backlog = backlog(&[(1000.0, 0.5, 0.0), (1100.0, 0.2, 0.0), (1250.0, 0.8, 0.0)]);
        g.now = (1300.0, 300.0);
        g.advance(5, 8, 60.0);
        // Ticks at 1060, 1120, 1180, 1240 s, then the value now.
        let want = [0.5, 0.5, 0.2, 0.2, 0.2, 0.5];
        assert_eq!(g.samples.len(), want.len());
        for (got, want) in g.samples.iter().zip(want) {
            assert!((got - want).abs() < 1e-6, "{:?}", g.samples);
        }
    }

    #[test]
    fn a_tick_gap_is_filled_from_the_backlog_and_eased() {
        let mut g = Graph {
            value: 0.0,
            smooth: 0.5,
            ..Graph::default()
        };
        g.advance(0, 8, 1.0);
        assert_eq!(g.samples, [0.0]);
        // Hidden from tick 0 to 4 (996 s to 1000 s on the boot clock);
        // the backlog ramps from 0 to 1 over its last 2 s.
        g.backlog = backlog(&[(996.0, 0.0, 0.0), (1000.0, 1.0, 2.0)]);
        g.now = (1000.0, 4.0);
        g.value = 1.0;
        g.advance(4, 8, 1.0);
        // Ticks 1 and 2 hold 0, tick 3 is halfway up, tick 4 the value
        // now; each eased by half.
        assert_eq!(g.samples, [0.0, 0.0, 0.0, 0.25, 0.625]);
        // Without a backlog a gap holds the value now, as before.
        let mut h = Graph {
            value: 0.0,
            ..Graph::default()
        };
        h.advance(0, 8, 1.0);
        h.value = 1.0;
        h.advance(3, 8, 1.0);
        assert_eq!(h.samples, [0.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn columns_are_right_aligned_with_the_newest_live() {
        let s: VecDeque<f32> = [0.1, 0.2, 0.3].into();
        assert_eq!(column_value(&s, 0.9, 4, 5), Some(0.9));
        assert_eq!(column_value(&s, 0.9, 3, 5), Some(0.3));
        assert_eq!(column_value(&s, 0.9, 1, 5), Some(0.1));
        assert_eq!(column_value(&s, 0.9, 0, 5), None, "no history yet");
    }
}
