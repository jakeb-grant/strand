//! `tray` against small zbus StatusNotifierItems with DBusMenus on a
//! private bus: the service as the watcher when the session has none,
//! and as a host of the session's watcher when it has one; items' icons
//! (names and pixmaps), tooltips, activation, scrolling, and their menus
//! (layout, updates, opening, choosing an entry).

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_core::Runtime;
use strand_services::tray::{TrayItem, TrayMenuItem, WATCHER, WATCHER_PATH};
use strand_services::{Data, ToData};
use support::*;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// A tooltip: icon name, pixmaps, title, description.
type Tip = (String, Vec<(i32, i32, Vec<u8>)>, String, String);
/// A DBusMenu layout: revision, then the root `(ia{sv}av)`.
type Layout = (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>));

#[derive(Debug, Default)]
struct State {
    icon_name: String,
    pixmap: Vec<(i32, i32, Vec<u8>)>,
    calls: Vec<String>,
    open_label: String,
    extra: bool,
    /// Its main loop is stuck: `Activate` never answers.
    frozen: bool,
    /// `ItemIsMenu`.
    is_menu: bool,
    /// `Activate` is refused (libappindicator items).
    no_activate: bool,
}

struct Item(Arc<Mutex<State>>);

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    async fn activate(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        let (frozen, refused) = {
            let mut s = self.0.lock().unwrap();
            s.calls.push("Activate".into());
            (s.frozen, s.no_activate)
        };
        if frozen {
            std::future::pending::<()>().await;
        }
        if refused {
            return Err(zbus::fdo::Error::UnknownMethod("no Activate".into()));
        }
        Ok(())
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push("SecondaryActivate".into());
    }
    fn scroll(&self, delta: i32, orientation: &str) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(format!("Scroll {delta} {orientation}"));
    }
    fn context_menu(&self, _x: i32, _y: i32) {
        self.0.lock().unwrap().calls.push("ContextMenu".into());
    }
    #[zbus(property)]
    fn id(&self) -> String {
        "mock".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        "Mock".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.0.lock().unwrap().icon_name.clone()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.0.lock().unwrap().pixmap.clone()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> Tip {
        (String::new(), Vec::new(), "Tip".into(), "More".into())
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/Menu").unwrap()
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        self.0.lock().unwrap().is_menu
    }
    #[zbus(signal)]
    async fn new_icon(e: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct Menu(Arc<Mutex<State>>);

fn node(
    id: i32,
    props: &[(&str, Value<'static>)],
    children: Vec<Value<'static>>,
) -> Value<'static> {
    let props: HashMap<String, Value<'static>> = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.try_clone().unwrap()))
        .collect();
    Value::from((id, props, children))
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(&self, _parent: i32, _depth: i32, _props: Vec<String>) -> Layout {
        let s = self.0.lock().unwrap();
        let mut children = vec![
            node(1, &[("label", Value::from(s.open_label.clone()))], vec![]),
            node(2, &[("type", Value::from("separator"))], vec![]),
            node(
                3,
                &[
                    ("label", Value::from("_Check")),
                    ("toggle-type", Value::from("checkmark")),
                    ("toggle-state", Value::from(1i32)),
                ],
                vec![],
            ),
            node(
                4,
                &[
                    ("label", Value::from("Sub")),
                    ("children-display", Value::from("submenu")),
                ],
                vec![node(
                    5,
                    &[
                        ("label", Value::from("Child")),
                        ("enabled", Value::from(false)),
                    ],
                    vec![],
                )],
            ),
            node(
                6,
                &[
                    ("label", Value::from("Hidden")),
                    ("visible", Value::from(false)),
                ],
                vec![],
            ),
        ];
        if s.extra {
            children.push(node(7, &[("label", Value::from("Extra"))], vec![]));
        }
        let children = children
            .into_iter()
            .map(|c| OwnedValue::try_from(c).unwrap())
            .collect();
        (1, (0, HashMap::new(), children))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.0
            .lock()
            .unwrap()
            .calls
            .push(format!("Event {id} {event_id}"));
    }

    fn about_to_show(&self, id: i32) -> bool {
        let mut s = self.0.lock().unwrap();
        s.calls.push("AboutToShow".into());
        s.calls.push(format!("AboutToShow {id}"));
        let first = !s.extra;
        s.extra = true;
        first
    }

    #[zbus(signal)]
    async fn layout_updated(e: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
}

/// An item serving at `/StatusNotifierItem` under `name`, not registered
/// yet.
fn item(
    tokio: &tokio::runtime::Runtime,
    address: &str,
    name: &str,
) -> (zbus::Connection, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        icon_name: "mock-icon".into(),
        open_label: "_Open".into(),
        ..State::default()
    }));
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .name(name)
            .unwrap()
            .serve_at("/StatusNotifierItem", Item(state.clone()))
            .unwrap()
            .serve_at("/Menu", Menu(state.clone()))
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    (conn, state)
}

fn items(b: &strand_services::Builtin, rt: &Runtime) -> Vec<TrayItem> {
    b.tray
        .cells()
        .items
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, i)| i.clone())
        .collect()
}

