//! A page's markdown as HTML: headings with GitHub's anchors (the
//! guide's own links use them), links resolved to the site's pages (or
//! to GitHub for a file the site does not publish, which must exist),
//! tables that scroll on their own, and code blocks by their info string:
//!
//! - ```` ```strand level=N file=NAME ```` is a guide step (strand-dev's
//!   guide test checks those, in page order);
//! - ```` ```strand file=NAME ```` is a whole file of the page's config,
//!   whose files, in page order, must check clean and be formatted;
//! - ```` ```strand fragment ```` is a piece of syntax, highlighted only;
//! - ```` ```strand-check ```` is a whole `shell.strand` shown with what
//!   `strand check` says of it, computed here: it must say something;
//! - ```` ```example NAME ```` is an animated example
//!   ([`crate::example`]);
//! - any other language is shown as written.
//!
//! A ```` ```strand ```` block that says none of these fails the build, so
//! nothing reads as checked that is not.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use pulldown_cmark::{CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::highlight::Highlighter;
use crate::html::{anchor, esc, relative};
use crate::nav::normalise;
use crate::{GITHUB_BLOB, example};

/// `(source, #anchor)` to the output page the anchor names (the
/// reference's `elements.md#node` is `reference/elements/node.html`).
pub type AnchorPages = HashMap<(String, String), String>;

/// What a page is rendered with.
pub struct Ctx<'a> {
    pub root: &'a Path,
    /// The page's source, from the repository's root (`docs/guide/level-1.md`).
    pub source: &'a str,
    /// Where it is written in the site (`guide/level-1.html`).
    pub out: &'a str,
    /// Every published source's output.
    pub outputs: &'a HashMap<String, String>,
    pub hl: &'a Highlighter,
    /// Anchors of a source that name a page of their own: a link to one
    /// goes to that page.
    pub anchor_pages: &'a AnchorPages,
    /// On a reference page: the types and elements its code links.
    pub links: Option<&'a crate::reference::Links>,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("source", &self.source)
            .field("out", &self.out)
            .finish_non_exhaustive()
    }
}

/// A link to check once every page is rendered.
#[derive(Debug)]
pub struct LinkRef {
    pub page: String,
    pub fragment: String,
    pub from: String,
}

#[derive(Debug, Default)]
pub struct Rendered {
    pub title: Option<String>,
    pub html: String,
    /// (level, text, id) of each `##` and `###` heading.
    pub toc: Vec<(u8, String, String)>,
    pub anchors: HashSet<String>,
    pub links: Vec<LinkRef>,
    /// The page's `strand file=` blocks, in order.
    pub config: Vec<(String, String)>,
    /// Plain text, for search.
    pub text: String,
    /// The same text split at each `##` and `###` heading, for search:
    /// the first part is the page's own, before its first heading.
    pub sections: Vec<SearchSection>,
}

/// A part of a page under one heading, as search finds it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SearchSection {
    /// The heading's text and anchor; empty for the page's opening part.
    pub heading: String,
    pub id: String,
    pub text: String,
}

impl Rendered {
    /// Text for search, to the part it is in.
    fn said(&mut self, t: &str) {
        self.text.push_str(t);
        self.text.push(' ');
        if self.sections.is_empty() {
            self.sections.push(SearchSection::default());
        }
        if let Some(s) = self.sections.last_mut() {
            s.text.push_str(t);
            s.text.push(' ');
        }
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
}

fn level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// `dest` as the page should link it.
fn resolve(ctx: &Ctx, dest: &str, r: &mut Rendered, errors: &mut Vec<String>) -> String {
    if dest.contains("://") || dest.starts_with("mailto:") {
        return dest.to_string();
    }
    let (path, fragment) = dest.split_once('#').unwrap_or((dest, ""));
    if path.is_empty() {
        if let Some(page) = ctx
            .anchor_pages
            .get(&(ctx.source.to_string(), fragment.to_string()))
        {
            r.links.push(LinkRef {
                page: page.clone(),
                fragment: String::new(),
                from: ctx.source.to_string(),
            });
            return relative(ctx.out, page);
        }
        r.links.push(LinkRef {
            page: ctx.out.to_string(),
            fragment: fragment.to_string(),
            from: ctx.source.to_string(),
        });
        return dest.to_string();
    }
    let dir = ctx.source.rsplit_once('/').map_or("", |(d, _)| d);
    let target = match normalise(&format!("{dir}/{path}")) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("{}: link `{dest}`: {e}", ctx.source));
            return dest.to_string();
        }
    };
    let hash = if fragment.is_empty() {
        String::new()
    } else {
        format!("#{fragment}")
    };
    if let Some(page) = ctx
        .anchor_pages
        .get(&(target.clone(), fragment.to_string()))
    {
        r.links.push(LinkRef {
            page: page.clone(),
            fragment: String::new(),
            from: ctx.source.to_string(),
        });
        return relative(ctx.out, page);
    }
    if let Some(out) = ctx.outputs.get(&target) {
        r.links.push(LinkRef {
            page: out.clone(),
            fragment: fragment.to_string(),
            from: ctx.source.to_string(),
        });
        return format!("{}{hash}", relative(ctx.out, out));
    }
    if !ctx.root.join(&target).exists() {
        errors.push(format!(
            "{}: link `{dest}`: {target} does not exist",
            ctx.source
        ));
    }
    format!("{GITHUB_BLOB}{target}{hash}")
}

