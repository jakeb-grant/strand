//! Compiler diagnostics as LSP diagnostics, and quick fixes for the
//! did-you-mean ones.

use lsp_types::{DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Uri};
use strand_compiler::FileId;
use strand_compiler::diagnostic::{Diagnostic, Severity, suggest};
use strand_compiler::hir::ElementKind;
use strand_compiler::syntax::Span;

use crate::walk;
use crate::workspace::Analysis;

fn uri(s: &str) -> Option<Uri> {
    s.parse().ok()
}

/// The diagnostics whose primary label is in `file`.
pub fn for_file(an: &Analysis, file: FileId) -> Vec<lsp_types::Diagnostic> {
    an.compiled
        .diagnostics
        .iter()
        .filter(|d| d.primary().is_some_and(|l| l.file == file))
        .map(|d| convert(an, d))
        .collect()
}

fn convert(an: &Analysis, d: &Diagnostic) -> lsp_types::Diagnostic {
    let primary = d.primary();
    let (file, span) = primary.map_or((FileId::default(), Span::default()), |l| (l.file, l.span));
    let mut message = d.message.clone();
    if let Some(l) = primary.filter(|l| !l.message.is_empty() && l.message != d.message) {
        message.push_str(&format!(" ({})", l.message));
    }
    if let Some(h) = &d.help {
        message.push_str(&format!("\nhelp: {h}"));
    }
    let related: Vec<DiagnosticRelatedInformation> = d
        .labels
        .iter()
        .filter(|l| !l.primary)
        .filter_map(|l| {
            Some(DiagnosticRelatedInformation {
                location: Location::new(uri(an.uri(l.file))?, an.range(l.file, l.span)),
                message: l.message.clone(),
            })
        })
        .collect();
    lsp_types::Diagnostic {
        range: an.range(file, span),
        severity: Some(match d.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
        }),
        code: Some(NumberOrString::String(d.code.to_string())),
        source: Some("strand".into()),
        message,
        related_information: (!related.is_empty()).then_some(related),
        ..lsp_types::Diagnostic::default()
    }
}

/// The suggestion of a "did you mean `x`?" help.
fn suggestion(help: &str) -> Option<&str> {
    help.strip_prefix("did you mean `")?.strip_suffix("`?")
}

/// The names of an "it takes `a`, `b`" help (a component's parameters).
fn takes(help: &str) -> Vec<&str> {
    help.strip_prefix("it takes ")
        .map(|rest| {
            rest.split(", ")
                .filter_map(|n| n.strip_prefix('`')?.strip_suffix('`'))
                .collect()
        })
        .unwrap_or_default()
}

/// `word` could be a misspelling of `suggestion`.
fn plausible(word: &str, suggestion: &str) -> bool {
    word != suggestion && suggest(word, [suggestion]) == Some(suggestion)
}

fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_$@.-".contains(&b))
}

/// What to replace for a suggestion: the misspelt name, or the letters of
/// a misspelt unit (`12pz` → `px`). A diagnostic's span is not always the
/// misspelt word (`on chnage a, b` points at `a`), so the replaced text
/// must be a plausible misspelling of the suggestion: the span, its last
/// segment after a `.`, or the word just before it on its line.
fn fix_span(src: &str, span: Span, suggestion: &str) -> Option<Span> {
    let text = span.text(src);
    if text.is_empty() || text.contains(['\n', '\r']) {
        return None;
    }
    let first = text.as_bytes()[0];
    if first.is_ascii_digit() && !suggestion.starts_with(|c: char| c.is_ascii_digit()) {
        let letters = text.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let start = span.end - letters.len() as u32;
        return (!letters.is_empty() && plausible(letters, suggestion))
            .then_some(Span::new(start, span.end));
    }
    if is_name(text) && plausible(text, suggestion) {
        return Some(span);
    }
    if let Some((_, last)) = text.rsplit_once('.')
        && !suggestion.contains('.')
        && is_name(last)
        && plausible(last, suggestion)
    {
        return Some(Span::new(span.end - last.len() as u32, span.end));
    }
    // The word before the span, on the same line.
    let before = src.get(..span.start as usize)?;
    let trimmed = before.trim_end_matches([' ', '\t']);
    let start = trimmed
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(0, |i| i + 1);
    let word = &trimmed[start..];
    (is_name(word) && plausible(word, suggestion))
        .then(|| Span::new(start as u32, trimmed.len() as u32))
}

