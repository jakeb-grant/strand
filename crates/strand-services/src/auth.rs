//! `auth`: the lock screen's password check.
//!
//! The store runs a [`strand_auth::Client`]: the `strand-auth` PAM
//! helper, a separate process started when the store starts (a lock
//! screen reads an `auth` field, so that is when a lock is shown) and
//! killed when it stops, respawned by the client when it dies. It is
//! spawned with [`child::restore_in_child`](crate::child) as its
//! `pre_exec`, as every program strand starts is.
//!
//! Each check is a PAM conversation ([`strand_auth::Client::converse`])
//! on a blocking task (decisions.md, m6-lock). PAM's texts set `message`
//! and its hidden prompts `prompt` as PAM says them, so a lock can show
//! "Place your finger on the fingerprint reader" while `pam_fprintd`
//! waits, then the password prompt. `auth.submit(password)` answers the
//! prompt PAM is waiting on, or the next one it asks (typed while a
//! fingerprint module waits, the password waits for `pam_unix`), and
//! sets `busy` until PAM answers it; a second submit meanwhile is
//! dropped. The password is a [`Password`] from the moment it arrives,
//! wiped once sent or dropped.
//!
//! While the binary's [`LockSignal`] says a lock is shown, the store
//! keeps a conversation open, so a fingerprint module runs before any
//! password is typed: one begins when the lock shows (or the store
//! starts under it), and again after each one that a password was given
//! to and that did not unlock; one that ended with no password (a
//! `pam_faillock` lockout, a helper that died) is begun again only by the
//! next submit, so a stack that fails at once cannot loop. When the lock
//! goes, a conversation with no password in it ends (its helper is
//! killed), and what it says no longer shows. One begun for a lock
//! session that has ended while another is shown (the signal went false
//! and true again, perhaps before the store saw the false) ends too, and
//! the new lock's begins once it has. A password submitted while a
//! conversation ends, or just after it ended, begins the next one.
//! Without a signal (tests, an embedder) each submit begins a
//! conversation of its own.
//!
//! A success mints a [`strand_auth::UnlockToken`], which goes to the
//! [`UnlockSink`] the binary [`configure`]s (it passes it to the surface
//! manager's unlock, the only way a session lock is released); this store
//! never unlocks anything itself. A refusal of a password sets `failed`.
//! A check that could not be made with a password in it (no helper, a
//! crash, a timeout, a PAM error) also sets `failed`, is a warning
//! diagnostic and goes to the [`FailureSink`] (the binary then shows its
//! built-in password field, whose own client may still work:
//! decisions.md, m4-lock-w2); one with no password in it is only logged,
//! and the next submit tries again. The helper's `login` fallback warns
//! once per process (decisions.md, m4-owner).
//!
//! The blocking task hands its verdict to the sinks itself, so a check
//! outlives the store: a lock that never reads an `auth` field holds no
//! reader, and the store stops [`STOP_GRACE`](crate::STOP_GRACE) after a
//! submit, mid-check if PAM is slow; its token or its failure still
//! arrives (m4-audit). Each conversation carries the [`SessionTag`] read
//! when it began, so that the binary can drop a verdict for a lock
//! session that has ended.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_auth::{Answers, Client, Password, Said, UnlockToken, Verdict};
use tokio::sync::{mpsc, watch};

use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `auth` service serves.
pub const SCHEMA: &str = strand_services_schema::AUTH;

/// Where an accepted password's [`UnlockToken`] goes, with the check's
/// session tag: the binary's main loop, which hands it to
/// `strand_surface::State::unlock`. Called on a blocking task's thread.
pub type UnlockSink = Arc<dyn Fn(UnlockToken, u64) + Send + Sync>;

/// Told why a password could not be checked (no helper, a crash, a
/// timeout, a PAM error; never a refusal), with the check's session tag:
/// the binary's main loop, which shows the built-in password field.
/// Called on a blocking task's thread or the services thread.
pub type FailureSink = Arc<dyn Fn(&str, u64) + Send + Sync>;

