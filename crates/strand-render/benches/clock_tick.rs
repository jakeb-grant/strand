//! Clock-tick repaint: a 2560×36 bar whose clock text changes every
//! iteration, painted into a buffer of age 1 (only the damage repaints).
//! Compared with a full repaint of the same bar.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use strand_render::{Renderer, TextBackend};
use strand_scene::*;
use strand_text::{FontConfig, TEST_FONT_FAMILY, TextEngine, test_font_path};

const BAR: SurfaceId = SurfaceId(1);

fn setup() -> (Renderer, NodeId, Vec<u8>) {
    let data = std::fs::read(test_font_path()).expect("vendored test font");
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let id = |i| NodeId::new(i, 0);
    let mut d = SceneDiff::new();
    d.create(id(0), NodeKind::Bar, None, 0)
        .set(
            id(0),
            Prop::Bg,
            PropValue::Color(Color::from_rgba8(30, 30, 46, 230)),
        )
        .set(id(0), Prop::Radius, PropValue::Number(10.0))
        .set(
            id(0),
            Prop::Color,
            PropValue::Color(Color::from_rgba8(205, 214, 244, 255)),
        )
        .set(
            id(0),
            Prop::Font,
            PropValue::Font(Font {
                family: TEST_FONT_FAMILY.into(),
                size: 13.0,
                weight: 400,
            }),
        );
    for i in 0..5u32 {
        d.create(id(1 + i), NodeKind::Box, Some(id(0)), i)
            .set(
                id(1 + i),
                Prop::X,
                PropValue::Number(12.0 + 14.0 * i as f32),
            )
            .set(id(1 + i), Prop::Y, PropValue::Number(14.0))
            .set(id(1 + i), Prop::Size, PropValue::Number(8.0))
            .set(id(1 + i), Prop::Radius, PropValue::Number(999.0))
            .set(
                id(1 + i),
                Prop::Bg,
                PropValue::Color(Color::from_rgba8(137, 180, 250, 255)),
            );
    }
    let texts = [
        (100.0, "~/src/strand — nvim"),
        (1262.0, "12:59"),
        (2480.0, "87%"),
    ];
    for (i, (x, t)) in texts.iter().enumerate() {
        let n = id(6 + i as u32);
        d.create(n, NodeKind::Text, Some(id(0)), 5 + i as u32)
            .set(n, Prop::X, PropValue::Number(*x))
            .set(n, Prop::Y, PropValue::Number(10.0))
            .set(n, Prop::Text, PropValue::Text((*t).into()));
    }
    r.apply(d);
    r.attach_surface(BAR, id(0));
    let mut pixels = vec![0u8; 2560 * 36 * 4];
    let mut t = PaintTarget::new(&mut pixels, Size::new(2560, 36), 2560 * 4, Scale::ONE, 0)
        .expect("valid target");
    r.paint(BAR, &mut t);
    (r, id(7), pixels)
}

fn bench(c: &mut Criterion) {
    let (mut r, clock, mut pixels) = setup();
    let times = ["12:59", "13:00"];
    let mut n = 0usize;
    c.bench_function("clock_tick_repaint_2560x36", |b| {
        b.iter(|| {
            n += 1;
            let mut d = SceneDiff::new();
            d.set(clock, Prop::Text, PropValue::Text(times[n % 2].into()));
            r.apply(d);
            let mut t = PaintTarget::new(&mut pixels, Size::new(2560, 36), 2560 * 4, Scale::ONE, 1)
                .expect("valid target");
            let damage = r.paint(BAR, &mut t);
            assert!(damage.area() <= 2000);
            black_box(damage)
        })
    });
    c.bench_function("idle_paint_2560x36", |b| {
        b.iter(|| {
            let mut t = PaintTarget::new(&mut pixels, Size::new(2560, 36), 2560 * 4, Scale::ONE, 1)
                .expect("valid target");
            black_box(r.paint(BAR, &mut t))
        })
    });
    c.bench_function("full_repaint_2560x36", |b| {
        b.iter(|| {
            let mut t = PaintTarget::new(&mut pixels, Size::new(2560, 36), 2560 * 4, Scale::ONE, 0)
                .expect("valid target");
            black_box(r.paint(BAR, &mut t))
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
