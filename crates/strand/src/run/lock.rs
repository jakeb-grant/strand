//! The session lock in the binary (design.md, "Lock screen": "The lock
//! is exempt from reload and fails closed: if anything faults, a built-in
//! password field appears"; docs/architecture.md, "The lock";
//! decisions.md, m4-lock-w1 and m4-lock-w2).
//!
//! - **Wiring** ([`Guard::wire`]): `auth`'s accepted passwords reach
//!   `State::unlock` over a channel into the main loop, and only then is
//!   `State::enable_session_lock` called, in the same function: the
//!   binary never takes a lock nothing releases. What the compositor
//!   reports (`SurfaceHost::lock_changed`) goes to logic as
//!   [`ToLogic::LockState`], which `Instance::set_session_lock` takes.
//! - **The fallback** ([`LockScreen`], held by the surface host): while a
//!   lock is asked for or held, the main thread paints render's built-in
//!   password field (`strand_render::lock_fallback`) on the lock's
//!   content surface, and takes its keys, once anything faults: logic
//!   ended or panicked; logic stopped answering the watchdog; SIGINT or
//!   SIGTERM (logic is told to stop, the run ends after the unlock); the
//!   lock drew no first frame within 1 s; no lock is mounted (the lock
//!   was asked for with no `lock` compiled, or its node went); a runtime
//!   fault inside the lock froze its component; the text worker died;
//!   `auth` could not check a password; or the compositor ended a lock it
//!   held. The fallback checks passwords with a `strand_auth::Client` of
//!   its own on a thread of its own, and its token unlocks like `auth`'s.
//!   Once shown it stays until the unlock.
//! - **Restart**: while the compositor says the session is locked, a
//!   marker (`$XDG_RUNTIME_DIR/strand-<display>.locked`) says so; a
//!   strand started while it is there locks at once with the fallback,
//!   so a strand killed while locked (`kill -9`, an allocation failure)
//!   and started again puts a password field back on the session the
//!   compositor kept locked. It goes once no lock is asked for or held
//!   (an unlock, or a lock the compositor refused).
//! - **Reloads**: a load deferred while the lock is shown is committed
//!   after the unlock ([`Shell::unlocked`]).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use strand_auth::{AuthError, Client, Password, UnlockToken, Verdict};
use strand_compiler::instantiate::SessionLock;
use strand_render::TextBackend;
use strand_render::lock_fallback::{Action, FieldState, LockFallback};
use strand_scene::{
    Color, Damage, InputEvent, NodeKind, Paint, PaintTarget, Prop, PropValue, SurfaceId, TokenScope,
};
use strand_surface::{LOCK_FALLBACK_NODE, LockState, State};

use super::shell::Shell;
use super::*;

/// How long a lock may go without a committed first frame of its
/// content before the fallback shows (architecture.md: "a lock with no
/// first frame within 1 s").
pub(crate) const FIRST_FRAME: Duration = Duration::from_secs(1);

/// While a lock is shown, a heartbeat goes to logic this long after the
/// last one was answered.
pub(crate) const BEAT_EVERY: Duration = Duration::from_secs(1);

/// A heartbeat unanswered this long means logic stopped (the watchdog).
pub(crate) const WATCHDOG: Duration = Duration::from_secs(3);

/// The colour of the solid lock surfaces while the fallback shows (its
/// own background).
const FALLBACK_SOLID: Color = Color::new(
    0x11 as f32 / 255.0,
    0x11 as f32 / 255.0,
    0x1b as f32 / 255.0,
    1.0,
);

/// The last heartbeat logic took ([`ToLogic::Beat`]).
static BEAT: AtomicU64 = AtomicU64::new(0);

/// Nodes of the runtime faults logic met while a lock was shown, for the
/// main thread to match against the lock's subtree.
static FAULTS: Mutex<Vec<NodeId>> = Mutex::new(Vec::new());

/// strand-surface's report as the compiler's (the compiler does not
/// depend on strand-surface).
pub(crate) fn session_lock(state: LockState) -> SessionLock {
    match state {
        LockState::Locked => SessionLock::Locked,
        LockState::Finished => SessionLock::Finished,
        LockState::Unlocked => SessionLock::Unlocked,
    }
}

// ---- the logic thread ------------------------------------------------------

/// Logic took heartbeat `seq`.
pub(super) fn beat(seq: u64) {
    BEAT.store(seq, Ordering::Release);
}

impl Shell {
    /// The compositor's report on the session lock.
    pub(super) fn lock_state(&mut self, state: SessionLock) {
        if state == SessionLock::Locked {
            faults::logic_locked();
        }
        self.inst.set_session_lock(state);
    }

    /// A step's runtime faults, while a lock is shown: their nodes go to
    /// the main thread, which shows the fallback for one inside the lock
    /// (its component is frozen, so its password field may be dead).
    pub(super) fn lock_faults(&self, errors: &[strand_compiler::instantiate::RuntimeError]) {
        if errors.is_empty() || !self.inst.lock_shown() {
            return;
        }
        if let Ok(mut f) = FAULTS.lock() {
            f.extend(errors.iter().filter_map(|e| e.node));
        }
    }

    /// After the unlock: the deferred load (or a hard reload still
    /// owed), committed now.
    pub(super) fn unlocked(&mut self) {
        if self.inst.lock_shown() {
            return;
        }
        if let Some(mut l) = self.deferred.take() {
            l.hard |= std::mem::take(&mut self.deferred_hard);
            // Saves after it may have broken the config again (held back,
            // the overlay up): the replay shows the newest attempt's
            // problems, not the ones it had when it was deferred.
            self.latest.onto(&mut l.outcome);
            self.apply(l, true);
        } else if std::mem::take(&mut self.deferred_hard) {
            let now = Instant::now();
            let mut l = Loaded {
                outcome: Outcome::default(),
                requested: true,
                clients: Vec::new(),
                hard: true,
                files: Vec::new(),
                saved: None,
                started: now,
                notices: Vec::new(),
            };
            // Its event reports the newest attempt's problems (the
            // overlay already lists them).
            self.latest.onto(&mut l.outcome);
            self.apply(Box::new(l), false);
        }
    }
}

// ---- the logic thread: password values kept out of logs and `strand watch` ---

/// What a redacted password value reads as.
pub(super) const REDACTED: &str = "<redacted>";

