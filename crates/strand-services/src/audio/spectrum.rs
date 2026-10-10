//! (M4) The `spectrum` element's FFT (design.md: "FFT via realfft"; "stops
//! when audio is silent"), on the audio thread.
//!
//! A meter's data thread keeps the last [`RING`] samples of its stream,
//! mixed to mono, in a [`Ring`] of atomics (no allocation, no lock on the
//! realtime thread). When the loop sends a reading with sound it takes
//! the last [`FFT_SIZE`] of them, applies a Hann window, runs a real FFT
//! and folds the magnitudes into [`BANDS`] bands spaced evenly in pitch
//! from [`LOW_HZ`] to [`HIGH_HZ`]: each band is the loudest bin in it (or
//! the bin nearest its centre when it is narrower than a bin), in
//! decibels mapped from [`FLOOR_DB`] (0) to 0 dBFS (1). So a full-scale
//! sine reads 1 in its band, and silence or noise below the floor 0.
//! Renderers resample the bands to their own bar count.
//!
//! The FFT runs only for readings the meter sends: at most once per
//! frame (1/60 s), only while a reader is visible (the meter runs only
//! then), and never on silence (a meter sends nothing while its device
//! plays silence).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Samples a meter keeps (a power of two).
pub(crate) const RING: usize = 4096;

/// Samples one FFT reads: about 43 ms at 48 kHz, a bin every 23 Hz.
pub const FFT_SIZE: usize = 2048;

/// Bands a reading carries.
pub const BANDS: usize = 64;

/// The lowest band's lower edge, Hz.
pub const LOW_HZ: f32 = 40.0;

/// The highest band's upper edge, Hz.
pub const HIGH_HZ: f32 = 16_000.0;

/// The level that reads 0, dBFS.
pub const FLOOR_DB: f32 = -72.0;

/// The last [`RING`] mono samples of a stream, written by the data
/// thread, read by the loop. A read racing a write may see a few
/// samples of the next cycle: harmless for a picture of the sound.
pub(crate) struct Ring {
    samples: Box<[AtomicU32]>,
    /// Samples written so far (wrapping).
    written: AtomicU32,
}

impl Default for Ring {
    fn default() -> Self {
        Ring {
            samples: (0..RING).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU32::new(0),
        }
    }
}

impl Ring {
    /// Appends one cycle's interleaved little-endian `f32` frames of
    /// `channels` channels, each mixed to its mean (on the data thread).
    pub(crate) fn push(&self, bytes: &[u8], channels: usize) {
        if channels == 0 {
            return;
        }
        let mut at = self.written.load(Ordering::Relaxed);
        let mut n = 0u32;
        for f in bytes.chunks_exact(channels * 4) {
            let mut sum = 0.0f32;
            for s in f.chunks_exact(4) {
                let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                if v.is_finite() {
                    sum += v;
                }
            }
            let v = sum / channels as f32;
            self.samples[at as usize % RING].store(v.to_bits(), Ordering::Relaxed);
            at = at.wrapping_add(1);
            n += 1;
        }
        self.written.fetch_add(n, Ordering::Release);
    }

    /// The last `out.len()` samples (at most [`RING`]), oldest first;
    /// zeros before the first written.
    pub(crate) fn last(&self, out: &mut [f32]) {
        let written = self.written.load(Ordering::Acquire);
        let n = out.len().min(RING);
        let have = (written as usize).min(n);
        let pad = out.len() - have;
        out[..pad].fill(0.0);
        let start = written.wrapping_sub(have as u32);
        for (i, o) in out[pad..].iter_mut().enumerate() {
            let at = start.wrapping_add(i as u32) as usize % RING;
            *o = f32::from_bits(self.samples[at].load(Ordering::Relaxed));
        }
    }
}

/// A meter's FFT: its plan, window and buffers, made on the first
/// reading with sound and kept while the meter runs.
pub(crate) struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// The window's sum: a sine of amplitude `a` peaks at `a · sum / 2`.
    gain: f32,
}

