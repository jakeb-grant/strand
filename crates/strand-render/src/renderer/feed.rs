//! (M4) Media feeds (architecture.md, "M4 additions"): spectrum bands
//! produced off the main thread reach their nodes through the binary
//! ([`Renderer::feed`]), and render says which of those nodes are visible
//! ([`Renderer::take_feed_demand`]) so producers run only for them. Media
//! nodes get their raster sources when logic creates them.

use strand_scene::{NodeId, NodeKind, Prop, PropValue};

use super::Renderer;
use crate::media::Source;

/// What a visible media node wants produced for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FeedDemand {
    pub node: NodeId,
    pub kind: FeedKind,
}

/// What a fed node takes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FeedKind {
    /// A `spectrum`'s bands, of the audio device its source names (the
    /// device's id as text; `""` before logic set it).
    Spectrum { device: String },
}

impl Renderer {
    /// Gives the media nodes a diff created their sources, and forgets
    /// those of nodes that are gone.
    pub(super) fn attach_media(&mut self, created: &[(NodeId, NodeKind)]) {
        for (id, kind) in created {
            if !self.tree.contains(*id) {
                continue;
            }
            if let Some(src) = Source::new(*kind) {
                self.extras.rasters.set(*id, Some(src.raster()));
                self.extras.media.sources.insert(*id, src);
            }
        }
        let tree = &self.tree;
        self.extras.media.sources.retain(|id, _| tree.contains(*id));
    }

    /// (M4) New spectrum bands for `node` (empty: the sound stopped): it
    /// repaints. Ignored for a node that is gone or no spectrum.
    pub fn feed(&mut self, node: NodeId, bands: &[f32]) {
        let Some(Source::Spectrum(s)) = self.extras.media.sources.get(&node) else {
            return;
        };
        s.feed(bands);
        self.mark_node_dirty(node);
    }

    /// (M4) The fed nodes visible now (drawn in some surface's last
    /// frame with pixels on it), if that changed since the last call:
    /// the binary runs producers for exactly these.
    ///
    /// Under `reduced_motion` no spectrum is fed (design.md: it turns off
    /// effects): each rests, drawn as dots, and no FFT runs for it.
    pub fn take_feed_demand(&mut self) -> Option<Vec<FeedDemand>> {
        let reduced = self.reduced_motion();
        if reduced {
            let resting: Vec<NodeId> = self
                .extras
                .media
                .sources
                .iter()
                .filter_map(|(id, src)| match src {
                    Source::Spectrum(s) if !s.at_rest() => {
                        s.feed(&[]);
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            for id in resting {
                self.mark_node_dirty(id);
            }
        }
        let mut now: Vec<FeedDemand> = Vec::new();
        for (id, src) in &self.extras.media.sources {
            let visible = self
                .surfaces
                .values()
                .any(|s| s.painted && s.records.get(id).is_some_and(|r| !r.bounds.is_empty()));
            if !visible {
                continue;
            }
            let kind = match src {
                Source::Spectrum(_) if reduced => continue,
                Source::Spectrum(_) => {
                    let device = match self.tree.get(*id).and_then(|n| n.get(Prop::Source)) {
                        Some(PropValue::Text(t) | PropValue::Keyword(t)) => t.clone(),
                        Some(PropValue::Number(n)) => format!("{n}"),
                        _ => String::new(),
                    };
                    FeedKind::Spectrum { device }
                }
                Source::Graph(_) => continue,
            };
            now.push(FeedDemand { node: *id, kind });
        }
        now.sort_by_key(|d| d.node);
        if now == self.extras.media.demand {
            return None;
        }
        self.extras.media.demand = now.clone();
        Some(now)
    }
}

impl Renderer {
    /// (M4) Columns a `graph` node has drawn so far (tests: a tick draws
    /// only its new columns).
    #[doc(hidden)]
    pub fn graph_columns_drawn(&self, node: NodeId) -> Option<u64> {
        match self.extras.media.sources.get(&node)? {
            Source::Graph(g) => Some(g.drawn_columns()),
            _ => None,
        }
    }
}