/// The values of the config's `type: password` inputs, as logic sees
/// them (its diffs, and the `Router`'s edits written back), so the
/// binary can redact them from the runtime fault messages it logs and
/// streams to `strand watch` (architecture.md, "The lock": "its value
/// is redacted by the binary in `strand watch`, logs"). An expression
/// that fails on a value read from the password's `state`
/// (`clock.format(secret)`) would otherwise print the password. Each
/// value is a [`Password`], zeroized when replaced or dropped.
#[derive(Default)]
pub(super) struct Secrets {
    inputs: std::collections::HashMap<NodeId, Option<Password>>,
}

impl Secrets {
    /// A diff logic sends: inputs that became (or stopped being)
    /// `type: password`, removed nodes, and the password inputs' text.
    pub(super) fn see_diff(&mut self, diff: &SceneDiff) {
        for op in &diff.ops {
            match op {
                SceneOp::SetProp {
                    id,
                    prop: Prop::InputType,
                    value,
                    ..
                } => {
                    if matches!(value, PropValue::Keyword(k) if k == "password") {
                        self.inputs.entry(*id).or_default();
                    } else {
                        self.inputs.remove(id);
                    }
                }
                SceneOp::Remove { id, .. } => {
                    self.inputs.remove(id);
                }
                _ => {}
            }
        }
        for op in &diff.ops {
            if let SceneOp::SetProp {
                id, prop, value, ..
            } = op
            {
                self.see_write(*id, *prop, value);
            }
        }
    }

    /// A write to `node`'s `prop` (the `Router` editing an input).
    pub(super) fn see_write(&mut self, node: NodeId, prop: Prop, value: &PropValue) {
        if prop != Prop::Text {
            return;
        }
        if let (Some(slot), PropValue::Text(t)) = (self.inputs.get_mut(&node), value) {
            *slot = (!t.is_empty()).then(|| Password::from(t.clone()));
        }
    }

    /// A runtime fault's message as it may be logged and streamed:
    /// `what` (a prop, handler or state name, never a value), then the
    /// error through [`Secrets::redact_error`].
    pub(super) fn redact_fault(&self, e: &strand_compiler::instantiate::RuntimeError) -> String {
        format!("{}: {}", e.what, self.redact_error(&e.error))
    }

    /// An error's message as it may be logged and streamed. While any
    /// `type: password` input is mounted, an error that can carry values
    /// (a handler's or builtin's own message, a keyed collection's key)
    /// is replaced whole by [`REDACTED`]: the value may be in it
    /// transformed (`secret.upper()`, a slice, an escaped quote) or
    /// copied to another state before the field was emptied, where no
    /// search for the current value finds it. The errors that carry only
    /// node ids and names, and every error while no password input is
    /// mounted, keep their text with current values replaced
    /// ([`Secrets::redact`]).
    pub(super) fn redact_error(&self, e: &strand_core::Error) -> std::borrow::Cow<'static, str> {
        use strand_core::Error as E;
        let names_only = matches!(
            e,
            E::Disposed(_)
                | E::TypeMismatch(_)
                | E::Cycle(_)
                | E::WriteInDerived { .. }
                | E::Reentrant
                | E::Cancelled
        );
        if !names_only && !self.inputs.is_empty() {
            return std::borrow::Cow::Borrowed(REDACTED);
        }
        std::borrow::Cow::Owned(self.redact(&e.to_string()).into_owned())
    }

    /// `text` with every password input's current value replaced by
    /// [`REDACTED`].
    pub(super) fn redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        let mut out = std::borrow::Cow::Borrowed(text);
        for p in self.inputs.values().flatten() {
            if let Ok(v) = std::str::from_utf8(p.as_bytes())
                && !v.is_empty()
                && out.contains(v)
            {
                out = std::borrow::Cow::Owned(out.replace(v, REDACTED));
            }
        }
        out
    }
}

// ---- the main thread: the surface host's part --------------------------------

/// What reaches the main loop about the lock from other threads.
pub(crate) enum LockMsg {
    /// `auth` accepted a password.
    Token(UnlockToken),
    /// `auth` could not check a password (never a refusal).
    AuthFailed(String),
    /// The fallback's own check answered.
    Checked(Verdict),
}

/// The fallback while it is shown.
struct Shown {
    field: LockFallback,
    /// Repaint the content surface.
    dirty: bool,
}

/// The fallback's password checks: a thread holding its own
/// `strand_auth::Client` (one helper for the lock session), answering
/// on the main loop's channel.
struct Checker {
    tx: std::sync::mpsc::Sender<Password>,
}

