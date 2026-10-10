//! The logic thread: the instance, the services and the step loop.

use super::shell::{Problems, Shell, diagnostics_json};
use super::sleep::{Inbox, Sleeper};
use super::*;

/// How long the first frame waits for the services it starts to send
/// their first reads (the portal's boot read has up to 500 ms; a desktop
/// portal answers in a few).
pub(super) const BOOT_SERVICES_HOLD: Duration = Duration::from_millis(100);

/// What the logic thread is given besides the main thread's channel: the
/// compiler worker's results and job queue, and the IPC socket to serve.
#[derive(Debug, Default)]
pub struct Live {
    pub worker: Option<Channel<FromWorker>>,
    pub jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// Where to serve `strand reload` and `strand watch`.
    pub socket: Option<PathBuf>,
    /// The buses the real services use (`strand run`: the environment's;
    /// tests: a private one). `None`: no bus at all, so services that
    /// need one keep their seeded values (`system` its last values).
    /// Unused under `STRAND_MOCK`, whose mock host serves everything.
    pub buses: Option<strand_services::Buses>,
    /// Called (on the services thread) when the portal's icon theme
    /// switches (`strand_services::icon_theme`, followed on the shared
    /// services runtime with the real services): the renderer's icons
    /// are looked up again. `None`: the theme is not followed.
    pub icon_theme_switched: Option<Switched>,
}

/// A callback for [`Live::icon_theme_switched`].
pub struct Switched(pub Box<dyn Fn() + Send>);

impl std::fmt::Debug for Switched {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Switched")
    }
}

