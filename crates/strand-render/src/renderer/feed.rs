//! (M4) Media feeds (architecture.md, "M4 additions"): spectrum bands
//! and window thumbnails' frames produced off the main thread reach their
//! nodes through the binary ([`Renderer::feed`], [`Renderer::feed_frame`]),
//! and render says which of those nodes are visible
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
    /// A `thumbnail`'s frames: of the window its source names (the
    /// window's id), at most `max` physical pixels (its drawn size,
    /// rounded up to a multiple of [`THUMBNAIL_STEP`] so a box that
    /// animates does not capture again every frame).
    Thumbnail { window: String, max: (u32, u32) },
}

/// The step a thumbnail's capture size is rounded up to.
pub const THUMBNAIL_STEP: u32 = 64;

fn step_up(v: u32) -> u32 {
    v.div_ceil(THUMBNAIL_STEP).max(1) * THUMBNAIL_STEP
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

    /// (history, graph-follow) Read `clock` as the boot clock at a time
    /// of the presentation clock: what a `graph`'s backlog is timed on
    /// and its columns fall on (a test's or a replay's; the real one is
    /// [`crate::clock::boot_at`]).
    pub fn set_history_clock(&mut self, clock: crate::clock::BootClock) {
        self.anim.set_boot_clock(clock);
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

    /// (M4) A new frame of `node`'s window (`None`: the window is gone):
    /// it repaints. Ignored for a node that is gone or no thumbnail.
    pub fn feed_frame(&mut self, node: NodeId, frame: Option<crate::ThumbnailFrame>) {
        let Some(Source::Thumbnail(s)) = self.extras.media.sources.get(&node) else {
            return;
        };
        s.feed(frame);
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
            // The largest it is drawn, in physical pixels.
            let mut drawn: Option<(u32, u32)> = None;
            for s in self.surfaces.values().filter(|s| s.painted) {
                if let Some(r) = s.records.get(id).filter(|r| !r.bounds.is_empty()) {
                    let (w, h) = drawn.unwrap_or((0, 0));
                    drawn = Some((w.max(r.bounds.w), h.max(r.bounds.h)));
                }
            }
            let Some(drawn) = drawn else {
                continue;
            };
            if let Some(kind) = self.feed_kind(*id, src, drawn, reduced) {
                now.push(FeedDemand { node: *id, kind });
            }
        }
        now.sort_by_key(|d| d.node);
        if now == self.extras.media.demand {
            return None;
        }
        self.extras.media.demand = now.clone();
        Some(now)
    }

    /// (lock-polish) The fed nodes `surface`'s first frame will draw, as
    /// the scene [`Renderer::update`] flattened for it has them: what
    /// [`Renderer::take_feed_demand`] asks for once that frame is
    /// painted, known before it is, so the binary can feed a lock's
    /// thumbnails the stills it already holds in time for its first
    /// frame. Empty once the surface has painted, or before its scene is
    /// flattened (it is configured, and laid out, before it paints).
    pub fn first_feed_demand(&self, surface: strand_scene::SurfaceId) -> Vec<FeedDemand> {
        let Some(s) = self.surfaces.get(&surface).filter(|s| !s.painted) else {
            return Vec::new();
        };
        let Some(scene) = &s.cache else {
            return Vec::new();
        };
        let reduced = self.reduced_motion();
        let mut out: Vec<FeedDemand> = self
            .extras
            .media
            .sources
            .iter()
            .filter_map(|(id, src)| {
                let r = scene.records.get(id).filter(|r| !r.bounds.is_empty())?;
                let kind = self.feed_kind(*id, src, (r.bounds.w, r.bounds.h), reduced)?;
                Some(FeedDemand { node: *id, kind })
            })
            .collect();
        out.sort_by_key(|d| d.node);
        out
    }

    /// What media node `id`, its source `src`, drawn at most `(dw, dh)`
    /// physical pixels, is fed; `None` for one fed nothing.
    fn feed_kind(
        &self,
        id: NodeId,
        src: &Source,
        (dw, dh): (u32, u32),
        reduced: bool,
    ) -> Option<FeedKind> {
        match src {
            Source::Spectrum(_) if reduced => None,
            Source::Spectrum(_) => {
                let device = match self.tree.get(id).and_then(|n| n.get(Prop::Source)) {
                    Some(PropValue::Text(t) | PropValue::Keyword(t)) => t.clone(),
                    Some(PropValue::Number(n)) => format!("{n}"),
                    _ => String::new(),
                };
                Some(FeedKind::Spectrum { device })
            }
            Source::Thumbnail(_) => match self.tree.get(id).and_then(|n| n.get(Prop::Source)) {
                Some(PropValue::Text(t) | PropValue::Keyword(t)) if !t.is_empty() => {
                    Some(FeedKind::Thumbnail {
                        window: t.clone(),
                        max: (step_up(dw), step_up(dh)),
                    })
                }
                _ => None,
            },
            Source::Graph(_) | Source::Svg(_) | Source::Lottie(_) => None,
        }
    }
}

impl Renderer {
    /// (M4) Whether a `spectrum` node rests (tests: a rested spectrum is
    /// not fed stale bands). `false` for anything else.
    #[doc(hidden)]
    pub fn spectrum_at_rest(&self, node: NodeId) -> bool {
        matches!(self.extras.media.sources.get(&node), Some(Source::Spectrum(s)) if s.at_rest())
    }

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
