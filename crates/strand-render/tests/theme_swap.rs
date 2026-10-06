//! Theme swaps on the render thread (design.md, "How a swap animates"):
//! only palette roots spring, in OKLab; derived tokens are re-evaluated
//! from them every frame; declared text/background pairs stay at 3:1 or
//! better in every frame, and a swap no spring can keep readable
//! crossfades from a snapshot of the old frame instead; fonts snap, and
//! `reduced_motion` snaps everything. Offline frames at fixed timestamps
//! compare with `tests/refs/theme_swap.png` and `theme_crossfade.png`.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test theme_swap`.

mod common;

use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;
use strand_theme::{Options, Palette, Partial, Role, Variant, from_seed, import};

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// Frame `k` after `T0` at `hz`.
fn at(k: u32, hz: u32) -> Duration {
    T0 + Duration::from_nanos(1_000_000_000 * k as u64 / hz as u64)
}

fn frame(k: u32) -> Duration {
    at(k, 60)
}

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

fn seed() -> Color {
    hex(strand_theme::defaults::DEFAULT_SEED)
}

fn material(seed: Color, dark: bool) -> Palette {
    from_seed(
        seed,
        Options {
            dark,
            ..Options::default()
        },
    )
}

/// The scene of `tests/themes.rs`: a `$surface` panel, a `$surface.hi`
/// card with a `$border`, an `$accent` pill, an `$accent.container` one,
/// text in `$fg` and `$fg.muted`, and a `set { $surface: … }` subtree
/// whose `$fg` the guard solves against it.
fn scene(t: TokenTable) -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    b.diff.set_tokens(t, Transition::Instant);
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
    let pill = b.node(NodeKind::Box, Some(root), pill);
    b.node(
        NodeKind::Text,
        Some(pill),
        vec![
            (Prop::X, num(8.0)),
            (Prop::Y, num(4.0)),
            (Prop::Text, text("on")),
            (Prop::Color, tok("on_accent")),
        ],
    );
    let mut soft = rect(136.0, 40.0, 56.0, 24.0);
    soft.push((Prop::Bg, tok("accent.container")));
    b.node(NodeKind::Box, Some(root), soft);
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
    let set = subtree_set();
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
    (b.diff, root)
}

/// The `set { $surface: $surface.mix($accent, 0.85) }` of [`scene`]'s
/// subtree.
fn subtree_set() -> TokenTable {
    let mut set = TokenTable::default();
    set.insert(
        "surface",
        PropValue::Token(TokenExpr::path("surface").call(
            TokenMethod::Mix,
            vec![TokenExpr::path("accent"), TokenExpr::value(num(0.85))],
        )),
    );
    set
}

struct Stage {
    r: Renderer,
    buf: Buffer,
}

impl Stage {
    /// The scene under `t`, painted once at `T0` (shown with a clock:
    /// a swap from now on animates).
    fn new(t: TokenTable, w: u32, h: u32) -> Stage {
        let (diff, root) = scene(t);
        let mut r = renderer();
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(w, h, Scale::ONE);
        assert!(!buf.paint_at(&mut r, S, 0, T0).is_empty());
        assert!(!r.wants_frame(S));
        Stage { r, buf }
    }

    fn swap(&mut self, t: TokenTable, how: Transition) {
        let mut d = SceneDiff::new();
        d.set_tokens(t, how);
        assert!(self.r.apply(d).is_empty());
    }

    fn paint(&mut self, t: Duration) -> Damage {
        self.buf.paint_at(&mut self.r, S, 1, t)
    }

    fn tokens(&self) -> &TokenTable {
        &self.r.tree().tokens
    }

    fn color(&self, path: &str) -> Color {
        match self.tokens().lookup(path) {
            Some(PropValue::Color(c)) => c,
            other => panic!("{path}: {other:?}"),
        }
    }
}

fn bgra(c: Color) -> [u8; 4] {
    let [r, g, b, a] = c.to_rgba8();
    [b, g, r, a]
}

fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

fn oklab_l(c: Color) -> f64 {
    c.to_oklab().l
}

/// Every declared pair of the table on screen, as the guard leaves it:
/// the lowest ratio of a text token over its opaque backgrounds, for
/// pairs some text can meet at 3:1 at both ends of the swap.
fn worst_pair(t: &TokenTable, readable: &[(String, Vec<String>)]) -> (f64, String) {
    let mut worst = (f64::INFINITY, String::new());
    for (text, bgs) in readable {
        let Some(PropValue::Color(fg)) = t.lookup(text) else {
            panic!("{text}")
        };
        for b in bgs.iter().filter(|b| *b != text) {
            if let Some(PropValue::Color(bg)) = t.lookup(b)
                && bg.a >= 1.0
            {
                let r = fg.contrast(bg);
                if r < worst.0 {
                    worst = (r, format!("{text} over {b}"));
                }
            }
        }
    }
    worst
}

