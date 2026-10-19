//! The live sandbox's window (docs/eval.md, "Live sandbox"): one
//! xdg-shell toplevel drawn with `wl_shm`, through smithay-client-toolkit
//! (the version the surface manager links). Its keys and pointer become
//! [`Live`] commands; so does each line read from stdin. The session's
//! clock runs at its speed between the window's events; files the config
//! is read from are checked for edits every 300 ms.
//!
//! It connects to `$WAYLAND_DISPLAY` like any client; the tests run it on
//! a headless sway of their own.

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use crate::Error;
use crate::live::view;
use crate::live::{Key, Live};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// How often the config's files are checked for edits.
const POLL: Duration = Duration::from_millis(300);
/// While playing, the verdicts are made again at most this often (and
/// whenever the session pauses or a segment comes to rest).
const ANALYSE: Duration = Duration::from_secs(2);

struct App {
    live: Live,
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    shm: Shm,
    pool: SlotPool,
    buffer: Option<Buffer>,
    window: Window,
    width: u32,
    height: u32,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    shift: bool,
    /// A button pressed on the stage (its release goes there too).
    pressed: Vec<&'static str>,
    exit: bool,
}

fn run_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Run(format!("{what}: {e}"))
}

/// Shows `live` until it quits or its window is closed.
pub fn run(live: Live) -> Result<(), Error> {
    let conn = Connection::connect_to_env()
        .map_err(|e| run_err("no Wayland display to show the sandbox on", e))?;
    let (globals, queue) = registry_queue_init(&conn).map_err(|e| run_err("the registry", e))?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().map_err(|e| run_err("the event loop", e))?;
    WaylandSource::new(conn.clone(), queue)
        .insert(event_loop.handle())
        .map_err(|e| run_err("the Wayland source", e))?;
    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| run_err("wl_compositor", e))?;
    let xdg = XdgShell::bind(&globals, &qh).map_err(|e| run_err("xdg_wm_base", e))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| run_err("wl_shm", e))?;
    let surface = compositor.create_surface(&qh);
    let window = xdg.create_window(surface, WindowDecorations::RequestServer, &qh);
    let name = live
        .session
        .opts
        .path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    window.set_title(format!("strand-eval live: {name}"));
    window.set_app_id("strand-eval-live");
    window.set_min_size(Some((480, 320)));
    window.commit();
    let pool = SlotPool::new(1280 * 800 * 4, &shm).map_err(|e| run_err("the shm pool", e))?;
    let mut app = App {
        live,
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),
        shm,
        pool,
        buffer: None,
        window,
        width: 1280,
        height: 800,
        configured: false,
        frame_pending: false,
        dirty: true,
        keyboard: None,
        pointer: None,
        shift: false,
        pressed: Vec::new(),
        exit: false,
    };

    // Commands from stdin, one per line; each answer is a JSON line on
    // stdout. Its end leaves the window open.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    println!(
        "{}",
        serde_json::json!({"ready": true, "status": app.live.status()})
    );
    let _ = std::io::stdout().flush();

    let mut last = Instant::now();
    let mut polled = Instant::now();
    let mut analysed = Instant::now();
    while !app.exit && !app.live.quit {
        let wait = app
            .live
            .session
            .until_next()
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        event_loop
            .dispatch(Some(wait), &mut app)
            .map_err(|e| run_err("the event loop", e))?;
        while let Ok(line) = rx.try_recv() {
            let answer = app.live.command(&line);
            println!("{answer}");
            let _ = std::io::stdout().flush();
            app.dirty = true;
        }
        if polled.elapsed() >= POLL {
            polled = Instant::now();
            if app.live.session.changed() {
                if let Err(e) = app.live.session.reload() {
                    app.live.session.msg = Some((format!("reload: {e}"), true));
                    // Not again until the file changes once more.
                    app.live.session.forget_edit();
                }
                app.dirty = true;
            }
        }
        let now = Instant::now();
        match app.live.session.play(now - last) {
            Ok(true) => app.dirty = true,
            Ok(false) => {}
            Err(e) => {
                app.live.session.msg = Some((e.to_string(), true));
                app.live.session.paused = true;
                app.dirty = true;
            }
        }
        last = now;
        let s = &mut app.live.session;
        if s.stale() && (s.paused || analysed.elapsed() >= ANALYSE) {
            s.analyse();
            analysed = Instant::now();
            app.dirty = true;
        }
        if app.configured && app.dirty && !app.frame_pending {
            app.draw(&qh);
        }
    }
    Ok(())
}

