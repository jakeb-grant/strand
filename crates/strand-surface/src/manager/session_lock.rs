//! The session lock (`ext_session_lock_v1`): design.md, "Lock screen";
//! docs/architecture.md, "strand-surface", "Session lock".
//!
//! A `lock` spec that opens asks the compositor for a lock
//! ([`State::lock`]). While the lock is asked for or held, every output
//! gets a lock surface, outputs plugged in later included. (lockmon) A
//! config's lock is one node per monitor (the compiler mounts an
//! instance on each, every monitor by default), each pinned to its
//! monitor by its spec's `screens:`; a node on `screens: focused` shows
//! on the focused monitor ([`State::set_focused_monitor`], else the
//! leftmost output, as logic's `screens.all` is ordered). Each output with a node shows that node's content, a
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
//! when it refuses or ends the lock, `Unlocking` while the lock plays its
//! way out, `Unlocked` after an unlock.
//!
//! (lock-polish) The lock plays its way out before it unlocks
//! ([`State::unlock_after_exit`]): the token is held while the lock's
//! nodes close (logic writes their two-way `open` false and render holds
//! each open, `SurfaceSpec::closing`, while its exit plays) and spent when
//! the last of them has closed, or [`LOCK_EXIT_CAP`] after it came, by a
//! timer of this manager's own whatever render does. Meanwhile the
//! session stays locked, input on the lock goes nowhere, each content
//! stays on its output and an output plugged in shows the solid. A node
//! seen leaving that opens again (the lock asked for again) keeps the
//! session locked: the token is dropped and the host hears `Locked`; one
//! still open at the cap is spent, as on any unlock (logic slow to close
//! it looks the same). A `finished` meanwhile ends the exit: the unlock
//! is sent at once.
//!
//! Nothing locks until the owner opts in ([`State::enable_session_lock`]):
//! a lock that only a token releases must not be taken by a build that
//! routes no token to [`State::unlock`]. Until then an open `lock` spec
//! is a warning and [`State::lock`] is [`LockError::NotEnabled`].
//!
//! Fail-closed rules:
//! - Only [`State::unlock`] and [`State::unlock_after_exit`], which take
//!   a [`strand_auth::UnlockToken`], release a lock the compositor holds
//!   (the exit only delays the unlock: nothing but the token it holds
//!   ends it) (the other `unlock_and_destroy`
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

/// (lock-polish) The longest a lock plays its way out: this long after
/// the token came, the unlock is sent whatever the exit is doing (a
/// stalled frame, a long or stuck pose). A second: two to four times what
/// a closing pose on Strand's springs takes (`$motion.spatial` settles in
/// about 0.2 s, a slow `spring(380, 0.86)` in about 0.5 s), the bound the
/// lock already gives a first frame (`FIRST_FRAME` in the binary) and
/// render a stalled exit (`EXIT_STALL`), and short enough that a stuck
/// one costs someone who has already typed their password one second.
pub const LOCK_EXIT_CAP: Duration = Duration::from_secs(1);

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

/// (lock-polish) An unlock waiting for the lock to play its way out.
#[derive(Debug)]
struct Leaving {
    /// Spent when the exit ends.
    token: UnlockToken,
    /// The lock nodes the unlock waits for: those shown on an output when
    /// the token came whose `open` logic closes (two-way).
    waited: BTreeSet<NodeId>,
    /// Of those, the ones seen leaving or closed since: one open again
    /// after that was asked for again.
    seen: BTreeSet<NodeId>,
    /// The cap's timer ([`LOCK_EXIT_CAP`]).
    timer: Option<RegistrationToken>,
    since: Instant,
}

/// (lock-polish) What the lock's nodes say about an exit in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStep {
    /// Some still play their exit (or logic has not closed them yet).
    Wait,
    /// Every one closed or went: unlock.
    Done,
    /// One seen leaving is open again: the lock was asked for again.
    AskedAgain,
}

