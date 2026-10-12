//! (M5, S-tools) Frame timing for `strand report`: p50/p95/p99 over a
//! surface's last 600 frames, presented and discarded
//! (docs/architecture.md, "M5 interfaces"). A frame is one `paint`: its
//! time is the render thread's work in it, and it is presented when it
//! drew something (the caller commits it), discarded when it damaged
//! nothing (nothing is committed). The percentiles are over both kinds.

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
    /// Frames presented and discarded among those summarised.
    pub presented: u64,
    pub discarded: u64,
}

/// The last [`FRAMES_SUMMARISED`] frames of each attached surface: each
/// its time in microseconds, shifted left one bit, the low bit set when
/// it was presented.
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
    /// A frame of `surface` that took `took` (presented or not).
    pub(crate) fn record(&mut self, surface: SurfaceId, took: Duration, presented: bool) {
        let micros = took.as_micros().min(u128::from(u32::MAX >> 1)) as u32;
        let frame = micros << 1 | u32::from(presented);
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
        let presented = r.frames.iter().filter(|f| *f & 1 == 1).count() as u64;
        let mut times: Vec<u32> = r.frames.iter().map(|f| f >> 1).collect();
        times.sort_unstable();
        // Nearest rank: the smallest time at least `q` of the frames
        // take no longer than.
        let at = |q: f64| {
            let rank = (q * times.len() as f64).ceil() as usize;
            Duration::from_micros(u64::from(times[rank.clamp(1, times.len()) - 1]))
        };
        Some(FrameSummary {
            p50: at(0.50),
            p95: at(0.95),
            p99: at(0.99),
            presented,
            discarded: times.len() as u64 - presented,
        })
    }
}

impl Renderer {
    /// `surface`'s frame timing over its last 600 frames; `None` before its first frame or once it is detached.
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

    /// Nearest-rank percentiles over the last 600 frames, presented and
    /// discarded counted apart; older frames fall out, and a detached
    /// surface has none.
    #[test]
    fn percentiles_are_over_the_last_600_frames() {
        let mut s = FrameStats::default();
        let a = SurfaceId(1);
        assert_eq!(s.summary(a), None);
        // 100 frames of 1..=100 ms, every tenth discarded.
        for i in 1..=100 {
            s.record(a, ms(i), i % 10 != 0);
        }
        let f = s.summary(a).unwrap();
        assert_eq!((f.p50, f.p95, f.p99), (ms(50), ms(95), ms(99)));
        assert_eq!((f.presented, f.discarded), (90, 10));
        // 600 more of 2 ms push every one of them out.
        for _ in 0..FRAMES_SUMMARISED {
            s.record(a, ms(2), true);
        }
        let f = s.summary(a).unwrap();
        assert_eq!((f.p50, f.p99), (ms(2), ms(2)));
        assert_eq!((f.presented, f.discarded), (600, 0));
        // One slow frame among 600 is past p99 (rank 594), not at it.
        s.record(a, ms(40), true);
        let f = s.summary(a).unwrap();
        assert_eq!(f.p99, ms(2));
        assert_eq!(s.summary(SurfaceId(2)), None);
        s.forget(a);
        assert_eq!(s.summary(a), None);
    }

    /// A single frame is every percentile.
    #[test]
    fn one_frame_is_every_percentile() {
        let mut s = FrameStats::default();
        s.record(SurfaceId(1), Duration::from_micros(1_234), false);
        let f = s.summary(SurfaceId(1)).unwrap();
        assert_eq!(f.p50, Duration::from_micros(1_234));
        assert_eq!(f.p99, f.p50);
        assert_eq!((f.presented, f.discarded), (0, 1));
    }
}
