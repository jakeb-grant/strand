//! The lock's runtime semantics (docs/architecture.md, "The lock";
//! decisions.md, m4-lock-w1).
//!
//! The binary passes on what the compositor says about the session lock
//! ([`Instance::set_session_lock`], from `ToLogic::LockState`):
//!
//! - While the session is [`SessionLock::Locked`], the `lock`'s `open`
//!   is held true: a config write of `false` (a handler, `strand set`) is
//!   ignored with a notice, so the content stays shown and live, and the
//!   scene never asks the surface manager to take the lock down (which
//!   it would refuse anyway: only an `UnlockToken` unlocks).
//! - Only an `auth` success unlocks: its token reaches the surface
//!   manager, which reports [`SessionLock::Unlocked`]; the runtime then
//!   writes `open` false through the lock's two-way binding
//!   (`open: <-> locked`), so the state that locked the session says it
//!   is unlocked and a later write of `true` locks again.
//! - [`Instance::lock_shown`] follows these reports: shown while
//!   `Locked`, not shown after `Finished` (the compositor refused or
//!   ended the lock) or `Unlocked`. Between a lock opening and the
//!   compositor's first answer (and in a host with no session lock, such
//!   as the tests and offline tools) a `lock` whose `open` is not false
//!   counts as shown, so a reload never races a lock coming up.
//!
//! The reload exemption itself is `Instance::reload`'s
//! (`EditClass::LockDeferred`; `crates/strand/src/run/tests.rs::
//! lock_edits_wait_for_the_unlock_and_then_land`).
//!
//! [`Instance::set_session_lock`]: super::Instance::set_session_lock
//! [`Instance::lock_shown`]: super::Instance::lock_shown

use std::cell::Cell;

use strand_core::{Runtime, Signal};
use strand_scene::PropValue;

/// The session lock as the compositor reports it (strand-surface's
/// `LockState`, passed on by the binary).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionLock {
    /// Every output shows a lock surface.
    Locked,
    /// The compositor refused the lock or ended it.
    Finished,
    /// An `auth` success released it.
    Unlocked,
}

/// Where the lock is, as far as the compositor has said.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// Nothing reported since the lock last opened: its `open` decides.
    #[default]
    Unreported,
    Locked,
    /// `Finished` or `Unlocked` since it last opened.
    Ended,
}

/// The instance's lock state, carried across reloads.
pub(crate) struct LockTrack {
    phase: Cell<Phase>,
    /// True while `Locked`: read by the `lock`'s `open` binding.
    held: Signal<bool>,
    /// The ignored-write notice was given in this lock session.
    warned: Cell<bool>,
}

/// The notice an ignored `open: false` gives, once per lock session.
pub const IGNORED_CLOSE: &str = "`lock`: `open` was set to false while the session is locked; \
     only a password unlocks it, so the lock stays";

impl LockTrack {
    pub(crate) fn new(rt: &Runtime) -> LockTrack {
        LockTrack {
            phase: Cell::new(Phase::Unreported),
            held: rt.signal(false),
            warned: Cell::new(false),
        }
    }

    /// Lets go of its signal (the instance shuts down).
    pub(crate) fn dispose(&self, rt: &Runtime) {
        self.held.dispose(rt);
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase.get()
    }

    /// The value the `lock`'s `open` binding sends, given what its
    /// expression gave (`out`): `true` while the session is locked,
    /// whatever the config wrote. Read inside the binding's memo, so it
    /// follows the reports. `Some(notice)` the first time a write was
    /// ignored in this lock session.
    pub(crate) fn hold_open(&self, rt: &Runtime, out: PropValue) -> (PropValue, Option<&str>) {
        let held = self.held.get(rt).unwrap_or(false);
        if !held || out == PropValue::Bool(true) {
            return (out, None);
        }
        let notice = (!self.warned.replace(true)).then_some(IGNORED_CLOSE);
        (PropValue::Bool(true), notice)
    }

    /// The lock's `open` turned true: a new request, which the next
    /// report is about.
    pub(crate) fn opened(&self) {
        if self.phase.get() == Phase::Ended {
            self.phase.set(Phase::Unreported);
        }
    }

    /// The compositor's report. True when the runtime should now write
    /// the lock's `open` false (an unlock).
    pub(crate) fn report(&self, rt: &Runtime, state: SessionLock) -> bool {
        let (phase, held) = match state {
            SessionLock::Locked => (Phase::Locked, true),
            SessionLock::Finished | SessionLock::Unlocked => (Phase::Ended, false),
        };
        self.phase.set(phase);
        if held {
            self.warned.set(false);
        }
        let _ = self.held.set(rt, held);
        state == SessionLock::Unlocked
    }
}

impl super::Ctx {
    /// The `lock`'s `open` binding's value, held true while the session
    /// is locked; an ignored write's notice goes to the tick's notices.
    pub(crate) fn hold_lock_open(&self, rt: &Runtime, out: super::PropOut) -> super::PropOut {
        let Some(track) = self.lock.borrow().clone() else {
            return out;
        };
        let (value, notice) = track.hold_open(rt, out.value);
        if let Some(n) = notice {
            self.notices.borrow_mut().push(n.to_string());
        }
        super::PropOut {
            value,
            source: out.source,
        }
    }

    /// A `lock` was shown (its `open` turned true).
    pub(crate) fn lock_opened(&self) {
        if let Some(track) = self.lock.borrow().as_ref() {
            track.opened();
        }
    }
}
