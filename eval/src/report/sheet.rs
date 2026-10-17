//! Contact sheets (docs/eval.md, "Reading a failure"): for a failing
//! finding at tick `k` on a node, the frames `k−3 … k+3` side by side at
//! 2×, cropped around the node, its drawn box outlined red, its box at
//! the segment's start dashed yellow, its laid box dotted cyan, the ticks
//! and the finding burned in. A frame with no image (images thinned or
//! off) is drawn as a wireframe of the trace's boxes.

use std::collections::{BTreeMap, HashMap};

use crate::checks::{Finding, Timeline};
use crate::harness::Image;
use crate::report::{font, png};
use crate::trace::{Rect, RunTrace};

/// The frames either side of the finding's.
const SPAN: i64 = 3;
/// Physical pixels kept around the node.
const PAD: f32 = 16.0;
/// The widest crop, physical pixels.
const MAX_CROP: f32 = 360.0;
const ZOOM: u32 = 2;
const STRIP: u32 = 11;
const GAP: u32 = 6;

const BG: [u8; 4] = [24, 24, 28, 255];
const PANEL: [u8; 4] = [44, 44, 52, 255];
const TEXT: [u8; 4] = [235, 235, 240, 255];
const RED: [u8; 4] = [255, 64, 64, 255];
const YELLOW: [u8; 4] = [255, 210, 0, 255];
const CYAN: [u8; 4] = [0, 200, 255, 255];
const WIRE: [u8; 4] = [120, 120, 135, 255];

/// An RGBA canvas.
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32, fill: [u8; 4]) -> Canvas {
        let px = fill
            .iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect();
        Canvas { w, h, px }
    }

    fn set(&mut self, x: i64, y: i64, c: [u8; 4]) {
        if x < 0 || y < 0 || x >= i64::from(self.w) || y >= i64::from(self.h) {
            return;
        }
        let at = ((y as u32 * self.w + x as u32) * 4) as usize;
        self.px[at..at + 4].copy_from_slice(&c);
    }

    /// A rectangle's outline, `dash` pixels on and off (0: solid).
    fn outline(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, c: [u8; 4], dash: i64) {
        let on = |i: i64| dash == 0 || (i / dash) % 2 == 0;
        for x in x0..=x1 {
            if on(x - x0) {
                self.set(x, y0, c);
                self.set(x, y1, c);
            }
        }
        for y in y0..=y1 {
            if on(y - y0) {
                self.set(x0, y, c);
                self.set(x1, y, c);
            }
        }
    }

    fn fill(&mut self, x0: i64, y0: i64, w: u32, h: u32, c: [u8; 4]) {
        for y in y0..y0 + i64::from(h) {
            for x in x0..x0 + i64::from(w) {
                self.set(x, y, c);
            }
        }
    }

    fn text(&mut self, x: i64, y: i64, t: &str, c: [u8; 4]) {
        font::draw(&mut self.px, self.w, self.h, x, y, t, c);
    }
}

/// A crop of a surface: its origin and size in physical pixels.
#[derive(Clone, Copy, Debug)]
struct Crop {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => {
            let x0 = a[0].min(b[0]);
            let y0 = a[1].min(b[1]);
            let x1 = (a[0] + a[2]).max(b[0] + b[2]);
            let y1 = (a[1] + a[3]).max(b[1] + b[3]);
            [x0, y0, x1 - x0, y1 - y0]
        }
    }
}

