//! Not a gate: what a promotion after the device was dropped costs
//! (`promote::DROP_AFTER`, 30 s idle; decisions.md m5-speed-gpu-drop):
//! the GPU thread and device started, a surface attached and its first
//! frame drawn (vello_gpu's pipelines built on that first frame), against
//! the second frame on the same device. The CPU keeps drawing while this
//! runs on the GPU thread, so it delays the switch, not a frame. Run
//! with `cargo test --profile timing -p strand-gpu --test start_cost --
//! --ignored --nocapture` (lavapipe in the container; `gpu.sh` for the
//! laptop's GPU).

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_gpu::kurbo::{Affine, Rect, Shape};
use strand_gpu::{AlphaColor, Brush, Frame, GpuReply, GpuRequest, Op};
use strand_scene::{Scale, Size};

fn fills(id: u64, size: Size) -> Frame {
    let ops = (0..32)
        .map(|i| {
            let x = f64::from(i % 8) * 80.0;
            let y = f64::from(i / 8) * 80.0;
            Op::Fill {
                path: Rect::new(x, y, x + 60.0, y + 60.0)
                    .to_rounded_rect(12.0)
                    .to_path(0.1),
                brush: Brush::Solid(AlphaColor::new([0.2, 0.6, 0.9, 1.0])),
                brush_transform: Affine::IDENTITY,
                even_odd: false,
            }
        })
        .collect();
    Frame {
        surface: S,
        id,
        size,
        scale: Scale::ONE,
        ops,
        uploads: vec![],
        retire: Vec::new(),
        clear: AlphaColor::TRANSPARENT,
    }
}

#[test]
#[ignore = "measurement, not a gate"]
fn a_promotion_after_the_drop_costs() {
    let size = Size::new(640, 320);
    let rounds = 4;
    let (mut start_ms, mut first_ms, mut second_ms) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..rounds {
        let t0 = Instant::now();
        let Some(mut h) = start() else { return };
        attach(&mut h, size);
        let t1 = Instant::now();
        frame(&mut h, fills(1, size));
        let t2 = Instant::now();
        frame(&mut h, fills(2, size));
        let t3 = Instant::now();
        start_ms.push((t1 - t0).as_secs_f64() * 1e3);
        first_ms.push((t2 - t1).as_secs_f64() * 1e3);
        second_ms.push((t3 - t2).as_secs_f64() * 1e3);
        h.gpu.send(GpuRequest::Release(S));
        assert!(matches!(h.next(), GpuReply::Released(S)));
        h.gpu.send(GpuRequest::Shutdown);
        assert!(matches!(h.next(), GpuReply::Exited));
        let deadline = Instant::now() + WAIT;
        while !h.gpu.try_join() {
            assert!(Instant::now() < deadline, "the GPU thread did not end");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let show = |v: &[f64]| {
        v.iter()
            .map(|t| format!("{t:.1}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "gpu start: device + attach [{}] ms; first frame [{}] ms; second frame [{}] ms",
        show(&start_ms),
        show(&first_ms),
        show(&second_ms)
    );
}
