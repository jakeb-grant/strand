//! The compositor's state as typed values, and the change stream that
//! publishes it.
//!
//! The records mirror the `Workspace` and `Window` records of the builtin
//! schema (`strand-compiler`'s `builtin.schema`) field for field, plus
//! `Workspace::active` and `Window::urgent`, which [`super::SCHEMA`] (the
//! text that replaces the provisional stubs) declares.

use strand_core::keyed::{VecDiff, keyed_diff};

/// A toplevel window: the schema's `Window`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Window {
    /// Its identity: the compositor's own id (Hyprland's address, a niri
    /// or sway container id) when an IPC adapter runs, else the
    /// `ext-foreign-toplevel-list` identifier.
    pub id: String,
    /// Its title.
    pub title: String,
    /// The app's id, such as `firefox` (Hyprland's `class`).
    pub app_id: String,
    /// An icon name for its app: the app id until the apps service
    /// resolves desktop entries.
    pub icon: String,
    /// The workspace it is on, when known.
    pub workspace: Option<i64>,
    /// It has keyboard focus.
    pub focused: bool,
    /// It is minimised (sway: in the scratchpad; Hyprland: a taskbar asked
    /// for it).
    pub minimized: bool,
    /// It is fullscreen.
    pub fullscreen: bool,
    /// It asks for attention.
    pub urgent: bool,
    /// Its `ext-foreign-toplevel-list-v1` identifier, when known: the one
    /// the adapter reports (sway 1.10+, Hyprland's `stableId`), or `id`
    /// itself with the protocols alone. Not a schema field: it is how M4's
    /// thumbnails (`ext-image-copy-capture-v1`) will find the window's
    /// handle on the protocol thread.
    pub toplevel: Option<String>,
}

/// A workspace: the schema's `Workspace`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Its identity.
    pub id: i64,
    /// Its name.
    pub name: String,
    /// It is the focused workspace (at most one is).
    pub focused: bool,
    /// It is shown on its screen (one per screen).
    pub active: bool,
    /// It holds windows.
    pub occupied: bool,
    /// A window on it asks for attention.
    pub urgent: bool,
    /// The name of the screen (connector) it is on; empty when unknown.
    pub screen: String,
    /// Its windows, in the order of [`WmState::windows`]. Copies: a
    /// window's change (a title) is also an `Update` of its workspace in
    /// the stream. Accepted for now (a handful of windows per workspace);
    /// a store may instead derive this list from `windows.all` by
    /// `workspace` (docs/decisions.md, wave4-wm).
    pub windows: Vec<Window>,
}

/// Everything the `workspaces`, `windows` and `wm` services show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WmState {
    /// The compositor's name (`Hyprland`, `niri`, `sway`); empty when no
    /// adapter knows it.
    pub name: String,
    /// Every workspace, in the compositor's order.
    pub workspaces: Vec<Workspace>,
    /// Every window.
    pub windows: Vec<Window>,
    /// The name of the screen with keyboard focus.
    pub focused_screen: Option<String>,
}

impl WmState {
    /// The focused workspace.
    pub fn focused_workspace(&self) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.focused)
    }

    /// The focused window.
    pub fn focused_window(&self) -> Option<&Window> {
        self.windows.iter().find(|w| w.focused)
    }

    /// The workspaces on screen `name`: `workspaces.on(screen)`.
    pub fn on_screen<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Workspace> + 'a {
        self.workspaces.iter().filter(move |w| w.screen == name)
    }

    /// The workspace with `id`.
    pub fn workspace(&self, id: i64) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// The window with `id`.
    pub fn window(&self, id: &str) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Fills the derived fields from the window list: each workspace's
    /// `windows`, `occupied` and (or-ed in) `urgent`, the window's icon
    /// when empty, and the focused screen from the focused workspace when
    /// the source did not say.
    pub(crate) fn derive(&mut self) {
        for w in &mut self.windows {
            if w.icon.is_empty() {
                w.icon = w.app_id.clone();
            }
        }
        for ws in &mut self.workspaces {
            ws.windows = self
                .windows
                .iter()
                .filter(|w| w.workspace == Some(ws.id))
                .cloned()
                .collect();
            ws.occupied = !ws.windows.is_empty();
            ws.urgent |= ws.windows.iter().any(|w| w.urgent);
        }
        if self.focused_screen.is_none() {
            self.focused_screen = self
                .workspaces
                .iter()
                .find(|w| w.focused && !w.screen.is_empty())
                .map(|w| w.screen.clone());
        }
    }
}

/// Which compositor IPC adapter runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompositorKind {
    /// Hyprland's two sockets.
    Hyprland,
    /// niri's JSON socket.
    Niri,
    /// sway's i3-compatible IPC.
    Sway,
}

impl CompositorKind {
    /// The name `wm.name` shows.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hyprland => "Hyprland",
            Self::Niri => "niri",
            Self::Sway => "sway",
        }
    }
}

