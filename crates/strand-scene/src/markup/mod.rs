//! (doc-formats) The text a text node's `markup:` shows, and the styles
//! on it, for both levels. One implementation serves the renderer (which
//! maps [`Run`]s to its text spans) and logic's `plain(markup)` and
//! `links(markup)` (decisions.md, doc-formats): strand-scene is the crate
//! both depend on, and it builds to wasm, as [`crate::keys`] does.
//!
//! - [`Level::Basic`]: the freedesktop notification markup (`<b>`, `<i>`,
//!   `<u>`, `<a href="…">`, `<img alt>`). Only those tags, written
//!   well-formed, are tags: anything else that looks like one (`a<b && c>d`,
//!   `Vec<T>`, `<span>`) is kept as text, so code and malformed bodies still
//!   show.
//! - [`Level::Html`]: an HTML fragment (feed summaries, release notes,
//!   calendar descriptions, mail-style bodies), read leniently in one
//!   linear pass with no tree and no CSS: blocks become line breaks, lists
//!   are marked, a few inline elements keep their style, script, style and
//!   other hidden content is dropped, and every other tag is dropped with
//!   its text kept.
//!
//! Both decode every HTML named character reference ending in `;` and the
//! numeric ones; `&nbsp;` is a plain space. Nested styles combine: the runs
//! are non-overlapping and carry every style in force. Malformed input
//! never fails, and the work is linear in the input.

use std::ops::Range;

#[rustfmt::skip]
mod entities;

/// Which markup a text is read as (the `markup:` property's `basic` and
/// `html`; `none` never parses).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    Basic,
    Html,
}

/// A styled byte range of [`Marked::text`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub range: Range<usize>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Code: shown in a monospace face.
    pub mono: bool,
    /// Inside a link (`<a href>`): the renderer underlines it in `$accent`.
    pub link: bool,
}

/// What markup shows: the text, its styled runs (ordered, never
/// overlapping) and the links' `href`s in order, as written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marked {
    pub text: String,
    pub runs: Vec<Run>,
    pub links: Vec<String>,
}

/// Most nested styles followed; deeper tags are ignored and their text
/// kept.
const MAX_DEPTH: usize = 32;

/// Most nested lists indented.
const MAX_INDENT: usize = 8;

/// `src` read as `level`. With `limit`, parsing stops once that many bytes
/// are shown, and the text is cut there (at a character boundary).
pub fn parse(src: &str, level: Level, limit: Option<usize>) -> Marked {
    parse_counted(src, level, limit).0
}

/// [`parse`] and the bytes its scans looked at (the linearity tests' step
/// counter).
fn parse_counted(src: &str, level: Level, limit: Option<usize>) -> (Marked, usize) {
    let limit = limit.unwrap_or(usize::MAX);
    let (mut m, steps) = match level {
        Level::Basic => (basic(src, limit), src.len()),
        Level::Html => {
            let mut h = Html::new(src, limit);
            h.run();
            let steps = h.steps;
            (h.finish(), steps)
        }
    };
    cut(&mut m, limit);
    (m, steps)
}

/// Cuts `m` to `limit` bytes, at a character boundary.
fn cut(m: &mut Marked, limit: usize) {
    if m.text.len() <= limit {
        return;
    }
    let mut end = limit;
    while !m.text.is_char_boundary(end) {
        end -= 1;
    }
    m.text.truncate(end);
    m.runs.retain_mut(|r| {
        r.range.end = r.range.end.min(end);
        r.range.start < r.range.end
    });
}

// ---------------------------------------------------------------------
// Character references.

