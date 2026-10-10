//! (M4) Media feeds (architecture.md, "M4 additions"): render says which
//! fed nodes are visible ([`strand_render::Renderer::take_feed_demand`],
//! asked by [`sync`] after each paint and each surface detach, and when a
//! producer sends), and this keeps one producer per
//! visible node: a `spectrum`'s audio level tap, whose readings (64
//! bands FFT'd on the audio thread) cross to the main thread on a
//! channel and are fed to the renderer there ([`fed`]), and a
//! `thumbnail`'s window capture (`strand_services::wm::capture`), whose
//! frames, already scaled down to the thumbnail's size on the protocol
//! thread, cross the same way.
//!
//! A tap lives exactly while its node is visible, so a hidden spectrum
//! costs no metering and no FFT and a hidden thumbnail no capture; one
//! whose device, window or size changes is tapped again. A spectrum that
//! loses its tap is fed silence, so it rests until it is shown and fed
//! again rather than keeping stale bars; a thumbnail keeps its last
//! frame, shown at once when it is visible again, until its window is
//! gone (its capture says so) or its source names another window (render
//! drops the frame then).
//!
//! Each tap is numbered, and what it sends carries its number: a reading
//! or frame from a tap that has since gone or been replaced (still queued
//! when its node was hidden, re-tapped or rested) is dropped, so it can
//! neither overwrite a rest nor show another window's frame.

use std::collections::HashMap;

use calloop::channel::Sender;
use strand_render::{FeedDemand, FeedKind, ThumbnailFrame};
use strand_scene::NodeId;
use strand_services::audio::{LevelTap, LevelTarget, tap_levels};
use strand_services::wm::capture::{CaptureTap, capture_window};

/// What a producer sends for its node, with its tap's number.
#[derive(Debug)]
pub(crate) enum Fed {
    /// Spectrum bands, from the audio thread.
    Bands(NodeId, u64, Vec<f32>),
    /// A thumbnail's frame (`None`: its window is gone), from the
    /// compositor protocol thread.
    Frame(NodeId, u64, Option<ThumbnailFrame>),
}

impl Fed {
    fn tap(&self) -> (NodeId, u64) {
        match self {
            Fed::Bands(node, tap, _) | Fed::Frame(node, tap, _) => (*node, *tap),
        }
    }
}

/// What a node is fed by, and what it was tapped for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Want {
    Spectrum(u32),
    Thumbnail(String, (u32, u32)),
}

/// A running producer.
#[allow(dead_code)] // held for its Drop
enum Tap {
    Levels(LevelTap),
    Capture(CaptureTap),
}

/// The producers of the visible fed nodes.
pub(crate) struct Feeds {
    tx: Sender<Fed>,
    /// Each tapped node's want, its tap's number and its tap.
    taps: HashMap<NodeId, (Want, u64, Tap)>,
    /// The last tap's number.
    next: u64,
}

impl std::fmt::Debug for Feeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeds")
            .field("taps", &self.taps.len())
            .finish_non_exhaustive()
    }
}

/// The audio device a spectrum's source names: logic sends the device's
/// id as text (`spectrum audio.sink`).
fn device(text: &str) -> Option<u32> {
    let t = text.trim();
    t.parse::<u32>().ok().or_else(|| {
        t.parse::<f64>()
            .ok()
            .filter(|v| v.fract() == 0.0 && *v >= 0.0 && *v <= u32::MAX as f64)
            .map(|v| v as u32)
    })
}

impl Feeds {
    pub(crate) fn new(tx: Sender<Fed>) -> Self {
        Self {
            tx,
            taps: HashMap::new(),
            next: 0,
        }
    }

