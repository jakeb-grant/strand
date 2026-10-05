//! Test harness: a private headless sway per test, a scriptable painter
//! host, and screenshots through grim.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use strand_scene::{
    Damage, Insets, LogicalRect, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Rect,
    Scale, Size, SurfaceChange, SurfaceId, SurfaceSpec,
};
use strand_surface::{Config, InputEvent, Monitor, MonitorId, SurfaceHost, SurfaceManager};
use wayland_client::Connection;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A headless sway with its own runtime directory, killed on drop.
pub struct Sway {
    child: Child,
    dir: PathBuf,
    pub display: String,
    pub ipc: PathBuf,
}

impl Sway {
    /// Starts sway, or returns `None` (after printing why) when sway is not
    /// installed, so the test is skipped.
    pub fn start(test: &str) -> Option<Sway> {
        if Command::new("sway").arg("--version").output().is_err() {
            eprintln!("skipping {test}: sway is not installed");
            return None;
        }
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "strand-surface-{}-{n}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = dir.join("sway.cfg");
        std::fs::write(
            &cfg,
            "xwayland disable\noutput HEADLESS-1 resolution 1920x1080@60Hz position 0 0\n",
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

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn connect(&self) -> Connection {
        let stream = UnixStream::connect(self.dir.join(&self.display)).unwrap();
        Connection::from_socket(stream).unwrap()
    }

    fn try_msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("swaymsg")
            .args(args)
            .env("SWAYSOCK", &self.ipc)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs `swaymsg` and returns its stdout; panics on failure.
    pub fn msg(&self, args: &[&str]) -> String {
        self.try_msg(args)
            .unwrap_or_else(|| panic!("swaymsg {args:?} failed"))
    }

    /// Output names, in sway's order.
    pub fn output_names(&self) -> Vec<String> {
        let json = self.msg(&["-t", "get_outputs", "-r"]);
        json_strings(&json, "name")
            .into_iter()
            .filter(|n| n.starts_with("HEADLESS-"))
            .collect()
    }

    /// `swaymsg create_output`; returns the new output's name.
    pub fn create_output(&self) -> String {
        let before = self.output_names();
        self.msg(&["create_output"]);
        let after = self.output_names();
        let name = after
            .into_iter()
            .find(|n| !before.contains(n))
            .expect("a new output");
        self.msg(&["output", &name, "resolution", "1920x1080@60Hz"]);
        name
    }

    /// Screenshot of one output as RGBA rows.
    pub fn grim(&self, output: &str) -> Image {
        let path = self.dir.join(format!("{output}.png"));
        let status = Command::new("grim")
            .arg("-o")
            .arg(output)
            .arg(&path)
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", &self.display)
            .status()
            .expect("grim runs");
        assert!(status.success(), "grim failed");
        Image::load(&path)
    }

    /// The usable area of the first workspace on `output` (what exclusive
    /// zones leave), as `(x, y, width, height)`.
    pub fn workspace_rect(&self, output: &str) -> (i64, i64, i64, i64) {
        let tree = self.msg(&["-t", "get_workspaces", "-r"]);
        // Each workspace object starts with its type, then its own rect;
        // its output comes later in the same object.
        for obj in tree.split("\"type\": \"workspace\"").skip(1) {
            if obj.contains(&format!("\"output\": \"{output}\"")) {
                let rect = &obj[obj.find("\"rect\"").unwrap()..];
                let num = |key: &str| -> i64 {
                    let at = rect.find(&format!("\"{key}\"")).unwrap() + key.len() + 2;
                    let rest = rect[at..].trim_start_matches([':', ' ']);
                    let end = rest
                        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
                        .unwrap();
                    rest[..end].parse().unwrap()
                };
                return (num("x"), num("y"), num("width"), num("height"));
            }
        }
        panic!("no workspace on {output}: {tree}");
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Every string value of `"key": "value"` in a JSON text.
fn json_strings(json: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\"");
    let mut out = Vec::new();
    let mut rest = json;
    while let Some(i) = rest.find(&pat) {
        rest = &rest[i + pat.len()..];
        let r = rest.trim_start_matches([':', ' ']);
        if let Some(r) = r.strip_prefix('"') {
            if let Some(end) = r.find('"') {
                out.push(r[..end].to_owned());
            }
        }
    }
    out
}

/// A decoded screenshot.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub channels: usize,
    pub data: Vec<u8>,
}

impl Image {
    fn load(path: &Path) -> Image {
        let file = std::fs::File::open(path).unwrap();
        let mut reader = png::Decoder::new(BufReader::new(file)).read_info().unwrap();
        let mut data = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut data).unwrap();
        data.truncate(info.buffer_size());
        let channels = info.color_type.samples();
        Image {
            width: info.width,
            height: info.height,
            channels,
            data,
        }
    }

    /// RGB at `(x, y)`.
    pub fn rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * self.channels;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

// ---- the test painter ----------------------------------------------------

/// Straight RGB colours the test paints (opaque).
pub const BLUE: [u8; 3] = [0x20, 0x60, 0xe0];
pub const RED: [u8; 3] = [0xe0, 0x30, 0x20];
/// The second colour of the checkerboard background.
pub const WHITE: [u8; 3] = [0xf0, 0xf0, 0xf0];

#[derive(Clone, Debug, PartialEq)]
pub struct PaintRecord {
    pub surface: SurfaceId,
    pub size: Size,
    pub scale: Scale,
    pub age: u8,
    pub time: Duration,
    pub damage: Damage,
}

/// What one surface last showed, per committed frame.
#[derive(Default)]
struct SurfaceState {
    size: Size,
    scale: Option<Scale>,
    /// Square (physical) of each committed frame, oldest first.
    frames: Vec<Option<Rect>>,
    /// Damage of each committed frame (what changed from the previous one).
    history: Vec<Damage>,
}

/// A painter whose content is a background colour and an optional square,
/// painted only inside the damage it returns, so a wrong buffer age shows
/// as wrong pixels. It checks on every paint that the buffer really holds
/// the frame its age claims.
#[derive(Default)]
pub struct TestHost {
    /// The square in logical pixels.
    pub square: Option<LogicalRect>,
    /// Bumped by every content change.
    version: u64,
    painted_version: HashMap<SurfaceId, u64>,
    surfaces: HashMap<SurfaceId, SurfaceState>,
    /// Frames still to animate (square moves 1 px per frame).
    pub animate: u32,
    pub opaque: bool,
    /// Background is a 1-physical-pixel BLUE/WHITE checkerboard (crispness
    /// checks) instead of plain BLUE.
    pub checker: bool,
    pub paints: Vec<PaintRecord>,
    /// Monitor `None`: a `screens: focused` surface placed by the
    /// compositor.
    pub attached: Vec<(SurfaceId, NodeId, Option<MonitorId>)>,
    pub entered: Vec<(SurfaceId, MonitorId)>,
    /// What [`SurfaceHost::input`] saw.
    pub input: Vec<InputEvent>,
    pub configured: Vec<(SurfaceId, Size, Scale)>,
    pub detached: Vec<SurfaceId>,
    pub monitors_added: Vec<(Monitor, bool)>,
    pub monitors_removed: Vec<Monitor>,
    pub monitors_changed: Vec<Monitor>,
    /// The first paint of each surface draws nothing and returns empty
    /// damage (the `Painter` contract allows it) while still wanting a
    /// frame.
    pub empty_first: bool,
    empty_done: HashSet<SurfaceId>,
    /// Pixels found not to hold the frame the buffer age claimed.
    pub age_errors: u64,
}

impl TestHost {
    pub fn set_square(&mut self, square: Option<LogicalRect>) {
        self.square = square;
        self.version += 1;
    }

    pub fn paints_of(&self, surface: SurfaceId) -> Vec<&PaintRecord> {
        self.paints
            .iter()
            .filter(|p| p.surface == surface)
            .collect()
    }

    fn square_px(&self, scale: Scale) -> Option<Rect> {
        self.square.map(|r| scale.snap_rect(r))
    }
}

fn put(px: &mut [u8], rgb: [u8; 3]) {
    // wl_shm ARGB8888 little-endian: B, G, R, A.
    px[0] = rgb[2];
    px[1] = rgb[1];
    px[2] = rgb[0];
    px[3] = 0xff;
}

fn color_at(square: Option<Rect>, checker: bool, x: i32, y: i32) -> [u8; 3] {
    match square {
        Some(r) if r.contains(strand_scene::Point::new(x, y)) => RED,
        _ if checker && (x + y) % 2 != 0 => WHITE,
        _ => BLUE,
    }
}

/// The checkerboard colour of physical pixel `(x, y)`.
pub fn checker_at(x: u32, y: u32) -> [u8; 3] {
    color_at(None, true, x as i32, y as i32)
}

impl Painter for TestHost {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        if self.empty_first && self.empty_done.insert(surface) {
            return Damage::new();
        }
        let scale = target.scale;
        let square = self.square_px(scale);
        let bounds = target.bounds();
        let version = self.version;
        let changed = self.painted_version.get(&surface) != Some(&version);
        let st = self.surfaces.entry(surface).or_default();
        let resized = st.size != target.size || st.scale != Some(scale);
        if resized {
            st.size = target.size;
            st.scale = Some(scale);
            st.frames.clear();
            st.history.clear();
        }
        let age = target.age as usize;
        // Check the buffer holds the frame its age claims.
        if age > 0 && age <= st.frames.len() && !resized {
            let held = st.frames[st.frames.len() - age];
            let stride = target.stride as usize;
            for (x, y) in [
                (0, 0),
                (bounds.w as i32 - 1, bounds.h as i32 - 1),
                (bounds.w as i32 / 2, bounds.h as i32 / 2),
            ]
            .into_iter()
            .chain(held.iter().map(|r| (r.x, r.y)))
            {
                let i = y as usize * stride + x as usize * 4;
                let want = color_at(held, self.checker, x, y);
                let got = [target.pixels[i + 2], target.pixels[i + 1], target.pixels[i]];
                if got != want {
                    self.age_errors += 1;
                }
            }
        }
        let prev = st.frames.last().copied().flatten();
        // What changed relative to the last committed frame.
        let mut this = Damage::new();
        if st.frames.is_empty() {
            this.add(bounds);
        } else if changed {
            for r in [prev, square].into_iter().flatten() {
                this.add(r);
            }
        }
        this.clip(bounds);
        let mut damage = this;
        if age == 0 || resized || age > st.frames.len() {
            damage = Damage::full(target.size);
        } else {
            for d in st.history.iter().rev().take(age - 1) {
                damage.union(d);
            }
        }
        damage.clip(bounds);
        if damage.is_empty() {
            self.painted_version.insert(surface, version);
            return damage;
        }
        let stride = target.stride as usize;
        for r in damage.rects() {
            for y in r.y..r.y + r.h as i32 {
                for x in r.x..r.x + r.w as i32 {
                    let i = y as usize * stride + x as usize * 4;
                    put(
                        &mut target.pixels[i..i + 4],
                        color_at(square, self.checker, x, y),
                    );
                }
            }
        }
        st.frames.push(square);
        st.history.push(this);
        self.painted_version.insert(surface, version);
        if self.animate > 0 {
            self.animate -= 1;
            if let Some(sq) = self.square.as_mut() {
                sq.x += 1.0;
            }
            self.version += 1;
        }
        self.paints.push(PaintRecord {
            surface,
            size: target.size,
            scale,
            age: target.age,
            time: target.time,
            damage,
        });
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.animate > 0 || self.painted_version.get(&surface) != Some(&self.version)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        match (self.opaque, self.surfaces.get(&surface)) {
            (true, Some(st)) => Damage::full(st.size),
            _ => Damage::new(),
        }
    }
}

impl SurfaceHost for TestHost {
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, monitor: Option<&Monitor>) {
        self.attached
            .push((surface, node, monitor.map(|m| m.id.clone())));
    }