/// Read when a check begins; the sinks get its value back (the binary:
/// its lock session's generation). Without one every check's tag is 0.
pub type SessionTag = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Whether a lock is shown, set by the binary as its lock sessions begin
/// and end: while it is, the store keeps a PAM conversation open (see
/// the module docs). Clones share one value.
#[derive(Clone, Debug)]
pub struct LockSignal(Arc<watch::Sender<bool>>);

impl Default for LockSignal {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }
}

impl LockSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// A lock is shown (`true`) or not.
    pub fn set(&self, shown: bool) {
        self.0
            .send_if_modified(|v| std::mem::replace(v, shown) != shown);
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Applied to every [`Client`] the store makes, before its first
/// password (the binary's `faults` build passes `STRAND_FAULT` through
/// with `Client::with_test_env`).
pub type ClientHook = Arc<dyn Fn(Client) -> Client + Send + Sync>;

/// What the `auth` store uses from its next start on.
#[derive(Clone)]
pub struct AuthConfig {
    /// The helper; `None` looks it up ([`strand_auth::default_helper`]).
    pub helper: Option<PathBuf>,
    /// How long PAM may stay quiet with a password
    /// ([`strand_auth::DEFAULT_TIMEOUT`]; a password is answered within
    /// [`strand_auth::LIMIT_FACTOR`] times it).
    pub timeout: Duration,
    /// Where unlocks go. Without one an accepted password unlocks
    /// nothing (and says so in the log).
    pub sink: Option<UnlockSink>,
    /// Where failures to check go.
    pub failed: Option<FailureSink>,
    /// Tags each check (see [`SessionTag`]).
    pub session: Option<SessionTag>,
    /// Applied to each client made.
    pub client_hook: Option<ClientHook>,
    /// Whether a lock is shown (see [`LockSignal`]); `None`: each submit
    /// begins its own conversation.
    pub lock: Option<LockSignal>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            helper: None,
            timeout: strand_auth::DEFAULT_TIMEOUT,
            sink: None,
            failed: None,
            session: None,
            client_hook: None,
            lock: None,
        }
    }
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("helper", &self.helper)
            .field("timeout", &self.timeout)
            .field("sink", &self.sink.is_some())
            .field("failed", &self.failed.is_some())
            .field("session", &self.session.is_some())
            .field("client_hook", &self.client_hook.is_some())
            .field("lock", &self.lock.is_some())
            .finish()
    }
}

static CONFIG: Mutex<Option<AuthConfig>> = Mutex::new(None);

/// Sets what the store uses from its next start (the binary: the unlock
/// sink; tests: a fake helper). `None` restores the default. A running
/// store keeps its config until it stops.
pub fn configure(config: Option<AuthConfig>) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = config;
    }
}

fn config() -> AuthConfig {
    CONFIG
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .unwrap_or_default()
}

/// `auth`'s actions.
#[derive(Call, Debug)]
pub enum AuthAction {
    /// `auth.submit(password)`.
    Submit { password: String },
}

/// See the module docs.
#[service(name = "auth", action = AuthAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Auth {
    /// A password is with PAM, or waits for PAM's next prompt.
    pub busy: bool,
    /// The last password was refused, or could not be checked.
    pub failed: bool,
    /// What PAM last told the user (`PAM_TEXT_INFO`, `PAM_ERROR_MSG`),
    /// as it said it; "" once PAM next asks for a password, and while no
    /// lock is shown.
    pub message: String,
    /// The hidden prompt PAM waits on ("Password: "), or "" while it
    /// waits on none.
    pub prompt: String,
}

/// What PAM said, from the checking task to the store.
enum Event {
    Message(String),
    Prompt(String),
}

/// A conversation that ended: the client back, [`deliver`]'s answer,
/// and whether a password was given to it.
type Ended = (Client, Delivered, bool);

