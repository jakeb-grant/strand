//! (M4) Drag and drop over `wl_data_device` on a headless sway (design.md,
//! "Drag and drop"): another program's text and files dropped on a
//! Strand surface, refused while nothing takes them, and a Strand drag
//! carried by the compositor from one Strand surface to another, or let
//! go where nothing takes it.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{BAR, Sway, WAIT, bar_spec, layer_spec};
use strand_scene::{
    Damage, DropKind, DropPayload, NodeId, NodeKind, PaintTarget, Painter, SurfaceChange, SurfaceId,
};
use strand_surface::{ButtonState, Config, InputEvent, SurfaceHost, SurfaceManager};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_data_device, wl_data_device::WlDataDevice,
    wl_data_offer::WlDataOffer, wl_data_source, wl_data_source::WlDataSource, wl_pointer,
    wl_pointer::WlPointer, wl_registry, wl_seat, wl_seat::WlSeat, wl_shm, wl_shm::WlShm,
    wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

// ---- the Strand side ---------------------------------------------------------

/// Paints its surfaces white once; takes drops on the surfaces in
/// `accept`, and has a `drag:` source in flight on `source`'s surface (as
/// the Router would after a press and a move).
#[derive(Default)]
struct DndHost {
    painted: HashSet<SurfaceId>,
    input: Vec<InputEvent>,
    accept: HashSet<SurfaceId>,
    source: Option<(SurfaceId, NodeId)>,
}

impl Painter for DndHost {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        target.pixels.fill(0xff);
        self.painted.insert(surface);
        Damage::full(target.size)
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        !self.painted.contains(&surface)
    }
}

impl SurfaceHost for DndHost {
    fn input(&mut self, event: &InputEvent) {
        self.input.push(event.clone());
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.painted.remove(&surface);
    }

    fn drop_accepted(&self, surface: SurfaceId) -> bool {
        self.accept.contains(&surface)
    }

    fn drag_source(&self, surface: SurfaceId) -> Option<NodeId> {
        self.source.filter(|s| s.0 == surface).map(|s| s.1)
    }
}

type Mgr = SurfaceManager<DndHost>;

fn wait(mgr: &mut Mgr, what: &str, done: impl Fn(&Mgr) -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done(mgr) {
        assert!(
            Instant::now() < deadline,
            "{what}: {:#?}",
            mgr.state().host().input
        );
        mgr.dispatch(Some(Duration::from_millis(20))).unwrap();
    }
}

fn shown(mgr: &mut Mgr, node: NodeId) -> SurfaceId {
    wait(mgr, "shown", |m| {
        m.state()
            .surfaces_of(node)
            .first()
            .and_then(|s| m.state().surface(*s))
            .is_some_and(|i| i.configured && i.stats.commits > 0)
    });
    mgr.state().surfaces_of(node)[0]
}

fn drops(host: &DndHost) -> Vec<(SurfaceId, DropPayload)> {
    host.input
        .iter()
        .filter_map(|e| match e {
            InputEvent::DragDrop {
                surface, payload, ..
            } => Some((*surface, payload.clone())),
            _ => None,
        })
        .collect()
}

// ---- a seat pointer --------------------------------------------------------

struct Pointer {
    queue: EventQueue<Client>,
    pointer: ZwlrVirtualPointerV1,
    time: u32,
}

struct Client;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Client: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(Client: ignore ZwlrVirtualPointerV1);

impl Pointer {
    fn new(sway: &Sway) -> Self {
        let conn = sway.connect();
        let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        queue.roundtrip(&mut Client).unwrap();
        Pointer {
            queue,
            pointer,
            time: 1,
        }
    }

    fn tick(&mut self) -> u32 {
        self.time += 10;
        self.time
    }

    fn to(&mut self, x: u32, y: u32) {
        let t = self.tick();
        self.pointer.motion_absolute(t, x, y, 1920, 1080);
        self.pointer.frame();
        self.queue.roundtrip(&mut Client).unwrap();
    }

