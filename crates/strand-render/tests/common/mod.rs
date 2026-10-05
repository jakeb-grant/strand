//! Shared helpers for offline render tests: a deterministic renderer, an
//! in-memory wl_shm-style buffer, and PNG reference comparison.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use strand_render::{Renderer, TextBackend};
use strand_scene::*;
use strand_text::{FontConfig, TEST_FONT_FAMILY, TextEngine, test_font_path};

pub fn engine() -> TextEngine {
    let data = std::fs::read(test_font_path()).unwrap();
    TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]))
}

/// A renderer shaping text inline with the vendored font.
pub fn renderer() -> Renderer {
    Renderer::new(TextBackend::Inline(Box::new(engine())))
}

pub fn font(size: f32) -> Font {
    Font {
        family: TEST_FONT_FAMILY.into(),
        size,
        weight: 400,
    }
}

pub fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap()
}

/// Builds diffs with sequential ids.
#[derive(Default)]
pub struct Builder {
    pub diff: SceneDiff,
    next: u32,
}

impl Builder {
    pub fn node(
        &mut self,
        kind: NodeKind,
        parent: Option<NodeId>,
        props: Vec<(Prop, PropValue)>,
    ) -> NodeId {
        let id = NodeId::new(self.next, 0);
        self.next += 1;
        let index = u32::MAX;
        self.diff.create(id, kind, parent, index);
        for (p, v) in props {
            self.diff.set(id, p, v);
        }
        id
    }
}

pub fn num(v: f32) -> PropValue {
    PropValue::Number(v)
}

pub fn color(c: &str) -> PropValue {
    PropValue::Color(hex(c))
}

pub fn text(s: &str) -> PropValue {
    PropValue::Text(s.into())
}

/// A tightly packed ARGB8888 buffer.
pub struct Buffer {
    pub size: Size,
    pub scale: Scale,
    pub pixels: Vec<u8>,
}

impl Buffer {
    pub fn new(w: u32, h: u32, scale: Scale) -> Self {
        Self {
            size: Size::new(w, h),
            scale,
            pixels: vec![0; (w * h * 4) as usize],
        }
    }

    pub fn paint(&mut self, r: &mut Renderer, surface: SurfaceId, age: u8) -> Damage {
        let mut t = PaintTarget::new(
            &mut self.pixels,
            self.size,
            self.size.w * 4,
            self.scale,
            age,
        )
        .unwrap();
        r.paint(surface, &mut t)
    }

    /// Premultiplied BGRA of pixel (x, y).
    pub fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.size.w + x) * 4) as usize;
        self.pixels[i..i + 4].try_into().unwrap()
    }

    /// Straight-alpha RGBA bytes for PNG.
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len());
        for p in self.pixels.chunks_exact(4) {
            let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
            let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
            out.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
        }
        out
    }
}

pub fn refs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/refs")
}

pub fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
}

pub fn read_png(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgba, "{path:?}");
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Compares a rendered buffer with `refs/<name>.png` in premultiplied space,
/// allowing `tolerance` per channel. With `STRAND_BLESS=1` the reference is
/// (re)written instead. On mismatch the actual image is written next to the
/// reference as `<name>.actual.png`.
pub fn assert_matches_ref(name: &str, buf: &Buffer, tolerance: u8) {
    let path = refs_dir().join(format!("{name}.png"));
    let rgba = buf.to_rgba();
    if std::env::var_os("STRAND_BLESS").is_some() {
        write_png(&path, buf.size.w, buf.size.h, &rgba);
        return;
    }
    assert!(
        path.exists(),
        "missing reference {path:?}; run with STRAND_BLESS=1"
    );
    let (w, h, want) = read_png(&path);
    assert_eq!((w, h), (buf.size.w, buf.size.h), "{name}: size differs");
    let mut worst = 0u8;
    let mut bad = 0usize;
    for (i, (got, exp)) in buf
        .pixels
        .chunks_exact(4)
        .zip(want.chunks_exact(4))
        .enumerate()
    {
        let a = exp[3] as u32;
        let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        let exp_bgra = [pm(exp[2]), pm(exp[1]), pm(exp[0]), exp[3]];
        let d = got
            .iter()
            .zip(exp_bgra)
            .map(|(g, e)| g.abs_diff(e))
            .max()
            .unwrap();
        if d > tolerance {
            bad += 1;
            if bad == 1 {
                eprintln!(
                    "{name}: first mismatch at ({}, {}): got {got:?} want {exp_bgra:?}",
                    i as u32 % w,
                    i as u32 / w
                );
            }
        }
        worst = worst.max(d);
    }
    if bad > 0 {
        let actual = refs_dir().join(format!("{name}.actual.png"));
        write_png(&actual, w, h, &rgba);
        panic!(
            "{name}: {bad} pixels differ by more than {tolerance} (worst {worst}); wrote {actual:?}"
        );
    }
}
