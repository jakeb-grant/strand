//! `strand run [dir]`: the config in `dir` (default
//! `$XDG_CONFIG_HOME/strand`) compiled and run, wired as
//! `docs/architecture.md` describes ("Threads", and the host-loop recipe
//! under Instantiation).
//!
//! - The main thread runs the surface manager and the renderer, as the
//!   demo does. Its host forwards the surface layer's monitor hooks to the
//!   logic thread as the `screens` service (`Screen.id` is the
//!   `MonitorId`; `monitor_forgotten` is `Instance::forget_screen`), and
//!   surface-level input and layout facts as `event`/`set_flag`/
//!   `set_size` on the surface's node (hit testing inside a surface is
//!   M2).
//! - The logic thread owns the runtime, the real service host
//!   (`SchemaHost::real`: the wall clock and calendar) and the
//!   `Instance`, and loops on `Instance::step(now, wall)`, sending one
//!   `SceneDiff` per tick and sleeping for `Wake::sleep_for`, a message
//!   from the main thread or the runtime's wake hook.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use calloop::channel::Event;
use strand_compiler::SourceMap;
use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::{Instance, NodeFlag, Storage};
use strand_compiler::lower::{self, Program};
use strand_compiler::source::find_files;
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_render::{Renderer, TextBackend};
use strand_scene::{NodeId, SceneDiff};
use strand_surface::{Config, SurfaceManager};
use strand_text::{FontConfig, TextWorker};

use crate::demo::host::Host;
use crate::demo::{DemoError, FIRST_FRAME_TEXT_WAIT, apply, connection_closed};
use crate::logging::LogConfig;

/// A monitor as the `screens` service shows it (plain data: it crosses
/// threads).
#[derive(Clone, Debug, PartialEq)]
pub struct ScreenInfo {
    /// The `MonitorId` (make, model and description).
    pub id: String,
    /// The connector (`DP-1`), or the id when the compositor gives none.
    pub name: String,
    pub make: String,
    pub model: String,
    pub description: String,
    pub scale: f64,
    /// Logical size, when known.
    pub width: f64,
    pub height: f64,
}

/// What the main thread tells the logic thread.
#[derive(Clone, Debug, PartialEq)]
pub enum ToLogic {
    /// The plugged-in monitors, in plug order (the first counts as
    /// focused until a compositor service says otherwise).
    Screens(Vec<ScreenInfo>),
    /// A monitor unplugged 30 s ago did not come back.
    Forget(String),
    /// `on <name>` on a surface's node, with numeric arguments (`scroll`:
    /// `dy, dx`).
    Event {
        node: NodeId,
        name: &'static str,
        args: Vec<f64>,
    },
    /// The pointer entered (`true`) or left a surface.
    Hover { node: NodeId, on: bool },
    /// A surface's logical size.
    Size {
        node: NodeId,
        width: f32,
        height: f32,
    },
    /// Something woke the runtime (an IO reply): step.
    Wake,
}

/// Read and compile the config in `dir`: the program, or the rendered
/// diagnostics.
pub fn compile(dir: &Path, style: Style) -> Result<Program, String> {
    let found = find_files(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    if found.files.is_empty() {
        return Err(format!("no .strand files in {}", dir.display()));
    }
    let mut map = SourceMap::new();
    for path in &found.files {
        let src = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        map.add(path.display().to_string(), src);
    }
    let compiled = strand_compiler::compile(&map);
    if compiled.errors() > 0 {
        return Err(render(&compiled.diagnostics, &map, style));
    }
    if !compiled.diagnostics.is_empty() {
        log::warn!("{}", render(&compiled.diagnostics, &map, Style::Plain));
    }
    Ok(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ))
}