/// The words of a code block's info string, and its `key=value` pairs.
fn info(s: &str) -> (Vec<&str>, HashMap<&str, &str>) {
    let mut words = Vec::new();
    let mut attrs = HashMap::new();
    for w in s.split_whitespace() {
        match w.split_once('=') {
            Some((k, v)) => {
                attrs.insert(k, v);
            }
            None => words.push(w),
        }
    }
    (words, attrs)
}

fn code_box(head: &str, lang: &str, body: &str) -> String {
    let head = if head.is_empty() {
        String::new()
    } else {
        format!("<div class=\"code-head\"><span class=\"code-file\">{head}</span></div>")
    };
    format!(
        "<div class=\"code\">{head}<pre><code class=\"language-{}\">{body}</code></pre></div>",
        esc(lang)
    )
}

fn code_block(
    ctx: &Ctx,
    info_str: &str,
    body: &str,
    r: &mut Rendered,
    errors: &mut Vec<String>,
) -> String {
    let (words, attrs) = info(info_str);
    let lang = words.first().copied().unwrap_or("");
    let at = ctx.source;
    match lang {
        "strand" => {
            let html = ctx.hl.html(body);
            if attrs.contains_key("level") {
                let file = attrs.get("file").copied().unwrap_or("");
                code_box(&esc(file), "strand", &html)
            } else if let Some(file) = attrs.get("file") {
                r.config.push(((*file).to_string(), body.to_string()));
                code_box(&esc(file), "strand", &html)
            } else if words.contains(&"signature") {
                match ctx.links {
                    Some(l) => code_box(
                        "",
                        "strand",
                        &l.signature(body, &mut |p, f| href(ctx, r, p, f)),
                    ),
                    None => code_box("", "strand", &html),
                }
            } else if words.contains(&"fragment") {
                code_box("", "strand", &html)
            } else {
                errors.push(format!(
                    "{at}: a ```strand block says `file=NAME` (a whole file, checked) or \
                     `fragment` (highlighted only): {}",
                    body.lines().next().unwrap_or("")
                ));
                code_box("", "strand", &html)
            }
        }
        "strand-check" => {
            let files = vec![("shell.strand".to_string(), body.to_string())];
            match crate::check::config(&format!("{at}-check-{}", r.config.len()), &files) {
                Ok(c) if c.errors + c.warnings == 0 => {
                    errors.push(format!(
                        "{at}: a ```strand-check block whose config checks clean shows nothing"
                    ));
                    String::new()
                }
                Ok(c) => format!(
                    "<div class=\"check\">{}<div class=\"check-out\"><div class=\"check-cmd\">\
                     <span class=\"prompt\">$</span> strand check</div><pre>{}</pre></div></div>",
                    code_box("shell.strand", "strand", &ctx.hl.html(body)),
                    esc(c.text.trim_end())
                ),
                Err(e) => {
                    errors.push(format!("{at}: {e}"));
                    String::new()
                }
            }
        }
        "example" => {
            let Some(name) = words.get(1) else {
                errors.push(format!("{at}: ```example names its scenario"));
                return String::new();
            };
            match example::load(ctx.root, name).and_then(|s| {
                // Its config is shown as a whole file: it checks clean and
                // is formatted, as a page's `strand file=` blocks are.
                let c = crate::check::config(&format!("example-{name}"), &s.files)?;
                if c.errors + c.warnings > 0 {
                    return Err(format!(
                        "{}: its config does not check clean:\n{}",
                        s.path, c.text
                    ));
                }
                if !c.unformatted.is_empty() {
                    return Err(format!(
                        "{}: `strand fmt` would change its {}",
                        s.path,
                        c.unformatted.join(", ")
                    ));
                }
                let m = example::media(ctx.root, &s)?;
                Ok(example::figure(&s, &m, ctx.hl, &crate::html::root(ctx.out)))
            }) {
                Ok(html) => html,
                Err(e) => {
                    errors.push(format!("{at}: {e}"));
                    String::new()
                }
            }
        }
        _ => {
            let file = attrs.get("file").map(|f| esc(f)).unwrap_or_default();
            code_box(&file, lang, &esc(body))
        }
    }
}

