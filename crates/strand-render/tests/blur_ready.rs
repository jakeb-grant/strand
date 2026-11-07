//! (blur-ready, decisions.md) The lock's opening, offline: a full-screen
//! `thumbnail screen` and over it a copy under `filter: blur(36),
//! brightness(0.6)` that fades in from its `enter { opacity: 0 }`, as the
//! owner's lock has on each monitor (2560 × 1440 logical at scale 1.5,
//! 3840 × 2160 pixels), fed one still shared by both.
//!
//! The copy is drawn at opacity 0 in the surface's first frame, so that
//! frame builds no group; the frame that first shows it builds its group
//! (and the raster pixmap the group holds) once, and later frames reuse
//! it. On a surface promoted after its first frame that is the first GPU
//! frame. These counts, and the pixels (a reference PNG at 960 × 540 and
//! hashes of the 4K frame and of what the first GPU frame uploads), are
//! the guard every change to the work in that frame keeps.
//!
//! `STRAND_BLUR_READY_REPORT=1` also prints what that frame costs, part
//! by part (run it with `--profile timing --nocapture`): the measurement
//! decisions.md blur-ready records. Regenerate the PNG with
//! `STRAND_BLESS=1`.

#![cfg(feature = "gpu")]

mod common;
mod gpu_host;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::*;
use strand_gpu::{Gpu, GpuReply, GpuRequest};
use strand_render::{FeedKind, Renderer, ThumbnailFrame};
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// Frame `k` of a 60 Hz clock from `T0`.
fn at(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * u64::from(k))
}

/// 64-bit FNV-1a: a hash of pixels that is the same on every run and
/// build (the pixels' guard at sizes too large for a reference PNG).
fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A screen's still, `w × h` premultiplied BGRA as captures come: a
/// gradient across and down with specks of noise over it, and a few pixels whose
/// alpha is below a channel (a compositor can send them).
fn still(w: u32, h: u32) -> ThumbnailFrame {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let n = if seed >> 56 == 0 {
                (seed >> 40) as u8 / 4
            } else {
                0
            };
            let r = ((x * 255 / w.max(1)) as u8).saturating_add(n);
            let g = ((y * 255 / h.max(1)) as u8).saturating_add(n);
            let b = (((x + y) * 127 / (w + h).max(1)) as u8).saturating_add(n);
            let a = if seed & 0xffff == 0 { 64 } else { 255 };
            px.extend_from_slice(&[b, g, r, a]);
        }
    }
    ThumbnailFrame {
        width: w,
        height: h,
        pixels: px.into(),
    }
}

fn call(name: &str, args: Vec<PropValue>) -> PropValue {
    PropValue::Call {
        name: name.into(),
        args,
    }
}

