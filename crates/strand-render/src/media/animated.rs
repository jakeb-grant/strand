//! (M4) Animated GIF, APNG and WebP (design.md, "Generative, data-driven
//! and media": `image "spin.gif"`, "frames streamed, not cached whole").
//!
//! A file is kept compressed. A [`Player`] per drawn image (its source at
//! one drawn size) decodes frames one after another into a canvas at the
//! source's own size, composing each over the last as the format says
//! (GIF disposal, APNG dispose and blend; WebP composes in its decoder),
//! so only the canvas and the frame asked for are ever decoded. Asking for
//! an earlier frame than the next one (the loop wrapped) starts the
//! decoder again from the file's start.
//!
//! The [`Timeline`] (each frame's delay, the loop count) is read once,
//! without decoding pixels where the format allows (GIF's frame headers,
//! APNG's `fcTL` chunks, WebP's `ANMF` headers), and travels with every
//! decoded frame so render can map a node's time to a frame.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use crate::image::{ImageError, ImageKey, MAX_DECODE_BYTES};

/// Most frames an animation is played with; later frames are not shown.
pub const MAX_FRAMES: usize = 4096;

/// The shortest frame delay played. A GIF or APNG delay under 20 ms (0
/// included) plays at 100 ms, as browsers do: such files were made for
/// decoders that treat them so.
const MIN_DELAY_MS: u64 = 20;
const SHORT_DELAY_MS: u64 = 100;

/// The longest clock tick an animation's frames are timed by (a slower
/// tick would draw a frame late).
const MIN_TICK_MS: u64 = 10;

/// Players kept at once: images being played (each holds its file and a
/// canvas at the source's size). The least recently used goes past this.
const MAX_PLAYERS: usize = 16;

/// Bytes all players may hold together ([`Player::bytes`]: their files,
/// each counted once however many sizes play it, their canvases and
/// their decoders' frames). Past it the least recently used players go,
/// never the one being drawn: one animation larger than this still plays,
/// alone (decisions.md, m4-effects-media-w2).
pub const MAX_PLAYER_BYTES: usize = 16 << 20;

/// An animated image's frames in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeline {
    /// Each frame's delay, milliseconds.
    pub delays: Vec<u64>,
    /// One loop, milliseconds.
    pub total: u64,
    /// The step the node's time moves in (the delays' greatest common
    /// divisor, at least 10 ms), so every frame change lands on one; the
    /// clock wakes only at frame changes ([`Timeline::until_change`]).
    pub tick: Duration,
    /// How many times it plays; `None` forever.
    pub loops: Option<u32>,
}

impl Timeline {
    fn new(raw: Vec<u64>, loops: Option<u32>) -> Option<Self> {
        if raw.len() < 2 {
            return None;
        }
        let delays: Vec<u64> = raw
            .into_iter()
            .take(MAX_FRAMES)
            .map(|d| if d < MIN_DELAY_MS { SHORT_DELAY_MS } else { d })
            .collect();
        let total = delays.iter().sum();
        let g = delays.iter().copied().fold(0, gcd).max(MIN_TICK_MS);
        Some(Timeline {
            delays,
            total,
            tick: Duration::from_millis(g),
            loops: loops.filter(|n| *n > 0),
        })
    }

    /// The frame shown `t` seconds after the image appeared: the last
    /// frame once a finite loop count has played out.
    pub fn frame_at(&self, t: f32) -> u32 {
        if self.done(t) {
            return self.delays.len() as u32 - 1;
        }
        let ms = (t.max(0.0) as f64 * 1000.0).round() as u64 % self.total.max(1);
        let mut acc = 0;
        for (i, d) in self.delays.iter().enumerate() {
            acc += d;
            if ms < acc {
                return i as u32;
            }
        }
        self.delays.len() as u32 - 1
    }

    /// How long after `t` the frame shown next changes (the next frame
    /// boundary, the loop's end included); `None` once a finite loop
    /// count has played out. The clock wakes for that boundary only, not
    /// for every [`Timeline::tick`] between (decisions.md,
    /// m4-effects-media-w2).
    pub fn until_change(&self, t: f32) -> Option<Duration> {
        if self.done(t) {
            return None;
        }
        let total = self.total.max(1);
        let into = (t.max(0.0) as f64 * 1000.0).round() as u64 % total;
        let mut acc = 0;
        for d in &self.delays {
            acc += d;
            if into < acc {
                return Some(Duration::from_millis(acc - into));
            }
        }
        Some(Duration::from_millis(total - into))
    }

