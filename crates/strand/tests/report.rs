//! (report) `strand report words…` end to end (design.md, "Reporting a
//! glitch"; docs/architecture.md, "Glitch reports"): `strand run` on a
//! headless sway with a virtual pointer and keyboard, a `from file`
//! service, a bar with a button and two `keyboard: exclusive` panels (a
//! text input, a password input); the bundle a report saves is read
//! back part by part.
//!
//! Skipped, loudly, without sway or grim (CI sets `STRAND_REQUIRE_SWAY`).

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
use serde_json::Value as Json;
use strand_core::flight::{self, Rec, kind};
use strand_services::testing::PrivateBus;
use support::keyboard::{self, CONTROL};
use support::pointer::Pointer;

/// What the virtual keyboard types (it has letters only) and what is
/// pasted: neither may be in any file of a bundle.
const TYPED: &str = "hunterqz";
const PASTED: &str = "hunter2-Z9q";

fn config(mood: &Path, big: &Path, small: &Path) -> String {
    format!(
        "export state clicks = 0
export state q = \"\"
export state pw = \"\"
export state pw_mode = false
service mood from file \"{mood}\" {{ level: int }}
export let level = mood.level
export let recording = system.replay
bar Top {{
  edge: top; height: 40
  row {{
    box {{ width: 80; height: 40; bg: clicks > 0 ? #00ff00 : #0000ff; on click {{ clicks = clicks + 1 }} }}
    box {{ width: 80; height: 40; bg: mood.level == 7 ? #ff0000 : #202020 }}
    image \"{small}\" {{ width: 20; height: 20 }}
    image \"{big}\" {{ width: 20; height: 20 }}
  }}
}}
panel Ask {{
  anchor: center; keyboard: exclusive; open: !pw_mode
  col {{ width: 300; pad: 8
    input {{ text: <-> q; focus: true }}
  }}
}}
panel Secret {{
  anchor: center; keyboard: exclusive; open: pw_mode
  col {{ width: 300; pad: 8
    input {{ type: password; text: <-> pw; focus: true }}
  }}
}}
",
        mood = mood.display(),
        big = big.display(),
        small = small.display(),
    )
}

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
    pointer: Pointer,
    _bus: PrivateBus,
    _sway: Proc,
    dir: PathBuf,
    display: String,
    log: PathBuf,
    /// Leave `dir` behind (the replay end to end reads its files).
    keep: bool,
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.strand.take();
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn tools() -> bool {
    for tool in ["sway", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("\n*** SKIPPED: {tool} is not installed; the report test did not run ***\n");
            return false;
        }
    }
    true
}

/// A PNG of `w`×`h` noise (incompressible): `w`×`h`×4 bytes and more.
fn noise_png(path: &Path, w: u32, h: u32) {
    let mut seed: u32 = 0x9e37_79b9;
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h * 4 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        px.push(seed as u8);
    }
    let f = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&px).unwrap();
}

impl Desk {
    fn start(tag: &str) -> Option<Desk> {
        Self::with(tag, "", &[])
    }

    /// [`Desk::start`] with `extra` appended to the config and `files`
    /// beside it.
    fn with(tag: &str, extra: &str, files: &[(&str, &str)]) -> Option<Desk> {
        if !tools() {
            return None;
        }
        // `system` (and so `system.replay`) runs on a session bus.
        let bus = PrivateBus::start()?;
        // Short: the socket paths must fit in sun_path.
        let dir = std::env::temp_dir().join(format!("strand-rep-{tag}-{}", std::process::id()));
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
        // The keyboard and pointer join the seat before strand connects.
        let keys = keyboard::Keyboard::new(&dir.join(&display), &dir);
        let pointer = Pointer::new(&dir.join(&display));
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("mood.json"), r#"{"level": 1}"#).unwrap();
        // Over the 2 MiB a copied file may have; and a small one.
        noise_png(&data.join("big.png"), 800, 800);
        noise_png(&data.join("small.png"), 8, 8);
        let config = dir.join("config");
        std::fs::create_dir_all(config.join("icons")).unwrap();
        std::fs::write(
            config.join("icons/mark.svg"),
            "<svg xmlns='http://www.w3.org/2000/svg'/>",
        )
        .unwrap();
        std::fs::write(
            config.join("report.strand"),
            format!("{}{extra}", config_text(&data)),
        )
        .unwrap();
        for (name, text) in files {
            std::fs::write(config.join(name), text).unwrap();
        }
        let mut desk = Desk {
            strand: None,
            keys,
            pointer,
            _bus: bus,
            _sway: sway,
            log: dir.join("strand.log"),
            display,
            dir,
            keep: false,
        };
        let child = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .envs(desk.env())
            .env("HOME", desk.dir.join("home"))
            .env("XDG_CACHE_HOME", desk.dir.join("cache"))
            .env("XDG_STATE_HOME", desk.dir.join("state"))
            .env("STRAND_LOG", "damage")
            .env_remove("STRAND_STATE_DIR")
            .envs(desk._bus.env())
            .env_remove("STRAND_MOCK")
            .env_remove("SWAYSOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&desk.log).unwrap())
            .spawn()
            .unwrap();
        desk.strand = Some(Proc(child));
        desk.wait("strand answers", |d| d.get("report.clicks").is_some());
        desk.wait("the bar and the panel show", |d| {
            d.log_text()
                .lines()
                .filter(|l| l.starts_with("strand: damage"))
                .count()
                >= 2
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

    fn reports(&self) -> PathBuf {
        self.dir.join("state/strand/reports")
    }

    fn data(&self) -> PathBuf {
        self.dir.join("data")
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

    fn ok(&self, args: &[&str]) -> String {
        let out = self.strand(args);
        assert!(
            out.status.success(),
            "strand {args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_text()
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn get(&self, path: &str) -> Option<String> {
        let out = self.strand(&["get", path]);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// `strand report --json <words>`: the answer.
    fn report(&self, words: &str) -> Json {
        let mut args = vec!["report", "--json"];
        args.extend(words.split(' '));
        let out = self.ok(&args);
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{out}: {e}"))
    }

    /// The scripted session: hover and click the button, a service
    /// update, typing and a paste into the text input, typing into the
    /// password input.
    fn session(&mut self) {
        self.pointer.motion(200, 300, 1280, 720);
        self.pointer.motion(30, 20, 1280, 720);
        self.pointer.motion(40, 20, 1280, 720);
        self.pointer.click(40, 20, 1280, 720);
        self.wait("the click counted", |d| {
            d.get("report.clicks").as_deref() == Some("1")
        });
        std::fs::write(self.data().join("mood.json"), r#"{"level": 7}"#).unwrap();
        self.wait("the service updated", |d| {
            d.get("report.level").as_deref() == Some("7")
        });
        self.keys.type_text(TYPED);
        self.wait("typed", |d| d.get("report.q").as_deref() == Some(TYPED));
        let other = DataControl::connect(
            wayland_client::Connection::from_socket(UnixStream::connect(self.socket()).unwrap())
                .unwrap(),
        )
        .expect("sway 1.9 offers wlr-data-control");
        other.copy_text(PASTED);
        assert!(other.wait(Duration::from_secs(10), |s| !s.mimes.is_empty()));
        self.keys.press_with(CONTROL, "v");
        let want = format!("{TYPED}{PASTED}");
        self.wait("pasted", |d| {
            d.get("report.q").as_deref() == Some(want.as_str())
        });
        self.ok(&["set", "report.pw_mode", "true"]);
        std::thread::sleep(Duration::from_millis(500));
        self.keys.type_text(TYPED);
        self.wait("typed the password", |d| {
            d.get("report.pw").as_deref() == Some(TYPED)
        });
    }
}

fn config_text(data: &Path) -> String {
    config(
        &data.join("mood.json"),
        &data.join("big.png"),
        &data.join("small.png"),
    )
}

/// Every file under `dir`.
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn read_json(p: &Path) -> Json {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn decode_png(p: &Path) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(p).unwrap()));
    let mut dec = dec;
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Every part of a bundle, the recording in order, the paste line, and
/// the modes of its files; bare `strand report` stays the diagnostics.
#[test]
fn a_bundle_holds_every_part() {
    let Some(mut desk) = Desk::start("parts") else {
        return;
    };
    desk.session();
    let ans = desk.report("the bar dipped");
    assert_eq!(ans["ok"], true, "{ans}");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    assert!(dir.starts_with(desk.reports()), "{}", dir.display());
    // The paste line names the bundle and what to run.
    let paste = ans["paste"].as_str().unwrap();
    assert_eq!(
        paste,
        format!(
            "Strand glitch report \"the bar dipped\" at {d}: run scripts/report-view.sh \
             --no-open {d} in the strand checkout and read {d}/replay/replay.txt.",
            d = dir.display()
        )
    );
    let text = desk.ok(&["report", "it", "flickered"]);
    assert!(text.starts_with("saved a glitch report to "), "{text}");
    assert!(
        text.contains("Strand glitch report \"it flickered\" at "),
        "{text}"
    );
    // Every part.
    for part in [
        "bundle.json",
        "words.txt",
        "diagnostics.json",
        "diagnostics.txt",
        "input.flight",
        "changes.flight",
        "services.json",
        "cells.json",
        "nodes.json",
        "config/report.strand",
        "config/icons/mark.svg",
    ] {
        assert!(dir.join(part).is_file(), "{part} in {}", dir.display());
    }
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    let mut all = Vec::new();
    files(&dir, &mut all);
    for f in &all {
        assert_eq!(mode(f), 0o600, "{}", f.display());
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("words.txt")).unwrap(),
        "the bar dipped\n"
    );
    let b = read_json(&dir.join("bundle.json"));
    assert_eq!(b["format"], 1);
    assert_eq!(b["words"], "the bar dipped");
    assert_eq!(b["replay"], true);
    assert_eq!(b["strand"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        b["strand"]["commit"]
            .as_str()
            .is_some_and(|c| !c.is_empty()),
        "{b}"
    );
    assert!(b["wall"].as_str().is_some_and(|w| w.contains('T')), "{b}");
    assert_eq!(b["outputs"][0]["name"], "HEADLESS-1");
    // The config's files byte for byte, and their hashes.
    let config = desk.dir.join("config");
    assert_eq!(
        std::fs::read(dir.join("config/report.strand")).unwrap(),
        std::fs::read(config.join("report.strand")).unwrap()
    );
    let key = config.join("report.strand").display().to_string();
    assert!(b["files"][&key].is_string(), "{b}");
    // A small asset is copied at its own path; the big one is missing.
    let small = desk.data().join("small.png");
    let at = dir.join("files").join(small.strip_prefix("/").unwrap());
    assert_eq!(std::fs::read(&at).unwrap(), std::fs::read(&small).unwrap());
    // The diagnostics are what bare `strand report` prints.
    let d = read_json(&dir.join("diagnostics.json"));
    for k in [
        "strand",
        "compositor",
        "outputs",
        "surfaces",
        "gpu",
        "memory",
    ] {
        assert!(d.get(k).is_some(), "{k} in {d}");
    }
    let bare = desk.ok(&["report"]);
    assert!(
        bare.starts_with("strand ") && bare.contains("\ncompositor: "),
        "{bare}"
    );
    let txt = std::fs::read_to_string(dir.join("diagnostics.txt")).unwrap();
    assert!(
        txt.starts_with("strand ") && txt.contains("\nsurfaces:\n"),
        "{txt}"
    );
    // Services and cells at the report.
    let s = read_json(&dir.join("services.json"));
    assert_eq!(s["mood"]["fields"]["level"], 7, "{s}");
    assert!(s.get("auth").is_none());
    let c = read_json(&dir.join("cells.json"));
    assert_eq!(c["report.clicks"], 1, "{c}");
    assert_eq!(c["report.pw_mode"], true, "{c}");
    // A PNG per mapped surface, at its buffer size.
    let surfaces = b["surfaces"].as_array().unwrap();
    assert!(surfaces.len() >= 2, "{b}");
    // The nodes the input names, each surface's root among them, by
    // their origin under the config directory.
    let n = read_json(&dir.join("nodes.json"));
    for s in surfaces {
        let root = s["root"].as_str().unwrap();
        let o = n[root]["origin"]
            .as_str()
            .unwrap_or_else(|| panic!("{root} in {n}"));
        assert!(o.starts_with("report.strand:"), "{o}");
    }
    for s in surfaces {
        let file = s["frame"]
            .as_str()
            .unwrap_or_else(|| panic!("a frame: {s}"));
        let (w, h, _) = decode_png(&dir.join(file));
        assert_eq!(serde_json::json!([w, h]), s["buffer"], "{s}");
    }
    // The input as it came, in order: the hover, the press and its hit,
    // the release; the surfaces mapped first.
    let input = flight::read(&std::fs::read(dir.join("input.flight")).unwrap()).unwrap();
    let e = input.entries();
    let kinds: Vec<u8> = e.iter().map(|e| e.rec.kind).collect();
    let first = |k: u8| kinds.iter().position(|x| *x == k);
    let (mapped, motion, button) = (
        first(kind::MAPPED).unwrap(),
        first(kind::MOTION).unwrap(),
        first(kind::BUTTON).unwrap(),
    );
    assert!(mapped < motion && motion < button, "{kinds:?}");
    assert_eq!(kinds[button + 1], kind::HIT, "{kinds:?}");
    assert!(
        kinds[button + 2..].contains(&kind::BUTTON),
        "the release: {kinds:?}"
    );
    assert!(
        e.windows(2).all(|w| w[0].rec.t <= w[1].rec.t),
        "times in order"
    );
    assert!(
        kinds.contains(&kind::TEXT) && kinds.contains(&kind::PASTE),
        "{kinds:?}"
    );
    // The changes: the service update with its old value, the click's
    // cell write.
    let changes = flight::read(&std::fs::read(dir.join("changes.flight")).unwrap()).unwrap();
    let ce = changes.entries();
    let moods: Vec<Json> = ce
        .iter()
        .filter(|e| e.rec.kind == kind::FIELD && e.name.as_deref() == Some("mood.level"))
        .map(|e| serde_json::from_slice(e.value.as_ref().unwrap()).unwrap())
        .collect();
    assert_eq!(
        moods,
        [serde_json::json!([null, 1]), serde_json::json!([1, 7])]
    );
    let clicks = ce
        .iter()
        .find(|e| e.rec.kind == kind::CELL && e.name.as_deref() == Some("report.clicks"))
        .unwrap();
    let v: Json = serde_json::from_slice(clicks.value.as_ref().unwrap()).unwrap();
    assert_eq!(v, serde_json::json!([0, 1]));
    assert!(ce.iter().any(|e| e.rec.kind == kind::ROOT), "surface roots");
}

/// What was typed and pasted, in an input and a password input, is in
/// no file of the bundle, as UTF-8 or UTF-16, and no key typed is named.
#[test]
fn a_typed_secret_is_in_no_file_of_the_bundle() {
    let Some(mut desk) = Desk::start("secret") else {
        return;
    };
    desk.session();
    let ans = desk.report("typing");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let mut all = Vec::new();
    files(&dir, &mut all);
    assert!(all.len() > 8);
    for f in &all {
        let bytes = std::fs::read(f).unwrap();
        for s in [TYPED, PASTED, "hunter"] {
            assert!(!contains(&bytes, s.as_bytes()), "{s} in {}", f.display());
            assert!(
                !contains(&bytes, &utf16(s)),
                "{s} (UTF-16) in {}",
                f.display()
            );
        }
    }
    // No letter typed is a name of the input ring.
    let input = flight::read(&std::fs::read(dir.join("input.flight")).unwrap()).unwrap();
    for n in &input.names {
        assert!(
            n.chars().count() > 1 || !TYPED.contains(n.as_str()),
            "{n} named"
        );
    }
    // The texts are lengths.
    let c = read_json(&dir.join("cells.json"));
    assert_eq!(
        c["report.q"],
        serde_json::json!({"~text": TYPED.len() + PASTED.len()}),
        "{c}"
    );
    // (report review) A password's cell is in no file, not even as a
    // length: neither in cells.json nor in the changes ring.
    assert!(c.get("report.pw").is_none(), "{c}");
    let changes = flight::read(&std::fs::read(dir.join("changes.flight")).unwrap()).unwrap();
    for e in changes.entries() {
        assert_ne!(e.name.as_deref(), Some("report.pw"), "{e:?}");
    }
    // The panel it is typed into was opened by `strand set`: a request.
    assert!(
        changes
            .entries()
            .iter()
            .any(|e| e.rec.kind == kind::REQUEST && e.name.as_deref() == Some("report.pw_mode")),
        "no request record"
    );
}

/// `system.replay` off drops the recording (a bundle then has none) and
/// is kept; on again records anew, from a gap and what is shown now.
#[test]
fn replay_off_drops_the_rings_and_bundles_have_no_recording() {
    let Some(mut desk) = Desk::start("off") else {
        return;
    };
    desk.ok(&["set", "system.replay", "false"]);
    desk.wait("recording off", |d| {
        d.get("report.recording").as_deref() == Some("false")
    });
    let ans = desk.report("off");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    assert!(!dir.join("input.flight").exists() && !dir.join("changes.flight").exists());
    assert_eq!(read_json(&dir.join("bundle.json"))["replay"], false);
    assert!(dir.join("services.json").is_file() && dir.join("frames").is_dir());
    assert_eq!(desk.get("report.recording").as_deref(), Some("false"));
    desk.ok(&["set", "system.replay", "true"]);
    desk.wait("recording on", |d| {
        d.get("report.recording").as_deref() == Some("true")
    });
    desk.pointer.motion(30, 20, 1280, 720);
    std::thread::sleep(Duration::from_millis(200));
    let ans = desk.report("on");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let input = flight::read(&std::fs::read(dir.join("input.flight")).unwrap()).unwrap();
    // (report review) Started anew: nothing from before is in it. It
    // opens with the gap's end, and the surfaces mapped now are written
    // again at that instant, so a replay of it knows what is shown;
    // no mapping from before the gap is kept.
    let entries = input.entries();
    let first = entries.first().expect("an empty ring");
    assert_eq!(first.rec.kind, kind::GAP_ENDS, "{first:?}");
    assert_eq!(first.rec.u8(0), flight::gap::OFF);
    let maps: Vec<_> = entries
        .iter()
        .filter(|e| e.rec.kind == kind::MAPPED)
        .collect();
    assert!(!maps.is_empty(), "the shown surfaces are not written again");
    assert!(
        maps.iter().all(|e| e.rec.t == first.rec.t),
        "a mapping from before the gap is kept: {maps:?}"
    );
    let changes = flight::read(&std::fs::read(dir.join("changes.flight")).unwrap()).unwrap();
    let first = changes.entries().first().cloned().expect("an empty ring");
    assert_eq!(first.rec.kind, kind::CHANGE_GAP_ENDS, "{first:?}");
}

/// A file over 2 MiB is listed missing with its size; a small one is
/// copied.
#[test]
fn an_asset_over_the_cap_is_listed_missing() {
    let Some(desk) = Desk::start("cap") else {
        return;
    };
    std::thread::sleep(Duration::from_millis(500));
    let ans = desk.report("assets");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let b = read_json(&dir.join("bundle.json"));
    let big = desk.data().join("big.png");
    let size = std::fs::metadata(&big).unwrap().len();
    assert!(size > 2 * 1024 * 1024);
    let missing = b["missing"].as_array().unwrap();
    let m = missing
        .iter()
        .find(|m| m["path"] == big.display().to_string())
        .unwrap_or_else(|| panic!("big.png missing: {b}"));
    assert_eq!(m["bytes"], size);
    assert!(m["why"].as_str().unwrap().starts_with("over 2 MiB"), "{m}");
    assert!(
        !dir.join("files")
            .join(big.strip_prefix("/").unwrap())
            .exists()
    );
    let small = desk.data().join("small.png");
    assert!(b["files"][small.display().to_string()].is_string(), "{b}");
}

/// Twenty-one reports keep the newest twenty; a directory that is not a
/// report is left alone.
#[test]
fn the_twenty_first_report_prunes_the_oldest_and_nothing_else() {
    let Some(desk) = Desk::start("prune") else {
        return;
    };
    let reports = desk.reports();
    std::fs::create_dir_all(reports.join("notes")).unwrap();
    std::fs::write(reports.join("notes/mine.txt"), "keep").unwrap();
    let mut made = Vec::new();
    for i in 0..21 {
        let ans = desk.report(&format!("report {i}"));
        made.push(PathBuf::from(ans["bundle"].as_str().unwrap()));
    }
    assert!(!made[0].exists(), "the oldest went");
    for d in &made[1..] {
        assert!(d.join("bundle.json").is_file(), "{} kept", d.display());
    }
    assert_eq!(
        std::fs::read_to_string(reports.join("notes/mine.txt")).unwrap(),
        "keep"
    );
    let count = std::fs::read_dir(&reports).unwrap().count();
    assert_eq!(count, 21, "twenty reports and the notes");
}

/// Each surface's PNG is the frame the compositor shows (its own
/// capture, `grim`), within the CPU's tolerance.
#[test]
fn each_frame_png_is_the_frame_shown() {
    let Some(mut desk) = Desk::start("frames") else {
        return;
    };
    desk.pointer.click(40, 20, 1280, 720);
    desk.wait("the click counted", |d| {
        d.get("report.clicks").as_deref() == Some("1")
    });
    // The cursor off the bar (the screen shows it), and the colour's
    // spring over.
    desk.pointer.motion(640, 500, 1280, 720);
    std::thread::sleep(Duration::from_millis(1500));
    let ans = desk.report("frames");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let b = read_json(&dir.join("bundle.json"));
    let bar = b["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "bar")
        .unwrap()
        .clone();
    let (w, h, ours) = decode_png(&dir.join(bar["frame"].as_str().unwrap()));
    let shot = desk.dir.join("shot.png");
    let ok = Command::new("grim")
        .args(["-g", &format!("0,0 {w}x{h}")])
        .arg(&shot)
        .envs(desk.env())
        .status()
        .unwrap();
    assert!(ok.success(), "grim");
    let (sw, sh, theirs) = decode_png(&shot);
    assert_eq!((sw, sh), (w, h));
    // The screen shows the surface over what is under it: its opaque
    // pixels are the frame's own.
    let (mut off, mut n) = (0usize, 0usize);
    for (a, b) in ours.chunks_exact(4).zip(theirs.chunks_exact(4)) {
        if a[3] != 255 {
            continue;
        }
        n += 1;
        if a[..3].iter().zip(&b[..3]).any(|(x, y)| x.abs_diff(*y) > 2) {
            off += 1;
        }
    }
    assert!(n * 2 > (w * h) as usize, "mostly opaque: {n}");
    assert!(off * 1000 <= n, "{off} of {n} opaque pixels differ");
    // The button is green, as clicked.
    let px = &ours[(20 * w as usize + 40) * 4..][..3];
    assert!(px[1] > 200 && px[0] < 40 && px[2] < 40, "{px:?}");
}

/// (report) The session `scripts/eval/test-report-view.sh --e2e`
/// replays: the pointer in, a hover on the second box, a click on the
/// button, a service update, then the springs at rest and a report. With
/// `STRAND_REPORT_KEEP=DIR` the run's directory is left behind (the
/// replay reads its files where the shell read them) and the bundle's
/// path is written to `DIR/bundle`; without it the bundle is only read
/// back.
#[test]
fn a_scripted_session_is_kept_for_its_replay() {
    let Some(mut desk) = Desk::start("replay") else {
        return;
    };
    let keep = std::env::var_os("STRAND_REPORT_KEEP").map(PathBuf::from);
    desk.keep = keep.is_some();
    desk.pointer.motion(30, 20, 1280, 720);
    desk.pointer.motion(120, 20, 1280, 720);
    std::thread::sleep(Duration::from_millis(400));
    desk.pointer.motion(40, 20, 1280, 720);
    desk.pointer.click(40, 20, 1280, 720);
    desk.wait("the click counted", |d| {
        d.get("report.clicks").as_deref() == Some("1")
    });
    std::fs::write(desk.data().join("mood.json"), r#"{"level": 7}"#).unwrap();
    desk.wait("the service updated", |d| {
        d.get("report.level").as_deref() == Some("7")
    });
    // (report review) As a compositor keybinding would: `strand toggle`
    // opens the password panel, which is then typed into. A replay
    // repeats the toggle, so the typing has its panel.
    desk.ok(&["toggle", "report.pw_mode"]);
    std::thread::sleep(Duration::from_millis(500));
    desk.keys.type_text(TYPED);
    desk.wait("typed the password", |d| {
        d.get("report.pw").as_deref() == Some(TYPED)
    });
    std::thread::sleep(Duration::from_millis(1500));
    let ans = desk.report("the session to replay");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let dump = std::fs::read(dir.join("input.flight")).unwrap();
    let kinds: Vec<u8> = flight::read(&dump)
        .unwrap()
        .records
        .iter()
        .map(|r| r.kind)
        .collect();
    for k in [kind::MAPPED, kind::ENTER, kind::BUTTON, kind::HIT] {
        assert!(kinds.contains(&k), "no record of kind {k:#x}");
    }
    let nodes = read_json(&dir.join("nodes.json"));
    assert!(!nodes.as_object().unwrap().is_empty());
    let changes = flight::read(&std::fs::read(dir.join("changes.flight")).unwrap()).unwrap();
    assert!(
        changes.entries().iter().any(|e| e.rec.kind == kind::REQUEST
            && e.name.as_deref() == Some("report.pw_mode")
            && e.rec.u8(0) == flight::request::TOGGLE),
        "no toggle request"
    );
    if let Some(k) = keep {
        std::fs::create_dir_all(&k).unwrap();
        std::fs::write(k.join("bundle"), dir.display().to_string()).unwrap();
        std::fs::write(k.join("run"), desk.dir.display().to_string()).unwrap();
    }
}

/// The words form prints the paste line, which names a whole bundle and
/// what to read in it.
#[test]
fn the_paste_line_names_the_bundle() {
    let Some(desk) = Desk::start("paste") else {
        return;
    };
    let out = desk.ok(&["report", "the", "clock", "jumped"]);
    let line = out
        .lines()
        .find(|l| l.starts_with("Strand glitch report"))
        .unwrap_or_else(|| panic!("{out}"));
    let dir = line
        .strip_prefix("Strand glitch report \"the clock jumped\" at ")
        .and_then(|r| r.split(": run ").next())
        .unwrap();
    assert!(Path::new(dir).join("bundle.json").is_file(), "{line}");
    assert!(
        line.ends_with(&format!("read {dir}/replay/replay.txt.")),
        "{line}"
    );
}

/// The input as it came on a headless sway, from a virtual pointer and
/// keyboard: the surfaces mapped first, then the hover, the press and
/// its hit, the release, the keys counted, the paste; times in order.
#[test]
fn input_on_a_headless_sway_is_recorded_in_order() {
    let Some(mut desk) = Desk::start("input") else {
        return;
    };
    desk.session();
    let ans = desk.report("input");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let input = flight::read(&std::fs::read(dir.join("input.flight")).unwrap()).unwrap();
    let e = input.entries();
    assert!(
        e.windows(2).all(|w| w[0].rec.t <= w[1].rec.t),
        "times in order"
    );
    let kinds: Vec<u8> = e.iter().map(|e| e.rec.kind).collect();
    let first = |k: u8| kinds.iter().position(|x| *x == k);
    let mapped = first(kind::MAPPED).expect("a surface mapped");
    let configured = first(kind::CONFIGURED).expect("configured");
    let output = first(kind::OUTPUT).expect("the output");
    let motion = first(kind::MOTION).expect("a move");
    let button = first(kind::BUTTON).expect("a press");
    assert!(output < mapped && mapped < configured, "{kinds:?}");
    assert!(configured < motion && motion < button, "{kinds:?}");
    // The press, at the button, then its hit, then the release.
    let press = &e[button].rec;
    assert_eq!(press.u8(10), 1, "pressed");
    assert_eq!(press.u16(8), 0, "the left button");
    assert!((press.f32(0) - 40.0).abs() < 1.0 && (press.f32(4) - 20.0).abs() < 1.0);
    assert_eq!(kinds[button + 1], kind::HIT, "{kinds:?}");
    let release = kinds[button + 2..]
        .iter()
        .position(|k| *k == kind::BUTTON)
        .map(|i| &e[button + 2 + i].rec)
        .expect("the release");
    assert_eq!(release.u8(10), 0);
    // The keys typed into the input: one count of them all; the paste a
    // count of its characters; the password's keys a span with no
    // count.
    let texts: Vec<&Rec> = e
        .iter()
        .filter(|e| e.rec.kind == kind::TEXT)
        .map(|e| &e.rec)
        .collect();
    let counted: u32 = texts
        .iter()
        .filter(|r| r.u8(8) == 0)
        .map(|r| r.u32(0))
        .sum();
    assert_eq!(counted as usize, TYPED.len(), "{texts:?}");
    assert!(
        texts.iter().any(|r| r.u8(8) == 1 && r.u32(0) == 0),
        "{texts:?}"
    );
    let paste = e.iter().find(|e| e.rec.kind == kind::PASTE).unwrap();
    assert_eq!(paste.rec.u32(0) as usize, PASTED.len());
    // Ctrl+V is named.
    assert!(
        e.iter().any(|e| e.rec.kind == kind::KEY
            && e.name.as_deref() == Some("v")
            && e.rec.u16(2) & strand_core::flight::modifiers::CTRL != 0),
        "{:?}",
        e.iter()
            .filter(|e| e.rec.kind == kind::KEY)
            .collect::<Vec<_>>()
    );
}

const AURORA: &str = "\
@group(1) @binding(0) var<uniform> u_speed: f32;
@fragment
fn main(v: StrandVertex) -> @location(0) vec4<f32> {
    let t = fract(strand.time * u_speed + v.uv.x);
    return vec4<f32>(t * 0.5, 0.2, 0.6 * (1.0 - t), 1.0);
}
";

/// A surface the GPU presents (an animated shader promotes it) is
/// repainted on the CPU for its PNG: at its buffer size, its boxes in
/// place, and marked so in `bundle.json`.
#[test]
fn a_gpu_surface_is_repainted_on_the_cpu() {
    if !cfg!(feature = "gpu") {
        eprintln!("\n*** SKIPPED: built without the GPU backend ***\n");
        return;
    }
    let glow = "panel Glow { anchor: bottom_left; width: 640; height: 320; open: true
  box { width: 640; height: 320
    shader \"aurora.wgsl\" { width: 640; height: 320; u_speed: 1 }
    box { place: absolute; x: 20; y: 20; width: 60; height: 60; bg: #f5c2e7 }
  }
}
";
    let Some(mut desk) = Desk::with("gpu", glow, &[("aurora.wgsl", AURORA)]) else {
        return;
    };
    let promoted = |d: &Desk| {
        let r: Json = serde_json::from_str(d.ok(&["report", "--json"]).trim()).unwrap();
        let log = d.log_text();
        if log.contains("everything draws on the CPU") {
            return Some(false);
        }
        r["surfaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["backend"].as_str().is_some_and(|b| b.starts_with("gpu")))
            .then_some(true)
    };
    desk.wait("the shader promoted its panel, or no device", |d| {
        promoted(d).is_some()
    });
    if promoted(&desk) == Some(false) {
        assert!(
            std::env::var_os("STRAND_REQUIRE_GPU").is_none(),
            "STRAND_REQUIRE_GPU is set but:\n{}",
            desk.log_text()
        );
        eprintln!("\n*** SKIPPED: no GPU device ***\n");
        return;
    }
    let ans = desk.report("gpu");
    let dir = PathBuf::from(ans["bundle"].as_str().unwrap());
    let b = read_json(&dir.join("bundle.json"));
    let glow = b["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["namespace"].as_str().is_some_and(|n| n.ends_with("Glow")))
        .unwrap_or_else(|| panic!("Glow in {b}"))
        .clone();
    assert!(
        glow["backend"].as_str().unwrap().starts_with("gpu"),
        "{glow}"
    );
    assert_eq!(glow["repainted"], true, "{glow}");
    let (w, h, px) = decode_png(&dir.join(glow["frame"].as_str().unwrap()));
    assert_eq!(serde_json::json!([w, h]), glow["buffer"]);
    assert_eq!((w, h), (640, 320));
    let at = |x: usize, y: usize| &px[(y * w as usize + x) * 4..][..4];
    assert_eq!(at(50, 50), [0xf5, 0xc2, 0xe7, 0xff], "the box in place");
    // The bar, on the CPU, is its own buffer's copy.
    let bar = b["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "bar")
        .unwrap()
        .clone();
    assert_eq!(bar["backend"], "cpu");
    assert_eq!(bar["repainted"], false);
}
