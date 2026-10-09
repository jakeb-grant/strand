//! (M4) Which backend draws a surface, and the GPU's state as render and
//! logic see it (docs/architecture.md, "`strand-gpu`"). These live here,
//! not in `strand-gpu`, so render, the binary and logic name them in a
//! build without the GPU backend too.

use crate::id::SurfaceId;

/// What draws a surface: `Renderer::set_backend(surface, Backend)` once
/// the GPU thread answers a promotion.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Backend {
    /// vello_cpu into the surface's shm buffer (every surface starts
    /// here).
    #[default]
    Cpu,
    /// The GPU thread presents through wgpu's WSI and is the surface's
    /// only committer while it does.
    GpuPresent,
    /// The GPU renders offscreen and reads the frame back; the main
    /// thread commits it as a CPU frame with full damage.
    GpuReadback,
}

impl Backend {
    /// True for the two GPU backends.
    pub const fn is_gpu(self) -> bool {
        !matches!(self, Backend::Cpu)
    }
}

/// What render asks the binary to do about the GPU
/// (`Renderer::take_backend_changes`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackendChange {
    /// Promote `surface`: start the `Gpu` if needed and attach it.
    Promote(SurfaceId),
    /// Give `surface` back to the CPU.
    Demote(SurfaceId),
    /// Nothing has used the GPU for 30 s: drop the `Gpu` (its thread
    /// ends with the device).
    Drop,
}

/// The adapter the GPU thread opened, for diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AdapterInfo {
    /// The device's name (`Intel(R) Graphics (PTL)`, `llvmpipe …`).
    pub name: String,
    /// The driver and its version, as the adapter reports them.
    pub driver: String,
    /// A software rasteriser (lavapipe), accepted only with
    /// `STRAND_GPU_SOFTWARE=1`.
    pub software: bool,
}

/// Why the GPU is not drawing, as render reports it
/// (`Renderer::gpu_status`) and logic hears it (`ToLogic::GpuStatus`,
/// while a shader or bundled effect is shown).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum GpuStatus {
    /// Nothing has asked for the GPU.
    #[default]
    Unused,
    /// The device is being created; the CPU fallback draws meanwhile.
    Starting,
    /// The device is up.
    Up(AdapterInfo),
    /// No device: the CPU fallback draws, and `reason` says why (no
    /// Vulkan driver, a lost device, [`GpuStatus::NOT_BUILT`]).
    Unavailable { reason: String },
}

impl GpuStatus {
    /// The reason a build without the `gpu` feature reports.
    pub const NOT_BUILT: &'static str = "built without the GPU backend";

    /// True while the CPU fallback draws what a GPU would.
    pub fn is_fallback(&self) -> bool {
        !matches!(self, GpuStatus::Up(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_and_status() {
        assert_eq!(Backend::default(), Backend::Cpu);
        assert!(!Backend::Cpu.is_gpu());
        assert!(Backend::GpuPresent.is_gpu() && Backend::GpuReadback.is_gpu());
        assert_eq!(GpuStatus::default(), GpuStatus::Unused);
        let up = GpuStatus::Up(AdapterInfo {
            name: "llvmpipe".into(),
            driver: "Mesa 25.2.8".into(),
            software: true,
        });
        assert!(!up.is_fallback());
        assert!(GpuStatus::Starting.is_fallback());
        let not_built = GpuStatus::Unavailable {
            reason: GpuStatus::NOT_BUILT.into(),
        };
        assert!(not_built.is_fallback());
        assert_eq!(not_built.clone(), not_built);
        let s = SurfaceId(3);
        assert_ne!(BackendChange::Promote(s), BackendChange::Demote(s));
    }
}
