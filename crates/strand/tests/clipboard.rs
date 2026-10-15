//! (M6) Copy and paste in an `input` end to end (spec.md 6.7;
//! owner-testing gap 30): `strand run` on a headless sway, a
//! `keyboard: exclusive` panel holding a focused `input` bound to an
//! exported state, keys from a virtual keyboard, and another program (a
//! wlr-data-control client) reading what Ctrl+C copied and setting what
//! Ctrl+V and Shift+Insert paste. The state is read with `strand get`.
//!
//! Skipped, loudly, without sway (CI sets `STRAND_REQUIRE_SWAY`).

mod support;

#[path = "../../strand-services/tests/common/data_control.rs"]
mod data_control;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use data_control::DataControl;
use support::keyboard::{self, CONTROL, SHIFT};

const CONFIG: &str = "\
export state q = \"\"

panel Clip {
  anchor: center; keyboard: exclusive
  col { width: 300; pad: 8
    input { text: <-> q; focus: true }
  }
}
";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Desk {
    strand: Option<Proc>,
    keys: keyboard::Keyboard,
    _sway: Proc,
    dir: PathBuf,
    display: String,
    log: PathBuf,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Desk {
    fn start(tag: &str) -> Option<Desk> {
        if Command::new("sway").arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "sway is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: sway is not installed; the clipboard test did not run ***\n");
            return None;
        }
        // Short: the socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-clip-{tag}-{}", std::process::id()));
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
        let mut cmd = Command::new("sway");
        // SAFETY: the hook runs between fork and exec and makes one
        // async-signal-safe syscall.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(std::io::Error::from)
            });
        }
        let sway = Proc(
            cmd.arg("-c")
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
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let display = loop {
            let found = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            if let Some(d) = found
                && UnixStream::connect(dir.join(&d)).is_ok()
            {
                break d;
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        // The keyboard joins the seat before strand connects (see
        // acceptance.rs's `Desk::start_with`).
        let keys = keyboard::Keyboard::new(&dir.join(&display), &dir);
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("clip.strand"), CONFIG).unwrap();
        let mut desk = Desk {
            strand: None,
            keys,
            _sway: sway,
            log: dir.join("strand.log"),
            display,
            dir,
        };
        let child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .envs(desk.env())
            .env("HOME", desk.dir.join("home"))
            .env("XDG_CACHE_HOME", desk.dir.join("cache"))
            .env("XDG_STATE_HOME", desk.dir.join("state"))
            .env("STRAND_LOG", "damage")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("STRAND_MOCK")
            .env_remove("SWAYSOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&desk.log).unwrap())
            .spawn()
            .unwrap();
        desk.strand = Some(Proc(child));
        // The panel painted (and so mapped, with the keyboard).
        desk.wait("the panel shows", |d| {
            d.log_text()
                .lines()
                .any(|l| l.starts_with("strand: damage"))
        });
        Some(desk)
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
            ("STRAND_SOCKET", self.dir.join("s.sock")),
        ]
    }

    fn socket(&self) -> PathBuf {
        self.dir.join(&self.display)
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Desk) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            if let Some(p) = &mut self.strand {
                assert!(
                    p.0.try_wait().unwrap().is_none(),
                    "strand exited: {}",
                    self.log_text()
                );
            }
            assert!(Instant::now() < deadline, "{what}\n{}", self.log_text());
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    fn strand(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `q`, as `strand get` reads it (`None` until it answers).
    fn q(&self) -> Option<String> {
        let out = self.strand(&["get", "clip.q"]);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    fn wait_q(&mut self, want: &str) {
        self.wait(&format!("q is {want:?}"), |d| {
            d.q().as_deref() == Some(want)
        });
    }
}

fn socket_ok(p: &Path) -> bool {
    UnixStream::connect(p).is_ok()
}

/// Typed text copied with Ctrl+A, Ctrl+C reaches another program; its
/// copy pastes with Ctrl+V over the selection and with Shift+Insert at
/// the caret.
#[test]
fn an_input_copies_and_pastes_through_the_compositor() {
    let Some(mut desk) = Desk::start("io") else {
        return;
    };
    assert!(socket_ok(&desk.socket()));
    let other = DataControl::connect(
        wayland_client::Connection::from_socket(UnixStream::connect(desk.socket()).unwrap())
            .unwrap(),
    )
    .expect("sway 1.9 offers wlr-data-control");
    desk.wait_q("");
    desk.keys.type_text("abc");
    desk.wait_q("abc");
    desk.keys.press_with(CONTROL, "a");
    desk.keys.press_with(CONTROL, "c");
    assert!(
        other.wait(Duration::from_secs(10), |s| s
            .mimes
            .iter()
            .any(|m| m == "text/plain;charset=utf-8")),
        "the copy became the selection: {:?}\n{}",
        other.seen(),
        desk.log_text()
    );
    assert_eq!(
        other.read_now("text/plain;charset=utf-8", Duration::from_secs(10)),
        Some(b"abc".to_vec())
    );
    // Another program's copy, pasted over the selection.
    other.copy_text("xyz");
    assert!(other.wait(Duration::from_secs(10), |s| s.mimes.len() == 5));
    desk.keys.press_with(CONTROL, "a");
    desk.keys.press_with(CONTROL, "v");
    desk.wait_q("xyz");
    // Shift+Insert pastes at the caret (after the paste).
    desk.keys.press_with(SHIFT, "Insert");
    desk.wait_q("xyzxyz");
}