    /// True once a finite loop count has played out at `t`: the last
    /// frame stays and nothing ticks any more.
    pub fn done(&self, t: f32) -> bool {
        self.loops
            .is_some_and(|n| (t.max(0.0) as f64 * 1000.0) >= (self.total as f64) * n as f64)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The animated formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Gif,
    Png,
    Webp,
}

/// The format `data` is in, if it is one played here (a PNG counts only
/// with an `acTL` chunk; a still PNG decodes as before).
pub fn format(data: &[u8]) -> Option<Format> {
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else if data.starts_with(b"\x89PNG") && png_has_actl(data) {
        Some(Format::Png)
    } else {
        None
    }
}

/// True if a PNG has an `acTL` chunk before its image data.
fn png_has_actl(data: &[u8]) -> bool {
    let mut at = 8;
    while at + 8 <= data.len() {
        let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        let kind = &data[at + 4..at + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        at = match at.checked_add(12).and_then(|a| a.checked_add(len)) {
            Some(a) => a,
            None => return false,
        };
    }
    false
}

/// Reads `data`'s timeline: `None` for a single frame (a still image).
pub fn timeline(data: &[u8], format: Format) -> Result<Option<Timeline>, ImageError> {
    match format {
        Format::Gif => {
            let mut opts = gif::DecodeOptions::new();
            opts.skip_frame_decoding(true);
            opts.set_memory_limit(gif::MemoryLimit::Bytes(
                std::num::NonZeroU64::new(MAX_DECODE_BYTES as u64)
                    .unwrap_or(std::num::NonZeroU64::MIN),
            ));
            let mut dec = opts.read_info(Cursor::new(data)).map_err(gif_err)?;
            let mut delays = Vec::new();
            while let Some(f) = dec.read_next_frame().map_err(gif_err)? {
                delays.push(f.delay as u64 * 10);
                if delays.len() > MAX_FRAMES {
                    break;
                }
            }
            let loops = match dec.repeat() {
                gif::Repeat::Infinite => None,
                // The count is of repeats after the first play.
                gif::Repeat::Finite(n) => Some(n as u32 + 1),
            };
            Ok(Timeline::new(delays, loops))
        }
        Format::Png => {
            let mut reader = png_reader(data)?;
            let Some(actl) = reader.info().animation_control else {
                return Ok(None);
            };
            let mut delays = Vec::new();
            // The default image is not a frame when no `fcTL` precedes it.
            let mut fc = reader.info().frame_control;
            let frames = (actl.num_frames as usize).min(MAX_FRAMES);
            while delays.len() < frames {
                let c = match fc.take() {
                    Some(c) => c,
                    None => *reader.next_frame_info().map_err(png_err)?,
                };
                let den = if c.delay_den == 0 {
                    100
                } else {
                    c.delay_den as u64
                };
                delays.push(c.delay_num as u64 * 1000 / den);
            }
            Ok(Timeline::new(delays, Some(actl.num_plays)))
        }
        Format::Webp => webp_timeline(data),
    }
}

/// A WebP's `ANMF` frame durations and `ANIM` loop count, read from the
/// RIFF chunks without decoding.
fn webp_timeline(data: &[u8]) -> Result<Option<Timeline>, ImageError> {
    let mut at = 12;
    let mut delays = Vec::new();
    let mut loops = None;
    while at + 8 <= data.len() {
        let kind = &data[at..at + 4];
        let len =
            u32::from_le_bytes([data[at + 4], data[at + 5], data[at + 6], data[at + 7]]) as usize;
        let body = data.get(at + 8..at + 8 + len).unwrap_or(&[]);
        match kind {
            b"ANIM" if body.len() >= 6 => {
                let n = u16::from_le_bytes([body[4], body[5]]);
                loops = Some(n as u32);
            }
            b"ANMF" if body.len() >= 16 => {
                let d = u32::from_le_bytes([body[12], body[13], body[14], 0]);
                delays.push(d as u64);
                if delays.len() > MAX_FRAMES {
                    break;
                }
            }
            _ => {}
        }
        // Chunks are padded to an even length.
        at = match at.checked_add(8 + len + (len & 1)) {
            Some(a) => a,
            None => break,
        };
    }
    Ok(Timeline::new(delays, loops))
}

fn gif_err(e: gif::DecodingError) -> ImageError {
    ImageError::Decode(e.to_string())
}

fn png_err(e: png::DecodingError) -> ImageError {
    ImageError::Decode(e.to_string())
}

fn webp_err(e: image_webp::DecodingError) -> ImageError {
    ImageError::Decode(e.to_string())
}

fn png_reader(data: &[u8]) -> Result<png::Reader<Cursor<&[u8]>>, ImageError> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    dec.read_info().map_err(png_err)
}

type Bytes = Arc<[u8]>;

/// A format's decoder, positioned before the next frame.
enum Decoder {
    Gif(Box<gif::Decoder<Cursor<Bytes>>>),
    Png(Box<png::Reader<Cursor<Bytes>>>),
    Webp(Box<image_webp::WebPDecoder<Cursor<Bytes>>>),
}

/// What happens to a frame's area before the next is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dispose {
    Keep,
    Clear,
    Restore,
}

