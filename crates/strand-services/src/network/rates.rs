//! (services-round) `network.rx` and `network.tx`: the bytes a second the
//! machine's network hardware receives and sends, from the kernel's
//! counters (`/proc/net/dev`), whichever daemon (or none) runs.
//!
//! Only interfaces backed by a device count (`/sys/class/net/<if>/device`:
//! Ethernet, Wi-Fi, a USB modem), so loopback, bridges, VPN tunnels and
//! containers' virtual links never count a byte twice. Sampled like `cpu`
//! ([`Ticker`]): once a second ([`PERIOD`]) while a visible reader reads
//! either field (they are stream fields), at the kept, coarser period
//! while a `graph` keeps the service's history and none does, and never
//! otherwise; each sample is the rate since the one before, the first
//! after a pause a fresh baseline (0 until the next).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::{Cx, Ticker};

use super::Network;

/// How often the rates are sampled while a visible reader reads them.
pub const PERIOD: Duration = Duration::from_secs(1);

static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// How many times every `network` service of this process read the
/// counters (tests: nobody reading the rates reads nothing).
pub fn samples() -> u64 {
    SAMPLES.load(Ordering::Relaxed)
}

static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Read the counters under `root` (its `proc/net/dev` and
/// `sys/class/net`) in runs started later; `None`: the machine's (tests).
pub fn configure_root(root: Option<PathBuf>) {
    *ROOT.lock().unwrap_or_else(|e| e.into_inner()) = root;
}

fn root() -> PathBuf {
    ROOT.lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Received and sent bytes by interface.
pub(crate) type Counters = BTreeMap<String, (u64, u64)>;

/// Each counted interface's received and sent bytes, from
/// `/proc/net/dev`'s `text`, keeping those `counted` says.
pub(crate) fn parse_dev(text: &str, counted: impl Fn(&str) -> bool) -> Counters {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(' ') || !counted(name) {
            continue;
        }
        let n: Vec<u64> = rest
            .split_ascii_whitespace()
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        // Receive: bytes packets errs drop fifo frame compressed
        // multicast; then transmit: bytes …
        if let (Some(rx), Some(tx)) = (n.first(), n.get(8)) {
            out.insert(name.to_string(), (*rx, *tx));
        }
    }
    out
}

/// The bytes received and sent from `before` to `now`, summed over the
/// interfaces in both: one that came or went counts from its next
/// reading, and a counter that went back (a driver reset) counts nothing.
pub(crate) fn moved(before: &Counters, now: &Counters) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for (name, (r, t)) in now {
        if let Some((r0, t0)) = before.get(name) {
            rx = rx.saturating_add(r.saturating_sub(*r0));
            tx = tx.saturating_add(t.saturating_sub(*t0));
        }
    }
    (rx, tx)
}

/// The rates' sampler; see the module docs.
#[derive(Debug)]
pub(crate) struct Rates {
    root: PathBuf,
    tick: Option<Ticker>,
    last: Option<(Instant, Counters)>,
    /// Bytes a second received, as last sampled.
    pub(crate) rx: f64,
    /// Bytes a second sent, as last sampled.
    pub(crate) tx: f64,
}

impl Rates {
    pub(crate) fn new() -> Rates {
        Rates {
            root: root(),
            tick: None,
            last: None,
            rx: 0.0,
            tx: 0.0,
        }
    }

    /// Something wants the rates: a visible reader reads one, or a graph
    /// keeps the service's history.
    fn wanted(cx: &Cx<Network>) -> bool {
        cx.watched("rx") || cx.watched("tx") || cx.kept().is_some()
    }

