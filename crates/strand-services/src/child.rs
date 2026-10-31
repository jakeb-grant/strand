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
//!
//! Signals survive `exec` the same way: strand blocks SIGINT and SIGTERM
//! on every thread for its signalfd, std's `Command` keeps the mask on
//! purpose, and an ignored signal stays ignored. So [`restore_in_child`]
//! also clears the mask and resets every disposition: a program must be
//! able to hear `rec.stop()`, a terminal's Ctrl-C and its death signal
//! (bead #17; decisions.md child-signals).
//!
//! Variables strand sets for its own process before `main`
//! ([`set_own_var`]: the Vulkan loader's driver filter, decisions.md
//! gpu-first) are given back as strand inherited them the same way.

use std::ffi::{OsStr, OsString};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// Variables strand set for its own process ([`set_own_var`]), each with
/// the value it inherited.
static OWN_VARS: Mutex<Vec<(&'static str, Option<OsString>)>> = Mutex::new(Vec::new());

/// Sets `var` to `value` for strand's own process only, remembering what
/// it inherited: [`child_env`] gives every program strand starts that
/// back (gpu-first review: the Vulkan loader's driver filter, which an
/// app the launcher starts must not inherit).
///
/// # Safety
///
/// It changes the process environment: call it only while no other
/// thread runs (an ELF constructor before `main`, as `process_setup!`
/// does).
pub unsafe fn set_own_var(var: &'static str, value: &OsStr) {
    let was = std::env::var_os(var);
    // SAFETY: no other thread runs (the caller's contract).
    unsafe { std::env::set_var(var, value) };
    OWN_VARS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((var, was));
}

/// Gives `cmd` back the values `own` records as inherited.
fn give_back(cmd: &mut std::process::Command, own: &[(&'static str, Option<OsString>)]) {
    for (var, was) in own {
        match was {
            Some(v) => cmd.env(var, v),
            None => cmd.env_remove(var),
        };
    }
}

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

/// One past the highest signal number on Linux (`_NSIG` is 65 on every
/// architecture but MIPS, where the extra numbers are refused and ignored).
const SIGNALS_END: libc::c_int = 65;

/// In a child between `fork` and `exec`: every signal to its default and
/// nothing blocked. A signal a handler caught is reset by `exec` itself;
/// this is for what is not, an ignored signal and the mask. Only
/// async-signal-safe calls. SIGKILL, SIGSTOP and the signals libc keeps
/// for itself refuse the change, which is ignored.
fn clean_signals() {
    // SAFETY: `signal` with `SIG_DFL` and `sigemptyset`/`pthread_sigmask`
    // on a local set are async-signal-safe and change only this process.
    unsafe {
        for sig in 1..SIGNALS_END {
            libc::signal(sig, libc::SIG_DFL);
        }
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
}

/// In a child between `fork` and `exec` (`CommandExt::pre_exec`): clear
/// the signal mask and every signal's disposition ([`clean_signals`]),
/// and give it back the transparent huge page setting strand itself
/// inherited (a no-op when [`thp_off`] never ran). Only async-signal-safe
/// calls (`signal`, `pthread_sigmask`, an atomic load, `prctl`). A failure
/// is ignored: the program still runs.
pub fn restore_in_child() {
    clean_signals();
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
/// socket it answers on (`STRAND_SOCKET`), the directory it keeps its
/// state in (`STRAND_STATE_DIR`), and (services-round) greetd's socket
/// when it is the greeter (`GREETD_SOCK`: only its `greetd` store logs
/// in, never a program it starts). A program strand starts never inherits
/// them, so a terminal opened from a second instance reaches the
/// display's shell with `strand set`, as one opened anywhere else does,
/// and keeps nothing in that instance's state (spec section 13;
/// decisions.md launch-env).
pub const INSTANCE_VARS: [&str; 3] = [
    "STRAND_SOCKET",
    strand_core::persist::STATE_DIR_VAR,
    crate::greetd::SOCKET_VAR,
];

/// The environment of a program strand starts: strand's own, with the
/// activation variables ([`ACTIVATION_VARS`]) set to `token`, the token
/// for the press that launched it, or removed with none (a program never
/// inherits the token strand itself was started with: spent, and meant
/// for strand's own window), [`INSTANCE_VARS`] removed, and the variables
/// strand set for itself ([`set_own_var`]) as it inherited them. Every
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
    give_back(cmd, &OWN_VARS.lock().unwrap_or_else(|e| e.into_inner()));
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
                ("GREETD_SOCK".into(), None),
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
                ("GREETD_SOCK".into(), None),
                ("STRAND_SOCKET".into(), None),
                ("STRAND_STATE_DIR".into(), None),
                ("XDG_ACTIVATION_TOKEN".into(), None),
            ]
        );
    }

    /// Signals 32 and 33, which libc keeps for itself and refuses to touch
    /// (a process manager above the test may leave 32 ignored).
    const LIBC_OWN: u64 = 0b11 << 31;

    /// The signal lines of a program's `/proc/self/status`, as hex words.
    fn signal_lines(status: &str) -> Vec<(String, u64)> {
        status
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                ["SigBlk", "SigIgn"].contains(&k).then(|| {
                    let held = u64::from_str_radix(v.trim(), 16).expect("a hex mask");
                    (k.to_string(), held & !LIBC_OWN)
                })
            })
            .collect()
    }

    /// (bead #17) A program starts with nothing blocked and nothing
    /// ignored, whatever the process that starts it holds: `signal_fd`'s
    /// mask, and any disposition strand itself was started with (`nohup`
    /// ignores SIGHUP, a shell's `&` SIGINT). Both survive `exec`, and
    /// std's `Command` keeps the mask on purpose. The child sets them up
    /// itself here so the test process keeps its own.
    #[test]
    fn a_program_starts_with_a_clean_signal_mask_and_default_dispositions() {
        use std::os::unix::process::CommandExt as _;
        let held = |clean: bool| {
            let mut cmd = std::process::Command::new("cat");
            cmd.arg("/proc/self/status");
            // SAFETY: only async-signal-safe calls, as `pre_exec` requires.
            unsafe {
                cmd.pre_exec(move || {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
                        libc::sigaddset(&mut set, sig);
                    }
                    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                    for sig in [libc::SIGINT, libc::SIGHUP, libc::SIGUSR2] {
                        libc::signal(sig, libc::SIG_IGN);
                    }
                    if clean {
                        restore_in_child();
                    }
                    Ok(())
                });
            }
            let out = cmd.output().expect("cat runs");
            signal_lines(&String::from_utf8_lossy(&out.stdout))
        };
        let bit = |sig: i32| 1u64 << (sig - 1);
        // The set-up does hold them when nothing cleans them.
        let dirty = held(false);
        assert_eq!(
            dirty,
            [
                (
                    "SigBlk".to_string(),
                    bit(libc::SIGINT) | bit(libc::SIGTERM) | bit(libc::SIGUSR1)
                ),
                (
                    "SigIgn".to_string(),
                    bit(libc::SIGINT) | bit(libc::SIGHUP) | bit(libc::SIGUSR2)
                ),
            ]
        );
        assert_eq!(
            held(true),
            [("SigBlk".to_string(), 0), ("SigIgn".to_string(), 0)]
        );
    }

    /// Every program the shell starts goes through [`restore_in_child`]:
    /// each file of the shell's own code that makes a `Command` calls it.
    /// `strand-auth` takes it as its `pre_exec` hook from its callers (a
    /// library without `strand-services`), and those are checked here.
    #[test]
    fn every_spawn_site_restores_the_child() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        // Code that starts nothing of a shell's: test support and benches.
        let not_the_shell = [
            "strand-services/src/testing.rs",
            "strand/src/bench.rs",
            "strand/src/run/tests.rs",
        ];
        // Tools beside the shell: they start programs from a terminal and
        // hold no signal state of their own to pass on.
        let not_a_shell_crate = ["strand-site", "strand-dev"];
        let mut files = Vec::new();
        let mut dirs = vec![crates.clone()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).expect("the crates are read") {
                let p = e.expect("an entry").path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "tests" && n != "target") {
                        dirs.push(p);
                    }
                } else if p.extension().is_some_and(|x| x == "rs")
                    && p.file_name().is_some_and(|n| n != "build.rs")
                {
                    files.push(p);
                }
            }
        }
        let mut missing = Vec::new();
        let mut seen = 0;
        for p in files {
            let rel = p.strip_prefix(&crates).expect("under crates");
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.contains("/src/")
                || not_the_shell.contains(&rel.as_str())
                || not_a_shell_crate.iter().any(|c| rel.starts_with(c))
            {
                continue;
            }
            let text = std::fs::read_to_string(&p).expect("a source file");
            // Tests start what they like.
            let shell = text.split("#[cfg(test)]\nmod ").next().unwrap_or_default();
            if !shell.contains("Command::new(") {
                continue;
            }
            seen += 1;
            if !shell.contains("restore_in_child") && !rel.starts_with("strand-auth/") {
                missing.push(rel);
            }
        }
        assert!(seen >= 7, "the walk found {seen} spawning files");
        assert_eq!(missing, Vec::<String>::new(), "no restore_in_child");
    }

    /// (gpu-first review) A variable strand set for itself goes back to
    /// what strand inherited: removed where it had none, its old value
    /// where it had one.
    #[test]
    fn a_program_gets_back_the_variables_strand_set_for_itself() {
        let own = [
            ("VK_LOADER_DRIVERS_DISABLE", None),
            ("STRAND_TEST_OWN", Some(OsString::from("theirs"))),
        ];
        let mut cmd = std::process::Command::new("true");
        give_back(&mut cmd, &own);
        let mut got: Vec<_> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(OsStr::to_os_string)))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("STRAND_TEST_OWN".into(), Some("theirs".into())),
                ("VK_LOADER_DRIVERS_DISABLE".into(), None),
            ]
        );
    }
}
