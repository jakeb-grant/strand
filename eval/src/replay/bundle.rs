//! A glitch report's bundle read back (docs/architecture.md, "Glitch
//! reports"; docs/eval.md, "Replaying a glitch report", step 1):
//! `bundle.json`, both ring dumps, `services.json`, `cells.json`,
//! `nodes.json`, (replay-owner) `drawn.json`, (history) `history.json` and the frames. A newer format is refused by its number.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use strand_core::flight::{self, Dump, Entry};

use crate::Error;

/// The bundle format this reader knows (`bundle.json`'s `format`).
pub const FORMAT: u64 = 1;

/// One mapped surface `bundle.json` lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SurfaceRec {
    /// Its slot in the input ring.
    pub slot: u16,
    pub namespace: String,
    pub kind: String,
    /// Its root's `node_ref`.
    pub root: String,
    pub output: Option<String>,
    pub backend: String,
    /// Logical size.
    pub size: [f64; 2],
    /// Buffer size.
    pub buffer: [u32; 2],
    pub scale: f64,
    /// Its frame's PNG, relative to the bundle.
    pub frame: Option<String>,
    /// (window) A window's last configure, at the report.
    pub window: Option<Toplevel>,
}

/// (window) What the compositor's configure said of a window: a
/// `TOPLEVEL` record's, or `bundle.json`'s `window` (at the report).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Toplevel {
    /// Its box, logical px.
    pub size: [u32; 2],
    /// The axes the compositor sized.
    pub sized: [bool; 2],
    pub maximized: bool,
    pub fullscreen: bool,
    pub tiled: bool,
    pub activated: bool,
    pub decorated: bool,
    /// `configure_bounds`, logical px (`None`: not said, the whole
    /// output then).
    pub bounds: Option<[u32; 2]>,
}

impl Toplevel {
    /// A `TOPLEVEL` record's.
    pub fn of_rec(r: &flight::Rec) -> Toplevel {
        use flight::toplevel as b;
        let bits = r.u8(4);
        let on = |v: u8| bits & v != 0;
        let bounds = [u32::from(r.u16(6)), u32::from(r.u16(8))];
        Toplevel {
            size: [u32::from(r.u16(0)), u32::from(r.u16(2))],
            sized: [on(b::SIZED_W), on(b::SIZED_H)],
            maximized: on(b::MAXIMIZED),
            fullscreen: on(b::FULLSCREEN),
            tiled: on(b::TILED),
            activated: on(b::ACTIVATED),
            decorated: on(b::DECORATED),
            bounds: (bounds[0] > 0 && bounds[1] > 0).then_some(bounds),
        }
    }

    /// `bundle.json`'s `window` of a surface (`None`: not a window).
    pub fn of_json(v: &Json) -> Option<Toplevel> {
        if !v.is_object() {
            return None;
        }
        let n = |i: usize| v["size"][i].as_u64().map_or(0, |n| n as u32);
        let on = |k: &str| v[k] == true;
        let b = |i: usize| v["bounds"][i].as_u64().map(|n| n as u32);
        let bounds = match (b(0), b(1)) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some([w, h]),
            _ => None,
        };
        Some(Toplevel {
            size: [n(0), n(1)],
            sized: [v["sized"][0] == true, v["sized"][1] == true],
            maximized: on("maximized"),
            fullscreen: on("fullscreen"),
            tiled: on("tiled"),
            activated: on("activated"),
            decorated: on("decorated"),
            bounds,
        })
    }

    /// The words of the `window` step that configures a window so
    /// (docs/eval.md, "Step vocabulary"): its size on the axes the
    /// compositor sized, marked `!` (sized as recorded, whatever size the
    /// window asks for in the replay; `_` where it left the window its
    /// own), then its states and its bounds when the compositor said
    /// them, `960!,1040! tiled activated decorated bounds 1920,1040`.
    pub fn words(&self) -> String {
        let axis = |i: usize| {
            if self.sized[i] {
                format!("{}!", self.size[i].max(1))
            } else {
                "_".to_string()
            }
        };
        let mut w = format!("{},{}", axis(0), axis(1));
        for (on, name) in [
            (self.maximized, "maximized"),
            (self.fullscreen, "fullscreen"),
            (self.tiled, "tiled"),
            (self.activated, "activated"),
            (self.decorated, "decorated"),
        ] {
            if on {
                w.push(' ');
                w.push_str(name);
            }
        }
        if let Some([bw, bh]) = self.bounds {
            w.push_str(&format!(" bounds {bw},{bh}"));
        }
        w
    }
}

