//! (M5, S-tools) The theming hub's work on the logic thread after each
//! tick's diff is sent (decisions.md m5-tools; architecture.md, "The
//! theming hub"): `export` requests waiting for their other mode.

use super::shell::Shell;

impl Shell {
    /// After the tick's diff is sent.
    pub(super) fn after_diff(&mut self) {
        self.retry_exports();
    }
}
