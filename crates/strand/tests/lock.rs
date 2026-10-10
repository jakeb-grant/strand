//! The lock's fault matrix, end to end: the `strand` binary (built with
//! the `faults` feature) on headless sway 1.9 with real PAM, **inside
//! the lock VM only** (scripts/container/lockvm.sh, scenario
//! `scripts/lockvm/scenarios/strand_lock.sh`). Every test that takes a
//! session lock skips unless it runs in the VM guest (`STRAND_LOCK_VM=1`,
//! hostname `lockvm`, PID 1 the VM's init); none ever runs against
//! another compositor.
//!
//! design.md, "Lock screen": "The lock is exempt from reload and fails
//! closed: if anything faults, a built-in password field appears." For
//! each fault (`STRAND_FAULT`, `run/lock.rs`'s `faults` module and
//! strand-auth's helper points) the test asserts, from grim's pixels,
//! that the session stays locked (the desktop, an orange bar drawn by a
//! client of the test's own, never shows), that the built-in password
//! field appears (its background `#11111b` and field `#1e1e2e`), that a
//! wrong password is refused (the field turns `#3b202c`) and leaves the
//! session locked, and that the test user's password (`strand-test`,
//! baked into the VM) unlocks it.
//!
//! Faults: the logic thread panics, or stops (the watchdog); the text
//! worker dies; the PAM helper crashes, hangs, answers garbage or is
//! missing; a runtime fault freezes the lock's component; the lock never
//! draws a first frame; a session locked with no `lock` compiled;
//! SIGTERM while locked, and then the content's output unplugged (the
//! lock's node kept), or the lock unmounted and then the content's
//! output unplugged; strand killed (SIGKILL) or aborted (SIGABRT, what
//! an allocation failure does) while locked and
//! started again; the compositor ends a lock it granted (`finished` after
//! `locked`, played by a Wayland proxy, tests/lock/proxy.rs: sway 1.9
//! never sends it); the compositor refusing the lock (another locker
//! holds it: not a fault, nothing shows, the run goes on); an output
//! plugged in while locked; a config write of `false` while locked
//! (only a password unlocks).
//!
//! No assertion depends on how long anything took: each wait is for
//! what grim shows, bounded only to fail instead of hanging.

#[path = "lock/proxy.rs"]
mod proxy;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use strand_auth::{Client, Password, UnlockToken, Verdict};
use strand_scene::{
    Color, Damage, Insets, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, SurfaceChange,
    SurfaceId, SurfaceSpec,
};
use strand_surface::{Config, SurfaceHost, SurfaceManager};

/// The test user's password (scripts/container/Dockerfile.lockvm).
const PASSWORD: &str = "strand-test";
const WRONG: &str = "wrong-password";

/// The desktop's bar (not red: sway paints an abandoned lock red).
const DESKTOP: [u8; 3] = [0xff, 0x80, 0x00];
/// The config's lock (`bg`), and after a refused password.
const LOCK_BG: [u8; 3] = [0x20, 0x50, 0xd0];
const LOCK_REFUSED: [u8; 3] = [0xa0, 0x20, 0x40];
/// The config's lock while its field holds text.
const LOCK_TYPED: [u8; 3] = [0x20, 0xa0, 0x50];
/// strand-render's fallback: background, field, refused field.
const FALLBACK_BG: [u8; 3] = [0x11, 0x11, 0x1b];
const FIELD: [u8; 3] = [0x1e, 0x1e, 0x2e];
const FIELD_REFUSED: [u8; 3] = [0x3b, 0x20, 0x2c];
/// Another locker's colour.
const OTHER: [u8; 3] = [0x00, 0xff, 0x00];

/// How long anything shown is waited for before the test fails.
const WAIT: Duration = Duration::from_secs(30);

