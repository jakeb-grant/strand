//! (M4) The host side of the GPU tests (`gpu.rs`, `gpu_effects.rs`):
//! starts the `Gpu` when the renderer asks and hands replies back, as
//! `strand run` does; and the device to test on.

#![allow(dead_code)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use strand_gpu::{Gpu, GpuOptions, GpuReply, GpuRequest};
use strand_render::Renderer;
use strand_scene::*;

/// How long a reply may take (a cold lavapipe device: well under this).
pub const WAIT: Duration = Duration::from_secs(60);

/// The host side: starts the `Gpu` when the renderer has requests and
/// hands replies back, as `strand run` does.
pub struct Host {
    pub gpu: Option<Gpu>,
    pings: mpsc::Receiver<()>,
    ping: mpsc::Sender<()>,
    opts: GpuOptions,
    pub replies: Vec<GpuReply>,
}

impl Host {
    pub fn new(opts: GpuOptions) -> Self {
        let (ping, pings) = mpsc::channel();
        Host {
            gpu: None,
            pings,
            ping,
            opts,
            replies: Vec::new(),
        }
    }

    /// Sends what the renderer asks (starting the `Gpu`), and attaches
    /// promoted surfaces for readback.
    pub fn send(&mut self, r: &mut Renderer) {
        for c in r.take_backend_changes() {
            match c {
                BackendChange::Promote(s) => {
                    self.start();
                    self.request(GpuRequest::Attach {
                        surface: s,
                        handles: None,
                        size: Size::new(240, 60),
                        scale: Scale::ONE,
                        opaque: false,
                    });
                }
                BackendChange::Demote(s) => self.request(GpuRequest::Release(s)),
                BackendChange::Drop => {
                    if let Some(g) = self.gpu.take() {
                        drop(g);
                    }
                }
            }
        }
        let reqs = r.take_gpu_requests();
        if !reqs.is_empty() {
            self.start();
        }
        for q in reqs {
            self.request(q);
        }
    }

    pub fn start(&mut self) {
        if self.gpu.is_none() {
            let ping = self.ping.clone();
            self.gpu = Some(Gpu::spawn(
                Box::new(move || {
                    let _ = ping.send(());
                }),
                self.opts,
            ));
        }
    }

    pub fn request(&mut self, q: GpuRequest) {
        if let Some(g) = &self.gpu {
            g.send(q);
        }
    }

    /// Waits for a reply `want` accepts, delivering every reply to `r`.
    pub fn until(&mut self, r: &mut Renderer, want: impl Fn(&GpuReply) -> bool) -> GpuReply {
        let deadline = Instant::now() + WAIT;
        loop {
            while let Some(reply) = self.gpu.as_mut().and_then(Gpu::try_recv) {
                let hit = want(&reply);
                self.replies.push(reply.clone());
                r.deliver_gpu(reply.clone());
                self.send(r);
                if hit {
                    return reply;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no reply in {WAIT:?}: {:?}", self.replies);
            let _ = self.pings.recv_timeout(left.min(Duration::from_millis(50)));
        }
    }
}

/// A device to test on, or `None` (skipped) without one, unless
/// `STRAND_REQUIRE_GPU=1`.
pub fn device() -> Option<GpuOptions> {
    let opts = GpuOptions::from_env();
    let (tx, rx) = mpsc::channel();
    let mut g = Gpu::spawn(
        Box::new(move || {
            let _ = tx.send(());
        }),
        opts,
    );
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(reply) = g.try_recv() {
            match reply {
                GpuReply::Ready(_) => return Some(opts),
                GpuReply::Unavailable(e) => {
                    assert!(
                        std::env::var("STRAND_REQUIRE_GPU").as_deref() != Ok("1"),
                        "STRAND_REQUIRE_GPU=1 and no device: {e}"
                    );
                    eprintln!("skipped: no GPU device ({e})");
                    return None;
                }
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "the GPU thread did not answer");
        let _ = rx.recv_timeout(Duration::from_millis(50));
    }
}
