//! Diagnostics: errors and warnings with labelled spans and fixes.
//!
//! Shared by every compiler stage. A [`Diagnostic`] is plain data whose
//! labels each name a file ([`FileId`]) and a file-local [`Span`], so one
//! diagnostic can point into several files. It is rendered against a
//! [`SourceMap`] with miette (file, line, column, caret and labels) by
//! [`render`], or as one `file:line:col` line by [`render_short`].

use std::fmt;
use std::sync::Arc;

use miette::{GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, SourceSpan};

use crate::source::{FileId, SourceMap};
use crate::syntax::{LineIndex, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
}

/// A span with an explanation. The primary label marks where the problem is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// The file `span` is in. Labels added with [`Diagnostic::with_label`]
    /// start in `FileId::default()`; a stage that works on one file stamps
    /// them with [`Diagnostic::in_file`] (the parser does this itself).
    pub file: FileId,
    pub span: Span,
    pub message: String,
    pub primary: bool,
}

/// A replacement a diagnostic proposes: `replacement` for the text at
/// `span` in `file`, such as `critical` for a misspelt `critcal`. Editors
/// offer each one as a quick fix, so tools never parse the help text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub file: FileId,
    pub span: Span,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable machine-readable code, such as `syntax::expected`.
    pub code: &'static str,
    pub message: String,
    pub labels: Vec<Label>,
    /// A fix, usually "did you mean `x`?".
    pub help: Option<String>,
    /// The replacements the diagnostic proposes, as data: one for a
    /// "did you mean `x`?" help ([`Diagnostic::suggest`]), or several
    /// choices (the parameters a call does not give yet).
    pub suggestions: Vec<Suggestion>,
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            labels: Vec::new(),
            help: None,
            suggestions: Vec::new(),
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message)
        }
    }

    /// Adds the primary label.
    pub fn with_label(self, span: Span, message: impl Into<String>) -> Self {
        self.with_label_in(FileId::default(), span, message)
    }

    /// Adds a secondary label, such as the `{` an unclosed block opened at.
    pub fn with_secondary(self, span: Span, message: impl Into<String>) -> Self {
        self.with_secondary_in(FileId::default(), span, message)
    }

    /// Adds the primary label in a given file.
    pub fn with_label_in(mut self, file: FileId, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            file,
            span,
            message: message.into(),
            primary: true,
        });
        self
    }

    /// Adds a secondary label in a given file, such as the other
    /// declaration of a name declared in two files.
    pub fn with_secondary_in(
        mut self,
        file: FileId,
        span: Span,
        message: impl Into<String>,
    ) -> Self {
        self.labels.push(Label {
            file,
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    /// Adds a secondary label in place, for builders holding `&mut`.
    pub fn add_secondary(
        &mut self,
        file: FileId,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Self {
        self.labels.push(Label {
            file,
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    /// Moves every label into `file`: for stages that see one file and
    /// build labels with [`Diagnostic::with_label`].
    pub fn in_file(mut self, file: FileId) -> Self {
        for l in &mut self.labels {
            l.file = file;
        }
        for s in &mut self.suggestions {
            s.file = file;
        }
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Proposes `replacement` for the text at `span`, in the primary
    /// label's file, and sets the help to "did you mean `replacement`?".
    pub fn suggest(&mut self, span: Span, replacement: impl Into<String>) -> &mut Self {
        let replacement = replacement.into();
        self.help = Some(format!("did you mean `{replacement}`?"));
        self.suggestions.clear();
        self.add_suggestion(span, replacement)
    }

    /// [`Diagnostic::suggest`] when there is a suggestion.
    pub fn suggest_opt(&mut self, span: Span, replacement: Option<String>) -> &mut Self {
        if let Some(r) = replacement {
            self.suggest(span, r);
        }
        self
    }

    /// [`Diagnostic::suggest`] for builders.
    pub fn with_suggestion(mut self, span: Span, replacement: impl Into<String>) -> Self {
        self.suggest(span, replacement);
        self
    }

    /// Adds `replacement` for the text at `span` as one of several
    /// choices, leaving the help alone.
    pub fn add_suggestion(&mut self, span: Span, replacement: impl Into<String>) -> &mut Self {
        let file = self.primary().map(|l| l.file).unwrap_or_default();
        self.suggestions.push(Suggestion {
            file,
            span,
            replacement: replacement.into(),
        });
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// The primary label, or the first label.
    pub fn primary(&self) -> Option<&Label> {
        self.labels
            .iter()
            .find(|l| l.primary)
            .or(self.labels.first())
    }

    /// The span of the primary label, or of the first label.
    pub fn primary_span(&self) -> Option<Span> {
        self.primary().map(|l| l.span)
    }

    /// The file of the primary label (the first file if there are no
    /// labels).
    pub fn file(&self) -> FileId {
        self.primary().map(|l| l.file).unwrap_or_default()
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The closest candidate to `word`, for "did you mean" help.
///
/// Uses optimal-string-alignment distance (a swap of two neighbouring
/// letters counts once), allowing about one edit per three characters.
/// A one-letter word is only matched by case (`X` → `x`), and a word of
/// two or more letters is never offered a one-letter name (`tp` must not
/// become `t`). Ties go, in order, to a plausible typo (letters only
/// dropped, only added, or swapped: `slt` → `slot`, not `set`; `drak` →
/// `dark`), then to the smallest
/// length difference, then to the alphabetically first, so a suggestion
/// never depends on the order a hash map lists its names in.
/// Candidates whose length rules them out are skipped before any
/// distance is computed, so long candidate lists stay cheap.
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let len = word.chars().count();
    let limit = len.div_ceil(3).max(1);
    let ascii = word.is_ascii();
    let mut word_letters = [0u16; 128];
    for b in word.bytes() {
        word_letters[usize::from(b & 0x7f)] += 1;
    }
    let mut letters = word_letters;
    let mut best: Option<((usize, bool, usize), &'a str)> = None;
    for c in candidates {
        if c == word {
            continue;
        }
        let clen = c.chars().count();
        let diff = clen.abs_diff(len);
        if diff > limit || (len >= 2 && clen == 1) {
            continue;
        }
        // An edit adds, drops or changes one letter (a swap none), so the
        // letters the two do not share bound the distance from below:
        // most candidates are ruled out without the quadratic distance.
        if ascii && c.is_ascii() {
            let mut shared = 0;
            for b in c.bytes() {
                let n = &mut letters[usize::from(b)];
                if *n > 0 {
                    *n -= 1;
                    shared += 1;
                }
            }
            for b in c.bytes() {
                let i = usize::from(b);
                letters[i] = word_letters[i];
            }
            let unshared = (len - shared).max(clen - shared);
            if unshared > limit && !c.eq_ignore_ascii_case(word) {
                continue;
            }
        }
        let distance = if c.eq_ignore_ascii_case(word) {
            0
        } else if len == 1 {
            continue;
        } else {
            strsim::osa_distance(word, c)
        };
        if distance > limit {
            continue;
        }
        let key = (distance, !plausible_typo(word, c), diff);
        let better = match best {
            None => true,
            Some((k, b)) => key < k || (key == k && c < b),
        };
        if better {
            best = Some((key, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Whether `a` and `b` differ only by added letters, only by dropped
/// letters, or only by order (the same letters).
fn plausible_typo(a: &str, b: &str) -> bool {
    fn sub(short: &str, long: &str) -> bool {
        let mut it = long.chars();
        short.chars().all(|c| it.any(|l| l == c))
    }
    let sorted = |s: &str| {
        let mut v: Vec<char> = s.chars().collect();
        v.sort_unstable();
        v
    };
    sub(a, b) || sub(b, a) || sorted(a) == sorted(b)
}

/// The closest candidate to `word` as an owned name, for
/// [`Diagnostic::suggest_opt`].
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<String> {
    suggest(word, candidates).map(str::to_string)
}

/// How [`render`] draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Unicode box drawing with ANSI colours, for terminals.
    Color,
    /// Unicode box drawing without colours, for logs and tests.
    Plain,
}

/// Diagnostics past this many per file are summarised, not drawn: each
/// drawn report scans its file, and a file with an error on every line
/// would otherwise take quadratic time to render.
pub const MAX_RENDERED_PER_FILE: usize = 50;

/// One miette report: the labels of a diagnostic that lie in one file.
/// Labels in other files become related reports.
struct Report<'a> {
    diag: &'a Diagnostic,
    file: FileId,
    source: NamedSource<Arc<str>>,
    message: String,
    /// False for the part showing labels in another file.
    main: bool,
    related: Vec<Report<'a>>,
}

impl fmt::Debug for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.diag, f)
    }
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Report<'_> {}

impl miette::Diagnostic for Report<'_> {
    fn code<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
        self.main
            .then(|| Box::new(self.diag.code) as Box<dyn fmt::Display + 'b>)
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
        })
    }

    fn help<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
        if !self.main {
            return None; // the main report carries the help
        }
        self.diag
            .help
            .as_ref()
            .map(|h| Box::new(h) as Box<dyn fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        Some(&self.source)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let len = self.source.inner().len();
        let file = self.file;
        Some(Box::new(
            self.diag
                .labels
                .iter()
                .filter(move |l| l.file == file)
                .map(move |l| {
                    let start = (l.span.start as usize).min(len);
                    let end = (l.span.end as usize).clamp(start, len);
                    let span = SourceSpan::from(start..end);
                    let msg = (!l.message.is_empty()).then(|| l.message.clone());
                    if l.primary {
                        LabeledSpan::new_primary_with_span(msg, span)
                    } else {
                        LabeledSpan::new_with_span(msg, span)
                    }
                }),
        ))
    }

    fn related<'b>(&'b self) -> Option<Box<dyn Iterator<Item = &'b dyn miette::Diagnostic> + 'b>> {
        if self.related.is_empty() {
            return None;
        }
        Some(Box::new(
            self.related.iter().map(|r| r as &dyn miette::Diagnostic),
        ))
    }
}

fn named(map: &SourceMap, file: FileId) -> NamedSource<Arc<str>> {
    match map.get(file) {
        Some(f) if f.text.contains('\r') => {
            NamedSource::new(&f.name, Arc::from(lone_cr_as_newline(&f.text)))
        }
        Some(f) => NamedSource::new(&f.name, Arc::clone(&f.text)),
        None => NamedSource::new("<unknown file>", Arc::from("")),
    }
}

/// A lone `\r` is a line break to the lexer and [`LineIndex`]; miette's
/// snippets only break at `\n`, so give it `\n` there (same length, so
/// every span stays put) and the gutter agrees with the header.
fn lone_cr_as_newline(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        if c == '\r' && bytes.get(i + 1) != Some(&b'\n') {
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// Lines longer than this (in characters) are not drawn: a snippet of a
/// minified or generated line is unreadable, and miette panics on columns
/// past 65,535. Such diagnostics fall back to [`render_short`].
pub const MAX_DRAWN_LINE: usize = 1000;

/// Whether any line a label of `diag` would draw (its own lines and one of
/// context either side) is longer than [`MAX_DRAWN_LINE`].
fn too_wide(diag: &Diagnostic, map: &SourceMap) -> bool {
    diag.labels.iter().any(|l| {
        let Some(f) = map.get(l.file) else {
            return false;
        };
        let text = &*f.text;
        let is_break = |b: u8| b == b'\n' || b == b'\r';
        let bytes = text.as_bytes();
        let start = (l.span.start as usize).min(text.len());
        let end = (l.span.end as usize).clamp(start, text.len());
        // Back to the start of the line before, forward to the end of the
        // line after.
        let mut from = start;
        for _ in 0..2 {
            from = bytes[..from.saturating_sub(1)]
                .iter()
                .rposition(|&b| is_break(b))
                .map_or(0, |i| i + 1)
                .min(from);
        }
        let mut to = end;
        for _ in 0..2 {
            to = bytes[(to + 1).min(bytes.len())..]
                .iter()
                .position(|&b| is_break(b))
                .map_or(bytes.len(), |i| to + 1 + i);
        }
        text.get(from..to).is_none_or(|region| {
            region
                .split(['\n', '\r'])
                .any(|line| line.len() > MAX_DRAWN_LINE && line.chars().count() > MAX_DRAWN_LINE)
        })
    })
}

fn report<'a>(diag: &'a Diagnostic, map: &SourceMap) -> Report<'a> {
    let home = diag.file();
    let mut others: Vec<FileId> = Vec::new();
    for l in &diag.labels {
        if l.file != home && !others.contains(&l.file) {
            others.push(l.file);
        }
    }
    let related = others
        .into_iter()
        .map(|file| Report {
            diag,
            file,
            source: named(map, file),
            message: format!("see also {}", map.get(file).map_or("?", |f| &f.name)),
            main: false,
            related: Vec::new(),
        })
        .collect();
    Report {
        diag,
        file: home,
        source: named(map, home),
        message: diag.message.clone(),
        main: true,
        related,
    }
}

/// Renders diagnostics with source snippets, carets and labels, each label
/// in its own file. At most [`MAX_RENDERED_PER_FILE`] diagnostics are drawn
/// per file (by the file of their primary label); the rest are counted in
/// an "and N more" line.
pub fn render(diags: &[Diagnostic], map: &SourceMap, style: Style) -> String {
    let theme = match style {
        Style::Color => GraphicalTheme::unicode(),
        Style::Plain => GraphicalTheme::unicode_nocolor(),
    };
    let handler = GraphicalReportHandler::new_themed(theme)
        .with_width(100)
        .with_links(false);
    let mut out = String::new();
    let mut drawn: std::collections::HashMap<FileId, usize> = Default::default();
    let mut hidden: Vec<(FileId, usize)> = Vec::new();
    for diag in diags {
        let file = diag.file();
        let n = drawn.entry(file).or_default();
        if *n >= MAX_RENDERED_PER_FILE {
            match hidden.iter_mut().find(|(f, _)| *f == file) {
                Some((_, c)) => *c += 1,
                None => hidden.push((file, 1)),
            }
            continue;
        }
        *n += 1;
        if too_wide(diag, map) || handler.render_report(&mut out, &report(diag, map)).is_err() {
            out.push_str(&render_short(std::slice::from_ref(diag), map));
        }
        out.push('\n');
    }
    for (file, count) in hidden {
        let name = map.get(file).map_or("?", |f| &f.name);
        let plural = if count == 1 { "" } else { "s" };
        out.push_str(&format!(
            "{name}: and {count} more diagnostic{plural} not shown\n\n"
        ));
    }
    out
}

/// One line per diagnostic: `file:line:col: error[code]: message; help`,
/// at the primary label. This is the form design.md quotes for a held-back
/// save (`bar.strand:12: unknown prop "expanded"; did you mean "open"?`),
/// with the column added: the help follows a `; ` and names quoted in
/// backticks for the caret render are double-quoted, as plain prose on one
/// line reads.
pub fn render_short(diags: &[Diagnostic], map: &SourceMap) -> String {
    let mut indexes: std::collections::HashMap<FileId, LineIndex> = Default::default();
    let mut out = String::new();
    for d in diags {
        let file = d.file();
        let (name, text) = map
            .get(file)
            .map_or(("?", ""), |f| (f.name.as_str(), &*f.text));
        let index = indexes.entry(file).or_insert_with(|| LineIndex::new(text));
        let (line, col) = d
            .primary_span()
            .map_or((1, 1), |s| index.line_col(text, s.start));
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        out.push_str(&format!(
            "{name}:{line}:{col}: {sev}[{}]: {}",
            d.code,
            plain_quotes(&d.message)
        ));
        if let Some(help) = &d.help {
            out.push_str("; ");
            out.push_str(&plain_quotes(help));
        }
        out.push('\n');
    }
    out
}

/// `` `name` `` becomes `"name"` for the one-line form. A quoted span that
/// already holds a `"` (a text literal) keeps its backticks, and an
/// unpaired backtick is left as written.
fn plain_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('`') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let body = &after[..close];
        if body.contains('"') {
            out.push('`');
            out.push_str(body);
            out.push('`');
        } else {
            out.push('"');
            out.push_str(body);
            out.push('"');
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_keywords() {
        let kws = ["component", "state", "critical", "change", "in"];
        assert_eq!(suggest("componnet", kws), Some("component"));
        assert_eq!(suggest("critcal", kws), Some("critical"));
        assert_eq!(suggest("chnage", kws), Some("change"));
        assert_eq!(suggest("im", kws), Some("in"));
        assert_eq!(suggest("banana", kws), None);
        assert_eq!(suggest("state", kws), None);
    }

    #[test]
    fn suggestions_prefer_plausible_typos() {
        // A word of two or more letters never becomes a one-letter name.
        assert_eq!(suggest("tp", ["t", "top"]), Some("top"));
        assert_eq!(suggest("ed", ["e"]), None);
        // One letter: only a change of case.
        assert_eq!(suggest("y", ["t", "x"]), None);
        assert_eq!(suggest("X", ["x", "K"]), Some("x"));
        // A dropped letter beats a changed one.
        assert_eq!(suggest("slt", ["set", "slot"]), Some("slot"));
        // Then the closer length, then the alphabet.
        assert_eq!(suggest("drak", ["dark", "drake"]), Some("dark"));
        assert_eq!(suggest("ab", ["ac", "ad"]), Some("ac"));
    }

    #[test]
    fn short_render_has_file_line_col() {
        let d = Diagnostic::error("syntax::expected", "expected `}`")
            .with_label(Span::new(4, 5), "here")
            .with_help("did you mean `x`?");
        let (map, _) = SourceMap::single("bar.strand", "a\nbc\n");
        let out = render_short(&[d], &map);
        assert_eq!(
            out,
            "bar.strand:2:3: error[syntax::expected]: expected \"}\"; did you mean \"x\"?\n"
        );
    }

    #[test]
    fn short_render_keeps_quoted_text_literals() {
        assert_eq!(plain_quotes("`a` and `b`"), "\"a\" and \"b\"");
        assert_eq!(plain_quotes("got `\"x\"`"), "got `\"x\"`");
        assert_eq!(plain_quotes("one ` left"), "one ` left");
    }

    #[test]
    fn labels_in_other_files_render_there() {
        let mut map = SourceMap::new();
        let a = map.add("a.strand", "state volume = 0\n");
        let b = map.add("b.strand", "\nstate volume = 1\n");
        let d = Diagnostic::error("check::redeclared", "`volume` is declared twice")
            .with_label_in(b, Span::new(7, 13), "declared again here")
            .with_secondary_in(a, Span::new(6, 12), "first declared here");
        assert_eq!(d.file(), b);
        let out = render(std::slice::from_ref(&d), &map, Style::Plain);
        assert!(out.contains("[b.strand:2:7]"), "{out}");
        assert!(out.contains("[a.strand:1:7]"), "{out}");
        assert!(out.contains("first declared here"), "{out}");
        assert_eq!(
            render_short(&[d], &map),
            "b.strand:2:7: error[check::redeclared]: \"volume\" is declared twice\n"
        );
    }

    #[test]
    fn rendering_is_capped_per_file() {
        let src: String = "x\n".repeat(200);
        let (map, file) = SourceMap::single("many.strand", src);
        let diags: Vec<_> = (0..200u32)
            .map(|i| {
                Diagnostic::error("syntax::expected", "bad")
                    .with_label(Span::new(i * 2, i * 2 + 1), "here")
                    .in_file(file)
            })
            .collect();
        let out = render(&diags, &map, Style::Plain);
        assert_eq!(
            out.matches("syntax::expected").count(),
            MAX_RENDERED_PER_FILE
        );
        assert!(
            out.contains("many.strand: and 150 more diagnostics not shown"),
            "{out}"
        );
    }
}
