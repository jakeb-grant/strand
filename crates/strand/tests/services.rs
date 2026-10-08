//! The real services end to end (M3): `strand run` on a headless sway,
//! without `STRAND_MOCK`, so the config reads the composite real host. A
//! private `dbus-daemon` carries a mock XDG portal answering "dark"; the
//! config's bar is red when `system.dark` and blue otherwise, and holds a
//! green box `10 + cpu.usage * 1000` px wide. While the test keeps every
//! core busy, a green box wider than the load since boot allows can only
//! come from the real `cpu` (procfs, sampled while visible) and `system`
//! (the portal on the services runtime) services: the schema defaults
//! would draw a 10 px box on blue, and the first reading (the load since
//! boot) a narrower one.
//!
//! A second test holds the idle budget on the real path: with the bar
//! settled, the logic and services threads sleep; a popup reading
//! `cpu.usage` wakes the sampling once a second while open, and closing
//! it puts the threads back to sleep.
//!
//! Skipped, loudly, without sway, grim or dbus-daemon (CI sets
//! `STRAND_REQUIRE_SWAY` and `STRAND_REQUIRE_DBUS`).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use strand_services::testing::{DbusMock, PrivateBus};
use zbus::zvariant::{OwnedValue, Value as ZValue};

const CONFIG: &str = "\
bar Top {
  edge: top; height: 40
  bg: system.dark ? #ff0000 : #0000ff
  row { box { width: 10 + cpu.usage * 1000; height: 40; bg: #00ff00 } }
}
";

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A mock `org.freedesktop.portal.Settings`.
struct MockPortal {
    values: std::collections::HashMap<String, OwnedValue>,
}

#[zbus::interface(name = "org.freedesktop.portal.Settings")]
impl MockPortal {
    async fn read_one(&self, namespace: &str, key: &str) -> zbus::fdo::Result<OwnedValue> {
        if namespace != "org.freedesktop.appearance" {
            return Err(zbus::fdo::Error::Failed("not found".into()));
        }
        self.values
            .get(key)
            .and_then(|v| v.try_clone().ok())
            .ok_or_else(|| zbus::fdo::Error::Failed("not found".into()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
}

/// An RGB screenshot.
struct Img {
    w: usize,
    rgb: Vec<u8>,
}

impl Img {
    /// A binary PPM (`P6`, maxval 255), as `grim -t ppm` writes.
    fn ppm(bytes: &[u8]) -> Img {
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            let start = at;
            while !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            fields.push(String::from_utf8_lossy(&bytes[start..at]).into_owned());
        }
        assert_eq!(fields[0], "P6");
        let w: usize = fields[1].parse().unwrap();
        Img {
            w,
            rgb: bytes[at + 1..].to_vec(),
        }
    }

    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// How many pixels from the left edge of row `y` are `c`.
    fn run(&self, y: usize, c: [u8; 3]) -> usize {
        (0..self.w).take_while(|&x| close(self.px(x, y), c)).count()
    }
}

fn close(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 24)
}

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];

/// A directory of test `name`'s own: beside the target's `tmp`
/// (`CARGO_TARGET_TMPDIR`), not in it, under a parent no other test
/// uses. Every test binary of the workspace makes and removes
/// directories in `tmp`, and this binary's tests run in parallel too
/// under `cargo test --workspace`, while the watcher watches each
/// ancestor of a shell's config (and the apps service its data dirs)
/// for children moved or deleted: a sibling's cleanup would wake an
/// idle window (as `budgets.rs`'s home).
fn own_dir(name: &str) -> PathBuf {
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    target_tmp
        .parent()
        .unwrap_or(target_tmp)
        .join(format!("strand-services-{name}"))
        .join(std::process::id().to_string())
}

fn tools() -> bool {
    for tool in ["sway", "swaymsg", "grim"] {
        if Command::new(tool).arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{tool} is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!(
                "\n*** SKIPPED: {tool} is not installed; the services e2e test did not run ***\n"
            );
            return false;
        }
    }
    true
}

