//! `notifications`: the shell's notification server on a private bus,
//! driven by a client the way apps do (`Notify`, `CloseNotification`),
//! with the store's popups, events and actions checked and the spec's
//! signals heard; and the name conflict with another server
//! (python-dbusmock's `notification_daemon` standing in for dunst or
//! mako) failing with a diagnostic that names it.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::StreamExt;
use strand_core::Runtime;
use strand_services::notifications::{NAME, Notification, PATH, Urgency};
use strand_services::testing::{DbusMock, PrivateBus};
use strand_services::{Data, ToData};
use support::*;
use zbus::zvariant::Value;

fn notify(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    replaces: u32,
    summary: &str,
    actions: &[&str],
    hints: HashMap<&str, Value<'_>>,
    timeout: i32,
) -> u32 {
    call(
        tokio,
        conn,
        NAME,
        PATH,
        NAME,
        "Notify",
        &(
            "Mail",
            replaces,
            "mail-unread",
            summary,
            "You have <b>mail</b>",
            actions,
            hints,
            timeout,
        ),
    )
    .body()
    .deserialize()
    .unwrap()
}

fn popups(b: &strand_services::Builtin, rt: &Runtime) -> Vec<Notification> {
    b.notifications
        .cells()
        .popups
        .get_untracked(rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, n)| n.clone())
        .collect()
}

/// The server's signals, as `Closed id reason` / `Invoked id key`.
fn hear(tokio: &tokio::runtime::Runtime, conn: &zbus::Connection) -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let h = heard.clone();
    let conn = conn.clone();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(NAME)
        .unwrap()
        .build();
    let mut stream = tokio
        .block_on(zbus::MessageStream::for_match_rule(rule, &conn, None))
        .unwrap();
    tokio.spawn(async move {
        while let Some(Ok(m)) = stream.next().await {
            let member = m
                .header()
                .member()
                .map(|m| m.to_string())
                .unwrap_or_default();
            let line = match member.as_str() {
                "NotificationClosed" => {
                    let (id, why): (u32, u32) = m.body().deserialize().unwrap();
                    format!("Closed {id} {why}")
                }
                "ActionInvoked" => {
                    let (id, key): (u32, String) = m.body().deserialize().unwrap();
                    format!("Invoked {id} {key}")
                }
                other => other.to_string(),
            };
            h.lock().unwrap().push(line);
        }
    });
    heard
}

