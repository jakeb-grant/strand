//! Diagnostics: errors and warnings with labelled spans and fixes.
//!
//! Shared by every compiler stage. A [`Diagnostic`] is plain data; it is
//! rendered with miette (file, line, column, caret and labels) by
//! [`render`], or as one `file:line:col` line by [`render_short`].

use std::fmt;

use miette::{GraphicalReportHandler, GraphicalTheme, LabeledSpan, NamedSource, SourceSpan};

use crate::syntax::{LineIndex, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
    Error,
    Warning,
}

/// A span with an explanation. The primary label marks where the problem is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: String,
    pub primary: bool,
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
}

impl Diagnostic {
    pub fn error(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            ..Self::error(code, message)
        }
    }

    /// Adds the primary label.
    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: true,
        });
        self
    }

    /// Adds a secondary label, such as the `{` an unclosed block opened at.
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
            primary: false,
        });
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// The span of the primary label, or of the first label.
    pub fn primary_span(&self) -> Option<Span> {
        self.labels
            .iter()
            .find(|l| l.primary)
            .or(self.labels.first())
            .map(|l| l.span)
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
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = word.chars().count().div_ceil(3).max(1);
    candidates
        .into_iter()
        .filter(|c| *c != word)
        .map(|c| (strsim::osa_distance(word, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Formats "did you mean `x`?" if a candidate is close enough.
pub fn did_you_mean<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    suggest(word, candidates).map(|s| format!("did you mean `{s}`?"))
}

/// How [`render`] draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Unicode box drawing with ANSI colours, for terminals.
    Color,
    /// Unicode box drawing without colours, for logs and tests.
    Plain,
}

struct Report<'a> {
    diag: &'a Diagnostic,
    source: NamedSource<String>,
}

impl fmt::Debug for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.diag, f)
    }
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diag.message)
    }
}

impl std::error::Error for Report<'_> {}

impl miette::Diagnostic for Report<'_> {
    fn code<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
        Some(Box::new(self.diag.code))
    }

    fn severity(&self) -> Option<miette::Severity> {
        Some(match self.diag.severity {
            Severity::Error => miette::Severity::Error,
            Severity::Warning => miette::Severity::Warning,
        })
    }

    fn help<'b>(&'b self) -> Option<Box<dyn fmt::Display + 'b>> {
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
        Some(Box::new(self.diag.labels.iter().map(move |l| {
            let start = (l.span.start as usize).min(len);
            let end = (l.span.end as usize).clamp(start, len);
            let span = SourceSpan::from(start..end);
            let msg = (!l.message.is_empty()).then(|| l.message.clone());
            if l.primary {
                LabeledSpan::new_primary_with_span(msg, span)
            } else {
                LabeledSpan::new_with_span(msg, span)
            }
        })))
    }
}

/// Renders diagnostics for one file with source snippets, carets and labels.
pub fn render(diags: &[Diagnostic], file_name: &str, source: &str, style: Style) -> String {
    let theme = match style {
        Style::Color => GraphicalTheme::unicode(),
        Style::Plain => GraphicalTheme::unicode_nocolor(),
    };
    let handler = GraphicalReportHandler::new_themed(theme)
        .with_width(100)
        .with_links(false);
    let mut out = String::new();
    for diag in diags {
        let report = Report {
            diag,
            source: NamedSource::new(file_name, source.to_string()),
        };
        if handler.render_report(&mut out, &report).is_err() {
            out.push_str(&render_short(std::slice::from_ref(diag), file_name, source));
        }
        out.push('\n');
    }
    out
}

/// One line per diagnostic: `file:line:col: error[code]: message`.
pub fn render_short(diags: &[Diagnostic], file_name: &str, source: &str) -> String {
    let index = LineIndex::new(source);
    let mut out = String::new();
    for d in diags {
        let (line, col) = d
            .primary_span()
            .map_or((1, 1), |s| index.line_col(source, s.start));
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        out.push_str(&format!(
            "{file_name}:{line}:{col}: {sev}[{}]: {}",
            d.code, d.message
        ));
        if let Some(help) = &d.help {
            out.push_str(&format!(" ({help})"));
        }
        out.push('\n');
    }
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
    fn short_render_has_file_line_col() {
        let d = Diagnostic::error("syntax::expected", "expected `}`")
            .with_label(Span::new(4, 5), "here")
            .with_help("did you mean `x`?");
        let out = render_short(&[d], "bar.strand", "a\nbc\n");
        assert_eq!(
            out,
            "bar.strand:2:3: error[syntax::expected]: expected `}` (did you mean `x`?)\n"
        );
    }
}
