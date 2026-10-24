//! (M4) `graph value { history: 60s; fill: paint; smooth: 0.5 }`
//! (design.md: "graphs and sparklines with built-in history"; "trivial;
//! only the new column repaints").
//!
//! A graph is a CPU raster node. Its box is a row of slots, each holding
//! one sample, so `history` spans the box: a slot lasts `history /
//! slots`. A slot is one physical pixel wide under `style: line` and
//! `steps`, and a bar and its `gap` under `style: bars`.
//!
//! (graph-follow) Slots are whole periods of the boot clock, the clock
//! kept history is timed on: tick `j` is the period from `j × column`
//! seconds of it, the node's clock ticks at those times
//! ([`crate::clock::Rate::Aligned`]), and a frame reads which tick it is
//! in from [`RasterProps::boot`] (the frame on the boot clock; a tick
//! counts as reached [`RasterProps::slack`], half a frame, early). A
//! sample is the value at its tick's start (review): the backlog's where
//! it reaches that time, else the value the last painted frame at or
//! before it saw ([`Series::known`]), the newest taken anew at each
//! advance; eased by `smooth` (0 follows the value, towards 1 it lags:
//! each tick moves `1 - smooth` of the way). So two graphs drawn at
//! different frame times (a live shell and its replay) put a value in
//! the same column. On a tick the kept pixels shift left by the slots
//! that passed and only the new ones are drawn; between ticks a new
//! value redraws only the newest slot, which shows the value now. Every
//! style keeps this; the one exception is a `fill` whose colours change
//! across the box (a gradient at an angle, a radial or a conic one),
//! which cannot move with the columns, so such a graph is drawn whole at
//! each tick (and between ticks, only its newest slot).
//!
//! Values are read against `range` ([`crate::range`]): 0 to 1 unless
//! written, its lower end at the bottom, clamped; under `range: auto` the
//! range fits the columns shown, and a change of it draws every column
//! anew. The line is the node's `color`, `line_width` wide, under it
//! `fill` (none by default), drawn as written over the graph's box: a
//! gradient is a gradient. A list value (`graph cpu.cores`, `graph
//! [cpu.usage, memory.usage]`) is several series, each sampled and eased
//! on its own, drawn over one another in order (the first at the back),
//! each in its turn of a `color` list and a `fill` list; a list that
//! changes length keeps the items still there and draws every slot anew.
//!
//! Under `reduced_motion` (time frozen) the history stops scrolling and
//! the newest column still follows the value.
//!
//! (history) A graph whose value logic keeps history of (a service
//! field, or an expression over file-scope values) also gets that
//! history as [`Prop::Backlog`] (`strand_scene::Backlog`, a list of them
//! for a list value): points timed on the boot clock, so tick `j`'s
//! value is the backlog's at `j × column`. Columns the graph did not
//! sample itself are drawn from it: every column but the newest when it
//! first draws (it shows the last `history` as soon as it mounts), and
//! the columns of a gap in its ticks (it was hidden, or a frame came
//! late), each value eased by `smooth` in order as a tick would have. A
//! column older than the backlog's oldest point stays empty, as one
//! older than the mount is without a backlog; a column inside a gap of
//! the backlog (the machine slept) is drawn as none. Columns it samples
//! while drawn are its own, one per tick, as before.

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
use crate::range::{Auto, End, Range, Spec};

/// `history` when none is set.
pub const DEFAULT_HISTORY: Duration = strand_scene::backlog::DEFAULT_HISTORY;

/// (review) When a value seen by a frame not painted yet was seen: after
/// every tick's time.
const NOT_YET: Duration = Duration::MAX;

/// The shortest column (a narrow graph over a short history).
const MIN_COLUMN: Duration = Duration::from_millis(16);

/// The line's width when unset, logical pixels.
const LINE: f32 = 1.5;

/// (graph-follow) A bar's width when unset, logical pixels.
const BAR: f32 = 2.0;

/// (graph-follow) The space between bars when `gap` is unset, logical
/// pixels.
const BAR_GAP: f32 = 1.0;

/// (graph-follow) `style`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum Style {
    #[default]
    Line,
    Steps,
    Bars,
}

/// (graph-follow) One series' `fill`.
#[derive(Clone, Debug, PartialEq)]
enum Fill {
    /// One colour, premultiplied.
    Flat([f32; 4]),
    /// A gradient over the graph's box.
    Gradient(Paint),
}

impl Fill {
    fn of(v: &PropValue) -> Option<Fill> {
        match v {
            PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => Some(Fill::Flat(premul(*c))),
            PropValue::Paint(p) => Some(Fill::Gradient(p.clone())),
            _ => None,
        }
    }

    /// Every column is painted the same: one colour, or a linear
    /// gradient straight up or down. Only then can kept columns move.
    fn same_across(&self) -> bool {
        match self {
            Fill::Flat(_) => true,
            Fill::Gradient(Paint::Linear { angle, .. }) => angle.rem_euclid(180.0) == 0.0,
            Fill::Gradient(_) => false,
        }
    }

    /// (review) Every row is painted the same: a linear gradient straight
    /// across, kept one pixel tall.
    fn same_down(&self) -> bool {
        match self {
            Fill::Flat(_) => true,
            Fill::Gradient(Paint::Linear { angle, .. }) => angle.rem_euclid(180.0) == 90.0,
            Fill::Gradient(_) => false,
        }
    }
}

