//! The logic thread's state between steps: messages, loads, reloads,
//! IPC requests and the events `strand watch` streams.

use super::*;

/// What a load attempt found wrong (held and unreadable files, its
/// diagnostics with their sources).
#[derive(Debug, Default)]
pub(super) struct Problems {
    pub(super) held: Vec<PathBuf>,
    pub(super) unreadable: Vec<(PathBuf, String)>,
    pub(super) diagnostics: Vec<strand_compiler::diagnostic::Diagnostic>,
    pub(super) sources: std::sync::Arc<strand_compiler::source::SourceMap>,
}

impl Problems {
    pub(super) fn of(o: &Outcome) -> Self {
        Problems {
            held: o.held.clone(),
            unreadable: o.unreadable.clone(),
            diagnostics: o.diagnostics.clone(),
            sources: o.sources.clone(),
        }
    }

    /// Put these on `o` in place of its own.
    pub(super) fn onto(&self, o: &mut Outcome) {
        o.held = self.held.clone();
        o.unreadable = self.unreadable.clone();
        o.diagnostics = self.diagnostics.clone();
        o.sources = self.sources.clone();
    }
}

/// The logic thread's state between steps (see [`logic`]).
pub(super) struct Shell {
    pub(super) inst: Instance,
    /// The schema host: the mock under `STRAND_MOCK`, else the fallback
    /// of the composite host (the names no service serves yet: `screens`,
    /// the clock).
    pub(super) host: Rc<SchemaHost>,
    /// The real services (not under `STRAND_MOCK`).
    pub(super) real: Option<crate::services::Real>,
    pub(super) build: Build,
    pub(super) overlay: Overlay,
    pub(super) server: Option<ipc::Server>,
    pub(super) jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// A build that changes a lock while one is shown: committed after
    /// the unlock.
    pub(super) deferred: Option<Box<Loaded>>,
    /// A hard reload asked for while a lock was shown, owed after the
    /// unlock (its load went stale under a newer commit).
    pub(super) deferred_hard: bool,
    /// Reload events waiting for the step that draws them (their total
    /// time ends when its diff is sent), with the clients whose
    /// `strand reload` each answers.
    pub(super) events: Vec<(Json, Instant, Vec<ipc::ClientId>)>,
    /// The newest load attempt's problems: a deferred load replayed after
    /// the unlock reports these, not its own older ones.
    pub(super) latest: Problems,
    /// The referenced files last given to the watcher.
    pub(super) watched: Vec<(PathBuf, Role)>,
    /// Cells kept over a changed default outside a reload while nobody
    /// watched (at boot): the next reload event lists them.
    pub(super) unheard: Vec<strand_compiler::reconcile::KeptCell>,
    /// The running config's check warnings (`check::dbus_unchecked`: a
    /// bus that did not answer), as `strand watch` events list them: a
    /// watcher that subscribes later hears them at once; a later reload
    /// event with none resolves them.
    pub(super) warnings: Vec<Json>,
    /// Notices from the main thread (`ToLogic::Notice`: the blur
    /// fallback's reason), said once per run: usually sent at boot, before
    /// anyone watched, so each watcher that subscribes hears them too.
    pub(super) host_notices: Vec<String>,
    /// Settings files (and their runtime overlays) read again since the
    /// last step, as notices name them.
    pub(super) settings_reread: Vec<String>,
    /// The last layout fact batch taken in since the last diff went out.
    pub(super) layout_seen: Option<u64>,
    /// (M4) The GPU status render last reported.
    pub(super) gpu: strand_scene::GpuStatus,
}

