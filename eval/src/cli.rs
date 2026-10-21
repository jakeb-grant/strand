//! The commands (docs/eval.md, "Quick start"): `run` and `check` load
//! scenarios from files or directories, run them and write a report
//! (`report::write_run`) under an output directory, printing one line
//! per check and per failing finding; `explain`, `approve`, `verdict` and
//! `ab` read reports back; `refs-check` is CI's check of the reference
//! files (docs/eval.md, "Reference recordings").
//!
//! `check --gate` is CI's step (scripts/eval/ci.sh step 5): it refuses
//! scenarios in `eval/scenarios/local/` and an empty set, and fails when
//! any scenario fails a check, passes one it is marked `expect_fail`
//! for (XPASS), or is CHANGED: it left its approved reference recording,
//! or has none.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::{self, RunOptions};
use crate::report::{self, Entry, Frames, RunInfo};
use crate::scenario::{Place, Scenario};
use crate::select::Selector;
use crate::trace::BuildInfo;
use crate::{Error, check_with};

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

/// (replay-owner) The options `replay` takes a value with. `strand
/// replay` keeps the same list (crates/strand/src/report/replay.rs
/// `VALUED`) to tell an option's value from the report directory;
/// eval/tests/replay.rs holds the two to each other.
pub const REPLAY_VALUED: [&str; 6] = ["--to", "--out", "-o", "--output", "--traced", "--frames"];

/// (report) `strand-eval replay <bundle> [--no-open] [--out DIR]
/// [--to SCN] [--output NAME] [--traced SECONDS|all] [--frames …]
/// [--gpu]`: a glitch report's bundle as a local scenario, run with
/// every check and held to what the live shell recorded (docs/eval.md,
/// "Replaying a glitch report").
///
/// (replay-owner) What `strand replay` hands off to, and what
/// `scripts/report-view.sh` runs in the container. It writes the replay
/// into `<bundle>/replay/` (`--out` elsewhere: the container's bundle is
/// read-only), prints the verdict in one plain line and the paths of
/// `replay.txt` and `index.html`, and opens the viewer with `xdg-open`
/// unless `--no-open`. It runs as a plain process on a desktop: its own
/// fake compositor and the CPU (`--gpu` adds the GPU leg), never the
/// session's Wayland display. It fails only when the replay cannot run;
/// the checks' findings are hints, never a failure.
pub fn replay(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("replay", args, &REPLAY_VALUED, &["--no-open", "--gpu"])?;
    let usage = "usage: strand-eval replay <bundle> [--no-open] [--out DIR] [--to SCN] [--output NAME] [--traced SECONDS|all] [--frames all|failing|none] [--gpu]";
    let [dir] = a.words.as_slice() else {
        return Err(Error::Usage(usage.into()));
    };
    let dir = Path::new(dir);
    if !dir.join("bundle.json").is_file() {
        return Err(Error::Usage(format!(
            "replay: {} is not a glitch report (no bundle.json)",
            dir.display()
        )));
    }
    // (replay-owner) Read and checked before anything is written: a
    // directory that holds some other `bundle.json` keeps its `replay/`.
    let bundle = crate::replay::Bundle::read(dir).map_err(|e| {
        Error::Usage(format!(
            "replay: {} is not a glitch report this strand-eval reads: {e}",
            dir.display()
        ))
    })?;
    if !bundle.config_dir().is_dir() {
        return Err(Error::Usage(format!(
            "replay: {} is not a glitch report (no config/, the copy of the shell's config)",
            dir.display()
        )));
    }
    drop(bundle);
    let traced = match a.value(&["--traced"]) {
        None => Some(crate::replay::plan::TRACED_S),
        Some("all") => None,
        Some(s) => Some(
            s.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| Error::Usage(format!("--traced {s}: seconds above 0, or `all`")))?,
        ),
    };
    let opts = crate::replay::Options {
        output: a.value(&["--output"]).map(str::to_string),
        traced,
        gpu: a.has("--gpu"),
    };
    let build = build_info();
    let out = match a.value(&["--out", "-o"]) {
        Some(o) => out_dir(Some(PathBuf::from(o)), &build)?,
        None => {
            // A replay of its own, in the report's folder, private as
            // the report is.
            let o = dir.join("replay");
            let _ = std::fs::remove_dir_all(&o);
            std::fs::create_dir_all(&o).map_err(|e| Error::Io(o.display().to_string(), e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&o, std::fs::Permissions::from_mode(0o700));
            }
            o
        }
    };
    let frames = match a.value(&["--frames"]) {
        None => None,
        Some(_) => Some(frames_opt(&a, 1)?),
    };
    let r = crate::replay::replay(
        dir,
        &opts,
        a.value(&["--to"]).map(Path::new),
        &out,
        frames,
        build,
    )?;
    let index = out.join("index.html");
    println!("{}", r.verdict.line());
    println!("replay: {}", out.join("replay.txt").display());
    println!("viewer: {}", index.display());
    if !a.has("--no-open") {
        open(&index);
    }
    Ok(())
}