/// The character reference at the start of `s` (which starts with `&`):
/// its one or two characters and the bytes it used. `None` when it is not
/// one: an unknown name, no `;`, or more than 8 digits.
fn entity(s: &str) -> Option<(char, Option<char>, usize)> {
    let b = s.as_bytes();
    if b.get(1) == Some(&b'#') {
        let (hex, from) = match b.get(2) {
            Some(b'x' | b'X') => (true, 3),
            _ => (false, 2),
        };
        let digits = b[from.min(b.len())..]
            .iter()
            .take(9)
            .take_while(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
            .count();
        if digits == 0 || digits > 8 || b.get(from + digits) != Some(&b';') {
            return None;
        }
        let n = u32::from_str_radix(&s[from..from + digits], if hex { 16 } else { 10 }).ok()?;
        return Some((numeric(n), None, from + digits + 1));
    }
    let len = b[1..]
        .iter()
        .take(entities::LONGEST + 1)
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    if len == 0 || len > entities::LONGEST || b.get(1 + len) != Some(&b';') {
        return None;
    }
    let (first, second) = named(&s[1..1 + len])?;
    Some((first, second, len + 2))
}

/// The characters of the named reference `name` (without `&` and `;`).
fn named(name: &str) -> Option<(char, Option<char>)> {
    let start = |i: usize| {
        if i == 0 {
            0
        } else {
            entities::ENDS[i - 1] as usize
        }
    };
    let at = |i: usize| &entities::NAMES[start(i)..entities::ENDS[i] as usize];
    // A binary search over the sorted names.
    let (mut lo, mut hi) = (0, entities::ENDS.len());
    let i = loop {
        if lo >= hi {
            return None;
        }
        let mid = lo + (hi - lo) / 2;
        match at(mid).as_bytes().cmp(name.as_bytes()) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => break mid,
        }
    };
    let code = entities::CODES[i];
    let first = char::from_u32(code & 0x1F_FFFF)?;
    let second = match code >> 21 {
        0 => None,
        k => char::from_u32(*entities::SECOND.get(k as usize - 1)?),
    };
    Some((first, second))
}