impl Shell {
    /// Apply one message from the main thread.
    pub(super) fn handle(&mut self, msg: ToLogic) {
        let inst = &self.inst;
        match msg {
            ToLogic::Screens(list) => set_screens(inst.runtime(), &self.host, &list),
            ToLogic::Forget(id) => {
                inst.forget_screen(&id);
            }
            ToLogic::Event { node, event } => {
                if event == NodeEvent::Click
                    && let Some(c) = self.overlay.click(node, inst)
                {
                    match c {
                        Click::Open(file, line, col) => overlay::open_editor(&file, line, col),
                        Click::Reset(path) => {
                            if let Err(e) = inst.reset(&path) {
                                log::warn!("[reset] {path}: {e}");
                            }
                        }
                        Click::Clear(file, field) => {
                            inst.clear_settings_overlay(&file, &field);
                        }
                        Click::Dismissed | Click::Nothing => {}
                    }
                    return;
                }
                inst.event(node, event.name(), event.args_with(&self.host));
            }
            ToLogic::Layout { seq, sizes } => {
                for (node, w, h) in sizes {
                    inst.set_size(node, w, h);
                }
                self.layout_seen = Some(seq);
            }
            ToLogic::Write { node, prop, value } => {
                if let Err(e) = inst.write(node, prop, value) {
                    log::debug!("write to {prop}: {e}");
                }
            }
            ToLogic::Flag { node, flag, on } => inst.set_flag(node, flag, on),
            ToLogic::Size {
                node,
                width,
                height,
            } => inst.set_size(node, width, height),
            ToLogic::ListWindow { list, first, count } => {
                super::lists::set_window(inst, list, first, count);
            }
            ToLogic::Notice(text) if !self.host_notices.contains(&text) => {
                if let Some(s) = &mut self.server {
                    s.broadcast(&json!({
                        "event": "notices",
                        "kept_over_default": [],
                        "notices": [&text],
                    }));
                }
                self.host_notices.push(text);
            }
            ToLogic::Notice(_) => {}
            ToLogic::GpuStatus(status) => self.gpu_status(status),
            ToLogic::Shutdown => {}
        }
    }

    /// (M4) The GPU status render reported: kept (for `strand report`),
    /// and each reason the CPU fallback draws is logged and made a
    /// `strand watch` notice once per run.
    pub(super) fn gpu_status(&mut self, status: strand_scene::GpuStatus) {
        let text = match &status {
            strand_scene::GpuStatus::Unavailable { reason } => Some(format!(
                "GPU unavailable: {reason}; shaders draw nothing (CPU fallback)"
            )),
            strand_scene::GpuStatus::Up(info) => {
                log::info!("GPU: {} ({})", info.name, info.driver);
                None
            }
            _ => None,
        };
        self.gpu = status;
        if let Some(text) = text
            && !self.host_notices.contains(&text)
        {
            log::warn!("{text}");
            self.handle(ToLogic::Notice(text));
        }
    }

    /// Service failures and notices: `strand watch` notices, and overlay
    /// rows for what the user must act on (another notification server
    /// owns the name), under a `strand: services` header; a resolved
    /// notice (the name taken over) takes its rows away and is a `strand
    /// watch` notice of its own (each is logged where it is made).
    pub(super) fn service_diagnostics(
        &mut self,
        diagnostics: Vec<strand_services::ServiceDiagnostic>,
    ) {
        let texts: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
        // The overlay shows what the user must act on; a failure the
        // service retries (no session bus) is a log line and a `strand
        // watch` notice.
        for d in diagnostics.iter().filter(|d| d.notice) {
            self.overlay.forget_service(&d.service, &self.inst);
            if !d.resolved {
                let rows = overlay::service_lines(&d.service, &d.to_string());
                self.overlay.note(rows, Instant::now(), &self.inst);
            }
        }
        if let Some(s) = &mut self.server {
            s.broadcast(&json!({
                "event": "notices",
                "kept_over_default": [],
                "notices": texts,
            }));
        }
    }

    /// A result from the compiler worker.
    pub(super) fn worker(&mut self, msg: FromWorker) {
        match msg {
            FromWorker::Settings(changes) => {
                for c in changes {
                    let p = c.path;
                    if self.inst.reload_settings_with(&p, c.read) {
                        // The next step's notices say what is still wrong
                        // in them; the rest of their rows go.
                        self.settings_reread.push(p.to_string_lossy().into_owned());
                        for o in self.inst.settings_overlay_paths(&p) {
                            self.settings_reread.push(o.to_string_lossy().into_owned());
                        }
                    }
                }
            }
            FromWorker::Theme(paths) => {
                self.inst.theme_files_changed(&paths);
            }
            FromWorker::Loaded(l) => self.commit(l),
        }
    }

