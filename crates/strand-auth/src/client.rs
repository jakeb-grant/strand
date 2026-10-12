//! The blocking client: spawns the helper, runs its PAM conversations,
//! answers PAM's prompts with passwords, mints [`UnlockToken`]s from its
//! success replies.
//!
//! A conversation's deadlines (decisions.md, m6-lock): while no password
//! is waiting for PAM or with it, there is none, since PAM may wait on
//! the user (a fingerprint reader) for as long as its module likes. Once
//! the helper has a password, PAM must say something (a text, the next
//! prompt or the verdict) within the client's timeout of the last thing
//! it said or was told; and a password must be answered (by the next
//! prompt or the verdict) within [`LIMIT_FACTOR`] timeouts of being sent,
//! including the time it waited for PAM's prompt (a fingerprint module's
//! wait ahead of `pam_unix`).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::Password;
use crate::protocol::{self, Code, HEADER, Message, ProtocolError, Service};

/// The helper's file name.
pub const HELPER_NAME: &str = "strand-auth";

/// How long PAM may stay quiet once it has a password before the helper
/// is killed and the check fails (a PAM module that hangs). Generous:
/// `pam_faildelay` and `pam_faillock` add seconds on purpose.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A password must be answered within this many timeouts of being sent
/// (90 s by default): room for a fingerprint module's wait ahead of
/// `pam_unix` (`pam_fprintd`'s default is 30 s) and for `pam_unix`
/// after it.
pub const LIMIT_FACTOR: u32 = 3;

/// Proof that the helper accepted a password: the only value
/// `strand-surface` releases a session lock for. Only [`Client::submit`]
/// makes one, from a success reply; it is `Send`, neither `Clone` nor
/// `Copy`, and has no public constructor, so no other code path can
/// unlock.
pub struct UnlockToken {
    _private: (),
}

impl UnlockToken {
    fn mint() -> Self {
        Self { _private: () }
    }
}

impl std::fmt::Debug for UnlockToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnlockToken")
    }
}

/// What a password check came to. Only `Unlocked` unlocks.
#[derive(Debug)]
pub enum Verdict {
    Unlocked(UnlockToken),
    /// PAM refused the password (or the account): PAM's message, if it
    /// gave one.
    Denied {
        message: Option<String>,
    },
    /// The check could not be made; the lock stays.
    Failed(AuthError),
}

impl Verdict {
    pub fn is_unlocked(&self) -> bool {
        matches!(self, Verdict::Unlocked(_))
    }
}