impl Checker {
    fn spawn(reply: Sender<LockMsg>) -> io::Result<Checker> {
        let (tx, rx) = std::sync::mpsc::channel::<Password>();
        std::thread::Builder::new()
            .name("strand-lock-auth".into())
            .spawn(move || {
                let mut client = strand_auth::default_helper()
                    .map(|h| Client::new(h, strand_services::child::restore_in_child));
                for password in rx {
                    let verdict = match client.as_mut() {
                        Some(c) => c.submit(password),
                        None => Verdict::Failed(AuthError::Spawn(io::Error::new(
                            io::ErrorKind::NotFound,
                            "no `strand-auth` helper is installed",
                        ))),
                    };
                    if reply.send(LockMsg::Checked(verdict)).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Checker { tx })
    }
}

/// The lock as the surface host sees it: the content surface, whether it
/// drew, and the fallback when it shows (see the module docs).
#[derive(Default)]
pub(crate) struct LockScreen {
    /// The lock's content surface and the node it shows (a `lock`'s, or
    /// [`LOCK_FALLBACK_NODE`]). strand-surface's `State::lock_content`
    /// decides which surface it is ([`LockScreen::sync_content`], each
    /// main-loop turn); `attached` only takes it early when render's
    /// tree already says so.
    content: Option<(SurfaceId, NodeId)>,
    /// The node each attached surface shows, so the content named by
    /// `State::lock_content` can be paired with its node even after
    /// that node left render's tree (the spec gone while locked: a
    /// reload or SIGTERM unmounting the `lock`).
    nodes: std::collections::HashMap<SurfaceId, NodeId>,
    /// The content committed a frame with damage in this lock session
    /// (a content surface moved to another output does not start the
    /// first-frame deadline again).
    drew: bool,
    /// When the main loop first saw this lock asked for or held (the
    /// first-frame deadline's start): the first frame's time is logged.
    asked: Option<Instant>,
    fallback: Option<Shown>,
    /// Why the fallback must show, from an event: `auth` failing, the
    /// compositor ending a held lock.
    pending: Option<String>,
    /// The compositor's last report.
    last: Option<LockState>,
    reply: Option<Sender<LockMsg>>,
    checker: Option<Checker>,
}

impl std::fmt::Debug for LockScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockScreen")
            .field("content", &self.content)
            .field("drew", &self.drew)
            .field("fallback", &self.fallback.is_some())
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

impl LockScreen {
    /// The fallback shows.
    pub(crate) fn fallback_shown(&self) -> bool {
        self.fallback.is_some()
    }

    fn on_content(&self, surface: SurfaceId) -> bool {
        self.content.is_some_and(|(s, _)| s == surface)
    }

    /// The fallback is painted on `surface`.
    pub(crate) fn shown_on(&self, surface: SurfaceId) -> bool {
        self.fallback.is_some() && self.on_content(surface)
    }

    /// A surface was attached. It is taken as the lock's content at once
    /// when it shows the fallback node or a node render's tree calls a
    /// `lock` (`kind`); otherwise [`LockScreen::sync_content`] decides on
    /// the main loop's next turn, from strand-surface's own answer.
    pub(crate) fn attached(&mut self, surface: SurfaceId, node: NodeId, kind: Option<NodeKind>) {
        self.nodes.insert(surface, node);
        if node == LOCK_FALLBACK_NODE || kind == Some(NodeKind::Lock) {
            self.set_content(Some((surface, node)));
        }
    }

    pub(crate) fn detached(&mut self, surface: SurfaceId) {
        self.nodes.remove(&surface);
        if self.on_content(surface) {
            self.content = None;
        }
    }

    /// The lock's content as strand-surface made it (`State::lock_content`):
    /// the only answer that holds when render's tree no longer knows the
    /// node the content was made with (the `lock` unmounted while no
    /// output existed, then an output came back).
    pub(crate) fn sync_content(&mut self, surface: Option<SurfaceId>) {
        let content =
            surface.map(|s| (s, self.nodes.get(&s).copied().unwrap_or(LOCK_FALLBACK_NODE)));
        if content != self.content {
            self.set_content(content);
        }
    }

    fn set_content(&mut self, content: Option<(SurfaceId, NodeId)>) {
        self.content = content;
        if content.is_some()
            && let Some(f) = &mut self.fallback
        {
            f.dirty = true;
        }
    }

    /// The content was configured (a new size): the fallback repaints.
    pub(crate) fn configured(&mut self, surface: SurfaceId) {
        if self.on_content(surface)
            && let Some(f) = &mut self.fallback
        {
            f.dirty = true;
        }
    }

    /// The fallback's frame of `surface`, or `None` for the renderer to
    /// paint it.
    pub(crate) fn paint(
        &mut self,
        surface: SurfaceId,
        target: &mut PaintTarget<'_>,
    ) -> Option<Damage> {
        if !self.on_content(surface) {
            return None;
        }
        let Some(f) = &mut self.fallback else {
            if faults::no_first_frame() {
                // The lock's own frame never comes.
                return Some(Damage::new());
            }
            return None;
        };
        if !std::mem::take(&mut f.dirty) {
            return Some(Damage::new());
        }
        Some(f.field.paint(target))
    }

    /// The renderer painted `surface` with `damage`.
    pub(crate) fn painted(&mut self, surface: SurfaceId, damage: &Damage) {
        if self.on_content(surface) && !damage.is_empty() {
            if !self.drew
                && let Some(at) = self.asked
            {
                // The VM's first-frame gate reads this.
                log::info!("lock: first frame after {} ms", at.elapsed().as_millis());
            }
            self.drew = true;
        }
    }

    /// Whether `surface` wants a frame, when the fallback decides.
    pub(crate) fn wants_frame(&self, surface: SurfaceId) -> Option<bool> {
        if !self.on_content(surface) {
            return None;
        }
        match &self.fallback {
            Some(f) => Some(f.dirty),
            None => faults::no_first_frame().then_some(false),
        }
    }

    /// Input while the fallback shows: keys on the lock's content go to
    /// the fallback, and nothing on it reaches the `Router`. True when it
    /// took the event (the caller wakes the main loop to repaint).
    pub(crate) fn input(&mut self, event: &InputEvent) -> bool {
        if !self.shown_on(event.surface()) {
            return false;
        }
        let InputEvent::Key { key, .. } = event else {
            return true;
        };
        let Some(f) = &mut self.fallback else {
            return true;
        };
        match f.field.key(key) {
            Action::None => {}
            Action::Changed => f.dirty = true,
            Action::Submit(bytes) => {
                f.dirty = true;
                let password = Password::from_bytes(bytes);
                if self.checker.is_none()
                    && let Some(reply) = self.reply.clone()
                {
                    match Checker::spawn(reply) {
                        Ok(c) => self.checker = Some(c),
                        Err(e) => log::error!("lock: the password check could not start: {e}"),
                    }
                }
                let sent = self
                    .checker
                    .as_ref()
                    .is_some_and(|c| c.tx.send(password).is_ok());
                if !sent {
                    self.checker = None;
                    f.field.set_state(FieldState::Failed);
                }
            }
        }
        true
    }

    /// The fallback's check answered: a token to unlock with, or the
    /// field shows the refusal.
    pub(crate) fn checked(&mut self, verdict: Verdict) -> Option<UnlockToken> {
        let f = self.fallback.as_mut()?;
        f.dirty = true;
        match verdict {
            Verdict::Unlocked(token) => {
                f.field.set_state(FieldState::Idle);
                Some(token)
            }
            Verdict::Denied { message } => {
                if let Some(m) = message {
                    log::info!("lock: refused: {m}");
                }
                f.field.set_state(FieldState::Failed);
                None
            }
            Verdict::Failed(e) => {
                log::warn!("lock: the password could not be checked: {e}");
                f.field.set_state(FieldState::Failed);
                None
            }
        }
    }

    /// `auth` could not check a password. Kept only while a lock is
    /// asked for or held: [`Guard::check`] drops it otherwise
    /// ([`LockScreen::idle`]), so a check that failed while unlocked
    /// never shows the fallback on the next, healthy lock.
    pub(crate) fn auth_failed(&mut self, why: String) {
        if self.fallback.is_none() && self.pending.is_none() {
            self.pending = Some(format!("`auth` failed ({why})"));
        }
    }

    /// The compositor's report.
    pub(crate) fn changed(&mut self, state: LockState) {
        faults::set_locked(state == LockState::Locked);
        if state == LockState::Finished && self.last == Some(LockState::Locked) {
            // It ended a lock it held, and the session may still be
            // locked: the manager asks for a new one, which shows the
            // field that depends on least.
            self.pending = Some("the compositor ended the lock".into());
        }
        if state == LockState::Unlocked {
            self.reset();
        }
        self.last = Some(state);
    }

    fn show(&mut self, reason: &str) {
        log::warn!("lock: showing the built-in password field: {reason}");
        self.pending = None;
        self.fallback = Some(Shown {
            field: LockFallback::new(),
            dirty: true,
        });
    }

    /// No lock is asked for or held: a reason to show the fallback has
    /// nothing to show it on, and must not wait for the next lock.
    fn idle(&mut self) {
        self.pending = None;
    }

    /// No lock any more: the fallback and its reasons go.
    fn reset(&mut self) {
        self.fallback = None;
        self.pending = None;
        self.drew = false;
        self.asked = None;
        // The helper goes with the lock session.
        self.checker = None;
    }
}

// ---- the main thread: the main loop's part -----------------------------------

/// The main loop's side of the lock: the wiring, the fault checks and the
/// watchdog.
pub(super) struct Guard {
    /// When the current lock was first seen asked for or held.
    since: Option<Instant>,
    /// Heartbeats sent.
    seq: u64,
    /// The heartbeat waiting for its answer, and when it went.
    waiting: Option<(u64, Instant)>,
    /// When the next heartbeat goes.
    next_beat: Option<Instant>,
    /// A signal came while locked: logic was told to stop.
    stopping: bool,
    /// The restart marker, and whether it is written.
    marker: Option<PathBuf>,
    marked: bool,
}

/// `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.locked`.
fn marker_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    if !dir.is_absolute() {
        return None;
    }
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
    let name = display
        .rsplit('/')
        .next()
        .unwrap_or("wayland-0")
        .to_string();
    Some(dir.join(format!("strand-{name}.locked")))
}

impl Guard {
    /// Wires the lock: `auth`'s tokens and failures to this loop, then
    /// `enable_session_lock` (never one without the other), then a lock
    /// with the fallback at once if a strand before this one was killed
    /// while the session was locked.
    pub(super) fn wire(
        handle: &calloop::LoopHandle<'static, State<Host>>,
        state: &mut State<Host>,
    ) -> Result<Guard, DemoError> {
        let (tx, rx) = calloop::channel::channel::<LockMsg>();
        handle
            .insert_source(rx, |event, _, state| {
                let Event::Msg(msg) = event else {
                    return;
                };
                let token = match msg {
                    LockMsg::Token(t) => Some(t),
                    LockMsg::AuthFailed(why) => {
                        state.host_mut().lock.auth_failed(why);
                        None
                    }
                    LockMsg::Checked(v) => state.host_mut().lock.checked(v),
                };
                if let Some(t) = token
                    && !state.unlock(t)
                {
                    log::warn!("lock: a password was accepted with no lock to release");
                }
                state.poll();
            })
            .map_err(|e| DemoError::Io(io::Error::other(e.error)))?;
        state.host_mut().lock.reply = Some(tx.clone());
        let tokens = Mutex::new(tx.clone());
        let failures = Mutex::new(tx);
        let mut config = strand_services::auth::AuthConfig {
            sink: Some(Arc::new(move |token| {
                if let Ok(t) = tokens.lock() {
                    let _ = t.send(LockMsg::Token(token));
                }
            })),
            failed: Some(Arc::new(move |why: &str| {
                if let Ok(t) = failures.lock() {
                    let _ = t.send(LockMsg::AuthFailed(why.to_string()));
                }
            })),
            ..Default::default()
        };
        faults::auth_config(&mut config);
        strand_services::auth::configure(Some(config));
        // Only now: a token reaches `unlock`.
        state.enable_session_lock();
        let mut guard = Guard {
            since: None,
            seq: 0,
            waiting: None,
            next_beat: None,
            stopping: false,
            marker: marker_path(),
            marked: false,
        };
        if let Some(m) = &guard.marker
            && m.exists()
        {
            log::warn!(
                "lock: strand stopped while the session was locked ({}): locking again",
                m.display()
            );
            guard.marked = true;
            match state.lock() {
                Ok(()) => {}
                Err(e @ strand_surface::LockError::Unsupported) => {
                    log::warn!("{e}");
                    let _ = std::fs::remove_file(m);
                    guard.marked = false;
                }
                Err(e) => log::warn!("{e}"),
            }
        }
        Ok(guard)
    }

