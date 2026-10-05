//! Shaping and atlas tests against the vendored Liberation Sans, so results
//! do not depend on the fonts installed on the machine.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use strand_scene::{Font, Scale};
use strand_text::*;

fn config() -> FontConfig {
    let data = std::fs::read(test_font_path()).unwrap();
    FontConfig::isolated(vec![Arc::new(data)])
}

fn request(key: u64, text: &str, size: f32, scale: Scale) -> TextRequest {
    TextRequest {
        key: TextKey(key),
        text: text.into(),
        style: TextStyle {
            font: Font {
                family: TEST_FONT_FAMILY.into(),
                size,
                weight: 400,
            },
            ..TextStyle::default()
        },
        max_width: None,
        scale,
    }
}

#[test]
fn vendored_font_is_the_only_family() {
    let mut e = TextEngine::new(config());
    assert_eq!(e.family_names(), vec![TEST_FONT_FAMILY.to_string()]);
}

#[test]
fn clock_text_has_font_metrics_width() {
    let mut e = TextEngine::new(config());
    let l = e.layout(&request(1, "12:59", 13.0, Scale::ONE));
    // Liberation Sans: digits advance 1139/2048 em, colon 569/2048 em.
    let expected = (4.0 * 1139.0 + 569.0) / 2048.0 * 13.0;
    assert!(
        (l.size.w - expected).abs() < 0.01,
        "{} vs {expected}",
        l.size.w
    );
    assert!(l.size.h > 13.0 && l.size.h < 20.0, "{}", l.size.h);
    assert_eq!(l.glyphs().count(), 5);
    assert!(l.baseline > 10.0 && l.baseline < l.size.h);
    // Ink sits inside the line box, give or take antialiasing.
    assert!(l.ink.x >= -1 && l.ink.right() <= l.size.w.ceil() as i64 + 1);
    assert!(l.ink.y >= 0 && l.ink.bottom() <= l.size.h.ceil() as i64);
    assert_eq!(l.key, TextKey(1));
}

#[test]
fn glyphs_are_cached_and_layouts_deterministic() {
    let mut e = TextEngine::new(config());
    let a = e.layout(&request(1, "12:59", 13.0, Scale::ONE));
    assert!(!a.uploads.is_empty());
    // Every placed glyph has an upload covering it, with real coverage.
    for g in a.glyphs() {
        let up = a
            .uploads
            .iter()
            .find(|u| u.page == g.slot.page && u.x == g.slot.x && u.y == g.slot.y)
            .unwrap();
        assert_eq!(up.alpha.len(), g.slot.w as usize * g.slot.h as usize);
        assert!(up.alpha.iter().any(|&v| v > 200));
    }
    let b = e.layout(&request(2, "12:59", 13.0, Scale::ONE));
    assert!(b.uploads.is_empty(), "second layout reuses the atlas");
    assert_eq!(a.runs, b.runs);
    // "13:00" only rasterises the new digits; "1" and ":" sit at the same
    // subpixel positions as in "12:59" and come from the cache.
    let c = e.layout(&request(3, "13:00", 13.0, Scale::ONE));
    let (ag, cg): (Vec<_>, Vec<_>) = (a.glyphs().collect(), c.glyphs().collect());
    assert_eq!(ag[0], cg[0]);
    assert_eq!(ag[2], cg[2]);
    for u in &c.uploads {
        assert!(cg.iter().any(|g| g.slot.x == u.x && g.slot.y == u.y));
        assert!(!ag.iter().any(|g| g.slot.x == u.x && g.slot.y == u.y));
    }
    assert!((2..=3).contains(&c.uploads.len()), "{:?}", c.uploads);
}

