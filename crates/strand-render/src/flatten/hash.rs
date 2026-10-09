//! Paint signatures: hashing display items for damage diffing.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{Color, Paint};
use vello_cpu::kurbo::{self, BezPath};

use super::{FillShape, Item};

pub(super) fn hash_f32(h: &mut impl Hasher, v: f32) {
    v.to_bits().hash(h);
}

pub(super) fn hash_color(h: &mut impl Hasher, c: &Color) {
    for v in [c.r, c.g, c.b, c.a] {
        hash_f32(h, v);
    }
}

pub(super) fn hash_stops(h: &mut impl Hasher, stops: &[strand_scene::GradientStop]) {
    stops.len().hash(h);
    for st in stops {
        hash_f32(h, st.offset);
        hash_color(h, &st.color);
    }
}

pub(super) fn hash_paint(h: &mut impl Hasher, p: &Paint) {
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

pub(super) fn hash_rect(h: &mut impl Hasher, r: kurbo::Rect) {
    for v in [r.x0, r.y0, r.x1, r.y1] {
        v.to_bits().hash(h);
    }
}

pub(super) fn hash_path(h: &mut impl Hasher, p: &BezPath) {
    use kurbo::PathEl;
    let mut pt = |p: kurbo::Point| {
        p.x.to_bits().hash(h);
        p.y.to_bits().hash(h);
    };
    for el in p.elements() {
        match *el {
            PathEl::MoveTo(a) => pt(a),
            PathEl::LineTo(a) => pt(a),
            PathEl::QuadTo(a, b) => {
                pt(a);
                pt(b);
            }
            PathEl::CurveTo(a, b, c) => {
                pt(a);
                pt(b);
                pt(c);
            }
            PathEl::ClosePath => pt(kurbo::Point::new(f64::NAN, 0.0)),
        }
    }
}

pub(super) fn hash_item(h: &mut impl Hasher, item: &Item) {
    match item {
        Item::PushClip(p) => {
            0u8.hash(h);
            hash_path(h, p);
        }
        Item::PopClip => 1u8.hash(h),
        Item::PushOpacity(o) => {
            2u8.hash(h);
            hash_f32(h, *o);
        }
        Item::PopOpacity => 3u8.hash(h),
        Item::PushTransform(a) => {
            8u8.hash(h);
            for v in a.as_coeffs() {
                v.to_bits().hash(h);
            }
        }
        Item::PopTransform => 9u8.hash(h),
        Item::Shadow {
            rect,
            radii,
            std_dev,
            color,
            clip,
            extent,
        } => {
            4u8.hash(h);
            hash_rect(h, *rect);
            for r in radii {
                hash_f32(h, *r);
            }
            hash_f32(h, *std_dev);
            hash_color(h, color);
            hash_path(h, clip);
            hash_rect(h, *extent);
        }
        Item::Fill {
            shape,
            paint,
            frame,
        } => {
            5u8.hash(h);
            match shape {
                FillShape::Rect(r) => hash_rect(h, *r),
                FillShape::Path(p) => hash_path(h, p),
            }
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Border { path, paint, frame } => {
            6u8.hash(h);
            hash_path(h, path);
            hash_paint(h, paint);
            hash_rect(h, *frame);
        }
        Item::Glyphs {
            x,
            y,
            layout,
            color,
            spans,
        } => {
            7u8.hash(h);
            (x, y, layout.key, layout.scale).hash(h);
            hash_color(h, color);
            for c in spans {
                hash_color(h, c);
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
            hash_rect(h, *rect);
            hash_rect(h, *dest);
            if let Some(c) = tint {
                hash_color(h, c);
            }
        }
    }
}