    /// A lock is asked for or held: the run outlives logic and signals.
    pub(super) fn holds(&self, state: &State<Host>) -> bool {
        state.lock_active()
    }

    /// Checks the lock's health once per main-loop turn (see the module
    /// docs) and shows the fallback on the first fault.
    pub(super) fn check(
        &mut self,
        state: &mut State<Host>,
        now: Instant,
        logic_gone: bool,
        signalled: bool,
        to_logic: &Sender<ToLogic>,
    ) {
        self.keep_marker(state);
        faults::unmount_lock(state);
        let content = state.lock_content();
        state.host_mut().lock.sync_content(content);
        if !state.lock_active() {
            if self.since.take().is_some() {
                state.host_mut().lock.reset();
            }
            state.host_mut().lock.idle();
            self.waiting = None;
            self.next_beat = None;
            if let Ok(mut f) = FAULTS.lock() {
                f.clear();
            }
            return;
        }
        let since = *self.since.get_or_insert(now);
        state.host_mut().lock.asked.get_or_insert(since);
        if signalled && !self.stopping {
            self.stopping = true;
            log::info!("asked to stop while locked: the run ends after the unlock");
            let _ = to_logic.send(ToLogic::Shutdown);
        }
        if state.host().lock.fallback_shown() {
            return;
        }
        let reason = self
            .reason(state, now, since, logic_gone)
            .or_else(|| state.host_mut().lock.pending.take());
        if let Some(why) = reason {
            state.host_mut().lock.show(&why);
            state.set_lock_color(FALLBACK_SOLID);
            state.poll();
            return;
        }
        // The other outputs show the lock's colour (the fallback's own,
        // set above, only while it shows).
        let color = state.host().lock.content.map_or(Color::BLACK, |(_, node)| {
            lock_color(state.host().renderer.tree(), node)
        });
        state.set_lock_color(color);
        // The heartbeat.
        match self.waiting {
            Some((seq, _)) if BEAT.load(Ordering::Acquire) >= seq => {
                self.waiting = None;
                self.next_beat = Some(now + BEAT_EVERY);
            }
            Some(_) => {}
            None if self.next_beat.is_none_or(|t| now >= t) => {
                self.seq += 1;
                if to_logic.send(ToLogic::Beat(self.seq)).is_ok() {
                    self.waiting = Some((self.seq, now));
                }
            }
            None => {}
        }
    }

