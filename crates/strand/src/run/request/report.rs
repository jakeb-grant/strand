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
        // Every service but the secret ones (`auth`, `polkit`, `greetd`),
        // under the redaction rules.
        let mut services = Map::new();
        if let Some(r) = &self.real {
            for s in r.services.snapshot(rt) {
                if strand_services::secret(&s.name) {
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
        // The renderer's drawn files are resolved on the report thread
        // (`bundle::write`), after these.
        let nodes = self.node_table(&main, config_dir.as_deref());
        let drawn = self.drawn_table(&main, config_dir.as_deref());
        let now_ns = strand_core::flight::now_ns();
        let history = self.history_table();
        let wall = chrono::Local::now().to_rfc3339();
        Job {
            root,
            words,
            report,
            screens: self.screens_named(),
            main,
            changes: rec.dump(),
            nodes,
            drawn,
            services: Json::Object(services),
            cells: Json::Object(cells),
            history,
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

impl Shell {
    /// (history) `history.json`: each series a `graph` keeps history of
    /// (but one reading a secret service: `auth`, `polkit`, `greetd`), its points as `[at, value, ramp]` (`at`
    /// on the boot clock, seconds; a `null` value starts a gap), with the
    /// boot clock at the report (`boot_ns`, read with `now_ns`), so the
    /// replay seeds what each held before its window. (graph-follow)
    /// `boot_ns` is written when nothing is kept too: the replay runs on
    /// the live boot clock, so a graph's columns fall where they fell.
    fn history_table(&self) -> Json {
        history_json(&self.inst.histories(), strand_scene::backlog::boot_now())
    }

    /// (report) `nodes.json`: each node the input ring names (a press's
    /// hit, a mapped surface's root) and each mapped surface's root, still
    /// mounted, by its `node_ref`: its origin (`file.strand:LINE:COL`, the
    /// file under the config directory), `id:` and `for` row key, what a
    /// replay compares the node its own press hits with.
    fn node_table(&self, main: &MainBundle, config_dir: Option<&Path>) -> Json {
        use strand_compiler::instantiate::inspect::{node_ref, parse_node_ref};
        use strand_core::flight::{self, kind};
        let mut ids: Vec<strand_scene::NodeId> = Vec::new();
        if let Some(dump) = main.ring.as_deref().and_then(|r| flight::read(r).ok()) {
            for r in &dump.records {
                if matches!(r.kind, kind::HIT | kind::MAPPED) {
                    let bits = r.u64(0);
                    ids.push(strand_scene::NodeId::new(bits as u32, (bits >> 32) as u32));
                }
            }
        }
        ids.extend(
            main.surfaces
                .iter()
                .filter_map(|s| parse_node_ref(s["root"].as_str()?)),
        );
        let screens = self.screen_ids();
        let mut out = Map::new();
        let rec = self.recorder.borrow();
        for id in ids {
            let r = node_ref(id);
            if out.contains_key(&r) {
                continue;
            }
            // Mounted now, else (report review) as a press found it
            // before the press removed it.
            let (place, ident) = match self.inst.origin(id) {
                Some((f, _, sp)) => ((f, sp), self.inst.identity(id)),
                None => match rec.hit_identity(id) {
                    Some(h) => ((h.file, h.span), h.ident.clone()),
                    None => continue,
                },
            };
            let Some(origin) = self.origin_text(place.0, place.1, config_dir) else {
                continue;
            };
            let id_name = ident.as_ref().and_then(|i| i.id.clone());
            let key = ident.as_ref().and_then(|i| row_key(i, &screens));
            out.insert(r, json!({"origin": origin, "id": id_name, "key": key}));
        }
        Json::Object(out)
    }
}

impl Shell {
    /// The screens' ids: a per-screen surface's nodes are mounted under
    /// theirs, which a replay on another output names otherwise.
    fn screen_ids(&self) -> Vec<String> {
        let rt = self.inst.runtime();
        let host = crate::run::logic::live_host(&self.host, &self.real);
        let types = &self.inst.vm().prog.types;
        rt.untrack(|rt| host.read_builtin(rt, "screens", "all"))
            .ok()
            .and_then(|v| {
                let list = v.as_list()?;
                Some(
                    list.iter()
                        .filter_map(|s| Some(s.field(types, "id")?.as_text()?.to_string()))
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    /// (replayfix) Each screen of `screens.all` as `bundle.json`'s
    /// outputs name it: its connector name, id and description.
    fn screens_named(&self) -> Vec<(String, String, String)> {
        let rt = self.inst.runtime();
        let host = crate::run::logic::live_host(&self.host, &self.real);
        let types = &self.inst.vm().prog.types;
        let text = |s: &strand_compiler::vm::Value, f: &str| {
            Some(s.field(types, f)?.as_text()?.to_string())
        };
        rt.untrack(|rt| host.read_builtin(rt, "screens", "all"))
            .ok()
            .and_then(|v| {
                let list = v.as_list()?;
                Some(
                    list.iter()
                        .filter_map(|s| {
                            Some((text(s, "name")?, text(s, "id")?, text(s, "description")?))
                        })
                        .collect(),
                )
            })
            .unwrap_or_default()
    }

    /// A source place as `file.strand:LINE:COL`, the file under the
    /// config directory.
    pub(in crate::run) fn origin_text(
        &self,
        file: strand_compiler::FileId,
        span: strand_compiler::syntax::Span,
        config_dir: Option<&Path>,
    ) -> Option<String> {
        let f = self.build.sources.get(file)?;
        let (l, c) = crate::overlay::line_col(&f.text, span.start);
        let rel = relative(&f.name, config_dir);
        Some(format!("{}:{l}:{c}", rel.display()))
    }

    /// (replay-owner) `drawn.json`: each mapped surface's nodes as its
    /// last frame drew them (`MainBundle::drawn`), named as `nodes.json`
    /// names a node (origin, `id:`, `for` row key), with the box each
    /// was drawn at (surface logical px, after transforms, to 1/64), its
    /// own opacity and how far each moves over the next frame period
    /// (`travel` px, `fade`), in paint order; and per surface when that
    /// frame was drawn (`drawn_ns`, ns since the rings' epoch, the
    /// clock of every record) and its frame period (`period_ns`). A
    /// node with no place in the config's source (none the replay can
    /// name either) is left out.
    pub(super) fn drawn_table(&self, main: &MainBundle, config_dir: Option<&Path>) -> Json {
        use strand_compiler::instantiate::inspect::node_ref;
        let screens = self.screen_ids();
        let q = |v: f32| f64::from((v * 64.0).round() / 64.0);
        let surfaces: Vec<Json> = main
            .drawn
            .iter()
            .map(|(slot, frame)| {
                let root = main
                    .surfaces
                    .iter()
                    .find(|s| s["slot"].as_u64() == Some(u64::from(*slot)))
                    .and_then(|s| s["root"].as_str())
                    .map(str::to_string);
                let nodes: Vec<Json> = frame
                    .nodes
                    .iter()
                    .filter_map(|n| {
                        let (f, _, sp) = self.inst.origin(n.node)?;
                        let origin = self.origin_text(f, sp, config_dir)?;
                        let ident = self.inst.identity(n.node);
                        let r = n.rect;
                        Some(json!({
                            "ref": node_ref(n.node),
                            "origin": origin,
                            "id": ident.as_ref().and_then(|i| i.id.clone()),
                            "key": ident.as_ref().and_then(|i| row_key(i, &screens)),
                            "box": [q(r.x), q(r.y), q(r.w), q(r.h)],
                            "opacity": q(n.opacity),
                            "travel": q(n.travel),
                            "fade": q(n.fade),
                        }))
                    })
                    .collect();
                json!({
                    "slot": slot,
                    "root": root,
                    "drawn_ns": frame.drawn_at.map(strand_core::flight::ns_at),
                    "period_ns": u64::try_from(frame.period.as_nanos()).unwrap_or(u64::MAX),
                    "nodes": nodes,
                })
            })
            .collect();
        json!({"format": 2, "surfaces": surfaces})
    }
}

/// (report) The `for` row key a node is mounted under: `None` outside
/// any, and for a node directly in a per-screen surface, whose innermost
/// qualifier is its screen's id (one of `screens`), which a replay on
/// another output names otherwise.
fn row_key(
    i: &strand_compiler::instantiate::inspect::Identity,
    screens: &[String],
) -> Option<String> {
    let key = i.key.as_ref()?;
    (!screens.contains(key)).then(|| key.clone())
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

/// (history) [`Shell::history_table`] of `kept` with the boot clock at
/// `boot`: (graph-follow review) an object with `boot_ns` even when
/// nothing is kept (`series` empty).
fn history_json(
    kept: &[strand_compiler::instantiate::HistoryInfo],
    boot: std::time::Duration,
) -> Json {
    let series: Vec<Json> = kept
        .iter()
        .filter(|h| h.services.iter().all(|(s, _)| !strand_services::secret(s)))
        .map(|h| {
            let points: Vec<Json> = h
                .ring
                .iter()
                .map(|p| {
                    let v = if p.value.is_finite() {
                        json!(p.value)
                    } else {
                        Json::Null
                    };
                    json!([p.at, v, p.ramp])
                })
                .collect();
            json!({
                "key": h.key,
                "span_ms": u64::try_from(h.span.as_millis()).unwrap_or(u64::MAX),
                "points": points,
            })
        })
        .collect();
    json!({
        "boot_ns": u64::try_from(boot.as_nanos()).unwrap_or(u64::MAX),
        "series": series,
    })
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

    /// (graph-follow review) A report of a shell that keeps no history
    /// still writes `history.json`, with the boot clock its replay runs
    /// on and no series.
    #[test]
    fn history_json_holds_the_boot_clock_when_nothing_is_kept() {
        let h = history_json(&[], std::time::Duration::from_secs(1234));
        assert_eq!(h, json!({ "boot_ns": 1_234_000_000_000u64, "series": [] }));
    }
}
