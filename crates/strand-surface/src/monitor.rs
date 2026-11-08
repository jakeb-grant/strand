//! Monitor identity and hotplug bookkeeping.
//!
//! `wl_output` exposes no EDID serial, so a monitor is identified by make,
//! model and description (design: "Monitors match on make, model and
//! description, and their state survives a 30-second unplug"). wlroots
//! appends the connector to the description (`"Dell Inc. DELL U2720Q XYZ
//! (DP-1)"`); that suffix is dropped from the identity so a monitor moved
//! to another port is still the same monitor (see [`identity_description`]).
//! The registry keeps unplugged monitors for [`MONITOR_RETENTION`] so a
//! monitor that comes back in time is recognised as the same one.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use strand_scene::Scale;

/// How long an unplugged monitor is remembered.
pub const MONITOR_RETENTION: Duration = Duration::from_secs(30);

/// A make or model as an identity writes it: `Unknown` when empty.
fn known(s: &str) -> &str {
    match s {
        "" => "Unknown",
        s => s,
    }
}

/// A monitor's stable identity: make, model and description.
///
/// The string form ([`MonitorId::as_str`]) is what `screens:` names in a
/// [`strand_scene::Screens::Named`] list.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(String);

impl MonitorId {
    /// `make`, `model` and `description` joined by `" | "` (pass the
    /// description through [`identity_description`] first). A second
    /// monitor with the same three values gets `" #2"` appended (see
    /// `docs/decisions.md`). An empty make or model is `Unknown`, as
    /// wlroots' `wl_output` says it; Hyprland's sends it empty, and the
    /// `outputs` service's `Output.id` writes it `Unknown` on every
    /// compositor (decisions.md output-id, review round 2).
    pub fn new(make: &str, model: &str, description: &str) -> Self {
        Self(format!(
            "{} | {} | {description}",
            known(make),
            known(model)
        ))
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

/// The part of a `wl_output` description that identifies the monitor: the
/// description without the output's own connector, which changes when the
/// same monitor is plugged into another port. wlroots ends it with
/// `" (<connector>)"`, or `" (<connector> via <sub>)"` when a DP
/// connector drives an HDMI or DVI sink; smithay (niri) with
/// `" - <connector>"` (`make - model - connector`). Anything else is kept
/// (review round 4 of output-id; strand-services' `output_id` has a copy).
pub fn identity_description<'a>(description: &'a str, connector: Option<&str>) -> &'a str {
    let Some(connector) = connector.filter(|c| !c.is_empty()) else {
        return description;
    };
    if let Some(inner) = description.strip_suffix(')')
        && let Some(open) = inner.rfind(" (")
    {
        let own = inner[open + 2..]
            .strip_prefix(connector)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(" via "));
        if own {
            return &description[..open];
        }
    }
    description
        .strip_suffix(connector)
        .and_then(|d| d.strip_suffix(" - "))
        .unwrap_or(description)
}

/// What is known about a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub id: MonitorId,
    /// The connector name (`DP-1`, `HEADLESS-2`), when the compositor gives
    /// one. Not part of the identity: connectors move between ports.
    /// Identical monitors are numbered as the `outputs` service numbers
    /// them, else in plug order (` #2`; [`Monitors::set_known`]), so which
    /// of two identical monitors is `#2` can change across restarts.
    pub connector: Option<String>,
    pub make: String,
    pub model: String,
    pub description: String,
    /// The output's scale (fractional when the compositor's mode and
    /// logical size give one, else its integer scale).
    pub scale: Scale,
    /// Size in logical pixels (xdg-output), when known.
    pub logical_size: Option<(i32, i32)>,
    /// Position in the compositor's logical layout (xdg-output), when
    /// known.
    pub position: Option<(i32, i32)>,
}

