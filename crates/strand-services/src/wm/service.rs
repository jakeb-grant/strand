//! The `windows`, `workspaces` and `wm` stores: three services to the
//! language (each with its own readers, start and 5 s stop), one
//! compositor service underneath ([`WmHub`]): the first of them to start
//! starts the adapter and the protocol thread, the last to stop stops
//! them.
//!
//! Each store's body runs on the shared current-thread runtime. It
//! subscribes to the hub of that runtime's thread ([`hub`]: one per
//! services runtime, made on first use; each start of it takes the
//! [`configure`]d [`WmConfig`], else [`WmConfig::from_env`] afresh, so a
//! compositor whose IPC socket appeared after an earlier start is found), turns each batch of
//! [`WmChange`]s into one envelope of its own patches (keyed diffs pass
//! through as keyed diffs: a title change is one `Update`, never a new
//! list), and runs its actions as [`WmAction`]s. The `strand-toplevel`
//! thread (the Wayland protocols, `!Send`) is the hub's; the IPC adapters
//! are tasks on the shared runtime. Nothing polls: an idle compositor
//! wakes nothing.

use std::cell::RefCell;
use std::sync::Mutex;

use strand_core::VecDiff;

use super::model::{self, WmChange};
use super::schema::{WINDOWS_SCHEMA, WM_SCHEMA, WORKSPACES_SCHEMA};
use super::{WmAction, WmConfig, WmHub, WmSubscription};
use crate::{Call, Cx, Data, Event, Msg, ServiceError, Store, ToData, service};

static CONFIG: Mutex<Option<WmConfig>> = Mutex::new(None);

/// What the compositor service connects to from its next start on
/// (tests: a fake compositor's sockets and display). `None` restores
/// [`WmConfig::from_env`]. A running service keeps its config until it
/// stops.
pub fn configure(config: Option<WmConfig>) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = config;
    }
}

fn config() -> WmConfig {
    CONFIG
        .lock()
        .ok()
        .and_then(|c| c.clone())
        .unwrap_or_else(|| WmConfig::from_env(None))
}

thread_local! {
    /// The hub of this services runtime thread.
    static HUB: RefCell<Option<WmHub>> = const { RefCell::new(None) };
}

/// The compositor hub of the current services runtime thread (made on
/// first use). `None` outside a tokio runtime.
pub fn hub() -> Option<WmHub> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    Some(HUB.with(|h| {
        h.borrow_mut()
            .get_or_insert_with(|| WmHub::fresh(config, handle))
            .clone()
    }))
}

/// A toplevel window: the schema's `Window`.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Window", key = id)]
pub struct WindowItem {
    pub id: String,
    pub title: String,
    pub app_id: String,
    pub icon: String,
    pub workspace: Option<i64>,
    pub focused: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub urgent: bool,
    pub app: Option<crate::apps::App>,
}

impl From<model::Window> for WindowItem {
    fn from(w: model::Window) -> Self {
        WindowItem {
            id: w.id,
            title: w.title,
            app_id: w.app_id,
            icon: w.icon,
            workspace: w.workspace,
            focused: w.focused,
            minimized: w.minimized,
            maximized: w.maximized,
            fullscreen: w.fullscreen,
            urgent: w.urgent,
            app: w.app,
        }
    }
}

/// A workspace: the schema's `Workspace`.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Workspace", key = id)]
pub struct WorkspaceItem {
    pub id: i64,
    pub name: String,
    pub focused: bool,
    pub active: bool,
    pub occupied: bool,
    pub urgent: bool,
    pub screen: String,
    pub windows: Vec<WindowItem>,
}

impl From<model::Workspace> for WorkspaceItem {
    fn from(w: model::Workspace) -> Self {
        WorkspaceItem {
            id: w.id,
            name: w.name,
            focused: w.focused,
            active: w.active,
            occupied: w.occupied,
            urgent: w.urgent,
            screen: w.screen,
            windows: w.windows.into_iter().map(WindowItem::from).collect(),
        }
    }
}

/// Keyed diffs of one item type as diffs of another.
fn map_diffs<K, A, B>(diffs: Vec<VecDiff<K, A>>, f: impl Fn(A) -> B) -> Vec<VecDiff<K, B>> {
    diffs
        .into_iter()
        .map(|d| match d {
            VecDiff::Reset { items } => VecDiff::Reset {
                items: items.into_iter().map(|(k, v)| (k, f(v))).collect(),
            },
            VecDiff::Insert { index, key, value } => VecDiff::Insert {
                index,
                key,
                value: f(value),
            },
            VecDiff::Update { index, key, value } => VecDiff::Update {
                index,
                key,
                value: f(value),
            },
            VecDiff::Remove { index, key } => VecDiff::Remove { index, key },
            VecDiff::Move { from, to, key } => VecDiff::Move { from, to, key },
        })
        .collect()
}

