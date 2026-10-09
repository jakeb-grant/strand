//! An adapter that is connected but cannot understand its compositor.
//!
//! Compositors change their IPC between releases (Hyprland 0.55 changed
//! how a dispatch is read; niri adds fields and events in patch
//! versions). A lost socket is an I/O error: the adapter reconnects with
//! backoff and the last state stays. A compositor that answers in a way
//! the adapter cannot read is different: its state would freeze or go
//! wrong. Then the session ends with [`SessionEnd::NotUnderstood`], and
//! the adapter reports [`super::AdapterMsg::Degraded`]: the service
//! serves windows and workspaces from the standard protocols, exactly as
//! when the adapter cannot connect, and the stores raise a
//! [`crate::ServiceDiagnostic`] that names the compositor, its version
//! when it tells, and what was not understood. The adapter retries on its
//! backoff ([`super::backoff`]), so a fixed or upgraded compositor
//! recovers without a restart; nothing else wakes while degraded.
//!
//! What counts (decisions.md, laptop-resilience):
//! - a reply to a state request that is not the JSON the adapter reads,
//!   or lacks a field it needs (an id, a name, an address): at once, as
//!   the state cannot be known without it;
//! - [`MAX_STRIKES`] consecutive event-stream messages that are not
//!   events at all (a niri line or sway payload that is not JSON, a
//!   Hyprland line without `>>`); each is followed by a re-read, so one
//!   costs only that;
//! - an action whose syntax the compositor refuses in every dialect it
//!   has (Hyprland's Lua and classic dispatches; niri's and sway's one),
//!   when the action is one every supported version has ([`tells_syntax`]:
//!   focus a workspace, focus or close a window): the window and workspace
//!   actions would all fail. Such a refusal holds for that compositor
//!   version: a retry that finds the same version stays degraded
//!   ([`Refused`]). The same answer to a later action (maximize,
//!   fullscreen; niri 25.08 has no `MaximizeWindowToEdges` and answers it
//!   `error parsing request`) is that action's: it is rejected, and the
//!   adapter stays.
//!
//! A retry must not flip the services between the two id spaces
//! ([`Degradation`]): after the event stream was not understood, the next
//! session comes up (sends its state) only once its stream has carried an
//! event; until then it reads and answers actions `NotConnected`. And a
//! session that never came up does not report its end again: the service
//! already has the reason, so the diagnostic is raised once per
//! degradation.
//!
//! An event the adapter does not know (a new event name, a new change
//! kind) is ignored or re-read as before: it never counts. An event it
//! knows whose data has a new shape is followed by a re-read, whose
//! replies decide.

use std::future::Future;
use std::io;

use tokio::sync::mpsc::UnboundedSender;

use super::backoff::Backoff;
use super::{AdapterMsg, IpcSnapshot, WmAction};

/// Consecutive event-stream messages that are not events at all before
/// the stream counts as not understood. One or two from a compositor that
/// changed something stay harmless (each is followed by a re-read); this
/// many in a row, with no event between, means the stream speaks
/// something else.
pub(crate) const MAX_STRIKES: u32 = 8;

/// How much of a reply a diagnostic quotes.
const EXCERPT: usize = 120;

/// Why a session ended.
#[derive(Debug)]
pub(crate) enum SessionEnd {
    /// The socket failed or closed: reconnect, keeping the last state.
    Io(io::Error),
    /// The compositor answered in a way the adapter cannot read.
    NotUnderstood(NotUnderstood),
}

impl From<io::Error> for SessionEnd {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::NotUnderstood(n) => write!(f, "not understood: {}", n.what),
        }
    }
}

/// Which part of the IPC was not understood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// A reply to a state request.
    Reply,
    /// The event stream ([`MAX_STRIKES`] messages that are no event).
    Stream,
    /// An action's syntax, refused in every dialect.
    Actions,
}

/// What the adapter could not understand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotUnderstood {
    /// For the diagnostic: `the reply to j/clients (…)`.
    pub what: String,
    pub part: Part,
}

impl SessionEnd {
    fn not_understood(what: impl Into<String>, part: Part) -> Self {
        Self::NotUnderstood(NotUnderstood {
            what: what.into(),
            part,
        })
    }

    /// A reply was not understood.
    pub(crate) fn reply(what: impl Into<String>) -> Self {
        Self::not_understood(what, Part::Reply)
    }

    /// The event stream was not understood.
    pub(crate) fn stream(what: impl Into<String>) -> Self {
        Self::not_understood(what, Part::Stream)
    }

    /// An action's syntax was refused in every dialect.
    pub(crate) fn actions(what: impl Into<String>) -> Self {
        Self::not_understood(what, Part::Actions)
    }
}

