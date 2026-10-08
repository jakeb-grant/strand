//! `icon` and `image` (design.md: images decode at drawn size into a
//! 6 MB LRU; icons from the freedesktop icon theme, symbolic icons
//! recoloured by `color`): offline PNGs against an icon theme made in a
//! temporary directory, so the result does not depend on the icons
//! installed on the machine.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test images`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::*;
use strand_render::Renderer;
use strand_render::image::{Fit, IMAGE_CACHE_BYTES, IconTheme, ImageKey, load};
use strand_scene::*;

const TOLERANCE: u8 = 3;

/// An X, 16 × 16, black: a symbolic icon (drawn in the node's colour).
const CLOSE_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16">
<path d="M3 3 L13 13 M13 3 L3 13" stroke="black" stroke-width="2.5" fill="none"/></svg>"#;

/// A full-colour scalable app icon: a green rounded square with a white dot.
const APP_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48">
<rect x="4" y="4" width="40" height="40" rx="10" fill="#98c379"/>
<circle cx="24" cy="24" r="8" fill="#ffffff"/></svg>"##;

fn write_png(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) {
    let mut rgba = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgba.extend_from_slice(&f(x, y));
        }
    }
    common::write_png(path, w, h, &rgba);
}

/// The test icon theme, `StrandTest`, under a temporary
/// `XDG_DATA_DIRS`. freedesktop-icons reads the search paths once per
/// process, so every test here shares it and sets it up before the
/// first lookup.
fn theme_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("strand-icons-{}", std::process::id()));
        let theme = root.join("share/icons/StrandTest");
        std::fs::create_dir_all(theme.join("scalable/status")).unwrap();
        std::fs::create_dir_all(theme.join("scalable/apps")).unwrap();
        std::fs::create_dir_all(theme.join("16x16/devices")).unwrap();
        std::fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=StrandTest\nInherits=hicolor\n\
             Directories=scalable/status,scalable/apps,16x16/devices\n\n\
             [scalable/status]\nSize=16\nMinSize=8\nMaxSize=512\nType=Scalable\n\n\
             [scalable/apps]\nSize=48\nMinSize=8\nMaxSize=512\nType=Scalable\n\n\
             [16x16/devices]\nSize=16\nType=Fixed\n",
        )
        .unwrap();
        std::fs::write(
            theme.join("scalable/status/window-close-symbolic.svg"),
            CLOSE_SVG,
        )
        .unwrap();
        std::fs::write(theme.join("scalable/apps/strand-app.svg"), APP_SVG).unwrap();
        // A fixed-size bitmap: orange left half, blue right half.
        write_png(
            &theme.join("16x16/devices/battery-full.png"),
            16,
            16,
            |x, _| {
                if x < 8 {
                    [0xd1, 0x9a, 0x66, 0xff]
                } else {
                    [0x61, 0xaf, 0xef, 0xff]
                }
            },
        );
        // SAFETY: set before any thread of this test binary looks icons
        // up (the first call to `theme_dir` comes before every lookup).
        unsafe {
            std::env::set_var("XDG_DATA_DIRS", root.join("share"));
            std::env::set_var("XDG_DATA_HOME", root.join("home-share"));
            std::env::set_var("HOME", root.join("home"));
        }
        root
    })
}

fn renderer_with_theme() -> Renderer {
    theme_dir();
    let mut r = renderer();
    r.set_icon_theme_inline("StrandTest");
    r
}

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn scene(photo: &str, gradient: &str) -> SceneDiff {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(280.0)),
            (Prop::Height, num(60.0)),
            (Prop::Bg, color("#282c34")),
            (Prop::Color, color("#e5c07b")),
        ],
    );
    let row = b.node(
        NodeKind::Row,
        Some(root),
        vec![(Prop::Pad, num(8.0)), (Prop::Gap, num(8.0))],
    );
    // A symbolic icon in the inherited colour, one in its own colour.
    b.node(
        NodeKind::Icon,
        Some(row),
        vec![
            (Prop::Source, text("window-close-symbolic")),
            (Prop::Size, num(20.0)),
        ],
    );
    b.node(
        NodeKind::Icon,
        Some(row),
        vec![
            (Prop::Source, text("window-close-symbolic")),
            (Prop::Size, num(14.0)),
            (Prop::Color, color("#e06c75")),
        ],
    );
    // A full-colour icon by name through `image` (an app's icon), at 32.
    b.node(
        NodeKind::Image,
        Some(row),
        vec![(Prop::Source, text("strand-app")), (Prop::Size, num(32.0))],
    );
    // A fixed-size bitmap icon scaled up.
    b.node(
        NodeKind::Icon,
        Some(row),
        vec![
            (Prop::Source, text("battery-full")),
            (Prop::Size, num(24.0)),
        ],
    );
    // A JPEG file, covering a square with rounded corners.
    b.node(
        NodeKind::Image,
        Some(row),
        vec![
            (Prop::Source, text(photo)),
            (Prop::Size, num(36.0)),
            (Prop::Fit, PropValue::Keyword("cover".into())),
            (Prop::Radius, num(8.0)),
        ],
    );
    // A PNG file, contained (letterboxed).
    b.node(
        NodeKind::Image,
        Some(row),
        vec![
            (Prop::Source, text(gradient)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(30.0)),
        ],
    );
    // A missing icon draws nothing and breaks nothing.
    b.node(
        NodeKind::Icon,
        Some(row),
        vec![
            (Prop::Source, text("no-such-icon")),
            (Prop::Size, num(16.0)),
        ],
    );
    b.diff
}

