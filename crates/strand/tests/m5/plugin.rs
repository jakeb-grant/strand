//! S-plugins: a plugin binary through `strand::Shell::builder()`
//! (docs/architecture.md, "Plugin builder"; decisions.md m5-plugins):
//!
//! - `plugin_hello` writes its schema with `--write-schemas`, the stock
//!   `strand check` checks a config reading `hello` against that file,
//!   and `plugin_hello run` serves the service to `strand call` and
//!   `strand get`;
//! - the stock `strand run` reads the file: a config reading the
//!   service it does not serve gets a notice naming the file, defaults
//!   for reads, and refusals for writes and calls;
//! - a plugin binary without `strand::process_setup!()` says so;
//! - the guide's profile block is the workspace's.
//!
//! The examples are built beside this binary: `cargo test` of the whole
//! package builds them, and a filtered run (`--test m5`, which does not)
//! has the cargo running it build them once, into the same target
//! directory and profile (decisions.md m5-fixups). Skipped, loudly,
//! without sway (CI sets `STRAND_REQUIRE_SWAY`).

use std::io::BufRead as _;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const WARNING: &str = "strand::process_setup!()";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `target/<profile>/examples/<name>`, built for this test binary's
/// profile and features ([`build_examples`]).
fn example(name: &str) -> PathBuf {
    let strand = Path::new(env!("CARGO_BIN_EXE_strand"));
    let path = strand
        .parent()
        .map(|p| p.join("examples").join(name))
        .unwrap_or_default();
    if let Err(e) = build_examples(strand) {
        panic!(
            "building the plugin examples: {e}\n(run `cargo build -p strand --examples`, or \
             `cargo test -p strand`, which builds them)"
        );
    }
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

/// Builds `plugin_hello` and `plugin_bare` once per test process with the
/// cargo running this test, into the target directory and profile
/// `strand` (`target/<dir>/<profile>/strand`) was built in, with this
/// build's features: up to date, it does nothing, and after a filtered
/// `cargo test --test m5` (which builds no examples) it builds them.
fn build_examples(strand: &Path) -> Result<(), String> {
    static BUILT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let profile_dir = strand.parent().ok_or("no profile directory")?;
            let target_dir = profile_dir.parent().ok_or("no target directory")?;
            let profile = match profile_dir.file_name().and_then(|n| n.to_str()) {
                Some("debug") => "dev",
                Some(p) => p,
                None => return Err("an unnamed profile directory".into()),
            };
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut cmd = Command::new(cargo);
            cmd.args(["build", "--offline", "--quiet", "--profile", profile])
                .arg("--manifest-path")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
                .arg("--target-dir")
                .arg(target_dir)
                .args(["--example", "plugin_hello", "--example", "plugin_bare"]);
            if !cfg!(feature = "gpu") {
                cmd.arg("--no-default-features");
            }
            let out = cmd.output().map_err(|e| format!("cargo: {e}"))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                let tail: String = err.lines().rev().take(30).collect::<Vec<_>>().join("\n");
                return Err(format!("cargo build failed:\n{tail}"));
            }
            Ok(())
        })
        .clone()
}

/// A command whose child starts with THP back on: the test binary turned
/// it off (`process_setup!`) and `PR_SET_THP_DISABLE` survives fork and
/// exec, so a child that reports it off turned it off itself.
fn thp_on(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    // SAFETY: the hook runs between fork and exec and makes two
    // async-signal-safe syscalls.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_THP_DISABLE, 0, 0, 0, 0);
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(std::io::Error::from)
        });
    }
    cmd
}

fn sway_tools() -> bool {
    for tool in ["sway", "swaymsg"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; the plugin test did not run ***\n");
            return false;
        }
    }
    true
}

fn text(out: &[u8]) -> String {
    String::from_utf8_lossy(out).into_owned()
}

/// A scratch directory with a config, a data directory for the schema
/// files and, once started, a headless sway and a shell on it.
struct Desk {
    shell: Option<Proc>,
    sway: Option<Proc>,
    dir: PathBuf,
    display: String,
    /// `STRAND_MOCK` for the shell `start` runs (none by default).
    mock: Option<&'static str>,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.shell.take();
        self.sway.take();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Desk {
    fn new(tag: &str) -> Desk {
        // Short: the socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-pl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Desk {
            shell: None,
            sway: None,
            dir,
            display: String::new(),
            mock: None,
        }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config")
    }

