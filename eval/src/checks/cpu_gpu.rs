//! `cpu-gpu`: a `backend = "both"` scenario runs twice, once drawn on the
//! CPU and once on the GPU leg (crate::gpu: every surface `GpuPresent`,
//! its frames drawn by strand-gpu and read back), on the same virtual
//! clock, so their ticks are the same ticks. At every tick both painted,
//! the two must agree: the same surfaces painted, the same nodes (by
//! key) where the same layout and motion put them (boxes within
//! [`RECT_GIVE`], opacity within [`OPACITY_GIVE`], the same motions,
//! hidden reasons and text states), the same `Painter` regions, and
//! pixels within the GPU tests' tolerance (crates/strand-render/tests/
//! gpu.rs: a channel may differ by [`GPU_TOLERANCE`], at most
//! [`EDGE_SHARE`] of the pixels by more). An `[allow] cpu-gpu` entry
//! leaves a node out of the node comparison and masks its box out of
//! the pixel one (a `shader`, a dithered gradient).
//!
//! Both legs flatten the same scene in the same renderer code, so the
//! regions compared here cannot differ by construction: this is no
//! detector for bug 23 (a GPU-presented surface's regions not reaching
//! the compositor), which lives in the surface manager, out of the
//! sandbox's reach (docs/decisions.md eval-sandbox).

use std::collections::{BTreeMap, BTreeSet};

use strand_render::probe::Axes;

use super::{Cx, Finding, first_per_node};
use crate::harness::Image;
use crate::scenario::CheckId;
use crate::trace::{Frame, Row, RunTrace};

/// Per-channel difference allowed between the GPU's pixel and the CPU's
/// (gpu.rs `GPU_TOLERANCE`).
pub const GPU_TOLERANCE: u8 = 6;
/// Share of a frame's pixels allowed past it (gpu.rs `EDGE_SHARE`).
pub const EDGE_SHARE: f64 = 0.02;
/// How far apart the two legs' boxes may be, in logical pixels.
pub const RECT_GIVE: f64 = 0.01;
/// How far apart their opacities may be.
pub const OPACITY_GIVE: f64 = 1e-4;

/// The two legs' runs: the CPU's is `cx`'s, the GPU's `gpu`, with the
/// images each painted surface showed.
pub fn run(
    cx: &Cx,
    gpu: &RunTrace,
    cpu_images: &BTreeMap<String, Image>,
    gpu_images: &BTreeMap<String, Image>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let fail = |msg: String| Finding::fail(CheckId::CpuGpu, msg);
    let (a, b) = (by_k(cx.trace), by_k(gpu));
    for k in a.keys().filter(|k| !b.contains_key(k)) {
        out.push(fail("painted on the CPU, not on the GPU".into()).at(*k));
    }
    for k in b.keys().filter(|k| !a.contains_key(k)) {
        out.push(fail("painted on the GPU, not on the CPU".into()).at(*k));
    }
    let exempt: BTreeSet<&str> = cx
        .trace
        .nodes
        .keys()
        .map(String::as_str)
        .filter(|key| cx.exempt(CheckId::CpuGpu, usize::MAX, key, Axes(u8::MAX)))
        .collect();
    let allowed = |key: &str| exempt.contains(key);
    for (k, fa) in &a {
        let Some(fb) = b.get(k) else { continue };
        if fa.painted != fb.painted {
            out.push(
                fail(format!(
                    "surfaces painted: {:?} on the CPU, {:?} on the GPU",
                    fa.painted, fb.painted
                ))
                .at(*k),
            );
            continue;
        }
        if fa.wants != fb.wants {
            out.push(
                fail(format!(
                    "surfaces still wanting a frame: {:?} on the CPU, {:?} on the GPU",
                    fa.wants, fb.wants
                ))
                .at(*k),
            );
        }
        nodes(fa, fb, &allowed, &mut out);
        for s in &fa.painted {
            if fa.regions.get(s) != fb.regions.get(s) {
                out.push(
                    fail(format!(
                        "surface {s}'s regions: {:?} on the CPU, {:?} on the GPU",
                        fa.regions.get(s),
                        fb.regions.get(s)
                    ))
                    .at(*k),
                );
            }
            let (Some(na), Some(nb)) = (fa.png.get(s), fb.png.get(s)) else {
                continue;
            };
            let (Some(ia), Some(ib)) = (cpu_images.get(na), gpu_images.get(nb)) else {
                continue;
            };
            let scale = cx.trace.scale_at(fa).max(0.25);
            let masks: Vec<[f32; 4]> =
                fa.n.iter()
                    .filter(|r| r.surface == *s && allowed(&r.key))
                    .map(|r| r.visual.map(|v| v * scale as f32))
                    .collect();
            match pixels(ia, ib, &masks) {
                Err(e) => out.push(fail(format!("surface {s}: {e}")).at(*k)),
                Ok((bad, total, worst)) => {
                    let share = bad as f64 / total.max(1) as f64;
                    if share > EDGE_SHARE {
                        out.push(
                            fail(format!(
                                "surface {s}: {bad} of {total} pixels differ by more than \
                                 {GPU_TOLERANCE} (worst {worst})"
                            ))
                            .at(*k)
                            .axis("pixels")
                            .got(share, [0.0, EDGE_SHARE]),
                        );
                    }
                }
            }
        }
    }
    first_per_node(out)
}

