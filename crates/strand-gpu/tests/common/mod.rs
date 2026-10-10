//! Shared by the GPU tests: start the GPU or skip, wait for replies.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use strand_gpu::{Frame, Gpu, GpuMode, GpuOptions, GpuReply, GpuRequest, Readback};
use strand_scene::{Scale, ShaderCode, Size, SurfaceId, UniformType};

pub const WAIT: Duration = Duration::from_secs(60);

pub struct Harness {
    pub gpu: Gpu,
    pub pings: mpsc::Receiver<()>,
}

impl Harness {
    pub fn next(&mut self) -> GpuReply {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(r) = self.gpu.try_recv() {
                return r;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no GPU reply within {WAIT:?}");
            let _ = self
                .pings
                .recv_timeout(left.min(Duration::from_millis(100)));
        }
    }
}

/// Starts the GPU, or skips (None) when there is no device and the
/// environment does not require one.
pub fn start() -> Option<Harness> {
    let (tx, pings) = mpsc::channel();
    let waker = Box::new(move || {
        let _ = tx.send(());
    });
    let mut h = Harness {
        gpu: Gpu::spawn(waker, GpuOptions::from_env()),
        pings,
    };
    match h.next() {
        GpuReply::Ready(info) => {
            eprintln!(
                "GPU: {} ({}), software {}",
                info.name, info.driver, info.software
            );
            Some(h)
        }
        GpuReply::Unavailable(e) => {
            let required = std::env::var("STRAND_REQUIRE_GPU").is_ok_and(|v| v == "1");
            assert!(!required, "STRAND_REQUIRE_GPU=1 but no GPU: {e}");
            eprintln!("skipped: no GPU ({e})");
            None
        }
        other => panic!("unexpected first reply {other:?}"),
    }
}

pub fn gpu_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|t| {
                    std::fs::read_to_string(t.path().join("comm"))
                        .is_ok_and(|c| c.trim() == strand_gpu::THREAD_NAME)
                })
                .count()
        })
        .unwrap_or(0)
}

pub fn pixel(rb: &Readback, x: u32, y: u32) -> [u8; 4] {
    let row = rb.row(y);
    let i = x as usize * 4;
    [row[i], row[i + 1], row[i + 2], row[i + 3]]
}

pub fn close(a: [u8; 4], b: [u8; 4], tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

pub const S: SurfaceId = SurfaceId(7);

pub fn attach(h: &mut Harness, size: Size) {
    h.gpu.send(GpuRequest::Attach {
        surface: S,
        handles: None,
        size,
        scale: Scale::ONE,
        opaque: false,
    });
    match h.next() {
        GpuReply::Attached { surface, mode } => {
            assert_eq!(surface, S);
            assert_eq!(mode, GpuMode::Readback, "no handles: readback");
        }
        other => panic!("expected Attached, got {other:?}"),
    }
}

pub fn frame(h: &mut Harness, f: Frame) -> Readback {
    let id = f.id;
    h.gpu.send(GpuRequest::Frame(f));
    match h.next() {
        GpuReply::Pixels {
            surface,
            frame,
            pixels,
        } => {
            assert_eq!((surface, frame), (S, id));
            pixels
        }
        other => panic!("expected Pixels, got {other:?}"),
    }
}

pub fn code(wgsl: &str, uniforms: Vec<(&str, UniformType, u32)>) -> Arc<ShaderCode> {
    Arc::new(ShaderCode {
        path: "test.wgsl".into(),
        wgsl: wgsl.into(),
        uniforms: ShaderCode::packed(
            uniforms
                .into_iter()
                .map(|(n, t, b)| (n.to_string(), t, b))
                .collect(),
        ),
    })
}
