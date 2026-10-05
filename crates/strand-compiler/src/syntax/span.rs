//! Byte spans and line/column lookup.

use std::ops::Range;

/// A half-open byte range `start..end` into one source file.
///
/// Every token and every AST node carries one. Spans are what reload
/// identity and the LSP map back to text, so they are exact byte offsets,
/// never character counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// An empty span at `offset`.
    pub const fn at(offset: u32) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub const fn is_empty(self) -> bool {
        self.end <= self.start
    }

    /// The smallest span covering both.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// True if `other` lies within `self`.
    pub fn contains(self, other: Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// The text this span covers, or `""` if it is out of bounds or splits
    /// a character.
    pub fn text(self, src: &str) -> &str {
        src.get(self.range()).unwrap_or("")
    }
}

/// Maps byte offsets to 1-based line and column numbers.
#[derive(Clone, Debug)]
pub struct LineIndex {
    line_starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(src: &str) -> Self {
        let mut line_starts = vec![0];
        let bytes = src.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            // `\n`, or a lone `\r` (as the lexer reads it).
            if b == b'\n' || b == b'\r' && bytes.get(i + 1) != Some(&b'\n') {
                line_starts.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
            }
        }
        Self { line_starts }
    }

    /// 1-based line and column of `offset`; the column counts characters.
    pub fn line_col(&self, src: &str, offset: u32) -> (u32, u32) {
        let line = self.line_starts.partition_point(|&s| s <= offset).max(1) - 1;
        let start = self.line_starts[line] as usize;
        let end = (offset as usize).min(src.len());
        let col = src
            .get(start..end)
            .map_or(end.saturating_sub(start), |s| s.chars().count());
        (line as u32 + 1, col as u32 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_chars() {
        let src = "ab\n‹x\n";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(src, 0), (1, 1));
        assert_eq!(idx.line_col(src, 3), (2, 1));
        assert_eq!(idx.line_col(src, 6), (2, 2)); // after the 3-byte ‹
        assert_eq!(idx.line_col(src, 8), (3, 1));
        // A lone `\r` ends a line; `\r\n` is one line break.
        let src = "a\rb\r\nc";
        let idx = LineIndex::new(src);
        assert_eq!(idx.line_col(src, 2), (2, 1));
        assert_eq!(idx.line_col(src, 5), (3, 1));
    }

    #[test]
    fn spans_cover_and_contain() {
        let a = Span::new(2, 5);
        let b = Span::new(4, 9);
        assert_eq!(a.to(b), Span::new(2, 9));
        assert!(a.to(b).contains(a));
        assert!(!a.contains(b));
    }
}
