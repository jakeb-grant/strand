//! (proc-lines, proc-streams, proc-reason; decisions.md) How strand reads
//! every program it runs (spec.md 5.8): a `from listen` or polled
//! command, an action's command and a `file`'s `run:`.
//!
//! - [`Framer`]: bytes in, ended lines and the one open line out, as a
//!   terminal shows them: `\n` and `\r\n` end a line; a `\r` not before
//!   `\n` starts the line again, what follows replacing it (a `\r` at the
//!   end of what was read waits for the next byte); `\b` removes the
//!   character before it; `ESC [ … final`, `ESC ] … BEL` or `ESC \`
//!   (and the other string escapes, `ESC P`, `X`, `^`, `_`), `ESC` and one
//!   byte (and its intermediate bytes before it: `ESC ( B`), and any
//!   other control character but `\t` are removed. A line
//!   is never ended by a pause. A line past [`MAX_DOCUMENT`] bytes is
//!   [`Framed::over`]: skipped for fields and events, kept cut in a
//!   [`Ring`].
//! - [`Tail`]: why a run failed, from what it printed: its stderr's
//!   non-blank lines whole up to [`TAIL_WHOLE`] bytes, else the first
//!   [`TAIL_HEAD`] and the last [`TAIL_END`], each cut at a line, joined
//!   by a line `…`; when stderr was empty, the last [`TAIL_STDOUT`] of
//!   stdout.
//! - [`Ring`]: an action's last [`RING_LINES`] lines, at most
//!   [`RING_BYTES`], each cut to [`SHOWN_CHARS`] characters, with what
//!   changed since it was last published ([`Ring::take`]).
//! - [`Reader`]: `poll(2)` over a command's stdout and stderr (and the
//!   caller's own wake-ups), reading what is there without blocking;
//!   stdout is left unread while the caller holds it back (a `line`
//!   handler's backlog, [`BACKLOG_LINES`]), stderr always drained.
//!
//! The services thread reads its commands on tokio and shares the
//! [`Framer`] and [`Tail`] ([`crate::custom`]); the strand crate's
//! threads (actions, `run:`) use the [`Reader`].

use std::collections::VecDeque;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use crate::custom::MAX_DOCUMENT;

/// A line shown in a [`Ring`] is cut to this many characters, then `…`.
pub const SHOWN_CHARS: usize = 2000;
/// A [`Ring`] keeps at most this many lines…
pub const RING_LINES: usize = 1000;
/// …and at most this many bytes of their text.
pub const RING_BYTES: usize = 256 * 1024;
/// A run's stderr is its reason whole up to this many bytes…
pub const TAIL_WHOLE: usize = 4096;
/// …past that its first this many…
pub const TAIL_HEAD: usize = 1024;
/// …and its last this many.
pub const TAIL_END: usize = 3072;
/// With nothing on stderr, the last this many bytes of stdout.
pub const TAIL_STDOUT: usize = 1024;
/// After a command ends, how long its stderr is still read: a process it
/// left in the background may hold the pipe open for good.
pub const STDERR_GRACE: Duration = Duration::from_millis(100);
/// While more than this many lines…
pub const BACKLOG_LINES: usize = 4096;
/// …or this many bytes wait for `line` handlers, a command's stdout is
/// not read.
pub const BACKLOG_BYTES: usize = 4 << 20;
/// What a line kept past [`MAX_DOCUMENT`] holds of its start (a ring
/// shows [`SHOWN_CHARS`] of it).
const OVER_KEPT: usize = 4 * SHOWN_CHARS;

/// Which pipe a line came on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

impl Stream {
    /// It is stderr.
    pub fn is_err(self) -> bool {
        self == Stream::Err
    }
}

/// An ended line, framed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Framed {
    /// Its bytes, controls removed (at most [`OVER_KEPT`] of a line that
    /// is [`Framed::over`]).
    pub bytes: Vec<u8>,
    /// It was longer than [`MAX_DOCUMENT`] bytes.
    pub over: bool,
}

impl Framed {
    /// Its text: UTF-8, with U+FFFD for what is not.
    pub fn text(&self) -> String {
        decode(&self.bytes)
    }

    /// Nothing but whitespace.
    pub fn is_blank(&self) -> bool {
        self.bytes.iter().all(u8::is_ascii_whitespace)
    }
}

/// A line's bytes as text, as a document's are
/// ([`crate::custom::decode_document`]): UTF-8, with U+FFFD for what is
/// not (windows-1252 after an XML declaration naming Latin-1).
pub fn decode(bytes: &[u8]) -> String {
    crate::custom::decode_document(bytes).into_owned()
}

