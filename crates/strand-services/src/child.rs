//! What every program the shell starts gets back between `fork` and
//! `exec`: the process-wide settings strand changed for itself only.
//!
//! `strand` turns transparent huge pages off for its own process from an
//! ELF constructor ([`thp_off`], the memory budget: decisions.md
//! wave4-exitMemory). The kernel keeps that flag (`MMF_DISABLE_THP`)
//! across `fork` and `execve`, so without [`restore_in_child`] every app
//! the launcher starts, every `from exec` service and the editor the
//! overlay opens, and their children, would run without THP for life.

use std::sync::atomic::{AtomicU8, Ordering};

/// THP was off for the process before [`thp_off`] (inherited from strand's
/// own parent): 0 no, 1 yes, 2 unknown ([`thp_off`] never ran).
static INHERITED: AtomicU8 = AtomicU8::new(2);

/// Turn transparent huge pages off for this process, remembering what it
/// inherited so children get that back. Makes two `prctl` calls and does
/// not allocate: safe from an ELF constructor before `main`. A failure (an
/// old kernel) leaves the default.
pub fn thp_off() {
    // SAFETY: `prctl` with integer arguments only.
    let was = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
    if was < 0 {
        return;
    }
    // SAFETY: as above.
    if unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) } == 0 {
        // The first call's view is the inherited one; a later call sees
        // strand's own setting.
        let _ =
            INHERITED.compare_exchange(2, u8::from(was != 0), Ordering::Relaxed, Ordering::Relaxed);
    }
}

/// In a child between `fork` and `exec` (`CommandExt::pre_exec`): give it
/// back the transparent huge page setting strand itself inherited. Only
/// async-signal-safe calls (an atomic load, `prctl`); a no-op when
/// [`thp_off`] never ran. A failure is ignored: the program still runs.
pub fn restore_in_child() {
    let inherited = INHERITED.load(Ordering::Relaxed);
    if inherited < 2 {
        // SAFETY: `prctl` with integer arguments only.
        unsafe {
            libc::prctl(
                libc::PR_SET_THP_DISABLE,
                libc::c_ulong::from(inherited),
                0,
                0,
                0,
            )
        };
    }
}

/// Whether THP is on for `status` (a `/proc/<pid>/status` text): the
/// `THP_enabled:` line, `None` on kernels without it (tests).
pub fn thp_enabled(status: &str) -> Option<bool> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("THP_enabled:"))
        .map(|v| v.trim() == "1")
}
