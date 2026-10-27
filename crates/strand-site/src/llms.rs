//! The site for language models (llmstxt.org): every page again as
//! markdown beside its HTML (`guide/level-1.html.md`), `llms.txt` (the
//! summary and the pages by sidebar section, each with its first
//! sentence) and `llms-full.txt` (every page in one file). The markdown
//! is what the page shows: repo-only spans are left out, links go to
//! the other pages' markdown, a docs example is its config, and a
//! playground is left out. Links are from the site's root
//! (`/guide/level-1.html.md`), so they hold on any host.

use std::ops::Range;
use std::path::Path;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::example;
use crate::markdown::{Ctx, Rendered, options, resolve, shown};
use crate::nav::normalise;

pub const INDEX: &str = "llms.txt";
pub const FULL: &str = "llms-full.txt";

/// The markdown copy of the page at `out`.
pub fn md_path(out: &str) -> String {
    format!("{out}.md")
}

/// A page's source as its markdown copy says it.
pub fn page(text: &str, ctx: &Ctx, errors: &mut Vec<String>) -> String {
    let text = shown(text).unwrap_or_else(|_| text.to_string());
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut scratch = Rendered::default();
    let mut ignored = Vec::new();
    for (ev, range) in Parser::new_ext(&text, options()).into_offset_iter() {
        match ev {
            Event::Start(Tag::Link { dest_url, .. }) => {
                let href = to_markdown(
                    ctx.out,
                    &resolve(ctx, &dest_url, &mut scratch, &mut ignored),
                );
                let raw = &text[range.clone()];
                let from = format!("]({dest_url}");
                if href != *dest_url
                    && let Some(at) = raw.find(&from)
                {
                    let start = range.start + at + 2;
                    edits.push((start..start + dest_url.len(), href));
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                if let Some(new) = fence(ctx.root, &info, &text[range.clone()], errors) {
                    edits.push((range, new));
                }
            }
            _ => {}
        }
    }
    let mut out = text.clone();
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    for (r, new) in edits {
        out.replace_range(r, &new);
    }
    out.trim().to_string() + "\n"
}

/// A link from the page at `from` to a page of the site goes to its
/// markdown, and every link in the site is from the site's root
/// (`/guide/level-1.html.md`): it holds on any host, and still reads
/// when the text is pasted without its address.
fn to_markdown(from: &str, href: &str) -> String {
    if href.contains("://") || href.starts_with('#') || href.starts_with("mailto:") {
        return href.to_string();
    }
    let (path, hash) = match href.split_once('#') {
        Some((p, h)) => (p, format!("#{h}")),
        None => (href, String::new()),
    };
    let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
    let Ok(target) = normalise(&format!("{dir}/{path}")) else {
        return href.to_string();
    };
    if target.ends_with(".html") {
        format!("/{}{hash}", md_path(&target))
    } else {
        format!("/{target}{hash}")
    }
}

/// A fenced block as its markdown copy has it, when it changes: an
/// example is its files, `strand-check` is the file it checks, a
/// playground goes, and a Strand block loses its build attributes.
fn fence(root: &Path, info: &str, raw: &str, errors: &mut Vec<String>) -> Option<String> {
    let mut words = info.split_whitespace();
    let lang = words.next()?;
    let body = || {
        let start = raw.find('\n').map_or(raw.len(), |i| i + 1);
        let end = raw.trim_end().rfind('\n').unwrap_or(start).max(start);
        raw[start..end].to_string()
    };
    match lang {
        "example" => {
            let name = words.next()?;
            match example::load(root, name) {
                Ok(s) => Some(
                    s.files
                        .iter()
                        .map(|(file, src)| {
                            format!("`{file}`:\n\n```strand\n{}\n```", src.trim_end())
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                ),
                Err(e) => {
                    errors.push(e);
                    None
                }
            }
        }
        "playground" => Some(String::new()),
        "strand-check" => Some(format!("```strand\n{}\n```", body().trim_end())),
        "strand" if words.next().is_some() => {
            Some(format!("```strand\n{}\n```", body().trim_end()))
        }
        _ => None,
    }
}

/// A page's first sentence, for `llms.txt`.
pub fn summary(markdown: &str) -> String {
    let mut text = String::new();
    let mut in_para = false;
    for ev in Parser::new_ext(markdown, options()) {
        match ev {
            Event::Start(Tag::Paragraph) => in_para = true,
            Event::End(TagEnd::Paragraph) if in_para && !text.trim().is_empty() => break,
            Event::End(TagEnd::Paragraph) => in_para = false,
            Event::Text(t) | Event::Code(t) if in_para => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak if in_para => text.push(' '),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = text
        .match_indices(". ")
        .map(|(i, _)| i + 1)
        .find(|&i| i >= 12)
        .unwrap_or(text.len());
    let first = &text[..end];
    // A line of llms.txt, not a paragraph: a long first sentence (the
    // guide's lists of what a level teaches) is cut at a word.
    if first.chars().count() <= 160 {
        return first.to_string();
    }
    let cut = first
        .char_indices()
        .nth(160)
        .map_or(first.len(), |(i, _)| i);
    let at = first[..cut].rfind(' ').unwrap_or(cut);
    format!("{}…", first[..at].trim_end_matches([',', ';', ':']))
}

/// One sidebar section's pages: (title, output page, summary).
pub type Section = (String, Vec<(String, String, String)>);

/// `llms.txt`.
pub fn index(sections: &[Section]) -> String {
    let mut o = String::from(
        "# Strand\n\n> Strand is a toolkit for Wayland shells: bars, launchers, lock screens \
         and panels written in one small typed language. Values animate on springs, saving a \
         file updates the running shell without restarting it, and an idle shell does no work.\n\n\
         Every page below is markdown (the HTML page's address with `.md` added); \
         [llms-full.txt](/llms-full.txt) is all of them in one file. Strand configs are `.strand` \
         files; `strand check` checks one and `strand run` runs it.\n",
    );
    for (title, pages) in sections {
        o.push_str(&format!("\n## {title}\n\n"));
        for (label, out, about) in pages {
            if about.is_empty() {
                o.push_str(&format!("- [{label}](/{})\n", md_path(out)));
            } else {
                o.push_str(&format!("- [{label}](/{}): {about}\n", md_path(out)));
            }
        }
    }
    o
}

/// `llms-full.txt`: each page, its address first.
pub fn full(pages: &[(String, String)]) -> String {
    let mut o =
        String::from("# Strand documentation\n\nEvery page of the Strand site, as markdown.\n");
    for (out, md) in pages {
        o.push_str(&format!("\n---\n\n<!-- page: {out} -->\n\n{md}"));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_to_pages_go_to_their_markdown() {
        let from = "guide/level-1.html";
        assert_eq!(
            to_markdown(from, "../concepts/motion.html#springs"),
            "/concepts/motion.html.md#springs"
        );
        assert_eq!(to_markdown(from, "level-2.html"), "/guide/level-2.html.md");
        assert_eq!(to_markdown(from, "#springs"), "#springs");
        assert_eq!(
            to_markdown(from, "https://github.com/x/y.html"),
            "https://github.com/x/y.html"
        );
        assert_eq!(
            to_markdown(from, "../media/hello-light.png"),
            "/media/hello-light.png"
        );
    }

    #[test]
    fn a_summary_is_the_first_sentence() {
        let md = "# Title\n\nA bar on every\nmonitor. It updates.\n\nMore.";
        assert_eq!(summary(md), "A bar on every monitor.");
        assert_eq!(
            summary("# T\n\nShort. Then a longer sentence."),
            "Short. Then a longer sentence."
        );
        let long = format!("You learn: {}.", ["word"; 60].join(", "));
        let cut = summary(&format!("# T\n\n{long}"));
        assert!(
            cut.ends_with("word…") && cut.chars().count() <= 161,
            "{cut}"
        );
    }

    #[test]
    fn the_index_lists_each_section_with_markdown_links() {
        let s: Vec<Section> = vec![(
            "Guide".into(),
            vec![(
                "1. The hello bar".into(),
                "guide/level-1.html".into(),
                "A bar.".into(),
            )],
        )];
        let t = index(&s);
        assert!(t.starts_with("# Strand\n\n> "), "{t}");
        assert!(
            t.contains("## Guide\n\n- [1. The hello bar](/guide/level-1.html.md): A bar.\n"),
            "{t}"
        );
    }
}
