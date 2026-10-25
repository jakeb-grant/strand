//! (services-round) `polkit`: Strand's own polkit authentication agent,
//! so a program asking for something that needs a password (`pkexec`, a
//! settings page) gets a dialog the config draws, as the lock screen's
//! is.
//!
//! **Registration.** The store runs only while a config reads `polkit`
//! (services start on their first reader and stop 5 s after the last), so
//! Strand is an agent only then. Running, it exports
//! `org.freedesktop.PolicyKit1.AuthenticationAgent` at [`AGENT_PATH`] on
//! the system bus and registers it with polkit's authority for its login
//! session (`RegisterAuthenticationAgent` with a `unix-session` subject:
//! logind's `GetSession("auto")`, which falls back to the user's
//! graphical session as polkit itself does, else the session in
//! `/proc/self/cgroup`, else `$XDG_SESSION_ID`). When another agent
//! already serves the session polkit refuses, and Strand leaves it be:
//! `available` is false, `error` says so, a warning diagnostic names it
//! once, and the store registers again only when polkit says its agents
//! changed (its `Changed` signal: that agent left) or polkit restarts. It
//! never unregisters another agent. Stopping, it unregisters and refuses
//! the requests still waiting. Without a helper to check passwords
//! ([`helper::Helper::find`]) it does not register, so another agent
//! can.
//!
//! **Requests.** polkit's `BeginAuthentication` (only from the
//! authority's own bus name: anyone else is refused) becomes a
//! [`PolkitRequest`], shown in turn: `request` is the first, `waiting`
//! counts the rest, and the D-Bus call is answered when it ends. Its
//! identities become `users` (a `unix-user`'s name, a `unix-group`'s
//! members), `user` the one asked for (this process's user when among
//! them, else the first; `choose(user)` picks another). The shown one
//! runs a conversation with polkit's helper ([`helper::converse`]): its
//! questions become `prompt` and `echo`, its texts `info`, and
//! `respond(answer)` answers the question waiting, or the next one asked.
//! The helper, not Strand, checks the password and tells the authority;
//! a `SUCCESS` ends the request (the call answered), a `FAILURE` after an
//! answer sets `failed` and begins a new conversation, so the dialog asks
//! again. One that ended with no answer, or could not be made (no
//! helper, a crash), sets `error`, is a warning diagnostic when an answer
//! was in it, and waits for the next `respond` to try again, so a broken
//! stack cannot loop. `cancel()` dismisses the request (the call fails
//! `org.freedesktop.PolicyKit1.Error.Cancelled`); the authority's
//! `CancelAuthentication` (the program gave up) withdraws it, shown or
//! waiting.
//!
//! **Answers.** An answer is a [`Password`] from the moment it arrives,
//! wiped once written to the helper or dropped; one the helper could not
//! read whole (a line break, a NUL, more than 511 bytes) is refused, not
//! cut. `polkit` is one of [`strand_services_schema::SECRET`]: the binary
//! never records or snapshots it, nor runs its actions from `strand
//! call`, and the dialog's `type: password` input is redacted from logs
//! and faults as the lock's is.

pub mod helper;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use strand_auth::Password;
use tokio::sync::{mpsc, oneshot};
use zbus::zvariant::{OwnedValue, Value};

use crate::dbus::{self, Daemon, DaemonEvent};
use crate::{Call, Cx, Msg, ServiceError, Store, service};
use helper::{Helper, Said};

/// The schema the `polkit` service serves.
pub const SCHEMA: &str = strand_services_schema::POLKIT;

/// polkit's authority: its bus name.
pub const AUTHORITY: &str = "org.freedesktop.PolicyKit1";
/// Its object.
pub const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
/// Its interface.
pub const AUTHORITY_IFACE: &str = "org.freedesktop.PolicyKit1.Authority";
/// Where Strand's agent is exported.
pub const AGENT_PATH: &str = "/org/freedesktop/PolicyKit1/AuthenticationAgent";
/// The agent's interface.
pub const AGENT_IFACE: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";

