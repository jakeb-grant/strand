//! The `=== steps` section of a scenario: one verb per line (docs/eval.md,
//! "Step vocabulary"). Instant steps act at the tick the next time step
//! starts, before logic steps; time steps advance the virtual clock.
//!
//! `#` starts a comment when it stands alone (`rest   # boot`), so `#bars`
//! stays a selector and `#ff8800` a colour.

use std::fmt;

use strand_render::probe::Axes;

use crate::scenario::BackendMode;
use crate::select::{Selector, unescape};

/// One step, with the line it is on (1-based, in the scenario file).
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub line: u32,
    /// The line as written, comment dropped.
    pub text: String,
    pub kind: StepKind,
}

/// A point in a surface's logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// When a released text reaches the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Via {
    /// Just before the next paint (the default).
    #[default]
    Paint,
    /// Then `Renderer::update` between frames: the runtime's `text_ready`
    /// path.
    Waker,
}

/// What an `intend` line says a node does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intention {
    Moves,
    Enters,
    Exits,
    Changes,
}

/// What an `expect` line asserts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpectKind {
    /// Its visual box (on `axis`, else x, y, w and h) does not move.
    Still,
    /// It shows now.
    Visible,
    /// It does not show now.
    Hidden,
    /// Its visual box (on `axis`, else any) moves.
    Moves,
}

/// An axis of a node's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    W,
    H,
    Opacity,
    Scale,
}

impl Axis {
    pub fn parse(s: &str) -> Option<Axis> {
        Some(match s {
            "x" => Axis::X,
            "y" => Axis::Y,
            "w" | "width" => Axis::W,
            "h" | "height" => Axis::H,
            "opacity" | "op" => Axis::Opacity,
            "scale" => Axis::Scale,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Axis::X => "x",
            Axis::Y => "y",
            Axis::W => "w",
            Axis::H => "h",
            Axis::Opacity => "opacity",
            Axis::Scale => "scale",
        }
    }

    /// The probe's bit for it.
    pub fn bits(self) -> Axes {
        match self {
            Axis::X => Axes::X,
            Axis::Y => Axes::Y,
            Axis::W => Axes::W,
            Axis::H => Axes::H,
            Axis::Opacity => Axes::OPACITY,
            Axis::Scale => Axes::SCALE,
        }
    }
}

/// What a step does.
#[derive(Clone, Debug, PartialEq)]
pub enum StepKind {
    // State and services.
    Set {
        path: String,
        value: String,
    },
    Toggle {
        path: String,
    },
    Emit {
        path: String,
        args: Vec<String>,
    },
    Notify {
        summary: String,
        body: Option<String>,
        app: Option<String>,
        urgency: Option<String>,
    },
    // Input.
    Pointer {
        at: Point,
        on: Option<Selector>,
    },
    Press {
        button: Button,
    },
    Release {
        button: Button,
    },
    Click {
        at: Point,
        on: Option<Selector>,
        button: Button,
    },
    Scroll {
        dy: f64,
    },
    /// A touchpad swipe: `dy` logical px spread evenly over `ms`, one
    /// axis frame a tick, then the fingers lift (which may fling).
    Swipe {
        dy: f64,
        ms: u32,
    },
    /// (eval-live) The pointer leaves every surface.
    Leave,
    // Perturbations.
    TextHold {
        filter: Option<String>,
    },
    TextRelease {
        filter: Option<String>,
        via: Via,
    },
    TextLate {
        n: u32,
        filter: Option<String>,
        via: Via,
    },
    Drop {
        n: u32,
        surface: Option<Selector>,
    },
    Jitter(Vec<f64>),
    // Environment.
    ReduceMotion(bool),
    Scale(f64),
    Hz(u32),
    Backend(BackendMode),
    // Annotation.
    Mark(String),
    Shot(String),
    Log(String),
    Intend {
        sel: Selector,
        what: Intention,
        axes: Axes,
    },
    Expect {
        what: ExpectKind,
        sel: Selector,
        axis: Option<Axis>,
        from: u32,
    },
    // Time.
    Frames(u32),
    Wait(u32),
    Rest {
        max: u32,
    },
    Glide {
        from: Point,
        to: Point,
        ms: u32,
        on: Option<Selector>,
    },
    Drag {
        from: Point,
        to: Point,
        ms: u32,
        on: Option<Selector>,
    },
}

/// `rest` without `max`: ticks before it fails `settle`.
pub const REST_MAX: u32 = 600;

impl StepKind {
    /// It advances the virtual clock.
    pub fn is_time(&self) -> bool {
        matches!(
            self,
            StepKind::Frames(_)
                | StepKind::Wait(_)
                | StepKind::Rest { .. }
                | StepKind::Glide { .. }
                | StepKind::Drag { .. }
                | StepKind::Swipe { .. }
        )
    }