/// Where the state comes from, for `strand report` and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sources {
    /// The IPC adapter, if one was detected.
    pub ipc: Option<CompositorKind>,
    /// The adapter is connected (false while it reconnects).
    pub connected: bool,
    /// The compositor advertises `ext_foreign_toplevel_list_v1`.
    pub toplevel_list: bool,
    /// The compositor advertises `ext_workspace_manager_v1`.
    pub workspace_protocol: bool,
}

/// One change to what the services show. A batch is what one compositor
/// event (or one burst of them) changed.
#[derive(Clone, Debug, PartialEq)]
pub enum WmChange {
    /// `wm.name`.
    Name(String),
    /// `workspaces.all`, keyed by id.
    Workspaces(Vec<VecDiff<i64, Workspace>>),
    /// `windows.all`, keyed by id.
    Windows(Vec<VecDiff<String, Window>>),
    /// `workspaces.focused`.
    FocusedWorkspace(Option<Workspace>),
    /// `windows.focused`.
    FocusedWindow(Option<Window>),
    /// The screen with keyboard focus (for `screens.focused`).
    FocusedScreen(Option<String>),
    /// `wm.config_reloaded`: `Some(failed)` from niri, `None` from
    /// Hyprland and sway, which do not say.
    ConfigReloaded {
        /// The new config failed to load, when the compositor says.
        failed: Option<bool>,
    },
    /// The sources changed (an adapter connected or lost its socket).
    Sources(Sources),
}

/// Turns successive [`WmState`]s into the smallest [`WmChange`]s.
#[derive(Debug, Default)]
pub struct Publisher {
    last: Option<WmState>,
    sources: Option<Sources>,
}

impl Publisher {
    /// A publisher that has sent nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last state published.
    pub fn state(&self) -> Option<&WmState> {
        self.last.as_ref()
    }

    /// The changes from the last published state to `next` (the first
    /// call sends whole lists as `Reset`s and every field). Empty when
    /// nothing changed.
    pub fn publish(&mut self, next: WmState) -> Vec<WmChange> {
        let mut out = Vec::new();
        let ws_items = |s: &WmState| -> Vec<(i64, Workspace)> {
            s.workspaces.iter().map(|w| (w.id, w.clone())).collect()
        };
        let win_items = |s: &WmState| -> Vec<(String, Window)> {
            s.windows
                .iter()
                .map(|w| (w.id.clone(), w.clone()))
                .collect()
        };
        match &self.last {
            None => {
                out.push(WmChange::Name(next.name.clone()));
                out.push(WmChange::Workspaces(vec![VecDiff::Reset {
                    items: ws_items(&next),
                }]));
                out.push(WmChange::Windows(vec![VecDiff::Reset {
                    items: win_items(&next),
                }]));
                out.push(WmChange::FocusedWorkspace(
                    next.focused_workspace().cloned(),
                ));
                out.push(WmChange::FocusedWindow(next.focused_window().cloned()));
                out.push(WmChange::FocusedScreen(next.focused_screen.clone()));
            }
            Some(last) => {
                if last.name != next.name {
                    out.push(WmChange::Name(next.name.clone()));
                }
                if last.workspaces != next.workspaces {
                    let d = keyed_diff(&ws_items(last), &ws_items(&next));
                    if !d.is_empty() {
                        out.push(WmChange::Workspaces(d));
                    }
                }
                if last.windows != next.windows {
                    let d = keyed_diff(&win_items(last), &win_items(&next));
                    if !d.is_empty() {
                        out.push(WmChange::Windows(d));
                    }
                }
                if last.focused_workspace() != next.focused_workspace() {
                    out.push(WmChange::FocusedWorkspace(
                        next.focused_workspace().cloned(),
                    ));
                }
                if last.focused_window() != next.focused_window() {
                    out.push(WmChange::FocusedWindow(next.focused_window().cloned()));
                }
                if last.focused_screen != next.focused_screen {
                    out.push(WmChange::FocusedScreen(next.focused_screen.clone()));
                }
            }
        }
        self.last = Some(next);
        out
    }

    /// Forgets the last published state: the next [`Publisher::publish`]
    /// sends whole lists as `Reset`s and every field again.
    pub fn forget(&mut self) {
        self.last = None;
    }

    /// A [`WmChange::Sources`] when `sources` differ from the last sent.
    pub fn sources(&mut self, sources: Sources) -> Option<WmChange> {
        if self.sources.as_ref() == Some(&sources) {
            return None;
        }
        self.sources = Some(sources.clone());
        Some(WmChange::Sources(sources))
    }
}

/// Applies changes to a mirror: what a consumer of the stream holds. Tests
/// use it to check that the stream reproduces the state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mirror {
    /// `wm.name`.
    pub name: String,
    /// `workspaces.all`.
    pub workspaces: Vec<(i64, Workspace)>,
    /// `windows.all`.
    pub windows: Vec<(String, Window)>,
    /// `workspaces.focused`.
    pub focused_workspace: Option<Workspace>,
    /// `windows.focused`.
    pub focused_window: Option<Window>,
    /// The focused screen.
    pub focused_screen: Option<String>,
    /// The `wm.config_reloaded` events seen, in order.
    pub reloads: Vec<Option<bool>>,
    /// The last sources seen.
    pub sources: Sources,
}

