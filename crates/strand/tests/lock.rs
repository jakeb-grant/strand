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
//! client of the test's own, never shows: no pixel of any shot is its
//! colour), that the built-in password
//! field appears (its background `#11111b` and field `#1e1e2e`), that a
//! wrong password is refused (the field turns `#3b202c`) and leaves the
//! session locked, and that the test user's password (`strand-test`,
//! baked into the VM) unlocks it.
//!
//! Faults: the logic thread panics, or stops (the watchdog); the text
//! worker dies; the PAM helper crashes, hangs, answers garbage or is
//! missing, is SIGKILLed mid-check from outside, or is deleted and
//! killed mid-session; a runtime fault freezes the lock's component; the lock never
//! draws a first frame; a session locked with no `lock` compiled;
//! SIGTERM while locked, and then the content's output unplugged (the
//! lock's node kept), or the lock unmounted and then the content's
//! output unplugged; strand killed (SIGKILL) or aborted (SIGABRT, what
//! an allocation failure does) while locked and
//! started again, by the test or by a supervisor (a restart loop with
//! the documented systemd unit's policy, read from architecture.md),
//! twice in a row and then in a crash loop past systemd's default start
//! limit, or aborted with its lock still pending (before `locked`); the compositor ends a lock it granted (`finished` after
//! `locked`, played by a Wayland proxy, tests/lock/proxy.rs: sway 1.9
//! never sends it); the compositor refusing the lock (another locker
//! holds it: not a fault, nothing shows, the run goes on); an output
//! plugged in while locked; a config write of `false` while locked
//! (only a password unlocks). Not a fault: the config's Caps Lock
//! warning (`keyboard.caps_lock`, gap 31) follows the lock's keyboard. A run under `STRAND_MOCK` never locks. The
//! inspector cannot pick while locked (M5). With a fingerprint-shaped
//! stack (`fingerprint_*`, run by `scripts/lockvm/scenarios/fingerprint.sh`
//! only, m6-lock): the lock shows PAM's text while the reader waits and a
//! finger unlocks it, the prompt once the reader gives up, a password
//! typed while it waits is tried after it, and the fallback's field takes
//! the password through the same stack.
//!
//! No assertion depends on how long anything took: each wait is for
//! what grim shows, bounded only to fail instead of hanging.

#[path = "lock/proxy.rs"]
mod proxy;
mod support;

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
/// (M6) PAM's text below the fallback's field.
const FALLBACK_TEXT: [u8; 3] = [0xba, 0xc2, 0xde];
/// Another locker's colour.
const OTHER: [u8; 3] = [0x00, 0xff, 0x00];

/// strand's log line when a lock missed its first-frame deadline.
const FIRST_FRAME_MISSED: &str = "drew no first frame within";

/// How long anything shown is waited for before the test fails.
const WAIT: Duration = Duration::from_secs(30);

