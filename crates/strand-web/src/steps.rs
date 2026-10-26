//! A scenario's steps on the web host (docs/live-examples.md): the part
//! of eval's step language (docs/eval.md, "Scenario format") the docs
//! examples are written in, run one tick at a time, so a page can play
//! them at the screen's rate and a test can run them to the end, each as
//! eval's driver runs it.
//!
//! - `rest [max N]`: ticks until the shell is at rest after one, at most
//!   N (600 when not given);
//! - `frames N`: N ticks; `wait MS` (or `MSms`): the ticks MS takes at
//!   the scenario's rate, rounded up, at least one;
//! - `set <path> <value>`: at the tick about to run, as [`Shell::set`]
//!   reads it (a single quoted word is its text);
//! - `mark <label>`: nothing to run.
//!
//! Any other step of eval's (`glide`, `press`, `expect`, …) is refused
//! with its name, rather than skipped: a run that left one out would not
//! be the scenario's.

use crate::shell::Shell;

/// `rest`'s most ticks when it names none (eval's `REST_MAX`).
pub const REST_MAX: u32 = 600;

/// One step.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Rest { max: u32 },
    Frames(u32),
    Wait(u32),
    Set { path: String, value: String },
    Mark(String),
}

impl Step {
    /// Its kind's word, as written.
    pub fn kind(&self) -> &'static str {
        match self {
            Step::Rest { .. } => "rest",
            Step::Frames(_) => "frames",
            Step::Wait(_) => "wait",
            Step::Set { .. } => "set",
            Step::Mark(_) => "mark",
        }
    }

    /// Whether it acts on the shell (a `set`), rather than letting time
    /// pass or naming a moment.
    pub fn acts(&self) -> bool {
        matches!(self, Step::Set { .. })
    }
}

/// A step as written in the scenario, with its line in the steps
/// section (from 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub line: u32,
    pub text: String,
    pub step: Step,
}

/// The line without its comment (`# ` after a space or at its start,
/// outside quotes), trimmed.
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
                return line[..i].trim();
            }
            _ => {}
        }
    }
    line.trim()
}

fn count(s: &str, what: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|_| format!("`{s}` is not a number of {what}"))
}

/// `"…"` with `\"` and `\\` read, when `s` is one quoted word.
fn unquoted(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => out.push(it.next()?),
            '"' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// One line of the steps section: `None` for a blank or comment line.
pub fn parse_line(raw: &str) -> Result<Option<Step>, String> {
    let t = uncommented(raw);
    if t.is_empty() {
        return Ok(None);
    }
    let (verb, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    let rest = rest.trim();
    let words: Vec<&str> = rest.split_whitespace().collect();
    let step = match verb {
        "rest" => match words.as_slice() {
            [] => Step::Rest { max: REST_MAX },
            ["max", n] => Step::Rest {
                max: count(n, "frames")?,
            },
            _ => return Err("`rest [max N]`".into()),
        },
        "frames" => match words.as_slice() {
            [n] => Step::Frames(count(n, "frames")?),
            _ => return Err("`frames N`".into()),
        },
        "wait" => match words.as_slice() {
            [ms] => Step::Wait(count(ms.strip_suffix("ms").unwrap_or(ms), "milliseconds")?),
            _ => return Err("`wait MS`".into()),
        },
        "set" => {
            let (path, value) = rest
                .split_once(char::is_whitespace)
                .ok_or("`set <path> <value>`")?;
            let value = value.trim();
            Step::Set {
                path: path.to_string(),
                value: unquoted(value).unwrap_or_else(|| value.to_string()),
            }
        }
        "mark" => match rest {
            "" => return Err("`mark <label>`".into()),
            label => Step::Mark(label.to_string()),
        },
        other => {
            return Err(format!(
                "`{other}` is a step the web host does not run (it runs rest, frames, wait, set and mark)"
            ));
        }
    };
    Ok(Some(step))
}

/// The steps section `text`: each step with its line, or the first that
/// does not parse, as `line N: why`.
pub fn parse(text: &str) -> Result<Vec<Line>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i as u32 + 1;
        if let Some(step) = parse_line(raw).map_err(|e| format!("line {line}: {e}"))? {
            out.push(Line {
                line,
                text: uncommented(raw).to_string(),
                step,
            });
        }
    }
    Ok(out)
}

/// The ticks `ms` takes at `hz`, rounded up, at least one (eval's
/// `Clock::ticks_for`).
pub fn ticks_for(ms: u32, hz: u32) -> u32 {
    let period = 1_000_000_000 / u128::from(hz.max(1));
    (u128::from(ms) * 1_000_000).div_ceil(period).max(1) as u32
}

/// What a time step has left.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    Ticks { n: u32, done: u32 },
    Rest { max: u32, done: u32 },
}