    fn reason(
        &mut self,
        state: &State<Host>,
        now: Instant,
        since: Instant,
        logic_gone: bool,
    ) -> Option<String> {
        if logic_gone {
            return Some("the shell's logic ended".into());
        }
        if self.stopping {
            return Some("strand was asked to stop".into());
        }
        if let Some((_, at)) = self.waiting
            && now.saturating_duration_since(at) > WATCHDOG
        {
            return Some(format!(
                "the shell's logic did not answer for {} s",
                WATCHDOG.as_secs()
            ));
        }
        let host = state.host();
        let screen = &host.lock;
        if let Some((_, node)) = screen.content {
            if node == LOCK_FALLBACK_NODE {
                return Some("the session was locked with no `lock` open".into());
            }
            let tree = host.renderer.tree();
            if tree.get(node).is_none() {
                return Some("the lock is no longer mounted".into());
            }
            let faults = FAULTS.lock().map(|mut f| std::mem::take(&mut *f));
            for n in faults.unwrap_or_default() {
                if inside(tree, n, node) {
                    return Some("a runtime fault froze the lock".into());
                }
            }
        }
        if let TextBackend::Worker(w) = host.renderer.text()
            && !w.is_running()
        {
            return Some("the text worker stopped".into());
        }
        if !screen.drew && now.saturating_duration_since(since) >= FIRST_FRAME {
            return Some(format!(
                "the lock drew no first frame within {} ms",
                FIRST_FRAME.as_millis()
            ));
        }
        None
    }

    /// The marker follows the compositor ([`marker_step`]).
    fn keep_marker(&mut self, state: &State<Host>) {
        let Some(m) = &self.marker else {
            return;
        };
        match marker_step(self.marked, state.is_locked(), state.lock_active()) {
            Some(Marker::Write) => {
                if let Err(e) = std::fs::write(m, b"locked\n") {
                    log::warn!("lock: {}: {e}", m.display());
                }
                self.marked = true;
            }
            Some(Marker::Remove) => {
                let _ = std::fs::remove_file(m);
                self.marked = false;
            }
            None => {}
        }
    }

    /// When the main loop must look again: the first-frame deadline, the
    /// next heartbeat or the watchdog.
    pub(super) fn wait(&self, now: Instant, state: &State<Host>) -> Option<Duration> {
        let since = self.since?;
        if state.host().lock.fallback_shown() {
            return None;
        }
        let mut at: Vec<Instant> = Vec::new();
        if !state.host().lock.drew {
            at.push(since + FIRST_FRAME);
        }
        match self.waiting {
            // Its answer is looked for a beat after it went; past that,
            // the watchdog fires just after its limit.
            Some((_, sent)) => {
                let look = sent + BEAT_EVERY;
                at.push(if look > now {
                    look
                } else {
                    sent + WATCHDOG + Duration::from_millis(10)
                });
            }
            None => at.extend(self.next_beat),
        }
        at.into_iter()
            .min()
            .map(|t| t.saturating_duration_since(now))
    }
}

/// The colour of the lock's other outputs while the config's lock shows
/// (architecture.md: "a single-pixel background in the lock's colour"):
/// the `lock` node's `bg`, its tokens resolved; a gradient's first stop;
/// black when it has none.
fn lock_color(tree: &strand_render::SceneTree, node: NodeId) -> Color {
    let Some(n) = tree.get(node) else {
        return Color::BLACK;
    };
    let mut levels = vec![&tree.tokens];
    if let Some(PropValue::Tokens(t)) = n.get(Prop::Tokens) {
        levels.push(t);
    }
    let scope = TokenScope::new(&levels);
    let bg = n.get(Prop::Bg).and_then(|v| scope.resolve(v));
    match bg.as_deref() {
        Some(PropValue::Color(c) | PropValue::Paint(Paint::Solid(c))) => *c,
        Some(PropValue::Paint(
            Paint::Linear { stops, .. } | Paint::Radial { stops } | Paint::Conic { stops, .. },
        )) => stops.first().map_or(Color::BLACK, |s| s.color),
        _ => Color::BLACK,
    }
}

/// What the restart marker needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Write,
    Remove,
}

/// The restart marker is written once the compositor says `locked`, and
/// removed once no lock is asked for or held: after an unlock, and also
/// after a lock the compositor refused (a restart's lock again, or the
/// one asked for after `finished`), since a strand killed with no lock
/// leaves nothing locked to come back to. A marker kept then would lock
/// the session on every later start.
fn marker_step(marked: bool, locked: bool, active: bool) -> Option<Marker> {
    if locked && !marked {
        Some(Marker::Write)
    } else if marked && !active {
        Some(Marker::Remove)
    } else {
        None
    }
}

/// `node` is `root` or one of its descendants in `tree`.
fn inside(tree: &strand_render::SceneTree, node: NodeId, root: NodeId) -> bool {
    let mut at = Some(node);
    for _ in 0..4096 {
        match at {
            Some(n) if n == root => return true,
            Some(n) => at = tree.get(n).and_then(|x| x.parent),
            None => return false,
        }
    }
    false
}

// ---- fault injection (the `faults` feature) ----------------------------------

/// `STRAND_FAULT` points for the lock VM's fault matrix (tests/lock.rs).
/// Compiled only with the `faults` feature; without it every function
/// is an empty inline no-op and the strings are not in the binary.
#[cfg(feature = "faults")]
pub(crate) mod faults {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// The session is locked (the text worker's fault fires only then).
    static LOCKED: AtomicBool = AtomicBool::new(false);

    fn value() -> String {
        std::env::var("STRAND_FAULT").unwrap_or_default()
    }

    /// `STRAND_FAULT` (comma-separated) names `fault`.
    pub(crate) fn on(fault: &str) -> bool {
        value().split(',').any(|f| f.trim() == fault)
    }

    pub(crate) fn set_locked(locked: bool) {
        LOCKED.store(locked, Ordering::Release);
    }

