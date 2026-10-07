//! Peak meters: one passive capture stream per wanted [`LevelTarget`].
//!
//! A sink is metered through its monitor (`stream.capture.sink`), a
//! source directly. The stream is passive (`node.passive`): it never keeps
//! a device running, so a device with nothing playing suspends and its
//! meter goes quiet (one reading of zeros, then no wakeups). It does not
//! follow the session manager's moves (`node.dont-reconnect`): the thread
//! retargets it itself when the default changes. Its process callback runs
//! on the `strand-pipewire` thread (no `RT_PROCESS`), which turns each
//! cycle into a reading; the meter holds readings so that at most one per
//! [`FRAME`] goes out, the loudest peak per channel in between (a client
//! asking for low latency shrinks the graph's cycle to ~1.3 ms, and a
//! level shown on screen needs no more than the frame rate).
//!
//! On a source the meter is passive too: it shows a level only while
//! something else records from it. It never opens a microphone by itself
//! (which would light the "microphone in use" indicators, the shell's own
//! included) nor keeps it awake.

use pipewire::core::CoreRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};
use std::time::Instant;

use super::model::{Direction, LevelTarget, Levels};
use super::thread::{FRAME, Queue, Work};

/// What a meter's stream reports to the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeterEvent {
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

/// A running meter.
pub(crate) struct Meter {
    /// Its number in this thread (stale events of a replaced meter carry an
    /// older one).
    pub id: u64,
    pub target: LevelTarget,
    pub device: u32,
    /// The stream failed; the next look at the meters starts it anew.
    pub failed: bool,
    /// Its readings.
    pub hold: Hold,
    _listener: StreamListener<u32>,
    _stream: StreamRc,
}

impl Meter {
    /// Starts a meter on `device`.
    pub(crate) fn start(
        core: &CoreRc,
        id: u64,
        target: LevelTarget,
        device: &MeterTarget<'_>,
        q: &Queue,
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

        let (q1, q2) = (q.clone(), q.clone());
        let listener = stream
            // The user data is the negotiated channel count.
            .add_local_listener_with_user_data(0u32)
            .state_changed(move |_, _, _, new| {
                let ev = match new {
                    StreamState::Paused => MeterEvent::Idle,
                    StreamState::Error(_) | StreamState::Unconnected => MeterEvent::Failed,
                    StreamState::Connecting | StreamState::Streaming => return,
                };
                q1.borrow_mut().push_back(Work::Meter {
                    meter: id,
                    event: ev,
                });
            })
            .param_changed(|_, channels, param_id, param| {
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
                    *channels = info.channels();
                }
            })
            .process(move |stream, channels| {
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
                if let Some(peaks) = peaks(bytes, *channels as usize) {
                    q2.borrow_mut()
                        .push_back(Work::Reading { meter: id, peaks });
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
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut [pod],
            )
            .map_err(|e| format!("cannot connect a meter stream: {e}"))?;
        Ok(Meter {
            id,
            target,
            device: device.id,
            failed: false,
            hold: Hold::default(),
            _listener: listener,
            _stream: stream,
        })
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
    /// The held reading, to send now.
    pub(crate) fn take(&mut self, now: Instant) -> Option<Levels> {
        Some(Levels {
            target: self.target,
            device: self.device,
            peaks: self.hold.take(now)?,
        })
    }

    /// The closing quiet reading, if the last reading sent showed sound.
    pub(crate) fn quiet(&mut self) -> Option<Levels> {
        self.hold.stop().then(|| Levels {
            target: self.target,
            device: self.device,
            peaks: Vec::new(),
        })
    }
}

/// The peak (largest absolute sample) of each channel of interleaved
/// little-endian `f32` audio. `None` without channels or whole frames.
pub(crate) fn peaks(bytes: &[u8], channels: usize) -> Option<Vec<f32>> {
    if channels == 0 {
        return None;
    }
    let frame = channels * 4;
    let frames = bytes.len() / frame;
    if frames == 0 {
        return None;
    }
    let mut out = vec![0f32; channels];
    for f in bytes.chunks_exact(frame) {
        for (c, s) in f.chunks_exact(4).enumerate() {
            let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]).abs();
            // NaN never wins `max`.
            out[c] = out[c].max(v);
        }
    }
    Some(out)
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
}
