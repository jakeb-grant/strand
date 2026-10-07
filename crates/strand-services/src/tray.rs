//! `tray`: a StatusNotifierItem host, with our own zbus client
//! (decisions.md, wave4-a2: system-tray always connects to the
//! machine's session bus and leaves detached tasks running after it is
//! dropped).
//!
//! On a session-bus connection of its own (the names go with it when the
//! service stops), the service owns `org.kde.StatusNotifierHost-<pid>-<n>`
//! and either registers with the session's watcher
//! (`org.kde.StatusNotifierWatcher`) or, when there is none, becomes the
//! watcher itself ([`Watcher`]: items register with it, and it tells them
//! when the host is there). Each item is read (`org.kde.StatusNotifierItem`
//! properties) and followed (`NewIcon`, `NewTitle`, `NewToolTip`,
//! `NewStatus`, … read it again), with its menu
//! (`com.canonical.dbusmenu` `GetLayout`, again on `LayoutUpdated` and
//! `ItemsPropertiesUpdated`); an item whose connection goes is removed.
//!
//! Icons: an icon name (looked up first in the item's `IconThemePath`),
//! else its largest pixmap written as a PNG file (`image` shows both);
//! `NeedsAttention` shows the attention icon when the item has one.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use tokio::sync::mpsc;
use zbus::fdo::RequestNameReply;
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{MatchRule, MessageStream};

use crate::dbus::{self, Props};
use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `tray` service serves.
pub const SCHEMA: &str = strand_services_schema::TRAY;

/// The watcher's bus name, path and interface.
pub const WATCHER: &str = "org.kde.StatusNotifierWatcher";
/// The watcher's object path.
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The items' interface.
pub const ITEM: &str = "org.kde.StatusNotifierItem";
/// An item's default object path.
pub const ITEM_PATH: &str = "/StatusNotifierItem";
/// The menus' interface.
pub const MENU: &str = "com.canonical.dbusmenu";

/// An entry of a tray item's menu.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "TrayMenuItem", key = id)]
pub struct TrayMenuItem {
    pub id: i64,
    pub item: String,
    pub label: String,
    pub enabled: bool,
    pub kind: String,
    pub toggle: String,
    pub checked: bool,
    pub icon: Option<String>,
    pub children: Vec<TrayMenuItem>,
}

/// A tray item's menu.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "TrayMenu")]
pub struct TrayMenu {
    pub item: String,
    pub items: Vec<TrayMenuItem>,
}

/// An item in the tray.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "TrayItem", key = id)]
pub struct TrayItem {
    pub id: String,
    pub title: String,
    pub icon: String,
    pub tooltip: Option<String>,
    pub status: String,
    pub menu: TrayMenu,
}

/// `tray`'s actions.
#[derive(Call, Debug)]
pub enum TrayAction {
    /// `item.activate()`.
    Activate { item: TrayItem },
    /// `item.secondary()`.
    Secondary { item: TrayItem },
    /// `item.scroll(dy)`.
    Scroll { item: TrayItem, dy: f64 },
    /// `item.menu.open()`.
    Open { item: TrayMenu },
    /// `entry.activate()` for an entry of a menu.
    #[call(name = "activate")]
    ActivateEntry { item: TrayMenuItem },
}

/// See the module docs.
#[service(name = "tray", action = TrayAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Tray {
    /// The tray's items, keyed by `id`: `for item in tray.items`.
    #[store(keyed)]
    pub items: Vec<TrayItem>,
}

/// An item's bus name and object path from what it registered: a bus
/// name (its path is the default), an object path (its bus name is the
/// sender's), or both run together (`:1.42/org/ayatana/NotificationItem/x`).
pub fn parse_registration(service: &str, sender: &str) -> (String, String) {
    if service.starts_with('/') {
        return (sender.to_string(), service.to_string());
    }
    match service.find('/') {
        Some(i) => (service[..i].to_string(), service[i..].to_string()),
        None => (service.to_string(), ITEM_PATH.to_string()),
    }
}

