//! How long the GPU ran a presented frame (`GpuReply::Presented`'s
//! `ran`), from timestamp queries read back after the fact: the GPU
//! thread never waits for a presented frame's work, so it starts the
//! next surface's frame while the GPU runs this one.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::device::Device;

/// What the device needs for a frame's timestamps (requested when the
/// adapter has them; every Vulkan driver strand meets, lavapipe too).
pub(crate) const FEATURES: wgpu::Features =
    wgpu::Features::TIMESTAMP_QUERY.union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);

/// Frames timed at once per surface: one frame's result is read while
/// the GPU may still run the next, so two let every frame be timed; a
/// frame that finds both busy goes untimed.
const SLOTS: usize = 2;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

struct Slot {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    frame: u64,
    state: Arc<AtomicU8>,
}

/// One presented surface's timestamp queries.
pub(crate) struct Timer {
    slots: Vec<Slot>,
    /// Nanoseconds per timestamp tick.
    period: f64,
}

impl Timer {
    /// None when the device has no timestamps inside encoders.
    pub fn new(dev: &Device) -> Option<Timer> {
        if !dev.device.features().contains(FEATURES) {
            return None;
        }
        let buffer = |usage| {
            dev.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("strand frame timer"),
                size: 16,
                usage,
                mapped_at_creation: false,
            })
        };
        let slots = (0..SLOTS)
            .map(|_| Slot {
                set: dev.device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("strand frame timer"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                }),
                resolve: buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
                read: buffer(wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
                frame: 0,
                state: Arc::new(AtomicU8::new(IDLE)),
            })
            .collect();
        Some(Timer {
            slots,
            period: f64::from(dev.queue.get_timestamp_period()),
        })
    }

    /// At the start of a frame's commands: its start time in a free slot,
    /// if there is one (the slot stays free until `submitted`).
    pub fn begin(&self, encoder: &mut wgpu::CommandEncoder) -> Option<usize> {
        let i = self
            .slots
            .iter()
            .position(|s| s.state.load(Ordering::Acquire) == IDLE)?;
        encoder.write_timestamp(&self.slots[i].set, 0);
        Some(i)
    }

    /// At the end of the frame's commands: its end time, and both copied
    /// where they can be read.
    pub fn end(&self, encoder: &mut wgpu::CommandEncoder, slot: usize) {
        let s = &self.slots[slot];
        encoder.write_timestamp(&s.set, 1);
        encoder.resolve_query_set(&s.set, 0..2, &s.resolve, 0);
        encoder.copy_buffer_to_buffer(&s.resolve, 0, &s.read, 0, 16);
    }

    /// After the frame's submission: its times are read once the GPU has
    /// run it, without waiting here.
    pub fn submitted(&mut self, slot: usize, frame: u64) {
        let s = &mut self.slots[slot];
        s.frame = frame;
        s.state.store(MAPPING, Ordering::Release);
        let state = s.state.clone();
        s.read.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            state.store(if r.is_ok() { MAPPED } else { FAILED }, Ordering::Release);
        });
    }

    /// The frames whose times have come since the last call (render's
    /// frame number and how long the GPU ran it), oldest first. Polls
    /// the device without waiting.
    pub fn ran(&mut self, dev: &Device) -> Vec<(u64, Duration)> {
        let _ = dev.device.poll(wgpu::PollType::Poll);
        let mut out = Vec::new();
        for s in &mut self.slots {
            match s.state.load(Ordering::Acquire) {
                MAPPED => {
                    if let Ok(view) = s.read.slice(..).get_mapped_range() {
                        let tick = |i: usize| {
                            view.get(i * 8..i * 8 + 8)
                                .and_then(|b| b.try_into().ok())
                                .map_or(0, u64::from_le_bytes)
                        };
                        let ns = tick(1).saturating_sub(tick(0)) as f64 * self.period;
                        out.push((s.frame, Duration::from_nanos(ns as u64)));
                    }
                    s.read.unmap();
                    s.state.store(IDLE, Ordering::Release);
                }
                FAILED => s.state.store(IDLE, Ordering::Release),
                _ => {}
            }
        }
        out.sort_by_key(|(frame, _)| *frame);
        out
    }
}