/// One image playing at one drawn size: its file, its decoder and the
/// canvas the frames compose on (straight RGBA at the source's size).
pub struct Player {
    data: Bytes,
    format: Format,
    decoder: Option<Decoder>,
    w: u32,
    h: u32,
    canvas: Vec<u8>,
    /// The canvas before the current frame, for a `Restore` disposal.
    saved: Vec<u8>,
    /// The current frame's disposal and area, applied before the next.
    dispose: Option<(Dispose, [u32; 4])>,
    /// The index of the next frame the decoder gives.
    next: u32,
    /// Frames in one loop (from the timeline; at least 1).
    frames: u32,
    /// The timeline, `None` for a still image.
    pub timeline: Option<Arc<Timeline>>,
    used: u64,
}

impl std::fmt::Debug for Player {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Player")
            .field("format", &self.format)
            .field("size", &(self.w, self.h))
            .field("next", &self.next)
            .field("frames", &self.frames)
            .finish()
    }
}

impl Player {
    /// A player for `data` in `format`: its timeline is read, nothing is
    /// decoded yet.
    pub fn new(data: Bytes, format: Format) -> Result<Self, ImageError> {
        let timeline = timeline(&data, format)?.map(Arc::new);
        let frames = timeline.as_ref().map_or(1, |t| t.delays.len() as u32);
        let mut p = Player {
            data,
            format,
            decoder: None,
            w: 0,
            h: 0,
            canvas: Vec::new(),
            saved: Vec::new(),
            dispose: None,
            next: 0,
            frames,
            timeline,
            used: 0,
        };
        p.restart()?;
        Ok(p)
    }

    /// The source's size.
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// Bytes it holds besides its file: its canvas, the canvas a
    /// `Restore` frame saved, and its decoder's frame (counted at the
    /// canvas's size, the most a frame takes).
    pub fn bytes(&self) -> usize {
        self.canvas.capacity() + self.saved.capacity() + self.canvas.len()
    }

    /// Starts the decoder again at the first frame, on a clear canvas.
    fn restart(&mut self) -> Result<(), ImageError> {
        let cur = Cursor::new(self.data.clone());
        let (dec, w, h) = match self.format {
            Format::Gif => {
                let mut opts = gif::DecodeOptions::new();
                opts.set_color_output(gif::ColorOutput::RGBA);
                opts.set_memory_limit(gif::MemoryLimit::Bytes(
                    std::num::NonZeroU64::new(MAX_DECODE_BYTES as u64)
                        .unwrap_or(std::num::NonZeroU64::MIN),
                ));
                let d = opts.read_info(cur).map_err(gif_err)?;
                let (w, h) = (d.width() as u32, d.height() as u32);
                (Decoder::Gif(Box::new(d)), w, h)
            }
            Format::Png => {
                let mut dec = png::Decoder::new(cur);
                dec.set_transformations(png::Transformations::normalize_to_color8());
                let r = dec.read_info().map_err(png_err)?;
                let (w, h) = (r.info().width, r.info().height);
                (Decoder::Png(Box::new(r)), w, h)
            }
            Format::Webp => {
                let mut d = image_webp::WebPDecoder::new(cur).map_err(webp_err)?;
                // A frame disposed to the background clears to
                // transparent, as libwebp's animation decoder does (the
                // `ANIM` colour is only a hint); a still image has no
                // background to set.
                if d.is_animated() {
                    let _ = d.set_background_color([0, 0, 0, 0]);
                }
                let (w, h) = d.dimensions();
                (Decoder::Webp(Box::new(d)), w, h)
            }
        };
        let px = (w as usize).saturating_mul(h as usize);
        if w == 0 || h == 0 {
            return Err(ImageError::Decode("empty image".into()));
        }
        if px.saturating_mul(4) > MAX_DECODE_BYTES {
            return Err(ImageError::TooLarge);
        }
        self.w = w;
        self.h = h;
        self.canvas.clear();
        self.canvas.resize(px * 4, 0);
        self.saved.clear();
        self.dispose = None;
        self.next = 0;
        self.decoder = Some(dec);
        Ok(())
    }