impl Mirror {
    /// Applies one change. A diff that does not fit the mirror is an error
    /// (the stream is inconsistent).
    pub fn apply(&mut self, change: &WmChange) -> Result<(), strand_core::keyed::KeyedError> {
        match change {
            WmChange::Name(n) => self.name = n.clone(),
            WmChange::Workspaces(d) => {
                for diff in d {
                    diff.apply(&mut self.workspaces)?;
                }
            }
            WmChange::Windows(d) => {
                for diff in d {
                    diff.apply(&mut self.windows)?;
                }
            }
            WmChange::FocusedWorkspace(w) => self.focused_workspace = w.clone(),
            WmChange::FocusedWindow(w) => self.focused_window = w.clone(),
            WmChange::FocusedScreen(s) => self.focused_screen = s.clone(),
            WmChange::ConfigReloaded { failed } => self.reloads.push(*failed),
            WmChange::Sources(s) => self.sources = s.clone(),
        }
        Ok(())
    }

    /// The workspace names in order.
    pub fn workspace_names(&self) -> Vec<String> {
        self.workspaces
            .iter()
            .map(|(_, w)| w.name.clone())
            .collect()
    }

    /// The workspace named `name`.
    pub fn workspace(&self, name: &str) -> Option<&Workspace> {
        self.workspaces
            .iter()
            .map(|(_, w)| w)
            .find(|w| w.name == name)
    }

    /// The first window whose app id is `app_id`.
    pub fn window_by_app(&self, app_id: &str) -> Option<&Window> {
        self.windows
            .iter()
            .map(|(_, w)| w)
            .find(|w| w.app_id == app_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: i64, name: &str, focused: bool) -> Workspace {
        Workspace {
            id,
            name: name.into(),
            focused,
            active: focused,
            screen: "DP-1".into(),
            ..Default::default()
        }
    }

    fn win(id: &str, wsid: i64, focused: bool) -> Window {
        Window {
            id: id.into(),
            title: format!("title {id}"),
            app_id: "foot".into(),
            workspace: Some(wsid),
            focused,
            ..Default::default()
        }
    }

    #[test]
    fn derive_nests_windows_and_marks_occupied_and_urgent() {
        let mut s = WmState {
            workspaces: vec![ws(1, "1", true), ws(2, "2", false)],
            windows: vec![win("a", 1, true), {
                let mut w = win("b", 2, false);
                w.urgent = true;
                w
            }],
            ..Default::default()
        };
        s.derive();
        assert_eq!(s.workspaces[0].windows.len(), 1);
        assert!(s.workspaces[0].occupied && !s.workspaces[0].urgent);
        assert!(s.workspaces[1].occupied && s.workspaces[1].urgent);
        assert_eq!(s.windows[0].icon, "foot");
        assert_eq!(s.focused_screen.as_deref(), Some("DP-1"));
    }

    #[test]
    fn publisher_sends_resets_first_then_minimal_diffs() {
        let mut p = Publisher::new();
        let mut m = Mirror::default();
        let mut s = WmState {
            name: "sway".into(),
            workspaces: vec![ws(1, "1", true), ws(2, "2", false)],
            windows: vec![win("a", 1, true)],
            focused_screen: Some("DP-1".into()),
        };
        s.derive();
        let first = p.publish(s.clone());
        assert!(
            matches!(&first[1], WmChange::Workspaces(d) if matches!(d[0], VecDiff::Reset { .. }))
        );
        for c in &first {
            m.apply(c).unwrap();
        }
        // Nothing changed: nothing sent.
        assert!(p.publish(s.clone()).is_empty());
        // Focus moves to workspace 2.
        s.workspaces[0].focused = false;
        s.workspaces[1].focused = true;
        let changes = p.publish(s.clone());
        assert_eq!(changes.len(), 2, "{changes:?}");
        match &changes[0] {
            WmChange::Workspaces(d) => {
                assert_eq!(d.len(), 2);
                assert!(d.iter().all(|d| matches!(d, VecDiff::Update { .. })));
            }
            other => panic!("{other:?}"),
        }
        for c in &changes {
            m.apply(c).unwrap();
        }
        assert_eq!(m.focused_workspace.as_ref().map(|w| w.id), Some(2));
        assert_eq!(m.workspaces.len(), 2);
        // A window closes: one Remove, workspace 1 no longer occupied.
        s.windows.clear();
        s.derive();
        for c in &p.publish(s.clone()) {
            m.apply(c).unwrap();
        }
        assert!(m.windows.is_empty());
        assert!(!m.workspace("1").unwrap().occupied);
        assert!(m.focused_window.is_none());
    }

    #[test]
    fn sources_are_sent_only_when_they_change() {
        let mut p = Publisher::new();
        let s = Sources {
            ipc: Some(CompositorKind::Sway),
            connected: true,
            ..Default::default()
        };
        assert!(p.sources(s.clone()).is_some());
        assert!(p.sources(s).is_none());
    }
}
