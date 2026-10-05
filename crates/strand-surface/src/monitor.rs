//! Monitor identity and hotplug bookkeeping.
//!
//! `wl_output` exposes no EDID serial, so a monitor is identified by make,
//! model and description (design: "Monitors match on make, model and
//! description, and their state survives a 30-second unplug"). The
//! registry keeps unplugged monitors for [`MONITOR_RETENTION`] so a monitor
//! that comes back in time is recognised as the same one.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// How long an unplugged monitor is remembered.
pub const MONITOR_RETENTION: Duration = Duration::from_secs(30);

/// A monitor's stable identity: make, model and description.
///
/// The string form ([`MonitorId::as_str`]) is what `screens:` names in a
/// [`strand_scene::Screens::Named`] list.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(String);

impl MonitorId {
    /// `make`, `model` and `description` joined by `" | "`. A second
    /// monitor with the same three values gets `" #2"` appended (see
    /// `docs/decisions.md`).
    pub fn new(make: &str, model: &str, description: &str) -> Self {
        Self(format!("{make} | {model} | {description}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn nth(&self, n: usize) -> Self {
        Self(format!("{} #{n}", self.0))
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What is known about a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub id: MonitorId,
    /// The connector name (`DP-1`, `HEADLESS-2`), when the compositor gives
    /// one. Not part of the identity: connectors move between ports.
    pub connector: Option<String>,
    pub make: String,
    pub model: String,
    pub description: String,
}

#[derive(Debug)]
struct Record {
    monitor: Monitor,
    /// `None` while plugged in.
    unplugged_at: Option<Instant>,
}

/// Monitors seen this session, keyed by identity, plus which `wl_output`
/// global each plugged-in one is.
#[derive(Debug, Default)]
pub(crate) struct Monitors {
    records: HashMap<MonitorId, Record>,
    /// `wl_output` global name → identity, for plugged-in monitors.
    outputs: HashMap<u32, MonitorId>,
}

/// Result of [`Monitors::plug`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plugged {
    pub monitor: Monitor,
    /// The same monitor was unplugged less than [`MONITOR_RETENTION`] ago.
    pub reconnected: bool,
}

impl Monitors {
    /// A `wl_output` global appeared (or changed its make, model or
    /// description, which callers handle as unplug + plug).
    pub fn plug(
        &mut self,
        global: u32,
        make: &str,
        model: &str,
        description: &str,
        connector: Option<String>,
        now: Instant,
    ) -> Plugged {
        self.expire(now);
        let base = MonitorId::new(make, model, description);
        // Identical monitors plugged in at once get numbered; an unplugged
        // record with the identity is reclaimed first.
        let mut id = base.clone();
        let mut n = 1;
        while self
            .records
            .get(&id)
            .is_some_and(|r| r.unplugged_at.is_none())
        {
            n += 1;
            id = base.nth(n);
        }
        let monitor = Monitor {
            id: id.clone(),
            connector,
            make: make.to_owned(),
            model: model.to_owned(),
            description: description.to_owned(),
        };
        let reconnected = self.records.contains_key(&id);
        self.records.insert(
            id.clone(),
            Record {
                monitor: monitor.clone(),
                unplugged_at: None,
            },
        );
        self.outputs.insert(global, id);
        Plugged {
            monitor,
            reconnected,
        }
    }

    /// A `wl_output` global went away. Its record is kept for
    /// [`MONITOR_RETENTION`].
    pub fn unplug(&mut self, global: u32, now: Instant) -> Option<Monitor> {
        let id = self.outputs.remove(&global)?;
        let record = self.records.get_mut(&id)?;
        record.unplugged_at = Some(now);
        Some(record.monitor.clone())
    }

    /// Drops records unplugged for at least [`MONITOR_RETENTION`] and
    /// returns them.
    pub fn expire(&mut self, now: Instant) -> Vec<Monitor> {
        let gone: Vec<MonitorId> = self
            .records
            .iter()
            .filter(|(_, r)| {
                r.unplugged_at
                    .is_some_and(|t| now.saturating_duration_since(t) >= MONITOR_RETENTION)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut out: Vec<Monitor> = gone
            .iter()
            .filter_map(|id| self.records.remove(id).map(|r| r.monitor))
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// When the next unplugged record expires.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.records
            .values()
            .filter_map(|r| r.unplugged_at)
            .min()
            .map(|t| t + MONITOR_RETENTION)
    }

    /// The identity of a plugged-in `wl_output` global.
    pub fn id_of(&self, global: u32) -> Option<&MonitorId> {
        self.outputs.get(&global)
    }

    pub fn get(&self, id: &MonitorId) -> Option<&Monitor> {
        self.records.get(id).map(|r| &r.monitor)
    }

    /// Plugged-in monitors.
    pub fn present(&self) -> impl Iterator<Item = &Monitor> {
        self.records
            .values()
            .filter(|r| r.unplugged_at.is_none())
            .map(|r| &r.monitor)
    }

    /// Unplugged monitors still remembered.
    pub fn remembered(&self) -> impl Iterator<Item = &Monitor> {
        self.records
            .values()
            .filter(|r| r.unplugged_at.is_some())
            .map(|r| &r.monitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_make_model_description() {
        let mut m = Monitors::default();
        let t = Instant::now();
        let a = m.plug(
            7,
            "Dell",
            "U2720Q",
            "Dell U2720Q (DP-1)",
            Some("DP-1".into()),
            t,
        );
        assert_eq!(a.monitor.id.as_str(), "Dell | U2720Q | Dell U2720Q (DP-1)");
        assert!(!a.reconnected);
        assert_eq!(m.id_of(7), Some(&a.monitor.id));
        assert_eq!(m.present().count(), 1);
    }

    #[test]
    fn state_survives_a_short_unplug() {
        let mut m = Monitors::default();
        let t0 = Instant::now();
        let a = m.plug(1, "LG", "27GL850", "LG 27", None, t0);
        assert_eq!(m.unplug(1, t0).map(|x| x.id), Some(a.monitor.id.clone()));
        assert_eq!(m.present().count(), 0);
        assert_eq!(m.remembered().count(), 1);
        assert_eq!(m.next_expiry(), Some(t0 + MONITOR_RETENTION));
        // Replugged on another global within 30 s: recognised.
        let t1 = t0 + Duration::from_secs(29);
        assert!(m.expire(t1).is_empty());
        let b = m.plug(9, "LG", "27GL850", "LG 27", None, t1);
        assert!(b.reconnected);
        assert_eq!(b.monitor.id, a.monitor.id);
        assert_eq!(m.next_expiry(), None);
    }

    #[test]
    fn records_expire_after_30_seconds() {
        let mut m = Monitors::default();
        let t0 = Instant::now();
        m.plug(1, "A", "B", "C", None, t0);
        m.unplug(1, t0);
        let gone = m.expire(t0 + MONITOR_RETENTION);
        assert_eq!(gone.len(), 1);
        assert_eq!(m.remembered().count(), 0);
        let again = m.plug(2, "A", "B", "C", None, t0 + Duration::from_secs(31));
        assert!(!again.reconnected);
    }

    #[test]
    fn identical_monitors_are_numbered() {
        let mut m = Monitors::default();
        let t = Instant::now();
        let a = m.plug(1, "A", "B", "C", None, t);
        let b = m.plug(2, "A", "B", "C", None, t);
        assert_ne!(a.monitor.id, b.monitor.id);
        assert_eq!(b.monitor.id.as_str(), "A | B | C #2");
        // Unplugging the second and plugging it back reuses "#2".
        m.unplug(2, t);
        let c = m.plug(3, "A", "B", "C", None, t);
        assert_eq!(c.monitor.id, b.monitor.id);
        assert!(c.reconnected);
        assert_eq!(m.unplug(42, t), None);
    }
}