    /// Runs producers for exactly `demand`; returns the spectra that lost
    /// theirs (to be fed silence).
    pub(crate) fn demand(&mut self, demand: Vec<FeedDemand>) -> Vec<NodeId> {
        let mut want: HashMap<NodeId, Want> = HashMap::new();
        for d in demand {
            match d.kind {
                FeedKind::Spectrum { device: text } => match device(&text) {
                    Some(id) => {
                        want.insert(d.node, Want::Spectrum(id));
                    }
                    None => log::debug!("spectrum {:?}: no audio device in {text:?}", d.node),
                },
                FeedKind::Thumbnail { window, max } => {
                    want.insert(d.node, Want::Thumbnail(window, max));
                }
            }
        }
        let mut lost = Vec::new();
        self.taps.retain(|node, (had, ..)| {
            let keep = want.get(node) == Some(had);
            if !keep && !want.contains_key(node) && matches!(had, Want::Spectrum(_)) {
                lost.push(*node);
            }
            keep
        });
        for (node, w) in want {
            if self.taps.contains_key(&node) {
                continue;
            }
            let tx = self.tx.clone();
            self.next += 1;
            let n = self.next;
            let tap = match &w {
                Want::Spectrum(dev) => {
                    log::debug!("spectrum {node:?}: metering audio device {dev}");
                    Tap::Levels(tap_levels(LevelTarget::Device(*dev), move |l| {
                        let _ = tx.send(Fed::Bands(node, n, l.bins.clone()));
                    }))
                }
                Want::Thumbnail(window, max) => {
                    log::debug!("thumbnail {node:?}: capturing window {window} at {max:?}");
                    Tap::Capture(capture_window(window, *max, move |f| {
                        let frame = f.map(|f| ThumbnailFrame {
                            width: f.width,
                            height: f.height,
                            pixels: f.pixels.clone(),
                        });
                        let _ = tx.send(Fed::Frame(node, n, frame));
                    }))
                }
            };
            self.taps.insert(node, (w, n, tap));
        }
        lost.sort();
        lost
    }

    /// Whether `tap` is the running tap of `node` (what it sent is
    /// current).
    fn current(&self, node: NodeId, tap: u64) -> bool {
        self.taps.get(&node).is_some_and(|(_, n, _)| *n == tap)
    }

    /// What each tapped node is fed by (tests).
    #[cfg(test)]
    fn tapped(&self) -> Vec<(NodeId, Want)> {
        let mut v: Vec<_> = self
            .taps
            .iter()
            .map(|(n, (w, ..))| (*n, w.clone()))
            .collect();
        v.sort_by_key(|(n, _)| *n);
        v
    }
}

/// Runs producers for the fed nodes visible now, if that changed (after
/// a paint, a surface detached, or a producer sent something): the
/// spectra that lost theirs are fed silence.
pub(crate) fn sync(renderer: &mut strand_render::Renderer, feeds: &mut Feeds) {
    if let Some(demand) = renderer.take_feed_demand() {
        for node in feeds.demand(demand) {
            renderer.feed(node, &[]);
        }
    }
}

/// Bands or a frame arrived for a node: it repaints. A producer whose
/// node is no longer visible with no paint since (its surface detached,
/// the others idle) is stopped here, at its first send; what it sent, and
/// anything else from a tap that is no longer its node's, is dropped
/// without a repaint.
pub(super) fn fed(state: &mut strand_surface::State<crate::demo::host::Host>, fed: Fed) {
    let host = state.host_mut();
    let Some(feeds) = &mut host.feeds else {
        return;
    };
    if !deliver(&mut host.renderer, feeds, fed) {
        return;
    }
    state.poll();
}

