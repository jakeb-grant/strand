//! The commands (docs/eval.md, "Quick start"): `run` and `check` load
//! scenarios from files or directories, run them and write a report
//! (`report::write_run`) under an output directory, printing one line
//! per check and per failing finding; `explain`, `verdict` and `ab` read
//! reports back.
//!
//! `check --gate` is CI's step (scripts/eval/ci.sh step 5): it refuses
//! scenarios in `eval/scenarios/local/` and an empty set, and fails when
//! any scenario fails a check or passes one it is marked `expect_fail`
//! for (XPASS).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::checks::Reference;
use crate::harness::{self, RunOptions};
use crate::report::{self, Entry, Frames, RunInfo};
use crate::scenario::{Place, Scenario};
use crate::select::Selector;
use crate::trace::BuildInfo;
use crate::{Error, check_scenario};

/// The `.scn` files `paths` name: files as given, directories walked
/// (sorted, so a run's order is stable).
pub fn collect(paths: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p).map_err(|e| Error::Io(p.display().to_string(), e))?;
        if meta.is_file() {
            out.push(p.clone());
            continue;
        }
        let mut stack = vec![p.clone()];
        let mut found = Vec::new();
        while let Some(dir) = stack.pop() {
            let rd =
                std::fs::read_dir(&dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
            for e in rd {
                let e = e.map_err(|e| Error::Io(dir.display().to_string(), e))?;
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "scn") {
                    found.push(path);
                }
            }
        }
        found.sort();
        out.extend(found);
    }
    Ok(out)
}

/// The build a run is of: the checkout's revision and whether it has
/// changes (`unknown` outside a git checkout).
pub fn build_info() -> BuildInfo {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(crate::scenario::eval_root())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let rev = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    BuildInfo {
        rev,
        dirty,
        profile: if cfg!(debug_assertions) {
            "dev".into()
        } else {
            "release".into()
        },
        features: vec!["eval-hooks".into()],
    }
}