/// A headless sway on `dir` (its `XDG_RUNTIME_DIR`): the process and its
/// `WAYLAND_DISPLAY`.
fn sway(dir: &Path) -> (Proc, String) {
    let cfg = dir.join("sway.cfg");
    std::fs::write(
        &cfg,
        "xwayland disable\noutput HEADLESS-1 resolution 1280x720 position 0 0 scale 1\n",
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("sway.log")).unwrap();
    let mut cmd = Command::new("sway");
    // The compositor dies with the thread that started it. SAFETY: the
    // hook runs between fork and exec and makes one async-signal-safe
    // syscall.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd
        .arg("-c")
        .arg(&cfg)
        .env("XDG_RUNTIME_DIR", dir)
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
    let sway = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let display = names
            .iter()
            .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
        let ipc = names.iter().find(|e| e.starts_with("sway-ipc."));
        if let (Some(d), Some(i)) = (display, ipc) {
            let ok = Command::new("swaymsg")
                .args(["-t", "get_version"])
                .env("SWAYSOCK", dir.join(i))
                .output()
                .is_ok_and(|o| o.status.success());
            if ok {
                return (sway, d.clone());
            }
        }
        assert!(Instant::now() < deadline, "sway did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shot(dir: &Path, display: &str) -> Option<Img> {
    let path = dir.join("shot.ppm");
    let ok = Command::new("grim")
        .args(["-t", "ppm", "-o", "HEADLESS-1"])
        .arg(&path)
        .env("XDG_RUNTIME_DIR", dir)
        .env("WAYLAND_DISPLAY", display)
        .status()
        .is_ok_and(|s| s.success());
    ok.then(|| Img::ppm(&std::fs::read(&path).unwrap()))
}

/// Keeps `n` cores busy until dropped.
struct Busy(Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>);

impl Busy {
    fn start(n: usize) -> Busy {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
                    }
                })
            })
            .collect();
        Busy(stop, threads)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        for t in self.1.drain(..) {
            let _ = t.join();
        }
    }
}

/// The load since boot from `/proc/stat` (what `cpu` reports first).
fn load_since_boot() -> f64 {
    let stat = std::fs::read_to_string("/proc/stat").unwrap();
    let cpu: Vec<u64> = stat
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    let total: u64 = cpu.iter().take(8).sum();
    let idle = cpu[3] + cpu.get(4).copied().unwrap_or(0);
    1.0 - idle as f64 / total.max(1) as f64
}

/// A private bus with a mock portal answering "dark", a headless sway and
/// `strand run` (without `STRAND_MOCK`) of `config_text` on them.
struct Setup {
    dir: PathBuf,
    display: String,
    log: PathBuf,
    home: PathBuf,
    strand: Proc,
    _sway: Proc,
    _portal: zbus::Connection,
    _tokio: tokio::runtime::Runtime,
    _mocks: Vec<DbusMock>,
    _bus: PrivateBus,
}

impl Setup {
    fn start(name: &str, config_text: &str) -> Option<Setup> {
        Setup::start_with(name, config_text, &[], &[])
    }

    /// [`Setup::start`] with dbusmock templates (`template`, the bus name
    /// it owns) on the system side of the private bus and extra
    /// environment for `strand run`.
    fn start_with(
        name: &str,
        config_text: &str,
        mocks: &[(&str, &str)],
        env: &[(&str, PathBuf)],
    ) -> Option<Setup> {
        Setup::start_prepared(name, config_text, mocks, env, |_| {})
    }

