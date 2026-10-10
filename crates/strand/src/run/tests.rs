use super::sleep::{Inbox, Sleeper};
use super::*;
use strand_compiler::instantiate::SceneMirror;
use strand_compiler::reconcile::loader::Loader;
use strand_scene::{Prop, PropValue};

pub(crate) fn screen(id: &str, name: &str) -> ScreenInfo {
    ScreenInfo {
        id: id.into(),
        name: name.into(),
        make: "Make".into(),
        model: id.into(),
        description: String::new(),
        scale: 1.0,
        width: 1920.0,
        height: 1080.0,
    }
}

/// The config in `dir` loaded once (no watcher, no cache).
fn load(dir: &Path) -> Outcome {
    let out = Loader::new(dir, crate::services::schema().clone(), None).boot();
    assert_eq!(out.errors(), 0, "{:?}", out.diagnostics);
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A calloop channel is read through a loop: one on a thread of its
/// own hands the diffs to a plain receiver.
pub(crate) fn inbox(rx: Channel<SceneDiff>) -> std::sync::mpsc::Receiver<SceneDiff> {
    let (tx, inbox) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut el = EventLoop::<bool>::try_new().unwrap();
        el.handle()
            .insert_source(rx, move |event, _, closed| match event {
                Event::Msg(d) => {
                    let _ = tx.send(d);
                }
                Event::Closed => *closed = true,
            })
            .unwrap();
        let mut closed = false;
        while !closed {
            el.dispatch(None, &mut closed).unwrap();
        }
    });
    inbox
}

/// The scene as the main thread sees it, fed from the diff channel.
struct Mirror {
    inbox: std::sync::mpsc::Receiver<SceneDiff>,
    scene: SceneMirror,
    /// Every diff must leave the bar up and the error overlay shut
    /// (saves that are valid once complete never flash it).
    steady: bool,
    /// Set while a save may really have removed a file (it was missing
    /// past the watcher's removal grace): the blank and steady
    /// checks are off.
    excused: bool,
    /// The last `reduced_motion` a diff carried.
    reduced: Option<bool>,
}

impl Mirror {
    fn new(rx: Channel<SceneDiff>) -> Self {
        Self {
            inbox: inbox(rx),
            scene: SceneMirror::new(),
            steady: false,
            excused: false,
            reduced: None,
        }
    }

    /// Apply one diff, checking what every diff must keep.
    fn apply(&mut self, what: &str, diff: &SceneDiff) {
        let had = !self.scene.roots().is_empty();
        if diff.reduced_motion.is_some() {
            self.reduced = diff.reduced_motion;
        }
        self.scene.apply(diff).unwrap();
        // No blank frame: once something shows, a diff never leaves
        // nothing.
        assert!(
            self.excused || !had || !self.scene.roots().is_empty(),
            "{what}: a blank frame"
        );
        if self.steady && !self.excused {
            assert_eq!(
                self.scene.of_kind(strand_scene::NodeKind::Bar).len(),
                1,
                "{what}: the bar went\n{}",
                self.scene.render()
            );
            assert!(
                self.scene.of_kind(strand_scene::NodeKind::Panel).is_empty(),
                "{what}: the error overlay flashed\n{}",
                self.scene.render()
            );
        }
    }

    /// Apply whatever diffs come for `d`.
    fn settle(&mut self, what: &str, d: Duration) {
        let deadline = Instant::now() + d;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            if let Ok(diff) = self.inbox.recv_timeout(left) {
                self.apply(what, &diff);
            }
        }
    }

    /// Apply diffs until `done` holds (10 s at most).
    fn until(&mut self, what: &str, done: impl Fn(&SceneMirror) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.scene) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(diff) => self.apply(what, &diff),
                Err(_) => panic!("{what}:\n{}", self.scene.render()),
            }
        }
    }

    /// Until a diff tells render `reduced_motion` is `want`.
    fn until_reduced(&mut self, want: bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.reduced != Some(want) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(diff) => self.apply("reduced motion", &diff),
                Err(_) => panic!("reduced_motion never {want}: {:?}", self.reduced),
            }
        }
    }

    fn texts(&self) -> Vec<String> {
        let mut t = self.scene.texts();
        t.sort();
        t
    }
}

