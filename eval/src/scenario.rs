//! Scenario files (`*.scn`, docs/eval.md "Scenario format"): a TOML
//! header up to the first `=== ` line, then `=== <relpath>.strand`
//! sections (the config, written in the file) and `=== steps`.
//!
//! The script is not Strand syntax: values for `set` and `[services]` are
//! read by the target's type as `strand set` reads them
//! (`strand_compiler::instantiate::parse_text`).
//!
//! A committed scenario (anything outside `eval/scenarios/local/`) may
//! name only files inside `eval/` and the bundled fonts; the parser
//! refuses anything else, so an owner's config or fonts never reach a
//! gate.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::Error;
use crate::steps::{self, Step};

/// The checks a scenario can run (docs/eval.md, "Checks").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckId {
    Settle,
    Bystander,
    Envelope,
    Teleport,
    Vanish,
    /// One frame's geometry hangs together (inherit, pivot, morph start,
    /// stacking).
    Coherence,
    /// A text's glyphs fit its box.
    Ink,
    Twin,
    Determinism,
    Regions,
    CpuGpu,
    /// A partial redraw leaves its buffer as a full repaint of the same
    /// frame draws it.
    Redraw,
    Budget,
    Reference,
    /// The `expect` steps (always evaluated; named for `expect_fail`).
    Expect,
}

impl CheckId {
    pub const ALL: [CheckId; 15] = [
        CheckId::Settle,
        CheckId::Bystander,
        CheckId::Envelope,
        CheckId::Teleport,
        CheckId::Vanish,
        CheckId::Coherence,
        CheckId::Ink,
        CheckId::Twin,
        CheckId::Determinism,
        CheckId::Regions,
        CheckId::CpuGpu,
        CheckId::Redraw,
        CheckId::Budget,
        CheckId::Reference,
        CheckId::Expect,
    ];

    /// What a scenario runs without a `checks` line: every CPU check.
    /// `determinism` is the gate's to add (curated scenarios), `regions`
    /// is opt-in and `cpu-gpu` comes with `backend = "both"`.
    pub const DEFAULT: [CheckId; 12] = [
        CheckId::Settle,
        CheckId::Bystander,
        CheckId::Envelope,
        CheckId::Teleport,
        CheckId::Vanish,
        CheckId::Coherence,
        CheckId::Ink,
        CheckId::Twin,
        CheckId::Redraw,
        CheckId::Budget,
        CheckId::Reference,
        CheckId::Expect,
    ];

    pub fn name(self) -> &'static str {
        match self {
            CheckId::Settle => "settle",
            CheckId::Bystander => "bystander",
            CheckId::Envelope => "envelope",
            CheckId::Teleport => "teleport",
            CheckId::Vanish => "vanish",
            CheckId::Coherence => "coherence",
            CheckId::Ink => "ink",
            CheckId::Twin => "twin",
            CheckId::Determinism => "determinism",
            CheckId::Regions => "regions",
            CheckId::CpuGpu => "cpu-gpu",
            CheckId::Redraw => "redraw",
            CheckId::Budget => "budget",
            CheckId::Reference => "reference",
            CheckId::Expect => "expect",
        }
    }

    pub fn parse(s: &str) -> Option<CheckId> {
        CheckId::ALL.into_iter().find(|c| c.name() == s)
    }
}

/// How text is shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TextMode {
    /// On the render thread, as a text worker that answers within the
    /// estimate wait (deterministic).
    #[default]
    Inline,
    /// On the text worker, as `strand run` shapes it (advisory, never
    /// gates).
    Worker,
}

/// Which renderers draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BackendMode {
    #[default]
    Cpu,
    Gpu,
    /// Both, in lockstep (adds `cpu-gpu`).
    Both,
}

impl BackendMode {
    pub fn parse(s: &str) -> Result<BackendMode, String> {
        Ok(match s {
            "cpu" => BackendMode::Cpu,
            "gpu" => BackendMode::Gpu,
            "both" => BackendMode::Both,
            o => return Err(format!("`{o}` is not a backend (cpu, gpu or both)")),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            BackendMode::Cpu => "cpu",
            BackendMode::Gpu => "gpu",
            BackendMode::Both => "both",
        }
    }
}

/// Which fonts shape text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Fonts {
    /// Liberation Sans regular and bold, from strand-text's assets.
    #[default]
    Bundled,
    /// The fonts in `$STRAND_EVAL_FONTS` (scenarios/local only).
    Local,
}