    /// Follow who wants the rates now (call after each message): sampling
    /// starts, changes period or stops. `true` when the values changed
    /// (they go back to 0 when sampling stops or starts afresh).
    pub(crate) fn sync(&mut self, cx: &Cx<Network>) -> bool {
        if !Self::wanted(cx) {
            self.tick = None;
            self.last = None;
            return self.zero();
        }
        let fresh = match &mut self.tick {
            None => {
                self.tick = Some(Ticker::new(cx, PERIOD));
                true
            }
            Some(t) => t.follow(cx),
        };
        if !fresh {
            return false;
        }
        // Sampling (again) after a pause: the bytes moved over it are not
        // what anyone asked for.
        self.last = Some((Instant::now(), self.read()));
        self.zero()
    }

    fn zero(&mut self) -> bool {
        let changed = self.rx != 0.0 || self.tx != 0.0;
        self.rx = 0.0;
        self.tx = 0.0;
        changed
    }

    /// The next sample is due; never while nothing wants the rates.
    pub(crate) async fn tick(&mut self) {
        match &mut self.tick {
            Some(t) => t.tick().await,
            None => std::future::pending().await,
        }
    }

    /// Read the counters: the rates since the last reading.
    pub(crate) fn sample(&mut self) {
        let now = Instant::now();
        let counters = self.read();
        if let Some((then, before)) = &self.last {
            let secs = now.duration_since(*then).as_secs_f64();
            if secs > 0.0 {
                let (rx, tx) = moved(before, &counters);
                self.rx = rx as f64 / secs;
                self.tx = tx as f64 / secs;
            }
        }
        self.last = Some((now, counters));
    }

    fn read(&self) -> Counters {
        SAMPLES.fetch_add(1, Ordering::Relaxed);
        let net = self.root.join("sys/class/net");
        std::fs::read_to_string(self.root.join("proc/net/dev"))
            .map(|text| parse_dev(&text, |name| hardware(&net, name)))
            .unwrap_or_default()
    }

    /// The rates as last sampled: received, sent.
    pub(crate) fn now(&self) -> (f64, f64) {
        (self.rx, self.tx)
    }
}

/// `name` is backed by a device (not loopback, a bridge, a tunnel or a
/// virtual link).
fn hardware(net: &Path, name: &str) -> bool {
    !name.contains('/') && net.join(name).join("device").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV: &str = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  900000    10    0    0    0     0          0         0   900000    10    0    0    0     0       0          0
  eth0:    1000     5    0    0    0     0          0         0     2000     6    0    0    0     0       0          0
wlan0:     300     1    0    0    0     0          0         0      400     2    0    0    0     0       0          0
";

    #[test]
    fn the_counters_are_read_per_interface() {
        let all = parse_dev(DEV, |_| true);
        assert_eq!(all.get("lo"), Some(&(900_000, 900_000)));
        assert_eq!(all.get("eth0"), Some(&(1000, 2000)));
        assert_eq!(
            all.get("wlan0"),
            Some(&(300, 400)),
            "no space after the name"
        );
        let some = parse_dev(DEV, |n| n != "lo");
        assert_eq!(some.len(), 2);
    }

    #[test]
    fn interfaces_that_come_go_or_reset_count_nothing() {
        let a = BTreeMap::from([
            ("eth0".to_string(), (100, 200)),
            ("usb0".to_string(), (5, 5)),
        ]);
        let b = BTreeMap::from([
            ("eth0".to_string(), (150, 260)),
            // A driver reset: its counters went back.
            ("usb0".to_string(), (1, 1)),
            // New since: from its next reading.
            ("wlan0".to_string(), (9_000, 9_000)),
        ]);
        assert_eq!(moved(&a, &b), (50, 60));
    }

    #[test]
    fn only_hardware_counts() {
        let dir = std::env::temp_dir().join(format!("strand-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("eth0/device")).unwrap();
        std::fs::create_dir_all(dir.join("veth1")).unwrap();
        assert!(hardware(&dir, "eth0"));
        assert!(!hardware(&dir, "veth1"));
        assert!(!hardware(&dir, "lo"));
        assert!(!hardware(&dir, "../eth0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
