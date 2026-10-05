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
        ..AtlasConfig::default()
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

/// Values from user expressions (`13/0`) or services must never hang or
/// kill shaping.
#[test]
fn non_finite_sizes_and_widths_are_safe() {
    let mut e = TextEngine::new(config());
    let bad = [f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1e30, -5.0, 0.0];
    let mut key = 0;
    for v in bad {
        key += 1;
        let l = e.layout(&request(key, "12:59", v, Scale::ONE));
        assert!(l.size.w.is_finite() && l.size.h.is_finite(), "size {v}");
        for w in bad {
            key += 1;
            let mut r = request(key, "a b c d", 13.0, Scale::ONE);
            r.max_width = Some(w);
            r.style.line_height = Some(v);
            let l = e.layout(&r);
            assert!(l.size.w.is_finite(), "width {w}");
        }
    }
    // A huge size is capped, not dropped.
    let l = e.layout(&request(1000, "8", 1e30, Scale::ONE));
    assert!(
        l.ink.h as f32 <= MAX_FONT_PX && l.ink.h > 100,
        "{:?}",
        l.ink
    );
}

/// A lock-screen clock: 200 px at 2× needs glyphs bigger than an atlas page.
#[test]
fn oversized_glyphs_draw() {
    let mut e = TextEngine::new(config());
    let l = e.layout(&request(1, "12:59", 200.0, Scale::new(240).unwrap()));
    assert_eq!(l.glyphs().count(), 5);
    assert!(l.glyphs().filter(|g| g.slot.h > 256).count() == 4, "digits");
    for g in l.glyphs() {
        let up = l
            .uploads
            .iter()
            .find(|u| u.page == g.slot.page && (u.x, u.y) == (g.slot.x, g.slot.y))
            .unwrap();
        assert!(up.page_size as u32 > g.slot.h.max(g.slot.w) as u32);
        assert!(up.alpha.contains(&255));
    }
}

/// Dropping a scale's atlas makes the next layout at that scale upload its
/// glyphs again, on pages whose generations were never used before.
#[test]
fn dropped_scale_reuploads() {
    let mut e = TextEngine::new(config());
    let a = e.layout(&request(1, "12:59", 13.0, Scale::ONE));
    assert!(
        e.layout(&request(2, "12:59", 13.0, Scale::ONE))
            .uploads
            .is_empty()
    );
    e.drop_scale(Scale::ONE);
    assert_eq!(e.atlas_pages(Scale::ONE), 0);
    let b = e.layout(&request(3, "12:59", 13.0, Scale::ONE));
    assert_eq!(b.uploads.len(), a.uploads.len());
    for (x, y) in a.uploads.iter().zip(&b.uploads) {
        assert_ne!(x.page.generation, y.page.generation);
    }
}

/// Cancelled requests that are still queued are skipped.
#[test]
fn worker_skips_cancelled_requests() {
    let w = TextWorker::spawn(config()).unwrap();
    // Keep the worker busy so the rest queue up behind it.
    w.request(request(1, &"x".repeat(2000), 13.0, Scale::ONE))
        .unwrap();
    for k in 2..20 {
        w.request(request(k, "12:59", 13.0, Scale::ONE)).unwrap();
        if k < 19 {
            w.cancel(TextKey(k)).unwrap();
        }
    }
    let mut got = Vec::new();
    while let Some(l) = w.recv_timeout(Duration::from_secs(10)).unwrap() {
        got.push(l.key.0);
        if l.key.0 == 19 {
            break;
        }
    }
    assert_eq!(got.first(), Some(&1));
    assert_eq!(got.last(), Some(&19));
    // Requests queued behind the first one were drained and skipped; at
    // most the ones that arrived before the drain could have run.
    assert!(got.len() < 18, "{got:?}");
}

/// One layout of many distinct huge glyphs stays within the atlas byte
/// budget instead of allocating a page per glyph.
#[test]
fn huge_distinct_glyphs_stay_within_the_byte_budget() {
    let mut e = TextEngine::new(config());
    let text = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let l = e.layout(&request(1, text, 1e30, Scale::ONE));
    assert!(l.glyphs().count() > 0, "some glyphs still draw");
    assert!(
        e.atlas_bytes(Scale::ONE) <= AtlasConfig::default().max_bytes,
        "{} bytes in {} pages",
        e.atlas_bytes(Scale::ONE),
        e.atlas_pages(Scale::ONE)
    );
    // Text past MAX_TEXT_BYTES is cut, at a character boundary.
    let long = "é".repeat(strand_text::MAX_TEXT_BYTES);
    let l = e.layout(&request(2, &long, 13.0, Scale::ONE));
    assert!(l.size.w.is_finite());
}

fn styled(
    key: u64,
    text: &str,
    max_width: Option<f32>,
    f: impl FnOnce(&mut TextStyle),
) -> TextRequest {
    let mut r = request(key, text, 13.0, Scale::ONE);
    r.max_width = max_width;
    f(&mut r.style);
    r
}

