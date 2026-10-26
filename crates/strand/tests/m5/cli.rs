//! S-ipc: `strand get | set | toggle | call | clear` over the socket,
//! against a headless `strand run` (docs/m5-plan.md, "S-ipc", tests):
//!
//! - under `STRAND_MOCK`: `get` (text and `--json` with the layer),
//!   `toggle` (and its refusal of a non-bool), and the token overlay:
//!   `strand set '$probe.c'` springs the bar's colour, is kept in
//!   `$XDG_STATE_HOME/strand/tokens.toml`, a save that changes the file's
//!   value is shadowed ("overlay wins", heard by `strand watch`), and
//!   `strand clear` drops it so the saved value shows;
//! - on the real notifications service (a private `dbus-daemon`): `strand
//!   call notifications.clear` dismisses what an app sent, and a call
//!   with a wrong action or arguments is refused saying what was meant
//!   (a wrong argument type, which no builtin service-level action can
//!   take but text, is `ipc::call`'s unit tests').
//!
//! Skipped, loudly, without sway or grim (CI sets `STRAND_REQUIRE_SWAY`),
//! and the call test without `dbus-daemon` (`STRAND_REQUIRE_DBUS`).

use std::collections::HashMap;
use std::io::BufRead as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use strand_services::testing::PrivateBus;

pub(super) struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A bar painted with `$probe.c`, a token of `tokens base`.
pub(super) fn bar(color: &str) -> String {
    format!(
        "tokens base {{ probe.c: {color} }}\n\
         export state open = false\n\
         export state label = \"hi\"\n\
         bar Top {{\n  edge: top; height: 40\n  bg: $probe.c\n  text label\n}}\n"
    )
}

pub(super) fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; the CLI test did not run ***\n");
            return false;
        }
    }
    true
}

