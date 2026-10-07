//! Audio devices as typed values, and the change stream that publishes
//! them.
//!
//! [`AudioDevice`] mirrors the builtin schema's `AudioDevice` record field
//! for field (`id`, `name`, `description`, `volume`, `muted`, `icon`,
//! `default`) and holds nothing else. [`AudioState`] is the `audio`
//! service: the default `sink` and `source`, and every `sinks` and
//! `sources`.

use strand_core::keyed::{KeyedError, VecDiff, keyed_diff};

/// Whether a device plays or records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// An output (`media.class` `Audio/Sink`).
    Sink,
    /// An input (`media.class` `Audio/Source`).
    Source,
}

impl Direction {
    /// The direction of a node with this `media.class`, if it is an audio
    /// device. `Audio/Duplex` counts as a sink: its playback side is what a
    /// volume control moves.
    pub fn of_media_class(class: &str) -> Option<Self> {
        match class {
            "Audio/Sink" | "Audio/Duplex" => Some(Self::Sink),
            "Audio/Source" | "Audio/Source/Virtual" => Some(Self::Source),
            _ => None,
        }
    }
}

/// An audio sink or source: the schema's `AudioDevice`, field for field
/// and nothing else (so the store's `#[derive(Data)]` record is exactly
/// this). Whether it plays or records is the list it is in; its channel
/// count stays on the audio thread.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDevice {
    /// Its PipeWire global id.
    pub id: u32,
    /// Its node name (`node.name`), what PipeWire's `default` metadata
    /// names.
    pub name: String,
    /// A readable description (`node.description`, else `node.nick`, else
    /// the name).
    pub description: String,
    /// Volume on the perceptual (cubic) scale `pactl` and `wpctl` show:
    /// the cube root of the loudest channel's linear volume. 1 is 100 %;
    /// above 1 is amplified (another program set it so; our writes stop
    /// at 1).
    pub volume: f64,
    /// Muted.
    pub muted: bool,
    /// An icon name for its volume and mute state ([`icon`]).
    pub icon: String,
    /// It is the default device of its direction.
    pub default: bool,
}

impl AudioDevice {
    /// A device with nothing known yet but its identity (volume 1,
    /// unmuted).
    pub fn new(id: u32, name: impl Into<String>, direction: Direction) -> Self {
        let name = name.into();
        Self {
            id,
            description: name.clone(),
            name,
            volume: 1.0,
            muted: false,
            icon: icon(direction, 1.0, false).to_owned(),
            default: false,
        }
    }
}

/// An icon name for a device's volume and mute state (Adwaita's names):
/// `audio-volume-{muted,low,medium,high,overamplified}-symbolic` for a
/// sink, `microphone-sensitivity-{muted,low,medium,high}-symbolic` for a
/// source.
pub fn icon(direction: Direction, volume: f64, muted: bool) -> &'static str {
    let v = volume;
    match direction {
        Direction::Sink => {
            if muted || v <= 0.0 {
                "audio-volume-muted-symbolic"
            } else if v <= 1.0 / 3.0 {
                "audio-volume-low-symbolic"
            } else if v <= 2.0 / 3.0 {
                "audio-volume-medium-symbolic"
            } else if v <= 1.0 + VOLUME_EPSILON {
                "audio-volume-high-symbolic"
            } else {
                "audio-volume-overamplified-symbolic"
            }
        }
        Direction::Source => {
            if muted || v <= 0.0 {
                "microphone-sensitivity-muted-symbolic"
            } else if v <= 1.0 / 3.0 {
                "microphone-sensitivity-low-symbolic"
            } else if v <= 2.0 / 3.0 {
                "microphone-sensitivity-medium-symbolic"
            } else {
                "microphone-sensitivity-high-symbolic"
            }
        }
    }
}

/// How far from 1 a volume may be and still count as 100 % (float noise
/// of the cube and its root).
pub(crate) const VOLUME_EPSILON: f64 = 1e-6;

/// The perceptual volume of linear channel volumes: the cube root of the
/// loudest channel, as `pactl` and `wpctl` show it. `None` for no
/// channels.
pub fn perceptual(channel_volumes: &[f32]) -> Option<f64> {
    let max = channel_volumes
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v))))?;
    Some(f64::from(max.max(0.0)).cbrt())
}

