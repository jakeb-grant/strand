//! The nightly Vulkan validation job (`scripts/container/validation.sh`;
//! decisions.md robustness) checks itself here: that a bad call fails the run,
//! that strand's GPU thread runs under the layer when the job asks, and that
//! nothing else ever does, since the layer changes the memory and the pixels
//! the gates hold.

mod common;

use wgpu::hal::api::Vulkan;

fn layer_is_mapped() -> bool {
    std::fs::read_to_string("/proc/self/maps")
        .unwrap()
        .contains("libVkLayer_khronos_validation")
}

/// With the job's environment this never returns: the layer traps on the bad
/// call. `validation.sh` runs it and requires exactly that; on its own, nothing
/// stops a run that passes, so it stays ignored.
#[test]
#[ignore = "run by scripts/container/validation.sh, which requires it to trap"]
fn a_bad_vulkan_call_traps() {
    // As `Device::open` makes them, which is private to the crate.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).unwrap();
    let (device, _queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
    let hal = unsafe { device.as_hal::<Vulkan>() }.unwrap();
    let raw = hal.raw_device();
    // VUID-VkBufferCreateInfo-size-00912: a buffer's size must be above zero.
    let info = ash::vk::BufferCreateInfo::default()
        .size(0)
        .usage(ash::vk::BufferUsageFlags::STORAGE_BUFFER);
    let buffer = unsafe { raw.create_buffer(&info, None) };
    if let Ok(buffer) = buffer {
        unsafe { raw.destroy_buffer(buffer, None) };
    }
}

/// The job's GPU tests measure nothing if the layer never loaded.
#[test]
#[ignore = "run by scripts/container/validation.sh, with the layer on"]
fn strands_gpu_thread_runs_under_the_layer() {
    let _h = common::start().expect("a GPU");
    assert!(layer_is_mapped(), "the validation layer is not loaded");
}

/// The image has the layer for that job alone: where the loader finds it, every
/// debug build's wgpu asks for it, the gates' tests included.
#[test]
fn the_layer_stays_off_unless_asked_for() {
    if std::env::var_os("VK_ADD_LAYER_PATH").is_some() {
        return;
    }
    let Some(_h) = common::start() else { return };
    assert!(
        !layer_is_mapped(),
        "the validation layer loaded with nothing asking for it: \
         its manifest is back on the loader's search path"
    );
}