/// A numeric reference's character, as HTML reads it: 0, surrogates and
/// values past Unicode are U+FFFD, and 0x80–0x9F are windows-1252's.
fn numeric(n: u32) -> char {
    const CP1252: [u32; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    let n = match n {
        0x80..=0x9F => CP1252[(n - 0x80) as usize],
        n => n,
    };
    match n {
        0 => '\u{FFFD}',
        n => char::from_u32(n).unwrap_or('\u{FFFD}'),
    }
}

/// `s` with its character references decoded (`&nbsp;` as a space).
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match entity(rest) {
            Some((a, b, used)) => {
                out.push(if a == '\u{A0}' { ' ' } else { a });
                out.extend(b);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------
// Basic.

/// The tags `markup: basic` knows.
const BASIC_TAGS: [&str; 5] = ["b", "i", "u", "a", "img"];

fn basic_style(open: &[&str]) -> Run {
    let mut st = Run::default();
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

/// A well-formed basic tag of a known name: `(closing, name)`.
fn basic_tag(tag: &str) -> Option<(bool, &'static str)> {
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
    let name = BASIC_TAGS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(&body[..len]))?;
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

/// The value of attribute `key` in a basic tag's text.
fn basic_attr(tag: &str, key: &str) -> Option<String> {
    let i = tag.find(&format!("{key}="))?;
    let v = &tag[i + key.len() + 1..];
    let q = v.chars().next()?;
    if q == '"' || q == '\'' {
        let v = &v[1..];
        let end = v.find(q)?;
        Some(decode(&v[..end]))
    } else {
        Some(v.split_whitespace().next()?.to_string())
    }
}

fn basic(src: &str, limit: usize) -> Marked {
    let mut out = String::with_capacity(src.len().min(limit));
    let mut runs = Vec::new();
    let mut links = Vec::new();
    // Open tags, outermost first, and where the run in their style began.
    let mut open: Vec<&'static str> = Vec::new();
    let mut run = 0;
    let flush = |out: &String, open: &[&str], run: &mut usize, runs: &mut Vec<Run>| {
        let st = basic_style(open);
        if out.len() > *run && st != Run::default() {
            runs.push(Run {
                range: *run..out.len(),
                ..st
            });
        }
        *run = out.len();
    };
    let mut rest = src;
    while let Some(i) = rest.find(['<', '&']) {
        if out.len() >= limit {
            break;
        }
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('&') {
            match entity(rest) {
                Some((a, b, used)) => {
                    out.push(if a == '\u{A0}' { ' ' } else { a });
                    out.extend(b);
                    rest = &rest[used..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
            continue;
        }
        // A tag ends at the first `>`, with no `<` before it.
        let end = rest[1..].find(['<', '>']).map(|e| e + 1);
        let parsed = end
            .filter(|e| rest[*e..].starts_with('>'))
            .and_then(|e| Some((e, basic_tag(&rest[1..e])?)));
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
                flush(&out, &open, &mut run, &mut runs);
                open.truncate(pos);
            }
        } else if name == "img" {
            // Notification images are not drawn inline; the alt text is.
            if let Some(alt) = basic_attr(tag, "alt") {
                out.push_str(&alt);
            }
        } else if !tag.trim_end().ends_with('/') && open.len() < MAX_DEPTH {
            flush(&out, &open, &mut run, &mut runs);
            if name == "a"
                && let Some(href) = basic_attr(tag, "href")
            {
                links.push(href);
            }
            open.push(name);
        }
    }
    if out.len() < limit {
        out.push_str(rest);
    }
    flush(&out, &open, &mut run, &mut runs);
    Marked {
        text: out,
        runs,
        links,
    }
}

// ---------------------------------------------------------------------
// HTML.

/// How an element breaks the text around it.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Break {
    None,
    Line,
    Blank,
}

/// What the html level does with an element it knows.
struct Element {
    name: &'static str,
    /// The break before and after it.
    brk: Break,
    /// The style its content takes, if any.
    style: Option<Run>,
    /// Its content is dropped (scripts, styles, media).
    drop: bool,
}

const fn el(name: &'static str, brk: Break) -> Element {
    Element {
        name,
        brk,
        style: None,
        drop: false,
    }
}

const fn styled(name: &'static str, brk: Break, style: Run) -> Element {
    Element {
        name,
        brk,
        style: Some(style),
        drop: false,
    }
}

const fn dropped(name: &'static str) -> Element {
    Element {
        name,
        brk: Break::None,
        style: None,
        drop: true,
    }
}

const fn st(bold: bool, italic: bool, underline: bool, strike: bool, mono: bool) -> Run {
    Run {
        range: 0..0,
        bold,
        italic,
        underline,
        strike,
        mono,
        link: false,
    }
}

const BOLD: Run = st(true, false, false, false, false);
const ITALIC: Run = st(false, true, false, false, false);
const UNDERLINE: Run = st(false, false, true, false, false);
const STRIKE: Run = st(false, false, false, true, false);
const MONO: Run = st(false, false, false, false, true);

/// The elements the html level knows; any other is dropped and its text
/// kept.
const ELEMENTS: &[Element] = &[
    styled("h1", Break::Blank, BOLD),
    styled("h2", Break::Blank, BOLD),
    styled("h3", Break::Blank, BOLD),
    styled("h4", Break::Blank, BOLD),
    styled("h5", Break::Blank, BOLD),
    styled("h6", Break::Blank, BOLD),
    el("p", Break::Blank),
    el("blockquote", Break::Blank),
    styled("pre", Break::Blank, MONO),
    el("table", Break::Blank),
    el("figure", Break::Blank),
    el("hr", Break::Blank),
    el("ul", Break::Blank),
    el("ol", Break::Blank),
    el("dl", Break::Blank),
    el("br", Break::Line),
    el("div", Break::Line),
    el("li", Break::Line),
    el("tr", Break::Line),
    styled("dt", Break::Line, BOLD),
    el("dd", Break::Line),
    el("section", Break::Line),
    el("article", Break::Line),
    el("header", Break::Line),
    el("footer", Break::Line),
    el("aside", Break::Line),
    el("main", Break::Line),
    el("nav", Break::Line),
    el("figcaption", Break::Line),
    el("address", Break::Line),
    el("details", Break::Line),
    el("summary", Break::Line),
    el("caption", Break::Line),
    el("td", Break::None),
    styled("th", Break::None, BOLD),
    el("img", Break::None),
    el("a", Break::None),
    styled("b", Break::None, BOLD),
    styled("strong", Break::None, BOLD),
    styled("i", Break::None, ITALIC),
    styled("em", Break::None, ITALIC),
    styled("cite", Break::None, ITALIC),
    styled("dfn", Break::None, ITALIC),
    styled("var", Break::None, ITALIC),
    styled("u", Break::None, UNDERLINE),
    styled("ins", Break::None, UNDERLINE),
    styled("s", Break::None, STRIKE),
    styled("strike", Break::None, STRIKE),
    styled("del", Break::None, STRIKE),
    styled("code", Break::None, MONO),
    styled("kbd", Break::None, MONO),
    styled("samp", Break::None, MONO),
    styled("tt", Break::None, MONO),
    dropped("script"),
    dropped("style"),
    dropped("head"),
    dropped("title"),
    dropped("template"),
    dropped("noscript"),
    dropped("svg"),
    dropped("math"),
    dropped("iframe"),
    dropped("object"),
    dropped("video"),
    dropped("audio"),
    dropped("canvas"),
    dropped("select"),
];

/// The element named `name` (any case).
fn element(name: &[u8]) -> Option<&'static Element> {
    if name.len() > 10 {
        return None;
    }
    ELEMENTS
        .iter()
        .find(|e| e.name.as_bytes().eq_ignore_ascii_case(name))
}

/// An open list: ordered ones count their items.
struct List {
    ordered: bool,
    next: i128,
}

fn ascii_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// The value of attribute `key` in the tag text `body` (after the name),
/// with its references decoded. Linear in `body`.
fn attr(body: &str, key: &str) -> Option<String> {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (ascii_ws(b[i]) || b[i] == b'/') {
            i += 1;
        }
        let n0 = i;
        while i < b.len() && !ascii_ws(b[i]) && !matches!(b[i], b'=' | b'/') {
            i += 1;
        }
        let name = &body[n0..i];
        while i < b.len() && ascii_ws(b[i]) {
            i += 1;
        }
        let mut value = None;
        if b.get(i) == Some(&b'=') {
            i += 1;
            while i < b.len() && ascii_ws(b[i]) {
                i += 1;
            }
            match b.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let v0 = i + 1;
                    let end = body[v0..].find(q as char).map_or(b.len(), |e| v0 + e);
                    value = Some(&body[v0..end]);
                    i = end + 1;
                }
                _ => {
                    let v0 = i;
                    while i < b.len() && !ascii_ws(b[i]) {
                        i += 1;
                    }
                    value = Some(&body[v0..i]);
                }
            }
        }
        if !name.is_empty() && name.eq_ignore_ascii_case(key) {
            return Some(decode(value.unwrap_or("")));
        }
        if name.is_empty() && value.is_none() {
            i += 1;
        }
    }
    None
}

