//! Paint signatures: hashing display items for damage diffing.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{Color, Paint};
use vello_cpu::kurbo::{self, BezPath};

use super::{FillShape, Item};

pub(crate) fn hash_f32(h: &mut impl Hasher, v: f32) {
    v.to_bits().hash(h);
}

pub(crate) fn hash_color(h: &mut impl Hasher, c: &Color) {
    for v in [c.r, c.g, c.b, c.a] {
        hash_f32(h, v);
    }
}

pub(crate) fn hash_stops(h: &mut impl Hasher, stops: &[strand_scene::GradientStop]) {
    stops.len().hash(h);
    for st in stops {
        hash_f32(h, st.offset);
        hash_color(h, &st.color);
    }
}

pub(crate) fn hash_paint(h: &mut impl Hasher, p: &Paint) {
    match p {
        Paint::Solid(c) => {
            0u8.hash(h);
            hash_color(h, c);
        }
        Paint::Linear { angle, stops } => {
            1u8.hash(h);
            hash_f32(h, *angle);
            hash_stops(h, stops);
        }
        Paint::Radial { stops } => {
            2u8.hash(h);
            hash_stops(h, stops);
        }
        Paint::Conic { from, stops } => {
            3u8.hash(h);
            hash_f32(h, *from);
            hash_stops(h, stops);
        }
    }
}

pub(super) fn hash_path(h: &mut impl Hasher, p: &BezPath) {
    hash_path_from(h, p, Shift::NONE);
}

/// (m6-owner-fixes, bug 17) An origin coordinates are hashed from: a
/// fade layer's key ([`crate::offscreen`]) hashes its group's items from
/// a pixel of their own, so the same content drawn whole pixels away
/// hashes the same. Its coordinates are hashed to 1/4096 of a pixel, as
/// the same rounded box built at another place differs in the last bits
/// of its curves' points (`13.000000000000002` against `18.0`); two
/// shapes that close draw the same pixels. [`Shift::NONE`] hashes every
/// coordinate exactly as it is (damage signatures).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Shift {
    pub x: i64,
    pub y: i64,
}

impl Shift {
    pub(crate) const NONE: Shift = Shift { x: 0, y: 0 };

    /// Steps of a pixel a moved coordinate is hashed to.
    const STEPS: f64 = 4096.0;

    fn none(self) -> bool {
        self == Self::NONE
    }

    /// The bits hashed for `v`, `from` the origin.
    fn bits(self, v: f64, from: i64) -> u64 {
        if self.none() {
            v.to_bits()
        } else {
            Self::step(v - from as f64)
        }
    }

    /// `v` in steps of [`Shift::STEPS`] (a NaN apart from every number).
    fn step(v: f64) -> u64 {
        if v.is_nan() {
            u64::MAX
        } else {
            (v * Self::STEPS).round() as i64 as u64
        }
    }

    fn fx(self, v: f64) -> u64 {
        self.bits(v, self.x)
    }

    fn fy(self, v: f64) -> u64 {
        self.bits(v, self.y)
    }

    /// `a` seen from the origin: `T(-o) · a · T(o)`.
    fn affine(self, a: kurbo::Affine) -> [u64; 6] {
        let c = a.as_coeffs();
        if self.none() {
            return c.map(f64::to_bits);
        }
        let (ox, oy) = (self.x as f64, self.y as f64);
        let q = Self::step;
        [
            q(c[0]),
            q(c[1]),
            q(c[2]),
            q(c[3]),
            q(c[4] + c[0] * ox + c[2] * oy - ox),
            q(c[5] + c[1] * ox + c[3] * oy - oy),
        ]
    }
}

fn hash_rect_from(h: &mut impl Hasher, r: kurbo::Rect, o: Shift) {
    for v in [o.fx(r.x0), o.fy(r.y0), o.fx(r.x1), o.fy(r.y1)] {
        v.hash(h);
    }
}

