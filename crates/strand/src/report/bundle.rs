//! (report) A glitch report's bundle (docs/architecture.md, "Glitch
//! reports"; design.md, "Reporting a glitch").
//!
//! Main's part ([`main_bundle`]) is taken between frames: its ring's
//! dump, each mapped surface's last frame (the newest committed shm
//! buffer, copied, or for a surface the GPU presents, its scene repainted
//! by the CPU), the files the renderer draws, and each surface's slot.
//! Logic adds its own (`run/request/report.rs`) and hands the whole to
//! [`spawn`]: a `strand-report` thread writes the directory, encodes the
//! PNGs, copies the config and the files it reads (bounded reads, capped
//! sizes), prunes the reports to the newest [`KEEP`], stores its outcome
//! in a shared slot and pings the logic loop. Neither loop waits on it.
//!
//! The directory is `reports/<YYYY-MM-DD_HH-MM-SS>` (local time, `-2`,
//! `-3` when taken) under the state directory, mode 0700, every file
//! 0600.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value as Json, json};
use strand_scene::{FramePixels, SurfaceId};
use strand_surface::State;

use crate::demo::host::Host;
use crate::run::SnapshotId;

/// Reports kept: the newest this many directories holding a
/// `bundle.json`.
pub(crate) const KEEP: usize = 20;
/// The most bytes one copied file may have.
pub(crate) const FILE_CAP: u64 = 2 * 1024 * 1024;
/// The most bytes the copied files may have in all.
pub(crate) const TOTAL_CAP: u64 = 16 * 1024 * 1024;
/// The bundle format this build writes.
pub(crate) const FORMAT: u64 = 1;

/// One mapped surface's last frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Frame {
    /// Its slot in the input ring.
    pub slot: u16,
    /// Its file's stem: the namespace, or `popup`.
    pub name: String,
    pub pixels: FramePixels,
    /// Repainted by the CPU (the GPU presents it).
    pub repainted: bool,
}

/// Main's part of a bundle ([`main_bundle`]).
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct MainBundle {
    /// The input ring's dump (`None`: `system.replay` is off).
    pub ring: Option<Vec<u8>>,
    pub frames: Vec<Frame>,
    /// The files the renderer draws for mounted nodes.
    pub images: Vec<PathBuf>,
    /// Each mapped surface: its slot, namespace, kind, root, output,
    /// backend and sizes, as `bundle.json`'s `surfaces` lists them.
    pub surfaces: Vec<Json>,
}

/// Main's part, between frames: no lock surface is in it.
pub(crate) fn main_bundle(state: &mut State<Host>) -> MainBundle {
    let infos = state.surfaces();
    let monitors = state.monitors();
    let entries = state.host().recorder.surfaces();
    let mut surfaces = Vec::new();
    let mut frames = Vec::new();
    for e in &entries {
        let Some(info) = infos.iter().find(|i| i.id == e.surface) else {
            continue;
        };
        if state.host().lock.is_lock_surface(e.surface) {
            continue;
        }
        let backend = super::backend(&state.host().renderer, e.surface);
        let output = info
            .monitor
            .as_ref()
            .and_then(|id| monitors.iter().find(|m| m.id == *id))
            .and_then(|m| m.connector.clone())
            .or_else(|| e.output.clone());
        let name = if info.kind == strand_scene::NodeKind::Popup {
            "popup".to_string()
        } else {
            info.namespace.clone()
        };
        let (pixels, repainted) = frame_of(state, e.surface, backend.is_gpu());
        let file = format!("frames/{}-{}.png", e.slot, file_stem(&name));
        surfaces.push(json!({
            "slot": e.slot,
            "namespace": info.namespace,
            "kind": info.kind.name(),
            "root": strand_compiler::instantiate::inspect::node_ref(e.root),
            "output": output,
            "backend": super::backend_name(backend),
            "size": [info.logical_size.0, info.logical_size.1],
            "buffer": [info.buffer_size.w, info.buffer_size.h],
            "scale": info.scale.as_f64(),
            "frame": pixels.as_ref().map(|_| file.clone()),
            "repainted": repainted,
        }));
        if let Some(pixels) = pixels {
            frames.push(Frame {
                slot: e.slot,
                name,
                pixels,
                repainted,
            });
        }
    }
    let host = state.host();
    MainBundle {
        ring: host.recorder.dump(),
        frames,
        images: host.renderer.image_sources(),
        surfaces,
    }
}