/// A label without its mnemonic underscores (`_Open` → `Open`, `__` →
/// `_`).
pub fn strip_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '_' {
            if chars.peek() == Some(&'_') {
                out.push('_');
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// One menu node of a `GetLayout` reply: `(ia{sv}av)`.
type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// A DBusMenu node's entries (its visible children), for tray item `item`.
fn entries(item: &str, children: &[OwnedValue], depth: usize) -> Vec<TrayMenuItem> {
    if depth > 8 {
        return Vec::new();
    }
    children
        .iter()
        .filter_map(|c| {
            let v: Value<'_> = c.try_clone().ok()?.into();
            let v = match v {
                Value::Value(inner) => *inner,
                v => v,
            };
            let (id, props, kids): Node = v.try_into().ok()?;
            let props: Props = props;
            if dbus::boolean(&props, "visible") == Some(false) {
                return None;
            }
            let kind = dbus::text(&props, "type").unwrap_or_else(|| "standard".into());
            let toggle = match dbus::text(&props, "toggle-type").as_deref() {
                Some("checkmark") => "checkmark",
                Some("radio") => "radio",
                _ => "none",
            };
            Some(TrayMenuItem {
                id: i64::from(id),
                item: item.to_string(),
                label: strip_mnemonic(&dbus::text(&props, "label").unwrap_or_default()),
                enabled: dbus::boolean(&props, "enabled").unwrap_or(true),
                kind: if kind == "separator" {
                    kind
                } else {
                    "standard".into()
                },
                toggle: toggle.to_string(),
                checked: dbus::number(&props, "toggle-state") == Some(1.0),
                icon: dbus::text(&props, "icon-name").filter(|s| !s.is_empty()),
                children: entries(item, &kids, depth + 1),
            })
        })
        .collect()
}

/// An icon named `name` the app ships under `theme` (its
/// `IconThemePath`), searched a few levels deep.
fn themed_icon(theme: &Path, name: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
        for ext in ["png", "svg", "xpm"] {
            let p = dir.join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
        if depth == 0 {
            return None;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        // Larger sizes sort later; prefer scalable, then the largest.
        dirs.reverse();
        dirs.iter().find_map(|d| walk(d, name, depth - 1))
    }
    walk(theme, name, 4)
}

/// The largest pixmap of an `a(iiay)` property, as a PNG file.
fn pixmap_icon(props: &Props, key: &str) -> Option<String> {
    let v = props.get(key)?.try_clone().ok()?;
    let list: Vec<(i32, i32, Vec<u8>)> = v.try_into().ok()?;
    let (w, h, data) = list
        .into_iter()
        .filter(|(w, h, _)| *w > 0 && *h > 0)
        .max_by_key(|(w, h, _)| i64::from(*w) * i64::from(*h))?;
    let written = crate::pixmap::from_argb32(w, h, &data)
        .and_then(|(w, h, rgba)| crate::pixmap::write_rgba(w, h, &rgba));
    match written {
        Ok(p) => Some(p.to_string_lossy().into_owned()),
        Err(e) => {
            log::debug!("tray: pixmap not shown: {e}");
            None
        }
    }
}

/// The icon to show for an item's properties.
fn icon(props: &Props) -> String {
    let theme = dbus::text(props, "IconThemePath").filter(|t| !t.is_empty());
    let named = |key: &str| {
        let name = dbus::text(props, key).filter(|n| !n.is_empty())?;
        if let Some(t) = &theme
            && let Some(p) = themed_icon(Path::new(t), &name)
        {
            return Some(p.to_string_lossy().into_owned());
        }
        Some(name)
    };
    let attention = dbus::text(props, "Status").as_deref() == Some("NeedsAttention");
    let attention_icon = attention
        .then(|| named("AttentionIconName").or_else(|| pixmap_icon(props, "AttentionIconPixmap")))
        .flatten();
    attention_icon
        .or_else(|| named("IconName"))
        .or_else(|| pixmap_icon(props, "IconPixmap"))
        .unwrap_or_default()
}

/// An item's tooltip: its title, and its description under it.
fn tooltip(props: &Props) -> Option<String> {
    let v = props.get("ToolTip")?.try_clone().ok()?;
    type Tip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);
    let (_, _, title, body): Tip = v.try_into().ok()?;
    let text = match (title.trim(), body.trim()) {
        ("", "") => return None,
        (t, "") => t.to_string(),
        ("", b) => b.to_string(),
        (t, b) => format!("{t}\n{b}"),
    };
    Some(text)
}

/// One tray item as followed.
#[derive(Debug)]
struct Entry {
    bus: String,
    path: String,
    /// The unique name behind `bus` (signals come from it).
    owner: String,
    props: Props,
    menu_path: Option<String>,
    menu: Vec<TrayMenuItem>,
    /// Its connection going away.
    gone: MessageStream,
}

impl Entry {
    fn id(&self) -> String {
        format!("{}{}", self.bus, self.path)
    }

    fn item(&self) -> TrayItem {
        let id = self.id();
        let title = dbus::text(&self.props, "Title")
            .filter(|t| !t.is_empty())
            .or_else(|| dbus::text(&self.props, "Id"))
            .unwrap_or_default();
        TrayItem {
            title,
            icon: icon(&self.props),
            tooltip: tooltip(&self.props),
            status: dbus::text(&self.props, "Status").unwrap_or_else(|| "Active".into()),
            menu: TrayMenu {
                item: id.clone(),
                items: self.menu.clone(),
            },
            id,
        }
    }

    async fn read_props(&mut self, conn: &zbus::Connection) -> zbus::Result<()> {
        self.props = dbus::get_all(conn, &self.bus, &self.path, ITEM).await?;
        self.menu_path = self
            .props
            .get("Menu")
            .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath<'_>>().ok())
            .map(|p| p.to_string())
            .filter(|p| p != "/");
        Ok(())
    }

    async fn read_menu(&mut self, conn: &zbus::Connection) {
        let Some(path) = &self.menu_path else {
            self.menu.clear();
            return;
        };
        let props: Vec<&str> = Vec::new();
        let reply = conn
            .call_method(
                Some(self.bus.as_str()),
                path.as_str(),
                Some(MENU),
                "GetLayout",
                &(0i32, -1i32, props),
            )
            .await;
        let layout = reply.and_then(|r| r.body().deserialize::<(u32, Node)>());
        match layout {
            Ok((_, (_, _, children))) => {
                let id = self.id();
                self.menu = entries(&id, &children, 0);
            }
            Err(e) => {
                log::debug!("tray: no menu for {}: {e}", self.id());
                self.menu.clear();
            }
        }
    }
}

/// From the bus (our watcher, or the session's) to the body.
#[derive(Debug)]
enum Registry {
    Registered { service: String, sender: String },
}

/// Our own `org.kde.StatusNotifierWatcher`, when the session has none.
struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
    tx: mpsc::UnboundedSender<Registry>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let (bus, path) = parse_registration(service, &sender);
        let id = format!("{bus}{path}");
        {
            let Ok(mut items) = self.items.lock() else {
                return;
            };
            if items.contains(&id) {
                return;
            }
            items.push(id.clone());
        }
        let _ = Self::status_notifier_item_registered(&emitter, &id).await;
        let _ = self.tx.send(Registry::Registered {
            service: service.to_string(),
            sender,
        });
    }

    async fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().map(|i| i.clone()).unwrap_or_default()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

fn signal_rule(iface: &str) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(iface.to_string())?
        .build())
}

