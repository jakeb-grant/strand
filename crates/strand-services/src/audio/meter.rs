//! Peak meters: one passive capture stream per wanted [`LevelTarget`].
//!
//! A sink is metered through its monitor (`stream.capture.sink`), a
//! source directly. The stream is passive (`node.passive`): it never keeps
//! a device running, so a device with nothing playing suspends and its
//! meter goes quiet (one reading of zeros, then no wakeups). It does not
//! follow the session manager's moves (`node.dont-reconnect`): the thread
//! retargets it itself when the default changes.
//!
//! Its process callback runs on PipeWire's data thread (`RT_PROCESS`) and
//! only folds each cycle's peaks into [`Shared`] atomics: no allocation,
//! no lock, no queue. It wakes the `strand-pipewire` loop (an eventfd)
//! only on the first cycle with sound after the loop last read the meter;
//! while sound flows the loop reads it on the frame timer instead, so the
//! loop wakes at most once per [`FRAME`] per meter however short the
//! graph's cycle is (a client asking for low latency shrinks it to
//! ~1.3 ms), and not at all while the device plays silence.
//!
//! On a source the meter is passive too: it shows a level only while
//! something else records from it. It never opens a microphone by itself
//! (which would light the "microphone in use" indicators, the shell's own
//! included) nor keeps it awake.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use pipewire::core::CoreRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};

use super::model::{Direction, LevelTarget, Levels};
use super::spectrum::{Analyzer, Ring};
use super::thread::{FRAME, Queue, Work};

/// The most channels a meter reads (`SPA_AUDIO_MAX_CHANNELS`).
pub(crate) const MAX_CHANNELS: usize = 64;

/// The first wait before a failed meter is started again; it doubles per
/// consecutive failure, up to [`RETRY_MAX`].
pub(crate) const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait before a failed meter is started again.
pub(crate) const RETRY_MAX: Duration = Duration::from_secs(30);

/// How long a meter that failed `failures` times in a row waits before it
/// is started again.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_FIRST.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// What a meter's stream reports to the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeterEvent {
    /// The stream runs (its device is reached).
    Running,
    /// The device stopped (suspended, or nothing plays): no readings until
    /// it runs again.
    Idle,
    /// The stream failed or was disconnected (its device left).
    Failed,
}

/// The device a meter reads.
pub(crate) struct MeterTarget<'a> {
    pub id: u32,
    pub name: &'a str,
    pub serial: Option<&'a str>,
    pub direction: Direction,
}

/// What a meter's realtime process callback shares with the loop: the
/// loudest peak per channel and the number of cycles since the loop last
/// read them, and whether the loop is already due to read them.
pub(crate) struct Shared {
    channels: AtomicU32,
    /// `f32` bits: for the non-negative peaks stored here, the bits order
    /// as the numbers do, so `fetch_max` keeps the loudest.
    peaks: [AtomicU32; MAX_CHANNELS],
    cycles: AtomicU32,
    /// The loop will read the meter (it was woken, or reads it on the
    /// frame timer): the data thread does not wake it again.
    armed: AtomicBool,
    /// (M4) The last samples, mixed to mono, for the spectrum.
    ring: Ring,
    /// (M4) The stream's sample rate, Hz (0 until its format is known).
    rate: AtomicU32,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            channels: AtomicU32::new(0),
            peaks: std::array::from_fn(|_| AtomicU32::new(0)),
            cycles: AtomicU32::new(0),
            armed: AtomicBool::new(false),
            ring: Ring::default(),
            rate: AtomicU32::new(0),
        }
    }
}

