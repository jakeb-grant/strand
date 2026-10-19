//! A live session (docs/eval.md, "Live sandbox"; docs/decisions.md
//! eval-live): a scenario written as it runs. What the owner does (a
//! knob, the pointer on the stage, a step typed in) is a step; the clock
//! running between two of them is the `rest` or `frames N` that runs the
//! same ticks. The session's steps run through [`Driver`], the driver
//! every scenario runs on, each at the line it will be written on, so
//! the scenario [`Session::record`] writes runs to the same trace: its
//! digest is the session's.
//!
//! The config is read again when a file of it changes: the steps so far
//! run on the new config at once up to the last thing the owner did,
//! which then plays again at the session's speed. Nothing here draws or
//! talks to a compositor (`live::window` does); a session can be driven
//! by commands alone, as its tests drive it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::lower::{Event, Node, Program};
use strand_compiler::schema::Schema;
use strand_scene::{Size, SurfaceId};

use crate::Error;
use crate::checks::{self, Report, Sev, Status};
use crate::explore::{self, Spot};
use crate::harness::{Driver, RunOptions, Span, TimeRun};
use crate::scenario::{BackendMode, Config, Fonts, Place, Scenario, Screen};
use crate::steps::{self, StepKind};
use crate::trace::RunTrace;

/// What `strand-eval live` opens and how.
#[derive(Clone, Debug)]
pub struct Options {
    /// A `.strand` file, a config directory or a `.scn`.
    pub path: PathBuf,
    /// The screen for a config (a scenario's header names its own).
    pub screen: Screen,
    pub hz: u32,
    pub fonts: Fonts,
    /// Where `record` writes (default: a new file in
    /// `eval/scenarios/local/`).
    pub record_to: Option<PathBuf>,
    /// Frames kept to step back through.
    pub ring: usize,
    /// Painted frames a session holds before it stops the clock.
    pub max_frames: usize,
    /// The speed it starts at, and whether it starts paused.
    pub speed: f64,
    pub paused: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            path: PathBuf::new(),
            screen: Screen {
                width: 1920,
                height: 1080,
                scale: 1.0,
            },
            hz: 60,
            fonts: Fonts::Bundled,
            record_to: None,
            ring: 240,
            max_frames: 20_000,
            speed: 1.0,
            paused: false,
        }
    }
}

/// The speeds `slower` and `faster` step through.
pub const SPEEDS: [f64; 6] = [0.1, 0.2, 0.25, 0.5, 0.75, 1.0];
/// The scales and rates the environment knobs step through.
pub const SCALES: [f64; 4] = [1.0, 1.25, 1.5, 2.0];
pub const RATES: [u32; 3] = [60, 144, 30];

/// The config as loaded: the scenario text up to its steps, the steps a
/// `.scn` brings, and the files watched for edits.
#[derive(Clone, Debug)]
struct Loaded {
    /// The header, the config's `===` sections and `=== steps`.
    head: String,
    /// The line the first step is written on.
    first: u32,
    /// A `.scn`'s own steps, to play.
    steps: Vec<String>,
    /// The files read, with a hash of each one's text.
    files: Vec<(PathBuf, blake3::Hash)>,
}

/// A scenario id from a file name: lowercase letters, digits, `.`, `_`
/// and `-`.
fn id_from(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "config".into() } else { s }
}

fn read(p: &Path) -> Result<String, Error> {
    std::fs::read_to_string(p).map_err(|e| Error::Io(p.display().to_string(), e))
}

/// The `.strand` files of a config (a file or a directory), named as a
/// scenario's sections name them, with their texts.
fn config_files(p: &Path) -> Result<Vec<(String, PathBuf, String)>, Error> {
    let found = strand_compiler::source::find_files(p)
        .map_err(|e| Error::Io(p.display().to_string(), e))?;
    let root = if p.is_file() {
        p.parent().unwrap_or(p).to_path_buf()
    } else {
        p.to_path_buf()
    };
    let mut out = Vec::new();
    for f in found.files {
        let text = read(&f)?;
        let name = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
        out.push((name, f, text));
    }
    if out.is_empty() {
        return Err(Error::Config(format!(
            "{}: the config has no `.strand` file",
            p.display()
        )));
    }
    Ok(out)
}

