//! Handing freed memory back to the system after structural bursts.

use super::*;

/// How long a thread stays quiet after a burst before [`trim`].
pub(super) const TRIM_AFTER: Duration = Duration::from_millis(500);

/// The longest a delayed trim is pushed back once armed: a surface that
/// never settles (a spinner's frames, each pushing the trim back)
/// trims anyway this long after the structural diff, at one of its
/// own wakes, so what the diff freed does not stay resident for good.
pub(super) const TRIM_HELD_AT_MOST: Duration = Duration::from_secs(5);

/// How long a thread goes without a [`trim`] before it trims inline at
/// the end of a wake it was given anyway (a tick, a poll, a service's
/// report): what an unarmed burst freed goes back within this, at no
/// wakeup of its own, and a 1 s poll pays a forced collect only every
/// fifth wake.
pub(super) const TRIM_EVERY: Duration = Duration::from_secs(5);

/// How long after an inline trim (see [`TRIM_EVERY`]) the burst it
/// began trims inline again at the end of each wake. A tick's first wake
/// on the main thread only applies the diff; the paint comes a wake or
/// three later (the frame callbacks), and what it freed stayed resident
/// until the next trim: 1-2 MB of the design bar's PSS on a loaded
/// machine (`docs/decisions.md`, wave4-exitMemory). Each wake of the
/// tail pays a collect, none pays a wakeup.
pub(super) const TRIM_TAIL: Duration = Duration::from_millis(250);

