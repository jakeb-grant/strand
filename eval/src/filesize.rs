//! (png-aspect) An image file's own pixel size, read from its header by
//! the sandbox, apart from the renderer's decoders: `placement`'s `fit`
//! places the file's pixels by it, so a decode that reports another
//! aspect for its source (a reduced side rounded up) is seen.

use std::io::Read;

/// The pixel size a PNG, JPEG or GIF file's header gives, if `path` is
/// one of them and can be read.
pub fn of(path: &std::path::Path) -> Option<[u32; 2]> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(1 << 16)
        .read_to_end(&mut head)
        .ok()?;
    of_bytes(&head)
}

/// [`of`] on a file's first bytes.
pub fn of_bytes(b: &[u8]) -> Option<[u32; 2]> {
    let be32 = |i: usize| Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?));
    let be16 = |i: usize| Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let le16 = |i: usize| Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32);
    let size = if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        [be32(16)?, be32(20)?]
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        [le16(6)?, le16(8)?]
    } else if b.starts_with(&[0xff, 0xd8]) {
        // Markers up to the first start of frame (SOF0 to SOF15, less
        // DHT, JPG and DAC): its height, then its width.
        let mut i = 2;
        loop {
            while *b.get(i)? != 0xff {
                i += 1;
            }
            while *b.get(i)? == 0xff {
                i += 1;
            }
            let m = *b.get(i)?;
            i += 1;
            if matches!(m, 0xd0..=0xd9 | 0x01) {
                continue;
            }
            let len = be16(i)? as usize;
            if (0xc0..=0xcf).contains(&m) && !matches!(m, 0xc4 | 0xc8 | 0xcc) {
                break [be16(i + 5)?, be16(i + 3)?];
            }
            i += len;
        }
    } else {
        return None;
    };
    (size[0] > 0 && size[1] > 0).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_png_jpeg_and_gif_give_their_sizes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend_from_slice(&60u32.to_be_bytes());
        png.extend_from_slice(&40u32.to_be_bytes());
        assert_eq!(of_bytes(&png), Some([60, 40]));
        let gif = [
            b"GIF89a".as_slice(),
            &61u16.to_le_bytes(),
            &37u16.to_le_bytes(),
        ]
        .concat();
        assert_eq!(of_bytes(&gif), Some([61, 37]));
        // SOI, an APP0 of 16 bytes, then SOF0: precision, height, width.
        let mut jpg = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16];
        jpg.extend_from_slice(&[0; 14]);
        jpg.extend_from_slice(&[0xff, 0xc0, 0, 17, 8, 0, 60, 0, 100]);
        assert_eq!(of_bytes(&jpg), Some([100, 60]));
        assert_eq!(of_bytes(b"<svg/>"), None);
    }
}
