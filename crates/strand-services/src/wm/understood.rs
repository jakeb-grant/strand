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
//!   has (Hyprland's Lua and classic dispatches; niri's and sway's one):
//!   the window and workspace actions would all fail. Such a refusal
//!   holds for that compositor version: a retry that finds the same
//!   version stays degraded ([`Refused`]).
//!
//! An event the adapter does not know (a new event name, a new change
//! kind) is ignored or re-read as before: it never counts. An event it
//! knows whose data has a new shape is followed by a re-read, whose
//! replies decide.

use std::io;

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

/// What the adapter could not understand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotUnderstood {
    /// For the diagnostic: `the reply to j/clients (…)`.
    pub what: String,
    /// It was an action's syntax, refused in every dialect.
    pub actions: bool,
}

impl SessionEnd {
    /// A reply or the event stream was not understood.
    pub(crate) fn reply(what: impl Into<String>) -> Self {
        Self::NotUnderstood(NotUnderstood {
            what: what.into(),
            actions: false,
        })
    }

    /// An action's syntax was refused in every dialect.
    pub(crate) fn actions(what: impl Into<String>) -> Self {
        Self::NotUnderstood(NotUnderstood {
            what: what.into(),
            actions: true,
        })
    }
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

/// The message a run loop sends when its session ended not understood
/// (`version`: what the compositor reports, read after the session):
/// [`super::AdapterMsg::Degraded`] with the diagnostic's text. A refusal
/// of actions is kept in `refused` for the next attempts.
pub(crate) fn degraded(
    compositor: &str,
    n: NotUnderstood,
    version: Option<String>,
    refused: &mut Option<Refused>,
) -> super::AdapterMsg {
    let text = message(compositor, version.as_deref(), &n.what);
    if n.actions {
        *refused = Some(Refused {
            version,
            what: n.what,
        });
    }
    super::AdapterMsg::Degraded(text)
}

/// Counts consecutive event-stream messages that are not events at all.
#[derive(Debug, Default)]
pub(crate) struct Strikes(u32);

impl Strikes {
    /// A message that was an event (known or not): the count starts over.
    pub(crate) fn event(&mut self) {
        self.0 = 0;
    }

    /// A message that was no event: `Err` once [`MAX_STRIKES`] came in a
    /// row.
    pub(crate) fn garbage(&mut self, compositor: &str, sample: &[u8]) -> Result<(), SessionEnd> {
        self.0 += 1;
        if self.0 >= MAX_STRIKES {
            return Err(SessionEnd::reply(format!(
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
                assert!(!n.actions);
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
}