/// A whole number attribute (`ol start`, `li value`): at most 20 digits.
fn number(v: &str) -> Option<i128> {
    let v = v.trim();
    let digits = v.strip_prefix(['-', '+']).unwrap_or(v);
    if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}

/// The html level's one pass.
struct Html<'a> {
    src: &'a str,
    limit: usize,
    out: String,
    runs: Vec<Run>,
    links: Vec<String>,
    /// Open styled elements, outermost first.
    open: Vec<(&'static str, Run)>,
    /// Where the current run's text began, once it has some.
    run_start: Option<usize>,
    /// Inside `pre`: whitespace is kept.
    pre: bool,
    /// A newline right after `<pre>` is dropped.
    skip_newline: bool,
    /// Spaces waiting for text: collapsed spaces and `&nbsp;`s.
    spaces: String,
    /// The last waiting space is a collapsible one.
    soft_last: bool,
    /// The break waiting for text.
    brk: Break,
    /// The next text starts a line (indent and list marker first).
    line_start: bool,
    /// A list item's marker, waiting for its text.
    marker: Option<String>,
    /// A table cell's separator, waiting for its text.
    sep: bool,
    lists: Vec<List>,
    /// Inside a `dd`: indented.
    dd: bool,
    /// Cells seen in the current table row.
    cells: usize,
    /// The next `>` at or after a position: `(from, at)`, `at` `None` when
    /// there is none.
    gt: Option<(usize, Option<usize>)>,
    /// A quoted attribute value ran to the end: tags end at the next `>`.
    quotes_off: bool,
    steps: usize,
}

