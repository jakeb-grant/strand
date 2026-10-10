//! Copying a texture into a mapped buffer: [`crate::Readback`].

use crate::device::Device;
use crate::{GpuError, GpuErrorKind, Readback};

/// Bytes per row of a `width`-pixel copy, padded to wgpu's alignment.
pub(crate) fn padded_stride(width: u32) -> u32 {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    (width * 4).div_ceil(align) * align
}

/// A mappable buffer for `width × height` reads, kept per target.
pub(crate) struct ReadBuffer {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
}

impl ReadBuffer {
    pub fn new(dev: &Device, width: u32, height: u32) -> Self {
        let buffer = dev.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strand readback"),
            size: u64::from(padded_stride(width)) * u64::from(height.max(1)),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            buffer,
            width,
            height,
        }
    }

    pub fn fits(&self, width: u32, height: u32) -> bool {
        self.width == width && self.height == height
    }

    /// Records the copy of `texture`'s top-left `width × height`.
    pub fn record(&self, encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture) {
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_stride(self.width)),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// After the copy was submitted: waits for it on this thread and
    /// takes the pixels.
    pub fn read(&self, dev: &Device) -> Result<Readback, GpuError> {
        let slice = self.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        dev.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::new(GpuErrorKind::Render, format!("readback: {e}")))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(GpuError::new(
                    GpuErrorKind::Render,
                    format!("readback map: {e}"),
                ));
            }
            Err(_) => {
                return Err(GpuError::new(
                    GpuErrorKind::Render,
                    "readback map never answered",
                ));
            }
        }
        let bytes = slice
            .get_mapped_range()
            .map_err(|e| GpuError::new(GpuErrorKind::Render, format!("readback range: {e}")))?
            .to_vec();
        self.buffer.unmap();
        Ok(Readback {
            width: self.width,
            height: self.height,
            stride: padded_stride(self.width),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_pad_to_256_bytes() {
        assert_eq!(super::padded_stride(1), 256);
        assert_eq!(super::padded_stride(64), 256);
        assert_eq!(super::padded_stride(65), 512);
    }
}
