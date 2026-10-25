//! (M4) Media feeds (architecture.md, "M4 additions"): render says which
//! fed nodes are visible ([`strand_render::Renderer::take_feed_demand`],
//! asked by [`sync`] after each paint, each diff applied and each
//! surface detach, and when a producer sends), and this keeps one
//! producer per
//! visible node: a `spectrum`'s audio level tap, whose readings (64
//! bands FFT'd on the audio thread) cross to the main thread on a
//! channel and are fed to the renderer there ([`fed`]), and a
//! `thumbnail`'s window capture (`strand_services::wm::capture`), whose
//! frames, already scaled down to the thumbnail's size on the protocol
//! thread, cross the same way. (services-round) A `thumbnail` of a
//! screen (`thumbnail screen`: its source is the `Screen`'s id, which the
//! host tells this as monitors come and go, [`Feeds::screen`]) takes one
//! frame of that monitor (`strand_services::wm::screen`) each time it is
//! shown, and keeps it while shown: a thumbnail on the screen it shows
//! would otherwise show itself, frame after frame.
//!
//! (lockmon) Stills taken before a surface covers the screen:
//!
//! - A surface whose tree holds a `thumbnail screen` takes a still of
//!   each screen it shows before it maps ([`Feeds::before_map`]): its
//!   first frame is held (at most [`HOLD`]) until they come, and its
//!   thumbnails show them, so a panel showing its own screen shows the
//!   screen as it was before the panel opened, not the panel. A
//!   thumbnail shown later than [`STILL_KEEP`] after its surface mapped
//!   (an `if` that mounts it then) takes a frame then, as before.
//! - The lock's: just before a lock is asked for, while every output
//!   still shows the desktop, a still of each screen a `thumbnail screen`
//!   inside the `lock` names is taken ([`Feeds::take_lock_stills`],
//!   bounded by [`LOCK_STILL_WAIT`]), and the lock's thumbnails show only
//!   those: never a frame taken while locked (it would show the lock).
//!   A monitor with none (plugged in while locked, or no frame in time)
//!   shows an empty thumbnail. They are dropped when the lock ends.
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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use calloop::channel::Sender;
use strand_render::{FeedDemand, FeedKind, ThumbnailFrame};
use strand_scene::{NodeId, NodeKind, Prop, PropValue, SurfaceId};
use strand_services::audio::{LevelTap, LevelTarget, tap_levels};
use strand_services::wm::capture::{CaptureFrame, CaptureTap, capture_window, downscale};
use strand_services::wm::screen::{ScreenShot, capture_screen};

/// What a producer sends for its node, with its tap's number.
#[derive(Debug)]
pub(crate) enum Fed {
    /// Spectrum bands, from the audio thread.
    Bands(NodeId, u64, Vec<f32>),
    /// A thumbnail's frame (`None`: its window is gone), from the
    /// compositor protocol thread.
    Frame(NodeId, u64, Option<ThumbnailFrame>),
    /// (lockmon) A still of a screen (its connector) taken before a
    /// surface mapped, with its shot's number ([`Feeds::before_map`]).
    Still(String, u64, Option<CaptureFrame>),
}

/// (lockmon) How long a surface's first frame waits for the stills of the
/// screens it shows ([`Feeds::before_map`]).
pub(crate) const HOLD: Duration = Duration::from_millis(300);

/// (lockmon) How long after it was taken a still taken before a surface
/// mapped stands for a frame of its screen.
pub(crate) const STILL_KEEP: Duration = Duration::from_secs(2);

/// (lockmon) How long the lock request waits for the stills of the
/// screens the lock shows ([`Feeds::take_lock_stills`]).
pub(crate) const LOCK_STILL_WAIT: Duration = Duration::from_millis(500);

/// What a node is fed by, and what it was tapped for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    Spectrum(u32),
    Thumbnail(String, (u32, u32)),
    /// (services-round) A screen's one frame: its connector name.
    Screen(String, (u32, u32)),
    /// (lockmon) Inside the lock, a screen's still from before the lock
    /// (its connector), or nothing: never a frame taken while locked.
    LockStill(String, (u32, u32)),
}

/// A running producer.
#[allow(dead_code)] // held for its Drop
enum Tap {
    Levels(LevelTap),
    Capture(CaptureTap),
    Shot(ScreenShot),
    /// (lockmon) A still sent at once, or nothing: no producer runs.
    Still,
    /// (lockmon) Waiting for the still its screen's shot before a
    /// surface mapped is taking: sent when it comes.
    Waiting,
}