fn labels(menu: &[TrayMenuItem]) -> Vec<String> {
    menu.iter().map(|e| e.label.clone()).collect()
}

fn wait_call(state: &Arc<Mutex<State>>, call: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !state.lock().unwrap().calls.iter().any(|c| c == call) {
        assert!(
            std::time::Instant::now() < deadline,
            "never called {call}: {:?}",
            state.lock().unwrap().calls
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_tray_is_the_watcher_when_there_is_none() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.tray.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(
        bus.wait_for_name(WATCHER, Duration::from_secs(5)),
        "it owns the watcher"
    );
    assert!(items(&b, &rt).is_empty());
    // Hosts follow the watcher's list by PropertiesChanged.
    let listing = connect(&tokio, &bus.address);
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(WATCHER_PATH)
        .unwrap()
        .member("PropertiesChanged")
        .unwrap()
        .build();
    let mut changes = tokio
        .block_on(zbus::MessageStream::for_match_rule(rule, &listing, None))
        .unwrap();
    let next_listing = |changes: &mut zbus::MessageStream| -> Vec<String> {
        use futures_lite::StreamExt;
        let m = tokio
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), changes.next()).await })
            .expect("a PropertiesChanged")
            .unwrap()
            .unwrap();
        let (_, changed, _): (String, HashMap<String, OwnedValue>, Vec<String>) =
            m.body().deserialize().unwrap();
        changed["RegisteredStatusNotifierItems"]
            .try_clone()
            .unwrap()
            .try_into()
            .unwrap()
    };

    // An app's item registers with the watcher.
    let name = "org.kde.StatusNotifierItem-4242-1";
    let (conn, state) = item(&tokio, &bus.address, name);
    call(
        &tokio,
        &conn,
        WATCHER,
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        &(name,),
    );
    until(&rt, &s, "the item", || items(&b, &rt).len() == 1);
    let it = items(&b, &rt).remove(0);
    assert_eq!(it.id, format!("{name}/StatusNotifierItem"));
    assert_eq!(it.title, "Mock");
    assert_eq!(it.icon, "mock-icon");
    assert_eq!(it.tooltip.as_deref(), Some("Tip\nMore"));
    assert_eq!(it.status, "Active");
    assert_eq!(
        labels(&it.menu.items),
        ["Open", "", "Check", "Sub"],
        "hidden left out"
    );
    assert_eq!(it.menu.items[1].kind, "separator");
    assert_eq!(it.menu.items[2].toggle, "checkmark");
    assert!(it.menu.items[2].checked);
    assert_eq!(labels(&it.menu.items[3].children), ["Child"]);
    assert!(!it.menu.items[3].children[0].enabled);
    // The watcher lists it.
    let listed: OwnedValue = call(
        &tokio,
        &conn,
        WATCHER,
        WATCHER_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(WATCHER, "RegisteredStatusNotifierItems"),
    )
    .body()
    .deserialize()
    .unwrap();
    let listed: Vec<String> = listed.try_into().unwrap();
    assert_eq!(listed, std::slice::from_ref(&it.id));
    assert_eq!(next_listing(&mut changes), std::slice::from_ref(&it.id));

    // Clicks and scrolls reach the app.
    let d = b.tray.dynamic();
    d.action(&rt, "activate", Some(&it.to_data()), &[]).unwrap();
    wait_call(&state, "Activate");
    d.action(&rt, "secondary", Some(&it.to_data()), &[])
        .unwrap();
    wait_call(&state, "SecondaryActivate");
    d.action(&rt, "scroll", Some(&it.to_data()), &[Data::Float(-2.0)])
        .unwrap();
    // Two notches up: 120 a notch, positive up (as KDE's host sends).
    wait_call(&state, "Scroll 240 vertical");

    // The menu opens: the app is told, updates it, and the entry shows;
    // the store says it is open (a popup's `open: item.menu.opened`).
    assert!(!it.menu.opened);
    d.action(&rt, "open", Some(&it.menu.to_data()), &[])
        .unwrap();
    wait_call(&state, "Event 0 opened");
    until(&rt, &s, "the menu opened", || items(&b, &rt)[0].menu.opened);
    until(&rt, &s, "the extra entry", || {
        items(&b, &rt)[0]
            .menu
            .items
            .iter()
            .any(|e| e.label == "Extra")
    });
    // A submenu opens, and the menu closes: the app is told.
    let sub = items(&b, &rt)[0].menu.items[3].clone();
    d.action(&rt, "open", Some(&sub.to_data()), &[]).unwrap();
    wait_call(&state, "AboutToShow 4");
    wait_call(&state, "Event 4 opened");
    assert!(items(&b, &rt)[0].menu.opened, "a submenu leaves it open");
    d.action(&rt, "close", Some(&it.menu.to_data()), &[])
        .unwrap();
    wait_call(&state, "Event 0 closed");
    until(&rt, &s, "the menu closed", || {
        !items(&b, &rt)[0].menu.opened
    });
    // An entry is chosen.
    let check = items(&b, &rt)[0].menu.items[2].clone();
    d.action(&rt, "activate", Some(&check.to_data()), &[])
        .unwrap();
    wait_call(&state, "Event 3 clicked");
    assert!(
        !state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| *c == "Activate")
            .nth(1)
            .is_some(),
        "an entry's activate is not the item's"
    );

    // The app relabels an entry.
    state.lock().unwrap().open_label = "Open _now".into();
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/Menu",
            "com.canonical.dbusmenu",
            "LayoutUpdated",
            &(2u32, 0i32),
        ))
        .unwrap();
    until(&rt, &s, "the new label", || {
        items(&b, &rt)[0].menu.items[0].label == "Open now"
    });

    // A new icon, as pixels.
    {
        let mut st = state.lock().unwrap();
        st.icon_name.clear();
        st.pixmap = vec![(1, 1, vec![255, 10, 20, 30])];
    }
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
            "NewIcon",
            &(),
        ))
        .unwrap();
    until(&rt, &s, "the pixmap icon", || {
        items(&b, &rt)[0].icon.ends_with(".png")
    });
    let png = std::fs::read(&items(&b, &rt)[0].icon).unwrap();
    assert!(png.starts_with(b"\x89PNG"));

    // The app quits: its item goes.
    drop(conn);
    until(&rt, &s, "the item gone", || items(&b, &rt).is_empty());
    assert!(next_listing(&mut changes).is_empty(), "unlisted");
    {
        // Its match rule is removed on the runtime.
        let _on = tokio.enter();
        drop(changes);
    }
    assert_eq!(b.tray.starts(), 1);
    s.shutdown();
}

