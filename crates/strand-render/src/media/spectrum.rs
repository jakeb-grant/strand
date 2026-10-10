//! (M4) `spectrum audio.sink { bars: 48; smooth: 0.6; style: mirror }`
//! (design.md: "Audio spectrum … FFT via realfft"; "stops when audio is
//! silent").
//!
//! The FFT runs on the audio thread (`strand_services::audio::spectrum`);
//! its bands reach render through the binary ([`crate::Renderer::feed`])
//! only while the node is visible ([`crate::Renderer::take_feed_demand`]).
//! A spectrum is a CPU raster node with no clock: it repaints when fed,
//! so it does no work while the audio is silent (the audio thread sends
//! nothing then) or hidden (nothing is fed).
//!
//! The fed bands are resampled to `bars` (default 32): each bar the
//! loudest of the bands it covers. Each feed moves the bars `1 - smooth` of the
//! way to the new levels (`smooth` default 0.5); a feed with no bands
//! (the sound stopped) drops them to rest at once. Styles: `bars`
//! (pills up from the bottom, the default), `mirror` (pills about the
//! middle), `wave` (a filled smooth curve through the bar tops) and
//! `line` (that curve stroked). A bar at rest is a dot, so the node keeps
//! its shape in silence. Drawn in the node's `color`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use strand_scene::{Color, Prop, PropValue, TimeContext};
use vello_cpu::RenderContext;
use vello_cpu::color::{AlphaColor, PremulRgba8, Srgb};
use vello_cpu::kurbo::{BezPath, Point, RoundedRect, Shape, Stroke};

use super::graph::number;
use crate::clock::Rate;
use crate::offscreen::{RasterProps, RasterSource};

/// `bars` when none is set.
pub const DEFAULT_BARS: usize = 32;

/// The most bars drawn.
pub const MAX_BARS: usize = 256;

/// `smooth` when none is set.
pub const DEFAULT_SMOOTH: f32 = 0.5;

/// How a spectrum draws (`style:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Style {
    #[default]
    Bars,
    Mirror,
    Wave,
    Line,
}

impl Style {
    fn of(v: Option<&PropValue>) -> Style {
        match v {
            Some(PropValue::Keyword(k) | PropValue::Text(k)) => match k.as_str() {
                "mirror" => Style::Mirror,
                "wave" => Style::Wave,
                "line" => Style::Line,
                _ => Style::Bars,
            },
            _ => Style::Bars,
        }
    }
}

#[derive(Debug, Default)]
struct Spectrum {
    /// The bands as last fed.
    bands: Vec<f32>,
    /// The bars shown, eased toward the bands on each feed.
    shown: Vec<f32>,
    /// Feeds so far.
    feeds: u64,
    bars: usize,
    smooth: f32,
    style: Style,
    color: [f32; 4],
}

/// A `spectrum` node's source.
#[derive(Debug, Default)]
pub struct SpectrumSource {
    inner: Mutex<Spectrum>,
}

/// `bands` resampled to `n` bars: each bar the loudest band it covers
/// (a tone lights its bar fully whatever the bar count, as each band is
/// the loudest bin in it); with more bars than bands, each bar shows the
/// band it falls in.
pub fn resample(bands: &[f32], n: usize) -> Vec<f32> {
    if bands.is_empty() || n == 0 {
        return vec![0.0; n];
    }
    let m = bands.len();
    (0..n)
        .map(|i| {
            let a = i * m / n;
            let b = ((i + 1) * m).div_ceil(n).clamp(a + 1, m);
            bands[a..b]
                .iter()
                .fold(0.0f32, |acc, v| acc.max(v.clamp(0.0, 1.0)))
        })
        .collect()
}

impl SpectrumSource {
    /// New bands from the audio thread (empty: the sound stopped).
    pub fn feed(&self, bands: &[f32]) {
        let Ok(mut s) = self.inner.lock() else {
            return;
        };
        s.bands.clear();
        s.bands.extend_from_slice(bands);
        s.feeds += 1;
        let bars = if s.bars == 0 { DEFAULT_BARS } else { s.bars };
        let target = resample(bands, bars);
        let k = 1.0 - s.smooth;
        if bands.is_empty() || s.shown.len() != bars {
            s.shown = target;
        } else {
            for (v, t) in s.shown.iter_mut().zip(target) {
                *v += (t - *v) * k;
            }
        }
    }

    /// True when every bar is at rest (nothing fed, or silence fed).
    pub fn at_rest(&self) -> bool {
        self.inner
            .lock()
            .map(|s| s.shown.iter().all(|v| *v == 0.0))
            .unwrap_or(true)
    }

    /// The bars shown (tests).
    #[cfg(test)]
    pub fn shown(&self) -> Vec<f32> {
        self.inner
            .lock()
            .map(|s| s.shown.clone())
            .unwrap_or_default()
    }
}

fn smooth_curve(tops: &[Point]) -> BezPath {
    let mut p = BezPath::new();
    let Some(first) = tops.first() else {
        return p;
    };
    p.move_to(*first);
    for w in tops.windows(2) {
        let mid = Point::new((w[0].x + w[1].x) / 2.0, (w[0].y + w[1].y) / 2.0);
        p.quad_to(w[0], mid);
    }
    if let Some(last) = tops.last() {
        p.line_to(*last);
    }
    p
}