/// `text title { max_width: 40%; ellipsis: end }`: a long title is cut to
/// one line that fits, instead of wrapping inside a 36 px bar.
#[test]
fn ellipsis_cuts_to_one_line_that_fits() {
    let mut e = TextEngine::new(config());
    let title = "Firefox — The Rust Programming Language — Chapter 16: Fearless Concurrency";
    let one_line = e.layout(&request(1, "Firefox", 13.0, Scale::ONE)).size.h;
    let wrapped = e.layout(&styled(2, title, Some(150.0), |_| {}));
    assert!(wrapped.size.h > 2.0 * one_line, "wraps without ellipsis");
    for (k, ellipsis) in [Ellipsis::End, Ellipsis::Start, Ellipsis::Middle]
        .into_iter()
        .enumerate()
    {
        let l = e.layout(&styled(3 + k as u64, title, Some(150.0), |s| {
            s.ellipsis = Some(ellipsis)
        }));
        assert_eq!(l.size.h, one_line, "{ellipsis:?}");
        assert!(
            l.size.w <= 150.5 && l.size.w > 120.0,
            "{ellipsis:?}: {}",
            l.size.w
        );
    }
    // Text that fits is untouched.
    let short = e.layout(&styled(9, "Firefox", Some(150.0), |s| {
        s.ellipsis = Some(Ellipsis::End)
    }));
    let plain = e.layout(&request(10, "Firefox", 13.0, Scale::ONE));
    assert_eq!(short.glyphs().count(), plain.glyphs().count());
}

/// `text n.body { max_lines: 4 }` keeps at most four lines; with an
/// ellipsis the last kept line ends in "…" and still fits.
#[test]
fn max_lines_limits_wrapped_text() {
    let mut e = TextEngine::new(config());
    let body = "word ".repeat(200);
    let line = e.layout(&request(1, "word", 13.0, Scale::ONE)).size.h;
    let l = e.layout(&styled(2, &body, Some(120.0), |s| s.max_lines = Some(4)));
    assert!(
        (l.size.h - 4.0 * line).abs() < 0.5,
        "{} vs {}",
        l.size.h,
        line
    );
    let l = e.layout(&styled(3, &body, Some(120.0), |s| {
        s.max_lines = Some(2);
        s.ellipsis = Some(Ellipsis::End);
    }));
    assert!((l.size.h - 2.0 * line).abs() < 0.5);
    assert!(l.size.w <= 120.5);
}

/// Spans restyle ranges: marks get their colour on their own glyph run,
/// and a weight span changes shaping.
#[test]
fn spans_colour_and_weight_ranges() {
    let mut e = TextEngine::new(config());
    let accent = strand_scene::Color::from_hex("#89b4fa").unwrap();
    let l = e.layout(&styled(1, "Firefox", None, |s| {
        s.spans = vec![TextSpan {
            range: 0..4,
            color: Some(accent),
            ..TextSpan::default()
        }]
    }));
    let marked: usize = l
        .runs
        .iter()
        .filter(|r| r.color == Some(accent))
        .map(|r| r.glyphs.len())
        .sum();
    let plain: usize = l
        .runs
        .iter()
        .filter(|r| r.color.is_none())
        .map(|r| r.glyphs.len())
        .sum();
    assert_eq!((marked, plain), (4, 3));
    // Out-of-range and mid-character spans are clipped, not a panic.
    let l = e.layout(&styled(2, "héllo", None, |s| {
        s.spans = vec![TextSpan {
            range: 2..99,
            italic: true,
            weight: Some(700),
            ..TextSpan::default()
        }]
    }));
    assert_eq!(l.glyphs().count(), 5);
}

/// A layout that could not place every glyph says so, and reports the
/// atlas pages that are still live.
#[test]
fn layouts_report_missing_glyphs_and_live_pages() {
    let mut cfg = config();
    cfg.atlas = AtlasConfig {
        page_size: 64,
        max_pages: 1,
        max_bytes: 64 * 64,
    };
    let mut e = TextEngine::new(cfg);
    let a = e.layout(&request(1, "ABCDEFGHIJKLMNOPQRSTUVWXYZ", 40.0, Scale::ONE));
    assert!(a.is_incomplete());
    let pages = a.atlas_pages().unwrap();
    assert_eq!(pages.len(), 1);
    assert!(a.glyphs().all(|g| pages.contains(&g.slot.page)));
    // While `a` leases the only page there is no room; once it is gone a
    // retry completes.
    assert!(
        e.layout(&request(2, "12", 13.0, Scale::ONE))
            .is_incomplete()
    );
    drop(a);
    let b = e.layout(&request(2, "12", 13.0, Scale::ONE));
    assert!(!b.is_incomplete());
    assert!(
        TextLayout::empty(TextKey(3), Scale::ONE)
            .atlas_pages()
            .is_none()
    );
}

/// Dropping the worker with a long queue does not shape the queue first.
#[test]
fn dropping_the_worker_discards_its_queue() {
    let big = "lorem ipsum ".repeat(MAX_TEXT_BYTES / 12);
    // How long one big request takes here (debug builds are slow).
    let mut e = TextEngine::new(config());
    let t = std::time::Instant::now();
    e.layout(&request(0, &big, 13.0, Scale::ONE));
    let one = t.elapsed();
    let w = TextWorker::spawn(config()).unwrap();
    for k in 1..=40 {
        w.request(request(k, &big, 13.0, Scale::ONE)).unwrap();
    }
    let t = std::time::Instant::now();
    drop(w);
    // At most the request in progress (plus font loading) finishes.
    assert!(
        t.elapsed() < one * 4 + Duration::from_millis(200),
        "{:?} with one request taking {one:?}",
        t.elapsed()
    );
}
