//! The claims ledger (`site/claims.toml`): each factual claim a site page
//! makes about Strand, with where it is proven. The build fails when a
//! claim's `text` is not on its page (whitespace runs count as one
//! space, so a rewrapped line still matches), when a `source` does not
//! exist (`path`, `path:line`, `path:first-last` with the lines in the
//! file, or `path.md#anchor` with the heading there), when a `proof` that
//! names a test (`path.rs::name`) names no such function, and when an
//! entry lacks `page`, `text` or `source`, or has a key the format does
//! not know. Whether the source says what the text says is for the
//! people who check the site: the ledger only keeps a claim from
//! outliving the words it was written for or the code it points at.

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

/// Why `source` (from the repository's root) points at nothing, if it
/// does not.
pub fn check_source(root: &Path, source: &str) -> Option<String> {
    let (path, rest) = if let Some((p, a)) = source.split_once('#') {
        (p, Some(('#', a)))
    } else if let Some((p, l)) = source.rsplit_once(':')
        && !l.is_empty()
        && l.chars().all(|c| c.is_ascii_digit() || c == '-')
    {
        (p, Some((':', l)))
    } else {
        (source, None)
    };
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|p| p == "..") {
        return Some(format!(
            "`{source}`: a source is a path from the repository's root"
        ));
    }
    let file = root.join(path);
    if !file.exists() {
        return Some(format!("`{source}`: {path} does not exist"));
    }
    match rest {
        None => None,
        Some(('#', anchor)) => {
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
        Some((_, lines)) => {
            let text = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => return Some(format!("`{source}`: {path}: {e}")),
            };
            let count = text.lines().count();
            let (a, b) = lines.split_once('-').unwrap_or((lines, lines));
            let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) else {
                return Some(format!("`{source}`: `{lines}` is not a line or a range"));
            };
            (a == 0 || b < a || b > count)
                .then(|| format!("`{source}`: {path} has {count} lines, not line {lines}"))
        }
    }
}

/// Why `proof` names a test that is not there, when it names one
/// (`path.rs::name`, the test's function); any other proof is a note.
pub fn check_proof(root: &Path, proof: &str) -> Option<String> {
    let (path, name) = proof.split_once("::")?;
    if !path.ends_with(".rs") || path.contains(char::is_whitespace) {
        return None;
    }
    let name = name.rsplit("::").next().unwrap_or(name);
    let text = match std::fs::read_to_string(root.join(path)) {
        Ok(t) => t,
        Err(_) => return Some(format!("proof `{proof}`: {path} does not exist")),
    };
    let found = text.lines().any(|l| {
        let l = l.trim_start();
        let l = l.strip_prefix("pub ").unwrap_or(l);
        let l = l.strip_prefix("async ").unwrap_or(l);
        l.strip_prefix("fn ")
            .and_then(|r| r.strip_prefix(name))
            .is_some_and(|r| r.starts_with('(') || r.starts_with('<'))
    });
    (!found).then(|| format!("proof `{proof}`: {path} has no `fn {name}`"))
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
                if !collapse(&md).contains(&collapse(&c.text)) {
                    errors.push(format!("{at}: {} does not say it", c.page));
                }
            }
            Err(e) => errors.push(format!("{at}: {}: {e}", c.page)),
        }
        for s in &c.source {
            if let Some(e) = check_source(root, s) {
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
            "strand-site-claims-{}-{:x}",
            std::process::id(),
            blake3::hash(ledger.as_bytes()).as_bytes()[0]
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
             source = [\"src/anim.rs:5\", \"src/anim.rs:1-5\", \"docs/design.md#motion-and-time-1\", \"src\"]\n\
             proof = \"src/anim.rs::overshoots\"\n\n\
             [[claim]]\n{PAGE}text = \"it settles\"\nsource = \"src/anim.rs\"\nproof = \"seen on the page\"\n"
        ));
        assert!(e.is_empty(), "{e:?}");
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
            ("src/anim.rs:6", "has 5 lines"),
            ("src/anim.rs:0", "has 5 lines"),
            ("src/anim.rs:4-9", "has 5 lines"),
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
    fn a_proof_naming_a_missing_test_fails() {
        let e = errors(&format!(
            "[[claim]]\n{PAGE}text = \"it settles\"\nsource = [\"src/anim.rs\"]\nproof = \"src/anim.rs::undershoots\"\n"
        ));
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("no `fn undershoots`"), "{e:?}");
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
    fn whitespace_runs_count_as_one_space() {
        assert_eq!(collapse("  a\n  b\t\tc "), "a b c");
    }
}