/// The output directory: `--out`, else
/// `target/eval/runs/<seconds>-<rev>[-dirty]`, with `target/eval/latest`
/// pointing at it.
pub fn out_dir(out: Option<PathBuf>, build: &BuildInfo) -> Result<PathBuf, Error> {
    if let Some(o) = out {
        std::fs::create_dir_all(&o).map_err(|e| Error::Io(o.display().to_string(), e))?;
        return Ok(o);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = format!(
        "{stamp}-{}{}",
        build.rev,
        if build.dirty { "-dirty" } else { "" }
    );
    let dir = PathBuf::from(format!("target/eval/runs/{name}"));
    std::fs::create_dir_all(&dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
    let latest = Path::new("target/eval/latest");
    let _ = std::fs::remove_file(latest);
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(format!("runs/{name}"), latest);
    Ok(dir)
}

/// The arguments of a command: words and `--flag [value]` options.
#[derive(Debug, Default)]
struct Args {
    words: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

impl Args {
    /// Reads `args`; `valued` are the options that take a value, `bare`
    /// those that do not.
    fn parse(cmd: &str, args: &[String], valued: &[&str], bare: &[&str]) -> Result<Args, Error> {
        let mut a = Args::default();
        let mut it = args.iter();
        while let Some(x) = it.next() {
            if valued.contains(&x.as_str()) {
                let v = it
                    .next()
                    .ok_or_else(|| Error::Usage(format!("{cmd}: {x} needs a value")))?;
                a.opts.push((x.clone(), Some(v.clone())));
            } else if bare.contains(&x.as_str()) {
                a.opts.push((x.clone(), None));
            } else if x.starts_with('-') && x.len() > 1 {
                return Err(Error::Usage(format!("{cmd}: unknown option `{x}`")));
            } else {
                a.words.push(x.clone());
            }
        }
        Ok(a)
    }

    fn has(&self, flag: &str) -> bool {
        self.opts.iter().any(|(f, _)| f == flag)
    }

    fn value(&self, flags: &[&str]) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|(f, _)| flags.contains(&f.as_str()))
            .and_then(|(_, v)| v.as_deref())
    }

    fn values(&self, flag: &str) -> Vec<&str> {
        self.opts
            .iter()
            .filter(|(f, _)| f == flag)
            .filter_map(|(_, v)| v.as_deref())
            .collect()
    }
}

fn frames_opt(a: &Args, n: usize) -> Result<Frames, Error> {
    match a.value(&["--frames"]) {
        None => Ok(Frames::default_for(n)),
        Some(f) => Frames::parse(f)
            .ok_or_else(|| Error::Usage(format!("--frames {f}: all, failing or none"))),
    }
}

/// A scenario that could not run, as a report entry.
fn error_entry(sc: &Scenario, e: &Error) -> Entry {
    Entry {
        id: sc.id.clone(),
        file: sc.file.display().to_string(),
        source: sc.source.clone(),
        error: Some(e.to_string()),
        ..Entry::default()
    }
}

/// `strand-eval run <scn|dir>... [--out DIR] [--reduced-motion]
/// [--frames all|failing|none] [--backend cpu|gpu]`: runs each scenario
/// and writes its trace and report, without checking it. `--backend`
/// runs that leg whatever the scenario says (a `both` scenario runs its
/// CPU leg otherwise).
pub fn run(args: &[String]) -> Result<(), Error> {
    let a = Args::parse(
        "run",
        args,
        &["--out", "-o", "--frames", "--backend"],
        &["--reduced-motion"],
    )?;
    if a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval run [--out DIR] [--frames all|failing|none] [--reduced-motion] [--backend cpu|gpu] <scenario.scn|dir>..."
                .into(),
        ));
    }
    let backend = match a.value(&["--backend"]) {
        None => None,
        Some("cpu") => Some(crate::scenario::BackendMode::Cpu),
        Some("gpu") => Some(crate::scenario::BackendMode::Gpu),
        Some(b) => {
            return Err(Error::Usage(format!(
                "--backend {b}: cpu or gpu (`check` runs a `both` scenario's two legs)"
            )));
        }
    };
    let files = collect(&a.words.iter().map(PathBuf::from).collect::<Vec<_>>())?;
    let build = build_info();
    let out = out_dir(a.value(&["--out", "-o"]).map(PathBuf::from), &build)?;
    let frames = frames_opt(&a, files.len())?;
    let opts = RunOptions {
        reduced_motion: a.has("--reduced-motion"),
        images: frames != Frames::None,
        build,
        backend,
    };
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    for f in files {
        let sc = Scenario::load(&f)?;
        let o = harness::run(&sc, &opts)?;
        for n in &o.notes {
            let l = format!("{} note: {n}", sc.id);
            println!("{l}");
            lines.push(l);
        }
        let l = format!(
            "{}: {} frames, digest {}",
            sc.id,
            o.trace.frames.len(),
            o.trace.digest
        );
        println!("{l}");
        lines.push(l);
        entries.push(Entry {
            id: sc.id.clone(),
            file: sc.file.display().to_string(),
            source: sc.source.clone(),
            trace: o.trace,
            images: o.images,
            notes: o.notes,
            ..Entry::default()
        });
    }
    let info = RunInfo {
        command: "run".into(),
        out: out.clone(),
        frames,
    };
    report::write_run(&info, &mut entries, &lines)?;
    println!("report: {}/index.html", out.display());
    Ok(())
}

