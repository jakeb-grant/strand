//! The session lock (`ext_session_lock_v1`): design.md, "Lock screen";
//! docs/architecture.md, "strand-surface", "Session lock".
//!
//! A `lock` spec that opens asks the compositor for a lock
//! ([`State::lock`]). While the lock is asked for or held, every output
//! gets a lock surface, outputs plugged in later included: the focused
//! one ([`State::set_focused_monitor`], else the first output) shows the
//! config's content, a [`Surface`] the host paints like any other (its
//! keys reach the host as usual); every other output shows a solid in the
//! lock's colour ([`State::set_lock_color`], opaque), whose keyboard focus
//! counts as the content's. A solid is one `wp_single_pixel_buffer_v1`
//! pixel the viewporter stretches over the output (design.md: "Scrims and
//! lock backgrounds use single-pixel buffers"), or the same shm fallback
//! as a scrim's when the compositor lacks the protocol ([`crate::solid`]). The host hears [`SurfaceHost::lock_changed`]:
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
//!   the content surface keeps its id, so the binary can paint the
//!   built-in fallback there ([`State::lock_content`]).
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
    /// The `lock` spec's node (kept while locked after the spec went).
    node: Option<NodeId>,
    /// The spec is gone (removed while locked).
    spec_gone: bool,
    /// No new lock until the spec closes: this one was unlocked or
    /// refused while the spec still said open.
    spent: bool,
    /// The surface showing the config's content.
    content: Option<SurfaceId>,
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
            node: None,
            spec_gone: false,
            spent: false,
            content: None,
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
        self.session_lock.spent = !self.session_lock.spec_gone && self.lock_spec_open();
        if self.session_lock.spec_gone {
            self.session_lock.node = None;
            self.session_lock.spec_gone = false;
        }
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

    /// The surface showing the lock's content while a lock is asked for
    /// or held: where the config's lock, or the built-in fallback, is
    /// painted.
    pub fn lock_content(&self) -> Option<SurfaceId> {
        self.session_lock.content
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

    fn lock_spec_open(&self) -> bool {
        self.session_lock
            .node
            .and_then(|n| self.specs.get(&n))
            .is_some_and(|s| s.open)
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
                self.session_lock.node == Some(node)
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
                if let Some(old) = self.session_lock.node
                    && old != node
                {
                    // A config has one `lock` (`check::lock_twice`), so a
                    // new node is that lock mounted again by a reload,
                    // whose `Created` comes before the old node's
                    // `Removed`. It takes over: the old spec goes (its
                    // `Removed` then finds nothing), a held lock stays
                    // and its content moves to the new node, and a spent
                    // lock stays spent (a remount is not a new request).
                    self.specs.remove(&old);
                    if self.lock_active()
                        && let Some(id) = self.session_lock.content.take()
                    {
                        self.destroy_surface(id);
                    }
                }
                self.session_lock.node = Some(node);
                self.session_lock.spec_gone = false;
                self.specs.insert(node, spec);
                self.reconcile_lock();
            }
            SurfaceChange::Removed => {
                self.specs.remove(&node);
                if self.session_lock.node == Some(node) {
                    if self.lock_active() {
                        // Fail closed: the lock stays, and its content
                        // surface keeps its id for the fallback.
                        self.session_lock.spec_gone = true;
                    } else {
                        self.session_lock.node = None;
                        self.session_lock.spent = false;
                    }
                }
            }
        }
        None
    }

    /// Follows the lock spec: an open spec asks for a lock (once per
    /// opening); a closed one never unlocks, it only re-arms the next
    /// opening.
    pub(super) fn reconcile_lock(&mut self) {
        let Some(spec) = self.session_lock.node.and_then(|n| self.specs.get(&n)) else {
            self.sync_lock_surfaces();
            return;
        };
        if !spec.open {
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
                Err(e) => {
                    log::warn!("{e}");
                    self.session_lock.spent = true;
                }
            }
            return;
        }
        self.sync_lock_surfaces();
    }

    // ---- lock surfaces -------------------------------------------------------

    /// The output that shows the content: the focused monitor's, else the
    /// first.
    fn lock_output(&self) -> Option<u32> {
        self.focused_output()
            .or_else(|| self.outputs.keys().next().copied())
    }

    /// Makes the lock surfaces match the outputs while a lock is asked
    /// for or held: the content on [`Self::lock_output`], a solid on each
    /// other output. Surfaces of gone outputs go first, and a surface
    /// that must move is destroyed before its replacement is made on
    /// that output (one lock surface per output).
    pub(super) fn sync_lock_surfaces(&mut self) {
        let Some(lock) = self.session_lock.phase.lock().cloned() else {
            return;
        };
        let want = self.lock_output();
        // (m4-audit) Keyboard focus on the content (its own, or a solid's
        // that counts as its) moves with it: the compositor sends no new
        // enter while the focused solid stays, and the new content has
        // another id.
        let mut refocus = false;
        if let Some(id) = self.session_lock.content {
            let on = self.surfaces.get(&id).and_then(|s| s.output);
            if on.is_none() || on != want {
                refocus = self.keyboard_focus == Some(id);
                self.session_lock.content = None;
                self.destroy_surface(id);
            }
        }
        let stale: Vec<u32> = self
            .session_lock
            .solids
            .keys()
            .copied()
            .filter(|g| !self.outputs.contains_key(g) || Some(*g) == want)
            .collect();
        for g in stale {
            if let Some(s) = self.session_lock.solids.remove(&g) {
                s.destroy();
            }
        }
        if self.session_lock.content.is_none()
            && let Some(g) = want
        {
            self.create_lock_content(&lock, g);
        }
        if refocus && let Some(id) = self.session_lock.content {
            self.keyboard_focus = Some(id);
            self.send_input(crate::InputEvent::KeyboardEnter { surface: id });
        }
        let missing: Vec<u32> = self
            .outputs
            .keys()
            .copied()
            .filter(|g| Some(*g) != want && !self.session_lock.solids.contains_key(g))
            .collect();
        for g in missing {
            self.create_solid(&lock, g);
        }
    }

    fn destroy_lock_surfaces(&mut self) {
        if let Some(id) = self.session_lock.content.take() {
            self.destroy_surface(id);
        }
        for (_, s) in std::mem::take(&mut self.session_lock.solids) {
            s.destroy();
        }
    }

    fn create_lock_content(&mut self, lock: &ExtSessionLockV1, global: u32) {
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
        let node = self.session_lock.node.unwrap_or(LOCK_FALLBACK_NODE);
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
            stats: Stats::default(),
        };
        self.surfaces.insert(id, surface);
        self.session_lock.content = Some(id);
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
    /// the lock content's for a solid lock surface (the compositor may
    /// focus any lock surface).
    pub(super) fn keyboard_target(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.surface_for(wl).or_else(|| {
            self.session_lock
                .solids
                .values()
                .any(|s| &s.wl == wl)
                .then_some(self.session_lock.content)
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
                if self.session_lock.spec_gone {
                    // A lock asked for again shows the fallback.
                    self.session_lock.node = None;
                    self.session_lock.spec_gone = false;
                }
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
