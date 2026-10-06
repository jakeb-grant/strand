//! `markup: basic`: the freedesktop notification markup (`<b>`, `<i>`,
//! `<u>`, `<a href="…">`, `<img>` and the XML entities) as plain text plus
//! [`TextSpan`]s. Unknown tags are dropped and their text kept; a stray `<`
//! that opens no tag is kept as text, so malformed bodies still show.

use strand_scene::Color;
use strand_text::TextSpan;

/// Most nested tags followed; deeper ones count as text.
const MAX_DEPTH: usize = 32;

/// The plain text of `src` and the spans its tags make. Links are
/// underlined and painted `link` (when given).
pub fn parse(src: &str, link: Option<Color>) -> (String, Vec<TextSpan>) {
    let mut out = String::with_capacity(src.len());
    let mut spans = Vec::new();
    // Open tags: name and where their text starts.
    let mut open: Vec<(String, usize)> = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(['<', '&']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('&') {
            let (ch, used) = entity(rest);
            out.push_str(ch);
            rest = &rest[used..];
            continue;
        }
        let Some(end) = rest.find('>') else {
            out.push_str(rest);
            rest = "";
            break;
        };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .trim()
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let starts_ok = tag
            .trim_start_matches('/')
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        if !starts_ok || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            // Not a tag: keep it as text.
            out.push('<');
            out.push_str(tag);
            out.push('>');
            continue;
        }
        if closing {
            if let Some(pos) = open.iter().rposition(|(n, _)| *n == name) {
                // Close it and anything left open inside it.
                for (n, start) in open.drain(pos..).rev() {
                    if let Some(sp) = span_for(&n, start..out.len(), link) {
                        spans.push(sp);
                    }
                }
            }
        } else if name == "img" {
            // Notification images are not drawn inline; the alt text is.
            if let Some(alt) = attr(tag, "alt") {
                out.push_str(&alt);
            }
        } else if !tag.trim_end().ends_with('/') && open.len() < MAX_DEPTH {
            open.push((name, out.len()));
        }
    }
    out.push_str(rest);
    for (n, start) in open.into_iter().rev() {
        if let Some(sp) = span_for(&n, start..out.len(), link) {
            spans.push(sp);
        }
    }
    // Spans in text order; nested ones after their parents, so they win.
    spans.sort_by_key(|s| (s.range.start, std::cmp::Reverse(s.range.end)));
    (out, spans)
}

fn span_for(name: &str, range: std::ops::Range<usize>, link: Option<Color>) -> Option<TextSpan> {
    if range.is_empty() {
        return None;
    }
    let mut sp = TextSpan {
        range,
        ..TextSpan::default()
    };
    match name {
        "b" => sp.weight = Some(700),
        "i" => sp.italic = true,
        "u" => sp.underline = true,
        "a" => {
            sp.underline = true;
            sp.color = link;
        }
        _ => return None,
    }
    Some(sp)
}

/// The value of attribute `key` in a tag's text.
fn attr(tag: &str, key: &str) -> Option<String> {
    let i = tag.find(&format!("{key}="))?;
    let v = &tag[i + key.len() + 1..];
    let q = v.chars().next()?;
    if q == '"' || q == '\'' {
        let v = &v[1..];
        let end = v.find(q)?;
        Some(entities(&v[..end]))
    } else {
        Some(v.split_whitespace().next()?.to_string())
    }
}

fn entities(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let (ch, used) = entity(&rest[i..]);
        out.push_str(ch);
        rest = &rest[i + used..];
    }
    out.push_str(rest);
    out
}

/// The text of the entity at the start of `s` and the bytes it used; an
/// unknown one is a literal `&`.
fn entity(s: &str) -> (&'static str, usize) {
    for (name, ch) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
    ] {
        if s.starts_with(name) {
            return (ch, name.len());
        }
    }
    ("&", 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Styled = (String, bool, bool, Option<u16>);

    fn ranges(src: &str) -> (String, Vec<Styled>) {
        let (t, sp) = parse(src, None);
        let v = sp
            .iter()
            .map(|s| {
                (
                    t[s.range.clone()].to_string(),
                    s.italic,
                    s.underline,
                    s.weight,
                )
            })
            .collect();
        (t, v)
    }

    #[test]
    fn tags_become_spans() {
        let (t, v) = ranges("Hello <b>bold</b> and <i>it<u>al</u></i>!");
        assert_eq!(t, "Hello bold and ital!");
        assert_eq!(
            v,
            vec![
                ("bold".into(), false, false, Some(700)),
                ("ital".into(), true, false, None),
                ("al".into(), false, true, None),
            ]
        );
        let accent = Color::WHITE;
        let (t, sp) = parse(
            "see <a href=\"https://x.org/?a=1&amp;b=2\">docs</a>",
            Some(accent),
        );
        assert_eq!(t, "see docs");
        assert_eq!(sp[0].range, 4..8);
        assert!(sp[0].underline && sp[0].color == Some(accent));
    }

    #[test]
    fn entities_unknown_tags_and_malformed_input() {
        assert_eq!(
            parse("a &lt;b&gt; &amp; c &nope;", None).0,
            "a <b> & c &nope;"
        );
        assert_eq!(parse("<span>x</span><br/>y", None).0, "xy");
        assert_eq!(parse("1 < 2 and 3 > 2", None).0, "1 < 2 and 3 > 2");
        assert_eq!(parse("open <b>never closed", None).1.len(), 1);
        assert_eq!(
            parse("x <img src=\"a.png\" alt=\"[pic]\"/> y", None).0,
            "x [pic] y"
        );
        assert_eq!(parse("trailing <", None).0, "trailing <");
        // Mismatched closes do not panic and keep the text.
        assert_eq!(parse("</b><i>a</b>b</i>", None).0, "ab");
    }
}
