//! Font weights against the faces a family has (CSS `font-synthesis-weight`,
//! docs/decisions.md "wave3-theme (review 3)" and "wave4-core: faux bold
//! reviewed"). design.md's `$font.ui` is Inter 500 and `$font.title` Inter
//! 600; where Inter is missing, the sans that stands in for it usually has
//! only a 400 and a 700 face. A 500 label draws the 400 face as it is; a
//! 600 or 700 label draws the real bold face when there is one, and the
//! regular face emboldened only when there is not.

mod common;

use std::sync::Arc;

use common::*;
use strand_render::{Renderer, TextBackend};
use strand_scene::*;
use strand_text::{FontConfig, TextEngine, test_bold_font_path, test_font_path};

const TOLERANCE: u8 = 3;
const S: SurfaceId = SurfaceId(1);
const WEIGHTS: [u16; 4] = [400, 500, 600, 700];
const W: u32 = 180;
const ROW: u32 = 26;

/// A renderer over the vendored test family: its regular face, plus its
/// bold face when `bold`.
fn renderer_with(bold: bool) -> Renderer {
    let mut faces = vec![Arc::new(std::fs::read(test_font_path()).unwrap())];
    if bold {
        faces.push(Arc::new(std::fs::read(test_bold_font_path()).unwrap()));
    }
    let engine = TextEngine::new(FontConfig::isolated(faces));
    Renderer::new(TextBackend::Inline(Box::new(engine)))
}

/// A `W`-wide panel with one `ROW`-high row per weight, each a label at
/// that weight, painted at 1×.
fn paint(bold: bool, weights: &[u16]) -> Buffer {
    let h = ROW * weights.len() as u32;
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(W as f32)),
            (Prop::Height, num(h as f32)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Color, color("#cdd6f4")),
            (Prop::Font, PropValue::Font(font(15.0))),
        ],
    );
    let col = b.node(NodeKind::Col, Some(root), vec![]);
    for &w in weights {
        let row = b.node(
            NodeKind::Row,
            Some(col),
            vec![(Prop::Height, num(ROW as f32)), (Prop::Pad, num(4.0))],
        );
        b.node(
            NodeKind::Text,
            Some(row),
            vec![
                (Prop::Text, text(&format!("Volume 42% · {w}"))),
                (Prop::Weight, num(w as f32)),
            ],
        );
    }
    let mut r = renderer_with(bold);
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    let mut buf = Buffer::new(W, h, Scale::ONE);
    buf.paint(&mut r, S, 0);
    buf
}

/// The pixels of one label, without its weight digits: the same text at
/// two weights compares equal exactly when it drew the same glyphs.
fn label(bold: bool, weight: u16) -> Vec<u8> {
    let buf = paint(bold, &[weight]);
    // "Volume 42%" ends before x = 90 at 15 px even in bold; the weight
    // digits after it differ, so only the part before them is compared.
    let mut out = Vec::new();
    for y in 0..ROW {
        let i = (y * W * 4) as usize;
        out.extend_from_slice(&buf.pixels[i..i + 90 * 4]);
    }
    out
}

/// A family with only a regular face: 500 draws it unchanged (no faux
/// bold), 600 and 700 draw it emboldened.
#[test]
fn medium_on_a_regular_only_family_draws_the_regular_face() {
    assert_matches_ref("weights_regular", &paint(false, &WEIGHTS), TOLERANCE);
    let regular = label(false, 400);
    assert_eq!(label(false, 500), regular, "500 is not faux bold");
    assert_ne!(label(false, 600), regular, "600 is emboldened");
    assert_ne!(label(false, 700), regular, "700 is emboldened");
}

/// A family with a 400 and a 700 face (DejaVu Sans, Liberation Sans): 500
/// draws the 400 face unchanged; 600 and 700 draw the 700 face, which is
/// not emboldened again.
#[test]
fn medium_on_a_regular_and_bold_family_draws_the_regular_face() {
    assert_matches_ref("weights_regular_bold", &paint(true, &WEIGHTS), TOLERANCE);
    let regular = label(true, 400);
    assert_eq!(label(true, 500), regular, "500 draws the 400 face as is");
    assert_eq!(
        label(false, 400),
        regular,
        "400 is the same face either way"
    );
    let bold = label(true, 700);
    assert_ne!(bold, regular, "700 draws the bold face");
    assert_eq!(label(true, 600), bold, "600 draws the 700 face too");
    assert_ne!(
        bold,
        label(false, 700),
        "the real bold face, not the regular emboldened"
    );
}
