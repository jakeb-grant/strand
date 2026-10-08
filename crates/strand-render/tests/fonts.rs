//! design.md, "Change sources": fonts are a cache the fontconfig
//! directories' changes invalidate. Text shaped before a font was
//! installed is shaped again with it once the fonts are reported changed
//! (the watcher's `CacheKind::Fonts`), inline and on the text worker.
//!
//! Its own test binary: fontconfig reads `FONTCONFIG_FILE` once per
//! process, and here it names a configuration over a temporary font
//! directory, so nothing depends on the fonts the machine has.

mod common;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::*;
use strand_render::{Renderer, TextBackend};
use strand_scene::*;
use strand_text::{FontConfig, TextEngine, TextWorker, test_font_path};

/// A font fontconfig always has here (fontique needs one): DejaVu Sans,
/// which CI installs (fonts-dejavu-core).
const FALLBACK: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";

/// The font directory fontconfig is pointed at: DejaVu Sans only at
/// first, so text asking for Liberation Sans falls back to it.
fn font_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("strand-fonts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fonts = root.join("fonts");
        std::fs::create_dir_all(&fonts).unwrap();
        std::fs::copy(FALLBACK, fonts.join("fallback.ttf")).unwrap();
        let conf = root.join("fonts.conf");
        std::fs::write(
            &conf,
            format!(
                "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n\
                 <fontconfig><dir>{}</dir><cachedir>{}</cachedir></fontconfig>\n",
                fonts.display(),
                root.join("cache").display()
            ),
        )
        .unwrap();
        // SAFETY: set before any thread of this binary uses fontconfig
        // (every test calls `font_dir` first, under `SERIAL`).
        unsafe {
            std::env::set_var("FONTCONFIG_FILE", &conf);
        }
        fonts
    })
}

/// The tests install and remove the same font: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn system_fonts() -> FontConfig {
    FontConfig {
        system_fonts: true,
        ..FontConfig::default()
    }
}

fn scene() -> (SceneDiff, NodeId) {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(200.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#000000")),
            (Prop::Color, color("#ffffff")),
            (Prop::Font, PropValue::Font(font(24.0))),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(root),
        vec![
            (Prop::X, num(4.0)),
            (Prop::Y, num(4.0)),
            (Prop::Text, text("Strand Hello")),
        ],
    );
    (b.diff, root)
}

/// Lit pixels in the buffer (the text's ink on black).
fn ink(buf: &Buffer) -> usize {
    let mut n = 0;
    for y in 0..40 {
        for x in 0..200 {
            if buf.px(x, y)[0] > 0x80 {
                n += 1;
            }
        }
    }
    n
}

fn install() -> PathBuf {
    let to = font_dir().join("installed.ttf");
    std::fs::copy(test_font_path(), &to).unwrap();
    to
}

/// Paints the scene once with an inline engine made now (fresh fonts).
fn fresh() -> Buffer {
    let mut r = Renderer::new(TextBackend::Inline(Box::new(TextEngine::new(
        system_fonts(),
    ))));
    let (diff, root) = scene();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(200, 40), Scale::ONE);
    let mut buf = Buffer::new(200, 40, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    buf
}

#[test]
fn installed_fonts_reshape_text_inline() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    font_dir();
    let before = fresh();
    let mut r = Renderer::new(TextBackend::Inline(Box::new(TextEngine::new(
        system_fonts(),
    ))));
    let (diff, root) = scene();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(200, 40), Scale::ONE);
    let mut buf = Buffer::new(200, 40, Scale::ONE);
    buf.paint(&mut r, SurfaceId(1), 0);
    assert!(ink(&buf) > 100, "drawn in the fallback font");
    assert_eq!(buf.to_rgba(), before.to_rgba());

    let file = install();
    let after = fresh();
    assert_ne!(
        after.to_rgba(),
        before.to_rgba(),
        "the fonts draw differently"
    );
    buf.paint(&mut r, SurfaceId(1), 0);
    assert_eq!(
        buf.to_rgba(),
        before.to_rgba(),
        "not told yet: the old shaping stays"
    );
    r.fonts_changed();
    buf.paint(&mut r, SurfaceId(1), 0);
    assert_eq!(
        buf.to_rgba(),
        after.to_rgba(),
        "shaped again with the new font"
    );
    std::fs::remove_file(file).unwrap();
}

#[test]
fn installed_fonts_reshape_text_on_the_worker() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    font_dir();
    let worker = TextWorker::spawn(system_fonts()).unwrap();
    let mut r = Renderer::new(TextBackend::Worker(worker));
    let (diff, root) = scene();
    assert!(r.apply(diff).is_empty());
    r.attach_surface(SurfaceId(1), root);
    r.configure_surface(SurfaceId(1), Size::new(200, 40), Scale::ONE);
    let mut buf = Buffer::new(200, 40, Scale::ONE);
    let settle = |r: &mut Renderer, buf: &mut Buffer| {
        // Shaping is off the thread: wait for it, then paint in full.
        r.wait_for_text(Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(5);
        while r.text_pending() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            r.update();
        }
        buf.paint(r, SurfaceId(1), 0);
    };
    settle(&mut r, &mut buf);
    let before = fresh();
    assert_eq!(
        buf.to_rgba(),
        before.to_rgba(),
        "drawn in the fallback font"
    );

    let file = install();
    let after = fresh();
    r.fonts_changed();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(10));
        r.update();
        settle(&mut r, &mut buf);
        if buf.to_rgba() == after.to_rgba() || Instant::now() > deadline {
            break;
        }
    }
    assert_eq!(
        buf.to_rgba(),
        after.to_rgba(),
        "shaped again with the new font"
    );
    std::fs::remove_file(file).unwrap();
}
