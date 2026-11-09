//! (lockmon) Declared actions' runs: `rec.running`, `rec.started`,
//! `rec.error`, (proc-run) `rec.code`, `rec.lines` and `rec.stop()` for
//! `action rec(…) […]` (spec.md 5.7; decisions.md lockmon, action-error,
//! proc-run).
//!
//! The host runs the commands, so it says how each action's stand: it
//! passes on, by the action's name, whether a command of it runs, when
//! the one running longest started, and what the last one printed and
//! how it ended ([`Instance::set_action_run`],
//! [`Instance::update_action_run`]). Each action has its signals, made
//! when the program declares it and kept by name across reloads (a
//! reload never stops a command), owned by no scope. A read of any field
//! reads the action's run whole, an `ActionRun` record, tracked on all of
//! them.
//!
//! [`Instance::set_action_run`]: crate::instantiate::Instance::set_action_run
//! [`Instance::update_action_run`]: crate::instantiate::Instance::update_action_run

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use chrono::{NaiveDateTime, Timelike as _};
use strand_core::{Error, Runtime, Signal};

use super::Value;
use crate::lower::Program;
use crate::ty::RecordId;

/// (proc-run) A line a run printed, as the host read it: a `Line`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunLine {
    /// Its number in the run, from 1.
    pub n: u64,
    /// As a terminal shows it.
    pub text: String,
    /// It came on stderr.
    pub stderr: bool,
    /// No line break yet: it may still change.
    pub open: bool,
}

/// (proc-run) What changed in an action's run since the host last said,
/// applied in one flush in this order (spec.md 5.7's end order): `reset`,
/// then `keep_from` and `lines`, then `code`, then `error`, then
/// `running`. A field left at its default leaves that part as it was.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunUpdate {
    /// A run started (a queued re-run included): `lines` emptied, `code`
    /// and `error` null, before the rest applies.
    pub reset: bool,
    /// Lines numbered below this have left the front of `lines` (the
    /// host's ring dropped them); 0 drops none.
    pub keep_from: u64,
    /// New lines in order, and changed ones: a line whose `n` is kept
    /// replaces it (the open line redrawn, or ended), any other is added
    /// after the last.
    pub lines: Vec<RunLine>,
    /// The status its command ended with, once it ends.
    pub code: Option<i64>,
    /// Its `error`: `Some(None)` clears it, `Some(Some(why))` sets it.
    pub error: Option<Option<String>>,
    /// Whether a command of it runs, and when the one running longest
    /// started ([`ActionRuns::set`]).
    pub running: Option<(bool, Option<NaiveDateTime>)>,
}

/// One action's run, as the host last said.
#[derive(Copy, Clone, Debug)]
struct Run {
    running: Signal<bool>,
    /// To the minute, as a `Date` holds it.
    started: Signal<Option<NaiveDateTime>>,
    /// Why its last command failed.
    error: Signal<Option<String>>,
    /// (proc-run) The status its last command ended with.
    code: Signal<Option<i64>>,
    /// (proc-run) Bumped when its `lines` change: what a read tracks, the
    /// lines themselves kept in [`ActionRuns::lines`].
    lines: Signal<u64>,
}

/// (proc-run) An action's `lines`, and the list value last built from
/// them (with the `Line` record it was built for).
#[derive(Default, Debug)]
struct Lines {
    kept: VecDeque<RunLine>,
    built: Option<(RecordId, Value)>,
}

impl Lines {
    fn apply(&mut self, keep_from: u64, lines: Vec<RunLine>) {
        while self.kept.front().is_some_and(|l| l.n < keep_from) {
            self.kept.pop_front();
        }
        for line in lines {
            if line.n < keep_from {
                continue;
            }
            // A changed line is the open one, the last, nearly always.
            if let Some(kept) = self.kept.iter_mut().rev().find(|l| l.n == line.n) {
                *kept = line;
            } else if self.kept.back().is_none_or(|l| l.n < line.n) {
                self.kept.push_back(line);
            }
            // Otherwise older than the last and not kept: gone already.
        }
        self.built = None;
    }

    /// The lines as a list of `record` (a `Line`), built once per change.
    fn value(&mut self, record: RecordId) -> Value {
        if let Some((r, v)) = &self.built
            && *r == record
        {
            return v.clone();
        }
        let v = Value::list(
            self.kept
                .iter()
                .map(|l| {
                    Value::record(
                        record,
                        vec![
                            Value::int(i64::try_from(l.n).unwrap_or(i64::MAX)),
                            Value::text(l.text.as_str()),
                            Value::Bool(l.stderr),
                            Value::Bool(l.open),
                        ],
                    )
                })
                .collect(),
        );
        self.built = Some((record, v.clone()));
        v
    }
}

/// Every declared action's run, by name: the instance's, kept across
/// reloads.
#[derive(Default, Debug)]
pub struct ActionRuns {
    runs: RefCell<HashMap<String, Run>>,
    /// (proc-run) Each action's `lines`, by name.
    lines: RefCell<HashMap<String, Lines>>,
}