#[test]
fn atlases_are_per_scale() {
    let mut e = TextEngine::new(config());
    let s1 = e.layout(&request(1, "Strand", 13.0, Scale::ONE));
    let s2 = e.layout(&request(2, "Strand", 13.0, Scale::new(180).unwrap()));
    assert!(
        !s2.uploads.is_empty(),
        "a new scale rasterises its own glyphs"
    );
    assert_eq!(e.atlas_pages(Scale::ONE), 1);
    assert_eq!(e.atlas_pages(Scale::new(180).unwrap()), 1);
    // Same logical box, 1.5× the pixels.
    assert!((s1.size.w - s2.size.w).abs() < 0.5);
    let r = s2.ink.w as f32 / s1.ink.w as f32;
    assert!((r - 1.5).abs() < 0.15, "{r}");
    assert!(
        s2.glyphs()
            .all(|g| g.slot.page.scale == Scale::new(180).unwrap())
    );
}

#[test]
fn wrapping_and_alignment() {
    let mut e = TextEngine::new(config());
    let one = e.layout(&request(1, "hello world again", 13.0, Scale::ONE));
    let mut req = request(2, "hello world again", 13.0, Scale::ONE);
    req.max_width = Some(50.0);
    let wrapped = e.layout(&req);
    assert!(wrapped.size.h > 2.0 * one.size.h - 1.0, "{wrapped:?}");
    assert!(wrapped.size.w <= 50.0);
    req.style.align = TextAlign::End;
    req.max_width = Some(200.0);
    let end = e.layout(&req);
    assert!(
        end.ink.x > 100,
        "end-aligned text starts right: {:?}",
        end.ink
    );
}

#[test]
fn atlas_is_lru_bounded_while_layouts_are_dropped() {
    let mut cfg = config();
    cfg.atlas = AtlasConfig {
        page_size: 64,
        max_pages: 2,
    };
    let mut e = TextEngine::new(cfg);
    for (i, size) in (8..23).enumerate() {
        let l = e.layout(&request(i as u64, "ABCDEFGH", size as f32, Scale::ONE));
        assert_eq!(l.glyphs().count(), 8);
        drop(l);
        assert!(e.atlas_pages(Scale::ONE) <= 2);
    }
    // Held layouts pin their pages: the atlas grows rather than corrupt them.
    let held: Vec<_> = (0..6)
        .map(|i| e.layout(&request(100 + i, "WXYZ", 30.0 + i as f32, Scale::ONE)))
        .collect();
    assert!(e.atlas_pages(Scale::ONE) > 2);
    let pages: std::collections::HashSet<_> = held
        .iter()
        .flat_map(|l| l.glyphs().map(|g| g.slot.page))
        .collect();
    assert!(held.iter().all(|l| l.pages_leased() >= 1));
    assert!(!pages.is_empty());
}

#[test]
fn worker_round_trip_and_waker() {
    let woken = Arc::new(AtomicUsize::new(0));
    let w2 = woken.clone();
    let worker = TextWorker::spawn_with_waker(
        config(),
        Some(Box::new(move || {
            w2.fetch_add(1, Ordering::SeqCst);
        })),
    )
    .unwrap();
    worker
        .request(request(7, "12:59", 13.0, Scale::ONE))
        .unwrap();
    worker
        .request(request(8, "13:00", 13.0, Scale::ONE))
        .unwrap();
    let a = worker
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let b = worker
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!((a.key, b.key), (TextKey(7), TextKey(8)));
    assert!(worker.try_recv().unwrap().is_none());
    // The waker runs after each send; joining the worker orders it.
    drop(worker);
    assert_eq!(woken.load(Ordering::SeqCst), 2);
}

#[test]
fn system_fonts_resolve_generic_families() {
    // Uses whatever the machine has; only checks that shaping produces
    // glyphs when any font is installed.
    let mut e = TextEngine::new(FontConfig::default());
    if e.family_names().is_empty() {
        return;
    }
    let mut req = request(1, "Strand", 13.0, Scale::ONE);
    req.style.font.family = "sans-serif".into();
    let l = e.layout(&req);
    assert_eq!(l.glyphs().count(), 6);
}