/// (graph-follow) What a graph's pixels are drawn with, besides its
/// values.
#[derive(Clone, Debug, Default, PartialEq)]
struct Look {
    style: Style,
    /// `line_width` and `gap`, logical pixels, as written.
    line_width: Option<f32>,
    gap: Option<f32>,
    /// The series' line colours (premultiplied), taken in turn.
    colors: Vec<[f32; 4]>,
    /// The series' fills, taken in turn (none: no fill).
    fills: Vec<Fill>,
}

impl Look {
    fn color(&self, i: usize) -> [f32; 4] {
        match self.colors.len() {
            0 => [0.0; 4],
            n => self.colors[i % n],
        }
    }

    /// The index of series `i`'s fill in `fills`.
    fn fill(&self, i: usize) -> Option<usize> {
        match self.fills.len() {
            0 => None,
            n => Some(i % n),
        }
    }

    fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.style.hash(&mut h);
        self.line_width.map(f32::to_bits).hash(&mut h);
        self.gap.map(f32::to_bits).hash(&mut h);
        for c in &self.colors {
            c.map(f32::to_bits).hash(&mut h);
        }
        for f in &self.fills {
            match f {
                Fill::Flat(c) => c.map(f32::to_bits).hash(&mut h),
                Fill::Gradient(p) => crate::flatten::hash::hash_paint(&mut h, p),
            }
        }
        h.finish()
    }

    /// The slots of a box `w` physical pixels wide at `scale`.
    fn slots(&self, w: usize, scale: f32) -> Slots {
        match self.style {
            Style::Bars => {
                let bar = (self.line_width.unwrap_or(BAR) * scale).round().max(1.0) as usize;
                let gap = (self.gap.unwrap_or(BAR_GAP) * scale).round().max(0.0) as usize;
                let pitch = bar + gap;
                let count = (w / pitch).max(1);
                Slots {
                    gap,
                    pitch,
                    count,
                    margin: w.saturating_sub(count * pitch),
                }
            }
            Style::Line | Style::Steps => Slots {
                gap: 0,
                pitch: 1,
                count: w.max(1),
                margin: 0,
            },
        }
    }
}

/// (graph-follow) Where a graph's slots are, physical pixels: the newest
/// flush right, each `pitch` wide (a bar after its `gap`), `margin` left
/// empty on the left.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Slots {
    gap: usize,
    pitch: usize,
    count: usize,
    margin: usize,
}

/// One series' value and samples.
#[derive(Debug, Default)]
struct Series {
    /// The value now, as written (not finite: none).
    value: f32,
    /// One eased sample per tick, newest last (at most the slots; not
    /// finite: a gap).
    samples: VecDeque<f32>,
    /// The eased value as of the newest sample.
    eased: Option<f32>,
    /// (review) The eased value before the newest sample: what it is
    /// eased from again when it is taken anew ([`Series::resample`]).
    before: Option<f32>,
    /// (history) The backlog as last read from the props.
    backlog: Option<Arc<Backlog>>,
    /// (review) The values frames saw, with when on the boot clock (each
    /// change once, oldest first, from the one in force at the newest
    /// sample's tick): what the value was at a tick's time when no
    /// backlog reaches it ([`Series::known`]).
    seen: VecDeque<(Duration, f32)>,
    /// (review) Added to a list that grew: its samples start at the next
    /// draw, from its backlog where it has one.
    fresh: bool,
}

impl Series {
    /// (review) A frame at `boot` on the boot clock sees `v`. A frame
    /// that is not painted (a preview, timed as the last painted one)
    /// sees it at no time yet, and the next painted frame stamps it with
    /// its own: a value is seen when it is first painted. A frame with
    /// no clock (zero) stands nowhere and logs nothing.
    fn see(&mut self, boot: Duration, commit: bool, v: f32) {
        self.value = v;
        if boot.is_zero() {
            return;
        }
        if commit {
            for (t, _) in self.seen.iter_mut().rev() {
                if *t != NOT_YET {
                    break;
                }
                *t = boot;
            }
        }
        if self
            .seen
            .back()
            .is_none_or(|(_, last)| last.to_bits() != v.to_bits())
        {
            self.seen
                .push_back((if commit { boot } else { NOT_YET }, v));
        }
    }

    /// (review) Drops what the samples taken from tick `j` (`col` each)
    /// on cannot need: every change but the last one at or before it.
    fn forget_seen(&mut self, j: u64, col: Duration) {
        let at = time_of(j, col);
        while self.seen.len() >= 2 && self.seen[1].0 <= at {
            self.seen.pop_front();
        }
    }

    /// (review) What is known of the value at tick `j`'s time (`col`
    /// each): the backlog's value then, from its oldest point on (logic
    /// times its points when the value changed, and sends them with the
    /// value, so it is exact whenever the frames came); without a
    /// backlog reaching that time, what the last painted frame at or
    /// before it saw; [`Past::Unknown`] when neither says.
    fn known(&self, j: u64, col: Duration) -> Past {
        match past(self.backlog.as_deref(), j, col.as_secs_f64()) {
            Past::Unknown => {}
            k => return k,
        }
        let at = time_of(j, col);
        match self.seen.iter().rev().find(|(t, _)| *t <= at) {
            Some((_, v)) if v.is_finite() => Past::Known(*v),
            Some(_) => Past::Gap,
            None => Past::Unknown,
        }
    }

    /// The sample of tick `j`: what is known at its time, else the value
    /// now (`None`: a gap).
    fn sample(&self, j: u64, col: Duration) -> Option<f32> {
        match self.known(j, col) {
            Past::Known(v) => Some(v),
            Past::Gap => None,
            Past::Unknown => Some(self.value),
        }
    }