/// Whether a refusal of `action`'s syntax in every dialect is the
/// syntax's rather than the action's: it is one every supported version of
/// each compositor has (focus a workspace, focus or close a window). A
/// compositor that cannot parse a later one (maximize, fullscreen,
/// minimize) may just not have it yet: that action is rejected and the
/// adapter stays (decisions.md, laptop-resilience).
pub(crate) fn tells_syntax(action: &WmAction) -> bool {
    matches!(
        action,
        WmAction::FocusWorkspace(_) | WmAction::FocusWindow(_) | WmAction::CloseWindow(_)
    )
}

/// The first characters of `text`, on one line, for a diagnostic.
pub(crate) fn excerpt(text: &str) -> String {
    let one_line: String = text
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if one_line.chars().count() > EXCERPT {
        let mut cut: String = one_line.chars().take(EXCERPT).collect();
        cut.push('…');
        cut
    } else {
        one_line
    }
}

/// The diagnostic's text: the compositor, its version when known, what
/// was not understood, and what the services do meanwhile.
pub(crate) fn message(compositor: &str, version: Option<&str>, what: &str) -> String {
    let who = match version {
        Some(v) => format!("{compositor} {v}"),
        None => format!("{compositor} (version unknown)"),
    };
    format!(
        "the {compositor} IPC adapter does not understand {who}: {what}; windows and \
         workspaces come from the standard Wayland protocols until it does (retrying)"
    )
}

/// Actions refused in every dialect, on this compositor version: they
/// stay refused until the version changes (a retry cannot tell otherwise
/// without running an action).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
    pub version: Option<String>,
    pub what: String,
}

impl Refused {
    /// Whether a compositor reporting `now` is still the one that refused.
    /// An unknown version cannot be told apart, so it is given another try.
    pub(crate) fn holds_for(&self, now: Option<&str>) -> bool {
        self.version.is_some() && self.version.as_deref() == now
    }
}

/// What an adapter keeps between sessions about not understanding its
/// compositor, so that retries neither flip the services between the
/// adapter's and the protocols' ids nor raise the diagnostic again.
#[derive(Debug, Default)]
pub(crate) struct Degradation {
    /// Actions refused in every dialect, by this version.
    refused: Option<Refused>,
    /// The event stream was not understood: the next session comes up only
    /// once its stream carried an event.
    stream: bool,
    /// The service was told it is degraded, and no session came up since.
    reported: bool,
}

impl Degradation {
    /// Before a session comes up: `Err` while the compositor reports the
    /// version that refused the actions (`version` is read only then).
    pub(crate) async fn check_refusal(
        &mut self,
        version: impl Future<Output = Option<String>>,
    ) -> Result<(), SessionEnd> {
        let Some(r) = &self.refused else {
            return Ok(());
        };
        if r.holds_for(version.await.as_deref()) {
            return Err(SessionEnd::actions(r.what.clone()));
        }
        self.refused = None;
        Ok(())
    }

    /// Whether the session must see an event before it comes up.
    pub(crate) fn on_probation(&self) -> bool {
        self.stream
    }

    /// The session comes up: `Connected(true)` and its state. `false` when
    /// the service is gone.
    pub(crate) fn come_up(
        &mut self,
        tx: &UnboundedSender<AdapterMsg>,
        backoff: &mut Backoff,
        snapshot: IpcSnapshot,
    ) -> bool {
        self.stream = false;
        self.reported = false;
        backoff.connected();
        tx.send(AdapterMsg::Connected(true)).is_ok() && tx.send(AdapterMsg::State(snapshot)).is_ok()
    }

    /// A session ended not understood (`version`: what the compositor
    /// reports, read after it): [`AdapterMsg::Degraded`] with the
    /// diagnostic's text, or `None` when no session came up since the last
    /// one was sent (the service already has the reason).
    pub(crate) fn ended(
        &mut self,
        compositor: &str,
        n: NotUnderstood,
        version: Option<String>,
    ) -> Option<AdapterMsg> {
        let text = message(compositor, version.as_deref(), &n.what);
        match n.part {
            Part::Reply => {}
            Part::Stream => self.stream = true,
            Part::Actions => {
                self.refused = Some(Refused {
                    version,
                    what: n.what,
                });
            }
        }
        if std::mem::replace(&mut self.reported, true) {
            log::debug!("still degraded: {text}");
            return None;
        }
        Some(AdapterMsg::Degraded(text))
    }
}

/// Counts consecutive event-stream messages that are not events at all.
#[derive(Debug, Default)]
pub(crate) struct Strikes {
    in_a_row: u32,
    seen_event: bool,
}

impl Strikes {
    /// A message that was an event (known or not): the count starts over.
    pub(crate) fn event(&mut self) {
        self.in_a_row = 0;
        self.seen_event = true;
    }

    /// Whether the stream carried an event (known or not) yet.
    pub(crate) fn seen_event(&self) -> bool {
        self.seen_event
    }