/// What the `polkit` store uses from its next start on (tests: a fake
/// helper).
#[derive(Clone, Debug, Default)]
pub struct PolkitConfig {
    /// The setuid helper; `None` looks it up ([`helper::HELPER_DIRS`]).
    pub helper: Option<PathBuf>,
    /// The helper's socket; `None` is polkit's ([`helper::SOCKET`]).
    pub socket: Option<PathBuf>,
}

static CONFIG: Mutex<Option<PolkitConfig>> = Mutex::new(None);

/// Sets what the store uses from its next start. `None` restores the
/// defaults. A running store keeps its config until it stops.
pub fn configure(config: Option<PolkitConfig>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> PolkitConfig {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// `polkit`'s actions.
#[derive(Call)]
pub enum PolkitAction {
    /// `polkit.respond(answer)`.
    Respond { answer: String },
    /// `polkit.cancel()`.
    Cancel,
    /// `polkit.choose(user)`.
    Choose { user: String },
}

impl std::fmt::Debug for PolkitAction {
    /// An answer shows only its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolkitAction::Respond { answer } => write!(f, "Respond(<{} bytes>)", answer.len()),
            PolkitAction::Cancel => f.write_str("Cancel"),
            PolkitAction::Choose { user } => write!(f, "Choose({user:?})"),
        }
    }
}

/// A request a dialog shows: the schema's `PolkitRequest`.
#[derive(crate::Data, Clone, Debug, Default, PartialEq, Eq)]
#[data(name = "PolkitRequest")]
pub struct PolkitRequest {
    pub action_id: String,
    pub message: String,
    pub icon: String,
    pub user: String,
    pub users: Vec<String>,
    pub prompt: String,
    pub echo: bool,
    pub info: String,
}

/// See the module docs.
#[service(name = "polkit", action = PolkitAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Polkit {
    /// The request being answered, or null when none waits.
    pub request: Option<PolkitRequest>,
    /// How many more requests wait behind `request`, each shown in turn.
    pub waiting: i64,
    /// An answer is being checked.
    pub busy: bool,
    /// The last answer was refused: `request` stays, and its prompt asks
    /// again.
    pub failed: bool,
    /// Strand is this session's polkit agent: false while another agent
    /// is (a desktop's own, `polkit-gnome`), or without polkit, when
    /// `error` says why.
    pub available: bool,
    /// Why Strand is not the agent ("another polkit agent serves this
    /// session"), or why the last request could not be answered (no
    /// helper); "" otherwise.
    pub error: String,
}

/// What the authority asked of the agent.
enum Asked {
    Begin(Box<Begin>, oneshot::Sender<Result<(), AgentError>>),
    Cancel(String),
}

/// A `BeginAuthentication` call.
struct Begin {
    cookie: String,
    request: PolkitRequest,
}

/// polkit's errors, as its agents answer `BeginAuthentication`.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
}

/// The authority's current unique name, for checking callers.
type Owner = Arc<Mutex<Option<String>>>;

/// The exported `AuthenticationAgent`.
struct Agent {
    tx: mpsc::UnboundedSender<Asked>,
    authority: Owner,
}

impl Agent {
    fn sent_by_authority(&self, h: &zbus::message::Header<'_>) -> bool {
        let owner = self
            .authority
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        matches!((owner, h.sender()), (Some(o), Some(s)) if o == s.as_str())
    }
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl Agent {
    /// Shows the request (in turn) and answers once it ends.
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        action_id: String,
        message: String,
        icon_name: String,
        _details: HashMap<String, String>,
        cookie: String,
        identities: Vec<(String, HashMap<String, OwnedValue>)>,
    ) -> Result<(), AgentError> {
        if !self.sent_by_authority(&h) {
            return Err(AgentError::Failed(
                "only polkit's authority is answered".into(),
            ));
        }
        let users = users_of(&identities);
        let Some(user) = default_user(&users) else {
            return Err(AgentError::Failed(
                "no user of this machine to authenticate as".into(),
            ));
        };
        let request = PolkitRequest {
            action_id,
            message,
            icon: icon_name,
            user,
            users,
            ..PolkitRequest::default()
        };
        let (tx, rx) = oneshot::channel();
        let begin = Box::new(Begin { cookie, request });
        if self.tx.send(Asked::Begin(begin, tx)).is_err() {
            return Err(AgentError::Cancelled("Strand's agent stopped".into()));
        }
        rx.await
            .unwrap_or_else(|_| Err(AgentError::Cancelled("Strand's agent stopped".into())))
    }

