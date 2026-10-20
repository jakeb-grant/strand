//! The live sandbox (docs/eval.md, "Live sandbox"; docs/decisions.md
//! eval-live): a session is the scenario it records (the recording runs
//! to the session's own trace digest and passes `check`), and the window
//! shows the stage and takes keys, the pointer and stdin's commands on a
//! headless sway of the test's own (never the desktop's).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value as Json;
use strand_eval::harness::{self, RunOptions};
use strand_eval::live::session::{self, KnobKind, Options, Session};
use strand_eval::scenario::{Scenario, Screen};

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("live-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// eval/fixtures/live.strand, copied where the test may edit it.
fn config(dir: &Path) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/live.strand");
    let to = dir.join("shell.strand");
    std::fs::copy(src, &to).unwrap();
    to
}

/// The recording runs to the session's digest and passes `check`.
fn holds(path: &Path, digest: &str) {
    let sc = Scenario::load(path).unwrap_or_else(|e| panic!("{e}"));
    let run = harness::run(&sc, &session::run_options()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        run.trace.digest, digest,
        "the recording does not run to the session's trace:\n{}",
        sc.source
    );
    let checked =
        strand_eval::check_scenario(&sc, &RunOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let failed: Vec<String> = checked
        .report
        .results
        .iter()
        .flat_map(|r| r.findings.iter())
        .filter(|f| f.sev == strand_eval::checks::Sev::Fail)
        .map(|f| format!("{} k{:?} {}", f.check, f.k, f.msg))
        .collect();
    assert!(checked.report.ok(), "{failed:#?}\n{}", sc.source);
}

/// Ticks until the clock's span comes to rest (the log's last step is a
/// `rest`) and nothing is queued.
fn to_rest(s: &mut Session) {
    for _ in 0..900 {
        if s.queued() == 0 && s.log().last().is_some_and(|l| l.starts_with("rest")) {
            return;
        }
        s.step(1).unwrap_or_else(|e| panic!("{e}"));
    }
    panic!("no rest: {:?}", s.log());
}

fn knob(s: &Session, f: impl Fn(&KnobKind) -> bool) -> usize {
    s.knobs()
        .iter()
        .position(|k| f(&k.kind))
        .unwrap_or_else(|| panic!("{:?}", s.knobs()))
}

#[test]
fn a_session_is_the_scenario_it_records() {
    let dir = scratch("session");
    let cfg = config(&dir);
    let mut s = Session::open(Options {
        path: cfg.clone(),
        screen: Screen {
            width: 400,
            height: 200,
            scale: 1.0,
        },
        record_to: Some(dir.join("rec.scn")),
        paused: true,
        ..Options::default()
    })
    .unwrap_or_else(|e| panic!("{e}"));
    // The knobs are what `new --explore` exercises: the states, the
    // popup's anchor, the click.
    let labels: Vec<&str> = s.knobs().iter().map(|k| k.label.as_str()).collect();
    for want in [
        "toggle wide",
        "cycle look",
        "hover shell.strand:16:5",
        "click shell.strand:12:5",
    ] {
        assert!(labels.contains(&want), "{labels:?}");
    }

    // A state flipped, run a tick at a time, then at a quarter speed
    // until it rests: the clock's span is written as a `rest`.
    s.act("toggle wide").unwrap();
    s.step(3).unwrap();
    s.set_speed(0.25).unwrap();
    s.paused = false;
    for _ in 0..8 {
        s.play(Duration::from_millis(70)).unwrap();
    }
    assert_eq!(
        s.log(),
        ["rest", "toggle wide"],
        "the boot settles at once; a span still running is not written"
    );
    to_rest(&mut s);
    assert_eq!(s.log(), ["rest", "toggle wide", "rest"]);

    // The enum's knob sets its next value; the hover knob opens the popup.
    let cycle = knob(&s, |k| matches!(k, KnobKind::Cycle(..)));
    s.knob(cycle).unwrap();
    to_rest(&mut s);
    assert_eq!(s.driver().value_text("look").as_deref(), Some("loud"));
    let hover = knob(&s, |k| matches!(k, KnobKind::Hover(..)));
    s.knob(hover).unwrap();
    to_rest(&mut s);
    assert_eq!(s.driver().surfaces().shown.len(), 2, "{:?}", s.log());
    assert!(s.log().iter().any(|l| l.starts_with("pointer ")));

    // The ring: frames back, then the newest again.
    let head = s.shown().unwrap().k;
    s.back(5);
    assert!(s.shown().unwrap().k < head);
    s.head();
    assert_eq!(s.shown().unwrap().k, head);

    // The pointer leaves the stage: `leave`, and the popup closes.
    s.aim(None);
    s.step(1).unwrap();
    assert!(s.log().iter().any(|l| l == "leave"), "{:?}", s.log());
    to_rest(&mut s);
    assert_eq!(s.driver().surfaces().shown.len(), 1);

    // An edit: the steps run on the new config at once up to the last
    // segment, which plays again.
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, text.replace("#3060ff", "#ff00ff")).unwrap();
    assert!(s.changed());
    s.reload().unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.reloads, 1);
    assert!(s.queued() > 0, "the last segment plays again");
    assert!(s.log().iter().any(|l| l.starts_with("pointer ")));
    to_rest(&mut s);

    // The recording is the session.
    let rec = s.record(None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rec.path, dir.join("rec.scn"));
    let written = std::fs::read_to_string(&rec.path).unwrap();
    assert!(
        written.contains("#ff00ff"),
        "the config as it ran:\n{written}"
    );
    holds(&rec.path, &rec.digest);

    // The recording opened again plays its steps and writes itself
    // again, to the same trace.
    let mut back = Session::open(Options {
        path: rec.path.clone(),
        paused: true,
        ..Options::default()
    })
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(back.busy());
    for _ in 0..2000 {
        if !back.busy() {
            break;
        }
        back.step(1).unwrap_or_else(|e| panic!("{e}"));
    }
    assert!(!back.busy());
    let again = back.record(Some(dir.join("again.scn"))).unwrap();
    assert_eq!(again.digest, rec.digest);
    assert_eq!(std::fs::read_to_string(&again.path).unwrap(), written);

    // A refused step changes nothing.
    let before = s.log().to_vec();
    assert!(s.act("toggle nothing_here").is_err());
    assert_eq!(s.log(), before.as_slice());
    let again = s.record(None).unwrap();
    assert_eq!(again.digest, rec.digest);
}

