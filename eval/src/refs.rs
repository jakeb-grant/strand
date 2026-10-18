//! Reference recordings (docs/eval.md, "Reference recordings"): every
//! committed scenario and every matrix cell has an approved recording of
//! its run ([`crate::recording`]), and a run that leaves it beyond a
//! quantum is CHANGED; recordings marked bad stay, and a run that comes
//! back to one fails as reproducing it.
//!
//! A reference file holds blocks, one per recording: a scenario's in
//! `eval/refs/<id>.ref`, the matrix's in one file per rate,
//! `eval/refs/matrix-<hz>.ref`. Each block names its scenario, verdict,
//! note, date, revision, approver and the scenario text's hash, then the
//! recording, and a seal over all of it: `approve` and `verdict` write
//! them, and [`lint`] refuses a block edited by hand. [`note_rule`]
//! refuses a recording that changed under the note it had (CI's
//! `refs-check` against the merge base), so every approval is a line a
//! reviewer reads.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::Error;
use crate::recording::{self, Recording, Tol};
use crate::report;
use crate::scenario::Scenario;

/// Today's date as `YYYY-MM-DD` (UTC), from the system clock.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil(secs / 86_400)
}

/// The civil date `days` after 1970-01-01 (Howard Hinnant's algorithm).
pub fn civil(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The first line of every reference file.
pub const HEADER: &str = "# strand-eval reference recordings (docs/eval.md, \"Reference recordings\"). Written by `strand-eval approve` and `verdict`; a block edited by hand fails its seal.";

/// The committed references' sizes, tripwires for a recording that
/// stopped being compact (docs/eval.md, "Reference recordings";
/// eval/tests/refs.rs): every file together, a scenario's file, and a
/// matrix rate's file per cell in it (it grows with the effects, each
/// recording should not).
pub const TOTAL_BUDGET: u64 = 5 * 1024 * 1024;
pub const SCENARIO_FILE_BUDGET: u64 = 64 * 1024;
pub const MATRIX_CELL_BUDGET: u64 = 8 * 1024;

/// Where approvals name their approver from when `--approver` does not.
pub const APPROVER_ENV: &str = "STRAND_EVAL_APPROVER";

/// The approver: `--approver` (`flag`), else `$STRAND_EVAL_APPROVER`
/// (`env`). Neither, or a name that says nobody ([`no_approver`]), is
/// refused: an approval names who looked.
pub fn approver(flag: Option<&str>, env: Option<String>) -> Result<String, Error> {
    let name = flag
        .map(str::to_string)
        .or(env)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if no_approver(&name) {
        return Err(Error::Usage(format!(
            "an approver is required: --approver NAME or ${APPROVER_ENV} (run.sh passes it in), naming who looked at the change"
        )));
    }
    Ok(name)
}

/// Whether an approver names nobody: empty, or `unknown` (what an
/// approval without one recorded before it was required).
pub fn no_approver(name: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n.eq_ignore_ascii_case("unknown")
}

/// The committed references: `eval/refs/`.
pub fn dir() -> PathBuf {
    crate::scenario::eval_root().join("refs")
}

/// The rate of a matrix cell (`m.<cell>.<hz>`, its scenario under
/// `eval/matrix/cells/`), `None` for a scenario.
pub fn matrix_hz(id: &str, scenario_file: &str) -> Option<u32> {
    let file = scenario_file.replace('\\', "/");
    if !id.starts_with("m.") || !file.contains("matrix/cells/") {
        return None;
    }
    id.rsplit('.').next()?.parse().ok()
}

/// The file scenario `id`'s references live in, under `dir`.
pub fn file_for(dir: &Path, id: &str, scenario_file: &str) -> PathBuf {
    match matrix_hz(id, scenario_file) {
        Some(hz) => dir.join(format!("matrix-{hz}.ref")),
        None => dir.join(format!("{id}.ref")),
    }
}

/// A recording's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Approved: the run must match it.
    Good,
    /// Known bad: the run must not come back to it.
    Bad,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Verdict> {
        match s {
            "good" => Some(Verdict::Good),
            "bad" => Some(Verdict::Bad),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Verdict::Good => "good",
            Verdict::Bad => "bad",
        }
    }
}

/// One recording in a reference file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub id: String,
    pub verdict: Verdict,
    /// Why it is right (or wrong): required.
    pub note: String,
    pub date: String,
    /// The build it was recorded from (`+changes`: with uncommitted ones).
    pub rev: String,
    pub approver: String,
    /// The scenario text it ran, hashed ([`source_hash`]).
    pub scenario: String,
    pub seal: String,
    /// The recording's text ([`recording::encode`]).
    pub body: String,
    /// Its first line in its file (1-based; 0 when made here).
    pub line: usize,
}