fn wait_heard(heard: &Arc<Mutex<Vec<String>>>, line: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !heard.lock().unwrap().iter().any(|l| l == line) {
        assert!(
            std::time::Instant::now() < deadline,
            "never heard {line}: {:?}",
            heard.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_server_serves_the_spec_and_the_store() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let heard = hear(&tokio, &conn);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    let received = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    b.notifications.dynamic().observe(Box::new(move |_, a| {
        if let strand_services::Applied::Event { args, .. } = a {
            r.lock().unwrap().push(args.clone());
        }
    }));
    b.notifications.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));

    // What the server says about itself.
    let caps: Vec<String> = call(&tokio, &conn, NAME, PATH, NAME, "GetCapabilities", &())
        .body()
        .deserialize()
        .unwrap();
    for c in ["body", "body-markup", "actions"] {
        assert!(caps.iter().any(|x| x == c), "{caps:?}");
    }
    let info: (String, String, String, String) =
        call(&tokio, &conn, NAME, PATH, NAME, "GetServerInformation", &())
            .body()
            .deserialize()
            .unwrap();
    assert_eq!(info.0, "strand");
    assert_eq!(info.3, "1.1", "no ActivationToken: not 1.2");

    // A notification with a default action, a button, a critical
    // urgency, an image and a timeout.
    let image = Value::from((
        2i32,
        1i32,
        8i32,
        false,
        8i32,
        3i32,
        vec![255u8, 0, 0, 0, 255, 0, 0, 0],
    ));
    let id = notify(
        &tokio,
        &conn,
        0,
        "Hello",
        &["default", "Open", "reply", "Reply"],
        HashMap::from([("urgency", Value::from(2u8)), ("image-data", image)]),
        4000,
    );
    assert!(id > 0);
    until(&rt, &s, "the popup", || popups(&b, &rt).len() == 1);
    let n = popups(&b, &rt).remove(0);
    assert_eq!(n.id, i64::from(id));
    assert_eq!(n.summary, "Hello");
    assert_eq!(n.body, "You have <b>mail</b>");
    assert_eq!(n.app.name, "Mail");
    assert_eq!(n.app.icon, "mail-unread");
    assert_eq!(n.urgency, Urgency::Critical);
    assert_eq!(n.timeout, Some(Duration::from_millis(4000)));
    assert_eq!(n.actions.len(), 1, "the default action is not a button");
    assert_eq!(n.actions[0].id, "reply");
    assert_eq!(n.actions[0].label, "Reply");
    let image = n.image.clone().unwrap();
    assert!(
        std::fs::read(&image).unwrap().starts_with(b"\x89PNG"),
        "{image}"
    );
    assert_eq!(b.notifications.cells().count.get_untracked(&rt), Ok(1));
    assert_eq!(received.lock().unwrap().len(), 1, "received fired once");
    assert_eq!(received.lock().unwrap()[0], vec![n.to_data()]);

    // The same id replaces it in place.
    let again = notify(&tokio, &conn, id, "Hello again", &[], HashMap::new(), -1);
    assert_eq!(again, id);
    until(&rt, &s, "the replacement", || {
        popups(&b, &rt)
            .first()
            .is_some_and(|n| n.summary == "Hello again")
    });
    assert_eq!(popups(&b, &rt).len(), 1);
    assert_eq!(
        popups(&b, &rt)[0].timeout,
        None,
        "-1 leaves it to the shell"
    );

    // A button: the sender hears it, the notification closes.
    let id2 = notify(
        &tokio,
        &conn,
        0,
        "Second",
        &["ok", "OK"],
        HashMap::new(),
        -1,
    );
    until(&rt, &s, "two popups", || popups(&b, &rt).len() == 2);
    let action = popups(&b, &rt)[1].actions[0].clone();
    b.notifications
        .dynamic()
        .action(&rt, "invoke", Some(&action.to_data()), &[])
        .unwrap();
    wait_heard(&heard, &format!("Invoked {id2} ok"));
    wait_heard(&heard, &format!("Closed {id2} 2"));
    until(&rt, &s, "one popup", || popups(&b, &rt).len() == 1);

    // Expired: out of the popups, kept open in `all` (not closed yet).
    let first = popups(&b, &rt)[0].to_data();
    b.notifications
        .dynamic()
        .action(&rt, "expire", Some(&first), &[])
        .unwrap();
    until(&rt, &s, "no popups", || popups(&b, &rt).is_empty());
    assert_eq!(b.notifications.cells().count.get_untracked(&rt), Ok(1));

    // The sender closes it: closed once, when it leaves `all`.
    call(&tokio, &conn, NAME, PATH, NAME, "CloseNotification", &(id,));
    wait_heard(&heard, &format!("Closed {id} 3"));
    until(&rt, &s, "none kept", || {
        b.notifications.cells().count.get_untracked(&rt) == Ok(0)
    });
    let closed = |id: u32| {
        heard
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(&format!("Closed {id} ")))
            .count()
    };
    assert_eq!(closed(id), 1, "{:?}", heard.lock().unwrap());
    // Closing it again, or an id never given, is an error (the spec).
    for gone in [id, 9999] {
        let r = tokio.block_on(conn.call_method(
            Some(NAME),
            PATH,
            Some(NAME),
            "CloseNotification",
            &(gone,),
        ));
        assert!(r.is_err(), "closing {gone} succeeded");
    }

    // 0 is "never expire": `persistent`; an urgency sent as another
    // integer type still counts.
    let kept = notify(
        &tokio,
        &conn,
        0,
        "Kept",
        &[],
        HashMap::from([("urgency", Value::from(0i32))]),
        0,
    );
    until(&rt, &s, "the persistent one", || {
        popups(&b, &rt).iter().any(|n| n.id == i64::from(kept))
    });
    let k = popups(&b, &rt)
        .into_iter()
        .find(|n| n.id == i64::from(kept))
        .unwrap();
    assert!(k.persistent && k.timeout.is_none());
    assert_eq!(k.urgency, Urgency::Low);
    b.notifications
        .dynamic()
        .action(&rt, "dismiss", Some(&k.to_data()), &[])
        .unwrap();
    wait_heard(&heard, &format!("Closed {kept} 2"));
    assert_eq!(closed(id2), 1, "{:?}", heard.lock().unwrap());
    until(&rt, &s, "none kept", || {
        b.notifications.cells().count.get_untracked(&rt) == Ok(0)
    });

    // Do not disturb: kept, not shown, unless critical.
    b.notifications
        .dynamic()
        .write(&rt, 3, &[], Data::Bool(true))
        .unwrap();
    rt.flush();
    let quiet = notify(&tokio, &conn, 0, "Quiet", &[], HashMap::new(), -1);
    let loud = notify(
        &tokio,
        &conn,
        0,
        "Loud",
        &[],
        HashMap::from([("urgency", Value::from(2u8))]),
        -1,
    );
    until(&rt, &s, "both kept", || {
        b.notifications.cells().count.get_untracked(&rt) == Ok(2)
    });
    let shown: Vec<i64> = popups(&b, &rt).iter().map(|n| n.id).collect();
    assert_eq!(shown, [i64::from(loud)], "only the critical one shows");
    assert_eq!(received.lock().unwrap().len(), 6, "received fires for each");

    // Activate: its default action (none here: nothing invoked), closed.
    let l = popups(&b, &rt)[0].to_data();
    b.notifications
        .dynamic()
        .action(&rt, "activate", Some(&l), &[])
        .unwrap();
    wait_heard(&heard, &format!("Closed {loud} 2"));
    // Clear: every one closes.
    b.notifications
        .dynamic()
        .action(&rt, "clear", None, &[])
        .unwrap();
    wait_heard(&heard, &format!("Closed {quiet} 2"));
    until(&rt, &s, "cleared", || {
        b.notifications.cells().count.get_untracked(&rt) == Ok(0)
    });
    assert!(
        !heard
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("Invoked") && l.ends_with("default")),
        "no default action was offered: {:?}",
        heard.lock().unwrap()
    );
    assert!(s.take_diagnostics().is_empty());
    // Exactly one NotificationClosed per notification.
    for n in [id, id2, kept, quiet, loud] {
        assert_eq!(closed(n), 1, "{n}: {:?}", heard.lock().unwrap());
    }

    // Stopped: the name is released with its connection.
    b.notifications.release(&rt);
    s.shutdown();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let dbus = tokio.block_on(zbus::fdo::DBusProxy::new(&conn)).unwrap();
    while tokio
        .block_on(dbus.name_has_owner(NAME.try_into().unwrap()))
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the name stayed owned"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn history_is_bounded_and_lying_pictures_are_refused() {
    use strand_services::notifications::KEPT;
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let heard = hear(&tokio, &conn);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.notifications.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));

    // image-data claiming 100000x100000 pixels in four bytes: answered,
    // shown without the picture, and the server lives on.
    let liar = Value::from((100_000i32, 100_000i32, 0i32, true, 8i32, 4i32, vec![0u8; 4]));
    let lied = notify(
        &tokio,
        &conn,
        0,
        "Liar",
        &[],
        HashMap::from([("image-data", liar)]),
        -1,
    );
    until(&rt, &s, "the liar's notification", || {
        popups(&b, &rt).iter().any(|n| n.id == i64::from(lied))
    });
    assert_eq!(popups(&b, &rt)[0].image, None);

    // A picture of its own on the first kept one: its file goes with it.
    let image = Value::from((1i32, 1i32, 4i32, true, 8i32, 4i32, vec![1u8, 2, 3, 4]));
    let pictured = notify(
        &tokio,
        &conn,
        0,
        "Pictured",
        &[],
        HashMap::from([("image-data", image)]),
        -1,
    );
    until(&rt, &s, "the picture", || {
        popups(&b, &rt)
            .iter()
            .any(|n| n.id == i64::from(pictured) && n.image.is_some())
    });
    let file = popups(&b, &rt)
        .into_iter()
        .find(|n| n.id == i64::from(pictured))
        .and_then(|n| n.image)
        .unwrap();
    assert!(std::path::Path::new(&file).exists());

    // KEPT + 10 more: the oldest close as expired, and `all` holds KEPT.
    let mut last = 0;
    for i in 0..KEPT + 10 {
        last = notify(&tokio, &conn, 0, &format!("n{i}"), &[], HashMap::new(), -1);
    }
    until(&rt, &s, "the last one", || {
        popups(&b, &rt)
            .last()
            .is_some_and(|n| n.id == i64::from(last))
    });
    assert_eq!(
        b.notifications.cells().count.get_untracked(&rt),
        Ok(KEPT as i64)
    );
    wait_heard(&heard, &format!("Closed {lied} 1"));
    wait_heard(&heard, &format!("Closed {pictured} 1"));
    let expired = heard
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("Closed ") && l.ends_with(" 1"))
        .count();
    assert_eq!(expired, 12, "the 12 oldest closed");
    until(&rt, &s, "the picture's file removed", || {
        !std::path::Path::new(&file).exists()
    });
    s.shutdown();
}

