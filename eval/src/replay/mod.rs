//! (report) `strand-eval replay <bundle>`: a glitch report's bundle
//! turned into a local scenario, run with every CPU check, and held to
//! what the live shell recorded (docs/eval.md, "Replaying a glitch
//! report"). It fails only when the replay cannot run; findings and
//! divergences are its output, not a failure.

pub mod bundle;
pub mod drawn;
pub mod fidelity;
pub mod plan;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::checks::Sev;
use crate::clock::T0;
use crate::harness::RunOptions;
use crate::report::{self, Entry, Frames, RunInfo};
use crate::scenario::Scenario;
use crate::trace::RunTrace;

pub use bundle::Bundle;
pub use drawn::Verdict;
pub use fidelity::Fidelity;
pub use plan::{Options, Plan};

/// The most motion lines `replay.txt` lists.
pub const MOTION_LINES: usize = 400;

/// What one replay made.
#[derive(Debug)]
pub struct Replayed {
    /// The scenario's id: its report is under `<out>/<id>/`.
    pub id: String,
    pub plan: Plan,
    pub scenario: PathBuf,
    pub fidelity: Fidelity,
    /// (replay-owner) The live boxes held to the replay's.
    pub verdict: Verdict,
    pub limits: Vec<String>,
    /// `replay.txt`.
    pub text: String,
    /// `fidelity.json`.
    pub json: serde_json::Value,
    /// The checks' lines, as `check` prints them.
    pub lines: Vec<String>,
    pub ok: bool,
}

/// Plans `bundle`'s replay and writes its scenario to `to` (by default
/// `eval/scenarios/local/report-<bundle name>.scn`).
pub fn write_scenario(
    b: &Bundle,
    opts: &Options,
    to: Option<&Path>,
) -> Result<(Plan, PathBuf), Error> {
    let mut plan = plan::plan(b, opts)?;
    if plan.backend_both && !opts.gpu {
        plan.backend_both = false;
        plan.limits.push(
            "a surface was on the GPU live: this replay drew on the CPU alone (`--gpu` adds the \
             GPU leg)"
                .into(),
        );
    }
    let id = plan::scenario_id(&b.name);
    let path = match to {
        Some(p) => p.to_path_buf(),
        None => local_dir().join(format!("{id}.scn")),
    };
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| Error::Io(d.display().to_string(), e))?;
        if let Some(icons) = flat_icons(b, &d.join(format!("{id}.icons")))? {
            plan.icons = Some(icons);
        }
    }
    let text = plan.scenario(b, &id);
    std::fs::write(&path, text).map_err(|e| Error::Io(path.display().to_string(), e))?;
    Ok((plan, path))
}

/// (replay-owner) Where a replay's scenario goes: the checkout's
/// `eval/scenarios/local/` when this `strand-eval` was built in one that
/// is still there, else `${XDG_CACHE_HOME:-~/.cache}/strand-eval/
/// scenarios/local/` (a `strand-eval` installed beside `strand`). Both
/// are local places: a scenario there may name the bundle.
pub fn local_dir() -> PathBuf {
    let root = crate::scenario::eval_root();
    if root.join("scenarios").is_dir() {
        return root.join("scenarios").join("local");
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("strand-eval").join("scenarios").join("local")
}

/// (replay-owner) The files the bundle copied (`files/<path>`) that are
/// not, now, what is at their path: a replay reads a file where the live
/// shell read it, which in the container is the bundle's copy (mounted
/// there) and on a desktop the file as it is now.
pub fn files_changed(b: &Bundle) -> Vec<String> {
    let root = b.dir.join("files");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(&root) else {
                continue;
            };
            let at = Path::new("/").join(rel);
            let same = match (std::fs::read(&p), std::fs::read(&at)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if !same {
                out.push(at.display().to_string());
            }
        }
    }
    out.sort();
    out
}