/// The pairs of `to` some text can meet at 3:1 in `from` and in `to`.
fn readable_pairs(from: &TokenTable, to: &TokenTable) -> Vec<(String, Vec<String>)> {
    let opaque = |t: &TokenTable, text: &str, bgs: &[String]| -> Vec<Color> {
        bgs.iter()
            .filter(|b| *b != text)
            .filter_map(|b| match t.lookup(b) {
                Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c),
                _ => None,
            })
            .collect()
    };
    to.contrast
        .iter()
        .filter(|(text, bgs)| {
            [from, to]
                .iter()
                .all(|t| Color::contrast_reachable(&opaque(t, text, bgs), MIN_CONTRAST))
        })
        .map(|(t, b)| (t.clone(), b.clone()))
        .collect()
}

/// A light→dark swap springs the palette roots: the first frame already
/// moves, every root moves monotonically through OKLab to land exactly
/// on logic's value, derived tokens are re-derived from the roots of
/// each frame (what is drawn is the table's value that frame), and the
/// surface is idle once it settles.
#[test]
fn a_swap_springs_palette_roots_and_rederives_every_frame() {
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let mut st = Stage::new(light.clone(), 320, 72);
    let from = st.color("surface");
    st.swap(dark.clone(), Transition::Default);
    assert!(st.r.wants_frame(S), "a swap asks for frames");
    let mut ls = vec![oklab_l(from)];
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        let t = st.tokens();
        // Only colours move: every other plain token is the new table's
        // at once, and the derived expressions are logic's.
        assert_eq!(t.derived, dark.derived);
        for (path, v) in &dark.tokens {
            if !matches!(v, PropValue::Color(_)) {
                assert_eq!(t.tokens.get(path), Some(v), "{path} snaps");
            }
        }
        let surface = st.color("surface");
        ls.push(oklab_l(surface));
        // The pixels are this frame's table, roots and derived alike.
        assert!(close(st.buf.px(2, 70), bgra(surface), 1), "frame {k}");
        assert!(
            close(st.buf.px(100, 50), bgra(st.color("surface.hi")), 1),
            "frame {k}: surface.hi"
        );
        assert!(
            close(st.buf.px(186, 26), bgra(st.color("accent")), 1),
            "frame {k}: accent"
        );
        k += 1;
        assert!(k < 120, "never settled");
    }
    assert!(ls[1] < ls[0] - 0.02, "the first frame moves: {ls:?}");
    assert!(ls.windows(2).all(|w| w[1] <= w[0] + 1e-6), "{ls:?}");
    assert!(k > 6, "a spring, not a snap: {k} frames");
    // Settled exactly on logic's table.
    assert_eq!(st.tokens(), &dark);
    assert!(!st.r.swapping());
    assert_eq!(st.r.swap_crossfades(), 0);
    // And idle: another frame draws nothing.
    assert!(st.paint(frame(k)).is_empty());
}

/// A mid-swap frame's derived tokens are the derived expressions over
/// that frame's roots: the same table built by hand from the roots
/// evaluates to the same colours, the guard included.
#[test]
fn derived_tokens_stay_exact_mid_swap() {
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let mut st = Stage::new(light.clone(), 320, 72);
    st.swap(dark.clone(), Transition::Default);
    st.paint(frame(1));
    st.paint(frame(3));
    let mid = st.tokens().clone();
    let mut by_hand = dark.clone();
    for (path, v) in &mid.tokens {
        by_hand.insert(path.clone(), v.clone());
    }
    for path in dark.derived.keys().chain(dark.tokens.keys()) {
        assert_eq!(mid.lookup(path), by_hand.lookup(path), "{path}");
    }
    // Mid-flight: each moving root lies between its two ends in OKLab.
    let mut between = 0;
    for (path, v) in &dark.tokens {
        let (PropValue::Color(a), PropValue::Color(b), Some(PropValue::Color(m))) = (
            light.tokens[path].clone(),
            v.clone(),
            mid.tokens.get(path).cloned(),
        ) else {
            continue;
        };
        if a == b {
            continue;
        }
        let (a, b, m) = (a.to_oklab(), b.to_oklab(), m.to_oklab());
        assert!(
            (m.l - a.l.min(b.l)) > -0.01 && (a.l.max(b.l) - m.l) > -0.01,
            "{path}: {m:?} outside {a:?}..{b:?}"
        );
        between += 1;
    }
    assert!(between > 30, "{between} roots in flight");
}