    /// Logic heard `Locked`: `logic_panic` panics the logic thread,
    /// `logic_hang` stops it for good.
    pub(crate) fn logic_locked() {
        if on("logic_panic") {
            panic!("STRAND_FAULT logic_panic");
        }
        if on("logic_hang") {
            log::warn!("STRAND_FAULT logic_hang");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    /// The text worker's waker (on its thread): `text_panic` kills the
    /// worker once the session is locked.
    pub(crate) fn text_waker() {
        if LOCKED.load(Ordering::Acquire) && on("text_panic") {
            panic!("STRAND_FAULT text_panic");
        }
    }

    /// The lock's own frame never comes (`lock_no_frame`).
    pub(crate) fn no_first_frame() -> bool {
        on("lock_no_frame")
    }

    /// The lock leaves render's tree once it drew while locked
    /// (`lock_unmount`), as logic removing it would: strand-surface keeps
    /// the lock and its content's node, which render no longer knows.
    ///
    /// `lock_unmount_unseen` removes it instead while a lock is asked for
    /// or held with no content surface yet (no output: a reload while
    /// pending), so the content strand-surface makes once an output comes
    /// is on a node this lock session never attached and render does
    /// not know.
    pub(crate) fn unmount_lock(state: &mut super::State<super::Host>) {
        static DONE: AtomicBool = AtomicBool::new(false);
        if state.lock_active()
            && state.lock_content().is_none()
            && !DONE.load(Ordering::Acquire)
            && on("lock_unmount_unseen")
        {
            let tree = state.host().renderer.tree();
            let lock = tree.roots().iter().copied().find(|n| {
                tree.get(*n)
                    .is_some_and(|n| n.kind == super::NodeKind::Lock)
            });
            if let Some(node) = lock {
                DONE.store(true, Ordering::Release);
                log::warn!("STRAND_FAULT lock_unmount_unseen");
                let mut diff = super::SceneDiff::new();
                diff.push(super::SceneOp::Remove {
                    id: node,
                    window: false,
                });
                super::apply(state, diff);
            }
            return;
        }
        if !state.is_locked() || DONE.load(Ordering::Acquire) || !on("lock_unmount") {
            return;
        }
        let screen = &state.host().lock;
        let Some((_, node)) = screen.content else {
            return;
        };
        if node == super::LOCK_FALLBACK_NODE || !screen.drew {
            return;
        }
        DONE.store(true, Ordering::Release);
        log::warn!("STRAND_FAULT lock_unmount");
        let mut diff = super::SceneDiff::new();
        diff.push(super::SceneOp::Remove {
            id: node,
            window: false,
        });
        super::apply(state, diff);
    }

    /// `auth`'s helper gets `STRAND_FAULT` (`auth_crash`, `auth_hang`,
    /// `auth_garbage`: strand-auth's own points), a 3 s timeout under
    /// `auth_hang`, and `auth_missing` points it at no helper at all.
    /// The fallback's own client gets none of it.
    pub(crate) fn auth_config(config: &mut strand_services::auth::AuthConfig) {
        let env = value();
        if env.is_empty() {
            return;
        }
        if on("auth_missing") {
            config.helper = Some("/nonexistent/strand-auth".into());
        }
        if on("auth_hang") {
            config.timeout = Duration::from_secs(3);
        }
        config.client_hook = Some(Arc::new(move |c: strand_auth::Client| {
            c.with_test_env(&[("STRAND_FAULT", env.as_str())])
        }));
    }
}

#[cfg(not(feature = "faults"))]
pub(crate) mod faults {
    #[inline(always)]
    pub(crate) fn set_locked(_: bool) {}
    #[inline(always)]
    pub(crate) fn logic_locked() {}
    #[inline(always)]
    pub(crate) fn text_waker() {}
    #[inline(always)]
    pub(crate) fn no_first_frame() -> bool {
        false
    }
    #[inline(always)]
    pub(crate) fn unmount_lock(_: &mut super::State<super::Host>) {}
    #[inline(always)]
    pub(crate) fn auth_config(_: &mut strand_services::auth::AuthConfig) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{ButtonState, KeyInput, Modifiers, Scale, Size};
    use strand_surface::SurfaceHost as _;

    fn key(name: &str, text: &str) -> InputEvent {
        InputEvent::Key {
            surface: SurfaceId(3),
            key: KeyInput {
                name: name.into(),
                text: text.into(),
                state: ButtonState::Pressed,
                repeat: false,
                modifiers: Modifiers::default(),
                time: 0,
            },
        }
    }

    fn paint(screen: &mut LockScreen, surface: SurfaceId) -> Option<Damage> {
        let size = Size::new(40, 20);
        let mut px = vec![0u8; 40 * 20 * 4];
        let mut t = PaintTarget::new(&mut px, size, 160, Scale::ONE, 0).unwrap();
        screen.paint(surface, &mut t)
    }

    /// Before a fault the renderer paints the lock; once the fallback
    /// shows it paints the content surface (only when it changed) and
    /// takes its keys, and nothing else.
    #[test]
    fn the_fallback_takes_the_content_surface_once_shown() {
        let mut s = LockScreen::default();
        let (content, other) = (SurfaceId(3), SurfaceId(4));
        s.attached(other, NodeId::new(1, 0), Some(NodeKind::Bar));
        s.attached(content, NodeId::new(2, 0), Some(NodeKind::Lock));
        assert_eq!(paint(&mut s, content), None, "the renderer paints the lock");
        assert!(!s.input(&key("a", "a")), "keys go to the Router");
        s.painted(content, &Damage::full(Size::new(40, 20)));
        assert!(s.drew);
        s.show("test");
        assert_eq!(s.wants_frame(content), Some(true));
        assert_eq!(s.wants_frame(other), None);
        assert_eq!(paint(&mut s, other), None);
        assert_eq!(
            paint(&mut s, content),
            Some(Damage::full(Size::new(40, 20)))
        );
        assert_eq!(paint(&mut s, content), Some(Damage::new()), "unchanged");
        assert!(s.input(&key("a", "a")));
        assert_eq!(s.wants_frame(content), Some(true), "a dot to draw");
        // Return with no channel to a checker: refused, never unlocked.
        assert!(s.input(&key("Return", "\r")));
        assert_eq!(
            s.fallback.as_ref().map(|f| f.field.state()),
            Some(FieldState::Failed)
        );
        // An unlock takes it away.
        s.changed(LockState::Unlocked);
        assert!(!s.fallback_shown());
        assert_eq!(paint(&mut s, content), None);
    }

    /// The content made again on another output after its node left
    /// render's tree (the spec gone while locked: SIGTERM unmounting)
    /// is still the lock's content, because strand-surface says so: the
    /// fallback paints it and takes its keys.
    #[test]
    fn the_content_made_again_with_its_node_gone_keeps_the_fallback() {
        let mut s = LockScreen::default();
        let node = NodeId::new(2, 0);
        s.attached(SurfaceId(5), node, Some(NodeKind::Lock));
        s.sync_content(Some(SurfaceId(5)));
        s.show("test");
        // Its output goes, then the spec: the node leaves the tree.
        s.detached(SurfaceId(5));
        s.sync_content(None);
        assert!(!s.shown_on(SurfaceId(5)));
        s.attached(SurfaceId(3), node, None);
        // Another surface on a node the tree does not know is not it.
        s.attached(SurfaceId(4), NodeId::new(7, 0), None);
        s.sync_content(Some(SurfaceId(3)));
        assert!(
            s.shown_on(SurfaceId(3)),
            "the new content shows the fallback"
        );
        assert!(!s.shown_on(SurfaceId(4)));
        assert_eq!(s.wants_frame(SurfaceId(3)), Some(true));
        assert!(paint(&mut s, SurfaceId(3)).is_some_and(|d| !d.is_empty()));
        assert!(s.input(&key("a", "a")), "its keys never reach the Router");
        assert_eq!(s.content, Some((SurfaceId(3), node)));
        // strand-surface's answer wins over a guess made at attach time.
        s.sync_content(None);
        assert_eq!(s.content, None);
    }

    /// The lock asked for with no output, its `lock` unmounted before an
    /// output came (a reload while pending): strand-surface makes the
    /// content with the old node, which render's tree no longer knows and
    /// this lock session never attached. The content is still the
    /// surface strand-surface names, so the main loop shows the fallback
    /// ("no longer mounted") on it and its keys reach the fallback.
    #[test]
    fn the_first_content_on_a_node_already_gone_is_the_content() {
        let mut s = LockScreen::default();
        let node = NodeId::new(2, 0);
        s.attached(SurfaceId(3), node, None);
        assert_eq!(s.content, None, "render's tree cannot tell");
        s.sync_content(Some(SurfaceId(3)));
        assert_eq!(s.content, Some((SurfaceId(3), node)));
        s.show("the lock is no longer mounted");
        assert!(s.shown_on(SurfaceId(3)));
        assert_eq!(s.wants_frame(SurfaceId(3)), Some(true));
        assert!(paint(&mut s, SurfaceId(3)).is_some_and(|d| !d.is_empty()));
        assert!(s.input(&key("a", "a")), "its keys never reach the Router");
        // A content surface with no attach seen falls back to the
        // fallback node: "locked with no `lock` open", never nothing.
        s.sync_content(Some(SurfaceId(9)));
        assert_eq!(s.content, Some((SurfaceId(9), LOCK_FALLBACK_NODE)));
    }

    /// The compositor ending a lock it held is a reason to show the
    /// fallback on the lock asked for next; a refusal is not, nor is
    /// `auth` failing while no lock is asked for or held.
    #[test]
    fn finished_after_locked_is_a_fault_and_a_refusal_is_not() {
        let mut s = LockScreen::default();
        s.changed(LockState::Finished);
        assert_eq!(s.pending, None, "refused: nothing to show it on");
        s.changed(LockState::Locked);
        s.changed(LockState::Finished);
        assert_eq!(s.pending.as_deref(), Some("the compositor ended the lock"));
        s.auth_failed("x".into());
        assert_eq!(
            s.pending.as_deref(),
            Some("the compositor ended the lock"),
            "the first reason stays"
        );
        s.changed(LockState::Unlocked);
        assert_eq!(s.pending, None);
        // `auth` failing with no lock: the main loop's next turn with no
        // lock drops it, so the next lock shows the config's content.
        s.auth_failed("the helper stopped".into());
        s.idle();
        assert_eq!(s.pending, None, "no fallback on the next, healthy lock");
    }

    /// The fallback's verdicts: only a success yields a token.
    #[test]
    fn only_a_success_yields_a_token() {
        let mut s = LockScreen::default();
        assert!(s.checked(Verdict::Denied { message: None }).is_none());
        s.show("test");
        assert!(s.checked(Verdict::Denied { message: None }).is_none());
        assert_eq!(
            s.fallback.as_ref().map(|f| f.field.state()),
            Some(FieldState::Failed)
        );
        assert!(s.checked(Verdict::Failed(AuthError::Timeout)).is_none());
    }

    /// The other outputs take the `lock`'s `bg`: a colour, a token, a
    /// gradient's first stop; black without one.
    #[test]
    fn the_lock_colour_is_the_lock_nodes_bg() {
        use strand_scene::{GradientStop, SceneDiff, TokenExpr, TokenTable};
        let blue = Color::new(0.125, 0.3125, 0.8125, 1.0);
        let red = Color::new(1.0, 0.0, 0.0, 1.0);
        let mut tree = strand_render::SceneTree::new();
        let ids: Vec<NodeId> = (1..=5).map(|i| NodeId::new(i, 0)).collect();
        let mut d = SceneDiff::new();
        for (i, id) in ids.iter().enumerate() {
            d.create(*id, NodeKind::Lock, None, i as u32);
        }
        let mut tokens = TokenTable::default();
        tokens.insert("lockbg", PropValue::Color(red));
        d.set_tokens(tokens, strand_scene::Transition::Instant);
        d.set(ids[0], Prop::Bg, PropValue::Color(blue))
            .set(
                ids[1],
                Prop::Bg,
                PropValue::Token(TokenExpr::path("lockbg")),
            )
            .set(
                ids[2],
                Prop::Bg,
                PropValue::Paint(Paint::Linear {
                    angle: 0.0,
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: blue,
                        },
                        GradientStop {
                            offset: 1.0,
                            color: red,
                        },
                    ],
                }),
            );
        assert!(tree.apply(d).is_empty());
        assert_eq!(lock_color(&tree, ids[0]), blue);
        assert_eq!(lock_color(&tree, ids[1]), red, "a token");
        assert_eq!(lock_color(&tree, ids[2]), blue, "a gradient's first stop");
        assert_eq!(lock_color(&tree, ids[3]), Color::BLACK, "no bg");
        assert_eq!(lock_color(&tree, NodeId::new(9, 0)), Color::BLACK);
    }