/// Replays the bundle in `dir`: writes its scenario, runs and checks it,
/// and writes the report into `out` with `replay.txt` and
/// `fidelity.json`.
pub fn replay(
    dir: &Path,
    opts: &Options,
    to: Option<&Path>,
    out: &Path,
    frames: Option<Frames>,
    build: crate::trace::BuildInfo,
) -> Result<Replayed, Error> {
    let b = Bundle::read(dir)?;
    let (plan, path) = write_scenario(&b, opts, to)?;
    let sc = Scenario::load(&path)?;
    let run_opts = RunOptions {
        images: true,
        build,
        log: true,
        ..RunOptions::default()
    };
    let checked = crate::check_with(
        &sc,
        &run_opts,
        crate::refs::for_scenario(&sc, &crate::refs::dir(), false),
    )?;
    let fid = fidelity::compare(&b, &plan, &checked.run);
    let verdict = drawn::compare(&b, &plan, &checked.run);
    let mut limits = plan.limits.clone();
    let changed = files_changed(&b);
    if !changed.is_empty() {
        let shown: Vec<&str> = changed.iter().take(8).map(String::as_str).collect();
        limits.push(format!(
            "files read as they are now, which differ from the report's copy ({}): {}{}",
            changed.len(),
            shown.join(", "),
            if changed.len() > shown.len() {
                ", …"
            } else {
                ""
            }
        ));
    }
    for (m, n) in &fid.mock_answered {
        limits.push(format!(
            "calls answered by the mock, no answer recorded: `{m}` ({n})"
        ));
    }
    let mut lines = Vec::new();
    for n in &checked.run.notes {
        lines.push(format!("{} note: {n}", sc.id));
    }
    lines.extend(checked.report.lines(&checked.run.trace));
    let ok = checked.report.ok();
    let text = replay_txt(&b, &plan, &fid, &verdict, &limits, &checked, &path);
    let json = fidelity_json(&b, &plan, &fid, &verdict, &limits);
    let mut entries = vec![Entry {
        id: sc.id.clone(),
        file: sc.file.display().to_string(),
        source: sc.source.clone(),
        trace: checked.run.trace,
        report: Some(checked.report),
        images: checked.run.images,
        notes: checked.run.notes,
        ..Entry::default()
    }];
    let info = RunInfo {
        command: "replay".into(),
        out: out.to_path_buf(),
        frames: frames.unwrap_or(Frames::All),
        replay: Some(serde_json::json!({
            "verdict": verdict.word(),
            "line": verdict.line(),
            "drifted": verdict.lines(),
            "fidelity": first_line(&fid),
        })),
    };
    let mut all = vec![format!("verdict: {}", verdict.line())];
    all.extend(lines.iter().cloned());
    all.push(format!("fidelity: {}", first_line(&fid)));
    report::write_run(&info, &mut entries, &all)?;
    report::write(&out.join("replay.txt"), text.as_bytes())?;
    report::write(&out.join("fidelity.json"), &verdict_first(&json)?)?;
    Ok(Replayed {
        id: sc.id.clone(),
        plan,
        scenario: path,
        fidelity: fid,
        verdict,
        limits,
        text,
        json,
        lines,
        ok,
    })
}

/// The icons the replay draws as one flat folder (the `icons` header):
/// the bundle's copy of the config's `icons/` and the theme's icons it
/// copied under `files/` (any file under an `icons` directory), by file
/// name, the config's first. `None` when the bundle copied no theme
/// icon: the config's copy is used as it is.
fn flat_icons(b: &Bundle, dest: &Path) -> Result<Option<PathBuf>, Error> {
    let mut theme = Vec::new();
    let mut stack = vec![b.dir.join("files")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.components().any(|c| c.as_os_str() == "icons")
                && p.extension().is_some_and(|x| x == "svg" || x == "png")
            {
                theme.push(p);
            }
        }
    }
    if theme.is_empty() {
        return Ok(None);
    }
    let io = |p: &Path, e| Error::Io(p.display().to_string(), e);
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| io(dest, e))?;
    let mut from: Vec<PathBuf> = std::fs::read_dir(b.config_dir().join("icons"))
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    theme.sort();
    from.extend(theme);
    for p in from {
        let Some(name) = p.file_name() else {
            continue;
        };
        let to = dest.join(name);
        if !to.exists() {
            std::fs::copy(&p, &to).map_err(|e| io(&p, e))?;
        }
    }
    Ok(Some(dest.to_path_buf()))
}

/// (replay-owner) `fidelity.json`: the comparison with the live shell
/// (`Fidelity::json`) and the verdict (`verdict`).
fn fidelity_json(
    b: &Bundle,
    plan: &Plan,
    fid: &Fidelity,
    verdict: &Verdict,
    limits: &[String],
) -> serde_json::Value {
    let mut json = fid.json(b, plan, limits);
    if let Some(o) = json.as_object_mut() {
        o.insert("verdict".into(), verdict.json());
    }
    json
}

/// `json` pretty, its `verdict` first (serde_json sorts the rest).
fn verdict_first(json: &serde_json::Value) -> Result<Vec<u8>, Error> {
    let err = |e: serde_json::Error| Error::Run(format!("fidelity.json: {e}"));
    let mut rest = json.clone();
    let verdict = rest
        .as_object_mut()
        .and_then(|o| o.remove("verdict"))
        .unwrap_or(serde_json::Value::Null);
    let v = serde_json::to_string_pretty(&verdict).map_err(err)?;
    let r = serde_json::to_string_pretty(&rest).map_err(err)?;
    let body = r.strip_prefix('{').unwrap_or(&r).trim_start_matches('\n');
    let v = v.replace('\n', "\n  ");
    let sep = if body.trim_start().starts_with('}') {
        "\n"
    } else {
        ",\n"
    };
    Ok(format!("{{\n  \"verdict\": {v}{sep}{body}\n").into_bytes())
}

