use super::*;

/// The hand-written fixtures: notification bodies (one per `-----`
/// block), calendar descriptions, a release note and trimmed feed
/// summaries.
pub(crate) const FIXTURES: &[(&str, &str)] = &[
    (
        "notification-bodies.txt",
        include_str!("../../tests/fixtures/markup/notification-bodies.txt"),
    ),
    (
        "calendar-google.html",
        include_str!("../../tests/fixtures/markup/calendar-google.html"),
    ),
    (
        "calendar-outlook.html",
        include_str!("../../tests/fixtures/markup/calendar-outlook.html"),
    ),
    (
        "release-note.html",
        include_str!("../../tests/fixtures/markup/release-note.html"),
    ),
    (
        "summary-quake.html",
        include_str!("../../tests/fixtures/markup/summary-quake.html"),
    ),
    (
        "summary-table.html",
        include_str!("../../tests/fixtures/markup/summary-table.html"),
    ),
    (
        "summary-social.html",
        include_str!("../../tests/fixtures/markup/summary-social.html"),
    ),
    (
        "summary-comic.html",
        include_str!("../../tests/fixtures/markup/summary-comic.html"),
    ),
    (
        "summary-blog.html",
        include_str!("../../tests/fixtures/markup/summary-blog.html"),
    ),
];

fn fixture(name: &str) -> &'static str {
    FIXTURES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
        .unwrap()
}

fn html(src: &str) -> String {
    parse(src, Level::Html, None).text
}

fn basic(src: &str) -> String {
    parse(src, Level::Basic, None).text
}

/// Each run's text and the styles named in `which` (b i u s m l).
fn runs(src: &str, level: Level) -> Vec<(String, String)> {
    let m = parse(src, level, None);
    m.runs
        .iter()
        .map(|r| {
            let mut f = String::new();
            for (on, c) in [
                (r.bold, 'b'),
                (r.italic, 'i'),
                (r.underline, 'u'),
                (r.strike, 's'),
                (r.mono, 'm'),
                (r.link, 'l'),
            ] {
                if on {
                    f.push(c);
                }
            }
            (m.text[r.range.clone()].to_string(), f)
        })
        .collect()
}

fn r(text: &str, flags: &str) -> (String, String) {
    (text.to_string(), flags.to_string())
}

// ---------------------------------------------------------------------
// Basic (moved from strand-render's markup.rs; same cases, the new
// run type).

#[test]
fn basic_is_unchanged() {
    // tags_become_spans
    let m = parse(
        "Hello <b>bold</b> and <i>it<u>al</u></i>!",
        Level::Basic,
        None,
    );
    assert_eq!(m.text, "Hello bold and ital!");
    assert_eq!(
        runs("Hello <b>bold</b> and <i>it<u>al</u></i>!", Level::Basic),
        vec![r("bold", "b"), r("it", "i"), r("al", "iu")]
    );
    let m = parse(
        "see <a href=\"https://x.org/?a=1&amp;b=2\">docs</a>",
        Level::Basic,
        None,
    );
    assert_eq!(m.text, "see docs");
    assert_eq!(m.runs[0].range, 4..8);
    assert!(m.runs[0].link);
    assert_eq!(m.links, ["https://x.org/?a=1&b=2"]);

    // nested_styles_combine
    assert_eq!(basic("<u>a<b>b</b></u>"), "ab");
    assert_eq!(
        runs("<u>a<b>b</b></u>", Level::Basic),
        vec![r("a", "u"), r("b", "bu")]
    );
    let m = parse("<a href=x>see <b>docs</b></a>", Level::Basic, None);
    assert_eq!(m.text, "see docs");
    assert_eq!(m.runs.len(), 2);
    assert!(m.runs.iter().all(|s| s.link));
    assert!(m.runs[1].bold);
    assert!(
        m.runs
            .windows(2)
            .all(|w| w[0].range.end <= w[1].range.start)
    );
    assert_eq!(m.links, ["x"]);

    // entities_unknown_tags_and_malformed_input
    assert_eq!(basic("a &lt;b&gt; &amp; c &nope;"), "a <b> & c &nope;");
    assert_eq!(basic("<span>x</span><br/>y"), "<span>x</span><br/>y");
    let m = parse("if a<b && c>d", Level::Basic, None);
    assert_eq!(m.text, "if a<b && c>d");
    assert!(m.runs.is_empty());
    assert_eq!(basic("Vec<T> <b >x</b >"), "Vec<T> x");
    assert_eq!(basic("a <b<i>c</i>"), "a <bc");
    assert_eq!(basic("1 < 2 and 3 > 2"), "1 < 2 and 3 > 2");
    assert_eq!(
        parse("open <b>never closed", Level::Basic, None).runs.len(),
        1
    );
    assert_eq!(basic("x <img src=\"a.png\" alt=\"[pic]\"/> y"), "x [pic] y");
    assert_eq!(basic("trailing <"), "trailing <");
    assert_eq!(basic("</b><i>a</b>b</i>"), "ab");

    // (doc-formats) The shared entity decoder: HTML's names now decode,
    // `&nbsp;` is a space, unknown ones stay literal.
    assert_eq!(basic("a&mdash;b&nbsp;c &#8217;"), "a—b c ’");
    assert_eq!(basic("&nope; &amp &copy=2"), "&nope; &amp &copy=2");
    // Basic keeps its newlines and unknown tags as written.
    assert_eq!(basic("a\n  <p>b</p>"), "a\n  <p>b</p>");
}