/// A bundle read back.
#[derive(Debug)]
pub struct Bundle {
    pub dir: PathBuf,
    /// The directory's name (`2026-10-20_14-03-07`).
    pub name: String,
    pub json: Json,
    pub words: String,
    /// When it was saved, ns since the process epoch.
    pub now_ns: u64,
    /// The epoch's wall time, ns since the Unix epoch.
    pub epoch_unix_ns: i64,
    /// The wall time `bundle.json` gives, with its offset.
    pub wall: String,
    pub input: Option<Dump>,
    pub changes: Option<Dump>,
    pub inputs: Vec<Entry>,
    pub change_entries: Vec<Entry>,
    pub services: Json,
    pub cells: Json,
    /// `nodes.json` (empty when the bundle has none).
    pub nodes: Json,
    /// (replay-owner) `drawn.json`: each mapped surface's nodes as its
    /// last live frame drew them; `Null` in a bundle from before it.
    pub drawn: Json,
    pub surfaces: Vec<SurfaceRec>,
    /// The diagnostics (`diagnostics.json`), `Null` when absent.
    pub diagnostics: Json,
    /// (history) `history.json`: each series a `graph` kept history of,
    /// its points on the boot clock, and the boot clock at the report
    /// (`boot_ns`, (graph-follow) what the replay's `boot` comes from);
    /// `Null` in a bundle from before it was written with every report
    /// and kept nothing.
    pub history: Json,
}

fn read_json(path: &Path) -> Result<Json, Error> {
    let text =
        std::fs::read_to_string(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    serde_json::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

fn read_opt_json(path: &Path) -> Result<Json, Error> {
    if path.is_file() {
        read_json(path)
    } else {
        Ok(Json::Null)
    }
}

fn read_dump(path: &Path) -> Result<Option<Dump>, Error> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    flight::read(&bytes)
        .map(Some)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))
}

impl Bundle {
    /// Reads the bundle in `dir`.
    pub fn read(dir: &Path) -> Result<Bundle, Error> {
        let json = read_json(&dir.join("bundle.json"))?;
        let format = json["format"].as_u64().unwrap_or(0);
        if format > FORMAT {
            return Err(Error::Config(format!(
                "{}: bundle format {format} is newer than this replay reads ({FORMAT})",
                dir.display()
            )));
        }
        if format == 0 {
            return Err(Error::Config(format!(
                "{}: bundle.json has no format",
                dir.display()
            )));
        }
        let input = read_dump(&dir.join("input.flight"))?;
        let changes = read_dump(&dir.join("changes.flight"))?;
        let surfaces = json["surfaces"]
            .as_array()
            .map(|a| a.iter().map(surface_rec).collect())
            .unwrap_or_default();
        let name = dir
            .file_name()
            .map_or_else(|| "bundle".into(), |n| n.to_string_lossy().into_owned());
        let words = json["words"].as_str().unwrap_or("").to_string();
        Ok(Bundle {
            dir: dir.to_path_buf(),
            name,
            now_ns: json["now_ns"].as_u64().unwrap_or(0),
            epoch_unix_ns: json["epoch_unix_ns"].as_i64().unwrap_or(0),
            wall: json["wall"].as_str().unwrap_or("").to_string(),
            inputs: input.as_ref().map(Dump::entries).unwrap_or_default(),
            change_entries: changes.as_ref().map(Dump::entries).unwrap_or_default(),
            input,
            changes,
            services: read_opt_json(&dir.join("services.json"))?,
            cells: read_opt_json(&dir.join("cells.json"))?,
            nodes: read_opt_json(&dir.join("nodes.json"))?,
            drawn: read_opt_json(&dir.join("drawn.json"))?,
            diagnostics: read_opt_json(&dir.join("diagnostics.json"))?,
            history: read_opt_json(&dir.join("history.json"))?,
            surfaces,
            words,
            json,
        })
    }

    /// The config's copy (`config/`).
    pub fn config_dir(&self) -> PathBuf {
        self.dir.join("config")
    }

    /// The directory the live shell read its config from.
    pub fn live_config_dir(&self) -> Option<PathBuf> {
        self.json["config_dir"].as_str().map(PathBuf::from)
    }

    /// The files the bundle lists as missing: path, bytes and why.
    pub fn missing(&self) -> Vec<(String, u64, String)> {
        self.json["missing"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|m| {
                        (
                            m["path"].as_str().unwrap_or("?").to_string(),
                            m["bytes"].as_u64().unwrap_or(0),
                            m["why"].as_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn surface_rec(s: &Json) -> SurfaceRec {
    let pair = |v: &Json| [v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0)];
    SurfaceRec {
        slot: s["slot"].as_u64().unwrap_or(0) as u16,
        namespace: s["namespace"].as_str().unwrap_or("").to_string(),
        kind: s["kind"].as_str().unwrap_or("").to_string(),
        root: s["root"].as_str().unwrap_or("").to_string(),
        output: s["output"].as_str().map(str::to_string),
        backend: s["backend"].as_str().unwrap_or("").to_string(),
        size: pair(&s["size"]),
        buffer: {
            let b = pair(&s["buffer"]);
            [b[0] as u32, b[1] as u32]
        },
        scale: s["scale"].as_f64().unwrap_or(1.0),
        frame: s["frame"].as_str().map(str::to_string),
        window: Toplevel::of_json(&s["window"]),
    }
}

/// A record's `NodeId` (the u64 at payload 0) as `node_ref` spells it.
pub fn node_ref_at(e: &Entry) -> String {
    let bits = e.rec.u64(0);
    format!("{}.{}", bits as u32, (bits >> 32) as u32)
}
