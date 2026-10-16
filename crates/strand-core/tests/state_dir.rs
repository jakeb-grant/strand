//! (launch-env) `STRAND_STATE_DIR` gives one shell a state directory of
//! its own without overriding `XDG_STATE_HOME`, which every program the
//! shell starts would inherit. Alone in its binary, since it sets the
//! process's environment.

use std::path::Path;

use strand_core::{PersistStore, SettingsStore};

#[test]
fn the_stores_follow_strand_state_dir_ahead_of_xdg_state_home() {
    // SAFETY: this binary's only test; nothing else reads the
    // environment while it is set.
    unsafe {
        std::env::set_var("STRAND_STATE_DIR", "/t/launcher");
        std::env::set_var("XDG_STATE_HOME", "/x/state");
    }
    assert_eq!(
        PersistStore::from_env().unwrap().dir(),
        Path::new("/t/launcher/persist")
    );
    assert_eq!(
        SettingsStore::from_env().unwrap().dir(),
        Path::new("/t/launcher/settings")
    );
    // Unset (or empty, or relative), the XDG rule as before.
    unsafe { std::env::set_var("STRAND_STATE_DIR", "") };
    assert_eq!(
        PersistStore::from_env().unwrap().dir(),
        Path::new("/x/state/strand/persist")
    );
    unsafe { std::env::remove_var("STRAND_STATE_DIR") };
    assert_eq!(
        PersistStore::from_env().unwrap().dir(),
        Path::new("/x/state/strand/persist")
    );
}
