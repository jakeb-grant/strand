//! A validated shader that never ends (naga bounds neither loops nor run
//! time) on whatever Vulkan device the environment has (see
//! `lifecycle.rs`). Alone in its process: the driver's queue keeps
//! spinning on it until the process exits.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use strand_gpu::{GpuReply, GpuRequest, PassFrame, PassGlobals};
use strand_scene::{ShaderInput, ShaderPass, ShaderRef, Size};

/// The GPU thread does not wait forever on a pass that never ends: the
/// pass fails, the device is reported lost (render then draws on the
/// CPU and does not ask for that pass again), and the thread ends, so
/// a hung driver never leaves render waiting on a reply.
#[test]
fn a_shader_that_never_ends_loses_the_device_and_ends_the_thread() {
    let Some(mut h) = start() else { return };
    // `strand.time` is never negative, which no compiler can know: the
    // loop has no end it could prove, and its sum is the colour, so it
    // cannot be dropped either.
    let wgsl = "
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    var acc = v.uv.x;
    var i = 0u;
    loop {
        acc = fract(acc * 1.618 + sin(f32(i)));
        i = i + 1u;
        if strand.time < -1.0 && acc > 2.0 {
            break;
        }
    }
    return vec4<f32>(acc, 0.0, 0.0, 1.0);
}
";
    let pass = ShaderPass {
        code: ShaderRef::File(code(wgsl, vec![])),
        uniforms: Arc::from([].as_slice()),
        input: ShaderInput::None,
    };
    h.gpu.send(GpuRequest::Pass(PassFrame {
        key: 9,
        id: 1,
        size: Size::new(16, 16),
        pass,
        globals: PassGlobals {
            time: 1.0,
            scale: 1.0,
            pointer: [-1.0, -1.0],
        },
        then: Vec::new(),
        input: None,
    }));
    match h.next() {
        GpuReply::Failed { key, .. } => assert_eq!(key, Some(9), "the pass fails"),
        other => panic!("expected the pass to fail, got {other:?}"),
    }
    match h.next() {
        GpuReply::Lost(e) => assert!(e.message.contains("stopped answering"), "{e}"),
        other => panic!("expected Lost, got {other:?}"),
    }
    assert!(matches!(h.next(), GpuReply::Exited));
    let deadline = Instant::now() + WAIT;
    while !h.gpu.try_join() {
        assert!(Instant::now() < deadline, "the GPU thread did not end");
        std::thread::sleep(Duration::from_millis(5));
    }
}