/// A host name of our own: `org.kde.StatusNotifierHost-<pid>-<n>`.
async fn own_host_name(conn: &zbus::Connection) -> zbus::Result<String> {
    for n in 1..100 {
        let name = format!("org.kde.StatusNotifierHost-{}-{n}", std::process::id());
        let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
        match conn
            .request_name_with_flags(name.as_str(), flags.into())
            .await
        {
            Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => return Ok(name),
            Ok(_) | Err(zbus::Error::NameTaken) => {}
            Err(e) => return Err(e),
        }
    }
    Err(zbus::Error::Failure("no free host name".into()))
}

impl Tray {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match crate::bus::own_session(cx.buses()).await {
            Ok(c) => c,
            Err(e) => return crate::battery::idle_without_bus(&mut cx, "session", e).await,
        };
        let host = own_host_name(&conn).await?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        // The session's watcher's signals (when it is not ours).
        let mut watcher_signals =
            MessageStream::for_match_rule(signal_rule(WATCHER)?, &conn, Some(64)).await?;
        let mut item_signals =
            MessageStream::for_match_rule(signal_rule(ITEM)?, &conn, Some(256)).await?;
        let mut menu_signals =
            MessageStream::for_match_rule(signal_rule(MENU)?, &conn, Some(256)).await?;
        let dbus_proxy = zbus::fdo::DBusProxy::new(&conn).await?;
        let mut watcher_owner_changes = dbus_proxy
            .receive_name_owner_changed_with_args(&[(0, WATCHER)])
            .await?;
        let shared: Arc<Mutex<Vec<String>>> = Arc::default();
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        'session: loop {
            // Be the watcher when there is none; else register with it.
            let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
            let ours = matches!(
                conn.request_name_with_flags(WATCHER, flags.into()).await,
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner)
            );
            let watcher_owner = dbus_proxy
                .get_name_owner(WATCHER.try_into()?)
                .await
                .map(|o| o.to_string())
                .ok();
            entries.clear();
            if let Ok(mut s) = shared.lock() {
                s.clear();
            }
            if ours {
                conn.object_server()
                    .at(
                        WATCHER_PATH,
                        Watcher {
                            items: shared.clone(),
                            tx: tx.clone(),
                        },
                    )
                    .await?;
                if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&conn, WATCHER_PATH) {
                    let _ = Watcher::status_notifier_host_registered(&emitter).await;
                }
            } else {
                let registered = conn
                    .call_method(
                        Some(WATCHER),
                        WATCHER_PATH,
                        Some(WATCHER),
                        "RegisterStatusNotifierHost",
                        &(host.as_str(),),
                    )
                    .await;
                if let Err(e) = registered {
                    log::warn!("tray: the session's watcher refused our host: {e}");
                }
                let listed = dbus::get(
                    &conn,
                    WATCHER,
                    WATCHER_PATH,
                    WATCHER,
                    "RegisteredStatusNotifierItems",
                )
                .await
                .ok()
                .and_then(|v| Vec::<String>::try_from(v).ok())
                .unwrap_or_default();
                for service in listed {
                    add(&conn, &mut entries, &service, "").await;
                }
            }
            if !cx.update(|s| s.items = entries.values().map(Entry::item).collect()) {
                return Ok(());
            }
            cx.ready();
            loop {
                let gone = std::future::poll_fn(|ctx| {
                    for (id, e) in entries.iter_mut() {
                        if let std::task::Poll::Ready(m) = e.gone.poll_next(ctx) {
                            return std::task::Poll::Ready((id.clone(), m.is_some()));
                        }
                    }
                    std::task::Poll::Pending
                });
                let changed = tokio::select! {
                    r = rx.recv() => match r {
                        Some(Registry::Registered { service, sender }) => {
                            add(&conn, &mut entries, &service, &sender).await
                        }
                        None => return Err(ServiceError("the tray watcher stopped".into())),
                    },
                    (id, _) = gone => {
                        // Its connection went: unregistered.
                        let removed = entries.remove(&id).is_some();
                        if ours {
                            if let Ok(mut s) = shared.lock() {
                                s.retain(|i| *i != id);
                            }
                            if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&conn, WATCHER_PATH) {
                                let _ = Watcher::status_notifier_item_unregistered(&emitter, &id).await;
                            }
                        }
                        removed
                    }
                    o = watcher_owner_changes.next() => {
                        let Some(o) = o else {
                            return Err(ServiceError("the session bus connection ended".into()));
                        };
                        let gone = o.args().is_ok_and(|a| a.new_owner().is_none());
                        if gone && !ours {
                            // The session's watcher went: be it.
                            continue 'session;
                        }
                        false
                    }
                    m = watcher_signals.next() => {
                        let Some(Ok(m)) = m else {
                            return Err(ServiceError("the session bus connection ended".into()));
                        };
                        let from_watcher = !ours
                            && m.header().sender().map(|s| s.to_string()) == watcher_owner;
                        if !from_watcher {
                            false
                        } else {
                            let service: String = m.body().deserialize().unwrap_or_default();
                            match dbus::member(&m).as_deref() {
                                Some("StatusNotifierItemRegistered") => add(&conn, &mut entries, &service, "").await,
                                Some("StatusNotifierItemUnregistered") => {
                                    let (bus, path) = parse_registration(&service, "");
                                    entries.remove(&format!("{bus}{path}")).is_some()
                                }
                                _ => false,
                            }
                        }
                    }
                    m = item_signals.next() => {
                        let Some(Ok(m)) = m else {
                            return Err(ServiceError("the session bus connection ended".into()));
                        };
                        // `NewIcon`, `NewTitle`, …: read it again.
                        match find(&mut entries, &m, false) {
                            Some(e) => {
                                let menu_before = e.menu_path.clone();
                                let read = e.read_props(&conn).await.is_ok();
                                if read && e.menu_path != menu_before {
                                    e.read_menu(&conn).await;
                                }
                                read
                            }
                            None => false,
                        }
                    }
                    m = menu_signals.next() => {
                        let Some(Ok(m)) = m else {
                            return Err(ServiceError("the session bus connection ended".into()));
                        };
                        let member = dbus::member(&m);
                        let relayout = matches!(member.as_deref(), Some("LayoutUpdated" | "ItemsPropertiesUpdated"));
                        match find(&mut entries, &m, true) {
                            Some(e) if relayout => {
                                e.read_menu(&conn).await;
                                true
                            }
                            _ => false,
                        }
                    }
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Action(a)) => {
                            act(&conn, &mut entries, a).await
                        }
                        Some(_) => false,
                    },
                };
                if changed && !cx.update(|s| s.items = entries.values().map(Entry::item).collect())
                {
                    return Ok(());
                }
            }
        }
    }
}