/// The linear channel volume of a perceptual volume (its cube).
pub fn linear(volume: f64) -> f32 {
    let v = volume.max(0.0);
    (v * v * v) as f32
}

/// Everything the `audio` service shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioState {
    /// Every output, by id.
    pub sinks: Vec<AudioDevice>,
    /// Every input, by id.
    pub sources: Vec<AudioDevice>,
    /// Connected to PipeWire. While it is away (a restart), the last
    /// devices stay.
    pub connected: bool,
}

impl AudioState {
    /// The default output: `audio.sink`.
    pub fn sink(&self) -> Option<&AudioDevice> {
        self.sinks.iter().find(|d| d.default)
    }

    /// The default input: `audio.source`.
    pub fn source(&self) -> Option<&AudioDevice> {
        self.sources.iter().find(|d| d.default)
    }

    /// The device with `id`, of either direction.
    pub fn device(&self, id: u32) -> Option<&AudioDevice> {
        self.sinks.iter().chain(&self.sources).find(|d| d.id == id)
    }
}

/// Which device a peak meter follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LevelTarget {
    /// Whatever the default output is (its monitor), following it when the
    /// default changes.
    DefaultSink,
    /// Whatever the default input is.
    DefaultSource,
    /// One device by id (a sink's monitor, or a source).
    Device(u32),
}

/// One reading of a peak meter: the loudest sample of each channel in the
/// last processing cycle (linear, 0 to 1 for unclipped audio). A meter
/// whose device fell silent, went idle or left sends one quiet reading
/// (all zeros, or no peaks at all when it stopped), then nothing until
/// sound comes back.
#[derive(Clone, Debug, PartialEq)]
pub struct Levels {
    /// The meter.
    pub target: LevelTarget,
    /// The device it reads now.
    pub device: u32,
    /// One peak per channel.
    pub peaks: Vec<f32>,
}

impl Levels {
    /// The loudest channel.
    pub fn peak(&self) -> f32 {
        self.peaks.iter().copied().fold(0.0, f32::max)
    }
}

/// One change to what the service shows. A batch is what one PipeWire
/// event (or the first sync after connecting) changed.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioChange {
    /// Connected to PipeWire, or lost it (the devices stay until the new
    /// connection's state replaces them). Always in the first batch.
    Connected(bool),
    /// `audio.sinks`, keyed by id.
    Sinks(Vec<VecDiff<i64, AudioDevice>>),
    /// `audio.sources`, keyed by id.
    Sources(Vec<VecDiff<i64, AudioDevice>>),
    /// `audio.sink`: the default output, if any. `None` (no default, or
    /// no PipeWire yet) is shown as the schema's defaults for the record
    /// (`id` 0, empty texts, `volume` 0, `muted` and `default` false).
    Sink(Option<AudioDevice>),
    /// `audio.source`: the default input, if any (`None` as for `Sink`).
    Source(Option<AudioDevice>),
    /// A peak meter's reading (only while one is asked for).
    Levels(Levels),
}

fn items(devices: &[AudioDevice]) -> Vec<(i64, AudioDevice)> {
    devices
        .iter()
        .map(|d| (i64::from(d.id), d.clone()))
        .collect()
}

/// Turns successive [`AudioState`]s into the smallest [`AudioChange`]s.
#[derive(Debug, Default)]
pub struct Publisher {
    last: Option<AudioState>,
}

impl Publisher {
    /// A publisher that has sent nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last state published.
    pub fn state(&self) -> Option<&AudioState> {
        self.last.as_ref()
    }