/// The contact sheet of `f` as a PNG file, or `None` when the finding
/// names no node or tick the trace shows.
pub fn sheet(
    trace: &RunTrace,
    images: &BTreeMap<String, Image>,
    f: &Finding,
) -> Result<Option<Vec<u8>>, crate::Error> {
    let (Some(key), Some(k)) = (f.key.as_deref(), f.k) else {
        return Ok(None);
    };
    let tl = Timeline::new(trace);
    let Some(at) = tl.at(k) else { return Ok(None) };
    let scale = if trace.scenario.scale > 0.0 {
        trace.scenario.scale as f32
    } else {
        1.0
    };
    let lo = (at as i64 - SPAN).max(0) as usize;
    let hi = ((at as i64 + SPAN) as usize).min(tl.ks.len() - 1);
    let Some(surface) = tl
        .row(at, key)
        .or_else(|| (lo..=hi).find_map(|i| tl.row(i, key)))
        .map(|r| r.surface)
    else {
        return Ok(None);
    };
    // The segment's start: the box the node is held to.
    let start = trace
        .segments
        .iter()
        .find(|s| f.segment.as_deref() == Some(s.name.as_str()) && s.s.is_some_and(|s| s <= k))
        .and_then(|s| tl.at(s.s?))
        .and_then(|i| tl.row(i, key))
        .map(|r| r.visual);
    let mut bounds: Option<Rect> = start;
    for i in lo..=hi {
        if let Some(r) = tl.row(i, key) {
            bounds = Some(union(bounds, r.visual));
            bounds = Some(union(bounds, r.laid));
        }
    }
    let Some(b) = bounds else { return Ok(None) };
    let Some(size) = tl.size(at, surface) else {
        return Ok(None);
    };
    let (sw, sh) = (size[0] * scale, size[1] * scale);
    let mut crop = Crop {
        x: (b[0] * scale - PAD).max(0.0),
        y: (b[1] * scale - PAD).max(0.0),
        w: 0.0,
        h: 0.0,
    };
    crop.w = ((b[0] + b[2]) * scale + PAD).min(sw) - crop.x;
    crop.h = ((b[1] + b[3]) * scale + PAD).min(sh) - crop.y;
    if crop.w > MAX_CROP {
        let cx = (b[0] + b[2] / 2.0) * scale;
        crop.x = (cx - MAX_CROP / 2.0).clamp(0.0, (sw - MAX_CROP).max(0.0));
        crop.w = MAX_CROP.min(sw);
    }
    crop.w = crop.w.max(1.0).floor();
    crop.h = crop.h.max(1.0).floor();
    let pw = crop.w as u32 * ZOOM;
    let ph = crop.h as u32 * ZOOM;
    let n = (hi - lo + 1) as u32;
    let head = format!(
        "{} {} K{}{}",
        f.check,
        trace.nodes.get(key).map_or(key, |m| m.label.as_str()),
        k,
        f.axis.as_ref().map_or(String::new(), |a| format!(" {a}")),
    );
    let head2 = match (f.got, f.bound) {
        (Some(g), Some(b)) => format!("GOT {g:.2} NOT IN [{:.2}, {:.2}]", b[0], b[1]),
        _ => String::new(),
    };
    let tw = (n * (pw + GAP) + GAP).max(font::width(&head) + 2 * GAP);
    let th = 2 * (STRIP + 2) + STRIP + ph + 2 * GAP;
    let mut c = Canvas::new(tw, th, BG);
    c.text(i64::from(GAP), 3, &head, TEXT);
    c.text(i64::from(GAP), i64::from(STRIP + 3), &head2, TEXT);
    let mut decoded: HashMap<&str, (u32, u32, Vec<u8>)> = HashMap::new();
    let top = 2 * (STRIP + 2);
    for (j, i) in (lo..=hi).enumerate() {
        let x0 = i64::from(GAP + j as u32 * (pw + GAP));
        let y0 = i64::from(top + STRIP);
        let tick = tl.ks[i];
        let label = if i == at {
            format!("K{tick} <")
        } else {
            format!("K{tick}")
        };
        c.text(
            x0,
            i64::from(top + 2),
            &label,
            if i == at { RED } else { TEXT },
        );
        c.fill(x0, y0, pw, ph, PANEL);
        // The image this surface shows at frame i: its last painted one.
        let name = (0..=i)
            .rev()
            .find_map(|x| tl.frames[x].png.get(&surface))
            .map(String::as_str);
        let img = match name.and_then(|n| images.get(n).map(|im| (n, im))) {
            Some((n, im)) => {
                if !decoded.contains_key(n) {
                    decoded.insert(n, png::decode(&im.png)?);
                }
                decoded.get(n)
            }
            None => None,
        };
        let map = |lx: f32, ly: f32| -> (i64, i64) {
            (
                x0 + ((lx * scale - crop.x) * ZOOM as f32).round() as i64,
                y0 + ((ly * scale - crop.y) * ZOOM as f32).round() as i64,
            )
        };
        match img {
            Some((iw, ih, rgba)) => {
                for py in 0..ph {
                    for px in 0..pw {
                        let sx = crop.x as u32 + px / ZOOM;
                        let sy = crop.y as u32 + py / ZOOM;
                        if sx >= *iw || sy >= *ih {
                            continue;
                        }
                        let at = ((sy * iw + sx) * 4) as usize;
                        let a = u32::from(rgba[at + 3]);
                        let mix = |s: u8, d: u8| {
                            ((u32::from(s) * a + u32::from(d) * (255 - a)) / 255) as u8
                        };
                        let col = [
                            mix(rgba[at], PANEL[0]),
                            mix(rgba[at + 1], PANEL[1]),
                            mix(rgba[at + 2], PANEL[2]),
                            255,
                        ];
                        c.set(x0 + i64::from(px), y0 + i64::from(py), col);
                    }
                }
            }
            None => {
                for r in tl.screens[i].values().filter(|r| r.surface == surface) {
                    let (a, b2) = map(r.visual[0], r.visual[1]);
                    let (e, d) = map(r.visual[0] + r.visual[2], r.visual[1] + r.visual[3]);
                    c.outline(a, b2, e - 1, d - 1, WIRE, 0);
                }
            }
        }
        if let Some(s) = start {
            let (a, b2) = map(s[0], s[1]);
            let (e, d) = map(s[0] + s[2], s[1] + s[3]);
            c.outline(a, b2, e - 1, d - 1, YELLOW, 3);
        }
        if let Some(r) = tl.row(i, key) {
            let (a, b2) = map(r.laid[0], r.laid[1]);
            let (e, d) = map(r.laid[0] + r.laid[2], r.laid[1] + r.laid[3]);
            c.outline(a, b2, e - 1, d - 1, CYAN, 1);
            let (a, b2) = map(r.visual[0], r.visual[1]);
            let (e, d) = map(r.visual[0] + r.visual[2], r.visual[1] + r.visual[3]);
            c.outline(a, b2, e - 1, d - 1, RED, 0);
        }
    }
    Ok(Some(png::encode(c.w, c.h, &c.px)?))
}
