//! The instance, adapter and device: one per process, created on the GPU
//! thread, requested without a surface so readback works whatever the WSI
//! can do.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{AdapterInfo, GpuError, GpuErrorKind, GpuOptions};

/// The device and what was opened to get it.
pub(crate) struct Device {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: AdapterInfo,
    /// Set by the device-lost callback (and by a panic caught on the
    /// thread).
    pub lost: Arc<AtomicBool>,
    /// Set when a submission ran past [`crate::readback::HUNG_AFTER`]
    /// (an endless loop in a validated shader: naga bounds neither, and
    /// lavapipe has no driver reset). The device is lost with it, and
    /// is never dropped: dropping waits for a queue that may never
    /// drain.
    pub hung: AtomicBool,
}

/// Whether wgpu's adapter is a software rasteriser.
pub(crate) fn is_software(info: &wgpu::AdapterInfo) -> bool {
    info.device_type == wgpu::DeviceType::Cpu
}

impl Device {
    /// Opens the highest-performance Vulkan adapter and a device on it.
    pub fn open(opts: &GpuOptions) -> Result<Device, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            ..wgpu::RequestAdapterOptions::default()
        }))
        .map_err(|e| GpuError::new(GpuErrorKind::NoAdapter, format!("no Vulkan adapter ({e})")))?;
        let raw = adapter.get_info();
        let info = AdapterInfo {
            name: raw.name.clone(),
            driver: [raw.driver.as_str(), raw.driver_info.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            software: is_software(&raw),
        };
        if info.software && !opts.software {
            return Err(GpuError::new(
                GpuErrorKind::Software,
                format!(
                    "only a software Vulkan adapter ({}); the CPU draws instead \
                     ({}=1 accepts it)",
                    info.name,
                    GpuOptions::SOFTWARE_ENV
                ),
            ));
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("strand"),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|e| {
            GpuError::new(
                GpuErrorKind::Device,
                format!("cannot open {}: {e}", info.name),
            )
        })?;
        let lost = Arc::new(AtomicBool::new(false));
        device.set_device_lost_callback({
            let lost = lost.clone();
            move |reason, msg| {
                log::warn!("GPU device lost ({reason:?}): {msg}");
                lost.store(true, Ordering::SeqCst);
            }
        });
        // Errors not caught by a scope (wgpu panics on them by default)
        // are logged: the frame or pass that caused them reports its own
        // failure through its scope.
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
            log::warn!("GPU error: {e}");
        }));
        Ok(Device {
            instance,
            adapter,
            device,
            queue,
            info,
            lost,
            hung: AtomicBool::new(false),
        })
    }

    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Runs `f` inside a validation and out-of-memory error scope and
    /// returns its error, if any.
    pub fn scoped<T>(&self, f: impl FnOnce() -> T) -> (T, Option<String>) {
        let oom = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f();
        let v = pollster::block_on(validation.pop());
        let o = pollster::block_on(oom.pop());
        (out, v.or(o).map(|e| e.to_string()))
    }
}
