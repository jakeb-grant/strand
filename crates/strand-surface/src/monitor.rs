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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use strand_scene::Scale;

/// How long an unplugged monitor is remembered.
pub const MONITOR_RETENTION: Duration = Duration::from_secs(30);

/// A make or model as an identity writes it, trimmed: `Unknown` when
/// empty.
fn known(s: &str) -> &str {
    match s.trim() {
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
    /// (output-id-order) Each part is trimmed, and an empty description
    /// is left out with its separator (`Unknown | Unknown`), so no id
    /// ends in a space.
    pub fn new(make: &str, model: &str, description: &str) -> Self {
        let (make, model) = (known(make), known(model));
        Self(match description.trim() {
            "" => format!("{make} | {model}"),
            d => format!("{make} | {model} | {d}"),
        })
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

/// (output-id-order) Connector names in order, a run of digits by its
/// number (`DP-2` before `DP-10`): identical monitors are numbered in
/// this order. A copy of strand-services' `outputs::by_name`, which this
/// crate does not depend on; a test in crates/strand holds the two equal.
pub fn connector_order(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run = |s: &[u8]| s.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (run(a), run(b));
                let zeros = |s: &[u8]| s.iter().take_while(|c| **c == b'0').count();
                let (da, db) = (&a[zeros(&a[..na])..na], &b[zeros(&b[..nb])..nb]);
                let o = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
                if o != Ordering::Equal {
                    return o;
                }
                a = &a[na..];
                b = &b[nb..];
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

/// What is known about a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub id: MonitorId,
    /// The connector name (`DP-1`, `HEADLESS-2`), when the compositor gives
    /// one. Not part of the identity: connectors move between ports.
    /// Identical monitors are numbered as the `outputs` service numbers
    /// them, else by their connectors' names (` #2`;
    /// [`Monitors::renumbering`]).
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

impl Monitor {
    /// Its identity before numbering: make, model and description.
    fn base(&self) -> MonitorId {
        base(
            &self.make,
            &self.model,
            &self.description,
            self.connector.as_deref(),
        )
    }
}

fn base(make: &str, model: &str, description: &str, connector: Option<&str>) -> MonitorId {
    MonitorId::new(make, model, identity_description(description, connector))
}

/// (output-id-order) A monitor about to be plugged in, as
/// [`Monitors::renumbering`] counts it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Joining<'a> {
    pub make: &'a str,
    pub model: &'a str,
    pub description: &'a str,
    pub connector: Option<&'a str>,
}

/// (output-id-order) What [`Monitors::renumbering`] says: the id a
/// monitor plugged in takes, and the monitors plugged in whose id is now
/// another.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Renumbering {
    pub joining: Option<MonitorId>,
    /// `wl_output` global and its new id.
    pub renamed: Vec<(u32, MonitorId)>,
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
    /// description, which callers handle as unplug + plug), numbered as
    /// [`Monitors::renumbering`] numbers it. The monitors whose id that
    /// changes are the caller's to plug in again ([`Monitors::plug_as`]).
    #[cfg(test)]
    pub fn plug(
        &mut self,
        global: u32,
        make: &str,
        model: &str,
        description: &str,
        connector: Option<String>,
        now: Instant,
    ) -> Plugged {
        let joining = Joining {
            make,
            model,
            description,
            connector: connector.as_deref(),
        };
        let id = self
            .renumbering(Some(joining))
            .joining
            .unwrap_or_else(|| base(make, model, description, connector.as_deref()));
        self.plug_as(global, make, model, description, connector, id, now)
    }

    /// A `wl_output` global appeared, with the id `id` that
    /// [`Monitors::renumbering`] gave it.
    #[allow(clippy::too_many_arguments)]
    pub fn plug_as(
        &mut self,
        global: u32,
        make: &str,
        model: &str,
        description: &str,
        connector: Option<String>,
        id: MonitorId,
        now: Instant,
    ) -> Plugged {
        self.expire(now);
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
    /// (`SurfaceHost::output_ids`), which [`Monitors::renumbering`]
    /// numbers by.
    pub fn set_known(&mut self, known: Vec<(String, String)>) {
        self.known = known;
    }

    /// The list [`Monitors::set_known`] last set.
    pub fn known(&self) -> &[(String, String)] {
        &self.known
    }

    /// (output-id-order) The ids of the monitors plugged in, with
    /// `joining` plugged in too. Identical monitors (the same make, model
    /// and description) are numbered by their connectors' names
    /// ([`connector_order`]; one without a connector after them), the
    /// first plain, then ` #2`, ` #3`: an id depends only on which
    /// monitors there are, never on the order they came in, so a restart
    /// gives the same ids. With the outputs service's list
    /// ([`Monitors::set_known`]), which also names the monitors connected
    /// but off (which have no `wl_output`), its ids, so `Screen.id` is
    /// `Output.id`; while it does not yet name every monitor of an
    /// identity plugged in (one just plugged in), they are numbered by
    /// name among those plugged in and the ones the list names. A monitor
    /// plugged in or unplugged can renumber the others of its identity
    /// (`renamed`), as the outputs service does.
    pub fn renumbering(&self, joining: Option<Joining<'_>>) -> Renumbering {
        struct Member {
            global: Option<u32>,
            connector: Option<String>,
            base: MonitorId,
            current: Option<MonitorId>,
        }
        let mut members: Vec<Member> = self
            .outputs
            .iter()
            .filter_map(|(g, id)| {
                let r = self.records.get(id)?;
                Some(Member {
                    global: Some(*g),
                    connector: r.monitor.connector.clone(),
                    base: r.monitor.base(),
                    current: Some(id.clone()),
                })
            })
            .collect();
        if let Some(j) = joining {
            members.push(Member {
                global: None,
                connector: j.connector.map(str::to_owned),
                base: base(j.make, j.model, j.description, j.connector),
                current: None,
            });
        }
        let of_base = |id: &str, base: &MonitorId| {
            id == base.as_str()
                || id
                    .strip_prefix(base.as_str())
                    .and_then(|r| r.strip_prefix(" #"))
                    .and_then(|n| n.parse::<usize>().ok())
                    .is_some_and(|n| n >= 2)
        };
        let given = |c: &Option<String>| {
            let c = c.as_deref()?;
            self.known
                .iter()
                .find(|(k, _)| k == c)
                .map(|(_, id)| id.as_str())
        };
        let mut out = Renumbering::default();
        let mut bases: Vec<&MonitorId> = members.iter().map(|m| &m.base).collect();
        bases.sort();
        bases.dedup();
        for b in bases {
            let group: Vec<&Member> = members.iter().filter(|m| m.base == *b).collect();
            // The list's ids, when it names every one, each of this
            // identity, none twice.
            let named: Option<Vec<MonitorId>> = group
                .iter()
                .map(|m| {
                    given(&m.connector)
                        .filter(|id| of_base(id, b))
                        .map(|id| MonitorId(id.to_owned()))
                })
                .collect();
            let named = named.filter(|ids| {
                let mut sorted: Vec<&MonitorId> = ids.iter().collect();
                sorted.sort();
                sorted.windows(2).all(|w| w[0] != w[1])
            });
            let ids: Vec<MonitorId> = match named {
                Some(ids) => ids,
                None => {
                    // By name, among these and the connectors the list
                    // gives this identity (connected, off or not yet shown).
                    let mut keys: Vec<(Option<&str>, Option<u32>, Option<usize>)> = group
                        .iter()
                        .enumerate()
                        .map(|(i, m)| (m.connector.as_deref(), m.global, Some(i)))
                        .collect();
                    for (c, id) in &self.known {
                        if of_base(id, b)
                            && !group.iter().any(|m| m.connector.as_deref() == Some(c))
                        {
                            keys.push((Some(c.as_str()), None, None));
                        }
                    }
                    keys.sort_by(|a, b| match (a.0, b.0) {
                        (Some(x), Some(y)) => connector_order(x, y),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        // Plugged in before the one joining, by global.
                        (None, None) => match (a.1, b.1) {
                            (Some(x), Some(y)) => x.cmp(&y),
                            (Some(_), None) => Ordering::Less,
                            (None, Some(_)) => Ordering::Greater,
                            (None, None) => Ordering::Equal,
                        },
                    });
                    let mut ids = vec![b.clone(); group.len()];
                    for (rank, (_, _, i)) in keys.iter().enumerate() {
                        if let Some(i) = i {
                            ids[*i] = match rank {
                                0 => b.clone(),
                                r => b.nth(r + 1),
                            };
                        }
                    }
                    ids
                }
            };
            for (m, id) in group.iter().zip(ids) {
                match (m.global, &m.current) {
                    (None, _) => out.joining = Some(id),
                    (Some(g), Some(now)) if *now != id => out.renamed.push((g, id)),
                    _ => {}
                }
            }
        }
        out.renamed.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// (output-id-order, review round 2) What a plug or unplug renames:
    /// [`Monitors::renumbering`] with `joining` plugged in. With the
    /// outputs service's list, every monitor whose id the list now gives
    /// otherwise, as `State::poll` does. With no list (no run of
    /// `outputs` up) only the monitors of the identities this changes:
    /// the joining monitor's and `left`, the identity of a monitor
    /// unplugged or described anew. A list gone empty renames nothing at
    /// a poll, so a plug or unplug of another identity does not either:
    /// an identical monitor shown as ` #2` beside one that is off keeps
    /// its id until a monitor of its own identity comes or goes,
    /// whatever else is plugged in.
    pub fn renumbering_after(
        &self,
        joining: Option<Joining<'_>>,
        left: Option<&MonitorId>,
    ) -> Renumbering {
        let mut plan = self.renumbering(joining);
        if self.known.is_empty() {
            let joined = joining.map(|j| base(j.make, j.model, j.description, j.connector));
            plan.renamed.retain(|(global, _)| {
                self.base_of(*global)
                    .is_some_and(|b| Some(&b) == joined.as_ref() || Some(&b) == left)
            });
        }
        plan
    }

    /// (output-id-order) The identity before numbering of a plugged-in
    /// `wl_output` global: make, model and description.
    pub fn base_of(&self, global: u32) -> Option<MonitorId> {
        let id = self.outputs.get(&global)?;
        self.records.get(id).map(|r| r.monitor.base())
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

    /// Plugs `monitors` (global, connector) in one at a time as the
    /// manager does: each renumbering's renamed monitors plugged in
    /// again with their new ids. The ids by connector.
    fn plug_all(m: &mut Monitors, monitors: &[(u32, &str)], t: Instant) -> Vec<(String, String)> {
        for (g, c) in monitors {
            let plan = m.renumbering(Some(Joining {
                make: "",
                model: "",
                description: "",
                connector: Some(c),
            }));
            replug(m, &plan.renamed, t);
            let id = plan.joining.unwrap();
            m.plug_as(*g, "", "", "", Some((*c).into()), id, t);
        }
        ids_by_connector(m)
    }

    /// As the manager does: every renamed one unplugged first, then each
    /// plugged in again (one's new id may be another's old one).
    fn replug(m: &mut Monitors, renamed: &[(u32, MonitorId)], t: Instant) {
        let old: Vec<Monitor> = renamed
            .iter()
            .map(|(g, _)| m.unplug(*g, t).unwrap())
            .collect();
        for ((g, id), old) in renamed.iter().zip(old) {
            m.plug_as(
                *g,
                &old.make,
                &old.model,
                &old.description,
                old.connector,
                id.clone(),
                t,
            );
        }
    }

    fn ids_by_connector(m: &Monitors) -> Vec<(String, String)> {
        let mut all: Vec<(String, String)> = m
            .present()
            .map(|p| (p.connector.clone().unwrap_or_default(), p.id.to_string()))
            .collect();
        all.sort_by(|a, b| connector_order(&a.0, &b.0));
        all
    }

    /// (output-id-order) An id ends in no separator or space: an empty
    /// description is left out (Hyprland's headless outputs), a part is
    /// trimmed (a description `make model serial` with no serial).
    #[test]
    fn an_id_has_no_trailing_separator() {
        assert_eq!(MonitorId::new("", "", "").as_str(), "Unknown | Unknown");
        assert_eq!(MonitorId::new(" ", "", " ").as_str(), "Unknown | Unknown");
        assert_eq!(
            MonitorId::new(
                "LG Display",
                "0x07C7",
                identity_description("LG Display 0x07C7  (eDP-1)", Some("eDP-1"))
            )
            .as_str(),
            "LG Display | 0x07C7 | LG Display 0x07C7"
        );
        assert_eq!(
            MonitorId::new("LG Display", "0x07C7", "LG Display 0x07C7 ").as_str(),
            "LG Display | 0x07C7 | LG Display 0x07C7"
        );
        assert_eq!(
            MonitorId::new("", "", "").nth(2).as_str(),
            "Unknown | Unknown #2"
        );
    }

    /// (output-id-order, strand-issues 2026-11-08) Three identical
    /// monitors plugged in one at a time (eDP-1, then DP-6, then DP-2, as
    /// a running Strand sees them) are numbered as the three plugged in
    /// after a restart, in any order: by their connectors' names. Each
    /// plug renames the ones a monitor on a port named before theirs
    /// comes ahead of; unplugging one numbers the others without it.
    #[test]
    fn identical_monitors_are_numbered_by_name_whatever_their_order() {
        let t = Instant::now();
        let x = "Unknown | Unknown";
        let want: Vec<(String, String)> = vec![
            ("DP-2".into(), x.into()),
            ("DP-6".into(), format!("{x} #2")),
            ("eDP-1".into(), format!("{x} #3")),
        ];
        let mut running = Monitors::default();
        assert_eq!(
            plug_all(&mut running, &[(1, "eDP-1"), (2, "DP-6"), (3, "DP-2")], t),
            want
        );
        for order in [
            [(7, "DP-2"), (8, "DP-6"), (9, "eDP-1")],
            [(9, "eDP-1"), (8, "DP-6"), (7, "DP-2")],
            [(8, "DP-6"), (9, "eDP-1"), (7, "DP-2")],
        ] {
            let mut restarted = Monitors::default();
            assert_eq!(plug_all(&mut restarted, &order, t), want, "{order:?}");
        }
        // DP-2 unplugged: DP-6 is plain, eDP-1 ` #2`; back, as before.
        running.unplug(3, t);
        let plan = running.renumbering(None);
        assert_eq!(plan.renamed.len(), 2, "{plan:?}");
        replug(&mut running, &plan.renamed, t);
        assert_eq!(
            ids_by_connector(&running),
            [
                ("DP-6".into(), x.into()),
                ("eDP-1".into(), format!("{x} #2"))
            ]
        );
        assert_eq!(plug_all(&mut running, &[(3, "DP-2")], t), want);
        assert!(running.renumbering(None).renamed.is_empty());
    }

    /// (output-id, review round 3; output-id-order) With the outputs
    /// service's list, `Screen.id` is its `Output.id`, which counts the
    /// monitors connected but off: turning one of two identical monitors
    /// off or on (its `wl_output` going or coming, in either order)
    /// renames neither. A monitor the list does not yet name (just
    /// plugged in) is numbered by name among those plugged in and the
    /// ones the list names, and once the list names it, by the list. No
    /// list: by name among those plugged in.
    #[test]
    fn identical_monitors_take_the_outputs_numbers() {
        let t = Instant::now();
        let (x, x2) = ("A | B | C", "A | B | C #2");
        let list = |a: &str, b: &str| -> Vec<(String, String)> {
            vec![("DP-1".into(), a.into()), ("DP-2".into(), b.into())]
        };
        let plug = |m: &mut Monitors, g: u32, c: &str| {
            let plan = m.renumbering(Some(Joining {
                make: "A",
                model: "B",
                description: "C",
                connector: Some(c),
            }));
            replug(m, &plan.renamed, t);
            let id = plan.joining.unwrap();
            m.plug_as(g, "A", "B", "C", Some(c.into()), id.clone(), t);
            id.to_string()
        };
        // DP-1 off (`X`), DP-2 on (`X #2`): DP-2 shown alone is `X #2`.
        // The profile turns DP-1 on and DP-2 off, in either order.
        for remove_first in [true, false] {
            let mut m = Monitors::default();
            m.set_known(list(x, x2));
            assert_eq!(plug(&mut m, 2, "DP-2"), x2);
            if remove_first {
                m.unplug(2, t);
                assert!(m.renumbering(None).renamed.is_empty());
            }
            assert_eq!(plug(&mut m, 1, "DP-1"), x, "{remove_first}");
            m.unplug(2, t);
            assert!(m.renumbering(None).renamed.is_empty());
            assert_eq!(ids_by_connector(&m), [("DP-1".into(), x.into())]);
        }
        // Without the list DP-2 alone is `X`; the list coming (the
        // outputs service started, with DP-1 connected but off) renames it.
        let mut m = Monitors::default();
        assert_eq!(plug(&mut m, 2, "DP-2"), x);
        m.set_known(list(x, x2));
        let plan = m.renumbering(None);
        assert_eq!(plan.renamed, [(2, MonitorId(x2.into()))]);
        // Not in the list yet (just plugged in on DP-0): by name among
        // the list's DP-1 and DP-2 and itself, so the one shown on DP-2
        // goes to ` #3` until the list names DP-0.
        let mut m = Monitors::default();
        m.set_known(list(x, x2));
        assert_eq!(plug(&mut m, 2, "DP-2"), x2);
        assert_eq!(plug(&mut m, 5, "DP-0"), x);
        assert_eq!(
            ids_by_connector(&m),
            [
                ("DP-0".into(), x.into()),
                ("DP-2".into(), "A | B | C #3".into())
            ]
        );
        m.set_known(vec![
            ("DP-0".into(), x.into()),
            ("DP-1".into(), x2.into()),
            ("DP-2".into(), "A | B | C #3".into()),
        ]);
        assert!(m.renumbering(None).renamed.is_empty());
        // A number the list gives that is another identity's is not
        // taken; two monitors given one number are numbered by name.
        let mut m = Monitors::default();
        m.set_known(vec![("DP-1".to_string(), "Other | B | C".to_string())]);
        assert_eq!(plug(&mut m, 1, "DP-1"), x);
        let mut m = Monitors::default();
        m.set_known(list(x, x));
        assert_eq!(plug(&mut m, 2, "DP-2"), x);
        assert_eq!(plug(&mut m, 1, "DP-1"), x);
        assert_eq!(
            ids_by_connector(&m),
            [("DP-1".into(), x.into()), ("DP-2".into(), x2.into())]
        );
    }

    /// (output-id-order, review round 2) With no list from the outputs
    /// service (its run stopped), a plug or unplug renumbers the
    /// monitors of its own identity and no other, as a list gone empty
    /// renames nothing at a poll: DP-2 shown as ` #2` beside DP-1, off,
    /// keeps its id while a monitor of another identity comes and goes.
    /// One of its own identity coming, going or described anew numbers
    /// them by name among those shown.
    #[test]
    fn without_the_list_a_plug_renumbers_its_own_identity_alone() {
        let t = Instant::now();
        let (x, x2) = ("A | B | C", "A | B | C #2");
        let join = |make: &'static str, c: &'static str| Joining {
            make,
            model: "B",
            description: "C",
            connector: Some(c),
        };
        let plug = |m: &mut Monitors, g: u32, j: Joining<'_>, left: Option<&MonitorId>| {
            let plan = m.renumbering_after(Some(j), left);
            replug(m, &plan.renamed, t);
            let id = plan.joining.unwrap();
            m.plug_as(
                g,
                j.make,
                j.model,
                j.description,
                j.connector.map(str::to_owned),
                id.clone(),
                t,
            );
            (id.to_string(), plan.renamed)
        };
        let mut m = Monitors::default();
        m.set_known(vec![("DP-1".into(), x.into()), ("DP-2".into(), x2.into())]);
        assert_eq!(plug(&mut m, 2, join("A", "DP-2"), None).0, x2);
        // The run stopped: an empty list at the next plug or unplug.
        m.set_known(Vec::new());
        let (id, renamed) = plug(&mut m, 3, join("Z", "HDMI-A-1"), None);
        assert_eq!((id.as_str(), renamed), ("Z | B | C", vec![]));
        let gone = m.unplug(3, t).unwrap();
        let plan = m.renumbering_after(None, Some(&gone.base()));
        assert!(plan.renamed.is_empty(), "{plan:?}");
        assert_eq!(ids_by_connector(&m), [("DP-2".into(), x2.into())]);
        // Numbered without the list, every identity would be renamed.
        assert_eq!(m.renumbering(None).renamed, [(2, MonitorId(x.into()))]);
        // One of its own identity plugged in on DP-3: by name.
        let (id, renamed) = plug(&mut m, 4, join("A", "DP-3"), None);
        assert_eq!(id, x2);
        assert_eq!(renamed, [(2, MonitorId(x.into()))]);
        // DP-2 described anew (another make): DP-3 alone of `X`.
        let before = m.base_of(2).unwrap();
        let old = m.unplug(2, t).unwrap();
        assert_eq!(old.base(), before);
        let (id, renamed) = plug(&mut m, 2, join("Y", "DP-2"), Some(&before));
        assert_eq!(id, "Y | B | C");
        assert_eq!(renamed, [(4, MonitorId(x.into()))]);
        assert_eq!(
            ids_by_connector(&m),
            [
                ("DP-2".into(), "Y | B | C".into()),
                ("DP-3".into(), x.into())
            ]
        );
    }
}