/// The config: a lock opened by `lock.locked`, its password checked by
/// `auth`, its colour showing typed text and a refusal, and a text that
/// faults once `lock.zero` is 0.
const CONFIG: &str = r#"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: #2050d0
  when auth.failed { bg: #a02040 }
  when secret != "" { bg: #20a050 }
  input { type: password; text: <-> secret; focus: true
    on key(k) { if k.name == "Return" { auth.submit(secret); secret = "" } }
  }
  text pct(10 / zero)
}
"#;

// ---- the VM guard -----------------------------------------------------------

/// In the lock VM's guest, and asked to run there.
fn in_lock_vm(test: &str) -> bool {
    let asked = std::env::var_os("STRAND_LOCK_VM").is_some_and(|v| v == "1");
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let init = std::fs::read("/proc/1/cmdline").unwrap_or_default();
    let vm = host.trim() == "lockvm" && init.windows(11).any(|w| w == b"lockvm-init");
    if !(asked && vm) {
        eprintln!(
            "skipping {test}: the lock's fault matrix runs only inside the lock VM \
             (scripts/container/lockvm.sh), never against another compositor"
        );
        return false;
    }
    true
}

// ---- sway -------------------------------------------------------------------

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Sway {
    _child: Proc,
    dir: PathBuf,
    display: String,
    ipc: PathBuf,
    outputs: usize,
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Sway {
    fn start(tag: &str) -> Sway {
        for tool in ["sway", "swaymsg", "grim"] {
            assert!(
                Command::new(tool).arg("--version").output().is_ok(),
                "{tool} is not installed in the lock VM"
            );
        }
        let dir = std::env::temp_dir().join(format!("sl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n",
        )
        .unwrap();
        let log = std::fs::File::create(dir.join("sway.log")).unwrap();
        let child = Command::new("sway")
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &dir)
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
            _child: Proc(child),
            dir: dir.clone(),
            display: String::new(),
            ipc: PathBuf::new(),
            outputs: 1,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let names: Vec<String> = std::fs::read_dir(&dir)
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
                sway.ipc = dir.join(i);
                if sway.msg(&["-t", "get_version"]).is_some() {
                    return sway;
                }
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join(&self.display)
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

    /// A new 1280×720 output to the right of the others: its name.
    fn plug(&mut self) -> String {
        self.msg(&["create_output"]).expect("create_output");
        self.outputs += 1;
        let name = format!("HEADLESS-{}", self.outputs);
        let x = (1280 * (self.outputs - 1)).to_string();
        self.msg(&[
            "output",
            &name,
            "resolution",
            "1280x720",
            "position",
            &x,
            "0",
        ])
        .expect("place the new output");
        name
    }

    /// Unplugs output `name` (a headless output's `unplug`).
    fn unplug(&self, name: &str) {
        self.msg(&["output", name, "unplug"])
            .unwrap_or_else(|| panic!("unplug {name}"));
    }

    fn connect(&self) -> wayland_client::Connection {
        wayland_client::Connection::from_socket(UnixStream::connect(self.socket()).unwrap())
            .unwrap()
    }

    /// `output` as grim captures it.
    fn shot(&self, output: &str) -> Shot {
        let path = self.dir.join(format!("{output}.ppm"));
        let _ = std::fs::remove_file(&path);
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-o", output])
            .arg(&path)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim {output}");
        let ppm = std::fs::read(&path).unwrap();
        // Binary PPM: "P6\n<w> <h>\n255\n" then RGB.
        let mut nl = ppm.iter().enumerate().filter(|(_, b)| **b == b'\n');
        let (a, b, c) = (
            nl.next().unwrap().0,
            nl.next().unwrap().0,
            nl.next().unwrap().0,
        );
        let dims = std::str::from_utf8(&ppm[a + 1..b]).unwrap();
        let mut it = dims.split_whitespace().map(|v| v.parse::<usize>().unwrap());
        let (w, h) = (it.next().unwrap(), it.next().unwrap());
        Shot {
            w,
            h,
            rgb: ppm[c + 1..].to_vec(),
        }
    }
}

/// One output's pixels.
struct Shot {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

fn near(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3)
}

impl Shot {
    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Where the desktop's bar is.
    fn top(&self) -> [u8; 3] {
        self.px(self.w - 10, 10)
    }

    /// The bottom-right corner: the lock's background, or the fallback's.
    fn corner(&self) -> [u8; 3] {
        self.px(self.w - 10, self.h - 10)
    }

    /// Inside the fallback's field, above its dots.
    fn field(&self) -> [u8; 3] {
        self.px(self.w / 2, self.h / 2 - 15)
    }

    fn desktop(&self) -> bool {
        near(self.top(), DESKTOP)
    }

    fn fallback(&self) -> bool {
        near(self.corner(), FALLBACK_BG)
            && (near(self.field(), FIELD) || near(self.field(), FIELD_REFUSED))
    }

    fn describe(&self) -> String {
        format!(
            "top {:02x?}, corner {:02x?}, field {:02x?}",
            self.top(),
            self.corner(),
            self.field()
        )
    }
}

// ---- the desktop: a bar on every output, drawn by a client of the test's own

#[derive(Default)]
struct Fill {
    rgb: [u8; 3],
    painted: HashSet<SurfaceId>,
    sizes: HashMap<SurfaceId, strand_scene::Size>,
}

impl Painter for Fill {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        if self.sizes.get(&surface) == Some(&target.size) && self.painted.contains(&surface) {
            return Damage::new();
        }
        self.sizes.insert(surface, target.size);
        self.painted.insert(surface);
        let [r, g, b] = self.rgb;
        for px in target.pixels.chunks_exact_mut(4) {
            px.copy_from_slice(&[b, g, r, 0xff]);
        }
        Damage::full(target.size)
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        !self.painted.contains(&surface)
    }
}

impl SurfaceHost for Fill {
    fn surface_detached(&mut self, surface: SurfaceId) {
        self.painted.remove(&surface);
        self.sizes.remove(&surface);
    }

    fn surface_configured(
        &mut self,
        surface: SurfaceId,
        _: strand_scene::Size,
        _: strand_scene::Scale,
    ) {
        self.painted.remove(&surface);
    }
}

/// The desktop's client, pumped on a thread of its own until dropped.
struct Desktop {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Desktop {
    fn start(sway: &Sway) -> Desktop {
        let socket = sway.socket();
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let conn =
                wayland_client::Connection::from_socket(UnixStream::connect(socket).unwrap())
                    .unwrap();
            let host = Fill {
                rgb: DESKTOP,
                ..Fill::default()
            };
            let mut mgr = SurfaceManager::with_connection(conn, host, Config::default())
                .expect("the desktop connects");
            let props: HashMap<Prop, PropValue> = [
                (Prop::Name, PropValue::Text("Desk".into())),
                (Prop::Edge, PropValue::Keyword("top".into())),
                (Prop::Height, PropValue::Number(36.0)),
                (Prop::Margin, PropValue::Insets(Insets::all(0.0))),
            ]
            .into_iter()
            .collect();
            let spec = SurfaceSpec::resolve(NodeKind::Bar, |p| props.get(&p));
            mgr.state_mut()
                .apply_surface_change(NodeId::new(1, 0), SurfaceChange::Created(spec));
            while !s.load(Ordering::Acquire) {
                let _ = mgr.dispatch(Some(Duration::from_millis(20)));
            }
        });
        Desktop {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- another locker (the refused lock) ----------------------------------------

/// A lock client of the test's own, painted green, unlocked with a
/// token from a fake helper that accepts any password.
struct Other {
    mgr: SurfaceManager<Fill>,
}

impl Other {
    fn lock(sway: &Sway) -> Other {
        let host = Fill {
            rgb: OTHER,
            ..Fill::default()
        };
        let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default())
            .expect("the other locker connects");
        mgr.state_mut()
            .set_lock_color(Color::new(0.0, 1.0, 0.0, 1.0));
        mgr.state_mut().enable_session_lock();
        mgr.state_mut().lock().expect("the other locker asks");
        let ok = mgr.dispatch_until(WAIT, |s| s.is_locked()).unwrap_or(false);
        assert!(ok, "the other locker never locked");
        Other { mgr }
    }

    fn pump(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let _ = self.mgr.dispatch(Some(Duration::from_millis(10)));
        }
    }

    fn unlock(mut self, dir: &Path) {
        assert!(self.mgr.state_mut().unlock(token(dir)), "the other unlocks");
        self.pump(Duration::from_millis(300));
    }
}

fn token(dir: &Path) -> UnlockToken {
    let path = dir.join("accept");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '\\003\\000\\000\\000\\001\\001\\000'; sleep 0.2; \
         printf '\\002\\000\\000\\000\\003\\000'; sleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    fn no_hook() {}
    let mut c = Client::new(path, no_hook).with_timeout(Duration::from_secs(5));
    match c.submit(Password::from("x".to_string())) {
        Verdict::Unlocked(t) => t,
        v => panic!("the fake helper did not accept: {v:?}"),
    }
}

// ---- a keyboard with the password's characters --------------------------------

mod keyboard {
    use super::*;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
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
    delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Client: ignore wl_seat::WlSeat);

    /// `c`'s evdev code and keysym (QWERTY letters, `-`).
    fn key(c: char) -> Option<(u32, String)> {
        if c == '-' {
            return Some((12, "minus".into()));
        }
        let row = |s: &str, first: u32| s.find(c).map(|i| first + i as u32);
        row("qwertyuiop", 16)
            .or_else(|| row("asdfghjkl", 30))
            .or_else(|| row("zxcvbnm", 44))
            .map(|k| (k, c.to_string()))
    }

    const RETURN: u32 = 28;

    pub struct Keyboard {
        _conn: Connection,
        queue: EventQueue<Client>,
        keyboard: ZwpVirtualKeyboardV1,
        time: u32,
    }

    impl Keyboard {
        pub fn new(sway: &Sway) -> Self {
            let conn = sway.connect();
            let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
            let qh = queue.handle();
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
            let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
            let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
            let mut codes = String::new();
            let mut symbols = String::new();
            let keys = ('a'..='z')
                .chain(['-'])
                .filter_map(key)
                .chain([(RETURN, "Return".to_string())]);
            for (k, sym) in keys {
                let k = k + 8;
                codes.push_str(&format!("<K{k}> = {k}; "));
                symbols.push_str(&format!("key <K{k}> {{ [ {sym} ] }}; "));
            }
            let keymap = format!(
                "xkb_keymap {{\n\
                 xkb_keycodes \"strand\" {{ minimum = 8; maximum = 255; {codes}}};\n\
                 xkb_types \"strand\" {{ type \"ONE_LEVEL\" {{ modifiers = none; level_name[Level1] = \"Any\"; }}; }};\n\
                 xkb_compatibility \"strand\" {{ }};\n\
                 xkb_symbols \"strand\" {{ {symbols}}};\n\
                 }};\n"
            );
            let path = sway.dir.join("keymap");
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(keymap.as_bytes()).unwrap();
            file.write_all(&[0]).unwrap();
            file.flush().unwrap();
            let file = std::fs::File::open(&path).unwrap();
            keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
            queue.roundtrip(&mut Client).unwrap();
            Self {
                _conn: conn,
                queue,
                keyboard,
                time: 0,
            }
        }

        fn tap(&mut self, code: u32) {
            for state in [1, 0] {
                self.time += 10;
                self.keyboard.key(self.time, code, state);
            }
            self.queue.roundtrip(&mut Client).unwrap();
            std::thread::sleep(Duration::from_millis(30));
        }

        /// Types `text` (lowercase letters and `-`).
        pub fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                let (code, _) = key(c).expect("a key of the keymap");
                self.tap(code);
            }
        }

        /// Types `text` and presses Return.
        pub fn enter(&mut self, text: &str) {
            self.type_text(text);
            self.tap(RETURN);
        }
    }
}

