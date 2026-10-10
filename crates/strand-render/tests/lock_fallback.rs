//! Offline PNG tests of the built-in fallback lock
//! (`strand_render::lock_fallback`; design.md, "Lock screen": "if
//! anything faults, a built-in password field appears"): the field empty,
//! with four dots, checking and refused, at 1× and 1.25×. Nothing here
//! starts a text worker or builds a renderer: the fallback draws without
//! either.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test lock_fallback`.

mod common;

use common::*;
use strand_render::lock_fallback::{Action, FieldState, LockFallback};
use strand_scene::{ButtonState, KeyInput, Modifiers, PaintTarget, Scale};

const TOLERANCE: u8 = 2;

fn press(name: &str, text: &str) -> KeyInput {
    KeyInput {
        name: name.into(),
        text: text.into(),
        state: ButtonState::Pressed,
        repeat: false,
        modifiers: Modifiers::default(),
        time: 0,
    }
}

/// A 480×200 logical surface at `scale`, painted by `f`.
fn paint(f: &LockFallback, scale: Scale) -> Buffer {
    let size = scale.physical_size(strand_scene::LogicalSize::new(480.0, 200.0));
    let mut buf = Buffer::new(size.w, size.h, scale);
    let mut t = PaintTarget::new(&mut buf.pixels, size, size.w * 4, scale, 0).unwrap();
    let damage = f.paint(&mut t);
    assert_eq!(damage, strand_scene::Damage::full(size), "every pixel");
    buf
}

/// The background everywhere off the field, opaque.
fn assert_opaque(buf: &Buffer) {
    assert!(
        buf.pixels.chunks_exact(4).all(|p| p[3] == 255),
        "the fallback hides the desktop: every pixel is opaque"
    );
}

/// Blue-ish (the checking border) or red-ish (refused) pixels on the
/// field's top edge, at its middle column.
fn border_at_top(buf: &Buffer, scale: f32) -> [u8; 4] {
    let x = buf.size.w / 2;
    let top = (buf.size.h as f32 / 2.0 - 24.0 * scale) as u32;
    buf.px(x, top + 1)
}

#[test]
fn the_fallback_field_is_empty_then_shows_four_dots() {
    for (num, name) in [(120, "1x"), (150, "1_25x")] {
        let scale = Scale::new(num).unwrap();
        let mut f = LockFallback::new();
        let empty = paint(&f, scale);
        assert_opaque(&empty);
        assert_matches_ref(&format!("lock_fallback_empty_{name}"), &empty, TOLERANCE);
        for c in ["a", "b", "c", "d"] {
            assert_eq!(f.key(&press(c, c)), Action::Changed);
        }
        let four = paint(&f, scale);
        assert_opaque(&four);
        assert_ne!(empty.pixels, four.pixels, "the dots draw");
        // The four dots sit on the centre row, symmetric about the middle.
        let (cy, cx) = (four.size.h / 2, four.size.w / 2);
        let lit = |x: u32| four.px(x, cy)[2] > 0xa0;
        let dots: Vec<u32> = (1..four.size.w)
            .filter(|&x| lit(x) && !lit(x - 1))
            .collect();
        assert_eq!(dots.len(), 4, "{name}: four dots start at {dots:?}");
        assert!(!lit(cx), "an even count leaves the middle between dots");
        assert_matches_ref(&format!("lock_fallback_dots_{name}"), &four, TOLERANCE);
    }
}

#[test]
fn checking_and_refused_tint_the_field() {
    for (num, name) in [(120, "1x"), (150, "1_25x")] {
        let scale = Scale::new(num).unwrap();
        let s = scale.as_f32();
        let mut f = LockFallback::new();
        for c in ["p", "w"] {
            f.key(&press(c, c));
        }
        let Action::Submit(bytes) = f.key(&press("Return", "\r")) else {
            panic!("Return submits");
        };
        assert_eq!(bytes, b"pw");
        assert_eq!(f.state(), FieldState::Checking);
        let checking = paint(&f, scale);
        let [b, _, r, _] = border_at_top(&checking, s);
        assert!(b > r, "{name}: a blue border while checking");
        assert!(f.set_state(FieldState::Failed));
        let failed = paint(&f, scale);
        assert_opaque(&failed);
        let [b, _, r, _] = border_at_top(&failed, s);
        assert!(r > b, "{name}: a red border once refused");
        assert_matches_ref(&format!("lock_fallback_failed_{name}"), &failed, TOLERANCE);
    }
}