    /// The canvas showing frame `n` (taken modulo the loop's frames):
    /// decoded from the current position, or from the start when `n` is
    /// behind it. Straight RGBA, [`Player::size`].
    pub fn frame(&mut self, n: u32) -> Result<&[u8], ImageError> {
        let n = n % self.frames.max(1);
        if self.next == 0 || n + 1 < self.next || self.decoder.is_none() {
            self.restart()?;
        }
        while self.next <= n {
            self.step()?;
        }
        Ok(&self.canvas)
    }

    /// Decodes the next frame onto the canvas.
    fn step(&mut self) -> Result<(), ImageError> {
        // The last frame's disposal.
        if let Some((d, [x, y, w, h])) = self.dispose.take() {
            match d {
                Dispose::Keep => {}
                Dispose::Clear => clear_rect(&mut self.canvas, self.w, [x, y, w, h]),
                Dispose::Restore => {
                    // The saved canvas becomes the canvas; none is kept
                    // until another frame needs restoring.
                    if self.saved.len() == self.canvas.len() {
                        self.canvas = std::mem::take(&mut self.saved);
                    }
                }
            }
        }
        let first = self.next == 0;
        let (cw, ch) = (self.w, self.h);
        match self.decoder.as_mut() {
            Some(Decoder::Gif(dec)) => {
                let Some(f) = dec.read_next_frame().map_err(gif_err)? else {
                    return Err(ImageError::Decode("GIF ended early".into()));
                };
                let rect = [f.left as u32, f.top as u32, f.width as u32, f.height as u32];
                let dispose = match f.dispose {
                    gif::DisposalMethod::Background => Dispose::Clear,
                    gif::DisposalMethod::Previous => Dispose::Restore,
                    _ => Dispose::Keep,
                };
                if dispose == Dispose::Restore {
                    self.saved.clone_from(&self.canvas);
                }
                // Transparent pixels (alpha 0) leave the canvas showing.
                blit(&mut self.canvas, cw, ch, &f.buffer, rect, true);
                self.dispose = Some((dispose, rect));
            }
            Some(Decoder::Png(r)) => {
                // A default image no `fcTL` precedes is no frame.
                if first && r.info().frame_control.is_none() {
                    let mut skip = vec![0; r.output_buffer_size().unwrap_or(0)];
                    r.next_frame(&mut skip).map_err(png_err)?;
                }
                let size = r
                    .output_buffer_size()
                    .ok_or_else(|| ImageError::Decode("PNG frame too large".into()))?;
                let mut buf = vec![0; size];
                let out = r.next_frame(&mut buf).map_err(png_err)?;
                let fc = r.info().frame_control.unwrap_or_default();
                let rect = [fc.x_offset, fc.y_offset, out.width, out.height];
                let rgba = to_rgba(&buf[..out.buffer_size()], out.color_type);
                let mut dispose = match fc.dispose_op {
                    png::DisposeOp::None => Dispose::Keep,
                    png::DisposeOp::Background => Dispose::Clear,
                    png::DisposeOp::Previous => Dispose::Restore,
                };
                // The first frame has nothing to restore.
                if first && dispose == Dispose::Restore {
                    dispose = Dispose::Clear;
                }
                if dispose == Dispose::Restore {
                    self.saved.clone_from(&self.canvas);
                }
                let over = fc.blend_op == png::BlendOp::Over;
                if over {
                    blend(&mut self.canvas, cw, ch, &rgba, rect);
                } else {
                    blit(&mut self.canvas, cw, ch, &rgba, rect, false);
                }
                self.dispose = Some((dispose, rect));
            }
            Some(Decoder::Webp(d)) => {
                let size = d
                    .output_buffer_size()
                    .ok_or_else(|| ImageError::Decode("WebP too large".into()))?;
                let mut buf = vec![0; size];
                if d.is_animated() {
                    d.read_frame(&mut buf).map_err(webp_err)?;
                } else {
                    d.read_image(&mut buf).map_err(webp_err)?;
                }
                let color = if d.has_alpha() {
                    png::ColorType::Rgba
                } else {
                    png::ColorType::Rgb
                };
                self.canvas = to_rgba(&buf, color);
                self.canvas.resize(cw as usize * ch as usize * 4, 0);
            }
            None => return Err(ImageError::Decode("no decoder".into())),
        }
        self.next += 1;
        Ok(())
    }
}

