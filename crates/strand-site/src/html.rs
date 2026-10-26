//! Escaping, and relative URLs between output pages.

/// `s` as HTML text or an attribute value.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

/// The URL of output page `to` from output page `from` (both relative to
/// the site's root, `/`-separated): relative, so the site works from disk
/// (`file://`) as well as from a server.
pub fn relative(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = match from.rsplit_once('/') {
        Some((d, _)) => d.split('/').collect(),
        None => Vec::new(),
    };
    let to_parts: Vec<&str> = to.split('/').collect();
    let (to_dir, to_file) = to_parts.split_at(to_parts.len() - 1);
    let common = from_dir
        .iter()
        .zip(to_dir)
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    for d in &to_dir[common..] {
        out.push_str(d);
        out.push('/');
    }
    out.push_str(to_file[0]);
    out
}

/// The prefix from output page `from` to the site's root (`""`, `"../"`).
pub fn root(from: &str) -> String {
    "../".repeat(from.matches('/').count())
}

/// A heading's anchor as GitHub writes it (lower case, spaces as `-`,
/// punctuation dropped), which the guide's own links use.
pub fn anchor(heading: &str) -> String {
    strand_dev::reference::anchor_of(heading)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls_climb_and_descend() {
        assert_eq!(
            relative("index.html", "guide/level-1.html"),
            "guide/level-1.html"
        );
        assert_eq!(
            relative("guide/level-1.html", "guide/level-2.html"),
            "level-2.html"
        );
        assert_eq!(
            relative("guide/level-1.html", "reference/elements/slider.html"),
            "../reference/elements/slider.html"
        );
        assert_eq!(
            relative("reference/elements/slider.html", "index.html"),
            "../../index.html"
        );
        assert_eq!(root("reference/elements/slider.html"), "../../");
        assert_eq!(root("index.html"), "");
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(esc("a < b && \"c\""), "a &lt; b &amp;&amp; &quot;c&quot;");
    }
}