fn gradient_png() -> String {
    let path = theme_dir().join("gradient.png");
    if !path.exists() {
        write_png(&path, 64, 32, |x, _| {
            let v = (x * 4) as u8;
            [v, 255 - v, 128, 255]
        });
    }
    path.to_string_lossy().into_owned()
}

fn show(r: &mut Renderer, diff: SceneDiff) -> Buffer {
    assert!(r.apply(diff).is_empty());
    let root = r.tree().roots()[0];
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(280, 60), Scale::ONE);
    let mut buf = Buffer::new(280, 60, Scale::ONE);
    buf.paint(r, SurfaceId(1), 0);
    buf
}

#[test]
fn icons_and_images_at_drawn_size() {
    let mut r = renderer_with_theme();
    let buf = show(&mut r, scene(&fixture("halves.jpg"), &gradient_png()));
    let boxes = r.boxes(SurfaceId(1)).unwrap().rects.clone();
    let mut nodes: Vec<(NodeId, LogicalRect)> = boxes.into_iter().collect();
    nodes.sort_by_key(|(n, _)| n.index);
    let rect = |i: usize| nodes.iter().find(|(n, _)| n.index as usize == i).unwrap().1;
    let px =
        |b: LogicalRect, fx: f32, fy: f32| buf.px((b.x + b.w * fx) as u32, (b.y + b.h * fy) as u32);
    // The symbolic X at its centre is the inherited colour (#e5c07b).
    let x = px(rect(2), 0.5, 0.5);
    assert!(x[2] > 0xd0 && x[1] > 0xa0 && x[0] < 0x90, "{x:?}");
    // ... and the second in its own colour (#e06c75).
    let x2 = px(rect(3), 0.5, 0.5);
    assert!(x2[2] > 0xc0 && x2[1] < 0x90, "{x2:?}");
    // The app icon keeps its colours: green body, white dot.
    let body = px(rect(4), 0.2, 0.5);
    assert!(body[1] > body[2] && body[1] > body[0], "{body:?}");
    assert!(px(rect(4), 0.5, 0.5)[0] > 0xf0);
    // The bitmap scaled to 24: orange then blue.
    assert!(px(rect(5), 0.2, 0.5)[2] > 0xc0);
    assert!(px(rect(5), 0.8, 0.5)[0] > 0xc0);
    // The JPEG covering a square: red on the left, blue on the right
    // (2:1 cropped to its middle).
    let (l, rr) = (px(rect(6), 0.2, 0.5), px(rect(6), 0.8, 0.5));
    assert!(l[2] > 0xc0 && rr[0] > 0xc0, "{l:?} {rr:?}");
    // The PNG contained: 40 × 30 holds a 2:1 image as 40 × 20, bands
    // of panel above and below.
    assert_eq!(px(rect(7), 0.5, 0.05), [0x34, 0x2c, 0x28, 0xff]);
    assert_ne!(px(rect(7), 0.5, 0.5), [0x34, 0x2c, 0x28, 0xff]);
    assert!(r.image_bytes() > 0 && r.image_bytes() <= IMAGE_CACHE_BYTES);
    assert_matches_ref("images_icons", &buf, TOLERANCE);
}

/// At 2× everything is decoded at twice the size: sharp, not scaled up.
#[test]
fn icons_are_decoded_for_the_scale() {
    theme_dir();
    let theme = IconTheme::named("StrandTest");
    let key = |w| ImageKey {
        source: "window-close-symbolic".into(),
        icon: true,
        w,
        h: w,
        fit: Fit::Contain,
        scale: (w / 16) as u16,
    };
    let one = load(&key(16), &theme).unwrap();
    let two = load(&key(32), &theme).unwrap();
    assert!(one.symbolic && two.symbolic);
    assert_eq!((one.pixmap.width(), two.pixmap.width()), (16, 32));
    // The stroke is about 2.5 px at 1× and 5 px at 2×.
    let ink = |p: &vello_cpu::Pixmap| p.data().iter().filter(|c| c.a > 128).count();
    let (a, b) = (ink(&one.pixmap), ink(&two.pixmap));
    assert!(b > 3 * a && b < 5 * a, "{a} {b}");
}