    /// The program gave up: its request goes.
    async fn cancel_authentication(
        &self,
        #[zbus(header)] h: zbus::message::Header<'_>,
        cookie: String,
    ) -> Result<(), AgentError> {
        if !self.sent_by_authority(&h) {
            return Err(AgentError::Failed(
                "only polkit's authority is answered".into(),
            ));
        }
        let _ = self.tx.send(Asked::Cancel(cookie));
        Ok(())
    }
}

// ---- users -----------------------------------------------------------------

/// The user names `identities` stand for, each once, in order: a
/// `unix-user`'s, and a `unix-group`'s members (as polkit's own dialogs
/// list them). Identities this machine cannot name are left out.
pub fn users_of(identities: &[(String, HashMap<String, OwnedValue>)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |name: String| {
        if !out.contains(&name) {
            out.push(name);
        }
    };
    for (kind, details) in identities {
        let id = |key: &str| details.get(key).and_then(|v| v.downcast_ref::<u32>().ok());
        match kind.as_str() {
            "unix-user" => {
                if let Some(name) = id("uid").and_then(user_name) {
                    add(name);
                }
            }
            "unix-group" => {
                for name in id("gid").map(group_members).unwrap_or_default() {
                    add(name);
                }
            }
            _ => {}
        }
    }
    out
}

/// The user asked for first: this process's when among `users`, else the
/// first.
fn default_user(users: &[String]) -> Option<String> {
    // SAFETY: getuid cannot fail.
    let me = user_name(unsafe { libc::getuid() });
    me.filter(|m| users.contains(m))
        .or_else(|| users.first().cloned())
}

/// The login name of `uid`.
pub fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: zeroed is a valid `passwd`; getpwuid_r writes into it
        // and `buf`, whose length it is told.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: as above.
        let r =
            unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
        if r == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if r != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r succeeded: pw_name is a C string in `buf`.
        let name = unsafe { std::ffi::CStr::from_ptr(pw.pw_name) };
        return name.to_str().ok().map(str::to_string);
    }
}

/// The members `gid`'s group lists.
pub fn group_members(gid: u32) -> Vec<String> {
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: zeroed is a valid `group`; getgrgid_r writes into it
        // and `buf`, whose length it is told.
        let mut gr: libc::group = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as above.
        let r =
            unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr().cast(), buf.len(), &mut out) };
        if r == libc::ERANGE && buf.len() < 1 << 22 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if r != 0 || out.is_null() || gr.gr_mem.is_null() {
            return Vec::new();
        }
        let mut names = Vec::new();
        let mut i = 0;
        loop {
            // SAFETY: gr_mem is a NULL-terminated array of C strings in
            // `buf` (getgrgid_r succeeded).
            let p = unsafe { *gr.gr_mem.add(i) };
            if p.is_null() {
                break;
            }
            // SAFETY: as above.
            if let Ok(n) = unsafe { std::ffi::CStr::from_ptr(p) }.to_str() {
                names.push(n.to_string());
            }
            i += 1;
        }
        return names;
    }
}

// ---- the session -----------------------------------------------------------