/// `strand-eval check [--gate] <scn|dir>... [--out DIR] [--frames
/// all|failing|none]`: runs and checks each scenario, and fails
/// ([`Error::Failed`]) when any fails. Writes the report
/// (`report::write_run`): per scenario its trace, checks, frames and
/// contact sheets; `summary.txt` (the lines printed), `summary.md`,
/// `summary.json` and `index.html`.
pub fn check(args: &[String]) -> Result<(), Error> {
    let a = Args::parse(
        "check",
        args,
        &["--out", "-o", "--frames"],
        &["--gate", "--reduced-motion"],
    )?;
    if a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval check [--gate] [--out DIR] [--frames all|failing|none] <scenario.scn|dir>..."
                .into(),
        ));
    }
    let gate = a.has("--gate");
    let files = collect(&a.words.iter().map(PathBuf::from).collect::<Vec<_>>())?;
    if gate && files.is_empty() {
        return Err(Error::Usage(
            "check --gate: no scenarios found (a gate over nothing passes nothing)".into(),
        ));
    }
    let build = build_info();
    let out = out_dir(a.value(&["--out", "-o"]).map(PathBuf::from), &build)?;
    let frames = frames_opt(&a, files.len())?;
    let opts = RunOptions {
        reduced_motion: a.has("--reduced-motion"),
        images: true,
        build,
        backend: None,
    };
    let mut lines = Vec::new();
    let mut failed = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for f in &files {
        let sc = Scenario::load(f)?;
        if gate && sc.place == Place::Local {
            return Err(Error::Scenario {
                file: f.display().to_string(),
                line: 1,
                msg: "eval/scenarios/local/ is the owner's own and never gates".into(),
            });
        }
        if ids.contains(&sc.id) {
            return Err(Error::Scenario {
                file: f.display().to_string(),
                line: 1,
                msg: format!("id `{}` is another scenario's too", sc.id),
            });
        }
        ids.push(sc.id.clone());
        let c = match check_scenario(&sc, &opts) {
            Ok(c) => c,
            Err(e) => {
                // A scenario that cannot run fails the gate, named.
                let l = format!("{} ERROR: {e}", sc.id);
                println!("{l}");
                lines.push(l);
                failed.push(sc.id.clone());
                entries.push(error_entry(&sc, &e));
                continue;
            }
        };
        for n in &c.run.notes {
            let l = format!("{} note: {n}", sc.id);
            println!("{l}");
            lines.push(l);
        }
        for l in c.report.lines(&c.run.trace) {
            println!("{l}");
            lines.push(l);
        }
        if !c.report.ok() {
            failed.push(sc.id.clone());
        }
        let verdict = Reference::load(&crate::refs::ref_path(&sc.id))?;
        entries.push(Entry {
            id: sc.id.clone(),
            file: sc.file.display().to_string(),
            source: sc.source.clone(),
            trace: c.run.trace,
            report: Some(c.report),
            images: c.run.images,
            notes: c.run.notes,
            verdict,
            ..Entry::default()
        });
    }
    let total = files.len();
    let tail = if failed.is_empty() {
        format!("{total} scenarios: all pass")
    } else {
        format!(
            "{total} scenarios: {} failed ({})",
            failed.len(),
            failed.join(", ")
        )
    };
    println!("{tail}");
    lines.push(tail.clone());
    let info = RunInfo {
        command: if gate { "check --gate" } else { "check" }.into(),
        out: out.clone(),
        frames,
    };
    report::write_run(&info, &mut entries, &lines)?;
    println!("report: {}/index.html", out.display());
    if !failed.is_empty() {
        return Err(Error::Failed(tail));
    }
    Ok(())
}

/// `a..b`, `a..`, `..b` or `k` as an inclusive tick range.
fn tick_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once("..") {
        Some((a, b)) => {
            let a = if a.is_empty() { 0 } else { a.parse().ok()? };
            let b = if b.is_empty() {
                u32::MAX
            } else {
                b.parse().ok()?
            };
            Some((a, b))
        }
        None => {
            let k = s.parse().ok()?;
            Some((k, k))
        }
    }
}

/// `strand-eval explain <run>/<id> [--node SEL] [--frames a..b] [--check
/// NAME]`: a node's frames as a text table (by default the nodes of the
/// failing findings).
pub fn explain(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("explain", args, &["--node", "--frames", "--check"], &[])?;
    let [dir] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval explain <run>/<id> [--node SEL] [--frames a..b] [--check NAME]"
                .into(),
        ));
    };
    let e = report::read_entry(Path::new(dir))?;
    let node = match a.value(&["--node"]) {
        Some(s) => Some(Selector::parse(s).map_err(|m| Error::Usage(format!("--node: {m}")))?),
        None => None,
    };
    let frames = match a.value(&["--frames"]) {
        Some(f) => {
            Some(tick_range(f).ok_or_else(|| Error::Usage(format!("--frames {f}: a..b ticks")))?)
        }
        None => None,
    };
    let findings: Vec<_> = e
        .report
        .iter()
        .flat_map(|r| r.results.iter())
        .flat_map(|r| r.findings.iter().cloned())
        .collect();
    let ask = report::explain::Ask {
        node,
        frames,
        check: a.value(&["--check"]).map(String::from),
    };
    let text = report::explain::explain(&e.trace, &findings, &ask).map_err(Error::Usage)?;
    print!("{text}");
    Ok(())
}

/// `strand-eval verdict <run>/<id> good|bad [--note TEXT]`: records the
/// owner's verdict as `eval/refs/<id>.ref.json`.
pub fn verdict(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("verdict", args, &["--note", "--refs"], &[])?;
    let [dir, v] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval verdict <run>/<id> good|bad [--note TEXT]".into(),
        ));
    };
    let refs = a.value(&["--refs"]).map(PathBuf::from);
    let out = crate::refs::record(
        Path::new(dir),
        v,
        a.value(&["--note"]).unwrap_or(""),
        refs.as_deref(),
    )?;
    println!("recorded {v}: {}", out.display());
    Ok(())
}

/// The entries of a report directory (a run's, or one scenario's).
fn entries_of(path: &Path) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    for d in report::entry_dirs(path)? {
        let mut e = report::read_entry(&d)?;
        e.verdict = Reference::load(&crate::refs::ref_path(&e.id))?;
        out.push(e);
    }
    Ok(out)
}