#[test]
fn a_frozen_app_does_not_hold_up_the_tray() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.tray.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    let register = |conn: &zbus::Connection, name: &str| {
        call(
            &tokio,
            conn,
            WATCHER,
            WATCHER_PATH,
            WATCHER,
            "RegisterStatusNotifierItem",
            &(name,),
        );
    };
    let frozen_name = "org.kde.StatusNotifierItem-5151-1";
    let (frozen, frozen_state) = item(&tokio, &bus.address, frozen_name);
    register(&frozen, frozen_name);
    until(&rt, &s, "the first item", || items(&b, &rt).len() == 1);
    // Its main loop gets stuck in a click (a modal dialog before the
    // reply, say).
    frozen_state.lock().unwrap().frozen = true;
    let first = items(&b, &rt).remove(0);
    b.tray
        .dynamic()
        .action(&rt, "activate", Some(&first.to_data()), &[])
        .unwrap();
    wait_call(&frozen_state, "Activate");
    // Another app's item registers and changes its icon meanwhile: shown
    // at once, long before the stuck call is given up.
    let started = std::time::Instant::now();
    let live_name = "org.kde.StatusNotifierItem-5151-2";
    let (live, live_state) = item(&tokio, &bus.address, live_name);
    register(&live, live_name);
    until(&rt, &s, "the second item", || items(&b, &rt).len() == 2);
    live_state.lock().unwrap().icon_name = "changed-icon".into();
    tokio
        .block_on(live.emit_signal(
            None::<&str>,
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
            "NewIcon",
            &(),
        ))
        .unwrap();
    until(&rt, &s, "the new icon", || {
        items(&b, &rt).iter().any(|i| i.icon == "changed-icon")
    });
    assert!(
        started.elapsed() < strand_services::dbus::CALL_TIMEOUT,
        "the tray waited on the frozen app: {:?}",
        started.elapsed()
    );
    drop(frozen);
    s.shutdown();
}