/// (replay-owner) Opens `page` in the desktop's browser with `xdg-open`,
/// left running on its own; says where it is when that cannot start.
fn open(page: &Path) {
    let started = Command::new("xdg-open")
        .arg(page)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if started.is_err() {
        eprintln!(
            "strand-eval replay: no xdg-open; open {} in a browser",
            page.display()
        );
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
        ..RunOptions::default()
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
        replay: None,
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
        ..RunOptions::default()
    };
    let mut lines = Vec::new();
    let mut failed = Vec::new();
    let mut changed = 0;
    let mut ids: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    let refs_dir = crate::refs::dir();
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
        let refs = crate::refs::for_scenario(&sc, &refs_dir, gate);
        let c = match check_with(&sc, &opts, refs) {
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
        if c.report.changed_line().is_some() {
            changed += 1;
            let l = format!(
                "  approve: scripts/container/run.sh eval approve {}/{} --note \"why\"",
                out.display(),
                sc.id
            );
            println!("{l}");
            lines.push(l);
        }
        if !c.report.ok() {
            failed.push(sc.id.clone());
        }
        let (refs, reference) = report::refs_of(c.refs.as_ref(), &c.report, &c.run.trace);
        entries.push(Entry {
            id: sc.id.clone(),
            file: sc.file.display().to_string(),
            source: sc.source.clone(),
            trace: c.run.trace,
            report: Some(c.report),
            images: c.run.images,
            notes: c.run.notes,
            refs,
            reference,
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
    if changed > 0 {
        let l = format!(
            "{changed} CHANGED: once each change is seen to be right, scripts/container/run.sh eval approve --all-changed {} --note \"why\"",
            out.display()
        );
        println!("{l}");
        lines.push(l);
    }
    println!("{tail}");
    lines.push(tail.clone());
    let info = RunInfo {
        command: if gate { "check --gate" } else { "check" }.into(),
        out: out.clone(),
        frames,
        replay: None,
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

/// `strand-eval verdict <run>/<id> good|bad --note TEXT [--refs DIR]
/// [--approver NAME]`:
/// `good` approves the run (`approve`); `bad` records it as a known-bad
/// recording its scenario must not come back to.
pub fn verdict(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("verdict", args, &["--note", "--refs", "--approver"], &[])?;
    let [dir, v] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval verdict <run>/<id> good|bad --note TEXT".into(),
        ));
    };
    let verdict = crate::refs::Verdict::parse(v)
        .ok_or_else(|| Error::Usage(format!("verdict `{v}`: good or bad")))?;
    record(&a, std::slice::from_ref(dir), verdict, false)
}

/// `strand-eval approve <run>/<id>|<matrix>/<cell>@<hz>... --note TEXT
/// [--refs DIR] [--approver NAME]`, or `approve --all-changed <run> --note
/// TEXT`: writes
/// each run's recording as its scenario's approved reference
/// (`eval/refs/<id>.ref`, a matrix cell's in `eval/refs/matrix-<hz>.ref`)
/// with the note, today's date, the run's revision and the approver
/// (`--approver`, else `$STRAND_EVAL_APPROVER`; required). `--all-changed` takes the runs that left
/// their reference or have none, and prunes the blocks of cells a whole
/// matrix rate no longer has. (eval-finish) `--renote` rewrites the note
/// of the approved recording each run matches, keeping the recording.
pub fn approve(args: &[String]) -> Result<(), Error> {
    let a = Args::parse(
        "approve",
        args,
        &["--note", "--refs", "--approver"],
        &["--all-changed", "--renote"],
    )?;
    if a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval approve <run>/<id>|<matrix>/<cell>@<hz>... --note TEXT\n       strand-eval approve --all-changed <run> --note TEXT"
                .into(),
        ));
    }
    if a.has("--renote") && a.has("--all-changed") {
        return Err(Error::Usage(
            "--renote rewrites the notes of runs that match their references, \
             --all-changed takes those that do not: one or the other"
                .into(),
        ));
    }
    record(
        &a,
        &a.words,
        crate::refs::Verdict::Good,
        a.has("--all-changed"),
    )
}