/// Fonts snap (a new `$font.ui` is there in the first frame) while the
/// colours of the same swap spring.
#[test]
fn fonts_snap_while_colours_spring() {
    let light = table(&material(seed(), false));
    let mut dark = table(&material(seed(), true));
    dark.insert("font.ui", PropValue::Font(font(20.0)));
    let mut st = Stage::new(light.clone(), 320, 72);
    st.swap(dark.clone(), Transition::Default);
    st.paint(frame(1));
    assert_eq!(st.tokens().get("font.ui"), dark.get("font.ui"));
    assert!(st.r.swapping());
    let surface = st.color("surface");
    assert_ne!(Some(&PropValue::Color(surface)), dark.get("surface"));
}

/// `reduced_motion` snaps a swap: the first frame shows the new table
/// and nothing moves after it. So does a table sent `Instant`, and a
/// swap while nothing is shown with a clock.
#[test]
fn reduced_motion_and_instant_tables_snap() {
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    for case in ["reduced", "instant", "token"] {
        let mut st = Stage::new(light.clone(), 320, 72);
        let mut to = dark.clone();
        let how = match case {
            "reduced" => {
                st.r.set_reduced_motion(true);
                Transition::Default
            }
            "token" => {
                to.insert("motion.reduced", PropValue::Bool(true));
                Transition::Default
            }
            _ => Transition::Instant,
        };
        st.swap(to.clone(), how);
        st.paint(frame(1));
        assert_eq!(st.tokens(), &to, "{case}");
        assert!(!st.r.swapping(), "{case}");
        assert!(!st.r.wants_frame(S), "{case}");
    }
    // Never painted with a clock: nothing to animate from.
    let (diff, root) = scene(light);
    let mut r = renderer();
    r.apply(diff);
    r.attach_surface(S, root);
    let mut buf = Buffer::new(320, 72, Scale::ONE);
    buf.paint(&mut r, S, 0);
    let mut d = SceneDiff::new();
    d.set_tokens(dark.clone(), Transition::Default);
    r.apply(d);
    assert!(!r.swapping());
    assert_eq!(r.tree().tokens, dark);
}

/// A swap interrupted mid-flight turns towards the newer table from
/// where it is, without a jump.
#[test]
fn a_swap_retargeted_mid_flight_does_not_jump() {
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let mut st = Stage::new(light.clone(), 320, 72);
    st.swap(dark, Transition::Default);
    let mut ls = Vec::new();
    for k in 1..=4 {
        st.paint(frame(k));
        ls.push(oklab_l(st.color("surface")));
    }
    st.swap(light.clone(), Transition::Default);
    st.paint(frame(5));
    let after = oklab_l(st.color("surface"));
    let step = (ls[3] - ls[2]).abs();
    assert!(
        (after - ls[3]).abs() <= step + 0.01,
        "{ls:?} then {after}: jumped"
    );
    let mut k = 6;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        k += 1;
        assert!(k < 200);
    }
    assert_eq!(st.tokens(), &light);
}

/// A tiny deterministic generator for palettes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn color(&mut self) -> Color {
        Color::rgb(self.unit(), self.unit(), self.unit())
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next() % items.len() as u64) as usize]
    }
}

fn random_material(rng: &mut Rng, dark: bool) -> Palette {
    from_seed(
        rng.color(),
        Options {
            variant: rng.pick(Variant::ALL),
            dark,
            contrast: rng.unit() as f64 * 2.0 - 1.0,
        },
    )
}

/// A palette an importer might hand over: a few roles, the rest filled
/// and guarded (surfaces need not agree with each other).
fn random_partial(rng: &mut Rng) -> Palette {
    Partial::new()
        .with(Role::Surface, rng.color())
        .with(Role::SurfaceHigh, rng.color())
        .with(Role::Fg, rng.color())
        .with(Role::Accent, rng.color())
        .fill()
}

/// Plays one swap through frame by frame at `hz`; returns the worst
/// ratio of a readable declared pair over every frame that sprang (a
/// crossfade's frames show snapshots, judged at their ends) and whether
/// it crossfaded.
fn play(from: &Palette, to: &Palette, hz: u32) -> (f64, String, bool) {
    let (a, b) = (table(from), table(to));
    let readable = readable_pairs(&a, &b);
    // A small scene: the pairs live in the table, not the pixels.
    let mut st = Stage::new(a.clone(), 48, 16);
    let before = st.r.swap_crossfades();
    st.swap(b.clone(), Transition::Default);
    let faded = st.r.swap_crossfades() > before;
    let mut worst = worst_pair(st.tokens(), &readable);
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(at(k, hz));
        let w = worst_pair(st.tokens(), &readable);
        if w.0 < worst.0 {
            worst = (w.0, format!("frame {k}: {}", w.1));
        }
        k += 1;
        assert!(k < 20 * hz, "never settled");
    }
    assert_eq!(st.tokens(), &b);
    (worst.0, worst.1, faded)
}