impl ActionRuns {
    /// Gives every action `prog` declares with a command its signals (not
    /// running), keeping those of actions already known.
    pub(crate) fn declare(&self, rt: &Runtime, prog: &Program) {
        let mut runs = self.runs.borrow_mut();
        for a in prog.execs.values() {
            if runs.contains_key(&a.name) {
                continue;
            }
            let running = rt.signal(false);
            let started = rt.signal(None);
            let error = rt.signal(None);
            let code = rt.signal(None);
            let lines = rt.signal(0u64);
            // Owned by no scope: they outlive whatever mounts first.
            let _ = rt.reparent(running.id(), None);
            let _ = rt.reparent(started.id(), None);
            let _ = rt.reparent(error.id(), None);
            let _ = rt.reparent(code.id(), None);
            let _ = rt.reparent(lines.id(), None);
            rt.set_name(running.id(), format!("{}.running", a.name));
            rt.set_name(started.id(), format!("{}.started", a.name));
            rt.set_name(error.id(), format!("{}.error", a.name));
            rt.set_name(code.id(), format!("{}.code", a.name));
            rt.set_name(lines.id(), format!("{}.lines", a.name));
            runs.insert(
                a.name.clone(),
                Run {
                    running,
                    started,
                    error,
                    code,
                    lines,
                },
            );
        }
    }

    /// The host's word on action `action`: whether a command of it runs,
    /// and when the one running longest started. An action the program
    /// does not declare is ignored.
    pub(crate) fn set(
        &self,
        rt: &Runtime,
        action: &str,
        running: bool,
        started: Option<NaiveDateTime>,
    ) {
        let Some(run) = self.runs.borrow().get(action).copied() else {
            return;
        };
        let started = started
            .filter(|_| running)
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0));
        let _ = run.running.set(rt, running);
        let _ = run.started.set(rt, started);
    }

    /// The host's word on why action `action`'s last command failed. An
    /// action the program does not declare is ignored.
    pub(crate) fn set_error(&self, rt: &Runtime, action: &str, error: Option<&str>) {
        let Some(run) = self.runs.borrow().get(action).copied() else {
            return;
        };
        let _ = run.error.set(rt, error.map(str::to_owned));
    }

    /// (proc-run) The host's word on action `action`'s run, applied in
    /// [`RunUpdate`]'s order. An action the program does not declare is
    /// ignored.
    pub(crate) fn update(&self, rt: &Runtime, action: &str, u: RunUpdate) {
        let Some(run) = self.runs.borrow().get(action).copied() else {
            return;
        };
        let changed = u.reset || u.keep_from > 0 || !u.lines.is_empty();
        if changed {
            let mut all = self.lines.borrow_mut();
            let lines = all.entry(action.to_owned()).or_default();
            if u.reset {
                lines.kept.clear();
            }
            lines.apply(u.keep_from, u.lines);
            drop(all);
            let next = run.lines.get_untracked(rt).unwrap_or(0).wrapping_add(1);
            let _ = run.lines.set(rt, next);
        }
        if u.reset {
            let _ = run.code.set(rt, None);
            let _ = run.error.set(rt, None);
        }
        if let Some(code) = u.code {
            let _ = run.code.set(rt, Some(code));
        }
        if let Some(error) = u.error {
            let _ = run.error.set(rt, error);
        }
        if let Some((running, started)) = u.running {
            self.set(rt, action, running, started);
        }
    }

    /// Whether a command of `action` runs, as last set (tests, hosts).
    pub(crate) fn running(&self, rt: &Runtime, action: &str) -> Option<bool> {
        let run = self.runs.borrow().get(action).copied()?;
        run.running.get_untracked(rt).ok()
    }

    /// `rec`'s run as an `ActionRun` record (`record`), tracked; its
    /// `lines` are `line`s (the `Line` record).
    pub(crate) fn read(
        &self,
        rt: &Runtime,
        action: &str,
        record: RecordId,
        date: Option<RecordId>,
        line: Option<RecordId>,
    ) -> Result<Value, Error> {
        let Some(run) = self.runs.borrow().get(action).copied() else {
            return Ok(Value::record(
                record,
                vec![
                    Value::Bool(false),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::list(Vec::new()),
                ],
            ));
        };
        let running = run.running.get(rt)?;
        let started = match (run.started.get(rt)?, date) {
            (Some(t), Some(d)) => super::clock::datetime_record(d, t),
            _ => Value::Null,
        };
        let error = run.error.get(rt)?.map_or(Value::Null, Value::text);
        let code = run.code.get(rt)?.map_or(Value::Null, Value::int);
        run.lines.get(rt)?;
        let lines = match line {
            Some(line) => self
                .lines
                .borrow_mut()
                .get_mut(action)
                .map_or_else(|| Value::list(Vec::new()), |l| l.value(line)),
            None => Value::list(Vec::new()),
        };
        Ok(Value::record(
            record,
            vec![Value::Bool(running), started, error, code, lines],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{Lines, RunLine};

    fn line(n: u64, text: &str, open: bool) -> RunLine {
        RunLine {
            n,
            text: text.into(),
            stderr: false,
            open,
        }
    }

    fn texts(l: &Lines) -> Vec<(u64, &str, bool)> {
        l.kept
            .iter()
            .map(|l| (l.n, l.text.as_str(), l.open))
            .collect()
    }

    /// (proc-run) A line of a kept number replaces it (the open line
    /// redrawn, then ended), a newer one is added, lines below
    /// `keep_from` leave the front, and an older line not kept is gone.
    #[test]
    fn lines_apply_by_number() {
        let mut l = Lines::default();
        l.apply(0, vec![line(1, "a", false), line(2, " 10%", true)]);
        l.apply(0, vec![line(2, " 50%", true)]);
        assert_eq!(texts(&l), [(1, "a", false), (2, " 50%", true)]);
        l.apply(0, vec![line(2, "100%", false), line(3, "b", false)]);
        assert_eq!(
            texts(&l),
            [(1, "a", false), (2, "100%", false), (3, "b", false)]
        );
        l.apply(3, vec![line(2, "late", false), line(4, "c", true)]);
        assert_eq!(texts(&l), [(3, "b", false), (4, "c", true)]);
    }
}