    fn surface_entered(&mut self, surface: SurfaceId, monitor: &Monitor) {
        self.entered.push((surface, monitor.id.clone()));
    }

    fn input(&mut self, event: &InputEvent) {
        self.input.push(event.clone());
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        self.configured.push((surface, size, scale));
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.detached.push(surface);
        self.surfaces.remove(&surface);
        self.painted_version.remove(&surface);
    }

    fn monitor_added(&mut self, monitor: &Monitor, reconnected: bool) {
        self.monitors_added.push((monitor.clone(), reconnected));
    }

    fn monitor_removed(&mut self, monitor: &Monitor) {
        self.monitors_removed.push(monitor.clone());
    }

    fn monitor_changed(&mut self, monitor: &Monitor) {
        self.monitors_changed.push(monitor.clone());
    }
}

/// A `panel` or `osd` of `w`×`h` at `anchor` with default `screens`
/// (focused), as the renderer would report it.
pub fn layer_spec(kind: NodeKind, name: &str, anchor: &str, w: f32, h: f32) -> SurfaceSpec {
    let props: HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text(name.into())),
        (Prop::Anchor, PropValue::Keyword(anchor.into())),
        (Prop::Width, PropValue::Number(w)),
        (Prop::Height, PropValue::Number(h)),
    ]
    .into_iter()
    .collect();
    SurfaceSpec::resolve(kind, |p| props.get(&p))
}