impl<'a> Html<'a> {
    fn new(src: &'a str, limit: usize) -> Self {
        Self {
            src,
            limit,
            out: String::with_capacity(src.len().min(limit)),
            runs: Vec::new(),
            links: Vec::new(),
            open: Vec::new(),
            run_start: None,
            pre: false,
            skip_newline: false,
            spaces: String::new(),
            soft_last: false,
            brk: Break::None,
            line_start: true,
            marker: None,
            sep: false,
            lists: Vec::new(),
            dd: false,
            cells: 0,
            gt: None,
            quotes_off: false,
            steps: 0,
        }
    }

    fn run(&mut self) {
        let src = self.src;
        let b = src.as_bytes();
        let mut i = 0;
        while i < b.len() && self.out.len() < self.limit {
            let j = src[i..].find(['<', '&']).map_or(b.len(), |j| i + j);
            self.steps += j - i;
            self.text(&src[i..j]);
            i = j;
            if i >= b.len() {
                break;
            }
            if b[i] == b'&' {
                // A reference is looked for in its longest name's bytes.
                self.steps += (b.len() - i).min(entities::LONGEST + 2);
                match entity(&src[i..]) {
                    Some((a, second, used)) => {
                        if a == '\u{A0}' && second.is_none() {
                            self.hard_space();
                        } else {
                            self.ch(a);
                            if let Some(c) = second {
                                self.ch(c);
                            }
                        }
                        i += used;
                    }
                    None => {
                        self.ch('&');
                        i += 1;
                    }
                }
                continue;
            }
            i = self.markup(i);
        }
    }

    fn finish(mut self) -> Marked {
        self.flush();
        // Both ends are trimmed: waiting spaces and breaks never went out,
        // but a `pre` can end in whitespace.
        let end = self.out.trim_end().len();
        self.out.truncate(end);
        self.runs.retain_mut(|r| {
            r.range.end = r.range.end.min(end);
            r.range.start < r.range.end
        });
        Marked {
            text: self.out,
            runs: self.runs,
            links: self.links,
        }
    }

    /// The next `>` at or after `from`, remembered so a `<` with none after
    /// it never scans again.
    fn next_gt(&mut self, from: usize) -> Option<usize> {
        if let Some((f, at)) = self.gt
            && from >= f
            && at.is_none_or(|a| a >= from)
        {
            return at;
        }
        let at = self.src[from..].find('>').map(|e| from + e);
        self.steps += at.map_or(self.src.len(), |a| a + 1) - from;
        self.gt = Some((from, at));
        at
    }

