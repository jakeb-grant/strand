//! A private PipeWire for the audio tests: its own runtime directory and
//! session bus (`dbus-daemon`), `pipewire` with two null sinks and a null
//! source from a config of its own, and WirePlumber (which creates the
//! `default` metadata and applies `default.configured.*`). Driven with
//! `wpctl`, `pw-cli`, `pw-metadata` and `pw-play`, as a user would. Never
//! the machine's own daemon or buses.
#![allow(dead_code)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use strand_services::audio::AudioConfig;

/// The daemon's config: no hardware, a dummy driver, and three devices.
const CONFIG: &str = r#"
context.properties = {
    core.daemon = true
    core.name   = pipewire-0
    default.clock.rate = 48000
    default.clock.quantum = 1024
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-profiler }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-spa-device-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-client-device }
    { name = libpipewire-module-access }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
    { name = libpipewire-module-session-manager }
]
context.objects = [
    { factory = spa-node-factory
        args = {
            factory.name    = support.node.driver
            node.name       = Dummy-Driver
            node.group      = pipewire.dummy
            priority.driver = 20000
        }
    }
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "strand-sink-a"
            node.description = "Strand Sink A"
            media.class      = "Audio/Sink"
            audio.position   = [ FL FR ]
            priority.session = 2000
        }
    }
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "strand-sink-b"
            node.description = "Strand Sink B"
            media.class      = "Audio/Sink"
            audio.position   = [ FL FR ]
            priority.session = 1000
        }
    }
    { factory = adapter
        args = {
            factory.name     = support.null-audio-sink
            node.name        = "strand-source"
            node.description = "Strand Source"
            media.class      = "Audio/Source/Virtual"
            audio.channels   = 1
            audio.position   = [ MONO ]
            # Above the sinks' monitors, which WirePlumber also ranks as
            # sources.
            priority.session = 3000
        }
    }
]
"#;

/// Whether a missing tool fails the test instead of skipping it (CI).
fn required() -> bool {
    ["STRAND_REQUIRE_PIPEWIRE", "STRAND_REQUIRE_DBUS"]
        .iter()
        .any(|v| std::env::var(v).is_ok_and(|v| v == "1"))
}

