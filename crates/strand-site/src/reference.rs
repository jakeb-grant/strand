//! The reference: strand-dev's generator's pages (`docs/reference/*.md`,
//! written from the schema `strand check` checks against), so the site
//! and the committed reference say the same thing. Elements and services
//! get a page each and an index of cards; types, functions and tokens a
//! page each. A type or element named in a signature or a type cell
//! links to its page ([`Links`]).

use std::collections::HashMap;
use std::fmt::Write as _;

use strand_compiler::schema::{DocKey, Schema};
use strand_compiler::ty::Origin;

use crate::html::{anchor, esc};

/// Where the element pages are written.
pub const DIR: &str = "reference/elements";
/// What their links resolve against: the page they come from.
pub const SOURCE: &str = "docs/reference/elements.md";

/// The service pages.
pub const SERVICES_DIR: &str = "reference/services";
pub const SERVICES_SOURCE: &str = "docs/reference/services.md";
/// The pages that are one page each: (set, source, output).
pub const SINGLE: &[(&str, &str, &str)] = &[
    (
        "reference/types",
        "docs/reference/types.md",
        "reference/types.html",
    ),
    (
        "reference/functions",
        "docs/reference/functions.md",
        "reference/functions.html",
    ),
    (
        "reference/tokens",
        "docs/reference/tokens.md",
        "reference/tokens.html",
    ),
];

/// One generated page, as markdown.
#[derive(Debug, Clone)]
pub struct RefPage {
    pub name: String,
    pub group: bool,
    pub out: String,
    /// What its links resolve against.
    pub source: &'static str,
    pub markdown: String,
}

/// A set of `site/SUMMARY.md`'s (`@reference/elements`): the page the
/// sidebar links, and the pages it leads to.
#[derive(Debug)]
pub struct Set {
    pub index: RefPage,
    pub pages: Vec<RefPage>,
}

/// The generator's page `name` (`types.md`) without its generated note.
fn generated(schema: &Schema, name: &str) -> String {
    let pages = strand_dev::reference::pages(schema);
    let text = pages.get(name).cloned().unwrap_or_default();
    match text.find("\n# ") {
        Some(i) => text[i + 1..].to_string(),
        None => text,
    }
}

/// The set `set` names, or `None` for one there is not.
pub fn set(schema: &Schema, set: &str) -> Option<Set> {
    match set {
        "reference/elements" => {
            let pages = element_pages(schema);
            let index = RefPage {
                name: "Elements".into(),
                group: false,
                out: format!("{DIR}/index.html"),
                source: SOURCE,
                markdown: element_index(schema, &pages),
            };
            Some(Set { index, pages })
        }
        "reference/services" => {
            let (intro, pages) = service_pages(schema);
            let index = RefPage {
                name: "Services".into(),
                group: false,
                out: format!("{SERVICES_DIR}/index.html"),
                source: SERVICES_SOURCE,
                markdown: service_index(schema, &intro, &pages),
            };
            Some(Set { index, pages })
        }
        _ => {
            let (_, source, out) = SINGLE.iter().find(|(s, _, _)| *s == set)?;
            let file = source.rsplit('/').next().unwrap_or(source);
            let mut markdown = generated(schema, file);
            if set == "reference/types" {
                markdown = type_anchors(&markdown);
            }
            let index = RefPage {
                name: file.trim_end_matches(".md").to_string(),
                group: false,
                out: (*out).to_string(),
                source,
                markdown,
            };
            Some(Set {
                index,
                pages: Vec::new(),
            })
        }
    }
}

/// Every element's and group's page.
pub fn element_pages(schema: &Schema) -> Vec<RefPage> {
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
            Some(RefPage {
                name: name.to_string(),
                group,
                out: format!("{DIR}/{name}.html"),
                source: SOURCE,
                markdown: format!("# `{name}`\n{body}"),
            })
        })
        .collect()
}

/// The services page split at its `## `name`` sections: its intro, and a
/// page per service.
fn service_pages(schema: &Schema) -> (String, Vec<RefPage>) {
    let text = generated(schema, "services.md");
    let mut parts = text.split("\n## `");
    let intro = parts.next().unwrap_or("").to_string();
    let pages = parts
        .filter_map(|p| {
            let (name, body) = p.split_once("`\n")?;
            Some(RefPage {
                name: name.to_string(),
                group: false,
                out: format!("{SERVICES_DIR}/{name}.html"),
                source: SERVICES_SOURCE,
                markdown: format!("# `{name}`\n{body}"),
            })
        })
        .collect();
    (intro, pages)
}