/// Returns the memory the allocator holds freed to the system. mimalloc
/// purges a freed span only on a later allocation once its delay (1 s)
/// has passed, so a shell that goes quiet after a burst (boot, a reload,
/// a surface opening) kept the burst's garbage resident for good: about
/// 2.4 MB of the design bar's PSS on the real services. A forced collect
/// purges every arena's pending spans and this thread's free pages (only
/// this thread's: the text worker runs it too, `strand_text::set_idle_hook`).
pub(crate) fn trim() {
    // SAFETY: `mi_collect` takes no pointers; it runs on a live thread
    // with mimalloc as the global allocator.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

/// Whether a diff changes the scene's structure: nodes created or
/// removed (boot, a reload, a surface, popup, toast or row appearing or
/// going) or the tokens swapped. Those are the bursts worth a [`trim`];
/// a minute tick or a poll only sets props, and never pays one.
/// (mimalloc's own commit count is no gauge: a purge of a partly
/// committed range forgets its commit without counting it down, so
/// reusing the range counts it again, and every trim made the next
/// tick look like growth.)
pub(super) fn structural(diff: &SceneDiff) -> bool {
    diff.ops.iter().any(|op| {
        matches!(
            op,
            SceneOp::Create { .. } | SceneOp::Remove { .. } | SceneOp::SetTokens { .. }
        )
    })
}

/// When a thread's loop [`trim`]s: [`TRIM_AFTER`] after the last wake of
/// a burst that [`Trimmer::arm`]ed it (a [`structural`] diff sent or
/// applied). A wake while armed pushes the trim back, to at most
/// [`TRIM_HELD_AT_MOST`] after the burst was first armed; an unarmed
/// wake (a tick, a poll) changes nothing.
#[derive(Default)]
pub(super) struct Trimmer {
    pub(super) at: Option<Instant>,
    /// When the pending delayed trim was first armed.
    pub(super) armed: Option<Instant>,
    /// When this thread last trimmed.
    pub(super) last: Option<Instant>,
    /// Until when the burst an inline trim began trims inline again.
    pub(super) tail: Option<Instant>,
}

impl Trimmer {
    /// A structural burst at `now`: trim once it has been quiet.
    pub(super) fn arm(&mut self, now: Instant) {
        let first = *self.armed.get_or_insert(now);
        self.at = Some(Self::quiet_from(first, now));
    }

    /// The trim a burst first armed at `first` owes after a wake at
    /// `now`: [`TRIM_AFTER`] on, held back no later than
    /// [`TRIM_HELD_AT_MOST`] after `first`.
    pub(super) fn quiet_from(first: Instant, now: Instant) -> Instant {
        (now + TRIM_AFTER).min(first + TRIM_HELD_AT_MOST)
    }

    /// A wake at `now`: whether the quiet ran out (trim now; the trim's
    /// own wake arms nothing). A wake while armed pushes the trim back.
    pub(super) fn wake(&mut self, now: Instant) -> bool {
        match self.at {
            Some(at) if now >= at => {
                self.at = None;
                self.armed = None;
                true
            }
            Some(_) => {
                let first = *self.armed.get_or_insert(now);
                self.at = Some(Self::quiet_from(first, now));
                false
            }
            None => false,
        }
    }

    /// How long the loop may sleep for the trim.
    pub(super) fn wait(&self, now: Instant) -> Option<Duration> {
        self.at.map(|t| t.saturating_duration_since(now))
    }

    /// A wake at `now`: trims when due.
    pub(super) fn run(&mut self, now: Instant) {
        if self.wake(now) {
            self.last = Some(now);
            trim();
        }
    }

    /// The end of a wake at `now`, before the loop sleeps: whether to
    /// trim inline, unarmed and [`TRIM_EVERY`] since the last trim, or
    /// within [`TRIM_TAIL`] of the inline trim that began this burst.
    /// Never while something on screen is `moving` (a spring, a crossfade:
    /// more frames are coming), so no forced collect lands between an
    /// animation's frames; its last frame's wake trims, and starts the
    /// tail, if one is due.
    pub(super) fn settles(&mut self, now: Instant, moving: bool) -> bool {
        if self.at.is_some() || moving {
            return false;
        }
        if self.tail.is_some_and(|t| now < t) {
            return true;
        }
        let due = self
            .last
            .is_none_or(|l| now.saturating_duration_since(l) >= TRIM_EVERY);
        if due {
            self.last = Some(now);
            self.tail = Some(now + TRIM_TAIL);
        }
        due
    }

    /// The end of a wake: trims inline when [`Trimmer::settles`].
    pub(super) fn settle(&mut self, now: Instant, moving: bool) {
        if self.settles(now, moving) {
            trim();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loop woken by a source every `period` (none: once, at the
    /// start) and by its [`Trimmer`], for `span`; the source's wakes in
    /// `armed` (by index) are structural bursts. Its wakes after the
    /// first, and trims.
    fn trims_over(period: Option<Duration>, span: Duration, armed: &[u32]) -> (u32, u32) {
        let t0 = Instant::now();
        let end = t0 + span;
        let mut trimmer = Trimmer::default();
        let mut now = t0;
        let mut source = period.map(|p| t0 + p);
        let (mut wakes, mut trims, mut fired) = (0, 0, 0u32);
        if armed.contains(&0) {
            trimmer.arm(now);
        }
        loop {
            if trimmer.wake(now) {
                trims += 1;
            }
            let trim = trimmer.wait(now).map(|w| now + w);
            let Some(next) = [source, trim].into_iter().flatten().min() else {
                break;
            };
            if next > end {
                break;
            }
            now = next;
            wakes += 1;
            if source == Some(now) {
                source = period.map(|p| now + p);
                fired += 1;
                if armed.contains(&fired) {
                    trimmer.arm(now);
                }
            }
        }
        (wakes, trims)
    }

    /// A thread woken each second (a cpu meter, a seconds clock) or each
    /// minute (the clock's tick) only sets props: it never pays a trim's
    /// wakeup. A structural burst (boot, a reload, a surface) trims once,
    /// [`TRIM_AFTER`] after it goes quiet.
    #[test]
    fn only_a_structural_burst_pays_a_trim_wakeup() {
        let span = Duration::from_secs(10);
        for period in [Duration::from_millis(700), Duration::from_secs(1)] {
            let polls = (span.as_millis() / period.as_millis()) as u32;
            assert_eq!(
                trims_over(Some(period), span, &[]),
                (polls, 0),
                "{period:?}"
            );
        }
        // Boot, then quiet: one trim.
        assert_eq!(trims_over(None, span, &[0]), (1, 1));
        // Ticks after boot: the boot's trim, none for the ticks.
        let minute = Duration::from_secs(60);
        let span = Duration::from_secs(150);
        assert_eq!(trims_over(Some(minute), span, &[0]), (3, 1));
        // A reload at the first tick: one trim more.
        assert_eq!(trims_over(Some(minute), span, &[0, 1]), (4, 2));
        // A poll while armed pushes the trim back past its own wakes.
        assert_eq!(
            trims_over(
                Some(Duration::from_millis(300)),
                Duration::from_secs(1),
                &[0]
            ),
            (3, 0)
        );
    }

    /// The inline trim at the end of a wake: on the first wake, then not
    /// again for [`TRIM_EVERY`] (a 1 s poll pays one every fifth wake),
    /// and never while a delayed trim is armed (that one comes first).
    #[test]
    fn a_wake_trims_inline_at_most_every_five_seconds() {
        let t0 = Instant::now();
        let mut t = Trimmer::default();
        let polls: Vec<bool> = (0..11)
            .map(|i| t.settles(t0 + Duration::from_secs(i), false))
            .collect();
        let at: Vec<usize> = polls
            .iter()
            .enumerate()
            .filter(|p| *p.1)
            .map(|p| p.0)
            .collect();
        assert_eq!(at, [0, 5, 10]);
        let mut t = Trimmer::default();
        t.arm(t0);
        assert!(!t.settles(t0, false), "armed: the delayed trim comes first");
        assert!(t.wake(t0 + TRIM_AFTER));
        t.last = Some(t0 + TRIM_AFTER);
        assert!(!t.settles(t0 + Duration::from_secs(1), false));
        assert!(t.settles(t0 + TRIM_AFTER + TRIM_EVERY, false));
    }

    /// A tick's burst: its first wake trims inline, and so does every
    /// wake of the burst's tail (the paint on the frame callbacks comes
    /// after the diff's wake), within [`TRIM_TAIL`]; a wake after the
    /// tail waits out [`TRIM_EVERY`] again.
    #[test]
    fn the_tail_of_an_inline_trimmed_burst_trims_too() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Trimmer::default();
        assert!(t.settles(t0, false), "the tick's first wake");
        assert!(t.settles(t0 + ms(7), false), "the first output's frame");
        assert!(t.settles(t0 + ms(7), false), "the second output's frame");
        assert!(t.settles(t0 + ms(16), false), "a buffer release");
        assert!(!t.settles(t0 + TRIM_TAIL, false), "past the tail");
        assert!(!t.settles(t0 + Duration::from_secs(1), false), "a poll");
        let next = t0 + TRIM_EVERY;
        assert!(t.settles(next, false), "the next burst after five seconds");
        assert!(t.settles(next + ms(16), false), "and its tail");
        // An armed trim's tail is the armed trim itself.
        let mut t = Trimmer::default();
        t.arm(t0);
        assert!(!t.settles(t0 + ms(7), false));
        t.run(t0 + TRIM_AFTER);
        assert!(!t.settles(t0 + TRIM_AFTER, false), "trimmed already");
        assert!(!t.settles(t0 + TRIM_AFTER + ms(16), false));
    }

    /// An animation's frames (a spring, a crossfade) never trim inline,
    /// however long since the last trim, and use up no due trim: the
    /// wake after its last frame does, and starts the tail then.
    #[test]
    fn no_inline_trim_lands_between_an_animations_frames() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Trimmer::default();
        for f in 0..30 {
            assert!(!t.settles(t0 + ms(16 * f), true), "frame {f} trimmed");
        }
        let done = t0 + ms(16 * 30);
        assert!(t.settles(done, false), "the settled frame's wake");
        assert!(t.settles(done + ms(16), false), "and its tail");
        assert!(!t.settles(done + ms(32), true), "moving again, in the tail");
        assert!(!t.settles(done + TRIM_TAIL, false), "past the tail");
    }

    /// A structural burst (a toast appearing) costs exactly one wake of
    /// its own: the trim [`TRIM_AFTER`] after its last wake, and nothing
    /// is armed after it.
    #[test]
    fn a_structural_burst_costs_one_trim_wake() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Trimmer::default();
        t.arm(t0);
        // The toast's frames.
        for f in 1..4 {
            let now = t0 + ms(16 * f);
            t.run(now);
            assert_eq!(t.wait(now), Some(TRIM_AFTER), "frame {f}");
        }
        let quiet = t0 + ms(48) + TRIM_AFTER;
        assert!(t.wake(quiet), "the one trim wake");
        assert_eq!(t.wait(quiet), None, "nothing armed after it");
    }

    /// A surface that never settles (a spinner's frames, every 16 ms)
    /// pushes an armed trim back no further than [`TRIM_HELD_AT_MOST`]
    /// after the structural diff: one of its frames trims then, and the
    /// next structural diff arms afresh.
    #[test]
    fn an_endless_animation_holds_the_trim_back_at_most_five_seconds() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Trimmer::default();
        t.arm(t0);
        let trimmed: Vec<u64> = (1..700)
            .map(|f| 16 * f)
            .filter(|&f| t.wake(t0 + ms(f)))
            .collect();
        assert_eq!(trimmed, [5008], "trims once, at the first frame past 5 s");
        let later = t0 + ms(12_000);
        t.arm(later);
        assert_eq!(t.wait(later), Some(TRIM_AFTER), "a fresh arm");
        assert!(!t.wake(later + ms(16)));
        assert!(t.wake(later + ms(16) + TRIM_AFTER));
    }

    /// Created and removed nodes and swapped tokens are structural; a
    /// prop set (the tick's text) is not.
    #[test]
    fn a_prop_set_is_not_structural() {
        use strand_scene::{Prop, PropValue};
        let mut tick = SceneDiff::new();
        tick.set(
            NodeId::new(7, 0),
            Prop::Text,
            PropValue::Text("12:35".into()),
        );
        assert!(!structural(&tick));
        assert!(!structural(&SceneDiff::new()));
        let mut closed = SceneDiff::new();
        closed.push(SceneOp::Remove {
            id: NodeId::new(7, 0),
        });
        assert!(structural(&closed));
    }
}
