//! PNG files of frames and contact sheets: straight-alpha RGBA, 8 bits.

use std::io::Cursor;

use crate::Error;

/// `rgba` (`width` × `height`, straight alpha) as a PNG file.
pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Error> {
    let bad = |e: png::EncodingError| Error::Run(format!("png: {e}"));
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut w = enc.write_header().map_err(bad)?;
    w.write_image_data(rgba).map_err(bad)?;
    w.finish().map_err(bad)?;
    Ok(out)
}

/// A PNG file as `(width, height, rgba)`. Grey, grey-alpha and RGB files
/// are widened to RGBA.
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), Error> {
    let bad = |e: png::DecodingError| Error::Run(format!("png: {e}"));
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut r = dec.read_info().map_err(bad)?;
    let size = r
        .output_buffer_size()
        .ok_or_else(|| Error::Run("png: too large to decode".into()))?;
    let mut buf = vec![0; size];
    let info = r.next_frame(&mut buf).map_err(bad)?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let px = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|g| [*g, *g, *g, 255]).collect(),
        png::ColorType::Indexed => {
            return Err(Error::Run("png: an indexed file was not expanded".into()));
        }
    };
    if rgba.len() != px * 4 {
        return Err(Error::Run("png: the image is not 8 bits deep".into()));
    }
    Ok((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_round_trips() {
        let rgba: Vec<u8> = (0..3 * 2 * 4).map(|i| (i * 11) as u8).collect();
        let png = encode(3, 2, &rgba).unwrap();
        assert_eq!(decode(&png).unwrap(), (3, 2, rgba));
    }
}