/// The types page with an anchor on each enum, alias and opaque type,
/// as its records have from their headings, so a signature can link
/// them.
fn type_anchors(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 2048);
    let mut section = "";
    for line in md.split_inclusive('\n') {
        if let Some(h) = line.strip_prefix("## ") {
            section = match h.trim() {
                "Enums" | "Aliases" => "table",
                "Opaque types" => "opaques",
                _ => "",
            };
            out.push_str(line);
            continue;
        }
        match section {
            "table" => {
                if let Some(rest) = line.strip_prefix("| `")
                    && let Some((name, tail)) = rest.split_once("` |")
                    && !name.contains(' ')
                {
                    let _ = write!(
                        out,
                        "| <code id=\"{}\">{}</code> |{tail}",
                        anchor(name),
                        esc(name)
                    );
                    continue;
                }
                out.push_str(line);
            }
            "opaques" if line.starts_with('`') => {
                // `A`, `B`. Its doc, `C`: each entry opens with its name,
                // at the line's start or after a `, `.
                let mut o = String::new();
                let mut rest = line;
                let mut at_entry = true;
                while let Some(i) = rest.find('`') {
                    let (before, after) = rest.split_at(i);
                    o.push_str(before);
                    let Some(end) = after[1..].find('`') else {
                        o.push_str(after);
                        rest = "";
                        break;
                    };
                    let name = &after[1..1 + end];
                    let entry = (at_entry && before.is_empty()) || before.ends_with(", ");
                    if entry && name.chars().all(|c| c.is_ascii_alphanumeric()) {
                        let _ = write!(o, "<code id=\"{}\">{}</code>", anchor(name), esc(name));
                    } else {
                        o.push_str(&after[..end + 2]);
                    }
                    at_entry = false;
                    rest = &after[end + 2..];
                }
                o.push_str(rest);
                out.push_str(&o);
            }
            _ => out.push_str(line),
        }
    }
    out
}

/// The output pages named by `#anchor`s of the generator's pages: an
/// element's or group's of `elements.md`, a service's of `services.md`.
pub fn anchor_pages(schema: &Schema) -> HashMap<(String, String), String> {
    let mut m = HashMap::new();
    for (name, _) in strand_dev::reference::element_names(schema) {
        m.insert(
            (SOURCE.to_string(), strand_dev::reference::anchor_of(name)),
            format!("{DIR}/{name}.html"),
        );
    }
    for name in schema.services.keys() {
        m.insert(
            (SERVICES_SOURCE.to_string(), anchor(name)),
            format!("{SERVICES_DIR}/{name}.html"),
        );
    }
    m
}

/// What a name in a signature links to.
#[derive(Debug, Default)]
pub struct Links {
    /// A type's name to its page and anchor (site paths): the records,
    /// enums, aliases and opaque types of `types.md`, and each service's
    /// record to its service's page.
    pub types: HashMap<String, (String, String)>,
    /// An element's or group's name to its page.
    pub elements: HashMap<String, String>,
}

/// Turns a site path and anchor into a page's URL, recording the link.
pub type Href<'a> = &'a mut dyn FnMut(&str, &str) -> String;

impl Links {
    pub fn new(schema: &Schema) -> Links {
        let mut types = HashMap::new();
        let page = "reference/types.html".to_string();
        let services: HashMap<&str, &str> = schema
            .services
            .iter()
            .map(|(n, id)| (schema.types.record(*id).name.as_str(), n.as_str()))
            .collect();
        for r in &schema.types.records {
            if let Some(svc) = services.get(r.name.as_str()) {
                types.insert(
                    r.name.clone(),
                    (format!("{SERVICES_DIR}/{svc}.html"), String::new()),
                );
            } else if matches!(r.origin, Origin::Schema) {
                types.insert(r.name.clone(), (page.clone(), anchor(&r.name)));
            }
        }
        for e in &schema.types.enums {
            if matches!(e.origin, Origin::Schema) {
                types.insert(e.name.clone(), (page.clone(), anchor(&e.name)));
            }
        }
        for n in schema.aliases.keys().chain(schema.opaques.keys()) {
            types.insert(n.clone(), (page.clone(), anchor(n)));
        }
        // Only a type's own name is linked, never a word of the language
        // spelled the same (`text`, `color`): a schema type starts with a
        // capital.
        types.retain(|n, _| n.starts_with(|c: char| c.is_ascii_uppercase()));
        let elements = strand_dev::reference::element_names(schema)
            .into_iter()
            .map(|(n, _)| (n.to_string(), format!("{DIR}/{n}.html")))
            .collect();
        Links { types, elements }
    }