fn hash16(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

/// A scenario text's hash, as a block records it.
pub fn source_hash(source: &str) -> String {
    hash16(source.as_bytes())
}

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// What a block says about itself besides its recording.
#[derive(Clone, Debug)]
pub struct Meta<'a> {
    pub verdict: Verdict,
    pub note: &'a str,
    pub date: &'a str,
    pub rev: &'a str,
    pub approver: &'a str,
    /// The scenario text's hash ([`source_hash`]).
    pub scenario: &'a str,
}

impl Block {
    /// A sealed block.
    pub fn new(id: &str, m: &Meta, body: String) -> Block {
        let mut b = Block {
            id: id.into(),
            verdict: m.verdict,
            note: m.note.into(),
            date: m.date.into(),
            rev: m.rev.into(),
            approver: m.approver.into(),
            scenario: m.scenario.into(),
            seal: String::new(),
            body,
            line: 0,
        };
        b.seal = b.compute_seal();
        b
    }

    fn meta(&self) -> String {
        format!(
            "ref {}\nverdict {}\nnote {}\ndate {}\nrev {}\napprover {}\nscenario {}\n",
            q(&self.id),
            self.verdict.name(),
            q(&self.note),
            self.date,
            q(&self.rev),
            q(&self.approver),
            self.scenario
        )
    }

    /// The hash of everything in the block but the seal.
    pub fn compute_seal(&self) -> String {
        hash16(format!("{}{}", self.meta(), self.body).as_bytes())
    }

    pub fn render(&self) -> String {
        format!("{}seal {}\n{}", self.meta(), self.seal, self.body)
    }

    /// Its recording.
    pub fn recording(&self) -> Result<Recording, String> {
        // The body starts after the eight meta lines.
        recording::decode(&self.body, self.line + 8)
    }

    /// `good (2026-10-17, a2d4d726, claude): why`.
    pub fn describe(&self) -> String {
        format!(
            "{} ({}, {}, {}): {}",
            self.verdict.name(),
            self.date,
            self.rev,
            self.approver,
            self.note
        )
    }
}

/// A reference file: its blocks, sorted by scenario, the approved one
/// first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefFile {
    pub blocks: Vec<Block>,
}

fn json_str(v: &str) -> Option<String> {
    serde_json::from_str::<String>(v).ok()
}

impl RefFile {
    /// The file at `path`, `None` when there is none.
    pub fn load(path: &Path) -> Result<Option<RefFile>, Error> {
        match std::fs::read_to_string(path) {
            Ok(t) => RefFile::parse(&t)
                .map(Some)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io(path.display().to_string(), e)),
        }
    }

    pub fn parse(text: &str) -> Result<RefFile, String> {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() && (lines[i].starts_with('#') || lines[i].trim().is_empty()) {
            i += 1;
        }
        let mut blocks = Vec::new();
        while i < lines.len() {
            let start = i;
            let field = |n: usize, name: &str| -> Result<&str, String> {
                lines
                    .get(start + n)
                    .and_then(|l| l.strip_prefix(name))
                    .and_then(|l| l.strip_prefix(' '))
                    .ok_or_else(|| format!("line {}: `{name} …` expected", start + n + 1))
            };
            let string = |n: usize, name: &str| -> Result<String, String> {
                json_str(field(n, name)?)
                    .ok_or_else(|| format!("line {}: {name}: a quoted string", start + n + 1))
            };
            let id = string(0, "ref")?;
            let verdict = Verdict::parse(field(1, "verdict")?)
                .ok_or_else(|| format!("line {}: verdict: good or bad", start + 2))?;
            let note = string(2, "note")?;
            let date = field(3, "date")?.to_string();
            let rev = string(4, "rev")?;
            let approver = string(5, "approver")?;
            let scenario = field(6, "scenario")?.to_string();
            let seal = field(7, "seal")?.to_string();
            i = start + 8;
            let mut body = String::new();
            while i < lines.len() && !lines[i].starts_with("ref ") {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            blocks.push(Block {
                id,
                verdict,
                note,
                date,
                rev,
                approver,
                scenario,
                seal,
                body,
                line: start + 1,
            });
        }
        Ok(RefFile { blocks })
    }

    pub fn render(&self) -> String {
        let mut out = format!("{HEADER}\n");
        for b in &self.blocks {
            out += &b.render();
        }
        out
    }

    pub fn save(&self, path: &Path) -> Result<(), Error> {
        report::write(path, self.render().as_bytes())
    }

    pub fn good(&self, id: &str) -> Option<&Block> {
        self.blocks
            .iter()
            .find(|b| b.id == id && b.verdict == Verdict::Good)
    }

    pub fn bad(&self, id: &str) -> Vec<&Block> {
        self.blocks
            .iter()
            .filter(|b| b.id == id && b.verdict == Verdict::Bad)
            .collect()
    }

    /// `b` in, replacing the approved block of its scenario (a bad one is
    /// added); blocks sorted by scenario, the approved one first.
    pub fn put(&mut self, b: Block) {
        if b.verdict == Verdict::Good {
            self.blocks
                .retain(|x| !(x.id == b.id && x.verdict == Verdict::Good));
        }
        self.blocks.push(b);
        self.blocks
            .sort_by(|a, b| (&a.id, a.verdict).cmp(&(&b.id, b.verdict)));
    }
}