/// The login session a `/proc/<pid>/cgroup` text places a process in, as
/// libsystemd reads it (`sd_pid_get_session`): the `session-<id>.scope`
/// in its unified-hierarchy path.
pub fn session_of_cgroup(text: &str) -> Option<String> {
    text.lines()
        .filter(|l| l.starts_with("0::"))
        .flat_map(|l| l[3..].split('/'))
        .filter_map(|part| part.strip_prefix("session-")?.strip_suffix(".scope"))
        .find(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .map(str::to_string)
}

/// This process's login session, as polkit will see it: logind's
/// `GetSession("auto")` (the caller's session, else the user's graphical
/// one), else the session its cgroup names, else `$XDG_SESSION_ID`.
async fn own_session(conn: &zbus::Connection) -> Option<String> {
    let logind = async {
        let path: zbus::zvariant::OwnedObjectPath = dbus::timed(conn.call_method(
            Some(crate::session::LOGIND),
            crate::session::MANAGER,
            Some("org.freedesktop.login1.Manager"),
            "GetSession",
            &("auto",),
        ))
        .await
        .and_then(|m| m.body().deserialize())
        .ok()?;
        let id = dbus::timed(dbus::get(
            conn,
            crate::session::LOGIND,
            path.as_str(),
            "org.freedesktop.login1.Session",
            "Id",
        ))
        .await
        .ok()?;
        id.downcast_ref::<&str>()
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };
    if let Some(id) = logind.await {
        return Some(id);
    }
    std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|t| session_of_cgroup(&t))
        .or_else(|| {
            std::env::var("XDG_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
}

/// The locale polkit translates its messages to.
fn locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "C".into())
}

/// The `unix-session` subject for `session`.
fn subject(session: &str) -> (&'static str, HashMap<&'static str, Value<'_>>) {
    (
        "unix-session",
        HashMap::from([("session-id", Value::from(session))]),
    )
}

/// Why polkit refused a registration: another agent serves the session.
fn held_by_another(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(_, Some(text), _) if text.contains("already exists"))
}

/// The text of a failed call.
fn error_text(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, text, _) => text.clone().unwrap_or_else(|| name.to_string()),
        e => e.to_string(),
    }
}

/// What a store says about another agent holding the session.
pub const HELD: &str = "another polkit authentication agent already serves this session, \
                        so Strand's is not used: stop that agent to use Strand's";

// ---- the requests ----------------------------------------------------------

/// A request with its reply.
struct Pending {
    cookie: String,
    request: PolkitRequest,
    reply: oneshot::Sender<Result<(), AgentError>>,
}

/// The conversation with the helper for the shown request.
struct Conv {
    id: u64,
    task: tokio::task::JoinHandle<()>,
    answers: mpsc::UnboundedSender<Password>,
    /// A question waits with no answer.
    asking: bool,
    /// An answer waits for the next question.
    queued: bool,
    /// An answer was given in it.
    answered: bool,
}

impl Drop for Conv {
    fn drop(&mut self) {
        // Its helper is killed with the task.
        self.task.abort();
    }
}

/// The requests, the first shown, and its conversation.
struct Requests {
    queue: VecDeque<Pending>,
    conv: Option<Conv>,
    next_id: u64,
    helper: Option<Helper>,
    said: mpsc::UnboundedSender<(u64, Said)>,
}

impl Requests {
    /// Begins a conversation for the shown request, with `answer` waiting
    /// for its first question.
    fn begin(&mut self, answer: Option<Password>) -> Result<(), String> {
        self.conv = None;
        let Some(front) = self.queue.front() else {
            return Ok(());
        };
        let Some(helper) = self.helper.clone() else {
            return Err("polkit's password helper is not installed".into());
        };
        let (answers, rx) = mpsc::unbounded_channel();
        let given = answer.is_some();
        if let Some(a) = answer {
            let _ = answers.send(a);
        }
        self.next_id += 1;
        let id = self.next_id;
        let (tx, mut said_rx) = mpsc::unbounded_channel();
        let out = self.said.clone();
        let user = front.request.user.clone();
        let cookie = front.cookie.clone();
        let task = tokio::spawn(async move {
            let talk = helper::converse(helper, user, cookie, rx, tx);
            let forward = async {
                while let Some(s) = said_rx.recv().await {
                    if out.send((id, s)).is_err() {
                        break;
                    }
                }
            };
            tokio::join!(talk, forward);
        });
        self.conv = Some(Conv {
            id,
            task,
            answers,
            asking: false,
            queued: given,
            answered: given,
        });
        Ok(())
    }

