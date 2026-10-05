//! `STRAND_LOG`: a comma-separated list of a level (`error`, `warn`,
//! `info`, `debug`, `trace`; default `warn`) and topics (`damage`: one
//! stderr line per painted frame with its damage area, followed by a
//! `dropped` line if that frame's commit fails).

use log::{LevelFilter, Log, Metadata, Record};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogConfig {
    pub level: LevelFilter,
    pub damage: bool,
}

impl LogConfig {
    pub fn parse(spec: &str) -> Self {
        let mut config = Self {
            level: LevelFilter::Warn,
            damage: false,
        };
        for item in spec.split(',').map(str::trim) {
            match item {
                "damage" => config.damage = true,
                "off" => config.level = LevelFilter::Off,
                "error" => config.level = LevelFilter::Error,
                "warn" => config.level = LevelFilter::Warn,
                "info" => config.level = LevelFilter::Info,
                "debug" => config.level = LevelFilter::Debug,
                "trace" => config.level = LevelFilter::Trace,
                _ => {}
            }
        }
        config
    }

    pub fn from_env() -> Self {
        Self::parse(&std::env::var("STRAND_LOG").unwrap_or_default())
    }

    /// Install the stderr logger at this level.
    pub fn install(&self) {
        static LOGGER: Stderr = Stderr;
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(self.level);
        }
    }
}

struct Stderr;

impl Log for Stderr {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!(
                "strand: {} {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels_and_topics() {
        assert_eq!(
            LogConfig::parse(""),
            LogConfig {
                level: LevelFilter::Warn,
                damage: false
            }
        );
        assert_eq!(
            LogConfig::parse("damage, debug"),
            LogConfig {
                level: LevelFilter::Debug,
                damage: true
            }
        );
    }
}