/// Straight RGBA from 8-bit samples of `color`.
fn to_rgba(px: &[u8], color: png::ColorType) -> Vec<u8> {
    let n = color.samples();
    let mut out = Vec::with_capacity(px.len() / n.max(1) * 4);
    for p in px.chunks_exact(n.max(1)) {
        out.extend_from_slice(&match color {
            png::ColorType::Rgba => [p[0], p[1], p[2], p[3]],
            png::ColorType::Rgb => [p[0], p[1], p[2], 255],
            png::ColorType::GrayscaleAlpha => [p[0], p[0], p[0], p[1]],
            _ => [p[0], p[0], p[0], 255],
        });
    }
    out
}

/// The part of `[x, y, w, h]` inside a `cw × ch` canvas, as row ranges.
fn clip(cw: u32, ch: u32, [x, y, w, h]: [u32; 4]) -> Option<(usize, usize, usize, usize)> {
    let x1 = x.saturating_add(w).min(cw);
    let y1 = y.saturating_add(h).min(ch);
    (x < x1 && y < y1).then_some((x as usize, y as usize, x1 as usize, y1 as usize))
}

fn clear_rect(canvas: &mut [u8], cw: u32, rect: [u32; 4]) {
    let ch = (canvas.len() / 4 / cw.max(1) as usize) as u32;
    let Some((x0, y0, x1, y1)) = clip(cw, ch, rect) else {
        return;
    };
    for y in y0..y1 {
        let row = (y * cw as usize + x0) * 4..(y * cw as usize + x1) * 4;
        canvas[row].fill(0);
    }
}

/// Copies a frame's `w × h` RGBA into the canvas at `(x, y)`;
/// `keep_clear`: its fully transparent pixels leave the canvas as it is.
fn blit(canvas: &mut [u8], cw: u32, ch: u32, src: &[u8], rect: [u32; 4], keep_clear: bool) {
    let [_, _, w, _] = rect;
    let Some((x0, y0, x1, y1)) = clip(cw, ch, rect) else {
        return;
    };
    let (rx, ry) = (rect[0] as usize, rect[1] as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let s = ((y - ry) * w as usize + (x - rx)) * 4;
            let Some(p) = src.get(s..s + 4) else {
                return;
            };
            if keep_clear && p[3] == 0 {
                continue;
            }
            let d = (y * cw as usize + x) * 4;
            canvas[d..d + 4].copy_from_slice(p);
        }
    }
}

/// Composes a frame over the canvas (straight alpha, source over).
fn blend(canvas: &mut [u8], cw: u32, ch: u32, src: &[u8], rect: [u32; 4]) {
    let [_, _, w, _] = rect;
    let Some((x0, y0, x1, y1)) = clip(cw, ch, rect) else {
        return;
    };
    let (rx, ry) = (rect[0] as usize, rect[1] as usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let s = ((y - ry) * w as usize + (x - rx)) * 4;
            let Some(p) = src.get(s..s + 4) else {
                return;
            };
            let d = (y * cw as usize + x) * 4;
            let q = &mut canvas[d..d + 4];
            let (sa, da) = (p[3] as f32 / 255.0, q[3] as f32 / 255.0);
            let oa = sa + da * (1.0 - sa);
            if oa <= 0.0 {
                q.fill(0);
                continue;
            }
            for k in 0..3 {
                let c = (p[k] as f32 * sa + q[k] as f32 * da * (1.0 - sa)) / oa;
                q[k] = c.round().clamp(0.0, 255.0) as u8;
            }
            q[3] = (oa * 255.0).round() as u8;
        }
    }
}

/// The players of the images being drawn, by key (a frame's key with
/// `frame` 0): at most [`MAX_PLAYERS`], least recently used dropped.
#[derive(Debug, Default)]
pub struct Players {
    players: HashMap<ImageKey, Player>,
    tick: u64,
}

