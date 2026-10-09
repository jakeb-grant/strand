//! Directional `pages` transitions (design.md, "Pages"): when `current`
//! moves to another page, the new page slides in from the side its
//! source order puts it on and the old one slides out the other way
//! (forward: in from the right, out to the left; backward mirrors). A
//! page with its own `enter`/`exit` plays that instead, a `pages` with a
//! `transition:` leaves the swap to its mask, and `reduced_motion` snaps.
//!
//! Logic tells the direction through `row_first` on the `pages` node:
//! the current page's place in source order. A diff that swaps pages
//! creates the new page, removes the old one and sets `row_first`; the
//! renderer notes each part as it applies and [`PageSwaps::settle`]
//! pairs them at the end of the diff. The pairing (entering, leaving,
//! direction) stays readable while the old page plays out, for the
//! transition masks (`Renderer::page_swap`).

use std::collections::HashMap;

use strand_scene::{NodeId, PropValue};

/// One swap of a `pages`' current page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSwap {
    /// The page coming in (`None`: `current` names no page now).
    pub entering: Option<NodeId>,
    /// The page going out, playing its exit as a ghost (`None`: there
    /// was no page before, or it left at once).
    pub leaving: Option<NodeId>,
    /// The new page comes after the old one in source order.
    pub forward: bool,
}

/// What one diff did to each `pages` node, collected while it applies.
#[derive(Debug, Default)]
pub(crate) struct PageSwaps {
    /// Per `pages` node: the page created, the page removed (kept as a
    /// ghost) and `row_first` before the diff set it.
    pending: HashMap<NodeId, Pending>,
    /// The last swap of each `pages` node, while its old page plays out.
    last: HashMap<NodeId, PageSwap>,
}

#[derive(Debug, Default)]
struct Pending {
    entering: Option<NodeId>,
    leaving: Option<NodeId>,
    /// `row_first` before this diff (`Some(None)`: it was unset).
    was: Option<Option<f32>>,
}

/// What [`PageSwaps::settle`] decided for one `pages` node.
#[derive(Debug, PartialEq)]
pub(crate) struct Settled {
    pub pages: NodeId,
    pub swap: PageSwap,
    /// The page moved at all (`row_first` changed): it slides.
    pub moved: bool,
}

impl PageSwaps {
    /// A page was created under `pages`.
    pub fn created(&mut self, pages: NodeId, page: NodeId) {
        self.pending.entry(pages).or_default().entering = Some(page);
    }

    /// A page under `pages` was removed and kept as a ghost to play out.
    pub fn removed(&mut self, pages: NodeId, page: NodeId) {
        self.pending.entry(pages).or_default().leaving = Some(page);
    }

    /// `pages`' `row_first` is about to change from `old`.
    pub fn moving(&mut self, pages: NodeId, old: Option<&PropValue>) {
        let p = self.pending.entry(pages).or_default();
        if p.was.is_none() {
            p.was = Some(match old {
                Some(PropValue::Number(f)) if f.is_finite() => Some(*f),
                _ => None,
            });
        }
    }

    /// The end of a diff: each `pages` that swapped, with its direction
    /// from `row_first` before (`was`) and now (`now(pages)`).
    pub fn settle(&mut self, now: impl Fn(NodeId) -> Option<f32>) -> Vec<Settled> {
        let mut out: Vec<Settled> = self
            .pending
            .drain()
            .filter(|(_, p)| p.entering.is_some() || p.leaving.is_some())
            .map(|(pages, p)| {
                let was = p.was.flatten();
                let now = now(pages);
                let (forward, moved) = match (was, now) {
                    (Some(a), Some(b)) => (b >= a, a != b),
                    _ => (true, false),
                };
                Settled {
                    pages,
                    swap: PageSwap {
                        entering: p.entering,
                        leaving: p.leaving,
                        forward,
                    },
                    moved,
                }
            })
            .collect();
        out.sort_by_key(|s| s.pages);
        for s in &out {
            self.last.insert(s.pages, s.swap);
        }
        out
    }

    /// The last swap of `pages`, while its old page is still playing out
    /// (`live(id)`: the node is in the tree, ghost or not).
    pub fn last(&self, pages: NodeId) -> Option<PageSwap> {
        self.last.get(&pages).copied()
    }

    /// Forgets swaps whose pages are gone and whose leaving page has
    /// finished playing out.
    pub fn prune(
        &mut self,
        mut playing: impl FnMut(NodeId) -> bool,
        mut live: impl FnMut(NodeId) -> bool,
    ) {
        self.last
            .retain(|pages, s| live(*pages) && s.leaving.is_some_and(&mut playing));
    }
}

/// The slide a page plays: entering from the right going forward (from
/// the left going back), leaving to the left going forward (to the right
/// going back). Each moves by the page's own width (`slide(edge)`).
pub(crate) fn slide(entering: bool, forward: bool) -> PropValue {
    let edge = if entering == forward { "right" } else { "left" };
    PropValue::Call {
        name: "slide".into(),
        args: vec![PropValue::Keyword(edge.into())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_follows_source_order() {
        let (pages, a, b) = (NodeId::new(1, 0), NodeId::new(2, 0), NodeId::new(3, 0));
        let mut s = PageSwaps::default();
        s.moving(pages, Some(&PropValue::Number(4.0)));
        s.removed(pages, a);
        s.created(pages, b);
        let out = s.settle(|_| Some(9.0));
        assert_eq!(
            out,
            [Settled {
                pages,
                swap: PageSwap {
                    entering: Some(b),
                    leaving: Some(a),
                    forward: true
                },
                moved: true
            }]
        );
        s.moving(pages, Some(&PropValue::Number(9.0)));
        s.removed(pages, b);
        s.created(pages, a);
        let out = s.settle(|_| Some(4.0));
        assert!(!out[0].swap.forward && out[0].moved);
        // A diff that only moves `row_first` (a reload) swaps nothing.
        s.moving(pages, Some(&PropValue::Number(4.0)));
        assert!(s.settle(|_| Some(5.0)).is_empty());
        assert_eq!(slide(true, true), slide(false, false));
        assert_ne!(slide(true, true), slide(false, true));
    }
}
