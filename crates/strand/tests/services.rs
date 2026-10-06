//! The real services end to end (M3): `strand run` on a headless sway,
//! without `STRAND_MOCK`, so the config reads the composite real host. A
//! private `dbus-daemon` carries a mock XDG portal answering "dark"; the
//! config's bar is red when `system.dark` and blue otherwise, and holds a
//! green box `10 + cpu.usage * 1000` px wide. While the test keeps two
//! cores busy, a wide green box on red can only come from the real `cpu`
//! (procfs) and `system` (the portal on the services runtime) services:
//! the schema defaults would draw a 10 px box on blue.
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

use strand_services::testing::PrivateBus;
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

#[test]
fn strand_run_reads_cpu_and_the_portal_through_the_real_services() {
    if !tools() {
        return;
    }
    let Some(bus) = PrivateBus::start() else {
        return;
    };
    // Short: the IPC socket paths must fit in sun_path.
    let dir: PathBuf = std::env::temp_dir().join(format!("strand-svc-{}", std::process::id()));
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
    let _portal = tokio.block_on(async {
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

    let (_sway, display) = sway(&dir);
    let home = dir.join("home");
    let config = home.join(".config/strand");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("bar.strand"), CONFIG).unwrap();
    let busy = Busy::start(2);
    let log = dir.join("strand.log");
    let _strand = Proc(
        Command::new(env!("CARGO_BIN_EXE_strand"))
            .arg("run")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &display)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .envs(bus.env())
            .env_remove("STRAND_MOCK")
            .stdin(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let read_log = || std::fs::read_to_string(&log).unwrap_or_default();
    // Red (dark) beside a green box well past its 10 px base.
    let wide = |img: &Img| img.run(20, GREEN) >= 60 && close(img.px(1270, 20), RED);
    let wait = |what: &str| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(img) = shot(&dir, &display)
                && wide(&img)
            {
                return img;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: the bar never showed `system.dark` and `cpu.usage`\n{}",
                read_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    wait("boot");
    // Past the first reading (the load since boot): the once-a-second
    // samples while the bar is visible keep it wide.
    std::thread::sleep(Duration::from_millis(2500));
    let img = wait("sampling");
    assert!(img.run(20, GREEN) < 1270, "usage stays below 1");
    drop(busy);
    let _ = std::fs::remove_dir_all(&dir);
}
