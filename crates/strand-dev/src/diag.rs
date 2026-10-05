//! Compiler diagnostics as LSP diagnostics, and quick fixes from the
//! replacements they propose.

use lsp_types::{DiagnosticRelatedInformation, DiagnosticSeverity, Location, NumberOrString, Uri};
use strand_compiler::FileId;
use strand_compiler::diagnostic::{Diagnostic, Severity};
use strand_compiler::syntax::Span;

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

/// A quick fix: replace `span` in the diagnostic's file with `new_text`.
pub struct QuickFix {
    pub title: String,
    pub diagnostic: lsp_types::Diagnostic,
    pub span: Span,
    pub new_text: String,
    pub preferred: bool,
}

/// Quick fixes for the diagnostics in `file` touching `range`: one per
/// replacement the compiler proposes ([`Diagnostic::suggestions`]), so a
/// "did you mean `x`?" gives one preferred fix and an unknown parameter
/// one fix per parameter its call does not set yet (design.md "What you
/// see" #2). The help text is never parsed.
pub fn quick_fixes(an: &Analysis, file: FileId, range: Span) -> Vec<QuickFix> {
    let mut out = Vec::new();
    for d in &an.compiled.diagnostics {
        let Some(l) = d.primary().filter(|l| l.file == file) else {
            continue;
        };
        let touches = |s: Span| s.end >= range.start && s.start <= range.end;
        if !touches(l.span) && !d.suggestions.iter().any(|s| touches(s.span)) {
            continue;
        }
        let only = d.suggestions.len() == 1;
        for s in d.suggestions.iter().filter(|s| s.file == file) {
            out.push(QuickFix {
                title: format!("Change to `{}`", s.replacement),
                diagnostic: convert(an, d),
                span: s.span,
                new_text: s.replacement.clone(),
                preferred: only,
            });
        }
    }
    out
}