    fn button(&mut self, state: wl_pointer::ButtonState) {
        let t = self.tick();
        self.pointer.button(t, 0x110, state);
        self.pointer.frame();
        self.queue.roundtrip(&mut Client).unwrap();
    }
}

/// Moves the held pointer from `from` to `to` in steps, letting the
/// manager read what each step brought.
fn glide(mgr: &mut Mgr, p: &mut Pointer, from: (u32, u32), to: (u32, u32)) {
    const STEPS: u32 = 12;
    for i in 1..=STEPS {
        let lerp =
            |a: u32, b: u32| (a as f32 + (b as f32 - a as f32) * i as f32 / STEPS as f32) as u32;
        p.to(lerp(from.0, to.0), lerp(from.1, to.1));
        pump_mgr(mgr, Duration::from_millis(25));
    }
}

fn pump_mgr(mgr: &mut Mgr, d: Duration) {
    let _ = mgr.dispatch_until(d, |_| false).unwrap();
}

// ---- another program -------------------------------------------------------

/// What the other program's drags offer: MIME types and their bytes.
type Offers = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// What the other program did.
#[derive(Default, Debug)]
struct Log {
    mapped: bool,
    started: u32,
    sent: Vec<String>,
    cancelled: u32,
    finished: u32,
}

/// Another program: a 300 px layer surface along the bottom edge whose
/// every press starts a drag offering `offers` (MIME type, bytes).
struct Program {
    log: Arc<Mutex<Log>>,
    offers: Offers,
    qh: QueueHandle<Program>,
    shm: WlShm,
    surface: WlSurface,
    ddm: WlDataDeviceManager,
    device: WlDataDevice,
    pointer: Option<WlPointer>,
    source: Option<WlDataSource>,
    file: PathBuf,
    buffer: Option<WlBuffer>,
}

struct ProgramHandle {
    log: Arc<Mutex<Log>>,
    offers: Offers,
}

impl ProgramHandle {
    fn offer(&self, offers: &[(&str, &[u8])]) {
        *self.offers.lock().unwrap() = offers
            .iter()
            .map(|(m, b)| ((*m).to_string(), b.to_vec()))
            .collect();
    }

    fn log<T>(&self, f: impl FnOnce(&Log) -> T) -> T {
        f(&self.log.lock().unwrap())
    }
}

