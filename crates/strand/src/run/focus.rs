//! (screens-focus) `screens.focused` and each `Screen.focused`: the
//! monitor the compositor says has the keyboard focus (the compositor
//! service's `WmChange::FocusedScreen`, a connector name), matched to
//! the monitors main reports by `Screen.name`; the first monitor while
//! that is unknown (no compositor adapter, nothing heard from it yet, a
//! name no monitor has). Main hears which monitor the compositor named
//! ([`ToMain::FocusedScreen`]) and places `screens: focused` surfaces
//! and the lock's single instance there (`State::set_focused_monitor`),
//! so each shows on the monitor its `screen` is; while none is named,
//! the compositor places them, as before.
//!
//! The compositor service runs for this only while something reads the
//! focus ([`reads_focus`]): the `screens.focused` cell has a reader (a
//! binding of it, a `screens:` that is `focused` now, a lock's single
//! instance), or the program names `Screen.focused`. The follow is let
//! go [`STOP_GRACE`] after its last reader, as a service is stopped, and
//! the screens fall back to the first.

use std::sync::{Arc, Mutex, PoisonError};

use strand_compiler::lower::Program;
use strand_compiler::vm::ServiceHost as _;
use strand_services::STOP_GRACE;
use strand_services::wm::FocusFollower;

use super::*;

/// What the follower last reported, until the logic thread takes it.
type Report = Arc<Mutex<Option<Option<String>>>>;

/// The `screens` service's facts on the logic thread.
pub(super) struct Focus {
    /// The monitors, in plug order, as main last reported them.
    list: Vec<ScreenInfo>,
    /// The compositor's focused screen while followed (`None`: it names
    /// none, or is not followed).
    named: Option<String>,
    /// The monitor (`Screen.id`) main was last told to place focused
    /// surfaces on.
    told: Option<String>,
    follow: Option<Follow>,
    /// Wakes the logic loop when the follower reports.
    ping: calloop::ping::Ping,
}

struct Follow {
    _follower: FocusFollower,
    report: Report,
    /// Since when nothing has read the focus.
    unread_since: Option<Instant>,
}

impl Focus {
    pub(super) fn new(ping: calloop::ping::Ping) -> Self {
        Focus {
            list: Vec::new(),
            named: None,
            told: None,
            follow: None,
            ping,
        }
    }

    /// The monitors main reported ([`ToLogic::Screens`]).
    pub(super) fn screens(
        &mut self,
        rt: &Runtime,
        host: &SchemaHost,
        list: Vec<ScreenInfo>,
        to_main: Option<&Sender<ToMain>>,
    ) {
        self.list = list;
        self.publish(rt, host, to_main);
    }

    /// Takes what the follower reported while the loop slept.
    pub(super) fn reported(
        &mut self,
        rt: &Runtime,
        host: &SchemaHost,
        to_main: Option<&Sender<ToMain>>,
    ) {
        let Some(f) = &self.follow else {
            return;
        };
        let got = f
            .report
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(named) = got
            && named != self.named
        {
            self.named = named;
            self.publish(rt, host, to_main);
        }
    }

    /// After a step: follows the compositor's focus while `reads` (what
    /// [`reads_focus`] says), and lets it go [`STOP_GRACE`] after the
    /// last reader. True when the screens changed (the next step shows
    /// them).
    pub(super) fn follow(
        &mut self,
        rt: &Runtime,
        host: &SchemaHost,
        real: Option<&crate::services::Real>,
        reads: bool,
        now: Instant,
        to_main: Option<&Sender<ToMain>>,
    ) -> bool {
        match (&mut self.follow, reads) {
            (Some(f), true) => f.unread_since = None,
            (Some(f), false) => {
                let since = *f.unread_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= STOP_GRACE {
                    self.follow = None;
                    if self.named.take().is_some() {
                        self.publish(rt, host, to_main);
                        return true;
                    }
                }
            }
            (None, true) => {
                // Under the mock no services run: the first stays.
                if let Some(r) = real {
                    let report = Report::default();
                    let (slot, ping) = (report.clone(), self.ping.clone());
                    let follower = strand_services::wm::follow_focused_screen(&r.services, {
                        move |named| {
                            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(named);
                            ping.ping();
                        }
                    });
                    self.follow = Some(Follow {
                        _follower: follower,
                        report,
                        unread_since: None,
                    });
                }
            }
            (None, false) => {}
        }
        false
    }