/// The M2 gate: contrast never drops below 3:1. Every frame of light→dark,
/// dark→light and wallpaper→mocha swaps over random palettes (seeds,
/// variants, contrast levels, Catppuccin flavours, partial imports), at
/// 60 and 144 Hz, keeps every declared pair that is readable at both
/// ends at 3:1 or better; a swap where no spring could crossfades
/// instead (its frames are the two readable ends, blended).
#[test]
fn contrast_never_drops_below_three_to_one_during_swaps() {
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let flavours = ["mocha", "macchiato", "frappe", "latte"];
    let mut sprang = 0;
    let mut faded = 0;
    let rounds: u32 = if cfg!(debug_assertions) { 12 } else { 60 };
    for round in 0..rounds {
        let hz = if round.is_multiple_of(3) { 144 } else { 60 };
        let s = rng.color();
        let light = material(s, false);
        let dark = material(s, true);
        let wall_dark = rng.next().is_multiple_of(2);
        let wall = random_material(&mut rng, wall_dark);
        let flavour = rng.pick(&flavours);
        let mocha = import(&format!("catppuccin:{flavour}"), None).unwrap();
        let partial = random_partial(&mut rng);
        let other_dark = rng.next().is_multiple_of(2);
        let other = random_material(&mut rng, other_dark);
        for (name, from, to) in [
            ("light→dark", &light, &dark),
            ("dark→light", &dark, &light),
            ("wallpaper→mocha", &wall, &mocha),
            ("partial→material", &partial, &other),
            ("material→partial", &other, &partial),
        ] {
            let (worst, at, fade) = play(from, to, hz);
            if fade {
                eprintln!("round {round} {name}: crossfaded");
                faded += 1;
            } else {
                sprang += 1;
                assert!(
                    worst >= MIN_CONTRAST - 1e-6,
                    "round {round} {name} at {hz} Hz: {worst:.3}:1 ({at})"
                );
            }
        }
    }
    eprintln!("{sprang} swaps sprang, {faded} crossfaded");
    assert!(sprang > faded * 4, "{sprang} sprang, {faded} crossfaded");
}

/// A swap no spring can keep readable: `$fg` is drawn over two
/// surfaces that part ways (one heading dark, one light), so mid-swap
/// no text lightness reaches 3:1 over both. It crossfades: the table
/// snaps (every frame's pairs are the new palette's, readable), and the
/// pixels go from a snapshot of the old frame to the new frame.
#[test]
fn an_unreadable_spring_crossfades_from_a_snapshot() {
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let mut a = table(&material(seed(), false));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert("surface.split", PropValue::Color(grey));
        t.insert_contrast("fg", vec!["surface".into(), "surface.split".into()]);
    }
    a.insert("surface", PropValue::Color(grey));
    b.insert("surface", PropValue::Color(Color::BLACK));
    b.insert("surface.split", PropValue::Color(Color::WHITE));
    let readable = readable_pairs(&a, &b);
    assert!(readable.iter().any(|(t, _)| t == "fg"));

    let mut st = Stage::new(a.clone(), 320, 72);
    let old = st.buf.pixels.clone();
    st.swap(b.clone(), Transition::Default);
    assert_eq!(st.r.swap_crossfades(), 1, "crossfades");
    // The new frame, painted alone.
    let mut fresh = Stage::new(b.clone(), 320, 72);
    fresh.paint(frame(1));
    let new = fresh.buf.pixels.clone();
    let mut k = 1;
    let mut mids = 0;
    while st.r.wants_frame(S) {
        let d = st.paint(frame(k));
        assert_eq!(d, Damage::full(Size::new(320, 72)), "frame {k}: full");
        // The table snapped: every frame's pairs are readable.
        assert!(worst_pair(st.tokens(), &readable).0 >= MIN_CONTRAST - 1e-6);
        // Each pixel lies between the old and the new frame's.
        for ((p, o), n) in st.buf.pixels.iter().zip(&old).zip(&new) {
            assert!(*p >= (*o).min(*n).saturating_sub(1) && *p <= (*o).max(*n).saturating_add(1));
        }
        if st.buf.pixels != old && st.buf.pixels != new {
            mids += 1;
        }
        k += 1;
        assert!(k < 120, "never settled");
    }
    assert!(mids > 4, "{mids} blended frames");
    assert_eq!(st.buf.pixels, new, "ends on the new frame exactly");
    assert!(!st.r.swapping());
    assert!(st.paint(frame(k)).is_empty(), "idle after");
}