    /// Ends the shown request with `outcome` and shows the next.
    fn finish(&mut self, outcome: Result<(), AgentError>) -> Result<(), String> {
        self.conv = None;
        if let Some(p) = self.queue.pop_front() {
            let _ = p.reply.send(outcome);
        }
        self.begin(None)
    }

    /// Refuses every request (the store stops, polkit went).
    fn drop_all(&mut self, why: &str) {
        self.conv = None;
        for p in self.queue.drain(..) {
            let _ = p.reply.send(Err(AgentError::Cancelled(why.to_string())));
        }
    }
}

// ---- the store -------------------------------------------------------------

impl Polkit {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match cx.system().await {
            Ok(c) => c,
            Err(e) => return dbus::idle_without_bus(&mut cx, "system", e).await,
        };
        let config = config();
        let found = Helper::find(config.socket.as_deref(), config.helper.as_deref());
        let mut daemon = Daemon::new(&conn, AUTHORITY)
            .await
            .map_err(|e| ServiceError(format!("polkit: {e}")))?;
        if let Ok(rule) = dbus::path_rule(AUTHORITY_PATH)
            && let Err(e) = daemon.subscribe("authority", rule).await
        {
            log::debug!("polkit: no Changed signals: {e}");
        }
        let owner: Owner = Arc::new(Mutex::new(daemon.owner().map(|o| o.to_string())));
        let (asked_tx, mut asked) = mpsc::unbounded_channel();
        // The last run's teardown first: it must not take down this one's.
        let prev = TEARDOWN
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(t) = prev {
            let _ = t.await;
        }
        let server = conn.object_server();
        let _ = server.remove::<Agent, _>(AGENT_PATH).await;
        let agent = Agent {
            tx: asked_tx,
            authority: owner.clone(),
        };
        server
            .at(AGENT_PATH, agent)
            .await
            .map_err(|e| ServiceError(format!("polkit: the agent could not be exported: {e}")))?;
        let session = own_session(&conn).await;
        let mut reg = Exported {
            conn: conn.clone(),
            session: session.clone(),
            registered: false,
            warned_held: false,
        };
        let (said_tx, mut said) = mpsc::unbounded_channel();
        let mut reqs = Requests {
            queue: VecDeque::new(),
            conv: None,
            next_id: 0,
            helper: found.clone(),
            said: said_tx,
        };
        let helped = found.is_some();
        if !register(
            &mut cx,
            &mut reg,
            &conn,
            &daemon,
            session.as_deref(),
            helped,
        )
        .await
        {
            return Ok(());
        }
        cx.ready();
        let end = loop {
            tokio::select! {
                ev = daemon.next() => match ev {
                    None => break Err(ServiceError("the system bus connection ended".into())),
                    Some(DaemonEvent::Owner) => {
                        *owner.lock().unwrap_or_else(PoisonError::into_inner) =
                            daemon.owner().map(|o| o.to_string());
                        // polkit restarted or went: its requests went with it,
                        // and a new one knows no agent.
                        reqs.drop_all("polkit restarted");
                        reg.registered = false;
                        if !publish(&mut cx, &reqs, |s| s.busy = false) {
                            break Ok(());
                        }
                        if !register(&mut cx, &mut reg, &conn, &daemon, session.as_deref(), helped)
                            .await
                        {
                            break Ok(());
                        }
                    }
                    // Its agents changed (another one left): try again.
                    Some(DaemonEvent::Signal(m)) => {
                        if !reg.registered
                            && dbus::member(&m).as_deref() == Some("Changed")
                            && !register(&mut cx, &mut reg, &conn, &daemon, session.as_deref(), helped)
                                .await
                        {
                            break Ok(());
                        }
                    }
                },
                Some(a) = asked.recv() => {
                    if !self::asked(&mut cx, &mut reqs, a) {
                        break Ok(());
                    }
                }
                Some((id, s)) = said.recv() => {
                    if reqs.conv.as_ref().is_some_and(|c| c.id == id)
                        && !self::said(&mut cx, &mut reqs, s)
                    {
                        break Ok(());
                    }
                }
                m = cx.recv() => match m {
                    None => break Ok(()),
                    Some(Msg::Action(a)) => {
                        if !act(&mut cx, &mut reqs, a) {
                            break Ok(());
                        }
                    }
                    Some(_) => {}
                },
            }
        };
        // Unregistered and taken down by `reg`'s drop, as when the store
        // stops (the body is dropped then, not returned from).
        reqs.drop_all("Strand's agent stopped");
        end
    }
}

