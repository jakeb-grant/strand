//! Bounded reads from compositor sockets.
//!
//! The compositor is trusted, but a peer that never ends a line (or a
//! frame header that names a huge length) must not grow a buffer without
//! limit: anything over [`MAX_MESSAGE`] is an I/O error, which reconnects
//! with backoff like any other lost socket.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// The largest line, reply or frame accepted (a whole `get_tree` of a big
/// session is well under 1 MiB).
pub(crate) const MAX_MESSAGE: usize = 16 << 20;

pub(crate) fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("a message over {MAX_MESSAGE} bytes"),
    )
}

/// The next `\n`-ended line as raw bytes (without the `\n`); `None` at the
/// end. Cancel safe: a partial line stays in `buf` for the next call.
pub(crate) async fn next_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Ok(Some(std::mem::take(buf)));
        }
        if let Some(i) = avail.iter().position(|b| *b == b'\n') {
            buf.extend_from_slice(&avail[..i]);
            reader.consume(i + 1);
            return Ok(Some(std::mem::take(buf)));
        }
        let n = avail.len();
        buf.extend_from_slice(avail);
        reader.consume(n);
        if buf.len() > MAX_MESSAGE {
            buf.clear();
            return Err(too_long());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn lines_are_split_and_capped() {
        let raw: &[u8] = b"a>>1\n\nb\xe9\npartial";
        let mut r = BufReader::new(raw);
        let mut buf = Vec::new();
        assert_eq!(
            next_line(&mut r, &mut buf).await.unwrap().as_deref(),
            Some(&b"a>>1"[..])
        );
        assert_eq!(
            next_line(&mut r, &mut buf).await.unwrap().as_deref(),
            Some(&b""[..])
        );
        assert_eq!(
            next_line(&mut r, &mut buf).await.unwrap().as_deref(),
            Some(&b"b\xe9"[..])
        );
        assert_eq!(
            next_line(&mut r, &mut buf).await.unwrap().as_deref(),
            Some(&b"partial"[..])
        );
        assert_eq!(next_line(&mut r, &mut buf).await.unwrap(), None);

        // A peer that never ends its line: an error past the cap, not an
        // ever-growing buffer.
        let (mut w, r) = tokio::io::duplex(1 << 16);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 1 << 16];
            while w.write_all(&chunk).await.is_ok() {}
        });
        let mut r = BufReader::new(r);
        let mut buf = Vec::new();
        let e = next_line(&mut r, &mut buf).await.unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        drop(r);
        writer.await.unwrap();
    }
}
