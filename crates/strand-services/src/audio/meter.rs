//! Peak meters: one passive capture stream per wanted [`LevelTarget`].
//!
//! A sink is metered through its monitor (`stream.capture.sink`), a
//! source directly. The stream is passive (`node.passive`): it never keeps
//! a device running, so a device with nothing playing suspends and its
//! meter goes quiet (one reading of zeros, then no wakeups). It does not
//! follow the session manager's moves (`node.dont-reconnect`): the thread
//! retargets it itself when the default changes. Its process callback runs
//! on the `strand-pipewire` thread (no `RT_PROCESS`), which turns each
//! cycle into one reading.

use pipewire::core::CoreRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::param::format::{MediaSubtype, MediaType};
use pipewire::spa::param::format_utils;
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::stream::{StreamFlags, StreamListener, StreamRc, StreamState};

use super::model::{Direction, LevelTarget};
use super::thread::{Queue, Work};

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
    /// The last reading was all zeros (or none came yet).
    pub silent: bool,
    /// The stream failed; it is not restarted until its device changes.
    pub failed: bool,
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
            silent: true,
            failed: false,
            _listener: listener,
            _stream: stream,
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
}