/// The two thumbnails over a `logical` panel, the second under `filter`
/// fading in (`opacity: 1 ~ 420ms; enter { opacity: 0 }; exit { opacity:
/// 0 }`) when `fades`. Returns the renderer, configured at `scale`, and
/// the two nodes.
fn scene(
    logical: (f32, f32),
    scale: Scale,
    filter: Vec<PropValue>,
    fades: bool,
) -> (Renderer, Buffer, [NodeId; 2]) {
    // A lock as logic puts it up: there closed, then its content mounted
    // in the diff that opens it, so that content plays its `enter` on the
    // lock's first frame.
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Lock,
        None,
        vec![
            (Prop::Open, PropValue::Bool(false)),
            (
                Prop::TwoWay,
                PropValue::List(vec![PropValue::Keyword("open".into())]),
            ),
            (Prop::Bg, color("#11111b")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(std::mem::take(&mut b.diff)).is_empty());
    r.take_surface_changes();
    let thumb = |b: &mut Builder, extra: Vec<(Prop, PropValue)>| {
        let mut props = vec![
            (Prop::Source, text("screen")),
            (Prop::Fit, PropValue::Keyword("cover".into())),
            (Prop::Width, num(logical.0)),
            (Prop::Height, num(logical.1)),
        ];
        props.extend(extra);
        b.node(NodeKind::Thumbnail, Some(root), props)
    };
    let sharp = thumb(&mut b, vec![]);
    let mut extra = vec![(Prop::Filter, PropValue::List(filter))];
    if fades {
        extra.push((
            Prop::Enter,
            PropValue::Pose(vec![(Prop::Opacity, num(0.0))]),
        ));
        extra.push((Prop::Exit, PropValue::Pose(vec![(Prop::Opacity, num(0.0))])));
    }
    let copy = thumb(&mut b, extra);
    if fades {
        b.diff.push(SceneOp::SetProp {
            id: copy,
            prop: Prop::Opacity,
            value: num(1.0),
            transition: Transition::Duration {
                duration: Duration::from_millis(420),
                easing: Easing::STANDARD,
            },
        });
    }
    b.diff.set(root, Prop::Open, PropValue::Bool(true));
    assert!(r.apply(b.diff).is_empty());
    r.take_surface_changes();
    r.set_surface_bounds(S, Some(LogicalSize::new(logical.0, logical.1)));
    r.attach_surface(S, root);
    let size = scale.physical_size(LogicalSize::new(logical.0, logical.1));
    r.configure_surface(S, size, scale);
    (r, Buffer::new(size.w, size.h, scale), [sharp, copy])
}

fn lock_filter() -> Vec<PropValue> {
    vec![
        call("blur", vec![num(36.0)]),
        call("brightness", vec![num(0.6)]),
    ]
}

/// The lock's scene at `logical` and scale 1.5, laid out and fed its
/// still before its first frame as `strand run` feeds a lock
/// (`first_feed_demand`): returns it before that frame.
fn lock_scene(logical: (f32, f32)) -> (Renderer, Buffer, [NodeId; 2]) {
    let scale = Scale::new(180).unwrap();
    let (mut r, buf, ids) = scene(logical, scale, lock_filter(), true);
    r.update();
    let frame = still(buf.size.w, buf.size.h);
    let demand = r.first_feed_demand(S);
    assert_eq!(demand.len(), 2, "both thumbnails ask: {demand:?}");
    for d in demand {
        assert!(matches!(d.kind, FeedKind::Thumbnail { .. }));
        r.feed_frame(d.node, Some(frame.clone()));
    }
    (r, buf, ids)
}

/// Raster pixmaps and groups built so far.
fn builds(r: &Renderer) -> (u64, u64) {
    (r.raster_nodes().0, r.offscreen_cache().1)
}

/// What a lowered frame uploads, hashed in a fixed order: the pixels the
/// GPU is handed, the same on every device.
fn uploads_hash(f: &strand_gpu::Frame) -> (usize, u64) {
    let mut ups: Vec<(u32, u32, u64)> = f
        .uploads
        .iter()
        .map(|u| {
            (
                u.pixmap.width() as u32,
                u.pixmap.height() as u32,
                fnv(u.pixmap.data_as_u8_slice()),
            )
        })
        .collect();
    ups.sort_unstable();
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for (w, hh, p) in &ups {
        for v in [u64::from(*w), u64::from(*hh), *p] {
            h ^= v;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    (ups.len(), h)
}

/// The surface promoted after its first frame, as a lock's is.
fn promote(r: &mut Renderer) {
    let _ = r.take_gpu_requests();
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
}

const LOCK_4K: (f32, f32) = (2560.0, 1440.0);
const LOCK_SMALL: (f32, f32) = (640.0, 360.0);

/// The CPU frame 2 of the 4K lock scene, hashed (blur-ready's M0, at
/// 3187e4d3): no change to the work in the frame that first shows the
/// blur may move a byte of it.
const CPU_4K_FRAME_2: u64 = 0x015f_5831_db3a_ff2e;
/// What the 4K lock scene's first GPU frame uploads, hashed (M0).
const GPU_4K_UPLOADS: (usize, u64) = (2, 0x02ec_c37b_68ea_ee6a);
/// The same at 960 × 540 (M0).
const GPU_SMALL_UPLOADS: (usize, u64) = (2, 0x1cd7_d8c5_d0e5_04be);

/// The copy is at opacity 0 in the first frame, which builds no group;
/// the frame that first shows it builds its group once, on the CPU and
/// on a surface promoted after its first frame (the first GPU frame),
/// and the frames after reuse it.
#[test]
fn the_lock_like_scene_builds_its_group_once_in_the_frame_that_shows_it() {
    // On the CPU.
    let (mut r, mut buf, _) = lock_scene(LOCK_4K);
    let before = builds(&r);
    buf.paint_at(&mut r, S, 0, at(0));
    let first = builds(&r);
    buf.paint_at(&mut r, S, 1, at(1));
    let second = builds(&r);
    buf.paint_at(&mut r, S, 1, at(2));
    let third = builds(&r);
    eprintln!("cpu: before {before:?} first {first:?} second {second:?} third {third:?}");
    let step = |a: (u64, u64), b: (u64, u64)| (b.0 - a.0, b.1 - a.1);
    // (raster pixmaps, groups) each frame built.
    assert_eq!(before, LAID_OUT, "laid out before the first frame");
    assert_eq!(step(before, first), FIRST, "the first frame: the copy at 0");
    assert_eq!(step(first, second), SHOWN, "the frame that first shows it");
    assert_eq!(step(second, third), (0, 0), "later frames build nothing");

    // Promoted after the first frame.
    let (mut r, mut buf, _) = lock_scene(LOCK_4K);
    buf.paint_at(&mut r, S, 0, at(0));
    let first = builds(&r);
    promote(&mut r);
    let f = r.paint_gpu(S, at(1)).expect("the first GPU frame");
    let second = builds(&r);
    let later = r.paint_gpu(S, at(2)).expect("the second GPU frame");
    let third = builds(&r);
    eprintln!(
        "gpu: first {first:?} second {second:?} third {third:?}; uploads {} then {}",
        f.uploads.len(),
        later.uploads.len()
    );
    assert_eq!(step(first, second), SHOWN, "the first GPU frame");
    assert_eq!(f.uploads.len(), FIRST_GPU_UPLOADS, "what it uploads");
    assert_eq!(step(second, third), (0, 0), "the next builds nothing");
    assert_eq!(later.uploads.len(), 0, "and uploads nothing");
}

/// (raster pixmaps, groups) built by the layout before the first frame
/// (two blank pixmaps: no frame yet), by the first frame (the sharp
/// still's pixmap; the copy is at 0) and by the frame that first shows
/// the copy (its pixmap and its group).
const LAID_OUT: (u64, u64) = (2, 0);
const FIRST: (u64, u64) = (1, 0);
const SHOWN: (u64, u64) = (1, 1);
/// The first GPU frame uploads the sharp still's pixmap and the group.
const FIRST_GPU_UPLOADS: usize = 2;

/// The pixels, as they were before blur-ready: CPU frame 2 at 960 × 540
/// (a reference PNG, compared exactly) and at 3840 × 2160 (a hash), and
/// what the first GPU frame uploads at both sizes (hashes, which every
/// device draws from); with a device, that frame read back is the CPU's
/// within the GPU tolerance.
#[test]
fn the_lock_like_scene_draws_as_today() {
    let bless = std::env::var_os("STRAND_BLESS").is_some();
    let mut seen = Vec::new();
    for (logical, name) in [(LOCK_SMALL, "small"), (LOCK_4K, "4k")] {
        let (mut r, mut buf, _) = lock_scene(logical);
        buf.paint_at(&mut r, S, 0, at(0));
        buf.paint_at(&mut r, S, 1, at(1));
        let cpu = fnv(&buf.pixels);
        if name == "small" {
            assert_matches_ref("blur_ready_frame_2", &buf, 0);
        }
        let (mut g, mut first, _) = lock_scene(logical);
        first.paint_at(&mut g, S, 0, at(0));
        promote(&mut g);
        let f = g.paint_gpu(S, at(1)).expect("the first GPU frame");
        let ups = uploads_hash(&f);
        eprintln!(
            "{name}: cpu frame 2 {cpu:#018x}, gpu uploads {ups:?} ({:#018x})",
            ups.1
        );
        seen.push((name, cpu, ups));
        if name == "small"
            && let Some(opts) = gpu_host::device()
        {
            let (ping, pings) = mpsc::channel();
            let mut gpu = Gpu::spawn(
                Box::new(move || {
                    let _ = ping.send(());
                }),
                opts,
            );
            gpu.send(GpuRequest::Attach {
                surface: S,
                handles: None,
                size: buf.size,
                scale: buf.scale,
                opaque: false,
            });
            let px = readback(&mut gpu, &pings, f);
            let bad = px
                .chunks_exact(4)
                .zip(buf.pixels.chunks_exact(4))
                .filter(|(a, b)| a.iter().zip(*b).any(|(p, q)| p.abs_diff(*q) > 6))
                .count();
            let share = bad as f64 / (px.len() / 4) as f64;
            assert!(
                !pixel_gates_held() || share <= 0.02,
                "the first GPU frame: {bad} pixels past 6 of the CPU's"
            );
        }
    }
    if bless || !pixel_gates_held() {
        return;
    }
    let (_, cpu4k, gpu4k) = seen[1];
    let (_, _, gpu_small) = seen[0];
    assert_eq!(cpu4k, CPU_4K_FRAME_2, "the 4K CPU frame 2 moved");
    assert_eq!(
        gpu4k, GPU_4K_UPLOADS,
        "the 4K first GPU frame's uploads moved"
    );
    assert_eq!(
        gpu_small, GPU_SMALL_UPLOADS,
        "the small first GPU frame's uploads moved"
    );
}

/// A presented surface's frame drawn on the GPU (attached for readback),
/// as BGRA bytes like a CPU frame.
fn readback(gpu: &mut Gpu, pings: &mpsc::Receiver<()>, frame: strand_gpu::Frame) -> Vec<u8> {
    let id = frame.id;
    gpu.send(GpuRequest::Frame(frame));
    let deadline = Instant::now() + gpu_host::WAIT;
    loop {
        while let Some(reply) = gpu.try_recv() {
            match reply {
                GpuReply::Pixels { frame, pixels, .. } if frame == id => {
                    let mut out = Vec::new();
                    for y in 0..pixels.height {
                        out.extend_from_slice(pixels.row(y));
                    }
                    return out;
                }
                GpuReply::Failed { error, .. } => panic!("frame {id} failed: {error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "no pixels for frame {id}");
        let _ = pings.recv_timeout(Duration::from_millis(50));
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// `STRAND_BLUR_READY_REPORT=1`: what the frame that first shows the
/// blur costs at 4K, part by part, the median of 5 (decisions.md
/// blur-ready). Nothing is asserted on time.
#[test]
fn blur_ready_report() {
    if std::env::var("STRAND_BLUR_READY_REPORT").as_deref() != Ok("1") {
        return;
    }
    const RUNS: usize = 5;
    let mut rows: Vec<(&str, Vec<f64>)> = Vec::new();
    let mut add = |name: &'static str, v: f64| match rows.iter_mut().find(|r| r.0 == name) {
        Some(r) => r.1.push(v),
        None => rows.push((name, vec![v])),
    };
    let device = gpu_host::device();
    for _ in 0..RUNS {
        // The CPU path: frames 1, 2 (the copy first shown) and 3.
        let (mut r, mut buf, _) = lock_scene(LOCK_4K);
        let t = Instant::now();
        buf.paint_at(&mut r, S, 0, at(0));
        add("cpu frame 1", ms(t.elapsed()));
        let (g0, p0) = r.build_times();
        let t = Instant::now();
        buf.paint_at(&mut r, S, 1, at(1));
        add("cpu frame 2", ms(t.elapsed()));
        let (g1, p1) = r.build_times();
        add("cpu frame 2: group", ms(g1 - g0));
        add("cpu frame 2: raster pixmaps", ms(p1 - p0));
        let t = Instant::now();
        buf.paint_at(&mut r, S, 1, at(2));
        add("cpu frame 3", ms(t.elapsed()));
        drop(r);

        // Promoted after frame 1: the first GPU frame.
        let (mut r, mut buf, _) = lock_scene(LOCK_4K);
        buf.paint_at(&mut r, S, 0, at(0));
        promote(&mut r);
        let (g0, p0) = r.build_times();
        let t = Instant::now();
        let f = r.paint_gpu(S, at(1)).expect("the first GPU frame");
        add("gpu frame 1: main thread", ms(t.elapsed()));
        let (g1, p1) = r.build_times();
        add("gpu frame 1: group", ms(g1 - g0));
        add("gpu frame 1: raster pixmaps", ms(p1 - p0));
        add("gpu frame 1: lower", ms(r.last_paint_times().lower));
        let bytes: usize = f
            .uploads
            .iter()
            .map(|u| u.pixmap.data_as_u8_slice().len())
            .sum();
        add("gpu frame 1: upload MB", bytes as f64 / 1e6);
        if let Some(opts) = device {
            strand_gpu::record_frame_phases(true);
            let (ping, pings) = mpsc::channel();
            let mut gpu = Gpu::spawn(
                Box::new(move || {
                    let _ = ping.send(());
                }),
                opts,
            );
            gpu.send(GpuRequest::Attach {
                surface: S,
                handles: None,
                size: buf.size,
                scale: buf.scale,
                opaque: false,
            });
            let _ = readback(&mut gpu, &pings, f);
            let later = r.paint_gpu(S, at(2)).expect("the second GPU frame");
            let _ = readback(&mut gpu, &pings, later);
            drop(gpu);
            let phases = strand_gpu::take_frame_phases();
            strand_gpu::record_frame_phases(false);
            for (i, p) in phases.iter().filter(|p| p.surface == Some(S)).enumerate() {
                let tag = if i == 0 { "first" } else { "second" };
                add(
                    if i == 0 {
                        "gpu thread: first frame total"
                    } else {
                        "gpu thread: second frame total"
                    },
                    ms(p.total),
                );
                for (name, d) in &p.phases {
                    let key: &'static str = Box::leak(format!("gpu thread: {tag} {name}").into());
                    add(key, ms(*d));
                }
            }
        }
        drop(r);

        // The filters alone at the group's size (960 × 540, σ 13.5).
        let mut px = vec![0u8; 960 * 540 * 4];
        for (i, p) in px.chunks_exact_mut(4).enumerate() {
            let v = (i % 251) as u8;
            p.copy_from_slice(&[v / 2, v / 3, v, 255]);
        }
        let t = Instant::now();
        strand_render::blur(&mut px, 960, 540, 13.5);
        add("blur 960x540 σ13.5", ms(t.elapsed()));
        let dim = brightness(0.6);
        let t = Instant::now();
        strand_render::color_matrix(&mut px, &dim);
        add("colour matrix 960x540", ms(t.elapsed()));
        let mut big = vec![0u8; 3840 * 2160 * 4];
        for (i, p) in big.chunks_exact_mut(4).enumerate() {
            let v = (i % 251) as u8;
            p.copy_from_slice(&[v / 2, v / 3, v, 255]);
        }
        let t = Instant::now();
        strand_render::color_matrix(&mut big, &dim);
        add("colour matrix 3840x2160", ms(t.elapsed()));

        // A full-screen colour matrix alone (no blur: full size).
        let (mut r, mut buf, _) = scene(
            LOCK_4K,
            Scale::new(180).unwrap(),
            vec![call("brightness", vec![num(0.6)])],
            false,
        );
        r.update();
        let frame = still(buf.size.w, buf.size.h);
        for d in r.first_feed_demand(S) {
            r.feed_frame(d.node, Some(frame.clone()));
        }
        let t = Instant::now();
        buf.paint_at(&mut r, S, 0, at(0));
        add("matrix-only 4K: frame 1", ms(t.elapsed()));
        add("matrix-only 4K: group", ms(r.build_times().0));
    }
    eprintln!("blur-ready report (median of {RUNS}, ms; upload in MB):");
    for (name, v) in rows {
        let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = v.iter().copied().fold(0.0, f64::max);
        eprintln!("  {name:<44} {:>9.2}  ({lo:.2} .. {hi:.2})", median(v));
    }
}

/// `brightness(k)` as the filter compiles it (straight colour scaled).
fn brightness(k: f32) -> [f32; 20] {
    let mut m = effect::IDENTITY_MATRIX;
    m[0] = k;
    m[6] = k;
    m[12] = k;
    m
}