impl Players {
    /// The player of `key`'s image, made from the file at `read()` the
    /// first time.
    pub fn get(
        &mut self,
        key: &ImageKey,
        read: impl FnOnce() -> Result<(Bytes, Format), ImageError>,
    ) -> Result<&mut Player, ImageError> {
        let k = ImageKey {
            frame: 0,
            ..key.clone()
        };
        self.tick += 1;
        if !self.players.contains_key(&k) {
            // Another size of the same image shares its file.
            let shared = self
                .players
                .iter()
                .find(|(o, _)| o.source == k.source && o.icon == k.icon)
                .map(|(_, p)| (p.data.clone(), p.format));
            let (data, format) = match shared {
                Some(s) => s,
                None => read()?,
            };
            let p = Player::new(data, format)?;
            self.players.insert(k.clone(), p);
            // Within the count and the bytes, the least recently used
            // first; the new one stays.
            while self.players.len() > 1
                && (self.players.len() > MAX_PLAYERS || self.bytes() > MAX_PLAYER_BYTES)
            {
                let Some(old) = self
                    .players
                    .iter()
                    .filter(|(o, _)| **o != k)
                    .min_by_key(|(_, p)| p.used)
                    .map(|(o, _)| o.clone())
                else {
                    break;
                };
                self.players.remove(&old);
            }
        }
        let p = self
            .players
            .get_mut(&k)
            .ok_or_else(|| ImageError::Decode("no player".into()))?;
        p.used = self.tick;
        Ok(p)
    }

    /// Drops the players of images `keep` rejects (by their frame-0 key).
    pub fn retain(&mut self, mut keep: impl FnMut(&ImageKey) -> bool) {
        self.players.retain(|k, _| keep(k));
    }

    /// Bytes the players hold: each file once, however many players
    /// share it, and each player's [`Player::bytes`].
    pub fn bytes(&self) -> usize {
        let mut files: Vec<*const u8> = Vec::new();
        let mut n = 0;
        for p in self.players.values() {
            let ptr = p.data.as_ptr();
            if !files.contains(&ptr) {
                files.push(ptr);
                n += p.data.len();
            }
            n += p.bytes();
        }
        n
    }

    /// Players kept.
    pub fn len(&self) -> usize {
        self.players.len()
    }

    /// True with no player kept.
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_the_delays_and_loops() {
        let t = Timeline::new(vec![100, 50, 150], None).unwrap();
        assert_eq!(t.total, 300);
        assert_eq!(t.tick, Duration::from_millis(50));
        assert_eq!(t.frame_at(0.0), 0);
        assert_eq!(t.frame_at(0.099), 0);
        assert_eq!(t.frame_at(0.1), 1);
        assert_eq!(t.frame_at(0.15), 2);
        assert_eq!(t.frame_at(0.3), 0, "loops");
        assert!(!t.done(100.0));
        // Short delays play at 100 ms; one frame is no animation.
        let s = Timeline::new(vec![0, 10], Some(2)).unwrap();
        assert_eq!(s.delays, [100, 100]);
        assert_eq!(s.frame_at(0.15), 1);
        assert_eq!(s.frame_at(0.25), 0);
        assert!(s.done(0.4) && !s.done(0.39));
        assert_eq!(s.frame_at(5.0), 1, "the last frame stays");
        assert_eq!(Timeline::new(vec![100], None), None);
        // A loop count of 0 is forever.
        assert_eq!(Timeline::new(vec![30, 70], Some(0)).unwrap().loops, None);
        assert_eq!(
            Timeline::new(vec![30, 70], None).unwrap().tick,
            Duration::from_millis(10)
        );
    }

    #[test]
    fn the_next_change_is_the_next_frame_boundary() {
        let t = Timeline::new(vec![70, 80, 90], None).unwrap();
        assert_eq!(t.tick, Duration::from_millis(10));
        let ms = Duration::from_millis;
        assert_eq!(t.until_change(0.0), Some(ms(70)));
        assert_eq!(t.until_change(0.07), Some(ms(80)));
        assert_eq!(t.until_change(0.1), Some(ms(50)));
        assert_eq!(t.until_change(0.23), Some(ms(10)), "the loop's end");
        assert_eq!(t.until_change(0.24), Some(ms(70)), "looped");
        let once = Timeline::new(vec![100, 100], Some(1)).unwrap();
        assert_eq!(once.until_change(0.15), Some(ms(50)));
        assert_eq!(once.until_change(0.2), None, "played out");
    }

