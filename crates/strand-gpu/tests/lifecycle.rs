//! The GPU thread's lifecycle on whatever Vulkan device the environment
//! has: lavapipe in the container and CI (with `STRAND_GPU_SOFTWARE=1`),
//! the laptop's GPU in `gpu.sh`. Without a device the test skips, unless
//! `STRAND_REQUIRE_GPU=1`. Alone in its process: it counts threads and
//! mappings.

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_gpu::{AlphaColor, Frame, GpuReply, GpuRequest};
use strand_scene::{Scale, Size};

#[test]
fn the_device_is_created_on_first_visible_effect_and_dropped_after_idle() {
    // Nothing GPU runs before something asks for it.
    assert_eq!(gpu_threads(), 0);
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    assert!(
        !maps.contains("libvulkan"),
        "no Vulkan library is mapped before the GPU starts"
    );
    let Some(mut h) = start() else { return };
    // The thread, and the driver's own threads it started (they take its
    // name).
    assert!(gpu_threads() >= 1, "the `strand-gpu` thread runs");
    attach(&mut h, Size::new(16, 8));
    let px = frame(
        &mut h,
        Frame {
            surface: S,
            id: 1,
            size: Size::new(16, 8),
            scale: Scale::ONE,
            ops: vec![],
            uploads: vec![],
            retire: Vec::new(),
            clear: AlphaColor::new([0.0, 0.0, 1.0, 1.0]),
        },
    );
    // Blue, as BGRA bytes.
    assert_eq!(pixel(&px, 3, 3), [255, 0, 0, 255]);
    h.gpu.send(GpuRequest::Release(S));
    assert!(matches!(h.next(), GpuReply::Released(S)));
    h.gpu.send(GpuRequest::Shutdown);
    assert!(matches!(h.next(), GpuReply::Exited));
    let deadline = Instant::now() + WAIT;
    while !h.gpu.try_join() {
        assert!(Instant::now() < deadline, "the GPU thread did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The driver's threads end with the device too (lavapipe's within a
    // moment of the drop).
    let deadline = Instant::now() + Duration::from_secs(5);
    while gpu_threads() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(gpu_threads(), 0, "the thread ends with the device");
    assert!(h.gpu.exited());
}
