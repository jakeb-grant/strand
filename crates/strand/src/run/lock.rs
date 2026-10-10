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
//! - **Restart**: while a lock is asked for or the compositor says the
//!   session is locked, a marker
//!   (`$XDG_RUNTIME_DIR/strand-<display>.locked`) says so; a
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
///
/// (M4) A value of at least [`FRAGMENT`] bytes replaced by one it is not
/// the start of (submitted and emptied, edited, its input removed) is
/// remembered, the newest [`REMEMBERED`] of them, so a state that copied
/// it still has it redacted after the field emptied or the lock went
/// away; and a message holding any [`FRAGMENT`]-byte run of a known
/// value is redacted whole, compared with both lower-cased and with the
/// message's `\"`, `\\`, `\n`, `\u{..}`/`\uXXXX` escapes undone, so a
/// slice, a case-changed copy or a Debug- or JSON-escaped copy does not
/// print. Shorter values are never matched (any message would match
/// them): such a password is covered only while its input is mounted
/// ([`Secrets::redact_error`]). Case is folded by Unicode lowercasing,
/// which misses the few letters whose upper case is longer (`ß`, `SS`).
#[derive(Default)]
pub(super) struct Secrets {
    inputs: std::collections::HashMap<NodeId, Option<Password>>,
    remembered: std::collections::VecDeque<Password>,
}

/// How many replaced password values [`Secrets`] keeps.
const REMEMBERED: usize = 4;

/// The shortest run of a password's bytes that redacts a message (a
/// shorter password: the whole of it).
const FRAGMENT: usize = 4;

/// True when one of `forms` (a message's [`forms`]) holds a run of
/// [`FRAGMENT`] bytes of `value` lower-cased.
fn holds_fragment(forms: &[String], value: &[u8]) -> bool {
    if value.len() < FRAGMENT {
        return false;
    }
    let lower = std::str::from_utf8(value).map(str::to_lowercase);
    let v = lower.as_ref().map_or(value, |l| l.as_bytes());
    v.windows(FRAGMENT).any(|w| {
        forms
            .iter()
            .any(|f| f.as_bytes().windows(FRAGMENT).any(|t| t == w))
    })
}

/// `text` lower-cased, as written and with its escapes undone (Rust's
/// Debug and JSON: `\\ \" \' \n \r \t \0 \u{..} \uXXXX`).
fn forms(text: &str) -> [String; 2] {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            plain.push(c);
            continue;
        }
        let un = match chars.peek() {
            Some(&e @ ('\\' | '"' | '\'')) => Some(e),
            Some('n') => Some('\n'),
            Some('r') => Some('\r'),
            Some('t') => Some('\t'),
            Some('0') => Some('\0'),
            _ => None,
        };
        if let Some(u) = un {
            chars.next();
            plain.push(u);
            continue;
        }
        if chars.peek() == Some(&'u') {
            let rest = chars.clone().skip(1);
            let braced = chars.clone().nth(1) == Some('{');
            let hex: String = if braced {
                rest.skip(1).take_while(|&h| h != '}').take(7).collect()
            } else {
                rest.take(4).collect()
            };
            let code = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
            if let Some(u) = code.filter(|_| braced || hex.len() == 4) {
                let used = 1 + hex.len() + if braced { 2 } else { 0 };
                for _ in 0..used {
                    chars.next();
                }
                plain.push(u);
                continue;
            }
        }
        plain.push(c);
    }
    [text.to_lowercase(), plain.to_lowercase()]
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
                    if let Some(Some(old)) = self.inputs.remove(id) {
                        self.remember(old);
                    }
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
        let Some(PropValue::Text(t)) = Some(value) else {
            return;
        };
        let Some(slot) = self.inputs.get_mut(&node) else {
            return;
        };
        let new = (!t.is_empty()).then(|| Password::from(t.clone()));
        let old = std::mem::replace(slot, new);
        // Typed on (the old value starts the new one): nothing to keep.
        if let Some(old) = old
            && !t.as_bytes().starts_with(old.as_bytes())
        {
            self.remember(old);
        }
    }

    /// Keeps a replaced value: one inside a kept value is dropped, and
    /// kept ones inside it go.
    fn remember(&mut self, value: Password) {
        let inside = |big: &[u8], small: &[u8]| big.windows(small.len()).any(|w| w == small);
        // Too short to match without matching everything.
        if value.len() < FRAGMENT
            || self
                .remembered
                .iter()
                .any(|r| inside(r.as_bytes(), value.as_bytes()))
        {
            return;
        }
        self.remembered
            .retain(|r| !inside(value.as_bytes(), r.as_bytes()));
        if self.remembered.len() == REMEMBERED {
            self.remembered.pop_front();
        }
        self.remembered.push_back(value);
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

    /// Kept-over-a-new-default cells as they may be logged, streamed and
    /// shown: a kept value is a state's, and a password input's state
    /// holds the password (still, after an unlock, when the config does
    /// not clear it), so each `shown` goes through [`Secrets::redact`].
    pub(super) fn redact_kept(
        &self,
        kept: &[strand_compiler::reconcile::KeptCell],
    ) -> Vec<strand_compiler::reconcile::KeptCell> {
        kept.iter()
            .map(|k| strand_compiler::reconcile::KeptCell {
                path: k.path.clone(),
                shown: self.redact(&k.shown).into_owned(),
            })
            .collect()
    }

    /// Notice lines (a kept cell's `KeptCell::notice` among them) through
    /// [`Secrets::redact`].
    pub(super) fn redact_lines(&self, lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| self.redact(l).into_owned()).collect()
    }

    /// `text` with every known password value of at least [`FRAGMENT`]
    /// bytes (the inputs' current ones and the remembered) replaced by
    /// [`REDACTED`]; [`REDACTED`] whole when a fragment of one is still
    /// in it (see [`Secrets`]).
    pub(super) fn redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        let known = || {
            self.inputs
                .values()
                .flatten()
                .chain(&self.remembered)
                .filter(|p| p.len() >= FRAGMENT)
        };
        if known().next().is_none() {
            return std::borrow::Cow::Borrowed(text);
        }
        let mut out = std::borrow::Cow::Borrowed(text);
        for p in known() {
            if let Ok(v) = std::str::from_utf8(p.as_bytes())
                && out.contains(v)
            {
                out = std::borrow::Cow::Owned(out.replace(v, REDACTED));
            }
        }
        let forms = forms(&out);
        if known().any(|p| holds_fragment(&forms, p.as_bytes())) {
            return std::borrow::Cow::Borrowed(REDACTED);
        }
        out
    }
}