    /// (review) Takes the newest sample (tick `j`) anew from what is
    /// known of its time now: a frame drew it before a later one (or a
    /// backlog point) said what the value was at its time. Unknown, it
    /// stays. `true` when it changed.
    fn resample(&mut self, j: u64, col: Duration, smooth: f32) -> bool {
        let v = match self.known(j, col) {
            Past::Known(v) => Some(v),
            Past::Gap => None,
            Past::Unknown => return false,
        };
        let Some(old) = self.samples.pop_back() else {
            return false;
        };
        self.eased = self.before;
        self.ease_in(v, smooth);
        self.samples
            .back()
            .is_some_and(|new| new.to_bits() != old.to_bits())
    }

    /// One more sample: `v` eased in by `smooth` (the first after a gap
    /// is `v`); `None` (or a value not finite) is a gap, drawn as no
    /// column.
    fn ease_in(&mut self, v: Option<f32>, smooth: f32) {
        self.before = self.eased;
        let Some(v) = v.filter(|v| v.is_finite()) else {
            self.eased = None;
            self.samples.push_back(f32::NAN);
            return;
        };
        let e = match self.eased {
            Some(e) => e + (v - e) * (1.0 - smooth),
            None => v,
        };
        self.eased = Some(e);
        self.samples.push_back(e);
    }

    /// The newest column's value: the newest sample eased toward the
    /// value now as its tick would.
    fn live(&self, smooth: f32) -> f32 {
        let e = self.eased.unwrap_or(self.value);
        e + (self.value - e) * (1.0 - smooth)
    }

    /// The samples afresh at `tick`: the columns before it from the
    /// backlog, from its oldest point on (none without one), each in its
    /// own column, the newest what is known at its time (the value now
    /// when nothing is).
    fn restart(&mut self, tick: u64, cols: usize, col: Duration, smooth: f32) {
        self.samples.clear();
        self.eased = None;
        self.before = None;
        self.fresh = false;
        for back in (1..cols).rev() {
            let Some(j) = tick.checked_sub(back as u64) else {
                continue;
            };
            match past(self.backlog.as_deref(), j, col.as_secs_f64()) {
                // Older than anything kept: no column yet.
                Past::Unknown if self.samples.is_empty() => {}
                Past::Unknown => self.ease_in(Some(self.value), smooth),
                Past::Known(v) => self.ease_in(Some(v), smooth),
                Past::Gap => self.ease_in(None, smooth),
            }
        }
        let newest = self.sample(tick, col);
        self.ease_in(newest.or(Some(self.value)), smooth);
    }
}

/// A graph's samples and kept pixels.
#[derive(Debug, Default)]
struct Graph {
    series: Vec<Series>,
    smooth: f32,
    history: Duration,
    /// (graph-follow) `range` as written, and an auto one's state.
    spec: Spec,
    auto: Auto,
    look: Look,
    /// (graph-follow) The frame being drawn on the boot clock, how early
    /// a tick counts as reached, and whether time stands still in it
    /// ([`RasterProps`]).
    boot: Duration,
    slack: Duration,
    still: bool,
    /// The tick of the newest sample, and the column the samples were
    /// taken at.
    tick: Option<u64>,
    col: Duration,
    /// The pixels drawn last.
    kept: Option<Kept>,
    /// Columns (slots) drawn so far (tests: a tick draws only the new
    /// ones).
    drawn_columns: u64,
    /// (graph-follow) Each gradient fill rendered over the box (a
    /// gradient the same across, one pixel wide; one the same down, one
    /// pixel tall), by index in `look.fills`.
    painted: Vec<Option<Painted>>,
    /// (review) The props `look` was made from, hashed: it is made again
    /// only when they change.
    look_key: Option<u64>,
    /// (review) Every slot is drawn at the next draw: a series started
    /// afresh (a list that grew or shrank, time that went back).
    redraw: bool,
    /// (review) The sample before the newest was taken anew at the last
    /// advance, so its slot is drawn again.
    revised: bool,
}

/// (graph-follow) A gradient fill's pixels, premultiplied RGBA (with red
/// and blue as the raster's pixmaps hold them).
#[derive(Debug)]
struct Painted {
    paint: Paint,
    w: usize,
    h: usize,
    px: Vec<PremulRgba8>,
}

