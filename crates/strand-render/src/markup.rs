//! `markup: basic`: the freedesktop notification markup (`<b>`, `<i>`,
//! `<u>`, `<a href="…">`, `<img>` and the XML entities) as plain text plus
//! [`TextSpan`]s. Only those tags, written well-formed, are tags: anything
//! else that looks like one (`a<b && c>d`, `Vec<T>`, `<span>`) is kept as
//! text, so code and malformed bodies still show. Nested tags combine
//! (`<u>a <b>b</b></u>` underlines both): the spans are non-overlapping
//! runs carrying every style in force.

use strand_scene::Color;
use strand_text::TextSpan;

/// Most nested tags followed; deeper ones count as text.
const MAX_DEPTH: usize = 32;

/// The tags `markup: basic` knows.
const TAGS: [&str; 5] = ["b", "i", "u", "a", "img"];

/// The styles the open tags put on text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    link: bool,
}

fn style_of(open: &[&str]) -> Style {
    let mut st = Style::default();
    for t in open {
        match *t {
            "b" => st.bold = true,
            "i" => st.italic = true,
            "u" => st.underline = true,
            "a" => st.link = true,
            _ => {}
        }
    }
    st
}

/// A well-formed tag of a known name: `(closing, name)`.
fn tag_of(tag: &str) -> Option<(bool, &'static str)> {
    let (closing, body) = match tag.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, tag),
    };
    let body = body.trim_end();
    let body = if closing {
        body
    } else {
        body.strip_suffix('/').unwrap_or(body)
    };
    let len = body
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(body.len());
    let name = TAGS.iter().find(|t| t.eq_ignore_ascii_case(&body[..len]))?;
    let mut rest = &body[len..];
    if closing {
        return rest.trim().is_empty().then_some((true, name));
    }
    // Attributes: `key="v"`, `key='v'`, `key=v` or a bare `key`, each
    // after whitespace.
    while !rest.is_empty() {
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            return None;
        }
        rest = trimmed;
        if rest.is_empty() {
            break;
        }
        let k = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':'))
            .unwrap_or(rest.len());
        if k == 0 {
            return None;
        }
        rest = &rest[k..];
        if let Some(v) = rest.strip_prefix('=') {
            match v.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let close = v[1..].find(q)?;
                    rest = &v[close + 2..];
                }
                Some(c) if !c.is_whitespace() => {
                    let e = v
                        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                        .unwrap_or(v.len());
                    if v[e..].starts_with(['"', '\'']) {
                        return None;
                    }
                    rest = &v[e..];
                }
                _ => return None,
            }
        }
    }
    Some((false, name))
}

/// The plain text of `src` and the spans its tags make. Links are
/// underlined and painted `link` (when given).
pub fn parse(src: &str, link: Option<Color>) -> (String, Vec<TextSpan>) {
    let mut out = String::with_capacity(src.len());
    let mut spans = Vec::new();
    // Open tags, outermost first, and where the run in their style began.
    let mut open: Vec<&'static str> = Vec::new();
    let mut run = 0;
    let flush = |out: &String, open: &[&str], run: &mut usize, spans: &mut Vec<TextSpan>| {
        let st = style_of(open);
        if out.len() > *run && st != Style::default() {
            spans.push(TextSpan {
                range: *run..out.len(),
                weight: st.bold.then_some(700),
                italic: st.italic,
                underline: st.underline || st.link,
                color: if st.link { link } else { None },
            });
        }
        *run = out.len();
    };
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
        // A tag ends at the first `>`, with no `<` before it.
        let end = rest[1..].find(['<', '>']).map(|e| e + 1);
        let parsed = end
            .filter(|e| rest[*e..].starts_with('>'))
            .and_then(|e| Some((e, tag_of(&rest[1..e])?)));
        let Some((end, (closing, name))) = parsed else {
            // Not a tag: the `<` is text, and scanning goes on after it.
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if closing {
            if let Some(pos) = open.iter().rposition(|n| *n == name) {
                // Close it and anything left open inside it.
                flush(&out, &open, &mut run, &mut spans);
                open.truncate(pos);
            }
        } else if name == "img" {
            // Notification images are not drawn inline; the alt text is.
            if let Some(alt) = attr(tag, "alt") {
                out.push_str(&alt);
            }
        } else if !tag.trim_end().ends_with('/') && open.len() < MAX_DEPTH {
            flush(&out, &open, &mut run, &mut spans);
            open.push(name);
        }
    }
    out.push_str(rest);
    flush(&out, &open, &mut run, &mut spans);
    (out, spans)
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
                ("it".into(), true, false, None),
                ("al".into(), true, true, None),
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

    /// Nested tags combine: every run under `<u>` is underlined, and a
    /// bold word in a link keeps the link's colour and underline.
    #[test]
    fn nested_styles_combine() {
        let (t, v) = ranges("<u>a<b>b</b></u>");
        assert_eq!(t, "ab");
        assert_eq!(
            v,
            vec![
                ("a".into(), false, true, None),
                ("b".into(), false, true, Some(700)),
            ]
        );
        let accent = Color::WHITE;
        let (t, sp) = parse("<a href=x>see <b>docs</b></a>", Some(accent));
        assert_eq!(t, "see docs");
        assert_eq!(sp.len(), 2);
        assert!(sp.iter().all(|s| s.underline && s.color == Some(accent)));
        assert_eq!(sp[1].weight, Some(700));
        // Runs never overlap.
        assert!(sp.windows(2).all(|w| w[0].range.end <= w[1].range.start));
    }

    #[test]
    fn entities_unknown_tags_and_malformed_input() {
        assert_eq!(
            parse("a &lt;b&gt; &amp; c &nope;", None).0,
            "a <b> & c &nope;"
        );
        // Only the known tags, well-formed, are tags.
        assert_eq!(
            parse("<span>x</span><br/>y", None).0,
            "<span>x</span><br/>y"
        );
        let (t, sp) = parse("if a<b && c>d", None);
        assert_eq!(t, "if a<b && c>d");
        assert!(sp.is_empty());
        assert_eq!(parse("Vec<T> <b >x</b >", None).0, "Vec<T> x");
        assert_eq!(parse("a <b<i>c</i>", None).0, "a <bc");
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