/// (lock-polish) The rule for an exit in progress: each waited node's
/// spec as `(open, closing)`, `None` when it went (its spec, or its
/// output). A node closed or going is done; one leaving is waited for; one
/// open and not leaving is waited for until it has been seen leaving
/// (logic has yet to close it), after which it was opened again. `seen`
/// gains the nodes seen leaving or closed.
fn exit_step(
    waited: &BTreeSet<NodeId>,
    seen: &mut BTreeSet<NodeId>,
    spec: impl Fn(NodeId) -> Option<(bool, bool)>,
) -> ExitStep {
    let mut done = true;
    for n in waited {
        match spec(*n) {
            None | Some((false, _)) => {
                seen.insert(*n);
            }
            Some((true, true)) => {
                seen.insert(*n);
                done = false;
            }
            Some((true, false)) if seen.contains(n) => return ExitStep::AskedAgain,
            Some((true, false)) => done = false,
        }
    }
    if done { ExitStep::Done } else { ExitStep::Wait }
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
    /// (lock-polish) The lock plays its way out, a token held.
    leaving: Option<Leaving>,
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
            leaving: None,
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
    ///
    /// (lock-polish) While the lock plays its way out, this ends the exit
    /// and unlocks at once (the fallback's password, which has no exit).
    pub fn unlock(&mut self, token: UnlockToken) -> bool {
        if let Some(t) = self.session_lock.leaving.take().and_then(|l| l.timer) {
            self.handle.remove(t);
        }
        self.release(token)
    }

    /// [`Self::unlock`]'s release, after an exit too.
    fn release(&mut self, token: UnlockToken) -> bool {
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
        // An open spec is not a new request until it closes. (lock-polish)
        // After an exit too: one open and not leaving at the cap is one
        // logic has not closed yet (it is slow, or stopped) as often as
        // one asked for again before render showed it leaving, and the
        // two look the same here; locking again would ask for a password
        // just accepted, so it waits to close as on an unlock with no exit.
        // One seen leaving that opened again never gets here
        // ([`ExitStep::AskedAgain`]).
        self.session_lock.spent = self.lock_spec_open();
        self.lock_session_ended();
        if let Err(e) = self.conn.flush() {
            log::warn!("flushing the unlock failed: {e}");
        }
        self.host.lock_changed(LockState::Unlocked);
        true
    }

    /// (lock-polish) Releases the session lock once the lock has played
    /// its way out: the same token as [`Self::unlock`], held until every
    /// lock node shown with a two-way `open` has closed (logic writes it
    /// false at `Unlocking`, and render keeps it open while its exit
    /// plays) and spent then, or [`LOCK_EXIT_CAP`] after now at the latest
    /// (a timer of this manager's, so neither render nor logic can hold
    /// the session past it). The host hears `Unlocking` now and
    /// `Unlocked` then. With nothing that can play an exit (no such node
    /// shown, the lock still pending, no timer) it is [`Self::unlock`].
    /// A token while an exit plays already is dropped: it unlocks nothing
    /// more. Input on the lock goes nowhere meanwhile ([`Self::lock_leaving`]).
    pub fn unlock_after_exit(&mut self, token: UnlockToken) -> bool {
        if self.session_lock.leaving.is_some() {
            log::info!(
                "lock: a password accepted while the lock plays its way out changes nothing"
            );
            return true;
        }
        if !matches!(self.session_lock.phase, Phase::Locked(_)) {
            return self.unlock(token);
        }
        let waited: BTreeSet<NodeId> = self
            .session_lock
            .contents
            .values()
            .filter_map(|id| self.surfaces.get(id).map(|s| s.node))
            .filter(|n| {
                self.specs
                    .get(n)
                    .is_some_and(|s| s.open && !s.closing && s.open_two_way)
            })
            .collect();
        if waited.is_empty() {
            return self.unlock(token);
        }
        let timer = self.handle.insert_source(
            Timer::from_duration(LOCK_EXIT_CAP),
            |_, _, state: &mut State<H>| {
                if let Some(l) = &mut state.session_lock.leaving {
                    l.timer = None;
                }
                state.end_exit(true);
                TimeoutAction::Drop
            },
        );
        let timer = match timer {
            Ok(t) => t,
            Err(e) => {
                log::warn!(
                    "lock: no timer for the exit's cap ({}): unlocking now",
                    e.error
                );
                return self.unlock(token);
            }
        };
        log::info!(
            "lock: the password was accepted: the lock plays its way out (at most {} ms)",
            LOCK_EXIT_CAP.as_millis()
        );
        self.session_lock.leaving = Some(Leaving {
            token,
            waited,
            seen: BTreeSet::new(),
            timer: Some(timer),
            since: Instant::now(),
        });
        self.host.lock_changed(LockState::Unlocking);
        true
    }

    /// (lock-polish) The lock plays its way out ([`Self::unlock_after_exit`]):
    /// the session is locked, and input on it goes nowhere.
    pub fn lock_leaving(&self) -> bool {
        self.session_lock.leaving.is_some()
    }

    /// (lock-polish) The exit ended: every node it waited for closed, or
    /// (`cap`) [`LOCK_EXIT_CAP`] passed. The held token unlocks.
    fn end_exit(&mut self, cap: bool) {
        let Some(l) = &self.session_lock.leaving else {
            return;
        };
        let ms = l.since.elapsed().as_millis();
        if cap {
            log::warn!(
                "lock: the exit was still playing at its {} ms cap: unlocking",
                LOCK_EXIT_CAP.as_millis()
            );
        } else {
            log::info!("lock: the exit ended after {ms} ms: unlocking");
        }
        if let Some(l) = self.session_lock.leaving.take() {
            if let Some(t) = l.timer {
                self.handle.remove(t);
            }
            self.release(l.token);
        }
    }

    /// (lock-polish) A lock node that was leaving opened again (the lock
    /// asked for again as it played its way out): the session never
    /// unlocked, so it stays locked. The token is dropped (a password is
    /// typed again) and the host hears `Locked` again.
    fn exit_asked_again(&mut self) {
        let Some(l) = self.session_lock.leaving.take() else {
            return;
        };
        if let Some(t) = l.timer {
            self.handle.remove(t);
        }
        // The token goes unspent with `l`.
        log::info!("lock: asked for again while it played its way out: the session stays locked");
        self.host.lock_changed(LockState::Locked);
        self.sync_lock_surfaces();
    }

    /// (lock-polish) Follows an exit in progress after a lock spec changed
    /// or an output went ([`exit_step`]).
    pub(super) fn follow_exit(&mut self) {
        let shown: BTreeSet<NodeId> = self
            .session_lock
            .contents
            .values()
            .filter_map(|id| self.surfaces.get(id).map(|s| s.node))
            .collect();
        let specs = &self.specs;
        let Some(l) = &mut self.session_lock.leaving else {
            return;
        };
        let step = exit_step(&l.waited, &mut l.seen, |n| {
            let s = specs.get(&n).filter(|_| shown.contains(&n))?;
            Some((s.open, s.closing))
        });
        match step {
            ExitStep::Wait => {}
            ExitStep::Done => self.end_exit(false),
            ExitStep::AskedAgain => self.exit_asked_again(),
        }
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

    /// The specs of the lock's nodes.
    fn lock_specs(&self) -> impl Iterator<Item = &SurfaceSpec> {
        self.session_lock
            .nodes
            .iter()
            .filter_map(|n| self.specs.get(n))
    }

    /// Some `lock` spec is open (its instances' `open` is one value); one
    /// render holds open only for its exit (lock-polish) is closed.
    fn lock_spec_open(&self) -> bool {
        self.lock_specs().any(|s| s.open && !s.closing)
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
                self.follow_exit();
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
                self.follow_exit();
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

    /// The focused monitor's output, else the first in the layout
    /// ([`first_in_layout`]): where a `screens: focused` instance shows,
    /// and the content keys on a solid reach.
    fn lock_output(&self) -> Option<u32> {
        self.focused_output().or_else(|| {
            first_in_layout(self.outputs.keys().map(|g| {
                let monitor = self.monitors.id_of(*g).and_then(|id| self.monitors.get(id));
                (*g, monitor.map(|m| m.position))
            }))
        })
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
        if l.leaving.is_some() {
            // (lock-polish) While the lock plays its way out each content
            // stays where it is, its exit playing there, and an output
            // plugged in meanwhile shows the solid.
            return l
                .contents
                .iter()
                .filter(|(g, _)| self.outputs.contains_key(g))
                .filter_map(|(g, id)| Some((*g, self.surfaces.get(id)?.node)))
                .collect();
        }
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
            stale_callbacks: 0,
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
                // (lock-polish) The compositor ended the lock while it
                // played its way out, the password accepted: the exit
                // ends and the unlock goes now. `unlock_and_destroy` is
                // also the protocol's answer to `finished`, and no new
                // lock is asked for: it would ask for the password again.
                if matches!(self.session_lock.phase, Phase::Locked(_))
                    && let Some(l) = self.session_lock.leaving.take()
                {
                    log::warn!(
                        "the compositor ended the session lock as it played its way out: unlocked"
                    );
                    if let Some(t) = l.timer {
                        self.handle.remove(t);
                    }
                    self.release(l.token);
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

/// (screen-position) An output's global and its monitor's position
/// (`None`: no monitor known for it yet; `Some(None)`: a monitor whose
/// position is not known).
type OutputPlace = (u32, Option<Option<(i32, i32)>>);

/// (screen-position) The lock's first monitor while none is focused:
/// the output whose monitor is leftmost, by `x`, then `y` (an unknown
/// position counting as 0, 0), the lowest global at one place, an output
/// whose monitor is not known yet last. It is the order logic puts
/// `screens.all` in (`run::layout_order`), so the monitor a `screens:
/// focused` lock shows on is the one whose `screen` logic gave it.
fn first_in_layout(outputs: impl Iterator<Item = OutputPlace>) -> Option<u32> {
    outputs
        .min_by_key(|(g, monitor)| {
            let (x, y) = monitor.flatten().unwrap_or((0, 0));
            (monitor.is_none(), x, y, *g)
        })
        .map(|(g, _)| g)
}

#[cfg(test)]
mod tests {
    use super::{AfterFinished, ExitStep, OutputPlace, after_finished, exit_step, first_in_layout};
    use std::collections::{BTreeMap, BTreeSet};
    use strand_scene::NodeId;

    /// (lock-polish) An exit waits while a node leaves (or logic has yet
    /// to close it), ends when every node closed or went, and is asked
    /// for again when a node seen leaving is open again; a node never
    /// seen leaving that is open is only not closed yet.
    #[test]
    fn an_exit_ends_when_its_nodes_close_and_not_before() {
        let (a, b) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let waited: BTreeSet<NodeId> = [a, b].into();
        let step = |seen: &mut BTreeSet<NodeId>, specs: &[(NodeId, (bool, bool))]| {
            let specs: BTreeMap<NodeId, (bool, bool)> = specs.iter().copied().collect();
            exit_step(&waited, seen, |n| specs.get(&n).copied())
        };
        let mut seen = BTreeSet::new();
        // Logic has not closed them yet.
        assert_eq!(
            step(&mut seen, &[(a, (true, false)), (b, (true, false))]),
            ExitStep::Wait
        );
        assert!(seen.is_empty());
        // Both leaving.
        assert_eq!(
            step(&mut seen, &[(a, (true, true)), (b, (true, true))]),
            ExitStep::Wait
        );
        // One closed, one still leaving.
        assert_eq!(
            step(&mut seen, &[(a, (false, false)), (b, (true, true))]),
            ExitStep::Wait
        );
        // The other closed: done.
        assert_eq!(
            step(&mut seen, &[(a, (false, false)), (b, (false, false))]),
            ExitStep::Done
        );
        // A node that went (its spec, or its output) is done too.
        let mut seen = BTreeSet::new();
        assert_eq!(step(&mut seen, &[(b, (false, false))]), ExitStep::Done);
        // Seen leaving, then open and not leaving: asked for again.
        let mut seen = BTreeSet::new();
        assert_eq!(
            step(&mut seen, &[(a, (true, true)), (b, (true, true))]),
            ExitStep::Wait
        );
        assert_eq!(
            step(&mut seen, &[(a, (true, false)), (b, (true, true))]),
            ExitStep::AskedAgain
        );
    }

    /// (screen-position) The first monitor is the leftmost, not the
    /// first advertised: then the topmost at one `x`, the lowest global
    /// at one place, and an output with no monitor known yet only when
    /// nothing else is.
    #[test]
    fn the_first_monitor_is_the_leftmost() {
        let first = |o: &[OutputPlace]| first_in_layout(o.iter().copied());
        assert_eq!(
            first(&[(10, Some(Some((1920, 0)))), (11, Some(Some((0, 0))))]),
            Some(11)
        );
        assert_eq!(
            first(&[(10, Some(Some((0, 0)))), (11, Some(Some((-2560, 0))))]),
            Some(11)
        );
        assert_eq!(
            first(&[(10, Some(Some((0, 1080)))), (11, Some(Some((0, -1440))))]),
            Some(11)
        );
        // `x` first: a monitor raised to the right stays after its left
        // neighbour.
        assert_eq!(
            first(&[(10, Some(Some((1920, -200)))), (11, Some(Some((0, 0))))]),
            Some(11)
        );
        assert_eq!(first(&[(10, Some(None)), (11, Some(None))]), Some(10));
        assert_eq!(first(&[(10, None), (11, Some(Some((1920, 0))))]), Some(11));
        assert_eq!(first(&[]), None);
    }

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