impl Shared {
    /// One cycle's samples (interleaved little-endian `f32`), on the data
    /// thread. True when the loop should be woken: the cycle had sound and
    /// the loop was not already due to read the meter.
    pub(crate) fn cycle(&self, bytes: &[u8]) -> bool {
        let channels = (self.channels.load(Ordering::Relaxed) as usize).min(MAX_CHANNELS);
        let mut local = [0f32; MAX_CHANNELS];
        if !fold_peaks(bytes, &mut local[..channels]) {
            return false;
        }
        self.ring.push(bytes, channels);
        let mut sound = false;
        for (slot, p) in self.peaks.iter().zip(&local[..channels]) {
            if *p > 0.0 {
                sound = true;
                slot.fetch_max(p.to_bits(), Ordering::Relaxed);
            }
        }
        self.cycles.fetch_add(1, Ordering::Release);
        sound && !self.armed.swap(true, Ordering::AcqRel)
    }

    /// Takes what the cycles since the last take left: the loudest peak
    /// per channel, and how many cycles there were.
    pub(crate) fn take(&self) -> (Vec<f32>, u32) {
        let cycles = self.cycles.swap(0, Ordering::Acquire);
        let channels = (self.channels.load(Ordering::Relaxed) as usize).min(MAX_CHANNELS);
        let peaks = self.peaks[..channels]
            .iter()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
            .collect();
        (peaks, cycles)
    }

    fn set_channels(&self, channels: u32) {
        self.channels.store(channels, Ordering::Relaxed);
    }

    fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    pub(crate) fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }
}

/// A running meter.
pub(crate) struct Meter {
    /// Its number in this thread (stale events of a replaced meter carry an
    /// older one).
    pub id: u64,
    pub target: LevelTarget,
    pub device: u32,
    /// The stream failed; it is started again at `retry_at`.
    pub failed: bool,
    /// How many times in a row a meter on this target and device failed
    /// (reset once a stream runs).
    pub failures: u32,
    pub retry_at: Option<Instant>,
    /// Its readings.
    pub hold: Hold,
    /// While sound flows, when the loop reads it next (on the frame
    /// timer); `None` while the data thread wakes the loop instead.
    pub next_tick: Option<Instant>,
    /// (M4) Its FFT, made on the first reading with sound.
    analyzer: Option<Analyzer>,
    shared: Arc<Shared>,
    // Dropped after `Drop::drop` disconnected the stream, so the data
    // thread no longer runs the process callback.
    _events: StreamListener<()>,
    _process: StreamListener<()>,
    stream: StreamRc,
}

impl Drop for Meter {
    fn drop(&mut self) {
        // Takes the stream off the data loop (synchronously) before its
        // listeners go.
        let _ = self.stream.disconnect();
    }
}