/// The offline tier: a light→dark swap sampled at fixed timestamps, and
/// the crossfade above, as filmstrips matching references; the same
/// timestamps on a fresh renderer give the same pixels.
#[test]
fn swaps_sampled_at_fixed_timestamps_match_reference() {
    let frames = [0u32, 1, 2, 3, 5, 8, 13, 30];
    let strip = |from: TokenTable, to: TokenTable| -> Buffer {
        let mut st = Stage::new(from, 320, 72);
        st.swap(to, Transition::Default);
        let mut out = Buffer::new(320, 72 * frames.len() as u32, Scale::ONE);
        let row = 320 * 72 * 4;
        for (i, k) in frames.iter().enumerate() {
            if *k > 0 {
                st.paint(frame(*k));
            }
            out.pixels[i * row..(i + 1) * row].copy_from_slice(&st.buf.pixels);
        }
        out
    };
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let swap = strip(light.clone(), dark.clone());
    assert_matches_ref("theme_swap", &swap, 3);
    assert!(swap.pixels == strip(light, dark).pixels, "deterministic");

    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let mut a = table(&material(seed(), false));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert("surface.split", PropValue::Color(grey));
        t.insert_contrast("fg", vec!["surface".into(), "surface.split".into()]);
    }
    a.insert("surface", PropValue::Color(grey));
    b.insert("surface", PropValue::Color(Color::BLACK));
    b.insert("surface.split", PropValue::Color(Color::WHITE));
    let fade = strip(a.clone(), b.clone());
    assert_matches_ref("theme_crossfade", &fade, 3);
    assert!(fade.pixels == strip(a, b).pixels, "deterministic");
}

/// Guarded text keeps to one side of its own background through a
/// swap: it changes sides (darker than it to lighter, or back) at most
/// once, so there is no light/dark/light flicker from frame to frame,
/// in the global scope and in the `set { }` subtree, for light→dark and
/// dark→light swaps of several seeds. (A pair no text can meet at one
/// end of the swap, as [`scene`]'s subtree `$fg` over all eight
/// surfaces of a dark palette when only `$surface` is overridden, is
/// met over its own background there and may change sides more as the
/// others come within reach: not checked.)
#[test]
fn guarded_text_changes_sides_at_most_once_in_a_swap() {
    // A lightly tinted subtree, whose `$fg` some text meets over every
    // surface at both ends.
    let mut set = TokenTable::default();
    set.insert(
        "surface",
        PropValue::Token(TokenExpr::path("surface").call(
            TokenMethod::Mix,
            vec![TokenExpr::path("accent"), TokenExpr::value(num(0.2))],
        )),
    );
    let scope_of = |t: &TokenTable, over: bool| -> Vec<TokenTable> {
        if over {
            vec![t.clone(), set.clone()]
        } else {
            vec![t.clone()]
        }
    };
    // +1 lighter than its own background (the pair's first), -1 darker.
    let side = |tables: &[&TokenTable]| -> f64 {
        let scope = TokenScope::new(tables);
        let lum = |p: &str| match scope.lookup(p) {
            Some(PropValue::Color(c)) => c.relative_luminance(),
            other => panic!("{p}: {other:?}"),
        };
        (lum("fg") - lum(&tables[0].contrast["fg"][0])).signum()
    };
    // Some text meets every background of `fg` in this scope.
    let meetable = |tables: &[TokenTable]| -> bool {
        let refs: Vec<&TokenTable> = tables.iter().collect();
        let scope = TokenScope::new(&refs);
        let bgs: Vec<Color> = tables[0].contrast["fg"]
            .iter()
            .filter(|b| *b != "fg")
            .filter_map(|b| match scope.lookup(b) {
                Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c),
                _ => None,
            })
            .collect();
        Color::contrast_reachable(&bgs, MIN_CONTRAST)
    };
    let mut checked = [0; 2];
    for s in [
        "#6750a4", "#1b6ef3", "#b3261e", "#386a20", "#7d5260", "#e8def8", "#006874",
    ] {
        let light = table(&material(hex(s), false));
        let dark = table(&material(hex(s), true));
        for (from, to) in [(&light, &dark), (&dark, &light)] {
            let mut st = Stage::new(from.clone(), 320, 72);
            st.swap(to.clone(), Transition::Default);
            let mut sides = [Vec::new(), Vec::new()];
            let mut k = 1;
            while st.r.wants_frame(S) {
                st.paint(frame(k));
                sides[0].push(side(&[st.tokens()]));
                sides[1].push(side(&[st.tokens(), &set]));
                k += 1;
                assert!(k < 200, "never settled");
            }
            for (i, name) in ["global", "subtree"].iter().enumerate() {
                let over = i == 1;
                if !(meetable(&scope_of(from, over)) && meetable(&scope_of(to, over))) {
                    continue;
                }
                checked[i] += 1;
                let flips = sides[i].windows(2).filter(|w| w[0] != w[1]).count();
                assert!(
                    flips <= 1,
                    "{s} {name}: {flips} side changes: {:?}",
                    sides[i]
                );
            }
        }
    }
    assert!(checked[0] >= 10 && checked[1] >= 2, "checked {checked:?}");
}

