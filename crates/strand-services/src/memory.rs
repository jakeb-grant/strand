//! `memory`: memory use from `/proc/meminfo` (`MemTotal` minus
//! `MemAvailable` is in use), sampled once a second ([`PERIOD`]) only
//! while a reader is visible.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::procfs;
use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `memory` service serves.
pub const SCHEMA: &str = r#"
/// Memory use, sampled once a second while a reader is visible.
service memory {
  /// The fraction in use, 0 to 1.
  usage: float
  /// Memory in use, in bytes (installed memory less what is available).
  used: float
  /// Memory installed, in bytes.
  total: float
}
"#;

/// How often it samples while visible.
pub const PERIOD: Duration = Duration::from_secs(1);

static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// How many samples every `memory` service of this process took.
pub fn samples() -> u64 {
    SAMPLES.load(Ordering::Relaxed)
}

/// See the module docs.
#[service(name = "memory")]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Memory {
    /// The fraction in use, 0 to 1.
    pub usage: f64,
    /// Memory in use, in bytes (installed memory less what is available).
    pub used: f64,
    /// Memory installed, in bytes.
    pub total: f64,
}

fn sample(m: &mut Memory) {
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    let Some((total, available)) = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(procfs::parse_meminfo)
    else {
        return;
    };
    m.total = total;
    m.used = (total - available).max(0.0);
    m.usage = if total > 0.0 { m.used / total } else { 0.0 };
}

impl Memory {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        cx.update(sample);
        cx.ready();
        let mut tick = tokio::time::interval(PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.reset();
        loop {
            tokio::select! {
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Visible(true)) => {
                        // Back in view: show the current value at once.
                        if !cx.update(sample) {
                            return Ok(());
                        }
                        tick.reset();
                    }
                    Some(_) => {}
                },
                _ = tick.tick(), if cx.visible() => {
                    if !cx.update(sample) {
                        return Ok(());
                    }
                }
            }
        }
    }
}
