//! The lock's side of reloads: a load deferred while a lock is shown.

use super::shell::Shell;
use super::*;

impl Shell {
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
