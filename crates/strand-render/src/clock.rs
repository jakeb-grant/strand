//! (M4) Per-node clocks with frame caps (design.md, "Runtime changes these
//! need", item 4): a node that reads time or draws a CPU raster source
//! has a clock, which runs at refresh or at its own capped rate
//! (`shimmer` at 30 fps). Each surface keeps the clocks its last fresh frame drew;
//! they keep its frame loop running while they are drawn, and stop it
//! when none is left.
//!
//! A capped clock's `t` steps in whole ticks of its period
//! ([`crate::time::NodeTimes::context`]), so it repaints only on a tick
//! whatever else paints the surface. Between ticks the surface asks for
//! no frame: [`Clocks::wants`] is true when the next frame is the one
//! nearest a tick, and otherwise [`Clocks::wake`] gives the instant to
//! wake the loop at (half a frame before the tick), which
//! [`crate::Renderer::next_wake`] includes. The frame period is
//! estimated per surface from the shortest gap between its frames, at
//! most 1/60 s (decisions.md, m4-runtime-w1).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use strand_scene::{NodeKind, Prop, PropValue, SurfaceId};

use crate::tree::Node;

/// `effect shimmer`'s cap: 30 fps.
pub(crate) const SHIMMER: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// The frame period assumed until a surface's frames show a shorter one.
const DEFAULT_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// Gaps shorter than this are not frame periods (two paints of one frame,
/// a resize): the estimate never goes below it (360 Hz).
const MIN_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 360);

/// How often a node's clock ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Every frame the surface draws.
    Refresh,
    /// Once per period: a frame cap.
    Every(Duration),
}

/// The clock of `node`, if it has one: a CPU raster node's at its
/// source's `raster` rate (an animated image's frames come the same way,
/// at its timeline's tick: `ImageStore::frame_rate`), a node whose props
/// read time (`timed`) at
/// refresh, either capped by its kind (`effect shimmer` at 30 fps; the
/// faster of the two for a raster node). A node's clock is one clock:
/// its time-bound props follow its cap. A built-in `effect`, `particles`
/// and `grain:` get their raster rate from their props
/// (`crate::effects::raster::rate`); a node with neither has no clock.
pub(crate) fn rate(node: &Node, timed: bool, raster: Option<Rate>) -> Option<Rate> {
    let cap = kind_cap(node);
    match (raster, cap) {
        (Some(r), Some(c)) => Some(r.faster(c)),
        (Some(r), None) => Some(r),
        (None, _) if timed => Some(cap.unwrap_or(Rate::Refresh)),
        (None, _) => None,
    }
}

impl Rate {
    /// The faster of two rates.
    fn faster(self, other: Rate) -> Rate {
        match (self, other) {
            (Rate::Every(a), Rate::Every(b)) => Rate::Every(a.min(b)),
            _ => Rate::Refresh,
        }
    }
}

/// The frame cap a node's kind puts on its clock.
fn kind_cap(node: &Node) -> Option<Rate> {
    if node.kind == NodeKind::Effect {
        let style = match node.get(Prop::Style) {
            Some(PropValue::Keyword(k) | PropValue::Text(k)) => k.as_str(),
            _ => "",
        };
        if style == "shimmer" {
            return Some(Rate::Every(SHIMMER));
        }
    }
    None
}

/// A clock a frame drew: when it next ticks on the presentation clock
/// (`None`: every frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Clock {
    pub next: Option<Duration>,
}

/// One surface's running clocks.
#[derive(Debug)]
struct Running {
    /// A clock runs at refresh.
    every_frame: bool,
    /// The earliest tick of its capped clocks.
    next: Option<Duration>,
    /// The loop was woken for that tick: the next frame draws it.
    woken: bool,
}

/// A surface's frames: its period estimate, and its last frame's time on
/// the presentation clock and on the wall clock.
#[derive(Clone, Copy, Debug)]
struct Frames {
    period: Duration,
    last: Duration,
    at: Instant,
}

/// What each surface's last fresh frame drew that runs on a clock, and
/// each surface's frames.
#[derive(Debug, Default)]
pub(crate) struct Clocks {
    surfaces: HashMap<SurfaceId, Running>,
    frames: HashMap<SurfaceId, Frames>,
}

impl Clocks {
    /// The clocks `surface`'s fresh frame drew (none: its loop may stop).
    pub(crate) fn drawn(&mut self, surface: SurfaceId, clocks: &[Clock]) {
        if clocks.is_empty() {
            self.surfaces.remove(&surface);
            return;
        }
        let every_frame = clocks.iter().any(|c| c.next.is_none());
        let next = clocks.iter().filter_map(|c| c.next).min();
        self.surfaces.insert(
            surface,
            Running {
                every_frame,
                next,
                woken: false,
            },
        );
    }

