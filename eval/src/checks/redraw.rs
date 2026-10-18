//! `redraw`: a partial redraw leaves its buffer as a full repaint of the
//! same frame draws it (docs/eval.md, "Checks"; docs/decisions.md
//! eval-refs, the `redraw` check).
//!
//! The renderer repaints only a frame's damage, widened by the buffer's
//! age, over what the buffer kept from the frames before. Damage that
//! misses something that changed leaves stale pixels no geometry check
//! sees: the nodes are where they belong, the buffer is not. So after
//! every paint the harness draws the frame again in full into a fresh
//! buffer (`Renderer::repaint_fresh`: the display list that frame drew,
//! at its time, by a raster with nothing cached; on the GPU leg
//! `Renderer::fresh_gpu_frame`, drawn on a device of its own that holds
//! nothing from earlier frames) and compares the two pixel by pixel
//! ([`compare`]). The repaint reads the renderer and changes nothing a
//! later frame reads, so the run is the same with the check on or off
//! (eval/tests/redraw.rs holds its digests and images equal).
//!
//! Both sides are drawn by the same code on the same machine from the
//! same display list. On the CPU they agree exactly ([`CPU_TOLERANCE`]
//! is 0); on the GPU a glyph or image drawn under a transform may round
//! one step apart ([`GPU_TOLERANCE`]). A difference past the leg's
//! tolerance is a finding: the surface, the tick, how many pixels, how
//! many of them lie outside what the frame redrew (stale) and the
//! bounding box of the region.

use super::Finding;
use crate::scenario::CheckId;
use crate::trace::{RedrawRec, RunTrace};

/// The channel difference (premultiplied, 0 to 255) a CPU pixel may
/// show before it counts. The presented buffer and the repaint are drawn
/// by the same raster code from the same display list on the same
/// machine, and the raster draws every pixel in the same fixed cell
/// whatever the damage (`Raster::paint_window`: a window is
/// bit-identical to that part of a full paint). Nothing rounds
/// differently between the two, so none is allowed.
pub const CPU_TOLERANCE: u8 = 0;

/// The same on the GPU leg: one step. The fresh device packs each
/// uploaded pixmap (a glyph atlas page, an image) into its image atlas
/// at another place than the presented device, which holds what earlier
/// frames uploaded, and a pixmap drawn under a transform (a rotated,
/// scaled or sheared glyph: `smooth`) is sampled bilinearly at
/// coordinates offset by that place, whose float rounding can move a
/// channel by one. A stale or missing upload differs by far more.
pub const GPU_TOLERANCE: u8 = 1;

/// The tolerance of a leg.
pub fn tolerance(gpu: bool) -> u8 {
    if gpu { GPU_TOLERANCE } else { CPU_TOLERANCE }
}

/// Compares `presented`, the buffer a frame left (premultiplied BGRA,
/// `width × height`, rows `width * 4` apart), with `fresh`, the same
/// frame repainted in full; `redrawn` is what the frame redrew (buffer
/// pixels, `[x, y, w, h]`); a channel difference up to `tolerance`
/// does not count. `None` when they are equal; the record otherwise
/// (`gpu` and `png` left for the caller).
pub fn compare(
    presented: &[u8],
    fresh: &[u8],
    width: u32,
    height: u32,
    redrawn: &[[u32; 4]],
    tolerance: u8,
) -> Option<RedrawRec> {
    let w = width as usize;
    let inside = |x: u32, y: u32| {
        redrawn
            .iter()
            .any(|r| x >= r[0] && y >= r[1] && x - r[0] < r[2] && y - r[1] < r[3])
    };
    let mut rec = RedrawRec::default();
    let (mut any_box, mut px_box) = (Bounds::default(), Bounds::default());
    for (i, (p, q)) in presented
        .chunks_exact(4)
        .zip(fresh.chunks_exact(4))
        .enumerate()
        .take(w * height as usize)
    {
        let d = p
            .iter()
            .zip(q)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        if d == 0 {
            continue;
        }
        let (x, y) = ((i % w) as u32, (i / w) as u32);
        rec.any += 1;
        rec.worst = rec.worst.max(d);
        any_box.add(x, y);
        if d > tolerance {
            rec.px += 1;
            px_box.add(x, y);
            if !inside(x, y) {
                rec.stale += 1;
            }
        }
    }
    if rec.any == 0 {
        return None;
    }
    rec.at = if rec.px > 0 { px_box } else { any_box }.rect();
    rec.redrawn = redrawn
        .iter()
        .filter(|r| r[2] > 0 && r[3] > 0)
        .fold(None, |b: Option<Bounds>, r| {
            let mut b = b.unwrap_or_default();
            b.add(r[0], r[1]);
            b.add(r[0] + r[2] - 1, r[1] + r[3] - 1);
            Some(b)
        })
        .map(|b| b.rect());
    Some(rec)
}