    /// Where the tag starting at `<` (`i`) ends (its `>`): the first `>`
    /// outside a quoted attribute value, else the first `>`. `None` when
    /// there is no `>` after it.
    fn tag_end(&mut self, i: usize) -> Option<usize> {
        let plain = self.next_gt(i + 1)?;
        if self.quotes_off {
            return Some(plain);
        }
        let b = self.src.as_bytes();
        let mut k = i + 1;
        let mut after_eq = false;
        while k < b.len() {
            self.steps += 1;
            match b[k] {
                b'>' => return Some(k),
                q @ (b'"' | b'\'') if after_eq => match self.src[k + 1..].find(q as char) {
                    Some(e) => {
                        self.steps += e + 1;
                        k += e + 2;
                        after_eq = false;
                        continue;
                    }
                    None => break,
                },
                b'=' => after_eq = true,
                c if ascii_ws(c) => {}
                _ => after_eq = false,
            }
            k += 1;
        }
        // An unterminated quote (or quotes hiding every `>`): from here on
        // tags end at their first `>`, so this scan never repeats.
        self.steps += b.len() - k;
        self.quotes_off = true;
        Some(plain)
    }

    /// Reads the markup at `<` (`i`) and returns where text goes on.
    fn markup(&mut self, i: usize) -> usize {
        self.steps += 1;
        let src = self.src;
        let b = src.as_bytes();
        let rest = &src[i..];
        match b.get(i + 1) {
            Some(b'!') => {
                if let Some(body) = rest.strip_prefix("<!--") {
                    // A comment (and a conditional one): to `-->`, or the
                    // end. `<!-->` and `<!--->` are empty ones.
                    if body.starts_with('>') {
                        return i + 5;
                    }
                    if body.starts_with("->") {
                        return i + 6;
                    }
                    let end = body.find("-->").map_or(b.len(), |e| i + 4 + e + 3);
                    self.steps += end - i;
                    return end;
                }
                if let Some(body) = rest.strip_prefix("<![CDATA[") {
                    let (text, end) = match body.find("]]>") {
                        Some(e) => (&body[..e], i + 9 + e + 3),
                        None => (body, b.len()),
                    };
                    self.steps += end - i;
                    self.text(text);
                    return end;
                }
                self.bogus(i)
            }
            Some(b'?') => self.bogus(i),
            Some(b'/') if b.get(i + 2).is_some_and(u8::is_ascii_alphabetic) => {
                let Some(end) = self.tag_end(i) else {
                    self.ch('<');
                    return i + 1;
                };
                let name = tag_name(&src[i + 2..end]);
                if let Some(e) = element(name.as_bytes()) {
                    self.close(e);
                }
                end + 1
            }
            Some(c) if c.is_ascii_alphabetic() => {
                let Some(end) = self.tag_end(i) else {
                    self.ch('<');
                    return i + 1;
                };
                let tag = &src[i + 1..end];
                let name = tag_name(tag);
                let body = &tag[name.len()..];
                let self_closing = body.trim_end().ends_with('/');
                match element(name.as_bytes()) {
                    Some(e) if e.drop && !self_closing => self.skip_to_close(end + 1, e.name),
                    Some(e) => {
                        self.open_tag(e, body, self_closing);
                        end + 1
                    }
                    None => end + 1,
                }
            }
            _ => {
                self.ch('<');
                i + 1
            }
        }
    }

    /// `<!…>` or `<?…>` that is no comment: dropped to its `>`, or the end.
    fn bogus(&mut self, i: usize) -> usize {
        let end = self.next_gt(i + 1).map_or(self.src.len(), |e| e + 1);
        self.steps += end - i;
        end
    }

