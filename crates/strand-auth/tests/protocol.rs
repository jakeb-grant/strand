//! The wire protocol: every message round-trips, and garbage is an
//! error, never a verdict.

use strand_auth::Password;
use strand_auth::protocol::{
    Code, HELLO, MAX_PAYLOAD, Message, ProtocolError, SUBMIT, Service, VERDICT, VERSION, encode,
    read_message, write_message,
};

fn round_trip(m: &Message) -> Message {
    let frame = encode(m);
    let mut r = frame.as_slice();
    let back = read_message(&mut r).unwrap();
    assert!(r.is_empty(), "one frame, all read");
    back
}

#[test]
fn every_message_round_trips() {
    for service in [Service::Strand, Service::Login] {
        let m = round_trip(&Message::Hello {
            version: VERSION,
            service,
        });
        assert!(
            matches!(m, Message::Hello { version: VERSION, service: s } if s == service),
            "{m:?}"
        );
    }
    for pw in ["", "hunter2", "pässwörd ✓", &"x".repeat(MAX_PAYLOAD)] {
        let m = round_trip(&Message::Submit(Password::from(pw.to_string())));
        let Message::Submit(p) = m else {
            panic!("{m:?}");
        };
        assert_eq!(p.as_bytes(), pw.as_bytes());
    }
    for (code, message) in [
        (Code::Success, ""),
        (Code::Denied, "Authentication failure"),
        (Code::Error, "Module is unknown\nsecond line"),
    ] {
        let m = round_trip(&Message::Verdict {
            code,
            message: message.to_string(),
        });
        let Message::Verdict {
            code: c,
            message: t,
        } = m
        else {
            panic!("{m:?}");
        };
        assert_eq!((c, t.as_str()), (code, message));
    }
}

#[test]
fn several_frames_read_in_order_from_one_stream() {
    let mut stream = Vec::new();
    write_message(
        &mut stream,
        &Message::Hello {
            version: VERSION,
            service: Service::Strand,
        },
    )
    .unwrap();
    write_message(
        &mut stream,
        &Message::Verdict {
            code: Code::Denied,
            message: "no".into(),
        },
    )
    .unwrap();
    let mut r = stream.as_slice();
    assert!(matches!(
        read_message(&mut r).unwrap(),
        Message::Hello { .. }
    ));
    assert!(matches!(
        read_message(&mut r).unwrap(),
        Message::Verdict {
            code: Code::Denied,
            ..
        }
    ));
    assert!(matches!(read_message(&mut r), Err(ProtocolError::Eof)));
}

/// A frame from its raw parts: length (kind + payload), kind, payload.
fn raw(len: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = len.to_le_bytes().to_vec();
    v.push(kind);
    v.extend_from_slice(payload);
    v
}

fn read(bytes: &[u8]) -> Result<Message, ProtocolError> {
    read_message(&mut &bytes[..])
}

#[test]
fn garbage_is_an_error_never_a_verdict() {
    let err = |bytes: &[u8]| match read(bytes) {
        Ok(m) => panic!("{bytes:?} read as {m:?}"),
        Err(e) => e,
    };
    assert!(matches!(err(&[]), ProtocolError::Eof));
    assert!(matches!(err(&[3, 0]), ProtocolError::Truncated));
    assert!(matches!(err(&raw(0, VERDICT, &[])), ProtocolError::Empty));
    assert!(matches!(
        err(&raw(MAX_PAYLOAD as u32 + 2, VERDICT, &[])),
        ProtocolError::TooLong(_)
    ));
    assert!(matches!(
        err(&raw(u32::MAX, VERDICT, &[])),
        ProtocolError::TooLong(_)
    ));
    assert!(matches!(
        err(&raw(1, 0x7f, &[])),
        ProtocolError::UnknownKind(0x7f)
    ));
    assert!(matches!(
        err(&raw(5, VERDICT, &[0])),
        ProtocolError::Truncated
    ));
    // Payloads that do not fit their kind.
    for bad in [
        raw(1, VERDICT, &[]),
        raw(2, VERDICT, &[9]),
        raw(3, VERDICT, &[0, 0xff]),
        raw(2, HELLO, &[1]),
        raw(3, HELLO, &[1, 7]),
        raw(4, HELLO, &[1, 0, 0]),
    ] {
        assert!(
            matches!(err(&bad), ProtocolError::BadPayload(_)),
            "{bad:?}: {:?}",
            read(&bad)
        );
    }
    // Text where a frame should be: "helo" is a 1.8 GB length.
    assert!(matches!(err(b"hello, world"), ProtocolError::TooLong(_)));
}

#[test]
fn an_empty_submit_is_an_empty_password() {
    let m = read(&raw(1, SUBMIT, &[])).unwrap();
    let Message::Submit(p) = m else {
        panic!("{m:?}");
    };
    assert!(p.is_empty());
}
