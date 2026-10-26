//! The reference's element pages: one page per element and prop group,
//! each its section of `docs/reference/elements.md` as strand-dev's
//! generator writes it from the schema, so the site and the committed
//! reference say the same thing, and an index of them all.

use std::collections::HashMap;
use std::fmt::Write as _;

use strand_compiler::schema::{DocKey, Schema};

use crate::html::esc;

/// Where the element pages are written.
pub const DIR: &str = "reference/elements";
/// What their links resolve against: the page they come from.
pub const SOURCE: &str = "docs/reference/elements.md";

/// One element's page, as markdown.
#[derive(Debug)]
pub struct ElementPage {
    pub name: String,
    pub group: bool,
    pub out: String,
    pub markdown: String,
}

/// Every element's and group's page.
pub fn pages(schema: &Schema) -> Vec<ElementPage> {
    strand_dev::reference::element_names(schema)
        .into_iter()
        .filter_map(|(name, group)| {
            let section = strand_dev::reference::element(schema, name)?;
            // Its `### `name`` heading becomes the page's title, and the
            // signature block says it is one.
            let body = section
                .split_once('\n')
                .map_or("", |(_, rest)| rest)
                .replacen("```strand\n", "```strand signature\n", 1);
            Some(ElementPage {
                name: name.to_string(),
                group,
                out: format!("{DIR}/{name}.html"),
                markdown: format!("# `{name}`\n{body}"),
            })
        })
        .collect()
}

/// `#anchor`s of `elements.md` that name an element's or group's page.
pub fn anchor_pages(schema: &Schema) -> HashMap<String, String> {
    strand_dev::reference::element_names(schema)
        .into_iter()
        .map(|(name, _)| {
            (
                strand_dev::reference::anchor_of(name),
                format!("{DIR}/{name}.html"),
            )
        })
        .collect()
}

/// A doc's first sentence, for the index.
fn first_sentence(doc: &str) -> String {
    let doc = doc.split("\n\n").next().unwrap_or("").replace('\n', " ");
    let mut end = doc.len();
    let mut ticks = 0;
    let b = doc.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'`' {
            ticks += 1;
        }
        if c == b'.' && ticks % 2 == 0 && b.get(i + 1).is_none_or(|n| *n == b' ') {
            end = i + 1;
            break;
        }
    }
    doc[..end].trim().to_string()
}

/// The index page's markdown body: groups, then elements, each with its
/// doc's first sentence.
pub fn index(schema: &Schema, pages: &[ElementPage]) -> String {
    let mut o = String::from(
        "# Elements\n\nEvery element is written `kind \
         [positional] { props; children }`. Each page lists an element's own props and events; \
         the props every node takes are on [`node`](#node). These pages are generated from \
         the schema `strand check` checks against.\n\n",
    );
    for (title, group) in [("Prop groups", true), ("Elements", false)] {
        let _ = writeln!(o, "## {title}\n\n<div class=\"ref-grid\">\n");
        for p in pages.iter().filter(|p| p.group == group) {
            let key = DocKey::Element(p.name.clone());
            let doc = schema.doc(&key).map(first_sentence).unwrap_or_default();
            let mut summary = String::new();
            pulldown_cmark::html::push_html(&mut summary, pulldown_cmark::Parser::new(&doc));
            let summary = summary
                .trim()
                .trim_start_matches("<p>")
                .trim_end_matches("</p>")
                .to_string();
            let _ = writeln!(
                o,
                "<a class=\"ref-card\" href=\"{}.html\"><code>{}</code><span>{summary}</span></a>",
                p.name,
                esc(&p.name)
            );
        }
        o.push_str("\n</div>\n\n");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentences_skip_dots_in_code() {
        assert_eq!(
            first_sentence("A slider: `slider { value: 0.5 }`. Dragged, it writes."),
            "A slider: `slider { value: 0.5 }`."
        );
        assert_eq!(
            first_sentence("Takes up the space left."),
            "Takes up the space left."
        );
    }
}