/// What a scenario is checked against.
#[derive(Clone, Debug, Default)]
pub struct Refs {
    /// The file they live in (a matrix rate's for a cell).
    pub file: PathBuf,
    pub good: Option<Block>,
    pub bad: Vec<Block>,
    /// A missing approved reference fails (`check --gate`, the matrix);
    /// else it is a skip.
    pub required: bool,
    /// Why the file could not be read.
    pub error: Option<String>,
}

impl Refs {
    /// Scenario `id`'s references in `file` (as read).
    pub fn of(
        file: &Path,
        read: &Result<Option<RefFile>, Error>,
        id: &str,
        required: bool,
    ) -> Refs {
        let mut r = Refs {
            file: file.to_path_buf(),
            required,
            ..Refs::default()
        };
        match read {
            Ok(Some(f)) => {
                r.good = f.good(id).cloned();
                r.bad = f.bad(id).into_iter().cloned().collect();
            }
            Ok(None) => {}
            Err(e) => r.error = Some(e.to_string()),
        }
        r
    }
}

/// `sc`'s references in `dir` (`eval/refs/` by default).
pub fn for_scenario(sc: &Scenario, dir: &Path, required: bool) -> Refs {
    let file = file_for(dir, &sc.id, &sc.file.display().to_string());
    Refs::of(&file, &RefFile::load(&file), &sc.id, required)
}

/// A path as the repository names it (`eval/refs/x.ref`), else as given.
pub fn shown(path: &Path) -> String {
    let root = crate::scenario::eval_root();
    match root.parent().and_then(|r| path.strip_prefix(r).ok()) {
        Some(p) => p.display().to_string(),
        None => path.display().to_string(),
    }
}

// --- approving

/// What an approval did to one scenario.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approved {
    pub id: String,
    pub file: PathBuf,
    /// `false`: the run matched the reference already (nothing written).
    pub wrote: bool,
}

/// The scenario directories under `path`: itself (`<run>/<id>`), each of
/// its scenarios (`<run>`), or each matrix cell's (`<matrix>`,
/// `<matrix>/<cell>@<hz>`).
pub fn entry_dirs(path: &Path) -> Result<Vec<PathBuf>, Error> {
    if path.join("trace.json").is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let rd = std::fs::read_dir(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let mut subs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    subs.sort();
    let mut out = Vec::new();
    for s in subs {
        if s.join("trace.json").is_file() {
            out.push(s);
        } else if let Ok(rd) = std::fs::read_dir(&s) {
            let mut inner: Vec<PathBuf> = rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.join("trace.json").is_file())
                .collect();
            inner.sort();
            out.extend(inner);
        }
    }
    if out.is_empty() {
        return Err(Error::Usage(format!(
            "{}: no run here (a run's report, a scenario's directory in one, a matrix or a cell of it)",
            path.display()
        )));
    }
    Ok(out)
}

/// What `approve` and `verdict` are asked.
#[derive(Clone, Debug)]
pub struct Ask {
    pub verdict: Verdict,
    pub note: String,
    pub approver: String,
    /// The reference directory (`eval/refs/` by default).
    pub dir: PathBuf,
}

