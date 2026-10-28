//! S-ipc: the D-Bus front end (`io.github.jakeb_grant.Strand` at `/`,
//! `strand_services::ipc_bus`) of a headless `strand run` on the real
//! services and a private `dbus-daemon` (a private bus only, and two instances on one
//! bus for the name collision):
//!
//! - `get`, `set`, `toggle`, `call`, `clear` and `reload` as methods,
//!   answering with the socket's answer, refusals as D-Bus errors with
//!   the socket's words; `Changed` arrives for a reload and a save;
//! - a second instance on the same bus runs without the name, says so,
//!   and still serves its socket; the first keeps the name.
//!
//! The PSS and idle window with the name owned are budgets.rs's design
//! bar (its private bus is the shell's session bus). Skipped, loudly,
//! without sway or grim (CI sets `STRAND_REQUIRE_SWAY`) or without
//! `dbus-daemon` (`STRAND_REQUIRE_DBUS`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::json;
use strand_core::ipc::DBUS_NAME;
use strand_services::testing::PrivateBus;

use super::cli::{Desk, bar, json, tools};

#[zbus::proxy(
    interface = "io.github.jakeb_grant.Strand",
    default_service = "io.github.jakeb_grant.Strand",
    default_path = "/"
)]
trait Shell {
    fn get(&self, path: &str) -> zbus::Result<String>;
    fn set(&self, path: &str, value: &str) -> zbus::Result<String>;
    fn toggle(&self, path: &str) -> zbus::Result<String>;
    fn clear(&self, path: &str) -> zbus::Result<String>;
    fn call(&self, target: &str, args: &[&str]) -> zbus::Result<String>;
    fn reload(&self, hard: bool) -> zbus::Result<String>;
    #[zbus(signal)]
    fn changed(&self, event: &str, data: &str) -> zbus::Result<()>;
}

/// A refusal's message (a `Failed` error).
fn refused(r: zbus::Result<String>) -> String {
    match r {
        Err(zbus::Error::MethodError(name, Some(why), _)) => {
            assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.Failed");
            why
        }
        other => panic!("not refused: {other:?}"),
    }
}

/// A client of `bus` on a runtime of the test's own.
struct Client {
    rt: tokio::runtime::Runtime,
    conn: zbus::Connection,
}

impl Client {
    fn new(bus: &PrivateBus) -> Client {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let conn = rt.block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        });
        Client { rt, conn }
    }

    fn shell(&self) -> ShellProxy<'static> {
        self.rt.block_on(ShellProxy::new(&self.conn)).unwrap()
    }

    /// The process owning `name`.
    fn owner_pid(&self, name: &str) -> u32 {
        self.rt.block_on(async {
            let dbus = zbus::fdo::DBusProxy::new(&self.conn).await.unwrap();
            let name = zbus::names::BusName::try_from(name).unwrap();
            dbus.get_connection_unix_process_id(name).await.unwrap()
        })
    }
}