/// The logic side of `strand run` without Wayland: monitors arrive as
/// messages and each gets its bar; surface input (hover, pressed,
/// click, secondary, scroll with its `dy, dx`) and sizes reach it;
/// an unplugged monitor's bar is parked and comes back with its
/// state, and once forgotten comes back fresh; `Shutdown` ends the
/// thread after the persisted state reached the disk.
#[test]
fn the_logic_thread_follows_the_monitors() {
    let dir = temp_dir("monitors");
    std::fs::write(
        dir.join("bar.strand"),
        "state total = 0 persist\nbar Top {\n  state n = 0\n  state e = \"-\"\n  opacity: hover ? 0.5 : 1\n  height: self.width > 1000 ? 40 : 32\n  when pressed { opacity: 0.25 }\n  on click {\n    n += 1\n    total += 1\n  }\n  on secondary { e = \"secondary\" }\n  on scroll(dy, dx) { e = join(\",\", dy, dx) }\n  text join(\" \", screen.name, n, total, e)\n}\n",
    )
    .unwrap();
    let program = load(&dir);
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let persist = storage.persist.clone().unwrap();
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t = std::thread::spawn(move || logic(program, storage, from_main, tx, Live::default()));
    let mut m = Mirror::new(rx);
    m.until("A's bar", |s| s.texts() == ["DP-1 0 0 -"]);
    let a = m.scene.roots()[0];
    let send = |msg| to_logic.send(msg).unwrap();
    // Surface input and size reach the bar's node.
    send(ToLogic::Flag {
        node: a,
        flag: NodeFlag::Hover,
        on: true,
    });
    send(ToLogic::Size {
        node: a,
        width: 1920.0,
        height: 32.0,
    });
    m.until("hover and size", |s| {
        s.prop(a, Prop::Opacity) == Some(&PropValue::Number(0.5))
            && s.prop(a, Prop::Height) == Some(&PropValue::Number(40.0))
    });
    // Layout facts are answered with their batch number, even when
    // they change nothing (render holds a query's frame for it).
    send(ToLogic::Layout {
        seq: 7,
        sizes: vec![(a, 1920.0, 40.0)],
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let diff = m.inbox.recv_timeout(left).expect("an answer to the facts");
        m.apply("facts", &diff);
        if diff.layout_seen == Some(7) {
            break;
        }
    }
    send(ToLogic::Flag {
        node: a,
        flag: NodeFlag::Pressed,
        on: true,
    });
    m.until("pressed", |s| {
        s.prop(a, Prop::Opacity) == Some(&PropValue::Number(0.25))
    });
    send(ToLogic::Flag {
        node: a,
        flag: NodeFlag::Pressed,
        on: false,
    });
    // A right click runs `on secondary` (not `on click`), and a
    // scroll runs `on scroll(dy, dx)` with its deltas in that order.
    send(ToLogic::Event {
        node: a,
        event: NodeEvent::Secondary,
    });
    m.until("a right click", |s| s.texts() == ["DP-1 0 0 secondary"]);
    send(ToLogic::Event {
        node: a,
        event: NodeEvent::Scroll { dy: 1.5, dx: -2.5 },
    });
    m.until("a scroll", |s| s.texts() == ["DP-1 0 0 1.5,-2.5"]);
    send(ToLogic::Event {
        node: a,
        event: NodeEvent::Click,
    });
    m.until("a click", |s| s.texts() == ["DP-1 1 1 1.5,-2.5"]);
    let two = || vec![screen("A", "DP-1"), screen("B", "HDMI-A-1")];
    send(ToLogic::Screens(two()));
    m.until("B's bar", |s| s.roots().len() == 2);
    let b = *m.scene.roots().iter().find(|&&r| r != a).unwrap();
    send(ToLogic::Event {
        node: b,
        event: NodeEvent::Click,
    });
    m.until("a click on B", |s| {
        let mut t = s.texts();
        t.sort();
        t == ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 1 2 -"]
    });
    // B unplugged: its bar is parked; back within 30 s, with its `n`.
    send(ToLogic::Screens(vec![screen("A", "DP-1")]));
    m.until("B parked", |s| s.roots().len() == 1);
    send(ToLogic::Screens(two()));
    m.until("B back", |s| s.roots().len() == 2);
    assert_eq!(m.texts(), ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 1 2 -"]);
    // Unplugged and forgotten: B comes back fresh (the top-level
    // `total` stays).
    send(ToLogic::Screens(vec![screen("A", "DP-1")]));
    m.until("B parked again", |s| s.roots().len() == 1);
    send(ToLogic::Forget("B".into()));
    send(ToLogic::Screens(two()));
    m.until("B fresh", |s| s.roots().len() == 2);
    assert_eq!(m.texts(), ["DP-1 1 2 1.5,-2.5", "HDMI-A-1 0 2 -"]);
    // Shutdown: the thread ends, and the persisted `total`, written
    // less than the 250 ms debounce ago, is on disk.
    send(ToLogic::Shutdown);
    assert_eq!(t.join().unwrap(), Ok(()));
    let stored = std::fs::read(persist.file_of("bar.total").unwrap()).unwrap();
    assert!(String::from_utf8_lossy(&stored).contains('2'), "{stored:?}");
    drop(persist);
    let _ = std::fs::remove_dir_all(dir);
}

/// Live reload without Wayland: the watcher and the compiler worker
/// feed the logic thread. A prop edit and an added node keep the
/// bar's state; a broken save keeps the last good tree and, after
/// 250 ms, opens the overlay listing the error with its fix; the fix
/// takes it away; `strand watch` streams each reload and `strand
/// reload` answers with its event.
/// A config that compiles with warnings (a check that could not run,
/// a `from poll` path naming a program): logged, and told to each
/// `strand watch` that subscribes, the boot's included; the reload
/// event that no longer has them resolves them.
#[test]
fn check_warnings_reach_strand_watch_and_are_resolved() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("warnings");
    let prog = dir.join("prog");
    std::fs::write(&prog, "#!/bin/sh\necho a=1\n").unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("doc"), "a=1\n").unwrap();
    let src = |path: &str| {
        format!(
            "service t from poll \"{path}\" every 5s {{ a: text = a }}\nbar Top {{ text \"x\" }}\n"
        )
    };
    let file = dir.join("bar.strand");
    std::fs::write(&file, src("./prog")).unwrap();
    let socket = dir.join("ipc.sock");
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&dir, None, wtx).unwrap();
    assert!(boot.build.is_some());
    assert_eq!(
        boot.diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect::<Vec<_>>(),
        ["check::poll_program"]
    );
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: Some(socket.clone()),
        buses: None,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let mut m = Mirror::new(rx);
    m.until("the bar", |s| s.texts() == ["x"]);
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    let mut next_event = || {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut events, &mut line).unwrap();
        serde_json::from_str::<Json>(&line).unwrap()
    };
    // The boot's warning, at once.
    let ev = next_event();
    assert_eq!(ev["event"], "notices", "{ev}");
    assert_eq!(ev["diagnostics"][0]["severity"], "warning", "{ev}");
    assert_eq!(ev["diagnostics"][0]["code"], "check::poll_program", "{ev}");
    assert!(ipc::describe(&ev).contains("is a program"), "{ev}");
    // Fixed: the reload commits with no warning (resolved).
    std::fs::write(&file, src("./doc")).unwrap();
    let ev = next_event();
    assert_eq!(ev["event"], "reload", "{ev}");
    assert_eq!(ev["diagnostics"], json!([]), "{ev}");
    // A watcher subscribing now hears nothing more.
    let mut late =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut late, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    // Broken again by a reload: the reload event carries it.
    std::fs::write(&file, src("./prog")).unwrap();
    let ev = next_event();
    assert_eq!(ev["diagnostics"][0]["code"], "check::poll_program", "{ev}");
    let mut line = String::new();
    std::io::BufRead::read_line(&mut late, &mut line).unwrap();
    let ev: Json = serde_json::from_str(&line).unwrap();
    assert_eq!(
        ev["event"], "reload",
        "the late watcher's first event: {ev}"
    );
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saves_reload_live_with_state_kept() {
    let dir = temp_dir("live");
    let bar = |opacity: &str, extra: &str| {
        format!(
            "bar Top {{\n  state n = 0\n  on click {{ n += 1 }}\n  text join(\" \", \"n\", n) {{ opacity: {opacity} }}\n{extra}}}\n"
        )
    };
    let file = dir.join("bar.strand");
    std::fs::write(&file, bar("0.5", "")).unwrap();
    let socket = dir.join("ipc.sock");
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&dir, None, wtx).unwrap();
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: Some(socket.clone()),
        buses: None,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let mut m = Mirror::new(rx);
    m.until("the bar", |s| s.texts() == ["n 0"]);
    let root = m.scene.roots()[0];
    to_logic
        .send(ToLogic::Event {
            node: root,
            event: NodeEvent::Click,
        })
        .unwrap();
    m.until("a click", |s| s.texts() == ["n 1"]);
    // A watcher.
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    let mut next_event = || {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut events, &mut line).unwrap();
        serde_json::from_str::<Json>(&line).unwrap()
    };
    // A prop edit: patched in place, the state kept.
    let text = m.scene.of_kind(strand_scene::NodeKind::Text)[0];
    std::fs::write(&file, bar("0.75", "")).unwrap();
    m.until("the new opacity", |s| {
        s.prop(text, strand_scene::Prop::Opacity) == Some(&PropValue::Number(0.75))
    });
    assert_eq!(m.texts(), ["n 1"]);
    let ev = next_event();
    assert_eq!(ev["event"], "reload");
    assert_eq!(ev["classes"], json!(["prop"]), "{ev}");
    assert!(ev["timing"]["total_ms"].as_f64().is_some(), "{ev}");
    // A node added: created, the old ones kept.
    std::fs::write(&file, bar("0.75", "  text \"new\"\n")).unwrap();
    m.until("the new node", |s| s.texts().len() == 2);
    assert_eq!(m.texts(), ["n 1", "new"]);
    assert_eq!(next_event()["classes"], json!(["node-added"]));
    // Broken: the bar keeps running; after 250 ms the overlay.
    std::fs::write(&file, bar("0.75", "  txet \"new\"\n")).unwrap();
    let ev = next_event();
    assert!(ev["held"].as_array().is_some_and(|h| h.len() == 1), "{ev}");
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    assert_eq!(ev["diagnostics"][0]["help"], "did you mean `text`?", "{ev}");
    m.until("the overlay", |s| {
        s.of_kind(strand_scene::NodeKind::Panel).len() == 1
    });
    let listed = m.texts().join("\n");
    assert!(listed.contains("bar.strand:5:3: error"), "{listed}");
    assert!(listed.contains("did you mean `text`?"), "{listed}");
    assert!(
        m.texts().contains(&"n 1".to_string()),
        "the last good tree runs"
    );
    // Fixed: committed, the overlay gone, the state still kept.
    std::fs::write(&file, bar("0.75", "  text \"fixed\"\n")).unwrap();
    m.until("the fix", |s| {
        s.of_kind(strand_scene::NodeKind::Panel).is_empty() && s.texts().len() == 2
    });
    assert_eq!(m.texts(), ["fixed", "n 1"]);
    let ev = next_event();
    assert_eq!(ev["diagnostics"], json!([]), "{ev}");
    assert_eq!(ev["kept_over_default"], json!([]), "{ev}");
    assert_eq!(ev["ambiguous"], json!([]), "{ev}");
    // A new default while the cell holds another value: kept, and
    // `strand watch` names the cell (not only in prose).
    std::fs::write(
        &file,
        bar("0.75", "  text \"fixed\"\n").replace("state n = 0", "state n = 7"),
    )
    .unwrap();
    let ev = next_event();
    assert_eq!(ev["classes"], json!(["state-default"]), "{ev}");
    let kept = &ev["kept_over_default"];
    assert_eq!(kept.as_array().map(Vec::len), Some(1), "{ev}");
    assert!(
        kept[0]["path"].as_str().is_some_and(|p| p.ends_with(".n")),
        "{ev}"
    );
    assert_eq!(kept[0]["shown"], "1", "{ev}");
    assert_eq!(m.texts(), ["fixed", "n 1"]);
    // `strand reload`: answered with its event once done.
    let mut client =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ans = ipc::request(
        &mut client,
        &ipc::Request::Reload { hard: false },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ans["ok"], true, "{ans}");
    assert_eq!(ans["event"]["requested"], true, "{ans}");
    // A client that half-closes after its request (`nc -N`) still
    // gets its answer.
    {
        use std::io::{Read, Write};
        let mut raw = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        raw.write_all(ipc::encode(&ipc::Request::Reload { hard: false }).as_bytes())
            .unwrap();
        raw.shutdown(std::net::Shutdown::Write).unwrap();
        raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut answer = String::new();
        raw.read_to_string(&mut answer).unwrap();
        let ans: Json = serde_json::from_str(answer.trim()).unwrap();
        assert_eq!(ans["ok"], true, "{ans}");
    }
    // `--hard`: state dropped, every surface recreated.
    let ans = ipc::request(
        &mut client,
        &ipc::Request::Reload { hard: true },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ans["event"]["classes"], json!(["hard"]), "{ans}");
    m.until("a fresh bar", |s| s.texts().contains(&"n 7".to_string()));
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    assert!(!socket.exists(), "the socket is removed at exit");
    let _ = std::fs::remove_dir_all(dir);
}

