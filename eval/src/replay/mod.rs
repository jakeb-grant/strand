//! (report) `strand-eval replay <bundle>`: a glitch report's bundle
//! turned into a local scenario, run with every CPU check, and held to
//! what the live shell recorded (docs/eval.md, "Replaying a glitch
//! report"). It fails only when the replay cannot run; findings and
//! divergences are its output, not a failure.

pub mod bundle;
pub mod clip;
pub mod drawn;
pub mod fidelity;
pub mod plan;

use std::collections::{BTreeMap, BTreeSet};
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
    /// Whether the checks found nothing. (replay-owner) Their findings
    /// are hints: a replay that ran is never a failure.
    pub ok: bool,
}

/// (replay-owner) What the checks' findings are called in a replay's
/// outputs: they are tuned on Strand's own scenarios, and a real shell
/// may meet one wrongly (decisions.md owner-2026-10-21).
pub const HINTS: &str = "hints (the checks may be wrong on a real shell)";

/// (replay-owner) A check's line as a replay prints it: a check's
/// status that would read as a verdict (`FAIL`, `CHANGED`, `XPASS`) is
/// `hint`, since a replay's findings are hints.
pub fn hinted(line: &str) -> String {
    for word in ["FAIL", "CHANGED"] {
        if let Some(head) = line.strip_suffix(&format!(": {word}")) {
            return format!("{head}: hint");
        }
    }
    if let Some((head, _)) = line.split_once(": XPASS") {
        return format!("{head}: hint");
    }
    if let Some(rest) = line.strip_prefix("  CHANGED: ") {
        return format!("  hint: {rest}");
    }
    line.to_string()
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
    std::fs::write(&path, &text).map_err(|e| Error::Io(path.display().to_string(), e))?;
    readable(&text, &path)?;
    Ok((plan, path))
}

/// (replayfix) A replay never runs a scenario it cannot read back: the
/// file is kept to look at, and the error says the replay wrote it
/// wrong, naming its line.
pub fn readable(text: &str, path: &Path) -> Result<(), Error> {
    match Scenario::parse(text, path) {
        Ok(_) => Ok(()),
        Err(Error::Scenario { file, line, msg }) => Err(Error::Scenario {
            file,
            line,
            msg: format!(
                "the replay wrote a line it cannot read back, a bug in `strand-eval replay` \
                 (not in the shell or the report): {msg}"
            ),
        }),
        Err(e) => Err(Error::Run(format!(
            "the replay wrote a scenario it cannot read back ({}), a bug in `strand-eval \
             replay` (not in the shell or the report): {e}",
            path.display()
        ))),
    }
}

