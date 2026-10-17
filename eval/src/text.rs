//! Text lateness (docs/eval.md, "Perturbations"): which texts the
//! renderer's hold keeps back and when they are released. Texts no rule
//! names are shaped inline, as without a hold (the hook's pick).
//!
//! - `text hold [where "s"]` holds matching texts until a release;
//! - `text release [where "s"] [via]` releases them once and ends the
//!   holds it names;
//! - `text late N [where "s"] [via]` holds each matching text for N
//!   ticks after the first it was drawn by its estimate in.
//!
//! `via paint` releases just before the next paint; `via waker` releases
//! and runs `Renderer::update` between frames, as the runtime's
//! `text_ready` wake does.

use std::collections::HashMap;

use strand_render::probe::{HeldText, HoldPick};

use crate::steps::Via;

#[derive(Clone, Debug, PartialEq)]
struct Late {
    n: u32,
    filter: Option<String>,
    via: Via,
}

/// A one-shot release waiting for its moment.
#[derive(Clone, Debug, PartialEq)]
struct Release {
    filter: Option<String>,
    via: Via,
}

fn matches(filter: &Option<String>, text: &str) -> bool {
    filter.as_deref().is_none_or(|f| text.contains(f))
}

/// The standing rules and pending releases.
#[derive(Debug, Default)]
pub struct TextRules {
    holds: Vec<Option<String>>,
    late: Vec<Late>,
    releases: Vec<Release>,
    /// Each held layout's first tick, by request key.
    since: HashMap<u64, u32>,
}

impl TextRules {
    pub fn hold(&mut self, filter: Option<String>) {
        self.holds.push(filter);
    }

    pub fn late(&mut self, n: u32, filter: Option<String>, via: Via) {
        if n > 0 {
            self.late.push(Late { n, filter, via });
        }
    }

    /// Ends the holds `filter` names (all of them for none), and releases
    /// what matches once.
    pub fn release(&mut self, filter: Option<String>, via: Via) {
        self.holds
            .retain(|h| filter.is_some() && h.as_deref() != filter.as_deref());
        self.releases.push(Release { filter, via });
    }

    /// Any rule holds anything.
    pub fn active(&self) -> bool {
        !self.holds.is_empty() || !self.late.is_empty()
    }

    /// The hook's pick for the rules now.
    pub fn pick(&self) -> Option<HoldPick> {
        if !self.active() {
            return None;
        }
        let filters: Vec<Option<String>> = self
            .holds
            .iter()
            .cloned()
            .chain(self.late.iter().map(|l| l.filter.clone()))
            .collect();
        Some(Box::new(move |_, text: &str| {
            filters.iter().any(|f| matches(f, text))
        }))
    }

    /// Notes the tick each held layout was first seen at (after a
    /// tick's paints).
    pub fn seen(&mut self, held: &[HeldText], k: u32) {
        self.since
            .retain(|key, _| held.iter().any(|h| h.key == *key));
        for h in held {
            self.since.entry(h.key).or_insert(k);
        }
    }

    /// The held layouts to release at tick `k` by `via`: those a pending
    /// release names, and those a `late` rule has held long enough (and
    /// no `hold` still holds).
    pub fn due(&mut self, held: &[HeldText], k: u32, via: Via) -> Vec<u64> {
        let mut out = Vec::new();
        let (now, later): (Vec<Release>, Vec<Release>) =
            self.releases.drain(..).partition(|r| r.via == via);
        self.releases = later;
        for h in held {
            let released = now.iter().any(|r| matches(&r.filter, &h.text));
            let held_on = self.holds.iter().any(|f| matches(f, &h.text));
            let late_done = self.late.iter().any(|l| {
                l.via == via
                    && matches(&l.filter, &h.text)
                    && self.since.get(&h.key).is_some_and(|s| k >= s + l.n)
            });
            if released || (late_done && !held_on) {
                out.push(h.key);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::NodeId;

    fn held(key: u64, text: &str) -> HeldText {
        HeldText {
            node: NodeId::new(1, 0),
            key,
            text: text.into(),
            paints: 1,
        }
    }

    #[test]
    fn late_releases_after_its_ticks_by_its_path() {
        let mut r = TextRules::default();
        r.late(2, Some("ii".into()), Via::Waker);
        let mut pick = r.pick().unwrap();
        assert!(pick(NodeId::new(1, 0), "iiii"));
        assert!(!pick(NodeId::new(1, 0), "other"));
        let h = [held(7, "iiii")];
        r.seen(&h, 10);
        assert!(r.due(&h, 11, Via::Waker).is_empty());
        assert!(r.due(&h, 12, Via::Paint).is_empty(), "not its path");
        assert_eq!(r.due(&h, 12, Via::Waker), [7]);
    }

    #[test]
    fn a_release_ends_the_hold_it_names() {
        let mut r = TextRules::default();
        r.hold(Some("a".into()));
        r.hold(Some("b".into()));
        let h = [held(1, "a"), held(2, "b")];
        r.seen(&h, 0);
        assert!(r.due(&h, 5, Via::Paint).is_empty());
        r.release(Some("a".into()), Via::Paint);
        assert!(r.due(&h, 5, Via::Waker).is_empty(), "waits for its path");
        assert_eq!(r.due(&h, 5, Via::Paint), [1]);
        assert!(r.active(), "b is still held");
        r.release(None, Via::Paint);
        assert!(!r.active());
        assert_eq!(r.due(&h[1..], 6, Via::Paint), [2]);
        assert!(r.pick().is_none());
    }
}