impl App {
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (w, h) = (self.width, self.height);
        let stride = w as i32 * 4;
        let pool = &mut self.pool;
        let buffer = match self.buffer.take() {
            Some(b) => b,
            None => {
                match pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888) {
                    Ok((b, _)) => b,
                    Err(e) => {
                        self.live.session.msg = Some((format!("shm buffer: {e}"), true));
                        return;
                    }
                }
            }
        };
        let buffer = if pool.canvas(&buffer).is_some() {
            buffer
        } else {
            // The compositor still holds it: a second one.
            match pool.create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888) {
                Ok((b, _)) => b,
                Err(e) => {
                    self.live.session.msg = Some((format!("shm buffer: {e}"), true));
                    self.buffer = Some(buffer);
                    return;
                }
            }
        };
        let Some(canvas) = pool.canvas(&buffer) else {
            self.buffer = Some(buffer);
            return;
        };
        self.live.layout = view::draw(&self.live.session, &self.live.view, canvas, w, h);
        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        if buffer.attach_to(surface).is_err() {
            self.buffer = Some(buffer);
            return;
        }
        self.window.commit();
        self.buffer = Some(buffer);
        self.frame_pending = true;
        self.dirty = false;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let w = configure.new_size.0.map_or(1280, |v| v.get());
        let h = configure.new_size.1.map_or(800, |v| v.get());
        if (w, h) != (self.width, self.height) {
            self.buffer = None;
        }
        self.width = w;
        self.height = h;
        self.configured = true;
        self.dirty = true;
        // A configure wants a frame back whatever the frame callback.
        self.frame_pending = false;
        self.draw(qh);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => self.live.session.msg = Some((format!("keyboard: {e}"), true)),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat.get_pointer(qh, &seat) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => self.live.session.msg = Some((format!("pointer: {e}"), true)),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if capability == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

/// The key a keysym (and its text) is to the sandbox.
fn key_of(event: &KeyEvent) -> Option<Key> {
    Some(match event.keysym {
        Keysym::space => Key::Space,
        Keysym::Left => Key::Left,
        Keysym::Right => Key::Right,
        Keysym::Up => Key::Up,
        Keysym::Down => Key::Down,
        Keysym::Home => Key::Home,
        Keysym::End => Key::End,
        Keysym::Escape => Key::Escape,
        Keysym::Return | Keysym::KP_Enter => Key::Return,
        Keysym::BackSpace => Key::Backspace,
        Keysym::F5 => Key::Reload,
        _ => {
            let c = event.utf8.as_deref()?.chars().next()?;
            if c.is_control() {
                return None;
            }
            Key::Char(c)
        }
    })
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.live.focused = true;
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.live.focused = false;
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(k) = key_of(&event) {
            self.live.key(k, self.shift);
            self.dirty = true;
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.shift = modifiers.shift;
    }
}

fn button_name(code: u32) -> Option<&'static str> {
    match code {
        BTN_LEFT => Some("left"),
        BTN_RIGHT => Some("right"),
        BTN_MIDDLE => Some("middle"),
        _ => None,
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.window.wl_surface() {
                continue;
            }
            let (x, y) = event.position;
            let s = &mut self.live.session;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    // Off the stage (on the strip) is off the shell. A
                    // past frame is looked at, not driven.
                    if s.at_head() {
                        s.aim(self.live.layout.screen_at(x, y));
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if s.at_head() {
                        s.aim(None);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if let Some(a) = self.live.layout.action_at(x, y) {
                        if button == BTN_LEFT {
                            self.live.action(a);
                        }
                    } else if self.live.layout.screen_at(x, y).is_some() {
                        if !s.at_head() {
                            // A click on a past frame comes back to now.
                            s.head();
                        } else if let Some(b) = button_name(button) {
                            s.aim(self.live.layout.screen_at(x, y));
                            match s.button(b, true) {
                                Ok(()) => self.pressed.push(b),
                                Err(e) => s.msg = Some((e.to_string(), true)),
                            }
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if let Some(b) = button_name(button)
                        && let Some(i) = self.pressed.iter().position(|p| *p == b)
                    {
                        self.pressed.remove(i);
                        if let Err(e) = s.button(b, false) {
                            s.msg = Some((e.to_string(), true));
                        }
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    if self.live.layout.screen_at(x, y).is_none() || !s.at_head() {
                        continue;
                    }
                    let notches = if vertical.value120 != 0 {
                        f64::from(vertical.value120) / 120.0
                    } else if vertical.discrete != 0 {
                        f64::from(vertical.discrete)
                    } else {
                        vertical.absolute / strand_render::WHEEL_STEP
                    };
                    if notches.abs() > 1e-3
                        && let Err(e) = s.scroll(notches)
                    {
                        s.msg = Some((e.to_string(), true));
                    }
                }
            }
            self.dirty = true;
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
