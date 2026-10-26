//! `site/SUMMARY.md`: the sidebar, in order. Each `## Section` holds
//! `- [Label](path)` entries, a path relative to `site/` (`../docs/guide/
//! level-1.md` reaches the guide where it lives) or a generated set
//! (`@reference/elements`).

/// One sidebar entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub label: String,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A markdown page, by its path from the repository's root.
    Page(String),
    /// A generated set of pages (`reference/elements`).
    Generated(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub title: String,
    pub entries: Vec<Entry>,
}

/// The sections of `text` (a SUMMARY.md, under `site/`).
pub fn parse(text: &str) -> Result<Vec<Section>, String> {
    let mut sections: Vec<Section> = Vec::new();
    let mut comment = false;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if comment || line.starts_with("<!--") {
            comment = !line.contains("-->");
            continue;
        }
        let at = || format!("site/SUMMARY.md:{}", n + 1);
        if let Some(title) = line.strip_prefix("## ") {
            sections.push(Section {
                title: title.trim().to_string(),
                entries: Vec::new(),
            });
        } else if let Some(item) = line.strip_prefix("- ") {
            let (label, path) = item
                .strip_prefix('[')
                .and_then(|r| r.split_once("]("))
                .and_then(|(l, r)| Some((l, r.strip_suffix(')')?)))
                .ok_or_else(|| format!("{}: expected `- [Label](path)`", at()))?;
            let target = match path.strip_prefix('@') {
                Some(set) => Target::Generated(set.to_string()),
                None => Target::Page(normalise(&format!("site/{path}"))?),
            };
            sections
                .last_mut()
                .ok_or_else(|| format!("{}: an entry before any `## Section`", at()))?
                .entries
                .push(Entry {
                    label: label.to_string(),
                    target,
                });
        } else if !(line.is_empty() || line.starts_with("# ")) {
            return Err(format!("{}: not a section or an entry: {line}", at()));
        }
    }
    Ok(sections)
}

/// `path` with `.` and `..` resolved (`/`-separated, relative to the
/// repository's root); an error when it climbs out of it.
pub fn normalise(path: &str) -> Result<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| format!("`{path}` leaves the repository"))?;
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

/// Where a page from `source` (from the repository's root) is written in
/// the site, or `None` for a source the site does not publish.
pub fn output_of(source: &str) -> Option<String> {
    let (dir, rest) = if let Some(r) = source.strip_prefix("site/") {
        ("", r)
    } else {
        ("guide/", source.strip_prefix("docs/guide/")?)
    };
    let stem = rest.strip_suffix(".md")?;
    let stem = match stem.rsplit_once('/') {
        Some((d, "README" | "index")) => format!("{d}/index"),
        None if stem == "README" || stem == "index" => "index".to_string(),
        _ => stem.to_string(),
    };
    Some(format!("{dir}{stem}.html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_and_entries() {
        let s = parse(
            "# Strand\n\n## Guide\n- [The guide](../docs/guide/README.md)\n\
             - [Level 1](../docs/guide/level-1.md)\n## Reference\n- [Elements](@reference/elements)\n",
        )
        .unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0].entries[1].target,
            Target::Page("docs/guide/level-1.md".into())
        );
        assert_eq!(
            s[1].entries[0].target,
            Target::Generated("reference/elements".into())
        );
    }

    #[test]
    fn outputs() {
        assert_eq!(
            output_of("docs/guide/README.md").as_deref(),
            Some("guide/index.html")
        );
        assert_eq!(
            output_of("docs/guide/level-1.md").as_deref(),
            Some("guide/level-1.html")
        );
        assert_eq!(
            output_of("site/concepts/motion.md").as_deref(),
            Some("concepts/motion.html")
        );
        assert_eq!(output_of("docs/design.md"), None);
        assert!(normalise("site/../../x").is_err());
    }
}
