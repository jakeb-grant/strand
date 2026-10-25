//! The session lock (`ext_session_lock_v1`): design.md, "Lock screen";
//! docs/architecture.md, "strand-surface", "Session lock".
//!
//! A `lock` spec that opens asks the compositor for a lock
//! ([`State::lock`]). While the lock is asked for or held, every output
//! gets a lock surface, outputs plugged in later included. (lockmon) A
//! config's lock is one node per monitor (the compiler mounts an
//! instance on each, every monitor by default), each pinned to its
//! monitor by its spec's `screens:`; a node on `screens: focused` shows
//! on the focused monitor ([`State::set_focused_monitor`], else the first
//! output). Each output with a node shows that node's content, a
//! [`Surface`] the host paints like any other (its keys reach the host
//! as usual); an output with none (a monitor no instance has reached
//! yet, one the config's `screens:` leaves out, one whose instance the
//! host gave up on: [`State::lock_instance_failed`]) shows a solid in the
//! lock's colour ([`State::set_lock_color`], opaque), whose keyboard
//! focus counts as the focused output's content's. A solid is one
//! `wp_single_pixel_buffer_v1` pixel the viewporter stretches over the
//! output (design.md: "Scrims and lock backgrounds use single-pixel
//! buffers"), or the same shm fallback as a scrim's when the compositor
//! lacks the protocol ([`crate::solid`]). While no node can show (none is
//! mounted, every one went or failed), the content surfaces there are
//! stay for the host's built-in field, or one is made on the focused
//! output. The host hears [`SurfaceHost::lock_changed`]:
//! `Locked` once the compositor says every output is covered, `Finished`
//! when it refuses or ends the lock, `Unlocked` after an unlock.
//!
//! Nothing locks until the owner opts in ([`State::enable_session_lock`]):
//! a lock that only a token releases must not be taken by a build that
//! routes no token to [`State::unlock`]. Until then an open `lock` spec
//! is a warning and [`State::lock`] is [`LockError::NotEnabled`].
//!
//! Fail-closed rules:
//! - Only [`State::unlock`], which takes a [`strand_auth::UnlockToken`],
//!   releases a lock the compositor holds (the other `unlock_and_destroy`
//!   answers `finished`, on an object the compositor already gave up). The spec closing (`open: false`) or going away
//!   (a reload, logic gone) changes nothing: the lock surfaces stay and
//!   the content surfaces keep their ids, so the binary can paint the
//!   built-in fallback there ([`State::lock_contents`]).
//! - A token that arrives before `locked` is kept and spent the moment
//!   `locked` is dispatched: `destroy` is a protocol error once the
//!   compositor has sent `locked`, and it may be on the wire already, so
//!   a pending lock is never destroyed for a token. `finished` drops it.
//! - Dropping the manager or losing the connection sends nothing: the
//!   compositor keeps the session locked (the protocol forbids unlocking
//!   when the client dies).
//! - `finished` without `locked` is a diagnostic and the lock counts as
//!   not shown; it is not asked for again until the spec closes and
//!   opens, so a compositor that refuses is not asked in a loop.
//! - `finished` after `locked` (the compositor ended a lock it held) is
//!   answered as the protocol asks, with `unlock_and_destroy` on that
//!   finished object (the compositor no longer uses it, so this releases
//!   the object, not the session), then by asking for a new lock at
//!   once, once per lock session
//!   ([`after_finished`]): the protocol leaves it to the compositor
//!   whether the session stays locked, and if it does, the user needs a
//!   password field again, not the compositor's blank fallback. A
//!   compositor that ends that one too, or refuses it, is not asked
//!   again until the spec closes and opens.
//! - The protocol forbids committing a lock surface before acking its
//!   first configure, committing one with no buffer, and committing at a
//!   size other than the acked one: the content surface acks a configure
//!   only right before the buffer commit at that size, and never makes a
//!   bare commit before its first buffer.

use super::scrim::{ScrimObject, shm_solid};
use crate::solid::{SolidBuffer, solid_buffer};
use smithay_client_toolkit::shm::raw::RawPool;
use strand_auth::UnlockToken;
use strand_scene::Color;
use wayland_protocols::ext::session_lock::v1::client::{
    ext_session_lock_manager_v1::{self, ExtSessionLockManagerV1},
    ext_session_lock_surface_v1::{self, ExtSessionLockSurfaceV1},
    ext_session_lock_v1::{self, ExtSessionLockV1},
};

use super::*;

/// The node a lock's content surface is attached to when no `lock` spec
/// is there ([`State::lock`] with nothing compiled): render has no such
/// node, so the binary paints its built-in fallback on it.
pub const LOCK_FALLBACK_NODE: NodeId = NodeId::new(u32::MAX, u32::MAX);