/// Decoded images stay within 6 MB however many are drawn: the least
/// recently drawn go first.
#[test]
fn the_image_cache_holds_six_megabytes() {
    let mut r = renderer_with_theme();
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Width, num(600.0)), (Prop::Height, num(600.0))],
    );
    let photo = fixture("halves.jpg");
    // Ten 512 × 512 decodes (1 MB each) of the same file, stacked.
    for i in 0..10 {
        b.node(
            NodeKind::Image,
            Some(root),
            vec![
                (Prop::Source, text(&photo)),
                (Prop::Place, PropValue::Keyword("absolute".into())),
                (Prop::Width, num(512.0 - i as f32)),
                (Prop::Height, num(512.0)),
            ],
        );
    }
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(600, 600), Scale::ONE);
    let mut buf = Buffer::new(600, 600, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    // One frame needs all ten (10 MB): kept while drawn. After the frame
    // a new image pushes the oldest out.
    let mut d = SceneDiff::new();
    for i in 0..10u32 {
        d.push(SceneOp::Remove {
            id: NodeId::new(i + 1, 0),
        });
    }
    r.apply(d);
    buf.paint(&mut r, SurfaceId(1), 1);
    let mut d = SceneDiff::new();
    d.create(NodeId::new(20, 0), NodeKind::Image, Some(root), 0)
        .set(NodeId::new(20, 0), Prop::Source, text(&photo))
        .set(NodeId::new(20, 0), Prop::Size, num(100.0));
    r.apply(d);
    buf.paint(&mut r, SurfaceId(1), 1);
    assert!(
        r.image_bytes() <= IMAGE_CACHE_BYTES,
        "{} bytes",
        r.image_bytes()
    );
}

/// With a text worker, images decode on a worker too and the frame that
/// follows the delivery draws them.
#[test]
fn a_worker_decodes_off_the_render_thread() {
    use strand_text::{FontConfig, TextWorker, test_font_path};
    theme_dir();
    let data = std::fs::read(test_font_path()).unwrap();
    let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w2 = woken.clone();
    let worker = TextWorker::spawn_with_waker(
        FontConfig::isolated(vec![std::sync::Arc::new(data)]),
        Some(Box::new(move || {
            w2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
    )
    .unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Width, num(40.0)), (Prop::Height, num(40.0))],
    );
    b.node(
        NodeKind::Image,
        Some(root),
        vec![
            (Prop::Source, text(&fixture("halves.jpg"))),
            (Prop::Size, num(40.0)),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(40, 40), Scale::ONE);
    let mut buf = Buffer::new(40, 40, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while r.image_bytes() == 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        r.update();
    }
    assert!(r.image_bytes() > 0, "decoded");
    assert!(
        woken.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "woke the loop"
    );
    use strand_scene::Painter;
    // A frame is wanted to draw it, unless the decode was so quick that
    // the first frame's paint already took it in and drew it.
    let drawn_first = buf.px(5, 20)[2] > 0xc0;
    assert!(r.wants_frame(SurfaceId(1)) || drawn_first);
    buf.paint(&mut r, SurfaceId(1), 1);
    assert!(buf.px(5, 20)[2] > 0xc0, "drawn: {:?}", buf.px(5, 20));
}

/// With the worker, an image whose box springs to a new size keeps
/// drawing its last decode, scaled into the box, on every frame of the
/// spring; the sizes it passes through are never decoded, and the size
/// it rests at is.
#[test]
fn a_springing_image_draws_its_last_decode_scaled() {
    use std::time::Duration;
    use strand_text::{FontConfig, TextWorker, test_font_path};
    theme_dir();
    let data = std::fs::read(test_font_path()).unwrap();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::isolated(vec![std::sync::Arc::new(data)]), None)
            .unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#000000")),
        ],
    );
    let img = b.node(
        NodeKind::Image,
        Some(root),
        vec![
            (Prop::Source, text(&fixture("halves.jpg"))),
            (Prop::Fit, PropValue::Keyword("fill".into())),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(40.0)),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(100, 40), Scale::ONE);
    let mut buf = Buffer::new(100, 40, Scale::ONE);
    let mut t = Duration::from_secs(1);
    let wait = |r: &mut Renderer, bytes: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while r.image_bytes() < bytes && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            r.update();
        }
    };
    buf.paint_at(&mut r, SurfaceId(1), 0, t);
    wait(&mut r, 40 * 40 * 4);
    t += Duration::from_millis(16);
    buf.paint_at(&mut r, SurfaceId(1), 1, t);
    assert!(buf.px(5, 20)[2] > 0xc0, "drawn: {:?}", buf.px(5, 20));

    let mut d = SceneDiff::new();
    d.set(img, Prop::Width, num(80.0));
    assert!(r.apply(d).is_empty());
    let mut frames = 0;
    use strand_scene::Painter;
    while r.wants_frame(SurfaceId(1)) && frames < 200 {
        t += Duration::from_millis(16);
        buf.paint_at(&mut r, SurfaceId(1), 1, t);
        r.update();
        frames += 1;
        // Red on the left, and blue reaching past the old 40 px once the
        // box has grown: the 40 px decode, stretched.
        let left = buf.px(3, 20);
        assert!(
            left[2] > 0xc0 && left[2] > left[0] + 0x40,
            "frame {frames}: {left:?}"
        );
        let w = r.boxes(SurfaceId(1)).unwrap().rects[&img].w;
        if w > 50.0 {
            let right = buf.px((w - 3.0) as u32, 20);
            assert!(right[0] > 0xc0, "frame {frames} at {w}: {right:?}");
        }
    }
    assert!(frames > 5, "it sprang over {frames} frames");
    // At rest: the 80 px decode arrives, and only it was decoded.
    wait(&mut r, (40 * 40 + 80 * 40) * 4);
    assert_eq!(r.image_bytes(), (40 * 40 + 80 * 40) * 4);
    t += Duration::from_millis(16);
    buf.paint_at(&mut r, SurfaceId(1), 1, t);
    assert!(buf.px(77, 20)[0] > 0xc0, "{:?}", buf.px(77, 20));
    assert!(buf.px(3, 20)[2] > 0xc0);
}