    /// The marker follows the lock: written once locked, kept while a
    /// lock is asked for or held (a restart's lock not granted yet, the
    /// lock asked for again after `finished`), removed once there is
    /// none, whether the last one was unlocked or refused.
    #[test]
    fn the_restart_marker_goes_once_no_lock_is_asked_for_or_held() {
        // (marked, locked, active)
        assert_eq!(marker_step(false, true, true), Some(Marker::Write));
        assert_eq!(marker_step(true, true, true), None);
        assert_eq!(marker_step(false, false, true), None, "asked for");
        // A restart's lock pending, or asked for again after `finished`.
        assert_eq!(marker_step(true, false, true), None);
        // Unlocked, or that lock refused: gone.
        assert_eq!(marker_step(true, false, false), Some(Marker::Remove));
        assert_eq!(marker_step(false, false, false), None);
    }

    #[test]
    fn a_fault_is_inside_the_lock_only_under_its_node() {
        use strand_scene::{NodeKind, SceneDiff};
        let mut tree = strand_render::SceneTree::new();
        let (lock, text, bar) = (NodeId::new(1, 0), NodeId::new(2, 0), NodeId::new(3, 0));
        let mut d = SceneDiff::new();
        d.create(lock, NodeKind::Lock, None, 0)
            .create(text, NodeKind::Text, Some(lock), 0)
            .create(bar, NodeKind::Bar, None, 1);
        assert!(tree.apply(d).is_empty());
        assert!(inside(&tree, text, lock));
        assert!(inside(&tree, lock, lock));
        assert!(!inside(&tree, bar, lock));
        assert!(!inside(&tree, NodeId::new(9, 0), lock));
    }

