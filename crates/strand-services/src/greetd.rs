//! (services-round) `greetd`: a login screen over greetd's socket, so a
//! greeter is written in the same language as the lock screen.
//!
//! greetd runs its greeter with `$GREETD_SOCK` naming a socket that
//! speaks length-prefixed JSON ([`proto`]: a native-endian `u32` length,
//! then a request or a response). The store connects when it starts
//! ([`configure`] names another socket: tests) and keeps one request in
//! flight at a time, as the protocol is strictly a request then its
//! answer:
//!
//! - `login(user)` is `create_session` (after `cancel_session` when a
//!   login is under way). greetd answers with a question (`prompt`,
//!   `echo` for a visible one), a text PAM shows (`message`, answered
//!   with an empty response at once so PAM goes on, as greetd's own
//!   `agreety` does), success, or an error (`failed`, `error`). A
//!   refused answer asks the same user again (`cancel_session`, then
//!   `create_session`), `failed` staying until the next answer, as the
//!   lock's `auth.failed` does.
//! - `submit(answer)` is `post_auth_message_response`: the question
//!   waiting, or the first one greetd asks when sent before it did. The
//!   answer is a [`Password`] from the moment it arrives, and its frame is
//!   built in a buffer sized once and wiped when dropped
//!   ([`proto::encode`]), so no copy of it is left behind.
//! - `start(command)` is `start_session` once the login succeeded, each
//!   word quoted for the shell line greetd makes of it
//!   ([`proto::shell_word`]), so a word is one word as given: sent
//!   with the last answer, it waits for the success; a failed login
//!   drops it. greetd then waits for its greeter to exit, so a success
//!   calls [`GreetdConfig::started`] (the binary ends `strand run` as on
//!   SIGTERM).
//! - `cancel()` is `cancel_session`.
//!
//! Without `$GREETD_SOCK` (Strand is not greetd's greeter) `available` is
//! false and every action says so in `error`. A lost connection ends the
//! run with an error, which the contract retries. `greetd` is one of
//! [`strand_services_schema::SECRET`]: the binary never records or
//! snapshots it, nor runs its actions from `strand call`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use strand_auth::Password;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `greetd` service serves.
pub const SCHEMA: &str = strand_services_schema::GREETD;

/// The variable greetd names its socket in.
pub const SOCKET_VAR: &str = "GREETD_SOCK";

/// Called once greetd has accepted `start(command)`: greetd waits for
/// its greeter to exit before it starts the session.
pub type Started = Arc<dyn Fn() + Send + Sync>;

/// What the `greetd` store uses from its next start on.
#[derive(Clone, Default)]
pub struct GreetdConfig {
    /// The socket; `None` is `$GREETD_SOCK`.
    pub socket: Option<PathBuf>,
    /// What a started session does to Strand (the binary: it exits).
    /// Without one it is only logged.
    pub started: Option<Started>,
}

impl std::fmt::Debug for GreetdConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GreetdConfig")
            .field("socket", &self.socket)
            .field("started", &self.started.is_some())
            .finish()
    }
}

static CONFIG: Mutex<Option<GreetdConfig>> = Mutex::new(None);