/// `surface`'s last frame: the committed buffer, or (on the GPU, or with
/// no buffer yet) the CPU's repaint of its scene.
fn frame_of(state: &mut State<Host>, surface: SurfaceId, gpu: bool) -> (Option<FramePixels>, bool) {
    if !gpu && let Some(f) = state.frame_pixels(surface) {
        return (Some(f), false);
    }
    let f = state.host_mut().renderer.repaint_offscreen(surface);
    (f, true)
}

/// `name` with what a file name cannot hold replaced.
fn file_stem(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Everything a bundle holds, gathered on the logic thread.
#[derive(Debug, Default)]
pub(crate) struct Job {
    /// `<state dir>/reports`.
    pub root: PathBuf,
    pub words: String,
    /// What `strand report --json` prints.
    pub report: Json,
    pub main: MainBundle,
    /// The changes ring's dump (`None`: off).
    pub changes: Option<Vec<u8>>,
    /// `services.json`, `cells.json`.
    pub services: Json,
    pub cells: Json,
    pub config_dir: Option<PathBuf>,
    /// The module set: each file's path under the config directory and
    /// its text as it was compiled.
    pub config: Vec<(PathBuf, Arc<str>)>,
    /// The other files the program and the renderer read,
    /// newest-referenced first.
    pub files: Vec<PathBuf>,
    pub version: String,
    pub commit: String,
    /// RFC 3339 with its offset.
    pub wall: String,
    pub now_ns: u64,
    pub epoch_unix_ns: i64,
    pub replay: bool,
    /// (report) `nodes.json`: the nodes the input ring names, by
    /// `node_ref` (`Shell::node_table`).
    pub nodes: Json,
}

/// What a finished report answers: `{"bundle", "paste", "report"}` or
/// why it failed.
pub(crate) type Outcome = Result<Json, String>;

/// Where `strand-report` threads leave their outcomes.
pub(crate) type Done = Arc<Mutex<Vec<(SnapshotId, Outcome)>>>;

/// Writes `job` on a `strand-report` thread; its outcome goes to `done`
/// under `id`, and `ping` wakes the logic loop.
pub(crate) fn spawn(job: Job, id: SnapshotId, done: Done, ping: calloop::ping::Ping) {
    let slot = done.clone();
    let started = std::thread::Builder::new()
        .name("strand-report".into())
        .spawn(move || {
            let outcome = write(&job).map_err(|e| format!("the report was not saved: {e}"));
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, outcome));
            ping.ping();
        });
    if let Err(e) = started {
        done.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, Err(format!("no thread to save the report on: {e}"))));
    }
}

/// The line to paste to a coding agent.
pub(crate) fn paste_line(words: &str, dir: &Path) -> String {
    let d = dir.display();
    format!(
        "Strand glitch report \"{words}\" at {d}: run scripts/report-view.sh --no-open {d} in \
         the strand checkout and read {d}/replay/replay.txt."
    )
}

fn private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