#[test]
fn another_notification_server_fails_clearly() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    // dunst or mako, played by python-dbusmock.
    let Some(other) = DbusMock::start(&bus, "notification_daemon", false, None, NAME) else {
        return;
    };
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.notifications.acquire(&rt);
    let d = std::cell::RefCell::new(Vec::new());
    until(&rt, &s, "the notice", || {
        d.borrow_mut().extend(s.take_diagnostics());
        !d.borrow().is_empty()
    });
    let d = d.into_inner();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].service, "notifications");
    assert!(
        d[0].notice && !d[0].resolved,
        "for the overlay: the user must act"
    );
    let msg = d[0].to_string();
    // The owner's process, by pid and command name.
    let pid = other.pid();
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap();
    assert!(msg.contains(&format!("(pid {pid})")), "{msg}");
    assert!(msg.contains(&format!("`{}`", comm.trim())), "{msg}");
    // Not a systemd user service (no systemd on the private bus): the
    // exact process is named for kill (python-dbusmock is an
    // interpreter: no pkill, which would stop every script it runs), and
    // the activation override given.
    assert!(msg.contains(&format!("`kill {pid}`")), "{msg}");
    assert!(!msg.contains("pkill"), "{msg}");
    assert!(!msg.contains("systemctl"), "{msg}");
    assert!(msg.contains("dbus-1/services"), "{msg}");
    assert!(msg.contains(NAME), "{msg}");
    // The other server keeps the name: no second server. The run waits
    // for the name, polling nothing: no timer, no restart.
    assert!(bus.wait_for_name(NAME, Duration::from_millis(100)));
    assert!(b.notifications.running());
    assert_eq!(rt.next_deadline(), None, "no retry timer");
    std::thread::sleep(Duration::from_millis(1500));
    s.pump(&rt);
    assert_eq!(b.notifications.starts(), 1, "not restarted");
    assert!(s.take_diagnostics().is_empty(), "reported once");
    // The other daemon stops: the name is taken over at once.
    let stopped = std::time::Instant::now();
    drop(other);
    until(&rt, &s, "the notice resolved", || {
        let d = s.take_diagnostics();
        assert!(d.iter().all(|d| d.resolved), "{d:?}");
        !d.is_empty()
    });
    let took = stopped.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "took the name over in {took:?}"
    );
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));
    assert_eq!(b.notifications.starts(), 1, "the same run");
    // It serves: a notification arrives.
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    notify(&tokio, &conn, 0, "Taken over", &[], HashMap::new(), -1);
    until(&rt, &s, "the notification", || {
        popups(&b, &rt).iter().any(|n| n.summary == "Taken over")
    });
    s.shutdown();
}

