//! Byte offsets ↔ LSP positions (UTF-16 code units, the protocol's
//! default), and `file://` URIs ↔ paths.

use std::path::{Path, PathBuf};

use lsp_types::{Position, Range};
use strand_compiler::syntax::Span;

/// Line starts of one text, for position conversion.
#[derive(Clone, Debug)]
pub struct Lines {
    starts: Vec<u32>,
}

impl Lines {
    pub fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut starts = vec![0];
        for (i, &b) in bytes.iter().enumerate() {
            // `\n`, or a lone `\r`, as the compiler reads line breaks.
            if b == b'\n' || b == b'\r' && bytes.get(i + 1) != Some(&b'\n') {
                starts.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
            }
        }
        Self { starts }
    }

    /// The position of byte `offset`.
    pub fn position(&self, text: &str, offset: u32) -> Position {
        let offset = offset.min(text.len() as u32);
        let line = self.starts.partition_point(|&s| s <= offset).max(1) - 1;
        let start = self.starts[line] as usize;
        let mut end = offset as usize;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        let character = text[start..end].encode_utf16().count() as u32;
        Position::new(line as u32, character)
    }

    /// The byte offset of `pos`, clamped to its line (a position past the
    /// end of a line is its end, before the line break).
    pub fn offset(&self, text: &str, pos: Position) -> u32 {
        let Some(&start) = self.starts.get(pos.line as usize) else {
            return text.len() as u32;
        };
        let end = self
            .starts
            .get(pos.line as usize + 1)
            .map_or(text.len(), |&e| e as usize);
        let line = &text[start as usize..end];
        let line = line.trim_end_matches(['\n', '\r']);
        let mut units = 0u32;
        for (i, c) in line.char_indices() {
            if units >= pos.character {
                return start + i as u32;
            }
            units += c.len_utf16() as u32;
        }
        start + line.len() as u32
    }

    pub fn range(&self, text: &str, span: Span) -> Range {
        Range::new(
            self.position(text, span.start),
            self.position(text, span.end),
        )
    }
}

/// The path of a `file://` URI.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://localhost/x` and `file:///x`.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(h) = rest.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(h, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(out).ok()?;
    Some(PathBuf::from(s))
}

/// The `file://` URI of an absolute path.
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_count_utf16_units() {
        let text = "ab\n‹𝄞x\r\ny";
        let l = Lines::new(text);
        assert_eq!(l.position(text, 0), Position::new(0, 0));
        assert_eq!(l.position(text, 3), Position::new(1, 0));
        // `‹` is one unit, `𝄞` two.
        let x = text.find('x').unwrap() as u32;
        assert_eq!(l.position(text, x), Position::new(1, 3));
        assert_eq!(l.offset(text, Position::new(1, 3)), x);
        let y = text.find('y').unwrap() as u32;
        assert_eq!(l.position(text, y), Position::new(2, 0));
        // Past the end of a line: its end.
        assert_eq!(l.offset(text, Position::new(0, 99)), 2);
        assert_eq!(l.offset(text, Position::new(9, 0)), text.len() as u32);
    }

    #[test]
    fn file_uris_round_trip() {
        let p = Path::new("/home/me/.config/strand/my bar ‹.strand");
        let uri = path_to_uri(p);
        assert_eq!(
            uri,
            "file:///home/me/.config/strand/my%20bar%20%E2%80%B9.strand"
        );
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path("untitled:1"), None);
        assert_eq!(
            uri_to_path("file://localhost/a/b.strand").unwrap(),
            Path::new("/a/b.strand")
        );
    }
}