/// The entry a signal is about: from its owner, at its item (or menu)
/// path.
fn find<'a>(
    entries: &'a mut BTreeMap<String, Entry>,
    m: &zbus::Message,
    menu: bool,
) -> Option<&'a mut Entry> {
    let sender = m.header().sender()?.to_string();
    let path = dbus::path(m)?;
    entries.values_mut().find(|e| {
        e.owner == sender
            && if menu {
                e.menu_path.as_deref() == Some(path.as_str())
            } else {
                e.path == path
            }
    })
}

/// Read and follow a registered item. Whether it was added.
async fn add(
    conn: &zbus::Connection,
    entries: &mut BTreeMap<String, Entry>,
    service: &str,
    sender: &str,
) -> bool {
    let (bus, path) = parse_registration(service, sender);
    let id = format!("{bus}{path}");
    if entries.contains_key(&id) || bus.is_empty() {
        return false;
    }
    let Ok(dbus_proxy) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(bus.as_str()) else {
        return false;
    };
    let Ok(owner) = dbus_proxy.get_name_owner(name).await else {
        return false;
    };
    let owner = owner.to_string();
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")
        .and_then(|b| b.interface("org.freedesktop.DBus"))
        .and_then(|b| b.member("NameOwnerChanged"))
        .and_then(|b| b.arg(0, owner.as_str()))
        .map(|b| b.build());
    let Ok(rule) = rule else {
        return false;
    };
    let Ok(gone) = MessageStream::for_match_rule(rule, conn, Some(4)).await else {
        return false;
    };
    let mut e = Entry {
        bus,
        path,
        owner,
        props: Props::new(),
        menu_path: None,
        menu: Vec::new(),
        gone,
    };
    if let Err(err) = e.read_props(conn).await {
        log::debug!("tray: {id} unreadable: {err}");
        return false;
    }
    e.read_menu(conn).await;
    entries.insert(id, e);
    true
}