/// A bounding box grown pixel by pixel.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds {
            x0: u32::MAX,
            y0: u32::MAX,
            x1: 0,
            y1: 0,
        }
    }
}

impl Bounds {
    fn add(&mut self, x: u32, y: u32) {
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
    }

    fn rect(self) -> [u32; 4] {
        if self.x0 > self.x1 {
            return [0; 4];
        }
        [
            self.x0,
            self.y0,
            self.x1 - self.x0 + 1,
            self.y1 - self.y0 + 1,
        ]
    }
}

/// The findings of `trace`'s comparisons: a failure per surface and
/// leg past its tolerance (at the first tick, with the count of ticks
/// after), an info line per difference within it.
pub fn findings(trace: &RunTrace) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    // Every painted frame is compared: one that is not is a frame the
    // check did not see.
    let painted: usize = trace.frames.iter().map(|f| f.painted.len()).sum();
    if (trace.redraw as usize) < painted {
        out.push(Finding::fail(
            CheckId::Redraw,
            format!(
                "only {} of {painted} painted frames were compared with a full repaint",
                trace.redraw
            ),
        ));
    }
    let mut first: std::collections::BTreeMap<(u32, bool), usize> = Default::default();
    for f in &trace.frames {
        for (s, r) in &f.redraw {
            let leg = if r.gpu { "GPU" } else { "CPU" };
            let label = trace.surfaces.iter().find(|x| x.id == *s).map_or_else(
                || format!("surface {s}"),
                |x| format!("surface {s} ({})", x.label),
            );
            if r.px == 0 {
                out.push(
                    Finding::fail(
                        CheckId::Redraw,
                        format!(
                            "{label} on the {leg}: {} pixels differ from a full repaint by at \
                             most {} (within the tolerance, {})",
                            r.any,
                            r.worst,
                            tolerance(r.gpu)
                        ),
                    )
                    .sev(super::Sev::Info)
                    .at(f.k),
                );
                continue;
            }
            if let Some(i) = first.get(&(*s, r.gpu)) {
                out[*i].more += 1;
                continue;
            }
            first.insert((*s, r.gpu), out.len());
            out.push(
                Finding::fail(CheckId::Redraw, describe(&label, leg, r))
                    .at(f.k)
                    .axis("pixels")
                    .got(r.px as f64, [0.0, 0.0]),
            );
        }
    }
    out
}

/// A failing comparison as a line.
fn describe(label: &str, leg: &str, r: &RedrawRec) -> String {
    let [x, y, w, h] = r.at;
    let mut s = format!(
        "{label} on the {leg}: {} pixels differ from a full repaint of the same frame by more \
         than {} (worst {}), in [{x}, {y}, {w}, {h}] (buffer px)",
        r.px,
        tolerance(r.gpu),
        r.worst
    );
    match r.redrawn {
        Some([rx, ry, rw, rh]) => {
            s += &format!(
                "; {} of them outside what the frame redrew ([{rx}, {ry}, {rw}, {rh}])",
                r.stale
            );
        }
        None => s += "; the frame redrew nothing",
    }
    if r.stale > 0 {
        s += ": stale pixels a partial redraw left";
    }
    if let Some(png) = &r.png {
        s += &format!("; the full repaint is {png}");
    }
    s
}