use keyboard::Keyboard;

// ---- strand ----------------------------------------------------------------------

struct Strand {
    child: Option<Child>,
    env: Vec<(String, std::ffi::OsString)>,
    config: PathBuf,
    log: PathBuf,
    starts: usize,
}

impl Strand {
    /// `strand run` of `source` (as `lock.strand`, exporting `zero`) on
    /// `display` (sway's, or a proxy's in sway's directory), with
    /// `STRAND_FAULT=faults`.
    fn start(sway: &Sway, display: &str, faults: &str, source: &str) -> Strand {
        let home = sway.dir.join("home");
        let config = home.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("lock.strand"), source).unwrap();
        let env: Vec<(String, std::ffi::OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("XDG_RUNTIME_DIR".into(), sway.dir.clone().into()),
            ("XDG_CONFIG_HOME".into(), home.join(".config").into()),
            ("XDG_CACHE_HOME".into(), home.join(".cache").into()),
            ("XDG_STATE_HOME".into(), home.join(".state").into()),
            ("WAYLAND_DISPLAY".into(), display.into()),
            ("STRAND_LOG".into(), "info".into()),
            ("STRAND_FAULT".into(), faults.into()),
        ];
        let mut s = Strand {
            child: None,
            env,
            config,
            log: sway.dir.join("strand.log"),
            starts: 0,
        };
        s.run();
        s
    }

    /// Starts the binary (again), its log appended.
    fn run(&mut self) {
        self.starts += 1;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        writeln!(log, "---- start {}", self.starts).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&self.config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("LANG", "C.UTF-8")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        // Up when its IPC answers.
        let deadline = Instant::now() + WAIT;
        while !self.try_cli(&["set", "lock.zero", "1"]) {
            assert!(
                self.running(),
                "strand exited at start:\n{}",
                self.log_text()
            );
            assert!(
                Instant::now() < deadline,
                "strand never answered:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|c| c.try_wait().ok().flatten().is_none())
    }

    fn try_cli(&self, args: &[&str]) -> bool {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn cli(&self, args: &[&str]) {
        assert!(self.try_cli(args), "strand {args:?}:\n{}", self.log_text());
    }

    fn signal(&mut self, sig: i32) {
        let pid = self.child.as_ref().expect("running").id();
        // SAFETY: kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(pid as i32, sig) }, 0, "kill {sig}");
    }

    /// Waits for the process to end (after a kill, or after the unlock
    /// once logic is gone).
    fn wait_exit(&mut self, what: &str) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(c) = self.child.as_mut()
                && let Ok(Some(status)) = c.try_wait()
            {
                self.child = None;
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: strand did not exit\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A thread of the running process is named `name`.
    fn has_thread(&self, name: &str) -> bool {
        let pid = self.child.as_ref().expect("running").id();
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .map(|tasks| {
                tasks.filter_map(|t| t.ok()).any(|t| {
                    std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == name)
                })
            })
            .unwrap_or(false)
    }

    /// The restart marker (run/lock.rs).
    fn marker(&self, display: &str) -> PathBuf {
        let dir = self
            .env
            .iter()
            .find(|(k, _)| k == "XDG_RUNTIME_DIR")
            .map(|(_, v)| PathBuf::from(v))
            .unwrap();
        dir.join(format!("strand-{display}.locked"))
    }
}

impl Drop for Strand {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ---- the scenario ----------------------------------------------------------------

/// One test's sway, desktop, keyboard and strand (dropped in that
/// order reversed: the clients before their compositor).
struct Vm {
    strand: Strand,
    keys: Keyboard,
    _desktop: Desktop,
    sway: Sway,
}

impl Vm {
    fn start(tag: &str, faults: &str) -> Vm {
        Vm::with(tag, faults, CONFIG)
    }

    /// [`Vm::start`] with another config.
    fn with(tag: &str, faults: &str, source: &str) -> Vm {
        let sway = Sway::start(tag);
        let display = sway.display.clone();
        Vm::on(sway, &display, faults, source)
    }

    /// Strand on `display` of `sway`'s directory.
    fn on(sway: Sway, display: &str, faults: &str, source: &str) -> Vm {
        let desktop = Desktop::start(&sway);
        let keys = Keyboard::new(&sway);
        let strand = Strand::start(&sway, display, faults, source);
        let vm = Vm {
            sway,
            _desktop: desktop,
            keys,
            strand,
        };
        vm.until("HEADLESS-1", "the desktop", Shot::desktop);
        vm
    }

    /// Polls grim on `output` until `ok`; panics with what it saw.
    fn until(&self, output: &str, what: &str, ok: impl Fn(&Shot) -> bool) -> Shot {
        self.until_locked(output, what, false, ok)
    }

    /// [`Vm::until`]; with `locked`, every shot on the way must hide the
    /// desktop (the session stayed locked throughout).
    fn until_locked(
        &self,
        output: &str,
        what: &str,
        locked: bool,
        ok: impl Fn(&Shot) -> bool,
    ) -> Shot {
        let deadline = Instant::now() + WAIT;
        loop {
            let shot = self.sway.shot(output);
            if locked {
                assert!(
                    !shot.desktop(),
                    "{output}: the desktop showed while locked, waiting for {what} ({})\n{}",
                    shot.describe(),
                    self.strand.log_text()
                );
            }
            if ok(&shot) {
                return shot;
            }
            assert!(
                Instant::now() < deadline,
                "{output}: never {what}: {}\n{}",
                shot.describe(),
                self.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `strand set lock.locked true`, then the session locked.
    fn lock(&self) {
        self.strand.cli(&["set", "lock.locked", "true"]);
        self.until("HEADLESS-1", "locked", |s| !s.desktop());
    }

    /// The config's lock shows.
    fn content(&self) {
        self.until_locked("HEADLESS-1", "the config's lock", true, |s| {
            near(s.corner(), LOCK_BG)
        });
    }

    /// The built-in password field shows, the session locked meanwhile,
    /// and the log says why.
    fn fallback(&self) {
        self.until_locked(
            "HEADLESS-1",
            "the built-in password field",
            true,
            Shot::fallback,
        );
        self.fallback_logged();
    }

    fn fallback_logged(&self) {
        let log = self.strand.log_text();
        assert!(
            log.contains("lock: showing the built-in password field"),
            "the fallback's reason is logged:\n{log}"
        );
    }

    /// In the fallback: a wrong password is refused and the session
    /// stays locked; the right one unlocks it.
    fn fallback_passwords(&mut self) {
        self.fallback_passwords_on("HEADLESS-1");
    }

    /// [`Vm::fallback_passwords`] with the fallback on `output`.
    fn fallback_passwords_on(&mut self, output: &str) {
        self.keys.enter(WRONG);
        self.until_locked(output, "the wrong password refused", true, |s| {
            near(s.corner(), FALLBACK_BG) && near(s.field(), FIELD_REFUSED)
        });
        self.keys.enter(PASSWORD);
        self.until(output, "unlocked by the right password", Shot::desktop);
    }

    /// Types `password` into the config's lock (seen in its colour) and
    /// presses Return.
    fn lock_enter(&mut self, password: &str) {
        self.keys.type_text(password);
        self.until_locked("HEADLESS-1", "the typed password", true, |s| {
            near(s.corner(), LOCK_TYPED)
        });
        self.keys.enter("");
    }

    /// In the config's lock: the same, through `auth`.
    fn lock_passwords(&mut self) {
        self.lock_enter(WRONG);
        self.until_locked("HEADLESS-1", "the wrong password refused", true, |s| {
            near(s.corner(), LOCK_REFUSED)
        });
        self.lock_enter(PASSWORD);
        self.until(
            "HEADLESS-1",
            "unlocked by the right password",
            Shot::desktop,
        );
    }

    fn log_has(&self, text: &str) {
        let log = self.strand.log_text();
        assert!(log.contains(text), "the log says {text:?}:\n{log}");
    }
}

/// A fault that shows the fallback once the session is locked; `before`
/// runs with the config's lock up (`None`: the fault strikes before it
/// can be seen).
fn fallback_scenario(test: &str, faults: &str, before: Option<fn(&mut Vm)>) -> Option<Vm> {
    if !in_lock_vm(test) {
        return None;
    }
    let mut vm = Vm::start(test, faults);
    vm.lock();
    if let Some(f) = before {
        vm.content();
        f(&mut vm);
    }
    vm.fallback();
    Some(vm)
}

// ---- the matrix ------------------------------------------------------------------

/// The logic thread panics on `Locked`: the fallback shows; a monitor
/// plugged in meanwhile is covered too; after the unlock the run ends
/// (logic is gone and no lock holds it).
#[test]
fn logic_panic_shows_the_fallback_and_covers_a_new_output() {
    let Some(mut vm) = fallback_scenario("logic_panic", "logic_panic", None) else {
        return;
    };
    vm.log_has("logic ended");
    let second = vm.sway.plug();
    vm.until_locked(&second, "the new output covered", true, |s| {
        near(s.corner(), FALLBACK_BG)
    });
    vm.fallback_passwords();
    vm.until(&second, "the desktop on the new output", Shot::desktop);
    vm.strand.wait_exit("logic gone, unlocked");
}

/// The logic thread stops answering: the watchdog shows the fallback.
#[test]
fn logic_stall_trips_the_watchdog() {
    let Some(mut vm) = fallback_scenario("logic_hang", "logic_hang", None) else {
        return;
    };
    vm.log_has("did not answer");
    vm.fallback_passwords();
}

/// The text worker dies (on its first job once locked).
#[test]
fn text_worker_death_shows_the_fallback() {
    let Some(mut vm) = fallback_scenario("text_panic", "text_panic", None) else {
        return;
    };
    vm.log_has("the text worker stopped");
    vm.fallback_passwords();
}

/// The right password, typed into the config's lock, cannot be checked
/// (`fault`): the session stays locked and the fallback, with a helper
/// of its own, takes over.
fn auth_fault(test: &str, fault: &str) {
    let Some(mut vm) = fallback_scenario(test, fault, Some(|vm| vm.lock_enter(PASSWORD))) else {
        return;
    };
    vm.log_has("`auth` failed");
    vm.fallback_passwords();
}

#[test]
fn pam_helper_crash_shows_the_fallback() {
    auth_fault("auth_crash", "auth_crash");
}

#[test]
fn pam_helper_hang_shows_the_fallback() {
    auth_fault("auth_hang", "auth_hang");
}

#[test]
fn pam_helper_garbage_shows_the_fallback() {
    auth_fault("auth_garbage", "auth_garbage");
}

#[test]
fn pam_helper_missing_shows_the_fallback() {
    auth_fault("auth_missing", "auth_missing");
}

/// The other outputs show the config's lock colour, the fallback's
/// while it shows, and the config's again on the next lock of the same
/// run (`auth_missing`: every submit through `auth` shows the fallback),
/// following the lock's `when` (`auth.failed`).
#[test]
fn a_lock_after_the_fallback_has_the_configs_colour_again() {
    let test = "colour_after_fallback";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "auth_missing");
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    // The second lock still says `auth.failed` (nothing succeeded
    // through `auth`): its colour is the refused one, on both outputs.
    for bg in [LOCK_BG, LOCK_REFUSED] {
        if bg != LOCK_BG {
            // A new lock session: closed, then opened again.
            vm.strand.cli(&["set", "lock.locked", "false"]);
        }
        vm.lock();
        vm.until_locked("HEADLESS-1", "the config's lock", true, |s| {
            near(s.corner(), bg)
        });
        vm.until_locked(
            &second,
            "the lock's colour on the other output",
            true,
            |s| near(s.corner(), bg),
        );
        vm.lock_enter(PASSWORD);
        vm.fallback();
        vm.until_locked(
            &second,
            "the fallback's colour on the other output",
            true,
            |s| near(s.corner(), FALLBACK_BG),
        );
        vm.fallback_passwords();
        vm.until(&second, "the desktop on the second output", Shot::desktop);
    }
}

/// A runtime fault inside the lock (`10 / zero`) freezes its component.
#[test]
fn runtime_fault_in_the_lock_shows_the_fallback() {
    let Some(mut vm) = fallback_scenario(
        "lock_fault",
        "",
        Some(|vm| vm.strand.cli(&["set", "lock.zero", "0"])),
    ) else {
        return;
    };
    vm.log_has("a runtime fault froze the lock");
    vm.fallback_passwords();
}

/// The lock never draws: the fallback draws instead.
#[test]
fn lock_without_a_first_frame_shows_the_fallback() {
    let Some(mut vm) = fallback_scenario("lock_no_frame", "lock_no_frame", None) else {
        return;
    };
    vm.log_has("no first frame");
    vm.fallback_passwords();
}

/// SIGTERM while locked: logic is told to stop, the fallback shows, and
/// the run ends only after the unlock.
#[test]
fn sigterm_while_locked_waits_for_the_unlock() {
    let Some(mut vm) = fallback_scenario("sigterm", "", Some(|vm| vm.strand.signal(libc::SIGTERM)))
    else {
        return;
    };
    assert!(vm.strand.running(), "strand outlives SIGTERM while locked");
    vm.fallback_passwords();
    let status = vm.strand.wait_exit("SIGTERM, unlocked");
    assert!(status.success(), "{status:?}");
}

/// SIGTERM while locked, logic ended, then the content's output
/// unplugged: the content is made again on the other output and the
/// fallback follows it there and takes the passwords. Before the fault
/// the other output shows the config's lock colour, after it the
/// fallback's. (Logic's unmount at its end sends render no diff, so the
/// lock's node stays in render's tree here; `lock_unmount` below plays
/// the node gone.)
#[test]
fn sigterm_then_the_contents_output_unplugged_keeps_the_fallback() {
    let test = "sigterm_unplug";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked(
        &second,
        "the lock's colour on the other output",
        true,
        |s| near(s.corner(), LOCK_BG),
    );
    vm.strand.signal(libc::SIGTERM);
    vm.fallback();
    vm.until_locked(
        &second,
        "the fallback's colour on the other output",
        true,
        |s| near(s.corner(), FALLBACK_BG),
    );
    // Logic has unmounted the lock and ended.
    let deadline = Instant::now() + WAIT;
    while vm.strand.has_thread("strand-logic") {
        assert!(
            Instant::now() < deadline,
            "logic never ended after SIGTERM:\n{}",
            vm.strand.log_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(vm.strand.running(), "strand outlives SIGTERM while locked");
    vm.sway.unplug("HEADLESS-1");
    vm.until_locked(
        &second,
        "the built-in password field on the remaining output",
        true,
        Shot::fallback,
    );
    vm.fallback_passwords_on(&second);
    let status = vm.strand.wait_exit("SIGTERM, unlocked");
    assert!(status.success(), "{status:?}");
}

/// The lock leaves render's tree while locked (`lock_unmount`: "the
/// lock is no longer mounted"), then the content's output is unplugged:
/// strand-surface makes the content again on the other output with the
/// node render no longer knows, and the fallback follows it there (not
/// a blank surface whose keys reach nothing).
#[test]
fn lock_unmounted_then_the_contents_output_unplugged_keeps_the_fallback() {
    let test = "unmount_unplug";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "lock_unmount");
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.fallback();
    vm.log_has("the lock is no longer mounted");
    vm.until_locked(
        &second,
        "the fallback's colour on the other output",
        true,
        |s| near(s.corner(), FALLBACK_BG),
    );
    vm.sway.unplug("HEADLESS-1");
    vm.until_locked(
        &second,
        "the built-in password field on the remaining output",
        true,
        Shot::fallback,
    );
    vm.fallback_passwords_on(&second);
    assert!(vm.strand.running(), "logic is alive: the run goes on");
}

/// Killed by `sig` while locked: sway keeps the session locked; strand
/// started again finds its marker, locks at once and shows the fallback.
fn restart_after(test: &str, sig: i32) {
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    let display = vm.sway.display.clone();
    vm.lock();
    vm.content();
    let marker = vm.strand.marker(&display);
    vm.until("HEADLESS-1", "the restart marker written", |_| {
        marker.exists()
    });
    vm.strand.signal(sig);
    vm.strand.wait_exit("killed");
    // No strand at all: still locked.
    std::thread::sleep(Duration::from_millis(500));
    let shot = vm.sway.shot("HEADLESS-1");
    assert!(
        !shot.desktop(),
        "locked with no locker: {}",
        shot.describe()
    );
    vm.strand.run();
    vm.fallback();
    vm.log_has("locking again");
    vm.fallback_passwords();
    vm.until("HEADLESS-1", "the marker removed", |_| !marker.exists());
}

/// A session locked with no `lock` compiled (a strand killed while
/// locked comes back with a config that has none): the fallback shows.
#[test]
fn no_lock_compiled_shows_the_fallback() {
    let test = "no_lock";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", "export state zero = 1\n");
    let marker = vm.strand.marker(&vm.sway.display.clone());
    vm.strand.signal(libc::SIGKILL);
    vm.strand.wait_exit("killed");
    // As a strand killed while locked leaves it.
    std::fs::write(&marker, b"locked\n").unwrap();
    vm.strand.run();
    vm.fallback();
    vm.log_has("with no `lock` open");
    vm.fallback_passwords();
    vm.until("HEADLESS-1", "the marker removed", |_| !marker.exists());
}

#[test]
fn killed_while_locked_locks_again_on_restart() {
    restart_after("sigkill", libc::SIGKILL);
}

/// An allocation failure aborts the process (`handle_alloc_error`):
/// SIGABRT stands in for it.
#[test]
fn aborted_while_locked_locks_again_on_restart() {
    restart_after("sigabrt", libc::SIGABRT);
}

/// The compositor ends a lock it granted (the proxy's `finished` after
/// `locked`): strand asks for a new lock, which shows the fallback.
#[test]
fn finished_after_locked_locks_again_with_the_fallback() {
    let test = "finished_after_locked";
    if !in_lock_vm(test) {
        return;
    }
    let sway = Sway::start("finished");
    let proxy = proxy::Proxy::start(&sway.dir, "wayland-proxy", sway.socket());
    let mut vm = Vm::on(sway, &proxy.name(), "", CONFIG);
    vm.lock();
    vm.content();
    proxy.end();
    // Between `finished` and the new lock sway may show a frame
    // unlocked (the compositor ended the lock), so the way there is not
    // asserted locked; the fallback itself is.
    vm.until("HEADLESS-1", "the built-in password field", Shot::fallback);
    assert!(proxy.ended());
    vm.fallback_logged();
    vm.log_has("the compositor ended the session lock");
    vm.log_has("the compositor ended the lock");
    vm.fallback_passwords();
}

/// Another locker holds the session: the compositor refuses strand's
/// lock. Not a fault: no fallback, the run goes on, and once the other
/// locker is gone strand's own lock works with the right password only.
#[test]
fn a_refused_lock_is_not_a_fault() {
    let test = "refused";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    let mut other = Other::lock(&vm.sway);
    other.pump(Duration::from_millis(300));
    vm.until("HEADLESS-1", "the other locker's lock", |s| {
        near(s.corner(), OTHER)
    });
    vm.strand.cli(&["set", "lock.locked", "true"]);
    let deadline = Instant::now() + WAIT;
    while !vm
        .strand
        .log_text()
        .contains("the compositor refused the session lock")
    {
        other.pump(Duration::from_millis(50));
        assert!(
            Instant::now() < deadline,
            "never refused:\n{}",
            vm.strand.log_text()
        );
    }
    other.pump(Duration::from_millis(1500));
    assert!(vm.strand.running(), "strand runs on after a refusal");
    let shot = vm.sway.shot("HEADLESS-1");
    assert!(near(shot.corner(), OTHER), "{}", shot.describe());
    let log = vm.strand.log_text();
    assert!(
        !log.contains("showing the built-in password field"),
        "a refusal is not a fault:\n{log}"
    );
    other.unlock(&vm.sway.dir);
    vm.until("HEADLESS-1", "the desktop back", Shot::desktop);
    // A new lock session: closed, then opened again.
    vm.strand.cli(&["set", "lock.locked", "false"]);
    vm.lock();
    vm.content();
    // Only a password unlocks: a config write of `false` is ignored.
    vm.strand.cli(&["set", "lock.locked", "false"]);
    std::thread::sleep(Duration::from_millis(1000));
    let shot = vm.sway.shot("HEADLESS-1");
    assert!(near(shot.corner(), LOCK_BG), "{}", shot.describe());
    vm.lock_passwords();
    assert!(vm.strand.running());
}

/// The binary under test carries the fault points (the other tests'
/// faults would otherwise pass as no-ops). Runs everywhere.
#[test]
fn the_binary_under_test_holds_the_fault_points() {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_strand")).unwrap();
    for needle in [&b"STRAND_FAULT"[..], b"logic_panic", b"lock_no_frame"] {
        assert!(
            bytes.windows(needle.len()).any(|w| w == needle),
            "{} missing: build with --features faults",
            String::from_utf8_lossy(needle)
        );
    }
}
