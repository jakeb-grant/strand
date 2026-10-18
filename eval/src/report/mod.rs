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
    /// Its reference recordings, as the report shows them.
    pub refs: Option<RefInfo>,
    /// Its approved reference drawn as a trace (wireframes), when the run
    /// left it: the viewer's B side.
    pub reference: Option<RunTrace>,
    /// Its contact sheets, by `(result, finding)` index, relative to the
    /// run's directory.
    pub sheets: BTreeMap<(usize, usize), String>,
}

impl Entry {
    /// `error`, `fail`, `changed` (it left its reference and failed
    /// nothing else), `xfail`, `pass` or `run` (traced, not checked).
    pub fn status(&self) -> &'static str {
        if self.error.is_some() {
            return "error";
        }
        let Some(r) = &self.report else { return "run" };
        let has = |f: fn(&Status) -> bool| r.results.iter().any(|x| f(&x.status));
        if r.only_changed() {
            "changed"
        } else if !r.ok() {
            "fail"
        } else if has(|s| matches!(s, Status::Xfail(_))) {
            "xfail"
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

/// A reference recording's meta, as a report shows it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct RefMeta {
    pub note: String,
    pub date: String,
    pub rev: String,
    pub approver: String,
}

impl RefMeta {
    fn of(b: &crate::refs::Block) -> RefMeta {
        RefMeta {
            note: b.note.clone(),
            date: b.date.clone(),
            rev: b.rev.clone(),
            approver: b.approver.clone(),
        }
    }
}

/// What a report shows of a scenario's references.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct RefInfo {
    /// `eval/refs/<id>.ref`, `eval/refs/matrix-60.ref`.
    pub file: String,
    /// `match`, `changed`, `missing`, `bad` (a known-bad recording
    /// reproduced) or `skip` (not compared).
    pub status: String,
    pub good: Option<RefMeta>,
    pub bad: Vec<RefMeta>,
    /// The CHANGED line, then each node's first difference.
    pub diff: Vec<String>,
}

/// What a report keeps of `refs` (`None`: not compared) after `report`:
/// the reference's state, and its recording as a trace on `trace`'s
/// scenario when the run left it.
pub fn refs_of(
    refs: Option<&crate::refs::Refs>,
    report: &Report,
    trace: &RunTrace,
) -> (Option<RefInfo>, Option<RunTrace>) {
    let Some(refs) = refs else {
        return (None, None);
    };
    use crate::checks::CheckId;
    let st = report.status(CheckId::Reference);
    let status = match st {
        Some(Status::Changed) if refs.good.is_none() => "missing",
        Some(Status::Changed) => "changed",
        Some(Status::Pass) => "match",
        Some(Status::Fail) => "bad",
        _ if refs.good.is_none() => "missing",
        _ => "skip",
    };
    let mut diff = Vec::new();
    for res in report
        .results
        .iter()
        .filter(|r| r.check == CheckId::Reference.name())
    {
        for f in &res.findings {
            diff.push(match (f.sev, st) {
                (Sev::Fail, Some(Status::Changed)) => format!("CHANGED: {}: {}", report.id, f.msg),
                _ => f.msg.clone(),
            });
        }
    }
    let reference = match (status, &refs.good) {
        ("changed", Some(b)) => b.recording().ok().map(|r| r.to_trace(trace)),
        _ => None,
    };
    let info = RefInfo {
        file: crate::refs::shown(&refs.file),
        status: status.into(),
        good: refs.good.as_ref().map(RefMeta::of),
        bad: refs.bad.iter().map(RefMeta::of).collect(),
        diff,
    };
    (Some(info), reference)
}

/// The viewer page of `entries`: with, when any run left its approved
/// reference, the references as a second run (B), drawn as wireframes.
pub fn page_of(info: &RunInfo, entries: &[Entry]) -> Result<String, Error> {
    let refs: Vec<Entry> = entries
        .iter()
        .filter_map(|e| {
            Some(Entry {
                id: e.id.clone(),
                file: e.file.clone(),
                source: e.source.clone(),
                trace: e.reference.clone()?,
                refs: e.refs.clone(),
                ..Entry::default()
            })
        })
        .collect();
    let mut page = html::Page::run(info, entries);
    if !refs.is_empty() {
        page.runs[0].label = format!("run {}", page.runs[0].label);
        page.runs.push(html::PageRun {
            label: "approved reference".into(),
            out: info.out.display().to_string(),
            frames: Frames::None,
            entries: &refs,
            reference: true,
        });
        page.ab = Some(crate::ab::compare(entries, &refs));
    }
    html::page(&page)
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
/// `summary.json` and `index.html` in `info.out`. Nothing outside
/// `info.out`: CI's step summary is the script's to append
/// (scripts/eval/ci.sh's gate, scripts/eval/matrix.sh), so a test or a
/// mutant run under CI never writes to it.
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
    let page = page_of(info, entries)?;
    write(&info.out.join("index.html"), page.as_bytes())?;
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
