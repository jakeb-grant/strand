//! `markup: basic` and `markup: html` as text spans. The reading itself is
//! [`strand_scene::markup`]'s, shared with logic's `plain(markup)` and
//! `links(markup)` (decisions.md, doc-formats): this maps its runs to the
//! text engine's [`TextSpan`]s. Links are underlined and painted `$accent`
//! (not clickable); bold is weight 700; code is shown in `$font.mono`'s
//! family, then the generic monospace; struck text is struck through.

use std::sync::Arc;

use strand_scene::Color;
use strand_scene::markup::{Level, Marked, parse};
use strand_text::{MAX_TEXT_BYTES, TextSpan};

/// The text `src` shows read as `level`, and its spans: links painted
/// `link` (when given), code in `mono` (a family list ending in a generic
/// family). Parsing stops at what the text engine shapes.
pub fn spans(
    src: &str,
    level: Level,
    link: Option<Color>,
    mono: &Arc<str>,
) -> (String, Vec<TextSpan>) {
    let Marked { text, runs, .. } = parse(src, level, Some(MAX_TEXT_BYTES));
    let spans = runs
        .into_iter()
        .map(|r| TextSpan {
            range: r.range,
            weight: r.bold.then_some(700),
            italic: r.italic,
            underline: r.underline || r.link,
            strikethrough: r.strike,
            family: r.mono.then(|| mono.clone()),
            color: if r.link { link } else { None },
        })
        .collect();
    (text, spans)
}

/// The family code is shown in: `$font.mono`'s (`family`), then the
/// generic monospace.
pub fn mono_family(family: Option<&str>) -> Arc<str> {
    match family.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("{f}, monospace").into(),
        None => "monospace".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// basic's runs map to the spans it always made: bold 700, links
    /// underlined in the accent, nothing else styled.
    #[test]
    fn runs_become_spans() {
        let accent = Color::WHITE;
        let mono = mono_family(None);
        let (t, sp) = spans(
            "see <a href=x>docs <b>now</b></a>",
            Level::Basic,
            Some(accent),
            &mono,
        );
        assert_eq!(t, "see docs now");
        assert_eq!(sp.len(), 2);
        assert!(sp.iter().all(|s| s.underline && s.color == Some(accent)));
        assert_eq!(sp[1].weight, Some(700));
        assert!(sp.iter().all(|s| !s.strikethrough && s.family.is_none()));
        let mono = mono_family(Some("JetBrains Mono"));
        assert_eq!(&*mono, "JetBrains Mono, monospace");
        let (t, sp) = spans("<s>old</s> <code>x</code>", Level::Html, None, &mono);
        assert_eq!(t, "old x");
        assert!(sp[0].strikethrough && sp[0].family.is_none() && !sp[0].underline);
        assert_eq!(sp[1].family.as_deref(), Some("JetBrains Mono, monospace"));
        assert_eq!(sp[1].color, None);
    }
}
