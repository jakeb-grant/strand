//! (M5, S-tools) Frame timing for `strand report`: p50/p95/p99 over the
//! frames a surface drew among its last 600 paints, with how many of
//! those paints were drawn and how many changed nothing
//! (docs/architecture.md, "M5 interfaces"). A paint is one `paint`: its
//! time is the render thread's work in it; it is drawn when it damaged
//! something (the caller commits it) and unchanged when it damaged
//! nothing (nothing is committed). The percentiles are over the drawn
//! frames only: an unchanged paint costs next to nothing and would pull
//! them under what a frame on screen costs (decisions.md m5-fixups). A
//! frame sent to the GPU (a promoted surface's) is timed from its
//! `paint`'s start through the GPU's render: a `GpuPresent` frame to its
//! `Presented`'s `at` (the GPU thread's submission, before `present`),
//! plus the GPU's run of it from timestamp queries (`ran`, which comes
//! with a later `Presented`; the frame counts only then), and a readback
//! frame to its `Pixels`, which comes after the GPU ran the frame; each
//! less its `waited` (the GPU thread's queue behind other surfaces' work,
//! and the swapchain's FIFO pacing). Both measure the frame's cost, not
//! vsync; it counts as drawn then, and not at all if its reply or run
//! never comes. Whether the compositor then showed or discarded a frame
//! (`wp_presentation`'s words, the `FrameClock`'s) is not measured here.

use std::collections::HashMap;
use std::time::Duration;

use strand_scene::SurfaceId;

use super::Renderer;

/// Frames kept per surface.
pub(crate) const FRAMES_SUMMARISED: usize = 600;

/// One surface's frame timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameSummary {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    /// The paints summarised that drew a frame (the percentiles' frames)
    /// and that changed nothing.
    pub drawn: u64,
    pub unchanged: u64,
}

/// The last [`FRAMES_SUMMARISED`] paints of each attached surface: each
/// its time in microseconds, shifted left one bit, the low bit set when
/// it drew a frame.
#[derive(Debug, Default)]
pub(crate) struct FrameStats {
    rings: HashMap<SurfaceId, Ring>,
}

#[derive(Debug, Default)]
struct Ring {
    frames: Vec<u32>,
    /// Where the next frame goes once `frames` is full.
    next: usize,
}

impl FrameStats {
    /// A paint of `surface` that took `took` (drew a frame or not).
    pub(crate) fn record(&mut self, surface: SurfaceId, took: Duration, drawn: bool) {
        let micros = took.as_micros().min(u128::from(u32::MAX >> 1)) as u32;
        let frame = micros << 1 | u32::from(drawn);
        let r = self.rings.entry(surface).or_default();
        if r.frames.len() < FRAMES_SUMMARISED {
            r.frames.push(frame);
        } else {
            r.frames[r.next] = frame;
            r.next = (r.next + 1) % FRAMES_SUMMARISED;
        }
    }

    /// `surface` detached: its frames go.
    pub(crate) fn forget(&mut self, surface: SurfaceId) {
        self.rings.remove(&surface);
    }

    fn summary(&self, surface: SurfaceId) -> Option<FrameSummary> {
        let r = self.rings.get(&surface).filter(|r| !r.frames.is_empty())?;
        let mut times: Vec<u32> = r
            .frames
            .iter()
            .filter(|f| *f & 1 == 1)
            .map(|f| f >> 1)
            .collect();
        times.sort_unstable();
        // Nearest rank: the smallest time at least `q` of the drawn
        // frames take no longer than (zero before the first).
        let at = |q: f64| {
            let rank = (q * times.len() as f64).ceil() as usize;
            let i = rank.clamp(1, times.len().max(1)) - 1;
            Duration::from_micros(times.get(i).map_or(0, |t| u64::from(*t)))
        };
        let drawn = times.len() as u64;
        Some(FrameSummary {
            p50: at(0.50),
            p95: at(0.95),
            p99: at(0.99),
            drawn,
            unchanged: r.frames.len() as u64 - drawn,
        })
    }
}

impl Renderer {
    /// `surface`'s frame timing over its last 600 paints; `None` before
    /// its first paint or once it is detached.
    pub fn frame_summary(&self, surface: SurfaceId) -> Option<FrameSummary> {
        self.frame_stats.summary(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Nearest-rank percentiles over the frames drawn among the last
    /// 600 paints, the unchanged ones (no damage) counted apart and left
    /// out of them; older paints fall out, and a detached surface has
    /// none.
    #[test]
    fn percentiles_are_over_the_drawn_frames_of_the_last_600_paints() {
        let mut s = FrameStats::default();
        let a = SurfaceId(1);
        assert_eq!(s.summary(a), None);
        // 100 paints of 1..=100 ms, every tenth unchanged: the drawn
        // ones are the 90 that are not a multiple of ten.
        for i in 1..=100 {
            s.record(a, ms(i), i % 10 != 0);
        }
        let f = s.summary(a).unwrap();
        // Rank 45 of the 90 is 49 ms (every tenth gone below it), 86 is
        // 95 ms, 90 is 99 ms.
        assert_eq!((f.p50, f.p95, f.p99), (ms(49), ms(95), ms(99)));
        assert_eq!((f.drawn, f.unchanged), (90, 10));
        // Unchanged paints, however many and however cheap, move no
        // percentile.
        let mut b = FrameStats::default();
        for _ in 0..500 {
            b.record(a, Duration::from_micros(3), false);
        }
        for _ in 0..100 {
            b.record(a, ms(8), true);
        }
        let f = b.summary(a).unwrap();
        assert_eq!((f.p50, f.p99), (ms(8), ms(8)));
        assert_eq!((f.drawn, f.unchanged), (100, 500));
        // 600 more of 2 ms push every one of them out.
        for _ in 0..FRAMES_SUMMARISED {
            s.record(a, ms(2), true);
        }
        let f = s.summary(a).unwrap();
        assert_eq!((f.p50, f.p99), (ms(2), ms(2)));
        assert_eq!((f.drawn, f.unchanged), (600, 0));
        // One slow frame among 600 is past p99 (rank 594), not at it.
        s.record(a, ms(40), true);
        let f = s.summary(a).unwrap();
        assert_eq!(f.p99, ms(2));
        assert_eq!(s.summary(SurfaceId(2)), None);
        s.forget(a);
        assert_eq!(s.summary(a), None);
    }

    /// A single drawn frame is every percentile; with none drawn yet the
    /// percentiles are zero.
    #[test]
    fn one_frame_is_every_percentile() {
        let mut s = FrameStats::default();
        s.record(SurfaceId(1), Duration::from_micros(700), false);
        let f = s.summary(SurfaceId(1)).unwrap();
        assert_eq!((f.p50, f.p99), (Duration::ZERO, Duration::ZERO));
        assert_eq!((f.drawn, f.unchanged), (0, 1));
        s.record(SurfaceId(1), Duration::from_micros(1_234), true);
        let f = s.summary(SurfaceId(1)).unwrap();
        assert_eq!(f.p50, Duration::from_micros(1_234));
        assert_eq!(f.p99, f.p50);
        assert_eq!((f.drawn, f.unchanged), (1, 1));
    }
}