/// A headless sway, one 1280x720 output, and `strand run` on a config
/// directory; the CLI reaches it through `STRAND_SOCKET`.
pub(super) struct Desk {
    strand: Option<Proc>,
    _sway: Proc,
    dir: PathBuf,
    config: PathBuf,
    display: String,
    socket: PathBuf,
    log: PathBuf,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.strand.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Desk {
    pub(super) fn start(tag: &str, files: &[(&str, &str)], env: &[(&str, &str)]) -> Desk {
        Self::start_with(tag, files, env, false)
    }

    /// [`Desk::start`], with `strand run` given sway's `SWAYSOCK` when
    /// `swaysock` (as in a sway session; S-tools' report test).
    pub(super) fn start_with(
        tag: &str,
        files: &[(&str, &str)],
        env: &[(&str, &str)],
        swaysock: bool,
    ) -> Desk {
        // Short: the socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-cli-{tag}-{}", std::process::id()));
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
        let (display, ipc) = loop {
            let names: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = names
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc)
                && Command::new("swaymsg")
                    .args(["-t", "get_version"])
                    .env("SWAYSOCK", dir.join(i))
                    .output()
                    .is_ok_and(|o| o.status.success())
            {
                break (d.clone(), dir.join(i));
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        for (name, text) in files {
            std::fs::write(config.join(name), text).unwrap();
        }
        let mut desk = Desk {
            strand: None,
            _sway: sway,
            socket: dir.join("s.sock"),
            log: dir.join("strand.log"),
            config,
            display,
            dir,
        };
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
        cmd.arg("run")
            .arg(&desk.config)
            .envs(desk.env())
            .env("HOME", desk.dir.join("home"))
            .env("XDG_CACHE_HOME", desk.dir.join("cache"))
            .env("XDG_STATE_HOME", desk.state())
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("STRAND_MOCK")
            .env_remove("SWAYSOCK")
            .env_remove("XDG_CURRENT_DESKTOP")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&desk.log).unwrap());
        if swaysock {
            cmd.env("SWAYSOCK", &ipc);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        desk.strand = Some(Proc(cmd.spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !desk.strand(&["get", "bar.label"]).status.success() {
            desk.assert_alive();
            assert!(
                Instant::now() < deadline,
                "strand did not answer: {}",
                desk.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        desk
    }

    /// The `strand run` process's id.
    pub(super) fn pid(&self) -> u32 {
        self.strand.as_ref().map_or(0, |p| p.0.id())
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    /// The test's directory: `config` (the config), `state` (its
    /// `XDG_STATE_HOME`), the sockets (S-tools' export tests).
    pub(super) fn root(&self) -> &std::path::Path {
        &self.dir
    }

    /// Stops `strand run` (killed), leaving sway and the directories.
    pub(super) fn stop(&mut self) {
        self.strand.take();
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_RUNTIME_DIR", self.dir.clone()),
            ("WAYLAND_DISPLAY", PathBuf::from(&self.display)),
            ("STRAND_SOCKET", self.socket.clone()),
        ]
    }

    pub(super) fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn assert_alive(&mut self) {
        if let Some(p) = &mut self.strand {
            assert!(
                p.0.try_wait().unwrap().is_none(),
                "strand exited: {}",
                self.log_text()
            );
        }
    }

    /// `strand <args>`, run to its end.
    fn strand(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .envs(self.env())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `strand <args>`, which must succeed: its stdout.
    pub(super) fn ok(&self, args: &[&str]) -> String {
        let out = self.strand(args);
        assert!(
            out.status.success(),
            "strand {args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_text()
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// `strand <args>`, which must fail: its stderr.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.strand(args);
        assert!(!out.status.success(), "strand {args:?} succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    /// The colour of one pixel of the output.
    fn pixel(&mut self, x: u32, y: u32) -> [u8; 3] {
        self.assert_alive();
        let path = self.dir.join("px.ppm");
        let ok = Command::new("grim")
            .args(["-t", "ppm", "-g", &format!("{x},{y} 1x1")])
            .arg(&path)
            .envs(self.env())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "grim");
        let bytes = std::fs::read(&path).unwrap();
        // `P6\n1 1\n255\n` then one RGB triple.
        let n = bytes.len();
        assert!(n >= 3, "a 1x1 ppm");
        [bytes[n - 3], bytes[n - 2], bytes[n - 1]]
    }

    /// Waits (bounded) for the bar to show `want` (the spring settled).
    fn wait_color(&mut self, what: &str, want: [u8; 3]) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let got = self.pixel(640, 20);
            if got.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: the bar is {got:?}, not {want:?}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Saves `text` as `name` the way an editor's atomic save does.
    pub(super) fn save(&self, name: &str, text: &str) {
        let tmp = self.dir.join(format!("{name}.new"));
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, self.config.join(name)).unwrap();
    }

    /// `strand watch --json`, its events on a channel once it is
    /// subscribed.
    pub(super) fn watch(&self) -> (Proc, mpsc::Receiver<serde_json::Value>) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(["watch", "--json"])
            .envs(self.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        let first: serde_json::Value = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(first["event"], "watching");
        (Proc(child), rx)
    }
}

/// Waits (bounded) for a `notices` event saying `text`.
fn heard(events: &mpsc::Receiver<serde_json::Value>, text: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(ev) = events.recv_timeout(left) else {
            return false;
        };
        if ev["notices"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_str() == Some(text)))
        {
            return true;
        }
    }
    false
}

pub(super) fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

/// `get`, `toggle` and the token overlay under `STRAND_MOCK`.
#[test]
fn get_toggle_and_a_token_overlay_over_the_socket() {
    if !tools() {
        return;
    }
    let mut d = Desk::start(
        "get",
        &[("bar.strand", &bar("#112233"))],
        &[("STRAND_MOCK", "acceptance")],
    );
    // get: text, and `--json` with the layer.
    assert_eq!(d.ok(&["get", "bar.label"]), "hi\n");
    assert_eq!(
        json(&d.ok(&["get", "--json", "bar.label"])),
        serde_json::json!({"ok": true, "value": "hi", "text": "hi", "layer": null})
    );
    assert!(d.refused(&["get", "bar.nope"]).contains("bar.nope"));
    // toggle: a bool flips; anything else is refused, naming its type.
    assert_eq!(d.ok(&["get", "bar.open"]), "false\n");
    d.ok(&["toggle", "bar.open"]);
    assert_eq!(d.ok(&["get", "bar.open"]), "true\n");
    let why = d.refused(&["toggle", "bar.label"]);
    assert!(why.contains("`bar.label` is a text, not a bool"), "{why}");

    // The token overlay: the colour springs to it and it is kept.
    d.wait_color("the file's colour", [0x11, 0x22, 0x33]);
    assert_eq!(d.ok(&["get", "$probe.c"]), "#112233\n");
    assert_eq!(
        json(&d.ok(&["get", "--json", "$probe.c"]))["layer"],
        "tokens base"
    );
    d.ok(&["set", "$probe.c", "#ff8800"]);
    d.wait_color("the overlay's colour", [0xff, 0x88, 0x00]);
    let got = json(&d.ok(&["get", "--json", "$probe.c"]));
    assert_eq!(got["value"], "#ff8800");
    assert_eq!(got["layer"], "overlay");
    let why = d.refused(&["set", "$probe.c", "orange"]);
    assert!(why.contains("`orange` is not a color"), "{why}");
    let why = d.refused(&["set", "$probe.cc", "#000000"]);
    assert!(why.contains("did you mean `$probe.c`?"), "{why}");
    let file = d.state().join("strand").join("tokens.toml");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&file).is_ok_and(|t| t.contains("\"probe.c\" = \"#ff8800\"")) {
        assert!(Instant::now() < deadline, "{} not written", file.display());
        std::thread::sleep(Duration::from_millis(20));
    }

    // A save that changes the token: the overlay wins, and says so.
    let (_watch, events) = d.watch();
    d.save("bar.strand", &bar("#445566"));
    assert!(
        heard(
            &events,
            "probe.c: file changed but runtime overlay wins [clear]"
        ),
        "no notice\n{}",
        d.log_text()
    );
    d.wait_color("the overlay holds", [0xff, 0x88, 0x00]);
    // `strand clear`: the saved value shows.
    d.ok(&["clear", "$probe.c"]);
    d.wait_color("the saved colour", [0x44, 0x55, 0x66]);
    assert_eq!(
        json(&d.ok(&["get", "--json", "$probe.c"]))["layer"],
        "tokens base"
    );
    // Clearing what has no overlay is not an error; a path without one is,
    // and so is a token this shell does not have (with the closest name,
    // as `set` says it).
    d.ok(&["clear", "$probe.c"]);
    let why = d.refused(&["clear", "$probe.cc"]);
    assert!(why.contains("did you mean `$probe.c`?"), "{why}");
    let why = d.refused(&["clear", "bar.open"]);
    assert!(why.contains("not a settings field or a token"), "{why}");
}

/// `strand call` on the real notifications service: an app's
/// notification is dismissed by `strand call notifications.clear`, and
/// bad calls are refused saying what was meant.
#[test]
fn call_runs_a_service_action() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let src = format!("{}export let count = notifications.count\n", bar("#112233"));
    let d = Desk::start(
        "call",
        &[("bar.strand", &src)],
        &[("DBUS_SESSION_BUS_ADDRESS", &bus.address)],
    );
    assert!(
        bus.wait_for_name("org.freedesktop.Notifications", Duration::from_secs(10)),
        "the shell's notification server\n{}",
        d.log_text()
    );
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let conn = tokio.block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    });
    let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
    tokio
        .block_on(conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "app",
                0u32,
                "",
                "Hello",
                "from the test",
                Vec::<&str>::new(),
                hints,
                -1i32,
            ),
        ))
        .unwrap();
    let until = |want: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = d.ok(&["get", "bar.count"]);
            if got.trim() == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "count {got:?}, not {want}\n{}",
                d.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    until("1");
    assert_eq!(
        json(&d.ok(&["call", "--json", "notifications.clear"])),
        serde_json::json!({"ok": true})
    );
    until("0");
    assert_eq!(d.ok(&["call", "notifications.clear"]), "");
    for (args, why) in [
        (
            &["call", "notifications.claer"][..],
            "`notifications` has no action `claer`; did you mean `clear`?",
        ),
        (
            &["call", "notifications.clear", "now"],
            "`notifications.clear()` takes 0 arguments, not 1",
        ),
        (
            &["call", "notifications.popups.clear"],
            "service-level actions only",
        ),
        (&["call", "auth.submit", "x"], "not run over IPC"),
    ] {
        let said = d.refused(args);
        assert!(said.contains(why), "{args:?}: {said}");
    }
}