#[test]
fn a_failing_rule_is_marked_on_its_frame_and_a_broken_edit_keeps_the_session() {
    let dir = scratch("marks");
    let cfg = dir.join("spill.strand");
    // A cut text in a box narrower than its ellipsis, which is drawn
    // past the box: `ink`'s spill rule fails on every frame, on the
    // text's box. (A word wider than its box with no ellipsis overflows
    // its end as CSS draws it, decisions.md launcher-rules.)
    let good = "bar Top {\n  height: 40\n  row { width: 400; height: 40; align: center; pad: 0, 8\n    \
                text \"WWWWWWWWWW\" { id: t; width: 4; shrink: 0; ellipsis: end; color: #000000 }\n  }\n}\n";
    std::fs::write(&cfg, good).unwrap();
    let mut s = Session::open(Options {
        path: cfg.clone(),
        screen: Screen {
            width: 400,
            height: 200,
            scale: 1.0,
        },
        paused: true,
        ..Options::default()
    })
    .unwrap_or_else(|e| panic!("{e}"));
    s.analyse();
    let ink = s.verdicts().into_iter().find(|(c, _)| c == "ink").unwrap();
    assert!(
        matches!(ink.1, session::Verdict::Fail(n) if n > 0),
        "{ink:?}"
    );
    let marks = s.marks();
    let m = marks
        .iter()
        .find(|m| m.check == "ink")
        .unwrap_or_else(|| panic!("{marks:?}"));
    assert!(m.msg.contains("spill"), "{}", m.msg);
    assert_eq!(m.rect[2], 4.0, "the text's box: {:?}", m.rect);
    assert!(s.next_finding());
    let ks: Vec<u32> = s.ring().iter().map(|f| f.k).collect();
    assert!(
        ks.windows(2).all(|w| w[0] < w[1]),
        "one shot per tick: {ks:?}"
    );
    assert!(!s.shown().unwrap().surfaces.is_empty(), "the bar is shown");
    // The panel draws it: the strip's verdict and the box outlined.
    let mut live = strand_eval::live::Live::new(s);
    let png = dir.join("marks.png");
    let r = live.command(&format!("shot {} 960x400", png.display()));
    assert_eq!(r["ok"], true, "{r}");
    assert!(png.is_file());
    let mut s = live.session;

    // An edit that does not compile is refused; the session runs on.
    let log = s.log().to_vec();
    std::fs::write(&cfg, "bar Top { height: }\n").unwrap();
    assert!(s.changed());
    assert!(s.reload().is_err());
    s.forget_edit();
    assert!(!s.changed());
    assert_eq!(s.log(), log.as_slice());
    s.step(2).unwrap();
}