/// Where the [`Framer`] is in an escape sequence.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum Esc {
    #[default]
    No,
    /// After `ESC`.
    Start,
    /// In `ESC [ …`, until its final byte.
    Csi,
    /// After `ESC` and an intermediate byte (`ESC ( B`, which `tput sgr0`
    /// prints): until a final byte.
    Inter,
    /// In a string (`ESC ]`, `P`, `X`, `^`, `_`), until `BEL` or `ESC \`.
    Str,
    /// An `ESC` inside a string: `\` ends it.
    StrEnd,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Framer {
    cur: Vec<u8>,
    over: bool,
    /// A `\r` read, waiting for the next byte.
    cr: bool,
    esc: Esc,
    /// The open line changed since [`Framer::take_changed`].
    changed: bool,
}

impl Framer {
    pub fn new() -> Framer {
        Framer::default()
    }

    /// Frames `bytes`, giving each line they end to `f`.
    pub fn push(&mut self, bytes: &[u8], f: &mut dyn FnMut(Framed)) {
        for &b in bytes {
            self.byte(b, f);
        }
    }

    fn byte(&mut self, b: u8, f: &mut dyn FnMut(Framed)) {
        if self.esc != Esc::No {
            match b {
                // A line's end ends any escape, then ends the line: an
                // escape never runs past a line, so one cut short (a
                // title `ESC ]` with no `BEL`, a stray `ESC P`) cannot
                // swallow the rest of the output.
                b'\n' => self.esc = Esc::No,
                // CAN and SUB abort an escape, as a terminal's parser
                // does, and are themselves dropped.
                0x18 | 0x1a => {
                    self.esc = Esc::No;
                    return;
                }
                _ => {}
            }
        }
        match self.esc {
            Esc::No => {}
            Esc::Start => {
                self.esc = match b {
                    b'[' => Esc::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => Esc::Str,
                    // `ESC ESC`: the second starts again.
                    0x1b => Esc::Start,
                    0x20..=0x2f => Esc::Inter,
                    _ => Esc::No,
                };
                return;
            }
            Esc::Inter => {
                if !(0x20..=0x2f).contains(&b) {
                    self.esc = Esc::No;
                }
                return;
            }
            Esc::Csi => {
                // Its final byte.
                if (0x40..=0x7e).contains(&b) {
                    self.esc = Esc::No;
                }
                return;
            }
            Esc::Str => {
                match b {
                    0x07 => self.esc = Esc::No,
                    0x1b => self.esc = Esc::StrEnd,
                    _ => {}
                }
                return;
            }
            Esc::StrEnd => {
                self.esc = match b {
                    b'\\' => Esc::No,
                    0x1b => Esc::StrEnd,
                    _ => Esc::Str,
                };
                return;
            }
        }
        if self.cr {
            match b {
                b'\n' => {
                    self.cr = false;
                    self.end(f);
                    return;
                }
                // `\r\r`: still a carriage return.
                b'\r' => return,
                _ => {
                    // The line starts again.
                    self.cr = false;
                    if !self.cur.is_empty() || self.over {
                        self.cur.clear();
                        self.over = false;
                        self.changed = true;
                    }
                }
            }
        }
        match b {
            b'\n' => self.end(f),
            b'\r' => self.cr = true,
            0x08 => {
                // The character before it, whole.
                while let Some(last) = self.cur.pop() {
                    self.changed = true;
                    if last & 0xc0 != 0x80 {
                        break;
                    }
                }
            }
            0x1b => self.esc = Esc::Start,
            b'\t' => self.keep(b),
            0x00..=0x1f | 0x7f => {}
            _ => self.keep(b),
        }
    }

    fn keep(&mut self, b: u8) {
        if self.over {
            return;
        }
        if self.cur.len() >= MAX_DOCUMENT {
            self.over = true;
            self.cur.truncate(OVER_KEPT);
            return;
        }
        self.cur.push(b);
        self.changed = true;
    }

    fn end(&mut self, f: &mut dyn FnMut(Framed)) {
        let bytes = std::mem::take(&mut self.cur);
        let over = std::mem::replace(&mut self.over, false);
        self.changed = true;
        f(Framed { bytes, over });
    }

    /// The output ended: its open line, if any, ends with it.
    pub fn finish(&mut self, f: &mut dyn FnMut(Framed)) {
        self.esc = Esc::No;
        self.cr = false;
        if !self.cur.is_empty() || self.over {
            self.end(f);
        }
    }

    /// The line no `\n` has ended yet, if it holds anything.
    pub fn open(&self) -> Option<&[u8]> {
        (!self.cur.is_empty()).then_some(&self.cur[..])
    }