fn record(
    a: &Args,
    words: &[String],
    verdict: crate::refs::Verdict,
    all_changed: bool,
) -> Result<(), Error> {
    let note = a.value(&["--note"]).unwrap_or("").trim().to_string();
    if note.is_empty() {
        return Err(Error::Usage(
            "a note is required: --note \"why this recording is right\" (or wrong, for bad)".into(),
        ));
    }
    let approver = crate::refs::approver(
        a.value(&["--approver"]),
        std::env::var(crate::refs::APPROVER_ENV).ok(),
    )?;
    let dir = a
        .value(&["--refs"])
        .map_or_else(crate::refs::dir, PathBuf::from);
    let mut dirs = Vec::new();
    for w in words {
        let p = Path::new(w);
        if all_changed {
            dirs.extend(crate::refs::changed(p, &dir)?);
        } else {
            let found = crate::refs::entry_dirs(p)?;
            if found.len() > 1 && verdict == crate::refs::Verdict::Bad {
                return Err(Error::Usage(format!(
                    "{w}: a bad verdict is one scenario's (<run>/<id>)"
                )));
            }
            dirs.extend(found);
        }
    }
    for d in &dirs {
        let e = report::read_entry(d)?;
        let reduced = e.report.as_ref().is_some_and(|r| {
            matches!(
                r.status(crate::checks::CheckId::Reference),
                Some(crate::checks::Status::Skip(why)) if why == crate::checks::reference::REDUCED
            )
        });
        if reduced {
            return Err(Error::Usage(format!(
                "{}: {}",
                d.display(),
                crate::checks::reference::REDUCED
            )));
        }
    }
    let ask = crate::refs::Ask {
        verdict,
        note,
        approver,
        dir: dir.clone(),
        renote: a.has("--renote"),
    };
    let done = crate::refs::approve(&dirs, &ask)?;
    let mut wrote = 0;
    for x in &done {
        if x.wrote {
            wrote += 1;
            println!(
                "{} {}: {}",
                if verdict == crate::refs::Verdict::Good {
                    "approved"
                } else {
                    "recorded bad"
                },
                x.id,
                crate::refs::shown(&x.file)
            );
        } else {
            println!("unchanged {}: it matches its reference", x.id);
        }
    }
    if all_changed {
        for w in words {
            for id in crate::refs::prune_matrix(Path::new(w), &dir)? {
                println!("pruned {id}: the matrix no longer has it");
            }
        }
    }
    println!("{wrote} written, {} unchanged", done.len() - wrote);
    Ok(())
}

/// `strand-eval refs-check [--base REV] [--refs DIR]`: CI's check of the
/// reference files: every block parses, holds its seal (nothing edited
/// by hand), has a note and an approver and lives in its file; with
/// `--base`, a recording that changed since REV under the same note is
/// refused (so every approval shows in review).
pub fn refs_check(args: &[String]) -> Result<(), Error> {
    let a = Args::parse("refs-check", args, &["--base", "--refs"], &[])?;
    if !a.words.is_empty() {
        return Err(Error::Usage(
            "usage: strand-eval refs-check [--base REV] [--refs DIR]".into(),
        ));
    }
    let dir = a
        .value(&["--refs"])
        .map_or_else(crate::refs::dir, PathBuf::from);
    let mut problems = crate::refs::lint(&dir)?;
    match a.value(&["--base"]) {
        Some(base) => problems.extend(note_changes(&dir, base)?),
        None => println!("refs-check: no --base: the note rule is not checked"),
    }
    for p in &problems {
        println!("{p}");
    }
    if problems.is_empty() {
        println!("refs-check: {} ok", crate::refs::shown(&dir));
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "refs-check: {} problem(s) in {}",
            problems.len(),
            crate::refs::shown(&dir)
        )))
    }
}