fn hash_path_from(h: &mut impl Hasher, p: &BezPath, o: Shift) {
    use kurbo::PathEl;
    let pt = |h: &mut _, p: kurbo::Point| {
        o.fx(p.x).hash(h);
        o.fy(p.y).hash(h);
    };
    for el in p.elements() {
        match *el {
            PathEl::MoveTo(a) => pt(h, a),
            PathEl::LineTo(a) => pt(h, a),
            PathEl::QuadTo(a, b) => {
                pt(h, a);
                pt(h, b);
            }
            PathEl::CurveTo(a, b, c) => {
                pt(h, a);
                pt(h, b);
                pt(h, c);
            }
            // A mark no point hashes as, never moved by the shift.
            PathEl::ClosePath => {
                f64::NAN.to_bits().hash(h);
                0f64.to_bits().hash(h);
            }
        }
    }
}

pub(crate) fn hash_item(h: &mut impl Hasher, item: &Item) {
    hash_item_from(h, item, Shift::NONE);
}

/// [`hash_item`] with its coordinates from `o` ([`Shift`]).
pub(crate) fn hash_item_from(h: &mut impl Hasher, item: &Item, o: Shift) {
    let affine = |h: &mut _, a: kurbo::Affine| {
        for v in o.affine(a) {
            v.hash(h);
        }
    };
    match item {
        Item::PushClip(p) => {
            0u8.hash(h);
            hash_path_from(h, p, o);
        }
        Item::PopClip => 1u8.hash(h),
        Item::PushOpacity(alpha, _) => {
            2u8.hash(h);
            hash_f32(h, *alpha);
        }
        Item::PopOpacity => 3u8.hash(h),
        Item::PushTransform(a) => {
            8u8.hash(h);
            affine(h, *a);
        }
        Item::PopTransform => 9u8.hash(h),
        Item::PushLayer(l) => {
            12u8.hash(h);
            crate::layers::hash_effects(h, &l.effects);
            l.mosaic.hash(h);
            hash_rect_from(h, l.frame, o);
            hash_f32(h, l.scale);
            affine(h, l.xform);
            l.gpu
                .as_ref()
                .map(|g| {
                    (
                        Arc::as_ptr(&g.pixmap) as usize,
                        g.x - o.x as i32,
                        g.y - o.y as i32,
                    )
                })
                .hash(h);
        }
        Item::Raster {
            node, key, rect, ..
        } => {
            14u8.hash(h);
            (node, key).hash(h);
            hash_rect_from(h, *rect, o);
        }
        Item::PopLayer => 13u8.hash(h),
        Item::Shadow {
            rect,
            radii,
            std_dev,
            color,
            clip,
            extent,
        } => {
            4u8.hash(h);
            hash_rect_from(h, *rect, o);
            for r in radii {
                hash_f32(h, *r);
            }
            hash_f32(h, *std_dev);
            hash_color(h, color);
            hash_path_from(h, clip, o);
            hash_rect_from(h, *extent, o);
        }
        Item::Fill {
            shape,
            paint,
            frame,
        } => {
            5u8.hash(h);
            match shape {
                FillShape::Rect(r) => hash_rect_from(h, *r, o),
                FillShape::Path(p) => hash_path_from(h, p, o),
            }
            hash_paint(h, paint);
            hash_rect_from(h, *frame, o);
        }
        Item::Border { path, paint, frame } => {
            6u8.hash(h);
            hash_path_from(h, path, o);
            hash_paint(h, paint);
            hash_rect_from(h, *frame, o);
        }
        Item::Glyphs {
            x,
            y,
            layout,
            color,
            spans,
            fill,
        } => {
            7u8.hash(h);
            (
                i64::from(*x) - o.x,
                i64::from(*y) - o.y,
                layout.key,
                layout.scale,
            )
                .hash(h);
            hash_color(h, color);
            for c in spans {
                hash_color(h, c);
            }
            // (M4) A letter's layout is one glyph of its text's (with
            // the text's key): its glyph tells letters apart.
            if let [run] = layout.runs.as_slice()
                && let [g] = run.glyphs.as_slice()
            {
                (g.x, g.y, g.slot.page, g.slot.x, g.slot.y).hash(h);
            }
            if let Some(f) = fill {
                hash_paint(h, &f.paint);
                hash_rect_from(h, f.frame, o);
                hash_rect_from(h, f.area, o);
            }
        }
        Item::Image {
            pixmap,
            rect,
            dest,
            tint,
        } => {
            10u8.hash(h);
            (Arc::as_ptr(pixmap) as usize).hash(h);
            hash_rect_from(h, *rect, o);
            hash_rect_from(h, *dest, o);
            if let Some(c) = tint {
                hash_color(h, c);
            }
        }
    }
}
