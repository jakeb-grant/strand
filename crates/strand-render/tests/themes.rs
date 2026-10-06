//! Offline theme renders: the same scene under the built-in theme's base
//! tokens with three palettes (Material light and dark from the default
//! seed, Catppuccin Mocha), compared with `tests/refs/theme_*.png`. Every
//! colour reaches the pixels through the token table: palette roots,
//! derived tokens (`$surface.hi`, `$border`, `$accent.container`,
//! `$fg.muted`), a `set { }` override and the contrast guard, with text
//! that names no colour drawn in `$fg`.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test themes`.

mod common;

use common::*;
use strand_scene::*;
use strand_theme::{Options, Palette, from_seed, import};

const TOLERANCE: u8 = 3;

fn tok(path: &str) -> PropValue {
    PropValue::Token(TokenExpr::path(path))
}

fn table(p: &Palette) -> TokenTable {
    let mut t = strand_theme::defaults::base_tokens();
    p.insert_into(&mut t);
    // Text in the test font, so the renders don't depend on Inter.
    t.insert("font.ui", PropValue::Font(font(14.0)));
    t
}

fn scene(p: &Palette) -> SceneDiff {
    let mut b = Builder::default();
    b.diff.set_tokens(table(p), Transition::Instant);
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("surface"))]);
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(y)),
            (Prop::Width, num(w)),
            (Prop::Height, num(h)),
            (Prop::Radius, tok("radius.md")),
        ]
    };
    let mut card = rect(8.0, 8.0, 120.0, 56.0);
    card.push((Prop::Bg, tok("surface.hi")));
    card.push((
        Prop::Border,
        PropValue::Token(TokenExpr::Template {
            value: Box::new(PropValue::Border(Border {
                width: 1.0,
                paint: Paint::Solid(Color::TRANSPARENT),
            })),
            colors: vec![Some(TokenExpr::path("border"))],
        }),
    ));
    b.node(NodeKind::Box, Some(root), card);
    let mut pill = rect(136.0, 8.0, 56.0, 24.0);
    pill.push((Prop::Bg, tok("accent")));
    b.node(NodeKind::Box, Some(root), pill);
    let mut soft = rect(136.0, 40.0, 56.0, 24.0);
    soft.push((Prop::Bg, tok("accent.container")));
    b.node(NodeKind::Box, Some(root), soft);
    // Text with no colour: `$fg`.
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(16.0)),
            (Prop::Y, num(14.0)),
            (Prop::Text, text("Strand")),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(16.0)),
            (Prop::Y, num(38.0)),
            (Prop::Text, text("muted")),
            (Prop::Color, tok("fg.muted")),
        ],
    );
    // `set { $surface: $surface.mix($accent, 85%) }`: an accent-coloured
    // subtree, its `$fg` text kept readable by the contrast guard.
    let mut set = TokenTable::default();
    set.insert(
        "surface",
        PropValue::Token(TokenExpr::path("surface").call(
            TokenMethod::Mix,
            vec![TokenExpr::path("accent"), TokenExpr::value(num(0.85))],
        )),
    );
    let mut sub = rect(200.0, 8.0, 112.0, 56.0);
    sub.push((Prop::Tokens, PropValue::Tokens(Box::new(set))));
    sub.push((Prop::Bg, tok("surface")));
    let subtree = b.node(NodeKind::Box, Some(root), sub);
    b.node(
        NodeKind::Text,
        Some(subtree),
        vec![
            (Prop::X, num(8.0)),
            (Prop::Y, num(18.0)),
            (Prop::Text, text("guarded")),
            (Prop::Color, tok("fg")),
        ],
    );
    b.diff
}