    /// `code` (an inline code span's text) as HTML, each type it names
    /// linked. A name after `.`, `$`, `#` or `@`, or inside a string, is
    /// not a type.
    pub fn code(&self, code: &str, href: Href) -> String {
        format!("<code>{}</code>", self.code_inner(code, href))
    }

    fn code_inner(&self, code: &str, href: Href) -> String {
        let mut o = String::new();
        let b = code.as_bytes();
        let mut i = 0;
        let mut quoted = false;
        let mut last = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'"' {
                quoted = !quoted;
                i += 1;
                continue;
            }
            if !(c.is_ascii_alphabetic() || c == b'_') {
                i += 1;
                continue;
            }
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &code[s..i];
            let prev = s.checked_sub(1).map(|p| b[p]);
            let a_name = !matches!(prev, Some(b'.' | b'$' | b'#' | b'@'));
            if !quoted
                && a_name
                && let Some((page, frag)) = self.types.get(word)
            {
                o.push_str(&esc(&code[last..s]));
                let _ = write!(o, "<a href=\"{}\">{}</a>", href(page, frag), esc(word));
                last = i;
            }
        }
        o.push_str(&esc(&code[last..]));
        o
    }

    /// An element's signature (`element spectrum(AudioDevice -> source):
    /// node`) as highlighted HTML: the keyword, the name, the
    /// positional's type and the prop it fills, and the groups it takes
    /// in, each type and group linked.
    pub fn signature(&self, sig: &str, href: Href) -> String {
        let sig = sig.trim_end();
        let (head, groups) = match sig.rfind(": ") {
            Some(i) if !sig[i..].contains(')') => (&sig[..i], Some(&sig[i + 2..])),
            _ => (sig, None),
        };
        let mut o = String::new();
        let (kw, rest) = head.split_once(' ').unwrap_or((head, ""));
        let _ = write!(o, "<span class=\"hl-keyword\">{}</span> ", esc(kw));
        let (name, arg) = match rest.split_once('(') {
            Some((n, a)) => (n, Some(a.strip_suffix(')').unwrap_or(a))),
            None => (rest, None),
        };
        let _ = write!(o, "<span class=\"hl-type\">{}</span>", esc(name));
        if let Some(arg) = arg {
            let (ty, prop) = arg.split_once(" -> ").unwrap_or((arg, ""));
            let _ = write!(
                o,
                "(<span class=\"hl-type\">{}</span> <span class=\"hl-operator\">-&gt;</span> \
                 <span class=\"hl-property\">{}</span>)",
                self.code_inner(ty, &mut *href),
                esc(prop)
            );
        }
        if let Some(groups) = groups {
            o.push_str(": ");
            for (i, g) in groups.split(", ").enumerate() {
                if i > 0 {
                    o.push_str(", ");
                }
                match self.elements.get(g) {
                    Some(page) => {
                        let _ = write!(o, "<a href=\"{}\">{}</a>", href(page, ""), esc(g));
                    }
                    None => o.push_str(&esc(g)),
                }
            }
        }
        o.push('\n');
        o
    }
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

/// A card per page, each with its doc's first sentence.
fn cards(o: &mut String, schema: &Schema, pages: &[&RefPage], key: fn(&str) -> DocKey) {
    // One HTML block, no blank line in it, so the cards are the grid's
    // own items rather than one paragraph inside it.
    let _ = writeln!(o, "<div class=\"ref-grid\">");
    for p in pages {
        let doc = schema
            .doc(&key(&p.name))
            .map(first_sentence)
            .unwrap_or_default();
        let mut summary = String::new();
        pulldown_cmark::html::push_html(&mut summary, pulldown_cmark::Parser::new(&doc));
        let summary = summary
            .trim()
            .trim_start_matches("<p>")
            .trim_end_matches("</p>")
            .to_string();
        let file = p.out.rsplit('/').next().unwrap_or(&p.out);
        let _ = writeln!(
            o,
            "<a class=\"ref-card\" href=\"{file}\"><code>{}</code><span>{summary}</span></a>",
            esc(&p.name)
        );
    }
    o.push_str("</div>\n\n");
}