    /// `surface` is drawn at `time` (presentation) at `now` (wall clock):
    /// its frame period estimate follows the gap from its last frame.
    pub(crate) fn frame(&mut self, surface: SurfaceId, time: Duration, now: Instant) {
        if time.is_zero() {
            return;
        }
        let f = self.frames.entry(surface).or_insert(Frames {
            period: DEFAULT_PERIOD,
            last: time,
            at: now,
        });
        let gap = time.saturating_sub(f.last);
        if gap >= MIN_PERIOD && gap < f.period {
            f.period = gap;
        }
        f.last = time;
        f.at = now;
    }

    /// The frame period of `surface`: the shortest gap between its
    /// frames, at most 1/60 s.
    pub(crate) fn period(&self, surface: SurfaceId) -> Duration {
        self.frames
            .get(&surface)
            .map_or(DEFAULT_PERIOD, |f| f.period)
    }

    /// Half a frame of `surface`: a capped clock's tick is drawn by the
    /// frame nearest it.
    pub(crate) fn slack(&self, surface: SurfaceId) -> Duration {
        self.period(surface) / 2
    }

    /// True while `surface` drew a clocked node in its last fresh frame.
    pub(crate) fn running(&self, surface: SurfaceId) -> bool {
        self.surfaces.contains_key(&surface)
    }

    /// True if `surface`'s clocks need its next frame: one runs at
    /// refresh, the loop was woken for a tick, or the frame after its
    /// last one is the one nearest a tick.
    pub(crate) fn wants(&self, surface: SurfaceId) -> bool {
        let Some(r) = self.surfaces.get(&surface) else {
            return false;
        };
        if r.every_frame || r.woken {
            return true;
        }
        match (r.next, self.frames.get(&surface)) {
            (Some(next), Some(f)) => next <= f.last + f.period + f.period / 2,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// When to wake the loop for `surface`'s next capped tick, if its
    /// clocks do not want the next frame: half a frame before the tick,
    /// mapped from the presentation clock to the wall clock through its
    /// last frame.
    pub(crate) fn wake(&self, surface: SurfaceId) -> Option<Instant> {
        if self.wants(surface) {
            return None;
        }
        let next = self.surfaces.get(&surface)?.next?;
        let f = self.frames.get(&surface)?;
        let due = next.saturating_sub(f.period / 2);
        Some(f.at + due.saturating_sub(f.last))
    }

    /// The earliest [`Clocks::wake`] of any surface.
    pub(crate) fn next_wake(&self) -> Option<Instant> {
        self.surfaces.keys().filter_map(|s| self.wake(*s)).min()
    }

    /// The loop woke at `now`: a surface whose tick's wake has come
    /// wants its next frame. True if one did.
    pub(crate) fn woke(&mut self, now: Instant) -> bool {
        let due: Vec<SurfaceId> = self
            .surfaces
            .keys()
            .copied()
            .filter(|s| self.wake(*s).is_some_and(|w| w <= now))
            .collect();
        for s in &due {
            if let Some(r) = self.surfaces.get_mut(s) {
                r.woken = true;
            }
        }
        !due.is_empty()
    }

    /// Forgets surfaces `keep` rejects (detached).
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(SurfaceId) -> bool) {
        self.surfaces.retain(|s, _| keep(*s));
        self.frames.retain(|s, _| keep(*s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: SurfaceId = SurfaceId(1);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_period_is_the_shortest_gap_up_to_a_sixtieth() {
        let mut c = Clocks::default();
        let now = Instant::now();
        assert_eq!(c.period(S), DEFAULT_PERIOD);
        c.frame(S, ms(1000), now);
        c.frame(S, ms(1100), now);
        assert_eq!(
            c.period(S),
            DEFAULT_PERIOD,
            "slower frames are not a period"
        );
        c.frame(S, ms(1107), now);
        assert_eq!(c.period(S), ms(7));
        c.frame(S, ms(1107), now);
        c.frame(S, ms(1108), now);
        assert_eq!(c.period(S), ms(7), "a gap under 1/360 s is ignored");
    }

    #[test]
    fn a_capped_clock_wants_the_frame_nearest_its_tick() {
        let at = Instant::now();
        let capped = |last| {
            let mut c = Clocks::default();
            c.frame(S, ms(last), at);
            c.drawn(
                S,
                &[Clock {
                    next: Some(ms(1100)),
                }],
            );
            c
        };
        // 60 Hz: the frame after 1080 lands at ~1097, nearest 1100.
        assert!(capped(1080).wants(S));
        let mut c = capped(1070);
        assert!(!c.wants(S));
        let wake = at + ms(1100) - DEFAULT_PERIOD / 2 - ms(1070);
        assert_eq!(c.wake(S), Some(wake));
        assert_eq!(c.next_wake(), Some(wake));
        assert!(!c.woke(at) && !c.wants(S), "not yet");
        assert!(c.woke(wake) && c.wants(S), "woken for the tick");
        assert_eq!(c.wake(S), None);
        // A refresh clock never waits.
        c.drawn(S, &[Clock { next: None }]);
        assert!(c.wants(S) && c.wake(S).is_none());
        c.drawn(S, &[]);
        assert!(!c.running(S) && !c.wants(S) && c.next_wake().is_none());
    }
}