fn render(p: &Palette) -> Buffer {
    let mut r = renderer();
    assert!(r.apply(scene(p)).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(SurfaceId(1), root);
    let mut buf = Buffer::new(320, 72, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    buf
}

fn bgra(c: Color) -> [u8; 4] {
    let [r, g, b, a] = c.to_rgba8();
    [b, g, r, a]
}

fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

/// The pixel in `x0..x1, y0..y1` that differs most from `bg`: the ink.
fn ink(buf: &Buffer, (x0, x1): (u32, u32), (y0, y1): (u32, u32), bg: [u8; 4]) -> [u8; 4] {
    let mut best = bg;
    let mut most = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = buf.px(x, y);
            let d: u32 = p.iter().zip(bg).map(|(a, b)| a.abs_diff(b) as u32).sum();
            if d > most {
                most = d;
                best = p;
            }
        }
    }
    best
}

fn luma(p: [u8; 4]) -> Color {
    Color::from_rgba8(p[2], p[1], p[0], 255)
}

fn check(name: &str, p: &Palette) {
    let buf = render(p);
    let t = table(p);
    let col = |path: &str| match t.lookup(path) {
        Some(PropValue::Color(c)) => c,
        other => panic!("{path}: {other:?}"),
    };
    // Roots and derived tokens, exactly.
    assert!(
        close(buf.px(2, 70), bgra(col("surface")), 1),
        "{name}: surface"
    );
    assert!(
        close(buf.px(100, 50), bgra(col("surface.hi")), 1),
        "{name}: surface.hi"
    );
    assert!(
        close(buf.px(164, 20), bgra(col("accent")), 1),
        "{name}: accent"
    );
    let soft = col("accent.container").over(col("surface"));
    assert!(
        close(buf.px(164, 52), bgra(soft), 2),
        "{name}: accent.container"
    );
    // Text with no colour is `$fg`: its ink is the fg end.
    let hi = bgra(col("surface.hi"));
    let fg_ink = luma(ink(&buf, (16, 70), (14, 32), hi));
    let fg = col("fg");
    assert!(
        (fg_ink.relative_luminance() - fg.relative_luminance()).abs() < 0.08,
        "{name}: text ink {fg_ink:?} vs fg {fg:?}"
    );
    // Inside the override the text is solved against the new surface.
    let surface = col("surface")
        .lerp_oklab(col("accent"), 0.85)
        .gamut_mapped();
    assert!(
        close(buf.px(204, 60), bgra(surface), 2),
        "{name}: overridden surface"
    );
    // The guard's answer, exactly: `$fg` in the override's scope keeps
    // 3:1 over the overridden surface...
    let mut set = TokenTable::default();
    set.insert(
        "surface",
        PropValue::Token(TokenExpr::path("surface").call(
            TokenMethod::Mix,
            vec![TokenExpr::path("accent"), TokenExpr::value(num(0.85))],
        )),
    );
    let levels = [&t, &set];
    let Some(PropValue::Color(fg_in)) = TokenScope::new(&levels).lookup("fg") else {
        panic!("{name}: no fg in the subtree")
    };
    assert!(
        fg_in.contrast(surface) >= MIN_CONTRAST - 1e-6,
        "{name}: guarded fg {:.2}:1",
        fg_in.contrast(surface)
    );
    // ...and that is what was drawn (antialiased stems at 14 px never
    // reach the full colour, so the ink is only on the right side).
    let guarded = luma(ink(&buf, (208, 300), (26, 44), bgra(surface)));
    assert_eq!(
        guarded.relative_luminance() > surface.relative_luminance(),
        fg_in.relative_luminance() > surface.relative_luminance(),
        "{name}: ink {guarded:?}"
    );
    assert!(guarded.contrast(surface) >= 2.0, "{name}");
    assert_matches_ref(name, &buf, TOLERANCE);
}

#[test]
fn the_built_in_theme_in_three_palettes() {
    let seed = hex(strand_theme::defaults::DEFAULT_SEED);
    check("theme_light", &from_seed(seed, Options::default()));
    check(
        "theme_dark",
        &from_seed(
            seed,
            Options {
                dark: true,
                ..Options::default()
            },
        ),
    );
    check("theme_mocha", &import("catppuccin:mocha", None).unwrap());
}