/// Where a scenario lives, which decides what it may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// Committed (curated, regressions, a test, a matrix cell): only
    /// files inside `eval/` and the bundled fonts.
    Committed,
    /// `eval/scenarios/local/`: gitignored, the owner's own.
    Local,
}

/// The screen the shell runs on (one entry of `screens.all`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// Tolerances (docs/eval.md, "Checks").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tolerance {
    /// An edge's give, logical px (at least one physical pixel).
    pub edge_px: f64,
    /// A derived motion's overshoot, as a share of its travel.
    pub overshoot: f64,
    /// A frame's largest move at 60 Hz, logical px.
    pub teleport_px: f64,
}

impl Default for Tolerance {
    fn default() -> Self {
        Tolerance {
            edge_px: 0.5,
            overshoot: 0.35,
            teleport_px: 32.0,
        }
    }
}

/// Per-frame ceilings on deterministic costs (`[budget]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub items: Option<u64>,
    pub records: Option<u64>,
    pub damage_px: Option<u64>,
    pub layout_nodes: Option<u64>,
    pub layout_passes: Option<u64>,
}

impl Budget {
    pub fn is_empty(&self) -> bool {
        *self == Budget::default()
    }
}

/// An `[allow]` entry: a node a check leaves alone, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Allow {
    pub node: crate::select::Selector,
    pub why: String,
    /// The header line it is on.
    pub line: u32,
}

/// A file of the config, written in the scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// Its path in the config directory (`shell.strand`, `lib/x.strand`).
    pub path: String,
    pub text: String,
    /// The scenario line its text starts on.
    pub line: u32,
}

/// Where the config comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Config {
    Inline(Vec<Section>),
    /// A `.strand` file or a config directory.
    Path(PathBuf),
}