    /// Commit a load: the new build into the running instance (a hard
    /// reload recreates everything), its diagnostics and reload notices
    /// to the overlay, its event queued for the watchers.
    ///
    /// While a lock is shown, a build that changes a lock (or a hard
    /// reload) is not committed (decisions.md, wave2-runtime): it waits
    /// in [`Shell::deferred`], a newer deferred load absorbing it, and is
    /// committed after the unlock; its event goes out at once (classes
    /// `lock-deferred`, `"deferred": true`), answering `strand reload`.
    /// A build committed meanwhile makes the deferred one stale: it is
    /// dropped (a deferred hard reload is still owed).
    pub(super) fn commit(&mut self, l: Box<Loaded>) {
        self.latest = Problems::of(&l.outcome);
        self.apply(l, true);
    }

    /// [`Shell::commit`]; `overlay`: the load's diagnostics replace the
    /// overlay's (not for a hard reload replayed after an unlock, whose
    /// load carries none).
    pub(super) fn apply(&mut self, mut l: Box<Loaded>, overlay: bool) {
        let began = Instant::now();
        let build = l.outcome.build.clone();
        let report = match (&build, l.hard) {
            (Some(b), false) => Some(self.inst.reload(b)),
            (b, true) => {
                let b = b.clone().unwrap_or_else(|| self.build.clone());
                Some(self.inst.reload_hard(&b))
            }
            (None, false) => None,
        };
        let deferred = report
            .as_ref()
            .is_some_and(|r| r.classes == [EditClass::LockDeferred]);
        if deferred {
            // Said in the event, the log and the overlay: an edit synced
            // in while locked (over ssh, a home-manager switch) does not
            // land until the unlock, also when it does not touch the
            // lock itself (decisions.md, wave2-runtime).
            let what = if l.files.is_empty() {
                "the reload".to_string()
            } else {
                l.files
                    .iter()
                    .map(|f| f.file_name().unwrap_or(f.as_os_str()).to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let why = if self.deferred.is_some() {
                "a lock edit is waiting"
            } else {
                "the lock changed while it is shown"
            };
            let notice = format!("{what}: waits for the unlock ({why})");
            log::info!("{notice}");
            self.overlay.note(
                vec![overlay::Line {
                    cell: Some(overlay::WAITS_FOR_UNLOCK.to_string()),
                    ..overlay::notice_line(&notice)
                }],
                Instant::now(),
                &self.inst,
            );
            l.notices.push(notice);
            if let Some(old) = self.deferred.take() {
                absorb(&mut l, &old);
            }
        } else if report.is_some() {
            self.overlay
                .forget_cell(overlay::WAITS_FOR_UNLOCK, &self.inst);
            // Committed: an older deferred build is stale (this one is
            // newer and has everything it had, the lock edit aside,
            // which this one either reverted or kept).
            if let Some(old) = self.deferred.take()
                && old.hard
                && !l.hard
            {
                self.deferred_hard = true;
            }
            if l.hard {
                self.deferred_hard = false;
            }
            if let Some(b) = build.or_else(|| l.hard.then(|| self.build.clone())) {
                self.build = b;
                self.overlay.set_running(true);
            }
        }
        let commit = began.elapsed();
        if overlay {
            // The overlay: every diagnostic of the attempt (none when it
            // all committed).
            let lines = overlay::lines(
                &l.outcome.diagnostics,
                &l.outcome.sources,
                &l.outcome.unreadable,
            );
            let errors = l.outcome.errors();
            self.overlay.set(
                if errors > 0 || !l.outcome.unreadable.is_empty() {
                    lines
                } else {
                    Vec::new()
                },
                Instant::now(),
                &self.inst,
            );
        }
        if let Some(r) = report.as_ref().filter(|_| !deferred) {
            for n in &r.notices {
                log::info!("{n}");
            }
            // Kept-over-a-new-default cells (with their `[reset]`),
            // renamed or retyped cells reset, cancelled `await`s:
            // overlay rows, after the same quiet period.
            self.overlay
                .note(overlay::report_lines(r), Instant::now(), &self.inst);
        }
        if !l.outcome.diagnostics.is_empty() {
            log::warn!(
                "{}",
                render(&l.outcome.diagnostics, &l.outcome.sources, Style::Plain)
            );
        }
        self.watch_settings();
        let mut ev = reload_event(&l, report.as_ref(), commit);
        if l.outcome.errors() == 0 {
            // What the running config is warned about now (none: the
            // earlier warnings are resolved).
            self.warnings = ev["diagnostics"].as_array().cloned().unwrap_or_default();
        }
        ev["deferred"] = json!(deferred);
        if !self.unheard.is_empty()
            && let Some(k) = ev["kept_over_default"].as_array_mut()
        {
            // Kept when nobody was watching (the boot's persisted cells).
            let earlier = kept_json(&std::mem::take(&mut self.unheard));
            if let Json::Array(earlier) = earlier {
                k.splice(0..0, earlier);
            }
        }
        let clients = std::mem::take(&mut l.clients);
        self.events
            .push((ev, l.saved.unwrap_or(l.started), clients));
        if deferred {
            self.deferred = Some(l);
        }
    }

    /// Give the worker the files the program now reads: its settings
    /// files, and the wallpapers and palette files its theme read (a
    /// wallpaper's link target is watched too).
    pub(super) fn watch_settings(&mut self) {
        let (images, imports) = self.inst.theme_files();
        let files: Vec<(PathBuf, Role)> = self
            .inst
            .settings_files()
            .into_iter()
            .map(|f| (f, Role::Settings))
            .chain(images.into_iter().map(|f| (f, Role::Wallpaper)))
            .chain(imports.into_iter().map(|f| (f, Role::Other)))
            .collect();
        if files != self.watched {
            self.watched = files.clone();
            if let Some(j) = &self.jobs {
                let _ = j.send(Job::Referenced {
                    files,
                    settings: self.inst.settings_sources(),
                });
            }
        }
    }

    /// A request from an IPC client.
    pub(super) fn request(&mut self, id: ipc::ClientId, req: ipc::Request) {
        match req {
            ipc::Request::Reload { hard } => match &self.jobs {
                Some(j)
                    if j.send(Job::Reload {
                        hard,
                        client: Some(id),
                    })
                    .is_ok() => {}
                _ => {
                    if let Some(s) = &mut self.server {
                        s.answer(
                            id,
                            &json!({"ok": false, "error": "this shell does not reload"}),
                        );
                    }
                }
            },
            ipc::Request::Reset { path } => {
                let ans = match self.inst.reset(&path) {
                    Ok(()) => {
                        self.overlay.forget_cell(&path, &self.inst);
                        json!({"ok": true})
                    }
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            ipc::Request::Set { path, value } => {
                // A service's `rw` field (`brightness.level +5%`) goes
                // through the service host; everything else is state or
                // settings.
                let rt = self.inst.runtime();
                let exported = self.inst.get(&path).is_ok();
                let r = if !exported && crate::services::is_service_path(&path) {
                    match &self.real {
                        Some(real) => real.set_text(rt, &path, &value),
                        None => crate::services::set_text(
                            &*self.host,
                            rt,
                            &crate::services::schema().types,
                            &path,
                            &value,
                        ),
                    }
                } else {
                    self.inst.set_text(&path, &value).map_err(|e| e.to_string())
                };
                let ans = match r {
                    Ok(()) => json!({"ok": true}),
                    Err(e) => json!({"ok": false, "error": e}),
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            ipc::Request::Mock(req) => {
                let ans = if crate::mock::requested().is_none() {
                    json!({"ok": false, "error": "`mock` needs a shell run with STRAND_MOCK"})
                } else {
                    match crate::mock::command(self.inst.runtime(), &self.host, &req) {
                        Ok(()) => json!({"ok": true}),
                        Err(e) => json!({"ok": false, "error": e}),
                    }
                };
                if let Some(s) = &mut self.server {
                    s.answer(id, &ans);
                }
            }
            // Answered by the server itself; the running config's
            // warnings and the main thread's notices follow (the boot's
            // were made before anyone watched).
            ipc::Request::Watch => {
                if !(self.warnings.is_empty() && self.host_notices.is_empty())
                    && let Some(s) = &mut self.server
                {
                    s.send(
                        id,
                        &json!({
                            "event": "notices",
                            "kept_over_default": [],
                            "notices": self.host_notices,
                            "diagnostics": self.warnings,
                        }),
                    );
                }
            }
        }
    }

    /// After a step: send the reload events it drew (and answer the
    /// clients waiting for a reload), report runtime faults.
    pub(super) fn after_step(&mut self, update: &strand_compiler::instantiate::Update) {
        for e in &update.errors {
            log::error!("{e}");
            // A runtime fault freezes its component, outlined red.
            let frozen = self.inst.freeze(e);
            if let Some(s) = &mut self.server {
                let at = match (e.file, e.span) {
                    (Some(f), Some(sp)) => self.build.sources.get(f).map(|file| {
                        let (l, c) = overlay::line_col(&file.text, sp.start);
                        format!("{}:{l}:{c}", file.name)
                    }),
                    _ => None,
                };
                s.broadcast(&json!({
                    "event": "fault",
                    "message": e.to_string(),
                    "at": at,
                    "frozen": frozen,
                }));
            }
        }
        let mut settings = Vec::new();
        for d in &update.diagnostics {
            match d {
                strand_core::Diagnostic::Settings(n) => {
                    log::warn!("{n}");
                    settings.push(n);
                }
                d => log::warn!("{d:?}"),
            }
        }
        let reread = std::mem::take(&mut self.settings_reread);
        if !settings.is_empty() || !reread.is_empty() {
            // Settings files: a bad value kept, a syntax error, a
            // read-only file going to an overlay, a file change shadowed
            // by the runtime overlay (with its `[clear]`). A file read
            // again without its old problem loses its row.
            let rows: Vec<_> = settings.iter().map(|n| overlay::settings_line(n)).collect();
            self.overlay
                .settings_read(&reread, rows, Instant::now(), &self.inst);
        }
        if !settings.is_empty()
            && let Some(s) = &mut self.server
        {
            let texts: Vec<String> = settings.iter().map(|n| n.to_string()).collect();
            s.broadcast(&json!({
                "event": "notices",
                "kept_over_default": [],
                "notices": texts,
            }));
        }
        for n in &update.notices {
            log::info!("{n}");
        }
        if !update.notices.is_empty() || !update.kept.is_empty() {
            // Persisted cells kept over a changed default at boot (their
            // `[reset]` from the structured record), lowering's warnings.
            let rows = overlay::kept_and_notices(&update.kept, &update.notices);
            self.overlay.note(rows, Instant::now(), &self.inst);
            // `strand watch` hears them as they happen; with nobody
            // watching (at boot), the next reload event carries them.
            match &mut self.server {
                Some(s) if s.watchers() > 0 => s.broadcast(&json!({
                    "event": "notices",
                    "kept_over_default": kept_json(&update.kept),
                    "notices": update.notices,
                })),
                _ => self.unheard.extend(update.kept.iter().cloned()),
            }
        }
        let now = Instant::now();
        for (mut ev, since, clients) in self.events.drain(..) {
            ev["timing"]["total_ms"] = json!(ms(now.saturating_duration_since(since)));
            log::info!("{}", ipc::describe(&ev).trim_end());
            if let Some(s) = &mut self.server {
                s.broadcast(&ev);
                // Each `strand reload` gets the event of its own load.
                for id in clients {
                    s.answer(id, &json!({"ok": true, "event": ev.clone()}));
                }
            }
        }
    }
}

/// A newer deferred load `l` takes in an older one it replaces: the
/// files it named and committed, and whether it was asked for (hard).
pub(super) fn absorb(l: &mut Loaded, old: &Loaded) {
    for f in &old.files {
        if !l.files.contains(f) {
            l.files.push(f.clone());
        }
    }
    for f in &old.outcome.committed {
        if !l.outcome.committed.contains(f) {
            l.outcome.committed.push(f.clone());
        }
    }
    // A deferred hard reload of the same sources has no build of its
    // own: it applies the deferred one.
    if l.outcome.build.is_none() {
        l.outcome.build = old.outcome.build.clone();
    }
    l.requested |= old.requested;
    l.hard |= old.hard;
    l.saved = match (l.saved, old.saved) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    for n in &old.notices {
        if !l.notices.contains(n) {
            l.notices.push(n.clone());
        }
    }
}

/// Cells kept over a changed default, as `strand watch` lists them.
pub(super) fn kept_json(kept: &[strand_compiler::reconcile::KeptCell]) -> Json {
    kept.iter()
        .map(|k| json!({"path": k.path, "shown": k.shown}))
        .collect()
}

pub(super) fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e5).round() / 100.0
}

/// An attempt's diagnostics as `strand watch` events list them.
pub(super) fn diagnostics_json(o: &Outcome) -> Vec<Json> {
    o.diagnostics
        .iter()
        .map(|d| {
            // One rendering per diagnostic: a multi-line one cannot
            // shift the others.
            let short = render_short(std::slice::from_ref(d), &o.sources);
            let line = short.trim_end();
            let at = d.primary().and_then(|lab| {
                o.sources.get(lab.file).map(|f| {
                    let (ln, col) = overlay::line_col(&f.text, lab.span.start);
                    json!({"file": f.name, "line": ln, "column": col})
                })
            });
            json!({
                "severity": if d.is_error() { "error" } else { "warning" },
                "code": d.code,
                "message": d.message,
                "help": d.help,
                "at": at,
                "labels": d.labels.iter().map(|lab| {
                    let at = o.sources.get(lab.file).map(|f| {
                        let (ln, col) = overlay::line_col(&f.text, lab.span.start);
                        json!({"file": f.name, "line": ln, "column": col})
                    });
                    json!({"message": lab.message, "primary": lab.primary, "at": at})
                }).collect::<Vec<_>>(),
                "short": line,
            })
        })
        .collect()
}

/// A load as `strand watch` streams it (`timing.total_ms` is filled in
/// when the step that draws it has sent its diff).
pub(super) fn reload_event(l: &Loaded, report: Option<&Report>, commit: Duration) -> Json {
    let paths =
        |v: &[PathBuf]| -> Vec<String> { v.iter().map(|p| p.display().to_string()).collect() };
    let diagnostics = diagnostics_json(&l.outcome);
    let r = report.cloned().unwrap_or_default();
    json!({
        "event": "reload",
        "requested": l.requested,
        "hard": l.hard,
        "files": paths(&l.files),
        "committed": paths(&l.outcome.committed),
        "held": paths(&l.outcome.held),
        "unreadable": l.outcome.unreadable.iter().map(|(p, e)| json!({"file": p.display().to_string(), "error": e})).collect::<Vec<_>>(),
        "from_cache": l.outcome.from_cache,
        "classes": r.classes.iter().map(|c| c.name()).collect::<Vec<_>>(),
        "kept": r.kept,
        "kept_over_default": kept_json(&r.kept_over_default),
        "reset": r.reset.iter().map(|(c, w)| json!({"cell": c, "why": w})).collect::<Vec<_>>(),
        "ambiguous": r.ambiguous,
        "notices": r.notices.iter().chain(&l.notices).collect::<Vec<_>>(),
        "restarted": r.restarted,
        "cancelled": r.cancelled,
        "timing": {
            "watch_ms": l.saved.map(|s| ms(l.started.saturating_duration_since(s))),
            "compile_ms": ms(l.outcome.compile_time),
            "commit_ms": ms(commit),
            "total_ms": Json::Null,
        },
        "diagnostics": diagnostics,
    })
}