/// The comparison with the live shell in one line: no divergence, or
/// the first.
pub fn first_line(f: &Fidelity) -> String {
    match f.divergences.first() {
        None => "no divergence from the live shell".into(),
        Some(d) => format!("{} divergences; the first: {}", f.divergences.len(), d.text),
    }
}

/// `t` (ns since the window's start) as seconds before the report.
fn before(plan: &Plan, t: u64) -> String {
    let len = plan.end - plan.start;
    format!("{:+.2}s", (t as f64 - len as f64) / 1e9)
}

/// A trace node as `replay.txt` names it: its origin, key and id.
fn node_name(trace: &RunTrace, key: &str) -> String {
    let m = trace.nodes.get(key);
    let origin = m
        .and_then(|m| m.origin.clone())
        .or_else(|| key.split('|').nth(1).map(str::to_string))
        .unwrap_or_else(|| key.to_string());
    let kind = m.map_or("", |m| m.kind.as_str());
    let id = m
        .and_then(|m| m.id.as_deref())
        .map_or(String::new(), |i| format!(" #{i}"));
    format!("@{origin} {kind}{id}")
}

/// One line per run of motion: a node whose drawn box or opacity
/// changes over consecutive frames.
pub fn motion_runs(trace: &RunTrace, at: impl Fn(u64) -> String) -> Vec<(u64, String)> {
    // key → (first k, first t, values at the run's start, last values, last k, motion bits)
    struct Run {
        k0: u32,
        t0: u64,
        from: [f32; 5],
        to: [f32; 5],
        k1: u32,
        mot: u32,
        moving: bool,
    }
    let mut runs: BTreeMap<String, Run> = BTreeMap::new();
    let mut out = Vec::new();
    let flush = |key: &str, r: &Run, out: &mut Vec<(u64, String)>| {
        let names = ["x", "y", "w", "h", "opacity"];
        for (i, n) in names.iter().enumerate() {
            if (r.from[i] - r.to[i]).abs() > 0.05 {
                let mot = crate::report::explain::motion_names(r.mot)
                    .join("+")
                    .to_lowercase();
                out.push((
                    r.t0,
                    format!(
                        "{} k{}..k{} {} {n} {:.1} -> {:.1}{}",
                        at(r.t0),
                        r.k0,
                        r.k1,
                        node_name(trace, key),
                        r.from[i],
                        r.to[i],
                        if mot.is_empty() {
                            String::new()
                        } else {
                            format!(" {mot}")
                        }
                    ),
                ));
            }
        }
    };
    for fr in &trace.frames {
        let t = fr.t_ns.saturating_sub(T0.as_nanos() as u64);
        for row in &fr.n {
            let v = [
                row.drawn[0],
                row.drawn[1],
                row.drawn[2],
                row.drawn[3],
                row.op,
            ];
            match runs.get_mut(&row.key) {
                None => {
                    runs.insert(
                        row.key.clone(),
                        Run {
                            k0: fr.k,
                            t0: t,
                            from: v,
                            to: v,
                            k1: fr.k,
                            mot: row.emot,
                            moving: false,
                        },
                    );
                }
                Some(r) => {
                    let changed = r.to.iter().zip(&v).any(|(a, b)| (a - b).abs() > 0.05);
                    if changed {
                        if !r.moving {
                            r.k0 = fr.k;
                            r.t0 = t;
                            r.from = r.to;
                            r.mot = 0;
                            r.moving = true;
                        }
                        r.to = v;
                        r.k1 = fr.k;
                        r.mot |= row.emot;
                    } else if r.moving {
                        flush(&row.key, r, &mut out);
                        r.moving = false;
                        r.from = v;
                        r.k1 = fr.k;
                    } else {
                        r.k1 = fr.k;
                    }
                }
            }
        }
    }
    for (key, r) in &runs {
        if r.moving {
            flush(key, r, &mut out);
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// `replay.txt`.
fn replay_txt(
    b: &Bundle,
    plan: &Plan,
    fid: &Fidelity,
    verdict: &Verdict,
    limits: &[String],
    checked: &crate::Checked,
    scenario: &Path,
) -> String {
    let (run, rep) = (&checked.run, &checked.report);
    let mut s = String::new();
    let len = plan.end - plan.start;
    // (replay-owner) The verdict first: whether the replay drew what the
    // live shell drew at the report.
    let _ = writeln!(s, "verdict: {}", verdict.line());
    for l in verdict.lines() {
        let _ = writeln!(s, "  {l}");
    }
    let _ = writeln!(s, "glitch report: {:?}", b.words);
    let _ = writeln!(s, "bundle: {}", b.dir.display());
    let _ = writeln!(s, "scenario: {}", scenario.display());
    let _ = writeln!(
        s,
        "window: {:.1} s up to the report, from {}; traced: the last {:.1} s",
        len as f64 / 1e9,
        plan.start_why,
        plan.traced_ns as f64 / 1e9
    );
    let o = &plan.output;
    let _ = writeln!(
        s,
        "output: {} {}x{} scale {} at {} Hz ({})",
        o.name, o.width, o.height, o.scale, o.hz, o.why
    );
    let _ = writeln!(
        s,
        "presses: {} live, {} replayed; cell writes: {} live, {} replayed",
        fid.presses.0, fid.presses.1, fid.writes.0, fid.writes.1
    );
    let _ = writeln!(s, "fidelity: {}", first_line(fid));
    for d in &fid.divergences {
        let _ = writeln!(s, "  {} {}: {}", before(plan, d.t), d.what, d.text);
    }
    if !fid.frames.is_empty() {
        let _ = writeln!(s, "frames at the report:");
        for f in &fid.frames {
            let what = match &f.why {
                Some(w) => w.clone(),
                None => format!(
                    "{:.2}% of {} pixels past {}{}",
                    f.share() * 100.0,
                    f.total,
                    crate::checks::cpu_gpu::GPU_TOLERANCE,
                    f.bbox.map_or_else(String::new, |b| format!(
                        ", in {}x{} at {},{}",
                        b[2], b[3], b[0], b[1]
                    ))
                ),
            };
            let _ = writeln!(s, "  {} @{}: {what}", f.png, f.origin);
        }
    }
    let _ = writeln!(s, "not reproduced:");
    for l in limits {
        let _ = writeln!(s, "  - {l}");
    }
    if !(plan.notes.is_empty() && fid.notes.is_empty()) {
        let _ = writeln!(s, "notes:");
        for n in plan.notes.iter().chain(&fid.notes) {
            let _ = writeln!(s, "  - {n}");
        }
    }

    // Events, time relative to the report.
    let mut events: Vec<(u64, String)> = plan
        .events
        .iter()
        .map(|e| (e.t, format!("{} {}", before(plan, e.t), e.text)))
        .collect();
    let at = |t: std::time::Duration| t.saturating_sub(T0).as_nanos() as u64;
    for p in &run.log.presses {
        let t = at(p.t);
        events.push((
            t,
            format!(
                "{} replay press hit {}",
                before(plan, t),
                p.hit
                    .as_ref()
                    .map_or_else(|| "nothing".into(), ToString::to_string)
            ),
        ));
    }
    for w in &run.log.writes {
        let t = at(w.t);
        events.push((
            t,
            format!("{} cell {} = {} (replay)", before(plan, t), w.path, w.value),
        ));
    }
    let traced_at = len.saturating_sub(plan.traced_ns);
    for sf in &run.trace.surfaces {
        let t = u64::from(sf.attached) * plan.period;
        if t > traced_at {
            events.push((
                t,
                format!(
                    "{} replay surface mapped {}",
                    before(plan, t),
                    node_name(&run.trace, &sf.root)
                ),
            ));
        }
        if let Some(d) = sf.detached {
            let t = u64::from(d) * plan.period;
            events.push((
                t,
                format!(
                    "{} replay surface gone {}",
                    before(plan, t),
                    node_name(&run.trace, &sf.root)
                ),
            ));
        }
    }
    events.sort_by_key(|(t, _)| *t);
    let _ = writeln!(s, "\nevents (seconds before the report):");
    for (_, l) in &events {
        let _ = writeln!(s, "  {l}");
    }

    let motion = motion_runs(&run.trace, |t| before(plan, t));
    let _ = writeln!(s, "\nmotion (one line per run, drawn box and opacity):");
    for (_, l) in motion.iter().take(MOTION_LINES) {
        let _ = writeln!(s, "  {l}");
    }
    if motion.len() > MOTION_LINES {
        let _ = writeln!(s, "  … {} more in trace.json", motion.len() - MOTION_LINES);
    }

    let _ = writeln!(s, "\nfindings:");
    let mut any = false;
    for r in &rep.results {
        for f in &r.findings {
            if f.sev == Sev::Info {
                continue;
            }
            any = true;
            let t = f.k.map_or_else(
                || "      ".into(),
                |k| before(plan, u64::from(k) * plan.period),
            );
            let _ = writeln!(
                s,
                "  {t} {} {}{}: {}",
                r.check,
                if f.sev == Sev::Fail { "FAIL" } else { "warn" },
                f.key
                    .as_deref()
                    .map_or_else(String::new, |k| format!(" {}", node_name(&run.trace, k))),
                f.msg
            );
        }
    }
    if !any {
        let _ = writeln!(s, "  none");
    }
    s
}