/// `=== name` sections of `files`, each ending with a newline.
fn sections(files: &[(String, String)]) -> Result<String, Error> {
    let mut out = String::new();
    for (name, text) in files {
        if text.lines().any(|l| l.starts_with("=== ")) {
            return Err(Error::Config(format!(
                "{name}: a line starts with `=== `, which a scenario's sections cannot hold"
            )));
        }
        out.push_str(&format!("=== {name}\n{text}"));
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

fn load(opts: &Options, id: &str) -> Result<Loaded, Error> {
    let p = &opts.path;
    let is_scn = p.extension().is_some_and(|x| x == "scn");
    let mut files = Vec::new();
    let (header, secs, steps) = if is_scn {
        let text = read(p)?;
        files.push((p.clone(), blake3::hash(text.as_bytes())));
        let sc = Scenario::parse_in(&text, p, Place::Local)?;
        // The header as written, but for where it names its config: the
        // session writes the config in, as it ran.
        let end = text
            .split_inclusive('\n')
            .scan(0usize, |at, l| {
                let here = *at;
                *at += l.len();
                Some((here, l))
            })
            .find(|(_, l)| l.starts_with("=== "))
            .map_or(text.len(), |(at, _)| at);
        let header: String = text[..end]
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("config ")
                    || t.starts_with("config=")
                    || t.starts_with("config_dir"))
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let secs = match &sc.config {
            Config::Inline(s) => s
                .iter()
                .map(|s| (s.path.clone(), s.text.clone()))
                .collect::<Vec<_>>(),
            Config::Path(c) => {
                let mut v = Vec::new();
                for (name, f, text) in config_files(c)? {
                    files.push((f, blake3::hash(text.as_bytes())));
                    v.push((name, text));
                }
                v
            }
        };
        let steps = sc.steps.iter().map(|s| s.text.clone()).collect();
        (header, secs, steps)
    } else {
        let mut v = Vec::new();
        for (name, f, text) in config_files(p)? {
            files.push((f, blake3::hash(text.as_bytes())));
            v.push((name, text));
        }
        let s = opts.screen;
        let mut header = format!(
            "id     = \"{id}\"\nabout  = \"Recorded in strand-eval live from {}.\"\n\
             screen = {{ width = {}, height = {}, scale = {:?} }}\nhz     = {}\n",
            p.file_name()
                .map_or_else(
                    || p.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
                .replace(['"', '\\'], ""),
            s.width,
            s.height,
            s.scale,
            opts.hz
        );
        if opts.fonts == Fonts::Local {
            header.push_str("fonts  = \"local\"\n");
        }
        (header, v, Vec::new())
    };
    // One blank line after the header, however many it had: a session's
    // recording, opened again, writes itself again line for line.
    let mut head = header.trim_end().to_string();
    head.push_str("\n\n");
    head.push_str(&sections(&secs)?);
    head.push_str("=== steps\n");
    let first = head.matches('\n').count() as u32 + 1;
    Ok(Loaded {
        head,
        first,
        steps,
        files,
    })
}

/// A surface as one frame of the ring shows it.
#[derive(Clone, Debug)]
pub struct ShotSurface {
    pub id: u32,
    /// Where its buffer's top-left lies on the screen, and its size,
    /// logical; its buffer's size.
    pub origin: [f32; 2],
    pub logical: [f32; 2],
    pub size: Size,
    /// Premultiplied BGRA, shared with the frames before while it does
    /// not change.
    pub pixels: Arc<Vec<u8>>,
}

/// One frame of the ring: what the stage showed after tick `k`.
#[derive(Clone, Debug)]
pub struct Shot {
    pub k: u32,
    pub t: Duration,
    pub surfaces: Vec<ShotSurface>,
}

/// A knob: one step the compiled program says is worth taking.
#[derive(Clone, Debug, PartialEq)]
pub struct Knob {
    pub label: String,
    pub kind: KnobKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum KnobKind {
    /// A `bool` state.
    Toggle(String),
    /// An enum state and its values: the next one.
    Cycle(String, Vec<String>),
    /// An element to hover (by origin).
    Hover(String),
    /// An element's input: `click`, `secondary`, `middle` or `scroll`.
    Input(String, String),
    /// A service event the config handles, with no arguments.
    Emit(String),
    /// One with arguments: a step to finish typing.
    Prefill(String),
}

/// The checks' verdicts on the session so far.
#[derive(Clone, Debug)]
pub struct Analysis {
    /// The tick it was made at.
    pub k: u32,
    pub report: Report,
    pub trace: RunTrace,
    pub places: BTreeMap<String, Spot>,
}

/// A rule failure to mark on a frame: its check and the node's box on
/// the screen, logical.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub check: String,
    pub msg: String,
    pub rect: [f32; 4],
}

/// What a time step in progress is.
#[derive(Debug)]
enum Cur {
    Idle,
    /// A step from the queue (or typed), already in the log.
    Run(TimeRun),
    /// The clock running between the owner's steps.
    Span(Span),
}

/// Where the pointer goes before the next tick.
#[derive(Clone, Debug, PartialEq)]
enum Aim {
    On(u32, [f32; 2]),
    Off,
}

/// What `record` wrote.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: PathBuf,
    /// The session's trace digest at the recording: the scenario runs to
    /// it.
    pub digest: String,
    pub steps: usize,
}

/// A live session.
#[derive(Debug)]
pub struct Session {
    pub opts: Options,
    id: String,
    loaded: Loaded,
    driver: Driver,
    /// The steps done, one per line from `loaded.first`.
    log: Vec<String>,
    /// Steps to play (a `.scn`'s, or the replay after an edit).
    queue: VecDeque<String>,
    cur: Cur,
    pub paused: bool,
    pub speed: f64,
    acc: Duration,
    ring: VecDeque<Shot>,
    /// The frame of the ring shown (`None`: the newest).
    view: Option<usize>,
    aim: Option<Aim>,
    /// Where the driver's pointer is (the last pointer step sent).
    sent: Option<Aim>,
    analysis: Option<Analysis>,
    stale: bool,
    knobs: Vec<Knob>,
    /// A message for the strip, and whether it is an error.
    pub msg: Option<(String, bool)>,
    pub reloads: u32,
    /// The file `record` writes this session's recordings to, once
    /// chosen.
    recorded_to: Option<PathBuf>,
    /// Selectors per surface id, for the driver booted now.
    selectors: HashMap<u32, String>,
    /// The screen area every frame of the session has shown, logical.
    pub seen: Option<[f32; 4]>,
}

impl Session {
    /// Opens `opts.path`: boots its config and, for a `.scn`, queues its
    /// steps.
    pub fn open(opts: Options) -> Result<Session, Error> {
        let stem = opts
            .path
            .file_stem()
            .map_or_else(|| "config".into(), |s| s.to_string_lossy().into_owned());
        let id = format!("live-{}", id_from(&stem));
        let loaded = load(&opts, &id)?;
        let driver = boot(&loaded, &record_file(&opts, &id))?;
        let mut s = Session {
            id,
            queue: loaded.steps.iter().cloned().collect(),
            loaded,
            driver,
            log: Vec::new(),
            cur: Cur::Idle,
            paused: opts.paused,
            speed: opts.speed.clamp(SPEEDS[0], 1.0),
            acc: Duration::ZERO,
            ring: VecDeque::new(),
            view: None,
            aim: None,
            sent: None,
            analysis: None,
            stale: true,
            knobs: Vec::new(),
            msg: None,
            reloads: 0,
            recorded_to: None,
            selectors: HashMap::new(),
            seen: None,
            opts,
        };
        s.knobs = knobs(&s.driver)?;
        s.capture(true);
        s.settle_boot()?;
        Ok(s)
    }

    /// A config's boot run to rest at once (its first step, `rest`), so
    /// the stage shows the shell as it starts, paused or not; a `.scn`
    /// plays its own steps.
    fn settle_boot(&mut self) -> Result<(), Error> {
        if self.opts.path.extension().is_some_and(|x| x == "scn") || !self.log.is_empty() {
            return Ok(());
        }
        // Each tick into the ring, as `tick` puts it there.
        self.start("rest")?;
        while let Cur::Run(mut run) = std::mem::replace(&mut self.cur, Cur::Idle) {
            let k = self.driver.k();
            let done = self.driver.advance(&mut run)?;
            if self.driver.k() != k {
                self.capture(false);
            }
            if !done {
                self.cur = Cur::Run(run);
            }
        }
        Ok(())
    }

    pub fn driver(&self) -> &Driver {
        &self.driver
    }

    /// The steps done so far, as they will be written.
    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// Steps waiting to play.
    pub fn queued(&self) -> usize {
        self.queue.len() + usize::from(matches!(self.cur, Cur::Run(_)))
    }

    /// Whether a step is still to play or under way (the clock's own
    /// span aside).
    pub fn busy(&self) -> bool {
        !self.queue.is_empty() || matches!(self.cur, Cur::Run(_))
    }

    pub fn knobs(&self) -> &[Knob] {
        &self.knobs
    }

    pub fn ring(&self) -> &VecDeque<Shot> {
        &self.ring
    }

    /// The frame shown: the ring's `view`, else its newest.
    pub fn shown(&self) -> Option<&Shot> {
        match self.view {
            Some(i) => self.ring.get(i),
            None => self.ring.back(),
        }
    }

    /// The time of the last tick run (the boot's first when none ran).
    pub fn head_time(&self) -> Duration {
        let period = Duration::from_nanos(1_000_000_000 / u64::from(self.driver.hz().max(1)));
        if self.driver.k() == 0 {
            self.driver.now()
        } else {
            self.driver.now().saturating_sub(period)
        }
    }

    /// Whether the newest frame is shown.
    pub fn at_head(&self) -> bool {
        self.view.is_none()
    }

    pub fn analysis(&self) -> Option<&Analysis> {
        self.analysis.as_ref()
    }

    fn note(&mut self, msg: impl Into<String>, error: bool) {
        self.msg = Some((msg.into(), error));
    }

    /// The line the next step is written on.
    fn next_line(&self) -> u32 {
        self.loaded.first + self.log.len() as u32
    }

    /// One tick at the head: the pointer's move first, then the step in
    /// progress (the queue's next, else the clock running on its own).
    pub fn tick(&mut self) -> Result<(), Error> {
        if self.driver.trace().frames.len() >= self.opts.max_frames {
            self.paused = true;
            return Err(Error::Run(format!(
                "the session holds {} painted frames (--max-frames): record it (w) and restart (r)",
                self.driver.trace().frames.len()
            )));
        }
        self.view = None;
        self.flush_aim()?;
        let k = self.driver.k();
        let mut turns = 0;
        while self.driver.k() == k {
            turns += 1;
            if turns > 10_000 {
                return Err(Error::Run("the queue's steps never run a tick".into()));
            }
            match std::mem::replace(&mut self.cur, Cur::Idle) {
                Cur::Run(mut run) => match self.driver.advance(&mut run) {
                    Ok(true) => {
                        self.stale = true;
                    }
                    Ok(false) => self.cur = Cur::Run(run),
                    Err(e) => return self.failed(e),
                },
                Cur::Span(mut span) => match self.driver.span_tick(&mut span) {
                    Ok(true) => {
                        let text = self.driver.span_end(span, true)?;
                        self.log.push(text);
                        self.stale = true;
                    }
                    Ok(false) => self.cur = Cur::Span(span),
                    Err(e) => return self.failed(e),
                },
                Cur::Idle => match self.queue.pop_front() {
                    Some(text) => self.start(&text)?,
                    None => {
                        let line = self.next_line();
                        self.cur = Cur::Span(self.driver.span_begin(line));
                    }
                },
            }
        }
        self.capture(false);
        Ok(())
    }

    /// A run error mid-tick: the driver may be half way through a step,
    /// so the session is booted again from its log.
    fn failed(&mut self, e: Error) -> Result<(), Error> {
        self.queue.clear();
        self.paused = true;
        let msg = e.to_string();
        if let Err(again) = self.rebuild(false) {
            self.note(
                format!("{msg}; replaying the steps failed too: {again}"),
                true,
            );
        }
        Err(e)
    }

    /// Begins `text` at the head, logged: an instant step done, a time
    /// step left to run. A step the shell refuses changes nothing: the
    /// session is booted again from its log (the driver may have taken
    /// part of it).
    fn start(&mut self, text: &str) -> Result<(), Error> {
        let line = self.next_line();
        let parsed = steps::parse(text, line).map_err(|e| Error::Usage(e.msg))?;
        let Some(step) = parsed.into_iter().next() else {
            return Ok(());
        };
        if let StepKind::Notify { .. } = step.kind {
            return Err(Error::Unsupported(
                "`notify` (set `notifications.*` and `emit` instead)".into(),
            ));
        }
        match self.driver.begin(&step) {
            Ok(run) => {
                self.log.push(step.text.clone());
                if let Some(run) = run {
                    self.cur = Cur::Run(run);
                }
                self.stale = true;
                Ok(())
            }
            Err(e) => {
                self.queue.clear();
                self.rebuild(false)?;
                Err(e)
            }
        }
    }

    /// Ends what is under way so a step can follow: the clock's span is
    /// written as the ticks it ran; a queued `frames`, `wait` or `rest`
    /// cut short as `frames N`; a glide or drag, which a shorter step
    /// cannot stand for, runs to its end. The queue is dropped: the owner
    /// went another way.
    fn cut(&mut self) -> Result<(), Error> {
        self.queue.clear();
        match std::mem::replace(&mut self.cur, Cur::Idle) {
            Cur::Idle => {}
            Cur::Span(span) => {
                let text = self.driver.span_end(span, false)?;
                self.log.push(text);
            }
            Cur::Run(mut run) => {
                if run.sends_input() {
                    while !self.driver.advance(&mut run)? {}
                    self.capture(false);
                } else if let Some(last) = self.log.last_mut() {
                    if run.done() == 0 {
                        self.log.pop();
                    } else {
                        *last = format!("frames {}", run.done());
                    }
                }
            }
        }
        Ok(())
    }

    /// The pointer's pending move, sent as its step.
    fn flush_aim(&mut self) -> Result<(), Error> {
        let Some(aim) = self.aim.take() else {
            return Ok(());
        };
        let text = match &aim {
            Aim::Off if self.driver.pointer_on().is_none() => return Ok(()),
            _ if self.sent.as_ref() == Some(&aim) => return Ok(()),
            Aim::Off => "leave".to_string(),
            Aim::On(id, at) => {
                let Some(sel) = self.selector(*id) else {
                    self.note(format!("surface {id} cannot be named by a selector"), true);
                    return Ok(());
                };
                format!("pointer {},{} on {sel}", num(at[0]), num(at[1]))
            }
        };
        self.cut()?;
        self.start(&text)?;
        self.sent = Some(aim);
        Ok(())
    }

    fn selector(&mut self, id: u32) -> Option<String> {
        if let Some(s) = self.selectors.get(&id) {
            return Some(s.clone());
        }
        let s = self.driver.surface_selector(SurfaceId(id))?;
        self.selectors.insert(id, s.clone());
        Some(s)
    }

    /// A step the owner took (a knob, the pointer's button, a step typed
    /// in), at the head: what is under way ends first.
    pub fn act(&mut self, text: &str) -> Result<(), Error> {
        let parsed = steps::parse(text, self.next_line()).map_err(|e| Error::Usage(e.msg))?;
        if parsed.is_empty() {
            return Ok(());
        }
        self.view = None;
        // The pointer's move comes first, as it happened first.
        self.flush_aim()?;
        self.cut()?;
        self.start(text)?;
        if matches!(
            parsed[0].kind,
            StepKind::Pointer { .. } | StepKind::Click { .. } | StepKind::Leave
        ) {
            // The pointer is where the step put it, not where the stage's
            // last move did.
            self.sent = None;
        }
        self.note(format!("> {text}"), false);
        Ok(())
    }

    /// The pointer over the stage at `at` (screen, logical; `None`: off
    /// every surface), sent before the next tick. It lands on the
    /// topmost surface there that takes input there.
    pub fn aim(&mut self, at: Option<[f32; 2]>) {
        let Some(at) = at else {
            self.aim = Some(Aim::Off);
            return;
        };
        let hit = self.driver.surfaces().shown.iter().rev().find_map(|s| {
            let local = [at[0] - s.origin[0], at[1] - s.origin[1]];
            let inside = local[0] >= 0.0
                && local[1] >= 0.0
                && local[0] < s.logical[0]
                && local[1] < s.logical[1];
            if !inside {
                return None;
            }
            let takes = self.driver.input_region(s.id).is_none_or(|rs| {
                rs.iter().any(|r| {
                    local[0] >= r.x
                        && local[1] >= r.y
                        && local[0] < r.x + r.w
                        && local[1] < r.y + r.h
                })
            });
            takes.then_some((s.id.0, local))
        });
        self.aim = Some(match hit {
            Some((id, local)) => Aim::On(id, [quant(local[0]), quant(local[1])]),
            None => Aim::Off,
        });
    }

    /// The pointer's button over the stage (`left`, `right`,
    /// `middle`), at where the pointer is now.
    pub fn button(&mut self, button: &str, pressed: bool) -> Result<(), Error> {
        self.flush_aim()?;
        if self.driver.pointer_on().is_none() {
            return Err(Error::Usage(
                "the pointer is on no surface of the shell".into(),
            ));
        }
        let verb = if pressed { "press" } else { "release" };
        self.act(&format!("{verb} {button}"))
    }

    /// A wheel scroll over the stage of `dy` notches (positive down).
    pub fn scroll(&mut self, dy: f64) -> Result<(), Error> {
        self.flush_aim()?;
        if self.driver.pointer_on().is_none() {
            return Err(Error::Usage(
                "the pointer is on no surface of the shell".into(),
            ));
        }
        self.act(&format!("scroll {}", num(dy as f32)))
    }

    /// Knob `i` (from 0): its step.
    pub fn knob(&mut self, i: usize) -> Result<(), Error> {
        let Some(k) = self.knobs.get(i).cloned() else {
            return Err(Error::Usage(format!(
                "no knob {} (there are {})",
                i + 1,
                self.knobs.len()
            )));
        };
        match self.knob_step(&k)? {
            KnobStep::Steps(steps) => {
                for s in steps {
                    self.act(&s)?;
                }
                Ok(())
            }
            KnobStep::Prefill(text) => Err(Error::Usage(format!(
                "type the step's arguments: `{text}…`"
            ))),
        }
    }

    /// What knob `k` does now, as steps (or a step to finish typing).
    pub fn knob_step(&mut self, k: &Knob) -> Result<KnobStep, Error> {
        let spot = |s: &mut Session, o: &str| -> Result<(String, [f32; 2]), Error> {
            if s.stale || s.analysis.is_none() {
                s.analyse();
            }
            let places = s.analysis.as_ref().map(|a| &a.places);
            let spot = places
                .and_then(|p| p.get(o))
                .cloned()
                .ok_or_else(|| Error::Usage(format!("{o} is not on screen now")))?;
            let id = s.surface_of(&spot.surface).ok_or_else(|| {
                Error::Usage(format!("{o}'s surface ({}) is not shown", spot.surface))
            })?;
            let sel = s
                .selector(id)
                .ok_or_else(|| Error::Usage(format!("surface {id} cannot be named")))?;
            Ok((sel, spot.at))
        };
        let at = |p: [f32; 2]| format!("{},{}", num(p[0]), num(p[1]));
        Ok(KnobStep::Steps(match &k.kind {
            KnobKind::Toggle(p) => vec![format!("toggle {p}")],
            KnobKind::Cycle(p, values) => {
                let now = self.driver.value_text(p);
                let i = now
                    .and_then(|v| values.iter().position(|x| *x == v))
                    .map_or(0, |i| (i + 1) % values.len().max(1));
                let v = values
                    .get(i)
                    .ok_or_else(|| Error::Usage(format!("{p} has no values")))?;
                vec![format!("set {p} {v}")]
            }
            KnobKind::Hover(o) => {
                let (sel, p) = spot(self, o)?;
                vec![format!("pointer {} on {sel}", at(p))]
            }
            KnobKind::Input(o, input) => {
                let (sel, p) = spot(self, o)?;
                match input.as_str() {
                    "click" => vec![format!("click {} on {sel}", at(p))],
                    "secondary" => vec![format!("click {} right on {sel}", at(p))],
                    "middle" => vec![format!("click {} middle on {sel}", at(p))],
                    _ => vec![format!("pointer {} on {sel}", at(p)), "scroll 1".into()],
                }
            }
            KnobKind::Emit(p) => vec![format!("emit {p}")],
            KnobKind::Prefill(t) => return Ok(KnobStep::Prefill(t.clone())),
        }))
    }

    /// The shown surface whose root is written at `origin` (`@file:l:c`).
    fn surface_of(&self, origin: &str) -> Option<u32> {
        let t = self.driver.trace();
        let metas = self.analysis.as_ref().map(|a| &a.trace.nodes)?;
        self.driver.surfaces().shown.iter().find_map(|s| {
            let info = t
                .surfaces
                .iter()
                .rev()
                .find(|i| i.id == s.id.0 && i.detached.is_none())?;
            let o = metas.get(&info.root)?.origin.as_ref()?;
            (format!("@{o}") == origin).then_some(s.id.0)
        })
    }

    /// Advances the clock by `wall` of real time at the session's speed:
    /// the ticks due (at most 4 at once; a session that falls behind
    /// slows rather than skips). Whether any ran.
    pub fn play(&mut self, wall: Duration) -> Result<bool, Error> {
        if self.paused {
            self.acc = Duration::ZERO;
            return Ok(false);
        }
        let period = Duration::from_nanos(1_000_000_000 / u64::from(self.driver.hz().max(1)));
        self.acc += wall.mul_f64(self.speed);
        let mut ran = false;
        for _ in 0..4 {
            if self.acc < period {
                break;
            }
            self.acc -= period;
            self.tick()?;
            ran = true;
        }
        if self.acc > period * 4 {
            self.acc = Duration::ZERO;
        }
        Ok(ran)
    }

    /// Wall time to the next tick at the session's speed.
    pub fn until_next(&self) -> Option<Duration> {
        if self.paused {
            return None;
        }
        let period = Duration::from_nanos(1_000_000_000 / u64::from(self.driver.hz().max(1)));
        let wall = period.div_f64(self.speed.max(0.01));
        Some(wall.saturating_sub(self.acc.div_f64(self.speed.max(0.01))))
    }

    /// `n` ticks now, paused after (`step` when paused).
    pub fn step(&mut self, n: u32) -> Result<(), Error> {
        self.paused = true;
        if let Some(i) = self.view {
            // Forward through the ring first.
            let to = i + n as usize;
            if to + 1 < self.ring.len() {
                self.view = Some(to);
                return Ok(());
            }
            self.view = None;
            return Ok(());
        }
        for _ in 0..n {
            self.tick()?;
        }
        Ok(())
    }

    /// `n` frames back through the ring (pauses).
    pub fn back(&mut self, n: u32) {
        self.paused = true;
        if self.ring.is_empty() {
            return;
        }
        let at = self.view.unwrap_or(self.ring.len() - 1);
        self.view = Some(at.saturating_sub(n as usize));
    }

    /// Back to the newest frame.
    pub fn head(&mut self) {
        self.view = None;
    }

    /// The next frame of the ring, after the one shown, with a failing
    /// finding (pauses); whether there was one.
    pub fn next_finding(&mut self) -> bool {
        if self.stale || self.analysis.is_none() {
            self.analyse();
        }
        let Some(a) = &self.analysis else {
            return false;
        };
        let from = self.shown().map_or(0, |s| s.k);
        let mut ks: Vec<u32> = a
            .report
            .results
            .iter()
            .flat_map(|r| r.findings.iter())
            .filter(|f| f.sev == Sev::Fail)
            .filter_map(|f| f.k)
            .collect();
        ks.sort_unstable();
        let pick = ks.iter().find(|k| **k > from).or(ks.first()).copied();
        let Some(k) = pick else {
            return false;
        };
        let Some(i) = self.ring.iter().position(|s| s.k >= k) else {
            self.note(
                format!("the failing frame k{k} is no longer in the ring"),
                true,
            );
            return false;
        };
        self.paused = true;
        self.view = (i + 1 < self.ring.len()).then_some(i);
        true
    }

    pub fn slower(&mut self) {
        self.speed = SPEEDS
            .iter()
            .rev()
            .find(|s| **s < self.speed - 1e-9)
            .copied()
            .unwrap_or(SPEEDS[0]);
    }

    pub fn faster(&mut self) {
        self.speed = SPEEDS
            .iter()
            .find(|s| **s > self.speed + 1e-9)
            .copied()
            .unwrap_or(1.0);
    }

    pub fn set_speed(&mut self, s: f64) -> Result<(), Error> {
        if !(SPEEDS[0] - 1e-9..=1.0 + 1e-9).contains(&s) {
            return Err(Error::Usage(format!("speed {s}: 0.1 to 1")));
        }
        self.speed = s;
        Ok(())
    }

    /// Reduced motion flipped (`reduce-motion on|off`).
    pub fn toggle_motion(&mut self) -> Result<(), Error> {
        let on = !self.driver.reduced();
        self.act(if on {
            "reduce-motion on"
        } else {
            "reduce-motion off"
        })
    }

    /// The next screen scale of [`SCALES`].
    pub fn next_scale(&mut self) -> Result<(), Error> {
        let now = self.driver.surfaces().screen.scale;
        let i = SCALES.iter().position(|s| (*s - now).abs() < 1e-9);
        let next = SCALES[i.map_or(0, |i| (i + 1) % SCALES.len())];
        self.act(&format!("scale {next}"))
    }

    /// The next refresh rate of [`RATES`].
    pub fn next_rate(&mut self) -> Result<(), Error> {
        let now = self.driver.hz();
        let i = RATES.iter().position(|r| *r == now);
        let next = RATES[i.map_or(0, |i| (i + 1) % RATES.len())];
        self.act(&format!("hz {next}"))
    }

    /// The checks run over the session so far, and where the knobs'
    /// elements are now.
    pub fn analyse(&mut self) {
        let trace = self.driver.snapshot();
        let report = checks::evaluate(self.driver.scenario(), &trace, None, None, None);
        let places = explore::places(&trace);
        self.analysis = Some(Analysis {
            k: self.driver.k(),
            report,
            trace,
            places,
        });
        self.stale = false;
    }

    /// Whether the verdicts are older than the session.
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// The failing findings at the frame shown, each with its node's box
    /// on the screen at that frame.
    pub fn marks(&self) -> Vec<Mark> {
        let (Some(a), Some(shot)) = (&self.analysis, self.shown()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in &a.report.results {
            for f in r.findings.iter().filter(|f| f.sev == Sev::Fail) {
                if f.k != Some(shot.k) {
                    continue;
                }
                let Some(key) = &f.key else {
                    out.push(Mark {
                        check: f.check.clone(),
                        msg: f.msg.clone(),
                        rect: [0.0; 4],
                    });
                    continue;
                };
                let frame = a
                    .trace
                    .frames
                    .iter()
                    .rev()
                    .find(|fr| fr.k <= shot.k && fr.n.iter().any(|row| row.key == *key));
                let Some(fr) = frame else { continue };
                if let Some(row) = fr.n.iter().find(|row| row.key == *key) {
                    let o = fr.origins.get(&row.surface).copied().unwrap_or([0.0, 0.0]);
                    out.push(Mark {
                        check: f.check.clone(),
                        msg: f.msg.clone(),
                        rect: [
                            row.drawn[0] + o[0],
                            row.drawn[1] + o[1],
                            row.drawn[2],
                            row.drawn[3],
                        ],
                    });
                }
            }
        }
        out
    }

    /// One line per check: its name and verdict.
    pub fn verdicts(&self) -> Vec<(String, Verdict)> {
        let Some(a) = &self.analysis else {
            return Vec::new();
        };
        a.report
            .results
            .iter()
            .filter(|r| !matches!(r.status, Status::Skip(_)))
            .map(|r| {
                let n = r.findings.iter().filter(|f| f.sev == Sev::Fail).count();
                let v = match r.status {
                    Status::Fail => Verdict::Fail(n),
                    Status::Xfail(_) => Verdict::Xfail,
                    _ => Verdict::Pass,
                };
                (r.check.clone(), v)
            })
            .collect()
    }

    /// The frame just run into the ring (`first`: the boot's frame).
    fn capture(&mut self, first: bool) {
        let k = self.driver.k().saturating_sub(1);
        let (painted, t) = match self.driver.trace().frames.last() {
            Some(f) if f.k == k || first => (f.painted.clone(), Duration::from_nanos(f.t_ns)),
            _ => (Vec::new(), self.head_time()),
        };
        let prev = self.ring.back();
        let shown = &self.driver.surfaces().shown;
        // A surface configured since its last paint shows what it last
        // committed, if it has committed anything.
        let now: Vec<u32> = shown
            .iter()
            .filter(|s| !s.fresh || prev.is_some_and(|p| p.surfaces.iter().any(|x| x.id == s.id.0)))
            .map(|s| s.id.0)
            .collect();
        let before: Vec<u32> =
            prev.map_or_else(Vec::new, |p| p.surfaces.iter().map(|x| x.id).collect());
        if !first && painted.is_empty() && now == before {
            return;
        }
        let mut surfaces = Vec::new();
        for s in shown {
            let before = prev.and_then(|p| p.surfaces.iter().find(|x| x.id == s.id.0));
            if s.fresh {
                // Configured since its last paint: the compositor shows
                // what it last committed.
                if let Some(b) = before {
                    surfaces.push(b.clone());
                }
                continue;
            }
            let reuse = before
                .filter(|b| !painted.contains(&s.id.0) && b.size == s.size && b.origin == s.origin);
            surfaces.push(match reuse {
                Some(b) => b.clone(),
                None => ShotSurface {
                    id: s.id.0,
                    origin: s.origin,
                    logical: s.logical,
                    size: s.size,
                    pixels: Arc::new(s.pixels.clone()),
                },
            });
        }
        for s in &surfaces {
            let r = [s.origin[0], s.origin[1], s.logical[0], s.logical[1]];
            self.seen = Some(match self.seen {
                None => r,
                Some(b) => {
                    let x0 = b[0].min(r[0]);
                    let y0 = b[1].min(r[1]);
                    let x1 = (b[0] + b[2]).max(r[0] + r[2]);
                    let y1 = (b[1] + b[3]).max(r[1] + r[3]);
                    [x0, y0, x1 - x0, y1 - y0]
                }
            });
        }
        // A tick shows one frame: a later look at the same tick (the
        // boot's surfaces painted after their configure) replaces it.
        if self.ring.back().is_some_and(|b| b.k == k) {
            self.ring.pop_back();
        }
        self.ring.push_back(Shot { k, t, surfaces });
        while self.ring.len() > self.opts.ring.max(2) {
            self.ring.pop_front();
            if let Some(v) = self.view.as_mut() {
                *v = v.saturating_sub(1);
            }
        }
    }

    /// Boots the config again and runs the log on it at once (`fresh`:
    /// a new ring). A step the new boot refuses ends the log there.
    fn rebuild(&mut self, fresh: bool) -> Result<(), Error> {
        let mut d = boot(&self.loaded, &record_file(&self.opts, &self.id))?;
        let log = std::mem::take(&mut self.log);
        let mut kept: Vec<String> = Vec::new();
        let mut broke = None;
        for (i, text) in log.iter().enumerate() {
            let line = self.loaded.first + i as u32;
            let step = match steps::parse(text, line) {
                Ok(v) => match v.into_iter().next() {
                    Some(s) => s,
                    None => continue,
                },
                Err(e) => {
                    broke = Some(format!("`{text}`: {}", e.msg));
                    break;
                }
            };
            let out = d.begin(&step).and_then(|run| match run {
                Some(mut run) => {
                    while !d.advance(&mut run)? {}
                    Ok(())
                }
                None => Ok(()),
            });
            if let Err(e) = out {
                broke = Some(format!("`{text}`: {e}"));
                // The driver took part of the step: boot again with the
                // steps before it.
                d = boot(&self.loaded, &record_file(&self.opts, &self.id))?;
                for (j, t) in kept.iter().enumerate() {
                    let line = self.loaded.first + j as u32;
                    if let Some(s) = steps::parse(t, line)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        && let Some(mut run) = d.begin(&s)?
                    {
                        while !d.advance(&mut run)? {}
                    }
                }
                break;
            }
            kept.push(text.clone());
        }
        self.driver = d;
        self.log = kept;
        self.cur = Cur::Idle;
        self.selectors.clear();
        self.sent = None;
        self.aim = None;
        self.stale = true;
        if fresh {
            self.ring.clear();
            self.view = None;
            self.seen = None;
        }
        self.capture(true);
        if let Some(b) = broke {
            self.note(format!("the steps stop at {b}"), true);
        }
        Ok(())
    }

    /// Whether a file of the config changed since it was read.
    pub fn changed(&self) -> bool {
        self.loaded.files.iter().any(|(p, h)| {
            std::fs::read_to_string(p).map_or(true, |t| blake3::hash(t.as_bytes()) != *h)
        })
    }

    /// The files as they are now taken as read (an edit that failed to
    /// load is not tried again until the files change once more).
    pub fn forget_edit(&mut self) {
        for (p, h) in &mut self.loaded.files {
            if let Ok(t) = std::fs::read_to_string(&*p) {
                *h = blake3::hash(t.as_bytes());
            }
        }
    }

    /// The config read again. Its steps so far run on it at once up to
    /// the last thing the owner did, which then plays again; a `.scn`'s
    /// steps play from the start. A config that does not compile leaves
    /// the session as it was.
    pub fn reload(&mut self) -> Result<(), Error> {
        let loaded = load(&self.opts, &self.id)?;
        // Compile (and boot) first: an edit that does not compile changes
        // nothing.
        boot(&loaded, &record_file(&self.opts, &self.id))?;
        self.cut()?;
        let is_scn = self.opts.path.extension().is_some_and(|x| x == "scn");
        let (now, then): (Vec<String>, Vec<String>) = if is_scn {
            (Vec::new(), loaded.steps.clone())
        } else {
            let at = last_segment(&self.log);
            (self.log[..at].to_vec(), self.log[at..].to_vec())
        };
        self.loaded = loaded;
        self.log = now;
        self.rebuild(true)?;
        self.queue = then.into();
        self.knobs = knobs(&self.driver)?;
        self.reloads += 1;
        if !matches!(&self.msg, Some((_, true))) {
            self.note(
                format!(
                    "reloaded ({}): {} steps replayed, {} to play",
                    self.reloads,
                    self.log.len(),
                    self.queue.len()
                ),
                false,
            );
        }
        Ok(())
    }

    /// A fresh session on the same config: booted, nothing done.
    pub fn restart(&mut self) -> Result<(), Error> {
        self.loaded = load(&self.opts, &self.id)?;
        self.log.clear();
        self.queue = self.loaded.steps.iter().cloned().collect();
        self.rebuild(true)?;
        self.knobs = knobs(&self.driver)?;
        self.settle_boot()
    }

    /// The scenario the session is now: its config as it ran and every
    /// step so far, the one under way written as the ticks it has run.
    pub fn scenario_text(&mut self) -> Result<String, Error> {
        // A glide or drag under way runs to its end: a shorter step would
        // not send the same input.
        if let Cur::Run(run) = &mut self.cur
            && run.sends_input()
        {
            while !self.driver.advance(run)? {}
            self.cur = Cur::Idle;
            self.capture(false);
        }
        let mut lines = self.log.clone();
        match &self.cur {
            Cur::Idle => {}
            Cur::Span(s) => lines.push(format!("frames {}", s.ticks())),
            Cur::Run(run) => {
                if let Some(last) = lines.last_mut() {
                    *last = format!("frames {}", run.done());
                    if run.done() == 0 {
                        lines.pop();
                    }
                }
            }
        }
        let mut text = self.loaded.head.clone();
        for l in &lines {
            text.push_str(l);
            text.push('\n');
        }
        Ok(text)
    }

    /// Writes the session as a scenario: to `to`, else where this
    /// session wrote before, else `--record-to`, else a new file in
    /// `eval/scenarios/local/`.
    pub fn record(&mut self, to: Option<PathBuf>) -> Result<Recorded, Error> {
        let text = self.scenario_text()?;
        let path = match to
            .or_else(|| self.recorded_to.clone())
            .or_else(|| self.opts.record_to.clone())
        {
            Some(p) => p,
            None => fresh_file(&self.id)?,
        };
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(dir.display().to_string(), e))?;
        }
        // It must parse where it is written (a committed place refuses
        // local fonts).
        Scenario::parse(&text, &path)?;
        std::fs::write(&path, &text).map_err(|e| Error::Io(path.display().to_string(), e))?;
        self.recorded_to = Some(path.clone());
        let digest = self.driver.snapshot().digest;
        let steps = text.lines().count() + 1 - self.loaded.first as usize;
        self.note(
            format!("recorded {} ({steps} steps)", path.display()),
            false,
        );
        Ok(Recorded {
            path,
            digest,
            steps,
        })
    }
}

