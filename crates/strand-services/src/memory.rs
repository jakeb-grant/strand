//! `memory`: memory use from `/proc/meminfo` (`MemTotal` minus
//! `MemAvailable` is in use; what is available, free, the page cache and
//! swap, all read at once), sampled once a second ([`PERIOD`]) only
//! while a reader is visible, or at the kept, coarser period while none
//! is and a `graph` keeps history of it ([`Ticker`]; history).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::procfs;
use crate::{Cx, Msg, ServiceError, Store, Ticker, service};

/// The schema the `memory` service serves.
pub const SCHEMA: &str = strand_services_schema::MEMORY;

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
    /// Memory available to start new programs without swapping, in bytes
    /// (the kernel's estimate, `MemAvailable`): free memory plus the cache
    /// it can drop.
    pub available: f64,
    /// Memory holding nothing at all, in bytes (`MemFree`): far less than
    /// `available`, since the kernel fills spare memory with cache.
    pub free: f64,
    /// The page cache and buffers, in bytes (`Cached` plus `Buffers`):
    /// files kept in memory, most of it given back when programs need it.
    pub cached: f64,
    /// Swap in use, in bytes (`SwapTotal` less `SwapFree`).
    pub swap_used: f64,
    /// Swap configured, in bytes; 0 without swap.
    pub swap_total: f64,
}

fn sample(m: &mut Memory) {
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    let Some(info) = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .as_deref()
        .and_then(procfs::parse_meminfo)
    else {
        return;
    };
    take(m, &info);
}

/// `m` from `/proc/meminfo`'s numbers.
fn take(m: &mut Memory, info: &procfs::Meminfo) {
    m.total = info.total;
    m.used = (info.total - info.available).max(0.0);
    m.usage = if info.total > 0.0 {
        m.used / info.total
    } else {
        0.0
    };
    m.available = info.available;
    m.free = info.free;
    m.cached = info.cached;
    m.swap_total = info.swap_total;
    m.swap_used = (info.swap_total - info.swap_free).max(0.0);
}

impl Memory {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        cx.update(sample);
        cx.ready();
        let mut tick = Ticker::new(&cx, PERIOD);
        loop {
            tokio::select! {
                m = cx.recv() => {
                    let shown = match m {
                        None => return Ok(()),
                        Some(m) => matches!(m, Msg::Visible(true)),
                    };
                    tick.follow(&cx);
                    if shown {
                        // Back in view: show the current value at once.
                        if !cx.update(sample) {
                            return Ok(());
                        }
                        tick.reset();
                    }
                }
                _ = tick.tick() => {
                    if !cx.update(sample) {
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

    /// (services-round) Every field from one reading: used from what is
    /// available, swap in use from what is free of it.
    #[test]
    fn the_breakdown_is_taken_from_one_reading() {
        let info = procfs::parse_meminfo(
            "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 600 kB\n\
             Buffers: 50 kB\nCached: 250 kB\nSwapTotal: 400 kB\nSwapFree: 300 kB\n",
        )
        .unwrap();
        let mut m = Memory::default();
        take(&mut m, &info);
        let k = 1024.0;
        assert_eq!(m.total, 1000.0 * k);
        assert_eq!(m.used, 400.0 * k);
        assert_eq!(m.usage, 0.4);
        assert_eq!(m.available, 600.0 * k);
        assert_eq!(m.free, 100.0 * k);
        assert_eq!(m.cached, 300.0 * k);
        assert_eq!(m.swap_total, 400.0 * k);
        assert_eq!(m.swap_used, 100.0 * k);
    }
}
