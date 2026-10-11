//! The D-Bus front end of a running shell (docs/architecture.md, "M5
//! interfaces": the request channel and `strand_services::ipc_bus`;
//! decisions.md m5-owner Q12 and m5-ipc).
//!
//! The bus name and interface are [`DBUS_NAME`]
//! (`io.github.jakeb_grant.Strand`) at object path `/`, owned on a
//! session-bus connection of the front end's own (so the name goes with
//! it) when `strand run` starts. The name is asked for without queueing:
//! a second instance that finds it owned runs without D-Bus IPC, as one
//! that finds the socket taken runs without the socket, and says so once.
//!
//! The interface carries the scripting commands, each a method taking the
//! request's fields and answering with the socket's answer as JSON text
//! (`{"ok": true, …}`, one shape for every client); a refusal is a D-Bus
//! error (`org.freedesktop.DBus.Error.Failed`) whose message is the
//! socket's `error`:
//!
//! - `Get(s path) → s`, `Set(s path, s value) → s`, `Toggle(s path) →
//!   s`, `Clear(s path) → s`, `Reset(s path) → s`, `Call(s target, as
//!   args) → s`, `Reload(b hard) → s` (answered once that load is
//!   committed, with its event, as `strand reload` is);
//! - the signal `Changed(s event, s data)`: every event a `strand watch`
//!   client gets (`reload`, `fault`, `notices`), `event` its kind (a
//!   match rule can filter on it) and `data` the whole event as JSON.
//!
//! Each method call becomes a [`Request`] sent through [`IpcRequests`] to
//! the logic loop and awaited through its `Reply`, so logic never waits
//! on this thread; the events come the other way through the
//! [`EventSink`] [`spawn`] returns. The inspector's commands, `report`
//! and `export` stay the socket's: their clients (`strand-dev inspect`,
//! `strand report`, `strand export`) are the CLI's.

use strand_core::ipc::{self, Answer, DBUS_NAME, EventSink, IpcRequests, Request};
use tokio::sync::{mpsc, oneshot};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::object_server::SignalEmitter;

use crate::{Buses, Services};

/// The object path the interface is served at.
pub const PATH: &str = "/";

/// The interface: its methods hand requests to the logic loop.
struct Front {
    requests: IpcRequests,
}

/// A field a command needs must not be empty (the protocol's rule: an
/// empty string is no value): refused with `why`, the socket's words.
fn need(v: &str, why: &str) -> zbus::fdo::Result<()> {
    if v.is_empty() {
        return Err(zbus::fdo::Error::Failed(why.into()));
    }
    Ok(())
}

impl Front {
    /// Ask the logic loop and answer as the socket would: the answer's
    /// JSON, or its refusal as an error.
    #[inline(never)]
    async fn ask(&self, request: Request) -> zbus::fdo::Result<String> {
        // `IpcClosed`: the logic loop is gone (said without building an
        // answer object, which would instantiate the JSON map here).
        let reply = self
            .requests
            .call(request)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let answer = reply.await;
        match ipc::refusal(&answer) {
            Some(why) => Err(zbus::fdo::Error::Failed(why)),
            None => Ok(answer.to_string()),
        }
    }
}

// The literal must be `DBUS_NAME` (the attribute takes no constant);
// `the_interface_is_the_one_name` holds them equal.
#[zbus::interface(name = "io.github.jakeb_grant.Strand")]
impl Front {
    /// `strand get <path>`.
    async fn get(&self, path: String) -> zbus::fdo::Result<String> {
        need(&path, "`get` needs `path`")?;
        self.ask(Request::Get { path }).await
    }

    /// `strand set <path> <value>`.
    async fn set(&self, path: String, value: String) -> zbus::fdo::Result<String> {
        need(&path, "`set` needs a `path` and a `value`")?;
        self.ask(Request::Set { path, value }).await
    }