// ---------------------------------------------------------------------
// HTML.

#[test]
fn html_blocks_break_lines() {
    assert_eq!(html("<p>one</p><p>two</p>"), "one\n\ntwo");
    assert_eq!(html("a<br>b<br/>c<br />d"), "a\nb\nc\nd");
    assert_eq!(html("a<br><br>b"), "a\n\nb");
    assert_eq!(html("<div>a</div><div>b</div>c"), "a\nb\nc");
    assert_eq!(html("<h2>Title</h2>text"), "Title\n\ntext");
    assert_eq!(
        runs("<h2>Title</h2>text", Level::Html),
        vec![r("Title", "b")]
    );
    assert_eq!(
        html("before<blockquote>quoted</blockquote>after"),
        "before\n\nquoted\n\nafter"
    );
    assert_eq!(html("a<hr>b"), "a\n\nb");
    assert_eq!(html("<pre>x\n  y</pre>after"), "x\n  y\n\nafter");
    // Unclosed paragraphs still break; breaks never stack past one blank
    // line, and both ends are trimmed.
    assert_eq!(html("<p>a<p>b"), "a\n\nb");
    assert_eq!(
        html("<p></p><br><p>a</p><br><br><br><div></div><p>b</p><br>"),
        "a\n\nb"
    );
    assert_eq!(html("x<section>y</section>z"), "x\ny\nz");
    // A `pre` ending in a newline is not followed by extra blank lines.
    let note = html(fixture("release-note.html"));
    assert!(
        note.contains("strand run --check\n  ok\n\nFull changelog"),
        "{note}"
    );
}

#[test]
fn html_lists_are_marked() {
    assert_eq!(html("<ul><li>a</li><li>b</li></ul>"), "• a\n• b");
    assert_eq!(
        html("intro<ol start=\"3\"><li>c<li>d<li value=\"9\">i<li>j</ol>end"),
        "intro\n\n3. c\n4. d\n9. i\n10. j\n\nend"
    );
    assert_eq!(
        html("<ul><li>a<ul><li>b<ol><li>c</li></ol></li></ul></li><li>d</li></ul>"),
        "• a\n  • b\n    1. c\n• d"
    );
    // Nesting indents at most 8 spaces.
    let deep = "<ul><li>x".repeat(12);
    let last = html(&deep).lines().last().unwrap().to_string();
    assert_eq!(last, "        • x");
    // Numbers keep at most 20 digits; more is no number.
    assert_eq!(
        html("<ol start=\"-5\"><li>a</ol><ol start=\"123456789012345678901\"><li>b</ol>"),
        "-5. a\n\n1. b"
    );
    // A description list: terms bold, descriptions indented.
    let quake = fixture("summary-quake.html");
    assert_eq!(
        html(quake),
        "Time\n  2026-11-07 19:55:10 UTC\n  2026-11-07 13:55:10 -06:00 at epicenter\n\
         Depth\n  10.00 km (6.21 mi)\nTemperature\n  12°C"
    );
    assert_eq!(
        runs(quake, Level::Html),
        vec![r("Time", "b"), r("Depth", "b"), r("Temperature", "b")]
    );
    // An unclosed `dt` ends at its description.
    assert_eq!(runs("<dl><dt>t<dd>d</dl>", Level::Html), vec![r("t", "b")]);
}

