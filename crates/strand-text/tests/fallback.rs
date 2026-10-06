//! A family that is not installed (a theme's `"Inter"`) falls back to the
//! generic sans whole, not glyph by glyph.

use std::sync::Arc;

use strand_scene::{Font, Scale};
use strand_text::*;

fn glyphs(e: &mut TextEngine, family: &str) -> (usize, strand_scene::Rect) {
    let l = e.layout(&TextRequest {
        key: TextKey(1),
        text: "Hello 12:00".into(),
        style: TextStyle {
            font: Font {
                family: family.into(),
                size: 13.0,
                weight: 400,
            },
            line_height: None,
            align: TextAlign::default(),
            ellipsis: None,
            max_lines: None,
            spans: vec![],
        },
        max_width: None,
        scale: Scale::ONE,
    });
    (l.runs.iter().map(|r| r.glyphs.len()).sum(), l.ink)
}

#[test]
fn a_missing_family_falls_back_to_sans_serif() {
    let data = std::fs::read(test_font_path()).unwrap();
    let mut e = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    let sans = glyphs(&mut e, "sans-serif");
    assert_eq!(sans.0, 10);
    assert_eq!(glyphs(&mut e, "Inter"), sans);
    assert_eq!(glyphs(&mut e, "\"No Such Font\""), sans);
}