/// Records each run in `dirs` (scenario directories, [`entry_dirs`]) as
/// `ask` says: the approved recording of its scenario (replacing the one
/// before, which must have had another note), or a bad one. A run that
/// matches its reference within a quantum is left alone.
pub fn approve(dirs: &[PathBuf], ask: &Ask) -> Result<Vec<Approved>, Error> {
    if ask.note.trim().is_empty() {
        return Err(Error::Usage(
            "a note is required (--note \"why this recording is right\")".into(),
        ));
    }
    let mut files: BTreeMap<PathBuf, RefFile> = BTreeMap::new();
    let mut dirty: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();
    let date = today();
    for d in dirs {
        let e = report::read_entry(d)?;
        if e.trace.frames.is_empty() {
            return Err(Error::Usage(format!(
                "{}: the run drew nothing (an error run has no recording)",
                d.display()
            )));
        }
        let file = file_for(&ask.dir, &e.id, &e.trace.scenario.file);
        if !files.contains_key(&file) {
            let f = RefFile::load(&file)?.unwrap_or_default();
            files.insert(file.clone(), f);
        }
        let Some(f) = files.get_mut(&file) else {
            continue;
        };
        let rec = Recording::from_trace(&e.trace);
        let same = |b: &Block| {
            b.recording()
                .ok()
                .is_some_and(|r| recording::diff(&r, &rec, Tol::QUANTUM).is_none())
        };
        let existing: Vec<&Block> = match ask.verdict {
            Verdict::Good => f.good(&e.id).into_iter().collect(),
            Verdict::Bad => f.bad(&e.id),
        };
        if existing.iter().any(|b| same(b)) {
            out.push(Approved {
                id: e.id.clone(),
                file,
                wrote: false,
            });
            continue;
        }
        if ask.verdict == Verdict::Good
            && let Some(b) = existing.first()
            && b.note == ask.note
        {
            return Err(Error::Usage(format!(
                "{}: the recording changed and the note is the old one's (\"{}\"): say why the new one is right",
                e.id, b.note
            )));
        }
        let rev = format!(
            "{}{}",
            e.trace.build.rev,
            if e.trace.build.dirty { "+changes" } else { "" }
        );
        let meta = Meta {
            verdict: ask.verdict,
            note: &ask.note,
            date: &date,
            rev: &rev,
            approver: &ask.approver,
            scenario: &source_hash(&e.source),
        };
        f.put(Block::new(&e.id, &meta, recording::encode(&rec)));
        dirty.insert(file.clone());
        out.push(Approved {
            id: e.id.clone(),
            file,
            wrote: true,
        });
    }
    for path in dirty {
        if let Some(f) = files.get(&path) {
            f.save(&path)?;
        }
    }
    Ok(out)
}

/// The scenario directories of `run` whose recording left its approved
/// reference, or that have none.
pub fn changed(run: &Path, dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files: BTreeMap<PathBuf, Option<RefFile>> = BTreeMap::new();
    let mut out = Vec::new();
    for d in entry_dirs(run)? {
        let e = report::read_entry(&d)?;
        let file = file_for(dir, &e.id, &e.trace.scenario.file);
        if !files.contains_key(&file) {
            let f = RefFile::load(&file)?;
            files.insert(file.clone(), f);
        }
        let good = files
            .get(&file)
            .and_then(|f| f.as_ref())
            .and_then(|f| f.good(&e.id));
        let leaves = match good.map(|b| b.recording()) {
            Some(Ok(r)) => {
                recording::diff(&r, &Recording::from_trace(&e.trace), Tol::QUANTUM).is_some()
            }
            Some(Err(_)) | None => true,
        };
        if leaves {
            out.push(d);
        }
    }
    Ok(out)
}

/// The cells a matrix run (`<matrix>/matrix.json`) ran at each rate it
/// covered whole (every effect alone and every pair).
pub fn whole_matrix_rates(run: &Path) -> Result<BTreeMap<u32, BTreeSet<String>>, Error> {
    let path = run.join("matrix.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(BTreeMap::new());
    };
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    let n = v["effects"].as_array().map_or(0, |a| a.len());
    let mut by_hz: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
    for c in v["cells"].as_array().into_iter().flatten() {
        let (Some(hz), Some(id)) = (c["hz"].as_u64(), c["id"].as_str()) else {
            continue;
        };
        by_hz.entry(hz as u32).or_default().insert(id.to_string());
    }
    by_hz.retain(|_, ids| ids.len() == n * (n + 1) / 2);
    Ok(by_hz)
}

