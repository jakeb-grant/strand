//! polkit's own password helper, as its authentication agents drive it
//! (`polkitagentsession.c`): `polkit-agent-helper-1`, setuid root and
//! started with the user's name as its argument, or (polkit 127 and
//! later, where it is not setuid) one systemd starts for each connection
//! to its socket, `/run/polkit/agent-helper.socket`, told the user's name
//! as the first line. Either way the agent then writes the request's
//! cookie on a line, and the helper runs PAM's `polkit-1` stack for that
//! user, writing one line per thing PAM says:
//!
//! | Line | Meaning |
//! |---|---|
//! | `PAM_PROMPT_ECHO_OFF <text>` | a hidden question: the agent writes the answer on a line |
//! | `PAM_PROMPT_ECHO_ON <text>` | a visible question, answered the same way |
//! | `PAM_ERROR_MSG <text>` | an error PAM shows |
//! | `PAM_TEXT_INFO <text>` | a text PAM shows |
//! | `SUCCESS` | authenticated: the helper has told polkit's authority itself |
//! | `FAILURE` | not authenticated |
//!
//! Each text is escaped with GLib's `g_strescape` (control characters,
//! `\` and `"` as escapes, bytes past ASCII as octal), undone here as
//! `g_strcompress` does. An answer is one line, read by the helper with
//! `fgets` into `PAM_MAX_RESP_SIZE` (512) bytes: one with a newline or a
//! NUL in it, or longer than 511 bytes, cannot be sent whole, so it is
//! refused ([`answer_line`]), never cut. Strand checks no password
//! itself: on `SUCCESS` the helper, not Strand, has answered polkit
//! (`AuthenticationAgentResponse2`).

use std::io;
use std::path::{Path, PathBuf};

use strand_auth::Password;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

/// The socket polkit 127 and later start a helper for, per connection.
pub const SOCKET: &str = "/run/polkit/agent-helper.socket";

/// The helper's name.
pub const HELPER_NAME: &str = "polkit-agent-helper-1";

/// Where distributions install the setuid helper: polkit's own
/// `$prefix/lib/polkit-1` (Arch, Debian, Ubuntu, Fedora), and the
/// `libexec` and older Debian directories some use.
pub const HELPER_DIRS: [&str; 5] = [
    "/usr/lib/polkit-1",
    "/usr/libexec",
    "/usr/libexec/polkit-1",
    "/usr/lib/policykit-1",
    "/usr/local/lib/polkit-1",
];

/// The longest answer the helper reads whole: `PAM_MAX_RESP_SIZE` (512)
/// less the newline's place.
pub const MAX_ANSWER: usize = 511;

/// The longest line the helper's output may hold before it is taken as
/// broken (a text PAM said is at most `PAM_MAX_MSG_SIZE`, 512 bytes, four
/// times that escaped).
const MAX_LINE: usize = 8 * 1024;

/// How the helper is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Helper {
    /// The socket to try first: a socket `owner` (root) owns.
    pub socket: Option<PathBuf>,
    /// The setuid helper, when one is installed (a file `owner` owns,
    /// setuid): used when there is no socket or it refuses the
    /// connection.
    pub program: Option<PathBuf>,
    /// (lockmon review) Whose they are (root): the peer that answers on
    /// the socket must be this uid, or nothing is sent to it.
    pub owner: u32,
}

