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
    worst_in(&[t], readable)
}

/// [`worst_pair`] in the scope `levels` (the global table, then the
/// `set { }` overrides).
fn worst_in(levels: &[&TokenTable], readable: &[(String, Vec<String>)]) -> (f64, String) {
    let scope = TokenScope::new(levels);
    let mut worst = (f64::INFINITY, String::new());
    for (text, bgs) in readable {
        let Some(PropValue::Color(fg)) = scope.lookup(text) else {
            panic!("{text}")
        };
        for b in bgs.iter().filter(|b| *b != text) {
            if let Some(PropValue::Color(bg)) = scope.lookup(b)
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
    readable_in(&[from], &[to])
}

/// [`readable_pairs`] in scopes (the global table, then the `set { }`
/// overrides).
fn readable_in(from: &[&TokenTable], to: &[&TokenTable]) -> Vec<(String, Vec<String>)> {
    let opaque = |levels: &[&TokenTable], text: &str, bgs: &[String]| -> Vec<Color> {
        let scope = TokenScope::new(levels);
        bgs.iter()
            .filter(|b| *b != text)
            .filter_map(|b| match scope.lookup(b) {
                Some(PropValue::Color(c)) if c.a >= 1.0 => Some(c),
                _ => None,
            })
            .collect()
    };
    to[0]
        .contrast
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

/// How one swap played.
struct Played {
    /// The worst ratio of a readable declared pair over every frame that
    /// sprang, in the global scope and in [`scene`]'s `set { }` subtree,
    /// and where.
    worst: f64,
    at: String,
    /// Every surface crossfaded (the table snapped).
    faded: bool,
    /// The subtree's surface crossfaded while the roots sprang (its
    /// pairs are the new table's in every frame).
    held: bool,
}

/// Plays one swap through frame by frame at `hz` on [`scene`] (whose
/// subtree is `set { $surface: $surface.mix($accent, 0.85) }`): the
/// worst ratio of a declared pair readable at both ends, over every
/// frame that sprang, globally and under the subtree's overrides (a
/// crossfade's frames show snapshots, judged at their ends).
fn play_tables(a: TokenTable, b: TokenTable, hz: u32) -> Played {
    let set = subtree_set();
    let readable = readable_pairs(&a, &b);
    let sub_readable = readable_in(&[&a, &set], &[&b, &set]);
    // A small scene: the pairs live in the table, not the pixels.
    let mut st = Stage::new(a.clone(), 48, 16);
    let before = st.r.swap_crossfades();
    st.swap(b.clone(), Transition::Default);
    let held = !st.r.swap_held().is_empty();
    let faded = st.r.swap_crossfades() > before && !held;
    let judge = |t: &TokenTable| -> (f64, String) {
        let g = worst_pair(t, &readable);
        if held {
            return g;
        }
        let s = worst_in(&[t, &set], &sub_readable);
        if s.0 < g.0 {
            (s.0, format!("set {{ }}: {}", s.1))
        } else {
            g
        }
    };
    let mut worst = judge(st.tokens());
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(at(k, hz));
        let w = judge(st.tokens());
        if w.0 < worst.0 {
            worst = (w.0, format!("frame {k}: {}", w.1));
        }
        k += 1;
        assert!(k < 20 * hz, "never settled");
    }
    assert_eq!(st.tokens(), &b);
    Played {
        worst: worst.0,
        at: worst.1,
        faded,
        held,
    }
}

fn play(from: &Palette, to: &Palette, hz: u32) -> Played {
    play_tables(table(from), table(to), hz)
}

/// Palettes made the way `material(image:)` makes them: synthetic
/// wallpapers (random bands of colour, mostly one) quantised by
/// strand-theme's quantiser, light and dark.
fn wallpaper_palettes(rng: &mut Rng, n: usize) -> Vec<(Palette, Palette)> {
    let dir = std::env::temp_dir().join(format!("strand-swap-walls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut q = strand_theme::Quantiser::new(None).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        let (w, h) = (160u32, 90u32);
        let bands: Vec<[u8; 3]> = (0..3)
            .map(|_| {
                let [r, g, b, _] = rng.color().to_rgba8();
                [r, g, b]
            })
            .collect();
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..h {
            for x in 0..w {
                let band = if x < w * 2 / 3 {
                    0
                } else if x < w * 5 / 6 {
                    1
                } else {
                    2
                };
                rgb.extend_from_slice(&bands[band]);
            }
        }
        let path = dir.join(format!("wall{i}.png"));
        let file = std::fs::File::create(&path).unwrap();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&rgb).unwrap();
        q.lookup(&path);
        assert!(q.wait(Duration::from_secs(30)));
        let strand_theme::Lookup::Ready(seed) = q.lookup(&path) else {
            panic!("wallpaper {i} not quantised")
        };
        let palette = |dark: bool| {
            from_seed(
                seed,
                Options {
                    dark,
                    ..Options::default()
                },
            )
            .with_source("wallpaper")
        };
        out.push((palette(false), palette(true)));
    }
    let _ = std::fs::remove_dir_all(dir);
    out
}

/// The M2 gate: contrast never drops below 3:1. Every frame of light→dark,
/// dark→light and wallpaper→mocha swaps over random palettes (seeds,
/// variants, contrast levels, Catppuccin flavours, partial imports, and
/// palettes quantised from synthetic wallpapers), at 60 and 144 Hz, keeps
/// every declared pair that is readable at both ends at 3:1 or better,
/// in the global scope and in a `set { }` subtree; a swap where no
/// spring could crossfades instead (its frames are the two readable
/// ends, blended: below 3:1 by construction mid-fade, so exempt), on
/// every surface or, for the subtree, on its surface alone.
#[test]
fn contrast_never_drops_below_three_to_one_during_swaps() {
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let flavours = ["mocha", "macchiato", "frappe", "latte"];
    let mut sprang = 0;
    let mut faded = 0;
    let mut held = 0;
    let mut tally = |name: &str, round: u32, hz: u32, p: Played| {
        if p.faded {
            eprintln!("round {round} {name}: crossfaded");
            faded += 1;
            return;
        }
        if p.held {
            eprintln!("round {round} {name}: the subtree crossfaded");
            held += 1;
        }
        sprang += 1;
        assert!(
            p.worst >= MIN_CONTRAST - 1e-6,
            "round {round} {name} at {hz} Hz: {:.3}:1 ({})",
            p.worst,
            p.at
        );
    };
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
            tally(name, round, hz, play(from, to, hz));
        }
    }
    // Wallpapers through the real quantiser (`material(image:)`).
    let walls = wallpaper_palettes(&mut rng, if cfg!(debug_assertions) { 3 } else { 8 });
    for (round, (light, dark)) in walls.iter().enumerate() {
        let round = round as u32;
        let hz = if round.is_multiple_of(2) { 144 } else { 60 };
        let flavour = flavours[round as usize % flavours.len()];
        let mocha = import(&format!("catppuccin:{flavour}"), None).unwrap();
        for (name, from, to) in [
            ("image wallpaper (light)→mocha", light, &mocha),
            ("image wallpaper (dark)→mocha", dark, &mocha),
            ("mocha→image wallpaper (light)", &mocha, light),
            ("image wallpaper light→dark", light, dark),
        ] {
            tally(name, round, hz, play(from, to, hz));
        }
    }
    eprintln!("{sprang} swaps sprang ({held} with the subtree crossfading), {faded} crossfaded");
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
    // One surface alone at the earlier clock (each surface's fade starts
    // from its own last frame).
    let (diff, root) = scene(a.clone());
    let mut r = renderer();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    let mut alone = Buffer::new(320, 72, Scale::ONE);
    alone.paint_at(&mut r, S, 0, T0);
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

/// The largest difference of one channel between two frames.
fn max_step(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// [`split_tables`]' `b`, its two surfaces swapped: from `b`, no
/// spring keeps `$fg` readable over both either.
fn split_swapped() -> TokenTable {
    let (_, mut c) = split_tables();
    c.insert("surface", PropValue::Color(Color::WHITE));
    c.insert("surface.split", PropValue::Color(Color::BLACK));
    c
}

/// A crossfade landing while another runs fades on from the blend on
/// screen: no frame jumps further than a crossfade's own steps (the old
/// fade's snapshot is not kept under a new frame at the old progress),
/// and it ends on the newest table's frame exactly.
#[test]
fn a_crossfade_landing_mid_crossfade_fades_on_from_what_shows() {
    let (a, b) = split_tables();
    let c = split_swapped();
    // The steps of a whole crossfade from `b` to `c`.
    let mut st = Stage::new(b.clone(), 320, 72);
    st.swap(c.clone(), Transition::Default);
    let mut prev = st.buf.pixels.clone();
    let mut whole_step = 0;
    let mut k = 1;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        whole_step = whole_step.max(max_step(&prev, &st.buf.pixels));
        prev = st.buf.pixels.clone();
        k += 1;
    }
    let mut st = Stage::new(a, 320, 72);
    st.swap(b, Transition::Default);
    assert_eq!(st.r.swap_crossfades(), 1);
    let mut prev = st.buf.pixels.clone();
    let mut fade_step = 0;
    for k in 1..=3 {
        st.paint(frame(k));
        fade_step = fade_step.max(max_step(&prev, &st.buf.pixels));
        prev = st.buf.pixels.clone();
    }
    assert!(fade_step > 8, "the first fade moves ({fade_step})");
    st.swap(c.clone(), Transition::Default);
    assert_eq!(st.r.swap_crossfades(), 2, "the second swap crossfades too");
    let most = fade_step.max(whole_step);
    let mut k = 4;
    while st.r.wants_frame(S) {
        st.paint(frame(k));
        let step = max_step(&prev, &st.buf.pixels);
        // Its first frame barely moves from the blend that showed.
        let bound = if k == 4 { fade_step } else { most };
        assert!(
            step <= bound + 2,
            "frame {k}: jumps {step}, a crossfade steps at most {bound}"
        );
        prev = st.buf.pixels.clone();
        k += 1;
        assert!(k < 120, "never settled");
    }
    let mut fresh = Stage::new(c, 320, 72);
    fresh.paint(frame(1));
    assert_eq!(st.buf.pixels, fresh.buf.pixels, "ends on the new frame");
    assert!(!st.r.swapping());
}

/// A surface that stops painting mid-crossfade (its output asleep)
/// neither keeps the swap running nor spoils the next crossfade: once
/// it has painted nothing for the exit stall `swapping()` is false, and
/// a later crossfade still blends on the surface that paints.
#[test]
fn a_surface_that_stops_painting_does_not_hold_up_later_crossfades() {
    const T: SurfaceId = SurfaceId(2);
    let stall = Duration::from_millis(60);
    let (a, b) = split_tables();
    let (diff, root) = scene(a.clone());
    let mut r = renderer();
    r.set_exit_stall(stall);
    assert!(r.apply(diff).is_empty());
    r.attach_surface(S, root);
    r.attach_surface(T, root);
    let mut one = Buffer::new(320, 72, Scale::ONE);
    let mut two = Buffer::new(320, 72, Scale::ONE);
    one.paint_at(&mut r, S, 0, T0);
    two.paint_at(&mut r, T, 0, T0);
    let swap = |r: &mut Renderer, t: &TokenTable| {
        let mut d = SceneDiff::new();
        d.set_tokens(t.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
    };
    swap(&mut r, &b);
    assert_eq!(r.swap_crossfades(), 1);
    // Only S paints.
    let mut k = 1;
    while r.wants_frame(S) {
        one.paint_at(&mut r, S, 1, frame(k));
        k += 1;
        assert!(k < 120, "never settled");
    }
    std::thread::sleep(stall + Duration::from_millis(20));
    assert!(!r.swapping(), "T painted nothing for the stall");
    // The next crossfade blends on S from its first frame.
    let old = one.pixels.clone();
    swap(&mut r, &a);
    // From b, a is reached by a spring; a third table needs a fade.
    let c = split_swapped();
    swap(&mut r, &c);
    assert!(r.swap_crossfades() >= 2, "{}", r.swap_crossfades());
    let mut fresh = Stage::new(c.clone(), 320, 72);
    fresh.paint(frame(1));
    let mut blended = 0;
    while r.wants_frame(S) {
        one.paint_at(&mut r, S, 1, frame(k));
        if one.pixels != fresh.buf.pixels && one.pixels != old {
            blended += 1;
        }
        k += 1;
        assert!(k < 240, "never settled");
    }
    assert!(blended > 4, "{blended} blended frames on S");
    assert_eq!(one.pixels, fresh.buf.pixels);
    std::thread::sleep(stall + Duration::from_millis(20));
    assert!(!r.swapping(), "T's new snapshot goes too");
}

/// A snapshot taken into a buffer two frames old (two buffers in turn)
/// is the frame on screen: the buffer's copy with the last frame's
/// change drawn again, the same pixels as from a buffer one frame old.
#[test]
fn a_snapshot_from_an_older_buffer_matches_one_from_the_last() {
    let (a, b) = split_tables();
    let run = |double: bool| -> Vec<Vec<u8>> {
        let (mut diff, root) = scene(a.clone());
        // A clock whose text changes in the frame before the swap.
        let clock = NodeId::new(900, 0);
        diff.create(clock, NodeKind::Text, Some(root), u32::MAX);
        diff.set(clock, Prop::X, num(16.0));
        diff.set(clock, Prop::Y, num(52.0));
        diff.set(clock, Prop::Text, text("12:59"));
        let mut r = renderer();
        assert!(r.apply(diff).is_empty());
        r.attach_surface(S, root);
        let mut bufs = [
            Buffer::new(320, 72, Scale::ONE),
            Buffer::new(320, 72, Scale::ONE),
        ];
        let mut ages = [0u8, 0u8];
        let mut k = 0u32;
        let mut paint = |r: &mut Renderer, k: u32| -> Vec<u8> {
            let i = if double { k as usize % 2 } else { 0 };
            let age = ages[i];
            bufs[i].paint_at(r, S, age, frame(k));
            ages[i] = if double { 2 } else { 1 };
            if double {
                ages[1 - i] = if ages[1 - i] == 0 { 0 } else { 2 };
            }
            bufs[i].pixels.clone()
        };
        paint(&mut r, k);
        k += 1;
        paint(&mut r, k);
        let mut d = SceneDiff::new();
        d.set(clock, Prop::Text, text("13:00"));
        assert!(r.apply(d).is_empty());
        k += 1;
        paint(&mut r, k);
        let mut d = SceneDiff::new();
        d.set_tokens(b.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
        assert_eq!(r.swap_crossfades(), 1);
        let mut out = Vec::new();
        for _ in 0..4 {
            k += 1;
            out.push(paint(&mut r, k));
        }
        out
    };
    let single = run(false);
    let double = run(true);
    for (i, (s, d)) in single.iter().zip(&double).enumerate() {
        assert!(s == d, "frame {i} of the fade differs");
    }
}

/// The global scope springs while a `set { }` scope under one surface
/// cannot: only that surface crossfades (shown the new table at once,
/// from its snapshot), the other one springs, and both end on the new
/// table exactly.
#[test]
fn only_the_surfaces_drawing_an_unreadable_subtree_crossfade() {
    const SUB: SurfaceId = SurfaceId(2);
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
    // As `a_subtree_that_a_spring_leaves_unreadable_crossfades`.
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
    // Two roots: a plain bar, and one drawing the subtree.
    let build = |t: &TokenTable| {
        let mut bl = Builder::default();
        bl.diff.set_tokens(t.clone(), Transition::Instant);
        let plain = bl.node(NodeKind::Bar, None, vec![(Prop::Bg, tok("base"))]);
        bl.node(
            NodeKind::Text,
            Some(plain),
            vec![(Prop::Text, text("global"))],
        );
        let other = bl.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("base"))]);
        let sub = bl.node(
            NodeKind::Box,
            Some(other),
            vec![
                (Prop::Y, num(30.0)),
                (Prop::Width, num(100.0)),
                (Prop::Height, num(30.0)),
                (Prop::Tokens, PropValue::Tokens(Box::new(set.clone()))),
                (Prop::Bg, tok("panel")),
            ],
        );
        bl.node(NodeKind::Text, Some(sub), vec![(Prop::Text, text("sub"))]);
        let mut r = renderer();
        assert!(r.apply(bl.diff).is_empty());
        r.attach_surface(S, plain);
        r.attach_surface(SUB, other);
        r
    };
    let mut r = build(&a);
    let mut bar = Buffer::new(160, 32, Scale::ONE);
    let mut panel = Buffer::new(160, 72, Scale::ONE);
    bar.paint_at(&mut r, S, 0, T0);
    panel.paint_at(&mut r, SUB, 0, T0);
    let old = panel.pixels.clone();
    let mut d = SceneDiff::new();
    d.set_tokens(b.clone(), Transition::Default);
    assert!(r.apply(d).is_empty());
    assert_eq!(r.swap_crossfades(), 1);
    assert_eq!(r.swap_held(), vec![SUB], "only the subtree's surface");
    assert!(r.swapping());
    // The ends.
    let mut end = build(&b);
    let mut bar_end = Buffer::new(160, 32, Scale::ONE);
    let mut panel_end = Buffer::new(160, 72, Scale::ONE);
    bar_end.paint_at(&mut end, S, 0, frame(1));
    panel_end.paint_at(&mut end, SUB, 0, frame(1));
    let mut k = 1;
    let mut sprang = 0;
    while r.wants_frame(S) || r.wants_frame(SUB) {
        bar.paint_at(&mut r, S, 1, frame(k));
        panel.paint_at(&mut r, SUB, 1, frame(k));
        // The bar springs: its background is between grey and black,
        // and neither.
        let bg = bar.px(150, 2);
        if bg != bar_end.px(150, 2) {
            sprang += 1;
        }
        // The panel crossfades: each pixel lies between its old frame and
        // the new table's (no springing colours there).
        for ((p, o), n) in panel.pixels.iter().zip(&old).zip(&panel_end.pixels) {
            assert!(
                *p >= (*o).min(*n).saturating_sub(1) && *p <= (*o).max(*n).saturating_add(1),
                "frame {k}: the panel shows a springing colour"
            );
        }
        k += 1;
        assert!(k < 120, "never settled");
    }
    assert!(sprang > 4, "{sprang} springing bar frames");
    assert_eq!(bar.pixels, bar_end.pixels);
    assert_eq!(panel.pixels, panel_end.pixels);
    assert_eq!(r.tree().tokens, b);
    assert!(r.swap_held().is_empty());
}