/// A parsed scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Scenario {
    pub id: String,
    pub about: String,
    /// The file it was read from (for messages; may not exist).
    pub file: PathBuf,
    pub place: Place,
    pub screen: Screen,
    pub hz: u32,
    pub text: TextMode,
    pub backend: BackendMode,
    pub fonts: Fonts,
    pub checks: Vec<CheckId>,
    pub expect_fail: BTreeMap<CheckId, String>,
    /// `[services]`: paths and values as text, set before boot.
    pub services: Vec<(String, String)>,
    pub allow: BTreeMap<CheckId, Vec<Allow>>,
    pub tolerance: Tolerance,
    pub budget: Budget,
    /// The twin check compares pixels too.
    pub twin_pixels: bool,
    /// `tooltip_delay` in ms (the renderer's default when unset).
    pub tooltip_delay: Option<u64>,
    /// Motion reduced from the start.
    pub reduce_motion: bool,
    /// The compositor applies surface poses (`Renderer::set_compositor_poses`):
    /// a surface root's fade, scale and offset are committed beside its
    /// pixels, which it paints at rest.
    pub compositor_poses: bool,
    pub config: Config,
    pub steps: Vec<Step>,
    /// The scenario's text as parsed (a run keeps it beside its trace, so
    /// `promote` can write it out again).
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    id: String,
    about: Option<String>,
    screen: Option<ScreenH>,
    hz: Option<u32>,
    text: Option<String>,
    backend: Option<String>,
    fonts: Option<String>,
    checks: Option<Vec<String>>,
    expect_fail: Option<BTreeMap<String, String>>,
    services: Option<BTreeMap<String, toml::Value>>,
    allow: Option<BTreeMap<String, Vec<AllowH>>>,
    tolerance: Option<TolH>,
    budget: Option<BudgetH>,
    config: Option<String>,
    config_dir: Option<String>,
    twin_pixels: Option<bool>,
    tooltip_delay: Option<u64>,
    reduce_motion: Option<bool>,
    compositor_poses: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenH {
    width: Option<u32>,
    height: Option<u32>,
    scale: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowH {
    node: String,
    why: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TolH {
    edge_px: Option<f64>,
    overshoot: Option<f64>,
    teleport_px: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetH {
    items: Option<u64>,
    records: Option<u64>,
    damage_px: Option<u64>,
    layout_nodes: Option<u64>,
    layout_passes: Option<u64>,
}

/// The `eval/` directory (this crate's).
pub fn eval_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `path` with `.` and `..` folded away, lexically.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// Whether `path` lies inside `eval/` (symlinks followed when it exists).
fn inside_eval(path: &Path) -> bool {
    let root = eval_root();
    let root = std::fs::canonicalize(&root).unwrap_or_else(|_| normal(&root));
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| normal(path));
    p.starts_with(root)
}

/// Where a scenario file at `file` lives.
pub fn place_of(file: &Path) -> Place {
    let n = normal(file);
    let parts: Vec<_> = n.components().map(|c| c.as_os_str().to_owned()).collect();
    let local = parts
        .windows(2)
        .any(|w| w[0] == "scenarios" && w[1] == "local");
    if local {
        Place::Local
    } else {
        Place::Committed
    }
}

/// `${VAR}` in `s`, from the environment.
fn expand(s: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("`{s}`: `${{` is not closed"))?;
        let name = &after[..end];
        let v = std::env::var(name).map_err(|_| format!("`{s}`: ${name} is not set"))?;
        out.push_str(&v);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The 1-based line of byte `at` in `text`.
fn line_at(text: &str, at: usize) -> u32 {
    text[..at.min(text.len())].matches('\n').count() as u32 + 1
}

/// The header line a TOML key is written on (its first `key =`), else 1.
fn key_line(header: &str, key: &str) -> u32 {
    header
        .lines()
        .position(|l| {
            let l = l.trim_start().trim_start_matches('[');
            l.strip_prefix(key)
                .is_some_and(|r| r.trim_start().starts_with(['=', ']', '.']))
        })
        .map_or(1, |i| i as u32 + 1)
}

impl Scenario {
    /// Reads and parses the scenario at `path`.
    pub fn load(path: &Path) -> Result<Scenario, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Io(format!("{}", path.display()), e))?;
        Scenario::parse(&text, path)
    }

    /// Parses `text`, the scenario at `file` (placed by its path).
    pub fn parse(text: &str, file: &Path) -> Result<Scenario, Error> {
        Scenario::parse_in(text, file, place_of(file))
    }

    /// Parses `text` as a scenario at `file` with the rules of `place`.
    pub fn parse_in(text: &str, file: &Path, place: Place) -> Result<Scenario, Error> {
        let err = |line: u32, msg: String| Error::Scenario {
            file: file.display().to_string(),
            line,
            msg,
        };
        // Split at `=== ` lines.
        let mut header_end = text.len();
        // (its line's start, its body's start, its line, its name)
        let mut marks: Vec<(usize, usize, u32, String)> = Vec::new();
        let mut at = 0;
        for (i, l) in text.split_inclusive('\n').enumerate() {
            if let Some(name) = l.strip_prefix("=== ") {
                if marks.is_empty() {
                    header_end = at;
                }
                marks.push((at, at + l.len(), i as u32 + 1, name.trim().to_string()));
            }
            at += l.len();
        }
        let header_text = &text[..header_end];
        let h: Header = toml::from_str(header_text).map_err(|e| {
            let line = e.span().map_or(1, |s| line_at(header_text, s.start));
            err(line, format!("header: {}", e.message().trim()))
        })?;
        let hl = |key: &str| key_line(header_text, key);
        let mut sections = Vec::new();
        let mut steps_at: Option<(u32, &str)> = None;
        for (i, (_, start, line, name)) in marks.iter().enumerate() {
            let end = marks.get(i + 1).map_or(text.len(), |m| m.0);
            let body = &text[*start..end];
            if steps_at.is_some() {
                return Err(err(*line, "`=== steps` is the last section".into()));
            }
            if name == "steps" {
                steps_at = Some((line + 1, body));
                continue;
            }
            let p = Path::new(name);
            let ok = name.ends_with(".strand")
                && p.is_relative()
                && p.components().all(|c| matches!(c, Component::Normal(_)));
            if !ok {
                return Err(err(
                    *line,
                    format!(
                        "`=== {name}`: a section is `=== steps` or a relative \
                         `<path>.strand` inside the config"
                    ),
                ));
            }
            if sections.iter().any(|s: &Section| s.path == *name) {
                return Err(err(*line, format!("`=== {name}` is written twice")));
            }
            sections.push(Section {
                path: name.clone(),
                text: body.to_string(),
                line: line + 1,
            });
        }
        let (first, body) = steps_at.ok_or_else(|| {
            err(
                line_at(text, text.len()),
                "no `=== steps` section".to_string(),
            )
        })?;
        let steps = steps::parse(body, first).map_err(|e| err(e.line, e.msg))?;

        // The header's keys.
        let id = h.id;
        let id_ok = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
            && !id.starts_with('.');
        if !id_ok {
            return Err(err(
                hl("id"),
                format!("id `{id}`: lowercase letters, digits, `.`, `_` and `-`"),
            ));
        }
        let screen = {
            let s = h.screen.unwrap_or(ScreenH {
                width: None,
                height: None,
                scale: None,
            });
            let scale = s.scale.unwrap_or(1.0);
            if !(scale > 0.0 && scale <= 4.0) {
                return Err(err(hl("screen"), format!("screen scale {scale}: 0 to 4")));
            }
            Screen {
                width: s.width.unwrap_or(300).max(1),
                height: s.height.unwrap_or(1080).max(1),
                scale,
            }
        };
        let hz = h.hz.unwrap_or(60);
        if hz == 0 || hz > 1000 {
            return Err(err(hl("hz"), format!("hz {hz}: 1 to 1000")));
        }
        let text_mode = match h.text.as_deref() {
            None | Some("inline") => TextMode::Inline,
            Some("worker") => TextMode::Worker,
            Some(o) => {
                return Err(err(hl("text"), format!("text `{o}`: inline or worker")));
            }
        };
        let backend = match h.backend.as_deref() {
            None => BackendMode::Cpu,
            Some(b) => BackendMode::parse(b).map_err(|m| err(hl("backend"), m))?,
        };
        let fonts = match h.fonts.as_deref() {
            None | Some("bundled") => Fonts::Bundled,
            Some("local") if place == Place::Local => Fonts::Local,
            Some("local") => {
                return Err(err(
                    hl("fonts"),
                    "fonts = \"local\" only in eval/scenarios/local/ (a committed \
                     scenario uses the bundled fonts)"
                        .into(),
                ));
            }
            Some(o) => return Err(err(hl("fonts"), format!("fonts `{o}`: bundled or local"))),
        };
        let check = |name: &str, key: &str| {
            CheckId::parse(name).ok_or_else(|| err(hl(key), format!("`{name}` is not a check")))
        };
        let mut checks = match &h.checks {
            Some(list) => {
                let mut v = Vec::new();
                for c in list {
                    let c = check(c, "checks")?;
                    if !v.contains(&c) {
                        v.push(c);
                    }
                }
                v
            }
            None => CheckId::DEFAULT.to_vec(),
        };
        if !checks.contains(&CheckId::Expect) {
            checks.push(CheckId::Expect);
        }
        if backend == BackendMode::Both && !checks.contains(&CheckId::CpuGpu) {
            checks.push(CheckId::CpuGpu);
        }
        let mut expect_fail = BTreeMap::new();
        for (c, why) in h.expect_fail.unwrap_or_default() {
            let id = check(&c, "expect_fail")?;
            if why.trim().is_empty() {
                return Err(err(
                    hl("expect_fail"),
                    format!("expect_fail `{c}` needs a reason"),
                ));
            }
            expect_fail.insert(id, why);
        }
        let mut services = Vec::new();
        for (path, v) in h.services.unwrap_or_default() {
            let text = match v {
                toml::Value::String(s) => s,
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                _ => {
                    return Err(err(
                        hl("services"),
                        format!("[services] `{path}`: a string, number or bool"),
                    ));
                }
            };
            services.push((path, text));
        }
        let mut allow: BTreeMap<CheckId, Vec<Allow>> = BTreeMap::new();
        for (c, list) in h.allow.unwrap_or_default() {
            let id = check(&c, "allow")?;
            for a in list {
                let line = hl("allow");
                if a.why.trim().is_empty() {
                    return Err(err(
                        line,
                        format!("[allow] {c} `{}`: `why` is empty", a.node),
                    ));
                }
                let node = crate::select::Selector::parse(&a.node).map_err(|m| err(line, m))?;
                allow.entry(id).or_default().push(Allow {
                    node,
                    why: a.why,
                    line,
                });
            }
        }
        let mut tolerance = Tolerance::default();
        if let Some(t) = h.tolerance {
            let pos = |v: Option<f64>, d: f64, name: &str| match v {
                Some(v) if v >= 0.0 && v.is_finite() => Ok(v),
                Some(v) => Err(err(
                    hl("tolerance"),
                    format!("tolerance {name} {v}: 0 or more"),
                )),
                None => Ok(d),
            };
            tolerance = Tolerance {
                edge_px: pos(t.edge_px, tolerance.edge_px, "edge_px")?,
                overshoot: pos(t.overshoot, tolerance.overshoot, "overshoot")?,
                teleport_px: pos(t.teleport_px, tolerance.teleport_px, "teleport_px")?,
            };
        }
        let budget = h.budget.map_or(Budget::default(), |b| Budget {
            items: b.items,
            records: b.records,
            damage_px: b.damage_px,
            layout_nodes: b.layout_nodes,
            layout_passes: b.layout_passes,
        });

        // The config.
        let base = file.parent().unwrap_or(Path::new("."));
        let config = match (h.config, h.config_dir, sections.is_empty()) {
            (Some(_), Some(_), _) => {
                return Err(err(
                    hl("config"),
                    "`config` or `config_dir`, not both".into(),
                ));
            }
            (Some(_), _, false) | (_, Some(_), false) => {
                return Err(err(
                    hl("config"),
                    "the config is either named (`config`) or written in `===` \
                     sections, not both"
                        .into(),
                ));
            }
            (Some(c), None, true) => {
                // `${VAR}` only where a scenario may name what is outside
                // eval/ (a committed one is held to its own files).
                let c = match place {
                    Place::Local => expand(&c).map_err(|m| err(hl("config"), m))?,
                    Place::Committed if c.contains("${") => {
                        return Err(err(
                            hl("config"),
                            format!("config `{c}`: `${{VAR}}` only in eval/scenarios/local/"),
                        ));
                    }
                    Place::Committed => c,
                };
                let p = normal(&base.join(&c));
                if place == Place::Committed && !inside_eval(&p) {
                    return Err(err(
                        hl("config"),
                        format!(
                            "config `{c}` is outside eval/: a committed scenario names \
                             only files inside eval/ (owner configs go in \
                             eval/scenarios/local/)"
                        ),
                    ));
                }
                Config::Path(p)
            }
            (None, Some(d), true) => {
                if place != Place::Local {
                    return Err(err(
                        hl("config_dir"),
                        "config_dir only in eval/scenarios/local/".into(),
                    ));
                }
                let d = expand(&d).map_err(|m| err(hl("config_dir"), m))?;
                Config::Path(normal(&base.join(d)))
            }
            (None, None, false) => Config::Inline(sections),
            (None, None, true) => {
                return Err(err(
                    1,
                    "no config: write it in `=== <file>.strand` sections or name it \
                     with `config = \"…\"`"
                        .into(),
                ));
            }
        };
        let about = h.about.unwrap_or_default();
        Ok(Scenario {
            id,
            about,
            file: file.to_path_buf(),
            place,
            screen,
            hz,
            text: text_mode,
            backend,
            fonts,
            checks,
            expect_fail,
            services,
            allow,
            tolerance,
            budget,
            twin_pixels: h.twin_pixels.unwrap_or(false),
            tooltip_delay: h.tooltip_delay,
            reduce_motion: h.reduce_motion.unwrap_or(false),
            compositor_poses: h.compositor_poses.unwrap_or(false),
            config,
            steps,
            source: text.to_string(),
        })
    }

    /// Whether it runs `check`.
    pub fn runs(&self, check: CheckId) -> bool {
        self.checks.contains(&check)
    }
}