// ---- the main thread: the surface host's part --------------------------------

/// What reaches the main loop about the lock from other threads. Each
/// carries the lock session (`LockScreen::session`) its check began in: a
/// verdict for a session that has ended is dropped, so a check still in
/// flight when one lock ended never unlocks, or marks failed, the next
/// (m4-audit).
pub(crate) enum LockMsg {
    /// `auth` accepted a password.
    Token(UnlockToken, u64),
    /// `auth` could not check a password (never a refusal).
    AuthFailed(String, u64),
    /// The fallback's own check answered.
    Checked(u64, Verdict),
}

/// The fallback while it is shown.
struct Shown {
    field: LockFallback,
    /// Repaint the content surface.
    dirty: bool,
}

/// The fallback's password checks: a thread holding its own
/// `strand_auth::Client` (one helper for the lock session), answering
/// on the main loop's channel. The helper is looked for at each check
/// until one is found: one deleted (a package upgrade mid-session) is
/// refused, never unlocked, and checks work again once it is back.
struct Checker {
    tx: std::sync::mpsc::Sender<Password>,
}

impl Checker {
    /// A checker for lock session `session`, whose verdicts say so.
    fn spawn(reply: Sender<LockMsg>, session: u64) -> io::Result<Checker> {
        let (tx, rx) = std::sync::mpsc::channel::<Password>();
        std::thread::Builder::new()
            .name("strand-lock-auth".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                for password in rx {
                    if client.is_none() {
                        client = strand_auth::default_helper()
                            .map(|h| Client::new(h, strand_services::child::restore_in_child));
                    }
                    let verdict = match client.as_mut() {
                        Some(c) => c.submit(password),
                        None => Verdict::Failed(AuthError::Spawn(io::Error::new(
                            io::ErrorKind::NotFound,
                            "no `strand-auth` helper is installed",
                        ))),
                    };
                    if reply.send(LockMsg::Checked(session, verdict)).is_err() {
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
    /// The lock session's generation, bumped when a lock ends: the tag
    /// of every check begun in it (shared with `auth`'s
    /// `AuthConfig::session`, read on the services thread).
    session: Arc<std::sync::atomic::AtomicU64>,
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
                    let session = self.session.load(std::sync::atomic::Ordering::SeqCst);
                    match Checker::spawn(reply, session) {
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

    /// The current lock session's generation.
    fn generation(&self) -> u64 {
        self.session.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A check begun in lock session `tag` still belongs to this one.
    fn current(&self, tag: u64, what: &str) -> bool {
        let current = tag == self.generation();
        if !current {
            log::info!("lock: {what} from a lock that has ended is dropped");
        }
        current
    }

    /// `auth` accepted a password in lock session `tag`: the token, if
    /// that session is still this one.
    pub(crate) fn token(&mut self, token: UnlockToken, tag: u64) -> Option<UnlockToken> {
        self.current(tag, "a password accepted").then_some(token)
    }

    /// The fallback's check, begun in lock session `tag`, answered: a
    /// token to unlock with, or the field shows the refusal. A verdict
    /// for an earlier session is dropped.
    pub(crate) fn checked(&mut self, tag: u64, verdict: Verdict) -> Option<UnlockToken> {
        if !self.current(tag, "a verdict") {
            return None;
        }
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
    pub(crate) fn auth_failed(&mut self, why: String, tag: u64) {
        if !self.current(tag, "a failure to check") {
            return;
        }
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

    /// (m4-audit) A lock is first seen asked for: a new lock session. A
    /// check begun before it (an `auth.submit` outside the `lock`, PAM
    /// still answering when an idle lock starts) carries the old tag, so
    /// its token cannot release a lock no password was typed into.
    fn begin(&mut self) {
        self.session
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// No lock any more: the fallback and its reasons go.
    fn reset(&mut self) {
        self.fallback = None;
        self.pending = None;
        self.drew = false;
        self.asked = None;
        // The helper goes with the lock session, and its checks still in
        // flight answer for a session that has ended.
        self.checker = None;
        self.session
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    ///
    /// Under `STRAND_MOCK` (`mocked`) the session lock stays off: the
    /// mock host has no `auth` store, so `auth.submit` would never answer
    /// and a lock it took could not be released or fall back (m4-audit).
    pub(super) fn wire(
        handle: &calloop::LoopHandle<'static, State<Host>>,
        state: &mut State<Host>,
        mocked: bool,
    ) -> Result<Guard, DemoError> {
        let (tx, rx) = calloop::channel::channel::<LockMsg>();
        handle
            .insert_source(rx, |event, _, state| {
                let Event::Msg(msg) = event else {
                    return;
                };
                let lock = &mut state.host_mut().lock;
                let token = match msg {
                    LockMsg::Token(t, tag) => lock.token(t, tag),
                    LockMsg::AuthFailed(why, tag) => {
                        lock.auth_failed(why, tag);
                        None
                    }
                    LockMsg::Checked(tag, v) => lock.checked(tag, v),
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
        let session = state.host_mut().lock.session.clone();
        let mut config = strand_services::auth::AuthConfig {
            sink: Some(Arc::new(move |token, tag| {
                if let Ok(t) = tokens.lock() {
                    let _ = t.send(LockMsg::Token(token, tag));
                }
            })),
            failed: Some(Arc::new(move |why: &str, tag| {
                if let Ok(t) = failures.lock() {
                    let _ = t.send(LockMsg::AuthFailed(why.to_string(), tag));
                }
            })),
            session: Some(Arc::new(move || {
                session.load(std::sync::atomic::Ordering::SeqCst)
            })),
            ..Default::default()
        };
        faults::auth_config(&mut config);
        strand_services::auth::configure(Some(config));
        let mut guard = Guard {
            since: None,
            seq: 0,
            waiting: None,
            next_beat: None,
            stopping: false,
            marker: if mocked { None } else { marker_path() },
            marked: false,
        };
        if mocked {
            // Info, not a warning: every mocked run says it, and the
            // design shells' tests fail on any WARN line.
            log::info!(
                "lock: STRAND_MOCK has no `auth` service, so this run never locks the session"
            );
            return Ok(guard);
        }
        // Only now: a token reaches `unlock`.
        state.enable_session_lock();
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
        let began = self.since.is_none();
        let since = *self.since.get_or_insert(now);
        if began {
            state.host_mut().lock.begin();
        }
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
        match marker_step(self.marked, state.lock_active()) {
            Some(Marker::Write) => {
                if let Err(e) = std::fs::write(m, b"locked\n") {
                    log::warn!("lock: {}: {e}", m.display());
                }
                self.marked = true;
                faults::marked(state.is_locked());
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

/// The restart marker is written as soon as a lock is asked for, before
/// the compositor says `locked` (m4-audit): a compositor may lock the
/// session at the request and keep it locked when the client dies, and
/// a strand that dies before `locked` (on its first frame, say) must
/// come back to that lock with a password field. It is removed once no
/// lock is asked for or held: after an unlock, and also after a lock the
/// compositor refused (a restart's lock again, or the one asked for after
/// `finished`), since a strand killed with no lock leaves nothing locked
/// to come back to. A marker kept then would lock the session on every
/// later start.
fn marker_step(marked: bool, active: bool) -> Option<Marker> {
    if active && !marked {
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

    /// The restart marker was just written: `abort_before_locked` aborts
    /// the process there if the compositor has not said `locked` yet (a
    /// strand dying while its lock is pending, as on its first frame).
    pub(crate) fn marked(locked: bool) {
        if on("abort_before_locked") {
            if locked {
                log::warn!("STRAND_FAULT abort_before_locked missed: already locked");
            } else {
                log::warn!("STRAND_FAULT abort_before_locked: the lock is pending");
                std::process::abort();
            }
        }
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
    /// `auth_hang` (unless `auth_hold`: the default timeout, for a test
    /// that kills the hung helper itself), and `auth_missing` points it
    /// at no helper at all. The fallback's own client gets none of it.
    pub(crate) fn auth_config(config: &mut strand_services::auth::AuthConfig) {
        let env = value();
        if env.is_empty() {
            return;
        }
        if on("auth_missing") {
            config.helper = Some("/nonexistent/strand-auth".into());
        }
        if on("auth_hang") && !on("auth_hold") {
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
    pub(crate) fn marked(_: bool) {}
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
        s.auth_failed("x".into(), 0);
        assert_eq!(
            s.pending.as_deref(),
            Some("the compositor ended the lock"),
            "the first reason stays"
        );
        s.changed(LockState::Unlocked);
        assert_eq!(s.pending, None);
        // `auth` failing with no lock: the main loop's next turn with no
        // lock drops it, so the next lock shows the config's content.
        s.auth_failed("the helper stopped".into(), 1);
        s.idle();
        assert_eq!(s.pending, None, "no fallback on the next, healthy lock");
    }

    /// The fallback's verdicts: only a success yields a token.
    #[test]
    fn only_a_success_yields_a_token() {
        let mut s = LockScreen::default();
        assert!(s.checked(0, Verdict::Denied { message: None }).is_none());
        s.show("test");
        assert!(s.checked(0, Verdict::Denied { message: None }).is_none());
        assert_eq!(
            s.fallback.as_ref().map(|f| f.field.state()),
            Some(FieldState::Failed)
        );
        assert!(s.checked(0, Verdict::Failed(AuthError::Timeout)).is_none());
    }

    /// (m4-audit) A verdict is tied to the lock session its check began
    /// in. A check still in flight when a lock ended (the fallback's or
    /// `auth`'s) answers into the next lock: its token unlocks nothing
    /// (`token` and `checked` take a token only from a current session),
    /// and its refusal or failure neither marks the new field failed
    /// (which would let a second password in while the new check runs)
    /// nor shows the fallback. The new session's own verdicts count.
    /// (A token cannot be made outside strand-auth's client, so the
    /// unlock path is checked through `current`, which both gate on.)
    #[test]
    fn a_verdict_from_an_ended_lock_is_dropped() {
        let mut s = LockScreen::default();
        s.changed(LockState::Locked);
        s.show("test");
        let first = s.generation();
        assert!(s.current(first, "test"));
        // The lock ends with that check in flight; a new one shows the
        // fallback at once, its own check running.
        s.changed(LockState::Unlocked);
        s.changed(LockState::Locked);
        s.show("test again");
        let field = |s: &LockScreen| s.fallback.as_ref().map(|f| f.field.state());
        let before = field(&s);
        assert!(!s.current(first, "test"), "the ended session's tag");
        assert!(
            s.checked(first, Verdict::Denied { message: None })
                .is_none()
        );
        assert!(
            s.checked(first, Verdict::Failed(AuthError::Timeout))
                .is_none()
        );
        assert_eq!(field(&s), before, "the new field is untouched");
        s.fallback = None;
        s.auth_failed("stale".into(), first);
        assert_eq!(s.pending, None, "auth's stale failure");
        // This session's own verdicts.
        let now = s.generation();
        assert!(s.current(now, "test"));
        s.auth_failed("now".into(), now);
        assert!(s.pending.is_some(), "auth's failure in this session");
        s.show("test again");
        assert!(s.checked(now, Verdict::Denied { message: None }).is_none());
        assert_eq!(field(&s), Some(FieldState::Failed));
    }

    /// (m4-audit) A check begun while no lock was active (an
    /// `auth.submit` from a popup) answers after a lock began: its tag is
    /// not the new lock's, so its token releases nothing.
    /// [`Guard::check`] calls `begin` when it first sees the lock asked
    /// for, before any of the lock's own input can reach it.
    #[test]
    fn a_verdict_from_before_a_lock_began_is_dropped() {
        let mut s = LockScreen::default();
        let unlocked = s.generation();
        s.begin();
        assert!(!s.current(unlocked, "test"), "a check from before the lock");
        let locked = s.generation();
        assert!(s.current(locked, "test"), "the lock's own check");
        s.changed(LockState::Locked);
        s.show("test");
        assert!(
            s.checked(unlocked, Verdict::Denied { message: None })
                .is_none()
        );
        assert_ne!(
            s.fallback.as_ref().map(|f| f.field.state()),
            Some(FieldState::Failed),
            "the stale refusal leaves the field alone"
        );
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

    /// The marker follows the lock: written once a lock is asked for
    /// (before `locked`: a strand dying then comes back to the lock;
    /// m4-audit), kept while a lock is asked for or held (a restart's
    /// lock not granted yet, the lock asked for again after `finished`),
    /// removed once there is none, whether the last one was unlocked or
    /// refused.
    #[test]
    fn the_restart_marker_goes_once_no_lock_is_asked_for_or_held() {
        // (marked, active)
        assert_eq!(marker_step(false, true), Some(Marker::Write), "asked for");
        assert_eq!(marker_step(true, true), None, "pending or locked");
        // Unlocked, or that lock refused: gone.
        assert_eq!(marker_step(true, false), Some(Marker::Remove));
        assert_eq!(marker_step(false, false), None);
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
        // Replaced by a value it does not start: the old one is kept.
        secrets.see_write(pw, Prop::Text, &PropValue::Text("s3cret".into()));
        assert_eq!(
            secrets.redact("a s3cret, hunter-two"),
            "a <redacted>, <redacted>"
        );
        // Emptied (submitted): still redacted, in a copy elsewhere.
        secrets.see_write(pw, Prop::Text, &PropValue::Text(String::new()));
        assert_eq!(secrets.redact("s3cret"), "<redacted>");
        secrets.see_write(pw, Prop::Text, &PropValue::Text("again".into()));
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: pw,
            window: false,
        });
        secrets.see_diff(&d);
        assert_eq!(secrets.redact("again"), "<redacted>", "removed, kept");
        assert!(matches!(secrets.redact("x"), std::borrow::Cow::Borrowed(_)));
        assert_eq!(secrets.redact("visible"), "visible");
    }

    /// A kept cell's value and its notice line never show a password: a
    /// reload after the unlock that changes the default of a state still
    /// holding the accepted password would print it in the `reload`
    /// event, the `notices` event, the log and the overlay.
    #[test]
    fn a_kept_password_is_redacted_in_kept_cells_and_notices() {
        use strand_compiler::reconcile::KeptCell;
        let pw = NodeId::new(1, 0);
        let mut secrets = Secrets::default();
        let mut d = SceneDiff::new();
        d.create(pw, NodeKind::Input, None, 0)
            .set(pw, Prop::Text, PropValue::Text("hunter-two".into()))
            .set(pw, Prop::InputType, PropValue::Keyword("password".into()));
        secrets.see_diff(&d);
        // Unlocked: the lock and its input unmount, the state keeps it.
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: pw,
            window: false,
        });
        secrets.see_diff(&d);
        let kept = [
            KeptCell {
                path: "lock.secret".into(),
                shown: "\"hunter-two\"".into(),
            },
            KeptCell {
                path: "bar.n".into(),
                shown: "3".into(),
            },
        ];
        let out = secrets.redact_kept(&kept);
        assert_eq!(out[0].path, "lock.secret");
        assert_eq!(out[0].shown, "\"<redacted>\"");
        assert_eq!(out[1].shown, "3");
        let notices = secrets.redact_lines(&[kept[0].notice(), kept[1].notice()]);
        assert!(!notices[0].contains("hunter"), "{notices:?}");
        assert_eq!(notices[1], kept[1].notice());
        let json = super::super::shell::kept_json(&out).to_string();
        assert!(!json.contains("hunter"), "{json}");
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
        // ... except a fragment of the password, its case changed.
        assert_eq!(
            secrets.redact_error(&Error::failed("`HUNT` is not a valid time pattern")),
            "<redacted>"
        );
    }

    /// (M4) A fault message holding a fragment of a password, any time
    /// after it was typed, is never logged: four bytes of it in any case,
    /// a short password whole; typing on does not fill the memory with
    /// prefixes, and only the newest values are kept.
    #[test]
    fn a_password_fragment_is_never_logged() {
        let pw = NodeId::new(1, 0);
        let mut secrets = Secrets::default();
        let mut d = SceneDiff::new();
        d.create(pw, NodeKind::Input, None, 0).set(
            pw,
            Prop::InputType,
            PropValue::Keyword("password".into()),
        );
        secrets.see_diff(&d);
        for typed in ["c", "co", "cor", "corr", "correct-horse"] {
            secrets.see_write(pw, Prop::Text, &PropValue::Text(typed.into()));
        }
        assert!(secrets.remembered.is_empty(), "typing on keeps nothing");
        // Submitted: the field empties, the lock goes away.
        secrets.see_write(pw, Prop::Text, &PropValue::Text(String::new()));
        let mut d = SceneDiff::new();
        d.push(SceneOp::Remove {
            id: pw,
            window: false,
        });
        secrets.see_diff(&d);
        assert_eq!(secrets.remembered.len(), 1);
        for leaked in [
            "`HORSE` is not a valid time pattern",
            "index 3 out of range for \"t-ho\"",
            "no key `rect` in the map",
            "correct-horse",
        ] {
            assert_eq!(secrets.redact(leaked), REDACTED, "{leaked}");
        }
        for fine in [
            "`HH:mm` is not a valid time pattern",
            "cor is fine",
            "hosting",
        ] {
            assert_eq!(secrets.redact(fine), fine);
        }
        // A value under four bytes (a typo, a short password) is
        // neither matched nor remembered: one letter typed and cleared
        // does not blank every message holding it.
        let mut d = SceneDiff::new();
        d.create(pw, NodeKind::Input, None, 0)
            .set(pw, Prop::InputType, PropValue::Keyword("password".into()))
            .set(pw, Prop::Text, PropValue::Text("q1z".into()));
        secrets.see_diff(&d);
        assert_eq!(secrets.redact("got q1z"), "got q1z");
        for v in ["x", "", "a", "", "q1z", ""] {
            secrets.see_write(pw, Prop::Text, &PropValue::Text(v.into()));
        }
        assert_eq!(secrets.remembered.len(), 1, "only correct-horse");
        for fine in ["a fault about x and a", "got Q1Z"] {
            assert_eq!(secrets.redact(fine), fine);
        }
        // Escaped quotes and backslashes (Debug, JSON) and a non-ASCII
        // password's case-changed copy are still found.
        for (v, leaked) in [
            (
                "a\"b\"c\"d",
                "index 9 out of range for \"a\\\"b\\\"c\\\"d\"",
            ),
            ("x\\y\\z\\w", "{\"t\":\"x\\\\y\\\\z\\\\w\"}"),
            ("ébène-été", "`ÉBÈNE-ÉTÉ` is not a valid time pattern"),
            ("tab\tbed", "no key \"tab\\tbed\""),
            (
                "é\u{1}é\u{1}é",
                "\"é\\u{1}é\\u{1}é\" / \"\\u00e9\\u0001\\u00e9\\u0001\"",
            ),
        ] {
            secrets.see_write(pw, Prop::Text, &PropValue::Text(v.into()));
            secrets.see_write(pw, Prop::Text, &PropValue::Text(String::new()));
            assert_eq!(secrets.redact(leaked), REDACTED, "{v}: {leaked}");
        }
        // A backspace keeps the longer value until a value holding it
        // replaces it; only the newest four values are kept.
        for v in [
            "abcd", "abc", "", "w0rd-one", "", "w0rd-two", "", "w0rd-3", "", "w0rd-4", "",
        ] {
            secrets.see_write(pw, Prop::Text, &PropValue::Text(v.into()));
        }
        assert_eq!(secrets.remembered.len(), REMEMBERED);
        assert_eq!(
            secrets.redact("correct-horse"),
            "correct-horse",
            "forgotten"
        );
        assert_eq!(secrets.redact("W0RD-4"), REDACTED);
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