/// What [`deliver`] made of a verdict: `failed`'s new value, if it
/// changes, the warning a failure to check is, and whether it unlocked.
#[derive(Debug, Default)]
struct Delivered {
    failed: Option<bool>,
    warning: Option<String>,
    unlocked: bool,
}

/// Hands `verdict` to the sinks (on the checking task's thread, so that a
/// stopped store still delivers it). `answered`: a password was given to
/// the conversation; without one a refusal or a failure is only logged.
fn deliver(config: &AuthConfig, verdict: Verdict, answered: bool, session: u64) -> Delivered {
    match verdict {
        Verdict::Unlocked(token) => {
            match &config.sink {
                Some(sink) => sink(token, session),
                None => log::warn!("auth: a password was accepted, but nothing takes the unlock"),
            }
            Delivered {
                failed: Some(false),
                unlocked: true,
                ..Delivered::default()
            }
        }
        Verdict::Denied { message } => {
            if let Some(m) = message {
                log::info!("auth: refused: {m}");
            }
            Delivered {
                failed: answered.then_some(true),
                ..Delivered::default()
            }
        }
        Verdict::Failed(strand_auth::AuthError::Cancelled) => Delivered::default(),
        Verdict::Failed(e) if !answered => {
            log::info!("auth: a conversation with no password ended: {e}");
            Delivered::default()
        }
        Verdict::Failed(e) => {
            let why = format!("the password could not be checked: {e}");
            if let Some(f) = &config.failed {
                f(&why, session);
            }
            Delivered {
                failed: Some(true),
                warning: Some(why),
                unlocked: false,
            }
        }
    }
}

/// The conversation in progress.
struct Running {
    task: tokio::task::JoinHandle<Ended>,
    /// Its passwords; dropped when the lock goes, which ends it unless a
    /// password is in it.
    answers: Option<Answers>,
    /// A password submitted while it ends (its answers dropped): the
    /// next conversation begins with it.
    next: Option<Password>,
    /// A submitted password waits for PAM's next prompt.
    queued: bool,
    /// PAM waits on a prompt no password has answered.
    asking: bool,
    tag: u64,
}