    #[test]
    fn disposal_and_blending_compose_the_canvas() {
        let mut c = vec![0u8; 4 * 4 * 4];
        blit(
            &mut c,
            4,
            4,
            &[255, 0, 0, 255].repeat(4),
            [1, 1, 2, 2],
            false,
        );
        assert_eq!(&c[(4 + 1) * 4..(4 + 1) * 4 + 4], &[255, 0, 0, 255]);
        // A transparent pixel kept clear leaves the red.
        let mut half = [0, 0, 255, 255].repeat(4);
        half[3] = 0;
        blit(&mut c, 4, 4, &half, [1, 1, 2, 2], true);
        assert_eq!(&c[(4 + 1) * 4..(4 + 1) * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(&c[(4 + 2) * 4..(4 + 2) * 4 + 4], &[0, 0, 255, 255]);
        // Half-transparent white over blue.
        blend(&mut c, 4, 4, &[255, 255, 255, 128], [2, 1, 1, 1]);
        assert_eq!(&c[(4 + 2) * 4..(4 + 2) * 4 + 4], &[128, 128, 255, 255]);
        clear_rect(&mut c, 4, [0, 0, 4, 2]);
        assert!(c[..32].iter().all(|v| *v == 0));
        // Off the canvas: nothing, no panic.
        blit(&mut c, 4, 4, &[1; 16], [3, 3, 2, 2], false);
        assert_eq!(&c[(3 * 4 + 3) * 4..], &[1, 1, 1, 1]);
        clear_rect(&mut c, 4, [9, 9, 2, 2]);
    }

    /// A two-frame GIF of `side × side`, both frames solid.
    fn big_gif(side: u16) -> Bytes {
        let palette = [255, 0, 0, 0, 0, 255];
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, side, side, &palette).unwrap();
            for i in 0..2u8 {
                let px = vec![i; side as usize * side as usize];
                let mut f = gif::Frame::from_palette_pixels(side, side, px, palette.to_vec(), None);
                f.delay = 10;
                enc.write_frame(&f).unwrap();
            }
        }
        Arc::from(out)
    }

    fn key(source: &str, w: u32) -> ImageKey {
        ImageKey {
            source: source.into(),
            icon: false,
            w,
            h: w,
            fit: crate::image::Fit::Contain,
            scale: 1,
            frame: 0,
        }
    }

    /// Players share a file across sizes, and stay within their bytes:
    /// the least recently used go, never the one being drawn.
    #[test]
    fn players_share_files_and_stay_within_their_bytes() {
        // 900 × 900: a 3.2 MB canvas, 6.5 MB with its decoder's frame;
        // two fit the budget, three do not.
        let file = big_gif(900);
        let each = 900 * 900 * 4 * 2;
        let mut players = Players::default();
        let mut reads = 0;
        for w in [48, 96] {
            let p = players
                .get(&key("/a.gif", w), || {
                    reads += 1;
                    Ok((file.clone(), Format::Gif))
                })
                .unwrap();
            p.frame(1).unwrap();
        }
        assert_eq!(reads, 1, "the second size shares the file");
        assert_eq!(players.len(), 2);
        assert_eq!(players.bytes(), file.len() + 2 * each);
        assert!(players.bytes() <= MAX_PLAYER_BYTES);
        // A third: the least recently used goes.
        players
            .get(&key("/b.gif", 48), || Ok((big_gif(900), Format::Gif)))
            .unwrap();
        assert_eq!(players.len(), 2);
        assert!(players.bytes() <= MAX_PLAYER_BYTES);
        assert!(!players.players.contains_key(&key("/a.gif", 48)));
        // One larger than the budget alone still plays, alone.
        let huge = big_gif(2048);
        let p = players
            .get(&key("/huge.gif", 48), || Ok((huge.clone(), Format::Gif)))
            .unwrap();
        assert_eq!(p.frame(1).unwrap().len(), 2048 * 2048 * 4);
        assert_eq!(players.len(), 1);
    }

    #[test]
    fn formats_are_told_by_their_bytes() {
        assert_eq!(format(b"GIF89a...."), Some(Format::Gif));
        assert_eq!(format(b"RIFF\0\0\0\0WEBPVP8X"), Some(Format::Webp));
        assert_eq!(format(b"\x89PNG\r\n\x1a\n"), None, "a still PNG");
        assert_eq!(format(b"\xff\xd8\xff"), None);
    }
}