/// Where the last segment of `log` starts: the first step that changes
/// something after the `rest` before its last such step (the whole of
/// what the owner did last, a hover's moves together); `log.len()` when
/// nothing changed anything.
pub fn last_segment(log: &[String]) -> usize {
    let kind = |t: &String| {
        steps::parse(t, 1)
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|s| s.kind)
    };
    let opens = |t: &String| kind(t).is_some_and(|k| k.opens_segment());
    let rest = |t: &String| matches!(kind(t), Some(StepKind::Rest { .. }));
    let Some(last) = log.iter().rposition(opens) else {
        return log.len();
    };
    let from = log[..last].iter().rposition(rest).map_or(0, |r| r + 1);
    log[from..=last]
        .iter()
        .position(opens)
        .map_or(last, |i| from + i)
}

/// What a knob does now.
#[derive(Clone, Debug, PartialEq)]
pub enum KnobStep {
    Steps(Vec<String>),
    Prefill(String),
}

/// A check's verdict for the strip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verdict {
    Pass,
    Fail(usize),
    Xfail,
}

/// A number as a step writes it: at most two decimals, no trailing
/// zeros.
fn num(v: f32) -> String {
    let r = (f64::from(v) * 100.0).round() / 100.0;
    let s = format!("{r:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.to_string() }
}

/// A pointer coordinate kept to 1/100 px (what a step writes).
fn quant(v: f32) -> f32 {
    ((f64::from(v) * 100.0).round() / 100.0) as f32
}

/// The file the session's scenario is written as (its `file`: where a
/// recording lands by default, a local scenario's place).
fn record_file(opts: &Options, id: &str) -> PathBuf {
    opts.record_to.clone().unwrap_or_else(|| {
        crate::scenario::eval_root()
            .join("scenarios")
            .join("local")
            .join(format!("{id}.scn"))
    })
}

/// A file in `eval/scenarios/local/` no recording has taken.
fn fresh_file(id: &str) -> Result<PathBuf, Error> {
    let dir = crate::scenario::eval_root().join("scenarios").join("local");
    for n in 1..10_000 {
        let name = if n == 1 {
            format!("{id}.scn")
        } else {
            format!("{id}-{n}.scn")
        };
        let p = dir.join(name);
        if !p.exists() {
            return Ok(p);
        }
    }
    Err(Error::Usage(format!(
        "{}: no free name for {id}",
        dir.display()
    )))
}

/// The live run's options: the CPU leg alone, no surface leg, no image
/// kept (the stage reads the buffers), as `run` of its recording with the
/// same options (docs/eval.md "Live sandbox", Limits).
pub fn run_options() -> RunOptions {
    RunOptions {
        backend: Some(BackendMode::Cpu),
        surface: Some(false),
        build: crate::cli::build_info(),
        ..RunOptions::default()
    }
}

fn boot(loaded: &Loaded, file: &Path) -> Result<Driver, Error> {
    let sc = Scenario::parse_in(&loaded.head, file, Place::Local)?;
    Driver::boot(sc, &run_options())
}

/// The service events a config handles (`on svc.event(…)`), with how
/// many arguments each takes.
fn handled_events(prog: &Program, schema: &Schema) -> Vec<(String, usize)> {
    fn walk(nodes: &[Node], out: &mut Vec<(String, String)>) {
        for n in nodes {
            match n {
                Node::Handler(h) => {
                    if let Event::Service { service, event } = &h.event {
                        out.push((service.clone(), event.clone()));
                    }
                }
                Node::Element(e) => {
                    walk(&e.children, out);
                    walk(&e.call_handlers, out);
                }
                Node::Surface(s) => {
                    walk(&s.element.children, out);
                    walk(&s.body.nodes, out);
                }
                Node::If { then, else_, .. } => {
                    walk(then, out);
                    walk(else_, out);
                }
                Node::For(f) => walk(&f.body.nodes, out),
                Node::Match { arms, .. } => {
                    for a in arms {
                        walk(a, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    for f in &prog.files {
        walk(&f.items, &mut found);
    }
    for c in prog.components.values() {
        walk(&c.body.nodes, &mut found);
    }
    found.sort();
    found.dedup();
    found
        .into_iter()
        .filter_map(|(svc, ev)| {
            let rid = schema.service(&svc)?;
            let params = prog
                .types
                .record(rid)
                .events
                .iter()
                .find(|e| e.name == ev)?
                .params
                .len();
            Some((format!("{svc}.{ev}"), params))
        })
        .collect()
}

/// The knobs of the config the driver runs: what `new --explore`
/// exercises (its states, hovers and inputs, `explore::declared`), and
/// the service events it handles.
fn knobs(driver: &Driver) -> Result<Vec<Knob>, Error> {
    let (prog, map) = crate::harness::compile(driver.scenario())?;
    let d = explore::declared(&prog, &map);
    let mut out = Vec::new();
    for t in &d.toggles {
        out.push(Knob {
            label: format!("toggle {t}"),
            kind: KnobKind::Toggle(t.clone()),
        });
    }
    for (p, values) in &d.enums {
        out.push(Knob {
            label: format!("cycle {p}"),
            kind: KnobKind::Cycle(p.clone(), values.clone()),
        });
    }
    for o in d.hovers.iter().filter(|o| !d.inputs.contains_key(*o)) {
        out.push(Knob {
            label: format!("hover {o}"),
            kind: KnobKind::Hover(o.clone()),
        });
    }
    for (o, inputs) in &d.inputs {
        for i in inputs {
            out.push(Knob {
                label: format!("{i} {o}"),
                kind: KnobKind::Input(o.clone(), i.clone()),
            });
        }
    }
    let services = strand_services_schema::schemas();
    let schema = Schema::builtin_with(&services)
        .map_err(|(i, e)| Error::Config(format!("service schema {i}: {e:?}")))?;
    for (ev, params) in handled_events(&prog, &schema) {
        out.push(if params == 0 {
            Knob {
                label: format!("emit {ev}"),
                kind: KnobKind::Emit(ev),
            }
        } else {
            Knob {
                label: format!("emit {ev} …"),
                kind: KnobKind::Prefill(format!("emit {ev} ")),
            }
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_short() {
        assert_eq!(num(12.0), "12");
        assert_eq!(num(12.5), "12.5");
        assert_eq!(num(1.0 / 3.0), "0.33");
        assert_eq!(num(-0.001), "0");
    }

    #[test]
    fn ids_from_file_names() {
        assert_eq!(id_from("My Shell"), "my-shell");
        assert_eq!(id_from("bar.v2"), "bar-v2");
        assert_eq!(id_from("..."), "config");
    }
}