/// A watcher of the session's own (a desktop's, or another bar's).
struct OtherWatcher {
    items: Vec<String>,
    hosts: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl OtherWatcher {
    fn register_status_notifier_host(&self, service: &str) {
        self.hosts.lock().unwrap().push(service.to_string());
    }
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.clone()
    }
}

#[test]
fn the_tray_hosts_for_the_sessions_watcher() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let name = "org.kde.StatusNotifierItem-4343-1";
    let (_item_conn, _state) = item(&tokio, &bus.address, name);
    let hosts = Arc::new(Mutex::new(Vec::new()));
    let watcher = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(WATCHER)
            .unwrap()
            .serve_at(
                WATCHER_PATH,
                OtherWatcher {
                    items: vec![name.to_string()],
                    hosts: hosts.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.tray.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert_eq!(items(&b, &rt).len(), 1, "the watcher's item");
    assert_eq!(items(&b, &rt)[0].title, "Mock");
    let h = hosts.lock().unwrap().clone();
    assert_eq!(h.len(), 1, "registered as a host");
    assert!(h[0].starts_with("org.kde.StatusNotifierHost-"), "{h:?}");
    // A second item registers there: the watcher says so.
    let second = "org.kde.StatusNotifierItem-4343-2";
    let (_second_conn, _) = item(&tokio, &bus.address, second);
    tokio
        .block_on(watcher.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            WATCHER,
            "StatusNotifierItemRegistered",
            &(second,),
        ))
        .unwrap();
    until(&rt, &s, "the second item", || items(&b, &rt).len() == 2);
    // The session's watcher goes: the tray becomes the watcher.
    drop(watcher);
    assert!(bus.wait_for_name(WATCHER, Duration::from_secs(5)));
    until(&rt, &s, "items cleared until they register again", || {
        items(&b, &rt).is_empty()
    });
    s.shutdown();
}