/// The part of a [`Monitor`] that changes while it stays plugged in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub scale: Scale,
    pub logical_size: Option<(i32, i32)>,
    pub position: Option<(i32, i32)>,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            scale: Scale::ONE,
            logical_size: None,
            position: None,
        }
    }
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
    /// (output-id) Connector → `Output.id` ([`Monitors::set_known`]).
    known: Vec<(String, String)>,
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
    /// description, which callers handle as unplug + plug). A second
    /// identical monitor takes the number the outputs service gave its
    /// connector ([`Monitors::set_known`]; decisions.md output-id, review
    /// round 3).
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
        let base = MonitorId::new(
            make,
            model,
            identity_description(description, connector.as_deref()),
        );
        let id = self.number(&base, connector.as_deref());
        let monitor = Monitor {
            id: id.clone(),
            connector,
            make: make.to_owned(),
            model: model.to_owned(),
            description: description.to_owned(),
            scale: Scale::ONE,
            logical_size: None,
            position: None,
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

    /// (output-id, review round 3) The output-management numbering,
    /// connector to `Output.id`, of every monitor connected, on or off
    /// (`SurfaceHost::output_ids`), which the next [`Monitors::plug`]
    /// numbers by.
    pub fn set_known(&mut self, known: Vec<(String, String)>) {
        self.known = known;
    }

    /// (output-id, review round 3) The id a monitor of identity `base`
    /// plugged in on `connector` takes. Its number is the one the
    /// outputs service gives that connector (`known`), so `Screen.id` is
    /// `Output.id` and turning one of two identical monitors on or off
    /// renumbers neither: a monitor off has no `wl_output`, so only the
    /// output-management list knows it is still connected and holds its
    /// number. Without one (no outputs service, or its list not yet
    /// updated for a monitor just plugged in), the lowest number no
    /// monitor plugged in holds and the list gives no other connector,
    /// which is how the outputs service numbers a monitor new to it.
    fn number(&self, base: &MonitorId, connector: Option<&str>) -> MonitorId {
        let known = &self.known;
        let plugged = |id: &MonitorId| {
            self.records
                .get(id)
                .is_some_and(|r| r.unplugged_at.is_none())
        };
        let of_base = |id: &str| {
            id == base.as_str()
                || id
                    .strip_prefix(base.as_str())
                    .and_then(|r| r.strip_prefix(" #"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .is_some_and(|n| n >= 2)
        };
        let given = connector.and_then(|c| known.iter().find(|(k, _)| k == c));
        if let Some((_, id)) = given
            && of_base(id)
        {
            let id = MonitorId(id.clone());
            if !plugged(&id) {
                return id;
            }
        }
        let others = |id: &MonitorId| {
            known
                .iter()
                .any(|(k, i)| Some(k.as_str()) != connector && i == id.as_str())
        };
        let mut id = base.clone();
        let mut n = 1;
        while plugged(&id) || others(&id) {
            n += 1;
            id = base.nth(n);
        }
        id
    }

    /// Records a plugged-in monitor's scale, logical size and position.
    /// Returns the updated monitor when any of them changed.
    pub fn set_geometry(&mut self, global: u32, g: Geometry) -> Option<Monitor> {
        let id = self.outputs.get(&global)?;
        let m = &mut self.records.get_mut(id)?.monitor;
        let old = Geometry {
            scale: m.scale,
            logical_size: m.logical_size,
            position: m.position,
        };
        if old == g {
            return None;
        }
        m.scale = g.scale;
        m.logical_size = g.logical_size;
        m.position = g.position;
        Some(m.clone())
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
        assert_eq!(a.monitor.id.as_str(), "Dell | U2720Q | Dell U2720Q");
        assert_eq!(a.monitor.description, "Dell U2720Q (DP-1)");
        assert!(!a.reconnected);
        assert_eq!(m.id_of(7), Some(&a.monitor.id));
        assert_eq!(m.present().count(), 1);
    }

    #[test]
    fn connector_suffix_is_not_identity() {
        assert_eq!(
            identity_description("Dell Inc. DELL U2720Q XYZ (DP-1)", Some("DP-1")),
            "Dell Inc. DELL U2720Q XYZ"
        );
        // Only the output's own connector, only as a suffix.
        assert_eq!(
            identity_description("Dell U2720Q (DP-1)", Some("DP-2")),
            "Dell U2720Q (DP-1)"
        );
        assert_eq!(
            identity_description("Panel (DP-1) rev 2", Some("DP-1")),
            "Panel (DP-1) rev 2"
        );
        assert_eq!(identity_description("eDP-1", Some("eDP-1")), "eDP-1");
        assert_eq!(identity_description("X (DP-1)", None), "X (DP-1)");
        // (review round 4) wlroots' DP connector driving an HDMI sink, and
        // smithay's (niri) `make - model - connector`.
        assert_eq!(
            identity_description("Dell Inc. DELL U2720Q XYZ (DP-5 via HDMI)", Some("DP-5")),
            "Dell Inc. DELL U2720Q XYZ"
        );
        assert_eq!(
            identity_description("Dell Inc. - DELL U2720Q - DP-3", Some("DP-3")),
            "Dell Inc. - DELL U2720Q"
        );
        assert_eq!(
            identity_description("Smithay - Winit - winit", Some("winit")),
            "Smithay - Winit"
        );
        // Another connector's, or not at the end: kept.
        assert_eq!(
            identity_description("X (DP-50 via HDMI)", Some("DP-5")),
            "X (DP-50 via HDMI)"
        );
        assert_eq!(identity_description("X - DP-30", Some("DP-3")), "X - DP-30");
        assert_eq!(identity_description("XDP-3", Some("DP-3")), "XDP-3");

        // The same monitor moved from DP-1 to DP-2 within 30 s is the same
        // monitor.
        let mut m = Monitors::default();
        let t = Instant::now();
        let a = m.plug(
            1,
            "Dell",
            "U2720Q",
            "Dell U2720Q (DP-1)",
            Some("DP-1".into()),
            t,
        );
        m.unplug(1, t);
        let b = m.plug(
            2,
            "Dell",
            "U2720Q",
            "Dell U2720Q (DP-2)",
            Some("DP-2".into()),
            t,
        );
        assert!(b.reconnected);
        assert_eq!(b.monitor.id, a.monitor.id);
        assert_eq!(b.monitor.connector.as_deref(), Some("DP-2"));
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
    fn geometry_updates_report_changes_only() {
        let mut m = Monitors::default();
        let t = Instant::now();
        m.plug(4, "A", "B", "C", None, t);
        let g = Geometry {
            scale: Scale::new(180).unwrap(),
            logical_size: Some((1280, 720)),
            position: Some((1920, 0)),
        };
        let changed = m.set_geometry(4, g).unwrap();
        assert_eq!(changed.scale, Scale::new(180).unwrap());
        assert_eq!(changed.logical_size, Some((1280, 720)));
        assert_eq!(changed.position, Some((1920, 0)));
        assert_eq!(m.set_geometry(4, g), None);
        assert_eq!(m.set_geometry(5, g), None);
        assert_eq!(m.present().next().unwrap(), &changed);
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

    /// (output-id, review round 3) Two identical monitors on DP-1 and
    /// DP-2 take the numbers the outputs service gives their connectors,
    /// on or off, so `Screen.id` is `Output.id` and turning one on while
    /// the other goes off (in either order of the two `wl_output`
    /// events) swaps nothing; one the list does not yet name takes the
    /// lowest number no monitor plugged in or connected holds.
    #[test]
    fn identical_monitors_take_the_outputs_numbers() {
        let t = Instant::now();
        let known = |a: &str, b: &str| -> Vec<(String, String)> {
            vec![("DP-1".into(), a.into()), ("DP-2".into(), b.into())]
        };
        let plug = |m: &mut Monitors, g: u32, c: &str, k: &[(String, String)]| {
            m.set_known(k.to_vec());
            m.plug(g, "A", "B", "C", Some(c.into()), t)
                .monitor
                .id
                .as_str()
                .to_string()
        };
        let (x, x2) = ("A | B | C", "A | B | C #2");
        // DP-2 on (`X`), DP-1 off (`X #2`); the profile turns DP-1 on and
        // DP-2 off: DP-1 is `X #2` whether DP-2's output goes first or not.
        for remove_first in [true, false] {
            let mut m = Monitors::default();
            let k = known(x2, x);
            assert_eq!(plug(&mut m, 2, "DP-2", &k), x);
            if remove_first {
                m.unplug(2, t);
            }
            assert_eq!(plug(&mut m, 1, "DP-1", &k), x2, "{remove_first}");
            m.unplug(2, t);
            // And back: DP-2 is `X` again, DP-1 still `X #2`.
            m.unplug(1, t);
            assert_eq!(plug(&mut m, 2, "DP-2", &k), x);
            assert_eq!(plug(&mut m, 1, "DP-1", &k), x2);
        }
        // Both off, then on in the other order: each its own number.
        let mut m = Monitors::default();
        let k = known(x, x2);
        assert_eq!(plug(&mut m, 2, "DP-2", &k), x2);
        assert_eq!(plug(&mut m, 1, "DP-1", &k), x);
        // Not in the list yet (just plugged in): not DP-1's `X`, which is
        // off but connected, nor a number plugged in.
        let mut m = Monitors::default();
        let only = vec![("DP-1".to_string(), x.to_string())];
        assert_eq!(plug(&mut m, 5, "DP-5", &only), x2);
        // No list (no outputs service): the lowest number plugged in.
        let mut m = Monitors::default();
        assert_eq!(plug(&mut m, 5, "DP-5", &[]), x);
        // A number the list gives that is another identity's, or held by
        // a monitor plugged in, is not taken.
        let mut m = Monitors::default();
        let wrong = vec![("DP-1".to_string(), "Other | B | C".to_string())];
        assert_eq!(plug(&mut m, 1, "DP-1", &wrong), x);
        let both = vec![("DP-2".to_string(), x.to_string())];
        assert_eq!(plug(&mut m, 2, "DP-2", &both), x2);
    }
}