/// `strand-eval ab <A> <B> [-o DIR] [--label-a TEXT] [--label-b TEXT]`:
/// the two reports' scenarios in one A/B page (`DIR/index.html`).
pub fn ab(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("ab", args, &["--out", "-o", "--label-a", "--label-b"], &[])?;
    let [pa, pb] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval ab <run A> <run B> [-o DIR] [--label-a TEXT] [--label-b TEXT]"
                .into(),
        ));
    };
    let ea = entries_of(Path::new(pa))?;
    let eb = entries_of(Path::new(pb))?;
    let label = |es: &[Entry], dir: &str| {
        es.first().map_or_else(
            || dir.to_string(),
            |e| {
                format!(
                    "{}{} ({dir})",
                    e.trace.build.rev,
                    if e.trace.build.dirty { "+" } else { "" }
                )
            },
        )
    };
    let la = a
        .value(&["--label-a"])
        .map_or_else(|| label(&ea, pa), String::from);
    let lb = a
        .value(&["--label-b"])
        .map_or_else(|| label(&eb, pb), String::from);
    let out = match a.value(&["--out", "-o"]) {
        Some(o) => PathBuf::from(o),
        None => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            PathBuf::from(format!("target/eval/ab-{stamp}"))
        }
    };
    let cmp = crate::ab::compare(&ea, &eb);
    let page = report::html::Page {
        title: format!("strand-eval A/B: {la} vs {lb}"),
        runs: vec![
            report::html::PageRun {
                label: la,
                out: pa.clone(),
                frames: Frames::All,
                entries: &ea,
            },
            report::html::PageRun {
                label: lb,
                out: pb.clone(),
                frames: Frames::All,
                entries: &eb,
            },
        ],
        ab: Some(cmp.clone()),
        budget: report::html::BUDGET,
    };
    let html = report::html::page(&page)?;
    report::write(&out.join("index.html"), html.as_bytes())?;
    report::write(
        &out.join("ab.json"),
        serde_json::to_string_pretty(&cmp)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    for (id, v) in cmp["scenarios"].as_object().into_iter().flatten() {
        if v["same"].as_bool() == Some(true) {
            println!("{id}: the same frames");
            continue;
        }
        println!("{id}: differs");
        for d in v["diverge"].as_array().into_iter().flatten().take(12) {
            println!(
                "  {} k{} {}: {} -> {}{}",
                d["label"].as_str().unwrap_or("?"),
                d["k"],
                d["what"].as_str().unwrap_or("?"),
                d["a"].as_str().unwrap_or("?"),
                d["b"].as_str().unwrap_or("?"),
                if d["uncertain"].as_bool() == Some(true) {
                    " (matched by sibling index)"
                } else {
                    ""
                }
            );
        }
    }
    println!("page: {}/index.html", out.display());
    Ok(())
}

/// `strand-eval matrix [--effects a,b,…|all] [--hz 60,144] [--singles]
/// [--jobs N] [--out DIR]`: the effect matrix (`matrix::run`), its
/// report in DIR (default `target/eval/matrix-<seconds>`); fails when a
/// cell fails a check known.toml does not list, a listed one passes, or a
/// cell cannot run.
pub fn matrix(args: &[String]) -> Result<(), Error> {
    use crate::matrix::{self, Outcome, Plan};
    let a = Args::parse(
        "matrix",
        args,
        &["--effects", "--hz", "--jobs", "--out", "-o", "--known"],
        &["--singles"],
    )?;
    if !a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval matrix [--effects a,b,…|all] [--hz 60,144] [--singles] [--jobs N] [--out DIR]"
                .into(),
        ));
    }
    let mut plan = Plan::new(build_info());
    if let Some(e) = a.value(&["--effects"])
        && e != "all"
    {
        plan.effects = Some(e.split(',').map(|x| x.trim().to_string()).collect());
    }
    if let Some(h) = a.value(&["--hz"]) {
        plan.hz = h
            .split(',')
            .map(|x| x.trim().parse::<u32>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::Usage(format!("--hz {h}: rates like 60,144")))?;
        if plan.hz.is_empty() || plan.hz.iter().any(|h| *h == 0 || *h > 1000) {
            return Err(Error::Usage(format!("--hz {h}: rates from 1 to 1000")));
        }
    }
    if let Some(j) = a.value(&["--jobs"]) {
        plan.jobs = j
            .parse()
            .map_err(|_| Error::Usage(format!("--jobs {j}: a count")))?;
    } else {
        plan.jobs = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .clamp(1, 8);
    }
    if let Some(k) = a.value(&["--known"]) {
        plan.known_file = PathBuf::from(k);
    }
    plan.singles = a.has("--singles");
    let out = match a.value(&["--out", "-o"]) {
        Some(o) => PathBuf::from(o),
        None => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            PathBuf::from(format!("target/eval/matrix-{stamp}"))
        }
    };
    let mut done = matrix::run(&plan)?;
    matrix::write(&out, &plan, &mut done)?;
    let mut gating = 0;
    for d in &done {
        match d.outcome {
            Outcome::Pass | Outcome::Na => {}
            _ => {
                if d.outcome.gates() {
                    gating += 1;
                }
                println!(
                    "{} {:?}: {}",
                    d.cell.dir_name(),
                    d.outcome,
                    if d.failed.is_empty() {
                        d.lines.first().cloned().unwrap_or_default()
                    } else {
                        d.failed.join(", ")
                    }
                );
            }
        }
    }
    let count = |o: Outcome| done.iter().filter(|d| d.outcome == o).count();
    let tail = format!(
        "{} cells: {} pass, {} known, {} n/a, {} fail, {} stale, {} error",
        done.len(),
        count(Outcome::Pass),
        count(Outcome::Known),
        count(Outcome::Na),
        count(Outcome::Fail),
        count(Outcome::Stale),
        count(Outcome::Error)
    );
    println!("{tail}");
    println!("report: {}/index.html", out.display());
    if gating > 0 {
        return Err(Error::Failed(tail));
    }
    Ok(())
}

