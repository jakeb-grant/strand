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
//! **One app cannot hold up the tray.** Apps freeze (a blocked main loop,
//! a modal dialog opened before replying), so nothing the service asks an
//! item waits in its loop: every read and action is a task of its own
//! (dropped with the service), each call given up after
//! [`dbus::CALL_TIMEOUT`], and reads come back as results the loop
//! applies. An item has at most one property read and one menu read in
//! flight; signals arriving meanwhile ask for one more read after it (an
//! animated icon is not a queue of reads).
//!
//! Icons: an icon name (looked up first in the item's `IconThemePath`),
//! else its largest pixmap written as a PNG file (`image` shows both);
//! `NeedsAttention` shows the attention icon when the item has one. They
//! are resolved off the runtime thread when the item's properties are
//! read, and kept with the item until it changes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_lite::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use zbus::fdo::RequestNameReply;
use zbus::message::Type as MessageType;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{MatchRule, MessageStream};

use crate::dbus::{self, Props, timed};
use crate::pixmap::Pinned;
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
    pub shortcut: Option<String>,
    pub children: Vec<TrayMenuItem>,
}

/// A tray item's menu.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "TrayMenu")]
pub struct TrayMenu {
    pub item: String,
    pub items: Vec<TrayMenuItem>,
    pub opened: bool,
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
    /// `item.scroll(dy)`: `dy` in wheel notches (one click is 1, positive
    /// down), sent to the app as 120 per notch, positive up
    /// ([`scroll_delta`]).
    Scroll { item: TrayItem, dy: f64 },
    /// `item.menu.open()`.
    Open { item: TrayMenu },
    /// `item.menu.close()`.
    Close { item: TrayMenu },
    /// `entry.activate()` for an entry of a menu.
    #[call(name = "activate")]
    ActivateEntry { item: TrayMenuItem },
    /// `entry.open()`: a submenu opens.
    #[call(name = "open")]
    OpenEntry { item: TrayMenuItem },
}

/// See the module docs.
#[service(name = "tray", action = TrayAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Tray {
    /// The tray's items, keyed by `id`: `for item in tray.items`.
    #[store(keyed)]
    pub items: Vec<TrayItem>,
}

/// What the SNI `Scroll` delta counts per wheel notch: angle units of an
/// eighth of a degree, 15 degrees a notch (Qt's wheel `angleDelta`, which
/// KDE's host sends; apps that read only the sign work the same).
pub const SCROLL_NOTCH: f64 = 120.0;

/// How long after a failed first read of a registered item it is read
/// once more (an app registering before it exported its item, or busy
/// as it starts).
pub const RETRY_READ: std::time::Duration = std::time::Duration::from_secs(2);

/// The SNI `Scroll` delta for `dy` notches (a shell's `on scroll(dy)`:
/// positive is down): 120 a notch, positive up as KDE's host sends it (a
/// wheel's `angleDelta`), at least one unit in its direction.
pub fn scroll_delta(dy: f64) -> i32 {
    if !dy.is_finite() || dy == 0.0 {
        return 0;
    }
    let d = (-dy * SCROLL_NOTCH)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    if d == 0 { -(dy.signum() as i32) } else { d }
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

/// A DBusMenu `shortcut` (`aas`: each a key combination, e.g.
/// `[["Control", "S"]]`) as text (`Control+S`; combinations comma
/// separated).
fn shortcut(props: &Props) -> Option<String> {
    let v = props.get("shortcut")?.try_clone().ok()?;
    let combos: Vec<Vec<String>> = v.try_into().ok()?;
    let text = combos
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| c.join("+"))
        .collect::<Vec<_>>()
        .join(", ");
    (!text.is_empty()).then_some(text)
}