impl Meter {
    /// Starts a meter on `device`. `wake` is the loop's eventfd; `failures`
    /// is how many meters on this target failed before it.
    pub(crate) fn start(
        core: &CoreRc,
        id: u64,
        target: LevelTarget,
        device: &MeterTarget<'_>,
        q: &Queue,
        wake: &Arc<OwnedFd>,
        failures: u32,
    ) -> Result<Meter, String> {
        let mut props = PropertiesBox::new();
        props.insert("media.type", "Audio");
        props.insert("media.category", "Capture");
        props.insert("media.name", "Strand peak meter");
        props.insert("node.name", "strand-levels");
        props.insert("application.name", "Strand");
        props.insert("node.passive", "true");
        props.insert("node.dont-reconnect", "true");
        props.insert("stream.dont-remix", "true");
        props.insert("target.object", device.serial.unwrap_or(device.name));
        if device.direction == Direction::Sink {
            props.insert("stream.capture.sink", "true");
        }
        let stream = StreamRc::new(core.clone(), "strand-levels", props)
            .map_err(|e| format!("cannot create a meter stream: {e}"))?;
        let shared = Arc::new(Shared::default());

        // Events on the loop: state and format.
        let q = q.clone();
        let format_shared = shared.clone();
        let events = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, _, _, new| {
                let ev = match new {
                    StreamState::Streaming => MeterEvent::Running,
                    StreamState::Paused => MeterEvent::Idle,
                    StreamState::Error(_) | StreamState::Unconnected => MeterEvent::Failed,
                    StreamState::Connecting => return,
                };
                q.borrow_mut().push_back(Work::Meter {
                    meter: id,
                    event: ev,
                });
            })
            .param_changed(move |_, _, param_id, param| {
                let Some(param) = param else { return };
                if param_id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Ok((MediaType::Audio, MediaSubtype::Raw)) = format_utils::parse_format(param)
                else {
                    return;
                };
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    format_shared.set_channels(info.channels());
                    format_shared.rate.store(info.rate(), Ordering::Relaxed);
                }
            })
            .register()
            .map_err(|e| format!("cannot listen to a meter stream: {e}"))?;
        // The process callback, on the data thread (`RT_PROCESS`): a
        // listener of its own, so it shares nothing with the one above
        // but the atomics and the eventfd.
        let (cycle_shared, wake) = (shared.clone(), wake.clone());
        let process = stream
            .add_local_listener_with_user_data(())
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data() else { return };
                let Some(bytes) = offset
                    .checked_add(size)
                    .and_then(|end| bytes.get(offset..end))
                else {
                    return;
                };
                if cycle_shared.cycle(bytes) {
                    let _ = rustix::io::write(&*wake, &1u64.to_ne_bytes());
                }
            })
            .register()
            .map_err(|e| format!("cannot listen to a meter stream: {e}"))?;

        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        let format = spa::pod::Value::Object(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        });
        let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &format)
            .map_err(|e| format!("cannot build a meter format: {e:?}"))?
            .0
            .into_inner();
        let pod = Pod::from_bytes(&bytes).ok_or("cannot build a meter format")?;
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
                &mut [pod],
            )
            .map_err(|e| format!("cannot connect a meter stream: {e}"))?;
        Ok(Meter {
            id,
            target,
            device: device.id,
            failed: false,
            failures,
            retry_at: None,
            hold: Hold::default(),
            next_tick: None,
            analyzer: None,
            shared,
            _events: events,
            _process: process,
            stream,
        })
    }

    /// The data thread woke the loop for this meter.
    pub(crate) fn woken(&self) -> bool {
        self.next_tick.is_none() && self.shared.armed()
    }

    /// Reads what the cycles left and returns the reading due now, if any.
    /// While sound flows (or a reading is held) the meter is read again on
    /// the frame timer (`next_tick`) and the data thread wakes nobody;
    /// after silence or a pause it wakes the loop at the next sound.
    pub(crate) fn collect(&mut self, now: Instant) -> Option<Levels> {
        self.shared.arm(false);
        let (peaks, cycles) = self.shared.take();
        let sound = peaks.iter().any(|p| *p > 0.0);
        if (cycles > 0 || sound) && !peaks.is_empty() {
            self.hold.add(peaks, now);
        }
        let out = if self.hold.due(now).is_some_and(|at| at <= now) {
            self.take(now)
        } else {
            None
        };
        if sound || self.hold.pending.is_some() {
            self.shared.arm(true);
            self.next_tick = Some(self.hold.due(now).unwrap_or(now + FRAME));
        } else {
            self.next_tick = None;
        }
        out
    }

    /// The device stopped (paused, failed) or the meter stops: what the
    /// cycles left is dropped, and the closing quiet reading is returned
    /// if the last reading showed sound.
    pub(crate) fn pause(&mut self) -> Option<Levels> {
        self.next_tick = None;
        self.shared.take();
        self.shared.arm(false);
        self.quiet()
    }
}

/// A meter's readings between the cycles and what is sent: at most one
/// reading per [`FRAME`] (the loudest peak per channel since the last),
/// and of silence only the first reading after sound.
#[derive(Debug)]
pub(crate) struct Hold {
    /// The last reading sent was all zeros or the closing quiet reading
    /// (or none was sent yet).
    pub silent: bool,
    /// The loudest peaks per channel since the last reading sent.
    pub pending: Option<Vec<f32>>,
    /// When the last reading went out.
    pub sent: Option<Instant>,
}

impl Default for Hold {
    fn default() -> Self {
        Hold {
            silent: true,
            pending: None,
            sent: None,
        }
    }
}

