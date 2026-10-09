//! The lock screen's authentication: the whole security boundary of
//! Strand's lock, small enough to review on its own (design.md, "Lock
//! screen"; docs/architecture.md, "Crate graph").
//!
//! Two halves:
//!
//! - **This library** (libc and zeroize only, no Strand crate): the
//!   framed request/reply [`protocol`] spoken over a socketpair, the
//!   blocking [`Client`] that fork+execs the helper and talks to it,
//!   [`Password`] buffers that are wiped when dropped, and
//!   [`UnlockToken`], which only a [`Client`] mints, from a success
//!   reply. `strand-surface` releases a session lock only for an
//!   `UnlockToken`, so no other code path can unlock.
//! - **The `strand-auth` binary** (`src/main.rs`, `src/pam.rs`): the PAM
//!   helper, a separate process with a hand-written PAM FFI, the only
//!   code that links libpam. It authenticates the user it runs as against
//!   the `strand` PAM service, or `login` with a one-time warning when
//!   `/etc/pam.d/strand` is missing (decisions.md, m4-owner), with
//!   `pam_authenticate` then `pam_acct_mgmt` and no `pam_setcred`. Every
//!   other PAM error fails closed: anything but `PAM_SUCCESS` from both
//!   calls is not an unlock.
//!
//! The `faults` feature (off in default and release builds, which
//! `tests/no_faults.rs` checks) adds the test hooks: `STRAND_FAULT`
//! points in the helper, a private PAM confdir, and
//! [`Client::with_test_env`].

mod client;
pub mod protocol;
mod secret;

pub use client::{
    AuthError, Client, DEFAULT_TIMEOUT, HELPER_NAME, UnlockToken, Verdict, default_helper,
    take_service_warning,
};
pub use protocol::Service;
pub use secret::Password;