impl Helper {
    /// The helper as `socket` and `program` say, or as installed
    /// ([`SOCKET`]; the first of [`HELPER_DIRS`] holding
    /// [`HELPER_NAME`]), taken only when `owner` (root; tests their own
    /// uid) owns it: a socket, or a setuid program. polkit 127 and later
    /// install the program without setuid, for the socket only; one used
    /// anyway could check nothing (it says it needs to be setuid root and
    /// fails every request), and Strand, registered, would keep another
    /// agent out. `Err` says why there is none: polkit's passwords
    /// cannot be checked here.
    pub fn find(
        socket: Option<&Path>,
        program: Option<&Path>,
        owner: u32,
    ) -> Result<Helper, String> {
        let socket_path = socket.map_or_else(|| PathBuf::from(SOCKET), Path::to_path_buf);
        let socket = owned_socket(&socket_path, owner).then_some(socket_path.clone());
        let candidates: Vec<PathBuf> = match program {
            Some(p) => vec![p.to_path_buf()],
            None => HELPER_DIRS
                .iter()
                .map(|d| Path::new(d).join(HELPER_NAME))
                .collect(),
        };
        let mut unusable = None;
        let program =
            candidates
                .into_iter()
                .filter(|p| p.is_file())
                .find(|p| match setuid_by(p, owner) {
                    Ok(()) => true,
                    Err(why) => {
                        unusable.get_or_insert(why);
                        false
                    }
                });
        if socket.is_some() || program.is_some() {
            return Ok(Helper {
                socket,
                program,
                owner,
            });
        }
        let socket_why = if socket_path.exists() {
            format!("{} is not a socket {}", socket_path.display(), whose(owner))
        } else {
            format!("there is no {}", socket_path.display())
        };
        Err(match unusable {
            Some(why) => format!(
                "polkit's password helper cannot be used ({why}, and {socket_why}): \
                 Strand cannot check polkit's passwords"
            ),
            None => format!(
                "polkit's password helper ({HELPER_NAME}) is not installed ({socket_why}): \
                 Strand cannot check polkit's passwords"
            ),
        })
    }
}

/// "root owns", or the uid that must.
fn whose(owner: u32) -> String {
    if owner == 0 {
        "root owns".into()
    } else {
        format!("uid {owner} owns")
    }
}

/// `path` is a socket `owner` owns (itself, not a link to one).
fn owned_socket(path: &Path, owner: u32) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket() && m.uid() == owner)
}

/// Why `path` is not a setuid program `owner` owns, if it is not.
fn setuid_by(path: &Path, owner: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.uid() != owner {
        return Err(format!("{} is not one {}", path.display(), whose(owner)));
    }
    if m.mode() & libc::S_ISUID == 0 {
        return Err(format!("{} is not setuid", path.display()));
    }
    Ok(())
}

/// One line of the helper's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    /// A question: its text, and whether its answer may be shown.
    Prompt {
        text: String,
        echo: bool,
    },
    /// A text PAM shows (`PAM_TEXT_INFO`).
    Info(String),
    /// An error PAM shows (`PAM_ERROR_MSG`).
    Error(String),
    Success,
    Failure,
    /// Anything else (ignored, as polkit's own agents ignore it).
    Unknown,
}

/// What a helper line says (without its newline).
pub fn parse_line(line: &[u8]) -> Line {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let after = |prefix: &[u8]| line.strip_prefix(prefix).map(unescape);
    if let Some(t) = after(b"PAM_PROMPT_ECHO_OFF ") {
        return Line::Prompt {
            text: t,
            echo: false,
        };
    }
    if let Some(t) = after(b"PAM_PROMPT_ECHO_ON ") {
        return Line::Prompt {
            text: t,
            echo: true,
        };
    }
    if let Some(t) = after(b"PAM_ERROR_MSG ") {
        return Line::Error(t);
    }
    if let Some(t) = after(b"PAM_TEXT_INFO ") {
        return Line::Info(t);
    }
    // polkit's agents take any line starting so.
    if line.starts_with(b"SUCCESS") {
        return Line::Success;
    }
    if line.starts_with(b"FAILURE") {
        return Line::Failure;
    }
    Line::Unknown
}