impl Program {
    /// Runs it on a thread of its own (it ends when sway does).
    fn spawn(conn: Connection, file: PathBuf) -> ProgramHandle {
        let log = Arc::new(Mutex::new(Log::default()));
        let offers = Arc::new(Mutex::new(Vec::new()));
        let handle = ProgramHandle {
            log: log.clone(),
            offers: offers.clone(),
        };
        std::thread::spawn(move || {
            let (globals, mut queue) = registry_queue_init::<Program>(&conn).unwrap();
            let qh = queue.handle();
            let compositor: WlCompositor = globals.bind(&qh, 4..=5, ()).unwrap();
            let shm: WlShm = globals.bind(&qh, 1..=1, ()).unwrap();
            let layers: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ()).unwrap();
            let seat: WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
            let ddm: WlDataDeviceManager = globals.bind(&qh, 3..=3, ()).unwrap();
            let device = ddm.get_data_device(&seat, &qh, ());
            let surface = compositor.create_surface(&qh, ());
            let layer = layers.get_layer_surface(
                &surface,
                None,
                zwlr_layer_shell_v1::Layer::Top,
                "other-program".into(),
                &qh,
                (),
            );
            layer.set_anchor(
                zwlr_layer_surface_v1::Anchor::Bottom
                    | zwlr_layer_surface_v1::Anchor::Left
                    | zwlr_layer_surface_v1::Anchor::Right,
            );
            layer.set_size(0, 300);
            surface.commit();
            let mut p = Program {
                log,
                offers,
                qh,
                shm,
                surface,
                ddm,
                device,
                pointer: None,
                source: None,
                file,
                buffer: None,
            };
            while queue.blocking_dispatch(&mut p).is_ok() {}
        });
        handle
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Program {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Program {
    fn event(
        p: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure {
            serial,
            width,
            height,
        } = event
        {
            layer.ack_configure(serial);
            let (w, h) = (width.max(1) as i32, height.max(1) as i32);
            let size = (w * h * 4) as usize;
            let mut f = std::fs::File::options()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&p.file)
                .unwrap();
            f.write_all(&vec![0xff; size]).unwrap();
            let pool = p.shm.create_pool(f.as_fd(), size as i32, qh, ());
            let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
            pool.destroy();
            p.surface.attach(Some(&buffer), 0, 0);
            p.surface.damage_buffer(0, 0, w, h);
            p.surface.commit();
            p.buffer = Some(buffer);
            p.log.lock().unwrap().mapped = true;
        }
    }
}

impl Dispatch<WlSeat, ()> for Program {
    fn event(
        p: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(c),
        } = event
            && c.contains(wl_seat::Capability::Pointer)
            && p.pointer.is_none()
        {
            p.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for Program {
    fn event(
        p: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Button {
            serial,
            state: WEnum::Value(wl_pointer::ButtonState::Pressed),
            ..
        } = event
        {
            let source = p.ddm.create_data_source(&p.qh, ());
            for (mime, _) in p.offers.lock().unwrap().iter() {
                source.offer(mime.clone());
            }
            source.set_actions(DndAction::Copy | DndAction::Move);
            p.device.start_drag(Some(&source), &p.surface, None, serial);
            p.source = Some(source);
            p.log.lock().unwrap().started += 1;
        }
    }
}

impl Dispatch<WlDataSource, ()> for Program {
    fn event(
        p: &mut Self,
        source: &WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let bytes = p
                    .offers
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(m, _)| *m == mime_type)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default();
                let mut f = std::fs::File::from(fd);
                let _ = f.write_all(&bytes);
                p.log.lock().unwrap().sent.push(mime_type);
            }
            wl_data_source::Event::Cancelled => {
                source.destroy();
                p.log.lock().unwrap().cancelled += 1;
            }
            wl_data_source::Event::DndFinished => {
                source.destroy();
                p.log.lock().unwrap().finished += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Program {
    fn event(
        _: &mut Self,
        _: &WlDataDevice,
        _: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(Program, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ())
    ]);
}

delegate_noop!(Program: WlCompositor);
delegate_noop!(Program: ignore WlSurface);
delegate_noop!(Program: ignore WlShm);
delegate_noop!(Program: WlShmPool);
delegate_noop!(Program: ignore WlBuffer);
delegate_noop!(Program: ZwlrLayerShellV1);
delegate_noop!(Program: WlDataDeviceManager);
delegate_noop!(Program: ignore WlDataOffer);

/// Starts sway, a manager showing the bar (36 px along the top) and the
/// other program along the bottom, and a seat pointer.
fn desk(test: &str) -> Option<(Sway, Mgr, SurfaceId, Pointer, ProgramHandle)> {
    let sway = Sway::start(test)?;
    let mut mgr =
        SurfaceManager::with_connection(sway.connect(), DndHost::default(), Config::default())
            .expect("surface manager starts");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let bar = shown(&mut mgr, BAR);
    let pointer = Pointer::new(&sway);
    let program = Program::spawn(sway.connect(), sway.dir().join("program.buf"));
    wait(&mut mgr, "the other program shows", |_| {
        program.log(|l| l.mapped)
    });
    pump_mgr(&mut mgr, Duration::from_millis(200));
    Some((sway, mgr, bar, pointer, program))
}

/// Presses on the other program at the bottom and drags to `to`, then
/// lets go.
fn drag_in(mgr: &mut Mgr, p: &mut Pointer, program: &ProgramHandle, to: (u32, u32)) {
    let started = program.log(|l| l.started);
    let from = (960, 950);
    p.to(from.0, from.1);
    p.button(wl_pointer::ButtonState::Pressed);
    wait(mgr, "the other program starts its drag", |_| {
        program.log(|l| l.started) > started
    });
    glide(mgr, p, from, to);
    p.button(wl_pointer::ButtonState::Released);
}

/// Text and files from another program: the drag entering the bar is a
/// `DragEnter` saying what it carries, and dropped on a surface that
/// takes it, the bytes are read (as UTF-8 text, the preferred type) and
/// arrive as `DragDrop`, and the offer is finished (the program sees
/// `dnd_finished`). Over a surface that takes nothing, the offer is never
/// accepted: the compositor cancels the drag and no drop arrives. A file
/// list arrives as paths.
#[test]
fn another_programs_text_and_files_drop_on_a_strand_surface() {
    let Some((_sway, mut mgr, bar, mut p, program)) =
        desk("another_programs_text_and_files_drop_on_a_strand_surface")
    else {
        return;
    };
    // Text, taken.
    program.offer(&[
        ("text/plain", b"plain"),
        (
            "text/plain;charset=utf-8",
            "hello from afar \u{2014}".as_bytes(),
        ),
    ]);
    mgr.state_mut().host_mut().accept.insert(bar);
    drag_in(&mut mgr, &mut p, &program, (960, 18));
    wait(&mut mgr, "a text drop", |m| {
        !drops(m.state().host()).is_empty()
    });
    let host = mgr.state().host();
    assert!(
        host.input.iter().any(|e| matches!(
            e,
            InputEvent::DragEnter { surface, kinds, .. }
                if *surface == bar && kinds == &[DropKind::Text]
        )),
        "{:#?}",
        host.input
    );
    assert!(
        host.input
            .iter()
            .any(|e| matches!(e, InputEvent::DragMotion { surface, .. } if *surface == bar))
    );
    assert_eq!(
        drops(host),
        [(
            bar,
            DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "hello from afar \u{2014}".into(),
                app_id: None,
            }
        )]
    );
    let InputEvent::DragDrop { at, .. } = host
        .input
        .iter()
        .rfind(|e| matches!(e, InputEvent::DragDrop { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert!(
        (at.x - 960.0).abs() < 2.0 && (at.y - 18.0).abs() < 2.0,
        "{at:?}"
    );
    wait(&mut mgr, "the program sees the drop finished", |_| {
        program.log(|l| l.finished == 1)
    });
    assert_eq!(
        program.log(|l| l.sent.clone()),
        ["text/plain;charset=utf-8"]
    );

    // Refused: nothing on the bar takes it.
    mgr.state_mut().host_mut().accept.clear();
    mgr.state_mut().host_mut().input.clear();
    drag_in(&mut mgr, &mut p, &program, (960, 18));
    wait(&mut mgr, "the compositor cancels the drag", |_| {
        program.log(|l| l.cancelled == 1)
    });
    pump_mgr(&mut mgr, Duration::from_millis(200));
    assert!(
        mgr.state()
            .host()
            .input
            .iter()
            .any(|e| matches!(e, InputEvent::DragEnter { surface, .. } if *surface == bar)),
        "it was offered: {:#?}",
        mgr.state().host().input
    );
    assert!(
        drops(mgr.state().host()).is_empty(),
        "{:#?}",
        mgr.state().host().input
    );
    assert_eq!(program.log(|l| l.sent.len()), 1, "nothing read");

    // Files, taken.
    mgr.state_mut().host_mut().accept.insert(bar);
    mgr.state_mut().host_mut().input.clear();
    program.offer(&[(
        "text/uri-list",
        b"file:///tmp/a%20photo.png\r\nfile:///home/u/notes.txt\r\n",
    )]);
    drag_in(&mut mgr, &mut p, &program, (400, 10));
    wait(&mut mgr, "a file drop", |m| {
        !drops(m.state().host()).is_empty()
    });
    assert_eq!(
        drops(mgr.state().host()),
        [(
            bar,
            DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/tmp/a photo.png".into(), "/home/u/notes.txt".into()],
                text: String::new(),
                app_id: None,
            }
        )]
    );
    assert!(mgr.state().host().input.iter().any(|e| matches!(
        e,
        InputEvent::DragEnter { kinds, .. } if kinds == &[DropKind::Files]
    )));
    wait(&mut mgr, "finished", |_| program.log(|l| l.finished == 2));
}

/// A Strand drag that leaves its surface with the button held is handed
/// to the compositor: it enters another Strand surface as ours (no
/// kinds), and dropped there arrives as the source's node, with nothing
/// read; the origin then gets a release far outside it, which ends the
/// Router's drag. Let go where no Strand surface is, the drag is
/// cancelled, the origin gets the same release, and nothing drops.
#[test]
fn a_strand_drag_moves_between_strand_surfaces() {
    let Some((_sway, mut mgr, bar, mut p, _program)) =
        desk("a_strand_drag_moves_between_strand_surfaces")
    else {
        return;
    };
    const PANEL: NodeId = NodeId::new(5, 0);
    const PIN: NodeId = NodeId::new(42, 7);
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Created(layer_spec(NodeKind::Panel, "Dock", "center", 400.0, 300.0)),
    );
    let panel = shown(&mut mgr, PANEL);
    pump_mgr(&mut mgr, Duration::from_millis(200));
    mgr.state_mut().host_mut().accept.insert(panel);

    let far = |e: &InputEvent| {
        matches!(
            e,
            InputEvent::PointerButton { surface, position, state: ButtonState::Released, .. }
                if *surface == bar && position.x < -1000.0 && position.y < -1000.0
        )
    };
    // Pressed on the bar, a drag of PIN in flight there (the Router's).
    p.to(100, 18);
    pump_mgr(&mut mgr, Duration::from_millis(50));
    p.button(wl_pointer::ButtonState::Pressed);
    pump_mgr(&mut mgr, Duration::from_millis(50));
    mgr.state_mut().host_mut().source = Some((bar, PIN));
    p.to(100, 30);
    pump_mgr(&mut mgr, Duration::from_millis(50));
    assert!(!mgr.state().carrying_drag(), "still inside the bar");
    // Below the bar: the compositor takes it.
    p.to(100, 60);
    wait(&mut mgr, "the drag is handed over", |m| {
        m.state().carrying_drag()
    });
    glide(&mut mgr, &mut p, (100, 60), (960, 540));
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "dropped on the panel", |m| {
        !drops(m.state().host()).is_empty() && m.state().host().input.iter().any(far)
    });
    let host = mgr.state().host();
    assert!(
        host.input.iter().any(|e| matches!(
            e,
            InputEvent::DragEnter { surface, kinds, .. } if *surface == panel && kinds.is_empty()
        )),
        "{:#?}",
        host.input
    );
    assert_eq!(drops(host), [(panel, DropPayload::Node(PIN))]);
    let drop_at = host
        .input
        .iter()
        .position(|e| matches!(e, InputEvent::DragDrop { .. }))
        .unwrap();
    assert!(
        host.input[drop_at..].iter().any(far),
        "the origin's drag ends after the drop"
    );
    let InputEvent::DragDrop { at, .. } = &host.input[drop_at] else {
        unreachable!()
    };
    assert!(
        (at.x - 200.0).abs() < 2.0 && (at.y - 132.0).abs() < 2.0,
        "the panel's middle (the bar's exclusive zone moves the panel 18 px down): {at:?}"
    );
    assert!(!mgr.state().carrying_drag());

    // Again, let go over the desktop: cancelled, nothing dropped.
    mgr.state_mut().host_mut().source = None;
    mgr.state_mut().host_mut().input.clear();
    p.to(100, 18);
    pump_mgr(&mut mgr, Duration::from_millis(50));
    p.button(wl_pointer::ButtonState::Pressed);
    pump_mgr(&mut mgr, Duration::from_millis(50));
    mgr.state_mut().host_mut().source = Some((bar, PIN));
    p.to(100, 60);
    wait(&mut mgr, "handed over again", |m| m.state().carrying_drag());
    glide(&mut mgr, &mut p, (100, 60), (100, 500));
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "the origin's drag ends", |m| {
        m.state().host().input.iter().any(far)
    });
    pump_mgr(&mut mgr, Duration::from_millis(200));
    assert!(
        drops(mgr.state().host()).is_empty(),
        "{:#?}",
        mgr.state().host().input
    );
    assert!(!mgr.state().carrying_drag());
}