/// The last stopped run's teardown, awaited by the next run before it
/// exports its own agent.
static TEARDOWN: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// The exported agent and its registration: dropped with the body
/// (returned from, or stopped, when the body is dropped mid-await), it
/// unregisters and takes the agent down on a task of its own, so polkit
/// never calls an agent that is gone and another agent can register.
struct Exported {
    conn: zbus::Connection,
    session: Option<String>,
    registered: bool,
    /// Another agent's hold on the session was warned about.
    warned_held: bool,
}

impl Drop for Exported {
    fn drop(&mut self) {
        let conn = self.conn.clone();
        let unregister = self.session.clone().filter(|_| self.registered);
        let task = async move {
            if let Some(session) = unregister {
                let r = dbus::timed(conn.call_method(
                    Some(AUTHORITY),
                    AUTHORITY_PATH,
                    Some(AUTHORITY_IFACE),
                    "UnregisterAuthenticationAgent",
                    &(subject(&session), AGENT_PATH),
                ))
                .await;
                match r {
                    Ok(_) => log::info!("polkit: Strand's agent unregistered"),
                    Err(e) => log::debug!("polkit: unregistering: {e}"),
                }
            }
            let _ = conn.object_server().remove::<Agent, _>(AGENT_PATH).await;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                let t = h.spawn(task);
                *TEARDOWN.lock().unwrap_or_else(PoisonError::into_inner) = Some(t);
            }
            // No runtime (the process ends): polkit drops the agent with
            // its connection.
            Err(_) => log::debug!("polkit: no runtime to unregister on"),
        }
    }
}

/// Registers Strand's agent unless it is, publishing whether it is and
/// why not; `false` when the store has stopped.
async fn register(
    cx: &mut Cx<Polkit>,
    reg: &mut Exported,
    conn: &zbus::Connection,
    daemon: &Daemon,
    session: Option<&str>,
    helped: bool,
) -> bool {
    if reg.registered {
        return true;
    }
    let (registered, error, held) = if !helped {
        (
            false,
            "polkit's password helper (polkit-agent-helper-1) is not installed: \
             Strand cannot check polkit's passwords"
                .to_string(),
            false,
        )
    } else if daemon.owner().is_none() {
        (false, "polkit is not running".to_string(), false)
    } else if let Some(session) = session {
        let r = dbus::timed(conn.call_method(
            Some(AUTHORITY),
            AUTHORITY_PATH,
            Some(AUTHORITY_IFACE),
            "RegisterAuthenticationAgent",
            &(subject(session), locale(), AGENT_PATH),
        ))
        .await;
        match r {
            Ok(_) => (true, String::new(), false),
            Err(e) if held_by_another(&e) => (false, HELD.to_string(), true),
            Err(e) => (
                false,
                format!("polkit refused Strand's agent: {}", error_text(&e)),
                false,
            ),
        }
    } else {
        (
            false,
            "this process is in no login session, so polkit has none to register \
             Strand's agent for"
                .to_string(),
            false,
        )
    };
    reg.registered = registered;
    if registered {
        log::info!("polkit: Strand is this session's authentication agent");
    } else if held {
        // Once per run: which agent serves the session is the user's
        // choice, and polkit says when it changes.
        if !std::mem::replace(&mut reg.warned_held, true) {
            cx.warn(error.clone());
        }
    } else if cx.state().error != error {
        cx.warn(error.clone());
    }
    cx.update(|s| {
        s.available = registered;
        s.error = error;
    })
}