/// The ids in matrix rate `hz`'s reference file (in `dir`) that `ids`, a
/// whole run at that rate, does not have: an effect removed or renamed.
pub fn stale_matrix(dir: &Path, hz: u32, ids: &BTreeSet<String>) -> Result<Vec<String>, Error> {
    let file = dir.join(format!("matrix-{hz}.ref"));
    let Some(f) = RefFile::load(&file)? else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = f
        .blocks
        .iter()
        .filter(|b| !ids.contains(&b.id))
        .map(|b| b.id.clone())
        .collect();
    out.dedup();
    Ok(out)
}

/// Removes, for each rate a matrix run covered whole, the blocks of cells
/// it did not have; returns the ids removed.
pub fn prune_matrix(run: &Path, dir: &Path) -> Result<Vec<String>, Error> {
    let mut removed = Vec::new();
    for (hz, ids) in whole_matrix_rates(run)? {
        let file = dir.join(format!("matrix-{hz}.ref"));
        let Some(mut f) = RefFile::load(&file)? else {
            continue;
        };
        let before = f.blocks.len();
        f.blocks.retain(|b| {
            let keep = ids.contains(&b.id);
            if !keep {
                removed.push(b.id.clone());
            }
            keep
        });
        if f.blocks.len() != before {
            f.save(&file)?;
        }
    }
    removed.dedup();
    Ok(removed)
}

// --- the CI side

/// Every reference file in `dir` read and checked: each parses as
/// `approve` writes it, each block's seal holds (nothing edited by hand),
/// has a note and an approver (not `unknown`), decodes, and lives in its file (a
/// scenario's alone, a matrix cell's in its rate's). One line per
/// problem.
pub fn lint(dir: &Path) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::Io(dir.display().to_string(), e)),
    };
    let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Some(stem) = name.strip_suffix(".ref") else {
            out.push(format!(
                "{}: not a reference file (only <id>.ref and matrix-<hz>.ref)",
                shown(&p)
            ));
            continue;
        };
        let text =
            std::fs::read_to_string(&p).map_err(|e| Error::Io(p.display().to_string(), e))?;
        let f = match RefFile::parse(&text) {
            Ok(f) => f,
            Err(e) => {
                out.push(format!("{}: {e}", shown(&p)));
                continue;
            }
        };
        if f.render() != text {
            out.push(format!(
                "{}: not as `approve` writes it (its header, order or spacing edited by hand)",
                shown(&p)
            ));
        }
        let hz: Option<u32> = stem.strip_prefix("matrix-").and_then(|h| h.parse().ok());
        for b in &f.blocks {
            let at = format!("{}:{} ({})", shown(&p), b.line, b.id);
            let home = match hz {
                Some(h) => b.id.starts_with("m.") && b.id.ends_with(&format!(".{h}")),
                None => b.id == stem,
            };
            if !home {
                out.push(format!("{at}: in the wrong file"));
            }
            if b.seal != b.compute_seal() {
                out.push(format!(
                    "{at}: its seal does not hold: edited by hand (approve rewrites a reference)"
                ));
            }
            if b.note.trim().is_empty() {
                out.push(format!("{at}: no note"));
            }
            if no_approver(&b.approver) {
                out.push(format!(
                    "{at}: no approver (approver \"{}\"): approve again with --approver or ${APPROVER_ENV}",
                    b.approver
                ));
            }
            if let Err(e) = b.recording() {
                out.push(format!("{at}: {e}"));
            }
        }
        if f.blocks.is_empty() {
            out.push(format!("{}: no recording in it", shown(&p)));
        }
    }
    Ok(out)
}

/// A reference file (`new`) against its version at the merge base
/// (`old`, `None` when it is new): a recording that changed under the
/// note it had is refused. `shown` names the file in the lines.
pub fn note_rule(old: Option<&str>, new: &str, shown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(new) = RefFile::parse(new) else {
        // `lint` reports it.
        return out;
    };
    let old = old.and_then(|t| RefFile::parse(t).ok()).unwrap_or_default();
    for b in &new.blocks {
        let before: Vec<&Block> = old
            .blocks
            .iter()
            .filter(|x| x.id == b.id && x.verdict == b.verdict)
            .collect();
        if before.iter().any(|x| x.body == b.body) {
            continue;
        }
        if let Some(x) = before.iter().find(|x| x.note == b.note) {
            out.push(format!(
                "{shown}:{} ({}): the {} recording changed but its note did not (\"{}\", {}): approve it with a note that says why",
                b.line,
                b.id,
                b.verdict.name(),
                x.note,
                x.date
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_become_dates() {
        assert_eq!(super::civil(0), "1970-01-01");
        assert_eq!(super::civil(20_743), "2026-10-17");
        assert_eq!(super::civil(11_016), "2000-02-29");
    }
}