/// Sets what the store uses from its next start. `None` restores the
/// defaults. A running store keeps its config until it stops.
pub fn configure(config: Option<GreetdConfig>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> GreetdConfig {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_default()
}

/// greetd's wire protocol (greetd_ipc 0.9 and later).
pub mod proto {
    use serde::Deserialize;
    use strand_auth::Password;
    use zeroize::Zeroizing;

    /// The longest response read (greetd's are a few hundred bytes).
    pub const MAX_FRAME: usize = 64 * 1024;

    /// A request to greetd (an answer's `Debug` shows only its length).
    #[derive(Debug)]
    pub enum Request<'a> {
        CreateSession {
            username: &'a str,
        },
        /// An answer to the question waiting, or `None` to go on after a
        /// text.
        PostAuthMessageResponse {
            response: Option<&'a Password>,
        },
        StartSession {
            cmd: &'a [String],
        },
        CancelSession,
    }

    /// The bytes of `s` as a JSON string's contents.
    fn escaped_len(s: &[u8]) -> usize {
        s.iter()
            .map(|&b| match b {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
                0..=0x1f => 6,
                _ => 1,
            })
            .sum()
    }

    fn push_escaped(out: &mut Vec<u8>, s: &[u8]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for &b in s {
            match b {
                b'"' => out.extend_from_slice(b"\\\""),
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                0..=0x1f => {
                    out.extend_from_slice(b"\\u00");
                    out.push(HEX[usize::from(b >> 4)]);
                    out.push(HEX[usize::from(b & 15)]);
                }
                _ => out.push(b),
            }
        }
    }

    /// `word` quoted for `sh` when it needs it: greetd joins a session's
    /// command with spaces into a shell line (`exec <cmd>`), so each of
    /// `greetd.start`'s words is quoted to stay one word, as given.
    pub fn shell_word(word: &str) -> String {
        let plain = !word.is_empty()
            && word
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
        if plain {
            word.to_string()
        } else {
            format!("'{}'", word.replace('\'', r"'\''"))
        }
    }

    /// `request`'s frame: its length (native-endian `u32`), then its
    /// JSON. Built in a buffer sized once and wiped when dropped, so an
    /// answer is copied exactly once, into it (no reallocation leaves a
    /// copy behind).
    pub fn encode(request: &Request<'_>) -> Zeroizing<Vec<u8>> {
        // The parts: literal JSON and strings to escape, in order.
        let mut parts: Vec<(&[u8], bool)> = Vec::new();
        match request {
            Request::CreateSession { username } => {
                parts.push((br#"{"type":"create_session","username":""#, false));
                parts.push((username.as_bytes(), true));
                parts.push((br#""}"#, false));
            }
            Request::PostAuthMessageResponse { response: Some(p) } => {
                parts.push((
                    br#"{"type":"post_auth_message_response","response":""#,
                    false,
                ));
                parts.push((p.as_bytes(), true));
                parts.push((br#""}"#, false));
            }
            Request::PostAuthMessageResponse { response: None } => {
                parts.push((
                    br#"{"type":"post_auth_message_response","response":null}"#,
                    false,
                ));
            }
            Request::StartSession { cmd } => {
                parts.push((br#"{"type":"start_session","cmd":["#, false));
                for (i, a) in cmd.iter().enumerate() {
                    parts.push((if i == 0 { b"\"" } else { b",\"" }, false));
                    parts.push((a.as_bytes(), true));
                    parts.push((b"\"", false));
                }
                parts.push((br#"],"env":[]}"#, false));
            }
            Request::CancelSession => parts.push((br#"{"type":"cancel_session"}"#, false)),
        }
        let len: usize = parts
            .iter()
            .map(|(p, esc)| if *esc { escaped_len(p) } else { p.len() })
            .sum();
        let mut out = Zeroizing::new(Vec::with_capacity(4 + len));
        out.extend_from_slice(&(len as u32).to_ne_bytes());
        for (p, esc) in parts {
            if esc {
                push_escaped(&mut out, p);
            } else {
                out.extend_from_slice(p);
            }
        }
        debug_assert_eq!(out.len(), 4 + len);
        out
    }

    /// What kind of message greetd shows.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum AuthMessageType {
        Visible,
        Secret,
        Info,
        Error,
    }

    /// What kind of error greetd reports.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ErrorType {
        Error,
        AuthError,
    }

    /// A response from greetd.
    #[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum Response {
        Success,
        Error {
            error_type: ErrorType,
            description: String,
        },
        AuthMessage {
            auth_message_type: AuthMessageType,
            auth_message: String,
        },
    }

    /// A response's JSON.
    pub fn decode(json: &[u8]) -> Result<Response, String> {
        serde_json::from_slice(json).map_err(|e| format!("greetd said something else: {e}"))
    }
}

use proto::{AuthMessageType, Request, Response};

/// `greetd`'s actions.
#[derive(Call)]
pub enum GreetdAction {
    /// `greetd.login(user)`.
    Login { user: String },
    /// `greetd.submit(answer)`.
    Submit { answer: String },
    /// `greetd.start(command)`.
    Start { command: Vec<String> },
    /// `greetd.cancel()`.
    Cancel,
}

impl std::fmt::Debug for GreetdAction {
    /// An answer shows only its length.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GreetdAction::Login { user } => write!(f, "Login({user:?})"),
            GreetdAction::Submit { answer } => write!(f, "Submit(<{} bytes>)", answer.len()),
            GreetdAction::Start { command } => write!(f, "Start({command:?})"),
            GreetdAction::Cancel => f.write_str("Cancel"),
        }
    }
}

/// See the module docs.
#[service(name = "greetd", action = GreetdAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Greetd {
    /// greetd answers: Strand runs as its greeter. False otherwise, when
    /// every action fails with `error`.
    pub available: bool,
    /// The user being logged in (`greetd.login(user)`), or "".
    pub user: String,
    /// What greetd asks ("Password: "), as PAM says it; "" while it asks
    /// nothing.
    pub prompt: String,
    /// The answer to `prompt` may be shown as it is typed (a visible
    /// question, such as a name), not a secret one.
    pub echo: bool,
    /// What PAM last told the user ("Place your finger on the fingerprint
    /// reader", "You are required to change your password"), or "".
    pub message: String,
    /// greetd is busy with a request: checking an answer, starting the
    /// session.
    pub busy: bool,
    /// The last answer was refused (greetd then asks the same user
    /// again), or a login or session could not start; `error` says why.
    /// It stays until the next answer or login.
    pub failed: bool,
    /// Why the last request failed, as greetd says it, or "".
    pub error: String,
}

/// A request waiting to be sent.
enum Out {
    Create(String),
    Answer(Option<Password>),
    Start(Vec<String>),
    Cancel,
}

/// What the request in flight was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    Create,
    Answer,
    Start,
    Cancel,
}

/// The login's progress.
#[derive(Default)]
struct Flow {
    inflight: Option<Sent>,
    /// The request in flight belongs to a login since dropped: its
    /// answer is not shown.
    stale: bool,
    outbox: VecDeque<Out>,
    /// greetd waits on a question.
    asking: bool,
    /// An answer sent before greetd asked: it answers the first question.
    held_answer: Option<Password>,
    /// A command to start once the login succeeds.
    held_start: Option<Vec<String>>,
    /// The login succeeded.
    authenticated: bool,
    /// greetd has a session being configured (created, not cancelled).
    configuring: bool,
    /// The user being logged in, asked again after a refused answer.
    user: String,
}

impl Flow {
    /// A login was asked for and greetd has not answered it yet.
    fn logging_in(&self) -> bool {
        (self.inflight == Some(Sent::Create) && !self.stale)
            || self.outbox.iter().any(|o| matches!(o, Out::Create(_)))
    }

    /// Drops the login under way (its answers wiped), queueing greetd's
    /// cancel when it has one.
    fn drop_login(&mut self) {
        self.outbox.clear();
        self.held_answer = None;
        self.held_start = None;
        self.asking = false;
        self.authenticated = false;
        if self.inflight.is_some() {
            self.stale = true;
        }
        if self.configuring || self.inflight.is_some() {
            self.outbox.push_back(Out::Cancel);
        }
    }
}

/// Why a frame could not be read.
async fn read_frame(r: &mut tokio::net::unix::OwnedReadHalf) -> Result<Response, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .await
        .map_err(|e| format!("greetd's socket closed: {e}"))?;
    let len = u32::from_ne_bytes(len) as usize;
    if len > proto::MAX_FRAME {
        return Err(format!("greetd sent a {len}-byte frame"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| format!("greetd's socket closed: {e}"))?;
    proto::decode(&body)
}

impl Greetd {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = config();
        let path = config
            .socket
            .clone()
            .or_else(|| std::env::var_os(SOCKET_VAR).map(PathBuf::from));
        let Some(path) = path else {
            return without_greetd(&mut cx).await;
        };
        let stream = tokio::net::UnixStream::connect(&path).await.map_err(|e| {
            ServiceError(format!(
                "greetd's socket {} does not answer: {e}",
                path.display()
            ))
        })?;
        let (mut r, mut w) = stream.into_split();
        let (tx, mut responses) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            loop {
                let got = read_frame(&mut r).await;
                let end = got.is_err();
                if tx.send(got).is_err() || end {
                    break;
                }
            }
        });
        let _reader = AbortOnDrop(reader);
        if !cx.update(|s| {
            *s = Greetd {
                available: true,
                ..Greetd::default()
            }
        }) {
            return Ok(());
        }
        cx.ready();
        let mut flow = Flow::default();
        loop {
            // Send the next request when none is in flight.
            if flow.inflight.is_none()
                && let Some(out) = flow.outbox.pop_front()
            {
                let (frame, sent) = match &out {
                    Out::Create(user) => (
                        proto::encode(&Request::CreateSession { username: user }),
                        Sent::Create,
                    ),
                    Out::Answer(a) => (
                        proto::encode(&Request::PostAuthMessageResponse {
                            response: a.as_ref(),
                        }),
                        Sent::Answer,
                    ),
                    Out::Start(cmd) => {
                        let words: Vec<String> = cmd.iter().map(|w| proto::shell_word(w)).collect();
                        (
                            proto::encode(&Request::StartSession { cmd: &words }),
                            Sent::Start,
                        )
                    }
                    Out::Cancel => (proto::encode(&Request::CancelSession), Sent::Cancel),
                };
                // The answer is wiped here, its frame when sent.
                drop(out);
                if sent == Sent::Create {
                    flow.configuring = true;
                }
                w.write_all(&frame)
                    .await
                    .map_err(|e| ServiceError(format!("greetd's socket closed: {e}")))?;
                drop(frame);
                flow.inflight = Some(sent);
                continue;
            }
            tokio::select! {
                got = responses.recv() => {
                    let got = got.unwrap_or_else(|| Err("greetd's socket closed".into()));
                    let response = got.map_err(ServiceError)?;
                    if !answered(&mut cx, &mut flow, &config, response) {
                        return Ok(());
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Action(a)) => {
                        if !act(&mut cx, &mut flow, a) {
                            return Ok(());
                        }
                    }
                    Some(_) => {}
                },
            }
        }
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// No `$GREETD_SOCK`: ready at the defaults, each action saying why.
async fn without_greetd(cx: &mut Cx<Greetd>) -> Result<(), ServiceError> {
    const WHY: &str = "greetd is not running Strand as its greeter (no $GREETD_SOCK)";
    if !cx.update(|s| *s = Greetd::default()) {
        return Ok(());
    }
    cx.ready();
    while let Some(m) = cx.recv().await {
        if let Msg::Action(_) = m
            && !cx.update(|s| s.error = WHY.into())
        {
            return Ok(());
        }
    }
    Ok(())
}

/// greetd answered the request in flight.
fn answered(cx: &mut Cx<Greetd>, flow: &mut Flow, config: &GreetdConfig, r: Response) -> bool {
    let Some(sent) = flow.inflight.take() else {
        log::info!("greetd: an answer to no request was ignored");
        return true;
    };
    if std::mem::take(&mut flow.stale) {
        // A dropped login's: a cancel follows in the outbox.
        return true;
    }
    match (sent, r) {
        (Sent::Cancel, _) => {
            flow.configuring = false;
            flow.authenticated = false;
            true
        }
        (Sent::Start, Response::Success) => {
            log::info!("greetd: the session starts; the greeter ends");
            match &config.started {
                Some(f) => f(),
                None => log::warn!("greetd: a session started, but nothing ends the greeter"),
            }
            true
        }
        (
            _,
            Response::Error {
                error_type,
                description,
            },
        ) => {
            log::info!("greetd: {description}");
            let start = sent == Sent::Start;
            if !start {
                flow.held_answer = None;
                flow.held_start = None;
                flow.asking = false;
                flow.authenticated = false;
                // A failed login's session is dead in greetd until
                // cancelled, so the next login starts afresh.
                flow.outbox.push_back(Out::Cancel);
                // A refused answer: the same user is asked again, as the
                // lock asks again (`failed` stays until the next answer).
                if sent == Sent::Answer
                    && error_type == proto::ErrorType::AuthError
                    && !flow.user.is_empty()
                {
                    flow.outbox.push_back(Out::Create(flow.user.clone()));
                }
            }
            cx.update(|s| {
                s.busy = false;
                s.failed = true;
                s.prompt.clear();
                s.error = description;
            })
        }
        (
            _,
            Response::AuthMessage {
                auth_message_type,
                auth_message,
            },
        ) => match auth_message_type {
            AuthMessageType::Info | AuthMessageType::Error => {
                // PAM goes on once the text is acknowledged.
                flow.outbox.push_back(Out::Answer(None));
                cx.update(|s| s.message = auth_message)
            }
            AuthMessageType::Visible | AuthMessageType::Secret => {
                let echo = auth_message_type == AuthMessageType::Visible;
                if let Some(a) = flow.held_answer.take() {
                    flow.outbox.push_back(Out::Answer(Some(a)));
                    return cx.update(|s| {
                        s.prompt.clear();
                        s.echo = echo;
                    });
                }
                flow.asking = true;
                cx.update(|s| {
                    s.prompt = auth_message;
                    s.echo = echo;
                    s.busy = false;
                })
            }
        },
        (_, Response::Success) => {
            flow.authenticated = true;
            flow.asking = false;
            let start = flow.held_start.take();
            let starting = start.is_some();
            if let Some(cmd) = start {
                flow.outbox.push_back(Out::Start(cmd));
            }
            cx.update(|s| {
                s.prompt.clear();
                s.busy = starting;
                s.failed = false;
                s.error.clear();
            })
        }
    }
}

/// An action from the config.
fn act(cx: &mut Cx<Greetd>, flow: &mut Flow, a: GreetdAction) -> bool {
    match a {
        GreetdAction::Login { user } => {
            flow.drop_login();
            flow.outbox.push_back(Out::Create(user.clone()));
            flow.user.clone_from(&user);
            cx.update(|s| {
                s.user = user;
                s.prompt.clear();
                s.echo = false;
                s.message.clear();
                s.busy = true;
                s.failed = false;
                s.error.clear();
            })
        }
        GreetdAction::Submit { answer } => {
            let answer = Password::from(answer);
            if flow.asking {
                flow.asking = false;
                flow.outbox.push_back(Out::Answer(Some(answer)));
                return cx.update(|s| {
                    s.prompt.clear();
                    s.busy = true;
                    s.failed = false;
                    s.error.clear();
                });
            }
            // Typed before greetd asked: it answers the first question.
            if flow.logging_in() && flow.held_answer.is_none() {
                flow.held_answer = Some(answer);
                return cx.update(|s| s.busy = true);
            }
            // Nothing asks (no login, one being checked): dropped, wiped.
            log::info!("greetd: an answer with no question waiting was dropped");
            true
        }
        GreetdAction::Start { command } => {
            if command.is_empty() {
                return cx.update(|s| s.error = "a session needs a command".into());
            }
            if flow.authenticated && flow.inflight.is_none() && flow.outbox.is_empty() {
                flow.outbox.push_back(Out::Start(command));
                return cx.update(|s| s.busy = true);
            }
            if flow.configuring || flow.logging_in() {
                // Starts once the login succeeds.
                flow.held_start = Some(command);
                return true;
            }
            cx.update(|s| s.error = "no user is logging in: `greetd.login(user)` first".into())
        }
        GreetdAction::Cancel => {
            flow.drop_login();
            flow.user.clear();
            cx.update(|s| {
                s.user.clear();
                s.prompt.clear();
                s.echo = false;
                s.message.clear();
                s.busy = false;
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::proto::*;
    use super::*;

    fn body(frame: &[u8]) -> (u32, serde_json::Value) {
        let len = u32::from_ne_bytes(frame[..4].try_into().unwrap());
        assert_eq!(len as usize, frame.len() - 4);
        (len, serde_json::from_slice(&frame[4..]).unwrap())
    }

    #[test]
    fn requests_encode_as_greetd_reads_them() {
        let f = encode(&Request::CreateSession { username: "alice" });
        assert_eq!(
            body(&f).1,
            serde_json::json!({"type": "create_session", "username": "alice"})
        );
        let p = Password::from("pa\"ss\\word\n\u{1}é".to_string());
        let f = encode(&Request::PostAuthMessageResponse { response: Some(&p) });
        assert_eq!(
            body(&f).1,
            serde_json::json!({"type": "post_auth_message_response", "response": "pa\"ss\\word\n\u{1}é"})
        );
        // Sized once: no reallocation left a copy of the answer behind.
        assert_eq!(f.capacity(), f.len());
        let f = encode(&Request::PostAuthMessageResponse { response: None });
        assert_eq!(
            body(&f).1,
            serde_json::json!({"type": "post_auth_message_response", "response": null})
        );
        let cmd = vec!["sway".to_string(), "--unsupported-gpu".to_string()];
        let f = encode(&Request::StartSession { cmd: &cmd });
        assert_eq!(
            body(&f).1,
            serde_json::json!({"type": "start_session", "cmd": ["sway", "--unsupported-gpu"], "env": []})
        );
        // Each word stays one through greetd's `exec <words joined>`.
        assert_eq!(shell_word("sway"), "sway");
        assert_eq!(
            shell_word("--config=/etc/sway/a.conf"),
            "--config=/etc/sway/a.conf"
        );
        assert_eq!(shell_word("id -un > f"), "'id -un > f'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("$HOME"), "'$HOME'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(
            body(&encode(&Request::CancelSession)).1,
            serde_json::json!({"type": "cancel_session"})
        );
    }

    #[test]
    fn responses_decode_as_greetd_writes_them() {
        assert_eq!(decode(br#"{"type":"success"}"#), Ok(Response::Success));
        assert_eq!(
            decode(br#"{"type":"error","error_type":"auth_error","description":"nope"}"#),
            Ok(Response::Error {
                error_type: ErrorType::AuthError,
                description: "nope".into()
            })
        );
        assert_eq!(
            decode(
                br#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password: "}"#
            ),
            Ok(Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password: ".into()
            })
        );
        assert!(decode(br#"{"type":"bogus"}"#).is_err());
    }

    #[test]
    fn an_answer_never_shows_in_debug() {
        let a = GreetdAction::Submit {
            answer: "hunter2".into(),
        };
        assert_eq!(format!("{a:?}"), "Submit(<7 bytes>)");
    }
}
