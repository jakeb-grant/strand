//! Shared helpers: a collector that mirrors the change stream, fixture
//! loading, and a private headless sway.
#![allow(dead_code)]

pub mod hyprland;
pub mod window;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use strand_services::wm::{Mirror, WmChange};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Receives the service's batches and applies them to a [`Mirror`].
pub struct Collector {
    rx: UnboundedReceiver<Vec<WmChange>>,
    pub mirror: Mirror,
    /// Batches received so far.
    pub batches: usize,
    /// Every change received, in order.
    pub log: Vec<WmChange>,
}

impl Collector {
    /// The sink to hand to `wm::run`, and the collector.
    pub fn new() -> (impl FnMut(Vec<WmChange>) + Send + 'static, Collector) {
        let (tx, rx) = unbounded_channel();
        (
            move |batch: Vec<WmChange>| {
                assert!(!batch.is_empty(), "the service sent an empty batch");
                let _ = tx.send(batch);
            },
            Collector {
                rx,
                mirror: Mirror::default(),
                batches: 0,
                log: Vec::new(),
            },
        )
    }

    fn take(&mut self, batch: Vec<WmChange>) {
        self.batches += 1;
        for c in &batch {
            self.mirror
                .apply(c)
                .unwrap_or_else(|e| panic!("inconsistent diff {c:?}: {e}"));
        }
        self.log.extend(batch);
    }

    /// Waits (at most `secs`) until `done` holds for the mirror.
    pub async fn until_within(&mut self, secs: u64, what: &str, done: impl Fn(&Mirror) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !done(&self.mirror) {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(b)) => self.take(b),
                Ok(None) => panic!("{what}: the service stopped"),
                Err(_) => panic!("{what}: timed out; mirror: {:#?}", self.mirror),
            }
        }
    }

    /// Waits (at most 5 s) until `done` holds for the mirror.
    pub async fn until(&mut self, what: &str, done: impl Fn(&Mirror) -> bool) {
        self.until_within(5, what, done).await;
    }

    /// Batches that arrive within `d`.
    pub async fn quiet_for(&mut self, d: Duration) -> usize {
        let before = self.batches;
        let deadline = tokio::time::Instant::now() + d;
        while let Ok(Some(b)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            self.take(b);
        }
        self.batches - before
    }
}

/// `tests/fixtures/<rel>`.
pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

/// The `## name` bursts of an events file: their traffic lines, each ended
/// by `\n`; `#` lines and blank lines are markers.
pub fn bursts(rel: &str) -> HashMap<String, String> {
    let text = std::fs::read_to_string(fixture(rel)).unwrap();
    let mut out: HashMap<String, String> = HashMap::new();
    let mut current = None;
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ") {
            current = Some(name.trim().to_string());
            out.entry(name.trim().to_string()).or_default();
        } else if line.starts_with('#') || line.trim().is_empty() {
            continue;
        } else if let Some(c) = &current {
            let b = out.get_mut(c).unwrap();
            b.push_str(line);
            b.push('\n');
        }
    }
    out
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private headless sway, killed on drop.
pub struct Sway {
    child: Child,
    pub dir: PathBuf,
    pub display: String,
    pub ipc: PathBuf,
}

impl Sway {
    /// Starts sway; `None` (after saying why) when it is not installed and
    /// `STRAND_REQUIRE_SWAY` is unset.
    pub fn start(test: &str) -> Option<Sway> {
        if Command::new("sway").arg("--version").output().is_err() {
            assert!(
                std::env::var_os("STRAND_REQUIRE_SWAY").is_none(),
                "{test}: sway is not installed but STRAND_REQUIRE_SWAY is set"
            );
            eprintln!("skipping {test}: sway is not installed");
            return None;
        }
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("strand-services-{}-{n}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1280x720@60Hz position 0 0\n",
        )
        .unwrap();
        let log = std::fs::File::create(dir.join("sway.log")).unwrap();
        let mut cmd = Command::new("sway");
        // SAFETY: one async-signal-safe syscall between fork and exec, so
        // a killed test binary never leaks the compositor.
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
            child,
            dir: dir.clone(),
            display: String::new(),
            ipc: PathBuf::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let entries: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let display = entries
                .iter()
                .find(|e| e.starts_with("wayland-") && !e.ends_with(".lock"));
            let ipc = entries.iter().find(|e| e.starts_with("sway-ipc."));
            if let (Some(d), Some(i)) = (display, ipc) {
                sway.display = d.clone();
                sway.ipc = dir.join(i);
                if sway.try_msg(&["-t", "get_version"]).is_some() {
                    return Some(sway);
                }
            }
            if Instant::now() > deadline {
                let log = std::fs::read_to_string(dir.join("sway.log")).unwrap_or_default();
                panic!("sway did not start:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The Wayland socket's path.
    pub fn socket(&self) -> PathBuf {
        self.dir.join(&self.display)
    }

    pub fn try_msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs `swaymsg`; panics on failure.
    pub fn msg(&self, args: &[&str]) -> String {
        self.try_msg(args)
            .unwrap_or_else(|| panic!("swaymsg {args:?} failed"))
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("sway.log")).unwrap_or_default()
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
