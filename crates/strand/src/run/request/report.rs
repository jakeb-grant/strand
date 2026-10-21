//! `strand report` (S-tools): compositor, protocols, scales, memory, GPU
//! and frame stats (`crate::report`). Logic asks main for its half
//! through the snapshot hop (`ToMain::Snapshot` with
//! `SnapshotKind::Report`), keeps the caller under the request's
//! `SnapshotId`, and answers once main's `ToLogic::Snapshot` comes back,
//! with its own half added. Main answers between frames and never waits
//! on logic. A socket request only, as `export` (architecture.md: the
//! D-Bus front end has no `Report`).
//!
//! (report) With words (`{"save": {"words": …}}`) it saves a glitch
//! report instead: refused while a lock is asked for or shown, main's
//! part asked with `SnapshotKind::Bundle`, logic's added here (its ring,
//! every service's and cell's value under the redaction rules, the
//! module set, the files the program reads), and the whole written by a
//! `strand-report` thread (`crate::report::bundle`), whose outcome
//! [`Shell::reports_written`] answers.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json, json};
use strand_core::ipc as proto;

use super::{Caller, Shell};
use crate::report::bundle::{self, Job, MainBundle};
use crate::run::record::ChangeRecorder;
use crate::run::{Snapshot, SnapshotId, SnapshotKind, ToMain};