/// `windows`' actions (on a `Window` item).
#[derive(Call, Debug)]
pub enum WindowAction {
    /// `win.focus()`.
    Focus { item: WindowItem },
    /// `win.close()`.
    Close { item: WindowItem },
    /// `win.minimize()`.
    Minimize { item: WindowItem },
    /// `win.maximize()`: maximise, or restore when maximised.
    Maximize { item: WindowItem },
    /// `win.fullscreen()`: fullscreen, or restore when fullscreen.
    Fullscreen { item: WindowItem },
}

/// `workspaces`' actions: on a `Workspace` item, and the service's own.
#[derive(Call, Debug)]
pub enum WorkspaceAction {
    /// `ws.focus()`.
    Focus { item: WorkspaceItem },
    /// `workspaces.focus(target)`: by id or by name (Gaps 2).
    #[call(name = "focus")]
    FocusTarget { target: WorkspaceTarget },
}

/// What `workspaces.focus(target)` switches to: an `int` is a
/// workspace's id (a configured one's included), a `text` its name.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceTarget {
    /// By id.
    Id(i64),
    /// By name.
    Name(String),
}

impl crate::FromData for WorkspaceTarget {
    fn from_data(d: &Data) -> Result<Self, crate::DataError> {
        match d {
            Data::Int(n) => Ok(Self::Id(*n)),
            // A number literal in a list is a float (owner-testing.md,
            // gaps 12): a whole one is an id.
            Data::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Ok(Self::Id(*f as i64)),
            Data::Text(t) => Ok(Self::Name(t.to_string())),
            other => Err(crate::DataError::new(format!(
                "a workspace is an id (int) or a name (text), not {}",
                other.kind()
            ))),
        }
    }
}

/// Toplevel windows, from `ext-foreign-toplevel-list`.
#[service(name = "windows", schema = WINDOWS_SCHEMA, action = WindowAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Windows {
    /// The focused window, if any: `windows.focused?.title`.
    pub focused: Option<WindowItem>,
    /// Every window, keyed by `id`.
    #[store(keyed)]
    pub all: Vec<WindowItem>,
}

/// Workspaces, from `ext-workspace-v1`; Hyprland, niri and sway IPC fill
/// in what the protocol does not cover yet.
#[service(
    name = "workspaces",
    schema = WORKSPACES_SCHEMA,
    action = WorkspaceAction,
    fns = workspace_fns
)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Workspaces {
    /// Every workspace, keyed by `id`.
    #[store(keyed)]
    pub all: Vec<WorkspaceItem>,
    /// The focused workspace, if any.
    pub focused: Option<WorkspaceItem>,
    /// The workspaces the compositor's config places on a screen that do
    /// not exist yet (Hyprland's numbered workspace rules, sway's
    /// `workspace … output …`), keyed by `id`: empty and inactive, made
    /// by `ws.focus()`. `workspaces.slots(screen)` puts them in place.
    #[store(keyed)]
    pub configured: Vec<WorkspaceItem>,
}

/// The compositor.
#[service(name = "wm", schema = WM_SCHEMA)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Wm {
    /// Its name, such as `Hyprland`, `niri` or `sway` (the desktop's name
    /// when only the standard protocols serve it).
    pub name: String,
    /// The compositor reloaded its config; `failed` says whether the new
    /// config failed to load, when the compositor tells (niri).
    pub config_reloaded: Event<Option<bool>>,
}

/// `workspaces.on(screen)`: the workspaces whose `screen` is the screen's
/// connector name, computed on the logic thread from the cells (a reader
/// depends on `workspaces.all`).
fn workspace_fns(
    cells: &WorkspacesCells,
    rt: &strand_core::Runtime,
    method: &str,
    args: &[Data],
) -> Option<Result<Data, strand_core::Error>> {
    match method {
        "on" => {
            let name = match args
                .first()
                .map(|s| crate::record_field(s, "Screen", "name"))
            {
                Some(Ok(Data::Text(t))) => Some(t.to_string()),
                _ => None,
            };
            Some(cells.all.with(rt, |v| {
                Data::List(
                    v.items()
                        .iter()
                        .filter(|(_, w)| name.as_deref() == Some(w.screen.as_str()))
                        .map(|(_, w)| w.to_data())
                        .collect(),
                )
            }))
        }
        "slots" => {
            let name = match args
                .first()
                .map(|s| crate::record_field(s, "Screen", "name"))
            {
                Some(Ok(Data::Text(t))) => Some(t.to_string()),
                _ => None,
            };
            let on = |w: &WorkspaceItem| name.as_deref() == Some(w.screen.as_str());
            let made = match cells.all.with(rt, |v| {
                v.items()
                    .iter()
                    .filter(|(_, w)| on(w))
                    .map(|(_, w)| w.clone())
                    .collect::<Vec<_>>()
            }) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            Some(cells.configured.with(rt, |v| {
                let configured = v
                    .items()
                    .iter()
                    .filter(|(_, w)| on(w))
                    .map(|(_, w)| w.clone());
                Data::List(
                    slots(made, configured)
                        .iter()
                        .map(ToData::to_data)
                        .collect(),
                )
            }))
        }
        _ => None,
    }
}