    /// The changes from the last published state to `next` (the first
    /// call sends whole lists as `Reset`s and every field). Empty when
    /// nothing changed.
    pub fn publish(&mut self, next: AudioState) -> Vec<AudioChange> {
        let mut out = Vec::new();
        match &self.last {
            None => {
                out.push(AudioChange::Connected(next.connected));
                out.push(AudioChange::Sinks(vec![VecDiff::Reset {
                    items: items(&next.sinks),
                }]));
                out.push(AudioChange::Sources(vec![VecDiff::Reset {
                    items: items(&next.sources),
                }]));
                out.push(AudioChange::Sink(next.sink().cloned()));
                out.push(AudioChange::Source(next.source().cloned()));
            }
            Some(last) => {
                if last.connected != next.connected {
                    out.push(AudioChange::Connected(next.connected));
                }
                if last.sinks != next.sinks {
                    let d = keyed_diff(&items(&last.sinks), &items(&next.sinks));
                    if !d.is_empty() {
                        out.push(AudioChange::Sinks(d));
                    }
                }
                if last.sources != next.sources {
                    let d = keyed_diff(&items(&last.sources), &items(&next.sources));
                    if !d.is_empty() {
                        out.push(AudioChange::Sources(d));
                    }
                }
                if last.sink() != next.sink() {
                    out.push(AudioChange::Sink(next.sink().cloned()));
                }
                if last.source() != next.source() {
                    out.push(AudioChange::Source(next.source().cloned()));
                }
            }
        }
        self.last = Some(next);
        out
    }
}

/// The service's state rebuilt from its change stream: for tests, and for
/// the store that will hold it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mirror {
    /// Connected to PipeWire.
    pub connected: bool,
    /// `audio.sinks`.
    pub sinks: Vec<(i64, AudioDevice)>,
    /// `audio.sources`.
    pub sources: Vec<(i64, AudioDevice)>,
    /// `audio.sink`.
    pub sink: Option<AudioDevice>,
    /// `audio.source`.
    pub source: Option<AudioDevice>,
    /// The last reading of each meter.
    pub levels: Vec<Levels>,
}

impl Mirror {
    /// Applies one change. A diff that does not fit the mirror is an error
    /// (the stream is inconsistent).
    pub fn apply(&mut self, change: &AudioChange) -> Result<(), KeyedError> {
        match change {
            AudioChange::Connected(c) => self.connected = *c,
            AudioChange::Sinks(d) => {
                for diff in d {
                    diff.apply(&mut self.sinks)?;
                }
            }
            AudioChange::Sources(d) => {
                for diff in d {
                    diff.apply(&mut self.sources)?;
                }
            }
            AudioChange::Sink(d) => self.sink = d.clone(),
            AudioChange::Source(d) => self.source = d.clone(),
            AudioChange::Levels(l) => match self.levels.iter_mut().find(|m| m.target == l.target) {
                Some(m) => *m = l.clone(),
                None => self.levels.push(l.clone()),
            },
        }
        Ok(())
    }

    /// The sink named `name`.
    pub fn sink_named(&self, name: &str) -> Option<&AudioDevice> {
        self.sinks.iter().map(|(_, d)| d).find(|d| d.name == name)
    }

    /// The source named `name`.
    pub fn source_named(&self, name: &str) -> Option<&AudioDevice> {
        self.sources.iter().map(|(_, d)| d).find(|d| d.name == name)
    }