/// `s` with GLib's `g_strescape` undone (`g_strcompress`): `\b \f \n \r
/// \t \v`, octal `\ooo` (one to three digits, a byte), and `\` before any
/// other character standing for that character; then read as UTF-8, a
/// byte that is not replaced.
pub fn unescape(s: &[u8]) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c != b'\\' || i == s.len() {
            out.push(c);
            continue;
        }
        let e = s[i];
        i += 1;
        match e {
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                let mut n = 1;
                while n < 3 && i < s.len() && (b'0'..=b'7').contains(&s[i]) {
                    v = v * 8 + u32::from(s[i] - b'0');
                    i += 1;
                    n += 1;
                }
                out.push((v & 0xff) as u8);
            }
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why `answer` cannot be given to the helper, if it cannot: a newline
/// or NUL in it (the helper reads one line, as a C string), or more than
/// [`MAX_ANSWER`] bytes (the rest would answer the next question).
pub fn refusal(answer: &Password) -> Option<&'static str> {
    let b = answer.as_bytes();
    if b.contains(&b'\n') || b.contains(&b'\r') || b.contains(&0) {
        return Some("an answer cannot hold a line break or a NUL");
    }
    (b.len() > MAX_ANSWER).then_some("an answer longer than 511 bytes cannot be checked")
}

/// `answer` and its newline, in a buffer sized once and wiped when
/// dropped (no reallocation leaves a copy behind); `None` when
/// [`refusal`] refuses it.
pub fn answer_line(answer: &Password) -> Option<Zeroizing<Vec<u8>>> {
    if refusal(answer).is_some() {
        return None;
    }
    let mut line = Zeroizing::new(Vec::with_capacity(answer.len() + 1));
    line.extend_from_slice(answer.as_bytes());
    line.push(b'\n');
    Some(line)
}

/// What a conversation with the helper said, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Said {
    /// A question waits for an answer.
    Prompt { text: String, echo: bool },
    /// A text or an error PAM shows.
    Text(String),
    /// It ended: `Ok(true)` authenticated (the helper told polkit),
    /// `Ok(false)` not, `Err` it could not be made (no helper, the helper
    /// died or said something else), with why.
    Done(Result<bool, String>),
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// The helper for `user`, connected: through its socket when it exists
/// and accepts, else the setuid program. The child is killed when it is
/// dropped (the conversation ended or was abandoned). (lockmon review)
/// The socket's peer must be the helper's owner (`SO_PEERCRED`), checked
/// before anything is written; one that is not is refused, with no
/// setuid program tried instead: the trust is in who answers, not in
/// the path seen when the store started.
async fn open(
    helper: &Helper,
    user: &str,
) -> io::Result<(Reader, Writer, Option<tokio::process::Child>)> {
    if let Some(socket) = &helper.socket {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(s) => {
                let peer = s.peer_cred()?.uid();
                if peer != helper.owner {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is answered by uid {peer}, not the uid {} that owns it",
                            socket.display(),
                            helper.owner
                        ),
                    ));
                }
                let (r, mut w) = s.into_split();
                let mut first = Vec::with_capacity(user.len() + 1);
                first.extend_from_slice(user.as_bytes());
                first.push(b'\n');
                w.write_all(&first).await?;
                return Ok((Box::new(r), Box::new(w), None));
            }
            Err(e) if helper.program.is_some() => {
                log::info!(
                    "polkit: the helper's socket {} refused ({e}): the setuid helper instead",
                    socket.display()
                );
            }
            Err(e) => return Err(e),
        }
    }
    let program = helper.program.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no {HELPER_NAME} and no socket for it"),
        )
    })?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg(user)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    // PAM's texts in the user's language (the helper keeps these).
    for key in ["LANG", "LANGUAGE", "LC_ALL", "LC_MESSAGES"] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    // SAFETY: only async-signal-safe calls between fork and exec:
    // `restore_in_child` and close_range(2),
    // which marks descriptors close-on-exec (std's exec-error pipe keeps
    // working; an old kernel without it fails harmlessly).
    unsafe {
        cmd.pre_exec(|| {
            crate::child::restore_in_child();
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_uint,
            );
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(io::Error::other("the helper's pipes were not opened"));
    };
    Ok((Box::new(stdout), Box::new(stdin), Some(child)))
}

/// Reads one line of at most [`MAX_LINE`] bytes; `None` at the end.
async fn read_line(r: &mut BufReader<Reader>, buf: &mut Vec<u8>) -> io::Result<Option<()>> {
    buf.clear();
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok((!buf.is_empty()).then_some(()));
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take);
        if buf.len() > MAX_LINE {
            return Err(io::Error::other("the helper wrote a line too long"));
        }
        if done {
            return Ok(Some(()));
        }
    }
}