/// One menu node of a `GetLayout` reply: `(ia{sv}av)`.
type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// A DBusMenu node's entries (its visible children), for tray item
/// `item`; the files of entries' `icon-data` go to `pins`.
fn entries(
    item: &str,
    children: &[OwnedValue],
    depth: usize,
    pins: &mut Vec<Pinned>,
) -> Vec<TrayMenuItem> {
    if depth > 8 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for c in children {
        let Some(v) = c.try_clone().ok().map(Value::from) else {
            continue;
        };
        let v = match v {
            Value::Value(inner) => *inner,
            v => v,
        };
        let Ok((id, props, kids)) = Node::try_from(v) else {
            continue;
        };
        let props: Props = props;
        if dbus::boolean(&props, "visible") == Some(false) {
            continue;
        }
        let kind = dbus::text(&props, "type").unwrap_or_else(|| "standard".into());
        let toggle = match dbus::text(&props, "toggle-type").as_deref() {
            Some("checkmark") => "checkmark",
            Some("radio") => "radio",
            _ => "none",
        };
        // An icon name, else the PNG the app sent.
        let icon = dbus::text(&props, "icon-name")
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let data: Vec<u8> = props.get("icon-data")?.try_clone().ok()?.try_into().ok()?;
                let pinned = crate::pixmap::write_png(&data)
                    .map_err(|e| log::debug!("tray: menu icon of {item} not shown: {e}"))
                    .ok()?;
                let text = pinned.text();
                pins.push(pinned);
                Some(text)
            });
        out.push(TrayMenuItem {
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
            icon,
            shortcut: shortcut(&props),
            children: entries(item, &kids, depth + 1, pins),
        });
    }
    out
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
fn pixmap_icon(props: &Props, key: &str) -> Option<Pinned> {
    let v = props.get(key)?.try_clone().ok()?;
    let list: Vec<(i32, i32, Vec<u8>)> = v.try_into().ok()?;
    let (w, h, data) = list
        .into_iter()
        .filter(|(w, h, _)| *w > 0 && *h > 0)
        .max_by_key(|(w, h, _)| i64::from(*w) * i64::from(*h))?;
    let written = crate::pixmap::from_argb32(w, h, &data)
        .and_then(|(w, h, rgba)| crate::pixmap::write_rgba(w, h, &rgba));
    match written {
        Ok(p) => Some(p),
        Err(e) => {
            log::debug!("tray: pixmap not shown: {e}");
            None
        }
    }
}

