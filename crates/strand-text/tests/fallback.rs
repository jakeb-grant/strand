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

/// With the system's fonts: a missing monospace family (`$font.mono`'s
/// `"JetBrains Mono"`) falls back to the system monospace, so every
/// glyph has the same advance.
#[test]
fn a_missing_monospace_family_stays_monospace() {
    let mut e = TextEngine::new(FontConfig::default());
    let width = |e: &mut TextEngine, text: &str| {
        e.layout(&TextRequest {
            key: TextKey(2),
            text: text.into(),
            style: TextStyle {
                font: Font {
                    family: "\"No Such Mono\"".into(),
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
        })
        .size
        .w
    };
    let (narrow, wide) = (width(&mut e, "iiii"), width(&mut e, "MMMM"));
    if narrow == 0.0 {
        eprintln!("no system fonts: skipped");
        return;
    }
    assert!((narrow - wide).abs() < 0.5, "iiii {narrow} vs MMMM {wide}");
}

/// A medium request (500) on a family with only a regular face draws
/// the regular face, not faux bold (CSS `font-synthesis-weight`: only a
/// bold request, 600 and up, emboldens a face lighter than 600). A
/// theme's `$font.ui` is Inter 500, which falls back to a sans with
/// only 400 and 700 faces on most machines.
#[test]
fn medium_is_not_synthesised_bold() {
    let data = std::fs::read(test_font_path()).unwrap();
    let mut e = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    let ink = |e: &mut TextEngine, weight: u16| {
        e.layout(&TextRequest {
            key: TextKey(3),
            text: "Hello 12:00".into(),
            style: TextStyle {
                font: Font {
                    family: "sans-serif".into(),
                    size: 13.0,
                    weight,
                },
                line_height: None,
                align: TextAlign::default(),
                ellipsis: None,
                max_lines: None,
                spans: vec![],
            },
            max_width: None,
            scale: Scale::ONE,
        })
        .ink
    };
    let regular = ink(&mut e, 400);
    assert_eq!(ink(&mut e, 500), regular, "500 draws the 400 face as is");
    assert_ne!(ink(&mut e, 700), regular, "700 is emboldened");
}