/// The split palette of `an_unreadable_spring_crossfades_from_a_snapshot`:
/// `$fg` over `$surface` and `$surface.split`, grey and grey, then black
/// and white (no spring keeps a text at 3:1 over both).
fn split_tables() -> (TokenTable, TokenTable) {
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    let mut a = table(&material(seed(), false));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert("surface.split", PropValue::Color(grey));
        t.insert_contrast("fg", vec!["surface".into(), "surface.split".into()]);
    }
    a.insert("surface", PropValue::Color(grey));
    b.insert("surface", PropValue::Color(Color::BLACK));
    b.insert("surface.split", PropValue::Color(Color::WHITE));
    (a, b)
}

/// A table that changes no colour (a length here: a `prefs.compact`
/// toggle, a re-sent table) arriving mid-crossfade leaves the fade
/// running from where it is; one sent `Instant` ends it.
#[test]
fn a_colourless_table_mid_crossfade_keeps_fading() {
    let (a, b) = split_tables();
    let mut st = Stage::new(a, 320, 72);
    st.swap(b.clone(), Transition::Default);
    assert_eq!(st.r.swap_crossfades(), 1);
    st.paint(frame(1));
    st.paint(frame(2));
    let mid = st.buf.pixels.clone();
    let mut fresh = Stage::new(b.clone(), 320, 72);
    fresh.paint(frame(1));
    let new = fresh.buf.pixels.clone();
    assert_ne!(mid, new, "still fading");
    // Only a length changes.
    let mut c = b.clone();
    c.insert("space.1", PropValue::Length(Length::Px(7.0)));
    st.swap(c.clone(), Transition::Default);
    assert_eq!(st.r.swap_crossfades(), 1, "no second crossfade");
    assert!(st.r.swapping(), "the fade goes on");
    st.paint(frame(3));
    assert_ne!(st.buf.pixels, new, "frame 3 is still blended");
    let mut k = 4;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        k += 1;
        assert!(k < 120, "never settled");
    }
    assert!(k > 6, "faded over several frames ({k})");
    assert_eq!(st.buf.pixels, new);
    // Sent `Instant` mid-fade: it ends at once.
    let (a, b) = split_tables();
    let mut st = Stage::new(a, 320, 72);
    st.swap(b.clone(), Transition::Default);
    st.paint(frame(1));
    st.swap(b, Transition::Instant);
    assert!(!st.r.swapping());
    st.paint(frame(2));
    assert_eq!(st.buf.pixels, new);
}

/// The pairs of a `set { }` subtree are played through too: a swap the
/// global scope could spring, but under whose override `$fg` would have
/// no readable lightness for a while, crossfades.
#[test]
fn a_subtree_that_a_spring_leaves_unreadable_crossfades() {
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    // Globally `$fg` is over `$base` and `$panel`, which move together;
    // the subtree draws on `$ink` as its `$panel`, which moves the other
    // way (paths no other pair reads).
    let mut a = table(&material(seed(), false));
    let mut b = a.clone();
    for t in [&mut a, &mut b] {
        t.insert_contrast("fg", vec!["base".into(), "panel".into()]);
    }
    for (path, ca, cb) in [
        ("base", grey, Color::BLACK),
        ("panel", grey, Color::BLACK),
        ("ink", grey, Color::WHITE),
    ] {
        a.insert(path, PropValue::Color(ca));
        b.insert(path, PropValue::Color(cb));
    }
    let mut set = TokenTable::default();
    set.insert("panel", tok("ink"));
    let build = |with_subtree: bool| {
        let mut bl = Builder::default();
        bl.diff.set_tokens(a.clone(), Transition::Instant);
        let root = bl.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("base"))]);
        bl.node(
            NodeKind::Text,
            Some(root),
            vec![(Prop::Text, text("global"))],
        );
        if with_subtree {
            let sub = bl.node(
                NodeKind::Box,
                Some(root),
                vec![
                    (Prop::Y, num(30.0)),
                    (Prop::Width, num(100.0)),
                    (Prop::Height, num(30.0)),
                    (Prop::Tokens, PropValue::Tokens(Box::new(set.clone()))),
                    (Prop::Bg, tok("panel")),
                ],
            );
            bl.node(NodeKind::Text, Some(sub), vec![(Prop::Text, text("sub"))]);
        }
        let mut r = renderer();
        assert!(r.apply(bl.diff).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(160, 72, Scale::ONE);
        buf.paint_at(&mut r, S, 0, T0);
        let mut d = SceneDiff::new();
        d.set_tokens(b.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
        r.swap_crossfades()
    };
    assert_eq!(build(false), 0, "the global scope alone springs");
    assert_eq!(build(true), 1, "the subtree's pair crossfades");
}