/// What PAM said during a conversation, as it said it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said<'a> {
    /// A `PAM_TEXT_INFO` or `PAM_ERROR_MSG` text ("Place your finger on
    /// the fingerprint reader").
    Message(&'a str),
    /// A hidden prompt ("Password: "): PAM waits for the next password.
    Prompt(&'a str),
}

/// What a conversation came to.
#[derive(Debug)]
pub struct Conversed {
    pub verdict: Verdict,
    /// A password was sent through its [`Answers`] (used, or still
    /// waiting when the verdict came: then it was dropped and wiped).
    pub answered: bool,
}

/// The sending half of a conversation's passwords ([`answers`]): held by
/// whoever takes them from the user, on any thread. Dropping it tells
/// the conversation nobody will answer: one with no password waiting for
/// PAM or with it then ends ([`AuthError::Cancelled`]).
#[derive(Debug)]
pub struct Answers {
    queue: Arc<Mutex<Option<Password>>>,
    wake: UnixStream,
}

/// The receiving half, which [`Client::converse`] takes.
#[derive(Debug)]
pub struct Inbox {
    queue: Arc<Mutex<Option<Password>>>,
    wake: UnixStream,
    /// The [`Answers`] were dropped.
    closed: bool,
}

/// A conversation's passwords: one at a time, each for the next prompt.
pub fn answers() -> io::Result<(Answers, Inbox)> {
    let (tx, rx) = UnixStream::pair()?;
    tx.set_nonblocking(true)?;
    rx.set_nonblocking(true)?;
    let queue = Arc::new(Mutex::new(None));
    Ok((
        Answers {
            queue: queue.clone(),
            wake: tx,
        },
        Inbox {
            queue,
            wake: rx,
            closed: false,
        },
    ))
}

impl Answers {
    /// Gives `password` to PAM's next prompt, or to the one waiting.
    /// `false` (and the password wiped) when one is already waiting.
    pub fn send(&self, password: Password) -> bool {
        let Ok(mut queue) = self.queue.lock() else {
            return false;
        };
        if queue.is_some() {
            return false;
        }
        *queue = Some(password);
        drop(queue);
        // A full buffer already holds a wake-up.
        let _ = (&self.wake).write(&[1]);
        true
    }
}

impl Inbox {
    /// Reads the wake-ups; notes when the [`Answers`] are gone.
    fn drain(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            match (&self.wake).read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    return;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
    }

    fn waiting(&self) -> bool {
        self.queue.lock().is_ok_and(|q| q.is_some())
    }

    fn take(&self) -> Option<Password> {
        self.queue.lock().ok().and_then(|mut q| q.take())
    }
}

/// Where a conversation's answers come from.
enum Source {
    /// The user, one password per prompt.
    Inbox(Inbox),
    /// [`Client::submit`]: every prompt gets this password.
    Repeat(Password),
}

/// Why `password` cannot go to PAM: PAM takes at most
/// [`protocol::MAX_PASSWORD`] bytes and stops at a NUL, so a longer one
/// or one with a NUL would be checked as a prefix; each is refused,
/// never cut. The helper refuses them too.
pub fn refusal(password: &Password) -> Option<String> {
    let bytes = password.as_bytes();
    if bytes.contains(&0) {
        return Some("a password cannot contain a NUL byte".into());
    }
    (bytes.len() > protocol::MAX_PASSWORD).then(|| {
        format!(
            "a password longer than {} bytes cannot be checked",
            protocol::MAX_PASSWORD
        )
    })
}

/// Why a check could not be made. Every one fails closed.
#[derive(Debug)]
pub enum AuthError {
    /// The helper could not be started (missing, not executable).
    Spawn(io::Error),
    /// The helper did not answer within the timeout; it was killed.
    Timeout,
    /// The helper went away before answering.
    HelperDied,
    /// PAM asked for a password and nobody was left to give one (the
    /// [`Answers`] were dropped): the helper was killed.
    Cancelled,
    /// The helper sent something that is not the protocol; it was
    /// killed.
    Protocol(ProtocolError),
    /// The socket failed.
    Io(io::Error),
    /// The helper reached PAM and PAM failed (a broken stack, a module
    /// error, an unknown user): its message.
    Pam(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "cannot start the PAM helper: {e}"),
            Self::Timeout => write!(f, "the PAM helper did not answer in time"),
            Self::HelperDied => write!(f, "the PAM helper stopped"),
            Self::Cancelled => write!(f, "nobody was left to answer PAM"),
            Self::Protocol(e) => write!(f, "the PAM helper answered garbage: {e}"),
            Self::Io(e) => write!(f, "talking to the PAM helper: {e}"),
            Self::Pam(m) => write!(f, "PAM: {m}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Where the helper is installed: next to the running executable (the
/// build tree; `/usr/bin` beside `strand`), else the usual libexec
/// paths. `None` when there is none; [`Client::submit`] then fails.
pub fn default_helper() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join(HELPER_NAME));
        // `cargo test` runs test binaries from `target/<profile>/deps`.
        if dir.ends_with("deps")
            && let Some(up) = dir.parent()
        {
            candidates.push(up.join(HELPER_NAME));
        }
    }
    for dir in [
        "/usr/libexec/strand",
        "/usr/lib/strand",
        "/usr/local/libexec/strand",
    ] {
        candidates.push(Path::new(dir).join(HELPER_NAME));
    }
    candidates.into_iter().find(|p| p.is_file())
}