/// The element index's markdown: groups, then elements.
fn element_index(schema: &Schema, pages: &[RefPage]) -> String {
    let mut o = String::from(
        "# Elements\n\nEvery element is written `kind \
         [positional] { props; children }`. Each page lists an element's own props and events; \
         the props every node takes are on [`node`](#node). These pages are generated from \
         the schema `strand check` checks against.\n\n",
    );
    for (title, group) in [("Prop groups", true), ("Elements", false)] {
        let _ = writeln!(o, "## {title}\n");
        let these: Vec<&RefPage> = pages.iter().filter(|p| p.group == group).collect();
        cards(&mut o, schema, &these, |n| DocKey::Element(n.to_string()));
    }
    o
}

/// The service index's markdown: the generator's intro, then a card per
/// service.
fn service_index(schema: &Schema, intro: &str, pages: &[RefPage]) -> String {
    let mut o = intro.trim_end().to_string();
    o.push_str(
        "\n\nEach service has a page of its own. These pages are generated from the schema \
         `strand check` checks against.\n\n",
    );
    let these: Vec<&RefPage> = pages.iter().collect();
    cards(&mut o, schema, &these, |n| DocKey::Type(n.to_string()));
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        strand_dev::reference::schema().unwrap()
    }

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

    fn href(page: &str, frag: &str) -> String {
        if frag.is_empty() {
            page.to_string()
        } else {
            format!("{page}#{frag}")
        }
    }

    #[test]
    fn a_type_in_code_links_to_its_entry_and_a_word_of_the_language_does_not() {
        let s = schema();
        let l = Links::new(&s);
        let html = l.code("fn blur(radius: length) -> Filter", &mut href);
        assert_eq!(
            html,
            "<code>fn blur(radius: length) -&gt; <a href=\"reference/types.html#filter\">Filter</a></code>"
        );
        let html = l.code("[AudioDevice]", &mut href);
        assert!(
            html.contains("types.html#audiodevice\">AudioDevice</a>]"),
            "{html}"
        );
        // Not a field, a token, a string or a lower-case word.
        let html = l.code("x.Filter $Filter \"Filter\" text color", &mut href);
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn a_signature_links_its_type_and_its_groups() {
        let l = Links::new(&schema());
        let html = l.signature("element spectrum(AudioDevice -> source): node\n", &mut href);
        assert!(
            html.contains("<a href=\"reference/types.html#audiodevice\">AudioDevice</a>"),
            "{html}"
        );
        assert!(
            html.contains(": <a href=\"reference/elements/node.html\">node</a>"),
            "{html}"
        );
        let html = l.signature("group node\n", &mut href);
        assert!(!html.contains("<a "), "{html}");
        let html = l.signature(
            "element thumbnail(Window | Screen -> source): node",
            &mut href,
        );
        assert!(html.contains(">Window</a> | <a"), "{html}");
    }

    #[test]
    fn every_enum_alias_and_opaque_type_gets_an_anchor() {
        let s = schema();
        let md = set(&s, "reference/types").unwrap().index.markdown;
        for e in s
            .types
            .enums
            .iter()
            .filter(|e| matches!(e.origin, Origin::Schema))
        {
            assert!(
                md.contains(&format!("<code id=\"{}\">", anchor(&e.name))),
                "{}",
                e.name
            );
        }
        for n in s.aliases.keys().chain(s.opaques.keys()) {
            assert!(md.contains(&format!("<code id=\"{}\">", anchor(n))), "{n}");
        }
        assert!(!md.contains("Generated by"), "the generated note stays out");
    }

    #[test]
    fn every_service_gets_a_page() {
        let s = schema();
        let set = set(&s, "reference/services").unwrap();
        assert_eq!(set.pages.len(), s.services.len());
        for p in &set.pages {
            assert!(p.markdown.starts_with(&format!("# `{}`\n", p.name)));
            assert!(
                set.index
                    .markdown
                    .contains(&format!("href=\"{}.html\"", p.name))
            );
        }
        assert!(set.index.markdown.starts_with("# Services"));
    }
}