/// Authenticates `user` for the request `cookie` names: opens the
/// helper, tells it the cookie, and hands each thing it says to `said`
/// (ending with one [`Said::Done`]). A question waits for the next
/// answer on `answers`; their closing (the conversation abandoned) ends
/// it, killing the helper.
pub async fn converse(
    helper: Helper,
    user: String,
    cookie: String,
    mut answers: mpsc::UnboundedReceiver<Password>,
    said: mpsc::UnboundedSender<Said>,
) {
    let done = |r: Result<bool, String>| {
        let _ = said.send(Said::Done(r));
    };
    let (r, mut w, _child) = match open(&helper, &user).await {
        Ok(c) => c,
        Err(e) => return done(Err(format!("polkit's helper could not be started: {e}"))),
    };
    let mut first = Vec::with_capacity(cookie.len() + 1);
    first.extend_from_slice(cookie.as_bytes());
    first.push(b'\n');
    if let Err(e) = w.write_all(&first).await {
        return done(Err(format!("polkit's helper went away: {e}")));
    }
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    loop {
        match read_line(&mut r, &mut buf).await {
            Ok(Some(())) => {}
            Ok(None) => return done(Err("polkit's helper ended without an answer".into())),
            Err(e) => return done(Err(format!("polkit's helper: {e}"))),
        }
        match parse_line(&buf) {
            Line::Prompt { text, echo } => {
                let _ = said.send(Said::Prompt { text, echo });
                // Abandoned: dropping the child kills the helper.
                let Some(answer) = answers.recv().await else {
                    return;
                };
                let Some(line) = answer_line(&answer) else {
                    // Refused before it was sent (the service checks
                    // first); the helper is told nothing it would cut.
                    return done(Ok(false));
                };
                drop(answer);
                if let Err(e) = async {
                    w.write_all(&line).await?;
                    w.flush().await
                }
                .await
                {
                    return done(Err(format!("polkit's helper went away: {e}")));
                }
            }
            Line::Info(t) | Line::Error(t) => {
                let _ = said.send(Said::Text(t));
            }
            Line::Success => return done(Ok(true)),
            Line::Failure => return done(Ok(false)),
            Line::Unknown => {
                log::debug!("polkit: an unknown line from the helper was ignored");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse_as_polkits_agents_read_them() {
        assert_eq!(
            parse_line(b"PAM_PROMPT_ECHO_OFF Password: \n"),
            Line::Prompt {
                text: "Password: ".into(),
                echo: false
            }
        );
        assert_eq!(
            parse_line(b"PAM_PROMPT_ECHO_ON login:"),
            Line::Prompt {
                text: "login:".into(),
                echo: true
            }
        );
        assert_eq!(
            parse_line(b"PAM_TEXT_INFO Place your finger\\n"),
            Line::Info("Place your finger\n".into())
        );
        assert_eq!(
            parse_line(b"PAM_ERROR_MSG Authentication failure"),
            Line::Error("Authentication failure".into())
        );
        assert_eq!(parse_line(b"SUCCESS\n"), Line::Success);
        assert_eq!(parse_line(b"FAILURE\n"), Line::Failure);
        assert_eq!(parse_line(b"PAM_SOMETHING x"), Line::Unknown);
    }

    #[test]
    fn escapes_are_undone_as_g_strcompress_does() {
        // g_strescape("Contraseña \"x\"\t\\") on UTF-8 bytes: octal escapes.
        assert_eq!(
            unescape(br#"Contrase\303\261a \"x\"\t\\"#),
            "Contraseña \"x\"\t\\"
        );
        assert_eq!(unescape(br"\101\0\1z"), "A\0\u{1}z");
        assert_eq!(unescape(br"a\qb\"), "aqb\\");
        assert_eq!(unescape(br"\b\f\v\r"), "\u{8}\u{c}\u{b}\r");
    }

    #[test]
    fn an_answer_is_sent_whole_or_not_at_all() {
        let ok = Password::from("hunter2".to_string());
        assert_eq!(answer_line(&ok).unwrap().as_slice(), b"hunter2\n");
        for bad in ["two\nlines", "carriage\rreturn", "nul\0byte"] {
            assert!(
                answer_line(&Password::from(bad.to_string())).is_none(),
                "{bad:?}"
            );
        }
        let longest = Password::from("x".repeat(MAX_ANSWER));
        assert!(answer_line(&longest).is_some());
        let longer = Password::from("x".repeat(MAX_ANSWER + 1));
        assert!(refusal(&longer).is_some());
    }

    /// (lockmon review) The socket is used when it accepts and its peer
    /// is the helper's owner; the setuid program only when connecting is
    /// refused; a peer that is not the owner is refused with nothing
    /// written and no program tried.
    #[test]
    fn the_socket_first_and_only_its_owners() {
        use std::io::Read as _;
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent-helper.socket");
        let program = dir.path().join("helper");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let both = Helper {
            socket: Some(socket.clone()),
            program: Some(program.clone()),
            owner: me,
        };
        // Accepting, its peer the owner: the socket, the user's name on it.
        let (_r, _w, child) = rt.block_on(open(&both, "tester")).unwrap();
        assert!(child.is_none(), "the socket, not the program");
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut got = [0u8; 7];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"tester\n");
        // Its peer another uid: refused, nothing written, no program.
        let other = Helper {
            owner: me.wrapping_add(1),
            ..both.clone()
        };
        let e = rt.block_on(open(&other, "tester")).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
        let (mut peer, _) = listener.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        let mut buf = [0u8; 1];
        assert!(
            matches!(peer.read(&mut buf), Ok(0)),
            "nothing reaches a peer that is not the owner"
        );
        // Connecting refused (no one listens): the program.
        drop(listener);
        let (_r, _w, child) = rt.block_on(open(&both, "tester")).unwrap();
        assert!(
            child.is_some(),
            "the setuid program when the socket refuses"
        );
        // A link to the socket is not the socket.
        let link = dir.path().join("link.socket");
        let _again = std::os::unix::net::UnixListener::bind(dir.path().join("real.socket"));
        std::os::unix::fs::symlink(dir.path().join("real.socket"), &link).unwrap();
        assert!(!owned_socket(&link, me));
        assert!(owned_socket(&dir.path().join("real.socket"), me));
    }

    #[test]
    fn only_a_setuid_program_or_a_socket_its_owner_owns_is_a_helper() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("none.socket");
        let program = dir.path().join("helper");
        let why = Helper::find(Some(&socket), Some(&program), me).unwrap_err();
        assert!(why.contains("is not installed"), "{why}");

        // Installed without setuid (polkit 127's, meant for its socket):
        // it could check nothing.
        std::fs::write(&program, "").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let why = Helper::find(Some(&socket), Some(&program), me).unwrap_err();
        assert!(why.contains("is not setuid"), "{why}");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o4755)).unwrap();
        let found = Helper::find(Some(&socket), Some(&program), me).unwrap();
        assert_eq!(found.program.as_deref(), Some(program.as_path()));
        assert_eq!(found.socket, None);
        // Setuid, but not root's.
        if me != 0 {
            let why = Helper::find(Some(&socket), Some(&program), 0).unwrap_err();
            assert!(why.contains("not one root owns"), "{why}");
        }

        // A file where the socket should be is no socket.
        std::fs::write(&socket, "").unwrap();
        let why = Helper::find(Some(&socket), Some(&dir.path().join("none")), me).unwrap_err();
        assert!(why.contains("is not a socket"), "{why}");
        std::fs::remove_file(&socket).unwrap();
        let _listening = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let found = Helper::find(Some(&socket), Some(&dir.path().join("none")), me).unwrap();
        assert_eq!(found.socket.as_deref(), Some(socket.as_path()));
        if me != 0 {
            assert!(Helper::find(Some(&socket), Some(&dir.path().join("none")), 0).is_err());
        }
    }
}
