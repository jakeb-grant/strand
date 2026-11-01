//! The claims ledger (`site/claims.toml`): each factual claim a site page
//! makes about Strand, with where it is proven. The build fails when a
//! claim's `text` is not on its page (whitespace runs count as one
//! space, so a rewrapped line still matches), when a `source` does not
//! exist (`path`, `path "snippet"` with the snippet in the file, under
//! the same whitespace rule, or `path.md#anchor` with the heading there),
//! when a source is `path:line` or `path:first-last`, which only says a
//! line is in range and rots unseen, when a source is the claim's own
//! page or the ledger, which hold its words whatever is true, when a
//! `proof` that names a test (`path.rs::name`) names no such function,
//! and when an entry lacks `page`, `text` or `source`, or has a key the
//! format does not know. A page's repository-only spans
//! ([`crate::markdown::shown`]) are not on it. Whether the source says
//! what the text says is for the people who check the site: the ledger
//! only keeps a claim from outliving the words it was written for or the
//! code it points at.

use std::collections::HashSet;
use std::path::Path;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use toml_edit::{DocumentMut, Item, Value};

/// The ledger, from the repository's root.
pub const LEDGER: &str = "site/claims.toml";

/// One claim, as the ledger has it.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub page: String,
    pub text: String,
    pub source: Vec<String>,
    pub proof: Option<String>,
}

const KEYS: &[&str] = &["page", "text", "source", "proof"];

/// The fewest characters a snippet may have, with whitespace runs as one
/// space: a shorter one turns up in a file by chance and proves nothing.
const MIN_SNIPPET: usize = 8;