    /// How long until a follow nobody reads is let go.
    pub(super) fn deadline(&self, now: Instant) -> Option<Duration> {
        let since = self.follow.as_ref()?.unread_since?;
        Some((since + STOP_GRACE).saturating_duration_since(now))
    }

    /// Sets the `screens` fields, and tells main the monitor the
    /// compositor named when that changed.
    fn publish(&mut self, rt: &Runtime, host: &SchemaHost, to_main: Option<&Sender<ToMain>>) {
        set_screens(rt, host, &self.list, self.named.as_deref());
        let pin = named_screen(&self.list, self.named.as_deref()).map(|s| s.id.clone());
        if pin != self.told {
            if let Some(tx) = to_main {
                // Main gone ends the run on the diff channel.
                let _ = tx.send(ToMain::FocusedScreen(pin.clone()));
            }
            self.told = pin;
        }
    }
}

impl super::shell::Shell {
    /// After a step: [`Focus::follow`], as the running program and its
    /// cells read the focus.
    pub(super) fn follow_focus(&mut self) -> bool {
        let rt = self.inst.runtime();
        let reads = reads_focus(rt, &self.host, &self.inst.vm().prog);
        self.focus.follow(
            rt,
            &self.host,
            self.real.as_ref(),
            reads,
            Instant::now(),
            self.to_main.as_ref(),
        )
    }
}

/// Something reads the focus: the `screens.focused` cell has a reader
/// (on its last run), or `prog` names `Screen.focused` anywhere (a
/// record's field, read through `screens.all` or a bar's `screen`).
pub(super) fn reads_focus(rt: &Runtime, host: &SchemaHost, prog: &Program) -> bool {
    prog.names_member("Screen", "focused")
        || host
            .sources_builtin(rt, "screens", Some("focused"))
            .into_iter()
            .any(|id| rt.observers(id).is_ok_and(|o| !o.is_empty()))
}

/// The screen named `named` (a connector name), if one has that name.
fn named_screen<'a>(screens: &'a [ScreenInfo], named: Option<&str>) -> Option<&'a ScreenInfo> {
    let named = named?;
    screens.iter().find(|s| s.name == named)
}

/// Which of `screens` has the focus: the one named `named`, else the
/// first (`None`: no screen).
pub(crate) fn focused_index(screens: &[ScreenInfo], named: Option<&str>) -> Option<usize> {
    named
        .and_then(|n| screens.iter().position(|s| s.name == n))
        .or((!screens.is_empty()).then_some(0))
}

#[cfg(test)]
mod tests {
    use super::super::tests::screen;
    use super::*;

    fn two() -> Vec<ScreenInfo> {
        vec![screen("A", "DP-1"), screen("B", "HDMI-A-1")]
    }

    #[test]
    fn the_named_screen_has_the_focus_else_the_first() {
        assert_eq!(focused_index(&two(), Some("HDMI-A-1")), Some(1));
        assert_eq!(focused_index(&two(), Some("DP-1")), Some(0));
        // Unknown, or a name none has: the first.
        assert_eq!(focused_index(&two(), None), Some(0));
        assert_eq!(focused_index(&two(), Some("VGA-9")), Some(0));
        assert_eq!(focused_index(&[], Some("DP-1")), None);
        assert_eq!(named_screen(&two(), Some("VGA-9")), None);
        assert_eq!(
            named_screen(&two(), Some("HDMI-A-1")).map(|s| s.id.as_str()),
            Some("B")
        );
    }

    fn host(rt: &Runtime) -> Rc<SchemaHost> {
        Rc::new(SchemaHost::new(rt, &crate::services::schema().types, None))
    }

    /// `screens.all`'s `focused` flags and `screens.focused`'s name.
    fn fields(rt: &Runtime, host: &SchemaHost) -> (Vec<bool>, String) {
        let types = &crate::services::schema().types;
        let all = host.read(rt, "screens", "all").unwrap();
        let flags = all
            .as_list()
            .unwrap()
            .iter()
            .map(|s| s.field(types, "focused").and_then(Value::as_bool).unwrap())
            .collect();
        let focused = host.read(rt, "screens", "focused").unwrap();
        let name = focused
            .field(types, "name")
            .and_then(Value::as_text)
            .unwrap_or("none")
            .to_string();
        (flags, name)
    }