    /// `strand toggle <path>`.
    async fn toggle(&self, path: String) -> zbus::fdo::Result<String> {
        need(&path, "`toggle` needs `path`")?;
        self.ask(Request::Toggle { path }).await
    }

    /// `strand clear <path>`.
    async fn clear(&self, path: String) -> zbus::fdo::Result<String> {
        need(&path, "`clear` needs `path`")?;
        self.ask(Request::Clear { path }).await
    }

    /// The overlay's `[reset]`: a state cell back to its default.
    async fn reset(&self, path: String) -> zbus::fdo::Result<String> {
        need(&path, "`reset` needs a `path`")?;
        self.ask(Request::Reset { path }).await
    }

    /// `strand call <service.action> [args]`.
    async fn call(&self, target: String, args: Vec<String>) -> zbus::fdo::Result<String> {
        need(&target, "`call` needs `target`")?;
        self.ask(Request::Call { target, args }).await
    }

    /// `strand reload [--hard]`: answered with the event of the load it
    /// causes.
    async fn reload(&self, hard: bool) -> zbus::fdo::Result<String> {
        self.ask(Request::Reload { hard }).await
    }

    /// An event `strand watch` gets: `event` its kind, `data` the event.
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, event: &str, data: &str) -> zbus::Result<()>;
}

/// Why the front end is not (or no longer) serving.
#[derive(Debug)]
pub enum Ended {
    /// The shell is gone: its event sink was dropped.
    Done,
    /// No session bus to serve on.
    NoBus(zbus::Error),
    /// Another instance owns [`DBUS_NAME`]: this one runs without D-Bus
    /// IPC.
    Taken,
    /// Serving failed.
    Failed(zbus::Error),
}

/// The events [`spawn`]'s sink hands on, for [`serve`].
pub type Events = mpsc::UnboundedReceiver<Answer>;

/// An [`EventSink`] for [`serve`], and the receiving end it serves. The
/// sink runs on the logic thread and never blocks: it queues the event
/// (unless the front end is gone) and returns.
pub fn events() -> (EventSink, Events) {
    let (tx, rx) = mpsc::unbounded_channel::<Answer>();
    let sink = EventSink::new(move |ev| {
        if !tx.is_closed() {
            let _ = tx.send(ev.clone());
        }
    });
    (sink, rx)
}

/// Serve the front end on a session-bus connection of its own on
/// `buses` until the event sink is dropped (`Ended::Done`), or say why
/// it does not. Runs on a tokio runtime (the services thread's).
pub async fn serve(buses: Buses, requests: IpcRequests, mut events: Events) -> Ended {
    let conn = match crate::bus::own_session(&buses).await {
        Ok(c) => c,
        Err(e) => return Ended::NoBus(e),
    };
    // Served before the name is asked for, so a call that arrives as
    // soon as the name is owned is answered.
    if let Err(e) = conn.object_server().at(PATH, Front { requests }).await {
        return Ended::Failed(e);
    }
    match conn
        .request_name_with_flags(DBUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        // zbus says `NameTaken` for an `Exists` reply.
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            return Ended::Taken;
        }
        Err(e) => return Ended::Failed(e),
    }
    let emitter = match SignalEmitter::new(&conn, PATH) {
        Ok(e) => e,
        Err(e) => return Ended::Failed(e),
    };
    while let Some(ev) = events.recv().await {
        let kind = ev.get("event").and_then(Answer::as_str).unwrap_or_default();
        if let Err(e) = Front::changed(&emitter, kind, &ev.to_string()).await {
            log::debug!("D-Bus `Changed` not sent: {e}");
        }
    }
    Ended::Done
}

/// Says why the front end ended, once.
fn said(ended: Ended) {
    match ended {
        Ended::Done => {}
        Ended::NoBus(e) => log::info!("no D-Bus IPC ({DBUS_NAME}): no session bus: {e}"),
        Ended::Taken => log::warn!(
            "no D-Bus IPC: another instance owns {DBUS_NAME} on the session bus; \
             this one runs without it"
        ),
        Ended::Failed(e) => log::warn!("no D-Bus IPC ({DBUS_NAME}): {e}"),
    }
}

