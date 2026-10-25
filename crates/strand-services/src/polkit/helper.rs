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
    /// The socket to try first (it is used when it exists).
    pub socket: PathBuf,
    /// The setuid helper, when one is installed: used when the socket is
    /// missing or refuses the connection.
    pub program: Option<PathBuf>,
}

impl Helper {
    /// The helper as `socket` and `program` say, or as installed
    /// ([`SOCKET`]; the first of [`HELPER_DIRS`] holding
    /// [`HELPER_NAME`]). `None` when neither exists: polkit's passwords
    /// cannot be checked here.
    pub fn find(socket: Option<&Path>, program: Option<&Path>) -> Option<Helper> {
        let socket = socket.map_or_else(|| PathBuf::from(SOCKET), Path::to_path_buf);
        let program = match program {
            Some(p) => Some(p.to_path_buf()).filter(|p| p.is_file()),
            None => HELPER_DIRS
                .iter()
                .map(|d| Path::new(d).join(HELPER_NAME))
                .find(|p| p.is_file()),
        };
        (socket.exists() || program.is_some()).then_some(Helper { socket, program })
    }
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
/// dropped (the conversation ended or was abandoned).
async fn open(
    helper: &Helper,
    user: &str,
) -> io::Result<(Reader, Writer, Option<tokio::process::Child>)> {
    if helper.socket.exists() {
        match tokio::net::UnixStream::connect(&helper.socket).await {
            Ok(s) => {
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
                    helper.socket.display()
                );
            }
            Err(e) => return Err(e),
        }
    }
    let program = helper.program.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no {HELPER_NAME} and no {}", helper.socket.display()),
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
    // `restore_in_child` (an atomic load and `prctl`) and close_range(2),
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

    #[test]
    fn no_helper_is_found_where_there_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("none.socket");
        let program = dir.path().join("none");
        assert_eq!(Helper::find(Some(&socket), Some(&program)), None);
        std::fs::write(&program, "").unwrap();
        let found = Helper::find(Some(&socket), Some(&program)).unwrap();
        assert_eq!(found.program.as_deref(), Some(program.as_path()));
    }
}
