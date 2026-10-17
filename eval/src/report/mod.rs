//! A run's report (docs/eval.md, "Reports"): per scenario its trace,
//! checks, scenario text, frame images and contact sheets; per run
//! `summary.md`, `summary.json`, `summary.txt` and `index.html`, the
//! self-contained viewer.
//!
//! - [`png`]: PNG files; [`font`] and [`sheet`]: contact sheets;
//! - [`summary`]: the headless summaries Claude reads;
//! - [`html`]: the viewer page, its trace embedded, images thinned to
//!   keep it under its size;
//! - [`explain`]: a node's frames as a text table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::checks::{Report, Sev, Status};
use crate::harness::Image;
use crate::trace::RunTrace;

pub mod explain;
pub mod font;
pub mod html;
pub mod png;
pub mod sheet;
pub mod summary;

/// Contact sheets drawn per check at most.
pub const SHEETS_PER_CHECK: usize = 8;

/// Which frame images a report keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frames {
    All,
    /// Those of scenarios that did not pass.
    Failing,
    None,
}

impl Frames {
    pub fn parse(s: &str) -> Option<Frames> {
        Some(match s {
            "all" => Frames::All,
            "failing" => Frames::Failing,
            "none" => Frames::None,
            _ => return None,
        })
    }

    /// The default for `n` scenarios: every frame for a few, the failing
    /// ones' for more.
    pub fn default_for(n: usize) -> Frames {
        if n <= 5 { Frames::All } else { Frames::Failing }
    }
}

/// One scenario's run, checked or not, as a report shows it.
#[derive(Debug, Default)]
pub struct Entry {
    pub id: String,
    /// The scenario file (for messages and commands).
    pub file: String,
    /// The scenario's text (`scenario.scn` beside its trace).
    pub source: String,
    pub trace: RunTrace,
    /// The checks, when they ran.
    pub report: Option<Report>,
    pub images: BTreeMap<String, Image>,
    pub notes: Vec<String>,
    /// Why it could not run.
    pub error: Option<String>,
    /// The owner's recorded verdict (`eval/refs/<id>.ref.json`).
    pub verdict: Option<crate::checks::Reference>,
    /// Its contact sheets, by `(result, finding)` index, relative to the
    /// run's directory.
    pub sheets: BTreeMap<(usize, usize), String>,
}

impl Entry {
    /// `error`, `fail`, `xfail`, `changed`, `pass` or `run` (traced, not
    /// checked).
    pub fn status(&self) -> &'static str {
        if self.error.is_some() {
            return "error";
        }
        let Some(r) = &self.report else { return "run" };
        let has = |f: fn(&Status) -> bool| r.results.iter().any(|x| f(&x.status));
        if !r.ok() {
            "fail"
        } else if has(|s| matches!(s, Status::Xfail(_))) {
            "xfail"
        } else if has(|s| matches!(s, Status::Changed)) {
            "changed"
        } else {
            "pass"
        }
    }

    /// Whether a report keeps its frames under `frames`.
    pub fn keeps_frames(&self, frames: Frames) -> bool {
        match frames {
            Frames::All => true,
            Frames::Failing => !matches!(self.status(), "pass" | "run"),
            Frames::None => false,
        }
    }

    /// The failing findings, with their `(result, finding)` index.
    pub fn failing(&self) -> Vec<((usize, usize), &crate::checks::Finding)> {
        let Some(r) = &self.report else {
            return Vec::new();
        };
        r.results
            .iter()
            .enumerate()
            .flat_map(|(i, res)| {
                res.findings
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| f.sev == Sev::Fail)
                    .map(move |(j, f)| ((i, j), f))
            })
            .collect()
    }
}

pub fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
    }
    std::fs::write(path, bytes).map_err(|e| Error::Io(path.display().to_string(), e))
}