    /// What main was told, in order.
    fn told(rx: Channel<ToMain>) -> Vec<Option<String>> {
        let mut el = EventLoop::<Vec<ToMain>>::try_new().unwrap();
        el.handle()
            .insert_source(rx, |e, _, got: &mut Vec<ToMain>| {
                if let Event::Msg(m) = e {
                    got.push(m);
                }
            })
            .unwrap();
        let mut got = Vec::new();
        el.dispatch(Some(Duration::ZERO), &mut got).unwrap();
        got.into_iter()
            .map(|m| match m {
                ToMain::FocusedScreen(s) => s,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    /// `screens.focused` and the `focused` flags name the compositor's
    /// screen, else the first; main is told the named one only, and only
    /// when it changes.
    #[test]
    fn the_screens_follow_the_named_screen_and_main_hears_it() {
        let rt = Runtime::new();
        let host = host(&rt);
        let (tx, rx) = calloop::channel::channel::<ToMain>();
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut f = Focus::new(ping);
        f.screens(&rt, &host, two(), Some(&tx));
        assert_eq!(fields(&rt, &host), (vec![true, false], "DP-1".into()));
        assert_eq!(f.told, None, "nothing named: the compositor places");
        f.named = Some("HDMI-A-1".into());
        f.publish(&rt, &host, Some(&tx));
        assert_eq!(fields(&rt, &host), (vec![false, true], "HDMI-A-1".into()));
        assert_eq!(f.told.as_deref(), Some("B"));
        // The same again: main is not told twice.
        f.screens(&rt, &host, two(), Some(&tx));
        // B unplugged: the first again, and main places nothing.
        f.screens(&rt, &host, vec![screen("A", "DP-1")], Some(&tx));
        assert_eq!(fields(&rt, &host), (vec![true], "DP-1".into()));
        assert_eq!(f.told, None);
        // Back: B again.
        f.screens(&rt, &host, two(), Some(&tx));
        assert_eq!(fields(&rt, &host), (vec![false, true], "HDMI-A-1".into()));
        // A name none has: the first, and nothing placed.
        f.named = Some("VGA-9".into());
        f.publish(&rt, &host, Some(&tx));
        assert_eq!(fields(&rt, &host), (vec![true, false], "DP-1".into()));
        drop(tx);
        assert_eq!(told(rx), [Some("B".into()), None, Some("B".into()), None]);
    }

    /// Without services (the mock) nothing is followed: the first stays.
    #[test]
    fn nothing_is_followed_without_services() {
        let rt = Runtime::new();
        let host = host(&rt);
        let (ping, _source) = calloop::ping::make_ping().unwrap();
        let mut f = Focus::new(ping);
        f.screens(&rt, &host, two(), None);
        let now = Instant::now();
        assert!(!f.follow(&rt, &host, None, true, now, None));
        assert!(f.follow.is_none());
        assert_eq!(f.deadline(now), None);
        assert_eq!(fields(&rt, &host), (vec![true, false], "DP-1".into()));
    }

    /// The readers: a reader of the `screens.focused` cell, and a program
    /// that names `Screen.focused`; a program reading neither, with no
    /// reader of the cell, reads no focus.
    #[test]
    fn the_focus_is_read_by_its_cells_readers_and_by_screen_focused() {
        let program = |src: &str| {
            let mut map = strand_compiler::SourceMap::new();
            map.add("shell.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, crate::services::schema());
            assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
            strand_compiler::lower::lower(&compiled.program, crate::services::schema())
        };
        let rt = Runtime::new();
        let host = host(&rt);
        let plain = program("bar Top { text screen.name }\n");
        assert!(!reads_focus(&rt, &host, &plain));
        let flag = program("bar Top { text screen.focused ? \"on\" : \"off\" }\n");
        assert!(reads_focus(&rt, &host, &flag));
        let h = host.clone();
        let reader = rt.memo(move |rt| Ok(h.read(rt, "screens", "focused").ok()));
        let _ = reader.get(&rt);
        assert!(reads_focus(&rt, &host, &plain), "a reader of the cell");
        reader.dispose(&rt);
        assert!(!reads_focus(&rt, &host, &plain), "its reader gone");
    }
}
