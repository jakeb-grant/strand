//! `properties` keeps one connection per bus: a `from dbus` check
//! refreshed every `TTL` asks over it instead of connecting anew, callers
//! asking at once share it, and a bus that restarted is reached on a new
//! one. Connections are counted on a private bus by a monitor of its
//! `Hello` calls (every connection's first call).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use strand_introspect::{Bus, properties};

/// A private `dbus-daemon` of its own configuration, killed on drop.
struct PrivateBus {
    child: Child,
    dir: PathBuf,
    address: String,
}

fn required() -> bool {
    std::env::var_os("STRAND_REQUIRE_DBUS").is_some()
}

impl PrivateBus {
    fn start(test: &str) -> Option<PrivateBus> {
        let have = Command::new("sh")
            .args(["-c", "command -v dbus-daemon"])
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !have {
            assert!(!required(), "{test}: no dbus-daemon (STRAND_REQUIRE_DBUS)");
            eprintln!("{test}: skipped, no dbus-daemon");
            return None;
        }
        let dir =
            std::env::temp_dir().join(format!("strand-introspect-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\"\n \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig>\n  <type>session</type>\n  <listen>unix:path={}/bus</listen>\n  \
             <auth>EXTERNAL</auth>\n  <policy context=\"default\">\n    \
             <allow send_destination=\"*\" eavesdrop=\"true\"/>\n    <allow eavesdrop=\"true\"/>\n    \
             <allow own=\"*\"/>\n  </policy>\n</busconfig>\n",
            dir.display()
        );
        std::fs::write(dir.join("bus.conf"), config).unwrap();
        let address = format!("unix:path={}/bus", dir.display());
        let child = spawn(&dir);
        Some(PrivateBus {
            child,
            dir,
            address,
        })
    }

    /// Kills the daemon and starts another at the same address.
    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(self.dir.join("bus"));
        self.child = spawn(&self.dir);
    }
}

fn spawn(dir: &Path) -> Child {
    let child = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", dir.join("bus.conf").display()))
        .args(["--nofork", "--nopidfile"])
        .stdout(Stdio::null())
        .spawn()
        .expect("dbus-daemon starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.join("bus").exists() {
        assert!(Instant::now() < deadline, "the private bus never listened");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Counts the `Hello` calls on `address` (the connections made) from now
/// on, on a thread of its own, until the bus goes.
fn count_connections(address: &str) -> Arc<AtomicUsize> {
    let hellos = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (count, address) = (hellos.clone(), address.to_string());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let conn = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Monitoring"),
                "BecomeMonitor",
                &(vec!["type='method_call',member='Hello'"], 0u32),
            )
            .await
            .unwrap();
            let mut stream = zbus::MessageStream::from(&conn);
            let _ = ready_tx.send(());
            loop {
                let next = std::future::poll_fn(|cx| {
                    zbus::export::futures_core::Stream::poll_next(
                        std::pin::Pin::new(&mut stream),
                        cx,
                    )
                })
                .await;
                match next {
                    Some(Ok(m)) => {
                        if m.header().member().is_some_and(|n| n.as_str() == "Hello") {
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        });
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the monitor started");
    hellos
}

/// The connections counted, once the monitor has seen what was sent.
fn counted(hellos: &AtomicUsize) -> usize {
    std::thread::sleep(Duration::from_millis(200));
    hellos.load(Ordering::SeqCst)
}

/// The bus daemon's own object: it is introspectable and has properties.
fn ask(bus: &Bus) -> Result<Vec<strand_introspect::Property>, String> {
    properties(bus, "org.freedesktop.DBus", "/org/freedesktop/DBus")
}

#[test]
fn refreshes_reuse_one_connection_and_a_restarted_bus_gets_a_new_one() {
    let Some(mut bus) = PrivateBus::start("reuse") else {
        return;
    };
    let hellos = count_connections(&bus.address);
    let target = Bus::Address(bus.address.clone());
    // Five refreshes (what five `TTL`s of a `from dbus` check ask).
    for _ in 0..5 {
        let props = ask(&target).unwrap();
        assert!(
            props.iter().any(|p| p.name == "Features"),
            "the bus's properties: {props:?}"
        );
    }
    assert_eq!(counted(&hellos), 1, "five questions, connections made");
    // Callers asking at once share it.
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let target = target.clone();
            std::thread::spawn(move || ask(&target))
        })
        .collect();
    for t in threads {
        t.join().unwrap().unwrap();
    }
    assert_eq!(counted(&hellos), 1, "callers at once made connections");
    // An answer that is an error (no such name) keeps the connection.
    assert!(properties(&target, "org.example.Nobody", "/").is_err());
    assert!(ask(&target).is_ok());
    assert_eq!(counted(&hellos), 1);

    // The bus restarts: the kept connection is broken, so the next
    // question connects once more, and that connection is kept.
    bus.restart();
    let hellos = count_connections(&bus.address);
    for _ in 0..3 {
        ask(&target).unwrap();
    }
    assert_eq!(counted(&hellos), 1, "after the restart, connections made");
}

/// A connection that owns `name` on `address` and never answers what is
/// asked of it (it has no object server), kept until the returned sender
/// is dropped.
fn silent_peer(address: &str, name: &'static str) -> std::sync::mpsc::Sender<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let address = address.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let conn = rt.block_on(async move {
            let conn = zbus::connection::Builder::address(address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
            conn.request_name(name).await.unwrap();
            conn
        });
        let _ = ready_tx.send(());
        let _ = stop_rx.recv();
        drop(conn);
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the silent peer started");
    stop_tx
}

/// A "bus" that accepts connections and never says a word: connecting to
/// it hangs until the caller gives up.
fn mute_bus(dir: &Path) -> (Bus, std::sync::mpsc::Sender<()>) {
    let path = dir.join("mute");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        while stop_rx.try_recv().is_err() {
            if let Ok((s, _)) = listener.accept() {
                held.push(s);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    (
        Bus::Address(format!("unix:path={}", path.display())),
        stop_tx,
    )
}

#[test]
fn a_question_that_times_out_does_not_wait_for_another_callers_connect() {
    let Some(bus) = PrivateBus::start("bound") else {
        return;
    };
    let _peer = silent_peer(&bus.address, "org.example.Silent");
    let target = Bus::Address(bus.address.clone());
    // The kept connection to the private bus is made now.
    ask(&target).unwrap();
    let (mute, _stop) = mute_bus(&bus.dir);

    // A question nobody answers: it times out at TIMEOUT and drops the
    // kept connection...
    let start = Instant::now();
    let silent = {
        let target = target.clone();
        std::thread::spawn(move || {
            let r = properties(&target, "org.example.Silent", "/");
            (r, start.elapsed())
        })
    };
    // ...while, from half way through, another caller is connecting to a
    // bus that never answers, until its own deadline (1.5 TIMEOUT).
    std::thread::sleep(strand_introspect::TIMEOUT / 2);
    let connecting = std::thread::spawn(move || ask(&mute));

    let (r, took) = silent.join().unwrap();
    assert!(r.is_err(), "the silent peer answered: {r:?}");
    assert!(
        took < strand_introspect::TIMEOUT + Duration::from_millis(400),
        "the timed-out question took {took:?}, past its {:?} bound",
        strand_introspect::TIMEOUT
    );
    assert!(connecting.join().unwrap().is_err());
    // The hung connection was dropped: the bus is reached on a new one.
    assert!(ask(&target).is_ok());
}