static WARNING: AtomicBool = AtomicBool::new(false);
static WARNED: AtomicBool = AtomicBool::new(false);

/// The one-time warning that the `login` service stands in for a
/// `strand` service libpam would not read (decisions.md, m4-owner; the
/// helper's rule, m4-audit: `/etc/pam.d/strand` missing or unreadable,
/// or only in a `/usr/lib/pam.d` this libpam ignores): `Some` once per
/// process, after a helper reported the fallback. The hello says only
/// that it fell back, so the text names every case.
pub fn take_service_warning() -> Option<String> {
    if !WARNING.load(Ordering::SeqCst) || WARNED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(
        "no `strand` PAM service libpam reads (/etc/pam.d/strand is missing or unreadable, \
         or only in /usr/lib/pam.d, which this libpam does not read), so the lock screen \
         checks passwords with the `login` PAM service; install a readable \
         /etc/pam.d/strand with your system's stack (`auth include system-auth` on Arch and \
         Fedora, `@include common-auth` on Debian and Ubuntu) to configure it separately"
            .to_string(),
    )
}

/// A running helper.
struct Helper {
    child: Child,
    sock: UnixStream,
    /// Its `HELLO` was read.
    service: Option<Service>,
}

impl Helper {
    /// The helper if it still runs. One that exited was reaped by
    /// `try_wait` (`waitpid`), so its pid, and its process group's once
    /// that is empty, may already belong to another process: it is only
    /// dropped, never signalled. An idle helper runs no PAM conversation,
    /// so nothing it started is left to kill. One whose state cannot be
    /// read is killed with its tree as usual.
    fn running(mut self) -> Option<Helper> {
        match self.child.try_wait() {
            Ok(None) => Some(self),
            Ok(Some(_)) => None,
            Err(_) => {
                self.kill();
                None
            }
        }
    }

    /// Kills the helper with everything it started (a hung PAM module's
    /// processes, which may have left its process group) and reaps it.
    /// Only for a helper not yet reaped (see [`Helper::running`]).
    fn kill(mut self) {
        #[cfg(test)]
        tests::KILLS.with(|k| k.set(k.get() + 1));
        let pid = self.child.id() as libc::pid_t;
        // Stopped first, it forks nothing more while its tree is read.
        // SAFETY: kill(2) takes no pointers; `pid` is our child, not yet
        // reaped, so the pid cannot have been reused.
        unsafe {
            libc::kill(pid, libc::SIGSTOP);
        }
        for p in descendants(pid) {
            // SAFETY: as above; a descendant that exited meanwhile is a
            // zombie of its parent, which is stopped, so its pid is not
            // reused either.
            unsafe {
                libc::kill(p, libc::SIGKILL);
            }
        }
        // SAFETY: a negative pid signals the helper's own process group
        // (`process_group(0)`).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every process below `root`, from `/proc` (each process's parent is
/// the fourth field of its `stat`).
fn descendants(root: libc::pid_t) -> Vec<libc::pid_t> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let parents: Vec<(libc::pid_t, libc::pid_t)> = dir
        .flatten()
        .filter_map(|e| {
            let pid: libc::pid_t = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            let (_, rest) = stat.rsplit_once(')')?;
            let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
            Some((pid, ppid))
        })
        .collect();
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for (child, _) in parents.iter().filter(|(_, pp)| *pp == p) {
            if !out.contains(child) && *child != root {
                out.push(*child);
                frontier.push(*child);
            }
        }
    }
    out
}