impl Auth {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = Arc::new(config());
        let helper = config.helper.clone().or_else(strand_auth::default_helper);
        let start = |path: &PathBuf| {
            let c = Client::new(path.clone(), crate::child::restore_in_child)
                .with_timeout(config.timeout);
            match &config.client_hook {
                Some(hook) => hook(c),
                None => c,
            }
        };
        let session = || config.session.as_ref().map_or(0, |s| s());
        let mut client = helper.as_ref().map(start);
        if helper.is_none() {
            cx.warn("no `strand-auth` helper is installed: the lock screen cannot check passwords");
        }
        if !cx.update(|s| *s = Auth::default()) {
            return Ok(());
        }
        cx.ready();
        let signalled = config.lock.is_some();
        let mut lock = config.lock.as_ref().map(LockSignal::subscribe);
        let shown =
            |lock: &Option<watch::Receiver<bool>>| lock.as_ref().is_some_and(|l| *l.borrow());
        // A conversation tagged `tag` belongs to a lock session that has
        // ended while another is shown (lock-r1-2): the binary drops its
        // verdict, so it must not stand in for this lock's.
        let is_stale = |lock: &Option<watch::Receiver<bool>>, tag: u64| {
            signalled && shown(lock) && tag != session()
        };
        // What a conversation tagged `tag` says may show: with a signal,
        // only while a lock is shown, and only for the session it began
        // in (lock-r1-3).
        let live = |lock: &Option<watch::Receiver<bool>>, tag: u64| {
            !signalled || (shown(lock) && tag == session())
        };
        let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
        let mut running: Option<Running> = None;
        // Begins a conversation with `password` waiting for its first
        // prompt, if any; `false` when the store has stopped.
        let begin = |cx: &mut Cx<Self>,
                     client: &mut Option<Client>,
                     running: &mut Option<Running>,
                     password: Option<Password>|
         -> bool {
            let tag = session();
            let asked = password.is_some();
            let pair = client
                .take()
                .ok_or_else(|| {
                    "no `strand-auth` helper is installed: the password could not be checked"
                        .to_string()
                })
                .and_then(|c| match strand_auth::answers() {
                    Ok(pair) => Ok((c, pair)),
                    Err(e) => {
                        *client = Some(c);
                        Err(format!("the password could not be checked: {e}"))
                    }
                });
            let (mut c, (answers, inbox)) = match pair {
                Ok(p) => p,
                Err(why) => {
                    if !asked {
                        return true;
                    }
                    if let Some(f) = &config.failed {
                        f(&why, tag);
                    }
                    cx.warn(why);
                    return cx.update(|s| {
                        s.busy = false;
                        s.failed = true;
                    });
                }
            };
            if let Some(p) = password {
                answers.send(p);
            }
            let config = config.clone();
            let events = events_tx.clone();
            let task = tokio::task::spawn_blocking(move || {
                let out = c.converse(inbox, &mut |said| {
                    let _ = events.send(match said {
                        Said::Message(m) => Event::Message(m.to_string()),
                        Said::Prompt(p) => Event::Prompt(p.to_string()),
                    });
                });
                let d = deliver(&config, out.verdict, out.answered, tag);
                (c, d, out.answered)
            });
            *running = Some(Running {
                task,
                answers: Some(answers),
                next: None,
                queued: asked,
                asking: false,
                tag,
            });
            !asked
                || cx.update(|s| {
                    s.busy = true;
                    s.failed = false;
                    s.prompt.clear();
                })
        };
        // What PAM said, in order; `live`: it may show.
        let said =
            |cx: &mut Cx<Self>, running: &mut Option<Running>, live: bool, event: Event| -> bool {
                match event {
                    Event::Message(m) => !live || cx.update(|s| s.message = m),
                    Event::Prompt(p) => {
                        let Some(r) = running.as_mut() else {
                            return true;
                        };
                        let queued = std::mem::take(&mut r.queued);
                        if !queued {
                            r.asking = true;
                        }
                        if !live {
                            return true;
                        }
                        if queued {
                            // The submitted password answers it at once.
                            return cx.update(|s| s.message.clear());
                        }
                        cx.update(|s| {
                            s.message.clear();
                            s.prompt = p;
                            s.busy = false;
                        })
                    }
                }
            };
        if shown(&lock) && !begin(&mut cx, &mut client, &mut running, None) {
            return Ok(());
        }
        loop {
            tokio::select! {
                done = async {
                    match running.as_mut() {
                        Some(r) => (&mut r.task).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let Some(ended) = running.take() else {
                        continue;
                    };
                    let tag = ended.tag;
                    // Its texts first: they were said before its verdict.
                    let live_now = live(&lock, tag);
                    while let Ok(e) = events.try_recv() {
                        if !said(&mut cx, &mut None, live_now, e) {
                            return Ok(());
                        }
                    }
                    let (d, answered) = match done {
                        Ok((c, d, answered)) => {
                            client = Some(c);
                            (d, answered)
                        }
                        // The blocking task panicked: its client (and
                        // helper) went with it; a new one takes over.
                        Err(e) => {
                            client = helper.as_ref().map(start);
                            let v = Verdict::Failed(strand_auth::AuthError::Io(
                                std::io::Error::other(e.to_string()),
                            ));
                            (deliver(&config, v, true, tag), true)
                        }
                    };
                    // A password submitted after the conversation had
                    // ended (it never saw it: lock-r1-5), or while it was
                    // ending: the next conversation's.
                    let late = ended
                        .answers
                        .as_ref()
                        .and_then(Answers::take_back)
                        .filter(|_| !answered);
                    let next = ended.next.or(late);
                    if let Some(w) = strand_auth::take_service_warning() {
                        cx.warn(w);
                    }
                    if let Some(w) = d.warning {
                        cx.warn(w);
                    }
                    let stale = is_stale(&lock, tag);
                    if !cx.update(|s| {
                        s.busy = false;
                        s.prompt.clear();
                        if let Some(f) = d.failed.filter(|_| !stale) {
                            s.failed = f;
                        }
                    }) {
                        return Ok(());
                    }
                    let unlocked = d.unlocked && !stale;
                    // Only while a lock is shown with a signal: outside
                    // one, a conversation would hold the reader. Else, the
                    // lock is still shown and the user tried, or this
                    // lock never had a conversation of its own: PAM
                    // starts over (a fingerprint module first).
                    let again = match next {
                        Some(p) if !unlocked && (!signalled || shown(&lock)) => Some(Some(p)),
                        _ if shown(&lock) && (stale || answered && !unlocked) => Some(None),
                        _ => None,
                    };
                    if let Some(p) = again
                        && !begin(&mut cx, &mut client, &mut running, p)
                    {
                        return Ok(());
                    }
                }
                Some(e) = events.recv() => {
                    let live_now = running.as_ref().is_some_and(|r| live(&lock, r.tag));
                    if !said(&mut cx, &mut running, live_now, e) {
                        return Ok(());
                    }
                }
                changed = async {
                    match lock.as_mut() {
                        Some(l) => l.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() {
                        // The binary's signal went: no lock shows again.
                        lock = None;
                    }
                    if shown(&lock) {
                        match running.as_mut() {
                            None => {
                                if !begin(&mut cx, &mut client, &mut running, None) {
                                    return Ok(());
                                }
                            }
                            // Begun for a lock session that has ended (the
                            // signal went false and true again before this
                            // store saw the false, or that conversation is
                            // still ending): it ends, what it showed goes,
                            // and this lock's begins once it has
                            // (lock-r1-2).
                            Some(r) if r.tag != session() => {
                                r.answers = None;
                                r.next = None;
                                r.queued = false;
                                r.asking = false;
                                if !cx.update(|s| {
                                    s.busy = false;
                                    s.message.clear();
                                    s.prompt.clear();
                                }) {
                                    return Ok(());
                                }
                            }
                            Some(_) => {}
                        }
                    } else {
                        // Ends a conversation with no password in it.
                        if let Some(r) = running.as_mut() {
                            r.answers = None;
                            r.next = None;
                        }
                        if !cx.update(|s| {
                            s.message.clear();
                            s.prompt.clear();
                        }) {
                            return Ok(());
                        }
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Action(AuthAction::Submit { password })) => {
                        let password = Password::from(password);
                        if cx.state().busy {
                            // One check at a time; this one is dropped
                            // (and wiped).
                            continue;
                        }
                        if let Some(why) = strand_auth::refusal(&password) {
                            log::info!("auth: refused: {why}");
                            if !cx.update(|s| s.failed = true) {
                                return Ok(());
                            }
                            continue;
                        }
                        let sent = match running.as_mut() {
                            Some(r) => match &r.answers {
                                Some(a) => {
                                    // `false`: one waits already.
                                    let sent = a.send(password);
                                    if sent {
                                        r.queued = !std::mem::take(&mut r.asking);
                                    }
                                    sent
                                }
                                // Ending: the next conversation takes it,
                                // while a lock is shown.
                                None if shown(&lock) && r.next.is_none() => {
                                    r.next = Some(password);
                                    true
                                }
                                None => false,
                            },
                            None => {
                                if !begin(&mut cx, &mut client, &mut running, Some(password)) {
                                    return Ok(());
                                }
                                continue;
                            }
                        };
                        if sent
                            && !cx.update(|s| {
                                s.busy = true;
                                s.failed = false;
                                s.prompt.clear();
                            })
                        {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }
}