#[test]
fn html_tables_are_rows() {
    let t = fixture("summary-table.html");
    assert_eq!(
        html(t),
        "Version: · 6.18-rc4\nReleased: · 2026-11-01\nSource: · linux-6.18-rc4.tar.gz"
    );
    assert_eq!(
        runs(t, Level::Html),
        vec![
            r("Version:", "b"),
            r("6.18-rc4", "b"),
            r("Released:", "b"),
            r("Source:", "b"),
            r("linux-6.18-rc4.tar.gz", "l"),
        ]
    );
    // Unclosed cells and a caption; an empty cell adds no separator.
    assert_eq!(
        html("<table><caption>Cap</caption><tr><td>a<td><td>b<tr><td>c</table>x"),
        "Cap\na · b\nc\n\nx"
    );
}

#[test]
fn html_styles_become_runs() {
    let src = "<b>b</b> <strong>s</strong> <em>e</em> <i>i</i> <u>u</u> <s>x</s> \
               <del>d</del> <code>c</code> <kbd>k</kbd> <a href=\"https://a.org/\">l<b>lb</b></a> \
               <a name=\"anchor\">n</a> <a href='/rel?a=1&amp;b=2'>r</a>";
    let m = parse(src, Level::Html, None);
    assert_eq!(m.text, "b s e i u x d c k llb n r");
    assert_eq!(
        runs(src, Level::Html),
        vec![
            r("b", "b"),
            r("s", "b"),
            r("e", "i"),
            r("i", "i"),
            r("u", "u"),
            r("x", "s"),
            r("d", "s"),
            r("c", "m"),
            r("k", "m"),
            r("l", "l"),
            r("lb", "bl"),
            r("r", "l"),
        ]
    );
    // Links as written, in order; an anchor without href is no link.
    assert_eq!(m.links, ["https://a.org/", "/rel?a=1&b=2"]);
    // Runs never overlap, and spaces next to a run stay outside it.
    assert!(
        m.runs
            .windows(2)
            .all(|w| w[0].range.end <= w[1].range.start)
    );
    assert_eq!(
        runs("see <a href=x> docs </a> now", Level::Html),
        vec![r("docs", "l")]
    );
    // A close pops to its open tag; anything still open closes at the end.
    assert_eq!(
        runs("<b>a<i>b</b>c</i><u>d", Level::Html),
        vec![r("a", "b"), r("b", "bi"), r("d", "u")]
    );
    assert_eq!(html("<b>a<i>b</b>c</i><u>d"), "abcd");
    // Styles nest at most 32 deep; deeper tags are ignored, text kept.
    let deep = format!("{}x", "<b>".repeat(100));
    assert_eq!(html(&deep), "x");
    assert_eq!(runs(&deep, Level::Html), vec![r("x", "b")]);
    // The release note: bold, mono, struck and link runs.
    let note = runs(fixture("release-note.html"), Level::Html);
    for want in [
        r("New features", "b"),
        r("pinned", "b"),
        r("pin", "m"),
        r("#101", "l"),
        r("old", "s"),
        r("v1...v2", "ml"),
    ] {
        assert!(note.contains(&want), "{want:?} in {note:?}");
    }
}