impl Hold {
    /// Holds one cycle's peaks. True if a reading is due now.
    pub(crate) fn add(&mut self, peaks: Vec<f32>, now: Instant) -> bool {
        let silent = peaks.iter().all(|p| *p == 0.0);
        match &mut self.pending {
            // Silence after silence: nothing to say.
            None if silent && self.silent => return false,
            Some(held) if held.len() == peaks.len() => {
                for (h, p) in held.iter_mut().zip(&peaks) {
                    *h = h.max(*p);
                }
            }
            held => *held = Some(peaks),
        }
        self.due(now).is_some_and(|at| at <= now)
    }

    /// When the held reading may go out (`now` if nothing was sent yet),
    /// if one is held.
    pub(crate) fn due(&self, now: Instant) -> Option<Instant> {
        self.pending.as_ref()?;
        Some(self.sent.map_or(now, |t| t + FRAME))
    }

    /// The held reading's peaks, to send now (`None` for silence after
    /// silence).
    pub(crate) fn take(&mut self, now: Instant) -> Option<Vec<f32>> {
        let peaks = self.pending.take()?;
        let silent = peaks.iter().all(|p| *p == 0.0);
        if silent && self.silent {
            return None;
        }
        self.silent = silent;
        self.sent = Some(now);
        Some(peaks)
    }

    /// The device stopped (paused, failed, or the meter stops): what was
    /// held is dropped. True if the closing quiet reading is due (the
    /// last reading sent showed sound).
    pub(crate) fn stop(&mut self) -> bool {
        self.pending = None;
        !std::mem::replace(&mut self.silent, true)
    }
}

impl Meter {
    /// The held reading, to send now, with the spectrum of the sound's
    /// last samples when it has sound.
    pub(crate) fn take(&mut self, now: Instant) -> Option<Levels> {
        let peaks = self.hold.take(now)?;
        let bins = if peaks.iter().any(|p| *p > 0.0) {
            let rate = self.shared.rate.load(Ordering::Relaxed);
            self.analyzer
                .get_or_insert_with(Analyzer::new)
                .bands(&self.shared.ring, rate)
        } else {
            Vec::new()
        };
        Some(Levels {
            target: self.target,
            device: self.device,
            peaks,
            bins,
        })
    }

    /// The closing quiet reading, if the last reading sent showed sound.
    pub(crate) fn quiet(&mut self) -> Option<Levels> {
        self.hold.stop().then(|| Levels {
            target: self.target,
            device: self.device,
            peaks: Vec::new(),
            bins: Vec::new(),
        })
    }
}

/// Folds the peak (largest absolute sample) of each channel of
/// interleaved little-endian `f32` audio into `out` (one slot per
/// channel). False without channels or whole frames.
pub(crate) fn fold_peaks(bytes: &[u8], out: &mut [f32]) -> bool {
    let channels = out.len();
    if channels == 0 || bytes.len() < channels * 4 {
        return false;
    }
    for f in bytes.chunks_exact(channels * 4) {
        for (o, s) in out.iter_mut().zip(f.chunks_exact(4)) {
            let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]).abs();
            // NaN never wins `max`.
            *o = o.max(v);
        }
    }
    true
}

