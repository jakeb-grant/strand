//! (M4) Media feeds (architecture.md, "M4 additions"): render says which
//! fed nodes are visible ([`strand_render::Renderer::take_feed_demand`],
//! asked after each paint by the host), and this keeps one producer per
//! visible node: a `spectrum`'s audio level tap, whose readings (64
//! bands FFT'd on the audio thread) cross to the main thread on a
//! channel and are fed to the renderer there ([`fed`]).
//!
//! A tap lives exactly while its node is visible, so a hidden spectrum
//! costs no metering and no FFT; one whose device changes is tapped
//! again. A node that loses its tap is fed silence, so it rests until it
//! is shown and fed again rather than keeping stale bars.

use std::collections::HashMap;

use calloop::channel::Sender;
use strand_render::{FeedDemand, FeedKind};
use strand_scene::NodeId;
use strand_services::audio::{LevelTap, LevelTarget, tap_levels};

/// Bands for a node, from the audio thread.
pub(crate) type Bands = (NodeId, Vec<f32>);

/// The producers of the visible fed nodes.
pub(crate) struct Feeds {
    tx: Sender<Bands>,
    taps: HashMap<NodeId, (u32, LevelTap)>,
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
    pub(crate) fn new(tx: Sender<Bands>) -> Self {
        Self {
            tx,
            taps: HashMap::new(),
        }
    }

    /// Runs producers for exactly `demand`; returns the nodes that lost
    /// theirs (to be fed silence).
    pub(crate) fn demand(&mut self, demand: Vec<FeedDemand>) -> Vec<NodeId> {
        let mut want: HashMap<NodeId, u32> = HashMap::new();
        for d in demand {
            match d.kind {
                FeedKind::Spectrum { device: text } => match device(&text) {
                    Some(id) => {
                        want.insert(d.node, id);
                    }
                    None => log::debug!("spectrum {:?}: no audio device in {text:?}", d.node),
                },
            }
        }
        let mut lost = Vec::new();
        self.taps.retain(|node, (dev, _)| {
            let keep = want.get(node) == Some(dev);
            if !keep && !want.contains_key(node) {
                lost.push(*node);
            }
            keep
        });
        for (node, dev) in want {
            if self.taps.contains_key(&node) {
                continue;
            }
            log::debug!("spectrum {node:?}: metering audio device {dev}");
            let tx = self.tx.clone();
            let tap = tap_levels(LevelTarget::Device(dev), move |l| {
                let _ = tx.send((node, l.bins.clone()));
            });
            self.taps.insert(node, (dev, tap));
        }
        lost.sort();
        lost
    }

    /// The device each tapped node meters (tests).
    #[cfg(test)]
    fn tapped(&self) -> Vec<(NodeId, u32)> {
        let mut v: Vec<_> = self.taps.iter().map(|(n, (d, _))| (*n, *d)).collect();
        v.sort();
        v
    }
}

/// Bands arrived for `node`: it repaints.
pub(super) fn fed(
    state: &mut strand_surface::State<crate::demo::host::Host>,
    (node, bands): Bands,
) {
    state.host_mut().renderer.feed(node, &bands);
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

    #[test]
    fn taps_follow_the_visible_spectra() {
        let (tx, _rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        assert_eq!(f.demand(vec![spectrum(1, "40"), spectrum(2, "41.0")]), []);
        assert_eq!(f.tapped(), [(a, 40), (b, 41)]);
        // The first's device changed: tapped again, not rested.
        assert_eq!(f.demand(vec![spectrum(1, "42"), spectrum(2, "41")]), []);
        assert_eq!(f.tapped(), [(a, 42), (b, 41)]);
        // The second hidden: its tap goes and it rests.
        assert_eq!(f.demand(vec![spectrum(1, "42")]), [b]);
        assert_eq!(f.tapped(), [(a, 42)]);
        // A source that names no device is not tapped.
        assert_eq!(f.demand(vec![spectrum(1, ""), spectrum(2, "-1")]), [a]);
        assert_eq!(f.tapped(), []);
    }
}
