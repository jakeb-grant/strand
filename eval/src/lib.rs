//! The animation evaluation sandbox (docs/eval.md; docs/decisions.md
//! eval-sandbox): scenario scripts run on a virtual clock against
//! strand-render (with its `eval-hooks` feature) and the logic VM in
//! one process, every painted frame traced node by node and checked.
//!
//! - [`scenario`], [`steps`], [`select`]: the `.scn` format, parsed with
//!   typed errors at `file:line`.
//! - [`harness`] (with [`clock`], [`surfaces`], [`input`], [`text`] and
//!   [`labels`]): the driver, which runs a scenario into a [`trace`].
//! - [`checks`]: the invariants over a trace, and [`check_scenario`],
//!   which runs a scenario with its twin and checks it.
//! - [`report`]: what a run writes (summaries, contact sheets, the
//!   viewer page); [`recording`] and [`refs`]: a run's compact recording
//!   and the approved (and known-bad) references it is held to; [`ab`]:
//!   two runs compared; [`matrix`]: every effect and pair of effects run on one
//!   fixture; [`promote`]: a run's scenario made a regression.

use std::fmt;

pub mod ab;
pub mod bench;
pub mod checks;
pub mod cli;
pub mod clock;
pub mod gpu;
pub mod harness;
pub mod input;
pub mod labels;
pub mod matrix;
pub mod paint;
pub mod promote;
pub mod recording;
pub mod refs;
pub mod report;
pub mod scaffold;
pub mod scenario;
pub mod select;
pub mod steps;
pub mod surfaces;
pub mod text;
pub mod trace;
pub mod wire;

/// What the sandbox can fail with; every command returns one of these
/// rather than panicking.
#[derive(Debug)]
pub enum Error {
    /// The command line asked for something this build cannot do yet.
    Unsupported(String),
    /// The command line itself is wrong.
    Usage(String),
    /// A scenario is wrong: its file, the line and why.
    Scenario {
        file: String,
        line: u32,
        msg: String,
    },
    /// The config did not load or compile.
    Config(String),
    /// The run went wrong (a scene error, a step the shell refused).
    Run(String),
    /// A file could not be read or written.
    Io(String, std::io::Error),
    /// The GPU leg found no device (a skip unless `STRAND_REQUIRE_GPU=1`).
    NoDevice(String),
    /// The run went as asked and a scenario failed its checks (exit 1;
    /// every other error exits 2).
    Failed(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "{what}: not implemented in this build"),
            Error::Usage(msg) => write!(f, "{msg}"),
            Error::Scenario { file, line, msg } => write!(f, "{file}:{line}: {msg}"),
            Error::Config(msg) => write!(f, "config: {msg}"),
            Error::Run(msg) => write!(f, "run: {msg}"),
            Error::Io(what, e) => write!(f, "{what}: {e}"),
            Error::NoDevice(why) => write!(f, "no GPU device: {why}"),
            Error::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// A scenario run and checked.
#[derive(Debug)]
pub struct Checked {
    pub run: harness::RunOutput,
    /// The reduced-motion re-run, when `twin` ran.
    pub twin: Option<trace::RunTrace>,
    /// The GPU leg's run, for a `backend = "both"` scenario.
    pub gpu: Option<harness::RunOutput>,
    pub report: checks::Report,
    /// The references it was held to (`None`: not compared, as for a
    /// `--reduced-motion` run).
    pub refs: Option<refs::Refs>,
}

/// Whether `sc` is a curated scenario (determinism runs it twice).
pub fn curated(sc: &scenario::Scenario) -> bool {
    sc.file
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("curated"))
}

/// Runs `sc`, its reduced-motion twin, for a curated scenario or one
/// asking for `determinism` a second run, and for a `backend = "both"`
/// one its GPU leg, and checks them against the scenario's reference
/// recordings in `eval/refs/` (a missing approved one is a skip: see
/// [`check_with`]).
pub fn check_scenario(
    sc: &scenario::Scenario,
    opts: &harness::RunOptions,
) -> Result<Checked, Error> {
    check_with(sc, opts, refs::for_scenario(sc, &refs::dir(), false))
}