/// [`fold_peaks`] into a new vector.
#[cfg(test)]
pub(crate) fn peaks(bytes: &[u8], channels: usize) -> Option<Vec<f32>> {
    let mut out = vec![0f32; channels];
    fold_peaks(bytes, &mut out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn peaks_per_channel() {
        let b = bytes(&[0.1, -0.5, -0.3, 0.2, 0.25, 0.0]);
        assert_eq!(peaks(&b, 2), Some(vec![0.3, 0.5]));
        assert_eq!(peaks(&b, 1), Some(vec![0.5]));
        assert_eq!(peaks(&b, 0), None);
        assert_eq!(peaks(&[], 2), None);
        // A trailing partial frame is ignored.
        let mut b = bytes(&[0.5, 0.25]);
        b.extend_from_slice(&[1, 2, 3]);
        assert_eq!(peaks(&b, 2), Some(vec![0.5, 0.25]));
        assert_eq!(peaks(&bytes(&[f32::NAN, 0.5]), 1), Some(vec![0.5]));
    }

    #[test]
    fn readings_are_held_to_one_per_frame_and_silence_is_said_once() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + std::time::Duration::from_millis(n);
        let mut h = Hold::default();
        // Silence before any sound: nothing.
        assert!(!h.add(vec![0.0, 0.0], ms(0)));
        assert_eq!(h.due(ms(0)), None);
        // The first sound goes out at once.
        assert!(h.add(vec![0.2, 0.1], ms(1)));
        assert_eq!(h.take(ms(1)), Some(vec![0.2, 0.1]));
        // Cycles within the frame are held, the loudest per channel kept.
        assert!(!h.add(vec![0.5, 0.0], ms(3)));
        assert!(!h.add(vec![0.1, 0.4], ms(5)));
        assert!(!h.add(vec![0.0, 0.0], ms(7)));
        assert_eq!(h.due(ms(7)), Some(ms(1) + FRAME));
        assert_eq!(h.take(ms(18)), Some(vec![0.5, 0.4]));
        // Silence: the first reading of it goes out, then nothing.
        assert!(h.add(vec![0.0, 0.0], ms(40)));
        assert_eq!(h.take(ms(40)), Some(vec![0.0, 0.0]));
        assert!(!h.add(vec![0.0, 0.0], ms(60)));
        assert_eq!(h.take(ms(60)), None);
        assert!(!h.stop(), "already quiet");
        // Sound, then the device stops: one closing quiet reading.
        assert!(h.add(vec![0.3, 0.3], ms(80)));
        assert_eq!(h.take(ms(80)), Some(vec![0.3, 0.3]));
        assert!(!h.add(vec![0.6, 0.6], ms(85)));
        assert!(h.stop(), "the quiet reading is due");
        assert_eq!(h.pending, None, "what was held is dropped");
        assert!(!h.stop());
        // A channel count change replaces what was held.
        assert!(h.add(vec![0.1], ms(200)));
        assert_eq!(h.take(ms(200)), Some(vec![0.1]));
        assert!(!h.add(vec![0.3], ms(201)));
        assert!(!h.add(vec![0.2, 0.2], ms(202)));
        assert_eq!(h.take(ms(220)), Some(vec![0.2, 0.2]));
    }

    #[test]
    fn the_data_thread_folds_cycles_and_wakes_once_per_read() {
        let s = Shared::default();
        s.set_channels(2);
        // Silence wakes nobody, but counts.
        assert!(!s.cycle(&bytes(&[0.0, 0.0, 0.0, 0.0])));
        // The first sound wakes the loop; more sound before it reads does
        // not wake it again.
        assert!(s.cycle(&bytes(&[0.2, -0.1])));
        assert!(!s.cycle(&bytes(&[-0.5, 0.05])));
        assert!(!s.cycle(&bytes(&[0.1, 0.3])));
        assert!(s.armed());
        assert_eq!(s.take(), (vec![0.5, 0.3], 4));
        assert_eq!(s.take(), (vec![0.0, 0.0], 0));
        // Still armed (the loop reads on its timer): no wake.
        assert!(!s.cycle(&bytes(&[0.4, 0.4])));
        s.arm(false);
        assert!(s.cycle(&bytes(&[0.4, 0.4])));
        // No channels known yet, or no whole frame: nothing.
        let t = Shared::default();
        assert!(!t.cycle(&bytes(&[0.5])));
        assert_eq!(t.take(), (vec![], 0));
        t.set_channels(2);
        assert!(!t.cycle(&bytes(&[0.5])));
        assert_eq!(t.take().1, 0);
        // Never past the slots there are.
        t.set_channels(1000);
        assert!(t.cycle(&bytes(&[0.5; MAX_CHANNELS])));
        assert_eq!(t.take().0.len(), MAX_CHANNELS);
    }

    #[test]
    fn a_failed_meter_waits_longer_each_time() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(3), Duration::from_secs(4));
        assert_eq!(retry_delay(6), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }
}