/// `s` with every run of whitespace as one space, trimmed.
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn string(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// The claims in `text`, or every problem with its entries.
pub fn parse(text: &str) -> Result<Vec<Claim>, Vec<String>> {
    let doc: DocumentMut = text.parse().map_err(|e| vec![format!("{LEDGER}: {e}")])?;
    let mut errors = Vec::new();
    let mut claims = Vec::new();
    for (key, item) in doc.iter() {
        if key != "claim" {
            errors.push(format!(
                "{LEDGER}: `{key}` is not part of the ledger (entries are `[[claim]]`)"
            ));
            continue;
        }
        let Some(tables) = item.as_array_of_tables() else {
            errors.push(format!("{LEDGER}: `claim` is written `[[claim]]`"));
            continue;
        };
        for (i, t) in tables.iter().enumerate() {
            let at = format!("{LEDGER}: claim {}", i + 1);
            for (k, _) in t.iter() {
                if !KEYS.contains(&k) {
                    errors.push(format!(
                        "{at}: no key `{k}` (a claim has {})",
                        KEYS.join(", ")
                    ));
                }
            }
            let get = |k: &str| t.get(k).and_then(Item::as_value);
            let page = get("page")
                .and_then(string)
                .filter(|s| !s.trim().is_empty());
            let text = get("text")
                .and_then(string)
                .filter(|s| !s.trim().is_empty());
            let source: Vec<String> = match get("source") {
                Some(Value::String(s)) => vec![s.value().clone()],
                Some(Value::Array(a)) => a.iter().filter_map(string).collect(),
                _ => Vec::new(),
            };
            let source: Vec<String> = source
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect();
            let proof = get("proof").and_then(string);
            let mut missing = Vec::new();
            if page.is_none() {
                missing.push("page");
            }
            if text.is_none() {
                missing.push("text");
            }
            if source.is_empty() {
                missing.push("source");
            }
            if !missing.is_empty() {
                let what = text
                    .as_deref()
                    .map(|t| format!(" (\"{}\")", collapse(t)))
                    .unwrap_or_default();
                errors.push(format!("{at}{what}: no {}", missing.join(", no ")));
                continue;
            }
            if let (Some(page), Some(text)) = (page, text) {
                claims.push(Claim {
                    page,
                    text,
                    source,
                    proof,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(claims)
    } else {
        Err(errors)
    }
}

/// The anchors of a markdown file's headings, as GitHub writes them
/// (and the site: [`crate::html::anchor`]), duplicates numbered.
pub fn heading_anchors(markdown: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut counts = std::collections::HashMap::new();
    let mut in_heading: Option<(String, Option<String>)> = None;
    for ev in Parser::new_ext(markdown, pulldown_cmark::Options::ENABLE_HEADING_ATTRIBUTES) {
        match ev {
            Event::Start(Tag::Heading { id, .. }) => {
                in_heading = Some((String::new(), id.map(|i| i.to_string())));
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some((plain, _)) = &mut in_heading {
                    plain.push_str(&t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((plain, id)) = in_heading.take() {
                    let base = id.unwrap_or_else(|| crate::html::anchor(&plain));
                    let n = counts.entry(base.clone()).or_insert(0);
                    out.insert(if *n == 0 {
                        base.clone()
                    } else {
                        format!("{base}-{n}")
                    });
                    *n += 1;
                }
            }
            _ => {}
        }
    }
    out
}

/// A path that stays inside the repository, or why `source` names none.
fn bad_path(source: &str, path: &str) -> Option<String> {
    // An empty part is an empty path, an absolute one or a doubled `/`. Each
    // spelling is refused so one file has one path, which the own-page and
    // ledger check compares.
    path.split('/')
        .any(|p| matches!(p, "" | "." | ".."))
        .then(|| format!("`{source}`: a source is a path from the repository's root"))
}

/// Why `path "snippet"` points at nothing, if it does not. `quoted` is
/// what follows the opening quote, so a snippet may hold quotes of its own.
fn check_snippet(root: &Path, source: &str, path: &str, quoted: &str) -> Option<String> {
    if let Some(e) = bad_path(source, path) {
        return Some(e);
    }
    let Some(snippet) = quoted.strip_suffix('"') else {
        return Some(format!("`{source}`: a snippet ends with `\"`"));
    };
    let snippet = collapse(snippet);
    if snippet.chars().count() < MIN_SNIPPET {
        return Some(format!(
            "`{source}`: a snippet has at least {MIN_SNIPPET} characters"
        ));
    }
    let file = root.join(path);
    if !file.exists() {
        return Some(format!("`{source}`: {path} does not exist"));
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => return Some(format!("`{source}`: {path}: {e}")),
    };
    (!collapse(&text).contains(&snippet))
        .then(|| format!("`{source}`: {path} does not contain \"{snippet}\""))
}

/// The path `source` names, in any of its forms.
fn source_path(source: &str) -> &str {
    let path = source.split_once(" \"").map_or(source, |(p, _)| p);
    path.split_once('#').map_or(path, |(p, _)| p)
}

/// Why `source` (from the repository's root) points at nothing, if it
/// does not.
pub fn check_source(root: &Path, source: &str) -> Option<String> {
    // First, since a snippet may hold anything the other forms look for.
    if let Some((path, quoted)) = source.split_once(" \"") {
        return check_snippet(root, source, path, quoted);
    }
    let (path, anchor) = match source.split_once('#') {
        Some((p, a)) => (p, Some(a)),
        None => (source, None),
    };
    if anchor.is_none()
        && let Some((p, l)) = source.rsplit_once(':')
        && !l.is_empty()
        && l.chars().all(|c| c.is_ascii_digit() || c == '-')
    {
        return Some(format!(
            "`{source}`: a line number rots unseen; quote the line instead: `{p} \"snippet\"`"
        ));
    }
    if let Some(e) = bad_path(source, path) {
        return Some(e);
    }
    let file = root.join(path);
    if !file.exists() {
        return Some(format!("`{source}`: {path} does not exist"));
    }
    let anchor = anchor?;
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => return Some(format!("`{source}`: {path}: {e}")),
    };
    if !path.ends_with(".md") {
        return Some(format!(
            "`{source}`: an `#anchor` names a heading of a markdown file"
        ));
    }
    (!heading_anchors(&text).contains(anchor))
        .then(|| format!("`{source}`: {path} has no heading `#{anchor}`"))
}

/// Why `proof` names a test that is not there, when it names any: every
/// `path.rs::name` in it (`path.rs::tests::name` too) must be a function
/// in that file. A bare `file.rs` is next to the path named before it in
/// the same proof. Words around them (`; ran in the container`, `(the
/// file is written)`) are notes.
pub fn check_proof(root: &Path, proof: &str) -> Option<String> {
    let path_char = |c: char| c.is_ascii_alphanumeric() || "_./-".contains(c);
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut dir: Option<String> = None;
    let mut rest = proof;
    let mut at = 0;
    while let Some(i) = rest.find("::") {
        let before = &proof[..at + i];
        let start = before
            .char_indices()
            .rev()
            .find(|&(_, c)| !path_char(c))
            .map_or(0, |(j, c)| j + c.len_utf8());
        let path = &before[start..];
        let after = &rest[i + 2..];
        at += i + 2;
        rest = after;
        if !path.ends_with(".rs") {
            continue;
        }
        // The function: the last of `a::b::name`.
        let mut chain = after;
        let name = loop {
            let end = chain.find(|c| !ident_char(c)).unwrap_or(chain.len());
            match chain[end..].strip_prefix("::") {
                Some(more) if more.starts_with(ident_char) => chain = more,
                _ => break &chain[..end],
            }
        };
        let full = match (&dir, path.contains('/')) {
            (Some(d), false) => format!("{d}/{path}"),
            _ => path.to_string(),
        };
        dir = full.rsplit_once('/').map(|(d, _)| d.to_string());
        let text = match std::fs::read_to_string(root.join(&full)) {
            Ok(t) => t,
            Err(_) => return Some(format!("proof `{proof}`: {full} does not exist")),
        };
        let found = !name.is_empty()
            && text.lines().any(|l| {
                let l = l.trim_start();
                let l = l.strip_prefix("pub ").unwrap_or(l);
                let l = l.strip_prefix("async ").unwrap_or(l);
                l.strip_prefix("fn ")
                    .and_then(|r| r.strip_prefix(name))
                    .is_some_and(|r| r.starts_with('(') || r.starts_with('<'))
            });
        if !found {
            return Some(format!("proof `{proof}`: {full} has no `fn {name}`"));
        }
    }
    None
}

/// Every problem with the ledger at `root`: entries, pages, texts,
/// sources and proofs. No ledger is no claims.
pub fn check(root: &Path) -> Vec<String> {
    let path = root.join(LEDGER);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => return vec![format!("{LEDGER}: {e}")],
    };
    let claims = match parse(&text) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut errors = Vec::new();
    for c in &claims {
        let at = format!("{LEDGER}: \"{}\"", collapse(&c.text));
        if crate::nav::output_of(&c.page).is_none() {
            errors.push(format!(
                "{at}: page `{}` is not a page the site publishes",
                c.page
            ));
        }
        match std::fs::read_to_string(root.join(&c.page)) {
            Ok(md) => {
                // What the page shows: a repository-only span's words are
                // not the site's (a broken marker is the build's error).
                let md = crate::markdown::shown(&md).unwrap_or(md);
                if !collapse(&md).contains(&collapse(&c.text)) {
                    errors.push(format!("{at}: {} does not say it", c.page));
                }
            }
            Err(e) => errors.push(format!("{at}: {}: {e}", c.page)),
        }
        for s in &c.source {
            let path = source_path(s);
            if path == c.page || path == LEDGER {
                errors.push(format!(
                    "{at}: source `{s}`: a claim's own page and the ledger hold its words, \
                     so they prove nothing; cite what makes it true"
                ));
            } else if let Some(e) = check_source(root, s) {
                errors.push(format!("{at}: source {e}"));
            }
        }
        if let Some(e) = c.proof.as_deref().and_then(|p| check_proof(root, p)) {
            errors.push(format!("{at}: {e}"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository of its own: a page, a source file and a doc.
    fn repo(ledger: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "strand-site-claims-{}-{}",
            std::process::id(),
            blake3::hash(ledger.as_bytes()).to_hex()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("site/concepts")).unwrap();
        std::fs::create_dir_all(d.join("docs")).unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(
            d.join("site/concepts/motion.md"),
            "# Motion\n\nBelow a ratio of 1 it\novershoots, and it\n  settles.\n",
        )
        .unwrap();
        std::fs::write(
            d.join("src/anim.rs"),
            "a\nb\nc\n#[test]\nfn overshoots() {}\n",
        )
        .unwrap();
        std::fs::write(
            d.join("docs/design.md"),
            "# Design\n\n## Motion and time\n\n## Motion and time\n",
        )
        .unwrap();
        std::fs::write(d.join(LEDGER), ledger).unwrap();
        d
    }

    fn errors(ledger: &str) -> Vec<String> {
        let d = repo(ledger);
        let e = check(&d);
        let _ = std::fs::remove_dir_all(&d);
        e
    }

    const PAGE: &str = "page = \"site/concepts/motion.md\"\n";

    #[test]
    fn a_claim_on_its_page_with_real_sources_passes() {
        let e = errors(&format!(
            "[[claim]]\n{PAGE}text = \"Below a ratio of 1 it overshoots, and it settles.\"\n\
             source = [\"docs/design.md#motion-and-time-1\", \"src\"]\n\
             proof = \"src/anim.rs::overshoots\"\n\n\
             [[claim]]\n{PAGE}text = \"it settles\"\nsource = \"src/anim.rs\"\nproof = \"seen on the page\"\n"
        ));
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn a_source_that_holds_the_claims_own_words_fails() {
        for source in [
            "site/concepts/motion.md \\\"it settles too\\\"",
            "site/concepts/motion.md#motion",
            "site/concepts/motion.md",
            "site/claims.toml \\\"it settles too\\\"",
            "site/claims.toml",
        ] {
            let e = errors(&format!(
                "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"{source}\"]\n"
            ));
            assert_eq!(e.len(), 1, "{source}: {e:?}");
            assert!(e[0].contains("prove nothing"), "{source}: {e:?}");
        }
    }

    #[test]
    fn a_text_the_page_does_not_say_fails() {
        let e = errors(&format!(
            "[[claim]]\n{PAGE}text = \"Below a ratio of 2 it overshoots\"\nsource = [\"src/anim.rs\"]\n"
        ));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("does not say it"), "{e:?}");
    }

    #[test]
    fn a_source_that_points_at_nothing_fails() {
        for (source, why) in [
            ("src/gone.rs", "does not exist"),
            ("docs/design.md#motion", "no heading `#motion`"),
            ("docs/design.md#motion-and-time-2", "no heading"),
            ("src/anim.rs#x", "markdown file"),
            ("../etc/passwd", "from the repository's root"),
        ] {
            let e = errors(&format!(
                "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"{source}\"]\n"
            ));
            assert_eq!(e.len(), 1, "{source}: {e:?}");
            assert!(e[0].contains(why), "{source}: {e:?}");
        }
    }

    #[test]
    fn a_snippet_in_its_file_passes_however_it_is_written_or_wrapped() {
        let e = errors(&format!(
            "[[claim]]\n{PAGE}text = \"it settles\"\n\
             source = ['src/anim.rs \"fn overshoots\"']\n\n\
             [[claim]]\n{PAGE}text = \"it settles\"\n\
             source = \"src/anim.rs \\\"fn overshoots\\\"\"\n\n\
             [[claim]]\n{PAGE}text = \"it settles\"\n\
             source = 'src/anim.rs \"#[test]  fn overshoots\"'\n"
        ));
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn a_snippet_may_hold_what_the_other_forms_look_for() {
        let d = repo("# a snippet holding a form\n");
        std::fs::write(
            d.join("src/quoted.rs"),
            "let a = \"say \"hi\" at x.rs:12#top\";\n",
        )
        .unwrap();
        let there = check_source(&d, r#"src/quoted.rs ""hi" at x.rs:12#top""#);
        let gone = check_source(&d, r#"src/quoted.rs ""hi" at x.rs:13#top""#);
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(there, None);
        assert!(gone.is_some_and(|e| e.contains("does not contain")));
    }

    #[test]
    fn a_snippet_that_proves_nothing_fails_and_the_error_names_the_claim() {
        for (source, why) in [
            (
                "src/anim.rs \"fn undershoots\"",
                "does not contain \"fn undershoots\"",
            ),
            (
                "src/gone.rs \"fn overshoots\"",
                "src/gone.rs does not exist",
            ),
            ("src/anim.rs \"fn overshoots", "ends with"),
            ("src/anim.rs \"\"", "at least 8 characters"),
            ("src/anim.rs \"       \"", "at least 8 characters"),
            ("src/anim.rs \"fn over\"", "at least 8 characters"),
            ("src \"fn overshoots\"", "src: "),
            ("src/anim.rs:5 \"fn overshoots\"", "does not exist"),
            (
                "../etc/passwd \"fn overshoots\"",
                "from the repository's root",
            ),
            (" \"fn overshoots\"", "from the repository's root"),
        ] {
            let e = errors(&format!(
                "[[claim]]\n{PAGE}text = \"it settles\"\nsource = ['{source}']\n"
            ));
            assert_eq!(e.len(), 1, "{source}: {e:?}");
            assert!(e[0].contains(why), "{source}: {e:?}");
            assert!(e[0].contains("\"it settles\""), "{source}: {e:?}");
        }
    }

    #[test]
    fn a_path_spelt_any_other_way_fails() {
        for path in ["./src", "src/", "docs//design.md", "/src", "../src"] {
            for source in [path.to_string(), format!("{path} \\\"it overshoots\\\"")] {
                let e = errors(&format!(
                    "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"{source}\"]\n"
                ));
                assert!(
                    e.iter().any(|m| m.contains("the repository's root")),
                    "{source}: {e:?}"
                );
            }
        }
    }

    #[test]
    fn a_line_source_fails_whether_or_not_the_line_is_there() {
        for source in [
            "src/anim.rs:5",
            "src/anim.rs:1-5",
            "src/anim.rs:99",
            "src/gone.rs:5",
        ] {
            let e = errors(&format!(
                "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"{source}\"]\n"
            ));
            assert_eq!(e.len(), 1, "{source}: {e:?}");
            assert!(e[0].contains("quote the line"), "{source}: {e:?}");
            assert!(e[0].contains("\"it settles\""), "{source}: {e:?}");
        }
    }

    #[test]
    fn the_other_forms_may_hold_what_a_line_source_looks_like() {
        let d = repo("# forms beside a line source\n");
        std::fs::write(d.join("docs/a:1.md"), "# Top\n").unwrap();
        let quoted = check_source(&d, "src/anim.rs \"fn overshoots\"");
        let heading = check_source(&d, "docs/design.md#motion-and-time");
        let colon_in_path = check_source(&d, "docs/a:1.md#top");
        let path = check_source(&d, "src/anim.rs");
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(
            (quoted, heading, colon_in_path, path),
            (None, None, None, None)
        );
    }

    #[test]
    fn a_proof_naming_a_missing_test_fails() {
        let e = errors(&format!(
            "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"src/anim.rs\"]\nproof = \"src/anim.rs::undershoots\"\n"
        ));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("no `fn undershoots`"), "{e:?}");
    }

    #[test]
    fn every_test_a_proof_names_is_checked_and_notes_around_them_are_not() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let here = "crates/strand-site/src/claims.rs";
        let ok = format!(
            "{here}::tests::a_proof_naming_a_missing_test_fails (a note); \\
             claims.rs::whitespace_runs_count_as_one_space; ran in the container"
        );
        assert_eq!(check_proof(&root, &ok), None);
        let bad = format!("{here}::a_proof_naming_a_missing_test_fails; claims.rs::nowhere (note)");
        let e = check_proof(&root, &bad).unwrap();
        assert!(e.contains("claims.rs has no `fn nowhere`"), "{e}");
        assert_eq!(check_proof(&root, "seen on the page, 2026-10-26"), None);
    }

    #[test]
    fn an_entry_without_page_text_or_source_fails_and_so_does_an_unknown_key() {
        let e = errors("[[claim]]\ntext = \"it settles\"\nsources = [\"src/anim.rs\"]\n");
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e.iter().any(|m| m.contains("no key `sources`")), "{e:?}");
        assert!(e.iter().any(|m| m.contains("no page, no source")), "{e:?}");
        let e = errors(&format!("[[claim]]\n{PAGE}source = []\ntext = \"  \"\n"));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("no text, no source"), "{e:?}");
        let e = errors("[claims]\nx = 1\n");
        assert!(e[0].contains("`claims` is not part of the ledger"), "{e:?}");
        let e = errors("[[claim]\n");
        assert_eq!(e.len(), 1, "{e:?}");
    }

    #[test]
    fn a_page_the_site_does_not_publish_fails() {
        let e = errors(
            "[[claim]]\npage = \"docs/design.md\"\ntext = \"Motion and time\"\nsource = [\"src/anim.rs\"]\n",
        );
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("not a page the site publishes"), "{e:?}");
    }

    #[test]
    fn a_text_only_in_a_repo_only_span_is_not_on_the_page() {
        let d = repo(&format!(
            "[[claim]]\n{PAGE}text = \"it settles\"\nsource = \"src/anim.rs\"\n\n\
             [[claim]]\n{PAGE}text = \"CI holds it\"\nsource = \"src/anim.rs\"\n"
        ));
        std::fs::write(
            d.join("site/concepts/motion.md"),
            "# Motion\n\nit settles.\n\n<!-- repo-only -->\nCI holds it.\n<!-- /repo-only -->\n",
        )
        .unwrap();
        let e = check(&d);
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(
            e[0].contains("\"CI holds it\"") && e[0].contains("does not say it"),
            "{e:?}"
        );
    }

    #[test]
    fn whitespace_runs_count_as_one_space() {
        assert_eq!(collapse("  a\n  b\t\tc "), "a b c");
    }
}