/// The tables of `a_subtree_that_a_spring_leaves_unreadable_crossfades`:
/// `$fg` over `$base` and `$panel` (grey to black), and a `set { $panel:
/// $ink }` whose `$ink` goes grey to white.
fn ink_tables() -> (TokenTable, TokenTable, TokenTable) {
    let grey = Color::from_oklch(Oklch {
        l: 0.6,
        c: 0.0,
        h: 0.0,
        alpha: 1.0,
    });
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
    (a, b, set)
}

/// A `$base` panel with text, and an empty fixed slot 30 px down.
fn ink_panel(bl: &mut Builder) -> (NodeId, NodeId) {
    let root = bl.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("base"))]);
    bl.node(
        NodeKind::Text,
        Some(root),
        vec![(Prop::Text, text("global"))],
    );
    let slot = bl.node(
        NodeKind::Box,
        Some(root),
        vec![
            (Prop::Place, PropValue::Keyword("absolute".into())),
            (Prop::Y, num(30.0)),
            (Prop::Width, num(100.0)),
            (Prop::Height, num(30.0)),
        ],
    );
    (root, slot)
}

/// A `$panel` box `y` down in `parent` with `set` as its `set { }`
/// scope, and text in it.
fn ink_subtree(bl: &mut Builder, parent: NodeId, set: &TokenTable, y: f32) {
    let sub = bl.node(
        NodeKind::Box,
        Some(parent),
        vec![
            (Prop::Y, num(y)),
            (Prop::Width, num(100.0)),
            (Prop::Height, num(30.0)),
            (Prop::Tokens, PropValue::Tokens(Box::new(set.clone()))),
            (Prop::Bg, tok("panel")),
        ],
    );
    bl.node(NodeKind::Text, Some(sub), vec![(Prop::Text, text("sub"))]);
}