/// The refusal while a lock is asked for or shown.
const LOCKED: &str = "the screen is locked";

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn report_request(&mut self, from: Caller, save: Option<String>) {
        if save.is_some() {
            if self.inst.lock_shown() {
                return self.answer(from, proto::refused(LOCKED));
            }
            if strand_core::persist::state_dir().is_none() {
                return self.answer(
                    from,
                    proto::refused("no state directory to save the report in (set HOME)"),
                );
            }
        }
        let reply = SnapshotId(self.next_snapshot);
        self.next_snapshot += 1;
        let kind = if save.is_some() {
            SnapshotKind::Bundle
        } else {
            SnapshotKind::Report
        };
        let asked = self
            .to_main
            .as_ref()
            .is_some_and(|tx| tx.send(ToMain::Snapshot { kind, reply }).is_ok());
        if !asked {
            return self.answer(from, proto::refused("no display thread to report on"));
        }
        self.reports.push((reply, from, save));
    }

    /// Main's answer to a snapshot: the report it is for, answered, or
    /// (report) its bundle handed to a writer.
    pub(in crate::run) fn snapshot_answered(&mut self, snap: Snapshot) {
        let Some(i) = self.reports.iter().position(|(id, ..)| *id == snap.reply) else {
            return;
        };
        let (reply, from, save) = self.reports.swap_remove(i);
        let report = crate::report::logic_part(snap.data);
        let Some(words) = save else {
            return self.answer(from, json!({"ok": true, "report": report}));
        };
        // A lock asked for while main answered: nothing is saved.
        if self.inst.lock_shown() {
            return self.answer(from, proto::refused(LOCKED));
        }
        let Some(state) = strand_core::persist::state_dir() else {
            return self.answer(from, proto::refused("no state directory"));
        };
        let main = snap.bundle.map(|b| *b).unwrap_or_default();
        let job = self.bundle_job(state.join("reports"), words, report, main);
        bundle::spawn(job, reply, self.written.clone(), self.report_ping.clone());
        self.saving.push((reply, from));
    }

    /// (report) The glitch reports written since the last look, answered.
    pub(in crate::run) fn reports_written(&mut self) {
        let done = std::mem::take(
            &mut *self
                .written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (id, outcome) in done {
            let Some(i) = self.saving.iter().position(|(s, _)| *s == id) else {
                continue;
            };
            let (_, from) = self.saving.swap_remove(i);
            match outcome {
                Ok(answer) => {
                    log::info!(
                        "saved a glitch report to {}",
                        answer["bundle"].as_str().unwrap_or("?")
                    );
                    self.answer(from, answer);
                }
                Err(why) => {
                    log::warn!("{why}");
                    self.answer(from, proto::refused(why));
                }
            }
        }
    }

    /// Logic's part of a glitch report, gathered now.
    fn bundle_job(&self, root: PathBuf, words: String, report: Json, main: MainBundle) -> Job {
        let rec = self.recorder.borrow();
        let rt = self.inst.runtime();
        // Every service but `auth`, under the redaction rules.
        let mut services = Map::new();
        if let Some(r) = &self.real {
            for s in r.services.snapshot(rt) {
                if s.name == "auth" {
                    continue;
                }
                // A declared service by its declared fields.
                let named = rec.declared_fields(&s.name, &s.fields).unwrap_or_else(|| {
                    s.fields
                        .iter()
                        .map(|(f, d)| (f.to_string(), d.clone()))
                        .collect()
                });
                let fields: Map<String, Json> = named
                    .iter()
                    .map(|(f, d)| {
                        let j = ChangeRecorder::service_json(&s.name, d);
                        (f.clone(), serde_json::from_str(&j).unwrap_or(Json::Null))
                    })
                    .collect();
                services.insert(
                    s.name.clone(),
                    json!({"running": s.running, "fields": fields}),
                );
            }
        }
        let cells: Map<String, Json> = self
            .inst
            .cells_snapshot()
            .into_iter()
            .map(|(p, v)| {
                let j = rec.cell_json(&p, &v);
                let v = serde_json::from_str(&j).unwrap_or(Json::Null);
                (p, v)
            })
            .collect();
        let config_dir = self.config_dir.clone();
        let config: Vec<(PathBuf, std::sync::Arc<str>)> = self
            .build
            .sources
            .iter()
            .map(|(_, f)| (relative(&f.name, config_dir.as_deref()), f.text.clone()))
            .collect();
        // The files the program reads: settings files and their
        // overlays, the theme's wallpapers and imports, the drawn assets,
        // then what the renderer holds decodes of.
        let mut files: Vec<PathBuf> = Vec::new();
        for (p, _) in &self.watched {
            files.push(p.clone());
            files.extend(self.inst.settings_overlay_paths(p));
        }
        files.extend(main.images.iter().cloned());
        let in_config = |p: &Path| {
            config_dir.as_deref().is_some_and(|d| {
                p.starts_with(d)
                    && (p.extension().is_some_and(|e| e == "strand")
                        || p.starts_with(d.join("icons")))
            })
        };
        files.retain(|p| !in_config(p));
        let now_ns = strand_core::flight::now_ns();
        let wall = chrono::Local::now().to_rfc3339();
        Job {
            root,
            words,
            report,
            main,
            changes: rec.dump(),
            services: Json::Object(services),
            cells: Json::Object(cells),
            config_dir,
            config,
            files,
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: env!("STRAND_COMMIT").to_string(),
            wall,
            now_ns,
            epoch_unix_ns: strand_core::flight::epoch_unix_ns(),
            replay: self.replay_on,
        }
    }
}

/// A module file's path under the config directory, from the name the
/// loader gave it (absolute under `dir`, or relative to it); its file
/// name otherwise.
fn relative(name: &str, dir: Option<&Path>) -> PathBuf {
    let p = Path::new(name);
    if p.is_relative() && !name.contains("..") {
        return p.to_path_buf();
    }
    if let Some(rel) = dir.and_then(|d| p.strip_prefix(d).ok()) {
        return rel.to_path_buf();
    }
    p.file_name()
        .map_or_else(|| PathBuf::from("module.strand"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_module_lands_under_config_at_its_own_path() {
        let d = Path::new("/home/u/.config/strand");
        assert_eq!(
            relative("/home/u/.config/strand/bar/clock.strand", Some(d)),
            PathBuf::from("bar/clock.strand")
        );
        assert_eq!(
            relative("shell.strand", Some(d)),
            PathBuf::from("shell.strand")
        );
        assert_eq!(
            relative("/elsewhere/x.strand", Some(d)),
            PathBuf::from("x.strand")
        );
        assert_eq!(relative("../x.strand", Some(d)), PathBuf::from("x.strand"));
    }
}
