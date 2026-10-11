//! `strand get <path>` (S-ipc): what `set` writes. Not served yet.

use super::{Caller, Shell};

impl Shell {
    pub(super) fn get_request(&mut self, from: Caller, path: &str) {
        let _ = path;
        self.not_served(from, "get");
    }
}