/// A name the theme lacks falls back to its symbolic variant, then to
/// generic names (`window-close-tab` → `window-close` →
/// `window-close-symbolic`), as GTK does.
#[test]
fn icon_lookup_falls_back_to_symbolic_and_generic_names() {
    theme_dir();
    let theme = IconTheme::named("StrandTest");
    let key = |s: &str| ImageKey {
        source: s.into(),
        icon: true,
        w: 16,
        h: 16,
        fit: Fit::Contain,
        scale: 1,
    };
    for name in [
        "window-close",
        "window-close-tab",
        "window-close-tab-symbolic",
    ] {
        let d = load(&key(name), &theme).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(d.symbolic, "{name} found the symbolic icon");
    }
    assert!(load(&key("battery-full-charging"), &theme).is_ok());
    assert!(load(&key("nothing-like-this"), &theme).is_err());
}

/// design.md, "Change sources": icons are a cache the icon theme's
/// changes invalidate. An icon missing from the theme draws nothing and
/// that miss is remembered (no lookup per frame); once it is installed
/// and the theme is reported changed (`index.theme`, the watcher's
/// `CacheKind::Icons`), the renderer looks it up afresh and draws it.
#[test]
fn an_icon_theme_change_redraws_icons_looked_up_afresh() {
    let dir = theme_dir();
    let mut r = renderer_with_theme();
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(40.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#000000")),
        ],
    );
    b.node(
        NodeKind::Image,
        Some(root),
        vec![
            (Prop::Source, text("strand-installed-later")),
            (Prop::Size, num(40.0)),
        ],
    );
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(40, 40), Scale::ONE);
    let mut buf = Buffer::new(40, 40, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    let body = |buf: &Buffer| buf.px(8, 20);
    assert_eq!(body(&buf), [0, 0, 0, 0xff], "missing: nothing drawn");

    let file = dir.join("share/icons/StrandTest/scalable/apps/strand-installed-later.svg");
    std::fs::write(&file, APP_SVG).unwrap();
    buf.paint(&mut r, SurfaceId(1), 1);
    assert_eq!(body(&buf), [0, 0, 0, 0xff], "the miss is remembered");

    r.icons_changed();
    use strand_scene::Painter;
    assert!(r.wants_frame(SurfaceId(1)), "the icon's surface repaints");
    buf.paint(&mut r, SurfaceId(1), 2);
    let px = body(&buf);
    assert!(
        px[1] > px[0] && px[1] > px[2] && px[1] > 0x80,
        "drawn: {px:?}"
    );

    // Removed and reported again: gone.
    std::fs::remove_file(&file).unwrap();
    r.icons_changed();
    buf.paint(&mut r, SurfaceId(1), 3);
    assert_eq!(body(&buf), [0, 0, 0, 0xff], "removed");
}