    /// It changes state, input or the environment: a segment starts at
    /// it (docs/eval.md, "Checks").
    pub fn opens_segment(&self) -> bool {
        !matches!(
            self,
            StepKind::Mark(_)
                | StepKind::Shot(_)
                | StepKind::Log(_)
                | StepKind::Intend { .. }
                | StepKind::Expect { .. }
                | StepKind::Frames(_)
                | StepKind::Wait(_)
                | StepKind::Rest { .. }
                | StepKind::TextHold { .. }
                | StepKind::TextLate { .. }
                | StepKind::Jitter(_)
                | StepKind::Drop { .. }
        )
    }
}

/// A line's words: whitespace-separated, `"…"` kept whole (unquoted),
/// `text("…")` kept whole (as written), a lone `#` ending the line.
pub(crate) fn words(line: &str) -> Result<Vec<Word>, String> {
    let mut out = Vec::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'#' && b.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()) {
            break;
        }
        if c == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            if j >= b.len() {
                return Err("a `\"` is not closed".into());
            }
            out.push(Word {
                text: unescape(&line[start..j]),
                quoted: true,
            });
            i = j + 1;
            continue;
        }
        let start = i;
        if line[i..].starts_with("text(\"") {
            let mut j = i + 6;
            while j < b.len() && !(b[j] == b'"' && b.get(j + 1) == Some(&b')')) {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            if j >= b.len() {
                return Err("`text(\"…\")` is not closed".into());
            }
            i = j + 2;
        } else {
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        out.push(Word {
            text: line[start..i].to_string(),
            quoted: false,
        });
    }
    Ok(out)
}

/// A word of a step line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Word {
    pub text: String,
    pub quoted: bool,
}

/// The line without its comment.
fn uncommented(line: &str) -> &str {
    let b = line.as_bytes();
    let mut quoted = false;
    for (i, c) in b.iter().enumerate() {
        match c {
            b'"' if i == 0 || b[i - 1] != b'\\' => quoted = !quoted,
            b'#' if !quoted
                && (i == 0 || b[i - 1].is_ascii_whitespace())
                && b.get(i + 1).is_none_or(|n| n.is_ascii_whitespace()) =>
            {
                return line[..i].trim_end();
            }
            _ => {}
        }
    }
    line.trim_end()
}

fn point(s: &str) -> Result<Point, String> {
    let (x, y) = s
        .split_once(',')
        .ok_or_else(|| format!("`{s}` is not a point (`X,Y`)"))?;
    let num = |t: &str| {
        t.trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("`{s}` is not a point (`X,Y`)"))
    };
    Ok(Point {
        x: num(x)?,
        y: num(y)?,
    })
}

fn count(s: &str, what: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|_| format!("`{s}` is not a whole number of {what}"))
}

fn button(s: Option<&str>) -> Result<Button, String> {
    Ok(match s {
        None | Some("left") => Button::Left,
        Some("right") => Button::Right,
        Some("middle") => Button::Middle,
        Some(o) => return Err(format!("`{o}` is not a button (left, right or middle)")),
    })
}

fn selector(s: &str) -> Result<Selector, String> {
    Selector::parse(s)
}

/// `[on <selector>]` at `w[i..]`.
fn on(w: &[Word], i: usize) -> Result<Option<Selector>, String> {
    match &w[i.min(w.len())..] {
        [] => Ok(None),
        [o, s] if o.text == "on" => Ok(Some(selector(&s.text)?)),
        rest => Err(format!(
            "unexpected `{}` (`on <selector>` names the surface)",
            rest[0].text
        )),
    }
}

