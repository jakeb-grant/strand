//! Strand code as HTML, coloured by the editor grammar's own highlight
//! queries (`tree-sitter-strand/queries/highlights.scm`), so the site and
//! every tree-sitter editor colour a config alike.

use tree_sitter::{Language, Parser, Query, QueryCursor, StreamingIterator};

use crate::html::esc;

/// The grammar and its highlight query, compiled once.
pub struct Highlighter {
    language: Language,
    query: Query,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

impl Highlighter {
    pub fn new() -> Result<Highlighter, String> {
        let language: Language = tree_sitter_strand::LANGUAGE.into();
        let query = Query::new(&language, tree_sitter_strand::HIGHLIGHTS_QUERY)
            .map_err(|e| format!("highlights.scm does not compile: {e}"))?;
        Ok(Highlighter { language, query })
    }

    /// `src` as escaped HTML, each highlighted run in a
    /// `<span class="hl-…">` named by its capture (`@keyword.conditional`
    /// is `hl-keyword-conditional`). As tree-sitter-highlight does, a
    /// node takes its first pattern's capture, and a node inside another
    /// is coloured by its own.
    pub fn html(&self, src: &str) -> String {
        let mut parser = Parser::new();
        if parser.set_language(&self.language).is_err() {
            return esc(src);
        }
        let Some(tree) = parser.parse(src, None) else {
            return esc(src);
        };
        let names = self.query.capture_names();
        let mut spans: Vec<(usize, usize, usize, &str)> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut captures = cursor.captures(&self.query, tree.root_node(), src.as_bytes());
        while let Some((m, i)) = captures.next() {
            let c = m.captures()[*i];
            let name = names[c.index as usize];
            let r = c.node.byte_range();
            if name == "spell" || r.is_empty() {
                continue;
            }
            spans.push((r.start, r.end, m.pattern_index, name));
        }
        // Outer ranges first, so an inner one paints over them; for one
        // range, the later pattern first, so the first pattern paints last.
        spans.sort_by(|a, b| {
            (b.1 - b.0)
                .cmp(&(a.1 - a.0))
                .then(a.0.cmp(&b.0))
                .then(b.2.cmp(&a.2))
        });
        let mut class: Vec<Option<&str>> = vec![None; src.len()];
        for &(s, e, _, name) in &spans {
            for c in &mut class[s..e] {
                *c = Some(name);
            }
        }
        let mut out = String::with_capacity(src.len() * 2);
        let mut run = String::new();
        let mut current: Option<&str> = None;
        let flush = |out: &mut String, run: &mut String, current: Option<&str>| {
            if run.is_empty() {
                return;
            }
            match current {
                Some(name) => {
                    out.push_str("<span class=\"hl-");
                    out.push_str(&name.replace('.', "-"));
                    out.push_str("\">");
                    out.push_str(&esc(run));
                    out.push_str("</span>");
                }
                None => out.push_str(&esc(run)),
            }
            run.clear();
        };
        for (at, ch) in src.char_indices() {
            // A line break ends a span, so no span crosses lines.
            let here = if ch == '\n' { None } else { class[at] };
            if here != current {
                flush(&mut out, &mut run, current);
                current = here;
            }
            run.push(ch);
        }
        flush(&mut out, &mut run, current);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_strings_and_types_are_coloured() {
        let h = Highlighter::new().unwrap();
        let html = h.html("bar Top {\n  text \"hi\" { color: $fg }\n}\n");
        assert!(
            html.contains("<span class=\"hl-type\">Top</span>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"hl-string\">&quot;hi&quot;</span>"),
            "{html}"
        );
        assert!(html.contains("hl-keyword"), "{html}");
    }

    #[test]
    fn text_is_kept_exactly() {
        let h = Highlighter::new().unwrap();
        let src = "state on = false // a <b> & c\nbar B { height: 30 }\n";
        let html = h.html(src);
        let mut plain = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => plain.push(c),
                _ => {}
            }
        }
        let plain = plain
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&");
        assert_eq!(plain, src);
    }
}
