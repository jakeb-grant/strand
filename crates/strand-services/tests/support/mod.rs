//! What the D-Bus service tests share: a tokio runtime for the test's own
//! calls, the registry, pumping until a condition holds, and calls into
//! python-dbusmock's mock objects.

#![allow(dead_code)]

use std::time::Duration;

use strand_core::Runtime;
use strand_services::{Builtin, Buses, Services};

pub fn tokio() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

pub fn services(rt: &Runtime, buses: Buses) -> (Services, Builtin) {
    let s = Services::new(rt, buses, || {});
    let b = Builtin::register(&s, rt);
    (s, b)
}

/// Pump until `cond` holds (30 s at most).
pub fn until(rt: &Runtime, s: &Services, what: &str, mut cond: impl FnMut() -> bool) {
    strand_test_support::wait::until(
        what,
        Duration::from_secs(30),
        Duration::from_millis(5),
        || {
            s.pump(rt);
            rt.flush();
            cond()
        },
    );
}

/// A connection of the test's own to `address`.
pub fn connect(tokio: &tokio::runtime::Runtime, address: &str) -> zbus::Connection {
    tokio.block_on(async {
        zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

/// Call `method` of `iface` on `dest`'s `path`.
pub fn call<B>(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Message
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    tokio
        .block_on(conn.call_method(Some(dest), path, Some(iface), method, body))
        .unwrap_or_else(|e| panic!("{dest} {path} {iface}.{method}: {e}"))
}

/// Call a python-dbusmock `org.freedesktop.DBus.Mock` method on `path`.
pub fn mock<B>(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    method: &str,
    body: &B,
) -> zbus::Message
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    call(
        tokio,
        conn,
        dest,
        path,
        "org.freedesktop.DBus.Mock",
        method,
        body,
    )
}

/// The unique bus name serving `iface` at `path` (an agent Strand
/// exports), if one does: each unique name's `Introspect` of `path`.
pub fn serving(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    path: &str,
    iface: &str,
) -> Option<String> {
    let dbus = tokio.block_on(zbus::fdo::DBusProxy::new(conn)).unwrap();
    let names = tokio.block_on(dbus.list_names()).unwrap();
    for n in names.iter().filter(|n| n.starts_with(':')) {
        let xml = tokio.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                conn.call_method(
                    Some(n.as_str()),
                    path,
                    Some("org.freedesktop.DBus.Introspectable"),
                    "Introspect",
                    &(),
                ),
            )
            .await
        });
        if let Ok(Ok(m)) = xml
            && m.body()
                .deserialize::<String>()
                .is_ok_and(|x| x.contains(iface))
        {
            return Some(n.to_string());
        }
    }
    None
}

/// The calls a dbusmock object logged for `method`: their arguments.
pub fn mock_calls(
    tokio: &tokio::runtime::Runtime,
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
    method: &str,
) -> Vec<Vec<zbus::zvariant::OwnedValue>> {
    let reply = mock(tokio, conn, dest, path, "GetMethodCalls", &(method,));
    let calls: Vec<(u64, Vec<zbus::zvariant::OwnedValue>)> = reply.body().deserialize().unwrap();
    calls.into_iter().map(|(_, args)| args).collect()
}
