//! The fuzzy matcher `apps.search` and `xs.fuzzy` share: nucleo's, wrapped
//! (design.md: "wrap it, fork if needed"), so both rank and mark alike.
//!
//! Ranges count characters, as `Range` and the renderer's `marks:` do,
//! though nucleo matches grapheme clusters.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Consecutive character indices as `[start, end)` ranges.
fn ranges_of(mut indices: Vec<u32>) -> Vec<Range<u32>> {
    indices.sort_unstable();
    indices.dedup();
    let mut out: Vec<Range<u32>> = Vec::new();
    for i in indices {
        match out.last_mut() {
            Some(r) if r.end == i => r.end = i + 1,
            _ => out.push(i..i + 1),
        }
    }
    out
}

/// A matcher for one query. It holds nucleo's scratch memory, so make one
/// per search, not one per text.
pub struct Fuzzy {
    pattern: Pattern,
    matcher: Matcher,
    buf: Vec<char>,
    /// The character offset where each grapheme of the text in `buf`
    /// starts, and the text's length in characters last.
    starts: Vec<u32>,
}

impl std::fmt::Debug for Fuzzy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fuzzy")
    }
}

impl Fuzzy {
    /// A matcher for `query` (smart case: lower case matches either
    /// case; accents ignored unless the query has them; words separated
    /// by spaces must all match). A blank query matches every text with
    /// score 0 and no ranges.
    pub fn new(query: &str) -> Fuzzy {
        Fuzzy {
            pattern: Pattern::parse(query, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
            buf: Vec::new(),
            starts: Vec::new(),
        }
    }

    /// `text` as nucleo matches it: ASCII as is, anything else one
    /// character per grapheme cluster (its first, as nucleo's own
    /// `Utf32Str::new` does; but never its ASCII shortcut over the bytes
    /// of a text whose clusters all start in ASCII, `e` + U+0301 say,
    /// which would match the combining mark's bytes as characters).
    /// `starts` maps the clusters back to characters.
    fn hay<'a>(text: &'a str, buf: &'a mut Vec<char>, starts: &mut Vec<u32>) -> Utf32Str<'a> {
        use unicode_segmentation::UnicodeSegmentation;
        starts.clear();
        if text.is_ascii() {
            return Utf32Str::Ascii(text.as_bytes());
        }
        buf.clear();
        let mut at = 0u32;
        for g in text.graphemes(true) {
            if let Some(c) = g.chars().next() {
                buf.push(c);
                starts.push(at);
            }
            at = at.saturating_add(u32::try_from(g.chars().count()).unwrap_or(u32::MAX));
        }
        starts.push(at);
        Utf32Str::Unicode(buf)
    }

    /// `text`'s score and the matched characters as ranges (character
    /// indices: a matched grapheme cluster covers all its characters);
    /// `None` when it does not match.
    pub fn matches(&mut self, text: &str) -> Option<(u32, Vec<Range<u32>>)> {
        let hay = Self::hay(text, &mut self.buf, &mut self.starts);
        let mut indices = Vec::new();
        let score = self.pattern.indices(hay, &mut self.matcher, &mut indices)?;
        if !self.starts.is_empty() {
            let starts = &self.starts;
            indices = indices
                .iter()
                .filter_map(|&g| {
                    let (a, b) = (starts.get(g as usize)?, starts.get(g as usize + 1)?);
                    Some(*a..*b)
                })
                .flatten()
                .collect();
        }
        Some((score, ranges_of(indices)))
    }

    /// `text`'s score, without ranges.
    pub fn score(&mut self, text: &str) -> Option<u32> {
        let hay = Self::hay(text, &mut self.buf, &mut self.starts);
        self.pattern.score(hay, &mut self.matcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ranges count characters, as the schema's `Range` and the
    /// renderer's marks do, though nucleo matches grapheme clusters: a
    /// matched cluster covers all its characters, and the characters
    /// after a combining mark keep their places.
    #[test]
    fn match_ranges_are_characters_not_clusters() {
        // `e` + U+0301: the clusters all start in ASCII.
        let (_, ranges) = Fuzzy::new("fil").matches("Cafe\u{301} Files").unwrap();
        assert_eq!(ranges, vec![6..9]);
        let (_, ranges) = Fuzzy::new("cafe").matches("Cafe\u{301} Files").unwrap();
        assert_eq!(ranges, vec![0..5], "the mark goes with its letter");
        // Devanagari: फ़ा इ लें, clusters of three, one and three characters.
        let (_, ranges) = Fuzzy::new("\u{932}")
            .matches("\u{92b}\u{93c}\u{93e}\u{907}\u{932}\u{947}\u{902}")
            .unwrap();
        assert_eq!(ranges, vec![4..7]);
        // A cluster-free text is unchanged.
        let (_, ranges) = Fuzzy::new("efo").matches("Firefox").unwrap();
        assert_eq!(ranges, vec![3..6]);
        let (_, ranges) = Fuzzy::new("öl").matches("Größe öl").unwrap();
        assert_eq!(ranges, vec![6..8]);
    }

    #[test]
    fn a_blank_query_matches_everything_with_no_ranges() {
        for q in ["", "   "] {
            let mut f = Fuzzy::new(q);
            assert_eq!(f.matches("Firefox"), Some((0, Vec::new())));
            assert_eq!(f.matches(""), Some((0, Vec::new())));
            assert_eq!(f.matches("Größe"), Some((0, Vec::new())));
        }
    }

    #[test]
    fn every_word_of_a_query_must_match() {
        let mut f = Fuzzy::new("fi ox");
        assert!(f.matches("Firefox").is_some());
        assert!(f.matches("Files").is_none());
    }
}