/// Writes `e` under `out/<id>/`: `trace.json`, `checks.json` when it was
/// checked, `scenario.scn`, its frames as `frames` says and a contact
/// sheet per failing finding (at most [`SHEETS_PER_CHECK`] per check),
/// noted in `e.sheets`.
pub fn write_entry(out: &Path, e: &mut Entry, frames: Frames) -> Result<(), Error> {
    let dir = out.join(&e.id);
    write(&dir.join("trace.json"), e.trace.to_json().as_bytes())?;
    if let Some(r) = &e.report {
        let json =
            serde_json::to_string_pretty(r).map_err(|x| Error::Run(format!("checks.json: {x}")))?;
        write(&dir.join("checks.json"), json.as_bytes())?;
    }
    if !e.source.is_empty() {
        write(&dir.join("scenario.scn"), e.source.as_bytes())?;
    }
    if e.keeps_frames(frames) {
        for (name, img) in &e.images {
            write(&dir.join(name), &img.png)?;
        }
    }
    let mut per: BTreeMap<String, usize> = BTreeMap::new();
    let mut sheets = BTreeMap::new();
    for (at, f) in e.failing() {
        let n = per.entry(f.check.clone()).or_default();
        if *n >= SHEETS_PER_CHECK {
            continue;
        }
        let Some(png) = sheet::sheet(&e.trace, &e.images, f)? else {
            continue;
        };
        *n += 1;
        let name = format!("fail-{}-{}.png", f.check, n);
        write(&dir.join(&name), &png)?;
        sheets.insert(at, format!("{}/{name}", e.id));
    }
    e.sheets = sheets;
    Ok(())
}

/// What a run's report is of.
#[derive(Debug)]
pub struct RunInfo {
    /// The command that made it (`check`, `run`, `matrix cell`).
    pub command: String,
    /// The report's directory as the user named it (for the commands
    /// the viewer copies).
    pub out: PathBuf,
    pub frames: Frames,
}

/// Writes every entry, then `summary.txt` (`lines`), `summary.md`,
/// `summary.json` and `index.html` in `info.out`. Appends `summary.md` to
/// `$GITHUB_STEP_SUMMARY` when that is set.
pub fn write_run(info: &RunInfo, entries: &mut [Entry], lines: &[String]) -> Result<(), Error> {
    for e in entries.iter_mut() {
        write_entry(&info.out, e, info.frames)?;
    }
    write(
        &info.out.join("summary.txt"),
        (lines.join("\n") + "\n").as_bytes(),
    )?;
    let md = summary::markdown(info, entries);
    write(&info.out.join("summary.md"), md.as_bytes())?;
    let json = summary::json(info, entries);
    write(&info.out.join("summary.json"), json.as_bytes())?;
    let page = html::page(&html::Page::run(info, entries))?;
    write(&info.out.join("index.html"), page.as_bytes())?;
    if let Ok(p) = std::env::var("GITHUB_STEP_SUMMARY")
        && !p.is_empty()
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&p)
        {
            let _ = f.write_all(md.as_bytes());
        }
    }
    Ok(())
}

/// Reads a run's scenario back from `dir` (`<run>/<id>`): its trace, its
/// checks if they were written, its scenario text and frame images.
pub fn read_entry(dir: &Path) -> Result<Entry, Error> {
    let read = |name: &str| -> Result<Option<String>, Error> {
        let p = dir.join(name);
        match std::fs::read_to_string(&p) {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(p.display().to_string(), e)),
        }
    };
    let text = read("trace.json")?.ok_or_else(|| {
        Error::Usage(format!(
            "{}: no trace.json (a scenario's directory in a run's report)",
            dir.display()
        ))
    })?;
    let trace = RunTrace::from_json(&text)
        .map_err(|e| Error::Config(format!("{}/trace.json: {e}", dir.display())))?;
    let report = match read("checks.json")? {
        Some(t) => Some(
            serde_json::from_str::<Report>(&t)
                .map_err(|e| Error::Config(format!("{}/checks.json: {e}", dir.display())))?,
        ),
        None => None,
    };
    let mut images = BTreeMap::new();
    let names: std::collections::BTreeSet<&String> =
        trace.frames.iter().flat_map(|f| f.png.values()).collect();
    for name in names {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            let (width, height) = png_size(&bytes).unwrap_or((0, 0));
            images.insert(
                name.clone(),
                Image {
                    width,
                    height,
                    png: bytes,
                },
            );
        }
    }
    let id = trace.scenario.id.clone();
    Ok(Entry {
        id,
        file: trace.scenario.file.clone(),
        source: read("scenario.scn")?.unwrap_or_default(),
        trace,
        report,
        images,
        ..Entry::default()
    })
}

/// A PNG file's size from its header.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
    let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
    Some((w, h))
}

/// The scenario directories under `path`: `path` itself when it holds a
/// `trace.json`, else its subdirectories that do (sorted).
pub fn entry_dirs(path: &Path) -> Result<Vec<PathBuf>, Error> {
    if path.join("trace.json").is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let rd = std::fs::read_dir(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("trace.json").is_file())
        .collect();
    out.sort();
    if out.is_empty() {
        return Err(Error::Usage(format!(
            "{}: no run here (a run's directory, or one scenario's in it)",
            path.display()
        )));
    }
    Ok(out)
}