/// An app registering before it exported its item (or too busy to
/// answer as it starts): the failed read is tried once more a little
/// later; given up, our watcher unlists it, so the app registering again
/// is heard. What the watcher lists is what the tray shows.
#[test]
fn an_item_unreadable_at_registration_is_read_again() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.tray.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(WATCHER, Duration::from_secs(5)));
    let listed = |conn: &zbus::Connection| -> Vec<String> {
        let v: OwnedValue = call(
            &tokio,
            conn,
            WATCHER,
            WATCHER_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(WATCHER, "RegisteredStatusNotifierItems"),
        )
        .body()
        .deserialize()
        .unwrap();
        v.try_into().unwrap()
    };
    let register = |conn: &zbus::Connection, name: &str| {
        call(
            &tokio,
            conn,
            WATCHER,
            WATCHER_PATH,
            WATCHER,
            "RegisterStatusNotifierItem",
            &(name,),
        );
    };
    let state = Arc::new(Mutex::new(State {
        icon_name: "mock-icon".into(),
        open_label: "_Open".into(),
        ..State::default()
    }));
    let bare = |name: &str| {
        tokio.block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .name(name)
                .unwrap()
                .build()
                .await
                .unwrap()
        })
    };
    let export = |conn: &zbus::Connection| {
        tokio.block_on(async {
            conn.object_server()
                .at("/StatusNotifierItem", Item(state.clone()))
                .await
                .unwrap();
            conn.object_server()
                .at("/Menu", Menu(state.clone()))
                .await
                .unwrap();
        })
    };

    // Registered, exported a moment later: the second read finds it.
    let early = "org.kde.StatusNotifierItem-777-1";
    let conn = bare(early);
    register(&conn, early);
    std::thread::sleep(Duration::from_millis(300));
    export(&conn);
    until(&rt, &s, "the item, read again", || {
        items(&b, &rt).len() == 1
    });
    assert_eq!(items(&b, &rt)[0].id, format!("{early}/StatusNotifierItem"));
    assert_eq!(listed(&conn), [format!("{early}/StatusNotifierItem")]);

    // Never readable in time: given up and unlisted; the app registering
    // again once it exported is heard.
    let late = "org.kde.StatusNotifierItem-778-1";
    let conn2 = bare(late);
    register(&conn2, late);
    let id = format!("{late}/StatusNotifierItem");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while listed(&conn).contains(&id) {
        assert!(std::time::Instant::now() < deadline, "never unlisted");
        s.pump(&rt);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(items(&b, &rt).len(), 1, "not shown");
    export(&conn2);
    register(&conn2, late);
    until(&rt, &s, "the late item", || items(&b, &rt).len() == 2);
    let mut shown: Vec<String> = items(&b, &rt).into_iter().map(|i| i.id).collect();
    let mut on_list = listed(&conn);
    shown.sort();
    on_list.sort();
    assert_eq!(shown, on_list, "the watcher lists what the tray shows");
    s.shutdown();
}

/// libappindicator and ayatana items (`ItemIsMenu`, no `Activate`): a
/// click opens their DBusMenu in the shell's popup (`item.menu.opened`),
/// as the SNI spec asks of a host; an item that refuses `Activate`
/// without saying `ItemIsMenu` gets its menu too.
#[test]
fn a_menu_only_item_opens_its_menu_on_activate() {
    let Some(bus) = strand_services::testing::PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.tray.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(WATCHER, Duration::from_secs(5)));
    let d = b.tray.dynamic();

    let name = "org.kde.StatusNotifierItem-5151-1";
    let (conn, state) = item(&tokio, &bus.address, name);
    {
        let mut st = state.lock().unwrap();
        st.is_menu = true;
        st.no_activate = true;
    }
    call(
        &tokio,
        &conn,
        WATCHER,
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        &(name,),
    );
    until(&rt, &s, "the item", || items(&b, &rt).len() == 1);
    let it = items(&b, &rt).remove(0);
    assert!(!it.menu.opened);
    d.action(&rt, "activate", Some(&it.to_data()), &[]).unwrap();
    until(&rt, &s, "the menu opened", || items(&b, &rt)[0].menu.opened);
    wait_call(&state, "AboutToShow 0");
    wait_call(&state, "Event 0 opened");
    assert!(
        !state.lock().unwrap().calls.iter().any(|c| c == "Activate"),
        "an ItemIsMenu item is not sent Activate"
    );
    d.action(&rt, "close", Some(&it.menu.to_data()), &[])
        .unwrap();
    until(&rt, &s, "the menu closed", || {
        !items(&b, &rt)[0].menu.opened
    });

    // Not marked, but refusing Activate: its menu opens all the same.
    {
        let mut st = state.lock().unwrap();
        st.is_menu = false;
        st.icon_name = "other-icon".into();
    }
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
            "NewIcon",
            &(),
        ))
        .unwrap();
    until(&rt, &s, "the item read again", || {
        items(&b, &rt)[0].icon == "other-icon"
    });
    d.action(&rt, "activate", Some(&it.to_data()), &[]).unwrap();
    wait_call(&state, "Activate");
    until(&rt, &s, "the menu opened on the refusal", || {
        items(&b, &rt)[0].menu.opened
    });
    assert!(
        !state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c == "ContextMenu")
    );
    drop(conn);
    s.shutdown();
}