/// Steps played on a shell, one tick at a time.
#[derive(Clone, Debug)]
pub struct Player {
    lines: Vec<Line>,
    /// The next step to begin.
    at: usize,
    /// The time step running, and its line's index.
    run: Option<(usize, Plan)>,
}

/// What a call to [`Player::advance`] did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Advance {
    /// Ran one tick, in the step at this index.
    Ticked(usize),
    /// Every step has run; no tick ran.
    Done,
}

impl Player {
    pub fn new(lines: Vec<Line>) -> Player {
        Player {
            lines,
            at: 0,
            run: None,
        }
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Whether every step has run.
    pub fn done(&self) -> bool {
        self.run.is_none() && self.at >= self.lines.len()
    }

    /// The step running or about to begin, by index.
    pub fn current(&self) -> Option<usize> {
        self.run
            .map(|(i, _)| i)
            .or((self.at < self.lines.len()).then_some(self.at))
    }

    /// One tick of the scenario: the instant steps due before it run at
    /// the tick about to run, then the time step they lead to ticks once
    /// (eval's `begin_step` and `advance`). A `rest` ends at the first
    /// tick after which the shell is at rest, or at its max.
    pub fn advance(&mut self, shell: &mut Shell) -> Result<Advance, String> {
        loop {
            if let Some((i, plan)) = self.run.as_mut() {
                let i = *i;
                match plan {
                    Plan::Ticks { n, done } => {
                        if *done >= *n {
                            self.run = None;
                            continue;
                        }
                        shell.tick()?;
                        *done += 1;
                        if *done >= *n {
                            self.run = None;
                        }
                    }
                    Plan::Rest { max, done } => {
                        if *done >= *max {
                            self.run = None;
                            continue;
                        }
                        shell.tick()?;
                        *done += 1;
                        if shell.at_rest() || *done >= *max {
                            self.run = None;
                        }
                    }
                }
                return Ok(Advance::Ticked(i));
            }
            let Some(line) = self.lines.get(self.at) else {
                return Ok(Advance::Done);
            };
            let i = self.at;
            self.at += 1;
            let at = |e: String| format!("step {} (`{}`): {e}", line.line, line.text);
            match &line.step {
                Step::Set { path, value } => shell.set(path, value).map_err(at)?,
                Step::Mark(_) => {}
                Step::Frames(n) => {
                    self.run = Some((i, Plan::Ticks { n: *n, done: 0 }));
                }
                Step::Wait(ms) => {
                    let n = ticks_for(*ms, shell.hz());
                    self.run = Some((i, Plan::Ticks { n, done: 0 }));
                }
                Step::Rest { max } => {
                    self.run = Some((i, Plan::Rest { max: *max, done: 0 }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_steps_the_docs_examples_use_parse_as_eval_reads_them() {
        let lines = parse(
            "rest\nmark volume-up  # a comment\n\nset audio.sink.volume 0.45\nframes 6\nwait 1000\nwait 250ms\nrest max 12\nset look \"a \\\"b\\\"\"\nset n [{\"id\": 1}]\n",
        )
        .unwrap();
        let steps: Vec<_> = lines.iter().map(|l| l.step.clone()).collect();
        assert_eq!(
            steps,
            vec![
                Step::Rest { max: REST_MAX },
                Step::Mark("volume-up".into()),
                Step::Set {
                    path: "audio.sink.volume".into(),
                    value: "0.45".into()
                },
                Step::Frames(6),
                Step::Wait(1000),
                Step::Wait(250),
                Step::Rest { max: 12 },
                Step::Set {
                    path: "look".into(),
                    value: "a \"b\"".into()
                },
                Step::Set {
                    path: "n".into(),
                    value: "[{\"id\": 1}]".into()
                },
            ]
        );
        assert_eq!(lines[1].text, "mark volume-up");
        assert_eq!(lines[2].line, 4);
    }

    #[test]
    fn a_step_the_web_does_not_run_is_refused_by_name() {
        let e = parse("rest\nglide 0,0 -> 10,10 200ms\n").unwrap_err();
        assert!(
            e.starts_with("line 2: `glide` is a step the web host does not run"),
            "{e}"
        );
        assert!(parse("frames x").is_err());
        assert!(parse("rest 5").is_err());
        assert!(parse("set onlypath").is_err());
    }

    #[test]
    fn a_wait_takes_the_ticks_eval_gives_it() {
        // A tick is 16 666 666 ns at 60 Hz, so a second is a little over
        // 60 of them: 61, as in eval.
        assert_eq!(ticks_for(1000, 60), 61);
        assert_eq!(ticks_for(1, 60), 1);
        assert_eq!(ticks_for(0, 60), 1);
        assert_eq!(ticks_for(17, 60), 2);
    }
}