/// `bar <name> { edge: top; height: <h> }` as the renderer would report it.
pub fn bar_spec(name: &str, height: f32) -> SurfaceSpec {
    bar_spec_with_margin(name, height, Insets::all(0.0))
}

/// `bar <name> { edge: top; height: <h>; margin: <margin> }`.
pub fn bar_spec_with_margin(name: &str, height: f32, margin: Insets) -> SurfaceSpec {
    let props: HashMap<Prop, PropValue> = [
        (Prop::Name, PropValue::Text(name.into())),
        (Prop::Edge, PropValue::Keyword("top".into())),
        (Prop::Height, PropValue::Number(height)),
        (Prop::Margin, PropValue::Insets(margin)),
    ]
    .into_iter()
    .collect();
    SurfaceSpec::resolve(NodeKind::Bar, |p| props.get(&p))
}

pub const BAR: NodeId = NodeId::new(1, 0);

/// Starts sway and a surface manager showing `bar Top { height: 36 }`.
pub fn start(test: &str, config: Config) -> Option<(Sway, SurfaceManager<TestHost>)> {
    let sway = Sway::start(test)?;
    let mut mgr = SurfaceManager::with_connection(sway.connect(), TestHost::default(), config)
        .expect("surface manager starts");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    Some((sway, mgr))
}

pub const WAIT: Duration = Duration::from_secs(10);

/// Dispatches until every surface of the bar has committed a frame and
/// there are `n` of them.
pub fn wait_for_bars(mgr: &mut SurfaceManager<TestHost>, n: usize) {
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let surfaces = s.surfaces();
            surfaces.len() == n && surfaces.iter().all(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(
        ok,
        "expected {n} painted bars, have {:?}",
        mgr.state().surfaces()
    );
}

/// Every size and scale reported through `surface_configured` was painted
/// (no configure for a size nobody saw).
pub fn assert_configured_sizes_painted(host: &TestHost) {
    for (surface, size, scale) in &host.configured {
        assert!(
            host.paints
                .iter()
                .any(|p| p.surface == *surface && p.size == *size && p.scale == *scale),
            "configured {surface:?} at {size:?} @ {scale:?} but never painted it: {:?}",
            host.configured
        );
    }
}

/// Dispatches for `d` (events only; returns early never).
pub fn pump(mgr: &mut SurfaceManager<TestHost>, d: Duration) {
    let _ = mgr.dispatch_until(d, |_| false).unwrap();
}