/// The `screens` service fields for `screens`.
fn set_screens(rt: &Runtime, host: &SchemaHost, screens: &[ScreenInfo]) {
    let records: Vec<Value> = screens
        .iter()
        .enumerate()
        .map(|(i, s)| {
            host.record(
                "Screen",
                &[
                    ("id", Value::text(s.id.as_str())),
                    ("name", Value::text(s.name.as_str())),
                    ("make", Value::text(s.make.as_str())),
                    ("model", Value::text(s.model.as_str())),
                    ("description", Value::text(s.description.as_str())),
                    ("scale", Value::float(s.scale)),
                    ("width", Value::float(s.width)),
                    ("height", Value::float(s.height)),
                    ("focused", Value::Bool(i == 0)),
                ],
            )
        })
        .collect();
    let focused = records.first().cloned().unwrap_or(Value::Null);
    if let Err(e) = host.set(rt, "screens.all", Value::list(records)) {
        log::error!("screens.all: {e}");
    }
    if let Err(e) = host.set(rt, "screens.focused", focused) {
        log::error!("screens.focused: {e}");
    }
}

/// Apply one message from the main thread.
fn handle(inst: &Instance, host: &SchemaHost, msg: ToLogic) {
    match msg {
        ToLogic::Screens(list) => set_screens(inst.runtime(), host, &list),
        ToLogic::Forget(id) => {
            inst.forget_screen(&id);
        }
        ToLogic::Event { node, name, args } => {
            inst.event(node, name, args.into_iter().map(Value::float).collect());
        }
        ToLogic::Hover { node, on } => inst.set_flag(node, NodeFlag::Hover, on),
        ToLogic::Size {
            node,
            width,
            height,
        } => inst.set_size(node, width, height),
        ToLogic::Wake => {}
    }
}

/// The logic thread: mount `program`, then step until the main thread
/// goes away.
pub fn logic(
    program: Arc<Program>,
    dir: PathBuf,
    rx: mpsc::Receiver<ToLogic>,
    wake: mpsc::Sender<ToLogic>,
    out: calloop::channel::Sender<SceneDiff>,
) -> Result<(), String> {
    let rt = Runtime::new();
    rt.set_wake_hook(move || {
        let _ = wake.send(ToLogic::Wake);
    });
    let host = Rc::new(SchemaHost::real(&rt, &program.types));
    let storage = match Storage::from_env(&dir) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("persist store: {e}; state is not kept across restarts");
            Storage {
                config_dir: Some(dir.clone()),
                ..Storage::none()
            }
        }
    };
    // Monitors the main thread already knows about.
    for msg in rx.try_iter() {
        if let ToLogic::Screens(list) = msg {
            set_screens(&rt, &host, &list);
        }
    }
    let inst = Instance::new(&rt, program, host.clone(), storage);
    let start = Instant::now();
    loop {
        let (update, wake) = inst.step(start.elapsed(), SystemTime::now());
        for e in &update.errors {
            log::error!("{e}");
        }
        for d in &update.diagnostics {
            log::warn!("{d:?}");
        }
        for n in &update.notices {
            log::info!("{n}");
        }
        if !update.diff.is_empty() && out.send(update.diff).is_err() {
            return Ok(());
        }
        let msg = match wake.sleep_for(start.elapsed(), SystemTime::now()) {
            Some(d) => match rx.recv_timeout(d) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            },
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return Ok(()),
            },
        };
        for m in msg.into_iter().chain(rx.try_iter()) {
            handle(&inst, &host, m);
        }
    }
}