#[test]
fn the_last_segment_is_what_the_owner_did_last() {
    let log: Vec<String> = [
        "toggle wide",
        "rest",
        "pointer 1,1 on bar[0]",
        "frames 2",
        "pointer 2,1 on bar[0]",
        "rest",
        "frames 30",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(session::last_segment(&log), 2);
    assert_eq!(session::last_segment(&log[..2]), 0);
    assert_eq!(session::last_segment(&["frames 3".to_string()]), 1);
}

/// (integration-r4) A `swipe` typed in the sandbox plays a tick at a time,
/// as a glide does (one axis frame each, the last lifting the fingers),
/// so it can be stepped through and is written as itself; cut short by
/// the recording it runs to its end first. The recording runs to the
/// session's digest and passes `check`.
#[test]
fn a_swipe_plays_tick_by_tick_and_records_as_itself() {
    let dir = scratch("swipe");
    let cfg = dir.join("shell.strand");
    std::fs::write(
        &cfg,
        "let rows = [\"a1\", \"a2\", \"a3\", \"a4\", \"a5\", \"a6\", \"a7\", \"a8\"]\n\
         panel Picker {\n  anchor: top_left; open: true\n  \
         col { width: 160; pad: 8; bg: #1e1e2e; color: #cdd6f4\n    \
         list { id: picks; height: 90; gap: 4\n      for r in rows key r {\n        \
         row { height: 30; pad: 0, 10; bg: #313244; align: center\n          text r\n        }\n      }\n    }\n  }\n}\n",
    )
    .unwrap();
    let mut s = Session::open(Options {
        path: cfg,
        screen: Screen {
            width: 300,
            height: 200,
            scale: 1.0,
        },
        record_to: Some(dir.join("rec.scn")),
        paused: true,
        ..Options::default()
    })
    .unwrap_or_else(|e| panic!("{e}"));
    s.act("pointer 40,40").unwrap();
    s.act("swipe 60 over 100ms").unwrap();
    // 100 ms at 60 Hz is six axis frames: two ticks in, it is under way.
    s.step(2).unwrap();
    assert_eq!(s.queued(), 1, "the swipe is still under way: {:?}", s.log());
    assert_eq!(
        s.log().last().map(String::as_str),
        Some("swipe 60 over 100ms"),
        "{:?}",
        s.log()
    );
    let rec = s.record(None).unwrap_or_else(|e| panic!("{e}"));
    let text = std::fs::read_to_string(&rec.path).unwrap();
    assert!(text.contains("\nswipe 60 over 100ms\n"), "{text}");
    holds(&rec.path, &rec.digest);
}

// --- the window, on a headless sway of the test's own

/// A headless sway with its own runtime directory, killed on drop.
struct Sway {
    child: Child,
    dir: PathBuf,
    display: String,
    ipc: PathBuf,
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Sway {
    fn start(dir: &Path) -> Option<Sway> {
        if Command::new("sway").arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "sway is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("skipping: sway is not installed");
            return None;
        }
        let run = dir.join("run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let cfg = run.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1920x1080@60Hz position 0 0\n\
             default_border none\n",
        )
        .unwrap();
        let log = std::fs::File::create(run.join("sway.log")).unwrap();
        let mut cmd = Command::new("sway");
        // SAFETY: one async-signal-safe syscall between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(std::io::Error::from)
            });
        }
        let child = cmd
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &run)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("SWAYSOCK")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut sway = Sway {
            child,
            dir: run.clone(),
            display: String::new(),
            ipc: PathBuf::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: Vec<String> = std::fs::read_dir(&run)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = names
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc) {
                sway.display = d.clone();
                sway.ipc = run.join(i);
                if sway.msg(&["-t", "get_version"]).is_some() {
                    return Some(sway);
                }
            }
            assert!(
                Instant::now() < deadline,
                "sway did not start:\n{}",
                std::fs::read_to_string(run.join("sway.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn socket(&self) -> PathBuf {
        self.dir.join(&self.display)
    }

    /// The output as RGBA, written to `path` too.
    fn grim(&self, path: &Path) -> (u32, u32, Vec<u8>) {
        let ok = Command::new("grim")
            .arg("-o")
            .arg("HEADLESS-1")
            .arg(path)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .status()
            .unwrap()
            .success();
        assert!(ok, "grim failed");
        let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf
                .chunks_exact(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            c => panic!("grim wrote {c:?}"),
        };
        (info.width, info.height, rgba)
    }

    /// The window's content rectangle on the output.
    fn window(&self) -> Option<[i64; 4]> {
        let tree: Json = serde_json::from_str(&self.msg(&["-t", "get_tree", "-r"])?).ok()?;
        fn find(n: &Json) -> Option<[i64; 4]> {
            if n["app_id"] == "strand-eval-live" {
                let r = &n["rect"];
                let w = &n["window_rect"];
                return Some([
                    r["x"].as_i64()? + w["x"].as_i64()?,
                    r["y"].as_i64()? + w["y"].as_i64()?,
                    w["width"].as_i64()?,
                    w["height"].as_i64()?,
                ]);
            }
            for k in ["nodes", "floating_nodes"] {
                for c in n[k].as_array()? {
                    if let Some(r) = find(c) {
                        return Some(r);
                    }
                }
            }
            None
        }
        find(&tree)
    }
}

mod virt {
    //! A virtual pointer and keyboard on the test sway's seat (its
    //! headless seat has neither).
    use std::io::Write as _;
    use std::os::fd::AsFd;

    use super::UnixStream;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    pub struct Client;
    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(Client: ignore ZwlrVirtualPointerManagerV1);
    delegate_noop!(Client: ignore ZwlrVirtualPointerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Client: ignore wl_seat::WlSeat);

    /// The keys the keymap has: (keysym name, evdev code).
    const KEYS: [(&str, u32); 8] = [
        ("space", 57),
        ("period", 52),
        ("comma", 51),
        ("bracketleft", 26),
        ("w", 17),
        ("m", 50),
        ("End", 107),
        ("q", 16),
    ];

    pub struct Input {
        _conn: Connection,
        queue: EventQueue<Client>,
        pointer: ZwlrVirtualPointerV1,
        keyboard: ZwpVirtualKeyboardV1,
        time: u32,
    }

    impl Input {
        pub fn new(socket: &std::path::Path, dir: &std::path::Path) -> Input {
            let conn = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let qh = queue.handle();
            let pm: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
            let pointer = pm.create_virtual_pointer(None, &qh, ());
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
            let km: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
            let keyboard = km.create_virtual_keyboard(&seat, &qh, ());
            let mut codes = String::new();
            let mut symbols = String::new();
            for (name, k) in KEYS {
                let k = k + 8;
                codes.push_str(&format!("<K{k}> = {k}; "));
                symbols.push_str(&format!("key <K{k}> {{ [ {name} ] }}; "));
            }
            let keymap = format!(
                "xkb_keymap {{\n\
                 xkb_keycodes \"live\" {{ minimum = 8; maximum = 255; {codes}}};\n\
                 xkb_types \"live\" {{ type \"ONE_LEVEL\" {{ modifiers = none; level_name[Level1] = \"Any\"; }}; }};\n\
                 xkb_compatibility \"live\" {{ }};\n\
                 xkb_symbols \"live\" {{ {symbols}}};\n\
                 }};\n"
            );
            let path = dir.join("keymap");
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(keymap.as_bytes()).unwrap();
            file.write_all(&[0]).unwrap();
            file.flush().unwrap();
            let file = std::fs::File::open(&path).unwrap();
            keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
            queue.roundtrip(&mut Client).unwrap();
            Input {
                _conn: conn,
                queue,
                pointer,
                keyboard,
                time: 0,
            }
        }

        /// The pointer to output point (`x`, `y`).
        pub fn motion(&mut self, x: i64, y: i64) {
            self.time += 10;
            self.pointer
                .motion_absolute(self.time, x as u32, y as u32, 1920, 1080);
            self.pointer.frame();
            self.queue.roundtrip(&mut Client).unwrap();
        }

        pub fn click(&mut self, x: i64, y: i64) {
            self.motion(x, y);
            for state in [
                wl_pointer::ButtonState::Pressed,
                wl_pointer::ButtonState::Released,
            ] {
                self.time += 10;
                self.pointer.button(self.time, 0x110, state);
                self.pointer.frame();
            }
            self.queue.roundtrip(&mut Client).unwrap();
        }

        /// A key of [`KEYS`] pressed and released.
        pub fn key(&mut self, name: &str) {
            let k = KEYS.iter().find(|(n, _)| *n == name).unwrap().1;
            for state in [1, 0] {
                self.time += 10;
                self.keyboard.key(self.time, k, state);
            }
            self.queue.roundtrip(&mut Client).unwrap();
        }
    }
}

/// `strand-eval live` on the sway: its stdin, and its answers.
struct App {
    child: Child,
    stdin: ChildStdin,
    out: mpsc::Receiver<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl App {
    fn send(&mut self, cmd: &str) -> Json {
        writeln!(self.stdin, "{cmd}").unwrap();
        self.stdin.flush().unwrap();
        let line = self
            .out
            .recv_timeout(Duration::from_secs(120))
            .unwrap_or_else(|_| panic!("no answer to `{cmd}`"));
        let v: Json = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(v["ok"], true, "`{cmd}`: {v}");
        v
    }

    /// Polls `status` until `f` holds.
    fn until(&mut self, what: &str, f: impl Fn(&Json) -> bool) -> Json {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let s = self.send("status");
            if f(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "{what}: {s}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Where screen point `p` (logical) is on the output, through the
/// window's stage.
fn on_output(win: [i64; 4], status: &Json, p: [f64; 2]) -> (i64, i64) {
    let st = &status["stage"];
    let z = st["zoom"].as_f64().unwrap();
    let o = [
        st["origin"][0].as_f64().unwrap(),
        st["origin"][1].as_f64().unwrap(),
    ];
    let top = st["top"].as_f64().unwrap();
    (
        win[0] + ((p[0] - o[0]) * z).round() as i64,
        win[1] + (top + (p[1] - o[1]) * z).round() as i64,
    )
}

fn px(img: &(u32, u32, Vec<u8>), x: i64, y: i64) -> [u8; 3] {
    let i = ((y as u32 * img.0 + x as u32) * 4) as usize;
    [img.2[i], img.2[i + 1], img.2[i + 2]]
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 8)
}

#[test]
fn the_window_runs_on_a_headless_sway() {
    let dir = scratch("window");
    let Some(sway) = Sway::start(&dir) else {
        return;
    };
    let cfg = config(&dir);
    let rec = dir.join("win.scn");
    let mut child = Command::new(env!("CARGO_BIN_EXE_strand-eval"));
    // SAFETY: as for sway.
    unsafe {
        child.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(std::io::Error::from)
        });
    }
    let mut child = child
        .arg("live")
        .arg(&cfg)
        .args(["--screen", "400x200", "--paused", "--speed", "0.5"])
        .arg("--record-to")
        .arg(&rec)
        .env("WAYLAND_DISPLAY", &sway.display)
        .env("XDG_RUNTIME_DIR", &sway.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(dir.join("live.log")).unwrap())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, out) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut app = App { child, stdin, out };
    let ready = app
        .out
        .recv_timeout(Duration::from_secs(120))
        .unwrap_or_else(|_| {
            panic!(
                "no ready line: {}",
                std::fs::read_to_string(dir.join("live.log")).unwrap_or_default()
            )
        });
    assert!(ready.contains("\"ready\":true"), "{ready}");
    let s = app.until("the window is drawn", |s| s["window"][0].as_u64() > Some(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    let win = loop {
        if let Some(w) = sway.window() {
            break w;
        }
        assert!(
            Instant::now() < deadline,
            "the window is not in sway's tree"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(s["paused"], true);
    let mut input = virt::Input::new(&sway.socket(), &dir);
    app.until("the window has the keyboard", |s| s["focused"] == true);
    // Its keymap arrives with the focus; a key pressed before is lost.
    std::thread::sleep(Duration::from_millis(300));

    // '.' steps one tick (the window has the keyboard).
    let k0 = s["k"].as_u64().unwrap();
    input.key("period");
    app.until("'.' steps a tick", |s| s["k"].as_u64() == Some(k0 + 1));

    // The pointer onto the popup's anchor (#tip, its centre at 144,20 on
    // the screen): a step, and the popup opens on the virtual clock.
    let s = app.send("status");
    let (x, y) = on_output(win, &s, [144.0, 20.0]);
    input.motion(x, y);
    std::thread::sleep(Duration::from_millis(200));
    app.send("step 40");
    let s = app.until("the popup opens", |s| s["surfaces"] == 2);
    let log = app.send("scenario");
    let text = log["text"].as_str().unwrap().to_string();
    assert!(text.contains("pointer 144,20 on bar[0]"), "{text}");
    // The stage shows it: the bar's boxes and the card under the anchor.
    let shot = sway.grim(&dir.join("open.png"));
    let s2 = app.send("status");
    assert_eq!(s2["stage"], s["stage"], "the stage did not move");
    let (cx, cy) = on_output(win, &s2, [144.0, 66.0]);
    assert!(
        near(px(&shot, cx, cy), [0xff, 0xcc, 0x00]),
        "card: {:?}",
        px(&shot, cx, cy)
    );
    let (gx, gy) = on_output(win, &s2, [38.0, 20.0]);
    assert!(
        near(px(&shot, gx, gy), [0xff, 0x30, 0x30]) || near(px(&shot, gx, gy), [0x30, 0xc0, 0x60]),
        "grow: {:?}",
        px(&shot, gx, gy)
    );
    // The strip is drawn above the stage.
    assert!(near(px(&shot, win[0] + 2, win[1] + 2), [0x1c, 0x1d, 0x24]));

    // The pointer onto #grow (the bar's first box): the popup's grace
    // ends and it closes. Its anchor is to move next, and a popup open
    // while its anchor moves follows it only at the shell's next wake, in
    // one jump `teleport` fails (docs/decisions.md integration-r4, the
    // product's popup-boot-wake trade-off, left to the owner): this test
    // is the window's, so the popup is closed first.
    let (x, y) = on_output(win, &s2, [38.0, 20.0]);
    input.motion(x, y);
    std::thread::sleep(Duration::from_millis(200));
    app.send("step 60");
    app.until("the popup closes", |s| s["surfaces"] == 1);
    // A click on #grow flips `wide`; space plays at half speed, '['
    // slows it to a quarter.
    input.click(x, y);
    std::thread::sleep(Duration::from_millis(200));
    input.key("bracketleft");
    input.key("space");
    app.until("it plays at a quarter speed", |s| {
        s["paused"] == false && s["speed"] == 0.25
    });
    std::thread::sleep(Duration::from_millis(800));
    input.key("space");
    let s = app.until("it pauses", |s| s["paused"] == true);
    assert!(s["k"].as_u64().unwrap() > k0 + 41, "{s}");
    app.send("step 60");
    let text = app.send("scenario")["text"].as_str().unwrap().to_string();
    assert!(text.contains("press left\nrelease left"), "{text}");
    assert!(!text.contains("speed"));
    sway.grim(&dir.join("wide.png"));

    // ',' steps back through the ring and End comes back.
    input.key("comma");
    input.key("comma");
    let back = app.until("back two frames", |s| s["head"] == false);
    assert!(back["view"].as_u64() < back["k"].as_u64(), "{back}");
    sway.grim(&dir.join("back.png"));
    input.key("End");
    app.until("the newest frame", |s| s["head"] == true);

    // An edit of the config: seen within a poll, the last segment plays
    // again on it.
    let t = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, t.replace("#3060ff", "#ff00ff")).unwrap();
    app.until("the edit is reloaded", |s| s["reloads"] == 1);
    app.send("step 120");
    let s = app.send("status");
    assert_eq!(s["queued"], 0, "{s}");
    let shot = sway.grim(&dir.join("reloaded.png"));
    let (nx, ny) = on_output(win, &s, [195.0, 20.0]);
    assert!(
        near(px(&shot, nx, ny), [0xff, 0x00, 0xff]),
        "next: {:?} at {nx},{ny}",
        px(&shot, nx, ny)
    );

    // 'w' records; `record` again returns its digest.
    input.key("w");
    app.until("'w' recorded", |s| {
        s["msg"].as_str().is_some_and(|m| m.starts_with("recorded"))
    });
    assert!(rec.is_file());
    let r = app.send("record");
    assert_eq!(r["path"], rec.display().to_string());
    let digest = r["digest"].as_str().unwrap().to_string();
    let checks = app.send("checks");
    assert!(checks["report"]["results"].is_array(), "{checks}");
    sway.grim(&dir.join("checks.png"));
    input.key("q");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(st) = app.child.try_wait().unwrap() {
            assert!(st.success(), "{st}");
            break;
        }
        assert!(Instant::now() < deadline, "'q' did not quit");
        std::thread::sleep(Duration::from_millis(50));
    }
    holds(&rec, &digest);
}
