//! `auth`: the lock screen's password check.
//!
//! The store runs a [`strand_auth::Client`]: the `strand-auth` PAM
//! helper, a separate process started when the store starts (a lock
//! screen reads `auth.busy` or `auth.failed`, so that is when a lock is
//! shown) and killed when it stops, respawned by the client when it dies.
//! It is spawned with [`child::restore_in_child`](crate::child) as its
//! `pre_exec`, as every program strand starts is. `auth.submit(password)`
//! hands the password to the client on a blocking task (the client
//! blocks until the helper answers or its timeout fires) and sets `busy`
//! meanwhile; a second submit while one is checked is dropped. The
//! password is a [`Password`] from the moment it arrives, wiped once
//! sent.
//!
//! A success mints a [`strand_auth::UnlockToken`], which goes to the
//! [`UnlockSink`] the binary [`configure`]s (it passes it to the surface
//! manager's unlock, the only way a session lock is released); this store
//! never unlocks anything itself. Anything else sets `failed`. A check
//! that could not be made (no helper, a crash, a timeout, a PAM error) is
//! also a warning diagnostic and goes to the [`FailureSink`] (the binary
//! then shows its built-in password field, whose own client may still
//! work: decisions.md, m4-lock-w2), and the helper's `login` fallback
//! warns once per process (decisions.md, m4-owner).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_auth::{Client, Password, UnlockToken, Verdict};

use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `auth` service serves.
pub const SCHEMA: &str = strand_services_schema::AUTH;

/// Where an accepted password's [`UnlockToken`] goes: the binary's main
/// loop, which hands it to `strand_surface::State::unlock`. Called on a
/// blocking task's thread.
pub type UnlockSink = Arc<dyn Fn(UnlockToken) + Send + Sync>;

/// Told why a password could not be checked (no helper, a crash, a
/// timeout, a PAM error; never a refusal): the binary's main loop, which
/// shows the built-in password field. Called on the services thread.
pub type FailureSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Applied to every [`Client`] the store makes, before its first
/// password (the binary's `faults` build passes `STRAND_FAULT` through
/// with `Client::with_test_env`).
pub type ClientHook = Arc<dyn Fn(Client) -> Client + Send + Sync>;

/// What the `auth` store uses from its next start on.
#[derive(Clone)]
pub struct AuthConfig {
    /// The helper; `None` looks it up ([`strand_auth::default_helper`]).
    pub helper: Option<PathBuf>,
    /// How long one check may take ([`strand_auth::DEFAULT_TIMEOUT`]).
    pub timeout: Duration,
    /// Where unlocks go. Without one an accepted password unlocks
    /// nothing (and says so in the log).
    pub sink: Option<UnlockSink>,
    /// Where failures to check go.
    pub failed: Option<FailureSink>,
    /// Applied to each client made.
    pub client_hook: Option<ClientHook>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            helper: None,
            timeout: strand_auth::DEFAULT_TIMEOUT,
            sink: None,
            failed: None,
            client_hook: None,
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
            .field("client_hook", &self.client_hook.is_some())
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
    /// A password is being checked.
    pub busy: bool,
    /// The last password was refused, or could not be checked.
    pub failed: bool,
}

type Check = tokio::task::JoinHandle<(Client, Verdict)>;

impl Auth {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let config = config();
        let helper = config.helper.clone().or_else(strand_auth::default_helper);
        let start = |path: &PathBuf| {
            let c = Client::new(path.clone(), crate::child::restore_in_child)
                .with_timeout(config.timeout);
            match &config.client_hook {
                Some(hook) => hook(c),
                None => c,
            }
        };
        let fail = |cx: &mut Cx<Self>, why: String| {
            if let Some(f) = &config.failed {
                f(&why);
            }
            cx.warn(why);
        };
        let mut client = helper.as_ref().map(start);
        if helper.is_none() {
            cx.warn("no `strand-auth` helper is installed: the lock screen cannot check passwords");
        }
        if !cx.update(|s| *s = Auth::default()) {
            return Ok(());
        }
        cx.ready();
        let mut check: Option<Check> = None;
        loop {
            tokio::select! {
                done = async {
                    match check.as_mut() {
                        Some(c) => c.await,
                        None => std::future::pending().await,
                    }
                } => {
                    check = None;
                    let verdict = match done {
                        Ok((c, v)) => {
                            client = Some(c);
                            v
                        }
                        // The blocking task panicked: its client (and
                        // helper) went with it; a new one takes over.
                        Err(e) => {
                            client = helper.as_ref().map(start);
                            Verdict::Failed(strand_auth::AuthError::Io(std::io::Error::other(
                                e.to_string(),
                            )))
                        }
                    };
                    if let Some(w) = strand_auth::take_service_warning() {
                        cx.warn(w);
                    }
                    let failed = match verdict {
                        Verdict::Unlocked(token) => {
                            match &config.sink {
                                Some(sink) => sink(token),
                                None => log::warn!(
                                    "auth: a password was accepted, but nothing takes the unlock"
                                ),
                            }
                            false
                        }
                        Verdict::Denied { message } => {
                            if let Some(m) = message {
                                log::info!("auth: refused: {m}");
                            }
                            true
                        }
                        Verdict::Failed(e) => {
                            fail(&mut cx, format!("the password could not be checked: {e}"));
                            true
                        }
                    };
                    if !cx.update(|s| {
                        s.busy = false;
                        s.failed = failed;
                    }) {
                        return Ok(());
                    }
                }
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Action(AuthAction::Submit { password })) => {
                        let password = Password::from(password);
                        if check.is_some() {
                            // One check at a time; this one is dropped
                            // (and wiped).
                            continue;
                        }
                        let Some(mut c) = client.take() else {
                            fail(
                                &mut cx,
                                "no `strand-auth` helper is installed: the password could not be checked"
                                    .to_string(),
                            );
                            if !cx.update(|s| s.failed = true) {
                                return Ok(());
                            }
                            continue;
                        };
                        if !cx.update(|s| {
                            s.busy = true;
                            s.failed = false;
                        }) {
                            return Ok(());
                        }
                        check = Some(tokio::task::spawn_blocking(move || {
                            let v = c.submit(password);
                            (c, v)
                        }));
                    }
                    Some(_) => {}
                },
            }
        }
    }
}