/// Run the config in `dir` until the compositor goes away (then `Ok`), or
/// until the logic thread fails.
pub fn run(dir: &Path, log: &LogConfig) -> Result<(), DemoError> {
    let program = Arc::new(compile(dir, Style::Plain).map_err(DemoError::Logic)?);
    let (ping, ping_source) = calloop::ping::make_ping()?;
    let worker =
        TextWorker::spawn_with_waker(FontConfig::default(), Some(Box::new(move || ping.ping())))
            .map_err(DemoError::Text)?;
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    renderer.set_first_frame_wait(FIRST_FRAME_TEXT_WAIT);
    let (to_logic, from_main) = mpsc::channel::<ToLogic>();
    let host = Host::new(renderer, log.damage).forwarding(to_logic.clone());
    let mut mgr = SurfaceManager::connect(host, Config::default())?;
    let handle = mgr.loop_handle();
    handle
        .insert_source(ping_source, |_, _, state| {
            state.host_mut().renderer.update();
            state.poll();
        })
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;
    let (tx, rx) = calloop::channel::channel::<SceneDiff>();
    let dir = dir.to_path_buf();
    let wake = to_logic.clone();
    drop(to_logic);
    let logic = std::thread::Builder::new()
        .name("strand-logic".into())
        .spawn(move || logic(program, dir, from_main, wake, tx))?;
    let hung_up = Rc::new(Cell::new(false));
    let flag = Rc::clone(&hung_up);
    handle
        .insert_source(rx, move |event, _, state| match event {
            Event::Msg(diff) => apply(state, diff),
            Event::Closed => flag.set(true),
        })
        .map_err(|e| DemoError::Io(std::io::Error::other(e.error)))?;
    while !hung_up.get() {
        match mgr.dispatch(None) {
            Ok(()) => {}
            Err(e) if connection_closed(&e) => {
                log::info!("the compositor went away: {e}");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
    match logic.join() {
        Ok(Ok(())) => Err(DemoError::Logic("ended".into())),
        Ok(Err(e)) => Err(DemoError::Logic(e)),
        Err(_) => Err(DemoError::Logic("panicked".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, name: &str) -> ScreenInfo {
        ScreenInfo {
            id: id.into(),
            name: name.into(),
            make: "Make".into(),
            model: id.into(),
            description: String::new(),
            scale: 1.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    /// The logic side of `strand run` without Wayland: monitors arrive as
    /// messages, each gets its bar, a forgotten one is dropped, and the
    /// loop ends when the main thread goes away.
    #[test]
    fn the_logic_thread_follows_the_monitors() {
        let dir = std::env::temp_dir().join(format!("strand-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bar.strand"),
            "state n = 0\nbar Top {\n  opacity: hover ? 0.5 : 1\n  height: self.width > 1000 ? 40 : 32\n  on click { n += 1 }\n  text join(\" \", screen.name, n)\n}\n",
        )
        .unwrap();
        let program = Arc::new(compile(&dir, Style::Plain).unwrap());
        let (to_logic, from_main) = mpsc::channel();
        let (tx, rx) = calloop::channel::channel::<SceneDiff>();
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        let wake = to_logic.clone();
        let d = dir.clone();
        let t = std::thread::spawn(move || logic(program, d, from_main, wake, tx));
        let mut mirror = strand_compiler::instantiate::SceneMirror::new();
        let recv = |mirror: &mut strand_compiler::instantiate::SceneMirror| {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match rx.try_recv() {
                    Ok(diff) => {
                        mirror.apply(&diff).unwrap();
                        return;
                    }
                    Err(_) => {
                        assert!(Instant::now() < deadline, "no diff");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        };
        recv(&mut mirror);
        assert_eq!(mirror.texts(), ["DP-1 0"]);
        // Surface input and size reach the bar's node.
        let bar = mirror.roots()[0];
        for msg in [
            ToLogic::Event {
                node: bar,
                name: "click",
                args: Vec::new(),
            },
            ToLogic::Hover {
                node: bar,
                on: true,
            },
            ToLogic::Size {
                node: bar,
                width: 1920.0,
                height: 32.0,
            },
        ] {
            to_logic.send(msg).unwrap();
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let done = |m: &strand_compiler::instantiate::SceneMirror| {
            m.texts() == ["DP-1 1"]
                && m.prop(bar, strand_scene::Prop::Opacity)
                    == Some(&strand_scene::PropValue::Number(0.5))
                && m.prop(bar, strand_scene::Prop::Height)
                    == Some(&strand_scene::PropValue::Number(40.0))
        };
        while !done(&mirror) {
            assert!(Instant::now() < deadline, "{}", mirror.render());
            recv(&mut mirror);
        }
        to_logic
            .send(ToLogic::Screens(vec![
                screen("A", "DP-1"),
                screen("B", "HDMI-A-1"),
            ]))
            .unwrap();
        recv(&mut mirror);
        assert_eq!(mirror.roots().len(), 2);
        // B unplugged (parked), then forgotten.
        to_logic
            .send(ToLogic::Screens(vec![screen("A", "DP-1")]))
            .unwrap();
        recv(&mut mirror);
        assert_eq!(mirror.roots().len(), 1);
        to_logic.send(ToLogic::Forget("B".into())).unwrap();
        drop(to_logic);
        // The wake hook keeps a sender: the loop ends when diffs can no
        // longer be sent.
        drop(rx);
        let _ = t;
        let _ = std::fs::remove_dir_all(dir);
    }
}