/// The URL from the page to `page#frag` (site paths), recorded as a link
/// the build checks.
fn href(ctx: &Ctx, r: &mut Rendered, page: &str, frag: &str) -> String {
    r.links.push(LinkRef {
        page: page.to_string(),
        fragment: frag.to_string(),
        from: ctx.source.to_string(),
    });
    let url = relative(ctx.out, page);
    if frag.is_empty() {
        url
    } else {
        format!("{url}#{frag}")
    }
}

/// The `id="…"`s of raw HTML, which are anchors as headings' are.
fn html_ids(html: &str, anchors: &mut HashSet<String>) {
    for rest in html.split(" id=\"").skip(1) {
        if let Some((id, _)) = rest.split_once('"') {
            anchors.insert(id.to_string());
        }
    }
}

/// Renders `text`, a page's markdown.
pub fn render(text: &str, ctx: &Ctx, errors: &mut Vec<String>) -> Rendered {
    let mut r = Rendered::default();
    let mut in_link = 0usize;
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Event> = Vec::new();
    let mut events = Parser::new_ext(text, options()).peekable();
    while let Some(ev) = events.next() {
        match ev {
            Event::Start(Tag::Heading { level: l, id, .. }) => {
                let mut inner = Vec::new();
                let mut plain = String::new();
                for e in events.by_ref() {
                    match &e {
                        Event::End(TagEnd::Heading(_)) => break,
                        Event::Text(t) | Event::Code(t) => plain.push_str(t),
                        _ => {}
                    }
                    inner.push(e);
                }
                let base = id.map(|i| i.to_string()).unwrap_or_else(|| anchor(&plain));
                let n = ids.entry(base.clone()).or_insert(0);
                let id = if *n == 0 {
                    base.clone()
                } else {
                    format!("{base}-{n}")
                };
                *n += 1;
                let lv = level(l);
                if lv == 1 && r.title.is_none() {
                    r.title = Some(plain.clone());
                }
                if lv == 2 || lv == 3 {
                    r.toc.push((lv, plain.clone(), id.clone()));
                }
                r.anchors.insert(id.clone());
                r.text.push_str(&plain);
                r.text.push('\n');
                if lv == 2 || lv == 3 {
                    r.sections.push(SearchSection {
                        heading: plain.clone(),
                        id: id.clone(),
                        text: String::new(),
                    });
                }
                let mut h = String::new();
                let inner: Vec<Event> = inner
                    .into_iter()
                    .map(|e| rewrite_link(e, ctx, &mut r, errors))
                    .collect();
                pulldown_cmark::html::push_html(&mut h, inner.into_iter());
                let html = if lv == 1 {
                    format!("<h1 id=\"{id}\">{h}</h1>\n")
                } else {
                    format!(
                        "<h{lv} id=\"{id}\"><a class=\"anchor\" href=\"#{id}\" aria-hidden=\"true\">#</a>{h}</h{lv}>\n"
                    )
                };
                out.push(Event::Html(CowStr::from(html)));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let info_str = match &kind {
                    CodeBlockKind::Fenced(i) => i.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let mut body = String::new();
                for e in events.by_ref() {
                    match e {
                        Event::End(TagEnd::CodeBlock) => break,
                        Event::Text(t) => body.push_str(&t),
                        _ => {}
                    }
                }
                let html = code_block(ctx, &info_str, &body, &mut r, errors);
                out.push(Event::Html(CowStr::from(html)));
            }
            Event::Start(Tag::Table(a)) => {
                out.push(Event::Html(CowStr::from("<div class=\"table\">")));
                out.push(Event::Start(Tag::Table(a)));
            }
            Event::End(TagEnd::Table) => {
                out.push(Event::End(TagEnd::Table));
                out.push(Event::Html(CowStr::from("</div>")));
            }
            Event::Text(t) => {
                r.said(&t);
                out.push(Event::Text(t));
            }
            Event::Code(t) => {
                r.said(&t);
                match ctx.links {
                    Some(l) if in_link == 0 => {
                        let html = l.code(&t, &mut |p, f| href(ctx, &mut r, p, f));
                        out.push(Event::InlineHtml(CowStr::from(html)));
                    }
                    _ => out.push(Event::Code(t)),
                }
            }
            Event::Html(t) => {
                html_ids(&t, &mut r.anchors);
                out.push(Event::Html(t));
            }
            Event::InlineHtml(t) => {
                html_ids(&t, &mut r.anchors);
                out.push(Event::InlineHtml(t));
            }
            e => {
                match &e {
                    Event::Start(Tag::Link { .. }) => in_link += 1,
                    Event::End(TagEnd::Link) => in_link = in_link.saturating_sub(1),
                    _ => {}
                }
                out.push(rewrite_link(e, ctx, &mut r, errors))
            }
        }
    }
    pulldown_cmark::html::push_html(&mut r.html, out.into_iter());
    r
}

fn rewrite_link<'e>(
    e: Event<'e>,
    ctx: &Ctx,
    r: &mut Rendered,
    errors: &mut Vec<String>,
) -> Event<'e> {
    match e {
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let dest = resolve(ctx, &dest_url, r, errors);
            Event::Start(Tag::Link {
                link_type,
                dest_url: CowStr::from(dest),
                title,
                id,
            })
        }
        e => e,
    }
}

