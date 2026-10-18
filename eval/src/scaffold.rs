//! `strand-eval new` (docs/eval.md, "Scaffolding"): a scenario skeleton
//! in `eval/scenarios/local/`, its config named or a bar to write into,
//! and, from a shots-keys steps file, its steps mapped one to one onto
//! the step vocabulary:
//!
//! - `ipc set <path> <value>` → `set <path> <value>`;
//! - `pause <ms>` (`250`, `250ms`, `1.5s`) → `wait <ms>`;
//! - `burst <n>` → `frames <n>`;
//! - `pointer` with a point (`X,Y` or `X Y`) → `click X,Y [button]` when
//!   it clicks, `glide <last> -> X,Y over MS` when it moves over a time
//!   from a known place, else `pointer X,Y`;
//! - `notify …` → `notify …`.
//!
//! A line it does not understand, or one whose mapping the step parser
//! refuses, is kept as a `# shots-keys:` comment: the skeleton parses,
//! and nothing of the original is lost. A mapping that drops words keeps
//! the line too, as a comment above it.

use std::path::PathBuf;

use crate::scenario::{Place, Scenario};
use crate::{Error, report, steps};

/// What to scaffold.
#[derive(Debug, Default)]
pub struct Ask {
    /// The scenario's id (and file name).
    pub id: String,
    /// A config to name (`config = …`) instead of writing one in.
    pub config: Option<String>,
    /// A shots-keys steps file to map.
    pub from_steps: Option<PathBuf>,
    /// Where it goes (default eval/scenarios/local).
    pub to: Option<PathBuf>,
}

/// The comment a line kept as it was starts with.
pub const KEPT: &str = "# shots-keys:";

/// A number of milliseconds: `250`, `250ms`, `1.5s`.
fn millis(s: &str) -> Option<u32> {
    let (num, scale) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num.parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * scale).round() as u32)
}

/// A whole number.
fn whole(s: &str) -> Option<u32> {
    s.parse().ok()
}

