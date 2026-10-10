//! (M4) Blurred album art (design.md: `image media.art { fit: cover;
//! filter: blur(30) }`, "cached"): the image drawn through a 30 px blur
//! in a cached offscreen group, built once per art and kept while the
//! rest of the panel repaints. The `filter:` prop lowering is
//! S-effects-paint's; until it lands here the group's effects come
//! through the `set_layer_effects` seam it replaces. Offline PNG.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test album_art`.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const TOLERANCE: u8 = 2;

fn art(name: &str, f: impl Fn(u32, u32) -> [u8; 4]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-art-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    let mut rgba = Vec::new();
    for y in 0..64 {
        for x in 0..64 {
            rgba.extend_from_slice(&f(x, y));
        }
    }
    write_png(&p, 64, 64, &rgba);
    p
}

fn url(p: &std::path::Path) -> PropValue {
    text(&format!("file://{}", p.display()))
}

#[test]
fn blurred_album_art_is_built_once_per_art() {
    // Four quadrants: the blur mixes them at the centre.
    let first = art("first.png", |x, y| match (x < 32, y < 32) {
        (true, true) => [0xf3, 0x8b, 0xa8, 255],
        (false, true) => [0x89, 0xb4, 0xfa, 255],
        (true, false) => [0xa6, 0xe3, 0xa1, 255],
        (false, false) => [0xf9, 0xe2, 0xaf, 255],
    });
    let second = art("second.png", |_, _| [0x89, 0xb4, 0xfa, 255]);
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(160.0)),
            (Prop::Height, num(100.0)),
            (Prop::Bg, color("#1e1e2e")),
            (Prop::Clip, PropValue::Bool(true)),
        ],
    );
    let stack = b.node(NodeKind::Stack, Some(root), vec![(Prop::Grow, num(1.0))]);
    let img = b.node(
        NodeKind::Image,
        Some(stack),
        vec![
            (Prop::Source, url(&first)),
            (Prop::Fit, PropValue::Keyword("cover".into())),
            (Prop::Width, num(160.0)),
            (Prop::Height, num(100.0)),
        ],
    );
    // A dot over the art that moves (the panel's other content).
    let dot = b.node(
        NodeKind::Box,
        Some(stack),
        vec![
            (Prop::Size, num(8.0)),
            (Prop::X, num(10.0)),
            (Prop::Y, num(10.0)),
            (Prop::Bg, color("#ffffff")),
            (Prop::Radius, num(4.0)),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.set_layer_effects(img, vec![Effect::Blur { radius: 30.0 }]);
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(160, 100, Scale::ONE);
    let t0 = Duration::from_secs(1);
    let mut at = t0;
    buf.paint_at(&mut r, S, 0, at);
    let mut k = 0;
    while r.wants_frame(S) {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
        k += 1;
        assert!(k < 50, "the art settles");
    }
    assert_matches_ref("album_art_blur", &buf, TOLERANCE);
    let built = r.offscreen_cache().1;
    assert!(built >= 1, "the blurred art is an offscreen group");
    // Blurred: the centre mixes all four quadrants, no quadrant's own
    // colour survives there.
    let [bb, g, rr, _] = buf.px(80, 50);
    for c in [
        [0xf3, 0x8b, 0xa8],
        [0x89, 0xb4, 0xfa],
        [0xa6, 0xe3, 0xa1],
        [0xf9, 0xe2, 0xaf],
    ] {
        let d = (rr as i32 - c[0]).abs() + (g as i32 - c[1]).abs() + (bb as i32 - c[2]).abs();
        assert!(d > 30, "centre {:?} is quadrant {c:?}", (rr, g, bb));
    }

    // The dot moves over it ten times: the art is never built again.
    for i in 1..=10 {
        let mut d = SceneDiff::default();
        d.set(dot, Prop::X, num(10.0 + 12.0 * i as f32));
        assert!(r.apply(d).is_empty());
        let mut k = 0;
        while r.wants_frame(S) {
            at += Duration::from_millis(16);
            buf.paint_at(&mut r, S, 1, at);
            k += 1;
            assert!(k < 100, "the dot settles");
        }
    }
    assert_eq!(r.offscreen_cache().1, built, "cached while the dot moves");

    // New art: built once more.
    let mut d = SceneDiff::default();
    d.set(img, Prop::Source, url(&second));
    assert!(r.apply(d).is_empty());
    let mut k = 0;
    while r.wants_frame(S) {
        at += Duration::from_millis(16);
        buf.paint_at(&mut r, S, 1, at);
        k += 1;
        assert!(k < 100, "the new art settles");
    }
    assert!(r.offscreen_cache().1 > built, "the new art is built");
    let [bb, g, rr, _] = buf.px(80, 50);
    // The new art's blue, darkened a little where the blur reaches past
    // its edges (as CSS's `blur()` fades a box's edges).
    let near = |v: u8, c: i32| (v as i32 - c).abs() < 32;
    assert!(
        near(rr, 0x89) && near(g, 0xb4) && near(bb, 0xfa) && bb > g && g > rr,
        "{:?}",
        (rr, g, bb)
    );
}