impl RasterSource for SpectrumSource {
    fn draw(&self, pixels: &mut [PremulRgba8], w: u32, h: u32, scale: f32, _time: TimeContext) {
        let Ok(s) = self.inner.lock() else {
            return;
        };
        let n = if s.bars == 0 { DEFAULT_BARS } else { s.bars };
        let mut bars = s.shown.clone();
        bars.resize(n, 0.0);
        let [r, g, b, a] = s.color;
        let style = s.style;
        drop(s);
        let (wf, hf) = (w as f64, h as f64);
        let dot = (2.0 * scale as f64).max(1.0).min(hf);
        let slot = wf / n as f64;
        let bw = (slot * 0.62).max(1.0);
        super::vector(pixels, w, h, |ctx: &mut RenderContext| {
            ctx.set_paint(AlphaColor::<Srgb>::new([r, g, b, a]));
            match style {
                Style::Bars | Style::Mirror => {
                    for (i, v) in bars.iter().enumerate() {
                        let x = i as f64 * slot + (slot - bw) / 2.0;
                        let bh = (*v as f64 * hf).max(dot);
                        let y = match style {
                            Style::Mirror => (hf - bh) / 2.0,
                            _ => hf - bh,
                        };
                        let rr = RoundedRect::new(x, y, x + bw, y + bh, bw.min(bh) / 2.0);
                        ctx.fill_path(&rr.to_path(0.1));
                    }
                }
                Style::Wave | Style::Line => {
                    let lw = (1.5 * scale as f64).max(1.0);
                    let tops: Vec<Point> = bars
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let y = hf - lw / 2.0 - (*v as f64) * (hf - lw);
                            Point::new((i as f64 + 0.5) * slot, y)
                        })
                        .collect();
                    let mut curve = smooth_curve(&tops);
                    if style == Style::Line {
                        ctx.set_stroke(Stroke::new(lw));
                        ctx.stroke_path(&curve);
                    } else {
                        curve.line_to(Point::new(wf - slot / 2.0, hf));
                        curve.line_to(Point::new(slot / 2.0, hf));
                        curve.close_path();
                        ctx.fill_path(&curve);
                    }
                }
            }
        });
    }

    fn rate(&self) -> Rate {
        Rate::Refresh
    }

    /// No clock: it repaints when fed.
    fn clock(&self) -> Option<Rate> {
        None
    }

    fn state(&self, props: &RasterProps<'_>) -> u64 {
        let Ok(mut s) = self.inner.lock() else {
            return 0;
        };
        let bars = number((props.get)(Prop::Bars)).map_or(DEFAULT_BARS, |b| {
            (b.round().max(1.0) as usize).min(MAX_BARS)
        });
        let smooth = number((props.get)(Prop::Smooth))
            .unwrap_or(DEFAULT_SMOOTH)
            .clamp(0.0, 0.99);
        s.style = Style::of((props.get)(Prop::Style));
        s.smooth = smooth;
        let c: Color = props.color;
        s.color = [c.r, c.g, c.b, c.a].map(|v| v.clamp(0.0, 1.0));
        if bars != s.bars {
            s.bars = bars;
            let target = resample(&s.bands, bars);
            s.shown = target;
        }
        let mut h = DefaultHasher::new();
        s.feeds.hash(&mut h);
        s.bars.hash(&mut h);
        s.style.hash(&mut h);
        for v in s.color {
            v.to_bits().hash(&mut h);
        }
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_resample_to_bars_by_their_loudest() {
        let bands: Vec<f32> = (0..64).map(|i| i as f32 / 63.0).collect();
        let four = resample(&bands, 4);
        assert_eq!(four.len(), 4);
        assert!(four.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(four[0], 15.0 / 63.0);
        // A tone in one band lights its bar fully.
        let mut tone = vec![0.0; 64];
        tone[29] = 0.9;
        let bars = resample(&tone, 16);
        assert_eq!(bars[7], 0.9);
        assert_eq!(bars.iter().filter(|v| **v > 0.0).count(), 1);
        // Bars that do not divide the bands: every band is in some bar.
        let mut lit = vec![0.0; 64];
        lit[63] = 1.0;
        assert_eq!(resample(&lit, 48)[47], 1.0);
        // More bars than bands: each band split evenly.
        let up = resample(&[0.0, 1.0], 4);
        assert_eq!(up, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(resample(&[], 3), [0.0; 3]);
    }

    #[test]
    fn feeds_ease_and_silence_rests() {
        let s = SpectrumSource::default();
        {
            let mut g = s.inner.lock().unwrap();
            (g.bars, g.smooth) = (2, 0.5);
        }
        s.feed(&[1.0, 1.0]);
        assert_eq!(s.shown(), [1.0, 1.0], "the first feed lands");
        s.feed(&[0.0, 0.0]);
        assert_eq!(s.shown(), [0.5, 0.5]);
        s.feed(&[]);
        assert_eq!(s.shown(), [0.0, 0.0], "silence rests at once");
    }
}