/// A `set { }` scope that appears while the roots spring (a subtree
/// created by a later diff, a surface attached mid-swap) was not played
/// through when the swap was planned: it is then, from the roots'
/// motions as they are. One that no spring keeps readable is shown the
/// new table at once, crossfading from what its surface shows (a surface
/// attached mid-swap showed nothing: it just shows the new table); one
/// that stays readable springs with the rest.
#[test]
fn a_scope_that_appears_mid_swap_is_played_through_too() {
    const SUB: SurfaceId = SurfaceId(2);
    let (a, b, unreadable) = ink_tables();
    let mut readable = TokenTable::default();
    readable.insert("panel", tok("base"));
    for (set, held) in [(readable, false), (unreadable, true)] {
        let mut bl = Builder::default();
        bl.diff.set_tokens(a.clone(), Transition::Instant);
        let (root, slot) = ink_panel(&mut bl);
        let mut r = renderer();
        assert!(r.apply(std::mem::take(&mut bl.diff)).is_empty());
        r.attach_surface(S, root);
        let mut buf = Buffer::new(160, 72, Scale::ONE);
        buf.paint_at(&mut r, S, 0, T0);
        let mut d = SceneDiff::new();
        d.set_tokens(b.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
        assert_eq!(r.swap_crossfades(), 0, "the global scope springs");
        buf.paint_at(&mut r, S, 1, frame(1));
        let old = buf.pixels.clone();
        // A subtree created mid-swap (in a fixed slot: nothing else
        // moves).
        ink_subtree(&mut bl, slot, &set, 0.0);
        assert!(r.apply(std::mem::take(&mut bl.diff)).is_empty());
        // A second surface, drawing one too, attached mid-swap.
        let other = bl.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("base"))]);
        ink_subtree(&mut bl, other, &set, 30.0);
        assert!(r.apply(std::mem::take(&mut bl.diff)).is_empty());
        r.attach_surface(SUB, other);
        assert!(r.swapping());
        if held {
            assert_eq!(
                r.swap_held(),
                vec![S, SUB],
                "the unreadable scope's surfaces"
            );
            assert_eq!(r.swap_crossfades(), 1, "only S showed a frame to fade from");
        } else {
            assert!(r.swap_held().is_empty(), "a readable scope springs");
            assert_eq!(r.swap_crossfades(), 0);
        }
        // The end: the same scene under the new table.
        let mut end_b = Builder::default();
        end_b.diff.set_tokens(b.clone(), Transition::Instant);
        let (end_root, end_slot) = ink_panel(&mut end_b);
        ink_subtree(&mut end_b, end_slot, &set, 0.0);
        let mut end = renderer();
        assert!(end.apply(end_b.diff).is_empty());
        end.attach_surface(S, end_root);
        let mut end_buf = Buffer::new(160, 72, Scale::ONE);
        end_buf.paint_at(&mut end, S, 0, frame(2));
        let mut panel = Buffer::new(160, 72, Scale::ONE);
        let mut k = 2;
        while r.wants_frame(S) || r.wants_frame(SUB) {
            buf.paint_at(&mut r, S, 1, frame(k));
            panel.paint_at(&mut r, SUB, 1, frame(k));
            if held {
                // A crossfade: each pixel lies between the frame shown
                // when the scope appeared and the new table's (but in
                // the slot, where the new subtree's text appears a frame
                // or two later).
                for (i, ((p, o), n)) in buf.pixels.iter().zip(&old).zip(&end_buf.pixels).enumerate()
                {
                    let (x, y) = ((i / 4) % 160, (i / 4) / 160);
                    if x < 100 && (30..60).contains(&y) {
                        continue;
                    }
                    assert!(
                        *p >= (*o).min(*n).saturating_sub(1)
                            && *p <= (*o).max(*n).saturating_add(1),
                        "frame {k}: a springing colour on a held surface"
                    );
                }
                // The attached surface shows the new table from its
                // first frame.
                assert_eq!(panel.px(50, 45), end_buf.px(50, 45), "frame {k}");
            }
            k += 1;
            assert!(k < 120, "never settled");
        }
        assert_eq!(buf.pixels, end_buf.pixels);
        assert_eq!(r.tree().tokens, b);
        assert!(r.swap_held().is_empty());
    }
}