#[test]
fn html_drops_unknown_tags_and_hidden_content() {
    // An Outlook-style whole document: head, style, script, `o:p`,
    // conditional comments and downlevel-revealed ones.
    let outlook = parse(fixture("calendar-outlook.html"), Level::Html, None);
    assert_eq!(
        outlook.text,
        "Hi all,\n\nVideo meeting\n\nJoin from your computer\nClick here to join\n\nNotes"
    );
    assert_eq!(
        outlook.links,
        ["https://meet.example.com/join/19%3ameeting_x"]
    );
    // A social post: spans dropped, text kept (no CSS: hidden spans show).
    assert_eq!(
        html(fixture("summary-social.html")),
        "Trying the new release today — looks good! #linux\n\nhttps://example.org/post/1"
    );
    // noscript, figure and an empty alt.
    assert_eq!(
        html(fixture("summary-blog.html")),
        "The new look\n\nWe’ve shipped a new look for the site… finally.\n\n\
         Simple is better.\n\nRead more on the blog."
    );
    // A comic whose summary is one image: its alt.
    assert_eq!(html(fixture("summary-comic.html")), "Sky");
    // Every dropped element, any case, and an unclosed one to the end.
    for tag in [
        "script", "style", "head", "title", "template", "noscript", "svg", "math", "iframe",
        "object", "video", "audio", "canvas", "select",
    ] {
        let up = tag.to_ascii_uppercase();
        assert_eq!(
            html(&format!("a<{tag} x=\"1\">hidden <b>x</b></{up}>b")),
            "ab",
            "{tag}"
        );
        assert_eq!(html(&format!("a<{tag}>hidden")), "a", "{tag}");
    }
    assert_eq!(
        html("<script>if (a</b) { x = \"</scriptx>\" }</script >shown"),
        "shown"
    );
    assert_eq!(
        html("<x-widget>a</x-widget><font color=red>b</font><o:p>c</o:p><span>d</span>"),
        "abcd"
    );
    assert_eq!(
        html("a<!-- c --> b<!---->c<!-->d<!doctype html><?xml version=\"1.0\"?>e"),
        "a bcde"
    );
    assert_eq!(html("x<![CDATA[<b>&amp;</b>]]>y"), "x<b>&amp;</b>y");
}

#[test]
fn html_whitespace_collapses_except_pre() {
    assert_eq!(html("  a \n\t b  \r\n c  "), "a b c");
    assert_eq!(html("<p>\n  indented\n  source\n</p>"), "indented source");
    assert_eq!(html("a <b> b </b> c"), "a b c");
    assert_eq!(
        html("<pre>\n  keep   this\n\tand this\n</pre>"),
        "  keep   this\n\tand this"
    );
    assert_eq!(html("<pre>a\r\nb</pre>"), "a\nb");
    // `&nbsp;` never collapses, but goes at a break like any space.
    assert_eq!(html("a&nbsp;&nbsp;b &nbsp; c"), "a  b   c");
    assert_eq!(html("<p>&nbsp;</p><p>x&nbsp;</p>&nbsp;"), "x");
    // Spaces beside breaks go.
    assert_eq!(html("a <br> b"), "a\nb");
    assert_eq!(html("<li> a </li>"), "• a");
}

#[test]
fn entities_named_and_numeric() {
    for (src, want) in [
        ("&nbsp;", " "),
        ("&deg;", "°"),
        ("&mdash;", "—"),
        ("&Oslash;", "Ø"),
        ("&amp;&lt;&gt;&quot;&apos;", "&<>\"'"),
        ("&NotEqualTilde;", "≂\u{338}"),
        ("&#8217;", "’"),
        ("&#8230;", "…"),
        ("&#x2014;", "—"),
        ("&#X2014;", "—"),
        ("&#150;", "–"),
        ("&#x80;", "€"),
        ("&#x81;", "\u{81}"),
        ("&#x1F600;", "😀"),
        ("&#xD800;", "\u{FFFD}"),
        ("&#x110000;", "\u{FFFD}"),
        ("&#0;", "\u{FFFD}"),
        ("&#00000065;", "A"),
        ("&#;", "&#;"),
        ("&#x;", "&#x;"),
        ("&#000000065;", "&#000000065;"),
        ("&#65", "&#65"),
        ("&amp", "&amp"),
        ("&copy 2026", "&copy 2026"),
        ("?a=1&copy=2", "?a=1&copy=2"),
        ("&nope;", "&nope;"),
        ("&AMP;", "&"),
        ("&", "&"),
        ("&;", "&;"),
        ("AT&T", "AT&T"),
        ("&CounterClockwiseContourIntegral;", "∳"),
        (
            "&CounterClockwiseContourIntegralX;",
            "&CounterClockwiseContourIntegralX;",
        ),
    ] {
        // (Bracketed, as html trims a lone space.)
        let (src, want) = (format!("[{src}]"), format!("[{want}]"));
        assert_eq!(html(&src), want, "html {src}");
        // Basic decodes the same (its `&nbsp;` too).
        assert_eq!(basic(&src), want, "basic {src}");
    }
    // Attribute values decode too.
    assert_eq!(
        parse("<a href=\"/q?a=1&amp;b=&#50;\">x</a>", Level::Html, None).links,
        ["/q?a=1&b=2"]
    );
}