/// (strand-fixes) `strand <command> --help` (and `-h`) prints the
/// command's usage on stdout and exits 0, for every command `strand
/// --help` lists, the socket's included (it asks no shell); a real usage
/// error stays on stderr with a non-zero exit.
#[test]
fn every_command_prints_its_help_on_stdout() {
    let top = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(top.status.success());
    let top = String::from_utf8(top.stdout).unwrap();
    let commands: Vec<&str> = top
        .split("\ncommands:\n")
        .nth(1)
        .unwrap()
        .lines()
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    assert!(commands.len() > 10, "{top}");
    let nobody = std::env::temp_dir().join(format!("strand-help-{}.sock", std::process::id()));
    for cmd in commands {
        for flag in ["--help", "-h"] {
            let out = Command::new(env!("CARGO_BIN_EXE_strand"))
                .args([cmd, flag])
                .env("STRAND_SOCKET", &nobody)
                .env_remove("WAYLAND_DISPLAY")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "strand {cmd} {flag}: {stderr}");
            assert!(
                stdout.starts_with(&format!("usage: strand {cmd}")),
                "strand {cmd} {flag}: {stdout}"
            );
            assert!(stderr.is_empty(), "strand {cmd} {flag}: {stderr}");
        }
    }
    for args in [&["run", "a", "b"][..], &["new", "a", "b"], &["set", "x"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .env("STRAND_SOCKET", &nobody)
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("usage: strand"), "{args:?}: {stderr}");
    }
}

/// (strand-fixes) `strand run` on a directory that is not there says so
/// in the user's words, with the command that makes one, not the
/// logic thread's.
#[test]
fn run_names_a_missing_config_dir_plainly() {
    let dir = std::env::temp_dir().join(format!("strand-no-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&dir)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let shown = dir.display();
    assert_eq!(
        stderr.trim_end(),
        format!(
            "strand run: {shown} does not exist; `strand new {shown}` writes a starter shell there"
        ),
    );
    // A file is no directory.
    std::fs::write(&dir, "").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_strand"))
        .arg("run")
        .arg(&dir)
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.trim_end(),
        format!("strand run: {shown} is not a directory")
    );
    let _ = std::fs::remove_file(&dir);
}