/// The front end, served on the shared services runtime until dropped.
#[derive(Debug)]
pub struct FrontEnd {
    _stop: Option<oneshot::Sender<()>>,
}

/// Serve the front end on `services`' shared runtime and session bus,
/// for `requests` (the logic loop's request channel); hand the returned
/// sink every event the socket's watchers get (`Live::events_to`'s
/// role). Serving stops when the [`FrontEnd`] is dropped.
pub fn spawn(services: &Services, requests: IpcRequests) -> (FrontEnd, EventSink) {
    let (sink, rx) = events();
    let stop =
        services.spawn_task(move |buses| async move { said(serve(buses, requests, rx).await) });
    (FrontEnd { _stop: stop }, sink)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use futures_lite::StreamExt;
    use serde_json::json;
    use strand_core::ipc::{IpcReceiver, channel};
    use zbus::object_server::Interface as _;

    use super::*;
    use crate::testing::PrivateBus;

    #[test]
    fn the_interface_is_the_one_name() {
        assert_eq!(Front::name().as_str(), DBUS_NAME);
    }

    #[test]
    fn empty_fields_are_refused_as_the_socket_refuses_them() {
        assert!(need("a.b", "`get` needs `path`").is_ok());
        let Err(zbus::fdo::Error::Failed(why)) = need("", "`get` needs `path`") else {
            panic!("an empty path passed");
        };
        assert_eq!(why, "`get` needs `path`");
        // Each as the socket's `parse` words it.
        for (line, why) in [
            (r#"{"cmd":"get","path":""}"#, "`get` needs `path`"),
            (
                r#"{"cmd":"set","path":"","value":"x"}"#,
                "`set` needs a `path` and a `value`",
            ),
            (r#"{"cmd":"reset","path":""}"#, "`reset` needs a `path`"),
            (r#"{"cmd":"call","target":""}"#, "`call` needs `target`"),
        ] {
            assert_eq!(ipc::parse(line), Err(why.to_string()), "{line}");
        }
    }

    /// A logic loop stand-in on a thread of its own: answers each request
    /// with what `answer` makes of it, until `stop`.
    fn answering(
        rx: IpcReceiver,
        stop: Arc<AtomicBool>,
        answer: impl Fn(&Request) -> Answer + Send + 'static,
    ) -> std::thread::JoinHandle<Vec<Request>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                for call in rx.drain() {
                    let a = answer(&call.request);
                    seen.push(call.request);
                    call.reply.send(a);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            seen
        })
    }

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
        fn reset(&self, path: &str) -> zbus::Result<String>;
        fn call(&self, target: &str, args: &[&str]) -> zbus::Result<String>;
        fn reload(&self, hard: bool) -> zbus::Result<String>;
        #[zbus(signal)]
        fn changed(&self, event: &str, data: &str) -> zbus::Result<()>;
    }

    fn failed(r: zbus::Result<String>) -> String {
        match r {
            Err(zbus::Error::MethodError(name, Some(why), _)) => {
                assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.Failed");
                why
            }
            other => panic!("not refused: {other:?}"),
        }
    }

    /// Every method becomes its request and answers with the logic
    /// loop's answer; a refusal is a `Failed` error; an empty field never
    /// reaches the loop; each event is a `Changed` signal; a second front
    /// end on the bus runs without the name.
    #[test]
    fn methods_reach_the_logic_loop_and_events_are_signals() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = channel(|| {});
        let stop = Arc::new(AtomicBool::new(false));
        let logic = answering(rx, stop.clone(), |r| match r {
            Request::Get { path } if path == "bar.label" => {
                json!({"ok": true, "value": "hi", "text": "hi", "layer": null})
            }
            Request::Reload { hard } => json!({"ok": true, "event": {"hard": hard}}),
            Request::Get { path } => ipc::refused(format!("no `{path}`")),
            _ => ipc::ok(),
        });
        let (sink, events) = events();
        let served = rt.spawn(serve(bus.buses(), tx.clone(), events));
        assert!(bus.wait_for_name(DBUS_NAME, Duration::from_secs(10)));
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let shell = ShellProxy::new(&conn).await.unwrap();
            let got: serde_json::Value =
                serde_json::from_str(&shell.get("bar.label").await.unwrap()).unwrap();
            assert_eq!(
                got,
                json!({"ok": true, "value": "hi", "text": "hi", "layer": null})
            );
            assert_eq!(failed(shell.get("bar.nope").await), "no `bar.nope`");
            assert_eq!(failed(shell.get("").await), "`get` needs `path`");
            assert_eq!(failed(shell.call("", &[]).await), "`call` needs `target`");
            assert_eq!(shell.set("bar.label", "").await.unwrap(), r#"{"ok":true}"#);
            shell.toggle("bar.open").await.unwrap();
            shell.clear("$accent").await.unwrap();
            shell.reset("bar.q").await.unwrap();
            shell.call("media.seek", &["5", "x y"]).await.unwrap();
            let reloaded: serde_json::Value =
                serde_json::from_str(&shell.reload(true).await.unwrap()).unwrap();
            assert_eq!(reloaded, json!({"ok": true, "event": {"hard": true}}));
            // Events: each a `Changed` signal, its kind first.
            let mut changed = shell.receive_changed().await.unwrap();
            sink.send(&json!({"event": "reload", "files": ["bar.strand"]}));
            let sig = tokio::time::timeout(Duration::from_secs(10), changed.next())
                .await
                .unwrap()
                .unwrap();
            let args = sig.args().unwrap();
            assert_eq!(*args.event(), "reload");
            let data: serde_json::Value = serde_json::from_str(args.data()).unwrap();
            assert_eq!(data, json!({"event": "reload", "files": ["bar.strand"]}));
        });
        // A second front end on the same bus does not get the name.
        let (_sink2, events2) = super::events();
        let second = rt.block_on(serve(bus.buses(), tx.clone(), events2));
        assert!(matches!(second, Ended::Taken), "{second:?}");
        // No bus: said, not served.
        let (_sink3, events3) = super::events();
        let none = rt.block_on(serve(Buses::none(), tx, events3));
        assert!(matches!(none, Ended::NoBus(_)), "{none:?}");
        // The shell gone (its sink dropped): the front end ends.
        drop(sink);
        let done = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(10), served).await })
            .unwrap()
            .unwrap();
        assert!(matches!(done, Ended::Done), "{done:?}");
        stop.store(true, Ordering::SeqCst);
        let seen = logic.join().unwrap();
        assert_eq!(
            seen,
            [
                Request::Get {
                    path: "bar.label".into()
                },
                Request::Get {
                    path: "bar.nope".into()
                },
                Request::Set {
                    path: "bar.label".into(),
                    value: String::new()
                },
                Request::Toggle {
                    path: "bar.open".into()
                },
                Request::Clear {
                    path: "$accent".into()
                },
                Request::Reset {
                    path: "bar.q".into()
                },
                Request::Call {
                    target: "media.seek".into(),
                    args: vec!["5".into(), "x y".into()]
                },
                Request::Reload { hard: true },
            ]
        );
    }

    /// The logic loop gone: a call is refused, never left hanging.
    #[test]
    fn a_call_after_the_loop_ended_is_refused() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = channel(|| {});
        drop(rx);
        let (_sink, events) = events();
        let _served = rt.spawn(serve(bus.buses(), tx, events));
        assert!(bus.wait_for_name(DBUS_NAME, Duration::from_secs(10)));
        rt.block_on(async {
            let conn = zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            let shell = ShellProxy::new(&conn).await.unwrap();
            assert_eq!(
                failed(shell.get("bar.label").await),
                "the shell's logic loop is gone"
            );
        });
    }
}