impl Analyzer {
    pub(crate) fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / FFT_SIZE as f32;
                0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
            })
            .collect();
        let gain = window.iter().sum();
        Analyzer {
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
            gain,
        }
    }

    /// The bands of the ring's last [`FFT_SIZE`] samples at `rate` Hz.
    pub(crate) fn bands(&mut self, ring: &Ring, rate: u32) -> Vec<f32> {
        ring.last(&mut self.input);
        self.bands_of_input(rate)
    }

    /// The bands of `samples` (the last [`FFT_SIZE`], zero-padded before)
    /// at `rate` Hz.
    #[cfg(test)]
    pub(crate) fn bands_of(&mut self, samples: &[f32], rate: u32) -> Vec<f32> {
        let n = samples.len().min(FFT_SIZE);
        let pad = FFT_SIZE - n;
        self.input[..pad].fill(0.0);
        self.input[pad..].copy_from_slice(&samples[samples.len() - n..]);
        self.bands_of_input(rate)
    }

    fn bands_of_input(&mut self, rate: u32) -> Vec<f32> {
        for (s, w) in self.input.iter_mut().zip(&self.window) {
            *s *= w;
        }
        if self
            .fft
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
            .is_err()
        {
            return vec![0.0; BANDS];
        }
        let rate = if rate == 0 { 48_000 } else { rate } as f32;
        let bin_hz = rate / FFT_SIZE as f32;
        let last = self.output.len() - 1;
        let amp = |k: usize| self.output[k.min(last)].norm() * 2.0 / self.gain;
        let ratio = HIGH_HZ / LOW_HZ;
        (0..BANDS)
            .map(|b| {
                let lo = LOW_HZ * ratio.powf(b as f32 / BANDS as f32);
                let hi = LOW_HZ * ratio.powf((b + 1) as f32 / BANDS as f32);
                let k0 = (lo / bin_hz).ceil() as usize;
                let k1 = (hi / bin_hz).ceil() as usize;
                let a = if k0 < k1 {
                    (k0..k1.min(last + 1)).map(amp).fold(0.0, f32::max)
                } else {
                    amp(((lo * hi).sqrt() / bin_hz).round() as usize)
                };
                level(a)
            })
            .collect()
    }
}

/// An amplitude (1 is full scale) as a band's 0 to 1.
fn level(a: f32) -> f32 {
    if a.is_nan() || a <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * a.log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// The band `hz` falls in.
pub fn band_of(hz: f32) -> usize {
    let x = (hz / LOW_HZ).ln() / (HIGH_HZ / LOW_HZ).ln();
    ((x * BANDS as f32).floor().max(0.0) as usize).min(BANDS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn a_sine_lights_its_band() {
        let mut a = Analyzer::new();
        for hz in [100.0, 1000.0, 5000.0] {
            let bands = a.bands_of(&sine(hz, 0.5, 48_000, FFT_SIZE), 48_000);
            assert_eq!(bands.len(), BANDS);
            let (peak, v) = bands
                .iter()
                .enumerate()
                .fold((0, 0.0), |m, (i, v)| if *v > m.1 { (i, *v) } else { m });
            assert!(
                peak.abs_diff(band_of(hz)) <= 1,
                "{hz} Hz peaks in band {peak}, not {}",
                band_of(hz)
            );
            // −6 dBFS reads about 0.92.
            assert!((v - 0.92).abs() < 0.05, "{hz} Hz: {v}");
            // An octave and more away, far below.
            let far = band_of(hz * 4.0).min(BANDS - 1);
            assert!(bands[far] < 0.5, "{hz} Hz: band {far} at {}", bands[far]);
        }
        let silent = a.bands_of(&[0.0; FFT_SIZE], 48_000);
        assert!(silent.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn the_ring_keeps_the_last_samples_mixed_to_mono() {
        let r = Ring::default();
        let mut out = [9.0f32; 4];
        r.last(&mut out);
        assert_eq!(out, [0.0; 4], "nothing written yet");
        // Two channels: (1, 3) and (-1, -3) mix to 2 and -2.
        let bytes: Vec<u8> = [1.0f32, 3.0, -1.0, -3.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        r.push(&bytes, 2);
        r.last(&mut out);
        assert_eq!(out, [0.0, 0.0, 2.0, -2.0]);
        // Wrapping past the ring's size keeps the newest.
        let many: Vec<u8> = (0..RING + 3)
            .flat_map(|i| (i as f32).to_le_bytes())
            .collect();
        r.push(&many, 1);
        r.last(&mut out);
        let n = (RING + 3) as f32;
        assert_eq!(out, [n - 4.0, n - 3.0, n - 2.0, n - 1.0]);
        r.push(&[1, 2, 3], 1);
        r.push(&bytes, 0);
    }

    #[test]
    fn bands_cover_the_range_in_pitch() {
        assert_eq!(band_of(LOW_HZ), 0);
        assert_eq!(band_of(10.0), 0);
        assert_eq!(band_of(HIGH_HZ * 2.0), BANDS - 1);
        assert_eq!(band_of((LOW_HZ * HIGH_HZ).sqrt()), BANDS / 2);
    }
}