    fn data(&self) -> PathBuf {
        self.dir.join("data")
    }

    fn schema_file(&self) -> PathBuf {
        self.data().join("strand/schemas/hello.schema")
    }

    fn log(&self) -> PathBuf {
        self.dir.join("shell.log")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// Saves `text` as the config's `bar.strand` the way an editor's
    /// atomic save does.
    fn save(&self, text: &str) {
        let tmp = self.dir.join("bar.strand.new");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, self.config().join("bar.strand")).unwrap();
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("STRAND_SOCKET", self.dir.join("s.sock"))
            .env("XDG_DATA_HOME", self.data())
            .env("HOME", self.dir.join("home"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null());
    }

    /// `program <args>` in this desk's environment, THP back on.
    fn run(&self, program: &Path, args: &[&str]) -> Output {
        let mut cmd = thp_on(program);
        self.env(&mut cmd);
        cmd.args(args).output().unwrap()
    }

    /// The stock `strand <args>`.
    fn strand(&self, args: &[&str]) -> Output {
        self.run(Path::new(env!("CARGO_BIN_EXE_strand")), args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.strand(args);
        assert!(
            out.status.success(),
            "strand {args:?}: {}\n{}",
            text(&out.stderr),
            self.log_text()
        );
        text(&out.stdout)
    }

    fn refused(&self, args: &[&str]) -> String {
        let out = self.strand(args);
        assert!(!out.status.success(), "strand {args:?} succeeded");
        text(&out.stderr)
    }

    /// `plugin_hello --write-schemas` into this desk's data directory.
    fn write_schemas(&self) {
        let out = self.run(&example("plugin_hello"), &["--write-schemas"]);
        let said = text(&out.stdout);
        assert!(out.status.success(), "{said}{}", text(&out.stderr));
        assert!(
            said.contains(&format!("wrote {}", self.schema_file().display())),
            "{said}"
        );
        assert!(
            !text(&out.stderr).contains(WARNING),
            "{}",
            text(&out.stderr)
        );
    }

    /// A headless sway, one 1280x720 output.
    fn sway(&mut self) {
        let cfg = self.dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n",
        )
        .unwrap();
        let log = std::fs::File::create(self.dir.join("sway.log")).unwrap();
        let mut cmd = thp_on(Path::new("sway"));
        let sway = cmd
            .arg("-c")
            .arg(&cfg)
            .env("XDG_RUNTIME_DIR", &self.dir)
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
        self.sway = Some(Proc(sway));
        let deadline = Instant::now() + Duration::from_secs(10);
        self.display = loop {
            let names: Vec<String> = std::fs::read_dir(&self.dir)
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
                    .env("SWAYSOCK", self.dir.join(i))
                    .output()
                    .is_ok_and(|o| o.status.success())
            {
                break d.clone();
            }
            assert!(Instant::now() < deadline, "sway did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
    }

    /// `program run <config>` on the sway, its stderr in the log; waits
    /// until `strand get <ready>` answers.
    fn start(&mut self, program: &Path, ready: &str) {
        self.shell.take();
        let mut cmd = thp_on(program);
        self.env(&mut cmd);
        cmd.arg("run")
            .arg(self.config())
            .env("STRAND_LOG", "info")
            .stderr(std::fs::File::create(self.log()).unwrap());
        if let Some(m) = self.mock {
            cmd.env("STRAND_MOCK", m);
        }
        self.shell = Some(Proc(cmd.spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.strand(&["get", ready]).status.success() {
            if let Some(p) = &mut self.shell {
                assert!(
                    p.0.try_wait().unwrap().is_none(),
                    "the shell exited: {}",
                    self.log_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "the shell did not answer: {}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Polls `strand get <path>` until it reads `want` (10 s).
    fn until(&self, path: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = self.ok(&["get", path]);
            if got.trim_end_matches('\n') == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{path} reads {got:?}, not {want:?}\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `strand watch --json`, its events on a channel once subscribed.
    fn watch(&self) -> (Proc, mpsc::Receiver<serde_json::Value>) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_strand"));
        self.env(&mut cmd);
        let mut child = cmd
            .args(["watch", "--json"])
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

/// Waits (bounded) for a `notices` event with a notice holding every one
/// of `parts`; returns it.
fn heard(events: &mpsc::Receiver<serde_json::Value>, parts: &[&str]) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let ev = events.recv_timeout(left).ok()?;
        let found = ev["notices"].as_array().and_then(|a| {
            a.iter()
                .filter_map(|n| n.as_str())
                .find(|n| parts.iter().all(|p| n.contains(p)))
                .map(str::to_string)
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// A bar reading the plugin's service, its values exported.
const READS_HELLO: &str = "export let greeting = hello.greeting\n\
     export let count = hello.count\n\
     bar B {\n  edge: top; height: 40\n  text greeting\n}\n";

/// A bar that does not read it.
const PLAIN: &str =
    "export state label = \"hi\"\nbar B {\n  edge: top; height: 40\n  text label\n}\n";

/// `plugin_hello --write-schemas`, the stock `strand check` with and
/// without the file, then `plugin_hello run` serving `hello` to the
/// stock `strand call` and `strand get`.
#[test]
fn an_extra_service_is_checked_and_runs() {
    let mut d = Desk::new("run");
    d.save(READS_HELLO);
    // Without the file the stock binary does not know `hello`.
    let out = d.strand(&["check", &d.config().display().to_string()]);
    assert!(!out.status.success(), "checked without the schema file");
    assert!(text(&out.stderr).contains("hello"), "{}", text(&out.stderr));
    d.write_schemas();
    let schema = std::fs::read_to_string(d.schema_file()).unwrap();
    assert!(
        schema.starts_with("// strand plugin schema, written by /")
            && schema.contains("service hello"),
        "{schema}"
    );
    let out = d.strand(&["check", &d.config().display().to_string()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    if !sway_tools() {
        return;
    }
    d.sway();
    d.start(&example("plugin_hello"), "bar.count");
    assert!(!d.log_text().contains(WARNING), "{}", d.log_text());
    d.until("bar.count", "41");
    assert_eq!(d.ok(&["call", "hello.greet", "world"]), "");
    // `call` answers once the action is queued: the service, the pump
    // and the export's recompute may still be pending.
    d.until("bar.greeting", "hello, world");
    d.until("bar.count", "42");
}

/// The stock `strand run` with `hello.schema` present: nothing until a
/// config reads `hello`, then a notice naming the file, defaults for
/// reads, refusals for `set` and `call`, and the notice resolved when
/// the read goes. A boot that reads it logs the notice.
#[test]
fn the_stock_strand_reads_plugin_schemas() {
    if !sway_tools() {
        return;
    }
    let mut d = Desk::new("stock");
    d.write_schemas();
    let file = d.schema_file().display().to_string();
    d.save(PLAIN);
    d.sway();
    let strand = PathBuf::from(env!("CARGO_BIN_EXE_strand"));
    d.start(&strand, "bar.label");
    let log = d.log_text();
    assert!(
        log.contains(&format!("{file} declares service `hello`")),
        "{log}"
    );
    assert!(!log.contains("does not serve it"), "{log}");
    // Service notices reach the watchers connected when they are taken:
    // the read comes in by a reload.
    let (_watch, events) = d.watch();
    d.save(READS_HELLO);
    let notice = heard(&events, &["`hello` is declared by", &file]);
    assert!(notice.is_some(), "no notice\n{}", d.log_text());
    d.until("bar.greeting", "");
    let why = d.refused(&["call", "hello.greet", "world"]);
    assert!(
        why.contains("does not serve it") && why.contains(&file),
        "{why}"
    );
    let why = d.refused(&["set", "hello.count", "1"]);
    assert!(
        why.contains("does not serve it") && why.contains(&file),
        "{why}"
    );
    d.save(PLAIN);
    assert!(
        heard(&events, &["resolved:", &file]).is_some(),
        "not resolved\n{}",
        d.log_text()
    );
    // Booted on a config reading it: the notice is in the log.
    d.save(READS_HELLO);
    d.start(&strand, "bar.count");
    let log = d.log_text();
    assert!(
        log.contains("does not serve it") && log.contains(&file),
        "{log}"
    );
}

/// The stock `strand run` under `STRAND_MOCK=desktop` with
/// `hello.schema` present: a config reading `hello` boots and reads its
/// defaults (decisions.md m5-plugins, review round 2: the mock host had
/// no cells for it and the mount failed).
#[test]
fn the_mock_reads_plugin_schemas() {
    if !sway_tools() {
        return;
    }
    let mut d = Desk::new("mock");
    d.write_schemas();
    d.save(READS_HELLO);
    d.sway();
    d.mock = Some("desktop");
    d.start(Path::new(env!("CARGO_BIN_EXE_strand")), "bar.count");
    d.until("bar.count", "0");
    d.until("bar.greeting", "");
    let log = d.log_text();
    assert!(!log.contains("has no field"), "{log}");
    let file = d.schema_file().display().to_string();
    assert!(
        log.contains(&format!("{file} declares service `hello`")),
        "{log}"
    );
}

/// Plugin schema files that are skipped (one not UTF-8, one that does
/// not apply) next to a good one: the stock `strand check` prints each
/// as a warning, counts it and still passes, and the stock `strand run`
/// logs each (decisions.md m5-plugins, review round 1 item 2).
#[test]
fn skipped_plugin_files_are_reported() {
    let mut d = Desk::new("skip");
    d.write_schemas();
    let dir = d.schema_file().parent().unwrap().to_path_buf();
    let not_utf8 = dir.join("broken.schema");
    std::fs::write(&not_utf8, b"service broken {\xff\n}\n").unwrap();
    let bad = dir.join("bad.schema");
    std::fs::write(&bad, "/// Bad.\nservice bad {\n").unwrap();
    d.save(READS_HELLO);
    let out = d.strand(&["check", &d.config().display().to_string()]);
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{said}");
    for path in [&not_utf8, &bad] {
        assert!(
            said.contains(&format!("warning: plugin schema: {}", path.display())),
            "{said}"
        );
    }
    assert!(said.contains("0 errors, 2 warnings"), "{said}");
    if !sway_tools() {
        return;
    }
    d.save(PLAIN);
    d.sway();
    d.start(&PathBuf::from(env!("CARGO_BIN_EXE_strand")), "bar.label");
    let log = d.log_text();
    for path in [&not_utf8, &bad] {
        assert!(log.contains(&path.display().to_string()), "{log}");
    }
}

/// A plugin binary without `strand::process_setup!()` says so; one with
/// it does not.
#[test]
fn the_setup_warning_shows_without_the_macro() {
    let d = Desk::new("setup");
    let out = d.run(&example("plugin_bare"), &["--version"]);
    assert!(out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains(WARNING), "{err}");
    let out = d.run(&example("plugin_hello"), &["--version"]);
    assert!(out.status.success());
    assert!(
        !text(&out.stderr).contains(WARNING),
        "{}",
        text(&out.stderr)
    );
    // The stock binary registers nothing to write.
    let out = d.strand(&["--write-schemas"]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("registers no plugin services"),
        "{}",
        text(&out.stdout)
    );
}

/// The `[profile.release*]` sections of a TOML text, without comments or
/// blank lines.
fn release_profile(toml: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line.starts_with("[profile.release");
        }
        if inside && !line.is_empty() && !line.starts_with('#') {
            out.push(line.to_string());
        }
    }
    out
}

/// The guide's plugin page shows the workspace's release profile, so a
/// plugin built from it keeps the size and speed figures.
#[test]
fn the_guide_profile_matches_the_workspace() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let guide = std::fs::read_to_string(root.join("docs/guide/plugins.md")).unwrap();
    let block = guide
        .split("```toml")
        .skip(1)
        .filter_map(|b| b.split("```").next())
        .find(|b| b.contains("[profile.release]"))
        .expect("docs/guide/plugins.md has a toml block with [profile.release]");
    let want = release_profile(&cargo);
    assert!(want.len() > 4, "{want:?}");
    assert_eq!(
        release_profile(block),
        want,
        "docs/guide/plugins.md's profile differs from the workspace's Cargo.toml"
    );
}