/// Why [`State::lock`] could not ask for a lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockError {
    /// The compositor offers no `ext_session_lock_manager_v1`.
    Unsupported,
    /// [`State::enable_session_lock`] was not called: nothing would
    /// release the lock.
    NotEnabled,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "the compositor does not offer ext-session-lock: the session cannot be locked"
            ),
            Self::NotEnabled => write!(
                f,
                "the session lock is not enabled in this build (nothing would unlock it): \
                 the session is not locked"
            ),
        }
    }
}

impl std::error::Error for LockError {}

/// Where the lock is.
#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    /// Asked for; no `locked` yet.
    Pending(ExtSessionLockV1),
    /// The compositor said `locked`.
    Locked(ExtSessionLockV1),
}

impl Phase {
    fn lock(&self) -> Option<&ExtSessionLockV1> {
        match self {
            Phase::Idle => None,
            Phase::Pending(l) | Phase::Locked(l) => Some(l),
        }
    }
}

/// The session lock's state in [`State`].
#[derive(Debug)]
pub(super) struct SessionLock {
    manager: Option<ExtSessionLockManagerV1>,
    phase: Phase,
    /// (lockmon) The `lock` specs' nodes (one per monitor's instance), in
    /// the order they came: a later one takes an output an earlier one
    /// also asks for (a reload mounts the lock again before the old one
    /// goes). One whose spec went while a lock is asked for or held stays
    /// until the lock ends: the fallback's content may still show it.
    nodes: Vec<NodeId>,
    /// (lockmon) Nodes the host gave up on in this lock session
    /// ([`State::lock_instance_failed`]): their outputs show the solid.
    failed: BTreeSet<NodeId>,
    /// No new lock until the spec closes: this one was unlocked or
    /// refused while the spec still said open.
    spent: bool,
    /// The surfaces showing a node's content, by `wl_output` global.
    contents: BTreeMap<u32, SurfaceId>,
    /// The other outputs' solids, by `wl_output` global.
    solids: BTreeMap<u32, Solid>,
    color: Color,
    /// [`State::enable_session_lock`] was called.
    enabled: bool,
    /// The not-enabled warning was given.
    warned_disabled: bool,
    /// A token that came while the lock was pending: spent on `locked`.
    deferred: Option<UnlockToken>,
    /// A lock was asked for again after `finished` followed `locked`, in
    /// this lock session (until an unlock or the spec closing).
    asked_again: bool,
}

impl SessionLock {
    pub(super) fn new(manager: Option<ExtSessionLockManagerV1>) -> Self {
        Self {
            manager,
            phase: Phase::Idle,
            nodes: Vec::new(),
            failed: BTreeSet::new(),
            spent: false,
            contents: BTreeMap::new(),
            solids: BTreeMap::new(),
            color: Color::BLACK,
            enabled: false,
            warned_disabled: false,
            deferred: None,
            asked_again: false,
        }
    }
}

/// A lock surface's protocol objects, destroyed (lock surface first) when
/// dropped.
pub(super) struct LockSurface {
    lock_surface: ExtSessionLockSurfaceV1,
    wl: wl_surface::WlSurface,
    /// The last configure not acked yet: acked right before the buffer
    /// commit at its size.
    serial: Option<u32>,
}

impl LockSurface {
    pub(super) fn wl(&self) -> &wl_surface::WlSurface {
        &self.wl
    }

    /// Acks the pending configure (before a buffer commit at its size).
    pub(super) fn ack(&mut self) {
        if let Some(serial) = self.serial.take() {
            self.lock_surface.ack_configure(serial);
        }
    }
}

impl Drop for LockSurface {
    fn drop(&mut self) {
        self.lock_surface.destroy();
        self.wl.destroy();
    }
}

/// A solid lock surface on an output that does not show the content.
#[derive(Debug)]
struct Solid {
    lock_surface: ExtSessionLockSurfaceV1,
    wl: wl_surface::WlSurface,
    viewport: Option<WpViewport>,
    /// Its buffer: a single pixel (no pool) or an shm one.
    buffer: Option<(Option<RawPool>, wl_buffer::WlBuffer)>,
    /// The size of its last configure (acked): every buffer matches it.
    size: Option<(u32, u32)>,
}

impl Solid {
    fn destroy(mut self) {
        if let Some((_, b)) = self.buffer.take() {
            b.destroy();
        }
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        self.lock_surface.destroy();
        self.wl.destroy();
    }
}

/// What `finished` leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterFinished {
    /// No new lock until the spec closes and opens.
    Spent,
    /// Ask for a new lock now.
    AskAgain,
}