/// Publishes the queue's shown request and count, after `also`.
fn publish(cx: &mut Cx<Polkit>, reqs: &Requests, also: impl FnOnce(&mut Polkit)) -> bool {
    let shown = reqs.queue.front().map(|p| p.request.clone());
    let waiting = reqs.queue.len().saturating_sub(1) as i64;
    cx.update(|s| {
        s.request = shown;
        s.waiting = waiting;
        also(s);
    })
}

/// The shown request, to change.
fn front(reqs: &mut Requests) -> Option<&mut PolkitRequest> {
    reqs.queue.front_mut().map(|p| &mut p.request)
}

/// Starts the shown request's conversation (and says why it could not).
fn begin_shown(cx: &mut Cx<Polkit>, reqs: &mut Requests, answer: Option<Password>) -> bool {
    let given = answer.is_some();
    match reqs.begin(answer) {
        Ok(()) => publish(cx, reqs, |s| {
            if given {
                s.busy = true;
            }
        }),
        Err(why) => {
            cx.warn(why.clone());
            publish(cx, reqs, |s| {
                s.busy = false;
                s.error = why;
            })
        }
    }
}

/// The authority asked something.
fn asked(cx: &mut Cx<Polkit>, reqs: &mut Requests, a: Asked) -> bool {
    match a {
        Asked::Begin(b, reply) => {
            let Begin { cookie, request } = *b;
            if reqs.queue.iter().any(|p| p.cookie == cookie) {
                let _ = reply.send(Err(AgentError::Failed(
                    "a request with this cookie is already shown".into(),
                )));
                return true;
            }
            reqs.queue.push_back(Pending {
                cookie,
                request,
                reply,
            });
            if reqs.queue.len() == 1 {
                return begin_shown(cx, reqs, None)
                    && publish(cx, reqs, |s| {
                        s.failed = false;
                        s.busy = false;
                    });
            }
            publish(cx, reqs, |_| {})
        }
        Asked::Cancel(cookie) => {
            let Some(i) = reqs.queue.iter().position(|p| p.cookie == cookie) else {
                return true;
            };
            let gone = Err(AgentError::Cancelled("the program withdrew it".into()));
            if i == 0 {
                let r = reqs.finish(gone);
                return shown_next(cx, reqs, r);
            }
            if let Some(p) = reqs.queue.remove(i) {
                let _ = p.reply.send(gone);
            }
            publish(cx, reqs, |_| {})
        }
    }
}

/// After the shown request ended: the next one shows (or why its
/// conversation could not begin).
fn shown_next(cx: &mut Cx<Polkit>, reqs: &mut Requests, began: Result<(), String>) -> bool {
    let error = match began {
        Ok(()) => String::new(),
        Err(why) => {
            cx.warn(why.clone());
            why
        }
    };
    let available = cx.state().available;
    publish(cx, reqs, |s| {
        s.busy = false;
        s.failed = false;
        s.error = if error.is_empty() && !available {
            s.error.clone()
        } else {
            error
        };
    })
}

/// The helper said `s` in the shown request's conversation.
fn said(cx: &mut Cx<Polkit>, reqs: &mut Requests, s: Said) -> bool {
    match s {
        Said::Prompt { text, echo } => {
            let Some(c) = reqs.conv.as_mut() else {
                return true;
            };
            let queued = std::mem::take(&mut c.queued);
            c.asking = !queued;
            if let Some(r) = front(reqs) {
                r.info.clear();
                r.prompt = if queued { String::new() } else { text };
                r.echo = echo;
            }
            publish(cx, reqs, |s| s.busy = queued)
        }
        Said::Text(t) => {
            if let Some(r) = front(reqs) {
                r.info = t;
            }
            publish(cx, reqs, |_| {})
        }
        Said::Done(Ok(true)) => {
            let r = reqs.finish(Ok(()));
            shown_next(cx, reqs, r)
        }
        Said::Done(Ok(false)) => {
            let answered = reqs.conv.as_ref().is_some_and(|c| c.answered);
            reqs.conv = None;
            if let Some(r) = front(reqs) {
                r.prompt.clear();
            }
            if answered {
                // Refused: the dialog asks again.
                return begin_shown(cx, reqs, None)
                    && publish(cx, reqs, |s| {
                        s.failed = true;
                        s.busy = false;
                    });
            }
            // It ended before asking anything (a lockout, a broken
            // stack): the next answer tries again.
            publish(cx, reqs, |s| {
                s.busy = false;
                s.error = "polkit's check ended before it asked for a password".into();
            })
        }
        Said::Done(Err(why)) => {
            let answered = reqs.conv.as_ref().is_some_and(|c| c.answered);
            reqs.conv = None;
            if let Some(r) = front(reqs) {
                r.prompt.clear();
            }
            if answered {
                cx.warn(why.clone());
            } else {
                log::info!("polkit: {why}");
            }
            publish(cx, reqs, |s| {
                s.busy = false;
                s.failed = s.failed || answered;
                s.error = why;
            })
        }
    }
}

