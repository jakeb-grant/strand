//! Compiler diagnostics as LSP diagnostics, and quick fixes for the
//! did-you-mean ones.

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionKind, DiagnosticRelatedInformation, DiagnosticSeverity, Location,
    NumberOrString, TextEdit, Uri, WorkspaceEdit,
};
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

/// The suggestion of a "did you mean `x`?" help.
fn suggestion(help: &str) -> Option<&str> {
    help.strip_prefix("did you mean `")?.strip_suffix("`?")
}

/// What to replace for a suggestion: the whole name, or the letters of a
/// misspelt unit (`12pz` → `px`).
fn fix_span(src: &str, span: Span, suggestion: &str) -> Option<Span> {
    let text = span.text(src);
    if text.is_empty() || text.contains(['\n', '\r']) {
        return None;
    }
    let first = text.as_bytes()[0];
    if first.is_ascii_digit() && !suggestion.starts_with(|c: char| c.is_ascii_digit()) {
        let letters = text.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.');
        let start = span.end - letters.len() as u32;
        return (!letters.is_empty()).then_some(Span::new(start, span.end));
    }
    let name_like = text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_$@.-".contains(&b));
    name_like.then_some(span)
}

/// Quick fixes for the did-you-mean diagnostics in `file` touching `range`.
pub fn quick_fixes(an: &Analysis, file: FileId, range: Span) -> Vec<CodeAction> {
    let src = an.text(file);
    let Some(file_uri) = uri(an.uri(file)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in &an.compiled.diagnostics {
        let Some(l) = d.primary().filter(|l| l.file == file) else {
            continue;
        };
        if l.span.end < range.start || l.span.start > range.end {
            continue;
        }
        let Some(s) = d.help.as_deref().and_then(suggestion) else {
            continue;
        };
        let Some(at) = fix_span(src, l.span, s) else {
            continue;
        };
        let edit = TextEdit {
            range: an.range(file, at),
            new_text: s.to_string(),
        };
        out.push(CodeAction {
            title: format!("Change to `{s}`"),
            kind: Some(CodeActionKind::QUICKFIX),
            diagnostics: Some(vec![convert(an, d)]),
            edit: Some(WorkspaceEdit {
                changes: Some(HashMap::from([(file_uri.clone(), vec![edit])])),
                ..WorkspaceEdit::default()
            }),
            is_preferred: Some(true),
            ..CodeAction::default()
        });
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
        assert_eq!(suggestion("did you mean `x`?"), Some("x"));
        assert_eq!(suggestion("use commas"), None);
    }
}
