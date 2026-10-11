//! `strand clear <path>` (S-ipc): drop a runtime overlay (`$accent`).
//! Not served yet.

use super::{Caller, Shell};

impl Shell {
    pub(super) fn clear_request(&mut self, from: Caller, path: &str) {
        let _ = path;
        self.not_served(from, "clear");
    }
}