fn by_k(t: &RunTrace) -> BTreeMap<u32, &Frame> {
    t.frames.iter().map(|f| (f.k, f)).collect()
}

/// The nodes of two frames of one tick, compared.
fn nodes(fa: &Frame, fb: &Frame, allowed: &dyn Fn(&str) -> bool, out: &mut Vec<Finding>) {
    let k = fa.k;
    let rows = |f: &Frame| -> BTreeMap<(u32, String), Row> {
        f.n.iter()
            .filter(|r| !allowed(&r.key))
            .map(|r| ((r.surface, r.key.clone()), r.clone()))
            .collect()
    };
    let (ra, rb) = (rows(fa), rows(fb));
    let ka: BTreeSet<_> = ra.keys().collect();
    let kb: BTreeSet<_> = rb.keys().collect();
    for (_, key) in ka.difference(&kb) {
        out.push(
            Finding::fail(CheckId::CpuGpu, "drawn on the CPU, not on the GPU")
                .at(k)
                .node(key),
        );
    }
    for (_, key) in kb.difference(&ka) {
        out.push(
            Finding::fail(CheckId::CpuGpu, "drawn on the GPU, not on the CPU")
                .at(k)
                .node(key),
        );
    }
    for id in ka.intersection(&kb) {
        let (Some(x), Some(y)) = (ra.get(*id), rb.get(*id)) else {
            continue;
        };
        let key = &id.1;
        for (what, p, q) in [
            ("drawn", x.drawn, y.drawn),
            ("visual", x.visual, y.visual),
            ("laid", x.laid, y.laid),
        ] {
            for (i, axis) in ["x", "y", "w", "h"].iter().enumerate() {
                let (u, v) = (f64::from(p[i]), f64::from(q[i]));
                if (u - v).abs() > RECT_GIVE {
                    out.push(
                        Finding::fail(
                            CheckId::CpuGpu,
                            format!("its {what} box differs between the legs"),
                        )
                        .at(k)
                        .node(key)
                        .axis(axis)
                        .got(v, [u - RECT_GIVE, u + RECT_GIVE]),
                    );
                }
            }
        }
        let (u, v) = (f64::from(x.op), f64::from(y.op));
        if (u - v).abs() > OPACITY_GIVE {
            out.push(
                Finding::fail(CheckId::CpuGpu, "its opacity differs between the legs")
                    .at(k)
                    .node(key)
                    .axis("opacity")
                    .got(v, [u - OPACITY_GIVE, u + OPACITY_GIVE]),
            );
        }
        if x.mot != y.mot || x.hidden != y.hidden || x.text != y.text {
            out.push(
                Finding::fail(
                    CheckId::CpuGpu,
                    format!(
                        "its motions, hidden reason or text differ between the legs: \
                         {:#x} {:?} {:?} on the CPU, {:#x} {:?} {:?} on the GPU",
                        x.mot, x.hidden, x.text, y.mot, y.hidden, y.text
                    ),
                )
                .at(k)
                .node(key),
            );
        }
    }
}

/// Two images compared in premultiplied alpha (the frames' own form; a
/// PNG keeps them straight), outside `masks` (buffer pixels): pixels past
/// [`GPU_TOLERANCE`], pixels compared and the worst channel difference.
pub fn pixels(a: &Image, b: &Image, masks: &[[f32; 4]]) -> Result<(usize, usize, u8), String> {
    if (a.width, a.height) != (b.width, b.height) {
        return Err(format!(
            "the CPU drew {}x{}, the GPU {}x{}",
            a.width, a.height, b.width, b.height
        ));
    }
    let da = crate::report::png::decode(&a.png)
        .map_err(|e| e.to_string())?
        .2;
    let db = crate::report::png::decode(&b.png)
        .map_err(|e| e.to_string())?
        .2;
    let masked = |x: u32, y: u32| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        masks
            .iter()
            .any(|m| fx >= m[0] && fy >= m[1] && fx < m[0] + m[2] && fy < m[1] + m[3])
    };
    let (mut bad, mut total, mut worst) = (0, 0, 0u8);
    for (i, (p, q)) in da.chunks_exact(4).zip(db.chunks_exact(4)).enumerate() {
        let (x, y) = (i as u32 % a.width, i as u32 / a.width);
        if masked(x, y) {
            continue;
        }
        total += 1;
        let d = premultiplied(p)
            .iter()
            .zip(premultiplied(q))
            .map(|(u, v)| u.abs_diff(v))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        if d > GPU_TOLERANCE {
            bad += 1;
        }
    }
    Ok((bad, total, worst))
}

/// A straight-alpha RGBA pixel premultiplied.
fn premultiplied(p: &[u8]) -> [u8; 4] {
    let a = u32::from(p[3]);
    let m = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
    [m(p[0]), m(p[1]), m(p[2]), p[3]]
}