/// The config: a lock opened by `lock.locked`, its password checked by
/// `auth` through auth.schema's documented idiom (`on activate`: Return
/// in a plain `input`), its colour showing typed text and a refusal, and
/// a text that faults once `lock.zero` is 0.
const CONFIG: &str = r#"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: #2050d0
  when auth.failed { bg: #a02040 }
  when secret != "" { bg: #20a050 }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
  text pct(10 / zero)
}
"#;

/// (m6-lock) The config's lock coloured by `auth.message` (PAM's text
/// shows) and `auth.prompt` (PAM asks for the password), which it also
/// shows as text, for the fingerprint tests.
const FINGER_CONFIG: &str = r#"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: #2050d0
  when auth.message != "" { bg: #806020 }
  when auth.prompt != "" { bg: #208080 }
  when auth.failed { bg: #a02040 }
  when secret != "" { bg: #20a050 }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
  text auth.message
  text auth.prompt
}
"#;
/// [`FINGER_CONFIG`]'s lock while PAM's text shows, and while PAM asks.
const LOCK_TOLD: [u8; 3] = [0x80, 0x60, 0x20];
const LOCK_ASKED: [u8; 3] = [0x20, 0x80, 0x80];

/// auth.schema's `on activate` handler, which [`CONFIG`] uses.
const IDIOM: &str = r#"on activate { if secret != "" { auth.submit(secret) }; secret = "" }"#;

/// (m4-audit) [`CONFIG`]'s field is the one auth.schema documents (and
/// `check::lock_no_auth`'s help suggests), so the VM unlocks with the
/// field users copy. Only text is compared: nothing locks here.
#[test]
fn the_vm_config_uses_the_documented_field() {
    let schema = include_str!("../../strand-services-schema/src/auth.schema");
    let doc: String = schema
        .lines()
        .filter_map(|l| l.trim().strip_prefix("///"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        doc.contains(IDIOM),
        "auth.schema no longer documents {IDIOM}"
    );
    assert!(CONFIG.contains(IDIOM));
}

/// [`CONFIG`] submitting through `on key` instead, the form m4-lock-w2
/// documented before Return activated a plain `input`; it keeps working.
fn config_on_key() -> String {
    CONFIG.replace(
        IDIOM,
        r#"on key(k) { if k.name == "Return" { auth.submit(secret); secret = "" } }"#,
    )
}

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
    /// (lock-4k) Where the next output goes: the right edge of the
    /// others, in logical pixels.
    right: u32,
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
            right: 1280,
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
        self.plug_mode((1280, 720), 1.0)
    }

    /// (lock-4k) A new output of `mode` at `scale` to the right of the
    /// others: its name.
    fn plug_mode(&mut self, mode: (u32, u32), scale: f64) -> String {
        self.msg(&["create_output"]).expect("create_output");
        self.outputs += 1;
        let name = format!("HEADLESS-{}", self.outputs);
        let x = self.right;
        self.set_mode(&name, mode, scale, x);
        name
    }

    /// (lock-4k) Output `name` at `mode` and `scale`, its left edge at
    /// logical `x`; the next output goes to its right.
    fn set_mode(&mut self, name: &str, (w, h): (u32, u32), scale: f64, x: u32) {
        let mode = format!("{w}x{h}");
        self.msg(&[
            "output",
            name,
            "resolution",
            &mode,
            "scale",
            &scale.to_string(),
            "position",
            &x.to_string(),
            "0",
        ])
        .expect("place the output");
        self.right = x + (f64::from(w) / scale).round() as u32;
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

    /// Any pixel of the output shows the desktop's bar: what a lock
    /// must hide everywhere, not only where [`Shot::top`] samples it (a
    /// surface covering part of the output, or part of the bar, still
    /// leaves some of it).
    fn desktop_anywhere(&self) -> Option<(usize, usize)> {
        self.rgb
            .chunks_exact(3)
            .position(|p| near([p[0], p[1], p[2]], DESKTOP))
            .map(|i| (i % self.w, i / self.w))
    }

    /// (M6) PAM's text shows below the fallback's field.
    fn text_below_field(&self) -> bool {
        let top = (self.h / 2 + 30).min(self.h);
        let bottom = (self.h / 2 + 160).min(self.h);
        (top..bottom).any(|y| (0..self.w).any(|x| self.px(x, y) == FALLBACK_TEXT))
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
        "#!/bin/sh\nprintf '\\003\\000\\000\\000\\001\\002\\000'; sleep 0.2; \
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
    /// (oi-lock) Tab, which focuses the lock's field when it has no
    /// `focus: true`.
    const TAB: u32 = 15;

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
                .chain([(RETURN, "Return".to_string()), (TAB, "Tab".to_string())]);
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

        /// (oi-lock) Presses Tab.
        pub fn tab(&mut self) {
            self.tap(TAB);
        }

        /// (gap 31) Sets the locked modifiers: [`CAPS_LOCK`] (`Lock`)
        /// or none.
        pub fn lock_keys(&mut self, locked: u32) {
            self.keyboard.modifiers(0, 0, locked, 0);
            self.queue.roundtrip(&mut Client).unwrap();
        }
    }

    /// The `Lock` modifier's bit: Caps Lock locked.
    pub const CAPS_LOCK: u32 = 1 << 1;
}

use keyboard::{CAPS_LOCK, Keyboard};

// ---- strand ----------------------------------------------------------------------

struct Strand {
    child: Option<Child>,
    env: Vec<(String, std::ffi::OsString)>,
    config: PathBuf,
    log: PathBuf,
    starts: usize,
    /// `child` is a supervisor: [`Supervisor::documented`], the restart
    /// policy of the user unit architecture.md gives.
    supervised: bool,
    /// The binary run: cargo's, or a copy ([`Vm::installed`]).
    bin: PathBuf,
}

impl Strand {
    /// `strand run` of `source` (as `lock.strand`, exporting `zero`) on
    /// `display` (sway's, or a proxy's in sway's directory), with
    /// `STRAND_FAULT=faults`.
    fn start(sway: &Sway, display: &str, faults: &str, source: &str) -> Strand {
        let mut s = Strand::prepare(sway, display, faults, source);
        s.run();
        s
    }

    /// [`Strand::start`] under a supervisor.
    fn supervised(sway: &Sway, source: &str) -> Strand {
        let mut s = Strand::prepare(sway, &sway.display.clone(), "", source);
        s.supervised = true;
        s.run();
        s
    }

    /// Its config and environment, not started.
    fn prepare(sway: &Sway, display: &str, faults: &str, source: &str) -> Strand {
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
        Strand {
            child: None,
            env,
            config,
            log: sway.dir.join("strand.log"),
            starts: 0,
            supervised: false,
            bin: PathBuf::from(env!("CARGO_BIN_EXE_strand")),
        }
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
        let mut cmd = if self.supervised {
            Supervisor::documented().command(self.bin.as_os_str(), &self.config)
        } else {
            let mut c = Command::new(&self.bin);
            c.arg("run").arg(&self.config);
            c
        };
        let child = cmd
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
        Command::new(&self.bin)
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

    /// (report) `strand <args>`, run to its end: its output.
    fn cli_out(&self, args: &[&str]) -> std::process::Output {
        Command::new(&self.bin)
            .args(args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// (report) Where `strand report` with words saves.
    fn reports(&self) -> PathBuf {
        let state = self
            .env
            .iter()
            .find(|(k, _)| k == "XDG_STATE_HOME")
            .map(|(_, v)| PathBuf::from(v))
            .unwrap();
        state.join("strand/reports")
    }

    /// (report) `strand report --json <words>`, which must succeed: the
    /// bundle's directory.
    fn report(&self, words: &str) -> PathBuf {
        let mut args = vec!["report", "--json"];
        args.extend(words.split(' '));
        let out = self.cli_out(&args);
        assert!(
            out.status.success(),
            "strand {args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_text()
        );
        let ans: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        PathBuf::from(ans["bundle"].as_str().unwrap())
    }

    /// Waits for the log to say `text`.
    fn until_log(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.log_text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "the log never said {text:?}:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
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

    /// The running strand's pid: the child's, or under a supervisor its
    /// child named `strand` (`None` between two runs, while it waits out
    /// its restart delay in a `sleep`).
    fn pid(&self) -> Option<u32> {
        let pid = self.child.as_ref()?.id();
        if !self.supervised {
            return Some(pid);
        }
        let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
        children
            .split_whitespace()
            .filter_map(|c| c.parse::<u32>().ok())
            .find(|c| {
                std::fs::read_to_string(format!("/proc/{c}/comm"))
                    .is_ok_and(|n| n.trim() == "strand")
            })
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
        // The supervisor first, so it starts nothing more; then its strand.
        let supervised = if self.supervised { self.pid() } else { None };
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if let Some(pid) = supervised {
            // SAFETY: kill(2) on the supervisor's child, ours to end.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

// ---- the supervisor ---------------------------------------------------------------

/// The user unit architecture.md documents for running strand, the
/// first ```ini block after "A user unit for it".
fn documented_unit() -> &'static str {
    const ARCH: &str = include_str!("../../../docs/architecture.md");
    let at = ARCH
        .find("A user unit for it")
        .expect("architecture.md documents a user unit");
    let rest = &ARCH[at..];
    let start = rest.find("```ini\n").expect("the unit's ini block") + "```ini\n".len();
    let len = rest[start..].find("```").expect("the ini block ends");
    &rest[start..start + len]
}

/// systemd's restart policy for a unit, as a restart loop runs it: with
/// `Restart=on-failure` it starts the service again after a non-zero
/// exit or a signal and stops after a clean exit; the n-th restart
/// waits `delays[n]` (the last for every later one); a start that would
/// be the `burst + 1`-th within `interval` is refused and the loop gives
/// up, as systemd marks the unit failed (`interval` zero: no limit).
#[derive(Debug)]
struct Supervisor {
    delays: Vec<Duration>,
    burst: u32,
    interval: Duration,
}

impl Supervisor {
    /// [`documented_unit`]'s policy.
    fn documented() -> Supervisor {
        Supervisor::of_unit(documented_unit())
    }

    /// The policy of unit file `ini`, with systemd's defaults
    /// (`DefaultStartLimitIntervalSec=10s`, `DefaultStartLimitBurst=5`,
    /// `RestartSec=100ms`) for what it leaves out. With `RestartSteps`
    /// (systemd 254) the delay grows from `RestartSec` to
    /// `RestartMaxDelaySec` over that many restarts, geometrically, as
    /// systemd interpolates it.
    fn of_unit(ini: &str) -> Supervisor {
        let key = |k: &str| {
            ini.lines()
                .filter_map(|l| l.trim().split_once('='))
                .filter(|(name, _)| name.trim() == k)
                .map(|(_, v)| v.trim().to_string())
                .next_back()
        };
        assert_eq!(key("Restart").as_deref(), Some("on-failure"), "{ini}");
        let span = |k: &str, default: Duration| key(k).map_or(default, |v| timespan(&v));
        let restart = span("RestartSec", Duration::from_millis(100));
        let max = span("RestartMaxDelaySec", restart);
        let steps: u32 = key("RestartSteps").map_or(0, |v| v.parse().expect("RestartSteps"));
        let delays = if steps == 0 || max <= restart {
            vec![restart]
        } else if restart.is_zero() {
            (0..=steps).map(|i| max * i / steps).collect()
        } else {
            let ratio = max.as_secs_f64() / restart.as_secs_f64();
            (0..=steps)
                .map(|i| restart.mul_f64(ratio.powf(f64::from(i) / f64::from(steps))))
                .collect()
        };
        Supervisor {
            delays,
            burst: key("StartLimitBurst").map_or(5, |v| v.parse().expect("StartLimitBurst")),
            interval: span("StartLimitIntervalSec", Duration::from_secs(10)),
        }
    }

    /// A shell running `strand run config` under this policy, saying
    /// `---- supervisor: strand exited (<status>), starting it again`
    /// after each failure and `---- supervisor: start limit hit` when it
    /// gives up.
    fn command(&self, strand: &std::ffi::OsStr, config: &Path) -> Command {
        const SCRIPT: &str = r#"strand=$1 config=$2 delays=$3 burst=$4 interval=$5
n=0 starts=
while :; do
  if [ "$interval" -gt 0 ]; then
    now=$(($(date +%s%N) / 1000000)) recent= k=0
    for t in $starts; do
      if [ $((now - t)) -lt "$interval" ]; then recent="$recent $t" k=$((k + 1)); fi
    done
    if [ "$k" -ge "$burst" ]; then
      echo "---- supervisor: start limit hit ($k starts in $interval ms), giving up"
      exit 1
    fi
    starts="$recent $now"
  fi
  "$strand" run "$config"
  s=$?
  [ "$s" -eq 0 ] && exit 0
  echo "---- supervisor: strand exited ($s), starting it again"
  i=0 d=0
  for x in $delays; do d=$x; [ "$i" -ge "$n" ] && break; i=$((i + 1)); done
  n=$((n + 1))
  sleep "$d"
done"#;
        let delays: Vec<String> = self
            .delays
            .iter()
            .map(|d| format!("{:.3}", d.as_secs_f64()))
            .collect();
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(SCRIPT)
            .arg("supervisor")
            .arg(strand)
            .arg(config)
            .arg(delays.join(" "))
            .arg(self.burst.to_string())
            .arg(self.interval.as_millis().to_string());
        c
    }
}

/// A systemd time span: `0`, `100ms`, `2s`, `5min`, or bare seconds.
fn timespan(v: &str) -> Duration {
    let (n, unit) = v
        .find(|c: char| !c.is_ascii_digit())
        .map_or((v, ""), |i| v.split_at(i));
    let n: u64 = n.parse().unwrap_or_else(|_| panic!("time span {v:?}"));
    match unit.trim() {
        "" | "s" | "sec" => Duration::from_secs(n),
        "ms" | "msec" => Duration::from_millis(n),
        "min" | "m" => Duration::from_secs(n * 60),
        other => panic!("time span unit {other:?} in {v:?}"),
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

    /// [`Vm::start`] with strand under a supervisor.
    fn supervised(tag: &str) -> Vm {
        let sway = Sway::start(tag);
        let desktop = Desktop::start(&sway);
        let keys = Keyboard::new(&sway);
        let strand = Strand::supervised(&sway, CONFIG);
        let vm = Vm {
            sway,
            _desktop: desktop,
            keys,
            strand,
        };
        vm.until("HEADLESS-1", "the desktop", Shot::desktop);
        vm
    }

    /// [`Vm::start`] (no faults) with strand and its helper copied into
    /// sway's directory and run from there, so a test may delete the
    /// helper: the build directory may belong to another user (CI's
    /// runner), and is shared with the host.
    fn installed(tag: &str) -> Vm {
        let sway = Sway::start(tag);
        let desktop = Desktop::start(&sway);
        let keys = Keyboard::new(&sway);
        let bin = sway.dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cargo = Path::new(env!("CARGO_BIN_EXE_strand"));
        for name in ["strand", "strand-auth"] {
            std::fs::copy(cargo.with_file_name(name), bin.join(name)).unwrap();
        }
        let mut strand = Strand::prepare(&sway, &sway.display.clone(), "", CONFIG);
        strand.bin = bin.join("strand");
        strand.run();
        let vm = Vm {
            sway,
            _desktop: desktop,
            keys,
            strand,
        };
        vm.until("HEADLESS-1", "the desktop", Shot::desktop);
        vm
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

    /// Every shot of `output` for `span` passes `ok` (`what` it must
    /// show). Taken only after a positive sign that strand handled what
    /// the absence check is about (m4-audit): the span then covers the
    /// compositor drawing the result, not strand's startup.
    fn stays(&self, output: &str, what: &str, span: Duration, ok: impl Fn(&Shot) -> bool) {
        let end = Instant::now() + span;
        loop {
            let shot = self.sway.shot(output);
            assert!(
                ok(&shot),
                "{output}: not {what}: {}\n{}",
                shot.describe(),
                self.strand.log_text()
            );
            if Instant::now() >= end {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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
            if locked && let Some((x, y)) = shot.desktop_anywhere() {
                panic!(
                    "{output}: the desktop showed at {x},{y} while locked, waiting for {what} ({})\n{}",
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
        self.until("HEADLESS-1", "locked", |s| s.desktop_anywhere().is_none());
    }

    /// The config's lock shows. Should it have missed its 1 s first-frame
    /// deadline (the fallback took over), that is a wall-clock gate
    /// miss, not a functional failure: the test stops with `GATE_MISS`.
    fn content(&self) {
        self.until_locked("HEADLESS-1", "the config's lock", true, |s| {
            let log = self.strand.log_text();
            if log.contains(FIRST_FRAME_MISSED) {
                panic!(
                    "GATE_MISS lock first frame: the config's lock drew no first frame \
                     within 1 s and the fallback took over\n{log}"
                );
            }
            near(s.corner(), LOCK_BG)
        });
    }

    /// The first frame's time strand logged for each lock, in ms.
    fn first_frames(&self) -> Vec<u64> {
        self.strand
            .log_text()
            .lines()
            .filter_map(|l| l.split("lock: first frame after ").nth(1))
            .filter_map(|r| r.split(' ').next()?.parse().ok())
            .collect()
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

    /// Waits until strand's log (its helpers' stderr included) says
    /// `text`.
    fn until_log(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.strand.log_text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "the log never said {text:?}:\n{}",
                self.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILLs every PAM helper strand runs (what the OOM killer
    /// does); returns their pids.
    fn kill_helpers(&self) -> Vec<u32> {
        let pid = self.strand.pid().expect("strand runs");
        let helpers: Vec<u32> = std::fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
            .flat_map(|c| {
                c.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .filter(|c| {
                std::fs::read_to_string(format!("/proc/{c}/comm"))
                    .is_ok_and(|n| n.trim() == "strand-auth")
            })
            .collect();
        for h in &helpers {
            // SAFETY: kill(2) on strand's child, which runs as us.
            assert_eq!(unsafe { libc::kill(*h as i32, libc::SIGKILL) }, 0);
        }
        helpers
    }
}

/// The PAM helper beside `strand` (a [`Vm::installed`] copy) moved away
/// for the guard's life (deleted, as far as anything looking for it can
/// tell), and put back when the guard drops.
struct HelperAway {
    path: PathBuf,
    away: PathBuf,
}

impl HelperAway {
    fn new(strand: &Path) -> HelperAway {
        let path = strand.with_file_name("strand-auth");
        let away = path.with_file_name("strand-auth.away");
        std::fs::rename(&path, &away).unwrap();
        HelperAway { path, away }
    }
}

impl Drop for HelperAway {
    fn drop(&mut self) {
        let _ = std::fs::rename(&self.away, &self.path);
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

/// Tier C's helper killed mid-check: the helper reads the right
/// password and hangs (`auth_hang` with the default timeout,
/// `auth_hold`), and is then SIGKILLed from outside, as the OOM killer
/// would. The check is not an unlock: the session stays locked, the
/// fallback takes over with a helper of its own, and only the right
/// password unlocks it.
#[test]
fn pam_helper_killed_mid_check_shows_the_fallback() {
    let Some(mut vm) = fallback_scenario(
        "auth_killed",
        "auth_hang,auth_hold",
        Some(|vm| {
            vm.lock_enter(PASSWORD);
            vm.until_log("strand-auth: STRAND_FAULT auth_hang");
            assert_eq!(vm.kill_helpers().len(), 1, "the hung helper");
        }),
    ) else {
        return;
    };
    vm.log_has("`auth` failed (the password could not be checked: the PAM helper stopped)");
    vm.fallback_passwords();
}

/// Tier C's helper deleted: the helper binary is gone and the running
/// helper killed, so `auth` cannot start another. The session stays
/// locked, the fallback shows, and while no helper exists the right
/// password is refused like any other; once the binary is back the
/// fallback finds it and only the right password unlocks.
#[test]
fn pam_helper_deleted_and_killed_keeps_the_session_locked_until_it_is_back() {
    let test = "auth_deleted";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::installed(test);
    vm.lock();
    vm.content();
    let away = HelperAway::new(&vm.strand.bin);
    assert_eq!(vm.kill_helpers().len(), 1, "`auth`'s helper");
    vm.lock_enter(PASSWORD);
    vm.fallback();
    vm.log_has("`auth` failed (the password could not be checked: cannot start the PAM helper");
    vm.keys.enter(PASSWORD);
    vm.until_log("no `strand-auth` helper is installed");
    vm.until_locked(
        "HEADLESS-1",
        "the right password refused with no helper",
        true,
        |s| near(s.corner(), FALLBACK_BG) && near(s.field(), FIELD_REFUSED),
    );
    drop(away);
    vm.fallback_passwords();
}

// ---- a fingerprint module ahead of the password (m6-lock) ------------------------

/// [`FINGER_CONFIG`] in the VM, with fingerprint.sh's stack: the finger's
/// path (`STRAND_LOCK_VM_FINGER`), which the stack's reader waits 5 s for.
fn finger_vm(test: &str) -> Option<(Vm, PathBuf)> {
    if !in_lock_vm(test) {
        return None;
    }
    let finger = finger();
    let vm = Vm::with(test, "", FINGER_CONFIG);
    Some((vm, finger))
}

/// The finger fingerprint.sh's reader waits for.
fn finger() -> PathBuf {
    std::env::var_os("STRAND_LOCK_VM_FINGER")
        .map(PathBuf::from)
        .expect("run by scripts/lockvm/scenarios/fingerprint.sh, which names the finger")
}

impl Vm {
    /// The lock shows PAM's text: the reader waits.
    fn told(&self) {
        self.until_locked("HEADLESS-1", "PAM's text in the lock", true, |s| {
            near(s.corner(), LOCK_TOLD)
        });
    }

    fn no_fallback(&self) {
        let log = self.strand.log_text();
        assert!(
            !log.contains("lock: showing the built-in password field"),
            "the config's lock unlocked without the fallback:\n{log}"
        );
    }
}

/// The lock shows PAM's text before any key is pressed, and a finger on
/// the reader unlocks it.
#[test]
fn fingerprint_unlocks_the_configs_lock_with_no_password() {
    let Some((vm, finger)) = finger_vm("fp_finger") else {
        return;
    };
    vm.lock();
    vm.told();
    std::fs::write(&finger, "").unwrap();
    vm.until("HEADLESS-1", "unlocked by the finger", Shot::desktop);
    vm.no_fallback();
}

/// No finger: once the reader gives up the lock shows PAM's prompt; a
/// wrong password is refused, PAM starts over (the reader again), and
/// the right one unlocks.
#[test]
fn fingerprint_gives_up_and_the_lock_asks_for_the_password() {
    let Some((mut vm, _finger)) = finger_vm("fp_prompt") else {
        return;
    };
    vm.lock();
    vm.told();
    vm.until_locked("HEADLESS-1", "PAM's password prompt", true, |s| {
        near(s.corner(), LOCK_ASKED)
    });
    vm.lock_enter(WRONG);
    vm.until_locked("HEADLESS-1", "the wrong password refused", true, |s| {
        near(s.corner(), LOCK_REFUSED)
    });
    vm.lock_enter(PASSWORD);
    vm.until(
        "HEADLESS-1",
        "unlocked by the right password",
        Shot::desktop,
    );
    vm.no_fallback();
}

/// (lock-r2-1) The reader gave up and the lock was left waiting on the
/// password (an idle lock, the user away): a key wakes it, PAM begins
/// again, and a finger unlocks it.
#[test]
fn fingerprint_a_woken_lock_listens_to_the_reader_again() {
    let Some((mut vm, finger)) = finger_vm("fp_woken") else {
        return;
    };
    vm.lock();
    vm.told();
    vm.until_locked("HEADLESS-1", "PAM's password prompt", true, |s| {
        near(s.corner(), LOCK_ASKED)
    });
    vm.keys.type_text("a");
    std::fs::write(&finger, "").unwrap();
    vm.until("HEADLESS-1", "unlocked by the finger", Shot::desktop);
    vm.no_fallback();
}

/// A password typed while the reader waits is tried once it gives up.
#[test]
fn fingerprint_a_password_typed_while_the_reader_waits_unlocks_after_it() {
    let Some((mut vm, _finger)) = finger_vm("fp_early") else {
        return;
    };
    vm.lock();
    vm.told();
    vm.lock_enter(PASSWORD);
    vm.until("HEADLESS-1", "unlocked after the reader", Shot::desktop);
    vm.no_fallback();
}

/// The fallback's field checks passwords through the same stack: each
/// waits for the reader, then only the right one unlocks. (M6) While
/// the reader waits, PAM's text shows below the field (the check still
/// running: the field not refused), and the refusal's own text after.
#[test]
fn fingerprint_stack_in_the_fallback() {
    let Some(mut vm) = fallback_scenario("fp_fallback", "logic_panic", None) else {
        return;
    };
    finger();
    assert!(
        !vm.sway.shot("HEADLESS-1").text_below_field(),
        "no text before a check"
    );
    vm.keys.enter(WRONG);
    vm.until_locked(
        "HEADLESS-1",
        "PAM's text below the fallback's field while the reader waits",
        true,
        |s| near(s.corner(), FALLBACK_BG) && near(s.field(), FIELD) && s.text_below_field(),
    );
    vm.until_locked("HEADLESS-1", "the wrong password refused", true, |s| {
        near(s.corner(), FALLBACK_BG) && near(s.field(), FIELD_REFUSED)
    });
    vm.keys.enter(PASSWORD);
    vm.until(
        "HEADLESS-1",
        "unlocked by the right password",
        Shot::desktop,
    );
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

/// A healthy config's lock draws its first frame within 1 s, so the
/// first-frame deadline does not show the fallback (a wall-clock gate,
/// `GATE_MISS`, checked after the functional part: whichever field shows
/// takes the right password only). Three lock sessions.
#[test]
fn a_healthy_lock_draws_its_first_frame_in_time() {
    let test = "first_frame";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    for round in 0..3 {
        if round > 0 {
            vm.strand.cli(&["set", "lock.locked", "false"]);
        }
        vm.lock();
        let shot = vm.until_locked("HEADLESS-1", "a password field", true, |s| {
            near(s.corner(), LOCK_BG) || s.fallback()
        });
        if shot.fallback() {
            vm.fallback_passwords();
        } else {
            vm.lock_passwords();
        }
    }
    let frames = vm.first_frames();
    let log = vm.strand.log_text();
    let missed = log.contains(FIRST_FRAME_MISSED);
    if missed || frames.len() < 3 || frames.iter().any(|ms| *ms >= 1000) {
        panic!(
            "GATE_MISS lock first frame: first frames {frames:?} ms (deadline 1000 ms), \
             fallback shown for it: {missed}\n{log}"
        );
    }
    eprintln!("lock first frames: {frames:?} ms");
}

/// (gap 31) The lock's Caps Lock warning: [`CONFIG`] with `when
/// keyboard.caps_lock { bg: … }`. The lock surface has the keyboard, so
/// `keyboard.caps_lock` is its keyboard's own: Caps Lock locked on the
/// virtual keyboard shows the warning, the session locked throughout;
/// unlocked, the warning goes; the test password still unlocks.
#[test]
fn the_lock_warns_while_caps_lock_is_on() {
    let test = "caps_lock";
    if !in_lock_vm(test) {
        return;
    }
    const WARNING: [u8; 3] = [0xd0, 0xb0, 0x10];
    let typed = "  when secret != \"\" { bg: #20a050 }\n";
    assert!(CONFIG.contains(typed));
    let source = CONFIG.replace(
        typed,
        &format!("{typed}  when keyboard.caps_lock {{ bg: #d0b010 }}\n"),
    );
    let mut vm = Vm::with(test, "", &source);
    vm.lock();
    vm.content();
    vm.keys.lock_keys(CAPS_LOCK);
    vm.until_locked("HEADLESS-1", "the Caps Lock warning", true, |s| {
        near(s.corner(), WARNING)
    });
    vm.keys.lock_keys(0);
    vm.until_locked("HEADLESS-1", "the warning gone", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    vm.lock_passwords();
    let log = vm.strand.log_text();
    assert!(
        !log.contains("lock: showing the built-in password field"),
        "the config's lock, not the fallback:\n{log}"
    );
}

/// The config's lock refuses a wrong password and unlocks with the
/// right one, submitted by auth.schema's idiom (`on activate`) and by
/// the older `on key` form: no fallback shows for either.
#[test]
fn the_configs_lock_unlocks_through_activate_and_through_key() {
    let test = "lock_idioms";
    if !in_lock_vm(test) {
        return;
    }
    let on_key = config_on_key();
    assert!(on_key.contains("on key(k)") && !on_key.contains("on activate"));
    for source in [CONFIG.to_string(), on_key] {
        let mut vm = Vm::with(test, "", &source);
        vm.lock();
        vm.content();
        vm.lock_passwords();
        let log = vm.strand.log_text();
        assert!(
            !log.contains("lock: showing the built-in password field"),
            "the config's lock unlocked without the fallback:\n{log}"
        );
    }
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

/// A lock asked for while no output exists (lid closed, monitors gone),
/// its `lock` gone from render's tree before any output came back
/// (`lock_unmount_unseen`, as a reload while pending would): the content
/// strand-surface makes on the returning output carries a node this lock
/// session never attached and render does not know. It is still the
/// lock's content (`State::lock_content` says so): the fallback paints
/// it and takes the passwords, not a blank surface whose keys reach the
/// `Router`.
#[test]
fn lock_unmounted_with_no_output_then_an_output_keeps_the_fallback() {
    let test = "unmount_no_output";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "lock_unmount_unseen");
    vm.sway.unplug("HEADLESS-1");
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.strand.until_log("STRAND_FAULT lock_unmount_unseen");
    let output = vm.sway.plug();
    vm.until_locked(
        &output,
        "the built-in password field on the returning output",
        true,
        Shot::fallback,
    );
    vm.fallback_logged();
    vm.fallback_passwords_on(&output);
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
        shot.desktop_anywhere().is_none(),
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

/// A strand that starts with the marker and has its lock refused (another
/// locker holds the session) removes the marker and runs on unlocked:
/// later starts do not lock the session again.
#[test]
fn a_refused_restart_lock_removes_the_marker() {
    let test = "refused_restart";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    let marker = vm.strand.marker(&vm.sway.display.clone());
    vm.strand.signal(libc::SIGKILL);
    vm.strand.wait_exit("killed");
    // As a strand killed while locked leaves it.
    std::fs::write(&marker, b"locked\n").unwrap();
    let mut other = Other::lock(&vm.sway);
    other.pump(Duration::from_millis(300));
    vm.strand.run();
    let deadline = Instant::now() + WAIT;
    while marker.exists() {
        other.pump(Duration::from_millis(50));
        assert!(
            Instant::now() < deadline,
            "the marker stayed after a refused lock:\n{}",
            vm.strand.log_text()
        );
    }
    vm.log_has("locking again");
    vm.log_has("the compositor refused the session lock");
    assert!(vm.strand.running(), "strand runs on after a refusal");
    other.unlock(&vm.sway.dir);
    vm.until("HEADLESS-1", "the desktop back", Shot::desktop);
    // Started again, it does not lock. The CLI answering is the sign
    // the new strand is up: its logic thread (which serves the CLI)
    // starts after the lock is wired and the marker read, so a lock that
    // start asked for is asked for by then.
    vm.strand.signal(libc::SIGTERM);
    vm.strand.wait_exit("SIGTERM, unlocked");
    let before = vm.strand.log_text().len();
    vm.strand.run();
    let deadline = Instant::now() + WAIT;
    while !vm.strand.try_cli(&["set", "lock.locked", "false"]) {
        assert!(
            Instant::now() < deadline,
            "the restarted strand never answered:\n{}",
            vm.strand.log_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    vm.stays(
        "HEADLESS-1",
        "the desktop",
        Duration::from_millis(1000),
        Shot::desktop,
    );
    let log = vm.strand.log_text();
    let start = &log[before.min(log.len())..];
    assert!(
        !start.contains("locking again") && !start.contains("lock: first frame"),
        "a later start locked:\n{start}"
    );
    assert!(!marker.exists());
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

/// (m4-audit) strand dying after it asked for the lock but before the
/// compositor said `locked` (`abort_before_locked`: an abort as the
/// marker is written with the lock pending, as a crash on the lock's
/// first frame would) leaves the marker, so strand started again locks
/// at once with the fallback; a compositor that locked the session at
/// the request is not left with an abandoned lock and no field.
#[test]
fn aborted_before_locked_locks_again_on_restart() {
    let test = "abort_before_locked";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "abort_before_locked");
    let marker = vm.strand.marker(&vm.sway.display.clone());
    assert!(!marker.exists());
    // The process may abort before the CLI hears back.
    let _ = vm.strand.try_cli(&["set", "lock.locked", "true"]);
    vm.strand.wait_exit("aborted with the lock pending");
    vm.log_has("STRAND_FAULT abort_before_locked: the lock is pending");
    assert!(marker.exists(), "no marker for a pending lock");
    for (k, v) in vm.strand.env.iter_mut() {
        if k == "STRAND_FAULT" {
            *v = "".into();
        }
    }
    vm.strand.run();
    vm.fallback();
    vm.log_has("locking again");
    vm.fallback_passwords();
    vm.until("HEADLESS-1", "the marker removed", |_| !marker.exists());
}

/// (m4-audit) Under `STRAND_MOCK` the mock host has no `auth` store, so
/// a lock could never be unlocked or fall back: the run takes no session
/// lock at all. The config's lock opened stays a state change; the
/// desktop keeps showing and no marker is written.
#[test]
fn a_mocked_run_never_locks_the_session() {
    let test = "mocked";
    if !in_lock_vm(test) {
        return;
    }
    let sway = Sway::start(test);
    let display = sway.display.clone();
    let desktop = Desktop::start(&sway);
    let keys = Keyboard::new(&sway);
    let mut strand = Strand::prepare(&sway, &display, "", CONFIG);
    strand.env.push(("STRAND_MOCK".into(), "desktop".into()));
    strand.run();
    let mut vm = Vm {
        sway,
        _desktop: desktop,
        keys,
        strand,
    };
    vm.until("HEADLESS-1", "the desktop", Shot::desktop);
    vm.strand
        .until_log("STRAND_MOCK has no `auth` service, so this run never locks the session");
    let marker = vm.strand.marker(&display);
    vm.strand.cli(&["set", "lock.locked", "true"]);
    // The surface manager's answer to the open `lock`: not locking.
    vm.strand
        .until_log("the session lock is not enabled in this build");
    vm.stays(
        "HEADLESS-1",
        "the desktop (a mocked run locked)",
        Duration::from_millis(1000),
        Shot::desktop,
    );
    assert!(!marker.exists(), "a mocked run wrote the marker");
    assert!(vm.strand.running(), "the mocked run goes on");
}

/// (M4) The supervisor stand-in behaves as systemd does: under a unit
/// with no start limit of its own (systemd's default, 5 starts in 10 s)
/// a strand that dies at once is started five times and then given up
/// on, which would leave a locked session with no password field. The
/// documented unit lifts the limit (`StartLimitIntervalSec=0`) and backs
/// off to a bounded delay instead, and names strand by an absolute
/// path. Runs anywhere (no lock taken).
#[test]
fn the_supervisor_gives_up_as_systemd_does_unless_the_unit_lifts_the_limit() {
    let default = Supervisor::of_unit("[Service]\nRestart=on-failure\nRestartSec=0\n");
    assert_eq!(default.interval, Duration::from_secs(10));
    assert_eq!(default.burst, 5);
    // `false run <config>` fails at once, as a strand crashing at start.
    let out = default
        .command(std::ffi::OsStr::new("false"), Path::new("/nonexistent"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert_eq!(text.matches("strand exited (1)").count(), 5, "{text}");
    assert!(text.contains("start limit hit (5 starts"), "{text}");

    let documented = Supervisor::documented();
    assert_eq!(
        documented.interval,
        Duration::ZERO,
        "the documented unit sets StartLimitIntervalSec=0:\n{}",
        documented_unit()
    );
    let longest = documented.delays.iter().max().copied().unwrap_or_default();
    assert!(
        documented.delays.len() > 1 && longest <= Duration::from_secs(5),
        "it backs off, to a bounded delay: {documented:?}"
    );
    // (m4-audit) systemd looks a bare ExecStart name up on its own fixed
    // path, never ~/.cargo/bin where docs/development.md's `cargo install` puts strand:
    // the unit names the binary by an absolute path (`%h` is the home).
    let exec = documented_unit()
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
        .expect("the unit has an ExecStart");
    assert!(
        exec.starts_with('/') || exec.starts_with("%h/"),
        "ExecStart names strand by an absolute path: {exec}"
    );
}

/// (M4) The deployment architecture.md asks for: strand under a
/// supervisor that starts it again when it exits abnormally, with the
/// restart policy of the unit it documents ([`Supervisor::documented`],
/// read from architecture.md itself). Killed while locked (SIGKILL), and
/// killed again by an abort (SIGABRT) once back, the session never shows
/// the desktop: every shot from each kill until the supervisor's new
/// strand shows the built-in password field hides it. Then a crash loop:
/// six more deaths, each as soon as the new strand runs, more starts
/// than systemd's default limit allows in its 10 s (the supervisor gives
/// up under it, as the test above shows); under the documented unit the
/// session stays locked throughout and the field comes back. No test
/// step starts strand, and only the right password unlocks. The
/// supervisor and its strand run on after the unlock.
#[test]
fn a_supervised_strand_dying_while_locked_keeps_the_session_locked() {
    let test = "supervised";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::supervised(test);
    let display = vm.sway.display.clone();
    let marker = vm.strand.marker(&display);
    vm.lock();
    vm.content();
    vm.until("HEADLESS-1", "the restart marker written", |_| {
        marker.exists()
    });
    // Kills the supervised strand with `sig`; shots until the supervisor
    // runs a new one never show the desktop.
    let kill_and_wait_restart = |vm: &mut Vm, round: &str, sig: i32| {
        let pid = vm.strand.pid().expect("strand under its supervisor");
        // SAFETY: kill(2) on the supervisor's child.
        assert_eq!(unsafe { libc::kill(pid as i32, sig) }, 0, "kill {sig}");
        let deadline = Instant::now() + WAIT;
        loop {
            let shot = vm.sway.shot("HEADLESS-1");
            assert!(
                shot.desktop_anywhere().is_none(),
                "{round}: the desktop showed after signal {sig}: {}\n{}",
                shot.describe(),
                vm.strand.log_text()
            );
            if vm.strand.pid().is_some_and(|p| p != pid) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{round}: the supervisor never started strand again\n{}",
                vm.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    for (round, sig) in [libc::SIGKILL, libc::SIGABRT].into_iter().enumerate() {
        kill_and_wait_restart(&mut vm, &format!("round {round}"), sig);
        vm.fallback();
        assert!(marker.exists(), "round {round}: still locked, still marked");
    }
    // The crash loop.
    for (death, sig) in [libc::SIGKILL, libc::SIGABRT]
        .repeat(3)
        .into_iter()
        .enumerate()
    {
        kill_and_wait_restart(&mut vm, &format!("crash loop death {death}"), sig);
    }
    vm.fallback();
    assert!(
        marker.exists(),
        "after the crash loop: still locked, still marked"
    );
    let log = vm.strand.log_text();
    assert_eq!(
        log.matches("---- supervisor: strand exited (137)").count(),
        4,
        "{log}"
    );
    assert_eq!(
        log.matches("---- supervisor: strand exited (134)").count(),
        4,
        "{log}"
    );
    assert!(!log.contains("start limit hit"), "{log}");
    vm.log_has("locking again");
    assert_eq!(vm.strand.starts, 1, "only the supervisor started strand");
    vm.fallback_passwords();
    vm.until("HEADLESS-1", "the marker removed", |_| !marker.exists());
    assert!(vm.strand.running(), "the supervisor runs on");
    assert!(vm.strand.pid().is_some(), "and so does its strand");
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

/// (lock-int review) The compositor ends the lock while it plays its way
/// out (the proxy's `finished` once the exit has started, its pose 8 s
/// long so the cap is far): `finished` after `locked` fails closed then
/// too. The exit's token goes unspent and strand asks for a new lock,
/// which shows the fallback, and only a password typed there unlocks.
/// Before, strand sent the unlock on the finished object and asked for
/// nothing, so a compositor that kept the session locked left the user on
/// its own blank screen, and the restart marker went with the lock.
#[test]
fn finished_as_the_lock_plays_out_locks_again_with_the_fallback() {
    let test = "finished_exit";
    if !in_lock_vm(test) {
        return;
    }
    let sway = Sway::start("finished_exit");
    let proxy = proxy::Proxy::start(&sway.dir, "wayland-proxy", sway.socket());
    let source = exit_config("exit { opacity: 0 }", "ease(linear, 8s)");
    let mut vm = Vm::on(sway, &proxy.name(), "", &source);
    vm.lock();
    vm.content();
    vm.until_locked("HEADLESS-1", "the card", true, |s| near(s.card(), CARD));
    vm.keys.enter(PASSWORD);
    vm.until_log("the lock plays its way out");
    proxy.end();
    vm.until("HEADLESS-1", "the built-in password field", Shot::fallback);
    assert!(proxy.ended());
    vm.fallback_logged();
    vm.log_has("the compositor ended the session lock as it played its way out");
    vm.log_has("session lock: asking for a new lock");
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
    // Only a password unlocks: a config write of `false` is ignored. No
    // log line follows a closed spec while locked, so every shot for a
    // second hides the desktop (an unlock honouring it would be quick).
    vm.strand.cli(&["set", "lock.locked", "false"]);
    vm.stays(
        "HEADLESS-1",
        "the lock",
        Duration::from_millis(1000),
        |shot| near(shot.corner(), LOCK_BG),
    );
    vm.lock_passwords();
    assert!(vm.strand.running());
}

/// (M5, S-inspect) The inspector cannot pick while locked: pick mode
/// ends when a lock starts (its client hears `pick_ended`), and
/// `inspect.pick` is refused while the lock shows; after the unlock it
/// is served again.
#[test]
fn the_inspector_cannot_pick_while_locked() {
    use std::io::BufRead as _;
    let test = "inspect";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    let socket = strand_core::ipc::socket_path_from(
        None,
        Some(vm.sway.dir.clone().into()),
        Some(vm.sway.display.clone().into()),
    )
    .unwrap();
    let stream = UnixStream::connect(&socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = std::io::BufReader::new(stream);
    let mut line = || {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        serde_json::from_str::<serde_json::Value>(&l).unwrap()
    };
    let pick = |w: &mut UnixStream, on: bool| {
        w.write_all(
            strand_core::ipc::encode(&strand_core::ipc::Request::InspectPick { on }).as_bytes(),
        )
        .unwrap();
    };
    pick(&mut w, true);
    assert_eq!(line(), serde_json::json!({"ok": true}));
    vm.lock();
    vm.content();
    assert_eq!(line(), serde_json::json!({"event": "pick_ended"}));
    pick(&mut w, true);
    let refused = line();
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("locked")),
        "{refused}"
    );
    vm.lock_passwords();
    pick(&mut w, true);
    assert_eq!(line(), serde_json::json!({"ok": true}));
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

// ---- (report) the glitch report and the lock ----------------------------------

/// Every file under `dir`.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The records of `ring` strictly inside its lock gap, and whether the
/// gap was found closed.
fn inside_lock_gap(
    d: &strand_core::flight::Dump,
    begins: u8,
    ends: u8,
) -> (Vec<strand_core::flight::Rec>, bool) {
    use strand_core::flight::gap;
    let recs = &d.records;
    let Some(b) = recs
        .iter()
        .position(|r| r.kind == begins && r.u8(0) == gap::LOCK)
    else {
        return (Vec::new(), false);
    };
    let Some(e) = recs[b..]
        .iter()
        .position(|r| r.kind == ends && r.u8(0) == gap::LOCK)
    else {
        return (recs[b + 1..].to_vec(), false);
    };
    (recs[b + 1..b + e].to_vec(), true)
}

/// (report) A lock with both passwords typed into it, then unlocked:
/// each ring holds the lock as one gap with nothing inside it, no lock
/// surface or root is recorded, and neither password is in any file of
/// the report saved after (design.md, "Reporting a glitch": "Nothing at
/// all while the screen is locked, and nothing of the lock itself").
#[test]
fn nothing_is_recorded_while_locked() {
    use strand_core::flight::{self, kind};
    let test = "report_lock_gap";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    // Recording is on (the default) before the lock.
    vm.keys.type_text("ab");
    vm.lock();
    vm.content();
    vm.lock_passwords();
    let dir = vm.strand.report("after the lock");
    let input = flight::read(&std::fs::read(dir.join("input.flight")).unwrap()).unwrap();
    let (inside, closed) = inside_lock_gap(&input, kind::GAP_BEGINS, kind::GAP_ENDS);
    assert!(closed, "the input ring's lock gap: {:?}", input.records);
    assert!(inside.is_empty(), "recorded while locked: {inside:?}");
    assert!(
        !input.records.iter().any(|r| r.kind == kind::MAPPED),
        "a lock surface was recorded: {:?}",
        input.records
    );
    let changes = flight::read(&std::fs::read(dir.join("changes.flight")).unwrap()).unwrap();
    let (inside, closed) =
        inside_lock_gap(&changes, kind::CHANGE_GAP_BEGINS, kind::CHANGE_GAP_ENDS);
    assert!(closed, "the changes ring's lock gap: {:?}", changes.records);
    assert!(inside.is_empty(), "recorded while locked: {inside:?}");
    assert!(
        !changes.records.iter().any(|r| r.kind == kind::ROOT),
        "the lock's root was recorded"
    );
    let b: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("bundle.json")).unwrap()).unwrap();
    assert_eq!(b["surfaces"], serde_json::json!([]), "no lock surface: {b}");
    // (report review) Nothing of the lock itself: its cell (the field's
    // `secret`) is in neither cells.json nor the changes ring, not even
    // as a length.
    let cells: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("cells.json")).unwrap()).unwrap();
    assert!(
        !cells
            .as_object()
            .unwrap()
            .keys()
            .any(|k| k.ends_with(".secret")),
        "a lock cell in cells.json: {cells}"
    );
    assert!(
        cells.as_object().unwrap().contains_key("lock.locked"),
        "the config's own cells are kept: {cells}"
    );
    for e in changes.entries() {
        assert!(
            !e.name.as_deref().is_some_and(|n| n.ends_with(".secret")),
            "a lock cell recorded: {e:?}"
        );
    }
    let mut all = Vec::new();
    files_under(&dir, &mut all);
    for f in &all {
        let bytes = std::fs::read(f).unwrap();
        for s in [PASSWORD, WRONG] {
            let utf16: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert!(
                !bytes.windows(s.len()).any(|w| w == s.as_bytes())
                    && !bytes.windows(utf16.len()).any(|w| w == utf16),
                "{s:?} in {}",
                f.display()
            );
        }
    }
}

/// (report) `strand report` with words is refused while the session is
/// locked and saves nothing; bare `strand report` still answers; after
/// the unlock a report is saved.
#[test]
fn a_report_with_words_is_refused_while_locked() {
    let test = "report_refused";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::start(test, "");
    vm.lock();
    vm.content();
    let out = vm.strand.cli_out(&["report", "the", "lock", "flickered"]);
    assert!(!out.status.success(), "a report was saved while locked");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("the screen is locked"), "{err}");
    let bare = vm.strand.cli_out(&["report"]);
    assert!(
        bare.status.success(),
        "{}",
        String::from_utf8_lossy(&bare.stderr)
    );
    let reports = vm.strand.reports();
    let saved = std::fs::read_dir(&reports).map_or(0, |d| d.count());
    assert_eq!(saved, 0, "nothing under {}", reports.display());
    vm.lock_passwords();
    let dir = vm.strand.report("after the unlock");
    assert!(dir.join("bundle.json").is_file());
}

// ---- (lockmon) the lock on every monitor -------------------------------------------

/// (lockmon) [`CONFIG`] with each monitor's instance in its own shade:
/// `HEADLESS-1`'s the colours above, every other monitor's a little
/// redder ([`LOCK_BG2`] and the others), so a shot tells an instance
/// from the solid (the solid has the first content's colour). Its text
/// faults once `lock.zero` is 0 on every monitor but `HEADLESS-1`.
const MULTI: &str = r#"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: screen.name == "HEADLESS-1" ? #2050d0 : #3050d0
  when auth.failed { bg: screen.name == "HEADLESS-1" ? #a02040 : #b02040 }
  when secret != "" { bg: screen.name == "HEADLESS-1" ? #20a050 : #30a050 }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
  text pct(10 / (screen.name == "HEADLESS-1" ? 1 : zero))
}
"#;
const LOCK_BG2: [u8; 3] = [0x30, 0x50, 0xd0];
const LOCK_REFUSED2: [u8; 3] = [0xb0, 0x20, 0x40];
const LOCK_TYPED2: [u8; 3] = [0x30, 0xa0, 0x50];

/// How many times strand's log says the session unlocked.
fn unlocks(vm: &Vm) -> usize {
    vm.strand
        .log_text()
        .matches("session lock: Unlocked")
        .count()
}

/// (lockmon) The lock shows on every monitor, each instance its own
/// (its shade, not the solid's), with one password: a wrong one is
/// refused on both; half the right one typed, the pointer moved and
/// clicked onto the other monitor, and the rest typed is one password
/// that unlocks, once.
#[test]
fn the_lock_shows_on_every_monitor_with_one_password() {
    let test = "lockmon_every";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", MULTI);
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked(&second, "the second monitor's own lock", true, |s| {
        near(s.corner(), LOCK_BG2)
    });
    vm.keys.enter(WRONG);
    for (o, bg) in [
        ("HEADLESS-1", LOCK_REFUSED),
        (second.as_str(), LOCK_REFUSED2),
    ] {
        vm.until_locked(o, "the wrong password refused", true, |s| {
            near(s.corner(), bg)
        });
    }
    vm.keys.type_text(&PASSWORD[..7]);
    for (o, bg) in [("HEADLESS-1", LOCK_TYPED), (second.as_str(), LOCK_TYPED2)] {
        vm.until_locked(o, "the typed half on every monitor", true, |s| {
            near(s.corner(), bg)
        });
    }
    let mut pointer = support::pointer::Pointer::new(&vm.sway.socket());
    pointer.click(1280 + 640, 600, 2560, 720);
    pointer.motion(1280 + 900, 650, 2560, 720);
    vm.keys.type_text(&PASSWORD[7..]);
    vm.until_locked(&second, "still typed after the pointer moved", true, |s| {
        near(s.corner(), LOCK_TYPED2)
    });
    vm.keys.enter("");
    for o in ["HEADLESS-1", second.as_str()] {
        vm.until(o, "unlocked by the one password", Shot::desktop);
    }
    assert_eq!(unlocks(&vm), 1, "{}", vm.strand.log_text());
    let log = vm.strand.log_text();
    assert!(
        !log.contains("showing the built-in password field") && !log.contains("shows the solid"),
        "{log}"
    );
}

/// (lockmon) sway 1.9 keeps the keys on the lock surface it focused
/// first and moves them only when that surface goes: half the password
/// typed on `HEADLESS-1`'s instance, `HEADLESS-1` unplugged, the keys
/// move to the instance on the monitor plugged in while locked (which
/// got its own instance, not the solid), and the rest typed there is one
/// password with the first half.
#[test]
fn typing_survives_the_keys_moving_to_another_monitor() {
    let test = "lockmon_moves";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", MULTI);
    vm.lock();
    vm.content();
    let second = vm.sway.plug();
    vm.until_locked(&second, "the plugged monitor's own lock", true, |s| {
        near(s.corner(), LOCK_BG2)
    });
    vm.keys.type_text(&PASSWORD[..7]);
    vm.until_locked(&second, "the typed half there too", true, |s| {
        near(s.corner(), LOCK_TYPED2)
    });
    vm.sway.unplug("HEADLESS-1");
    vm.until_locked(&second, "still the second's own lock", true, |s| {
        near(s.corner(), LOCK_TYPED2)
    });
    vm.keys.enter(&PASSWORD[7..]);
    vm.until(&second, "unlocked by the one password", Shot::desktop);
    assert_eq!(unlocks(&vm), 1, "{}", vm.strand.log_text());
}

/// (oi-lock) Keys reach the password on whichever monitor has them, and
/// carry on when the keyboard moves mid-typing, with a field that has
/// no `focus: true` (Tab focuses it): Tab and the first letters on the
/// monitor sway gave the keys, `HEADLESS-1` unplugged (the keys stay or
/// move to the second), more letters, a third monitor plugged in while
/// locked, the second unplugged (the keys move to the third, for sure),
/// and the rest typed there is one password that unlocks, once. Before,
/// the keys entering another monitor's lock went to its surface, not
/// its field, and the rest of the password was lost.
#[test]
fn the_keys_carry_the_password_to_whichever_monitor_has_them() {
    let test = "oilock_keys";
    if !in_lock_vm(test) {
        return;
    }
    let config = MULTI.replace("; focus: true\n", "\n");
    assert_ne!(config, MULTI);
    let mut vm = Vm::with(test, "", &config);
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked(&second, "the second monitor's own lock", true, |s| {
        near(s.corner(), LOCK_BG2)
    });
    // No field has the keys yet: a letter goes nowhere.
    vm.keys.type_text("x");
    for (o, bg) in [("HEADLESS-1", LOCK_BG), (second.as_str(), LOCK_BG2)] {
        vm.stays(
            o,
            "a lock with nothing typed",
            Duration::from_millis(300),
            |s| near(s.corner(), bg),
        );
    }
    vm.keys.tab();
    vm.keys.type_text(&PASSWORD[..4]);
    for (o, bg) in [("HEADLESS-1", LOCK_TYPED), (second.as_str(), LOCK_TYPED2)] {
        vm.until_locked(o, "the first letters on every monitor", true, |s| {
            near(s.corner(), bg)
        });
    }
    vm.sway.unplug("HEADLESS-1");
    vm.until_locked(
        &second,
        "still typed after the first monitor went",
        true,
        |s| near(s.corner(), LOCK_TYPED2),
    );
    vm.keys.type_text(&PASSWORD[4..7]);
    let third = vm.sway.plug();
    vm.until_locked(&third, "the typed text on the plugged monitor", true, |s| {
        near(s.corner(), LOCK_TYPED2)
    });
    vm.sway.unplug(&second);
    vm.until_locked(
        &third,
        "still typed after the second monitor went",
        true,
        |s| near(s.corner(), LOCK_TYPED2),
    );
    vm.keys.enter(&PASSWORD[7..]);
    vm.until(&third, "unlocked by the one password", Shot::desktop);
    assert_eq!(unlocks(&vm), 1, "{}", vm.strand.log_text());
}

/// (lockmon) One monitor's instance faults (`10 / zero` there only): that
/// monitor shows the solid (the first content's colour, never the
/// desktop), the other keeps its lock, which still unlocks; nothing
/// falls back to the built-in field.
#[test]
fn a_faulting_monitors_lock_shows_the_solid() {
    let test = "lockmon_fault";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", MULTI);
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked(&second, "the second monitor's own lock", true, |s| {
        near(s.corner(), LOCK_BG2)
    });
    vm.strand.cli(&["set", "lock.zero", "0"]);
    vm.until_locked(&second, "the solid where the instance faulted", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    vm.until_log("one monitor's lock shows the solid");
    vm.stays(
        "HEADLESS-1",
        "the first monitor's lock",
        Duration::from_millis(500),
        |s| near(s.corner(), LOCK_BG) && s.desktop_anywhere().is_none(),
    );
    vm.lock_passwords();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    let log = vm.strand.log_text();
    assert!(
        !log.contains("showing the built-in password field"),
        "{log}"
    );
    assert_eq!(unlocks(&vm), 1, "{log}");
}

/// (lockmon review) A fault with no node of its own is still the lock's.
/// In the lock's `screens:` pick it freezes every monitor's instance: the
/// built-in field shows, and the right password unlocks (before, nothing
/// saw it and the frozen lock ignored every key).
#[test]
fn a_fault_in_the_locks_screens_shows_the_fallback() {
    let test = "lockmon_screens_fault";
    if !in_lock_vm(test) {
        return;
    }
    let config = CONFIG.replace(
        "  text pct(10 / zero)\n",
        "  screens: 10 / zero > 0 ? [\"HEADLESS-1\"] : [\"none\"]\n",
    );
    let mut vm = Vm::with(test, "", &config);
    vm.lock();
    vm.content();
    vm.strand.cli(&["set", "lock.zero", "0"]);
    vm.fallback();
    vm.log_has("a runtime fault froze the lock");
    vm.fallback_passwords();
    assert_eq!(unlocks(&vm), 1, "{}", vm.strand.log_text());
}

/// (lockmon review) A fault in an `if` of one monitor's instance (no
/// node of its own): that monitor shows the solid, the other's lock
/// carries on and unlocks.
#[test]
fn a_fault_in_one_monitors_if_shows_the_solid() {
    let test = "lockmon_if_fault";
    if !in_lock_vm(test) {
        return;
    }
    let config = MULTI.replace(
        "  text pct(10 / (screen.name == \"HEADLESS-1\" ? 1 : zero))\n",
        "  if 10 / (screen.name == \"HEADLESS-1\" ? 1 : zero) > 0 { text \"x\" }\n",
    );
    assert_ne!(config, MULTI);
    let mut vm = Vm::with(test, "", &config);
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked(&second, "the second monitor's own lock", true, |s| {
        near(s.corner(), LOCK_BG2)
    });
    vm.strand.cli(&["set", "lock.zero", "0"]);
    vm.until_log("one monitor's lock shows the solid");
    vm.until_locked(&second, "the solid where the instance faulted", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    vm.lock_passwords();
    let log = vm.strand.log_text();
    assert!(
        !log.contains("showing the built-in password field"),
        "{log}"
    );
    assert_eq!(unlocks(&vm), 1, "{log}");
}

/// strand's PSS (KiB), from `smaps_rollup`.
fn pss_kib(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// (lockmon review) What the lock's content on a second monitor costs,
/// a tripwire (the budgets catch regressions, never cap a config):
/// strand's PSS locked on two 1280×720 monitors with [`MULTI`]'s content
/// on both (the default) less the same with `screens: focused` (the
/// second a solid). Measured in the lock VM on 2026-10-25: 7,185 KiB
/// (57,900 less 50,715; about two 1280×720 shm buffers), so the gate is
/// 10 MiB (decisions.md lockmon review).
const SECOND_MONITOR_KIB: u64 = 10 * 1024;

#[test]
fn the_locks_second_monitor_costs_one_surface() {
    let test = "lockmon_memory";
    if !in_lock_vm(test) {
        return;
    }
    let measure = |tag: &str, source: &str| -> u64 {
        let mut vm = Vm::with(&format!("{test}_{tag}"), "", source);
        let second = vm.sway.plug();
        vm.until(&second, "the desktop on the second output", Shot::desktop);
        vm.lock();
        vm.content();
        let want = if tag == "every" { LOCK_BG2 } else { LOCK_BG };
        vm.until_locked(&second, "the second monitor's lock", true, |s| {
            near(s.corner(), want)
        });
        // Its frames and buffers settle.
        std::thread::sleep(Duration::from_secs(2));
        let pid = vm.strand.pid().expect("strand runs");
        let pss = pss_kib(pid);
        vm.lock_passwords();
        pss
    };
    let every = measure("every", MULTI);
    let focused = measure(
        "focused",
        &MULTI.replace(
            "  open: <-> locked\n",
            "  open: <-> locked\n  screens: focused\n",
        ),
    );
    let cost = every.saturating_sub(focused);
    eprintln!(
        "lock memory: every monitor {every} KiB, focused only {focused} KiB: \
         the second monitor's lock costs {cost} KiB"
    );
    assert!(every > 0 && focused > 0, "no PSS read");
    assert!(
        cost <= SECOND_MONITOR_KIB,
        "the second monitor's lock costs {cost} KiB, over its {SECOND_MONITOR_KIB} KiB tripwire"
    );
}

/// (lockmon) [`CONFIG`] with a still of its monitor in the lock, and
/// (oi-lock) a [`MARK`] square shown only where the still is `blank`.
fn still_config() -> String {
    CONFIG.replace(
        "  text pct(10 / zero)\n",
        "  text pct(10 / zero)\n  thumbnail screen { id: still; fit: cover; width: 320; height: 180 }\n  \
         box { width: 64; height: 64; bg: #c040c0; opacity: still.blank ? 1 : 0 }\n",
    )
}

/// (oi-lock) [`still_config`]'s square where a monitor has no still.
const MARK: [u8; 3] = [0xc0, 0x40, 0xc0];

impl Shot {
    /// Pixels of the desktop's colour in row `y`.
    fn desktop_in_row(&self, y: usize) -> usize {
        (0..self.w)
            .filter(|x| near(self.px(*x, y), DESKTOP))
            .count()
    }

    /// Pixels of the desktop's colour anywhere.
    fn desktop_pixels(&self) -> usize {
        self.pixels_of(DESKTOP)
    }

    /// Pixels of colour `c` anywhere.
    fn pixels_of(&self, c: [u8; 3]) -> usize {
        self.rgb
            .chunks_exact(3)
            .filter(|p| near([p[0], p[1], p[2]], c))
            .count()
    }
}

/// (lockmon) A `thumbnail screen` inside the lock shows a still of its
/// monitor from just before the lock covered it: the desktop's bar,
/// scaled into the thumbnail (a still taken after would show black or
/// the lock), on every monitor, while the session stays locked (the
/// bar's own row is the lock's, but where the thumbnail is). A monitor
/// plugged in while locked has no still: its thumbnail is empty.
///
/// (oi-lock) The plugged monitor's thumbnail draws nothing, so the
/// lock's own `bg` shows where it is, from the first shot of its lock on
/// (no desktop, no black or other block where the still would be), and
/// the config tells: `still.blank` shows [`MARK`] there and only there.
#[test]
fn the_locks_thumbnail_is_a_still_from_before_the_lock() {
    let test = "lockmon_still";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", &still_config());
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    // (lock-polish) Its still shows the desktop's bar from its first
    // frame: locked is the lock's colour, not the bar gone.
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until("HEADLESS-1", "locked", |s| near(s.corner(), LOCK_BG));
    for o in ["HEADLESS-1", second.as_str()] {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = vm.sway.shot(o);
            assert!(
                s.desktop_in_row(10) < s.w / 2,
                "{o}: the desktop shows while locked\n{}",
                vm.strand.log_text()
            );
            // The bar is 36 px of 720: 9 rows of 320 in the still.
            if near(s.corner(), LOCK_BG) && s.desktop_pixels() >= 320 * 6 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{o}: never the desktop's still in the lock ({} pixels): {}\n{}",
                s.desktop_pixels(),
                s.describe(),
                vm.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    vm.log_has("2 of 2 screen stills before the lock");
    // (oi-lock) Not blank where a still shows.
    for o in ["HEADLESS-1", second.as_str()] {
        vm.stays(
            o,
            "a lock with its still",
            Duration::from_millis(300),
            |s| s.pixels_of(MARK) == 0,
        );
    }
    let third = vm.sway.plug();
    // Every shot once its lock shows: the lock's colour where the still
    // would be (the 1280×720 lock holds nothing but its colour, the
    // field, a short text and the square: a 320×180 block of anything
    // else would be 57,600 pixels).
    let lock_only = |s: &Shot| {
        let other = (s.rgb.len() / 3)
            .saturating_sub(s.pixels_of(LOCK_BG))
            .saturating_sub(s.pixels_of(MARK));
        s.desktop_pixels() == 0 && other < 20_000
    };
    let first = vm.until_locked(&third, "the plugged monitor's lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    assert!(
        lock_only(&first),
        "{third}: its first lock frame is not the lock's colour where the still would be: {}\n{}",
        first.describe(),
        vm.strand.log_text()
    );
    // Every shot until the square shows is the lock's colour too: no
    // stale, black or garbage block in between.
    vm.until_locked(&third, "the square of a blank still", true, |s| {
        assert!(
            lock_only(s),
            "{third}: a lock frame is not the lock's colour where the still would be: {}\n{}",
            s.describe(),
            vm.strand.log_text()
        );
        s.pixels_of(MARK) >= 64 * 64 * 9 / 10
    });
    vm.stays(
        &third,
        "a lock with no still",
        Duration::from_millis(500),
        |s| lock_only(s) && s.pixels_of(MARK) >= 64 * 64 * 9 / 10,
    );
    vm.stays(
        "HEADLESS-1",
        "still not blank",
        Duration::from_millis(300),
        |s| s.pixels_of(MARK) == 0,
    );
    // The password (the still is the desktop's colour, so the waits do
    // not take it for the desktop).
    vm.keys.type_text(PASSWORD);
    vm.until("HEADLESS-1", "the typed password", |s| {
        near(s.corner(), LOCK_TYPED) && s.desktop_in_row(10) < s.w / 2
    });
    vm.keys.enter("");
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
    assert!(vm.sway.shot("HEADLESS-1").desktop_in_row(10) > 1200);
}

// ---- (lock-gpu) a lock presented by the GPU ----------------------------------------

/// (lock-gpu) A lock whose whole background springs between two colours
/// (`glow`), a large animation render promotes to the GPU. Its password
/// shows nothing, so its colour is only ever `glow`'s or a refusal's.
/// (lock-int review) As the owner's lock does, it clears the field when
/// `locked` turns false, so a write of `false` while the session stays
/// locked loses what was typed.
const GLOW_CONFIG: &str = r#"export state locked = false
export state zero = 1
export state glow = false
export state secret = ""
on change locked { if !locked { secret = "" } }
lock Gate {
  open: <-> locked
  bg: glow ? #d05020 : #2050d0 ~ 250ms
  when auth.failed { bg: #a02040 }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
  text pct(10 / zero)
}
"#;
/// [`GLOW_CONFIG`]'s lock with `glow` on.
const GLOW: [u8; 3] = [0xd0, 0x50, 0x20];

impl Vm {
    /// (lock-gpu) [`Vm::with`] with lavapipe, the lock VM's only Vulkan
    /// driver, accepted (`STRAND_GPU_SOFTWARE=1`) and each frame's
    /// `damage` line logged; the device up before the test goes on.
    fn gpu(tag: &str, faults: &str, source: &str) -> Vm {
        let sway = Sway::start(tag);
        let desktop = Desktop::start(&sway);
        let keys = Keyboard::new(&sway);
        let mut strand = Strand::prepare(&sway, &sway.display.clone(), faults, source);
        strand.env.retain(|(k, _)| k != "STRAND_LOG");
        strand.env.push(("STRAND_LOG".into(), "info,damage".into()));
        strand.env.push(("STRAND_GPU_SOFTWARE".into(), "1".into()));
        strand.run();
        let vm = Vm {
            sway,
            _desktop: desktop,
            keys,
            strand,
        };
        vm.until("HEADLESS-1", "the desktop", Shot::desktop);
        vm.until_log("GPU: the device is up");
        vm
    }

    /// (lock-gpu) Springs `glow` to `on` and waits for its colour, the
    /// session locked throughout.
    fn glow(&self, on: bool) {
        self.strand
            .cli(&["set", "lock.glow", if on { "true" } else { "false" }]);
        let want = if on { GLOW } else { LOCK_BG };
        self.until_locked("HEADLESS-1", "the lock's glow", true, |s| {
            near(s.corner(), want)
        });
    }

    /// (lock-gpu) Springs `glow` back and forth (`on` its last value)
    /// until the lock's content is attached for the GPU, and checks it is
    /// presented, never read back: its surface.
    fn until_presented(&self, on: &mut bool) -> u32 {
        let deadline = Instant::now() + WAIT;
        let (id, mode) = loop {
            *on = !*on;
            self.glow(*on);
            if let Some(a) = self.gpu_attached() {
                break a;
            }
            assert!(
                Instant::now() < deadline,
                "the lock was never promoted:\n{}",
                self.strand.log_text()
            );
        };
        assert_eq!(
            mode,
            "Present",
            "a lock is presented, never read back:\n{}",
            self.strand.log_text()
        );
        id
    }

    /// (lock-gpu) How many surfaces strand's log says it attached for the
    /// GPU.
    fn gpu_attaches(&self) -> usize {
        self.strand
            .log_text()
            .lines()
            .filter(|l| l.contains("GPU: surface ") && l.contains(" attached ("))
            .count()
    }

    /// (lock-gpu) The surface strand's log says it attached for the GPU,
    /// and in which mode, once it has.
    fn gpu_attached(&self) -> Option<(u32, String)> {
        self.strand.log_text().lines().find_map(|l| {
            let rest = l.split("GPU: surface ").nth(1)?;
            let (id, mode) = rest.split_once(" attached (")?;
            Some((id.parse().ok()?, mode.trim_end_matches(')').to_string()))
        })
    }
}

/// (lock-gpu) The `damage` lines of `surface` in `log`, in order: whether
/// the GPU thread presented each (`gpu=1`), and the CPU's paint (its
/// flatten and raster ms).
fn frames_of(log: &str, surface: u32) -> Vec<(bool, f64)> {
    let tag = format!("strand: damage surface={surface} ");
    log.lines()
        .filter_map(|l| l.split(&tag).nth(1))
        .map(|l| {
            let field = |k: &str| {
                l.split_whitespace()
                    .find_map(|f| f.strip_prefix(k))
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            (field("gpu=") == 1.0, field("flatten=") + field("raster="))
        })
        .collect()
}

/// (lock-gpu) The GPU thread's times of `surface`'s presented frames in
/// `log` (`gpu-frame` lines): their CPU part plus their GPU run, in ms.
fn gpu_frames_of(log: &str, surface: u32) -> Vec<f64> {
    let tag = format!("strand: gpu-frame surface={surface} ");
    log.lines()
        .filter_map(|l| l.split(&tag).nth(1))
        .map(|l| {
            l.split_whitespace()
                .filter_map(|f| f.split_once('='))
                .filter_map(|(_, v)| v.parse::<f64>().ok())
                .sum()
        })
        .collect()
}

/// Median and p95 of `v`, in its order of size.
fn spread(mut v: Vec<f64>) -> (f64, f64, usize) {
    if v.is_empty() {
        return (0.0, 0.0, 0);
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95), v.len())
}

/// (lock-gpu, OWNER RULING) A lock that animates its whole output is
/// presented by the GPU thread like a panel, never read back: its first
/// frame is the CPU's (the session counts as locked from it), and once the
/// GPU thread presents it, every frame of it is the GPU's (no CPU frame
/// in between: nothing alternates). The session stays locked throughout
/// and the password unlocks it. Prints the first frame's latency and the
/// frame times on each side.
#[test]
fn a_lock_that_animates_is_presented_and_never_alternates() {
    let test = "lock_presented";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::gpu(test, "", GLOW_CONFIG);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the config's lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    let mut on = false;
    let id = vm.until_presented(&mut on);
    for _ in 0..6 {
        on = !on;
        vm.glow(on);
    }
    let log = vm.strand.log_text();
    assert!(!log.contains("lock: the GPU failed"), "{log}");
    assert!(!log.contains("stays on the CPU"), "{log}");
    let frames = frames_of(&log, id);
    let first_gpu = frames
        .iter()
        .position(|(gpu, _)| *gpu)
        .unwrap_or_else(|| panic!("no frame of surface {id} was presented:\n{log}"));
    assert!(first_gpu > 0, "the first frame is the CPU's:\n{log}");
    let after = &frames[first_gpu..];
    assert!(
        after.iter().all(|(gpu, _)| *gpu),
        "a CPU frame of surface {id} after it was presented: {:?}\n{log}",
        frames.iter().map(|(g, _)| u8::from(*g)).collect::<Vec<_>>()
    );
    assert!(after.len() >= 6, "{} presented frames:\n{log}", after.len());
    let cpu = spread(frames[..first_gpu].iter().map(|(_, ms)| *ms).collect());
    let gpu = spread(gpu_frames_of(&log, id));
    eprintln!(
        "lock presented: first frames {:?} ms; CPU paints before it median {:.2} p95 {:.2} ms \
         ({} frames); presented frames median {:.2} p95 {:.2} ms ({} timed, {} presented)",
        vm.first_frames(),
        cpu.0,
        cpu.1,
        cpu.2,
        gpu.0,
        gpu.1,
        gpu.2,
        after.len()
    );
    vm.keys.enter(PASSWORD);
    vm.until(
        "HEADLESS-1",
        "unlocked by the right password",
        Shot::desktop,
    );
}

/// (lock-4k) A main thread held past the GPU bound while a lock frame is
/// in flight is no stall (`main_busy`: once the GPU has presented the
/// lock, the main thread sleeps twice the bound right after a pump sent
/// its frames, as painting two 3840 × 2160 stills held it on the owner's
/// desk). The GPU thread answered at once, so nothing is given up on: the
/// lock stays on the GPU, presenting, and the password unlocks it.
#[test]
fn a_busy_main_thread_is_no_gpu_stall() {
    let test = "lock_main_busy";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::gpu(test, "main_busy", GLOW_CONFIG);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the config's lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    // Springs until the GPU has presented the lock and the fault held
    // the main thread.
    let mut on = false;
    let deadline = Instant::now() + WAIT;
    while !vm.strand.log_text().contains("STRAND_FAULT main_busy") {
        on = !on;
        vm.strand
            .cli(&["set", "lock.glow", if on { "true" } else { "false" }]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            Instant::now() < deadline,
            "the fault never held the main thread:\n{}",
            vm.strand.log_text()
        );
    }
    // Frames after the hold.
    for _ in 0..4 {
        on = !on;
        vm.glow(on);
    }
    let log = vm.strand.log_text();
    assert!(
        !log.contains("answered nothing for"),
        "a busy main thread taken for a stalled GPU thread:\n{log}"
    );
    assert!(
        !log.contains("is drawn on the CPU from now on"),
        "the lock was renewed:\n{log}"
    );
    let (surface, mode) = vm.gpu_attached().expect("the lock went to the GPU");
    assert_eq!(mode, "Present", "{log}");
    let after = log
        .split("STRAND_FAULT main_busy")
        .nth(1)
        .unwrap_or_default();
    assert!(
        frames_of(after, surface).iter().any(|(gpu, _)| *gpu),
        "the lock goes on presenting after the hold:\n{log}"
    );
    vm.keys.enter(PASSWORD);
    vm.until("HEADLESS-1", "unlocked by the password", Shot::desktop);
}

/// (lock-gpu, OWNER RULING) The GPU thread stalls under a presented lock
/// (`gpu_stall`: it stops for good after the lock's first presented
/// frame). The lock never waits on it: within its bound the lock's
/// content is renewed, a fresh lock surface on its output drawn on the
/// CPU from then on, and the lock goes on animating. The session stays
/// locked throughout, the keyboard follows, and the password typed in
/// three parts (before the lock is presented, while the stalled thread
/// presents it, after the renewal) unlocks it. (lock-gpu review) The
/// next lock, on the same stalled thread, never goes to it: its content
/// animates on the CPU from its first frame, with nothing to renew.
#[test]
fn a_lock_whose_gpu_stalls_goes_on_on_the_cpu() {
    let test = "lock_gpu_stall";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::gpu(test, "gpu_stall", GLOW_CONFIG);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the config's lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    let (head, rest) = PASSWORD.split_at(4);
    let (mid, tail) = rest.split_at(4);
    vm.keys.type_text(head);
    // Springs until the GPU thread has presented the lock: the fault
    // stops the thread at its next reply.
    let mut on = false;
    let deadline = Instant::now() + WAIT;
    'armed: loop {
        on = !on;
        vm.strand
            .cli(&["set", "lock.glow", if on { "true" } else { "false" }]);
        for _ in 0..20 {
            if vm.strand.log_text().contains("STRAND_FAULT gpu_stall") {
                break 'armed;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            Instant::now() < deadline,
            "the lock was never presented:\n{}",
            vm.strand.log_text()
        );
    }
    // Typed while the stalled thread presents the lock, inside its bound.
    let armed = Instant::now();
    vm.keys.type_text(mid);
    let before = !vm
        .strand
        .log_text()
        .contains("is drawn on the CPU from now on");
    // Frames go on asking for the thread until the lock gives up on it;
    // the log is read every 10 ms, so the times below are that coarse.
    let mut asked = Instant::now();
    let (old, new, renewed) = loop {
        let log = vm.strand.log_text();
        if let Some((old, new)) = log.lines().find_map(|l| {
            let rest = l.split("lock: surface ").nth(1)?;
            let (old, rest) = rest.split_once(" is drawn on the CPU from now on, as surface ")?;
            Some((old.parse::<u32>().ok()?, rest.trim().parse::<u32>().ok()?))
        }) {
            break (old, new, Instant::now());
        }
        if asked.elapsed() >= Duration::from_millis(300) {
            on = !on;
            vm.strand
                .cli(&["set", "lock.glow", if on { "true" } else { "false" }]);
            asked = Instant::now();
        }
        assert!(
            Instant::now() < deadline,
            "the stalled lock was never renewed:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let drawn = format!("strand: damage surface={new} ");
    while !vm.strand.log_text().contains(&drawn) {
        assert!(
            Instant::now() < deadline,
            "the renewed surface never drew:\n{}",
            vm.strand.log_text()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let drew = renewed.elapsed();
    // (lock-int review) The renewal is no focus loss: the state the lock
    // is bound to stays true (the keyboard's leave of the old surface
    // closed the new one's lock, and the field was cleared), and nothing
    // wrote it false.
    std::thread::sleep(Duration::from_millis(300));
    let out = vm.strand.cli_out(&["get", "lock.locked"]);
    let locked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let log = vm.strand.log_text();
    assert_eq!(locked, "true", "`locked` across the renewal:\n{log}");
    assert!(
        !log.contains("`open` was set to false while the session is locked"),
        "something wrote the lock's `open` false:\n{log}"
    );
    eprintln!(
        "lock gpu stall: renewed {} ms after the fault armed, its first CPU frame {} ms after \
         that; the middle of the password was typed {} the renewal",
        renewed.duration_since(armed).as_millis(),
        drew.as_millis(),
        if before { "before" } else { "after" }
    );
    // (lock-4k) Given up on at the bound, 2 s of the thread's silence
    // (the test sees the fault's line up to a poll late, hence 1900),
    // and not much past it.
    let after = renewed.duration_since(armed);
    assert!(
        after >= Duration::from_millis(1900) && after < Duration::from_millis(2600),
        "renewed {after:?} after the fault armed, not at the 2 s bound:\n{log}"
    );
    let log = vm.strand.log_text();
    assert!(
        log.contains(&format!(
            "lock: the GPU failed surface {old} (its GPU thread stalled)"
        )),
        "{log}"
    );
    assert!(
        log.contains("no lock surface goes to it until it answers again"),
        "{log}"
    );
    assert_eq!(
        vm.gpu_attached(),
        Some((old, "Present".to_string())),
        "{log}"
    );
    // The lock goes on, on the CPU.
    for _ in 0..4 {
        on = !on;
        vm.glow(on);
    }
    let log = vm.strand.log_text();
    let frames = frames_of(&log, new);
    assert!(!frames.is_empty(), "the renewed surface drew:\n{log}");
    assert!(
        frames.iter().all(|(gpu, _)| !*gpu),
        "the renewed surface is the CPU's:\n{log}"
    );
    assert!(
        !log.contains(&format!("GPU: surface {new} attached")),
        "the renewed surface is never attached:\n{log}"
    );
    // The rest of the password, on the renewed surface.
    vm.keys.enter(tail);
    vm.until("HEADLESS-1", "unlocked by the password", Shot::desktop);
    // Locked again, the thread still stalled: the new lock's content
    // animates on the CPU and is never attached, so nothing waits a bound
    // or is renewed.
    let attaches = vm.gpu_attaches();
    vm.lock();
    vm.until_locked("HEADLESS-1", "the config's lock again", true, |s| {
        near(s.corner(), if on { GLOW } else { LOCK_BG })
    });
    for _ in 0..4 {
        on = !on;
        vm.glow(on);
    }
    let log = vm.strand.log_text();
    assert_eq!(
        vm.gpu_attaches(),
        attaches,
        "no lock content goes to the stalled thread:\n{log}"
    );
    assert_eq!(
        log.matches("is drawn on the CPU from now on").count(),
        1,
        "nothing to renew in the second lock:\n{log}"
    );
    vm.keys.enter(PASSWORD);
    vm.until("HEADLESS-1", "unlocked again", Shot::desktop);
}

/// (lock-gpu review) The built-in password field takes over a lock the
/// GPU thread presents, its thread healthy (a runtime fault in the lock,
/// `10 / zero`): only the main thread draws the field, so the content is
/// renewed on the CPU, its old surface released by the thread that still
/// presents it, and the field takes a wrong password and then the right
/// one, the session locked throughout.
#[test]
fn a_presented_lock_that_faults_shows_the_fallback_on_the_cpu() {
    let test = "lock_gpu_fallback";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::gpu(test, "", GLOW_CONFIG);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the config's lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    let mut on = false;
    let id = vm.until_presented(&mut on);
    on = !on;
    vm.glow(on);
    let log = vm.strand.log_text();
    assert!(
        frames_of(&log, id).iter().any(|(gpu, _)| *gpu),
        "presented before the fault:\n{log}"
    );
    vm.strand.cli(&["set", "lock.zero", "0"]);
    vm.fallback();
    vm.log_has("a runtime fault froze the lock");
    vm.log_has(&format!(
        "lock: the GPU failed surface {id} (the built-in password field shows)"
    ));
    vm.log_has(&format!(
        "lock: surface {id} is drawn on the CPU from now on"
    ));
    vm.until_log(&format!("GPU: surface {id} released"));
    let log = vm.strand.log_text();
    assert!(
        !log.contains("no lock surface goes to it"),
        "the thread is healthy:\n{log}"
    );
    vm.fallback_passwords();
}

/// (lock-gpu review) A lock that animates from its first frame, as the
/// owner's lock opens from the desktop: a full-output box springs from
/// opaque to clear as the lock opens, so it asks for the GPU from its
/// first frames. It is presented by the GPU thread while it opens, its
/// first frame the CPU's and none of the CPU's after the first presented
/// one. (On sway the first promotion comes after the first commit, so
/// this does not reach a refusal that passes; `run/gpu.rs`'s
/// `a_lock_promotion_is_refused_for_now_or_for_good` and session_lock.rs's
/// handed-off test pin that.)
const FADE_CONFIG: &str = r#"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: #2050d0
  box { width: 100%; height: 100%; bg: #d05020
    opacity: 0 ~ spring(120, 1)
    enter { opacity: 1 }
  }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
}
"#;

#[test]
fn a_lock_that_animates_from_its_first_frame_is_presented() {
    let test = "lock_fade_presented";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::gpu(test, "", FADE_CONFIG);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the faded-in lock", true, |s| {
        near(s.corner(), LOCK_BG)
    });
    let log = vm.strand.log_text();
    let (id, mode) = vm
        .gpu_attached()
        .unwrap_or_else(|| panic!("the opening lock was never promoted:\n{log}"));
    assert_eq!(mode, "Present", "{log}");
    assert!(!log.contains("stays on the CPU"), "{log}");
    let frames = frames_of(&log, id);
    let first_gpu = frames
        .iter()
        .position(|(gpu, _)| *gpu)
        .unwrap_or_else(|| panic!("no frame of surface {id} was presented:\n{log}"));
    assert!(first_gpu > 0, "the first frame is the CPU's:\n{log}");
    assert!(
        frames[first_gpu..].iter().all(|(gpu, _)| *gpu),
        "a CPU frame after it was presented: {:?}\n{log}",
        frames.iter().map(|(g, _)| u8::from(*g)).collect::<Vec<_>>()
    );
    // (lock-int review) Its clock was held across the hand-off, so the
    // fade goes on from the CPU's last frame rather than jumping over the
    // time the first presented frame took to come.
    let held = log
        .lines()
        .find_map(|l| {
            l.split(&format!("lock: surface {id}'s clock was held "))
                .nth(1)
        })
        .unwrap_or_else(|| panic!("no clock held across the hand-off:\n{log}"))
        .to_string();
    eprintln!(
        "lock fade: first frames {:?} ms; {} CPU frames, then {} presented; the clock held {}",
        vm.first_frames(),
        first_gpu,
        frames.len() - first_gpu,
        held.trim()
    );
    vm.keys.enter(PASSWORD);
    vm.until(
        "HEADLESS-1",
        "unlocked by the right password",
        Shot::desktop,
    );
}

/// (lock-polish) The lock's first frame already shows its still, so a
/// lock can open on the desktop exactly as it was: grim grabs HEADLESS-1
/// back to back from the lock request on, and the first shot that shows
/// the lock at all shows the desktop's bar in its thumbnail, as do the
/// shots after it (before, a few frames showed the lock's own colour
/// there and then cut to the still). strand's log says the still was fed
/// before that frame was drawn, which holds however the shots fall.
#[test]
fn the_locks_first_frame_shows_its_still() {
    let test = "lock_first_still";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", &still_config());
    vm.strand.cli(&["set", "lock.locked", "true"]);
    // The bar is 36 px of 720: 9 rows of 320 in the still; the real bar,
    // across the output, is hidden.
    let still = |s: &Shot| s.desktop_pixels() >= 320 * 6 && s.desktop_in_row(10) < s.w / 2;
    let deadline = Instant::now() + WAIT;
    let mut before = 0;
    let first = loop {
        let s = vm.sway.shot("HEADLESS-1");
        if near(s.corner(), LOCK_BG) {
            break s;
        }
        before += 1;
        assert!(
            Instant::now() < deadline,
            "never the lock: {}\n{}",
            s.describe(),
            vm.strand.log_text()
        );
    };
    assert!(
        still(&first),
        "the first shot of the lock ({before} shots before it) shows no still \
         ({} pixels of the desktop's colour): {}\n{}",
        first.desktop_pixels(),
        first.describe(),
        vm.strand.log_text()
    );
    for i in 0..8 {
        let s = vm.sway.shot("HEADLESS-1");
        assert!(
            still(&s) && near(s.corner(), LOCK_BG),
            "shot {i} after the first of the lock shows no still: {}\n{}",
            s.describe(),
            vm.strand.log_text()
        );
    }
    let log = vm.strand.log_text();
    let fed = log.find("first frame shows 1 of 1 screen stills");
    let drawn = log.find("lock: first frame after");
    assert!(
        matches!((fed, drawn), (Some(f), Some(d)) if f < d),
        "the still was not fed before the lock's first frame was drawn:\n{log}"
    );
    vm.log_has("1 of 1 screen stills before the lock");
    let took = log
        .lines()
        .find(|l| l.contains("screen stills before the lock in"))
        .unwrap_or_default();
    eprintln!(
        "lock first frame with a still: {:?} ms, {before} shots before the first of the lock \
         ({took})",
        vm.first_frames()
    );
    vm.keys.type_text(PASSWORD);
    vm.until("HEADLESS-1", "the typed password", |s| {
        near(s.corner(), LOCK_TYPED)
    });
    vm.keys.enter("");
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
}

/// (lock-polish) A `shadow:` inside the lock is paint: the lock fills its
/// output and the shadow is cut at its edge. Before, the shadows' room a
/// panel keeps outside its box inset the whole lock (black around it: 73
/// px left and right and 91 px top and bottom with the owner's card), and
/// the lock's first frame waited 500 ms (the first-frame wait) for a
/// configure at its content's size plus that room, which never comes.
#[test]
fn a_shadow_inside_the_lock_is_cut_at_its_edge() {
    let test = "lock_shadow";
    if !in_lock_vm(test) {
        return;
    }
    let source = CONFIG.replace(
        "  text pct(10 / zero)\n",
        "  text pct(10 / zero)\n  \
         box { width: 420; height: 268; bg: #e0e0e0; shadow: 0 8px 32px #00000080 }\n",
    );
    let mut vm = Vm::with(test, "", &source);
    vm.lock();
    vm.content();
    // Its last row and column, and its corners, are the lock's: nothing
    // of it is inset, and no black shows around it.
    let filled = |s: &Shot| {
        let (w, h) = (s.w, s.h);
        (0..w).all(|x| near(s.px(x, h - 1), LOCK_BG))
            && (0..h).all(|y| near(s.px(w - 1, y), LOCK_BG))
            && near(s.px(w - 1, 0), LOCK_BG)
            && near(s.px(0, h - 1), LOCK_BG)
    };
    let s = vm.sway.shot("HEADLESS-1");
    assert!(
        filled(&s),
        "the lock does not fill its output: {}, its bottom-left {:02x?}, top-right {:02x?}\n{}",
        s.describe(),
        s.px(0, s.h - 1),
        s.px(s.w - 1, 0),
        vm.strand.log_text()
    );
    // The card and its shadow show.
    assert!(
        s.pixels_of([0xe0, 0xe0, 0xe0]) >= 400 * 250,
        "no card: {}",
        s.describe()
    );
    vm.lock_passwords();
    let frames = vm.first_frames();
    eprintln!("lock first frame with a shadow: {frames:?} ms");
    // The wall clock last (CLAUDE.md): the first-frame wait the size
    // hold took was 500 ms.
    if frames.first().is_none_or(|ms| *ms >= 500) {
        panic!(
            "GATE_MISS lock first frame with a shadow: {frames:?} ms (under 500 ms: \
             no hold for a configure)\n{}",
            vm.strand.log_text()
        );
    }
}

// ---- (lock-polish) the lock plays its way out ------------------------------

/// The light card [`exit_config`] puts in the middle of the lock.
const CARD: [u8; 3] = [0xe0, 0xe0, 0xe0];

/// [`CONFIG`] with a 400 × 240 card on the lock that plays
/// `exit` (`exit_pose`) on the curve of its `opacity` (`curve`), and, on
/// the lock itself, every key that reaches it counted in `lock.typed`.
fn exit_config(exit_pose: &str, curve: &str) -> String {
    exit_config_on(CONFIG, exit_pose, curve)
}

/// [`exit_config`] on config `base`, a [`CONFIG`] with more in it.
fn exit_config_on(base: &str, exit_pose: &str, curve: &str) -> String {
    base.replace(
            "export state zero = 1\n",
            "export state zero = 1\nexport state typed = 0\n",
        )
        .replace(
            "  text pct(10 / zero)\n",
            &format!(
                "  text pct(10 / zero)\n  on key(k) {{ typed += 1 }}\n  \
                 box {{ width: 400; height: 240; bg: #e0e0e0; opacity: 1 ~ {curve}; {exit_pose} }}\n"
            ),
        )
}

impl Shot {
    /// The middle of [`exit_config`]'s card (the lock stacks its content
    /// at its top left; the card is drawn over the field and the text).
    fn card(&self) -> [u8; 3] {
        self.px(200, 120)
    }
}

impl Vm {
    /// [`exit_config`]'s lock, locked with its card showing and the
    /// password typed (not sent).
    fn exit_lock(test: &str, exit_pose: &str, curve: &str) -> Vm {
        Vm::exit_lock_with(test, "", exit_pose, curve)
    }

    /// [`Vm::exit_lock`] under `faults`.
    fn exit_lock_with(test: &str, faults: &str, exit_pose: &str, curve: &str) -> Vm {
        let mut vm = Vm::with(test, faults, &exit_config(exit_pose, curve));
        vm.lock();
        vm.content();
        vm.until_locked("HEADLESS-1", "the card", true, |s| near(s.card(), CARD));
        vm.keys.type_text(PASSWORD);
        vm.until_locked("HEADLESS-1", "the typed password", true, |s| {
            near(s.corner(), LOCK_TYPED) && near(s.card(), CARD)
        });
        vm
    }

    /// `lock.typed`: the keys the lock took.
    fn typed(&self) -> u64 {
        let out = self.strand.cli_out(&["get", "lock.typed"]);
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        got.trim()
            .parse()
            .unwrap_or_else(|_| panic!("lock.typed: {got:?}"))
    }
}

/// (lock-polish) The lock plays its way out before the session unlocks:
/// grim grabs HEADLESS-1 back to back from the Return that sends the right
/// password. Shots in the middle of the card's exit show it part way out,
/// neither as it was in the last locked frame nor gone, and no shot shows
/// the desktop until the exit has ended: strand logs the exit's end before
/// it unlocks. Before, the lock went the moment the password was accepted,
/// and no shot showed the card fading.
#[test]
fn the_lock_plays_its_way_out_before_it_unlocks() {
    let test = "lock_exit";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock(test, "enter { opacity: 0 }", "ease(linear, 600ms)");
    let last = vm.sway.shot("HEADLESS-1");
    vm.keys.enter("");
    let deadline = Instant::now() + WAIT;
    let mut shots = Vec::new();
    let end = loop {
        let s = vm.sway.shot("HEADLESS-1");
        if s.desktop() {
            break s;
        }
        if let Some((x, y)) = s.desktop_anywhere() {
            panic!(
                "the desktop showed at {x},{y} before the unlock: {}\n{}",
                s.describe(),
                vm.strand.log_text()
            );
        }
        shots.push(s);
        assert!(
            Instant::now() < deadline,
            "never unlocked\n{}",
            vm.strand.log_text()
        );
    };
    let cards: Vec<[u8; 3]> = shots.iter().map(Shot::card).collect();
    let mid = shots
        .iter()
        .filter(|s| {
            let c = s.card();
            !near(c, last.card()) && !near(c, LOCK_BG) && !near(c, end.card())
        })
        .count();
    assert!(
        mid >= 1,
        "no shot caught the card part way out ({} shots, the card in each {cards:02x?}; \
         the last locked frame's {:02x?}, the desktop's {:02x?})\n{}",
        shots.len(),
        last.card(),
        end.card(),
        vm.strand.log_text()
    );
    let log = vm.strand.log_text();
    let ended = log.find("lock: the exit ended after ");
    let unlocked = log.find("session lock: Unlocked");
    assert!(
        matches!((ended, unlocked), (Some(e), Some(u)) if e < u),
        "the unlock was not sent after the exit ended:\n{log}"
    );
    assert!(
        !log.contains("ms cap: unlocking"),
        "the exit ended by itself, not at the cap:\n{log}"
    );
    let took = log
        .lines()
        .find_map(|l| l.split("lock: the exit ended after ").nth(1))
        .unwrap_or_default();
    eprintln!(
        "lock exit: {} shots while it played, {mid} part way out, the card in each \
         {cards:02x?}; it ended after {took}",
        shots.len()
    );
    // Locked again, it mounts its content anew and plays in again.
    vm.lock();
    vm.content();
    vm.until_locked("HEADLESS-1", "the card again", true, |s| {
        near(s.card(), CARD)
    });
    vm.lock_enter(PASSWORD);
    vm.until("HEADLESS-1", "unlocked again", Shot::desktop);
}

/// (lock-polish) The exit is capped: a card whose exit takes 8 s holds
/// the unlock one second (`LOCK_EXIT_CAP`), which strand-surface's own
/// timer enforces, and strand says so. The desktop shows a second or so
/// after the Return, not before (the exit held it) and not after the 8 s
/// pose (render's own end).
#[test]
fn the_exit_cap_unlocks_an_exit_that_does_not_settle() {
    let test = "lock_exit_cap";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock(test, "exit { opacity: 0 }", "ease(linear, 8s)");
    let at = Instant::now();
    vm.keys.enter("");
    vm.until("HEADLESS-1", "unlocked at the cap", Shot::desktop);
    let took = at.elapsed();
    let log = vm.strand.log_text();
    assert!(
        log.contains("lock: the exit was still playing at its 1000 ms cap: unlocking"),
        "the cap did not unlock:\n{log}"
    );
    assert!(
        took >= Duration::from_millis(900),
        "unlocked {took:?} after the Return, before the cap: the exit held nothing\n{log}"
    );
    eprintln!("lock exit cap: unlocked {took:?} after the Return");
    if took >= Duration::from_secs(6) {
        panic!(
            "GATE_MISS lock exit cap: unlocked {took:?} after the Return (the cap is 1 s, the \
             pose 8 s)\n{log}"
        );
    }
}

/// (lock-int, second review) The GPU thread stalls under a presented lock
/// as it plays its way out (`gpu_stall_exit`: it stops for good after the
/// first frame it presents once the password was accepted), the card's
/// exit 8 s long, past the cap. The session stays locked until the
/// unlock: no shot grabbed back to back from the Return shows the desktop
/// before it. (lock-4k) The stall's bound (2 s) is past the exit's cap
/// (1 s), so nothing is renewed while it plays out: the unlock comes at
/// the cap, as for any exit that never finishes. (Before, with a 500 ms
/// bound, the renewal landed inside the exit and ended it there.)
#[test]
fn a_lock_whose_gpu_stalls_as_it_plays_out_stays_locked_until_it_unlocks() {
    let test = "lock_gpu_stall_exit";
    if !in_lock_vm(test) {
        return;
    }
    let source = exit_config_on(GLOW_CONFIG, "exit { opacity: 0 }", "ease(linear, 8s)");
    let mut vm = Vm::gpu(test, "gpu_stall_exit", &source);
    vm.lock();
    vm.until_locked("HEADLESS-1", "the card", true, |s| near(s.card(), CARD));
    let mut on = false;
    let old = vm.until_presented(&mut on);
    vm.keys.type_text(PASSWORD);
    let at = Instant::now();
    vm.keys.enter("");
    let deadline = Instant::now() + WAIT;
    let mut shots = 0;
    loop {
        let s = vm.sway.shot("HEADLESS-1");
        if s.desktop() {
            break;
        }
        if let Some((x, y)) = s.desktop_anywhere() {
            panic!(
                "the desktop showed at {x},{y} before the unlock: {}\n{}",
                s.describe(),
                vm.strand.log_text()
            );
        }
        shots += 1;
        assert!(
            Instant::now() < deadline,
            "never unlocked\n{}",
            vm.strand.log_text()
        );
    }
    let took = at.elapsed();
    let log = vm.strand.log_text();
    let exit = log.find("the lock plays its way out");
    let armed = log.find("STRAND_FAULT gpu_stall_exit");
    let unlocked = log.find("session lock: Unlocked");
    assert!(
        matches!(
            (exit, armed, unlocked),
            (Some(e), Some(a), Some(u)) if e < a && a < u
        ),
        "the stall did not come inside the exit (exit {exit:?}, armed {armed:?}, unlocked \
         {unlocked:?}):\n{log}"
    );
    let before = &log[..unlocked.unwrap_or(log.len())];
    assert!(
        !before.contains(&format!(
            "lock: surface {old} is drawn on the CPU from now on"
        )) && !before.contains("answered nothing for"),
        "renewed inside the exit, before the bound:\n{log}"
    );
    assert!(
        log.contains("lock: the exit was still playing at its 1000 ms cap: unlocking"),
        "the cap did not unlock:\n{log}"
    );
    assert!(
        took >= Duration::from_millis(900),
        "unlocked {took:?} after the Return, before the cap\n{log}"
    );
    eprintln!(
        "lock gpu stall in the exit: {shots} shots locked, unlocked {took:?} after the Return"
    );
}

/// (lock-polish) Keys typed while the lock plays its way out go nowhere:
/// strand-surface drops them, so the lock's `on key` (which counted the
/// password's) hears none of them: none reaches the field, so none is
/// sent to `auth` or shows. The keys are typed once strand logs the exit's start and before
/// it unlocks, and the desktop is not shown until after them.
#[test]
fn keys_typed_while_the_lock_plays_out_go_nowhere() {
    let test = "lock_exit_keys";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock(test, "exit { opacity: 0 }", "ease(linear, 8s)");
    let before = vm.typed();
    assert!(
        before >= PASSWORD.len() as u64,
        "the lock counts the keys it takes ({before})"
    );
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    vm.keys.enter("abcd");
    let log = vm.strand.log_text();
    assert!(
        !log.contains("session lock: Unlocked"),
        "unlocked before the keys were typed: they would prove nothing\n{log}"
    );
    let s = vm.sway.shot("HEADLESS-1");
    assert!(
        s.desktop_anywhere().is_none() && !near(s.corner(), LOCK_TYPED),
        "the keys show, or the desktop does: {}",
        s.describe()
    );
    vm.until("HEADLESS-1", "unlocked at the cap", Shot::desktop);
    let after = vm.typed();
    eprintln!("lock exit keys: the lock took {before} keys before the Return, {after} after");
    assert!(
        after <= before + 1,
        "the lock took keys typed as it played out: {before} before the Return, {after} after\n{}",
        vm.strand.log_text()
    );
}

/// (lock-polish) The lock asked for again while it plays its way out
/// stays locked: strand-surface drops the token and the session never
/// unlocks, well past the exit's cap, until the password is typed again.
#[test]
fn a_lock_asked_for_again_as_it_plays_out_stays_locked() {
    let test = "lock_exit_again";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock(test, "exit { opacity: 0 }", "ease(linear, 8s)");
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until_log("asked for again while it played its way out: the session stays locked");
    // Twice the cap, and the session is still locked.
    vm.stays("HEADLESS-1", "locked", Duration::from_secs(2), |s| {
        s.desktop_anywhere().is_none()
    });
    let log = vm.strand.log_text();
    assert!(
        !log.contains("ms cap: unlocking") && !log.contains("session lock: Unlocked"),
        "unlocked after all:\n{log}"
    );
    vm.until_locked("HEADLESS-1", "the card again", true, |s| {
        near(s.card(), CARD)
    });
    vm.lock_enter(PASSWORD);
    vm.until(
        "HEADLESS-1",
        "unlocked by the password typed again",
        Shot::desktop,
    );
}

/// (lock-polish) A monitor plugged in while the lock plays its way out is
/// covered by the lock's colour (the solid: nothing plays out on it), and
/// never shows the desktop before strand sends the unlock at the cap.
#[test]
fn an_output_plugged_in_as_the_lock_plays_out_shows_its_colour() {
    let test = "lock_exit_plug";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock(test, "exit { opacity: 0 }", "ease(linear, 8s)");
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    let second = vm.sway.plug();
    let deadline = Instant::now() + WAIT;
    let mut covered = 0;
    loop {
        let log = vm.strand.log_text();
        let s = vm.sway.shot(&second);
        if log.contains("session lock: Unlocked") {
            break;
        }
        if let Some((x, y)) = s.desktop_anywhere() {
            let log = vm.strand.log_text();
            assert!(
                log.contains("ms cap: unlocking"),
                "{second} showed the desktop at {x},{y} before the unlock: {}\n{log}",
                s.describe()
            );
            break;
        }
        if near(s.corner(), LOCK_BG) {
            covered += 1;
        }
        assert!(Instant::now() < deadline, "never unlocked\n{log}");
    }
    eprintln!("lock exit, plugged in: {covered} shots of {second} in the lock's colour");
    assert!(
        covered >= 1,
        "{second} never showed the lock's colour before the unlock\n{}",
        vm.strand.log_text()
    );
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
}

/// (lock-polish review) A logic too slow to close the lock before the
/// exit's cap (`logic_slow_unlocking` holds `Unlocking` 1.5 s) leaves the
/// lock open, not leaving, when the cap unlocks. That open lock is not a
/// new request (an unlock with no exit spends it too): the session
/// unlocks at the cap and stays unlocked, the password just accepted
/// never asked for again, by the config's lock or the built-in field.
/// Before, it was taken as the lock asked for again and locked at once,
/// and the guard kept the old lock's start, so the built-in field took
/// over for "no first frame".
#[test]
fn a_slow_logic_at_the_cap_does_not_lock_again() {
    let test = "lock_exit_slow_logic";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::exit_lock_with(
        test,
        "logic_slow_unlocking",
        "exit { opacity: 0 }",
        "ease(linear, 600ms)",
    );
    let at = Instant::now();
    vm.keys.enter("");
    vm.until("HEADLESS-1", "unlocked at the cap", Shot::desktop);
    let took = at.elapsed();
    vm.until_log("STRAND_FAULT logic_slow_unlocking");
    // Past logic's hold and a first-frame deadline: still the desktop.
    vm.stays(
        "HEADLESS-1",
        "unlocked",
        Duration::from_secs(3),
        Shot::desktop,
    );
    let log = vm.strand.log_text();
    assert!(
        log.contains("ms cap: unlocking"),
        "the cap did not unlock:\n{log}"
    );
    assert_eq!(
        log.matches("session lock: Locked").count(),
        1,
        "locked again after the unlock:\n{log}"
    );
    assert!(
        !log.contains("showing the built-in password field"),
        "the built-in field showed:\n{log}"
    );
    eprintln!("lock exit, slow logic: unlocked {took:?} after the Return, and stayed so");
    // The next lock is the config's, and unlocks.
    vm.lock();
    vm.content();
    vm.lock_enter(PASSWORD);
    vm.until("HEADLESS-1", "unlocked again", Shot::desktop);
}

/// (lock-polish review) The still of the desktop in the lock, with no
/// pose of its own, stays drawn while the lock plays its way out (a card
/// fading out on `exit`), so the last lock frame can be the desktop as it
/// was: every shot from the Return to the unlock shows the still, and
/// one shows the card part way out. The card is under the still, so it
/// is read where the still does not cover it.
#[test]
fn the_locks_still_stays_drawn_as_it_plays_out() {
    let test = "lock_exit_still";
    if !in_lock_vm(test) {
        return;
    }
    let config = exit_config_on(
        &still_config(),
        "exit { opacity: 0 }",
        "ease(linear, 600ms)",
    );
    let mut vm = Vm::with(test, "", &config);
    let card = |s: &Shot| s.px(360, 200);
    let still = |s: &Shot| s.desktop_pixels() >= 320 * 6 && s.desktop_in_row(10) < s.w / 2;
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until("HEADLESS-1", "the card and the still", |s| {
        near(s.corner(), LOCK_BG) && near(card(s), CARD) && still(s)
    });
    vm.keys.type_text(PASSWORD);
    vm.until("HEADLESS-1", "the typed password", |s| {
        near(s.corner(), LOCK_TYPED)
    });
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    let deadline = Instant::now() + WAIT;
    let mut shots = Vec::new();
    loop {
        let s = vm.sway.shot("HEADLESS-1");
        if vm.strand.log_text().contains("session lock: Unlocked") {
            break;
        }
        // The desktop itself, not its still, is the unlock.
        if s.desktop() {
            break;
        }
        assert!(
            still(&s),
            "a shot as the lock played out lost the still ({} desktop pixels): {}\n{}",
            s.desktop_pixels(),
            s.describe(),
            vm.strand.log_text()
        );
        shots.push(card(&s));
        assert!(
            Instant::now() < deadline,
            "never unlocked\n{}",
            vm.strand.log_text()
        );
    }
    let mid = shots
        .iter()
        .filter(|c| !near(**c, CARD) && !near(**c, LOCK_BG) && !near(**c, LOCK_TYPED))
        .count();
    eprintln!(
        "lock exit, still: {} shots with the still as it played out, {mid} with the card \
         part way out ({shots:02x?})",
        shots.len()
    );
    assert!(
        mid >= 1,
        "no shot caught the card part way out: {shots:02x?}\n{}",
        vm.strand.log_text()
    );
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
}

/// (lock-int) Between the exit's end and the unlock the lock keeps the
/// exit's last frame on screen: the still, with no pose of its own, and
/// the card gone. `lock_slow_close` hands render's report that the lock
/// closed to strand-surface 300 ms late (a busy main loop), so whatever
/// the lock draws in that gap stays up long enough for grim: every shot
/// from the Return to the unlock shows the still, none the lock's `bg`
/// alone, and the shots in the hold show the card gone and nothing
/// changing. Before, render drew the lock with nothing kept in it the
/// frame after it closed, and that frame could show for a moment before
/// the unlock landed (the owner's "blip to dark"): here it would show for
/// the whole hold. The unlock still comes from the exit, not the cap: a
/// 150 ms exit and the hold leave half the cap spare.
#[test]
fn the_locks_last_frame_stays_until_the_unlock() {
    last_frame_stays("lock_exit_settled", false);
}

/// (lock-int review) [`the_locks_last_frame_stays_until_the_unlock`] with
/// the lock presented by the GPU thread (lavapipe): the freeze holds on the
/// GPU's paint and present path too.
#[test]
fn the_locks_last_frame_stays_until_the_unlock_on_the_gpu() {
    last_frame_stays("lock_exit_settled_gpu", true);
}

fn last_frame_stays(test: &str, gpu: bool) {
    if !in_lock_vm(test) {
        return;
    }
    let base = if gpu { GLOW_CONFIG } else { CONFIG };
    let base = base.replace(
        "  text pct(10 / zero)\n",
        "  text pct(10 / zero)\n  thumbnail screen { id: still; fit: cover; width: 320; height: 180 }\n",
    );
    let config = exit_config_on(&base, "exit { opacity: 0 }", "ease(linear, 150ms)");
    let mut vm = if gpu {
        Vm::gpu(test, "lock_slow_close", &config)
    } else {
        Vm::with(test, "lock_slow_close", &config)
    };
    let card = |s: &Shot| s.px(360, 200);
    let still = |s: &Shot| s.desktop_pixels() >= 320 * 6 && s.desktop_in_row(10) < s.w / 2;
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until("HEADLESS-1", "the card and the still", |s| {
        near(card(s), CARD) && still(s)
    });
    if gpu {
        // The glow springs until the lock is promoted (`until_presented`
        // takes the still's desktop pixels for the desktop).
        let deadline = Instant::now() + WAIT;
        let mut on = false;
        while vm.gpu_attached().is_none() {
            on = !on;
            vm.strand
                .cli(&["set", "lock.glow", if on { "true" } else { "false" }]);
            std::thread::sleep(Duration::from_millis(400));
            assert!(
                Instant::now() < deadline,
                "the lock was never promoted:\n{}",
                vm.strand.log_text()
            );
        }
        if on {
            vm.strand.cli(&["set", "lock.glow", "false"]);
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    vm.keys.type_text(PASSWORD);
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    let deadline = Instant::now() + WAIT;
    let (mut shots, mut in_hold) = (0, Vec::new());
    loop {
        let s = vm.sway.shot("HEADLESS-1");
        let log = vm.strand.log_text();
        if log.contains("session lock: Unlocked") || s.desktop() {
            break;
        }
        assert!(
            still(&s),
            "a shot before the unlock lost the still ({} desktop pixels, {} of the lock's bg): {}\n{log}",
            s.desktop_pixels(),
            s.pixels_of(LOCK_BG),
            s.describe(),
        );
        shots += 1;
        if log.contains("STRAND_FAULT lock_slow_close") {
            in_hold.push(card(&s));
        }
        assert!(Instant::now() < deadline, "never unlocked\n{log}");
    }
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
    let log = vm.strand.log_text();
    eprintln!(
        "lock exit, settled{}: {shots} shots before the unlock, {} in the hold ({:02x?})",
        if gpu { ", GPU" } else { "" },
        in_hold.len(),
        in_hold
    );
    assert!(
        in_hold.len() >= 2,
        "too few shots in the hold to tell ({} of {shots})\n{log}",
        in_hold.len()
    );
    // The first shot in the hold may have been grabbed as the hold began,
    // with the exit's last frame on its way: from the second on, the end.
    let end = in_hold[1];
    assert!(
        !near(end, CARD) && in_hold[1..].iter().all(|c| near(*c, end)),
        "the card is not gone, or the lock changed, in the hold: {in_hold:02x?}\n{log}"
    );
    assert!(
        log.contains("lock: the exit ended after ") && !log.contains("ms cap: unlocking"),
        "the exit did not end by itself:\n{log}"
    );
    if gpu {
        assert!(
            vm.gpu_attached().is_some_and(|(_, m)| m == "Present"),
            "the lock was not presented:\n{log}"
        );
    }
}

/// (lock-polish review) A `screens: focused` lock whose monitor is
/// unplugged as it plays its way out: its spec stays, but its content
/// went with the output, so nothing is left to play and the exit ends
/// then, not at the cap. Before, nothing looked again until a spec
/// changed, and the unlock waited for the cap.
#[test]
fn an_exit_whose_output_goes_ends_then() {
    let test = "lock_exit_unplug";
    if !in_lock_vm(test) {
        return;
    }
    let config = exit_config_on(
        &CONFIG.replace(
            "  open: <-> locked\n",
            "  open: <-> locked\n  screens: focused\n",
        ),
        "exit { opacity: 0 }",
        "ease(linear, 8s)",
    );
    let mut vm = Vm::with(test, "", &config);
    let second = vm.sway.plug();
    vm.until(&second, "the desktop on the second output", Shot::desktop);
    vm.lock();
    vm.content();
    vm.until_locked("HEADLESS-1", "the card", true, |s| near(s.card(), CARD));
    vm.keys.type_text(PASSWORD);
    vm.until_locked("HEADLESS-1", "the typed password", true, |s| {
        near(s.corner(), LOCK_TYPED) && near(s.card(), CARD)
    });
    vm.keys.enter("");
    vm.until_log("the lock plays its way out");
    vm.sway.unplug("HEADLESS-1");
    vm.until(&second, "unlocked", Shot::desktop);
    let log = vm.strand.log_text();
    assert!(
        log.contains("lock: the exit ended after ") && !log.contains("ms cap: unlocking"),
        "the exit did not end with its output:\n{log}"
    );
    let took = log
        .lines()
        .find_map(|l| l.split("lock: the exit ended after ").nth(1))
        .unwrap_or_default();
    eprintln!("lock exit, output unplugged: the exit ended after {took}");
}

/// (lock-int) A lock's still is taken at its screen's own size: on a
/// screen larger than the thumbnails' cap (1920×1080 here, `STILL_MAX`
/// 1280×720), a lock drawing it over the whole screen shows the desktop
/// as it was, pixel for pixel. Scaled up from the cap it was soft, and
/// the sharp desktop showing as the session unlocked read as a flash.
#[test]
fn a_locks_still_is_the_desktop_pixel_for_pixel() {
    let test = "lock_still_sharp";
    if !in_lock_vm(test) {
        return;
    }
    let config = CONFIG
        .replace(
            "  input { type: password",
            "  thumbnail screen { width: screen.width; height: screen.height; fit: cover }\n  \
             input { type: password",
        )
        .replace("  bg: #2050d0\n", "")
        + r#"
panel Detail {
  anchor: top_left; width: 1900; height: 300; open: true; bg: #ffffff
  text "The quick brown fox jumps over the lazy dog 0123456789" { color: #101010; font: "DejaVu Sans" 44px 400 }
}
"#;
    let vm = Vm::with(test, "", &config);
    vm.sway
        .msg(&["output", "HEADLESS-1", "resolution", "1920x1080"])
        .expect("a 1920x1080 output");
    let wide = |s: &Shot| s.w == 1920 && s.h == 1080;
    vm.until("HEADLESS-1", "the desktop at 1920x1080", |s| {
        wide(s) && s.desktop_anywhere().is_some()
    });
    std::thread::sleep(Duration::from_millis(500));
    let before = vm.sway.shot("HEADLESS-1");
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until_log("first frame shows 1 of 1 screen stills");
    std::thread::sleep(Duration::from_millis(800));
    let after = vm.sway.shot("HEADLESS-1");
    assert!(wide(&before) && wide(&after), "shots at 1920x1080");
    // The field and the text sit at the lock's top left; the rest is the still.
    let (mut off, mut worst) = (0, 0u8);
    for y in 0..after.h {
        for x in 0..after.w {
            if x < 420 && y < 120 {
                continue;
            }
            let (a, b) = (before.px(x, y), after.px(x, y));
            let d = (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0);
            worst = worst.max(d);
            if d > 3 {
                off += 1;
            }
        }
    }
    eprintln!("lock still: {off} pixels off the desktop, worst {worst}");
    assert_eq!(
        off,
        0,
        "the lock's still is not the desktop as it was ({off} pixels off, worst {worst})\n{}",
        vm.strand.log_text()
    );
}

// ---- (lock-int) a lock's stills show the desktop at rest -------------------

/// The owner's launcher and island (decisions.md lock-int, 2026-11-05),
/// cut down: the island's two icons fade out while the launcher is open
/// and back over 240 ms; the launcher's card, the island's colour, drops
/// open and folds back over the island, and its surface goes 300 ms
/// after `open` turns false, with `exit { opacity: 0 }`. Its Lock
/// (`pick`) folds it and asks for the lock as its surface goes, in the
/// tick that starts its exit, as `session.lock()` from the owner's
/// launcher does. `glow` springs the card's colour, a large animation
/// render promotes to the GPU.
const LAUNCH_CONFIG: &str = r#"export state locked = false
export state zero = 1
export state open = false
export state pick = 0
export state glow = false
state lock_next = false
state shown = false
state dropped = false
on change open { if open { shown = true } else { dropped = false } }
on change open after 110ms { if open { dropped = true } }
on change open after 300ms { if !open { shown = false } }
on change shown { if !shown { open = false; if lock_next { lock_next = false; locked = true } } }
on change pick { lock_next = true; open = false }

panel Island {
  anchor: top; margin: 6, 0, 0; size: auto; open: true
  stack { width: 640; height: 34; bg: #303040
    row { width: 640; height: 34; align: center; pad: 0, 12
      opacity: 1 ~ 240ms
      when open { opacity: 0 ~ 80ms }
      box { width: 200; height: 20; bg: #40d040 }
      spacer {}
      box { width: 200; height: 20; bg: #40d040 }
    }
  }
}

panel Launcher {
  layer: overlay; anchor: top; margin: 6, 0, 0; size: auto
  keyboard: exclusive; open: <-> shown
  exit { opacity: 0 }
  col { clip: true; width: 640; height: (dropped ? 300 : 34) ~ spring(700, 1)
    bg: glow ? #383850 : #303040 ~ 250ms
    row { height: 34; shrink: 0; pad: 0, 14; align: center
      box { width: 16; height: 16; bg: #d0d0d0 }
    }
  }
}

lock Gate {
  state secret = ""
  open: <-> locked
  bg: #2050d0
  thumbnail screen { width: screen.width; height: screen.height; fit: cover }
  col { width: 200; height: screen.height; pad: 600, 0, 0, 0
    box { width: 64; height: 64; bg: #2050d0 }
  }
  input { type: password; text: <-> secret; focus: true
    on activate { if secret != "" { auth.submit(secret) }; secret = "" }
  }
}
"#;
/// The island's icons; 8000 pixels when both show.
const ICON: [u8; 3] = [0x40, 0xd0, 0x40];
/// The launcher's search glyph: only its card shows it.
const GLYPH: [u8; 3] = [0xd0, 0xd0, 0xd0];
/// What a lock's settle logs ([`strand_surface::LOCK_SETTLE_CAP`]).
const SETTLED: &str = "lock: Strand's surfaces settled in ";
const NOT_SETTLED: &str = "lock: Strand's surfaces had not settled";

impl Vm {
    /// [`LAUNCH_CONFIG`]'s island at rest, both icons showing.
    fn island(&self) {
        self.until("HEADLESS-1", "the island", |s| s.pixels_of(ICON) >= 7900);
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Opens [`LAUNCH_CONFIG`]'s launcher and waits for it dropped.
    fn launcher(&self) {
        self.strand.cli(&["set", "lock.open", "true"]);
        self.until("HEADLESS-1", "the launcher dropped", |s| {
            s.pixels_of(GLYPH) > 100 && s.pixels_of(ICON) == 0
        });
        std::thread::sleep(Duration::from_millis(600));
    }

    /// One lock, asked for directly (the control) or from the launcher's
    /// Lock (`owner`): the lock's first shot, its still under the lock's
    /// box, once unlocked again.
    fn launch_lock(&mut self, owner: bool) -> Shot {
        let lock_shown = |s: &Shot| near(s.px(30, 630), LOCK_BG);
        if owner {
            self.launcher();
            // A new value each time: `on change pick` fires on a change.
            static PICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            let pick = PICKS.fetch_add(1, Ordering::Relaxed).to_string();
            self.strand.cli(&["set", "lock.pick", &pick]);
        } else {
            self.strand.cli(&["set", "lock.locked", "true"]);
        }
        let deadline = Instant::now() + WAIT;
        let first = loop {
            let s = self.sway.shot("HEADLESS-1");
            if lock_shown(&s) {
                break s;
            }
            assert!(
                Instant::now() < deadline,
                "never the lock\n{}",
                self.strand.log_text()
            );
        };
        let n = self
            .strand
            .log_text()
            .matches("session lock: Unlocked")
            .count();
        self.keys.enter(PASSWORD);
        let deadline = Instant::now() + WAIT;
        while self
            .strand
            .log_text()
            .matches("session lock: Unlocked")
            .count()
            <= n
        {
            assert!(
                Instant::now() < deadline,
                "never unlocked\n{}",
                self.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.island();
        first
    }
}

/// (lock-int) A lock asked for from the owner's launcher opens on the
/// desktop at rest. The launcher's Lock folds it into the island and
/// asks for the lock in the tick its `exit` starts; before the fix the
/// stills were taken at once, the exit not yet drawn, so the lock's
/// still showed the launcher's folded card over the island, its search
/// glyph and no icons (the owner's "empty island" around the lock; on
/// the code before, `owner` read 0 icon and 256 glyph pixels every run).
/// Now Strand's own surfaces settle first: the launcher's exit ends and
/// its surface goes (the island's icons, 240 ms back in, had already come
/// back under the card, which hid them), and the still shows both icons
/// and no glyph, as a lock asked for directly does (the control). The
/// direct lock has nothing moving, so it settles nothing and waits for
/// nothing: no settle line in its log. The island still moving when the
/// lock comes: `a_lock_from_the_launcher_shows_the_island_faded_in`.
#[test]
fn a_lock_from_the_launcher_shows_the_desktop_at_rest() {
    let test = "lock_launch";
    if !in_lock_vm(test) {
        return;
    }
    let mut vm = Vm::with(test, "", LAUNCH_CONFIG);
    vm.island();
    let direct = vm.launch_lock(false);
    let log = vm.strand.log_text();
    assert!(
        !log.contains(SETTLED) && !log.contains(NOT_SETTLED),
        "a lock with nothing moving settles nothing:\n{log}"
    );
    let owner = vm.launch_lock(true);
    let log = vm.strand.log_text();
    let firsts = vm.first_frames();
    eprintln!(
        "lock_launch: direct icon {} glyph {}, owner icon {} glyph {}; first frames {firsts:?} ms; {}",
        direct.pixels_of(ICON),
        direct.pixels_of(GLYPH),
        owner.pixels_of(ICON),
        owner.pixels_of(GLYPH),
        log.lines()
            .filter(|l| l.contains(SETTLED) || l.contains(NOT_SETTLED))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    for (name, shot) in [("direct", &direct), ("owner", &owner)] {
        let (icon, glyph) = (shot.pixels_of(ICON), shot.pixels_of(GLYPH));
        assert!(
            icon >= 7900 && glyph == 0,
            "{name}: the lock's still shows the launcher's card over the island \
             (icon {icon} px, glyph {glyph} px)\n{log}"
        );
    }
    assert_eq!(
        log.matches(SETTLED).count(),
        1,
        "the launcher's lock settled once, within its cap:\n{log}"
    );
    assert!(
        log.contains("(1 exits ended, 1 surfaces closed, 0 surfaces snapped to rest; 1 surfaces)"),
        "the launcher's exit ended and it went; nothing else moved:\n{log}"
    );
}

/// (lock-int review) The same with the island still fading its icons
/// back in when the lock comes (600 ms, the lock 300 ms in): a surface
/// that stays open, mid-animation, so a frame callback is owed when the
/// settle starts. Its settled frame is painted at once all the same, the
/// fade at its end, and the still shows both icons whole; the settle
/// counts it snapped and ends in a few milliseconds, well inside its cap
/// (14.3 ms on the code that waited for the frame callbacks). On the code
/// before the settle the still showed the folded card, as in the test
/// above.
#[test]
fn a_lock_from_the_launcher_shows_the_island_faded_in() {
    let test = "lock_launch_fade";
    if !in_lock_vm(test) {
        return;
    }
    let config = LAUNCH_CONFIG.replace("opacity: 1 ~ 240ms", "opacity: 1 ~ 600ms");
    assert_ne!(config, LAUNCH_CONFIG);
    let mut vm = Vm::with(test, "", &config);
    vm.island();
    let mut times = Vec::new();
    for round in 0..3 {
        let owner = vm.launch_lock(true);
        let log = vm.strand.log_text();
        let (icon, glyph) = (owner.pixels_of(ICON), owner.pixels_of(GLYPH));
        assert!(
            icon >= 7900 && glyph == 0,
            "round {round}: the lock's still shows the island part-way or the card \
             (icon {icon} px, glyph {glyph} px)\n{log}"
        );
        assert!(!log.contains(NOT_SETTLED), "the cap was hit:\n{log}");
        let lines: Vec<&str> = log.lines().filter(|l| l.contains(SETTLED)).collect();
        assert_eq!(lines.len(), round + 1, "{log}");
        let line = lines[round];
        assert!(
            line.contains(
                "(1 exits ended, 1 surfaces closed, 1 surfaces snapped to rest; 2 surfaces)"
            ),
            "the island snapped, the launcher went: {line}"
        );
        let ms: f64 = line
            .split(SETTLED)
            .nth(1)
            .and_then(|r| r.split(" ms").next())
            .and_then(|m| m.parse().ok())
            .unwrap_or(f64::NAN);
        times.push(ms);
    }
    eprintln!("lock_launch_fade: settled in {times:?} ms");
    // Painted at once, not after the frame callback the fade owed and
    // its own: 14.3 ms in the one round that ran on the code that waited
    // for them (at least a refresh, 16.7 ms, less the paint), 3.5 to
    // 7.2 ms now.
    // The first round pays for first paints; the fastest shows the wait.
    let fastest = times.iter().copied().fold(f64::INFINITY, f64::min);
    assert!(
        times.iter().all(|ms| *ms < 20.0) && fastest < 8.0,
        "the settled frame waited for the refresh: {times:?} ms"
    );
}

/// (lock-int) The same with the launcher presented by the GPU thread
/// (lavapipe): its surface goes only once the thread has let go of its
/// swapchain, and the still waits for that too. On the code before the
/// fix its still showed the folded card as the CPU's did.
#[test]
fn a_lock_from_a_gpu_presented_launcher_shows_the_desktop_at_rest() {
    let test = "lock_launch_gpu";
    if !in_lock_vm(test) {
        return;
    }
    // A card 900 wide: dropped, 0.27 Mpx, a frame of its colour's spring
    // is large enough for the GPU (0.2 Mpx); folded, it covers the island
    // as the 640 one does.
    let config = LAUNCH_CONFIG.replace(
        "col { clip: true; width: 640",
        "col { clip: true; width: 900",
    );
    assert_ne!(config, LAUNCH_CONFIG);
    let mut vm = Vm::gpu(test, "", &config);
    vm.island();
    vm.launcher();
    // The card's colour springs until render promotes it.
    let deadline = Instant::now() + WAIT;
    let mut glow = false;
    while !vm.strand.log_text().contains("attached (Present)") {
        glow = !glow;
        vm.strand
            .cli(&["set", "lock.glow", if glow { "true" } else { "false" }]);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            Instant::now() < deadline,
            "the launcher never went to the GPU\n{}",
            vm.strand.log_text()
        );
    }
    let picked = vm.strand.log_text().len();
    let owner = vm.launch_lock(true);
    let log = vm.strand.log_text();
    // The launcher was the GPU's when the lock was asked for: attached
    // before, released only after the pick, and its release came before
    // the settle ended.
    let attached = log[..picked]
        .lines()
        .filter_map(|l| l.split("GPU: surface ").nth(1))
        .filter_map(|r| r.strip_suffix(" attached (Present)"))
        .next_back()
        .map(str::to_string);
    let Some(launcher) = attached else {
        panic!("no surface attached for presenting before the pick\n{log}");
    };
    let released = format!("GPU: surface {launcher} released");
    assert!(
        !log[..picked]
            .lines()
            .rev()
            .take_while(|l| !l.contains("attached (Present)"))
            .any(|l| l.contains(&released)),
        "the launcher was released before the pick\n{log}"
    );
    let after = &log[picked..];
    let (r, st) = (after.find(&released), after.find(SETTLED));
    assert!(
        r.zip(st).is_some_and(|(r, st)| r < st),
        "the launcher's release came before the settle ended ({r:?}, {st:?})\n{after}"
    );
    let (icon, glyph) = (owner.pixels_of(ICON), owner.pixels_of(GLYPH));
    eprintln!(
        "lock_launch_gpu: owner icon {icon} glyph {glyph}; first frames {:?} ms; {}",
        vm.first_frames(),
        log.lines()
            .filter(|l| l.contains(SETTLED) || l.contains(NOT_SETTLED) || l.contains("released"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    assert!(
        icon >= 7900 && glyph == 0,
        "the lock's still shows the launcher's card over the island \
         (icon {icon} px, glyph {glyph} px)\n{log}"
    );
    assert!(log.contains(SETTLED), "it settled within its cap:\n{log}");
}

// ---- the owner's desk (lock-4k) -----------------------------------------------

/// (lock-4k) A lock shaped as the owner's opens: a still of the desktop
/// and, over it, a blurred and dimmed copy fading in over 420 ms; a card
/// springing from the size of a bar's island to the middle, fading in as
/// it grows, with a clock in a large monospace font, a text and the
/// password field. Modelled on that lock, not copied from it.
fn desk_config() -> String {
    r##"export state locked = false
export state zero = 1
lock Gate {
  state secret = ""
  open: <-> locked
  bg: #11111b
  thumbnail screen { width: screen.width; height: screen.height; fit: cover }
  thumbnail screen { width: screen.width; height: screen.height; fit: cover
    filter: blur(36), brightness(0.6)
    opacity: 1 ~ 420ms
    enter { opacity: 0 }
    exit { opacity: 0 }
  }
  col { width: screen.width; height: screen.height; align: center
    stack {
      width: 420 ~ spring(380, 0.86)
      height: 268 ~ spring(380, 0.86)
      y: (screen.height / 2 - 134) ~ spring(380, 0.86)
      opacity: 1 ~ 420ms
      enter { width: 640; height: 34; y: 0; opacity: 0 }
      exit { width: 640; height: 34; y: 0; opacity: 0 }
      bg: #2050d0
      radius: 12
      col { width: 100%; height: 100%; align: center; pad: 30, 28, 22, 28
        text clock.format("%H:%M") { font: "DejaVu Sans Mono" 64px 600; color: #ffffff }
        text pct(10 / zero) { color: #ffffff }
        input { type: password; text: <-> secret; focus: true
          on activate { if secret != "" { auth.submit(secret) }; secret = "" }
        }
      }
    }
  }
}
"##
    .to_string()
}

/// (lock-4k) The desk's outputs: HEADLESS-1 at 1920 × 1200, then
/// 1920 × 1080, then two 3840 × 2160 at scale 1.5, left to right.
const DESK: [((u32, u32), f64); 4] = [
    ((1920, 1200), 1.0),
    ((1920, 1080), 1.0),
    ((3840, 2160), 1.5),
    ((3840, 2160), 1.5),
];

impl Shot {
    /// (lock-4k) [`desk_config`]'s card shows, grown: a point inside its
    /// lower left, clear of its text and field.
    fn desk_card(&self) -> bool {
        near(self.px(self.w / 2 - 190, self.h / 2 + 120), LOCK_BG)
    }
}

impl Vm {
    /// (lock-4k) [`desk_config`] on the desk's outputs ([`DESK`]), with
    /// the GPU as [`Vm::gpu`] has it when `gpu`; the outputs' names.
    fn desk(tag: &str, faults: &str, gpu: bool) -> (Vm, Vec<String>) {
        let mut sway = Sway::start(tag);
        let (mode, scale) = DESK[0];
        sway.set_mode("HEADLESS-1", mode, scale, 0);
        let mut outputs = vec!["HEADLESS-1".to_string()];
        for (mode, scale) in &DESK[1..] {
            outputs.push(sway.plug_mode(*mode, *scale));
        }
        let desktop = Desktop::start(&sway);
        let keys = Keyboard::new(&sway);
        let mut strand = Strand::prepare(&sway, &sway.display.clone(), faults, &desk_config());
        if gpu {
            strand.env.retain(|(k, _)| k != "STRAND_LOG");
            strand.env.push(("STRAND_LOG".into(), "info,damage".into()));
            strand.env.push(("STRAND_GPU_SOFTWARE".into(), "1".into()));
        }
        strand.run();
        let vm = Vm {
            sway,
            _desktop: desktop,
            keys,
            strand,
        };
        for o in &outputs {
            vm.until(o, "the desktop", Shot::desktop);
        }
        if gpu {
            vm.until_log("GPU: the device is up");
        }
        (vm, outputs)
    }

    /// (lock-4k) Locks [`desk_config`]'s lock, lets its opening play, and
    /// unlocks it with the password: the log of that lock, from the
    /// request to its end (`GpuHost`'s longest silence when `gpu`).
    fn desk_lock(&mut self, outputs: &[String], gpu: bool) -> String {
        let from = self.strand.log_text().len();
        self.strand.cli(&["set", "lock.locked", "false"]);
        self.strand.cli(&["set", "lock.locked", "true"]);
        self.until("HEADLESS-1", "the lock's card", Shot::desk_card);
        let deadline = Instant::now() + WAIT;
        while self.strand.log_text()[from..]
            .matches(" first committed ")
            .count()
            < outputs.len()
        {
            assert!(
                Instant::now() < deadline,
                "not every output's lock was committed:\n{}",
                self.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // The opening plays out.
        std::thread::sleep(Duration::from_millis(1500));
        self.keys.enter(PASSWORD);
        let ended = if gpu {
            "longest silence with a lock request waiting"
        } else {
            ": unlocking"
        };
        while !self.strand.log_text()[from..].contains(ended) {
            assert!(
                Instant::now() < deadline + WAIT,
                "the lock never ended ({ended:?}):\n{}",
                self.strand.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        self.until("HEADLESS-1", "unlocked", Shot::desktop);
        std::thread::sleep(Duration::from_millis(300));
        self.strand.log_text()[from..].to_string()
    }
}

/// (lock-4k) The ms of each line of `log` holding `what`, the number
/// right after it.
fn ms_of(log: &str, what: &str) -> Vec<u64> {
    log.lines()
        .filter_map(|l| l.split(what).nth(1))
        .filter_map(|r| r.trim_start().split(' ').next()?.parse().ok())
        .collect()
}

/// (lock-4k) Each output's lock surface's first commit in a lock's `log`,
/// in ms after the lock request, by output (exactly one each).
fn first_commits(log: &str, outputs: &[String]) -> Vec<u64> {
    outputs
        .iter()
        .map(|o| {
            let ms = ms_of(log, &format!(" on {o} first committed "));
            assert_eq!(ms.len(), 1, "{o}: one first commit per lock:\n{log}");
            ms[0]
        })
        .collect()
}

/// (lock-4k) The hand-off lines of a lock's `log`: each presented lock
/// surface's clock held, and what its hand-off took, in ms.
fn hand_offs(log: &str) -> Vec<(u32, u64, u64)> {
    log.lines()
        .filter_map(|l| {
            let r = l.split("lock: surface ").nth(1)?;
            let (id, r) = r.split_once("'s clock was held ")?;
            let (held, r) = r.split_once(" ms across its hand-off to the GPU")?;
            let took = r.split("(the hand-off took ").nth(1)?.split(' ').next()?;
            Some((id.parse().ok()?, held.parse().ok()?, took.parse().ok()?))
        })
        .collect()
}

/// (lock-4k) The surfaces a lock's `log` says were attached to present.
fn presented_in(log: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|l| l.split("GPU: surface ").nth(1))
        .filter_map(|r| r.strip_suffix(" attached (Present)"))
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// (lock-4k) The owner's desk: every monitor's lock surface is committed
/// after the lock request on every lock, the first after `strand run`
/// started and the two after it, each output once, with all four stills;
/// on the GPU, each presented lock surface's hand-off is logged with what
/// it took, and the GPU thread's longest silence once per lock. Prints
/// the numbers (decisions.md lock-4k).
fn desk_commits(test: &str, gpu: bool) {
    if !in_lock_vm(test) {
        return;
    }
    let (mut vm, outputs) = Vm::desk(test, "", gpu);
    let mut report = Vec::new();
    let mut late = Vec::new();
    for round in 0..3 {
        let log = vm.desk_lock(&outputs, gpu);
        let stills = log
            .lines()
            .find_map(|l| {
                l.split("lock: ")
                    .nth(1)?
                    .split(" screen stills before")
                    .next()
            })
            .unwrap_or("no")
            .to_string();
        // (lock-4k) A still at its own size is copied in rows, so the 4K
        // ones come within the stills' wait too.
        assert_eq!(stills, "4 of 4", "lock {round}: every still:\n{log}");
        let commits = first_commits(&log, &outputs);
        let waits = ms_of(&log, "'s first frame waited ");
        assert!(
            waits.iter().all(|ms| *ms <= LOCK_TEXT_WAIT),
            "lock {round}: a first frame held past {LOCK_TEXT_WAIT} ms for its text: \
             {waits:?}\n{log}"
        );
        for (o, ms) in outputs.iter().zip(&commits) {
            if *ms > DESK_COMMIT_GATE {
                late.push(format!("lock {round} output {o} first committed {ms} ms"));
            }
        }
        let mut line = format!("lock {round}: {stills} stills; first commits {commits:?} ms");
        if gpu {
            let offs = hand_offs(&log);
            for id in presented_in(&log) {
                assert!(
                    offs.iter().any(|(s, ..)| *s == id),
                    "lock {round}: surface {id} was presented with no hand-off line:\n{log}"
                );
            }
            let silence = ms_of(&log, "longest silence with a lock request waiting was ");
            assert_eq!(silence.len(), 1, "lock {round}: one silence line:\n{log}");
            // (lock-4k) A working thread is never given up on.
            assert!(
                ms_of(&log, " renewed ").is_empty() && !log.contains("answered nothing for"),
                "lock {round}: a working GPU thread's lock was renewed:\n{log}"
            );
            line += &format!(
                "; hand-offs (surface, held, took) {offs:?} ms; longest GPU silence {} ms; \
                 lent {:?} ms after the request; attaches {:?} ms; first presented {:?} ms; \
                 renewals {:?} ms",
                silence[0],
                ms_of(&log, " lent to the GPU thread "),
                ms_of(&log, "'s attach was answered "),
                ms_of(&log, "'s first presented frame was presented "),
                ms_of(&log, " renewed ")
            );
        }
        line += &format!(
            "; text waits {:?} ms",
            ms_of(&log, "'s first frame waited ")
        );
        report.push(line);
        if round == 0 {
            // The first lock after start, line by line, for the record.
            let lines: Vec<&str> = log
                .lines()
                .filter(|l| l.contains("lock: ") || l.contains("GPU: ") || l.contains("gpu-frame"))
                .collect();
            eprintln!("{test}: the first lock's log:\n  {}", lines.join("\n  "));
        }
    }
    eprintln!("{test}:\n  {}", report.join("\n  "));
    desk_gate(&late);
}

/// (lock-4k) The most a lock's first frame is held for its text
/// (strand-render's `LOCK_FIRST_FRAME_TEXT_WAIT`), as its log says.
const LOCK_TEXT_WAIT: u64 = 100;

/// (lock-4k) The most a desk lock surface's first commit may come after
/// the lock request in the lock VM, in ms. Hyprland shows its dead-lock
/// screen on an output with no lock surface a second after the request
/// (`misc:lockdead_screen_delay`), and sway has no such timer, so
/// Strand's log is the judge. The four outputs' first frames are painted
/// one after another on the main thread, and in the VM's debug build a
/// 3840 × 2160 one takes about 220 ms (44 to 54 ms on the desk's release
/// build), so the last output's commit comes 530 to 640 ms after the
/// request with nothing held at all: the gate is above that, generously
/// for a slower runner, and below the second, which a first frame held
/// for its text as before (another 500 ms) passes.
const DESK_COMMIT_GATE: u64 = 900;

/// (lock-4k) The desk's wall-clock gate, checked after the functional
/// asserts: every first commit `late` past [`DESK_COMMIT_GATE`].
fn desk_gate(late: &[String]) {
    if !late.is_empty() {
        panic!(
            "GATE_MISS: {} after the lock request (gate {DESK_COMMIT_GATE} ms)",
            late.join("; ")
        );
    }
}

/// (lock-4k) A GPU thread slow to answer is no stall (`gpu_slow`: after
/// the reply it sends once the lock's surfaces are lent, the thread stops
/// 1.2 s before the next of the desk's four attaches): the lock waits for
/// it, renewing nothing, the hand-off goes on, and the password unlocks.
/// Before, the 500 ms bound renewed every lock surface, leaving each
/// output without one until the new one's first frame (Hyprland's
/// dead-lock screen once the lock is a second old).
#[test]
fn a_slow_gpu_attach_is_no_stall() {
    let test = "lock_gpu_slow";
    if !in_lock_vm(test) {
        return;
    }
    let (mut vm, outputs) = Vm::desk(test, "gpu_slow", true);
    let log = vm.desk_lock(&outputs, true);
    assert!(
        log.contains("STRAND_FAULT gpu_slow"),
        "the fault fired:\n{log}"
    );
    assert!(
        !log.contains("answered nothing for") && ms_of(&log, " renewed ").is_empty(),
        "a slow thread taken for a stalled one:\n{log}"
    );
    assert!(
        !log.contains("is drawn on the CPU from now on"),
        "a lock surface was renewed:\n{log}"
    );
    let silence = ms_of(&log, "longest silence with a lock request waiting was ");
    assert!(
        silence.iter().any(|ms| *ms >= 1000),
        "the fault left a lock request waiting through its silence: {silence:?}\n{log}"
    );
    let offs = hand_offs(&log);
    assert!(!offs.is_empty(), "the lock went on to the GPU:\n{log}");
    eprintln!(
        "lock gpu slow: longest silence {silence:?} ms; first commits {:?} ms; attaches {:?} ms; \
         hand-offs (surface, held, took) {offs:?} ms",
        first_commits(&log, &outputs),
        ms_of(&log, "'s attach was answered "),
    );
}

/// (lock-4k) A cold text worker on the lock (`text_slow`: once the
/// session is locked, its reply is held 2 s) holds no monitor's lock
/// surface past a short wait: every output's first commit comes inside a
/// second of the request (functional: the hold must end), with no
/// fallback and no missed first frame; the clock's text shows once it
/// comes, and the password unlocks. Before, a lock's first frame waited
/// 500 ms for its text, moved on by every other output's paint, past
/// the second.
#[test]
fn a_lock_whose_text_is_slow_still_commits_in_time() {
    let test = "lock_text_slow";
    if !in_lock_vm(test) {
        return;
    }
    let (mut vm, outputs) = Vm::desk(test, "text_slow", false);
    vm.strand.cli(&["set", "lock.locked", "true"]);
    vm.until("HEADLESS-1", "the lock's card", Shot::desk_card);
    let deadline = Instant::now() + WAIT;
    while vm.strand.log_text().matches(" first committed ").count() < outputs.len() {
        assert!(
            Instant::now() < deadline,
            "not every output's lock was committed:\n{}",
            vm.strand.log_text()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = vm.strand.log_text();
    assert!(
        log.contains("STRAND_FAULT text_slow"),
        "the fault fired:\n{log}"
    );
    let commits = first_commits(&log, &outputs);
    assert!(
        commits.iter().all(|ms| *ms < 1000),
        "a lock surface past a second: {commits:?}\n{log}"
    );
    let waits = ms_of(&log, "'s first frame waited ");
    assert!(
        !waits.is_empty(),
        "a first frame waited for its text:\n{log}"
    );
    assert!(
        waits.iter().all(|ms| *ms <= LOCK_TEXT_WAIT),
        "a first frame held past {LOCK_TEXT_WAIT} ms for its text: {waits:?}\n{log}"
    );
    assert!(!log.contains(FIRST_FRAME_MISSED), "{log}");
    assert!(
        !log.contains("lock: showing the built-in password field"),
        "{log}"
    );
    // The clock's white text on the card, once it came.
    let shot = vm.until("HEADLESS-1", "the clock's text", |s| {
        s.pixels_of([0xff, 0xff, 0xff]) > 200
    });
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("lock: ") || l.contains("STRAND_FAULT"))
        .collect();
    eprintln!(
        "lock text slow: first commits {commits:?} ms; text waits {waits:?} ms; {} white \
         pixels; the lock's log:\n  {}",
        shot.pixels_of([0xff, 0xff, 0xff]),
        lines.join("\n  ")
    );
    std::thread::sleep(Duration::from_millis(500));
    vm.keys.enter(PASSWORD);
    vm.until_log(": unlocking");
    vm.until("HEADLESS-1", "unlocked", Shot::desktop);
    let late: Vec<String> = outputs
        .iter()
        .zip(&commits)
        .filter(|(_, ms)| **ms > DESK_COMMIT_GATE)
        .map(|(o, ms)| format!("output {o} first committed {ms} ms"))
        .collect();
    desk_gate(&late);
}

#[test]
fn every_monitors_lock_is_committed_after_the_request() {
    desk_commits("desk_commits", false);
}

#[test]
fn every_monitors_lock_is_committed_after_the_request_on_the_gpu() {
    desk_commits("desk_commits_gpu", true);
}