    /// Whether the open line changed (or ended) since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::replace(&mut self.changed, false)
    }

    /// Frames a whole output at once (a polled command's, a `-> T`
    /// call's): its lines joined by `\n`, the open one ended.
    pub fn whole(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut framer = Framer::new();
        let mut first = true;
        let mut join = |l: Framed| {
            if !first {
                out.push(b'\n');
            }
            first = false;
            out.extend_from_slice(&l.bytes);
        };
        framer.push(bytes, &mut join);
        framer.finish(&mut join);
        out
    }
}

/// `text` cut to its first `max` bytes, at a character.
fn head_of(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

/// `text` cut to its last `max` bytes, at a character.
fn end_of(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = text.len() - max;
    while !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..]
}

/// Lines kept to at most `max` bytes (each counted with its line
/// break), the oldest leaving first; a line alone longer is cut to its
/// end.
#[derive(Debug, Default)]
struct Last {
    lines: VecDeque<String>,
    bytes: usize,
}

impl Last {
    fn push(&mut self, line: String, max: usize) {
        let line = if line.len() + 1 > max {
            end_of(&line, max.saturating_sub(1)).to_string()
        } else {
            line
        };
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > max {
            match self.lines.pop_front() {
                Some(l) => self.bytes -= l.len() + 1,
                None => break,
            }
        }
    }