/// `[where "<substr>"] [via waker|paint]` at `w[i..]`.
fn text_rule(w: &[Word], mut i: usize, via_ok: bool) -> Result<(Option<String>, Via), String> {
    let mut filter = None;
    let mut via = Via::Paint;
    while i < w.len() {
        match (w[i].text.as_str(), w.get(i + 1)) {
            ("where", Some(s)) if filter.is_none() => filter = Some(s.text.clone()),
            ("via", Some(v)) if via_ok => {
                via = match v.text.as_str() {
                    "waker" => Via::Waker,
                    "paint" => Via::Paint,
                    o => return Err(format!("`via {o}`: `via waker` or `via paint`")),
                }
            }
            (o, _) => {
                return Err(format!(
                    "unexpected `{o}` (`where \"<text>\"`{})",
                    if via_ok { ", `via waker|paint`" } else { "" }
                ));
            }
        }
        i += 2;
    }
    Ok((filter, via))
}

/// `X,Y -> X2,Y2 over MS [on S]`.
fn path(w: &[Word]) -> Result<(Point, Point, u32, Option<Selector>), String> {
    match w {
        [_, a, arrow, b, over, ms, ..] if arrow.text == "->" && over.text == "over" => {
            let ms = ms
                .text
                .strip_suffix("ms")
                .unwrap_or(&ms.text)
                .parse::<u32>()
                .map_err(|_| format!("`{}` is not milliseconds", ms.text))?;
            Ok((point(&a.text)?, point(&b.text)?, ms, on(w, 6)?))
        }
        _ => Err(format!(
            "`{}` takes `X,Y -> X2,Y2 over MS [on <surface>]`",
            w[0].text
        )),
    }
}

fn axes(words: &[Word]) -> Result<Axes, String> {
    let mut out = Axes::NONE;
    for w in words {
        let a = Axis::parse(&w.text)
            .ok_or_else(|| format!("`{}` is not an axis (x, y, w, h, opacity, scale)", w.text))?;
        out |= a.bits();
    }
    Ok(out)
}