/// The live pipeline on `dir` (no cache): the compiler worker, the
/// logic thread with one monitor, a mirror of its scene.
fn spawn_live(
    dir: &Path,
    socket: Option<PathBuf>,
) -> (
    Worker,
    calloop::channel::Sender<ToLogic>,
    std::thread::JoinHandle<Result<(), String>>,
    Mirror,
) {
    spawn_live_with(dir, socket, None, Storage::none())
}

/// [`spawn_live`] with the services on `buses`, with storage.
fn spawn_live_with(
    dir: &Path,
    socket: Option<PathBuf>,
    buses: Option<strand_services::Buses>,
    storage: Storage,
) -> (
    Worker,
    calloop::channel::Sender<ToLogic>,
    std::thread::JoinHandle<Result<(), String>>,
    Mirror,
) {
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(dir, None, wtx).unwrap();
    compiler.register_own_writes(&storage);
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket,
        buses,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t = std::thread::spawn(move || logic(boot, storage, from_main, tx, live));
    (compiler, to_logic, t, Mirror::new(rx))
}

/// A first config with a typo and no last good version: nothing runs
/// but the overlay; the fix mounts the per-monitor bar with `screen`
/// in scope (the builtin services are there from the start).
#[test]
fn a_config_broken_at_first_boot_runs_once_fixed() {
    let dir = temp_dir("first-boot");
    let file = dir.join("bar.strand");
    std::fs::write(&file, "bar Top {\n  txet screen.name\n}\n").unwrap();
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
    m.until("the overlay", |s| {
        s.of_kind(strand_scene::NodeKind::Panel).len() == 1
    });
    std::fs::write(&file, "bar Top {\n  text screen.name\n}\n").unwrap();
    m.until("the bar", |s| {
        s.of_kind(strand_scene::NodeKind::Panel).is_empty() && s.texts() == ["DP-1"]
    });
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// While the lock is shown, saves that change it wait: `strand watch`
/// and `strand reload` hear `"deferred": true` at once, a second
/// deferred save absorbs the first, and after the unlock both the
/// lock edit and the bar edit land. The lock is driven as the binary
/// drives it (`run/lock.rs`): the compositor's `Locked` and, after an
/// `auth` success, `Unlocked` reach logic as `ToLogic::LockState`; while
/// locked a config write of `false` changes nothing, and the unlock
/// writes the two-way `open` false itself.
#[test]
fn lock_edits_wait_for_the_unlock_and_then_land() {
    use strand_compiler::instantiate::SessionLock;
    let dir = temp_dir("lock");
    let src = |lock: &str, bar: &str| {
        format!(
            "export state locked = true\nlock L {{\n  open: <-> locked\n  on click {{ locked = false }}\n  text \"{lock}\"\n}}\nbar Top {{\n  on click {{ locked = true }}\n  text \"{bar}\"\n}}\n"
        )
    };
    let file = dir.join("shell.strand");
    std::fs::write(&file, src("lock a", "bar a")).unwrap();
    let socket = dir.join("ipc.sock");
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("lock and bar", |s| {
        let mut t = s.texts();
        t.sort();
        t == ["bar a", "lock a"]
    });
    // The compositor locks the session.
    to_logic
        .send(ToLogic::LockState(SessionLock::Locked))
        .unwrap();
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    let mut next_event = || {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut events, &mut line).unwrap();
        serde_json::from_str::<Json>(&line).unwrap()
    };
    // An edit that leaves the lock alone commits at once, lock shown.
    std::fs::write(&file, src("lock a", "bar a2")).unwrap();
    let ev = next_event();
    assert_eq!(ev["deferred"], false, "{ev}");
    m.until("the bar edit while locked", |s| {
        s.texts().contains(&"bar a2".to_string())
    });
    std::fs::write(&file, src("lock b", "bar b")).unwrap();
    let ev = next_event();
    assert_eq!(ev["deferred"], true, "{ev}");
    assert_eq!(ev["classes"], json!(["lock-deferred"]), "{ev}");
    assert_eq!(
        ev["notices"],
        json!(["shell.strand: waits for the unlock (the lock changed while it is shown)"]),
        "{ev}"
    );
    // Once a lock edit waits, a later save waits with it (the
    // loader's sources carry the lock edit; decisions.md).
    // `strand reload --hard` while locked: answered at once,
    // deferred (and it absorbs the deferred save).
    let mut client =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    std::fs::write(&file, src("lock b", "bar c")).unwrap();
    let ev = next_event();
    assert_eq!(ev["deferred"], true, "{ev}");
    // A save that waits only because a lock edit does is told so.
    assert!(
        ev["notices"].as_array().is_some_and(|n| n.contains(&json!(
            "shell.strand: waits for the unlock (a lock edit is waiting)"
        ))),
        "{ev}"
    );
    let ans = ipc::request(
        &mut client,
        &ipc::Request::Reload { hard: true },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ans["event"]["deferred"], true, "{ans}");
    let _ = next_event();
    assert_eq!(m.texts(), ["bar a2", "lock a"], "nothing committed yet");
    // A config write of `false` while locked: the lock stays shown and
    // the edits keep waiting (only a password unlocks).
    let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
    to_logic
        .send(ToLogic::Event {
            node: lock,
            event: NodeEvent::Click,
        })
        .unwrap();
    // `strand watch` hears why (and the overlay says so).
    let ev = next_event();
    assert_eq!(ev["event"], "notices", "{ev}");
    assert_eq!(
        ev["notices"],
        json!([strand_compiler::instantiate::IGNORED_CLOSE]),
        "{ev}"
    );
    m.until("the ignored close's notice", |s| {
        s.texts()
            .contains(&strand_compiler::instantiate::IGNORED_CLOSE.to_string())
    });
    m.settle("the ignored close", Duration::from_millis(300));
    let texts = m.texts();
    assert!(
        texts.contains(&"bar a2".to_string()) && texts.contains(&"lock a".to_string()),
        "still locked: {texts:?}"
    );
    assert!(
        !texts.contains(&"bar c".to_string()),
        "nothing committed: {texts:?}"
    );
    assert_eq!(m.scene.of_kind(strand_scene::NodeKind::Lock), [lock]);
    // Unlock (`auth`'s token released the compositor's lock): the
    // newest deferred build lands, with both edits.
    to_logic
        .send(ToLogic::LockState(SessionLock::Unlocked))
        .unwrap();
    m.until("the bar edit", |s| s.texts().contains(&"bar c".to_string()));
    let ev = next_event();
    assert_eq!(ev["deferred"], false, "{ev}");
    assert!(
        ev["files"].as_array().is_some_and(|f| f
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.ends_with("shell.strand")))),
        "{ev}"
    );
    // The unlock wrote `locked` false through the two-way `open`.
    // Locked again: the lock shows its edit.
    let bar = m.scene.of_kind(strand_scene::NodeKind::Bar)[0];
    to_logic
        .send(ToLogic::Event {
            node: bar,
            event: NodeEvent::Click,
        })
        .unwrap();
    m.until("the lock edit", |s| {
        s.texts().contains(&"lock b".to_string())
    });
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// A runtime fault whose message holds a password input's value, here
/// transformed (`secret.upper()`, which no search for the value finds),
/// reaches `strand watch` and the log with its error redacted whole
/// while the password input is mounted, its location kept
/// (architecture.md, "The lock"; `lock::Secrets`).
#[test]
fn a_password_value_is_redacted_from_fault_messages() {
    let dir = temp_dir("redact");
    std::fs::write(
        dir.join("shell.strand"),
        "bar Top {\n  state secret = \"\"\n  input { type: password; text: <-> secret }\n  \
         text clock.format(secret.upper())\n}\n",
    )
    .unwrap();
    let socket = dir.join("ipc.sock");
    let (_compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("the input", |s| {
        !s.of_kind(strand_scene::NodeKind::Input).is_empty()
    });
    let input = m.scene.of_kind(strand_scene::NodeKind::Input)[0];
    let mut next = watch(&socket);
    // `%Q` is no time pattern: `clock.format` fails, quoting it.
    let password = "pa%Qss";
    to_logic
        .send(ToLogic::Write {
            node: input,
            prop: Prop::Text,
            value: PropValue::Text(password.into()),
        })
        .unwrap();
    let ev = loop {
        let ev = next();
        if ev["event"] == "fault" {
            break ev;
        }
    };
    let message = ev["message"].as_str().unwrap_or_default();
    assert!(
        message.ends_with(": <redacted>")
            && !message.contains(password)
            && !message.contains(&password.to_uppercase()),
        "{ev}"
    );
    assert!(ev["at"].as_str().is_some_and(|a| !a.is_empty()), "{ev}");
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A watcher on `socket`: the next event it hears.
fn watch(socket: &Path) -> impl FnMut() -> Json {
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    move || {
        let mut line = String::new();
        std::io::BufRead::read_line(&mut events, &mut line).unwrap();
        serde_json::from_str::<Json>(&line).unwrap()
    }
}

fn panels(s: &SceneMirror) -> usize {
    s.of_kind(strand_scene::NodeKind::Panel).len()
}

/// design.md: "The fix commits and the overlay vanishes", also when
/// the fix is an undo back to the last good text (nothing to commit).
#[test]
fn a_revert_to_the_last_good_text_closes_the_overlay() {
    let dir = temp_dir("revert");
    let file = dir.join("bar.strand");
    let good = "bar Top {\n  text \"a\"\n}\n";
    std::fs::write(&file, good).unwrap();
    let socket = dir.join("ipc.sock");
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("the bar", |s| s.texts() == ["a"]);
    let mut next_event = watch(&socket);
    std::fs::write(&file, "bar Top {\n  txet \"a\"\n}\n").unwrap();
    let ev = next_event();
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    m.until("the overlay", |s| panels(s) == 1);
    std::fs::write(&file, good).unwrap();
    let ev = next_event();
    assert_eq!(ev["diagnostics"], json!([]), "{ev}");
    assert_eq!(ev["held"], json!([]), "{ev}");
    m.until("the overlay gone", |s| panels(s) == 0);
    assert_eq!(m.texts(), ["a"]);
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// A deferred lock edit replayed after the unlock does not take away
/// the overlay of a broken save made after it: the config on disk is
/// still broken.
#[test]
fn an_unlock_replay_keeps_the_newer_errors() {
    let dir = temp_dir("lock-errors");
    let src = |lock: &str, bar: &str, el: &str| {
        format!(
            "export state locked = true\nlock L {{\n  open: locked\n  on click {{ locked = false }}\n  text \"{lock}\"\n}}\nbar Top {{\n  {el} \"{bar}\"\n}}\n"
        )
    };
    let file = dir.join("shell.strand");
    std::fs::write(&file, src("lock a", "bar a", "text")).unwrap();
    let socket = dir.join("ipc.sock");
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("lock and bar", |s| s.texts().len() == 2);
    let mut next_event = watch(&socket);
    std::fs::write(&file, src("lock b", "bar b", "text")).unwrap();
    assert_eq!(next_event()["deferred"], true);
    std::fs::write(&file, src("lock b", "bar b", "txet")).unwrap();
    let ev = next_event();
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    m.until("the overlay", |s| panels(s) == 1);
    let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
    to_logic
        .send(ToLogic::Event {
            node: lock,
            event: NodeEvent::Click,
        })
        .unwrap();
    m.until("the deferred edit", |s| {
        s.texts().contains(&"bar b".to_string())
    });
    let ev = next_event();
    assert_eq!(ev["deferred"], false, "{ev}");
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    assert_eq!(ev["held"].as_array().map(Vec::len), Some(1), "{ev}");
    // The replay's diff (the one with `bar b`) left the overlay up.
    assert_eq!(panels(&m.scene), 1, "the config is still broken");
    assert!(
        m.texts().join("\n").contains("unknown element"),
        "{:?}",
        m.texts()
    );
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// A hard reload asked for while locked, made stale by a bar edit
/// committed meanwhile, is still owed after the unlock; its replayed
/// event reports the newest attempt's problems (a broken save after
/// it), not a clean reload.
#[test]
fn a_replayed_hard_reload_reports_the_newer_errors() {
    let dir = temp_dir("hard-replay");
    let src = |bar: &str, el: &str| {
        format!(
            "export state locked = true\nlock L {{\n  open: locked\n  on click {{ locked = false }}\n  text \"lock\"\n}}\nbar Top {{\n  {el} \"{bar}\"\n}}\n"
        )
    };
    let file = dir.join("shell.strand");
    std::fs::write(&file, src("bar a", "text")).unwrap();
    let socket = dir.join("ipc.sock");
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("lock and bar", |s| s.texts().len() == 2);
    let mut next_event = watch(&socket);
    let mut client =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ans = ipc::request(
        &mut client,
        &ipc::Request::Reload { hard: true },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ans["event"]["deferred"], true, "{ans}");
    assert_eq!(next_event()["deferred"], true);
    // A bar edit commits at once (the hard reload is still owed).
    std::fs::write(&file, src("bar b", "text")).unwrap();
    assert_eq!(next_event()["deferred"], false);
    m.until("the bar edit", |s| s.texts().contains(&"bar b".to_string()));
    // Broken.
    std::fs::write(&file, src("bar c", "txet")).unwrap();
    let ev = next_event();
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    let lock = m.scene.of_kind(strand_scene::NodeKind::Lock)[0];
    to_logic
        .send(ToLogic::Event {
            node: lock,
            event: NodeEvent::Click,
        })
        .unwrap();
    let ev = next_event();
    assert_eq!(ev["classes"], json!(["hard"]), "{ev}");
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    assert_eq!(ev["held"].as_array().map(Vec::len), Some(1), "{ev}");
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// design.md: "Format-on-save and delete-then-create saves never
/// flash it": a broken text fixed within 100 ms (a formatter's second
/// write) lands without the overlay ever opening.
#[test]
fn a_save_fixed_at_once_never_opens_the_overlay() {
    let dir = temp_dir("format-on-save");
    let file = dir.join("bar.strand");
    std::fs::write(&file, "bar Top {\n  text \"a\"\n}\n").unwrap();
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
    m.until("the bar", |s| s.texts() == ["a"]);
    m.steady = true;
    for (i, gap) in [5u64, 40, 90].into_iter().enumerate() {
        // The editor's write, then the formatter's.
        std::fs::write(&file, format!("bar Top {{\ntext \"b{i}\"\n")).unwrap();
        std::thread::sleep(Duration::from_millis(gap));
        std::fs::write(&file, format!("bar Top {{\n  text \"b{i}\"\n}}\n")).unwrap();
        let want = format!("b{i}");
        m.until("the formatted save", |s| s.texts() == [want.as_str()]);
    }
    m.settle("after the saves", Duration::from_millis(500));
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// `strand reload` on a broken config: one compile and one event
/// (the watcher's own re-listing that follows finds nothing new).
#[test]
fn a_reload_of_a_broken_config_is_one_event() {
    use std::io::BufRead;
    let dir = temp_dir("reload-broken");
    let file = dir.join("bar.strand");
    std::fs::write(&file, "bar Top {\n  text \"a\"\n}\n").unwrap();
    let socket = dir.join("ipc.sock");
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, Some(socket.clone()));
    m.until("the bar", |s| s.texts() == ["a"]);
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    let mut next = |wait: Duration| -> Option<Json> {
        events.get_ref().set_read_timeout(Some(wait)).unwrap();
        let mut line = String::new();
        match events.read_line(&mut line) {
            Ok(n) if n > 0 => Some(serde_json::from_str(&line).unwrap()),
            _ => None,
        }
    };
    std::fs::write(&file, "bar Top {\n  txet \"a\"\n}\n").unwrap();
    let ev = next(Duration::from_secs(10)).expect("the broken save");
    assert_eq!(ev["diagnostics"][0]["severity"], "error", "{ev}");
    let mut client =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ans = ipc::request(
        &mut client,
        &ipc::Request::Reload { hard: false },
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(ans["event"]["requested"], true, "{ans}");
    assert_eq!(
        ans["event"]["diagnostics"][0]["severity"], "error",
        "the reload still reports the error: {ans}"
    );
    let ev = next(Duration::from_secs(10)).expect("the reload's event");
    assert_eq!(ev["requested"], true, "{ev}");
    if let Some(ev) = next(Duration::from_millis(500)) {
        panic!("a second event: {ev}");
    }
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// p95 of `v` (milliseconds).
pub(crate) fn p95(v: &[f64]) -> f64 {
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let i = ((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

/// Save to pixels through the real pipeline: each edit is written
/// in place, goes through the watcher (its 15 ms coalescing
/// included), the compiler worker and the logic thread, and its diff
/// is applied to a renderer and the bar painted (2560×40 at 1×).
/// Returns the token edits' and the markup edits' times (ms).
fn reload_latency(rounds: usize) -> (Vec<f64>, Vec<f64>) {
    use std::sync::Arc;
    use strand_scene::{NodeKind, PaintTarget, Painter, Scale, SceneOp, Size, SurfaceId};
    let dir = temp_dir("latency");
    let file = dir.join("bar.strand");
    let src = |bg: &str, extra: bool| {
        format!(
            "tokens base {{ bar.bg: {bg} }}\n\
             bar Top {{\n\
             \x20 state n = 0\n\
             \x20 edge: top; height: 40\n\
             \x20 bg: $bar.bg\n\
             \x20 on click {{ n += 1 }}\n\
             \x20 split {{\n\
             \x20   start  {{ text \"start\" }}\n\
             \x20   center {{ text clock.format(\"%H:%M\") }}\n\
             \x20   end    {{ text join(\"\", n) }}\n\
             {}\
             \x20 }}\n\
             }}\n",
            if extra { "    text \"extra\"\n" } else { "" }
        )
    };
    std::fs::write(&file, src("#204080", false)).unwrap();
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
    let font = std::fs::read(strand_text::test_font_path()).unwrap();
    let engine = strand_text::TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    let size = Size::new(2560, 40);
    let mut px = vec![0u8; (size.w * size.h * 4) as usize];
    let mut attached = false;
    // Apply diffs until `done` holds for one, painting each; the
    // time the matching one is painted.
    let mut until = |m: &mut Mirror, done: &dyn Fn(&SceneDiff) -> bool| -> (Instant, SceneDiff) {
        loop {
            let d = m
                .inbox
                .recv_timeout(Duration::from_secs(10))
                .expect("a diff");
            m.scene.apply(&d).unwrap();
            let hit = done(&d);
            let kept = hit.then(|| d.clone());
            r.apply(d);
            if !attached && let Some(&bar) = m.scene.of_kind(NodeKind::Bar).first() {
                r.attach_surface(SurfaceId(1), bar);
                attached = true;
            }
            let mut target = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 1).unwrap();
            r.paint(SurfaceId(1), &mut target);
            if let Some(d) = kept {
                return (Instant::now(), d);
            }
        }
    };
    until(&mut m, &|_| true);
    // The clock's text may tick during a token edit; nothing else
    // may change with it.
    let clock = m
        .scene
        .of_kind(NodeKind::Text)
        .into_iter()
        .find(|&n| {
            matches!(m.scene.prop(n, strand_scene::Prop::Text),
                Some(PropValue::Text(t)) if t.contains(':'))
        })
        .expect("the clock");
    let (mut tokens, mut markup) = (Vec::new(), Vec::new());
    let mut extra = false;
    for i in 1..=rounds {
        std::thread::sleep(Duration::from_millis(40));
        // A token edit: the bar's colour.
        let bg = format!("#{:02x}4080", (i * 7) % 256);
        let saved = Instant::now();
        std::fs::write(&file, src(&bg, extra)).unwrap();
        let (painted, d) = until(&mut m, &|d| {
            d.ops.iter().any(|o| matches!(o, SceneOp::SetTokens { .. }))
        });
        // A pure token edit: the token swap and nothing else.
        assert!(
            d.ops.iter().all(|o| match o {
                SceneOp::SetTokens { .. } => true,
                SceneOp::SetProp { id, .. } => *id == clock,
                _ => false,
            }),
            "{:#?}",
            d.ops
        );
        tokens.push(painted.duration_since(saved).as_secs_f64() * 1e3);
        std::thread::sleep(Duration::from_millis(40));
        // A markup edit: a node added or removed.
        extra = !extra;
        let saved = Instant::now();
        std::fs::write(&file, src(&bg, extra)).unwrap();
        let (painted, _) = until(&mut m, &|d| {
            d.ops
                .iter()
                .any(|o| matches!(o, SceneOp::Create { .. } | SceneOp::Remove { .. }))
        });
        markup.push(painted.duration_since(saved).as_secs_f64() * 1e3);
    }
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
    (tokens, markup)
}

/// design.md, "Live reload": at p95 a token edit shows within 35 ms
/// of save and a markup edit within 50 ms. Measured save → painted
/// buffer through the real watcher, compiler worker, logic thread
/// and renderer (the compositor's present is not in it). Checked on
/// an optimised build only, against the budget itself: CI runs
/// `cargo test --profile timing -p strand --bin strand reload_latency`
/// (an unoptimised repaint alone takes about half the token budget,
/// so a debug run would measure the build, not the design).
/// `STRAND_LATENCY_ROUNDS` sets the edits per kind (20).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "a budget test: run optimised (cargo test --release)"
)]
fn reload_latency_meets_its_budget() {
    use crate::bench::GATE_MISS;
    let rounds = std::env::var("STRAND_LATENCY_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(20);
    let (tokens, markup) = reload_latency(rounds);
    let (pt, pm) = (p95(&tokens), p95(&markup));
    eprintln!(
        "reload latency over {rounds} edits each: token p95 {pt:.1} ms (max {:.1}), markup p95 {pm:.1} ms (max {:.1})",
        tokens.iter().copied().fold(0.0, f64::max),
        markup.iter().copied().fold(0.0, f64::max),
    );
    assert!(
        pt <= 35.0,
        "{GATE_MISS}: token edits: p95 {pt:.1} ms: {tokens:?}"
    );
    assert!(
        pm <= 50.0,
        "{GATE_MISS}: markup edits: p95 {pm:.1} ms: {markup:?}"
    );
}

/// design.md, "Live reload": monitor changes show on the next frame.
/// The logic thread answers a plug, a scale change, an unplug and a
/// replug each in the first diff it sends after hearing of it (no
/// reload pipeline, no coalescing), with the whole change in it; the
/// main thread paints that diff in its next frame
/// (`bench.rs::reload_latency_to_the_presented_frame` times a plug on
/// sway).
#[test]
fn a_monitor_change_is_in_the_next_diff() {
    let dir = temp_dir("next-frame");
    std::fs::write(
        dir.join("bar.strand"),
        "bar Top {\n  state n = 0\n  on click { n += 1 }\n  text join(\" \", screen.name, screen.scale, n)\n}\n",
    )
    .unwrap();
    let (compiler, to_logic, t, mut m) = spawn_live(&dir, None);
    m.until("the bar", |s| s.texts() == ["DP-1 1 0"]);
    let a = m.scene.roots()[0];
    to_logic
        .send(ToLogic::Event {
            node: a,
            event: NodeEvent::Click,
        })
        .unwrap();
    m.until("a click", |s| s.texts() == ["DP-1 1 1"]);
    // Nothing else is coming: no clock, no animation.
    m.settle("quiet", Duration::from_millis(100));
    let next = |m: &mut Mirror, what: &str, msg: ToLogic| {
        to_logic.send(msg).unwrap();
        let d = m
            .inbox
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{what}: no diff"));
        m.apply(what, &d);
        m.texts()
    };
    let b = screen("B", "HDMI-A-1");
    let mut b2 = b.clone();
    b2.scale = 2.0;
    let a1 = screen("A", "DP-1");
    assert_eq!(
        next(
            &mut m,
            "a plug",
            ToLogic::Screens(vec![a1.clone(), b.clone()])
        ),
        ["DP-1 1 1", "HDMI-A-1 1 0"]
    );
    assert_eq!(
        next(
            &mut m,
            "a scale change",
            ToLogic::Screens(vec![a1.clone(), b2.clone()])
        ),
        ["DP-1 1 1", "HDMI-A-1 2 0"]
    );
    assert_eq!(
        next(&mut m, "an unplug", ToLogic::Screens(vec![a1.clone()])),
        ["DP-1 1 1"]
    );
    assert_eq!(
        next(&mut m, "a replug", ToLogic::Screens(vec![a1, b2])),
        ["DP-1 1 1", "HDMI-A-1 2 0"]
    );
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// The scene as text with surfaces in a fixed order.
pub(crate) fn canonical(scene: &SceneMirror) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for line in scene.render().lines() {
        if !line.starts_with(' ') || blocks.is_empty() {
            blocks.push(String::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    blocks.sort();
    blocks.concat()
}

/// What a cold boot of `dir` shows (on one monitor `A`).
fn cold_boot(dir: &Path) -> String {
    let out = load(dir);
    let build = out.build.unwrap();
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
    set_screens(&rt, &host, &[screen("A", "DP-1")]);
    let inst = Instance::from_build(&rt, &build, host, Storage::none());
    let mut m = SceneMirror::new();
    m.apply(&inst.flush().diff).unwrap();
    canonical(&m)
}

/// The reload fuzzer at the process level: random valid edits of a
/// two-file config, each saved in one of five editor styles (in
/// place, rename over, backup-then-rename, delete-then-create, a
/// symlink swapped to a new target), go through the real watcher and
/// compiler worker; after each the running scene equals a cold boot
/// of the same files, and no diff ever blanks it.
#[test]
fn five_save_styles_land_on_a_cold_boot() {
    let dir = temp_dir("styles");
    let store = dir.join("store");
    std::fs::create_dir_all(&store).unwrap();
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    // Valid variants of each file, chosen at random.
    let bar = |r: &mut dyn FnMut(u64) -> u64| -> String {
        let mut s = String::from("bar Top {\n  state n = ");
        s.push_str(&r(5).to_string());
        s.push_str("\n  height: ");
        s.push_str(&(24 + r(3) * 8).to_string());
        s.push('\n');
        if r(2) == 0 {
            s.push_str("  opacity: 0.");
            s.push_str(&(1 + r(9)).to_string());
            s.push('\n');
        }
        s.push_str("  row {\n");
        for i in 0..r(4) {
            s.push_str(&format!("    text join(\" \", theme.label, n, {i})\n"));
        }
        if r(2) == 0 {
            s.push_str("    Pill\n");
        }
        s.push_str("  }\n}\n");
        s
    };
    let theme = |r: &mut dyn FnMut(u64) -> u64| -> String {
        format!(
            "export let label = \"v{}\"\ntokens base {{ pill.gap: {}px }}\ncomponent Pill {{\n  state on = false\n  box {{ width: {}; on click {{ on = !on }} }}\n}}\n",
            r(7),
            r(9),
            10 + r(5)
        )
    };
    let bar_path = config.join("bar.strand");
    let theme_path = config.join("theme.strand");
    let first_bar = bar(&mut rnd);
    std::fs::write(&bar_path, &first_bar).unwrap();
    // theme.strand is a link into the store, as home-manager makes it.
    let mut version = 0;
    let target = store.join(format!("theme-{version}.strand"));
    std::fs::write(&target, theme(&mut rnd)).unwrap();
    std::os::unix::fs::symlink(&target, &theme_path).unwrap();
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&config, None, wtx).unwrap();
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: None,
        buses: None,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let mut m = Mirror::new(rx);
    let expect = cold_boot(&config);
    m.until("the boot", |s| canonical(s) == expect);
    // From here on every diff keeps the bar and never opens the
    // error overlay: a delete-then-create or backup-then-rename save
    // is briefly a missing file, never shown as an error.
    m.steady = true;
    let rounds = std::env::var("STRAND_SAVE_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    // How long a delete-then-create save leaves the file missing
    // (`STRAND_SAVE_GAP_MS`, 5 by default: an editor's). Past the
    // watcher's removal grace (a stalled machine, or a longer gap
    // set here to prove the excuse) the round is excused.
    let missing = Duration::from_millis(
        std::env::var("STRAND_SAVE_GAP_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
    );
    let grace = strand_watch::Options::default().removal_grace;
    let mut excused = 0;
    for round in 0..rounds {
        let which = rnd(2);
        let style = round % 5;
        let (path, text) = if which == 0 {
            (bar_path.clone(), bar(&mut rnd))
        } else {
            (theme_path.clone(), theme(&mut rnd))
        };
        // How long a delete-then-create file was missing: past the
        // watcher's 50 ms removal grace (a stalled machine) the
        // removal is real and may show, so a failure names it.
        let mut gap = String::new();
        match style {
            // In place: truncate and write.
            0 => std::fs::write(&path, &text).unwrap(),
            // Rename over (Helix, VS Code's atomic save).
            1 => {
                let tmp = config.join(".tmp-save");
                std::fs::write(&tmp, &text).unwrap();
                std::fs::rename(&tmp, &path).unwrap();
            }
            // Backup then rename (Vim's backupcopy=no).
            2 => {
                let backup = path.with_extension("strand~");
                std::fs::rename(&path, &backup).unwrap();
                std::fs::write(&path, &text).unwrap();
                std::fs::remove_file(&backup).unwrap();
            }
            // Delete, then create.
            3 => {
                // Taken before the removal and after the write, so
                // the gap read is never shorter than the real one.
                let removed = Instant::now();
                std::fs::remove_file(&path).unwrap();
                std::thread::sleep(missing);
                std::fs::write(&path, &text).unwrap();
                let elapsed = removed.elapsed();
                gap = format!(", missing {} ms", elapsed.as_millis());
                // Past the grace the removal is real: the bar may go
                // (or the overlay open, for an imported file) until
                // the file is back. CI run 37810171999 stalled here.
                m.excused = elapsed >= grace;
            }
            // A symlink swapped to a new target.
            _ => {
                version += 1;
                let target = store.join(format!("v{version}.strand"));
                std::fs::write(&target, &text).unwrap();
                let link = config.join(".link-tmp");
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(&target, &link).unwrap();
                std::fs::rename(&link, &path).unwrap();
            }
        }
        let expect = cold_boot(&config);
        let what = format!("round {round} (style {style}{gap})");
        m.until(&what, |s| canonical(s) == expect);
        if m.excused {
            // What the real removal sent may still be on its way (the
            // new text can equal the old, and an overlay opens 250 ms
            // after an error): let it land, then the scene must be
            // the cold boot's again.
            m.settle(&what, Duration::from_millis(400));
            m.until(&what, |s| canonical(s) == expect);
            m.excused = false;
            excused += 1;
        }
    }
    eprintln!("{excused} of {rounds} rounds missing a file past the removal grace");
    // An overlay a save had armed would open 250 ms after it.
    m.settle("after the saves", Duration::from_millis(400));
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// The main thread gone without a word (every sender dropped) ends
/// the logic thread too: the runtime's wake hook holds no sender.
#[test]
fn the_logic_thread_ends_with_the_main_thread() {
    let dir = temp_dir("orphan");
    std::fs::write(dir.join("bar.strand"), "bar Top { text \"hi\" }\n").unwrap();
    let program = load(&dir);
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let t =
        std::thread::spawn(move || logic(program, Storage::none(), from_main, tx, Live::default()));
    let mut m = Mirror::new(rx);
    m.until("the bar", |s| s.texts() == ["hi"]);
    drop(to_logic);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !t.is_finished() {
        assert!(Instant::now() < deadline, "the logic thread did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(t.join().unwrap(), Ok(()));
    let _ = std::fs::remove_dir_all(dir);
}

/// The sleep ends at the wall-clock wake even when the logic clock
/// has nothing due for long: the wake is a `CLOCK_REALTIME` timer,
/// so a deadline already past (a resume after a suspend, a clock set
/// forward) steps at once instead of after a monotonic countdown.
#[test]
fn a_wall_clock_wake_in_the_past_steps_at_once() {
    let (_tx, rx) = calloop::channel::channel::<ToLogic>();
    let (mut sleeper, _ping) = Sleeper::new(rx).unwrap();
    let mut inbox = Inbox::default();
    let now = SystemTime::now();
    let started = Instant::now();
    // The clock jumped past the wake: due an hour ago.
    sleeper
        .sleep(
            Some(Duration::from_secs(60)),
            Some(now - Duration::from_secs(3600)),
            now,
            &mut inbox,
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    // Due shortly: woken by the timer, not the 60 s timeout.
    let started = Instant::now();
    sleeper
        .sleep(
            Some(Duration::from_secs(60)),
            Some(SystemTime::now() + Duration::from_millis(50)),
            SystemTime::now(),
            &mut inbox,
        )
        .unwrap();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(40), "{waited:?}");
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    // The clock set back after the step: no wait at all.
    let started = Instant::now();
    sleeper
        .sleep(
            Some(Duration::from_secs(60)),
            Some(SystemTime::now() + Duration::from_secs(60)),
            SystemTime::now() + Duration::from_secs(3600),
            &mut inbox,
        )
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(inbox.msgs.is_empty() && !inbox.closed);
}

/// No state directory: persisted state is not kept, but settings
/// files still are.
#[test]
fn settings_survive_a_missing_state_directory() {
    let dir = Path::new("/etc/strand-config");
    let s = storage_or(dir, Err(strand_core::PersistError::NoStateDir));
    assert!(s.persist.is_none());
    assert!(s.settings.is_some());
    assert_eq!(s.config_dir.as_deref(), Some(dir));
}

/// A settings file edited by hand with a bad value: the field keeps
/// its last good value and the overlay says so; the fix applies.
/// (Saved whole, as editors do: a truncate-then-write save can be
/// read empty in between, and an empty file is every default.)
#[test]
fn a_bad_settings_value_is_kept_and_shown() {
    fn save(path: &Path, text: &str) {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }
    let dir = temp_dir("settings-notice");
    std::fs::write(dir.join("prefs.toml"), "# mine\ngap = 6\n").unwrap();
    std::fs::write(
        dir.join("bar.strand"),
        "state prefs from \"prefs.toml\" { gap: int = 4 }\nbar Top { text join(\" \", \"gap\", prefs.gap) }\n",
    )
    .unwrap();
    let storage = Storage::in_dirs(dir.join("state"), &dir);
    let (compiler, to_logic, t, mut m) = spawn_live_with(&dir, None, None, storage);
    m.until("the file's value", |s| s.texts() == ["gap 6"]);
    save(&dir.join("prefs.toml"), "# mine\ngap = \"wide\"\n");
    m.until("the notice", |s| {
        s.texts()
            .iter()
            .any(|t| t.contains("gap") && t.contains("keeping its last good value"))
    });
    assert!(m.scene.texts().contains(&"gap 6".to_string()), "kept");
    // A settings-only overlay says so (no `[reset]` here).
    m.until("the settings header", |s| {
        s.texts()
            .iter()
            .any(|t| t.starts_with("strand: settings files"))
    });
    assert!(
        !m.scene.texts().iter().any(|t| t.contains("[reset]")),
        "{:?}",
        m.scene.texts()
    );
    save(&dir.join("prefs.toml"), "# mine\ngap = 8\n");
    m.until("the fix", |s| s.texts().contains(&"gap 8".to_string()));
    // Fixed: its notice goes, and the overlay with it.
    m.until("the notice gone", |s| {
        !s.texts()
            .iter()
            .any(|t| t.contains("keeping its last good value") || t.starts_with("strand:"))
    });
    // A syntax error, then the file parses again: same.
    save(&dir.join("prefs.toml"), "# mine\ngap = = 8\n");
    m.until("the syntax notice", |s| {
        s.texts()
            .iter()
            .any(|t| t.contains("keeping every last good value"))
    });
    save(&dir.join("prefs.toml"), "# mine\ngap = 9\n");
    m.until("parsed again", |s| {
        s.texts().contains(&"gap 9".to_string())
            && !s.texts().iter().any(|t| t.starts_with("strand:"))
    });
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// A mock `org.freedesktop.portal.Settings` for the theme test.
pub(crate) struct MockPortal {
    pub(crate) values: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl MockPortal {
    async fn read_one(
        &self,
        namespace: &str,
        key: &str,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedValue> {
        if namespace != "org.freedesktop.appearance" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        self.values
            .get(key)
            .map(|v| v.try_clone().unwrap())
            .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

pub(crate) struct Bus {
    child: std::process::Child,
    pub(crate) address: String,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private session bus (skipped without `dbus-daemon` unless
/// `STRAND_REQUIRE_DBUS` is set).
pub(crate) fn private_bus(dir: &Path) -> Option<Bus> {
    use std::io::BufRead;
    let spawned = std::process::Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .arg(format!("--address=unix:path={}/bus", dir.display()))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) if std::env::var_os("STRAND_REQUIRE_DBUS").is_none() => {
            eprintln!("skipping: dbus-daemon unavailable ({e})");
            return None;
        }
        Err(e) => panic!("dbus-daemon: {e}"),
    };
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    Some(Bus {
        child,
        address: line.trim().to_string(),
    })
}

fn png(major: [u8; 3], minor: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_fn(320, 180, |x, _| {
        image::Rgb(if x < 240 { major } else { minor })
    });
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn accent(s: &SceneMirror) -> Option<strand_scene::Color> {
    match s.tokens.lookup("accent") {
        Some(PropValue::Color(c)) => Some(c),
        _ => None,
    }
}

/// design.md's theme.strand in `strand run`, end to end: the portal's
/// boot read and changes (`system.dark`, `system.accent`), `strand
/// set theme.look …` over IPC, a wallpaper palette that follows the
/// file, its symlink and the link's target, and a restart that boots
/// straight into the last palette.
#[test]
fn the_theme_follows_the_portal_the_wallpaper_and_strand_set() {
    use strand_theme::{Options, Role, from_seed};
    use zbus::zvariant::{OwnedValue, Value as ZValue};
    let dir = temp_dir("theme");
    let config = dir.join("config");
    let walls = dir.join("walls");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&walls).unwrap();
    let Some(bus) = private_bus(&dir) else {
        return;
    };
    let theme = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../strand-compiler/tests/fixtures/theme.strand"
    ))
    .unwrap()
    .replace("~/.config/strand/prefs.toml", "prefs.toml");
    std::fs::write(config.join("theme.strand"), theme).unwrap();
    std::fs::write(
        config.join("bar.strand"),
        "bar Top {\n  bg: $surface\n  text \"themed\" { color: $fg }\n}\n",
    )
    .unwrap();
    // The wallpaper is a link into the wallpapers directory.
    let (blue, red, green) = (
        png([30, 90, 200], [240, 200, 40]),
        png([200, 40, 40], [20, 20, 20]),
        png([40, 170, 60], [230, 230, 230]),
    );
    std::fs::write(walls.join("blue.png"), &blue).unwrap();
    std::fs::write(walls.join("red.png"), &red).unwrap();
    let wall = dir.join("wall.png");
    std::os::unix::fs::symlink(walls.join("blue.png"), &wall).unwrap();
    std::fs::write(
        config.join("prefs.toml"),
        format!("# my prefs\nwallpaper = \"{}\"\n", wall.display()),
    )
    .unwrap();

    // The portal: dark, a red accent.
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let owned = |v: ZValue<'_>| -> OwnedValue { v.try_into().unwrap() };
    let values = std::collections::HashMap::from([
        ("color-scheme".to_string(), owned(ZValue::from(1u32))),
        (
            "accent-color".to_string(),
            owned(ZValue::from((0.88f64, 0.11f64, 0.14f64))),
        ),
        ("contrast".to_string(), owned(ZValue::from(0u32))),
        ("reduced-motion".to_string(), owned(ZValue::from(1u32))),
    ]);
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at("/org/freedesktop/portal/desktop", MockPortal { values })
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let state = dir.join("state");
    let storage = Storage::in_dirs(&state, &config);
    let socket = dir.join("s.sock");
    let (compiler, to_logic, t, mut m) = spawn_live_with(
        &config,
        Some(socket.clone()),
        Some(strand_services::Buses::private(&bus.address)),
        storage,
    );
    let portal_accent = strand_scene::Color::rgb(0.88, 0.11, 0.14);
    let seeded = |dark| {
        from_seed(
            // The portal's accent arrives as f64 sRGB.
            portal_accent,
            Options {
                dark,
                ..Options::default()
            },
        )
        .get(Role::Accent)
    };
    // look: auto → the portal's dark and accent.
    m.until("the portal's boot read", |s| {
        accent(s) == Some(seeded(true))
    });
    // The portal's `reduced-motion` reaches render (it snaps every
    // spring), and its change too.
    m.until_reduced(true);
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "SettingChanged",
            &(
                "org.freedesktop.appearance",
                "reduced-motion",
                ZValue::from(0u32),
            ),
        ))
        .unwrap();
    m.until_reduced(false);
    // The desktop switches to light.
    tokio
        .block_on(conn.emit_signal(
            None::<&str>,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "SettingChanged",
            &(
                "org.freedesktop.appearance",
                "color-scheme",
                ZValue::from(2u32),
            ),
        ))
        .unwrap();
    m.until("light", |s| accent(s) == Some(seeded(false)));

    let mut ipc_conn =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let mut ask = |look: &str| -> Json {
        ipc::request(
            &mut ipc_conn,
            &ipc::Request::Set {
                path: "theme.look".into(),
                value: look.into(),
            },
            Duration::from_secs(10),
        )
        .unwrap()
    };
    let mut set = |look: &str| {
        let ans = ask(look);
        assert_eq!(ans["ok"], json!(true), "{ans}");
        ans
    };
    // strand set theme.look mocha.
    set("mocha");
    m.until("mocha", |s| {
        accent(s) == strand_scene::Color::from_hex("#cba6f7")
    });
    // The wallpaper: quantised, then its palette.
    let image_accent = |bytes: &[u8]| {
        let seed = strand_theme::image::seed_from_bytes(bytes).unwrap();
        from_seed(
            seed,
            Options {
                dark: true,
                ..Options::default()
            },
        )
        .get(Role::Accent)
    };
    set("wallpaper");
    m.until("the blue wallpaper", |s| {
        accent(s) == Some(image_accent(&blue))
    });
    // swww-style: the link swapped to another file.
    let tmp = dir.join("wall.png.new");
    std::os::unix::fs::symlink(walls.join("red.png"), &tmp).unwrap();
    std::fs::rename(&tmp, &wall).unwrap();
    m.until("the link swapped to red", |s| {
        accent(s) == Some(image_accent(&red))
    });
    // The link's target replaced in its own directory.
    std::fs::write(walls.join("next.png"), &green).unwrap();
    std::fs::rename(walls.join("next.png"), walls.join("red.png")).unwrap();
    m.until("the target replaced by green", |s| {
        accent(s) == Some(image_accent(&green))
    });
    set("auto");
    m.until("auto again", |s| accent(s) == Some(seeded(false)));
    // Bad input is refused without closing anything.
    let ans = ask("sepia");
    assert_eq!(ans["ok"], json!(false), "{ans}");
    assert!(ans["error"].as_str().unwrap().contains("sepia"), "{ans}");
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    drop(conn);
    drop(bus);

    // A restart without the portal: the last values are in the boot
    // table itself (no default-colour frame while a portal answers).
    let storage = Storage::in_dirs(&state, &config);
    let (compiler, to_logic, t, mut m) = spawn_live_with(&config, None, None, storage);
    m.until("the first table", |s| accent(s).is_some());
    assert_eq!(accent(&m.scene), Some(seeded(false)));
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    drop(compiler);
    let _ = std::fs::remove_dir_all(dir);
}

/// An app installed together with its icon, in a theme directory
/// deeper than the icon watch sees and without touching the theme's
/// `icon-theme.cache`: the icon's name, looked up (and missed) before,
/// resolves after the `applications/` change, and the renderer is told
/// to drop its icons too.
#[test]
fn an_app_installed_with_its_icon_resolves_it() {
    let root = std::env::temp_dir().join(format!("strand-app-icon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let theme = format!("StrandAppIcon{}", std::process::id());
    let base = root.join("icons");
    std::fs::create_dir_all(base.join(&theme)).unwrap();
    std::fs::write(
        base.join(&theme).join("index.theme"),
        "[Icon Theme]\nName=T\nDirectories=256x256/apps\n\n[256x256/apps]\nSize=256\nType=Fixed\n",
    )
    .unwrap();
    // The defaults too: other tests resolve the system's icons.
    let mut bases = strand_icons::default_base_dirs();
    bases.push(base.clone());
    strand_icons::set_base_dirs(Some(bases));
    let name = "strand-new-app";
    assert_eq!(strand_icons::resolve(name, 256, 1, Some(&theme)), None);
    let icon = base.join(&theme).join("256x256/apps/strand-new-app.png");
    std::fs::create_dir_all(icon.parent().unwrap()).unwrap();
    std::fs::write(&icon, b"png").unwrap();
    assert_eq!(
        strand_icons::resolve(name, 256, 1, Some(&theme)),
        None,
        "the miss is remembered"
    );
    assert_eq!(cache_changed(CacheKind::Apps), CacheKind::Icons);
    assert_eq!(
        strand_icons::resolve(name, 256, 1, Some(&theme)),
        Some(icon)
    );
    strand_icons::set_base_dirs(None);
    let _ = std::fs::remove_dir_all(&root);
}

/// A notice from the main thread (`ToLogic::Notice`: the blur
/// fallback's reason) is sent at boot, before anyone watches: each
/// `strand watch` that subscribes later hears it at once, and the same
/// notice again is not repeated.
#[test]
fn main_thread_notices_reach_watchers_that_come_later() {
    let dir = temp_dir("notices");
    std::fs::write(dir.join("bar.strand"), "bar Top { text \"x\" }\n").unwrap();
    let socket = dir.join("ipc.sock");
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&dir, None, wtx).unwrap();
    assert!(boot.build.is_some() && boot.diagnostics.is_empty());
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: Some(socket.clone()),
        buses: None,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let said = "strand-Top asks for blur: the compositor does not blur";
    to_logic.send(ToLogic::Notice(said.into())).unwrap();
    to_logic.send(ToLogic::Notice(said.into())).unwrap();
    let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let mut m = Mirror::new(rx);
    m.until("the bar", |s| s.texts() == ["x"]);
    let watch = || {
        let mut events =
            std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
        let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
        assert_eq!(ok["ok"], true);
        events
    };
    let next = |events: &mut std::io::BufReader<std::os::unix::net::UnixStream>| {
        let mut line = String::new();
        std::io::BufRead::read_line(events, &mut line).unwrap();
        serde_json::from_str::<Json>(&line).unwrap()
    };
    let mut first = watch();
    let ev = next(&mut first);
    assert_eq!(ev["event"], "notices", "{ev}");
    assert_eq!(ev["notices"], json!([said]), "once, at once: {ev}");
    assert_eq!(ev["diagnostics"], json!([]), "{ev}");
    // A new notice reaches the watcher already there; a second watcher
    // hears both.
    to_logic.send(ToLogic::Notice("another".into())).unwrap();
    let ev = next(&mut first);
    assert_eq!(ev["notices"], json!(["another"]), "{ev}");
    let mut second = watch();
    let ev = next(&mut second);
    assert_eq!(ev["notices"], json!([said, "another"]), "{ev}");
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// (M4) A GPU status from the main thread (`ToLogic::GpuStatus`): an
/// unavailable device's reason is a `strand watch` notice once, however
/// often it is reported, and a device that comes up says nothing there.
#[test]
fn an_unavailable_gpu_is_a_notice_once() {
    let dir = temp_dir("gpu-status");
    std::fs::write(dir.join("bar.strand"), "bar Top { text \"x\" }\n").unwrap();
    let socket = dir.join("ipc.sock");
    let (wtx, wrx) = calloop::channel::channel();
    let (compiler, boot) = Worker::spawn(&dir, None, wtx).unwrap();
    let live = Live {
        worker: Some(wrx),
        jobs: Some(compiler.jobs()),
        socket: Some(socket.clone()),
        buses: None,
        icon_theme_switched: None,
    };
    let (to_logic, from_main) = calloop::channel::channel();
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    to_logic
        .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
        .unwrap();
    let gone = strand_scene::GpuStatus::Unavailable {
        reason: "no Vulkan adapter".into(),
    };
    to_logic.send(ToLogic::GpuStatus(gone.clone())).unwrap();
    to_logic.send(ToLogic::GpuStatus(gone)).unwrap();
    to_logic
        .send(ToLogic::GpuStatus(strand_scene::GpuStatus::Up(
            strand_scene::AdapterInfo {
                name: "a GPU".into(),
                driver: "a driver".into(),
                software: false,
            },
        )))
        .unwrap();
    let t = std::thread::spawn(move || logic(boot, Storage::none(), from_main, tx, live));
    let mut m = Mirror::new(rx);
    m.until("the bar", |s| s.texts() == ["x"]);
    let mut events =
        std::io::BufReader::new(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let ok = ipc::request(&mut events, &ipc::Request::Watch, Duration::from_secs(10)).unwrap();
    assert_eq!(ok["ok"], true);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut events, &mut line).unwrap();
    let ev: Json = serde_json::from_str(&line).unwrap();
    assert_eq!(ev["event"], "notices", "{ev}");
    assert_eq!(
        ev["notices"],
        json!(["GPU unavailable: no Vulkan adapter; shaders draw nothing (CPU fallback)"]),
        "{ev}"
    );
    to_logic.send(ToLogic::Shutdown).unwrap();
    assert_eq!(t.join().unwrap(), Ok(()));
    let _ = std::fs::remove_dir_all(&dir);
}