/// [`fed`] without the repaint: syncs the taps, then feeds `fed` to its
/// node if its tap is still the node's; false when it was dropped.
fn deliver(renderer: &mut strand_render::Renderer, feeds: &mut Feeds, fed: Fed) -> bool {
    sync(renderer, feeds);
    let (node, tap) = fed.tap();
    if !feeds.current(node, tap) {
        return false;
    }
    match fed {
        Fed::Bands(node, _, bands) => renderer.feed(node, &bands),
        Fed::Frame(node, _, frame) => renderer.feed_frame(node, frame),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spectrum(node: u32, device: &str) -> FeedDemand {
        FeedDemand {
            node: NodeId::new(node, 0),
            kind: FeedKind::Spectrum {
                device: device.into(),
            },
        }
    }

    fn thumbnail(node: u32, window: &str, max: (u32, u32)) -> FeedDemand {
        FeedDemand {
            node: NodeId::new(node, 0),
            kind: FeedKind::Thumbnail {
                window: window.into(),
                max,
            },
        }
    }

    #[test]
    fn taps_follow_the_visible_spectra() {
        let (tx, _rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let s = Want::Spectrum;
        assert_eq!(f.demand(vec![spectrum(1, "40"), spectrum(2, "41.0")]), []);
        assert_eq!(f.tapped(), [(a, s(40)), (b, s(41))]);
        // The first's device changed: tapped again, not rested.
        assert_eq!(f.demand(vec![spectrum(1, "42"), spectrum(2, "41")]), []);
        assert_eq!(f.tapped(), [(a, s(42)), (b, s(41))]);
        // The second hidden: its tap goes and it rests.
        assert_eq!(f.demand(vec![spectrum(1, "42")]), [b]);
        assert_eq!(f.tapped(), [(a, s(42))]);
        // A source that names no device is not tapped.
        assert_eq!(f.demand(vec![spectrum(1, ""), spectrum(2, "-1")]), [a]);
        assert_eq!(f.tapped(), []);
    }

    #[test]
    fn captures_follow_the_visible_thumbnails() {
        let (tx, _rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let t = |w: &str, m| Want::Thumbnail(w.into(), m);
        assert_eq!(
            f.demand(vec![
                thumbnail(1, "w1", (128, 64)),
                thumbnail(2, "w2", (64, 64))
            ]),
            []
        );
        assert_eq!(
            f.tapped(),
            [(a, t("w1", (128, 64))), (b, t("w2", (64, 64)))]
        );
        // A new size: captured again at it.
        f.demand(vec![
            thumbnail(1, "w1", (192, 64)),
            thumbnail(2, "w2", (64, 64)),
        ]);
        assert_eq!(
            f.tapped(),
            [(a, t("w1", (192, 64))), (b, t("w2", (64, 64)))]
        );
        // Hidden: its capture goes, and it is not fed silence.
        assert_eq!(f.demand(vec![thumbnail(1, "w1", (192, 64))]), []);
        assert_eq!(f.tapped(), [(a, t("w1", (192, 64)))]);
        // A spectrum and a thumbnail side by side.
        assert_eq!(f.demand(vec![spectrum(2, "40")]), []);
        assert_eq!(f.tapped(), [(b, Want::Spectrum(40))]);
    }

    /// A panel holding one `spectrum` of device 40, on surface 1.
    fn spectrum_scene() -> (strand_render::Renderer, NodeId) {
        use strand_scene::{NodeKind, Prop, PropValue, SceneDiff, SurfaceId};
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let fonts = strand_text::FontConfig::isolated(vec![std::sync::Arc::new(font)]);
        let engine = strand_text::TextEngine::new(fonts);
        let mut r =
            strand_render::Renderer::new(strand_render::TextBackend::Inline(Box::new(engine)));
        let (root, node) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let mut d = SceneDiff::default();
        d.create(root, NodeKind::Panel, None, u32::MAX);
        d.set(root, Prop::Width, PropValue::Number(80.0));
        d.set(root, Prop::Height, PropValue::Number(40.0));
        d.create(node, NodeKind::Spectrum, Some(root), u32::MAX);
        d.set(node, Prop::Source, PropValue::Text("40".into()));
        d.set(node, Prop::Width, PropValue::Number(64.0));
        d.set(node, Prop::Height, PropValue::Number(32.0));
        assert!(r.apply(d).is_empty());
        r.attach_surface(SurfaceId(1), root);
        paint(&mut r);
        (r, node)
    }

    fn paint(r: &mut strand_render::Renderer) {
        use strand_scene::{PaintTarget, Painter, Scale, Size, SurfaceId};
        let size = Size::new(80, 40);
        let mut px = vec![0u8; (size.w * size.h * 4) as usize];
        let mut t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 1).unwrap();
        r.paint(SurfaceId(1), &mut t);
    }

    /// What a spectrum's old tap sent (still queued when it was re-tapped
    /// or rested) is dropped: it neither overwrites the rest nor
    /// repaints.
    #[test]
    fn a_spectrum_drops_what_its_old_tap_sent() {
        use strand_scene::{Prop, PropValue, SceneDiff, SurfaceId};
        let (mut r, node) = spectrum_scene();
        let (tx, _rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        sync(&mut r, &mut f);
        assert_eq!(f.tapped(), [(node, Want::Spectrum(40))]);
        let first = f.taps[&node].1;
        let loud = vec![1.0; 64];
        assert!(deliver(
            &mut r,
            &mut f,
            Fed::Bands(node, first, loud.clone())
        ));
        assert!(!r.spectrum_at_rest(node), "fed");
        // Another device: tapped again, and the old tap's bands dropped.
        let mut d = SceneDiff::default();
        d.set(node, Prop::Source, PropValue::Text("41".into()));
        assert!(r.apply(d).is_empty());
        paint(&mut r);
        sync(&mut r, &mut f);
        let second = f.taps[&node].1;
        assert_ne!(first, second);
        assert!(!deliver(
            &mut r,
            &mut f,
            Fed::Bands(node, first, loud.clone())
        ));
        assert!(deliver(
            &mut r,
            &mut f,
            Fed::Bands(node, second, loud.clone())
        ));
        // Its surface detached: the tap goes and it rests; bands still
        // queued from that tap leave it resting.
        r.detach_surface(SurfaceId(1));
        assert!(!deliver(&mut r, &mut f, Fed::Bands(node, second, loud)));
        assert_eq!(f.tapped(), []);
        assert!(r.spectrum_at_rest(node), "rests, not stale bars");
    }
}