/// Parses one line (its comment already dropped); `None` for a blank.
fn parse_line(text: &str) -> Result<Option<StepKind>, String> {
    let w = words(text)?;
    let Some(verb) = w.first() else {
        return Ok(None);
    };
    let arg = |i: usize| w.get(i).map(|w| w.text.as_str());
    let need = |i: usize, what: &str| -> Result<&str, String> {
        arg(i).ok_or_else(|| format!("`{}` takes {what}", verb.text))
    };
    let only = |n: usize| -> Result<(), String> {
        match w.get(n) {
            Some(x) => Err(format!("unexpected `{}` after `{}`", x.text, verb.text)),
            None => Ok(()),
        }
    };
    let rest_of = |after: &str| -> String {
        // The raw text after the verb's first `n` words.
        text.trim_start()
            .strip_prefix(after)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let k = match verb.text.as_str() {
        "set" => {
            let path = need(1, "a path and a value")?.to_string();
            need(2, "a path and a value")?;
            let value = if w.len() == 3 && w[2].quoted {
                w[2].text.clone()
            } else {
                rest_of("set")
                    .strip_prefix(path.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            StepKind::Set { path, value }
        }
        "toggle" => {
            only(2)?;
            StepKind::Toggle {
                path: need(1, "a path")?.to_string(),
            }
        }
        "emit" => StepKind::Emit {
            path: need(1, "`service.event` and its arguments")?.to_string(),
            args: w[2..].iter().map(|w| w.text.clone()).collect(),
        },
        "notify" => {
            let summary = need(1, "\"<summary>\" [\"<body>\"] [app=… urgency=…]")?.to_string();
            let mut body = None;
            let mut app = None;
            let mut urgency = None;
            for x in &w[2..] {
                if let Some(v) = x.text.strip_prefix("app=").filter(|_| !x.quoted) {
                    app = Some(v.to_string());
                } else if let Some(v) = x.text.strip_prefix("urgency=").filter(|_| !x.quoted) {
                    urgency = Some(v.to_string());
                } else if body.is_none() && x.quoted {
                    body = Some(x.text.clone());
                } else {
                    return Err(format!("unexpected `{}` in `notify`", x.text));
                }
            }
            StepKind::Notify {
                summary,
                body,
                app,
                urgency,
            }
        }
        "pointer" => StepKind::Pointer {
            at: point(need(1, "`X,Y [on <surface>]`")?)?,
            on: on(&w, 2)?,
        },
        "press" | "release" => {
            only(2)?;
            let b = button(arg(1))?;
            if verb.text == "press" {
                StepKind::Press { button: b }
            } else {
                StepKind::Release { button: b }
            }
        }
        "click" => {
            let at = point(need(1, "`X,Y [left|right|middle] [on <surface>]`")?)?;
            let (b, i) = match arg(2) {
                Some(b @ ("left" | "right" | "middle")) => (button(Some(b))?, 3),
                _ => (Button::Left, 2),
            };
            StepKind::Click {
                at,
                on: on(&w, i)?,
                button: b,
            }
        }
        "scroll" => {
            only(2)?;
            let dy = need(1, "a distance DY")?;
            StepKind::Scroll {
                dy: dy
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| format!("`{dy}` is not a distance"))?,
            }
        }
        "swipe" => {
            let usage = "`swipe DY over MS`";
            let (dy, ms) = match (arg(1), arg(2), arg(3)) {
                (Some(dy), Some("over"), Some(ms)) => (dy, ms),
                _ => return Err(format!("`swipe` takes {usage}")),
            };
            only(4)?;
            StepKind::Swipe {
                dy: dy
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| format!("`{dy}` is not a distance"))?,
                ms: ms
                    .strip_suffix("ms")
                    .unwrap_or(ms)
                    .parse::<u32>()
                    .ok()
                    .filter(|&m| m > 0)
                    .ok_or_else(|| format!("`{ms}` is not milliseconds"))?,
            }
        }
        "leave" => {
            only(1)?;
            StepKind::Leave
        }
        "text" => match arg(1) {
            Some("hold") => {
                let (filter, _) = text_rule(&w, 2, false)?;
                StepKind::TextHold { filter }
            }
            Some("release") => {
                let (filter, via) = text_rule(&w, 2, true)?;
                StepKind::TextRelease { filter, via }
            }
            Some("late") => {
                let n = count(need(2, "a number of frames")?, "frames")?;
                let (filter, via) = text_rule(&w, 3, true)?;
                StepKind::TextLate { n, filter, via }
            }
            _ => return Err("`text hold`, `text release` or `text late N`".into()),
        },
        "drop" => {
            let n = count(need(1, "a number of frames")?, "frames")?;
            let surface = match arg(2) {
                Some(s) => Some(selector(s.strip_prefix("surface=").ok_or_else(|| {
                    format!("unexpected `{s}` (`drop N [surface=<selector>]`)")
                })?)?),
                None => None,
            };
            only(3)?;
            StepKind::Drop { n, surface }
        }
        "jitter" => {
            only(2)?;
            let list = need(1, "per-tick period factors (`1,1,2,1`)")?;
            let f = list
                .split(',')
                .map(|t| {
                    t.trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| *v > 0.0 && v.is_finite())
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| format!("`{list}`: factors are numbers above 0"))?;
            StepKind::Jitter(f)
        }
        "reduce-motion" => {
            only(2)?;
            StepKind::ReduceMotion(match need(1, "on or off")? {
                "on" => true,
                "off" => false,
                o => return Err(format!("`reduce-motion {o}`: on or off")),
            })
        }
        "scale" => {
            only(2)?;
            let s = need(1, "a scale")?;
            StepKind::Scale(
                s.parse::<f64>()
                    .ok()
                    .filter(|v| *v > 0.0 && v.is_finite())
                    .ok_or_else(|| format!("`{s}` is not a scale"))?,
            )
        }
        "hz" => {
            only(2)?;
            let n = count(need(1, "a refresh rate")?, "hertz")?;
            if n == 0 {
                return Err("`hz 0`: a refresh rate is above 0".into());
            }
            StepKind::Hz(n)
        }
        "backend" => {
            only(2)?;
            StepKind::Backend(BackendMode::parse(need(1, "cpu, gpu or both")?)?)
        }
        "mark" => StepKind::Mark(need(1, "a label")?.to_string() + &tail(&w, 2)),
        "shot" => StepKind::Shot(need(1, "a label")?.to_string() + &tail(&w, 2)),
        "log" => StepKind::Log(rest_of("log")),
        "intend" => {
            let sel = selector(need(1, "`<selector> moves|enters|exits|changes [axes]`")?)?;
            let what = match need(2, "`<selector> moves|enters|exits|changes [axes]`")? {
                "moves" => Intention::Moves,
                "enters" => Intention::Enters,
                "exits" => Intention::Exits,
                "changes" => Intention::Changes,
                o => {
                    return Err(format!("`intend … {o}`: moves, enters, exits or changes"));
                }
            };
            let axes = match axes(&w[3..])? {
                Axes::NONE => Axes::ALL,
                a => a,
            };
            StepKind::Intend { sel, what, axes }
        }
        "expect" => {
            let usage = "`still|visible|hidden|moves <selector> [axis] [from +K]`";
            let what = match need(1, usage)? {
                "still" => ExpectKind::Still,
                "visible" => ExpectKind::Visible,
                "hidden" => ExpectKind::Hidden,
                "moves" => ExpectKind::Moves,
                o => return Err(format!("`expect {o}`: {usage}")),
            };
            let sel = selector(need(2, usage)?)?;
            let mut i = 3;
            let mut axis = None;
            if let Some(a) = arg(3).and_then(Axis::parse) {
                axis = Some(a);
                i = 4;
            }
            let mut from = 0;
            match (arg(i), arg(i + 1)) {
                (None, _) => {}
                (Some("from"), Some(k)) => {
                    from = k
                        .strip_prefix('+')
                        .and_then(|k| k.parse::<u32>().ok())
                        .ok_or_else(|| format!("`from {k}`: write `from +K`, K frames"))?;
                    i += 2;
                }
                (Some(o), _) => return Err(format!("unexpected `{o}`: {usage}")),
            }
            only(i)?;
            if axis.is_some() && matches!(what, ExpectKind::Visible | ExpectKind::Hidden) {
                return Err(format!("`expect {}` takes no axis", need(1, usage)?));
            }
            StepKind::Expect {
                what,
                sel,
                axis,
                from,
            }
        }
        "frames" => {
            only(2)?;
            StepKind::Frames(count(need(1, "a number of frames")?, "frames")?)
        }
        "wait" => {
            only(2)?;
            let ms = need(1, "milliseconds")?;
            StepKind::Wait(count(ms.strip_suffix("ms").unwrap_or(ms), "milliseconds")?)
        }
        "rest" => match (arg(1), arg(2)) {
            (None, _) => StepKind::Rest { max: REST_MAX },
            (Some("max"), Some(n)) => {
                only(3)?;
                StepKind::Rest {
                    max: count(n, "frames")?,
                }
            }
            _ => return Err("`rest [max N]`".into()),
        },
        "glide" => {
            let (from, to, ms, on) = path(&w)?;
            StepKind::Glide { from, to, ms, on }
        }
        "drag" => {
            let (from, to, ms, on) = path(&w)?;
            StepKind::Drag { from, to, ms, on }
        }
        o => return Err(format!("`{o}` is not a step")),
    };
    Ok(Some(k))
}

/// The words from `i` on, each after a space.
fn tail(w: &[Word], i: usize) -> String {
    w.get(i..)
        .unwrap_or_default()
        .iter()
        .map(|w| format!(" {}", w.text))
        .collect()
}

/// A step that did not parse: its line (in the scenario file) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepError {
    pub line: u32,
    pub msg: String,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.line, self.msg)
    }
}

/// Parses the steps section `text`, whose first line is line `first` of
/// the file.
pub fn parse(text: &str, first: u32) -> Result<Vec<Step>, StepError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = first + i as u32;
        let t = uncommented(raw).trim();
        let kind = parse_line(t).map_err(|msg| StepError { line, msg })?;
        if let Some(kind) = kind {
            out.push(Step {
                line,
                text: t.to_string(),
                kind,
            });
        }
    }
    Ok(out)
}
