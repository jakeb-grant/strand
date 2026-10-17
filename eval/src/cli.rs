//! The `run` and `check` commands (docs/eval.md, "Quick start"): load
//! scenarios from files or directories, run them, write each run's trace
//! and checks under an output directory, and print one line per check
//! and per failing finding.
//!
//! `check --gate` is CI's step (scripts/eval/ci.sh step 5): it refuses
//! scenarios in `eval/scenarios/local/` and an empty set, and fails when
//! any scenario fails a check or passes one it is marked `expect_fail`
//! for (XPASS).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::harness::{self, RunOptions};
use crate::scenario::{Place, Scenario};
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

fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
    }
    std::fs::write(path, text).map_err(|e| Error::Io(path.display().to_string(), e))
}

/// The output directory: `--out`, else `target/eval/runs/<seconds>`, with
/// `target/eval/latest` pointing at it.
fn out_dir(out: Option<PathBuf>) -> Result<PathBuf, Error> {
    if let Some(o) = out {
        return Ok(o);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let dir = PathBuf::from(format!("target/eval/runs/{stamp}"));
    std::fs::create_dir_all(&dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
    let latest = Path::new("target/eval/latest");
    let _ = std::fs::remove_file(latest);
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(format!("runs/{stamp}"), latest);
    Ok(dir)
}

/// What `run` and `check` read from their arguments.
#[derive(Debug, Default)]
struct Args {
    paths: Vec<PathBuf>,
    out: Option<PathBuf>,
    gate: bool,
    reduced_motion: bool,
}

fn parse(cmd: &str, args: &[String]) -> Result<Args, Error> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--out" | "-o" => {
                let v = it
                    .next()
                    .ok_or_else(|| Error::Usage(format!("{cmd}: --out needs a directory")))?;
                a.out = Some(PathBuf::from(v));
            }
            "--gate" if cmd == "check" => a.gate = true,
            "--reduced-motion" => a.reduced_motion = true,
            f if f.starts_with('-') => {
                return Err(Error::Usage(format!("{cmd}: unknown option `{f}`")));
            }
            p => a.paths.push(PathBuf::from(p)),
        }
    }
    if a.paths.is_empty() {
        return Err(Error::Usage(format!(
            "usage: strand-eval {cmd} [--out DIR]{} <scenario.scn|dir>...",
            if cmd == "check" { " [--gate]" } else { "" }
        )));
    }
    Ok(a)
}

/// `strand-eval run <scn|dir>... [--out DIR] [--reduced-motion]`: runs
/// each scenario and writes its trace, without checking it.
pub fn run(args: &[String]) -> Result<(), Error> {
    let a = parse("run", args)?;
    let files = collect(&a.paths)?;
    let out = out_dir(a.out)?;
    let opts = RunOptions {
        reduced_motion: a.reduced_motion,
        images: false,
        build: build_info(),
    };
    for f in files {
        let sc = Scenario::load(&f)?;
        let o = harness::run(&sc, &opts)?;
        let path = out.join(&sc.id).join("trace.json");
        write(&path, &o.trace.to_json())?;
        for n in &o.notes {
            println!("{} note: {n}", sc.id);
        }
        println!(
            "{}: {} frames, digest {} -> {}",
            sc.id,
            o.trace.frames.len(),
            o.trace.digest,
            path.display()
        );
    }
    Ok(())
}

/// `strand-eval check [--gate] <scn|dir>... [--out DIR]`: runs and checks
/// each scenario, and fails ([`Error::Failed`]) when any fails. Writes `<out>/<id>/{trace,checks}.json`,
/// `<out>/summary.txt` (the lines printed) and `<out>/summary.md`.
pub fn check(args: &[String]) -> Result<(), Error> {
    let a = parse("check", args)?;
    let files = collect(&a.paths)?;
    if a.gate && files.is_empty() {
        return Err(Error::Usage(
            "check --gate: no scenarios found (a gate over nothing passes nothing)".into(),
        ));
    }
    let out = out_dir(a.out)?;
    let opts = RunOptions {
        reduced_motion: a.reduced_motion,
        images: false,
        build: build_info(),
    };
    let mut lines = Vec::new();
    let mut md = vec!["# strand-eval check".to_string(), String::new()];
    let mut failed = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for f in &files {
        let sc = Scenario::load(f)?;
        if a.gate && sc.place == Place::Local {
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
                md.push(format!("- **{}** error: {e}", sc.id));
                lines.push(l);
                failed.push(sc.id.clone());
                continue;
            }
        };
        let dir = out.join(&sc.id);
        write(&dir.join("trace.json"), &c.run.trace.to_json())?;
        let report = serde_json::to_string_pretty(&c.report)
            .map_err(|e| Error::Run(format!("checks.json: {e}")))?;
        write(&dir.join("checks.json"), &report)?;
        for n in &c.run.notes {
            let l = format!("{} note: {n}", sc.id);
            println!("{l}");
            lines.push(l);
        }
        for l in c.report.lines(&c.run.trace) {
            println!("{l}");
            lines.push(l);
        }
        let ok = c.report.ok();
        md.push(format!(
            "- **{}** {} ({} frames)",
            sc.id,
            if ok { "pass" } else { "FAIL" },
            c.run.trace.frames.len()
        ));
        for r in &c.report.results {
            for f in r
                .findings
                .iter()
                .filter(|f| f.sev == crate::checks::Sev::Fail)
            {
                md.push(format!("  - {}", crate::checks::describe(f, &c.run.trace)));
            }
        }
        if !ok {
            failed.push(sc.id.clone());
        }
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
    md.push(String::new());
    md.push(tail.clone());
    write(&out.join("summary.txt"), &(lines.join("\n") + "\n"))?;
    write(&out.join("summary.md"), &(md.join("\n") + "\n"))?;
    println!("report: {}", out.display());
    if !failed.is_empty() {
        return Err(Error::Failed(tail));
    }
    Ok(())
}