/// A coordinate: a finite number.
fn coord(s: &str) -> Option<f32> {
    s.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// A point in `words`: an `X,Y` word, or two numbers in a row. Returns
/// it and the indices of the words it took.
fn point_in(words: &[&str]) -> Option<([f32; 2], Vec<usize>)> {
    for (i, w) in words.iter().enumerate() {
        if let Some((x, y)) = w.split_once(',')
            && let (Some(x), Some(y)) = (coord(x), coord(y))
        {
            return Some(([x, y], vec![i]));
        }
        if let (Some(x), Some(y)) = (coord(w), words.get(i + 1).and_then(|n| coord(n))) {
            return Some(([x, y], vec![i, i + 1]));
        }
    }
    None
}

fn pt(p: [f32; 2]) -> String {
    format!("{},{}", p[0], p[1])
}

/// Maps one `pointer …` line's words (after `pointer`); `last` is where
/// the pointer was, and becomes where it is.
fn pointer(words: &[&str], last: &mut Option<[f32; 2]>) -> Option<(String, bool)> {
    let (at, took) = point_in(words)?;
    let mut used = took.clone();
    let clicks = words
        .iter()
        .position(|w| matches!(*w, "click" | "tap" | "press"));
    let button = words
        .iter()
        .position(|w| matches!(*w, "left" | "right" | "middle"));
    let over = words.iter().enumerate().find_map(|(i, w)| {
        if *w == "over" {
            Some((millis(words.get(i + 1)?)?, vec![i, i + 1]))
        } else if w.ends_with("ms") || (w.ends_with('s') && w.len() > 1) {
            Some((millis(w)?, vec![i]))
        } else {
            None
        }
    });
    let step = if let Some(c) = clicks {
        used.push(c);
        let b = match button {
            Some(b) => {
                used.push(b);
                format!(" {}", words[b])
            }
            None => String::new(),
        };
        format!("click {}{b}", pt(at))
    } else if let (Some((ms, ws)), Some(from)) = (&over, *last) {
        used.extend(ws);
        format!("glide {} -> {} over {ms}", pt(from), pt(at))
    } else {
        if let Some(m) = words.iter().position(|w| *w == "move") {
            used.push(m);
        }
        format!("pointer {}", pt(at))
    };
    *last = Some(at);
    let lossy = (0..words.len()).any(|i| !used.contains(&i));
    Some((step, lossy))
}

/// Maps a shots-keys steps file's text onto steps, one line per line
/// (a lossy mapping adds the original as a comment above it).
pub fn map_steps(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last: Option<[f32; 2]> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        if let Some(c) = line.strip_prefix('#') {
            out.push(format!("# {}", c.trim_start()).trim_end().to_string());
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let rest_after = |n: usize| -> &str {
            // The text after the first `n` words, as written.
            let mut s = line;
            for _ in 0..n {
                s = s.trim_start();
                s = s.find(char::is_whitespace).map_or("", |i| &s[i..]);
            }
            s.trim()
        };
        let mapped: Option<(String, bool)> = match words.as_slice() {
            ["ipc", "set", _, _, ..] => Some((format!("set {}", rest_after(2)), false)),
            ["pause", ms] => millis(ms).map(|ms| (format!("wait {ms}"), false)),
            ["pause", ms, ..] => millis(ms).map(|ms| (format!("wait {ms}"), true)),
            ["burst", n] => whole(n).map(|n| (format!("frames {n}"), false)),
            ["burst", n, ..] => whole(n).map(|n| (format!("frames {n}"), true)),
            ["pointer", rest @ ..] => pointer(rest, &mut last),
            ["notify", _, ..] => Some((format!("notify {}", rest_after(1)), false)),
            _ => None,
        };
        match mapped {
            Some((step, lossy)) => match steps::parse(&step, 1) {
                Ok(_) => {
                    if lossy {
                        out.push(format!("{KEPT} {line}"));
                    }
                    out.push(step);
                }
                Err(e) => out.push(format!("{KEPT} {line}  (as `{step}`: {})", e.msg)),
            },
            None => out.push(format!("{KEPT} {line}")),
        }
    }
    out
}

/// The scenario's text.
pub fn skeleton(ask: &Ask, steps_text: Option<&str>) -> String {
    let source = ask
        .from_steps
        .as_ref()
        .map(|p| format!(" from {}", p.display()))
        .unwrap_or_default();
    let mut s = format!(
        "id     = \"{}\"\nabout  = \"\"\"Scaffolded by `strand-eval new`{source}. Say what it \
         holds and what a correct shell does.\"\"\"\n",
        ask.id
    );
    if let Some(c) = &ask.config {
        s += &format!("config = \"{c}\"\n");
    }
    s.push('\n');
    if ask.config.is_none() {
        s += "=== shell.strand\nbar Top {\n  height: 30\n}\n\n";
    }
    s += "=== steps\nrest\n";
    if let Some(t) = steps_text {
        let mapped = map_steps(t);
        for l in &mapped {
            s += l;
            s.push('\n');
        }
        let ends_at_rest = mapped
            .iter()
            .rev()
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .is_some_and(|l| l.starts_with("rest"));
        if !ends_at_rest {
            s += "rest\n";
        }
    }
    s
}

/// Writes the skeleton; returns its path. It never replaces a file.
pub fn scaffold(ask: &Ask) -> Result<PathBuf, Error> {
    let ok = !ask.id.is_empty()
        && ask
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !ask.id.starts_with('.');
    if !ok {
        return Err(Error::Usage(format!(
            "new: `{}` is not an id (letters, digits, `-`, `_`, `.`)",
            ask.id
        )));
    }
    let steps_text = match &ask.from_steps {
        Some(p) => {
            Some(std::fs::read_to_string(p).map_err(|e| Error::Io(p.display().to_string(), e))?)
        }
        None => None,
    };
    let text = skeleton(ask, steps_text.as_deref());
    let dir = ask
        .to
        .clone()
        .unwrap_or_else(|| crate::scenario::eval_root().join("scenarios").join("local"));
    let path = dir.join(format!("{}.scn", ask.id));
    if path.exists() {
        return Err(Error::Usage(format!(
            "{} exists: new never replaces a scenario",
            path.display()
        )));
    }
    // A config is read when the scenario runs, not here: only the steps
    // and the header must parse.
    if ask.config.is_none() {
        Scenario::parse_in(&text, &path, Place::Local)?;
    } else if let Some(at) = text.find("=== steps\n") {
        steps::parse(&text[at + "=== steps\n".len()..], 1)
            .map_err(|e| Error::Usage(format!("new: {e}")))?;
    }
    report::write(&path, text.as_bytes())?;
    Ok(path)
}