/// The producers of the visible fed nodes.
pub(crate) struct Feeds {
    tx: Sender<Fed>,
    /// Each tapped node's want, its tap's number and its tap.
    taps: HashMap<NodeId, (Want, u64, Tap)>,
    /// The last tap's number.
    next: u64,
    /// (services-round) Each plugged monitor's connector name, by its
    /// `Screen` id.
    screens: HashMap<String, String>,
    /// (lockmon) The fed nodes inside a `lock`, as [`sync`] last saw.
    lock_nodes: HashSet<NodeId>,
    /// (lockmon) Each screen's still from just before the lock, by
    /// connector, while the lock lasts.
    lock_stills: HashMap<String, CaptureFrame>,
    /// (lockmon) Stills taken before a surface mapped, by connector, with
    /// when they came; the shots still taking them, numbered; and the
    /// surfaces whose first frame waits for them, until when.
    stills: HashMap<String, (Instant, Option<CaptureFrame>)>,
    shooting: HashMap<String, (u64, ScreenShot, Instant)>,
    held: HashMap<SurfaceId, (Instant, Vec<String>)>,
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
            screens: HashMap::new(),
            lock_nodes: HashSet::new(),
            lock_stills: HashMap::new(),
            stills: HashMap::new(),
            shooting: HashMap::new(),
            held: HashMap::new(),
        }
    }

    /// (lockmon) The connectors of the plugged screens a `thumbnail` in
    /// `root`'s subtree of `tree` shows.
    pub(crate) fn screens_under(
        &self,
        tree: &strand_render::SceneTree,
        root: NodeId,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            let Some(node) = tree.get(n) else {
                continue;
            };
            if node.kind == NodeKind::Thumbnail
                && let Some(PropValue::Text(t) | PropValue::Keyword(t)) = node.get(Prop::Source)
                && let Some(c) = self.screens.get(t)
                && !out.contains(c)
            {
                out.push(c.clone());
            }
            stack.extend(node.children.iter().copied());
        }
        out
    }

    /// (lockmon) The connectors of the plugged screens a `thumbnail`
    /// inside a `lock` of `tree` shows.
    pub(crate) fn lock_screens(&self, tree: &strand_render::SceneTree) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for root in tree.roots() {
            if tree.get(*root).is_some_and(|n| n.kind == NodeKind::Lock) {
                for c in self.screens_under(tree, *root) {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    /// (lockmon) A lock is about to be asked for: a still of each screen
    /// in `connectors`, taken now (the desktop still shows), waiting at
    /// most `wait`. Kept until [`Feeds::lock_ended`]; a lock asked for
    /// again while they are kept (the compositor ended the lock it held)
    /// keeps the first ones.
    pub(crate) fn take_lock_stills(&mut self, connectors: &[String], wait: Duration) {
        if connectors.is_empty() || !self.lock_stills.is_empty() {
            return;
        }
        let started = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel::<(String, Option<CaptureFrame>)>();
        let shots: Vec<ScreenShot> = connectors
            .iter()
            .map(|c| {
                let tx = tx.clone();
                let name = c.clone();
                capture_screen(c, (0, 0), move |f| {
                    let _ = tx.send((name, f));
                })
            })
            .collect();
        drop(tx);
        let deadline = started + wait;
        let mut answered = 0;
        while answered < connectors.len() {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok((c, frame)) = rx.recv_timeout(left) else {
                break;
            };
            answered += 1;
            if let Some(f) = frame {
                self.lock_stills.insert(c, f);
            }
        }
        // Those not in yet are cancelled.
        drop(shots);
        log::info!(
            "lock: {} of {} screen stills before the lock in {} ms",
            self.lock_stills.len(),
            connectors.len(),
            started.elapsed().as_millis()
        );
    }

    /// (lockmon) The lock ended: its stills go.
    pub(crate) fn lock_ended(&mut self) {
        self.lock_stills.clear();
    }

    /// (lockmon) Surface `surface` is about to map showing the screens
    /// `connectors` in thumbnails: a still of each is taken, and its
    /// first frame waits for them ([`Feeds::holds`]) at most [`HOLD`].
    pub(crate) fn before_map(&mut self, surface: SurfaceId, connectors: Vec<String>) {
        if connectors.is_empty() {
            return;
        }
        let now = Instant::now();
        self.stills
            .retain(|_, (at, _)| now.saturating_duration_since(*at) < STILL_KEEP);
        for c in &connectors {
            if self.shooting.contains_key(c) {
                continue;
            }
            self.next += 1;
            let n = self.next;
            let tx = self.tx.clone();
            let name = c.clone();
            let shot = capture_screen(c, (0, 0), move |f| {
                let _ = tx.send(Fed::Still(name, n, f));
            });
            self.shooting.insert(c.clone(), (n, shot, now));
        }
        self.held.insert(surface, (now + HOLD, connectors));
    }

    /// (lockmon) Until when `surface`'s first frame waits for the stills
    /// of its screens, if it still does at `now`.
    pub(crate) fn holds(&self, surface: SurfaceId, now: Instant) -> Option<Instant> {
        let (until, connectors) = self.held.get(&surface)?;
        (*until > now && connectors.iter().any(|c| self.shooting.contains_key(c))).then_some(*until)
    }

    /// (lockmon) `surface` went: nothing waits for it.
    pub(crate) fn detached(&mut self, surface: SurfaceId) {
        self.held.remove(&surface);
    }

    /// (lockmon) A still came (`shot` its number): kept for the
    /// thumbnails shown next. False when that shot was not this
    /// screen's latest.
    fn still(&mut self, connector: String, shot: u64, frame: Option<CaptureFrame>) -> bool {
        if self
            .shooting
            .get(&connector)
            .is_none_or(|(n, ..)| *n != shot)
        {
            return false;
        }
        if let Some((.., at)) = self.shooting.remove(&connector) {
            log::debug!(
                "screen {connector}: a still before a surface mapped in {} ms ({})",
                at.elapsed().as_millis(),
                if frame.is_some() { "taken" } else { "none" }
            );
        }
        // The thumbnails waiting for it get it (none when it failed:
        // they show nothing, rather than a frame taken now).
        for (node, (want, n, tap)) in &mut self.taps {
            if let (Want::Screen(c, max), Tap::Waiting) = (&*want, &*tap)
                && *c == connector
            {
                if let Some(f) = &frame {
                    let _ = self.tx.send(Fed::Frame(*node, *n, Some(scaled(f, *max))));
                }
                *tap = Tap::Still;
            }
        }
        self.stills.insert(connector, (Instant::now(), frame));
        let shooting = &self.shooting;
        self.held
            .retain(|_, (_, cs)| cs.iter().any(|c| shooting.contains_key(c)));
        true
    }

    /// (services-round) Monitor `id` (a `Screen`'s id) is plugged in as
    /// `connector` (`None`: unplugged, or no connector named).
    pub(crate) fn screen(&mut self, id: &str, connector: Option<&str>) {
        match connector {
            Some(c) => {
                self.screens.insert(id.to_string(), c.to_string());
            }
            None => {
                self.screens.remove(id);
            }
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
                    // A screen's id names no window: a `Screen` source.
                    let w = match self.screens.get(&window) {
                        Some(connector) if self.lock_nodes.contains(&d.node) => {
                            Want::LockStill(connector.clone(), max)
                        }
                        Some(connector) => Want::Screen(connector.clone(), max),
                        None => Want::Thumbnail(window, max),
                    };
                    want.insert(d.node, w);
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
                Want::LockStill(connector, max) => {
                    let still = self.lock_stills.get(connector);
                    log::debug!(
                        "thumbnail {node:?}: screen {connector}'s still from before the lock: {}",
                        if still.is_some() { "shown" } else { "none" }
                    );
                    if let Some(f) = still {
                        let _ = tx.send(Fed::Frame(node, n, Some(scaled(f, *max))));
                    }
                    Tap::Still
                }
                Want::Screen(connector, max)
                    if self
                        .stills
                        .get(connector)
                        .is_some_and(|(at, f)| f.is_some() && at.elapsed() < STILL_KEEP) =>
                {
                    log::debug!(
                        "thumbnail {node:?}: screen {connector}'s still from before it mapped"
                    );
                    if let Some((_, Some(f))) = self.stills.get(connector) {
                        let _ = tx.send(Fed::Frame(node, n, Some(scaled(f, *max))));
                    }
                    Tap::Still
                }
                Want::Screen(connector, _) if self.shooting.contains_key(connector) => {
                    log::debug!("thumbnail {node:?}: waiting for screen {connector}'s still");
                    Tap::Waiting
                }
                Want::Screen(connector, max) => {
                    log::debug!("thumbnail {node:?}: a frame of screen {connector} at {max:?}");
                    Tap::Shot(capture_screen(connector, *max, move |f| {
                        let frame = f.map(|f| ThumbnailFrame {
                            width: f.width,
                            height: f.height,
                            pixels: f.pixels,
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
    pub(crate) fn tapped(&self) -> Vec<(NodeId, Want)> {
        let mut v: Vec<_> = self
            .taps
            .iter()
            .map(|(n, (w, ..))| (*n, w.clone()))
            .collect();
        v.sort_by_key(|(n, _)| *n);
        v
    }
}

/// (lockmon) A still scaled down to cover `max`, as a thumbnail's frame.
fn scaled(f: &CaptureFrame, max: (u32, u32)) -> ThumbnailFrame {
    let f = downscale(&f.pixels, f.width, f.height, f.width * 4, false, max);
    ThumbnailFrame {
        width: f.width,
        height: f.height,
        pixels: f.pixels,
    }
}

/// Runs producers for the fed nodes visible now, if that changed (after
/// a paint, a diff applied, a surface detached, or a producer sent
/// something): the spectra that lost theirs are fed silence.
pub(crate) fn sync(renderer: &mut strand_render::Renderer, feeds: &mut Feeds) {
    if let Some(demand) = renderer.take_feed_demand() {
        let tree = renderer.tree();
        feeds.lock_nodes = demand
            .iter()
            .map(|d| d.node)
            .filter(|n| {
                tree.root_of(*n)
                    .and_then(|r| tree.get(r))
                    .is_some_and(|r| r.kind == NodeKind::Lock)
            })
            .collect();
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
    match fed {
        // (lockmon) The surfaces it held paint now.
        Fed::Still(connector, shot, frame) => feeds.still(connector, shot, frame),
        Fed::Bands(node, tap, bands) => {
            let current = feeds.current(node, tap);
            if current {
                renderer.feed(node, &bands);
            }
            current
        }
        Fed::Frame(node, tap, frame) => {
            let current = feeds.current(node, tap);
            if current {
                renderer.feed_frame(node, frame);
            }
            current
        }
    }
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

    /// (services-round) A thumbnail whose source is a plugged monitor's
    /// `Screen` id takes a frame of that monitor by its connector; one
    /// shown again takes another; an unplugged one's id names a window
    /// again (none, so nothing is captured).
    #[test]
    fn a_screens_thumbnail_takes_a_frame_of_its_monitor() {
        let (tx, _rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let a = NodeId::new(1, 0);
        f.screen("Dell U2720Q", Some("DP-1"));
        f.screen("BOE 0x0bca", Some("eDP-1"));
        f.demand(vec![thumbnail(1, "Dell U2720Q", (320, 180))]);
        assert_eq!(f.tapped(), [(a, Want::Screen("DP-1".into(), (320, 180)))]);
        let first = f.taps[&a].1;
        // Hidden, then shown again: a new frame.
        f.demand(vec![]);
        f.demand(vec![thumbnail(1, "Dell U2720Q", (320, 180))]);
        assert_ne!(f.taps[&a].1, first);
        // Moved to another port: the frame of its new connector.
        f.screen("Dell U2720Q", Some("DP-2"));
        f.demand(vec![]);
        f.demand(vec![thumbnail(1, "Dell U2720Q", (320, 180))]);
        assert_eq!(f.tapped(), [(a, Want::Screen("DP-2".into(), (320, 180)))]);
        f.screen("Dell U2720Q", None);
        f.demand(vec![]);
        f.demand(vec![thumbnail(1, "Dell U2720Q", (320, 180))]);
        assert_eq!(
            f.tapped(),
            [(a, Want::Thumbnail("Dell U2720Q".into(), (320, 180)))]
        );
    }

    /// A 4×2 still, every pixel `v`.
    fn still(v: u8) -> CaptureFrame {
        CaptureFrame {
            width: 4,
            height: 2,
            pixels: vec![v; 4 * 2 * 4].into(),
        }
    }

    /// The frames sent so far: (node, its size).
    fn frames(rx: &calloop::channel::Channel<Fed>) -> Vec<(NodeId, Option<(u32, u32)>)> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let Fed::Frame(node, _, frame) = f {
                out.push((node, frame.map(|f| (f.width, f.height))));
            }
        }
        out
    }

    /// (lockmon) A `thumbnail screen` inside the lock shows that screen's
    /// still from before the lock, scaled to it, and never takes a frame
    /// while locked: a screen with no still (plugged in while locked)
    /// shows nothing. The stills go when the lock ends; a lock asked for
    /// again while they are kept keeps the first ones.
    #[test]
    fn a_lock_thumbnail_shows_the_still_from_before_the_lock() {
        let (tx, rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        f.screen("Dell", Some("DP-1"));
        f.screen("BOE", Some("eDP-1"));
        f.lock_stills.insert("DP-1".into(), still(9));
        f.take_lock_stills(&["DP-1".into()], Duration::ZERO);
        assert_eq!(f.lock_stills["DP-1"].pixels[0], 9, "the first kept");
        f.lock_nodes = [a, b].into_iter().collect();
        f.demand(vec![
            thumbnail(1, "Dell", (2, 1)),
            thumbnail(2, "BOE", (2, 1)),
        ]);
        assert_eq!(
            f.tapped(),
            [
                (a, Want::LockStill("DP-1".into(), (2, 1))),
                (b, Want::LockStill("eDP-1".into(), (2, 1)))
            ]
        );
        assert!(f.taps.values().all(|(_, _, t)| matches!(t, Tap::Still)));
        assert_eq!(frames(&rx), [(a, Some((2, 1)))], "no frame for eDP-1");
        f.lock_ended();
        assert!(f.lock_stills.is_empty());
    }

    /// (lockmon) A surface showing a screen in a thumbnail holds its
    /// first frame for that screen's still (at most `HOLD`); the still
    /// stands for the frame its thumbnail shows, not one taken after the
    /// surface mapped (a thumbnail shown before it came waits for it),
    /// until `STILL_KEEP` has passed.
    #[test]
    fn a_still_from_before_a_surface_mapped_is_what_its_thumbnail_shows() {
        let (tx, rx) = calloop::channel::channel();
        let mut f = Feeds::new(tx);
        let a = NodeId::new(1, 0);
        let s = SurfaceId(4);
        f.screen("Dell", Some("DP-1"));
        let now = Instant::now();
        f.before_map(s, vec!["DP-1".into()]);
        assert!(
            f.holds(s, now)
                .is_some_and(|t| t <= now + HOLD + Duration::from_millis(50))
        );
        assert_eq!(
            f.holds(s, now + HOLD + Duration::from_millis(50)),
            None,
            "bounded"
        );
        let shot = f.shooting["DP-1"].0;
        // Shown before the still came (the hold ran out): it waits for
        // it rather than taking a frame of the surface now.
        f.demand(vec![thumbnail(1, "Dell", (2, 1))]);
        assert!(matches!(f.taps[&a].2, Tap::Waiting));
        assert!(
            !f.still("DP-1".into(), shot + 1, Some(still(1))),
            "another shot's"
        );
        assert!(f.still("DP-1".into(), shot, Some(still(7))));
        assert_eq!(f.holds(s, now), None, "it came");
        assert!(matches!(f.taps[&a].2, Tap::Still));
        assert_eq!(frames(&rx), [(a, Some((2, 1)))]);
        // Shown again within `STILL_KEEP`: the still at once.
        f.demand(vec![]);
        f.demand(vec![thumbnail(1, "Dell", (4, 2))]);
        assert!(matches!(f.taps[&a].2, Tap::Still));
        assert_eq!(frames(&rx), [(a, Some((4, 2)))]);
        // Too old: a frame is taken again.
        f.demand(vec![]);
        if let Some((at, _)) = f.stills.get_mut("DP-1") {
            *at = Instant::now() - STILL_KEEP;
        }
        f.demand(vec![thumbnail(1, "Dell", (4, 2))]);
        assert!(matches!(f.taps[&a].2, Tap::Shot(_)));
        // Its surface gone: nothing waits for it.
        f.before_map(s, vec!["DP-1".into()]);
        f.detached(s);
        assert_eq!(f.holds(s, Instant::now()), None);
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