    /// [`Setup::start_with`], with `prepare` filling the home directory
    /// (made empty but for the config) before `strand run` starts.
    fn start_prepared(
        name: &str,
        config_text: &str,
        mocks: &[(&str, &str)],
        env: &[(&str, PathBuf)],
        prepare: impl FnOnce(&Path),
    ) -> Option<Setup> {
        if !tools() {
            return None;
        }
        let bus = PrivateBus::start()?;
        let mut started = Vec::new();
        for (template, owns) in mocks {
            started.push(DbusMock::start(&bus, template, true, None, owns)?);
        }
        // Short: the IPC socket paths must fit in sun_path.
        let dir: PathBuf =
            std::env::temp_dir().join(format!("strand-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The portal: dark.
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let values = std::collections::HashMap::from([(
            "color-scheme".to_string(),
            OwnedValue::try_from(ZValue::from(1u32)).unwrap(),
        )]);
        let portal = tokio.block_on(async {
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
        let (sway, display) = sway(&dir);
        // The config outside /tmp: the watcher's light watches on its
        // ancestors would wake for other tests' directories there.
        let home = own_dir(name);
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config/strand");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("bar.strand"), config_text).unwrap();
        prepare(&home);
        let log = dir.join("strand.log");
        let strand = Proc(
            Command::new(env!("CARGO_BIN_EXE_strand"))
                .arg("run")
                .arg(&config)
                .env("XDG_RUNTIME_DIR", &dir)
                .env("WAYLAND_DISPLAY", &display)
                .env("HOME", &home)
                .env("XDG_CACHE_HOME", dir.join("cache"))
                .env("XDG_STATE_HOME", dir.join("state"))
                // The XDG homes follow HOME: a runner's own (GitHub's
                // ubuntu images export XDG_CONFIG_HOME) would move GTK's
                // settings and fontconfig's user dir out of this HOME.
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env_remove("STRAND_ICON_THEME")
                .envs(bus.env())
                .envs(env.iter().map(|(k, v)| (*k, v.as_os_str())))
                .env_remove("STRAND_MOCK")
                .stdin(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        Some(Setup {
            dir,
            display,
            log,
            home,
            strand,
            _sway: sway,
            _portal: portal,
            _tokio: tokio,
            _mocks: started,
            _bus: bus,
        })
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Screenshots until `ok`, 30 s at most.
    fn wait(&self, what: &str, ok: impl Fn(&Img) -> bool) -> Img {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(img) = shot(&self.dir, &self.display)
                && ok(&img)
            {
                return img;
            }
            assert!(Instant::now() < deadline, "never: {what}\n{}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// `strand <args>` against this instance.
    fn cli(&self, args: &[&str]) {
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .env("HOME", &self.home)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "strand {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.strand.0.kill();
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn strand_run_reads_cpu_and_the_portal_through_the_real_services() {
    let Some(setup) = Setup::start("svc", CONFIG) else {
        return;
    };
    let boot = load_since_boot();
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let busy = Busy::start(cores);
    // Red (dark) beside a green box well past its 10 px base.
    let base = |img: &Img| img.run(20, GREEN) >= 12 && close(img.px(1270, 20), RED);
    setup.wait("`system.dark` and `cpu.usage` at boot", base);
    // Every core busy: the samples taken once a second while the bar is
    // visible read close to 1, well above the load since boot (the first
    // reading), so the box widens past what that reading could draw.
    let since_boot_px = 10 + (boot * 1000.0) as usize;
    if boot < 0.6 {
        let img = setup.wait("the samples of a busy machine", |img| {
            base(img) && img.run(20, GREEN) >= since_boot_px + 300
        });
        assert!(img.run(20, GREEN) < 1270, "usage stays below 1");
    } else {
        eprintln!("*** the load since boot is {boot:.2}: too high to tell samples from it ***");
    }
    drop(busy);
}

/// Context switches per thread of `pid` named `name` (its `comm`).
fn switches_of(pid: u32, name: &str) -> u64 {
    let mut total = 0;
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return 0;
    };
    for task in tasks.flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if comm.trim() != name {
            continue;
        }
        let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
        total += status
            .lines()
            .filter(|l| l.contains("ctxt_switches:"))
            .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            .sum::<u64>();
    }
    total
}

fn pss_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

const IDLE_CONFIG: &str = "\
export state open = false
let load = cpu.usage
bar Top {
  edge: top; height: 40
  bg: system.dark ? #ff0000 : #0000ff
  row { box { width: 30; height: 40; bg: #00ff00 } }
  popup { open: <-> open; text pct(load) }
}
";

/// The idle budget on the real services: once the bar settles, the logic
/// and services threads do not wake at all (the portal follow waits on
/// D-Bus, nothing polls; `cpu`, read only by the closed popup through a
/// top-level `let`, stays stopped), within the memory ceiling; a popup
/// reading `cpu.usage` samples once a second while open, and closing it stops the
/// samples at once and the service 5 s later.
#[test]
fn the_real_services_sleep_when_nothing_changes() {
    let Some(setup) = Setup::start("idle", IDLE_CONFIG) else {
        return;
    };
    setup.wait("the dark bar", |img| close(img.px(1270, 20), RED));
    let pid = setup.strand.0.id();
    let threads = ["strand-logic", "strand-services"];
    let woke = || -> u64 { threads.iter().map(|t| switches_of(pid, t)).sum() };
    // Settled: a whole second without a wakeup.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let w = woke();
        std::thread::sleep(Duration::from_secs(1));
        if woke() == w {
            break;
        }
        assert!(Instant::now() < deadline, "never settled\n{}", setup.log());
    }
    assert!(
        switches_of(pid, "strand-services") > 0,
        "the services thread runs (it follows the portal)"
    );
    // The portal's icon theme is followed there too, not on a thread
    // (and runtime, and bus connection) of its own.
    let names: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|tasks| {
            tasks
                .flatten()
                .filter_map(|t| std::fs::read_to_string(t.path().join("comm")).ok())
                .map(|c| c.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(!names.iter().any(|n| n.contains("icon-the")), "{names:?}");
    let before: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    std::thread::sleep(Duration::from_secs(4));
    let after: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    assert_eq!(before, after, "{threads:?} woke while idle");
    let pss = pss_kb(pid);
    let (limit, what) = if cfg!(debug_assertions) {
        (64 * 1024, "debug ceiling")
    } else {
        // design.md: aims at 34 MB, fails above 38.
        (38 * 1024, "38 MB ceiling")
    };
    eprintln!("bar on the real services: PSS {pss} kB ({what} {limit} kB)");
    assert!(pss <= limit, "PSS {pss} kB over the {limit} kB {what}");
    // The popup opens: cpu starts and samples once a second.
    setup.cli(&["set", "bar.open", "true"]);
    std::thread::sleep(Duration::from_millis(1500));
    let s0 = switches_of(pid, "strand-services");
    let l0 = switches_of(pid, "strand-logic");
    std::thread::sleep(Duration::from_secs(5));
    let sampled = switches_of(pid, "strand-services") - s0;
    let logic = switches_of(pid, "strand-logic") - l0;
    eprintln!("cpu sampling 5 s: services {sampled} switches, logic {logic}");
    assert!(
        sampled >= 4,
        "the open popup's cpu samples woke the services thread {sampled} times in 5 s"
    );
    // The logic thread wakes once per sample: the allocator's trim
    // (run.rs, `Trimmer`) does not come between the samples (it did, at
    // 500 ms after each wake: 10 switches in 5 s).
    assert!(
        logic <= 7,
        "the logic thread woke {logic} times in 5 s of 1 s cpu samples"
    );
    // Closed: the samples stop at once (the exit pose and in-flight work
    // done first); the stop comes 5 s after the close.
    setup.cli(&["set", "bar.open", "false"]);
    let closed = Instant::now();
    std::thread::sleep(Duration::from_millis(1500));
    let before: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    std::thread::sleep(Duration::from_millis(2500));
    let after: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    assert_eq!(before, after, "{threads:?} woke after the popup closed");
    // Past the grace: the logic thread's stop timer fired once and the
    // service ended; then everything sleeps again.
    std::thread::sleep((closed + Duration::from_secs(7)).saturating_duration_since(Instant::now()));
    assert!(
        switches_of(pid, "strand-logic") > after[0],
        "the stop 5 s after the close"
    );
    let before: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    std::thread::sleep(Duration::from_secs(3));
    let after: Vec<u64> = threads.iter().map(|t| switches_of(pid, t)).collect();
    assert_eq!(before, after, "{threads:?} woke after cpu stopped");
}

/// `strand check` reaches the system bus its environment names
/// (`DBUS_SYSTEM_BUS_ADDRESS`: a private bus with dbusmock's
/// `power_profiles_daemon`) and checks the design's ppd service against
/// the daemon's introspection: clean as written, a misspelled property an
/// error with its did-you-mean.
#[test]
fn strand_check_checks_from_dbus_services_on_the_system_bus() {
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    let Some(_ppd) = DbusMock::start(
        &bus,
        "power_profiles_daemon",
        true,
        None,
        "net.hadess.PowerProfiles",
    ) else {
        return;
    };
    let dir = own_dir("ppd-check");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let check = |property: &str| -> (bool, String) {
        std::fs::write(
            dir.join("ppd.strand"),
            format!(
                "service ppd from dbus system \"net.hadess.PowerProfiles\" {{ profile: text rw = {property} }}\nbar B {{ text ppd.profile }}\n"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("check")
            .arg(&dir)
            .envs(bus.env())
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (ok, text) = check("ActiveProfile");
    assert!(ok && text.contains("0 errors, 0 warnings"), "{text}");
    let (ok, text) = check("ActiveProfil");
    assert!(!ok, "{text}");
    assert!(
        text.contains("check::dbus_property") && text.contains("ActiveProfile"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The idle budget with M3's other services running: `apps` (its
/// `applications/` directories watched), a `from file` service (inotify),
/// the design's ppd `from dbus` service (a PropertiesChanged match on
/// dbusmock's `power_profiles_daemon`), and a `from poll` service read only
/// by a closed popup. The bar turns red only once every one of them has
/// read; then the logic and services threads do not wake at all, and the
/// poll's command never ran. Opening the popup runs it.
#[test]
fn the_m3_services_sleep_when_nothing_changes() {
    let data = own_dir("m3-idle");
    let _ = std::fs::remove_dir_all(&data);
    let apps = data.join("share/applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("one.desktop"),
        "[Desktop Entry]\nType=Application\nName=One\nExec=true\n",
    )
    .unwrap();
    std::fs::write(data.join("mood.json"), r#"{"level": 7}"#).unwrap();
    let runs = data.join("runs");
    let config = format!(
        "permit exec \"sh\"
export state open = false
service ppd from dbus system \"net.hadess.PowerProfiles\" {{ profile: text = ActiveProfile }}
service mood from file \"{mood}\" {{ level: int }}
service tick from poll [\"sh\", \"-c\", \"echo run >> {runs}; echo 1\"] every 1s {{ n: int }}
let t = tick.n
bar Top {{
  edge: top; height: 40
  bg: system.dark && ppd.profile == \"balanced\" && mood.level == 7 && apps.all.count(a => true) == 1 ? #ff0000 : #0000ff
  row {{ box {{ width: 30; height: 40; bg: #00ff00 }} }}
  popup {{ open: <-> open; text join(\" \", t) }}
}}
",
        mood = data.join("mood.json").display(),
        runs = runs.display(),
    );
    let env = [
        ("XDG_DATA_DIRS", data.join("share")),
        ("XDG_DATA_HOME", data.join("home-share")),
    ];
    let Some(setup) = Setup::start_with(
        "m3idle",
        &config,
        &[("power_profiles_daemon", "net.hadess.PowerProfiles")],
        &env,
    ) else {
        let _ = std::fs::remove_dir_all(&data);
        return;
    };
    setup.wait("every service read (the red bar)", |img| {
        close(img.px(1270, 20), RED)
    });
    let pid = setup.strand.0.id();
    let threads = ["strand-logic", "strand-services"];
    let woke = || -> Vec<u64> { threads.iter().map(|t| switches_of(pid, t)).collect() };
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let w = woke();
        std::thread::sleep(Duration::from_secs(1));
        if woke() == w {
            break;
        }
        assert!(Instant::now() < deadline, "never settled\n{}", setup.log());
    }
    let before = woke();
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(
        before,
        woke(),
        "{threads:?} woke while idle\n{}",
        setup.log()
    );
    assert!(!runs.exists(), "the closed popup's poll ran");
    // Open: the poll runs.
    setup.cli(&["set", "bar.open", "true"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runs.exists() {
        assert!(Instant::now() < deadline, "the open popup's poll never ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(setup);
    let _ = std::fs::remove_dir_all(&data);
}

const MAGENTA: [u8; 3] = [255, 0, 255];
const CYAN: [u8; 3] = [0, 255, 255];

/// A one-colour scalable icon.
fn svg(fill: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\">\
         <rect width=\"16\" height=\"16\" fill=\"{fill}\"/></svg>\n"
    )
}

/// An icon theme of scalable apps icons.
fn index_theme(name: &str, inherits: &str) -> String {
    format!(
        "[Icon Theme]\nName={name}\n{inherits}Directories=scalable/apps\n\n\
         [scalable/apps]\nSize=32\nMinSize=8\nMaxSize=512\nType=Scalable\n"
    )
}

/// Pixels of row `y` that are `c`.
fn count(img: &Img, y: usize, c: [u8; 3]) -> usize {
    (0..img.w).filter(|&x| close(img.px(x, y), c)).count()
}

/// The bottom bar's pixels (rows 680 to 719 of the 1280x720 output).
fn bottom(img: &Img) -> Vec<u8> {
    img.rgb[680 * img.w * 3..720 * img.w * 3].to_vec()
}

/// design.md, "Change sources": apps, icons and fonts are caches their
/// directories' changes invalidate, live and without a reload. `strand
/// run` on headless sway, with `$HOME` its own and fontconfig reading the
/// default user directories (`~/.local/share/fonts`, and
/// `~/.config/fontconfig/fonts.conf` included): installing an app
/// (`~/.local/share/applications`) widens the green box `apps.all`
/// sizes; installing hicolor's `index.theme` shows the magenta icon
/// already in it; naming the `Strand` theme in GTK's settings shows its
/// cyan icon; installing Liberation Sans in `~/.local/share/fonts`, then
/// its bold face in a directory fontconfig's user configuration adds,
/// draws the bottom bar's bold "Liberation Sans" text anew each time.
/// This runs the list `strand run` watches (`live::cache_sources`) and
/// its wiring to the apps service and the renderer end to end.
#[test]
fn installed_apps_icons_and_fonts_show_without_a_reload() {
    let name = "caches";
    let home = own_dir(name);
    let fonts_root = home.join("fontconfig-root");
    let config = "\
bar Top {
  edge: top; height: 40; bg: #000000
  row {
    box { width: 10 + apps.all.count(a => true) * 100; height: 40; bg: #00ff00 }
    icon \"strand-a\" { size: 32 }
    icon \"strand-b\" { size: 32 }
  }
}
bar Bottom {
  edge: bottom; height: 40; bg: #000000
  row { text \"Strand Hello\" { font: \"Liberation Sans\" 24px 700; color: #ffffff } }
}
";
    let env = [
        ("XDG_DATA_DIRS", home.join("share")),
        ("FONTCONFIG_FILE", fonts_root.join("fonts.conf")),
        ("STRAND_LOG", PathBuf::from("info")),
    ];
    let prepare = |home: &Path| {
        let share = home.join(".local/share");
        for d in [
            "share",
            ".local/share/applications",
            ".local/share/fonts",
            ".local/share/icons/hicolor/scalable/apps",
            ".local/share/icons/Strand/scalable/apps",
            ".config/gtk-3.0",
            ".config/fontconfig",
            "fontconfig-root/base",
            "fontconfig-root/bold",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let icons = share.join("icons");
        // hicolor's icon without its `index.theme`: not a theme yet.
        std::fs::write(
            icons.join("hicolor/scalable/apps/strand-a.svg"),
            svg("#ff00ff"),
        )
        .unwrap();
        std::fs::write(
            icons.join("Strand/index.theme"),
            index_theme("Strand", "Inherits=hicolor\n"),
        )
        .unwrap();
        std::fs::write(
            icons.join("Strand/scalable/apps/strand-b.svg"),
            svg("#00ffff"),
        )
        .unwrap();
        // fontconfig: DejaVu Sans to fall back on, the user directories.
        let root = home.join("fontconfig-root");
        std::fs::copy(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            root.join("base/fallback.ttf"),
        )
        .unwrap();
        std::fs::copy(
            strand_text::test_bold_font_path(),
            root.join("bold/LiberationSans-Bold.ttf"),
        )
        .unwrap();
        std::fs::write(
            root.join("fonts.conf"),
            format!(
                "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n\
                 <fontconfig><dir>{}</dir><dir>{}</dir>\
                 <include ignore_missing=\"yes\">{}</include>\
                 <cachedir>{}</cachedir></fontconfig>\n",
                root.join("base").display(),
                share.join("fonts").display(),
                home.join(".config/fontconfig/fonts.conf").display(),
                root.join("cache").display(),
            ),
        )
        .unwrap();
    };
    let Some(setup) = Setup::start_prepared(name, config, &[], &env, prepare) else {
        return;
    };
    let share = home.join(".local/share");
    let booted = setup.wait("both bars drawn", |img| {
        count(img, 20, GREEN) >= 10 && bottom(img).iter().any(|&v| v > 0x80)
    });
    assert_eq!(count(&booted, 20, GREEN), 10, "no app yet");
    assert_eq!(count(&booted, 20, MAGENTA), 0, "hicolor is no theme yet");
    assert_eq!(
        count(&booted, 20, CYAN),
        0,
        "the Strand theme is not chosen yet"
    );
    let reloads = |s: &Setup| s.log().matches("reload (").count();
    let booted_reloads = reloads(&setup);

    // An app installed.
    std::fs::write(
        share.join("applications/strand-test.desktop"),
        "[Desktop Entry]\nType=Application\nName=Strand Test\nExec=true\n",
    )
    .unwrap();
    setup.wait("the installed app counted", |img| {
        count(img, 20, GREEN) >= 100
    });

    // An icon theme installed (its `index.theme`).
    std::fs::write(
        share.join("icons/hicolor/index.theme"),
        index_theme("Hicolor", ""),
    )
    .unwrap();
    setup.wait("hicolor's icon", |img| count(img, 20, MAGENTA) > 0);

    // The desktop's icon theme switched in GTK's settings.
    std::fs::write(
        home.join(".config/gtk-3.0/settings.ini"),
        "[Settings]\ngtk-icon-theme-name=Strand\n",
    )
    .unwrap();
    setup.wait("the Strand theme's icon", |img| count(img, 20, CYAN) > 0);

    // A font installed in `~/.local/share/fonts`.
    let before = bottom(&setup.wait("settled", |_| true));
    std::fs::copy(
        strand_text::test_font_path(),
        share.join("fonts/LiberationSans-Regular.ttf"),
    )
    .unwrap();
    let regular = bottom(&setup.wait("the text in Liberation Sans", |img| bottom(img) != before));

    // fontconfig's user configuration adds a directory with the bold face.
    std::fs::write(
        home.join(".config/fontconfig/fonts.conf"),
        format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"fonts.dtd\">\n\
             <fontconfig><dir>{}</dir></fontconfig>\n",
            fonts_root.join("bold").display()
        ),
    )
    .unwrap();
    setup.wait("the text in Liberation Sans Bold", |img| {
        let now = bottom(img);
        now != regular && now != before
    });
    assert_eq!(
        reloads(&setup),
        booted_reloads,
        "a cache change reloaded the config\n{}",
        setup.log()
    );
    // A config save is what reloads (and is logged so).
    std::fs::write(
        home.join(".config/strand/bar.strand"),
        format!("{config}// saved\n"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while reloads(&setup) == booted_reloads {
        assert!(
            Instant::now() < deadline,
            "a save never logged its reload\n{}",
            setup.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
