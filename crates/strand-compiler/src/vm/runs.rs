//! (lockmon) Declared actions' runs: `rec.running`, `rec.started` and
//! `rec.stop()` for `action rec(…) […]` (spec.md 5.7; decisions.md
//! lockmon).
//!
//! The host runs the commands, so it says how each action's stand: it
//! passes on, by the action's name, whether a command of it runs and
//! when the one running longest started ([`Instance::set_action_run`]).
//! Each action has two signals, made when the program declares it and
//! kept by name across reloads (a reload never stops a command), owned by
//! no scope. A read of `rec.running` or `rec.started` reads the action's
//! run whole, an `ActionRun` record, tracked on both.
//!
//! [`Instance::set_action_run`]: crate::instantiate::Instance::set_action_run

use std::cell::RefCell;
use std::collections::HashMap;

use chrono::{NaiveDateTime, Timelike as _};
use strand_core::{Error, Runtime, Signal};

use super::Value;
use crate::lower::Program;

/// One action's run, as the host last said.
#[derive(Copy, Clone, Debug)]
struct Run {
    running: Signal<bool>,
    /// To the minute, as a `Date` holds it.
    started: Signal<Option<NaiveDateTime>>,
}

/// Every declared action's run, by name: the instance's, kept across
/// reloads.
#[derive(Default, Debug)]
pub struct ActionRuns {
    runs: RefCell<HashMap<String, Run>>,
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
            // Owned by no scope: they outlive whatever mounts first.
            let _ = rt.reparent(running.id(), None);
            let _ = rt.reparent(started.id(), None);
            rt.set_name(running.id(), format!("{}.running", a.name));
            rt.set_name(started.id(), format!("{}.started", a.name));
            runs.insert(a.name.clone(), Run { running, started });
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

    /// Whether a command of `action` runs, as last set (tests, hosts).
    pub(crate) fn running(&self, rt: &Runtime, action: &str) -> Option<bool> {
        let run = self.runs.borrow().get(action).copied()?;
        run.running.get_untracked(rt).ok()
    }

    /// `rec`'s run as an `ActionRun` record (`record`), tracked.
    pub(crate) fn read(
        &self,
        rt: &Runtime,
        action: &str,
        record: crate::ty::RecordId,
        date: Option<crate::ty::RecordId>,
    ) -> Result<Value, Error> {
        let Some(run) = self.runs.borrow().get(action).copied() else {
            return Ok(Value::record(record, vec![Value::Bool(false), Value::Null]));
        };
        let running = run.running.get(rt)?;
        let started = match (run.started.get(rt)?, date) {
            (Some(t), Some(d)) => super::clock::datetime_record(d, t),
            _ => Value::Null,
        };
        Ok(Value::record(record, vec![Value::Bool(running), started]))
    }
}