/// The contrast play-through's work is bounded (`CHECK_WORK`), and the
/// global scope is played through first: where many `set { }` scopes
/// and a slow, bouncy spring use the rest up, only the surfaces drawing
/// those scopes crossfade (shown the new table at once), and the global
/// roots still spring everywhere else. With a few scopes the same swap
/// springs everywhere.
#[test]
fn set_scopes_that_use_up_the_check_crossfade_only_their_surfaces() {
    const SUB: SurfaceId = SurfaceId(2);
    let spring = |dark: bool| {
        let mut t = table(&material(seed(), dark));
        t.insert(
            "motion.effects",
            PropValue::Transition(Transition::of_spring(Spring::new(60.0, 0.5).unwrap())),
        );
        t
    };
    let (light, dark) = (spring(false), spring(true));
    for scopes in [4usize, 32] {
        let mut bl = Builder::default();
        bl.diff.set_tokens(light.clone(), Transition::Instant);
        let bar = bl.node(NodeKind::Bar, None, vec![(Prop::Bg, tok("surface"))]);
        bl.node(NodeKind::Text, Some(bar), vec![(Prop::Text, text("bar"))]);
        let panel = bl.node(NodeKind::Panel, None, vec![(Prop::Bg, tok("surface"))]);
        for i in 0..scopes {
            let mut set = TokenTable::default();
            let k = 0.04 + 0.5 * i as f32 / 32.0;
            set.insert(
                "surface",
                PropValue::Token(TokenExpr::path("surface").call(
                    TokenMethod::Mix,
                    vec![TokenExpr::path("accent"), TokenExpr::value(num(k))],
                )),
            );
            let sub = bl.node(
                NodeKind::Box,
                Some(panel),
                vec![
                    (Prop::X, num(4.0 + 20.0 * (i % 8) as f32)),
                    (Prop::Y, num(4.0 + 20.0 * (i / 8) as f32)),
                    (Prop::Width, num(18.0)),
                    (Prop::Height, num(18.0)),
                    (Prop::Tokens, PropValue::Tokens(Box::new(set))),
                    (Prop::Bg, tok("surface")),
                ],
            );
            bl.node(
                NodeKind::Text,
                Some(sub),
                vec![(Prop::Text, text("1")), (Prop::Color, tok("fg"))],
            );
        }
        let mut r = renderer();
        assert!(r.apply(bl.diff).is_empty());
        r.attach_surface(S, bar);
        r.attach_surface(SUB, panel);
        let mut a = Buffer::new(160, 24, Scale::ONE);
        let mut p = Buffer::new(170, 90, Scale::ONE);
        a.paint_at(&mut r, S, 0, T0);
        p.paint_at(&mut r, SUB, 0, T0);
        let mut d = SceneDiff::new();
        d.set_tokens(dark.clone(), Transition::Default);
        assert!(r.apply(d).is_empty());
        assert!(r.swapping(), "{scopes} scopes: the global roots spring");
        if scopes == 4 {
            assert_eq!(r.swap_crossfades(), 0, "a few scopes are checked in full");
            assert!(r.swap_held().is_empty());
        } else {
            assert_eq!(r.swap_crossfades(), 1, "{scopes} scopes");
            assert_eq!(r.swap_held(), vec![SUB], "only the scopes' surface");
        }
        // The bar springs: some frame shows its background between the
        // ends.
        let (from, to) = (bgra(light_surface(&light)), bgra(light_surface(&dark)));
        let mut k = 1;
        let mut between = 0;
        while r.wants_frame(S) || r.wants_frame(SUB) {
            a.paint_at(&mut r, S, 1, frame(k));
            p.paint_at(&mut r, SUB, 1, frame(k));
            let px = a.px(150, 20);
            if !close(px, from, 1) && !close(px, to, 1) {
                between += 1;
            }
            k += 1;
            assert!(k < 600, "never settled");
        }
        assert!(
            between > 4,
            "{scopes} scopes: {between} springing bar frames"
        );
        assert_eq!(r.tree().tokens, dark);
    }
}

fn light_surface(t: &TokenTable) -> Color {
    match t.lookup("surface") {
        Some(PropValue::Color(c)) => c,
        other => panic!("surface: {other:?}"),
    }
}