/// `get`, `set`, `toggle`, `call`, `clear` and `reload` over D-Bus, and
/// the `Changed` signal.
#[test]
fn get_set_toggle_call_and_changed_over_dbus() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let src = format!("{}export let count = notifications.count\n", bar("#112233"));
    let d = Desk::start(
        "dbus",
        &[("bar.strand", &src)],
        &[("DBUS_SESSION_BUS_ADDRESS", &bus.address)],
    );
    assert!(
        bus.wait_for_name(DBUS_NAME, Duration::from_secs(10)),
        "the shell owns {DBUS_NAME}\n{}",
        d.log_text()
    );
    let c = Client::new(&bus);
    // zbus's streams and proxies are dropped inside the runtime (a
    // signal stream's drop spawns its match rule's removal).
    let _inside = c.rt.enter();
    assert_eq!(c.owner_pid(DBUS_NAME), d.pid());
    let shell = c.shell();
    let get = |path: &str| json(&c.rt.block_on(shell.get(path)).unwrap());
    // get: the socket's answer, word for word.
    assert_eq!(
        get("bar.label"),
        json!({"ok": true, "value": "hi", "text": "hi", "layer": null})
    );
    assert!(refused(c.rt.block_on(shell.get("bar.nope"))).contains("bar.nope"));
    assert_eq!(refused(c.rt.block_on(shell.get(""))), "`get` needs `path`");
    // set, then read back by the CLI as well.
    assert_eq!(
        json(&c.rt.block_on(shell.set("bar.label", "yo")).unwrap()),
        json!({"ok": true})
    );
    assert_eq!(d.ok(&["get", "bar.label"]), "yo\n");
    // toggle: a bool flips; a text is refused, naming its type.
    c.rt.block_on(shell.toggle("bar.open")).unwrap();
    assert_eq!(get("bar.open")["value"], true);
    let why = refused(c.rt.block_on(shell.toggle("bar.label")));
    assert!(why.contains("`bar.label` is a text, not a bool"), "{why}");
    // A token overlay set over D-Bus is cleared over D-Bus.
    c.rt.block_on(shell.set("$probe.c", "#ff8800")).unwrap();
    assert_eq!(get("$probe.c")["layer"], "overlay");
    assert_eq!(
        json(&c.rt.block_on(shell.clear("$probe.c")).unwrap()),
        json!({"ok": true, "cleared": true})
    );
    assert_eq!(get("$probe.c")["layer"], "tokens base");

    // call: the shell's own notification server forgets what an app
    // sent.
    assert!(
        bus.wait_for_name("org.freedesktop.Notifications", Duration::from_secs(10)),
        "the shell's notification server\n{}",
        d.log_text()
    );
    let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    c.rt.block_on(c.conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "app",
            0u32,
            "",
            "Hello",
            "over D-Bus",
            Vec::<&str>::new(),
            hints,
            -1i32,
        ),
    ))
    .unwrap();
    let until = |want: i64| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while get("bar.count")["value"] != want {
            assert!(
                Instant::now() < deadline,
                "count is not {want}\n{}",
                d.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    until(1);
    assert_eq!(
        json(
            &c.rt
                .block_on(shell.call("notifications.clear", &[]))
                .unwrap()
        ),
        json!({"ok": true})
    );
    until(0);
    let why = refused(c.rt.block_on(shell.call("notifications.claer", &[])));
    assert!(why.contains("did you mean `clear`?"), "{why}");
    let why = refused(c.rt.block_on(shell.call("auth.submit", &["x"])));
    assert!(why.contains("not run over IPC"), "{why}");

    // reload: answered with its load's event, which `Changed` carries
    // too.
    let mut changed = c.rt.block_on(shell.receive_changed()).unwrap();
    let ans = json(&c.rt.block_on(shell.reload(false)).unwrap());
    assert_eq!(ans["ok"], true, "{ans}");
    assert_eq!(ans["event"]["requested"], true, "{ans}");
    let mut next = |what: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let sig =
                c.rt.block_on(tokio::time::timeout(left, changed.next()))
                    .unwrap_or_else(|_| panic!("no `Changed` for {what}\n{}", d.log_text()))
                    .unwrap();
            let args = sig.args().unwrap();
            if *args.event() == "reload" {
                return json(args.data());
            }
        }
    };
    let data = next("the reload");
    assert_eq!(data["event"], "reload");
    assert_eq!(data["requested"], true, "{data}");
    // A save is heard as well.
    d.save("bar.strand", &src.replace("#112233", "#445566"));
    let data = next("the save");
    assert_eq!(data["requested"], false, "{data}");
}

/// A second instance on the same bus runs without the name and says so;
/// the first keeps it, and the second still serves its socket.
#[test]
fn a_second_instance_runs_without_the_name() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let env = [("DBUS_SESSION_BUS_ADDRESS", bus.address.as_str())];
    let first = Desk::start("dbus-a", &[("bar.strand", &bar("#112233"))], &env);
    assert!(
        bus.wait_for_name(DBUS_NAME, Duration::from_secs(10)),
        "the first shell owns {DBUS_NAME}\n{}",
        first.log_text()
    );
    let second = Desk::start("dbus-b", &[("bar.strand", &bar("#112233"))], &env);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !second
        .log_text()
        .contains(&format!("another instance owns {DBUS_NAME}"))
    {
        assert!(
            Instant::now() < deadline,
            "the second shell did not say it runs without D-Bus IPC\n{}",
            second.log_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let c = Client::new(&bus);
    assert_eq!(c.owner_pid(DBUS_NAME), first.pid());
    // Each answers its own clients: the bus reaches the first only.
    second.ok(&["set", "bar.label", "second"]);
    let shell = c.shell();
    assert_eq!(
        json(&c.rt.block_on(shell.get("bar.label")).unwrap())["value"],
        "hi"
    );
    assert_eq!(second.ok(&["get", "bar.label"]), "second\n");
    assert!(!first.log_text().contains("another instance owns"));
}