/// An action from the config.
fn act(cx: &mut Cx<Polkit>, reqs: &mut Requests, a: PolkitAction) -> bool {
    if reqs.queue.is_empty() {
        return true;
    }
    match a {
        PolkitAction::Respond { answer } => {
            let answer = Password::from(answer);
            if cx.state().busy {
                // One check at a time; this one is dropped (and wiped).
                return true;
            }
            if let Some(why) = helper::refusal(&answer) {
                log::info!("polkit: an answer was refused: {why}");
                return cx.update(|s| s.failed = true);
            }
            let Some(c) = reqs.conv.as_mut() else {
                // Ended before asking, or broken: this answer tries again.
                return begin_shown(cx, reqs, Some(answer))
                    && publish(cx, reqs, |s| {
                        s.failed = false;
                        s.error.clear();
                    });
            };
            if c.queued || c.answers.send(answer).is_err() {
                return true;
            }
            c.answered = true;
            c.queued = !std::mem::take(&mut c.asking);
            if let Some(r) = front(reqs) {
                r.prompt.clear();
            }
            publish(cx, reqs, |s| {
                s.busy = true;
                s.failed = false;
            })
        }
        PolkitAction::Cancel => {
            let r = reqs.finish(Err(AgentError::Cancelled("dismissed".into())));
            shown_next(cx, reqs, r)
        }
        PolkitAction::Choose { user } => {
            let Some(r) = front(reqs) else {
                return true;
            };
            if r.user == user || !r.users.contains(&user) {
                return true;
            }
            r.user = user;
            r.prompt.clear();
            r.info.clear();
            begin_shown(cx, reqs, None)
                && publish(cx, reqs, |s| {
                    s.busy = false;
                    s.failed = false;
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_is_read_from_the_cgroup_as_systemd_does() {
        assert_eq!(
            session_of_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n").as_deref(),
            Some("3")
        );
        assert_eq!(
            session_of_cgroup("12:pids:/x\n0::/user.slice/user-1000.slice/session-c2.scope/sub")
                .as_deref(),
            Some("c2")
        );
        // A user service is in no session.
        assert_eq!(
            session_of_cgroup("0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope"),
            None
        );
        assert_eq!(session_of_cgroup("0::/session-.scope"), None);
    }

    #[test]
    fn identities_become_user_names_each_once() {
        let user = |uid: u32| {
            (
                "unix-user".to_string(),
                HashMap::from([("uid".to_string(), OwnedValue::from(uid))]),
            )
        };
        let ids = vec![
            user(0),
            ("unix-netgroup".to_string(), HashMap::new()),
            user(0),
            // A uid with no user is left out.
            user(4_000_000_000),
        ];
        assert_eq!(users_of(&ids), vec!["root".to_string()]);
        assert_eq!(default_user(&[]), None);
        let me = user_name(unsafe { libc::getuid() }).unwrap();
        assert_eq!(
            default_user(&["root".to_string(), me.clone()]).as_deref(),
            Some(me.as_str())
        );
    }

    #[test]
    fn an_answer_never_shows_in_debug() {
        let a = PolkitAction::Respond {
            answer: "hunter2".into(),
        };
        assert_eq!(format!("{a:?}"), "Respond(<7 bytes>)");
    }
}
