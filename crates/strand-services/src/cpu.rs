//! `cpu`: processor load from `/proc/stat`, temperature from hwmon or a
//! thermal zone.
//!
//! Sampled once a second ([`PERIOD`], as its schema says) only while a
//! reader is visible: with every reader hidden the service waits on its
//! messages and wakes nothing. Its first value, at start, is the load
//! since boot, so a bar does not show 0 for a second.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::procfs::{self, Times};
use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `cpu` service serves.
pub const SCHEMA: &str = r#"
/// Processor load, sampled once a second while a reader is visible.
service cpu {
  /// Load over every core, 0 to 1: `graph cpu.usage`.
  usage: float
  /// The load of each core, 0 to 1.
  cores: [float]
  /// The processor's temperature in degrees Celsius, when a sensor
  /// reports one.
  temperature: float?
}
"#;

/// How often it samples while visible.
pub const PERIOD: Duration = Duration::from_secs(1);

static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// How many samples every `cpu` service of this process took (tests: a
/// hidden reader stops the sampling).
pub fn samples() -> u64 {
    SAMPLES.load(Ordering::Relaxed)
}

/// See the module docs.
#[service(name = "cpu", schema = SCHEMA)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Cpu {
    /// Load over every core, 0 to 1.
    pub usage: f64,
    /// The load of each core, 0 to 1.
    pub cores: Vec<f64>,
    /// Degrees Celsius.
    pub temperature: Option<f64>,
}

/// The last reading, to take differences from.
struct Sampler {
    all: Times,
    cores: Vec<Times>,
    temperature: Option<std::path::PathBuf>,
}

impl Sampler {
    fn new() -> Sampler {
        Sampler {
            all: Times::default(),
            cores: Vec::new(),
            temperature: procfs::find_temperature(Path::new("/sys")),
        }
    }

    /// Read `/proc/stat` and fill `cpu` with the load since the last
    /// reading (since boot for the first).
    fn sample(&mut self, cpu: &mut Cpu) {
        SAMPLES.fetch_add(1, Ordering::Relaxed);
        if let Ok(text) = std::fs::read_to_string("/proc/stat") {
            let (all, cores) = procfs::parse_stat(&text);
            if let Some(all) = all {
                cpu.usage = procfs::usage(self.all, all);
                self.all = all;
            }
            cpu.cores = cores
                .iter()
                .enumerate()
                .map(|(i, c)| procfs::usage(self.cores.get(i).copied().unwrap_or_default(), *c))
                .collect();
            self.cores = cores;
        }
        cpu.temperature = self
            .temperature
            .as_deref()
            .and_then(procfs::read_temperature);
    }

    /// A fresh baseline, without reporting (sampling resumes after a
    /// pause: the load over the pause is not what anyone asked for).
    fn rebase(&mut self) {
        if let Ok(text) = std::fs::read_to_string("/proc/stat") {
            let (all, cores) = procfs::parse_stat(&text);
            if let Some(all) = all {
                self.all = all;
            }
            self.cores = cores;
        }
    }
}

impl Cpu {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let mut sampler = Sampler::new();
        cx.update(|c| sampler.sample(c));
        cx.ready();
        let mut tick = tokio::time::interval(PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.reset();
        loop {
            tokio::select! {
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(true)) => {
                        sampler.rebase();
                        tick.reset();
                    }
                    Some(_) => {}
                },
                _ = tick.tick(), if cx.visible() => {
                    if !cx.update(|c| sampler.sample(c)) {
                        return Ok(());
                    }
                }
            }
        }
    }
}