    /// A password input's value, from logic's diff or the `Router`'s
    /// write, is redacted; another input's is not; a removed input's
    /// value is forgotten.
    #[test]
    fn password_values_are_redacted() {
        let (pw, plain) = (NodeId::new(1, 0), NodeId::new(2, 0));
        let mut secrets = Secrets::default();
        let mut d = SceneDiff::new();
        d.create(pw, NodeKind::Input, None, 0)
            .create(plain, NodeKind::Input, None, 1)
            .set(pw, Prop::Text, PropValue::Text("hunter-two".into()))
            .set(pw, Prop::InputType, PropValue::Keyword("password".into()))
            .set(plain, Prop::Text, PropValue::Text("visible".into()));
        secrets.see_diff(&d);
        assert_eq!(
            secrets.redact("`hunter-two` is not a valid time pattern; visible"),
            "`<redacted>` is not a valid time pattern; visible"
        );
        secrets.see_write(pw, Prop::Text, &PropValue::Text("s3cret".into()));
        assert_eq!(
            secrets.redact("a s3cret, hunter-two"),
            "a <redacted>, hunter-two"
        );
        // Emptied (submitted): nothing to redact, nothing matches "".
        secrets.see_write(pw, Prop::Text, &PropValue::Text(String::new()));
        assert_eq!(secrets.redact("s3cret"), "s3cret");
        secrets.see_write(pw, Prop::Text, &PropValue::Text("again".into()));
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: pw,
            window: false,
        });
        secrets.see_diff(&d);
        assert_eq!(secrets.redact("again"), "again", "removed");
        assert!(matches!(secrets.redact("x"), std::borrow::Cow::Borrowed(_)));
    }

    /// While a password input is mounted, an error that can carry values
    /// is redacted whole, so a transformed or copied password (upper
    /// case, a slice, an escaped quote, another state's copy after the
    /// field emptied) never prints; errors made of ids and names keep
    /// their text, and with no password input mounted only current
    /// values are replaced.
    #[test]
    fn a_fault_that_can_carry_values_is_redacted_whole_while_a_password_is_mounted() {
        use strand_core::Error;
        let pw = NodeId::new(1, 0);
        let fault = |msg: &str| strand_compiler::instantiate::RuntimeError {
            what: "text.text".into(),
            error: Error::failed(msg),
            file: None,
            span: None,
            node: None,
            component: None,
            scope: None,
        };
        let mut secrets = Secrets::default();
        // No password input: the message stays.
        assert_eq!(
            secrets.redact_fault(&fault("`HUNTER` is not a valid time pattern")),
            "text.text: `HUNTER` is not a valid time pattern"
        );
        let mut d = SceneDiff::new();
        d.create(pw, NodeKind::Input, None, 0)
            .set(pw, Prop::InputType, PropValue::Keyword("password".into()))
            .set(pw, Prop::Text, PropValue::Text("hunter\"q".into()));
        secrets.see_diff(&d);
        for leaked in [
            "`HUNTER\"Q` is not a valid time pattern",
            "`hunt` is not a valid time pattern",
            "\"hunter\\\"q\" is not a number",
        ] {
            assert_eq!(
                secrets.redact_fault(&fault(leaked)),
                "text.text: <redacted>",
                "{leaked}"
            );
        }
        // Emptied on submit, a copy elsewhere faults: still redacted.
        secrets.see_write(pw, Prop::Text, &PropValue::Text(String::new()));
        assert_eq!(
            secrets.redact_fault(&fault("`hunter\"q` is not a valid time pattern")),
            "text.text: <redacted>"
        );
        // Ids and names only: kept.
        assert_eq!(
            secrets.redact_error(&Error::Cancelled),
            Error::Cancelled.to_string()
        );
        // The input gone: messages print again.
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: pw,
            window: false,
        });
        secrets.see_diff(&d);
        assert_eq!(secrets.redact_error(&Error::failed("x")), "x");
    }

    /// The host passes the compositor's reports to logic as the
    /// compiler's `SessionLock`.
    #[test]
    fn the_host_forwards_lock_reports_to_logic() {
        let font = std::fs::read(strand_text::test_font_path()).unwrap();
        let engine = strand_text::TextEngine::new(strand_text::FontConfig::isolated(vec![
            std::sync::Arc::new(font),
        ]));
        let renderer = Renderer::new(TextBackend::Inline(Box::new(engine)));
        let (tx, rx) = calloop::channel::channel();
        let mut el = calloop::EventLoop::<Vec<ToLogic>>::try_new().unwrap();
        el.handle()
            .insert_source(rx, |e, _, out: &mut Vec<ToLogic>| {
                if let Event::Msg(m) = e {
                    out.push(m);
                }
            })
            .unwrap();
        let mut host = Host::new(renderer, false).forwarding(tx);
        for s in [LockState::Locked, LockState::Finished, LockState::Unlocked] {
            host.lock_changed(s);
        }
        let mut out = Vec::new();
        el.dispatch(Some(Duration::ZERO), &mut out).unwrap();
        assert_eq!(
            out,
            [
                ToLogic::LockState(SessionLock::Locked),
                ToLogic::LockState(SessionLock::Finished),
                ToLogic::LockState(SessionLock::Unlocked),
            ]
        );
    }
}