/// The page's table of contents, `##` and `###`.
pub fn toc(r: &Rendered) -> String {
    if r.toc.len() < 2 {
        return String::new();
    }
    let mut o = String::from(
        "<nav class=\"toc\" aria-label=\"On this page\"><p class=\"toc-title\">On this page</p><ul>",
    );
    for (lv, text, id) in &r.toc {
        let _ = write!(
            o,
            "<li class=\"toc-{lv}\"><a href=\"#{id}\">{}</a></li>",
            esc(text)
        );
    }
    o.push_str("</ul></nav>");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn run(text: &str) -> (Rendered, Vec<String>) {
        let root = repo();
        let hl = Highlighter::new().unwrap();
        let mut outputs = HashMap::new();
        outputs.insert(
            "docs/guide/level-2.md".to_string(),
            "guide/level-2.html".to_string(),
        );
        let ctx = Ctx {
            root: &root,
            source: "docs/guide/level-1.md",
            out: "guide/level-1.html",
            outputs: &outputs,
            hl: &hl,
            anchor_pages: &AnchorPages::new(),
            links: None,
        };
        let mut errors = Vec::new();
        let r = render(text, &ctx, &mut errors);
        (r, errors)
    }

    #[test]
    fn links_resolve_to_pages_or_to_github_and_must_exist() {
        let (r, errors) = run("[a](level-2.md#tokens) [b](../design.md) [c](nowhere.md)");
        assert!(
            r.html.contains("href=\"level-2.html#tokens\""),
            "{}",
            r.html
        );
        assert!(
            r.html
                .contains(&format!("href=\"{GITHUB_BLOB}docs/design.md\""))
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("docs/guide/nowhere.md does not exist"));
        // The page link is checked against the other page's headings later.
        assert!(
            r.links
                .iter()
                .any(|l| l.page == "guide/level-2.html" && l.fragment == "tokens")
        );
    }

    #[test]
    fn headings_get_github_ids_and_duplicates_are_numbered() {
        let (r, errors) = run("# Title\n\n## Same name\n\n## Same name\n\n### `x` and y");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(r.title.as_deref(), Some("Title"));
        let ids: Vec<&str> = r.toc.iter().map(|(_, _, id)| id.as_str()).collect();
        assert_eq!(ids, ["same-name", "same-name-1", "x-and-y"]);
    }

    #[test]
    fn a_strand_block_says_what_it_is() {
        let (_, errors) = run("```strand\nbar Top { }\n```");
        assert_eq!(errors.len(), 1, "{errors:?}");
        let (r, errors) =
            run("```strand file=shell.strand\nbar Top { }\n```\n\n```strand fragment\nx: 1\n```");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            r.config,
            [("shell.strand".to_string(), "bar Top { }\n".to_string())]
        );
    }

    #[test]
    fn a_check_block_shows_the_real_diagnostic_and_must_have_one() {
        let (r, errors) = run("```strand-check\nbar Top {\n  text pct(battery.precent)\n}\n```");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(r.html.contains("did you mean `percent`"), "{}", r.html);
        let (_, errors) = run("```strand-check\nbar Top {\n  text pct(battery.percent)\n}\n```");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("checks clean"));
    }
}