/// The icon to show for an item's properties (and the file it wrote, if
/// any). Blocking: file lookups and PNG encoding.
fn icon(props: &Props) -> (String, Option<Pinned>) {
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
    let pixmap = |key: &str| pixmap_icon(props, key).map(|p| (p.text(), Some(p)));
    let attention = dbus::text(props, "Status").as_deref() == Some("NeedsAttention");
    let attention_icon = attention
        .then(|| {
            named("AttentionIconName")
                .map(|n| (n, None))
                .or_else(|| pixmap("AttentionIconPixmap"))
        })
        .flatten();
    attention_icon
        .or_else(|| named("IconName").map(|n| (n, None)))
        .or_else(|| pixmap("IconPixmap"))
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

/// What an item's properties show, resolved once per read.
#[derive(Debug, Default)]
struct Look {
    title: String,
    icon: String,
    /// The icon's file, when it is pixels the item sent.
    _pin: Option<Pinned>,
    tooltip: Option<String>,
    status: String,
    menu_path: Option<String>,
    /// `ItemIsMenu`: the item only shows a menu (libappindicator and
    /// ayatana items); a click opens it.
    is_menu: bool,
}

impl Look {
    /// Blocking (icon lookups, PNG encoding): run off the runtime thread.
    fn of(props: &Props) -> Look {
        let (icon, pin) = icon(props);
        Look {
            title: dbus::text(props, "Title")
                .filter(|t| !t.is_empty())
                .or_else(|| dbus::text(props, "Id"))
                .unwrap_or_default(),
            icon,
            _pin: pin,
            tooltip: tooltip(props),
            status: dbus::text(props, "Status").unwrap_or_else(|| "Active".into()),
            menu_path: props
                .get("Menu")
                .and_then(|v| v.downcast_ref::<zbus::zvariant::ObjectPath<'_>>().ok())
                .map(|p| p.to_string())
                .filter(|p| p != "/"),
            is_menu: props
                .get("ItemIsMenu")
                .and_then(|v| v.downcast_ref::<bool>().ok())
                .unwrap_or(false),
        }
    }
}

/// An item's menu as read, with the files of its entries' icons.
#[derive(Debug, Default)]
struct MenuModel {
    items: Vec<TrayMenuItem>,
    _pins: Vec<Pinned>,
}

/// A read in flight, and whether another is wanted after it.
#[derive(Debug, Default, Clone, Copy)]
struct Reading {
    busy: bool,
    again: bool,
}

/// One tray item as followed.
#[derive(Debug)]
struct Entry {
    bus: String,
    path: String,
    /// The unique name behind `bus` (signals come from it).
    owner: String,
    /// Tells this entry's reads from those of an entry of the same id
    /// before it.
    generation: u64,
    look: Look,
    menu: MenuModel,
    props_read: Reading,
    menu_read: Reading,
    /// Its menu is open (`item.menu.open()` until `close()`).
    menu_open: bool,
    /// Its connection going away.
    gone: MessageStream,
}

impl Entry {
    fn id(&self) -> String {
        format!("{}{}", self.bus, self.path)
    }

    fn item(&self) -> TrayItem {
        let id = self.id();
        TrayItem {
            title: self.look.title.clone(),
            icon: self.look.icon.clone(),
            tooltip: self.look.tooltip.clone(),
            status: self.look.status.clone(),
            menu: TrayMenu {
                item: id.clone(),
                items: self.menu.items.clone(),
                opened: self.menu_open,
            },
            id,
        }
    }

    fn target(&self) -> Target {
        Target {
            id: self.id(),
            generation: self.generation,
            bus: self.bus.clone(),
            path: self.path.clone(),
            menu_path: self.look.menu_path.clone(),
            is_menu: self.look.is_menu,
        }
    }
}

/// Where a task's calls go: an item, as it was when the task started.
#[derive(Clone, Debug)]
struct Target {
    id: String,
    generation: u64,
    bus: String,
    path: String,
    menu_path: Option<String>,
    is_menu: bool,
}

/// An item's properties, read and resolved (`GetAll`, then the icon off
/// the runtime thread).
async fn read_look(conn: &zbus::Connection, bus: &str, path: &str) -> zbus::Result<Look> {
    let props = timed(dbus::get_all(conn, bus, path, ITEM)).await?;
    tokio::task::spawn_blocking(move || Look::of(&props))
        .await
        .map_err(|e| zbus::Error::Failure(format!("icon lookup failed: {e}")))
}

/// An item's menu (`GetLayout`; empty without one, or when it does not
/// answer).
async fn read_menu(conn: &zbus::Connection, t: &Target) -> MenuModel {
    let Some(path) = &t.menu_path else {
        return MenuModel::default();
    };
    let props: Vec<&str> = Vec::new();
    let reply = timed(conn.call_method(
        Some(t.bus.as_str()),
        path.as_str(),
        Some(MENU),
        "GetLayout",
        &(0i32, -1i32, props),
    ))
    .await;
    let layout = reply.and_then(|r| r.body().deserialize::<(u32, Node)>());
    match layout {
        Ok((_, (_, _, children))) => {
            let id = t.id.clone();
            tokio::task::spawn_blocking(move || {
                let mut pins = Vec::new();
                let items = entries(&id, &children, 0, &mut pins);
                MenuModel { items, _pins: pins }
            })
            .await
            .unwrap_or_default()
        }
        Err(e) => {
            log::debug!("tray: no menu for {}: {e}", t.id);
            MenuModel::default()
        }
    }
}

/// What a task hands back to the loop.
#[derive(Debug)]
enum Done {
    /// A registered item, read (`None`: it could not be). `retry`: this
    /// was its second read.
    Added {
        id: String,
        bus: String,
        path: String,
        entry: Option<Box<Entry>>,
        retry: bool,
    },
    /// An item's properties read again.
    Look(String, u64, Option<Look>),
    /// An item's menu read again.
    Menu(String, u64, MenuModel),
    /// An action asked for the menu to be read again (the app updated it
    /// before showing it).
    Relayout(String, u64),
    /// `Activate` was refused by an item with a DBusMenu: open the menu.
    OpenMenu(String, u64),
    /// An action was sent.
    Sent,
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
        let _ = self
            .registered_status_notifier_items_changed(&emitter)
            .await;
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

/// Our watcher lost item `id`: unlisted, and the bus told.
async fn unregistered(conn: &zbus::Connection, id: &str) {
    let Ok(iface) = conn
        .object_server()
        .interface::<_, Watcher>(WATCHER_PATH)
        .await
    else {
        return;
    };
    let emitter = iface.signal_emitter();
    let watcher = iface.get().await;
    if let Ok(mut s) = watcher.items.lock() {
        s.retain(|i| i != id);
    }
    let _ = Watcher::status_notifier_item_unregistered(emitter, id).await;
    let _ = watcher
        .registered_status_notifier_items_changed(emitter)
        .await;
}

fn signal_rule(iface: &str) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(iface.to_string())?
        .build())
}

/// A host name of our own: `org.kde.StatusNotifierHost-<pid>-<n>`.
/// A host serves no interface, but the connection's object server is set
/// up first so the watcher's interface is never late for its name.
async fn own_host_name(conn: &zbus::Connection) -> zbus::Result<String> {
    let _ = conn.object_server();
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

/// The loop's own state besides the connection's streams.
struct Host {
    conn: zbus::Connection,
    entries: BTreeMap<String, Entry>,
    /// Items being read before they are added.
    adding: HashSet<String>,
    /// Items given up on (unreadable twice), for the watcher to unlist.
    failed: Vec<String>,
    tasks: JoinSet<Done>,
    generation: u64,
}

impl Host {
    /// Read and follow a registered item, in a task of its own.
    fn add(&mut self, service: &str, sender: &str) {
        let (bus, path) = parse_registration(service, sender);
        let id = format!("{bus}{path}");
        if bus.is_empty() || self.entries.contains_key(&id) || !self.adding.insert(id.clone()) {
            return;
        }
        self.read_new(id, bus, path, false);
    }

    /// Read item `id` (being added) in a task; a retry waits
    /// [`RETRY_READ`] first.
    fn read_new(&mut self, id: String, bus: String, path: String, retry: bool) {
        self.generation += 1;
        let generation = self.generation;
        let conn = self.conn.clone();
        self.tasks.spawn(async move {
            if retry {
                tokio::time::sleep(RETRY_READ).await;
            }
            let entry = new_entry(&conn, bus.clone(), path.clone(), generation).await;
            Done::Added {
                id,
                bus,
                path,
                entry: entry.map(Box::new),
                retry,
            }
        });
    }

    /// Read an item's properties again (one read at a time).
    fn reread(&mut self, id: &str) {
        let Some(e) = self.entries.get_mut(id) else {
            return;
        };
        if e.props_read.busy {
            e.props_read.again = true;
            return;
        }
        e.props_read = Reading {
            busy: true,
            again: false,
        };
        let t = e.target();
        let conn = self.conn.clone();
        self.tasks.spawn(async move {
            let look = read_look(&conn, &t.bus, &t.path).await;
            if let Err(err) = &look {
                log::debug!("tray: {} unreadable: {err}", t.id);
            }
            Done::Look(t.id, t.generation, look.ok())
        });
    }

    /// Read an item's menu again (one read at a time).
    fn relayout(&mut self, id: &str) {
        let Some(e) = self.entries.get_mut(id) else {
            return;
        };
        if e.menu_read.busy {
            e.menu_read.again = true;
            return;
        }
        e.menu_read = Reading {
            busy: true,
            again: false,
        };
        let t = e.target();
        let conn = self.conn.clone();
        self.tasks.spawn(async move {
            let menu = read_menu(&conn, &t).await;
            Done::Menu(t.id, t.generation, menu)
        });
    }

    /// The entry `id` of `generation`, if it is still the one followed.
    fn current(&mut self, id: &str, generation: u64) -> Option<&mut Entry> {
        self.entries
            .get_mut(id)
            .filter(|e| e.generation == generation)
    }

    /// Apply a task's result; whether the state changed.
    fn done(&mut self, d: Done) -> bool {
        match d {
            Done::Added {
                id,
                bus,
                path,
                entry,
                retry,
            } => match entry {
                Some(e) => {
                    self.adding.remove(&id);
                    self.entries.insert(id, *e);
                    true
                }
                // Read once more a little later (still being added: a
                // registration meanwhile is the same one).
                None if !retry => {
                    self.read_new(id, bus, path, true);
                    false
                }
                // Given up: our watcher lists it no more, so the app
                // registering again is heard.
                None => {
                    self.adding.remove(&id);
                    self.failed.push(id);
                    false
                }
            },
            Done::Look(id, generation, look) => {
                let Some(e) = self.current(&id, generation) else {
                    return false;
                };
                let again = e.props_read.again;
                e.props_read = Reading::default();
                let mut changed = false;
                if let Some(look) = look {
                    let menu_moved = look.menu_path != e.look.menu_path;
                    e.look = look;
                    changed = true;
                    if menu_moved {
                        self.relayout(&id);
                    }
                }
                if again {
                    self.reread(&id);
                }
                changed
            }
            Done::Menu(id, generation, menu) => {
                let Some(e) = self.current(&id, generation) else {
                    return false;
                };
                let again = e.menu_read.again;
                e.menu_read = Reading::default();
                let changed = e.menu.items != menu.items;
                e.menu = menu;
                if again {
                    self.relayout(&id);
                }
                changed
            }
            Done::Relayout(id, generation) => {
                if self.current(&id, generation).is_some() {
                    self.relayout(&id);
                }
                false
            }
            Done::OpenMenu(id, generation) => {
                self.current(&id, generation).is_some() && self.open_menu(&id)
            }
            Done::Sent => false,
        }
    }

    /// The entry a signal is about: from its owner, at its item (or menu)
    /// path.
    fn find(&self, m: &zbus::Message, menu: bool) -> Option<String> {
        let sender = m.header().sender()?.to_string();
        let path = dbus::path(m)?;
        self.entries
            .iter()
            .find(|(_, e)| {
                e.owner == sender
                    && if menu {
                        e.look.menu_path.as_deref() == Some(path.as_str())
                    } else {
                        e.path == path
                    }
            })
            .map(|(id, _)| id.clone())
    }

    /// Send a call to an item in a task of its own, given up after
    /// [`dbus::CALL_TIMEOUT`]; `then` is what the loop does with its
    /// reply.
    fn send<B>(
        &mut self,
        t: &Target,
        path: &str,
        iface: &'static str,
        method: &'static str,
        body: B,
        then: Then,
    ) where
        B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
    {
        let conn = self.conn.clone();
        let t = t.clone();
        let path = path.to_string();
        self.tasks.spawn(async move {
            let r = timed(conn.call_method(
                Some(t.bus.as_str()),
                path.as_str(),
                Some(iface),
                method,
                &body,
            ))
            .await;
            match (r, then) {
                (Ok(reply), Then::RelayoutIfTrue) => {
                    if reply.body().deserialize::<bool>().unwrap_or(false) {
                        return Done::Relayout(t.id, t.generation);
                    }
                }
                // Refused (a menu-only item not marked `ItemIsMenu`): its
                // DBusMenu opens in the shell's popup, as for
                // `item.menu.open()`. An app that did not answer in time
                // (frozen) is asked nothing more.
                (Err(zbus::Error::MethodError(..)), Then::MenuOnError) if t.menu_path.is_some() => {
                    return Done::OpenMenu(t.id, t.generation);
                }
                // Without one the app shows its own.
                (Err(zbus::Error::MethodError(..)), Then::MenuOnError) => {
                    let _ = timed(conn.call_method(
                        Some(t.bus.as_str()),
                        t.path.as_str(),
                        Some(ITEM),
                        "ContextMenu",
                        &(0i32, 0i32),
                    ))
                    .await;
                }
                (Err(e), _) => log::debug!("tray: {method} on {}: {e}", t.id),
                _ => {}
            }
            Done::Sent
        });
    }

    /// Run an action (each call a task of its own); whether the state
    /// changed (a menu opened or closed).
    fn act(&mut self, a: TrayAction) -> bool {
        let target = |id: &str| self.entries.get(id).map(Entry::target);
        match a {
            TrayAction::Activate { item } => {
                let Some(t) = target(&item.id) else {
                    return false;
                };
                let path = t.path.clone();
                match (t.is_menu, &t.menu_path) {
                    // `ItemIsMenu` (the SNI spec: the host shows the menu
                    // on activation, as KDE and waybar do): its DBusMenu
                    // opens, as `item.menu.open()` does.
                    (true, Some(_)) => return self.open_menu(&item.id),
                    (true, None) => {
                        self.send(&t, &path, ITEM, "ContextMenu", (0i32, 0i32), Then::Nothing);
                    }
                    (false, _) => {
                        self.send(&t, &path, ITEM, "Activate", (0i32, 0i32), Then::MenuOnError);
                    }
                }
            }
            TrayAction::Secondary { item } => {
                if let Some(t) = target(&item.id) {
                    let path = t.path.clone();
                    self.send(
                        &t,
                        &path,
                        ITEM,
                        "SecondaryActivate",
                        (0i32, 0i32),
                        Then::Nothing,
                    );
                }
            }
            TrayAction::Scroll { item, dy } => {
                let steps = scroll_delta(dy);
                if steps != 0
                    && let Some(t) = target(&item.id)
                {
                    let path = t.path.clone();
                    self.send(
                        &t,
                        &path,
                        ITEM,
                        "Scroll",
                        (steps, "vertical"),
                        Then::Nothing,
                    );
                }
            }
            TrayAction::Open { item } => {
                let Some(t) = target(&item.item) else {
                    return false;
                };
                match t.menu_path.clone() {
                    // No DBusMenu: the app shows its own.
                    None => {
                        let path = t.path.clone();
                        self.send(&t, &path, ITEM, "ContextMenu", (0i32, 0i32), Then::Nothing);
                    }
                    Some(_) => return self.open_menu(&item.item),
                }
            }
            TrayAction::OpenEntry { item } => {
                if let Some(t) = target(&item.item)
                    && let Some(menu) = t.menu_path.clone()
                {
                    self.opened(&t, &menu, item.id as i32);
                }
            }
            TrayAction::Close { item } => {
                if let Some(t) = target(&item.item)
                    && let Some(menu) = t.menu_path.clone()
                {
                    self.event(&t, &menu, 0, "closed");
                }
                if let Some(e) = self.entries.get_mut(&item.item)
                    && e.menu_open
                {
                    e.menu_open = false;
                    return true;
                }
            }
            TrayAction::ActivateEntry { item } => {
                if let Some(t) = target(&item.item)
                    && let Some(menu) = t.menu_path.clone()
                {
                    self.event(&t, &menu, item.id as i32, "clicked");
                }
            }
        }
        false
    }

    /// Item `id`'s DBusMenu opens: the app is told (`AboutToShow`,
    /// `opened`) and the shell's popup shows it (`open:
    /// item.menu.opened`). Whether the state changed.
    fn open_menu(&mut self, id: &str) -> bool {
        let Some(t) = self.entries.get(id).map(Entry::target) else {
            return false;
        };
        let Some(menu) = t.menu_path.clone() else {
            return false;
        };
        self.opened(&t, &menu, 0);
        match self.entries.get_mut(id) {
            Some(e) if !e.menu_open => {
                e.menu_open = true;
                true
            }
            _ => false,
        }
    }

    /// Menu entry `id` (0: the root) opens: the app may update it first
    /// (`AboutToShow` answering true: read again), and is told.
    fn opened(&mut self, t: &Target, menu: &str, id: i32) {
        self.send(t, menu, MENU, "AboutToShow", (id,), Then::RelayoutIfTrue);
        self.event(t, menu, id, "opened");
    }

    /// A DBusMenu `Event` for entry `id`.
    fn event(&mut self, t: &Target, menu: &str, id: i32, what: &'static str) {
        self.send(
            t,
            menu,
            MENU,
            "Event",
            (id, what, Value::from(0i32), 0u32),
            Then::Nothing,
        );
    }
}

/// What the loop does with a sent call's reply.
#[derive(Clone, Copy, Debug)]
enum Then {
    Nothing,
    /// `AboutToShow`: read the menu again when it answers true.
    RelayoutIfTrue,
    /// `Activate`: open the DBusMenu when refused, else `ContextMenu`.
    MenuOnError,
}

/// A registered item, read: its owner followed, its properties and menu
/// read (each call with its timeout). `None` when it cannot be.
async fn new_entry(
    conn: &zbus::Connection,
    bus: String,
    path: String,
    generation: u64,
) -> Option<Entry> {
    let id = format!("{bus}{path}");
    let dbus_proxy = zbus::fdo::DBusProxy::new(conn).await.ok()?;
    let name = zbus::names::BusName::try_from(bus.as_str()).ok()?;
    let owner = timed(async { Ok(dbus_proxy.get_name_owner(name).await?) })
        .await
        .ok()?
        .to_string();
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")
        .and_then(|b| b.interface("org.freedesktop.DBus"))
        .and_then(|b| b.member("NameOwnerChanged"))
        .and_then(|b| b.arg(0, owner.as_str()))
        .map(|b| b.build())
        .ok()?;
    let gone = MessageStream::for_match_rule(rule, conn, Some(4))
        .await
        .ok()?;
    let look = match read_look(conn, &bus, &path).await {
        Ok(l) => l,
        Err(err) => {
            log::debug!("tray: {id} unreadable: {err}");
            return None;
        }
    };
    let mut e = Entry {
        bus,
        path,
        owner,
        generation,
        look,
        menu: MenuModel::default(),
        props_read: Reading::default(),
        menu_read: Reading::default(),
        menu_open: false,
        gone,
    };
    e.menu = read_menu(conn, &e.target()).await;
    Some(e)
}

impl Tray {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let conn = match crate::bus::own_session(cx.buses()).await {
            Ok(c) => c,
            Err(e) => return crate::dbus::idle_without_bus(&mut cx, "session", e).await,
        };
        let host_name = own_host_name(&conn).await?;
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
        // Dropped with the body: no task outlives the service.
        let mut host = Host {
            conn: conn.clone(),
            entries: BTreeMap::new(),
            adding: HashSet::new(),
            failed: Vec::new(),
            tasks: JoinSet::new(),
            generation: 0,
        };
        'session: loop {
            // Be the watcher when there is none; else register with it.
            // The interface is served before the name is asked for: an
            // app registering as soon as the name appears must find it.
            if let Ok(mut s) = shared.lock() {
                s.clear();
            }
            let served = conn
                .object_server()
                .at(
                    WATCHER_PATH,
                    Watcher {
                        items: shared.clone(),
                        tx: tx.clone(),
                    },
                )
                .await?;
            let flags = zbus::fdo::RequestNameFlags::DoNotQueue;
            let ours = matches!(
                conn.request_name_with_flags(WATCHER, flags.into()).await,
                Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner)
            );
            if !ours && served {
                conn.object_server()
                    .remove::<Watcher, _>(WATCHER_PATH)
                    .await?;
            }
            let watcher_owner = dbus_proxy
                .get_name_owner(WATCHER.try_into()?)
                .await
                .map(|o| o.to_string())
                .ok();
            host.entries.clear();
            host.adding.clear();
            host.failed.clear();
            if ours {
                if let Ok(emitter) = zbus::object_server::SignalEmitter::new(&conn, WATCHER_PATH) {
                    let _ = Watcher::status_notifier_host_registered(&emitter).await;
                }
            } else {
                let registered = timed(conn.call_method(
                    Some(WATCHER),
                    WATCHER_PATH,
                    Some(WATCHER),
                    "RegisterStatusNotifierHost",
                    &(host_name.as_str(),),
                ))
                .await;
                if let Err(e) = registered {
                    log::warn!("tray: the session's watcher refused our host: {e}");
                }
                let listed = timed(dbus::get(
                    &conn,
                    WATCHER,
                    WATCHER_PATH,
                    WATCHER,
                    "RegisteredStatusNotifierItems",
                ))
                .await
                .ok()
                .and_then(|v| Vec::<String>::try_from(v).ok())
                .unwrap_or_default();
                for service in listed {
                    host.add(&service, "");
                }
                // The items listed are shown at once (each read bounded by
                // its timeouts).
                while !host.adding.is_empty() {
                    match host.tasks.join_next().await {
                        Some(Ok(d)) => {
                            host.done(d);
                        }
                        Some(Err(_)) => {}
                        None => break,
                    }
                }
            }
            if !cx.update(|s| s.items = host.entries.values().map(Entry::item).collect()) {
                return Ok(());
            }
            cx.ready();
            loop {
                let entries = &mut host.entries;
                let gone = std::future::poll_fn(|ctx| {
                    for (id, e) in entries.iter_mut() {
                        if let std::task::Poll::Ready(m) = e.gone.poll_next(ctx) {
                            return std::task::Poll::Ready((id.clone(), m.is_some()));
                        }
                    }
                    std::task::Poll::Pending
                });
                let changed = tokio::select! {
                    Some(d) = host.tasks.join_next(), if !host.tasks.is_empty() => {
                        let changed = match d {
                            Ok(d) => host.done(d),
                            Err(_) => false,
                        };
                        for id in std::mem::take(&mut host.failed) {
                            if ours {
                                unregistered(&conn, &id).await;
                            }
                        }
                        changed
                    }
                    r = rx.recv() => match r {
                        Some(Registry::Registered { service, sender }) => {
                            host.add(&service, &sender);
                            false
                        }
                        None => return Err(ServiceError("the tray watcher stopped".into())),
                    },
                    (id, _) = gone => {
                        // Its connection went: unregistered.
                        let removed = host.entries.remove(&id).is_some();
                        if ours {
                            unregistered(&conn, &id).await;
                        }
                        removed
                    }
                    o = watcher_owner_changes.next() => {
                        if o.is_none() {
                            return Err(ServiceError("the session bus connection ended".into()));
                        }
                        // The session's watcher went or was replaced: be
                        // it, or register with the new one.
                        if !ours {
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
                                Some("StatusNotifierItemRegistered") => {
                                    host.add(&service, "");
                                    false
                                }
                                Some("StatusNotifierItemUnregistered") => {
                                    let (bus, path) = parse_registration(&service, "");
                                    host.entries.remove(&format!("{bus}{path}")).is_some()
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
                        if let Some(id) = host.find(&m, false) {
                            host.reread(&id);
                        }
                        false
                    }
                    m = menu_signals.next() => {
                        let Some(Ok(m)) = m else {
                            return Err(ServiceError("the session bus connection ended".into()));
                        };
                        let member = dbus::member(&m);
                        let relayout = matches!(member.as_deref(), Some("LayoutUpdated" | "ItemsPropertiesUpdated"));
                        if relayout && let Some(id) = host.find(&m, true) {
                            host.relayout(&id);
                        }
                        false
                    }
                    m = cx.recv() => match m {
                        None => return Ok(()),
                        Some(Msg::Action(a)) => host.act(a),
                        Some(_) => false,
                    },
                };
                if changed
                    && !cx.update(|s| s.items = host.entries.values().map(Entry::item).collect())
                {
                    return Ok(());
                }
            }
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
        // A notch down is -120 (KDE: positive up); up is +120.
        assert_eq!(scroll_delta(1.0), -120);
        assert_eq!(scroll_delta(-2.0), 240);
        assert_eq!(scroll_delta(0.25), -30);
        assert_eq!(scroll_delta(-0.001), 1, "at least one unit");
        assert_eq!(scroll_delta(0.0), 0);
        assert_eq!(scroll_delta(f64::NAN), 0);
        assert_eq!(scroll_delta(-1e30), i32::MAX);
        assert_eq!(strip_mnemonic("_Open"), "Open");
        assert_eq!(strip_mnemonic("Save__As"), "Save_As");
        let mut p = Props::new();
        p.insert(
            "shortcut".into(),
            OwnedValue::try_from(Value::from(vec![vec!["Control", "S"]])).unwrap(),
        );
        assert_eq!(shortcut(&p).as_deref(), Some("Control+S"));
    }
}