    /// Past the close tag of the dropped element `name` from `from`, or the
    /// end when it never closes. One forward search.
    fn skip_to_close(&mut self, from: usize, name: &str) -> usize {
        let src = self.src;
        let b = src.as_bytes();
        let mut k = from;
        while let Some(e) = src[k..].find("</") {
            let at = k + e;
            self.steps += e + 2;
            let n = at + 2;
            let close = b
                .get(n..n + name.len())
                .is_some_and(|w| w.eq_ignore_ascii_case(name.as_bytes()))
                && b.get(n + name.len())
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b':' && *c != b'-');
            if close {
                return self.next_gt(n).map_or(b.len(), |g| g + 1);
            }
            k = at + 2;
        }
        self.steps += b.len() - k;
        b.len()
    }

    fn open_tag(&mut self, e: &'static Element, body: &str, self_closing: bool) {
        match e.name {
            "br" => {
                self.brk = match self.brk {
                    Break::None => Break::Line,
                    _ => Break::Blank,
                };
                return;
            }
            "img" => {
                if let Some(alt) = attr(body, "alt") {
                    self.text(&alt);
                }
                return;
            }
            "a" => {
                let href = attr(body, "href");
                let link = href.is_some();
                if let Some(h) = href {
                    self.links.push(h);
                }
                if !self_closing {
                    self.push(
                        "a",
                        Run {
                            link,
                            ..Run::default()
                        },
                    );
                }
                return;
            }
            "ul" | "ol" => {
                self.request(if self.lists.is_empty() {
                    Break::Blank
                } else {
                    Break::Line
                });
                if self.lists.len() < MAX_DEPTH {
                    let next = attr(body, "start").and_then(|v| number(&v)).unwrap_or(1);
                    self.lists.push(List {
                        ordered: e.name == "ol",
                        next,
                    });
                }
                return;
            }
            "dl" => {
                self.request(if self.lists.is_empty() {
                    Break::Blank
                } else {
                    Break::Line
                });
                return;
            }
            "li" => {
                self.request(Break::Line);
                let marker = match self.lists.last_mut() {
                    Some(l) if l.ordered => {
                        let n = attr(body, "value")
                            .and_then(|v| number(&v))
                            .unwrap_or(l.next);
                        l.next = n.saturating_add(1);
                        format!("{n}. ")
                    }
                    _ => "• ".to_string(),
                };
                self.marker = Some(marker);
                return;
            }
            "dt" | "dd" => {
                // A `dt` left open ends at the next term or description.
                self.pop("dt");
                self.request(Break::Line);
                self.dd = e.name == "dd";
            }
            "tr" => {
                self.pop("th");
                self.request(Break::Line);
                self.cells = 0;
                return;
            }
            "td" | "th" => {
                self.pop("th");
                if self.cells > 0 {
                    self.sep = true;
                }
                self.cells += 1;
            }
            _ => {}
        }
        if e.brk != Break::None {
            self.request(e.brk);
        }
        if let Some(style) = &e.style
            && !self_closing
        {
            self.push(e.name, style.clone());
            if e.name == "pre" {
                self.skip_newline = true;
            }
        }
    }

    fn close(&mut self, e: &'static Element) {
        match e.name {
            "ul" | "ol" => {
                let ordered = e.name == "ol";
                if let Some(pos) = self.lists.iter().rposition(|l| l.ordered == ordered) {
                    self.lists.truncate(pos);
                }
                self.request(if self.lists.is_empty() {
                    Break::Blank
                } else {
                    Break::Line
                });
                return;
            }
            "dl" => {
                self.pop("dt");
                self.dd = false;
                self.request(if self.lists.is_empty() {
                    Break::Blank
                } else {
                    Break::Line
                });
                return;
            }
            "dd" => self.dd = false,
            "tr" => self.cells = 0,
            "br" => {
                // `</br>` is read as `<br>`, as browsers do.
                self.brk = match self.brk {
                    Break::None => Break::Line,
                    _ => Break::Blank,
                };
                return;
            }
            _ => {}
        }
        if e.style.is_some() || e.name == "a" {
            self.pop(e.name);
        }
        if e.brk != Break::None {
            self.request(e.brk);
        }
    }

    /// Opens a styled element, unless [`MAX_DEPTH`] are open.
    fn push(&mut self, name: &'static str, style: Run) {
        if self.open.len() < MAX_DEPTH {
            self.flush();
            self.open.push((name, style));
            self.pre = self.open.iter().any(|(n, _)| *n == "pre");
        }
    }

    /// Closes the innermost open `name` and anything left open inside it.
    fn pop(&mut self, name: &str) {
        if let Some(pos) = self.open.iter().rposition(|(n, _)| *n == name) {
            self.flush();
            self.open.truncate(pos);
            self.pre = self.open.iter().any(|(n, _)| *n == "pre");
        }
    }

    /// The style in force: every open element's, combined.
    fn style(&self) -> Run {
        let mut s = Run::default();
        for (_, o) in &self.open {
            s.bold |= o.bold;
            s.italic |= o.italic;
            s.underline |= o.underline;
            s.strike |= o.strike;
            s.mono |= o.mono;
            s.link |= o.link;
        }
        s
    }

    /// Ends the current run (before the style changes).
    fn flush(&mut self) {
        if let Some(start) = self.run_start.take() {
            let st = self.style();
            if self.out.len() > start && st != Run::default() {
                self.runs.push(Run {
                    range: start..self.out.len(),
                    ..st
                });
            }
        }
    }

    /// Asks for a break before the next text.
    fn request(&mut self, brk: Break) {
        self.brk = match (self.brk, brk) {
            (Break::Blank, _) | (_, Break::Blank) => Break::Blank,
            (Break::Line, _) | (_, Break::Line) => Break::Line,
            _ => Break::None,
        };
    }

    fn text(&mut self, s: &str) {
        for c in s.chars() {
            self.ch(c);
        }
    }

    fn ch(&mut self, c: char) {
        if self.pre {
            if c == '\r' {
                return;
            }
            if std::mem::take(&mut self.skip_newline) && c == '\n' {
                return;
            }
            self.begin();
            self.out.push(c);
            if c == '\n' {
                self.line_start = true;
            }
            return;
        }
        self.skip_newline = false;
        if c.is_ascii() && ascii_ws(c as u8) {
            if !self.soft_last {
                self.spaces.push(' ');
                self.soft_last = true;
            }
        } else {
            self.begin();
            self.out.push(c);
        }
    }

    /// `&nbsp;`: a space that never collapses (but still goes at a break).
    fn hard_space(&mut self) {
        if self.pre {
            self.begin();
            self.out.push(' ');
        } else {
            self.spaces.push(' ');
            self.soft_last = false;
        }
    }

    /// Puts out what waits before text: a break, then a line's indent and
    /// list marker, a cell separator, or the waiting spaces.
    fn begin(&mut self) {
        if self.brk != Break::None {
            if !self.out.is_empty() {
                // Counting the newlines a `pre` already ended in.
                let want = if self.brk == Break::Blank { 2 } else { 1 };
                let have = self
                    .out
                    .bytes()
                    .rev()
                    .take(2)
                    .take_while(|b| *b == b'\n')
                    .count();
                self.out
                    .extend(std::iter::repeat_n('\n', want - have.min(want)));
                self.line_start = true;
            }
            self.brk = Break::None;
        }
        if self.line_start {
            let nest = self.lists.len().saturating_sub(1);
            let indent = (2 * nest).min(MAX_INDENT) + if self.dd { 2 } else { 0 };
            self.out.extend(std::iter::repeat_n(' ', indent));
            if let Some(m) = self.marker.take() {
                self.out.push_str(&m);
            }
            self.line_start = false;
            self.spaces.clear();
        } else if self.sep {
            self.out.push_str(" · ");
            self.spaces.clear();
        } else {
            self.out.push_str(&self.spaces);
            self.spaces.clear();
        }
        self.sep = false;
        self.soft_last = false;
        if self.run_start.is_none() {
            self.run_start = Some(self.out.len());
        }
    }
}

/// A tag's name: up to whitespace, `/` or the end.
fn tag_name(tag: &str) -> &str {
    let end = tag
        .find(|c: char| c.is_ascii_whitespace() || c == '/')
        .unwrap_or(tag.len());
    &tag[..end]
}

#[cfg(test)]
mod tests;