impl Painted {
    fn at(&self, x: usize, y: usize) -> [f32; 4] {
        let x = x.min(self.w.saturating_sub(1));
        let y = y.min(self.h.saturating_sub(1));
        self.px.get(y * self.w + x).map_or([0.0; 4], |c| {
            [c.b, c.g, c.r, c.a].map(|v| f32::from(v) / 255.0)
        })
    }
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

/// What `backlog` holds for tick `j` (`col` seconds each): its value at
/// the tick's start on the boot clock. A tick after the newest point
/// holds that point.
fn past(backlog: Option<&Backlog>, j: u64, col: f64) -> Past {
    let Some(b) = backlog else {
        return Past::Unknown;
    };
    let at = j as f64 * col;
    match b.oldest() {
        Some(o) if at >= o => {}
        _ => return Past::Unknown,
    }
    match b.value_at(at) {
        Some(v) => Past::Known(v),
        None => Past::Gap,
    }
}

#[derive(Debug)]
struct Kept {
    w: u32,
    h: u32,
    scale: u32,
    tick: u64,
    look: u64,
    range: Range,
    pixels: Vec<PremulRgba8>,
}

/// A `graph` node's source.
#[derive(Debug, Default)]
pub struct GraphSource {
    inner: Mutex<Graph>,
    /// The slots last drawn: how many columns `history` is split into.
    columns: AtomicU32,
}

impl GraphSource {
    /// Columns drawn so far (tests: a tick draws only its new columns).
    pub fn drawn_columns(&self) -> u64 {
        self.inner.lock().map(|g| g.drawn_columns).unwrap_or(0)
    }
}

/// How long one of `slots` columns of `history` lasts.
fn column(history: Duration, slots: u32) -> Duration {
    (history / slots.max(1)).max(MIN_COLUMN)
}

/// The tick a time of the boot clock is in.
fn tick_of(boot: Duration, col: Duration) -> u64 {
    u64::try_from(boot.as_nanos() / col.as_nanos().max(1)).unwrap_or(u64::MAX)
}

/// (review) The time on the boot clock tick `j` starts at, to the
/// nanosecond (as [`tick_of`] counts).
fn time_of(j: u64, col: Duration) -> Duration {
    let ns = u128::from(j).saturating_mul(col.as_nanos());
    Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
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

/// A paint as one colour: a gradient's stops averaged (a `spectrum`'s
/// fill).
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

fn color_of(v: &PropValue) -> Option<Color> {
    match v {
        PropValue::Color(c) | PropValue::Paint(Paint::Solid(c)) => Some(*c),
        _ => None,
    }
}

pub(crate) fn number(v: Option<&PropValue>) -> Option<f32> {
    match v {
        Some(PropValue::Number(n)) if n.is_finite() => Some(*n),
        Some(PropValue::Length(strand_scene::Length::Px(n))) if n.is_finite() => Some(*n),
        _ => None,
    }
}

impl Graph {
    /// Brings every series' samples up to `tick` (`col`: a column): each
    /// tick passed eases a value in once more (at most a box's worth are
    /// kept), each what is known at its tick's time ([`Series::sample`]:
    /// the backlog where it reaches that time, else what a frame saw
    /// then). The newest sample is taken anew at each advance, so a
    /// frame that came early or a backlog point that came late leaves
    /// every tick its value at its time, whenever the frames came.
    fn advance(&mut self, tick: u64, columns: usize, col: Duration) {
        let cols = columns.max(1);
        if col != self.col {
            // Other columns: the history starts again.
            self.tick = None;
            self.col = col;
        }
        let smooth = self.smooth;
        self.revised = false;
        match self.tick {
            Some(t) if tick >= t => {
                let n = (tick - t).min(cols as u64);
                for s in &mut self.series {
                    if s.fresh {
                        s.restart(tick, cols, col, smooth);
                        self.redraw = true;
                        continue;
                    }
                    self.revised |= s.resample(t, col, smooth);
                    for back in (0..n).rev() {
                        let j = tick - back;
                        let v = s.sample(j, col);
                        s.ease_in(v, smooth);
                    }
                }
            }
            // Time went back (a remount, another clock), or no history
            // yet: it starts again, from the backlog if there is one.
            _ => self.restart(tick, cols, col),
        }
        self.tick = Some(tick);
        for s in &mut self.series {
            while s.samples.len() > cols {
                s.samples.pop_front();
            }
            s.forget_seen(tick, col);
        }
    }

    fn restart(&mut self, tick: u64, cols: usize, col: Duration) {
        let smooth = self.smooth;
        for s in &mut self.series {
            s.restart(tick, cols, col, smooth);
        }
        self.redraw = true;
    }

    /// The range the columns are drawn against: `range` as written, or
    /// an auto one fitted to the `cols` columns shown.
    fn range(&mut self, cols: usize) -> Range {
        if !self.spec.is_auto() {
            return self.spec.fixed();
        }
        let smooth = self.smooth;
        let mut shown: Option<(f32, f32)> = None;
        for s in &self.series {
            let older = s.samples.iter().rev().take(cols.saturating_sub(1));
            for v in older.copied().chain([s.live(smooth)]) {
                if v.is_finite() {
                    shown = Some(shown.map_or((v, v), |(a, b)| (a.min(v), b.max(v))));
                }
            }
        }
        self.auto.fit(self.spec, shown)
    }

    /// Renders each gradient fill for a `w × h` box (once per paint and
    /// size).
    fn paint_fills(&mut self, w: usize, h: usize) {
        self.painted.resize_with(self.look.fills.len(), || None);
        for (k, f) in self.look.fills.iter().enumerate() {
            let Fill::Gradient(p) = f else {
                self.painted[k] = None;
                continue;
            };
            let pw = if f.same_across() { 1 } else { w };
            let ph = if f.same_down() { 1 } else { h };
            if self.painted[k]
                .as_ref()
                .is_some_and(|d| d.paint == *p && d.w == pw && d.h == ph)
            {
                continue;
            }
            // A 90deg gradient rendered one pixel tall is its row of a
            // box `h` tall; one straight down, one pixel wide, its column.
            let px = crate::cache::render_gradient_part(
                p,
                w as u32,
                h as u32,
                (0, 0, pw as u32, ph as u32),
            )
            .map(|pm| pm.data().to_vec());
            self.painted[k] = px.map(|px| Painted {
                paint: p.clone(),
                w: pw,
                h: ph,
                px,
            });
        }
    }

    /// Series `i`'s fill at pixel `(x, y)`, premultiplied.
    fn fill_at(&self, i: usize, x: usize, y: usize) -> Option<[f32; 4]> {
        let k = self.look.fill(i)?;
        match self.look.fills.get(k)? {
            Fill::Flat(c) => Some(*c),
            Fill::Gradient(_) => self.painted.get(k)?.as_ref().map(|d| d.at(x, y)),
        }
    }

    /// Draws slot `s` of `slots` into the `w × h` pixels: each series'
    /// value there (`vals`: as fractions of the range, and the previous
    /// slot's), over one another in order.
    #[allow(clippy::too_many_arguments)]
    fn draw_slot(
        &self,
        px: &mut [PremulRgba8],
        w: usize,
        h: usize,
        slots: Slots,
        s: usize,
        vals: &[(Option<f32>, Option<f32>)],
        lw: f32,
    ) {
        let x0 = slots.margin + s * slots.pitch;
        let x1 = (x0 + slots.pitch).min(w);
        for row in 0..h {
            for x in x0..x1 {
                px[row * w + x] = PremulRgba8::from_u8_array([0; 4]);
            }
        }
        let hf = h as f32;
        for (i, &(v, prev)) in vals.iter().enumerate() {
            let Some(v) = v else { continue };
            let line = self.look.color(i);
            match self.look.style {
                Style::Line | Style::Steps => {
                    let steps = self.look.style == Style::Steps;
                    // Steps: whole pixel rows and no edge softened.
                    let lw = if steps { lw.round().max(1.0) } else { lw };
                    let y_of = |v: f32| {
                        let y = (1.0 - v) * (hf - lw);
                        if steps { y.round() } else { y + lw / 2.0 }
                    };
                    let y = y_of(v);
                    let yp = prev.map_or(y, y_of);
                    let (top, bot) = if steps {
                        (y.min(yp), y.max(yp) + lw)
                    } else {
                        (y.min(yp) - lw / 2.0, y.max(yp) + lw / 2.0)
                    };
                    for x in x0..x1 {
                        for row in 0..h {
                            let (r0, r1) = (row as f32, row as f32 + 1.0);
                            // How much of the pixel is under the value
                            // (fill), and how much the line covers.
                            let under = (r1 - y.max(r0)).clamp(0.0, 1.0);
                            let on = (r1.min(bot) - r0.max(top)).clamp(0.0, 1.0);
                            let fill = self.fill_at(i, x, row).unwrap_or([0.0; 4]);
                            let mut c = [0.0f32; 4];
                            for k in 0..4 {
                                c[k] = line[k] * on + fill[k] * under * (1.0 - line[3] * on);
                            }
                            over(&mut px[row * w + x], c);
                        }
                    }
                }
                Style::Bars => {
                    let top = (1.0 - v) * hf;
                    for x in (x0 + slots.gap).min(x1)..x1 {
                        for row in 0..h {
                            let (r0, r1) = (row as f32, row as f32 + 1.0);
                            let cov = (r1 - top.max(r0)).clamp(0.0, 1.0);
                            if cov <= 0.0 {
                                continue;
                            }
                            let paint = self.fill_at(i, x, row).unwrap_or(line);
                            over(&mut px[row * w + x], paint.map(|c| c * cov));
                        }
                    }
                }
            }
        }
    }
}

/// `src` (premultiplied, 0 to 1) over the pixel.
fn over(dst: &mut PremulRgba8, src: [f32; 4]) {
    let d = dst.to_u8_array().map(|v| f32::from(v) / 255.0);
    let a = src[3];
    let mut out = [0u8; 4];
    for k in 0..4 {
        out[k] = ((src[k] + d[k] * (1.0 - a)).clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    *dst = PremulRgba8::from_u8_array(out);
}

/// The value of slot `x` of `w`: the samples right-aligned, the newest
/// slot live.
fn column_value(samples: &VecDeque<f32>, live: f32, x: usize, w: usize) -> Option<f32> {
    if x + 1 == w {
        return Some(live).filter(|v| v.is_finite());
    }
    // Slots before the newest show samples, newest last.
    let back = w - 1 - x;
    let n = samples.len();
    // A gap's slots (not finite) are drawn as none.
    (back <= n)
        .then(|| samples[n - back])
        .filter(|v| v.is_finite())
}

impl RasterSource for GraphSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, _time: TimeContext) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        let (wu, hu) = (w as usize, h as usize);
        let slots = g.look.slots(wu, scale);
        let count = u32::try_from(slots.count).unwrap_or(u32::MAX);
        self.columns.store(count, Ordering::Relaxed);
        let col = column(g.history, count);
        // Frozen time keeps the tick it has.
        let tick = match g.tick {
            Some(t) if g.still && g.col == col => t,
            _ => tick_of(g.boot + g.slack, col),
        };
        g.advance(tick, slots.count, col);
        if g.boot.is_zero() {
            // A frame with no clock places no columns: it draws the value
            // now, and the first frame with a clock starts the columns
            // afresh where it stands (else the ticks from zero to it would
            // each be filled with the value now).
            g.tick = None;
        }
        let range = g.range(slots.count);
        g.paint_fills(wu, hu);
        let look = g.look.hash();
        let across = g.look.fills.iter().all(Fill::same_across);
        let lw = (g.look.line_width.unwrap_or(LINE) * scale).max(1.0);
        let scale_bits = scale.to_bits();
        let redraw = std::mem::take(&mut g.redraw);
        // Kept pixels at this size, look and range shift by the ticks
        // passed; only the slots that changed are drawn. (review) A fill
        // that changes across the box cannot shift, so a tick draws such
        // a graph whole, but between ticks it too draws only its newest
        // slot.
        let mut kept = match g.kept.take() {
            Some(k)
                if k.w == w
                    && k.h == h
                    && k.scale == scale_bits
                    && k.look == look
                    && k.range == range
                    && !redraw
                    && (across || k.tick == tick) =>
            {
                k
            }
            _ => Kept {
                w,
                h,
                scale: scale_bits,
                tick: u64::MAX,
                look,
                range,
                pixels: vec![PremulRgba8::from_u8_array([0; 4]); wu * hu],
            },
        };
        let first_new = if kept.tick == u64::MAX || tick < kept.tick {
            0
        } else {
            let shift = ((tick - kept.tick) as usize).min(slots.count);
            if shift >= slots.count {
                0
            } else {
                if shift > 0 {
                    let by = shift * slots.pitch;
                    for row in kept.pixels.chunks_exact_mut(wu) {
                        row.copy_within(by.., 0);
                        // The margin stays empty.
                        for p in &mut row[..slots.margin.min(wu)] {
                            *p = PremulRgba8::from_u8_array([0; 4]);
                        }
                    }
                }
                // The previously newest slot showed the live value; it
                // now shows its sample, so it is drawn again too, and
                // (review) the one before it when its sample was taken
                // anew.
                (slots.count - shift)
                    .saturating_sub(1)
                    .saturating_sub(usize::from(g.revised))
            }
        };
        let smooth = g.smooth;
        let lives: Vec<f32> = g.series.iter().map(|s| s.live(smooth)).collect();
        let frac = |v: Option<f32>| v.and_then(|v| range.frac(v));
        let mut vals = Vec::with_capacity(g.series.len());
        for x in first_new..slots.count {
            vals.clear();
            for (s, &live) in g.series.iter().zip(&lives) {
                let v = column_value(&s.samples, live, x, slots.count);
                let prev = if x == 0 {
                    None
                } else {
                    column_value(&s.samples, live, x - 1, slots.count)
                };
                vals.push((frac(v), frac(prev)));
            }
            g.draw_slot(&mut kept.pixels, wu, hu, slots, x, &vals, lw);
        }
        g.drawn_columns += (slots.count - first_new) as u64;
        kept.tick = tick;
        let n = pixels.len().min(kept.pixels.len());
        pixels[..n].copy_from_slice(&kept.pixels[..n]);
        g.kept = Some(kept);
    }

    fn rate(&self) -> Rate {
        // Until a frame has shown its width, the next frame comes at once
        // and learns it.
        let slots = self.columns.load(Ordering::Relaxed);
        if slots == 0 {
            return Rate::Refresh;
        }
        let history = self.inner.lock().map_or(DEFAULT_HISTORY, |g| g.history);
        let history = if history.is_zero() {
            DEFAULT_HISTORY
        } else {
            history
        };
        Rate::Aligned(column(history, slots))
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut g) = self.inner.lock() else {
            return 0;
        };
        // A value that is not a number (an item of a list, or the one
        // value: review) is a gap.
        let values: Vec<f32> = match (props.get)(Prop::Value) {
            Some(PropValue::List(items)) => items
                .iter()
                .map(|v| number(Some(v)).unwrap_or(f32::NAN))
                .collect(),
            None => vec![0.0],
            v => vec![number(v).unwrap_or(f32::NAN)],
        };
        if values.len() != g.series.len() {
            // (review) The items still there keep their samples; those
            // added start from their backlogs at the next draw, and every
            // slot is drawn again (a dropped series' line goes at once).
            let n = values.len();
            g.series.truncate(n);
            g.series.resize_with(n, || Series {
                fresh: true,
                ..Series::default()
            });
            g.redraw = true;
        }
        // Not part of the state: a backlog draws nothing by itself, only
        // the columns a later tick fills.
        let backlogs: Vec<Option<Arc<Backlog>>> = match (props.get)(Prop::Backlog) {
            Some(PropValue::Backlog(b)) => vec![Some(b.clone())],
            Some(PropValue::List(items)) => items
                .iter()
                .map(|v| match v {
                    PropValue::Backlog(b) => Some(b.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let boot = props.boot;
        for (i, (s, v)) in g.series.iter_mut().zip(values).enumerate() {
            s.see(boot, props.commit, v);
            s.backlog = backlogs.get(i).cloned().flatten();
        }
        g.smooth = number((props.get)(Prop::Smooth))
            .unwrap_or(0.0)
            .clamp(0.0, 0.99);
        g.history = match (props.get)(Prop::History) {
            Some(PropValue::Duration(d)) if !d.is_zero() => *d,
            _ => DEFAULT_HISTORY,
        };
        // (review) A range not usable now (an end not a finite number)
        // keeps the last usable one.
        if let Some(spec) = Spec::of((props.get)(Prop::Range)) {
            g.spec = spec;
        }
        // (review) The look is made again only when its props change.
        let mut lk = DefaultHasher::new();
        for p in [
            Prop::Style,
            Prop::LineWidth,
            Prop::Gap,
            Prop::Color,
            Prop::Fill,
        ] {
            hash_value(&mut lk, (props.get)(p));
        }
        crate::flatten::hash::hash_color(&mut lk, &props.color);
        let lk = lk.finish();
        if g.look_key != Some(lk) {
            g.look_key = Some(lk);
            g.look = look_of(props);
        }
        g.boot = boot;
        g.slack = props.slack;
        g.still = props.still;
        let mut h = DefaultHasher::new();
        for s in &g.series {
            s.live(g.smooth).to_bits().hash(&mut h);
        }
        g.smooth.to_bits().hash(&mut h);
        g.history.hash(&mut h);
        lk.hash(&mut h);
        for e in [g.spec.lo, g.spec.hi] {
            match e {
                End::At(v) => v.to_bits().hash(&mut h),
                End::Auto => u64::MAX.hash(&mut h),
            }
        }
        // The tick this frame is in, as the last width splits history.
        let slots = self.columns.load(Ordering::Relaxed);
        if slots > 0 && !g.still {
            tick_of(g.boot + g.slack, column(g.history, slots)).hash(&mut h);
        }
        h.finish()
    }
}

/// (graph-follow) What a graph's pixels are drawn with, from its props.
fn look_of(props: &RasterProps<'_>) -> Look {
    let colors: Vec<[f32; 4]> = match (props.get)(Prop::Color) {
        Some(PropValue::List(items)) => items.iter().filter_map(color_of).map(premul).collect(),
        _ => Vec::new(),
    };
    Look {
        style: match (props.get)(Prop::Style) {
            Some(PropValue::Keyword(k)) if k == "steps" => Style::Steps,
            Some(PropValue::Keyword(k)) if k == "bars" => Style::Bars,
            _ => Style::Line,
        },
        line_width: number((props.get)(Prop::LineWidth)),
        gap: number((props.get)(Prop::Gap)),
        colors: if colors.is_empty() {
            vec![premul(props.color)]
        } else {
            colors
        },
        fills: match (props.get)(Prop::Fill) {
            Some(PropValue::List(items)) => items.iter().filter_map(Fill::of).collect(),
            Some(v) => Fill::of(v).into_iter().collect(),
            None => Vec::new(),
        },
    }
}

/// (review) Hashes what a look prop's value draws: its colours, paints,
/// numbers and keywords.
fn hash_value(h: &mut DefaultHasher, v: Option<&PropValue>) {
    use crate::flatten::hash::{hash_color, hash_paint};
    match v {
        None => 0u8.hash(h),
        Some(PropValue::Color(c)) => {
            1u8.hash(h);
            hash_color(h, c);
        }
        Some(PropValue::Paint(p)) => {
            2u8.hash(h);
            hash_paint(h, p);
        }
        Some(PropValue::List(items)) => {
            3u8.hash(h);
            items.len().hash(h);
            for i in items {
                hash_value(h, Some(i));
            }
        }
        Some(PropValue::Keyword(k)) => {
            4u8.hash(h);
            k.hash(h);
        }
        Some(v) => {
            5u8.hash(h);
            number(Some(v)).map(f32::to_bits).hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(value: f32) -> Graph {
        Graph {
            series: vec![Series {
                value,
                ..Series::default()
            }],
            ..Graph::default()
        }
    }

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn samples_ease_and_scroll_by_ticks() {
        let mut g = one(1.0);
        g.smooth = 0.5;
        let col = secs(0.1);
        g.advance(0, 4, col);
        assert_eq!(g.series[0].samples, [1.0]);
        g.series[0].value = 0.0;
        assert_eq!(g.series[0].live(0.5), 0.5, "the newest column eases now");
        g.advance(2, 4, col);
        assert_eq!(g.series[0].samples, [1.0, 0.5, 0.25]);
        g.advance(10, 4, col);
        assert_eq!(g.series[0].samples.len(), 4, "at most a box's worth");
        g.advance(1, 4, col);
        assert_eq!(g.series[0].samples, [0.0], "time went back: start again");
    }

    fn backlog(points: &[(f64, f32, f32)]) -> Option<Arc<Backlog>> {
        Some(Arc::new(Backlog {
            points: points
                .iter()
                .map(|&(at, value, ramp)| strand_scene::BacklogPoint { at, value, ramp })
                .collect(),
        }))
    }

    fn with_backlog(value: f32, b: Option<Arc<Backlog>>) -> Graph {
        let mut g = one(value);
        g.series[0].backlog = b;
        g
    }

    #[test]
    fn a_first_draw_fills_its_columns_from_the_backlog() {
        // One-second columns, drawn in tick 1000 (1000 s on the boot
        // clock); the backlog holds 0.2 from 997.5 s, then 0.6 from
        // 998.5 s, and the value now is 0.9.
        let mut g = with_backlog(
            0.9,
            backlog(&[(997.5, 0.2, 0.0), (998.5, 0.6, 0.0), (999.7, 0.9, 0.0)]),
        );
        g.advance(1000, 5, secs(1.0));
        // Ticks 996 and 997 are older than the backlog: no sample. Tick
        // 998 is 0.2, tick 999 0.6, tick 1000 the value since 999.7 s.
        assert_eq!(g.series[0].samples, [0.2, 0.6, 0.9]);
    }

    /// (review) A backlog whose newest point is older than the draw keeps
    /// every column at its own time: the columns after the newest point
    /// hold it, so the older ones do not slide toward now.
    #[test]
    fn a_restart_keeps_older_columns_in_place() {
        let mut g = with_backlog(0.8, backlog(&[(985.0, 0.2, 0.0), (990.0, 0.8, 0.0)]));
        g.advance(1000, 21, secs(1.0));
        let s = &g.series[0].samples;
        let n = s.len();
        let back = |k: usize| s[n - 1 - k];
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
        let mut g = with_backlog(
            0.7,
            backlog(&[(990.0, 0.4, 0.0), (993.0, f32::NAN, 0.0), (997.0, 0.7, 3.0)]),
        );
        g.advance(1000, 12, secs(1.0));
        let s = &g.series[0];
        let shown: Vec<Option<f32>> = (0..12)
            .map(|x| column_value(&s.samples, s.live(0.0), x, 12))
            .collect();
        let o = Some(0.4);
        let v = Some(0.7);
        assert_eq!(
            shown,
            [o, o, o, None, None, None, None, v, v, v, v, v],
            "990-992 s 0.4, 993-996 s nothing, then 0.7 (the last column live)"
        );
    }

    /// (review) A backlog whose newest point is long past (a graph hidden
    /// by opacity while its value did not change for minutes) fills a
    /// gap in its ticks at each column's own time.
    #[test]
    fn a_long_past_backlog_fills_each_column_at_its_own_time() {
        let mut g = one(0.5);
        let col = secs(60.0);
        // Minute columns; drawn in the tick from 960 s, then hidden five
        // ticks.
        g.advance(16, 8, col);
        g.series[0].backlog = backlog(&[(960.0, 0.5, 0.0), (1100.0, 0.2, 0.0), (1250.0, 0.8, 0.0)]);
        g.series[0].value = 0.8;
        g.advance(21, 8, col);
        // Ticks from 1020, 1080, 1140, 1200 and 1260 s.
        let want = [0.5, 0.5, 0.5, 0.2, 0.2, 0.8];
        let got = &g.series[0].samples;
        assert_eq!(got.len(), want.len());
        for (got, want) in got.iter().zip(want) {
            assert!((got - want).abs() < 1e-6, "{got:?}");
        }
    }

    #[test]
    fn a_tick_gap_is_filled_from_the_backlog_and_eased() {
        let mut g = one(0.0);
        g.smooth = 0.5;
        let col = secs(1.0);
        g.advance(996, 8, col);
        assert_eq!(g.series[0].samples, [0.0]);
        // Hidden from tick 996 to 1000; the backlog ramps from 0 to 1
        // over its last 2 s.
        g.series[0].backlog = backlog(&[(996.0, 0.0, 0.0), (1000.0, 1.0, 2.0)]);
        g.series[0].value = 1.0;
        g.advance(1000, 8, col);
        // Ticks 997 and 998 hold 0, tick 999 is halfway up, tick 1000
        // the value now; each eased by half.
        assert_eq!(g.series[0].samples, [0.0, 0.0, 0.0, 0.25, 0.625]);
        // Without a backlog a gap holds the value now, as before.
        let mut h = one(0.0);
        h.advance(0, 8, col);
        h.series[0].value = 1.0;
        h.advance(3, 8, col);
        assert_eq!(h.series[0].samples, [0.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn columns_are_right_aligned_with_the_newest_live() {
        let s: VecDeque<f32> = [0.1, 0.2, 0.3].into();
        assert_eq!(column_value(&s, 0.9, 4, 5), Some(0.9));
        assert_eq!(column_value(&s, 0.9, 3, 5), Some(0.3));
        assert_eq!(column_value(&s, 0.9, 1, 5), Some(0.1));
        assert_eq!(column_value(&s, 0.9, 0, 5), None, "no history yet");
    }

    /// (graph-follow) Each series keeps its own samples and its own
    /// backlog.
    #[test]
    fn each_series_keeps_its_own_history() {
        let mut g = Graph {
            series: vec![
                Series {
                    value: 0.3,
                    backlog: backlog(&[(998.0, 0.1, 0.0), (999.5, 0.3, 0.0)]),
                    ..Series::default()
                },
                Series {
                    value: 0.6,
                    ..Series::default()
                },
            ],
            ..Graph::default()
        };
        g.advance(1000, 4, secs(1.0));
        assert_eq!(g.series[0].samples, [0.1, 0.1, 0.3]);
        assert_eq!(g.series[1].samples, [0.6], "no backlog: from the mount");
        g.series[1].value = 0.9;
        g.advance(1001, 4, secs(1.0));
        assert_eq!(g.series[0].samples, [0.1, 0.1, 0.3, 0.3]);
        assert_eq!(g.series[1].samples, [0.6, 0.9]);
    }

    /// (graph-follow) Bars are a bar and its gap each, the newest flush
    /// right; line and steps a pixel each.
    #[test]
    fn bars_take_their_width_and_gap() {
        let bars = Look {
            style: Style::Bars,
            ..Look::default()
        };
        let s = bars.slots(40, 1.0);
        assert_eq!((s.gap, s.pitch, s.count, s.margin), (1, 3, 13, 1));
        let s = bars.slots(40, 2.0);
        assert_eq!((s.pitch, s.count, s.margin), (6, 6, 4));
        let wide = Look {
            line_width: Some(4.0),
            gap: Some(0.0),
            ..bars
        };
        assert_eq!(wide.slots(40, 1.0).count, 10);
        assert_eq!(Look::default().slots(40, 1.0).count, 40);
    }

    /// (graph-follow) An auto range fits the columns shown, every series.
    #[test]
    fn an_auto_range_fits_every_series_shown() {
        let mut g = Graph {
            series: vec![
                Series {
                    value: 30.0,
                    ..Series::default()
                },
                Series {
                    value: 140.0,
                    ..Series::default()
                },
            ],
            spec: Spec::of(Some(&PropValue::Keyword("auto".into()))).unwrap(),
            ..Graph::default()
        };
        g.advance(5, 4, secs(1.0));
        assert_eq!(g.range(4), Range { lo: 0.0, hi: 200.0 });
        g.spec = Spec::default();
        assert_eq!(g.range(4), Range::UNIT, "unset: 0 to 1");
    }

    #[test]
    fn a_fill_moves_with_the_columns_only_when_the_same_across() {
        let lin = |angle| {
            Fill::Gradient(Paint::Linear {
                angle,
                stops: Vec::new(),
            })
        };
        assert!(lin(180.0).same_across());
        assert!(lin(0.0).same_across());
        assert!(!lin(90.0).same_across());
        assert!(!Fill::Gradient(Paint::Radial { stops: Vec::new() }).same_across());
        assert!(Fill::Flat([0.0; 4]).same_across());
    }
}