/// A screen's slots: its workspaces and the configured ones not made yet,
/// by the number their name starts with (as compositors number them),
/// then the rest in their own order (Gaps 3, decisions.md
/// m6-w2-services).
pub fn slots(
    made: impl IntoIterator<Item = WorkspaceItem>,
    configured: impl IntoIterator<Item = WorkspaceItem>,
) -> Vec<WorkspaceItem> {
    let mut all: Vec<WorkspaceItem> = made.into_iter().chain(configured).collect();
    let number = |w: &WorkspaceItem| -> Option<u64> {
        let digits: String = w.name.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    // Stable: unnumbered ones keep their order, after the numbered.
    all.sort_by_key(|w| (number(w).is_none(), number(w)));
    all
}

/// The hub's subscription, or the error that ends the body.
fn subscribe() -> Result<WmSubscription, ServiceError> {
    hub()
        .map(|h| h.subscribe())
        .ok_or_else(|| ServiceError("no services runtime".into()))
}

/// Runs `action` and logs its failure (the outcome is not the
/// language's: the change it makes arrives in the stream).
fn request(sub: &WmSubscription, calls: &mut tokio::task::JoinSet<()>, action: WmAction) {
    let reply = sub.request(action.clone());
    calls.spawn(async move {
        if let Err(e) = reply.await {
            log::warn!("compositor action {action:?}: {e}");
        }
    });
}

/// The shared loop of the three stores: batches in (`apply` makes this
/// store's patches and says whether its state is now known), actions out.
async fn follow<S: crate::Service>(
    mut cx: Cx<S>,
    mut apply: impl FnMut(&mut Cx<S>, Vec<WmChange>) -> Option<bool>,
    action: impl Fn(S::Action) -> Option<WmAction>,
) -> Result<(), ServiceError> {
    let mut sub = subscribe()?;
    let mut calls = tokio::task::JoinSet::new();
    // The adapter's degradation last seen (`Sources::degraded`).
    let mut degraded: Option<String> = None;
    loop {
        tokio::select! {
            Some(_) = calls.join_next(), if !calls.is_empty() => {}
            m = cx.recv() => match m {
                None => return Ok(()),
                Some(Msg::Action(a)) => {
                    if let Some(a) = action(a) {
                        request(&sub, &mut calls, a);
                    }
                }
                Some(_) => {}
            },
            b = sub.recv() => match b {
                None => return Err(ServiceError("the compositor service ended".into())),
                Some(batch) => {
                    report_degraded(&mut cx, &sub, &batch, &mut degraded);
                    match apply(&mut cx, batch) {
                        None => return Ok(()),
                        Some(true) => {
                            cx.ready();
                        }
                        Some(false) => {}
                    }
                }
            },
        }
    }
}

/// A batch whose `Sources` say the adapter does not understand the
/// compositor (a new reason) becomes a [`crate::ServiceDiagnostic`]
/// (`Cx::warn`: logged and sent to `strand watch`; not an overlay notice,
/// as the service retries it itself), raised by one store only
/// ([`WmSubscription::reports`]). Its end is logged by the service.
fn report_degraded<S: crate::Service>(
    cx: &mut Cx<S>,
    sub: &WmSubscription,
    batch: &[WmChange],
    last: &mut Option<String>,
) {
    let Some(sources) = batch.iter().rev().find_map(|c| match c {
        WmChange::Sources(s) => Some(s),
        _ => None,
    }) else {
        return;
    };
    if sources.degraded == *last {
        return;
    }
    last.clone_from(&sources.degraded);
    if let Some(why) = &sources.degraded
        && sub.reports()
    {
        cx.warn(why.clone());
    }
}

impl Windows {
    async fn run(cx: Cx<Self>) -> Result<(), ServiceError> {
        follow(
            cx,
            |cx, batch| {
                let mut patches = Vec::new();
                let mut known = false;
                for change in batch {
                    match change {
                        WmChange::Windows(d) => {
                            known = true;
                            patches.push(WindowsPatch::All(map_diffs(d, WindowItem::from)));
                        }
                        WmChange::FocusedWindow(w) => {
                            patches.push(WindowsPatch::Focused(w.map(WindowItem::from)));
                        }
                        _ => {}
                    }
                }
                cx.send(patches).then_some(known)
            },
            |a| {
                Some(match a {
                    WindowAction::Focus { item } => WmAction::FocusWindow(item.id),
                    WindowAction::Close { item } => WmAction::CloseWindow(item.id),
                    WindowAction::Minimize { item } => WmAction::MinimizeWindow(item.id),
                    WindowAction::Maximize { item } => WmAction::MaximizeWindow(item.id),
                    WindowAction::Fullscreen { item } => WmAction::FullscreenWindow(item.id),
                })
            },
        )
        .await
    }
}

impl Workspaces {
    async fn run(cx: Cx<Self>) -> Result<(), ServiceError> {
        follow(
            cx,
            |cx, batch| {
                let mut patches = Vec::new();
                let mut known = false;
                for change in batch {
                    match change {
                        WmChange::Workspaces(d) => {
                            known = true;
                            patches.push(WorkspacesPatch::All(map_diffs(d, WorkspaceItem::from)));
                        }
                        WmChange::FocusedWorkspace(w) => {
                            patches.push(WorkspacesPatch::Focused(w.map(WorkspaceItem::from)));
                        }
                        WmChange::Configured(d) => {
                            patches.push(WorkspacesPatch::Configured(map_diffs(
                                d,
                                WorkspaceItem::from,
                            )));
                        }
                        _ => {}
                    }
                }
                cx.send(patches).then_some(known)
            },
            |a| {
                Some(match a {
                    WorkspaceAction::Focus { item } => WmAction::FocusWorkspace(item.id),
                    WorkspaceAction::FocusTarget {
                        target: WorkspaceTarget::Id(id),
                    } => WmAction::FocusWorkspace(id),
                    WorkspaceAction::FocusTarget {
                        target: WorkspaceTarget::Name(name),
                    } => WmAction::FocusWorkspaceNamed(name),
                })
            },
        )
        .await
    }
}

impl Wm {
    async fn run(cx: Cx<Self>) -> Result<(), ServiceError> {
        follow(
            cx,
            |cx, batch| {
                let mut known = false;
                let mut reloads = Vec::new();
                let mut patches = Vec::new();
                for change in batch {
                    match change {
                        WmChange::Name(n) => {
                            known = true;
                            patches.push(WmPatch::Name(n));
                        }
                        WmChange::ConfigReloaded { failed } => reloads.push(failed),
                        _ => {}
                    }
                }
                if !cx.send(patches) {
                    return None;
                }
                for failed in reloads {
                    if !cx.emit(WmEvent::ConfigReloaded(failed)) {
                        return None;
                    }
                }
                Some(known)
            },
            |_: crate::NoCall| None,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FromData, Keyed, Service};

    #[test]
    fn the_stores_are_the_schema_records() {
        assert_eq!(<Windows as Service>::NAME, "windows");
        assert_eq!(<Workspaces as Service>::NAME, "workspaces");
        assert_eq!(<Wm as Service>::NAME, "wm");
        let names: Vec<&str> = Workspaces::FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, ["all", "focused", "configured"]);
        assert_eq!((Workspaces::FIELDS[0].ty)(), "[Workspace]");
        assert_eq!((Windows::FIELDS[0].ty)(), "Window?");
        assert_eq!(Wm::EVENTS[0].name, "config_reloaded");
        assert_eq!(Wm::EVENTS[0].arity, 1);
        let w = model::Workspace {
            id: 3,
            name: "web".into(),
            screen: "DP-1".into(),
            windows: vec![model::Window {
                id: "7".into(),
                toplevel: Some("t".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let item = WorkspaceItem::from(w);
        assert_eq!(item.key(), 3);
        assert_eq!(WorkspaceItem::from_data(&item.to_data()), Ok(item.clone()));
        assert_eq!(item.windows[0].id, "7");
    }

    #[test]
    fn diffs_keep_their_shape() {
        let d = vec![
            VecDiff::Insert {
                index: 0,
                key: 1i64,
                value: model::Workspace {
                    id: 1,
                    ..Default::default()
                },
            },
            VecDiff::Move {
                from: 0,
                to: 1,
                key: 1,
            },
            VecDiff::Remove { index: 1, key: 1 },
        ];
        let m = map_diffs(d, WorkspaceItem::from);
        assert!(matches!(m[0], VecDiff::Insert { index: 0, key: 1, ref value } if value.id == 1));
        assert!(matches!(
            m[1],
            VecDiff::Move {
                from: 0,
                to: 1,
                key: 1
            }
        ));
        assert!(matches!(m[2], VecDiff::Remove { index: 1, key: 1 }));
    }

    #[test]
    fn the_bodies_are_send_to_start() {
        // `Start::shared` takes a `Send` closure building a local future;
        // the hub's run must still be a `Send` future for its runtime.
        fn send<T: Send>(_: &T) {}
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let fut = super::super::run(WmConfig::default(), |_| {}, rx);
        send(&fut);
    }
}