/// The server stops (nobody read `notifications` for 5 s) and starts
/// again: what the first run held was closed with it (one
/// `NotificationClosed`, reason 3, before the name went), and the new run
/// starts with none, so an id it hands out again never names an old
/// notification.
#[test]
fn a_restarted_server_starts_with_no_notifications() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let heard = hear(&tokio, &conn);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.notifications.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));
    let image = Value::from((1i32, 1i32, 4i32, true, 8i32, 4i32, vec![1u8, 2, 3, 4]));
    let first = notify(
        &tokio,
        &conn,
        0,
        "First",
        &[],
        HashMap::from([("image-data", image)]),
        -1,
    );
    until(&rt, &s, "the first", || {
        popups(&b, &rt).iter().any(|n| n.image.is_some())
    });
    // Its last reader leaves; past the grace the server stops.
    b.notifications.release(&rt);
    rt.tick(strand_services::STOP_GRACE + Duration::from_secs(1));
    until(&rt, &s, "stopped", || !b.notifications.running());
    wait_heard(&heard, &format!("Closed {first} 3"));
    // A reader again: a new server, with nothing from the old one.
    b.notifications.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));
    assert_eq!(b.notifications.cells().count.get_untracked(&rt), Ok(0));
    assert!(popups(&b, &rt).is_empty());
    let second = notify(&tokio, &conn, 0, "Second", &[], HashMap::new(), -1);
    until(&rt, &s, "the second", || popups(&b, &rt).len() == 1);
    let all: Vec<(i64, String)> = b
        .notifications
        .cells()
        .all
        .get_untracked(&rt)
        .unwrap()
        .items()
        .iter()
        .map(|(_, n)| (n.id, n.summary.clone()))
        .collect();
    assert_eq!(all, [(i64::from(second), "Second".to_string())]);
    // The old id names nothing on this server (it was closed).
    let closed = heard
        .lock()
        .unwrap()
        .iter()
        .filter(|l| *l == &format!("Closed {first} 3"))
        .count();
    assert_eq!(closed, 1, "closed once: {:?}", heard.lock().unwrap());
    assert!(s.take_diagnostics().is_empty());
    s.shutdown();
}

/// strand exiting (`Services::shutdown`) closes what the server holds:
/// one `NotificationClosed` (reason 3) each, sent before the services'
/// runtime goes.
#[test]
fn shutdown_closes_the_notifications_still_open() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let tokio = tokio();
    let conn = connect(&tokio, &bus.address);
    let heard = hear(&tokio, &conn);
    let rt = Runtime::new();
    let (s, b) = services(&rt, bus.buses());
    b.notifications.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    assert!(bus.wait_for_name(NAME, Duration::from_secs(5)));
    let one = notify(&tokio, &conn, 0, "One", &[], HashMap::new(), -1);
    let two = notify(&tokio, &conn, 0, "Two", &[], HashMap::new(), 0);
    until(&rt, &s, "both", || popups(&b, &rt).len() == 2);
    s.shutdown();
    wait_heard(&heard, &format!("Closed {one} 3"));
    wait_heard(&heard, &format!("Closed {two} 3"));
}