/// `strand-eval promote <run>/<id> --as NAME [--xfail CHECK]... [--note
/// TEXT] [--to DIR] [--force]`: the run's scenario as a regression
/// (default eval/scenarios/regressions/NAME.scn).
pub fn promote(args: &[String]) -> Result<(), Error> {
    let a = Args::parse(
        "promote",
        args,
        &["--as", "--xfail", "--note", "--to"],
        &["--force"],
    )?;
    let ([from], Some(name)) = (a.words.as_slice(), a.value(&["--as"])) else {
        return Err(Error::Usage(
            "usage: strand-eval promote <run>/<id> --as NAME [--xfail CHECK]... [--note TEXT] [--to DIR]"
                .into(),
        ));
    };
    let ask = crate::promote::Ask {
        from: PathBuf::from(from),
        name: name.to_string(),
        xfail: a.values("--xfail").into_iter().map(String::from).collect(),
        note: a.value(&["--note"]).map(String::from),
        to: a.value(&["--to"]).map(PathBuf::from),
        force: a.has("--force"),
    };
    let path = crate::promote::promote(&ask)?;
    println!("wrote {}", path.display());
    println!(
        "run it: scripts/container/run.sh eval check {}",
        path.display()
    );
    Ok(())
}

/// `strand-eval bench <run>... [--out FILE]`: per scenario, the p50, p95
/// and max of each cost a paint records, as a markdown table (wall
/// times advisory). It never fails on what it measures.
pub fn bench(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("bench", args, &["--out", "-o"], &[])?;
    if a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval bench <run>... [--out FILE]".into(),
        ));
    }
    let mut scenarios: std::collections::BTreeMap<String, crate::bench::Samples> =
        std::collections::BTreeMap::new();
    for w in &a.words {
        for d in report::entry_dirs(Path::new(w))? {
            let e = report::read_entry(&d)?;
            crate::bench::sample(scenarios.entry(e.id.clone()).or_default(), &e.trace);
        }
    }
    let table = crate::bench::table(&scenarios);
    print!("{table}");
    if let Some(o) = a.value(&["--out", "-o"]) {
        report::write(Path::new(o), table.as_bytes())?;
        println!("wrote {o}");
    }
    Ok(())
}

/// `strand-eval new <id> [--config F] [--from-steps FILE] [--to DIR]`:
/// a scenario skeleton in eval/scenarios/local/ (docs/eval.md,
/// "Scaffolding").
pub fn new(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("new", args, &["--config", "--from-steps", "--to"], &[])?;
    let [id] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval new <id> [--config F] [--from-steps FILE] [--to DIR]".into(),
        ));
    };
    let ask = crate::scaffold::Ask {
        id: id.clone(),
        config: a.value(&["--config"]).map(String::from),
        from_steps: a.value(&["--from-steps"]).map(PathBuf::from),
        to: a.value(&["--to"]).map(PathBuf::from),
    };
    let path = crate::scaffold::scaffold(&ask)?;
    println!("wrote {}", path.display());
    println!(
        "run it: scripts/container/run.sh eval check {}",
        path.display()
    );
    Ok(())
}