/// [`check_scenario`] against `refs` (`refs.required`: a missing approved
/// reference is CHANGED, as `check --gate` and the matrix hold it). A
/// `--reduced-motion` or one-leg run is not compared.
pub fn check_with(
    sc: &scenario::Scenario,
    opts: &harness::RunOptions,
    refs: refs::Refs,
) -> Result<Checked, Error> {
    use checks::{CheckId, Finding, Status};
    let refs = (!opts.reduced_motion && opts.backend.is_none()).then_some(refs);
    let both = opts.backend.is_none() && sc.backend == scenario::BackendMode::Both;
    let mut opts = opts.clone();
    if both {
        // The pixel comparison needs both legs' images.
        opts.images = true;
    }
    // Each leg's frames are held to their full repaints; the twin and
    // the determinism run only to what they compare.
    opts.redraw = sc.runs(CheckId::Redraw);
    let run = harness::run(sc, &opts)?;
    let mut twin_error = None;
    let twin = if sc.runs(CheckId::Twin) && !sc.reduce_motion && !opts.reduced_motion {
        let o = harness::RunOptions {
            reduced_motion: true,
            images: false,
            redraw: false,
            surface: Some(false),
            ..opts.clone()
        };
        match harness::run(sc, &o) {
            Ok(t) => Some(t.trace),
            Err(e) => {
                twin_error = Some(e.to_string());
                None
            }
        }
    } else {
        None
    };
    let again = if sc.runs(CheckId::Determinism) || curated(sc) {
        let o = harness::RunOptions {
            images: false,
            redraw: false,
            surface: Some(false),
            ..opts.clone()
        };
        Some(harness::run(sc, &o)?.trace)
    } else {
        None
    };
    let gpu = if both {
        let o = harness::RunOptions {
            backend: Some(scenario::BackendMode::Gpu),
            ..opts.clone()
        };
        Some(harness::run(sc, &o))
    } else {
        None
    };
    let mut report = checks::evaluate(sc, &run.trace, twin.as_ref(), again.as_ref(), refs.as_ref());
    if refs.is_none() && opts.reduced_motion {
        report.set(
            sc,
            CheckId::Reference,
            Vec::new(),
            Some(Status::Skip(checks::reference::REDUCED.into())),
        );
    }
    if let Some(e) = twin_error
        && let Some(r) = report
            .results
            .iter_mut()
            .find(|r| r.check == CheckId::Twin.name())
    {
        r.findings.push(Finding::fail(
            CheckId::Twin,
            format!("the reduced-motion run failed: {e}"),
        ));
        r.status = match sc.expect_fail.get(&CheckId::Twin) {
            Some(why) => Status::Xfail(why.clone()),
            None => Status::Fail,
        };
    }
    let gpu = match gpu {
        None => None,
        Some(Ok(g)) => {
            let cx = checks::Cx::new(sc, &run.trace);
            let findings = checks::cpu_gpu::run(&cx, &g.trace, &run.images, &g.images);
            report.set(sc, CheckId::CpuGpu, findings, None);
            // The GPU leg's commits beside the CPU leg's.
            if g.trace.wire.is_some() {
                let gpu_leg = |v: Vec<Finding>| {
                    v.into_iter().map(|mut f| {
                        f.msg = format!("GPU leg: {}", f.msg);
                        f
                    })
                };
                let mut findings = checks::wire::regions_on_wire(&run.trace);
                findings.extend(gpu_leg(checks::wire::regions_on_wire(&g.trace)));
                report.set(sc, CheckId::RegionsOnWire, findings, None);
                let mut findings = checks::wire::late_present(&run.trace);
                findings.extend(gpu_leg(checks::wire::late_present(&g.trace)));
                report.set(sc, CheckId::LatePresent, findings, None);
            }
            // The GPU leg's frames against their fresh repaints, beside
            // the CPU leg's.
            if g.trace.redraw > 0 {
                let mut findings = checks::redraw::findings(&run.trace);
                findings.extend(checks::redraw::findings(&g.trace));
                report.set(sc, CheckId::Redraw, findings, None);
            }
            Some(g)
        }
        Some(Err(Error::NoDevice(why))) if !gpu::required() => {
            report.set(
                sc,
                CheckId::CpuGpu,
                Vec::new(),
                Some(Status::Skip(format!(
                    "no GPU device ({why}); {}=1 makes this a failure",
                    gpu::REQUIRE_ENV
                ))),
            );
            None
        }
        Some(Err(e)) => {
            report.set(
                sc,
                CheckId::CpuGpu,
                vec![Finding::fail(
                    CheckId::CpuGpu,
                    format!("the GPU leg's run failed: {e}"),
                )],
                None,
            );
            None
        }
    };
    Ok(Checked {
        run,
        twin,
        gpu,
        report,
        refs,
    })
}

/// The subcommands `strand-eval` knows, in the order docs/eval.md
/// describes them.
pub const COMMANDS: [&str; 12] = [
    "run",
    "check",
    "matrix",
    "approve",
    "promote",
    "verdict",
    "refs-check",
    "explain",
    "ab",
    "bench",
    "new",
    "selftest",
];

/// Runs `strand-eval` with `args` (without the program name).
pub fn main_with(args: &[String]) -> Result<(), Error> {
    let Some(cmd) = args.first() else {
        return Err(Error::Usage(usage()));
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        "run" => cli::run(&args[1..]),
        "check" => cli::check(&args[1..]),
        "explain" => cli::explain(&args[1..]),
        "verdict" => cli::verdict(&args[1..]),
        "approve" => cli::approve(&args[1..]),
        "refs-check" => cli::refs_check(&args[1..]),
        "ab" => cli::ab(&args[1..]),
        "matrix" => cli::matrix(&args[1..]),
        "promote" => cli::promote(&args[1..]),
        "bench" => cli::bench(&args[1..]),
        "new" => cli::new(&args[1..]),
        // `selftest` is scripts/eval/ci.sh, run by scripts/eval/eval.sh.
        c if COMMANDS.contains(&c) => Err(Error::Unsupported(format!("strand-eval {c}"))),
        other => Err(Error::Usage(format!(
            "unknown command `{other}`\n{}",
            usage()
        ))),
    }
}

/// The usage text.
pub fn usage() -> String {
    format!(
        "usage: strand-eval <command> [args...]\ncommands: {}",
        COMMANDS.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_command_is_a_usage_error() {
        let e = main_with(&["frobnicate".into()]).unwrap_err();
        assert!(matches!(e, Error::Usage(m) if m.contains("frobnicate")));
    }

    #[test]
    fn no_command_is_a_usage_error() {
        assert!(matches!(main_with(&[]), Err(Error::Usage(_))));
    }

    #[test]
    fn selftest_is_the_scripts_and_a_typed_error_here() {
        let e = main_with(&["selftest".into()]).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
    }

    #[test]
    fn bench_and_new_ask_for_their_arguments() {
        for c in ["bench", "new"] {
            let e = main_with(&[c.into()]).unwrap_err();
            assert!(matches!(e, Error::Usage(ref m) if m.contains(c)), "{e}");
        }
    }
}
