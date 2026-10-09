//! The wire protocol between [`Client`](crate::Client) and the helper,
//! over a socketpair (the helper's stdin and stdout).
//!
//! Every message is one frame: a little-endian `u32` length, then that
//! many bytes, of which the first is the kind and the rest the payload.
//! A frame's payload is at most [`MAX_PAYLOAD`] bytes; a longer length,
//! a zero length, an unknown kind or a payload that does not parse is a
//! [`ProtocolError`], and a client that sees one kills the helper (it
//! fails closed: no unlock).
//!
//! | Kind | Direction | Payload |
//! |---|---|---|
//! | `HELLO` (1) | helper → client, once at start | `[VERSION, service]` (0 `strand`, 1 `login`) |
//! | `SUBMIT` (2) | client → helper | the password's bytes |
//! | `VERDICT` (3) | helper → client, one per `SUBMIT` | `[code]` (0 success, 1 denied, 2 error), then a UTF-8 message |
//!
//! Frames that carry a password are built and read into buffers that are
//! wiped when dropped, sized once, so no reallocation leaves a copy
//! behind (`tests/zeroize.rs`).

use std::io::{self, Read, Write};

use zeroize::Zeroizing;

use crate::Password;

/// The protocol version a `HELLO` carries.
pub const VERSION: u8 = 1;
/// The longest payload of a frame: a password (PAM itself takes at most
/// 512 bytes of one) or a verdict's message.
pub const MAX_PAYLOAD: usize = 1024;
/// The frame header: the length (4 bytes) and the kind.
pub const HEADER: usize = 5;

pub const HELLO: u8 = 1;
pub const SUBMIT: u8 = 2;
pub const VERDICT: u8 = 3;

/// The PAM service the helper authenticates with.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    /// `strand` (`/etc/pam.d/strand`).
    Strand,
    /// `login`, because `/etc/pam.d/strand` is missing (decisions.md,
    /// m4-owner): the client warns once.
    Login,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::Strand => "strand",
            Service::Login => "login",
        }
    }
}

/// What the helper made of a password.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Code {
    /// `pam_authenticate` and `pam_acct_mgmt` both returned `PAM_SUCCESS`.
    Success,
    /// PAM refused it: a wrong password, a locked or expired account.
    Denied,
    /// Anything else (a broken PAM stack, a module error, no such user):
    /// not an unlock.
    Error,
}

/// One frame's content.
#[derive(Debug)]
pub enum Message {
    Hello { version: u8, service: Service },
    Submit(Password),
    Verdict { code: Code, message: String },
}

/// Why a frame could not be read.
#[derive(Debug)]
pub enum ProtocolError {
    /// The stream ended between frames.
    Eof,
    /// The stream ended inside a frame.
    Truncated,
    /// A zero length.
    Empty,
    /// A length past [`MAX_PAYLOAD`] (plus the kind byte).
    TooLong(u32),
    UnknownKind(u8),
    /// The payload does not fit its kind.
    BadPayload(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "the stream ended"),
            Self::Truncated => write!(f, "the stream ended inside a frame"),
            Self::Empty => write!(f, "an empty frame"),
            Self::TooLong(n) => write!(f, "a {n}-byte frame (at most {})", MAX_PAYLOAD + 1),
            Self::UnknownKind(k) => write!(f, "an unknown frame kind {k}"),
            Self::BadPayload(why) => write!(f, "a malformed frame: {why}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// The frame of `message`, in a buffer wiped when dropped.
pub fn encode(message: &Message) -> Zeroizing<Vec<u8>> {
    let (kind, head, body): (u8, Vec<u8>, &[u8]) = match message {
        Message::Hello { version, service } => (
            HELLO,
            vec![
                *version,
                match service {
                    Service::Strand => 0,
                    Service::Login => 1,
                },
            ],
            &[],
        ),
        Message::Submit(p) => (SUBMIT, Vec::new(), p.as_bytes()),
        Message::Verdict { code, message } => (
            VERDICT,
            vec![match code {
                Code::Success => 0,
                Code::Denied => 1,
                Code::Error => 2,
            }],
            truncate(message, MAX_PAYLOAD - 1).as_bytes(),
        ),
    };
    let body = &body[..body.len().min(MAX_PAYLOAD - head.len())];
    let len = 1 + head.len() + body.len();
    // Sized once: extending within capacity never reallocates, so the
    // password is copied exactly once, into this buffer.
    let mut out = Zeroizing::new(Vec::with_capacity(4 + len));
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&head);
    out.extend_from_slice(body);
    out
}

/// The longest prefix of `s` of at most `max` bytes that ends on a
/// character boundary.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A frame header's kind and payload length.
pub fn header(bytes: [u8; HEADER]) -> Result<(u8, usize), ProtocolError> {
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if len == 0 {
        return Err(ProtocolError::Empty);
    }
    if len as usize > MAX_PAYLOAD + 1 {
        return Err(ProtocolError::TooLong(len));
    }
    Ok((bytes[4], len as usize - 1))
}

/// A frame's message from its kind and payload. A `SUBMIT`'s payload
/// becomes the [`Password`] itself (no copy).
pub fn decode(kind: u8, mut payload: Zeroizing<Vec<u8>>) -> Result<Message, ProtocolError> {
    match kind {
        HELLO => match payload.as_slice() {
            [version, service] => Ok(Message::Hello {
                version: *version,
                service: match service {
                    0 => Service::Strand,
                    1 => Service::Login,
                    _ => return Err(ProtocolError::BadPayload("an unknown PAM service")),
                },
            }),
            _ => Err(ProtocolError::BadPayload("a hello is two bytes")),
        },
        SUBMIT => Ok(Message::Submit(Password::from_bytes(std::mem::take(
            &mut *payload,
        )))),
        VERDICT => {
            let Some((&code, text)) = payload.split_first() else {
                return Err(ProtocolError::BadPayload("a verdict has a code"));
            };
            let code = match code {
                0 => Code::Success,
                1 => Code::Denied,
                2 => Code::Error,
                _ => return Err(ProtocolError::BadPayload("an unknown verdict code")),
            };
            let message = std::str::from_utf8(text)
                .map_err(|_| ProtocolError::BadPayload("a verdict's message is not UTF-8"))?
                .to_string();
            Ok(Message::Verdict { code, message })
        }
        k => Err(ProtocolError::UnknownKind(k)),
    }
}

/// Reads one frame from a blocking reader (the helper; tests).
pub fn read_message(r: &mut impl Read) -> Result<Message, ProtocolError> {
    let mut head = [0u8; HEADER];
    let mut got = 0;
    while got < HEADER {
        match r.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Err(ProtocolError::Eof),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(ProtocolError::Io(e)),
        }
    }
    let (kind, len) = header(head)?;
    let mut payload = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut payload).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ProtocolError::Truncated,
        _ => ProtocolError::Io(e),
    })?;
    decode(kind, payload)
}

/// Writes one frame to a blocking writer and flushes it.
pub fn write_message(w: &mut impl Write, message: &Message) -> io::Result<()> {
    let frame = encode(message);
    w.write_all(&frame)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_messages_are_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_PAYLOAD);
        let frame = encode(&Message::Verdict {
            code: Code::Denied,
            message: long,
        });
        let m = read_message(&mut frame.as_slice()).unwrap();
        let Message::Verdict { message, .. } = m else {
            panic!("{m:?}");
        };
        assert!(message.len() < MAX_PAYLOAD);
        assert!(message.chars().all(|c| c == 'é'));
    }
}