/// (replay-owner) Where a replay's scenario goes: `scenarios/local/`
/// under [`crate::scenario::eval_root`], (replayfix) the `eval/` of the
/// strand checkout holding the current directory, else
/// `${XDG_CACHE_HOME:-~/.cache}/strand-eval/` (a `strand-eval`
/// installed beside `strand`, run anywhere), never the tree it was built
/// in. Both are local places: a scenario there may name the bundle.
pub fn local_dir() -> PathBuf {
    crate::scenario::eval_root().join("scenarios").join("local")
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
    let mut verdict = drawn::compare(&b, &plan, &checked.run);
    let mut limits = plan.limits.clone();
    let changed = files_changed(&b);
    verdict.changed_files = changed.clone();
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
    // (fidelity) A node the live shell showed on several outputs at
    // once: the sandbox shows a node on one, so the others are named.
    for (origin, live) in &plan.shared {
        let drawn: BTreeSet<&str> = checked
            .run
            .trace
            .surfaces
            .iter()
            .filter(|s| {
                fidelity::root_origin(&checked.run.trace, &s.root)
                    .is_some_and(|o| fidelity::same_origin(&o, origin))
            })
            .filter_map(|s| s.output.as_deref())
            .collect();
        let not: Vec<&str> = live
            .iter()
            .map(String::as_str)
            .filter(|o| !drawn.contains(o))
            .collect();
        if !not.is_empty() {
            limits.push(format!(
                "@{origin} was shown live on {} at once, one surface of one node on each (its \
                 `screens:` is `all` or a list); the sandbox shows a node on one output, so the \
                 replay drew it {} and not on {}",
                live.join(", "),
                if drawn.is_empty() {
                    "on none".to_string()
                } else {
                    format!(
                        "on {} only",
                        drawn.iter().copied().collect::<Vec<_>>().join(", ")
                    )
                },
                not.join(", ")
            ));
        }
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
    lines.extend(
        checked
            .report
            .lines(&checked.run.trace)
            .into_iter()
            .map(|l| hinted(&l)),
    );
    let ok = checked.report.ok();
    let text = replay_txt(&b, &plan, &fid, &verdict, &limits, &checked, &path);
    let json = fidelity_json(&b, &plan, &fid, &verdict, &limits, &checked);
    let line = verdict_line(&verdict, &fid, &plan);
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
            "line": line,
            "drifted": verdict.lines(),
            "fidelity": first_line(&fid),
            "hints": HINTS,
            "window": format!(
                "{:.1} s up to the report, from {}; traced: {}",
                (plan.end - plan.start) as f64 / 1e9,
                plan.start_why,
                plan.traced_words()
            ),
            "timeline": timeline_json(&plan, &fid),
            // (replay-clip) `fidelity.json` itself: a clip states what
            // it says of the clip's frames (eval/viewer/clip.js).
            "record": json.clone(),
        })),
    };
    let mut all = vec![
        format!("verdict: {line}"),
        format!("fidelity: {}", first_line(&fid)),
        format!("{HINTS}:"),
    ];
    all.extend(lines.iter().cloned());
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
/// (`Fidelity::json`), the verdict (`verdict`) and the checks' findings
/// as hints (`hints`), never a failure.
fn fidelity_json(
    b: &Bundle,
    plan: &Plan,
    fid: &Fidelity,
    verdict: &Verdict,
    limits: &[String],
    checked: &crate::Checked,
) -> serde_json::Value {
    let mut json = fid.json(b, plan, limits);
    let trace = &checked.run.trace;
    let findings: Vec<serde_json::Value> = checked
        .report
        .results
        .iter()
        .flat_map(|r| r.findings.iter().map(move |f| (r, f)))
        .filter(|(_, f)| f.sev != Sev::Info)
        .map(|(r, f)| {
            serde_json::json!({
                "check": r.check,
                "sev": if f.sev == Sev::Fail { "fail" } else { "warn" },
                "node": f.key.as_deref().map(|k| node_name(trace, k)),
                "t": f.k.map(|k| (u64::from(k) * plan.period) as f64 / 1e9
                    - (plan.end - plan.start) as f64 / 1e9),
                "msg": f.msg,
            })
        })
        .collect();
    if let Some(o) = json.as_object_mut() {
        let mut v = verdict.json();
        if let Some(vo) = v.as_object_mut() {
            vo.insert("line".into(), verdict_line(verdict, fid, plan).into());
            vo.insert("parted".into(), parted_json(fid, plan));
        }
        o.insert("verdict".into(), v);
        o.insert(
            "hints".into(),
            serde_json::json!({"label": HINTS, "findings": findings}),
        );
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

/// (fidelity) The divergences before the report's frame: every one but
/// the frames compared at the report (which the verdict holds itself).
pub fn parted(fid: &Fidelity) -> Vec<&fidelity::Divergence> {
    fid.divergences
        .iter()
        .filter(|d| d.what != "frame")
        .collect()
}

/// (fidelity) The verdict in one line, saying when the replay parted
/// from the live shell earlier in the window: VERIFIED and DRIFTED are
/// of the frame at the report, and a replay that parted from the live
/// shell before it may have drawn that frame from another state, so the
/// line never says VERIFIED alone then (how many times, the first's
/// time before the report, and what).
pub fn verdict_line(verdict: &Verdict, fid: &Fidelity, plan: &Plan) -> String {
    let line = verdict.line();
    let p = parted(fid);
    let Some(first) = p.first() else {
        return line;
    };
    format!(
        "{line}; but the replay parted from the live shell {} earlier in the window, first at \
         {} ({}: {})",
        match p.len() {
            1 => "once".to_string(),
            n => format!("{n} times"),
        },
        before(plan, first.t),
        first.what,
        first.text
    )
}

/// (fidelity) [`parted`] as `fidelity.json`'s `verdict.parted`: how
/// many, and the first (seconds before the report, what, text).
pub fn parted_json(fid: &Fidelity, plan: &Plan) -> serde_json::Value {
    let len = plan.end - plan.start;
    let p = parted(fid);
    serde_json::json!({
        "count": p.len(),
        "first": p.first().map(|d| serde_json::json!({
            "t": (d.t as f64 - len as f64) / 1e9,
            "what": d.what,
            "text": d.text,
        })),
    })
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

/// (replayfix) The head of `replay.txt`'s timeline.
pub const TIMELINE: &str = "timeline (seconds before the report): input, changes, surfaces, the replay's repaints and idle spans:";

/// (replayfix) A span this long or longer with no repaint is idle: the
/// viewer and `replay.txt` say "nothing changed" over it.
pub const IDLE_NS: u64 = 1_000_000_000;

/// (replayfix) The live pointer's moves as `replay.txt` lists them: one
/// line per run of moves on one surface (none longer apart than
/// [`IDLE_NS`]), and one where it leaves the replayed output's
/// surfaces.
pub fn pointer_runs(plan: &Plan) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    let track = &plan.pointer;
    while i < track.len() {
        let p = &track[i];
        let Some(on) = &p.origin else {
            out.push((
                p.t,
                format!(
                    "{} input pointer off the replayed surfaces",
                    before(plan, p.t)
                ),
            ));
            i += 1;
            continue;
        };
        let mut j = i;
        while j + 1 < track.len()
            && track[j + 1].origin.as_ref() == Some(on)
            && track[j + 1].t.saturating_sub(track[j].t) < IDLE_NS
        {
            j += 1;
        }
        let q = &track[j];
        let n = j - i + 1;
        out.push((
            p.t,
            if n == 1 {
                format!(
                    "{} input pointer at {:.1},{:.1} on @{on}",
                    before(plan, p.t),
                    p.x,
                    p.y
                )
            } else {
                format!(
                    "{} input pointer on @{on}: {n} moves over {:.2} s, {:.1},{:.1} -> {:.1},{:.1}",
                    before(plan, p.t),
                    q.t.saturating_sub(p.t) as f64 / 1e9,
                    p.x,
                    p.y,
                    q.x,
                    q.y
                )
            },
        ));
        i = j + 1;
    }
    out
}

/// (replayfix) The replay's painted frames as `replay.txt` lists them:
/// one line per run of repaints (none further apart than [`IDLE_NS`]),
/// and one per span of [`IDLE_NS`] or more with none, the last frame
/// held: `nothing changed: no repaint for 12.40 s`. Up to the report.
pub fn repaint_runs(trace: &RunTrace, plan: &Plan) -> Vec<(u64, String)> {
    let len = plan.end - plan.start;
    let ts: Vec<(u64, u32)> = trace
        .frames
        .iter()
        .map(|f| (f.t_ns.saturating_sub(T0.as_nanos() as u64), f.k))
        .filter(|(t, _)| *t <= len)
        .collect();
    let mut out = Vec::new();
    let from = len.saturating_sub(plan.traced_ns);
    let idle = |out: &mut Vec<(u64, String)>, a: u64, b: u64| {
        if b.saturating_sub(a) >= IDLE_NS {
            out.push((
                a,
                format!(
                    "{} nothing changed: no repaint for {:.2} s (to {})",
                    before(plan, a),
                    (b - a) as f64 / 1e9,
                    before(plan, b)
                ),
            ));
        }
    };
    match ts.first() {
        None => {
            idle(&mut out, from, len);
            return out;
        }
        Some((t0, _)) => idle(&mut out, from, *t0),
    }
    let mut i = 0;
    while i < ts.len() {
        let mut j = i;
        while j + 1 < ts.len() && ts[j + 1].0 - ts[j].0 < IDLE_NS {
            j += 1;
        }
        let n = j - i + 1;
        out.push((
            ts[i].0,
            if n == 1 {
                format!("{} replay repainted k{}", before(plan, ts[i].0), ts[i].1)
            } else {
                format!(
                    "{} replay repainted k{}..k{}: {n} frames over {:.2} s",
                    before(plan, ts[i].0),
                    ts[i].1,
                    ts[j].1,
                    (ts[j].0 - ts[i].0) as f64 / 1e9
                )
            },
        ));
        let next = ts.get(j + 1).map_or(len, |x| x.0);
        idle(&mut out, ts[j].0, next);
        i = j + 1;
    }
    out
}

/// (replayfix) The viewer's timeline of a replay (`replay.timeline` in
/// the page's data): the window in seconds before the report, the tick
/// period, where the trace starts, the live events by kind (`input`,
/// `change`, `surface`, `late`), the live pointer's place over time and
/// the divergences.
pub fn timeline_json(plan: &Plan, fid: &Fidelity) -> serde_json::Value {
    let len = plan.end - plan.start;
    let rel = |t: u64| (t as f64 - len as f64) / 1e9;
    serde_json::json!({
        "seconds": len as f64 / 1e9,
        "period_ns": plan.period,
        "traced_from": rel(len.saturating_sub(plan.traced_ns)),
        "idle_s": IDLE_NS as f64 / 1e9,
        "events": plan.events.iter().map(|e| serde_json::json!({
            "t": rel(e.t), "kind": e.kind, "text": e.text,
        })).collect::<Vec<_>>(),
        "pointer": plan.pointer.iter().map(|p| serde_json::json!({
            "t": rel(p.t), "on": p.origin, "output": p.output, "x": p.x, "y": p.y,
        })).collect::<Vec<_>>(),
        "divergences": fid.divergences.iter().map(|d| serde_json::json!({
            "t": rel(d.t), "what": d.what, "text": d.text,
        })).collect::<Vec<_>>(),
    })
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
    let _ = writeln!(s, "verdict: {}", verdict_line(verdict, fid, plan));
    for l in verdict.lines() {
        let _ = writeln!(s, "  {l}");
    }
    let _ = writeln!(s, "glitch report: {:?}", b.words);
    let _ = writeln!(s, "bundle: {}", b.dir.display());
    let _ = writeln!(s, "scenario: {}", scenario.display());
    let _ = writeln!(
        s,
        "window: {:.1} s up to the report, from {}; traced: {}",
        len as f64 / 1e9,
        plan.start_why,
        plan.traced_words()
    );
    let o = &plan.output;
    let _ = writeln!(
        s,
        "output: {} {}x{} scale {} at {} Hz ({})",
        o.name, o.width, o.height, o.scale, o.hz, o.why
    );
    // (fidelity) Every output, each a screen of the sandbox.
    if plan.outputs.len() > 1 {
        let _ = writeln!(s, "screens (screens.all's order, each replayed):");
        for o in &plan.outputs {
            let _ = writeln!(
                s,
                "  {} {}x{} scale {}{}{}",
                o.name,
                o.width,
                o.height,
                o.scale,
                o.id.as_deref()
                    .map_or_else(String::new, |i| format!(" ({i})")),
                if o.name == plan.output.name {
                    ", the main one"
                } else {
                    ""
                }
            );
        }
    }
    let _ = writeln!(
        s,
        "presses: {} live, {} replayed; cell writes: {} live, {} replayed; service writes: {} \
         live, {} replayed",
        fid.presses.0,
        fid.presses.1,
        fid.writes.0,
        fid.writes.1,
        fid.service_writes.0,
        fid.service_writes.1
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
            let _ = writeln!(
                s,
                "  {} @{}{}: {what}",
                f.png,
                f.origin,
                plan::on_words(&f.output, plan.outputs.len())
            );
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
    // (replayfix) The live pointer's moves, a line per run of them, and
    // the replay's repaints, a line per run, with each span of no
    // repaint said as such: the replay paints only on change.
    events.extend(pointer_runs(plan));
    events.extend(repaint_runs(&run.trace, plan));
    events.push((len, format!("{} the report", before(plan, len))));
    events.sort_by_key(|(t, _)| *t);
    let _ = writeln!(s, "\n{TIMELINE}");
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

    let _ = writeln!(s, "\n{HINTS}:");
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
                if f.sev == Sev::Fail { "hint" } else { "warn" },
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