/// Run an action; whether the state changed.
async fn act(
    conn: &zbus::Connection,
    entries: &mut BTreeMap<String, Entry>,
    a: TrayAction,
) -> bool {
    let call = |e: &Entry, method: &'static str| (e.bus.clone(), e.path.clone(), method);
    match a {
        TrayAction::Activate { item } | TrayAction::Secondary { item }
            if !entries.contains_key(&item.id) =>
        {
            log::debug!("tray: no item {}", item.id);
            false
        }
        TrayAction::Activate { item } => {
            let Some(e) = entries.get(&item.id) else {
                return false;
            };
            let (bus, path, method) = call(e, "Activate");
            let r = conn
                .call_method(
                    Some(bus.as_str()),
                    path.as_str(),
                    Some(ITEM),
                    method,
                    &(0i32, 0i32),
                )
                .await;
            if r.is_err() {
                // Menu-only items (`ItemIsMenu`) often have no Activate.
                let _ = conn
                    .call_method(
                        Some(bus.as_str()),
                        path.as_str(),
                        Some(ITEM),
                        "ContextMenu",
                        &(0i32, 0i32),
                    )
                    .await;
            }
            false
        }
        TrayAction::Secondary { item } => {
            let Some(e) = entries.get(&item.id) else {
                return false;
            };
            let (bus, path, method) = call(e, "SecondaryActivate");
            if let Err(err) = conn
                .call_method(
                    Some(bus.as_str()),
                    path.as_str(),
                    Some(ITEM),
                    method,
                    &(0i32, 0i32),
                )
                .await
            {
                log::debug!("tray: {method} on {}: {err}", item.id);
            }
            false
        }
        TrayAction::Scroll { item, dy } => {
            let Some(e) = entries.get(&item.id) else {
                return false;
            };
            let steps = dy.round() as i32;
            let steps = if steps == 0 && dy != 0.0 {
                dy.signum() as i32
            } else {
                steps
            };
            if steps != 0 {
                let (bus, path, method) = call(e, "Scroll");
                let _ = conn
                    .call_method(
                        Some(bus.as_str()),
                        path.as_str(),
                        Some(ITEM),
                        method,
                        &(steps, "vertical"),
                    )
                    .await;
            }
            false
        }
        TrayAction::Open { item } => {
            let Some(e) = entries.get_mut(&item.item) else {
                return false;
            };
            let Some(menu) = e.menu_path.clone() else {
                // No DBusMenu: the app shows its own.
                let _ = conn
                    .call_method(
                        Some(e.bus.as_str()),
                        e.path.as_str(),
                        Some(ITEM),
                        "ContextMenu",
                        &(0i32, 0i32),
                    )
                    .await;
                return false;
            };
            let bus = e.bus.clone();
            let update = conn
                .call_method(
                    Some(bus.as_str()),
                    menu.as_str(),
                    Some(MENU),
                    "AboutToShow",
                    &(0i32,),
                )
                .await
                .and_then(|r| r.body().deserialize::<bool>())
                .unwrap_or(false);
            let _ = conn
                .call_method(
                    Some(bus.as_str()),
                    menu.as_str(),
                    Some(MENU),
                    "Event",
                    &(0i32, "opened", Value::from(0i32), 0u32),
                )
                .await;
            if update {
                e.read_menu(conn).await;
            }
            update
        }
        TrayAction::ActivateEntry { item } => {
            let Some(e) = entries.get(&item.item) else {
                return false;
            };
            let Some(menu) = e.menu_path.clone() else {
                return false;
            };
            let r = conn
                .call_method(
                    Some(e.bus.as_str()),
                    menu.as_str(),
                    Some(MENU),
                    "Event",
                    &(item.id as i32, "clicked", Value::from(0i32), 0u32),
                )
                .await;
            if let Err(err) = r {
                log::debug!("tray: menu entry {} of {}: {err}", item.id, item.item);
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_and_labels_parse() {
        assert_eq!(
            parse_registration(":1.4", ""),
            (":1.4".to_string(), ITEM_PATH.to_string())
        );
        assert_eq!(
            parse_registration("/org/ayatana/NotificationItem/x", ":1.9"),
            (
                ":1.9".to_string(),
                "/org/ayatana/NotificationItem/x".to_string()
            )
        );
        assert_eq!(
            parse_registration(":1.9/org/x", ""),
            (":1.9".to_string(), "/org/x".to_string())
        );
        assert_eq!(strip_mnemonic("_Open"), "Open");
        assert_eq!(strip_mnemonic("Save__As"), "Save_As");
    }
}