/// A quick fix: replace `span` in the diagnostic's file with `new_text`.
pub struct QuickFix {
    pub title: String,
    pub diagnostic: lsp_types::Diagnostic,
    pub span: Span,
    pub new_text: String,
    pub preferred: bool,
}

/// The parameters already given in the call holding `at` (the props of
/// the call of component `name` whose element spans it).
fn given_params(an: &Analysis, file: FileId, at: Span) -> Vec<String> {
    let p = &an.compiled.program;
    walk::prop_lists(p)
        .into_iter()
        .filter(|(f, owner, _)| {
            *f == file
                && owner.is_some_and(|o| {
                    matches!(o.kind, ElementKind::Component(_))
                        && o.span.start <= at.start
                        && at.end <= o.span.end
                })
        })
        .min_by_key(|(_, owner, _)| owner.map_or(u32::MAX, |o| o.span.len()))
        .map(|(_, _, props)| props.iter().map(|q| q.name.clone()).collect())
        .unwrap_or_default()
}

/// Quick fixes for the did-you-mean diagnostics in `file` touching `range`,
/// and for an unknown parameter of a component call (one per parameter
/// the call does not give yet; design.md "What you see" #2).
pub fn quick_fixes(an: &Analysis, file: FileId, range: Span) -> Vec<QuickFix> {
    let src = an.text(file);
    let mut out = Vec::new();
    for d in &an.compiled.diagnostics {
        let Some(l) = d.primary().filter(|l| l.file == file) else {
            continue;
        };
        if l.span.end < range.start || l.span.start > range.end {
            continue;
        }
        let Some(help) = d.help.as_deref() else {
            continue;
        };
        if let Some(s) = suggestion(help) {
            if let Some(at) = fix_span(src, l.span, s) {
                out.push(QuickFix {
                    title: format!("Change to `{s}`"),
                    diagnostic: convert(an, d),
                    span: at,
                    new_text: s.to_string(),
                    preferred: true,
                });
            }
            continue;
        }
        if d.code == "check::unknown_param" && is_name(l.span.text(src)) {
            let given = given_params(an, file, l.span);
            let open: Vec<&str> = takes(help)
                .into_iter()
                .filter(|n| !given.iter().any(|g| g == n))
                .collect();
            let only = open.len() == 1;
            for n in open {
                out.push(QuickFix {
                    title: format!("Change to `{n}`"),
                    diagnostic: convert(an, d),
                    span: l.span,
                    new_text: n.to_string(),
                    preferred: only,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_replace_the_misspelt_part() {
        let src = "a: critcal; w: 12pz; $fg.mutd";
        let at = |w: &str| {
            let s = src.find(w).unwrap() as u32;
            Span::new(s, s + w.len() as u32)
        };
        assert_eq!(
            fix_span(src, at("critcal"), "critical"),
            Some(at("critcal"))
        );
        assert_eq!(fix_span(src, at("12pz"), "px"), Some(at("pz")));
        assert_eq!(
            fix_span(src, at("$fg.mutd"), "$fg.muted"),
            Some(at("$fg.mutd"))
        );
        assert_eq!(fix_span(src, at("a: critcal"), "x"), None);
        // The span is the word after the misspelt one.
        let src2 = "on chnage a, b { x = 1 }";
        let a = Span::new(10, 11);
        assert_eq!(fix_span(src2, a, "change"), Some(Span::new(3, 9)));
        assert_eq!(fix_span(src2, a, "zebra"), None);
        assert_eq!(takes("it takes `open`, `day`"), vec!["open", "day"]);
        assert_eq!(suggestion("did you mean `x`?"), Some("x"));
        assert_eq!(suggestion("use commas"), None);
    }
}