/// Two surfaces whose presentation clocks are 7 ms apart (two outputs)
/// each show a swap at their own times: a crossfade's frames on each
/// are what one surface alone shows at those times (none jumps to the
/// end when the other's clock settles it), and a spring's land exactly
/// on the new table on both.
#[test]
fn two_surfaces_on_offset_clocks_swap_at_their_own_times() {
    const S2: SurfaceId = SurfaceId(2);
    let off = Duration::from_millis(7);
    // A timed crossfade, so a frame near the end is still visibly mixed.
    let how = Transition::Duration {
        duration: Duration::from_millis(250),
        easing: Easing::Linear,
    };
    let (a, b) = split_tables();
    // One surface alone at the earlier clock, its last frame before the
    // swap shown when the later clock's was (where both start from).
    let (diff, root) = scene(a.clone());
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    let mut alone = Buffer::new(320, 72, Scale::ONE);
    alone.paint_at(&mut r, S, 0, T0 + off);
    let mut d = SceneDiff::new();
    d.set_tokens(b.clone(), how.clone());
    assert!(r.apply(d).is_empty());
    let mut solo = Vec::new();
    let mut k = 1;
    while r.wants_frame(S) {
        alone.paint_at(&mut r, S, 1, frame(k));
        solo.push(alone.pixels.clone());
        k += 1;
        assert!(k < 120);
    }
    assert!(solo.len() > 10, "{} frames", solo.len());
    // The same root on two surfaces; the second one's clock is ahead.
    let (diff, root) = scene(a.clone());
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    r.attach_surface(S2, root);
    let mut one = Buffer::new(320, 72, Scale::ONE);
    let mut two = Buffer::new(320, 72, Scale::ONE);
    one.paint_at(&mut r, S, 0, T0);
    two.paint_at(&mut r, S2, 0, T0 + off);
    let mut d = SceneDiff::new();
    d.set_tokens(b.clone(), how.clone());
    assert!(r.apply(d).is_empty());
    assert_eq!(r.swap_crossfades(), 1);
    let new = solo.last().unwrap().clone();
    let mut k = 1;
    while r.wants_frame(S) || r.wants_frame(S2) {
        // The later clock paints first: its end must not end the
        // earlier one's fade.
        two.paint_at(&mut r, S2, 1, frame(k) + off);
        one.paint_at(&mut r, S, 1, frame(k));
        if let Some(want) = solo.get(k as usize - 1) {
            assert!(one.pixels == *want, "frame {k}: as one surface alone");
        } else {
            assert_eq!(one.pixels, new, "frame {k}");
        }
        if r.swapping() {
            assert!(
                strand_scene::Painter::opaque_region(&r, S).is_empty() || one.pixels == new,
                "a blended frame claims no opaque region"
            );
        }
        k += 1;
        assert!(k < 120, "never settled");
    }
    assert_eq!(one.pixels, new);
    assert_eq!(two.pixels, new);

    // A spring on both: every frame of both readable, both end exact.
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let readable = readable_pairs(&light, &dark);
    let (diff, root) = scene(light.clone());
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    r.attach_surface(S2, root);
    one.paint_at(&mut r, S, 0, T0);
    two.paint_at(&mut r, S2, 0, T0 + off);
    let mut d = SceneDiff::new();
    d.set_tokens(dark.clone(), Transition::Default);
    assert!(r.apply(d).is_empty());
    assert_eq!(r.swap_crossfades(), 0);
    let mut k = 1;
    while r.wants_frame(S) || r.wants_frame(S2) {
        one.paint_at(&mut r, S, 1, frame(k));
        assert!(worst_pair(&r.tree().tokens, &readable).0 >= MIN_CONTRAST - 1e-6);
        two.paint_at(&mut r, S2, 1, frame(k) + off);
        assert!(worst_pair(&r.tree().tokens, &readable).0 >= MIN_CONTRAST - 1e-6);
        k += 1;
        assert!(k < 200, "never settled");
    }
    assert_eq!(r.tree().tokens, dark);
    let mut end = Stage::new(dark, 320, 72);
    end.paint(frame(1));
    assert!(one.pixels == end.buf.pixels && two.pixels == end.buf.pixels);
}

