//! What every program the shell starts gets back between `fork` and
//! `exec`: the process-wide settings strand changed for itself only; and
//! the environment it gets ([`child_env`]): strand's own, less what
//! addresses this shell only.
//!
//! `strand` turns transparent huge pages off for its own process from an
//! ELF constructor ([`thp_off`], the memory budget: decisions.md
//! wave4-exitMemory), except for regions it advises (its largest pixel
//! buffers, on kernels with `PR_THP_DISABLE_EXCEPT_ADVISED`: decisions.md
//! m5-speed-thp). The kernel keeps that flag across `fork` and `execve`,
//! so without [`restore_in_child`] every app the launcher starts, every
//! `from exec` service and the editor the overlay opens, and their
//! children, would run without THP for life.

use std::sync::atomic::{AtomicU8, Ordering};

/// `PR_SET_THP_DISABLE`'s flag (Linux 6.18) that keeps THP for regions
/// advised `MADV_HUGEPAGE` (not in `libc` yet).
const PR_THP_DISABLE_EXCEPT_ADVISED: libc::c_ulong = 1 << 1;

/// What `PR_GET_THP_DISABLE` said before [`thp_off`] (inherited from
/// strand's own parent): 0 THP on, 1 off, 3 off except advised regions;
/// [`UNKNOWN`] when [`thp_off`] never ran.
static INHERITED: AtomicU8 = AtomicU8::new(UNKNOWN);
const UNKNOWN: u8 = u8::MAX;

/// Whether [`thp_off`] left THP on for advised regions (the kernel has
/// `PR_THP_DISABLE_EXCEPT_ADVISED`).
static EXCEPT_ADVISED: AtomicU8 = AtomicU8::new(0);

/// Turn transparent huge pages off for this process but for regions it
/// advises `MADV_HUGEPAGE` (all of them on a kernel older than 6.18),
/// remembering what it inherited so children get that back. Makes at
/// most three `prctl` calls and does not allocate: safe from an ELF
/// constructor before `main`. A failure (an old kernel) leaves the
/// default.
pub fn thp_off() {
    // SAFETY: `prctl` with integer arguments only.
    let was = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
    if was < 0 {
        return;
    }
    // SAFETY: as above.
    let advised = unsafe {
        libc::prctl(
            libc::PR_SET_THP_DISABLE,
            1 as libc::c_ulong,
            PR_THP_DISABLE_EXCEPT_ADVISED,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    } == 0;
    // SAFETY: as above.
    if advised || unsafe { libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0) } == 0 {
        EXCEPT_ADVISED.store(u8::from(advised), Ordering::Relaxed);
        // The first call's view is the inherited one; a later call sees
        // strand's own setting.
        let _ = INHERITED.compare_exchange(
            UNKNOWN,
            (was as u8) & 3,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Whether THP stays on for the regions strand advises (the kernel took
/// `PR_THP_DISABLE_EXCEPT_ADVISED`); false before [`thp_off`] ran.
pub fn thp_on_for_advised() -> bool {
    EXCEPT_ADVISED.load(Ordering::Relaxed) != 0
}

/// In a child between `fork` and `exec` (`CommandExt::pre_exec`): give it
/// back the transparent huge page setting strand itself inherited. Only
/// async-signal-safe calls (an atomic load, `prctl`); a no-op when
/// [`thp_off`] never ran. A failure is ignored: the program still runs.
pub fn restore_in_child() {
    let inherited = INHERITED.load(Ordering::Relaxed);
    if inherited != UNKNOWN {
        let off = libc::c_ulong::from(inherited & 1);
        let flags = libc::c_ulong::from(inherited) & PR_THP_DISABLE_EXCEPT_ADVISED;
        // SAFETY: `prctl` with integer arguments only.
        unsafe {
            libc::prctl(
                libc::PR_SET_THP_DISABLE,
                off,
                flags,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
    }
}

/// (M5) The variables a launched program reads its xdg-activation token
/// from: the protocol's, and the startup-notification one older toolkits
/// read (the same token, as the xdg-activation spec asks).
pub const ACTIVATION_VARS: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

/// The variables that address one running shell, not the session: the
/// socket it answers on (`STRAND_SOCKET`) and the directory it keeps its
/// state in (`STRAND_STATE_DIR`). A program strand starts never inherits
/// them, so a terminal opened from a second instance reaches the
/// display's shell with `strand set`, as one opened anywhere else does,
/// and keeps nothing in that instance's state (spec section 13;
/// decisions.md launch-env).
pub const INSTANCE_VARS: [&str; 2] = ["STRAND_SOCKET", strand_core::persist::STATE_DIR_VAR];

/// The environment of a program strand starts: strand's own, with the
/// activation variables ([`ACTIVATION_VARS`]) set to `token`, the token
/// for the press that launched it, or removed with none (a program never
/// inherits the token strand itself was started with: spent, and meant
/// for strand's own window), and [`INSTANCE_VARS`] removed. Every
/// program strand starts goes through it, beside [`restore_in_child`]:
/// an app launch, a declared action's command, an `exec` source, a
/// `file` element's `run:` and the overlay's editor. It is done on the
/// `Command` before `fork` (setting the environment between `fork` and
/// `exec` is not async-signal-safe).
pub fn child_env(cmd: &mut std::process::Command, token: Option<&str>) {
    for var in ACTIVATION_VARS {
        match token {
            Some(t) => cmd.env(var, t),
            None => cmd.env_remove(var),
        };
    }
    for var in INSTANCE_VARS {
        cmd.env_remove(var);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A launch with a token carries it in both variables; one without
    /// carries neither, even one strand inherited (an explicit removal
    /// overrides the inherited environment); neither carries the
    /// variables that address this shell.
    #[test]
    fn a_launch_carries_its_token_and_never_an_inherited_one() {
        let envs = |cmd: &std::process::Command| -> Vec<(String, Option<String>)> {
            cmd.get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect()
        };
        let mut cmd = std::process::Command::new("true");
        child_env(&mut cmd, Some("tok"));
        let mut got = envs(&cmd);
        got.sort();
        assert_eq!(
            got,
            [
                ("DESKTOP_STARTUP_ID".into(), Some("tok".into())),
                ("STRAND_SOCKET".into(), None),
                ("STRAND_STATE_DIR".into(), None),
                ("XDG_ACTIVATION_TOKEN".into(), Some("tok".into())),
            ]
        );
        let mut cmd = std::process::Command::new("true");
        cmd.env("XDG_ACTIVATION_TOKEN", "stale");
        cmd.env("STRAND_SOCKET", "/run/user/1000/strand-launcher.sock");
        child_env(&mut cmd, None);
        let mut got = envs(&cmd);
        got.sort();
        assert_eq!(
            got,
            [
                ("DESKTOP_STARTUP_ID".into(), None),
                ("STRAND_SOCKET".into(), None),
                ("STRAND_STATE_DIR".into(), None),
                ("XDG_ACTIVATION_TOKEN".into(), None),
            ]
        );
    }
}
