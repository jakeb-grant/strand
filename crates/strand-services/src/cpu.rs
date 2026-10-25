//! `cpu`: processor load from `/proc/stat`, temperature from hwmon or a
//! thermal zone.
//!
//! Sampled once a second ([`PERIOD`], as its schema says) only while a
//! reader is visible: with every reader hidden the service waits on its
//! messages and wakes nothing, unless a `graph` keeps history of it
//! (history): then it samples at the kept, coarser period ([`Ticker`]),
//! each sample the load since the one before. Its first value, at start,
//! is the load since boot, so a bar does not show 0 for a second.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::procfs::{self, Times};
use crate::{Cx, Msg, ServiceError, Store, Ticker, service};

/// The schema the `cpu` service serves.
pub const SCHEMA: &str = strand_services_schema::CPU;

/// How often it samples while visible.
pub const PERIOD: Duration = Duration::from_secs(1);

static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// How many samples every `cpu` service of this process took (tests: a
/// hidden reader stops the sampling).
pub fn samples() -> u64 {
    SAMPLES.load(Ordering::Relaxed)
}

static REBASES: AtomicU64 = AtomicU64::new(0);

/// How many fresh baselines every `cpu` service of this process took
/// (tests: sampling that resumes after a pause, or a reader shown, starts
/// from now, not from before the pause).
pub fn rebases() -> u64 {
    REBASES.load(Ordering::Relaxed)
}

/// See the module docs.
#[service(name = "cpu")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Cpu {
    /// Load over every core, 0 to 1: `graph cpu.usage`.
    pub usage: f64,
    /// The load of each core, 0 to 1.
    pub cores: Vec<f64>,
    /// The processor's temperature in degrees Celsius, when a sensor
    /// reports one.
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
            self.take(&text, cpu);
        }
        cpu.temperature = self
            .temperature
            .as_deref()
            .and_then(procfs::read_temperature);
    }

    /// `cpu`'s loads from `/proc/stat`'s `text`, since the last reading.
    fn take(&mut self, text: &str, cpu: &mut Cpu) {
        let (all, cores) = procfs::parse_stat(text);
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

    /// A fresh baseline, without reporting (sampling resumes after a
    /// pause: the load over the pause is not what anyone asked for).
    fn rebase(&mut self) {
        REBASES.fetch_add(1, Ordering::Relaxed);
        if let Ok(text) = std::fs::read_to_string("/proc/stat") {
            self.base(&text);
        }
    }

    /// The baseline from `/proc/stat`'s `text`.
    fn base(&mut self, text: &str) {
        let (all, cores) = procfs::parse_stat(text);
        if let Some(all) = all {
            self.all = all;
        }
        self.cores = cores;
    }
}

impl Cpu {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let mut sampler = Sampler::new();
        cx.update(|c| sampler.sample(c));
        cx.ready();
        let mut tick = Ticker::new(&cx, PERIOD);
        loop {
            tokio::select! {
                m = cx.recv() => {
                    let shown = match m {
                        None => return Ok(()),
                        Some(m) => matches!(m, Msg::Visible(true)),
                    };
                    // Sampling again after a pause, or shown: the load
                    // over the pause is not what anyone asked for.
                    if tick.follow(&cx) || shown {
                        sampler.rebase();
                        tick.reset();
                    }
                }
                _ = tick.tick() => {
                    if !cx.update(|c| sampler.sample(c)) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(busy: u64, idle: u64) -> String {
        format!("cpu  {busy} 0 0 {idle} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {idle} 0 0 0 0 0 0\n")
    }

    /// (history) The polled builtins' periods, as the schema crate
    /// tells the eval sandbox's stand-ins.
    #[test]
    fn the_polled_periods_are_the_schema_crates() {
        assert_eq!(
            strand_services_schema::POLLED,
            [
                ("cpu", PERIOD),
                ("memory", crate::memory::PERIOD),
                ("disk", crate::disk::PERIOD),
                ("network", crate::network::rates::PERIOD)
            ]
        );
    }

    /// (history, the review) A fresh baseline after a pause: the first
    /// sample after it is the load since then, not over the pause.
    #[test]
    fn a_sample_after_a_rebase_is_the_load_since_it() {
        let mut s = Sampler::new();
        let mut cpu = Cpu::default();
        s.take(&stat(100, 900), &mut cpu);
        // Paused: the machine ran flat out (900 busy of 1000).
        s.base(&stat(1000, 1000));
        // Then idle for 100.
        s.take(&stat(1000, 1100), &mut cpu);
        assert_eq!(cpu.usage, 0.0, "not the pause's load");
        assert_eq!(cpu.cores, [0.0]);
        // Without the baseline the pause's load would show.
        let mut t = Sampler::new();
        t.take(&stat(100, 900), &mut cpu);
        t.take(&stat(1000, 1100), &mut cpu);
        assert!(cpu.usage > 0.8, "{}", cpu.usage);
    }
}