    /// The last reading of `target`'s meter.
    pub fn levels(&self, target: LevelTarget) -> Option<&Levels> {
        self.levels.iter().find(|l| l.target == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: u32, name: &str, dir: Direction, volume: f64) -> AudioDevice {
        AudioDevice {
            volume,
            icon: icon(dir, volume, false).to_owned(),
            ..AudioDevice::new(id, name, dir)
        }
    }

    #[test]
    fn volume_is_the_cube_root_of_the_loudest_channel() {
        // What `wpctl set-volume 0.5` writes and `wpctl get-volume` reads.
        assert_eq!(perceptual(&[0.125, 0.125]), Some(0.5));
        // Unbalanced channels: the loudest, as pactl and wpctl show it.
        assert_eq!(perceptual(&[0.125, 0.001]), Some(0.5));
        assert_eq!(perceptual(&[]), None);
        assert_eq!(perceptual(&[f32::NAN, 0.0]), Some(0.0));
        assert_eq!(perceptual(&[-1.0]), Some(0.0));
        assert_eq!(linear(0.5), 0.125);
        assert_eq!(linear(-0.5), 0.0);
        for v in [0.0, 0.05, 0.37, 0.5, 0.99, 1.0, 1.5] {
            let back = perceptual(&[linear(v)]).unwrap();
            assert!((back - v).abs() < 1e-6, "{v} -> {back}");
        }
    }

    #[test]
    fn icons_follow_volume_and_mute() {
        let sink = |v, m| icon(Direction::Sink, v, m);
        assert_eq!(sink(0.2, false), "audio-volume-low-symbolic");
        assert_eq!(sink(0.5, false), "audio-volume-medium-symbolic");
        assert_eq!(sink(1.0, false), "audio-volume-high-symbolic");
        assert_eq!(sink(1.2, false), "audio-volume-overamplified-symbolic");
        assert_eq!(sink(1.2, true), "audio-volume-muted-symbolic");
        assert_eq!(sink(0.0, false), "audio-volume-muted-symbolic");
        let source = |v, m| icon(Direction::Source, v, m);
        assert_eq!(source(0.9, false), "microphone-sensitivity-high-symbolic");
        assert_eq!(source(0.9, true), "microphone-sensitivity-muted-symbolic");
        assert_eq!(
            AudioDevice::new(1, "a", Direction::Sink).icon,
            "audio-volume-high-symbolic"
        );
    }

    #[test]
    fn media_classes() {
        assert_eq!(
            Direction::of_media_class("Audio/Sink"),
            Some(Direction::Sink)
        );
        assert_eq!(
            Direction::of_media_class("Audio/Source/Virtual"),
            Some(Direction::Source)
        );
        assert_eq!(Direction::of_media_class("Stream/Output/Audio"), None);
        assert_eq!(Direction::of_media_class("Video/Source"), None);
    }

    #[test]
    fn the_publisher_sends_the_smallest_changes_and_the_mirror_follows() {
        let mut p = Publisher::new();
        let mut m = Mirror::default();
        let mut s = AudioState {
            sinks: vec![
                AudioDevice {
                    default: true,
                    ..dev(30, "a", Direction::Sink, 0.5)
                },
                dev(31, "b", Direction::Sink, 1.0),
            ],
            sources: vec![dev(40, "mic", Direction::Source, 1.0)],
            connected: true,
        };
        let first = p.publish(s.clone());
        assert_eq!(first.len(), 5);
        assert_eq!(first[0], AudioChange::Connected(true));
        for c in &first {
            m.apply(c).unwrap();
        }
        assert_eq!(m.sink.as_ref().map(|d| d.id), Some(30));
        assert_eq!(m.source, None);
        assert!(p.publish(s.clone()).is_empty());

        // A volume change on the default sink: one update and the sink.
        s.sinks[0].volume = 0.25;
        let c = p.publish(s.clone());
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(
            matches!(&c[0], AudioChange::Sinks(d) if matches!(d[..], [VecDiff::Update { index: 0, .. }]))
        );
        assert!(matches!(&c[1], AudioChange::Sink(Some(d)) if d.volume == 0.25));
        for c in &c {
            m.apply(c).unwrap();
        }

        // The default moves to b; a source appears.
        s.sinks[0].default = false;
        s.sinks[1].default = true;
        s.sources[0].default = true;
        let c = p.publish(s.clone());
        for c in &c {
            m.apply(c).unwrap();
        }
        assert_eq!(m.sink.as_ref().map(|d| d.name.as_str()), Some("b"));
        assert_eq!(m.source.as_ref().map(|d| d.name.as_str()), Some("mic"));
        assert_eq!(m.sinks.len(), 2);
        assert_eq!(m.sink_named("a").map(|d| d.volume), Some(0.25));

        // A sink leaves; the connection drops (the devices stay).
        s.sinks.remove(0);
        s.connected = false;
        let c = p.publish(s.clone());
        assert_eq!(c[0], AudioChange::Connected(false));
        for c in &c {
            m.apply(c).unwrap();
        }
        assert_eq!(m.sinks.len(), 1);
        assert!(!m.connected);

        m.apply(&AudioChange::Levels(Levels {
            target: LevelTarget::DefaultSink,
            device: 31,
            peaks: vec![0.5, 0.25],
        }))
        .unwrap();
        assert_eq!(
            m.levels(LevelTarget::DefaultSink).map(Levels::peak),
            Some(0.5)
        );
    }
}