/// The rule for `finished`: after `locked`, the compositor ended a lock
/// it held, and it may keep the session locked, so a new lock is asked
/// for, once per lock session; a refusal (`finished` before `locked`)
/// or a second end is spent.
fn after_finished(was_locked: bool, asked_again: bool) -> AfterFinished {
    if was_locked && !asked_again {
        AfterFinished::AskAgain
    } else {
        AfterFinished::Spent
    }
}

/// User data of a lock object.
#[derive(Debug)]
pub struct LockTag;

/// User data of a lock surface: the content surface, or the solid on an
/// output (by its global).
#[derive(Debug, Clone, Copy)]
pub enum LockSurfaceTag {
    Content(SurfaceId),
    Solid(u32),
}

/// The config every lock surface has (the layer fields are unused: a
/// lock surface has no layer, anchors or margins; the compositor sizes
/// it to its output).
fn lock_config() -> LayerConfig {
    LayerConfig {
        namespace: "strand-lock".to_string(),
        layer: Layer::Overlay,
        anchors: crate::placement::Anchors {
            top: true,
            bottom: true,
            left: true,
            right: true,
        },
        width: 0,
        height: 0,
        exclusive_zone: 0,
        margin: [0; 4],
        keyboard: Keyboard::Exclusive,
        overhang: [0; 4],
        click_through: false,
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    // ---- the public API ------------------------------------------------------

    /// Lets this manager take session locks. Off by default, so a build
    /// that does not route `auth`'s tokens to [`Self::unlock`] can never
    /// lock a session it cannot release; the binary calls this only once
    /// it has wired them (m4-lock wave 2, `run/lock.rs`).
    pub fn enable_session_lock(&mut self) {
        self.session_lock.enabled = true;
        self.reconcile_lock();
    }

    /// [`Self::enable_session_lock`] was called.
    pub fn session_lock_enabled(&self) -> bool {
        self.session_lock.enabled
    }

    /// The compositor offers `ext_session_lock_manager_v1`: without it a
    /// `lock` cannot lock the session ([`LockError::Unsupported`]).
    pub fn session_lock_supported(&self) -> bool {
        self.session_lock.manager.is_some()
    }

    /// Asks the compositor to lock the session, if no lock is asked for
    /// or held. Every output gets a lock surface at once (the protocol
    /// asks for them before `locked`); the host hears `Locked` or
    /// `Finished`. Called when a `lock` spec opens; the binary also calls
    /// it to lock with its fallback when no lock is compiled.
    /// [`LockError::NotEnabled`] before [`Self::enable_session_lock`].
    pub fn lock(&mut self) -> Result<(), LockError> {
        if self.session_lock.phase.lock().is_some() {
            return Ok(());
        }
        if !self.session_lock.enabled {
            return Err(LockError::NotEnabled);
        }
        if self.session_lock.manager.is_none() {
            return Err(LockError::Unsupported);
        }
        // (lockmon) The host's last look at the desktop.
        self.host.before_lock();
        let Some(manager) = &self.session_lock.manager else {
            return Err(LockError::Unsupported);
        };
        let lock = manager.lock(&self.qh, LockTag);
        self.session_lock.phase = Phase::Pending(lock);
        self.sync_lock_surfaces();
        Ok(())
    }

    /// Releases the session lock: the only way it is released, and only
    /// for an [`UnlockToken`] (`strand_auth`'s client mints one from the
    /// PAM helper's success). False when no lock was asked for or held.
    /// The lock is not asked for again until its spec closes.
    ///
    /// While the lock is pending (asked for, no `locked` dispatched yet)
    /// the token is kept and the unlock happens when `locked` arrives;
    /// a `finished` instead drops it. `destroy` is never sent for a
    /// token: if `locked` is already on the wire it is a protocol error
    /// (`invalid_destroy`) that would end the connection and leave the
    /// session locked with no lock client.
    pub fn unlock(&mut self, token: UnlockToken) -> bool {
        let lock = match std::mem::take(&mut self.session_lock.phase) {
            Phase::Locked(lock) => lock,
            Phase::Idle => return false,
            Phase::Pending(lock) => {
                self.session_lock.phase = Phase::Pending(lock);
                self.session_lock.deferred = Some(token);
                log::info!("session lock: unlocking once the compositor has locked");
                return true;
            }
        };
        let _spent = token;
        lock.unlock_and_destroy();
        self.session_lock.asked_again = false;
        // After the unlock request, so the compositor never shows its own
        // fallback colour between them.
        self.destroy_lock_surfaces();
        self.session_lock.spent = self.lock_spec_open();
        self.lock_session_ended();
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the unlock failed: {e}");
        }
        self.host.lock_changed(LockState::Unlocked);
        true
    }

    /// The compositor has locked the session (`locked`) and no unlock has
    /// been sent.
    pub fn is_locked(&self) -> bool {
        matches!(self.session_lock.phase, Phase::Locked(_))
    }

    /// A lock is asked for or held.
    pub fn lock_active(&self) -> bool {
        self.session_lock.phase.lock().is_some()
    }

    /// The surface whose keys the lock takes by default while a lock is
    /// asked for or held: the content on the focused output (else the
    /// first output that has one). (lockmon) Every monitor may show
    /// content: [`Self::lock_contents`] lists them all.
    pub fn lock_content(&self) -> Option<SurfaceId> {
        let l = &self.session_lock;
        self.lock_output()
            .and_then(|g| l.contents.get(&g))
            .or_else(|| l.contents.values().next())
            .copied()
    }

    /// (lockmon) Every surface showing the lock's content (a config's
    /// instance, or where the built-in fallback is painted), in output
    /// order.
    pub fn lock_contents(&self) -> Vec<SurfaceId> {
        self.session_lock.contents.values().copied().collect()
    }

    /// (lockmon) The surface showing the lock's content on output
    /// `output` (connector name, else monitor id).
    pub fn lock_content_on(&self, output: &str) -> Option<SurfaceId> {
        self.session_lock.contents.iter().find_map(|(g, id)| {
            let m = self.monitors.get(self.monitors.id_of(*g)?)?;
            let name = m.connector.clone().unwrap_or_else(|| m.id.to_string());
            (name == output).then_some(*id)
        })
    }

    /// (lockmon) The host gave up on the lock's instance `node` (a
    /// fault in it, no first frame): its output shows the solid from
    /// now until the lock ends, fail closed, while the other monitors
    /// keep theirs. False when `node` is no lock node, or failed already.
    pub fn lock_instance_failed(&mut self, node: NodeId) -> bool {
        if !self.session_lock.nodes.contains(&node) || !self.session_lock.failed.insert(node) {
            return false;
        }
        log::warn!("lock: the lock's instance {node:?} failed: its output shows the solid");
        self.sync_lock_surfaces();
        true
    }

    /// The outputs (by connector name, else monitor id) showing the
    /// lock's solid colour.
    pub fn lock_solid_outputs(&self) -> Vec<String> {
        self.session_lock
            .solids
            .keys()
            .filter_map(|g| {
                let id = self.monitors.id_of(*g)?;
                let m = self.monitors.get(id)?;
                Some(m.connector.clone().unwrap_or_else(|| id.to_string()))
            })
            .collect()
    }

    /// The colour of the solid lock surfaces (made opaque); black until
    /// set. Solids shown already are repainted.
    pub fn set_lock_color(&mut self, color: Color) {
        let color = color.with_alpha(1.0);
        if self.session_lock.color == color {
            return;
        }
        self.session_lock.color = color;
        let shown: Vec<(u32, (u32, u32))> = self
            .session_lock
            .solids
            .iter()
            .filter_map(|(g, s)| Some((*g, s.size?)))
            .collect();
        for (g, size) in shown {
            self.paint_solid(g, size);
        }
    }

    // ---- specs ---------------------------------------------------------------

    /// Some `lock` spec is open (its instances' `open` is one value).
    fn lock_spec_open(&self) -> bool {
        self.session_lock
            .nodes
            .iter()
            .filter_map(|n| self.specs.get(n))
            .any(|s| s.open)
    }

    /// The lock ended (an unlock, `finished`): nodes whose spec went and
    /// failures are forgotten.
    fn lock_session_ended(&mut self) {
        let specs = &self.specs;
        self.session_lock.nodes.retain(|n| specs.contains_key(n));
        self.session_lock.failed.clear();
    }

    /// Takes a change of a `lock` spec (or of the lock's node) off the
    /// layer-surface path; gives every other change back.
    pub(super) fn lock_spec_change(
        &mut self,
        node: NodeId,
        change: SurfaceChange,
    ) -> Option<SurfaceChange> {
        let is_lock = match &change {
            SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. } => {
                spec.kind == NodeKind::Lock
            }
            SurfaceChange::Removed => {
                self.session_lock.nodes.contains(&node)
                    || self
                        .specs
                        .get(&node)
                        .is_some_and(|s| s.kind == NodeKind::Lock)
            }
        };
        if !is_lock {
            return Some(change);
        }
        match change {
            SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. } => {
                // (lockmon) One node per monitor's instance; a reload's
                // new nodes come before the old ones go, and take their
                // outputs over (`lock_want`): a held lock stays, and a
                // spent one stays spent (a remount is not a new request).
                if !self.session_lock.nodes.contains(&node) {
                    self.session_lock.nodes.push(node);
                }
                self.specs.insert(node, spec);
                self.reconcile_lock();
            }
            SurfaceChange::Removed => {
                self.specs.remove(&node);
                if !self.lock_active() {
                    self.session_lock.nodes.retain(|n| *n != node);
                    self.session_lock.failed.remove(&node);
                    if self.session_lock.nodes.is_empty() {
                        self.session_lock.spent = false;
                    }
                }
                // Fail closed while locked: its content goes only for
                // another instance's (`lock_want`); with none left, the
                // content surfaces stay, with their ids, for the fallback.
                self.sync_lock_surfaces();
            }
        }
        None
    }

    /// Follows the lock spec: an open spec asks for a lock (once per
    /// opening); a closed one never unlocks, it only re-arms the next
    /// opening.
    pub(super) fn reconcile_lock(&mut self) {
        let any = self
            .session_lock
            .nodes
            .iter()
            .any(|n| self.specs.contains_key(n));
        if !any {
            self.sync_lock_surfaces();
            return;
        }
        if !self.lock_spec_open() {
            self.session_lock.spent = false;
            self.session_lock.asked_again = false;
        } else if !self.session_lock.spent && !self.lock_active() {
            match self.lock() {
                Ok(()) => {}
                // Not spent: enabling it later takes the lock.
                Err(e @ LockError::NotEnabled) => {
                    if !std::mem::replace(&mut self.session_lock.warned_disabled, true) {
                        log::warn!("`lock`: {e}");
                    }
                }
                // No ext-session-lock: the lock is refused like one the
                // compositor finished, so the host and logic stop
                // counting it as shown (decisions.md, m4-audit).
                Err(e @ LockError::Unsupported) => {
                    log::warn!("{e}");
                    self.session_lock.spent = true;
                    self.host.lock_changed(LockState::Finished);
                }
            }
            return;
        }
        self.sync_lock_surfaces();
    }

    // ---- lock surfaces -------------------------------------------------------

    /// The focused monitor's output, else the first: where a `screens:
    /// focused` instance shows, and the content keys on a solid reach.
    fn lock_output(&self) -> Option<u32> {
        self.focused_output()
            .or_else(|| self.outputs.keys().next().copied())
    }

    /// (lockmon) Which node's content each output shows: the newest live
    /// (spec mounted, not failed) node pinned to its monitor (`screens:`
    /// a name, or `all`), else on the focused output the newest live
    /// `screens: focused` node. A content whose node's spec went keeps
    /// its output while no live node takes it (the host paints its
    /// built-in field there); with no live node at all, a single one
    /// follows the focused output, or one is made there for the last
    /// node the lock had that did not fail (else [`LOCK_FALLBACK_NODE`]).
    fn lock_want(&self) -> BTreeMap<u32, NodeId> {
        let l = &self.session_lock;
        let live: Vec<(NodeId, &SurfaceSpec)> = l
            .nodes
            .iter()
            .filter(|n| !l.failed.contains(n))
            .filter_map(|n| Some((*n, self.specs.get(n)?)))
            .collect();
        let mut want = BTreeMap::new();
        for g in self.outputs.keys() {
            let Some(m) = self.monitors.id_of(*g).and_then(|id| self.monitors.get(id)) else {
                continue;
            };
            let pinned = live.iter().rev().find(|(_, spec)| match &spec.screens {
                Screens::Focused => false,
                _ => Self::wants(spec, m),
            });
            if let Some((n, _)) = pinned {
                want.insert(*g, *n);
            }
        }
        if let Some(g) = self.lock_output()
            && !want.contains_key(&g)
            && let Some((n, _)) = live
                .iter()
                .rev()
                .find(|(_, spec)| spec.screens == Screens::Focused)
        {
            want.insert(g, *n);
        }
        // Contents whose node's spec went (logic gone, an instance
        // unmounted) stay where no live node takes their output: the host
        // paints its built-in field there. With nothing live at all, a
        // single one follows the focused output, as the lock's one
        // content always did.
        let orphans: Vec<(u32, NodeId)> = l
            .contents
            .iter()
            .filter(|(g, _)| self.outputs.contains_key(g) && !want.contains_key(g))
            .filter_map(|(g, id)| Some((*g, self.surfaces.get(id)?.node)))
            .filter(|(_, n)| !l.failed.contains(n) && !self.specs.contains_key(n))
            .collect();
        if want.is_empty() && orphans.len() <= 1 {
            let node = orphans
                .first()
                .map(|(_, n)| *n)
                .or_else(|| {
                    l.nodes
                        .iter()
                        .rev()
                        .find(|n| !l.failed.contains(n))
                        .copied()
                })
                .unwrap_or(LOCK_FALLBACK_NODE);
            if let Some(g) = self.lock_output() {
                want.insert(g, node);
            }
            return want;
        }
        want.extend(orphans);
        want
    }

    /// Makes the lock surfaces match the outputs while a lock is asked
    /// for or held: each output's content ([`Self::lock_want`]), a solid
    /// on every other output. Surfaces of gone outputs go first, and a
    /// surface that must change is destroyed before its replacement is
    /// made on that output (one lock surface per output).
    pub(super) fn sync_lock_surfaces(&mut self) {
        let Some(lock) = self.session_lock.phase.lock().cloned() else {
            return;
        };
        let want = self.lock_want();
        // (m4-audit) Keyboard focus on a content that goes (its own, or a
        // solid's that counts as its) moves to the content keys on a
        // solid reach: the compositor sends no new enter while a focused
        // solid stays, and the new content has another id.
        let mut refocus = false;
        let stale: Vec<(u32, SurfaceId)> = self
            .session_lock
            .contents
            .iter()
            .filter(|(g, id)| {
                let s = self.surfaces.get(id);
                s.is_none_or(|s| s.output != Some(**g) || want.get(g) != Some(&s.node))
            })
            .map(|(g, id)| (*g, *id))
            .collect();
        for (g, id) in stale {
            refocus |= self.keyboard_focus == Some(id);
            self.session_lock.contents.remove(&g);
            self.destroy_surface(id);
        }
        let solids: Vec<u32> = self
            .session_lock
            .solids
            .keys()
            .copied()
            .filter(|g| !self.outputs.contains_key(g) || want.contains_key(g))
            .collect();
        for g in solids {
            if let Some(s) = self.session_lock.solids.remove(&g) {
                s.destroy();
            }
        }
        for (g, node) in &want {
            if !self.session_lock.contents.contains_key(g) {
                self.create_lock_content(&lock, *g, *node);
            }
        }
        let missing: Vec<u32> = self
            .outputs
            .keys()
            .copied()
            .filter(|g| {
                !self.session_lock.contents.contains_key(g)
                    && !self.session_lock.solids.contains_key(g)
            })
            .collect();
        for g in missing {
            self.create_solid(&lock, g);
        }
        if refocus && let Some(id) = self.lock_content() {
            self.keyboard_focus = Some(id);
            self.send_input(crate::InputEvent::KeyboardEnter { surface: id });
            // (gap 31) The same keyboard: its state again.
            self.report_keyboard();
        }
        // (services-round) The spec may have turned `inhibit_idle` on or
        // off in place.
        for id in self.lock_contents() {
            self.sync_idle_inhibitor(id);
        }
    }

    fn destroy_lock_surfaces(&mut self) {
        for (_, id) in std::mem::take(&mut self.session_lock.contents) {
            self.destroy_surface(id);
        }
        for (_, s) in std::mem::take(&mut self.session_lock.solids) {
            s.destroy();
        }
    }

    fn create_lock_content(&mut self, lock: &ExtSessionLockV1, global: u32, node: NodeId) {
        let Some(output) = self.outputs.get(&global).cloned() else {
            return;
        };
        let monitor = self
            .monitors
            .id_of(global)
            .and_then(|id| self.monitors.get(id))
            .cloned();
        let Some(monitor) = monitor else {
            return;
        };
        let placement = Placement::Monitor(monitor.id.clone());
        let key = (node, placement.clone());
        let id = match self.ids.get(&key) {
            Some(id) => *id,
            None => {
                let id = SurfaceId(self.next_id);
                self.next_id = self.next_id.wrapping_add(1).max(1);
                self.ids.insert(key, id);
                id
            }
        };
        let generation = self.next_generation;
        self.next_generation += 1;
        let wl = self.compositor.create_surface(&self.qh);
        let lock_surface =
            lock.get_lock_surface(&wl, &output, &self.qh, LockSurfaceTag::Content(id));
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_manager) {
            (Some(vp), Some(fm)) => (
                Some(vp.get_viewport(&wl, &self.qh, SurfaceTag(id))),
                Some(fm.get_fractional_scale(&wl, &self.qh, SurfaceTag(id))),
            ),
            _ => (None, None),
        };
        let (scale, integer_scale) = self.initial_scale(Some(global), fractional.is_some());
        self.by_wl.insert(wl.id(), id);
        // No commit: a lock surface's first commit carries its first
        // buffer, after its first configure.
        let surface = Surface {
            id,
            generation,
            node,
            kind: NodeKind::Lock,
            placement,
            monitor: Some(monitor.id.clone()),
            output: Some(global),
            requested_output: Some(global),
            role: Role::Lock(LockSurface {
                lock_surface,
                wl,
                serial: None,
            }),
            config: lock_config(),
            viewport,
            fractional,
            configured: false,
            logical: (0, 0),
            scale,
            reported_scale: None,
            integer_scale,
            buffers: ShmBuffers::new(id, self.max_buffers),
            geometry_dirty: true,
            callback_pending: false,
            commit_seq: 0,
            in_flight: None,
            throttled_at: None,
            ack_pending: false,
            repaint: true,
            opaque: Vec::new(),
            blur: None,
            blur_sent: Some(Vec::new()),
            pose: strand_scene::SurfacePose::IDENTITY,
            alpha: None,
            origin: None,
            zone_on: None,
            last_damage: Vec::new(),
            click_through: false,
            input_region: None,
            painted_input: None,
            stats: Stats::default(),
        };
        self.surfaces.insert(id, surface);
        self.session_lock.contents.insert(global, id);
        // (services-round) `inhibit_idle:` holds on a lock as on any
        // surface.
        self.sync_idle_inhibitor(id);
        self.host.surface_attached(id, node, Some(&monitor));
    }

    fn create_solid(&mut self, lock: &ExtSessionLockV1, global: u32) {
        let Some(output) = self.outputs.get(&global).cloned() else {
            return;
        };
        let wl = self.compositor.create_surface(&self.qh);
        let lock_surface =
            lock.get_lock_surface(&wl, &output, &self.qh, LockSurfaceTag::Solid(global));
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&wl, &self.qh, SurfaceTag(SurfaceId(0))));
        self.session_lock.solids.insert(
            global,
            Solid {
                lock_surface,
                wl,
                viewport,
                buffer: None,
                size: None,
            },
        );
    }

    /// Fills solid `global` at `(w, h)` logical pixels and commits it,
    /// as a scrim is filled (`catcher.rs`): one single-pixel buffer the
    /// viewport stretches when the compositor offers both protocols, a
    /// 1×1 shm pixel with the viewporter alone, else an shm buffer of the
    /// whole size (buffer scale 1, so the surface is `(w, h)`).
    fn paint_solid(&mut self, global: u32, (w, h): (u32, u32)) {
        let color = self.session_lock.color;
        let single_pixel = self.single_pixel.clone();
        let Some(s) = self.session_lock.solids.get(&global) else {
            return;
        };
        let solid = solid_buffer(color, (w, h), single_pixel.is_some(), s.viewport.is_some());
        let (buffer, (bw, bh)) = match (solid, &single_pixel) {
            (SolidBuffer::SinglePixel([r, g, b, a]), Some(sp)) => (
                (
                    None,
                    sp.create_u32_rgba_buffer(r, g, b, a, &self.qh, ScrimObject),
                ),
                (1, 1),
            ),
            (
                SolidBuffer::Shm {
                    width,
                    height,
                    pixel,
                },
                _,
            ) => match shm_solid(&self.shm, &self.qh, width, height, pixel) {
                Some(made) => made,
                None => {
                    log::warn!("no buffer for a lock background");
                    return;
                }
            },
            // Not reached: a single pixel is chosen only with the manager.
            (SolidBuffer::SinglePixel(_), None) => return,
        };
        let Some(s) = self.session_lock.solids.get_mut(&global) else {
            return;
        };
        if let Some(v) = &s.viewport {
            v.set_destination(clamp_i32(w.max(1)), clamp_i32(h.max(1)));
        }
        s.wl.set_buffer_scale(1);
        s.wl.attach(Some(&buffer.1), 0, 0);
        s.wl.damage_buffer(0, 0, bw, bh);
        s.wl.commit();
        if let Some((_, old)) = s.buffer.replace(buffer) {
            old.destroy();
        }
    }

    /// The buffer kind of solid lock surface `output` (connector name,
    /// else monitor id): `Some(true)` for a single pixel, `Some(false)`
    /// for shm, `None` when it has none yet.
    pub fn lock_solid_is_single_pixel(&self, output: &str) -> Option<bool> {
        self.session_lock.solids.iter().find_map(|(g, s)| {
            let id = self.monitors.id_of(*g)?;
            let m = self.monitors.get(id)?;
            let name = m.connector.clone().unwrap_or_else(|| id.to_string());
            (name == output).then(|| s.buffer.as_ref().map(|(pool, _)| pool.is_none()))?
        })
    }

    /// The surface whose keys a keyboard focus on `wl` delivers: ours, or
    /// for a solid lock surface (the compositor may focus any lock
    /// surface) the focused output's content ([`Self::lock_content`]).
    pub(super) fn keyboard_target(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.surface_for(wl).or_else(|| {
            self.session_lock
                .solids
                .values()
                .any(|s| &s.wl == wl)
                .then(|| self.lock_content())
                .flatten()
        })
    }

    // ---- events --------------------------------------------------------------

    fn lock_event(&mut self, lock: &ExtSessionLockV1, event: ext_session_lock_v1::Event) {
        let ours = self.session_lock.phase.lock() == Some(lock);
        match event {
            ext_session_lock_v1::Event::Locked => {
                if !ours {
                    return;
                }
                if let Phase::Pending(l) = std::mem::take(&mut self.session_lock.phase) {
                    self.session_lock.phase = Phase::Locked(l);
                    self.host.lock_changed(LockState::Locked);
                    if let Some(token) = self.session_lock.deferred.take() {
                        self.unlock(token);
                    }
                }
            }
            ext_session_lock_v1::Event::Finished => {
                if !ours {
                    return;
                }
                let was_locked = match std::mem::take(&mut self.session_lock.phase) {
                    Phase::Pending(l) => {
                        log::warn!(
                            "the compositor refused the session lock (another locker holds it, \
                             or its policy): the lock is not shown"
                        );
                        l.destroy();
                        false
                    }
                    Phase::Locked(l) => {
                        // The compositor ended the lock itself. The
                        // protocol's reply to `finished` after `locked`
                        // is `unlock_and_destroy` (`destroy` would be a
                        // protocol error): the compositor no longer uses
                        // this object, so it only releases it. Without
                        // the reply the object stays alive, and a
                        // compositor that counts it as the session's
                        // locker until it goes refuses the new lock
                        // asked for below. The new lock follows it in
                        // the same flush.
                        log::warn!("the compositor ended the session lock");
                        l.unlock_and_destroy();
                        true
                    }
                    Phase::Idle => false,
                };
                // A token for a lock the compositor never held unlocks
                // nothing.
                self.session_lock.deferred = None;
                self.destroy_lock_surfaces();
                self.session_lock.spent = true;
                // A lock asked for again shows what is mounted (the
                // fallback when nothing is).
                self.lock_session_ended();
                self.host.lock_changed(LockState::Finished);
                if after_finished(was_locked, self.session_lock.asked_again)
                    == AfterFinished::AskAgain
                {
                    // The session may still be locked: put the password
                    // field back with a new lock (fail closed).
                    self.session_lock.asked_again = true;
                    log::warn!("session lock: asking for a new lock");
                    if let Err(e) = self.lock() {
                        log::warn!("{e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn lock_surface_configure(&mut self, tag: LockSurfaceTag, serial: u32, (w, h): (u32, u32)) {
        match tag {
            LockSurfaceTag::Content(id) => {
                let Some(s) = self.surfaces.get_mut(&id) else {
                    return;
                };
                let Role::Lock(l) = &mut s.role else {
                    return;
                };
                l.serial = Some(serial);
                self.configured(id, (w.max(1), h.max(1)));
            }
            LockSurfaceTag::Solid(global) => {
                let Some(s) = self.session_lock.solids.get_mut(&global) else {
                    return;
                };
                s.lock_surface.ack_configure(serial);
                s.size = Some((w.max(1), h.max(1)));
                self.paint_solid(global, (w.max(1), h.max(1)));
            }
        }
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ExtSessionLockManagerV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &ExtSessionLockManagerV1,
        _: ext_session_lock_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ExtSessionLockV1, State<H>> for LockTag {
    fn event(
        &self,
        state: &mut State<H>,
        lock: &ExtSessionLockV1,
        event: ext_session_lock_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        state.lock_event(lock, event);
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<ExtSessionLockSurfaceV1, State<H>> for LockSurfaceTag {
    fn event(
        &self,
        state: &mut State<H>,
        _: &ExtSessionLockSurfaceV1,
        event: ext_session_lock_surface_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            state.lock_surface_configure(*self, serial, (width, height));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AfterFinished, after_finished};

    #[test]
    fn finished_after_locked_asks_for_a_new_lock_once() {
        // The compositor ended a lock it held: ask again.
        assert_eq!(after_finished(true, false), AfterFinished::AskAgain);
        // It ended the one asked for again too: no loop.
        assert_eq!(after_finished(true, true), AfterFinished::Spent);
        // A refusal is never asked again, whatever came before.
        assert_eq!(after_finished(false, false), AfterFinished::Spent);
        assert_eq!(after_finished(false, true), AfterFinished::Spent);
    }
}