fn have(tool: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A running private PipeWire.
pub struct PipeWire {
    /// The runtime directory (short, under /tmp: a socket path has a
    /// 108-byte limit).
    pub dir: tempfile::TempDir,
    bus: String,
    dbus: Option<Child>,
    pipewire: Option<Child>,
    wireplumber: Option<Child>,
}

impl PipeWire {
    /// Starts the bus, the daemon and WirePlumber, or `None` (after saying
    /// why) when a tool is missing and the tier is not required.
    pub fn start(test: &str) -> Option<PipeWire> {
        let tools = ["dbus-daemon", "pipewire", "wireplumber", "wpctl", "pw-cli"];
        if let Some(t) = tools.iter().find(|t| !have(t)) {
            assert!(
                !required(),
                "{test}: {t} is missing (STRAND_REQUIRE_PIPEWIRE)"
            );
            eprintln!("{test}: skipped, {t} is missing");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("strand-pw.")
            .tempdir_in("/tmp")
            .expect("a runtime directory");
        std::fs::write(dir.path().join("pipewire.conf"), CONFIG).expect("the config");
        for d in ["state", "config"] {
            std::fs::create_dir(dir.path().join(d)).expect("a state directory");
        }
        let bus_path = dir.path().join("bus");
        let bus = format!("unix:path={}", bus_path.display());
        let mut pw = PipeWire {
            dir,
            bus,
            dbus: None,
            pipewire: None,
            wireplumber: None,
        };
        let dbus = pw
            .command("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile"])
            .arg(format!("--address={}", pw.bus))
            .spawn()
            .expect("dbus-daemon starts");
        pw.dbus = Some(dbus);
        wait_for(&bus_path, "the session bus");
        pw.start_daemon();
        Some(pw)
    }

    /// The service's config: our socket, by absolute path.
    pub fn config(&self) -> AudioConfig {
        AudioConfig {
            remote: Some(self.socket().display().to_string()),
        }
    }

    /// The daemon's socket.
    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("pipewire-0")
    }

    /// A command in this instance's environment (and nobody else's).
    pub fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        let d = self.dir.path();
        cmd.env("XDG_RUNTIME_DIR", d)
            .env("PIPEWIRE_RUNTIME_DIR", d)
            .env_remove("PIPEWIRE_REMOTE")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus)
            // No system bus: WirePlumber's logind and BlueZ parts fail
            // quietly instead of reaching the machine's.
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                format!("unix:path={}", d.join("no-system-bus").display()),
            )
            .env("XDG_STATE_HOME", d.join("state"))
            .env("XDG_CONFIG_HOME", d.join("config"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only an async-signal-safe prctl between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(std::io::Error::from)
            });
        }
        cmd
    }

    /// Starts `pipewire` (waiting for its socket), then WirePlumber.
    pub fn start_daemon(&mut self) {
        let conf = self.dir.path().join("pipewire.conf");
        let child = self
            .command("pipewire")
            .arg("-c")
            .arg(&conf)
            .spawn()
            .expect("pipewire starts");
        self.pipewire = Some(child);
        wait_for(&self.socket(), "the PipeWire socket");
        self.start_wireplumber();
    }

    /// Starts WirePlumber and waits for it to choose the defaults.
    pub fn start_wireplumber(&mut self) {
        let child = self
            .command("wireplumber")
            .spawn()
            .expect("wireplumber starts");
        self.wireplumber = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.metadata().contains("default.audio.sink") {
            assert!(
                Instant::now() < deadline,
                "WirePlumber chose no default sink"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stops the daemon (WirePlumber goes with it) and waits until its
    /// socket is gone.
    pub fn kill_daemon(&mut self) {
        for c in [self.wireplumber.take(), self.pipewire.take()]
            .into_iter()
            .flatten()
        {
            let mut c = c;
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(self.socket());
    }

    /// Stops WirePlumber alone.
    pub fn kill_wireplumber(&mut self) {
        if let Some(mut c) = self.wireplumber.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Runs a tool and returns its output; panics when it fails.
    pub fn run(&self, program: &str, args: &[&str]) -> String {
        let out = self
            .command(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("{program}: {e}"));
        assert!(
            out.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `wpctl args…`.
    pub fn wpctl(&self, args: &[&str]) -> String {
        self.run("wpctl", args)
    }

    /// `pw-metadata` (the `default` metadata, as text).
    pub fn metadata(&self) -> String {
        let out = self.command("pw-metadata").stdout(Stdio::piped()).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// `wpctl get-volume id`: `(volume, muted)`.
    pub fn volume(&self, id: u32) -> (f64, bool) {
        let out = self.wpctl(&["get-volume", &id.to_string()]);
        // "Volume: 0.40" or "Volume: 0.40 [MUTED]"
        let v = out
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("wpctl get-volume: {out}"));
        (v, out.contains("[MUTED]"))
    }

    /// Whether a node named `name` exists.
    pub fn has_node(&self, name: &str) -> bool {
        self.run("pw-cli", &["ls", "Node"])
            .contains(&format!("node.name = \"{name}\""))
    }

    /// Plays `wav` to the node named `target`, in the background.
    pub fn play(&self, wav: &Path, target: &str) -> Child {
        self.command("pw-play")
            .arg("--target")
            .arg(target)
            .arg(wav)
            .spawn()
            .expect("pw-play starts")
    }
}

impl Drop for PipeWire {
    fn drop(&mut self) {
        for c in [
            self.wireplumber.take(),
            self.pipewire.take(),
            self.dbus.take(),
        ]
        .into_iter()
        .flatten()
        {
            let mut c = c;
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} did not appear");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A 16-bit stereo WAV of a ±`amplitude` square wave at 48 kHz.
pub fn square_wav(path: &Path, secs: f32, amplitude: f32) {
    let frames = (48_000.0 * secs) as u32;
    let level = (amplitude * 32768.0).round().clamp(0.0, 32767.0) as i16;
    let mut data = Vec::with_capacity(frames as usize * 4);
    for i in 0..frames {
        let s = if (i / 48) % 2 == 0 { level } else { -level };
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    std::fs::write(path, wav).expect("the wav file");
}
