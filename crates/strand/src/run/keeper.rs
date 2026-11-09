//! (proc-keeper) The keeper: a job's output must never kill it
//! (decisions.md proc-keeper; spec.md 5.7).
//!
//! An action without a result (a job) has its stdout and stderr piped to
//! strand, which reads them into its `lines`. Were strand the only reader,
//! its exit (a crash, `pkill -f '^strand run'`, a restart) would leave the
//! job writing into pipes nobody reads: the next write raises SIGPIPE
//! (pacman exits 141 mid-transaction, Python 120). So strand starts one
//! keeper, lazily, the first time a job runs: `/proc/self/exe __keep`
//! (dispatched first in `main`), joined to strand by a socket pair, in a
//! process group of its own and with nothing of strand's to inherit
//! (`child::child_env`, `child::restore_in_child`).
//!
//! Strand sends it a duplicate of each run's pipe read ends
//! ([`Keeper::hold`], `SCM_RIGHTS`) and does all the reading; the keeper
//! sleeps. When strand is done with a run's pipes ([`Keeper::release`]),
//! the keeper reads what is left of them to their end, discarding it,
//! and closes them (a process the job left in the background may hold
//! them for long; it never blocks on a full pipe). When strand goes, the
//! socket ends: the keeper reads every pipe it still holds to its end,
//! discarding the bytes, and exits once none is left. A job thus never
//! gets SIGPIPE, and never blocks on a full pipe, because strand went.
//!
//! The protocol: one datagram per message, 9 bytes, a tag (1 hold, 2
//! release) and the run's id (little-endian `u64`); a hold carries the
//! fds.

use std::collections::HashMap;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg, sendmsg, socketpair,
};

/// The argument `main` dispatches to [`main`].
pub const ARG: &str = "__keep";

const HOLD: u8 = 1;
const RELEASE: u8 = 2;

/// The keeper as strand holds it: started on the first [`Keeper::hold`],
/// started again when it went.
#[derive(Default)]
pub struct Keeper {
    inner: Mutex<Option<Running>>,
    /// The next run's id ([`Keeper::next_run`]).
    ids: std::sync::atomic::AtomicU64,
}

struct Running {
    socket: OwnedFd,
    child: std::process::Child,
}

impl std::fmt::Debug for Keeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keeper").finish_non_exhaustive()
    }
}

fn message(tag: u8, run: u64) -> [u8; 9] {
    let mut m = [0u8; 9];
    m[0] = tag;
    m[1..].copy_from_slice(&run.to_le_bytes());
    m
}