fn private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(p) = path.parent() {
        private_dir(p)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// A new directory under `root` named by the local time, `-2`, `-3`
/// appended while taken.
fn new_dir(root: &Path) -> io::Result<PathBuf> {
    private_dir(root)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    for n in 1..1000 {
        let name = if n == 1 {
            stamp.clone()
        } else {
            format!("{stamp}-{n}")
        };
        let dir = root.join(name);
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free report name"))
}

/// `pixels` (premultiplied RGBA) as a PNG of straight RGBA.
pub(crate) fn png(pixels: &FramePixels) -> io::Result<Vec<u8>> {
    let (w, h) = (pixels.width as usize, pixels.height as usize);
    let mut straight = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = pixels
            .rgba
            .get(y * pixels.stride as usize..y * pixels.stride as usize + w * 4)
            .ok_or_else(|| io::Error::other("a frame shorter than its size"))?;
        for px in row.chunks_exact(4) {
            let a = px[3];
            let un = |c: u8| match a {
                0 => 0,
                255 => c,
                _ => ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8,
            };
            straight.extend_from_slice(&[un(px[0]), un(px[1]), un(px[2]), a]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, pixels.width, pixels.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(io::Error::other)?;
        w.write_image_data(&straight).map_err(io::Error::other)?;
    }
    Ok(out)
}

/// Reads `path` if it is a regular file of at most `cap` bytes, reading
/// no more than that (a file growing as it is read is cut there and
/// refused).
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, (u64, String)> {
    let meta = fs::metadata(path).map_err(|e| (0, e.to_string()))?;
    if !meta.is_file() {
        return Err((0, "not a file".into()));
    }
    if meta.len() > cap {
        return Err((meta.len(), format!("over {} MiB", cap / (1024 * 1024))));
    }
    let f = File::open(path).map_err(|e| (meta.len(), e.to_string()))?;
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| (meta.len(), e.to_string()))?;
    if bytes.len() as u64 > cap {
        return Err((
            bytes.len() as u64,
            format!("over {} MiB", cap / (1024 * 1024)),
        ));
    }
    Ok(bytes)
}

/// Every regular file under `dir`, sorted (links not followed out of it).
fn files_under(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() && depth < 8 => files_under(&p, out, depth + 1),
            Ok(m) if m.is_file() => out.push(p),
            _ => {}
        }
    }
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Writes the bundle: see the module docs. Returns the answer.
pub(crate) fn write(job: &Job) -> io::Result<Json> {
    let dir = new_dir(&job.root)?;
    let mut hashes = serde_json::Map::new();
    let mut missing = Vec::new();
    private_file(
        &dir.join("words.txt"),
        format!("{}\n", job.words).as_bytes(),
    )?;
    private_file(
        &dir.join("diagnostics.json"),
        format!("{}\n", job.report).as_bytes(),
    )?;
    private_file(
        &dir.join("diagnostics.txt"),
        super::text(&job.report).as_bytes(),
    )?;
    if let Some(r) = &job.main.ring {
        private_file(&dir.join("input.flight"), r)?;
    }
    if let Some(r) = &job.changes {
        private_file(&dir.join("changes.flight"), r)?;
    }
    private_file(
        &dir.join("services.json"),
        format!("{:#}\n", job.services).as_bytes(),
    )?;
    private_file(
        &dir.join("cells.json"),
        format!("{:#}\n", job.cells).as_bytes(),
    )?;
    private_file(
        &dir.join("nodes.json"),
        format!("{:#}\n", job.nodes).as_bytes(),
    )?;
    for f in &job.main.frames {
        let name = format!("frames/{}-{}.png", f.slot, file_stem(&f.name));
        private_file(&dir.join(name), &png(&f.pixels)?)?;
    }
    // The module set, as it was compiled.
    for (rel, text) in &job.config {
        private_file(&dir.join("config").join(rel), text.as_bytes())?;
        let at = job
            .config_dir
            .as_ref()
            .map_or_else(|| rel.clone(), |d| d.join(rel));
        hashes.insert(
            at.display().to_string(),
            Json::String(blake3_hex(text.as_bytes())),
        );
    }
    let mut total: u64 = 0;
    let mut copy = |src: &Path, dst: &Path, total: &mut u64| -> io::Result<()> {
        let left = TOTAL_CAP.saturating_sub(*total);
        match read_bounded(src, FILE_CAP.min(left)) {
            Ok(bytes) => {
                *total += bytes.len() as u64;
                private_file(dst, &bytes)?;
                hashes.insert(src.display().to_string(), Json::String(blake3_hex(&bytes)));
            }
            Err((bytes, why)) => {
                let why = if bytes <= FILE_CAP && left < FILE_CAP && bytes > left {
                    format!("over the {} MiB in all", TOTAL_CAP / (1024 * 1024))
                } else {
                    why
                };
                missing
                    .push(json!({"path": src.display().to_string(), "bytes": bytes, "why": why}));
            }
        }
        Ok(())
    };
    // The config's `icons/` folder, under `config/`.
    if let Some(cd) = &job.config_dir {
        let mut icons = Vec::new();
        files_under(&cd.join("icons"), &mut icons, 0);
        for p in icons {
            if let Ok(rel) = p.strip_prefix(cd) {
                copy(&p, &dir.join("config").join(rel), &mut total)?;
            }
        }
    }
    // Every other file, at its own path under `files/`.
    let mut seen = std::collections::HashSet::new();
    for p in &job.files {
        if !p.is_absolute() || !seen.insert(p.clone()) {
            continue;
        }
        let rel: PathBuf = p.components().skip(1).collect();
        copy(p, &dir.join("files").join(rel), &mut total)?;
    }
    let bundle = json!({
        "format": FORMAT,
        "words": job.words,
        "wall": job.wall,
        "now_ns": job.now_ns,
        "epoch_unix_ns": job.epoch_unix_ns,
        "strand": {"version": job.version, "commit": job.commit},
        "config_dir": job.config_dir.as_ref().map(|d| d.display().to_string()),
        "outputs": job.report["outputs"],
        "surfaces": job.main.surfaces,
        "replay": job.replay,
        "files": hashes,
        "missing": missing,
    });
    // Written last: a directory holding a `bundle.json` is a whole report
    // (pruning counts only those).
    private_file(&dir.join("bundle.json"), format!("{bundle:#}\n").as_bytes())?;
    prune(&job.root, KEEP);
    let paste = paste_line(&job.words, &dir);
    Ok(json!({
        "ok": true,
        "bundle": dir.display().to_string(),
        "paste": paste,
        "report": job.report,
    }))
}

/// Removes all but the newest `keep` directories under `root` that hold
/// a `bundle.json` (by its time, then by name); nothing else is touched.
pub(crate) fn prune(root: &Path, keep: usize) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    let mut reports: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let m = fs::symlink_metadata(&p).ok()?;
            if !m.is_dir() {
                return None;
            }
            let b = fs::metadata(p.join("bundle.json")).ok()?;
            Some((b.modified().ok()?, p))
        })
        .collect();
    reports.sort();
    let over = reports.len().saturating_sub(keep);
    for (_, p) in reports.into_iter().take(over) {
        if let Err(e) = fs::remove_dir_all(&p) {
            log::warn!("report: {} not pruned: {e}", p.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strand-bundle-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn pruning_keeps_the_newest_bundles_and_nothing_else_goes() {
        let root = tmp("prune");
        for i in 0..23 {
            let d = root.join(format!("2026-10-20_10-00-{i:02}"));
            private_dir(&d).unwrap();
            private_file(&d.join("bundle.json"), b"{}").unwrap();
            // Distinct times, in name order.
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + i);
            File::options()
                .write(true)
                .open(d.join("bundle.json"))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        // Not reports: left alone.
        private_dir(&root.join("notes")).unwrap();
        private_file(&root.join("README"), b"mine").unwrap();
        prune(&root, KEEP);
        let mut left: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left.len(), 22, "{left:?}");
        assert!(!left.contains(&"2026-10-20_10-00-00".to_string()));
        assert!(!left.contains(&"2026-10-20_10-00-02".to_string()));
        assert!(left.contains(&"2026-10-20_10-00-03".to_string()));
        assert!(left.contains(&"notes".to_string()) && left.contains(&"README".to_string()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_frame_is_encoded_straight_and_whole() {
        let pixels = FramePixels {
            width: 2,
            height: 1,
            scale: strand_scene::Scale::ONE,
            stride: 8,
            rgba: vec![128, 0, 0, 128, 0, 0, 255, 255],
        };
        let bytes = png(&pixels).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut r = dec.read_info().unwrap();
        let mut buf = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..8], &[255, 0, 0, 128, 0, 0, 255, 255]);
    }

    #[test]
    fn a_file_over_the_cap_is_refused_without_reading_it_whole() {
        let root = tmp("cap");
        private_dir(&root).unwrap();
        let big = root.join("big.png");
        fs::write(&big, vec![0u8; 4096]).unwrap();
        assert_eq!(read_bounded(&big, 4096).unwrap().len(), 4096);
        let (bytes, why) = read_bounded(&big, 1024).unwrap_err();
        assert_eq!(bytes, 4096);
        assert!(why.starts_with("over"), "{why}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_paste_line_names_the_directory_and_the_command() {
        let line = paste_line("bar dipped", Path::new("/s/reports/2026-10-20_10-00-00"));
        assert_eq!(
            line,
            "Strand glitch report \"bar dipped\" at /s/reports/2026-10-20_10-00-00: run \
             scripts/report-view.sh --no-open /s/reports/2026-10-20_10-00-00 in the strand \
             checkout and read /s/reports/2026-10-20_10-00-00/replay/replay.txt."
        );
    }
}