/// The logic thread: mount `boot` (the loader's first outcome: its build,
/// or nothing and its diagnostics on the overlay), then step until
/// [`ToLogic::Shutdown`] (or until the main thread is gone), committing
/// what the compiler worker sends and serving the IPC socket; then
/// unmount and drop the stores so pending writes reach the disk.
pub fn logic(
    boot: Outcome,
    storage: Storage,
    rx: Channel<ToLogic>,
    out: Sender<SceneDiff>,
    live: Live,
) -> Result<(), String> {
    let (mut sleeper, ping) =
        Sleeper::with_worker(rx, live.worker).map_err(|e| format!("logic loop: {e}"))?;
    let handle = sleeper.event_loop.handle();
    let server = live
        .socket
        .as_deref()
        .and_then(|p| match ipc::Server::bind(p, &handle) {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!(
                    "no IPC socket (strand reload, strand watch): {}: {e}",
                    p.display()
                );
                None
            }
        });
    let rt = Runtime::new();
    let services_ping = ping.clone();
    rt.set_wake_hook(move || ping.ping());
    // With nothing to run yet (broken at boot, no last good version),
    // the host still serves the builtin services (`screens`, the clock):
    // the fixed config later mounts against this host.
    let host_types = match &boot.build {
        Some(b) => b.program.types.clone(),
        None => crate::services::schema().types.clone(),
    };
    let build = boot.build.clone().unwrap_or_else(Build::empty);
    let mock = crate::mock::requested();
    // The acceptance mock's clock stands still (UTC), so its screenshots
    // are the same every run.
    let frozen = crate::mock::frozen_time();
    let host = Rc::new(match frozen {
        Some(at) => {
            let utc = chrono::FixedOffset::east_opt(0).map_or(Zone::Local, Zone::Fixed);
            let clock = Clock::new(&rt, &host_types, utc, at);
            SchemaHost::new(&rt, &host_types, Some(clock))
        }
        None => SchemaHost::real(&rt, &host_types),
    });
    if let Some(m) = &mock {
        crate::mock::desktop(&rt, &host, m);
    }
    // The real services (none under the mock): registered now, each
    // started by its first reader.
    let real = mock.is_none().then(|| {
        let buses = live
            .buses
            .clone()
            .unwrap_or_else(strand_services::Buses::none);
        let real = crate::services::Real::start(&rt, &host_types, buses, host.clone(), move || {
            services_ping.ping()
        });
        real.custom.set_config_dir(storage.config_dir.clone());
        real
    });
    // GNOME (and any portal backend exposing GSettings) names the icon
    // theme in `org.gnome.desktop.interface`, not GTK's settings files:
    // followed for the whole run as a task of the services' shared
    // runtime, a switch invalidates like an `index.theme` change.
    let _icon_theme = real
        .as_ref()
        .zip(live.icon_theme_switched)
        .map(|(r, switched)| strand_services::icon_theme::spawn(&r.services, switched.0));
    // Monitors the main thread already knows about.
    let mut inbox = Inbox::default();
    sleeper
        .event_loop
        .dispatch(Some(Duration::ZERO), &mut inbox)
        .map_err(|e| format!("logic loop: {e}"))?;
    let mut stop = inbox.closed;
    // Notices for `strand watch` from before the shell is up: kept.
    let mut host_notices: Vec<String> = Vec::new();
    // (M4) GPU statuses from before the shell is up: said once it is.
    let mut early_gpu = Vec::new();
    for msg in inbox.msgs.drain(..) {
        match msg {
            ToLogic::Screens(list) => set_screens(&rt, &host, &list),
            ToLogic::Shutdown => stop = true,
            ToLogic::Notice(n) if !host_notices.contains(&n) => host_notices.push(n),
            ToLogic::GpuStatus(s) => early_gpu.push(s),
            _ => {}
        }
    }
    // `system`'s last values before the first frame (the portal's boot
    // read may take up to 500 ms), kept off the logic thread whenever
    // the service reports new ones. The runtime itself reads
    // `system.reduced_motion` (render snaps every spring while it is
    // on), so it holds one reader of `system` for the whole run.
    let system_file = system::Last::file(storage.palette_dir());
    let saved = (
        system_file.clone(),
        strand_theme::FileWriter::new()
            .inspect_err(|e| log::warn!("not keeping the portal's settings: {e}"))
            .ok(),
    );
    let mut last = system_file
        .as_deref()
        .map(system::Last::load)
        .unwrap_or_default();
    if let Some(r) = &real {
        if let Err(e) = r.builtin.system.seed(&rt, |s| last.seed(s)) {
            log::warn!("system: {e}");
        }
        r.builtin.system.acquire(&rt);
    }
    let inst = Instance::from_build(&rt, &build, live_host(&host, &real), storage);
    // The first frame waits (at most BOOT_SERVICES_HOLD) for the first
    // reads of the services the config started, so a desktop whose
    // scheme or accent changed while Strand was not running does not show
    // the kept values first and then switch. A slower service keeps its
    // seeded or default values until its read arrives.
    if let Some(r) = &real {
        r.services.wait_ready(&rt, BOOT_SERVICES_HOLD);
        keep_system(&rt, r, &mut last, &saved);
    }
    let mut shell = Shell {
        inst,
        host,
        real,
        build,
        overlay: Overlay::default(),
        server,
        jobs: live.jobs,
        deferred: None,
        deferred_hard: false,
        events: Vec::new(),
        latest: Problems::of(&boot),
        watched: Vec::new(),
        unheard: Vec::new(),
        warnings: Vec::new(),
        host_notices,
        settings_reread: Vec::new(),
        layout_seen: None,
        gpu: strand_scene::GpuStatus::default(),
    };
    shell.overlay.set_running(boot.build.is_some());
    // The boot's diagnostics: a config broken at boot runs its last good
    // version (or nothing) with the overlay up.
    if boot.errors() > 0 || !boot.unreadable.is_empty() {
        log::warn!("{}", render(&boot.diagnostics, &boot.sources, Style::Plain));
        let lines = overlay::lines(&boot.diagnostics, &boot.sources, &boot.unreadable);
        shell.overlay.set(lines, Instant::now(), &shell.inst);
    } else if !boot.diagnostics.is_empty() {
        // Warnings only (a `from dbus` service whose bus did not
        // answer): logged, and told to each `strand watch` that
        // subscribes until a reload resolves them.
        log::warn!("{}", render(&boot.diagnostics, &boot.sources, Style::Plain));
        shell.warnings = diagnostics_json(&boot);
    }
    if boot.from_cache {
        log::warn!("the config does not compile: running its last good version");
    }
    for s in early_gpu {
        shell.gpu_status(s);
    }
    shell.watch_settings();
    let start = Instant::now();
    // The reduced-motion preference render last heard of.
    let mut reduced_sent = false;
    // When the allocator's freed memory goes back to the system: once the
    // process has been quiet a moment ([`trim`]).
    let mut trimmer = Trimmer::default();
    while !stop {
        if shell.deferred.is_some() || shell.deferred_hard {
            shell.unlocked();
        }
        let wall = frozen.unwrap_or_else(SystemTime::now);
        let (mut update, wake) = shell.inst.step(start.elapsed(), wall);
        let mut diff = std::mem::take(&mut update.diff);
        diff.layout_seen = shell.layout_seen.take();
        // `system.reduced_motion` (the portal's, or its last value) goes
        // to render, which snaps every spring while it is on.
        let reduced = {
            let rt = shell.inst.runtime();
            let host = live_host(&shell.host, &shell.real);
            rt.untrack(|rt| host.read(rt, "system", "reduced_motion"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        if reduced != reduced_sent {
            diff.reduced_motion = Some(reduced);
            reduced_sent = reduced;
        }
        let shaped = structural(&diff);
        if !diff.is_empty() && out.send(diff).is_err() {
            break;
        }
        shell.after_step(&update);
        // Output a client's socket cannot take yet waits for its write
        // source to wake the loop (no polling).
        if let Some(s) = shell.server.as_mut() {
            s.flush(&handle);
        }
        let now = Instant::now();
        // A structural burst arms the trim; any wake while armed pushes
        // it back.
        if shaped {
            trimmer.arm(now);
        }
        trimmer.run(now);
        trimmer.settle(now, false);
        let mut timeout = wake.deadline.map(|d| d.saturating_sub(start.elapsed()));
        let mut also = |t: Option<Duration>| {
            if let Some(t) = t {
                timeout = Some(timeout.map_or(t, |x| x.min(t)));
            }
        };
        also(trimmer.wait(now));
        also(
            shell
                .overlay
                .deadline()
                .map(|d| d.saturating_duration_since(now)),
        );
        // A step that closed the lock: the deferred load commits now
        // (no polling while it stays shown: only a step can unlock).
        if (shell.deferred.is_some() || shell.deferred_hard) && !shell.inst.lock_shown() {
            also(Some(Duration::ZERO));
        }
        sleeper
            .sleep(
                timeout,
                wake.wall.filter(|_| frozen.is_none()),
                wall,
                &mut inbox,
            )
            .map_err(|e| format!("logic loop: {e}"))?;
        for m in inbox.msgs.drain(..) {
            if m == ToLogic::Shutdown {
                stop = true;
                break;
            }
            shell.handle(m);
        }
        stop |= inbox.closed;
        for w in inbox.worker.drain(..) {
            shell.worker(w);
        }
        // What the services sent while the loop slept, applied outside
        // handlers (reports are not handler writes) before the next step.
        if let Some(r) = &shell.real
            && r.services.pump(shell.inst.runtime())
        {
            keep_system(shell.inst.runtime(), r, &mut last, &saved);
        }
        let failed = shell
            .real
            .as_ref()
            .map(|r| r.services.take_diagnostics())
            .unwrap_or_default();
        if !failed.is_empty() {
            shell.service_diagnostics(failed);
        }
        if shell.inst.take_theme_files_changed() {
            shell.watch_settings();
        }
        let ready = std::mem::take(&mut inbox.ipc);
        if let Some(s) = &mut shell.server {
            if ready.accept {
                s.accept(&handle);
            }
            let mut reqs = Vec::new();
            for id in ready.readable {
                for r in s.read(id) {
                    reqs.push((id, r));
                }
            }
            for (id, r) in reqs {
                shell.request(id, r);
            }
        }
        shell.overlay.tick(Instant::now(), &shell.inst);
    }
    // Unmount: debounced `persist` writes and settings write-outs are
    // flushed as their cells go; the runtime's shutdown waits for the
    // persist queue (bounded), and the last store handle joins its IO
    // thread.
    let Shell {
        mut inst,
        host,
        real,
        ..
    } = shell;
    inst.shutdown();
    drop(inst);
    if let Some(r) = &real {
        r.shutdown(&rt);
    }
    rt.shutdown();
    drop(real);
    drop(host);
    Ok(())
}

/// The host the instance runs against: the real services' composite, or
/// the schema host alone (the mock).
pub(super) fn live_host(
    host: &Rc<SchemaHost>,
    real: &Option<crate::services::Real>,
) -> Rc<dyn strand_compiler::vm::ServiceHost> {
    match real {
        Some(r) => r.host.clone(),
        None => host.clone(),
    }
}

/// `system`'s values, written to the disk (off the logic thread) when
/// they changed.
pub(super) fn keep_system(
    rt: &Runtime,
    real: &crate::services::Real,
    last: &mut system::Last,
    saved: &(Option<PathBuf>, Option<strand_theme::FileWriter>),
) {
    let Ok(now) = real.builtin.system.cells().snapshot(rt) else {
        return;
    };
    let now = system::Last::of(&now);
    if now != *last {
        *last = now;
        if let (Some(f), Some(w)) = saved {
            w.write(f.clone(), last.to_text());
        }
    }
}