/// [`crate::refs::note_rule`] for each reference file that differs from
/// `base` (committed or not).
fn note_changes(dir: &Path, base: &str) -> Result<Vec<String>, Error> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let git = |args: &[&str]| -> Result<std::process::Output, Error> {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|e| Error::Io("git".into(), e))
    };
    let o = git(&["diff", "--name-only", "--relative", base, "--", "."])?;
    if !o.status.success() {
        return Err(Error::Run(format!(
            "git diff {base}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )));
    }
    let mut out = Vec::new();
    for name in String::from_utf8_lossy(&o.stdout).lines() {
        let path = dir.join(name);
        let Ok(new) = std::fs::read_to_string(&path) else {
            continue;
        };
        let old = git(&["show", &format!("{base}:./{name}")])?;
        let old = old
            .status
            .success()
            .then(|| String::from_utf8_lossy(&old.stdout).to_string());
        out.extend(crate::refs::note_rule(
            old.as_deref(),
            &new,
            &crate::refs::shown(&path),
        ));
    }
    Ok(out)
}

/// The entries of a report directory (a run's, or one scenario's).
fn entries_of(path: &Path) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    for d in report::entry_dirs(path)? {
        out.push(report::read_entry(&d)?);
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
                reference: false,
            },
            report::html::PageRun {
                label: lb,
                out: pb.clone(),
                frames: Frames::All,
                entries: &eb,
                reference: false,
            },
        ],
        ab: Some(cmp.clone()),
        replay: None,
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
    let stale = matrix::stale_refs(&plan, &done)?;
    let mut gating = stale.len();
    for id in &stale {
        println!(
            "STALE REFERENCE: {id}: no such cell (approve --all-changed {} prunes it)",
            out.display()
        );
    }
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
                // The diff summary under the CHANGED line.
                if d.outcome == Outcome::Changed
                    && let Some(r) = d.entry.as_ref().and_then(|e| e.refs.as_ref())
                {
                    for l in r.diff.iter().skip(1) {
                        println!("    {l}");
                    }
                }
            }
        }
    }
    let count = |o: Outcome| done.iter().filter(|d| d.outcome == o).count();
    let tail = format!(
        "{} cells: {} pass, {} known, {} n/a, {} fail, {} stale, {} changed, {} error",
        done.len(),
        count(Outcome::Pass),
        count(Outcome::Known),
        count(Outcome::Na),
        count(Outcome::Fail),
        count(Outcome::Stale),
        count(Outcome::Changed),
        count(Outcome::Error)
    );
    if count(Outcome::Changed) > 0 {
        println!(
            "{} CHANGED: once each change is seen to be right, scripts/container/run.sh eval approve --all-changed {} --note \"why\"",
            count(Outcome::Changed),
            out.display()
        );
    }
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
    let a = Args::parse(
        "new",
        args,
        &["--config", "--from-steps", "--to"],
        &["--explore"],
    )?;
    let [id] = a.words.as_slice() else {
        return Err(Error::Usage(
            "usage: strand-eval new <id> [--config F [--explore]] [--from-steps FILE] [--to DIR]"
                .into(),
        ));
    };
    let ask = crate::scaffold::Ask {
        id: id.clone(),
        config: a.value(&["--config"]).map(String::from),
        from_steps: a.value(&["--from-steps"]).map(PathBuf::from),
        to: a.value(&["--to"]).map(PathBuf::from),
        explore: a.has("--explore"),
    };
    let path = crate::scaffold::scaffold(&ask)?;
    println!("wrote {}", path.display());
    println!(
        "run it: scripts/container/run.sh eval {} {}",
        if ask.explore { "local" } else { "check" },
        path.display()
    );
    Ok(())
}

/// `strand-eval local [check's flags] [scn...]`: `check` over the
/// gitignored eval/scenarios/local/ (or the scenarios named in it);
/// `strand-eval local new <id> --config PATH [--to DIR]`: a starter
/// there that exercises what the config declares (`new --explore`).
/// `scripts/container/run.sh eval local` mounts the configs the local
/// scenarios name and the host's fonts, read-only (docs/eval.md, "Local
/// configs and fonts").
pub fn local(args: &[String]) -> Result<(), Error> {
    let dir = crate::scenario::eval_root().join("scenarios").join("local");
    if args.first().map(String::as_str) == Some("new") {
        let mut rest: Vec<String> = args[1..].to_vec();
        rest.push("--explore".into());
        return new(&rest);
    }
    if args.iter().any(|a| a == "--gate") {
        return Err(Error::Usage(
            "local: eval/scenarios/local/ is the owner's own and never gates".into(),
        ));
    }
    let mut words = Vec::new();
    let mut flags = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with('-') {
            flags.push(a.clone());
            if matches!(a.as_str(), "--out" | "-o" | "--frames")
                && let Some(v) = it.next()
            {
                flags.push(v.clone());
            }
        } else {
            words.push(a.clone());
        }
    }
    for w in &words {
        if crate::scenario::place_of(Path::new(w)) != Place::Local {
            return Err(Error::Usage(format!(
                "local: `{w}` is not in eval/scenarios/local/ (run committed scenarios with `check`)"
            )));
        }
    }
    if words.is_empty() {
        if !dir.is_dir() || collect(std::slice::from_ref(&dir))?.is_empty() {
            return Err(Error::Usage(format!(
                "local: no scenarios in {}: scaffold one with `local new <id> --config PATH`",
                dir.display()
            )));
        }
        words.push(dir.display().to_string());
    }
    flags.extend(words);
    check(&flags)
}