/// Talks to one helper process over a socketpair, one conversation at a
/// time; fork+execs it again when it has died. Blocking: the `auth`
/// service and the binary's
/// fallback lock (its `strand-lock-auth` thread) each call it on a thread
/// of their own, never on the main thread.
pub struct Client {
    helper_path: PathBuf,
    pre_exec: fn(),
    timeout: Duration,
    helper: Option<Helper>,
    spawns: u32,
    #[cfg(feature = "faults")]
    test_env: Vec<(String, String)>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("helper", &self.helper_path)
            .field("timeout", &self.timeout)
            .field("pid", &self.helper_pid())
            .field("spawns", &self.spawns)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the helper at `helper`. `pre_exec` runs in the child
    /// between fork and exec, in both the first spawn and every respawn:
    /// its owner passes `strand_services::child::restore_in_child`
    /// (docs/architecture.md, "Crate graph"). It must be
    /// async-signal-safe. The helper starts at once, so it is ready by
    /// the first password; a failed start is retried then.
    pub fn new(helper: PathBuf, pre_exec: fn()) -> Self {
        let mut c = Self {
            helper_path: helper,
            pre_exec,
            timeout: DEFAULT_TIMEOUT,
            helper: None,
            spawns: 0,
            #[cfg(feature = "faults")]
            test_env: Vec::new(),
        };
        let _ = c.ensure_helper();
        c
    }

    /// How long PAM may stay quiet once it has a password
    /// ([`DEFAULT_TIMEOUT`]); a password must be answered within
    /// [`LIMIT_FACTOR`] times it.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// (`faults`) Passes `vars` through the helper's scrubbed environment
    /// (`STRAND_FAULT`, `STRAND_AUTH_PAM_CONFDIR`); the helper is started
    /// again with them, and [`Client::spawns`] counts from that start.
    #[cfg(feature = "faults")]
    pub fn with_test_env(mut self, vars: &[(&str, &str)]) -> Self {
        self.test_env = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some(h) = self.helper.take() {
            h.kill();
        }
        self.spawns = 0;
        let _ = self.ensure_helper();
        self
    }

    /// The running helper's pid.
    pub fn helper_pid(&self) -> Option<u32> {
        self.helper.as_ref().map(|h| h.child.id())
    }

    /// How many times a helper was started.
    pub fn spawns(&self) -> u32 {
        self.spawns
    }

    /// The PAM service the running helper uses, once it has said.
    pub fn service(&self) -> Option<Service> {
        self.helper.as_ref().and_then(|h| h.service)
    }

    /// Checks `password` (wiped once the check ends): one conversation
    /// whose every hidden prompt is answered with it, as a login prompt
    /// fed one password would be (the fallback lock's field; tests).
    /// Blocks until the verdict or a deadline (the module docs; the whole
    /// check within [`LIMIT_FACTOR`] timeouts). Only a success reply
    /// mints an [`UnlockToken`]; anything else, including every failure
    /// to ask, is not an unlock. A password PAM cannot take
    /// ([`refusal`]) is denied without asking. A denial's or an error's
    /// message joins what PAM said on the way.
    pub fn submit(&mut self, password: Password) -> Verdict {
        if let Some(why) = refusal(&password) {
            return Verdict::Denied { message: Some(why) };
        }
        let mut said = Vec::new();
        let result = self.run(
            &mut Source::Repeat(password),
            &mut |s| {
                if let Said::Message(m) = s {
                    said.push(m.to_string());
                }
            },
            &mut false,
        );
        let result = result.map(|(code, m)| {
            if !m.is_empty() {
                said.push(m);
            }
            (code, said.join("\n"))
        });
        self.verdict(result)
    }