#[test]
fn entity_table_is_sorted_and_complete() {
    let n = entities::ENDS.len();
    assert_eq!(entities::CODES.len(), n);
    let name = |i: usize| {
        let s = if i == 0 {
            0
        } else {
            entities::ENDS[i - 1] as usize
        };
        &entities::NAMES[s..entities::ENDS[i] as usize]
    };
    for i in 1..n {
        assert!(name(i - 1).as_bytes() < name(i).as_bytes(), "{}", name(i));
    }
    assert_eq!(entities::ENDS[n - 1] as usize, entities::NAMES.len());
    for (k, c) in [
        ("amp", '&'),
        ("nbsp", '\u{A0}'),
        ("deg", '°'),
        ("zwj", '\u{200D}'),
    ] {
        assert_eq!(named(k), Some((c, None)), "{k}");
    }
    // WHATWG lists 2125 names ending in `;` (and 106 legacy forms
    // without, which are not decoded).
    assert_eq!(n, 2125);
    assert!((0..n).all(|i| name(i).len() <= entities::LONGEST));
    assert!(
        entities::CODES
            .iter()
            .all(|c| char::from_u32(c & 0x1F_FFFF).is_some())
    );
}

#[test]
fn malformed_html_never_fails_and_stays_linear() {
    let mut inputs: Vec<String> = vec![
        "a<script>never closed <b>x".into(),
        "a<!-- never closed".into(),
        "a<p class=\"never closed>b</p>c".into(),
        "<a href=\"https://x".into(),
        "<".repeat(1 << 20),
        "<b>".repeat(100_000),
        "<p>".repeat(100_000),
        "<a x='".repeat(50_000),
        "<a x=\"1\" ".repeat(50_000),
        "<x a='>\n".repeat(50_000),
        "</".repeat(100_000),
        "<!".repeat(100_000),
        "<![CDATA[".repeat(50_000),
        format!("<script>{}", "</s".repeat(100_000)),
        "&CounterClockwiseContourIntegra".repeat(30_000),
        "&#99999999".repeat(50_000),
        "<li value=99999999999999999999>".repeat(10_000),
        "<ol start=-99999999999999999999><li>".repeat(10_000),
        "<pre>\n".repeat(50_000),
        "<td>".repeat(100_000),
        "<dd>x".repeat(100_000),
    ];
    // A 300 KB fragment.
    inputs.push(fixture("release-note.html").repeat(300_000 / 551 + 1));
    for src in &inputs {
        for level in [Level::Basic, Level::Html] {
            let (m, steps) = parse_counted(src, level, None);
            assert!(
                steps <= 8 * src.len() + 64,
                "{level:?}: {steps} steps for {} bytes of {:?}…",
                src.len(),
                &src[..src.len().min(24)]
            );
            // The output stays near the input's size.
            assert!(m.text.len() <= 4 * src.len() + 16, "{level:?}");
            assert!(m.runs.iter().all(|r| r.range.end <= m.text.len()
                && m.text.is_char_boundary(r.range.start)
                && m.text.is_char_boundary(r.range.end)));
        }
    }
    // Every prefix of every fixture.
    for (name, src) in FIXTURES {
        for end in (0..=src.len()).filter(|e| src.is_char_boundary(*e)) {
            for level in [Level::Basic, Level::Html] {
                let m = parse(&src[..end], level, None);
                assert!(
                    m.runs.iter().all(|r| r.range.end <= m.text.len()),
                    "{name}[..{end}]"
                );
            }
        }
    }
}

#[test]
fn limit_stops_the_parse() {
    let src = "<p><b>bold words</b> and more text</p>".repeat(1000);
    for level in [Level::Basic, Level::Html] {
        let whole = parse(&src, level, None);
        let cut = parse(&src, level, Some(25));
        assert_eq!(cut.text.len(), 25, "{level:?}");
        assert!(whole.text.starts_with(&cut.text));
        assert!(cut.runs.iter().all(|r| r.range.end <= 25));
        assert!(!cut.runs.is_empty());
    }
    // Cut at a character boundary.
    let m = parse("ééé", Level::Html, Some(3));
    assert_eq!(m.text, "é");
    // The parse stops: a long text's limit bounds the work, not just the
    // output.
    let (_, steps) = parse_counted(&"<i>x</i> ".repeat(100_000), Level::Html, Some(100));
    assert!(steps < 2_000, "{steps}");
}