/// Mark and link colours are not shaped with: a swap springing
/// `$accent` sends no text to be shaped again (worker backend: nothing
/// pending after any frame), and the marked glyphs are drawn in each
/// frame's `$accent`.
#[test]
fn span_colours_follow_the_swap_without_reshaping() {
    use std::sync::Arc;
    use strand_render::TextBackend;
    use strand_text::{FontConfig, TextWorker, test_font_path};

    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    let mut r = Renderer::new(TextBackend::Worker(worker));
    let light = table(&material(seed(), false));
    let dark = table(&material(seed(), true));
    let mut b = Builder::default();
    b.diff.set_tokens(light.clone(), Transition::Instant);
    let root = b.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("surface"))]);
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::Text, text("MMMM")),
            (Prop::Font, PropValue::Font(font(48.0))),
            (
                Prop::Marks,
                PropValue::List(vec![PropValue::List(vec![num(0.0), num(4.0)])]),
            ),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::Y, num(60.0)),
            (Prop::Text, text("<a href=\"x\">link</a>")),
            (Prop::Markup, PropValue::Keyword("basic".into())),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, root);
    r.configure_surface(S, Size::new(240, 90), Scale::ONE);
    assert!(r.wait_for_text(Duration::from_secs(10)));
    let mut buf = Buffer::new(240, 90, Scale::ONE);
    buf.paint_at(&mut r, S, 0, T0);
    assert!(!r.text_pending());
    let mut d = SceneDiff::new();
    d.set_tokens(dark.clone(), Transition::Default);
    assert!(r.apply(d).is_empty());
    let accent_at = |r: &Renderer| match r.tree().tokens.lookup("accent") {
        Some(PropValue::Color(c)) => bgra(c),
        other => panic!("{other:?}"),
    };
    let mut k = 1;
    let mut mids = 0;
    let (first, last) = (accent_at(&r), {
        let mut t = dark.clone();
        t.freeze();
        match t.lookup("accent") {
            Some(PropValue::Color(c)) => bgra(c),
            _ => panic!(),
        }
    });
    while r.wants_frame(S) {
        buf.paint_at(&mut r, S, 1, frame(k));
        assert!(!r.text_pending(), "frame {k}: text sent to be shaped again");
        let accent = accent_at(&r);
        if accent != first && accent != last {
            mids += 1;
        }
        // A fully covered pixel of the marked "MMMM" is this frame's
        // accent.
        let hit = (0..56)
            .flat_map(|y| (0..240).map(move |x| (x, y)))
            .any(|(x, y)| close(buf.px(x, y), accent, 1));
        assert!(hit, "frame {k}: no glyph in {accent:?}");
        k += 1;
        assert!(k < 200, "never settled");
    }
    assert!(mids > 4, "{mids} frames mid-swap");
}

/// A surface whose snapshot would pass the crossfade's memory cap (a
/// buffer larger than 1920×1080) snaps to the new frame instead of
/// fading; a smaller one beside it fades.
#[test]
fn a_surface_too_large_to_snapshot_snaps() {
    const BIG: SurfaceId = SurfaceId(2);
    let (a, b) = split_tables();
    let (diff, root) = scene(a);
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    r.attach_surface(BIG, root);
    let mut small = Buffer::new(320, 72, Scale::ONE);
    let mut big = Buffer::new(2000, 1100, Scale::ONE);
    small.paint_at(&mut r, S, 0, T0);
    big.paint_at(&mut r, BIG, 0, T0);
    let mut d = SceneDiff::new();
    d.set_tokens(b.clone(), Transition::Default);
    assert!(r.apply(d).is_empty());
    assert_eq!(r.swap_crossfades(), 1);
    let mut fresh = Stage::new(b, 320, 72);
    fresh.paint(frame(1));
    small.paint_at(&mut r, S, 1, frame(1));
    big.paint_at(&mut r, BIG, 1, frame(1));
    assert_ne!(small.pixels, fresh.buf.pixels, "the small one fades");
    // The big one shows the new frame at once: its top-left corner is
    // the small one's new frame.
    for y in 0..72 {
        let row = &big.pixels[y * 2000 * 4..y * 2000 * 4 + 320 * 4];
        assert_eq!(
            row,
            &fresh.buf.pixels[y * 320 * 4..(y + 1) * 320 * 4],
            "row {y}"
        );
    }
}