impl Keeper {
    /// An id for a run's pipes, unique among the runs that share this
    /// keeper (a job's, a `file`'s `run:`).
    pub fn next_run(&self) -> u64 {
        self.ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Hands run `run`'s pipe read ends `fds` to the keeper (duplicates:
    /// strand keeps its own), starting it first if it does not run.
    /// Failing is logged: the job runs on without it.
    pub fn hold(&self, run: u64, fds: &[BorrowedFd<'_>]) {
        if fds.is_empty() {
            return;
        }
        let mut inner = super::exec::lock(&self.inner);
        for attempt in 0..2 {
            if inner.is_none() {
                match start() {
                    Ok(r) => *inner = Some(r),
                    Err(e) => {
                        log::warn!("no keeper for the actions' output: {e}");
                        return;
                    }
                }
            }
            let Some(r) = inner.as_ref() else { return };
            match send(&r.socket, &message(HOLD, run), fds) {
                Ok(()) => return,
                // It went (killed, or it failed): another, once.
                Err(e) => {
                    if let Some(mut r) = inner.take() {
                        let _ = r.child.try_wait();
                    }
                    if attempt == 1 {
                        log::warn!("the actions' output keeper is gone: {e}");
                    }
                }
            }
        }
    }

    /// Strand is done with run `run`'s pipes: the keeper reads them to
    /// their end and closes them.
    pub fn release(&self, run: u64) {
        let inner = super::exec::lock(&self.inner);
        if let Some(r) = inner.as_ref() {
            let _ = send(&r.socket, &message(RELEASE, run), &[]);
        }
    }

    /// The keeper's pid, while it runs (tests).
    #[cfg(test)]
    pub fn pid(&self) -> Option<u32> {
        super::exec::lock(&self.inner)
            .as_ref()
            .map(|r| r.child.id())
    }
}

fn send(socket: &OwnedFd, msg: &[u8], fds: &[BorrowedFd<'_>]) -> std::io::Result<()> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(std::io::Error::other("too many fds"));
    }
    sendmsg(
        socket,
        &[IoSlice::new(msg)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    Ok(())
}

/// The program that is the keeper and its arguments: this program,
/// `__keep`.
#[cfg(not(test))]
fn program() -> (
    std::path::PathBuf,
    Vec<String>,
    Option<(&'static str, &'static str)>,
) {
    ("/proc/self/exe".into(), vec![ARG.into()], None)
}

/// In tests the program is the test binary: it runs one test,
/// [`tests::keeper_main`], which serves.
#[cfg(test)]
fn program() -> (
    std::path::PathBuf,
    Vec<String>,
    Option<(&'static str, &'static str)>,
) {
    let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
    let args = [
        "--exact",
        "run::keeper::tests::keeper_main",
        "--test-threads=1",
        "--nocapture",
    ];
    (
        exe,
        args.map(String::from).to_vec(),
        Some((tests::KEEPER, "1")),
    )
}

fn start() -> std::io::Result<Running> {
    let (ours, theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let (exe, args, env) = program();
    let mut cmd = Command::new(exe);
    cmd.args(args);
    if let Some((k, v)) = env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::from(theirs))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    strand_services::child::child_env(&mut cmd, None);
    // SAFETY: `restore_in_child` makes only async-signal-safe calls, as
    // `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            strand_services::child::restore_in_child();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok(Running {
        socket: ours,
        child,
    })
}

/// `strand __keep`: serves strand on its stdin (the socket) until strand
/// has gone and every pipe it was given has ended.
pub fn main() -> ExitCode {
    let socket = std::io::stdin().as_fd().try_clone_to_owned();
    match socket {
        Ok(s) => {
            serve(&s);
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::FAILURE,
    }
}

/// The keeper's loop: see the module docs.
pub fn serve(socket: &OwnedFd) {
    // Each run's pipes, and whether strand released them (they are read).
    let mut held: HashMap<u64, (Vec<OwnedFd>, bool)> = HashMap::new();
    let mut strand = true;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // While strand runs, only its socket and released pipes are
        // read; once it went, every pipe.
        let mut fds = Vec::new();
        let mut which = Vec::new();
        if strand {
            fds.push(PollFd::new(socket, PollFlags::IN));
        }
        for (run, (pipes, released)) in &held {
            if *released || !strand {
                for (i, p) in pipes.iter().enumerate() {
                    fds.push(PollFd::new(p, PollFlags::IN));
                    which.push((*run, i));
                }
            }
        }
        if fds.is_empty() {
            return;
        }
        match poll(&mut fds, None) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
        let socket_ready = strand && !fds[0].revents().is_empty();
        let base = usize::from(strand);
        let ready: Vec<(u64, usize)> = fds[base..]
            .iter()
            .zip(&which)
            .filter(|(f, _)| !f.revents().is_empty())
            .map(|(_, w)| *w)
            .collect();
        drop(fds);
        if socket_ready && !receive(socket, &mut held) {
            strand = false;
        }
        // Read what each ready pipe holds, discarding it; at its end it
        // is closed.
        let mut ended = Vec::new();
        for (run, i) in ready {
            let Some((pipes, _)) = held.get(&run) else {
                continue;
            };
            let Some(p) = pipes.get(i) else { continue };
            match rustix::io::read(p, &mut buf) {
                Ok(0) => ended.push((run, i)),
                Ok(_) | Err(rustix::io::Errno::INTR) | Err(rustix::io::Errno::AGAIN) => {}
                Err(_) => ended.push((run, i)),
            }
        }
        // Highest index first, so the others keep theirs.
        ended.sort_unstable_by_key(|e| std::cmp::Reverse(e.1));
        for (run, i) in ended {
            if let Some((pipes, _)) = held.get_mut(&run) {
                if i < pipes.len() {
                    pipes.remove(i);
                }
                if pipes.is_empty() {
                    held.remove(&run);
                }
            }
        }
        if !strand && held.is_empty() {
            return;
        }
    }
}

/// One message from strand into `held`; false: strand has gone.
fn receive(socket: &OwnedFd, held: &mut HashMap<u64, (Vec<OwnedFd>, bool)>) -> bool {
    let mut msg = [0u8; 9];
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut msg)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    );
    let n = match got {
        Ok(r) => r.bytes,
        Err(rustix::io::Errno::INTR) | Err(rustix::io::Errno::AGAIN) => return true,
        Err(_) => return false,
    };
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(got) = m {
            fds.extend(got);
        }
    }
    if n == 0 {
        return false;
    }
    if n != msg.len() {
        return true;
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&msg[1..]);
    let run = u64::from_le_bytes(id);
    match msg[0] {
        HOLD => {
            for fd in &fds {
                set_nonblocking(fd.as_fd());
            }
            held.insert(run, (fds, false));
        }
        RELEASE => {
            if let Some(h) = held.get_mut(&run) {
                h.1 = true;
            }
        }
        _ => {}
    }
    true
}

fn set_nonblocking(fd: BorrowedFd<'_>) {
    if let Ok(flags) = rustix::fs::fcntl_getfl(fd) {
        let _ = rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// Set in the test binary started as the keeper.
    pub const KEEPER: &str = "STRAND_TEST_KEEPER";

    /// Not a test: the test binary started as the keeper serves here.
    #[test]
    fn keeper_main() {
        if std::env::var_os(KEEPER).is_none() {
            return;
        }
        use std::os::fd::AsFd as _;
        let Ok(socket) = std::io::stdin().as_fd().try_clone_to_owned() else {
            std::process::exit(1);
        };
        super::serve(&socket);
        std::process::exit(0);
    }
}