    /// Runs one PAM conversation: blocks until its verdict. Each text and
    /// prompt goes to `said` as PAM says it, and each prompt is answered
    /// with the next password sent through `inbox`'s [`Answers`] (one
    /// already waiting answers at once). With no password waiting or with
    /// PAM it may wait on the user for as long as PAM does; the deadlines
    /// are in the module docs. Dropping the [`Answers`] ends it while no
    /// password is waiting or with PAM ([`AuthError::Cancelled`]), and
    /// otherwise once PAM next asks. A password PAM cannot take
    /// ([`refusal`]) is wiped unsent, said as a [`Said::Message`], and the
    /// prompt waits for the next.
    pub fn converse(&mut self, inbox: Inbox, said: &mut dyn FnMut(Said<'_>)) -> Conversed {
        let mut answered = false;
        let result = self.run(&mut Source::Inbox(inbox), said, &mut answered);
        Conversed {
            verdict: self.verdict(result),
            answered,
        }
    }

    /// A conversation's result as a verdict; a helper that failed it is
    /// not trusted again.
    fn verdict(&mut self, result: Result<(Code, String), AuthError>) -> Verdict {
        match result {
            Ok((Code::Success, _)) => Verdict::Unlocked(UnlockToken::mint()),
            Ok((Code::Denied, m)) => Verdict::Denied {
                message: (!m.is_empty()).then_some(m),
            },
            Ok((Code::Error, m)) => Verdict::Failed(AuthError::Pam(m)),
            Err(e) => {
                if let Some(h) = self.helper.take() {
                    h.kill();
                }
                Verdict::Failed(e)
            }
        }
    }

    /// Starts the helper unless one is running.
    fn ensure_helper(&mut self) -> Result<(), AuthError> {
        // It may have died while idle (killed, OOM): then another starts.
        self.helper = self.helper.take().and_then(Helper::running);
        if self.helper.is_some() {
            return Ok(());
        }
        let (ours, theirs) = UnixStream::pair().map_err(AuthError::Io)?;
        let theirs = OwnedFd::from(theirs);
        let out = theirs.try_clone().map_err(AuthError::Io)?;
        let mut cmd = Command::new(&self.helper_path);
        cmd.env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .stdin(Stdio::from(theirs))
            .stdout(Stdio::from(out))
            .stderr(Stdio::inherit())
            .process_group(0);
        // PAM's messages in the user's language.
        for key in ["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES", "LC_CTYPE"] {
            if let Some(v) = std::env::var_os(key) {
                cmd.env(key, v);
            }
        }
        #[cfg(feature = "faults")]
        for (k, v) in &self.test_env {
            cmd.env(k, v);
        }
        let hook = self.pre_exec;
        // SAFETY: the closure runs in the forked child before exec. It
        // calls the owner's hook (documented async-signal-safe) and
        // close_range(2), which only marks descriptors close-on-exec, so
        // std's own exec-error pipe keeps working; an old kernel without
        // it fails harmlessly (std's descriptors are CLOEXEC already).
        unsafe {
            cmd.pre_exec(move || {
                hook();
                libc::syscall(
                    libc::SYS_close_range,
                    3 as libc::c_uint,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC as libc::c_uint,
                );
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(AuthError::Spawn)?;
        self.spawns += 1;
        self.helper = Some(Helper {
            child,
            sock: ours,
            service: None,
        });
        Ok(())
    }

    /// One conversation, from `BEGIN` to the verdict (see
    /// [`Client::converse`]); `answered` is set once a password came
    /// from `source`.
    fn run(
        &mut self,
        source: &mut Source,
        said: &mut dyn FnMut(Said<'_>),
        answered: &mut bool,
    ) -> Result<(Code, String), AuthError> {
        let timeout = self.timeout;
        let limit = timeout.saturating_mul(LIMIT_FACTOR);
        // Since when a password has been waiting for PAM or with it.
        let mut since = match source {
            Source::Inbox(inbox) if !inbox.waiting() => None,
            // One sent before the conversation began waits from now.
            _ => {
                *answered = true;
                Some(Instant::now())
            }
        };
        self.ensure_helper()?;
        let Some(h) = self.helper.as_mut() else {
            return Err(AuthError::HelperDied);
        };
        if h.service.is_none() {
            match read_message(&h.sock, Instant::now() + timeout)? {
                Message::Hello { version, service } if version == protocol::VERSION => {
                    if service == Service::Login {
                        WARNING.store(true, Ordering::SeqCst);
                    }
                    h.service = Some(service);
                }
                Message::Hello { .. } => {
                    return Err(AuthError::Protocol(ProtocolError::BadPayload(
                        "another protocol version",
                    )));
                }
                _ => {
                    return Err(AuthError::Protocol(ProtocolError::BadPayload(
                        "the helper did not say hello",
                    )));
                }
            }
        }
        send_all(
            &h.sock,
            &protocol::encode(&Message::Begin),
            Instant::now() + timeout,
        )?;
        // PAM waits for an answer.
        let mut prompt = false;
        // PAM has a password: the last time it said or was told something.
        let mut quiet: Option<Instant> = None;
        loop {
            if prompt {
                let password = match source {
                    Source::Repeat(p) => Some(Password::from_bytes(p.as_bytes().to_vec())),
                    Source::Inbox(inbox) => inbox.take(),
                };
                match password {
                    Some(password) => {
                        *answered = true;
                        if let Some(why) = refusal(&password) {
                            // Wiped unsent; the prompt waits for the next.
                            drop(password);
                            said(Said::Message(&why));
                            since = None;
                        } else {
                            let frame = protocol::encode(&Message::Submit(password));
                            send_all(&h.sock, &frame, Instant::now() + timeout)?;
                            drop(frame);
                            prompt = false;
                            let now = Instant::now();
                            since.get_or_insert(now);
                            quiet = Some(now);
                        }
                    }
                    None => {
                        if matches!(source, Source::Inbox(i) if i.closed) {
                            return Err(AuthError::Cancelled);
                        }
                    }
                }
            }
            let deadline = [since.map(|t| t + limit), quiet.map(|t| t + timeout)]
                .into_iter()
                .flatten()
                .min();
            let wake = match source {
                Source::Inbox(i) if !i.closed => Some(i.wake.as_raw_fd()),
                _ => None,
            };
            let (from_helper, woken) = wait_either(&h.sock, wake, deadline)?;
            if woken && let Source::Inbox(inbox) = source {
                inbox.drain();
                if inbox.waiting() {
                    *answered = true;
                    since.get_or_insert_with(Instant::now);
                }
                if inbox.closed && since.is_none() && !inbox.waiting() {
                    return Err(AuthError::Cancelled);
                }
            }
            if !from_helper {
                continue;
            }
            // A frame, once begun, comes whole within the timeout.
            match read_message(&h.sock, Instant::now() + timeout)? {
                Message::Info(text) => {
                    said(Said::Message(&text));
                    if quiet.is_some() {
                        quiet = Some(Instant::now());
                    }
                }
                Message::Prompt(text) => {
                    said(Said::Prompt(&text));
                    prompt = true;
                    // The password PAM had is answered: the next one
                    // starts its own clock (every prompt of a `submit`
                    // shares one).
                    if quiet.take().is_some() && matches!(source, Source::Inbox(_)) {
                        since = None;
                    }
                }
                Message::Verdict { code, message } => return Ok((code, message)),
                _ => {
                    return Err(AuthError::Protocol(ProtocolError::BadPayload(
                        "a frame the helper does not send in a conversation",
                    )));
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(h) = self.helper.take() {
            h.kill();
        }
    }
}

/// Waits until `fd` is ready for `events` or `deadline` passes.
fn wait(fd: &UnixStream, events: libc::c_short, deadline: Instant) -> Result<(), AuthError> {
    let mut fds = [libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    }];
    poll(&mut fds, Some(deadline))
}

/// Waits until the helper's socket or `wake` is readable (or hung up), or
/// `deadline` passes (`None`: no deadline). Which of them is ready.
fn wait_either(
    sock: &UnixStream,
    wake: Option<RawFd>,
    deadline: Option<Instant>,
) -> Result<(bool, bool), AuthError> {
    let pollfd = |fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // A negative fd is ignored by poll(2).
    let mut fds = [pollfd(sock.as_raw_fd()), pollfd(wake.unwrap_or(-1))];
    poll(&mut fds, deadline)?;
    Ok((fds[0].revents != 0, fds[1].revents != 0))
}

/// poll(2) until something in `fds` is ready, or `deadline` passes.
fn poll(fds: &mut [libc::pollfd], deadline: Option<Instant>) -> Result<(), AuthError> {
    loop {
        let ms = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(AuthError::Timeout);
                }
                left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int
            }
            None => -1,
        };
        // SAFETY: `fds` is a valid array of pollfds for the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(AuthError::Io(e));
        }
        if n > 0 {
            // Readable, writable, or hung up: the next call says which.
            return Ok(());
        }
    }
}

fn send_all(sock: &UnixStream, mut buf: &[u8], deadline: Instant) -> Result<(), AuthError> {
    while !buf.is_empty() {
        wait(sock, libc::POLLOUT, deadline)?;
        // SAFETY: `buf` is valid for `buf.len()` bytes. MSG_NOSIGNAL: a
        // dead helper is an error here, not a SIGPIPE.
        let n = unsafe {
            libc::send(
                sock.as_raw_fd(),
                buf.as_ptr().cast(),
                buf.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => continue,
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => {
                    return Err(AuthError::HelperDied);
                }
                _ => return Err(AuthError::Io(e)),
            }
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

/// Reads exactly `buf.len()` bytes before `deadline`. `Ok(false)` when
/// the stream ended before the first byte.
fn recv_exact(sock: &UnixStream, buf: &mut [u8], deadline: Instant) -> Result<bool, AuthError> {
    let mut got = 0;
    while got < buf.len() {
        wait(sock, libc::POLLIN, deadline)?;
        let rest = &mut buf[got..];
        // SAFETY: `rest` is valid for writes of `rest.len()` bytes.
        let n = unsafe {
            libc::recv(
                sock.as_raw_fd(),
                rest.as_mut_ptr().cast(),
                rest.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => continue,
                io::ErrorKind::ConnectionReset => return Err(AuthError::HelperDied),
                _ => return Err(AuthError::Io(e)),
            }
        }
        if n == 0 {
            return if got == 0 {
                Ok(false)
            } else {
                Err(AuthError::Protocol(ProtocolError::Truncated))
            };
        }
        got += n as usize;
    }
    Ok(true)
}

fn read_message(sock: &UnixStream, deadline: Instant) -> Result<Message, AuthError> {
    let mut head = [0u8; HEADER];
    if !recv_exact(sock, &mut head, deadline)? {
        return Err(AuthError::HelperDied);
    }
    let (kind, len) = protocol::header(head).map_err(AuthError::Protocol)?;
    let mut payload = Zeroizing::new(vec![0u8; len]);
    if len > 0 && !recv_exact(sock, &mut payload, deadline)? {
        return Err(AuthError::Protocol(ProtocolError::Truncated));
    }
    protocol::decode(kind, payload).map_err(AuthError::Protocol)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::Helper;

    thread_local! {
        /// [`Helper::kill`] calls on this thread.
        pub(super) static KILLS: Cell<u32> = const { Cell::new(0) };
    }

    fn helper(script: &str) -> Helper {
        let child = Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let (sock, _) = UnixStream::pair().unwrap();
        Helper {
            child,
            sock,
            service: None,
        }
    }

    /// (m4-audit) A helper that exited while idle is reaped by the check
    /// and then only dropped: no SIGSTOP, `/proc` walk or group SIGKILL
    /// at a pid that may have been reused. A running one is kept.
    #[test]
    fn an_exited_helper_is_dropped_without_signals() {
        let live = helper("sleep 30");
        let pid = live.child.id();
        let live = live.running().expect("still running");
        assert_eq!(live.child.id(), pid);
        live.kill();
        assert_eq!(KILLS.with(Cell::get), 1);

        let dead = helper("exit 3");
        let deadline = Instant::now() + Duration::from_secs(10);
        // Wait for the exit without reaping it (the check must reap).
        let path = format!("/proc/{}/stat", dead.child.id());
        while std::fs::read_to_string(&path).ok().and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, r)| r.trim_start().starts_with('Z'))
        }) != Some(true)
        {
            assert!(Instant::now() < deadline, "the helper never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(dead.running().is_none());
        assert_eq!(KILLS.with(Cell::get), 1, "no kill for a reaped helper");
    }
}
