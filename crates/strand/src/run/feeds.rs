//! (M4) Media feeds (architecture.md, "M4 additions"): render says which
//! fed nodes are visible ([`strand_render::Renderer::take_feed_demand`],
//! asked after each paint by the host), and this keeps one producer per
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
//! frame, shown at once when it is visible again.

use std::collections::HashMap;

use calloop::channel::Sender;
use strand_render::{FeedDemand, FeedKind, ThumbnailFrame};
use strand_scene::NodeId;
use strand_services::audio::{LevelTap, LevelTarget, tap_levels};
use strand_services::wm::capture::{CaptureTap, capture_window};

/// What a producer sends for its node.
#[derive(Debug)]
pub(crate) enum Fed {
    /// Spectrum bands, from the audio thread.
    Bands(NodeId, Vec<f32>),
    /// A thumbnail's frame, from the compositor protocol thread.
    Frame(NodeId, ThumbnailFrame),
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
    taps: HashMap<NodeId, (Want, Tap)>,
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
        self.taps.retain(|node, (had, _)| {
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
            let tap = match &w {
                Want::Spectrum(dev) => {
                    log::debug!("spectrum {node:?}: metering audio device {dev}");
                    Tap::Levels(tap_levels(LevelTarget::Device(*dev), move |l| {
                        let _ = tx.send(Fed::Bands(node, l.bins.clone()));
                    }))
                }
                Want::Thumbnail(window, max) => {
                    log::debug!("thumbnail {node:?}: capturing window {window} at {max:?}");
                    Tap::Capture(capture_window(window, *max, move |f| {
                        let _ = tx.send(Fed::Frame(
                            node,
                            ThumbnailFrame {
                                width: f.width,
                                height: f.height,
                                pixels: f.pixels.clone(),
                            },
                        ));
                    }))
                }
            };
            self.taps.insert(node, (w, tap));
        }
        lost.sort();
        lost
    }

    /// What each tapped node is fed by (tests).
    #[cfg(test)]
    fn tapped(&self) -> Vec<(NodeId, Want)> {
        let mut v: Vec<_> = self
            .taps
            .iter()
            .map(|(n, (w, _))| (*n, w.clone()))
            .collect();
        v.sort_by_key(|(n, _)| *n);
        v
    }
}

/// Bands or a frame arrived for a node: it repaints.
pub(super) fn fed(state: &mut strand_surface::State<crate::demo::host::Host>, fed: Fed) {
    let renderer = &mut state.host_mut().renderer;
    match fed {
        Fed::Bands(node, bands) => renderer.feed(node, &bands),
        Fed::Frame(node, frame) => renderer.feed_frame(node, Some(frame)),
    }
    state.poll();
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
}