    /// A message that was no event: `Err` once [`MAX_STRIKES`] came in a
    /// row.
    pub(crate) fn garbage(&mut self, compositor: &str, sample: &[u8]) -> Result<(), SessionEnd> {
        self.in_a_row += 1;
        if self.in_a_row >= MAX_STRIKES {
            return Err(SessionEnd::stream(format!(
                "its event stream ({MAX_STRIKES} messages in a row that are no {compositor} \
                 event, the last `{}`)",
                excerpt(&String::from_utf8_lossy(sample))
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_compositor_its_version_and_what() {
        let m = message("Hyprland", Some("0.57.0"), "the reply to j/clients (x)");
        assert!(
            m.contains("Hyprland 0.57.0: the reply to j/clients (x)"),
            "{m}"
        );
        assert!(m.contains("standard Wayland protocols"), "{m}");
        let m = message("niri", None, "y");
        assert!(m.contains("niri (version unknown): y"), "{m}");
    }

    #[test]
    fn excerpts_are_one_short_line() {
        assert_eq!(excerpt(" a\nb\t"), "a b");
        let long = "x".repeat(500);
        let e = excerpt(&long);
        assert_eq!(e.chars().count(), EXCERPT + 1);
        assert!(e.ends_with('…'));
    }

    #[test]
    fn strikes_count_only_in_a_row() {
        let mut s = Strikes::default();
        for _ in 0..MAX_STRIKES - 1 {
            assert!(s.garbage("niri", b"junk").is_ok());
        }
        s.event();
        for _ in 0..MAX_STRIKES - 1 {
            assert!(s.garbage("niri", b"junk").is_ok());
        }
        match s.garbage("niri", b"junk") {
            Err(SessionEnd::NotUnderstood(n)) => {
                assert_eq!(n.part, Part::Stream);
                assert!(n.what.contains("`junk`"), "{}", n.what);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_refusal_holds_for_its_version_only() {
        let r = Refused {
            version: Some("0.57.0".into()),
            what: "x".into(),
        };
        assert!(r.holds_for(Some("0.57.0")));
        assert!(!r.holds_for(Some("0.57.1")));
        assert!(!r.holds_for(None));
        let unknown = Refused {
            version: None,
            what: "x".into(),
        };
        assert!(!unknown.holds_for(None));
    }

    #[test]
    fn only_actions_every_version_has_tell_the_syntax() {
        assert!(tells_syntax(&WmAction::FocusWorkspace(1)));
        assert!(tells_syntax(&WmAction::FocusWindow("1".into())));
        assert!(tells_syntax(&WmAction::CloseWindow("1".into())));
        assert!(!tells_syntax(&WmAction::MaximizeWindow("1".into())));
        assert!(!tells_syntax(&WmAction::FullscreenWindow("1".into())));
        assert!(!tells_syntax(&WmAction::MinimizeWindow("1".into())));
    }

    fn end(part: Part) -> NotUnderstood {
        NotUnderstood {
            what: "x".into(),
            part,
        }
    }

    /// One `Degraded` per degradation: retries that never come up stay
    /// silent; a session that came up reports its next end again. A stream
    /// that was not understood puts the next session on probation, until
    /// one comes up.
    #[tokio::test]
    async fn a_degradation_is_reported_once_and_a_stream_one_waits_for_an_event() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut d = Degradation::default();
        assert!(!d.on_probation());
        let first = d.ended("niri", end(Part::Stream), Some("26.04".into()));
        assert!(matches!(first, Some(AdapterMsg::Degraded(t)) if t.contains("niri 26.04: x")));
        assert!(d.on_probation());
        assert!(d.ended("niri", end(Part::Stream), None).is_none());
        assert!(d.ended("niri", end(Part::Reply), None).is_none());
        assert!(d.on_probation(), "a reply failure keeps the probation");

        let mut backoff = Backoff::new();
        assert!(d.come_up(&tx, &mut backoff, IpcSnapshot::default()));
        assert!(matches!(rx.try_recv(), Ok(AdapterMsg::Connected(true))));
        assert!(matches!(rx.try_recv(), Ok(AdapterMsg::State(_))));
        assert!(!d.on_probation());
        assert!(d.ended("niri", end(Part::Reply), None).is_some());
        assert!(!d.on_probation(), "a reply failure needs no event");
    }

    #[tokio::test]
    async fn a_refusal_holds_across_retries_for_its_version() {
        let mut d = Degradation::default();
        assert!(d.check_refusal(async { None }).await.is_ok());
        assert!(
            d.ended("sway", end(Part::Actions), Some("1.9".into()))
                .is_some()
        );
        assert!(d.check_refusal(async { Some("1.9".into()) }).await.is_err());
        assert!(d.check_refusal(async { Some("1.10".into()) }).await.is_ok());
        assert!(d.check_refusal(async { Some("1.9".into()) }).await.is_ok());
    }
}