    fn joined(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Tail {
    /// stderr's lines while they fit [`TAIL_WHOLE`]; past that, its last
    /// [`TAIL_END`].
    err: Last,
    /// stderr's first [`TAIL_HEAD`], once it outgrew [`TAIL_WHOLE`].
    head: Option<Vec<String>>,
    out: Last,
}

impl Tail {
    pub fn new() -> Tail {
        Tail::default()
    }

    /// A line the command printed on `stream` (framed; blank lines are
    /// not kept).
    pub fn push(&mut self, stream: Stream, line: &str) {
        let line = line.trim_end();
        if line.trim().is_empty() {
            return;
        }
        match stream {
            Stream::Out => self.out.push(line.to_string(), TAIL_STDOUT),
            Stream::Err if self.head.is_some() => self.err.push(line.to_string(), TAIL_END),
            Stream::Err => {
                self.err.bytes += line.len() + 1;
                self.err.lines.push_back(line.to_string());
                if self.err.bytes > TAIL_WHOLE {
                    self.split();
                }
            }
        }
    }

    /// stderr outgrew [`TAIL_WHOLE`]: its first [`TAIL_HEAD`] are kept
    /// apart, and the rest from then on to [`TAIL_END`].
    fn split(&mut self) {
        let mut head = Vec::new();
        let mut used = 0;
        while let Some(l) = self.err.lines.front() {
            if used + l.len() + 1 > TAIL_HEAD {
                if head.is_empty() {
                    // A first line longer than the head: its start.
                    head.push(head_of(l, TAIL_HEAD - 1).to_string());
                    self.err.lines.pop_front();
                }
                break;
            }
            used += l.len() + 1;
            if let Some(l) = self.err.lines.pop_front() {
                head.push(l);
            }
        }
        let rest = std::mem::take(&mut self.err);
        for l in rest.lines {
            self.err.push(l, TAIL_END);
        }
        self.head = Some(head);
    }

    /// Why the run failed, from what it printed: `None` when it printed
    /// nothing.
    pub fn reason(&self) -> Option<String> {
        if let Some(head) = &self.head {
            let mut lines = head.clone();
            lines.push("…".into());
            lines.extend(self.err.lines.iter().cloned());
            return Some(lines.join("\n"));
        }
        if !self.err.lines.is_empty() {
            return Some(self.err.joined());
        }
        (!self.out.lines.is_empty()).then(|| self.out.joined())
    }

    /// `headline`, then `": "` and [`Tail::reason`] when there is one.
    pub fn explain(&self, headline: &str) -> String {
        match self.reason() {
            Some(r) => format!("{headline}: {r}"),
            None => headline.to_string(),
        }
    }
}

/// A line of a [`Ring`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingLine {
    /// Its number in the run, from 1.
    pub n: u64,
    /// As a terminal shows it, cut to [`SHOWN_CHARS`] characters.
    pub text: String,
    pub stderr: bool,
    /// No line break yet.
    pub open: bool,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Ring {
    lines: VecDeque<RingLine>,
    bytes: usize,
    /// The last number given.
    last: u64,
    /// Each stream's open line's number, while it has one.
    open: [Option<u64>; 2],
    /// The last number [`Ring::take`] gave out.
    sent: u64,
    /// Lines up to `sent` changed since (an open line redrawn or ended).
    redrawn: Vec<u64>,
    /// The front's number at the last [`Ring::take`].
    sent_front: u64,
}

/// `text` as a [`Ring`] keeps it: cut to [`SHOWN_CHARS`] characters and
/// `…`.
pub fn shown(text: &str) -> String {
    match text.char_indices().nth(SHOWN_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

impl Ring {
    pub fn new() -> Ring {
        Ring::default()
    }

    fn slot(stream: Stream) -> usize {
        usize::from(stream.is_err())
    }

    /// A line of `stream` ended: its open line, if it had one, ends with
    /// this text.
    pub fn end(&mut self, stream: Stream, text: &str) {
        match self.open[Self::slot(stream)].take() {
            Some(n) => self.redraw(n, text, false),
            None => {
                self.add(stream, text, false);
            }
        }
    }

    /// `stream`'s open line now reads `text` (`None`: nothing, after a
    /// `\r`; a stream that has none yet gets none).
    pub fn open(&mut self, stream: Stream, text: Option<&str>) {
        let slot = Self::slot(stream);
        match (self.open[slot], text) {
            (Some(n), Some(t)) => self.redraw(n, t, true),
            (None, Some(t)) => {
                let n = self.add(stream, t, true);
                self.open[slot] = Some(n);
            }
            // Redrawn to nothing yet (`\r` and an erase): kept, empty,
            // so numbers only ever grow.
            (Some(n), None) => self.redraw(n, "", true),
            (None, None) => {}
        }
    }

    fn add(&mut self, stream: Stream, text: &str, open: bool) -> u64 {
        self.last += 1;
        let text = shown(text);
        self.bytes += text.len();
        self.lines.push_back(RingLine {
            n: self.last,
            text,
            stderr: stream.is_err(),
            open,
        });
        while self.lines.len() > RING_LINES || self.bytes > RING_BYTES {
            let Some(l) = self.lines.pop_front() else {
                break;
            };
            self.bytes -= l.text.len();
            for o in &mut self.open {
                if *o == Some(l.n) {
                    *o = None;
                }
            }
        }
        self.last
    }

    fn redraw(&mut self, n: u64, text: &str, open: bool) {
        let Some(l) = self.lines.iter_mut().rev().find(|l| l.n == n) else {
            return;
        };
        let text = shown(text);
        if l.text == text && l.open == open {
            return;
        }
        self.bytes = self.bytes - l.text.len() + text.len();
        l.text = text;
        l.open = open;
        if n <= self.sent {
            self.redrawn.push(n);
        }
    }

    /// The output ended: each open line is ended as it reads.
    pub fn close(&mut self) {
        for slot in 0..2 {
            let Some(n) = self.open[slot].take() else {
                continue;
            };
            if let Some(l) = self.lines.iter_mut().find(|l| l.n == n) {
                l.open = false;
                if n <= self.sent {
                    self.redrawn.push(n);
                }
            }
        }
    }

    /// The lines it holds, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = &RingLine> {
        self.lines.iter()
    }

    /// What changed since the last call: the number below which lines
    /// left the front, and the lines added or changed (a line asked for
    /// that has left since is not given). `None`: nothing changed.
    pub fn take(&mut self) -> Option<(u64, Vec<RingLine>)> {
        let front = self.lines.front().map_or(self.last + 1, |l| l.n);
        let mut redrawn = std::mem::take(&mut self.redrawn);
        redrawn.sort_unstable();
        redrawn.dedup();
        let mut out: Vec<RingLine> = redrawn
            .iter()
            .filter_map(|n| self.lines.iter().find(|l| l.n == *n).cloned())
            .collect();
        out.extend(self.lines.iter().filter(|l| l.n > self.sent).cloned());
        let moved = front != self.sent_front;
        if out.is_empty() && !moved && redrawn.is_empty() {
            return None;
        }
        self.sent = self.last;
        self.sent_front = front;
        Some((front, out))
    }
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Reader {
    out: Option<OwnedFd>,
    err: Option<OwnedFd>,
}

/// At most this much is read from one pipe in one [`Reader::step`], so
/// a command printing without pause still lets its reader publish.
const STEP_READ: usize = 256 * 1024;

impl Reader {
    /// Reads `out` and `err` (either may be absent), each set
    /// non-blocking.
    pub fn new(out: Option<OwnedFd>, err: Option<OwnedFd>) -> Reader {
        for fd in out.iter().chain(err.iter()) {
            nonblocking(fd.as_fd());
        }
        Reader { out, err }
    }

    /// Whether `stream`'s pipe is still open (not at its end).
    pub fn is_open(&self, stream: Stream) -> bool {
        match stream {
            Stream::Out => self.out.is_some(),
            Stream::Err => self.err.is_some(),
        }
    }

    /// Both pipes are at their end.
    pub fn done(&self) -> bool {
        self.out.is_none() && self.err.is_none()
    }

    /// The pipes still open, to hand on (the keeper drains them).
    pub fn fds(&self) -> Vec<BorrowedFd<'_>> {
        self.out
            .iter()
            .chain(self.err.iter())
            .map(AsFd::as_fd)
            .collect()
    }

    /// Waits at most `timeout` (`None`: until something happens) for
    /// either pipe or one of `wakes`, then reads what the pipes hold:
    /// `f(stream, Some(bytes))` for what was read, `f(stream, None)` at a
    /// pipe's end. stdout is neither waited for nor read unless
    /// `read_out`. The result says which of `wakes` were ready.
    pub fn step(
        &mut self,
        wakes: &[BorrowedFd<'_>],
        timeout: Option<Duration>,
        read_out: bool,
        f: &mut dyn FnMut(Stream, Option<&[u8]>),
    ) -> std::io::Result<Vec<bool>> {
        use rustix::event::{PollFd, PollFlags, poll};
        let watch_out = read_out && self.out.is_some();
        let mut fds: Vec<PollFd<'_>> = Vec::with_capacity(2 + wakes.len());
        if watch_out && let Some(o) = &self.out {
            fds.push(PollFd::new(o, PollFlags::IN));
        }
        if let Some(e) = &self.err {
            fds.push(PollFd::new(e, PollFlags::IN));
        }
        let base = fds.len();
        for w in wakes {
            fds.push(PollFd::from_borrowed_fd(*w, PollFlags::IN));
        }
        let ts = timeout.map(|t| rustix::time::Timespec {
            tv_sec: i64::try_from(t.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(t.subsec_nanos()),
        });
        match poll(&mut fds, ts.as_ref()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
        let ready = |fd: &PollFd<'_>| !fd.revents().is_empty();
        let mut i = 0;
        let out_ready = watch_out && {
            i += 1;
            ready(&fds[0])
        };
        let err_ready = self.err.is_some() && ready(&fds[i]);
        let woke: Vec<bool> = fds[base..].iter().map(ready).collect();
        drop(fds);
        if out_ready {
            Self::read(&mut self.out, Stream::Out, f)?;
        }
        if err_ready {
            Self::read(&mut self.err, Stream::Err, f)?;
        }
        Ok(woke)
    }

    /// Reads what `pipe` holds now, at most [`STEP_READ`]; closes it at
    /// its end.
    fn read(
        pipe: &mut Option<OwnedFd>,
        stream: Stream,
        f: &mut dyn FnMut(Stream, Option<&[u8]>),
    ) -> std::io::Result<()> {
        let Some(fd) = pipe.as_ref() else {
            return Ok(());
        };
        let mut buf = [0u8; 16 * 1024];
        let mut got = 0;
        while got < STEP_READ {
            match rustix::io::read(fd, &mut buf) {
                Ok(0) => {
                    *pipe = None;
                    f(stream, None);
                    return Ok(());
                }
                Ok(n) => {
                    got += n;
                    f(stream, Some(&buf[..n]));
                }
                Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => {
                    // An unreadable pipe is at its end.
                    *pipe = None;
                    f(stream, None);
                    return Err(e.into());
                }
            }
        }
        Ok(())
    }

    /// Reads both pipes to their end or until `until`, whichever comes
    /// first, giving `f` what they hold: a run's last words after its
    /// program ended ([`STDERR_GRACE`]).
    pub fn drain_until(
        &mut self,
        until: std::time::Instant,
        f: &mut dyn FnMut(Stream, Option<&[u8]>),
    ) {
        loop {
            let now = std::time::Instant::now();
            if self.done() || now >= until {
                return;
            }
            if self.step(&[], Some(until - now), true, f).is_err() && self.done() {
                return;
            }
        }
    }
}

/// Sets `fd` non-blocking.
fn nonblocking(fd: BorrowedFd<'_>) {
    if let Ok(flags) = rustix::fs::fcntl_getfl(fd) {
        let _ = rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(chunks: &[&[u8]]) -> (Vec<String>, Option<String>) {
        let mut f = Framer::new();
        let mut lines = Vec::new();
        for c in chunks {
            f.push(c, &mut |l| lines.push(l.text()));
        }
        let open = f.open().map(decode);
        (lines, open)
    }

    fn lines(chunks: &[&[u8]]) -> Vec<String> {
        let mut f = Framer::new();
        let mut lines = Vec::new();
        for c in chunks {
            f.push(c, &mut |l| lines.push(l.text()));
        }
        f.finish(&mut |l| lines.push(l.text()));
        lines
    }

    /// `\n` and `\r\n` end a line, across the edges of what was read.
    #[test]
    fn line_ends_across_chunks() {
        assert_eq!(lines(&[b"a\nb\r", b"\nc"]), ["a", "b", "c"]);
        assert_eq!(lines(&[b"one\r\ntwo\r\n"]), ["one", "two"]);
        assert_eq!(lines(&[b"x\n\ny\n"]), ["x", "", "y"]);
        assert_eq!(lines(&[b"a\r\r\n"]), ["a"], "`\\r\\r\\n` ends it once");
    }

    /// A bare `\r` starts the line again: a bar redrawn is one line, its
    /// last drawing; a `\r` at a chunk's end waits for the next byte.
    #[test]
    fn a_bare_carriage_return_redraws_the_line() {
        let (done, open) = frame(&[b"10%\r20%\r", b"30%"]);
        assert!(done.is_empty());
        assert_eq!(open.as_deref(), Some("30%"));
        let (done, open) = frame(&[b"10%\r"]);
        assert!(done.is_empty());
        assert_eq!(open.as_deref(), Some("10%"), "shown until redrawn");
        assert_eq!(lines(&[b"\r1/3\r2/3\r3/3\n"]), ["3/3"]);
        assert_eq!(lines(&[b"long text\rshort\n"]), ["short"]);
        // git's hundred updates in one read.
        let mut many = Vec::new();
        for i in 0..100 {
            many.extend_from_slice(format!("Receiving objects: {i}%\r").as_bytes());
        }
        many.extend_from_slice(b"Receiving objects: 100%, done.\n");
        assert_eq!(lines(&[&many]), ["Receiving objects: 100%, done."]);
        assert_eq!(lines(&[b"drawn\r"]), ["drawn"], "ended by the output's end");
    }

    /// `\b` removes the character before it, a multibyte one whole.
    #[test]
    fn backspace_removes_a_character() {
        assert_eq!(lines(&[b"spin|\x08/\x08-\x08\\\x08done\n"]), ["spindone"]);
        assert_eq!(lines(&["caf\u{e9}\x08e\n".as_bytes()]), ["cafe"]);
        assert_eq!(lines(&[b"\x08\x08ok\n"]), ["ok"]);
    }

    /// CSI, OSC and two-byte escapes and other controls are removed, `\t`
    /// kept, also when split across reads.
    #[test]
    fn escapes_and_controls_are_removed() {
        assert_eq!(lines(&[b"\x1b[1;31merror:\x1b[0m bad\n"]), ["error: bad"]);
        assert_eq!(lines(&[b"\x1b]0;title\x07shown\n"]), ["shown"]);
        assert_eq!(
            lines(&[b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\n"]),
            ["link"]
        );
        assert_eq!(lines(&[b"\x1b(Bplain\x1b=\n"]), ["plain"]);
        assert_eq!(lines(&[b"a\tb\x00\x07\x7fc\n"]), ["a\tbc"]);
        assert_eq!(
            lines(&[b"\x1b[", b"32", b"mgreen\x1b", b"[0m\n"]),
            ["green"]
        );
        // pacman's redrawn bar: erase the line, then draw.
        assert_eq!(
            lines(&[b"(1/2) a  50%\r\x1b[K(1/2) a 100%\n"]),
            ["(1/2) a 100%"]
        );
    }

    /// An escape cut short never runs past its line: `\n` ends every
    /// escape (a string's, `ESC P`'s, an `ESC` alone) and then the line,
    /// and CAN or SUB abort one.
    #[test]
    fn an_escape_never_runs_past_its_line() {
        assert_eq!(
            lines(&[b"start\n\x1b]0;title never ended\nline two\nline three\n"]),
            ["start", "", "line two", "line three"]
        );
        assert_eq!(lines(&[b"a\n\x1bPq#0\nb\nc\n"]), ["a", "", "b", "c"]);
        for intro in [b"]", b"P", b"X", b"^", b"_"] {
            let mut bytes = b"a\x1b".to_vec();
            bytes.extend_from_slice(intro);
            bytes.extend_from_slice(b"junk\nb\n");
            assert_eq!(lines(&[&bytes]), ["a", "b"], "{intro:?}");
        }
        // An `ESC` inside a string, then the line's end.
        assert_eq!(lines(&[b"\x1b]0;t\x1b", b"\nb\n"]), ["", "b"]);
        // An `ESC` and an intermediate byte, then the line's end.
        assert_eq!(lines(&[b"x\x1b(\ny\n"]), ["x", "y"]);
        // An `ESC` right before `\n` keeps the line break.
        assert_eq!(lines(&[b"a\x1b\nb\n"]), ["a", "b"]);
        assert_eq!(lines(&[b"a\x1b", b"\nb\n"]), ["a", "b"]);
        // CAN and SUB abort an escape and are dropped.
        assert_eq!(lines(&[b"\x1b]0;t\x18shown\n"]), ["shown"]);
        assert_eq!(lines(&[b"\x1b[12\x1ashown\n"]), ["shown"]);
        // A whole output (a poll's) keeps its lines after one.
        assert_eq!(Framer::whole(b"\x1b]0;t\nv=1\n"), b"\nv=1");
    }

    /// wget prints a line in two parts seconds apart: one open line,
    /// ended whole at its `\n`, never at a pause.
    #[test]
    fn an_open_line_is_shown_then_ended_whole() {
        let mut f = Framer::new();
        let mut done = Vec::new();
        f.push(b"Connecting to example.com... ", &mut |l| {
            done.push(l.text())
        });
        assert!(done.is_empty());
        assert!(f.take_changed());
        assert_eq!(f.open(), Some(&b"Connecting to example.com... "[..]));
        assert!(!f.take_changed(), "nothing new");
        f.push(b"connected.\n", &mut |l| done.push(l.text()));
        assert_eq!(done, ["Connecting to example.com... connected."]);
        assert_eq!(f.open(), None);
        // A prompt is the open line for as long as it waits.
        let (done, open) = frame(&[b":: Proceed with installation? [Y/n] "]);
        assert!(done.is_empty());
        assert_eq!(
            open.as_deref(),
            Some(":: Proceed with installation? [Y/n] ")
        );
        // A partial line at the end ends with the output.
        assert_eq!(lines(&[b"a\npartial"]), ["a", "partial"]);
    }

    /// Bytes that are not UTF-8 are U+FFFD, decided per line.
    #[test]
    fn invalid_utf8_is_replaced_per_line() {
        assert_eq!(
            lines(&[b"ok\n\xff\xfebad\n", "\u{e9}".as_bytes(), b"\n"]),
            ["ok", "\u{fffd}\u{fffd}bad", "\u{e9}"]
        );
        // A character split across reads is whole.
        let e = "\u{e9}".as_bytes();
        assert_eq!(lines(&[&e[..1], &e[1..], b"\n"]), ["\u{e9}"]);
    }

    /// A line past 4 MiB is said to be over, kept short; a ring shows it
    /// cut to 2000 characters.
    #[test]
    fn a_long_line_is_over_and_cut_in_a_ring() {
        let mut f = Framer::new();
        let mut got = Vec::new();
        let big = vec![b'x'; MAX_DOCUMENT + 10];
        f.push(&big, &mut |l| got.push(l));
        f.push(b"\nnext\n", &mut |l| got.push(l));
        assert_eq!(got.len(), 2);
        assert!(got[0].over && got[0].bytes.len() <= OVER_KEPT);
        assert!(!got[1].over);
        let mut r = Ring::new();
        r.end(Stream::Out, &got[0].text());
        let l = r.lines().next().unwrap();
        assert_eq!(l.text.chars().count(), SHOWN_CHARS + 1);
        assert!(l.text.ends_with('…'));
    }

    /// A ring keeps its last 1000 lines, at most 256 KiB, and says what
    /// changed: new lines, an open line redrawn and ended, the front.
    #[test]
    fn the_ring_is_capped_and_publishes_changes() {
        let mut r = Ring::new();
        for i in 0..1500 {
            r.end(Stream::Out, &format!("line {i}"));
        }
        assert_eq!(r.lines().count(), RING_LINES);
        assert_eq!(r.lines().next().unwrap().n, 501);
        let (front, got) = r.take().unwrap();
        assert_eq!(front, 501);
        assert_eq!(got.len(), 1000);
        assert_eq!(r.take(), None);
        // Bytes bound it too.
        let mut r = Ring::new();
        let long = "y".repeat(SHOWN_CHARS);
        for _ in 0..200 {
            r.end(Stream::Err, &long);
        }
        let held: usize = r.lines().map(|l| l.text.len()).sum();
        assert!(held <= RING_BYTES, "{held}");
        assert!(r.lines().count() < 200);
        // The open line: added, redrawn, ended, each one change.
        let mut r = Ring::new();
        r.end(Stream::Out, "a");
        r.open(Stream::Err, Some("10%"));
        assert_eq!(
            r.take()
                .unwrap()
                .1
                .iter()
                .map(|l| (l.n, l.open))
                .collect::<Vec<_>>(),
            [(1, false), (2, true)]
        );
        r.open(Stream::Err, Some("50%"));
        r.end(Stream::Out, "b");
        let (_, got) = r.take().unwrap();
        assert_eq!(
            got.iter()
                .map(|l| (l.n, l.text.as_str(), l.open))
                .collect::<Vec<_>>(),
            [(2, "50%", true), (3, "b", false)]
        );
        r.end(Stream::Err, "100%");
        let (_, got) = r.take().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].n, got[0].open), (2, false));
        assert!(got[0].stderr);
    }

    /// The tail: stderr whole up to 4 KiB; past that its first 1 KiB and
    /// its last 3 KiB at line boundaries, joined by `…`; stdout's last
    /// 1 KiB when stderr said nothing; nothing at all is no reason.
    #[test]
    fn the_tail_keeps_head_and_end_at_lines() {
        let mut t = Tail::new();
        assert_eq!(t.reason(), None);
        assert_eq!(t.explain("`x` failed"), "`x` failed");
        t.push(Stream::Out, "echoed why");
        assert_eq!(t.reason().as_deref(), Some("echoed why"));
        t.push(Stream::Err, "error: failed to commit transaction");
        t.push(Stream::Err, "");
        t.push(Stream::Err, "foo: /usr/bin/foo exists in filesystem");
        assert_eq!(
            t.explain("`pacman` failed (exit status: 1)"),
            "`pacman` failed (exit status: 1): error: failed to commit transaction\nfoo: /usr/bin/foo exists in filesystem"
        );
        let mut t = Tail::new();
        t.push(
            Stream::Err,
            "error: failed to commit transaction (conflicting files)",
        );
        for i in 0..200 {
            t.push(
                Stream::Err,
                &format!("pkg{i}: /usr/lib/file{i} exists in filesystem"),
            );
        }
        t.push(Stream::Err, "Errors occurred, no packages were upgraded.");
        let why = t.reason().unwrap();
        assert!(why.len() <= TAIL_HEAD + TAIL_END + 8, "{}", why.len());
        let lines: Vec<&str> = why.lines().collect();
        assert_eq!(
            lines[0],
            "error: failed to commit transaction (conflicting files)"
        );
        assert!(lines.contains(&"…"));
        assert_eq!(
            lines.last(),
            Some(&"Errors occurred, no packages were upgraded.")
        );
        assert!(
            lines.iter().all(|l| *l == "…"
                || l.ends_with("filesystem")
                || l.starts_with("error")
                || l.starts_with("Errors")),
            "whole lines: {why}"
        );
        // stdout falls back only when stderr is empty, its last 1 KiB.
        let mut t = Tail::new();
        for i in 0..500 {
            t.push(Stream::Out, &format!("out {i}"));
        }
        let why = t.reason().unwrap();
        assert!(
            why.len() <= TAIL_STDOUT && why.ends_with("out 499"),
            "{why}"
        );
        // One huge stderr line: its start, then its end.
        let mut t = Tail::new();
        t.push(Stream::Err, &"h".repeat(10_000));
        t.push(Stream::Err, "last");
        let why = t.reason().unwrap();
        assert!(
            why.starts_with("hhh") && why.ends_with("…\nlast"),
            "{}",
            why.len()
        );
    }

    /// A whole output framed: escapes gone, the redrawn bar its last
    /// drawing, the lines joined.
    #[test]
    fn a_whole_output_is_framed() {
        assert_eq!(
            Framer::whole(b"\x1b[32m{\"a\":\r\n 1}\x1b[0m\n"),
            b"{\"a\":\n 1}".to_vec()
        );
        assert_eq!(Framer::whole(b"10%\r100%"), b"100%".to_vec());
    }

    /// The reader reads both pipes, holds stdout back when asked while
    /// stderr drains, and says each pipe's end.
    #[test]
    fn the_reader_reads_both_streams() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sh")
            .args(["-c", "echo out; echo err >&2; sleep .2; echo more"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().map(OwnedFd::from);
        let err = child.stderr.take().map(OwnedFd::from);
        let mut r = Reader::new(out, err);
        let mut got: Vec<(Stream, Option<Vec<u8>>)> = Vec::new();
        // stdout held back: only stderr is read.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !got.iter().any(|(s, b)| *s == Stream::Err && b.is_none()) {
            assert!(std::time::Instant::now() < deadline);
            r.step(&[], Some(Duration::from_millis(50)), false, &mut |s, b| {
                got.push((s, b.map(<[u8]>::to_vec)))
            })
            .unwrap();
        }
        assert!(got.iter().all(|(s, _)| *s == Stream::Err));
        assert!(r.is_open(Stream::Out));
        r.drain_until(deadline, &mut |s, b| got.push((s, b.map(<[u8]>::to_vec))));
        assert!(r.done());
        let out: Vec<u8> = got
            .iter()
            .filter(|(s, _)| *s == Stream::Out)
            .filter_map(|(_, b)| b.clone())
            .flatten()
            .collect();
        assert_eq!(out, b"out\nmore\n");
        let _ = child.wait();
    }
}
