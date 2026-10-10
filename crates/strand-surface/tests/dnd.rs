//! (M4) Drag and drop over `wl_data_device` on a headless sway (design.md,
//! "Drag and drop"): another program's text and files dropped on a
//! Strand surface, refused while nothing takes them, and a Strand drag
//! carried by the compositor from one Strand surface to another, or let
//! go where nothing takes it; (M4 interaction-finish) a Strand drag out
//! to another program carrying its text or files, and a drag from
//! another Strand process recognised and read.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    wl_data_offer, wl_data_offer::WlDataOffer, wl_data_source, wl_data_source::WlDataSource,
    wl_pointer, wl_pointer::WlPointer, wl_registry, wl_seat, wl_seat::WlSeat, wl_shm,
    wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
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
    /// What the source gives other programs.
    export: Option<DropPayload>,
    /// Its icon.
    icon: Option<strand_scene::DragImage>,
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

    fn drag_image(&mut self, _: SurfaceId, node: NodeId) -> Option<strand_scene::DragImage> {
        self.source
            .filter(|s| s.1 == node)
            .and_then(|_| self.icon.clone())
    }

    fn drag_data(&self, node: NodeId) -> Option<DropPayload> {
        self.source
            .filter(|s| s.1 == node)
            .and_then(|_| self.export.clone())
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

/// What the other program did. Each drag counts once, as the first of
/// `cancelled` and `dnd_finished` its source got: under load a source
/// can see a `cancelled` after its `dnd_finished` (the finished offer
/// going away), and that is not a second drag ending.
#[derive(Default, Debug)]
struct Log {
    mapped: bool,
    started: u32,
    sent: Vec<String>,
    cancelled: u32,
    finished: u32,
    ended: HashSet<wayland_client::backend::ObjectId>,
    /// Drops it took: the MIME type read and its bytes.
    received: Vec<(String, Vec<u8>)>,
    /// Drops it asked for while hanging: the pipe held, never read.
    unread: Vec<std::io::PipeReader>,
}

/// Another program: a 300 px layer surface along the bottom edge whose
/// every press starts a drag offering `offers` (MIME type, bytes).
struct Program {
    log: Arc<Mutex<Log>>,
    offers: Offers,
    hang: Arc<AtomicBool>,
    /// Pipes asked for while hanging: kept open, never written.
    held: Vec<std::fs::File>,
    qh: QueueHandle<Program>,
    shm: WlShm,
    surface: WlSurface,
    ddm: WlDataDeviceManager,
    device: WlDataDevice,
    pointer: Option<WlPointer>,
    source: Option<WlDataSource>,
    file: PathBuf,
    buffer: Option<WlBuffer>,
    /// The MIME type it takes from drags entering it (`None`: none).
    want: Arc<Mutex<Option<String>>>,
    /// The offer over it now.
    over: Option<WlDataOffer>,
    /// The MIME types the newest offer named.
    offered: Vec<String>,
}

struct ProgramHandle {
    log: Arc<Mutex<Log>>,
    offers: Offers,
    want: Arc<Mutex<Option<String>>>,
    /// While set, a drop's pipe is held open and never written.
    hang: Arc<AtomicBool>,
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
        let hang = Arc::new(AtomicBool::new(false));
        let want = Arc::new(Mutex::new(None));
        let handle = ProgramHandle {
            log: log.clone(),
            offers: offers.clone(),
            want: want.clone(),
            hang: hang.clone(),
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
                hang,
                held: Vec::new(),
                qh,
                shm,
                surface,
                ddm,
                device,
                pointer: None,
                source: None,
                file,
                buffer: None,
                want,
                over: None,
                offered: Vec::new(),
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
            wl_data_source::Event::Send { mime_type, fd } if p.hang.load(Ordering::SeqCst) => {
                p.held.push(std::fs::File::from(fd));
                p.log.lock().unwrap().sent.push(mime_type);
            }
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
                let mut log = p.log.lock().unwrap();
                if log.ended.insert(source.id()) {
                    log.cancelled += 1;
                }
            }
            wl_data_source::Event::DndFinished => {
                source.destroy();
                let mut log = p.log.lock().unwrap();
                if log.ended.insert(source.id()) {
                    log.finished += 1;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Program {
    /// A drag entering it is accepted (copy) as the type it wants; one
    /// dropped on it is read whole, then finished.
    fn event(
        p: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let want = p
            .want
            .lock()
            .unwrap()
            .clone()
            .filter(|w| p.offered.contains(w));
        match event {
            wl_data_device::Event::DataOffer { .. } => p.offered.clear(),
            wl_data_device::Event::Enter {
                serial,
                id: Some(offer),
                ..
            } => {
                offer.accept(serial, want);
                offer.set_actions(DndAction::Copy, DndAction::Copy);
                p.over = Some(offer);
            }
            wl_data_device::Event::Leave => p.over = None,
            wl_data_device::Event::Drop => {
                let (Some(offer), Some(mime)) = (p.over.take(), want) else {
                    return;
                };
                let (mut read, write) = std::io::pipe().unwrap();
                offer.receive(mime.clone(), write.as_fd());
                drop(write);
                conn.flush().unwrap();
                if p.hang.load(Ordering::SeqCst) {
                    p.log.lock().unwrap().unread.push(read);
                    return;
                }
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut read, &mut bytes).unwrap();
                offer.finish();
                offer.destroy();
                p.log.lock().unwrap().received.push((mime, bytes));
            }
            _ => {}
        }
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
impl Dispatch<WlDataOffer, ()> for Program {
    fn event(
        p: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            p.offered.push(mime_type);
        }
    }
}

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

/// A drop that cannot be read lands nowhere: more than `MAX_DROP_BYTES`,
/// or a sender that never closes its pipe (given up after
/// `DROP_READ_TIMEOUT`), ends with `DragLeave` and no `DragDrop`, and
/// the offer is destroyed unfinished, which the compositor tells the
/// other program as `cancelled` (never `dnd_finished`).
#[test]
fn a_drop_that_cannot_be_read_lands_nowhere() {
    // dnd.rs's limits (the module is the manager's own, not exported).
    const MAX_DROP_BYTES: usize = 4 << 20;
    const DROP_READ_TIMEOUT: Duration = Duration::from_secs(5);
    let Some((_sway, mut mgr, bar, mut p, program)) =
        desk("a_drop_that_cannot_be_read_lands_nowhere")
    else {
        return;
    };
    mgr.state_mut().host_mut().accept.insert(bar);
    let left = |m: &Mgr| {
        m.state()
            .host()
            .input
            .iter()
            .any(|e| matches!(e, InputEvent::DragLeave { surface } if *surface == bar))
    };
    // Too much.
    let big = vec![b'x'; MAX_DROP_BYTES + 1];
    program.offer(&[("text/plain", &big)]);
    drag_in(&mut mgr, &mut p, &program, (960, 18));
    wait(&mut mgr, "the program sees the drop cancelled", |_| {
        program.log(|l| l.cancelled == 1)
    });
    wait(&mut mgr, "the host sees the offer leave", left);
    assert!(drops(mgr.state().host()).is_empty());
    assert_eq!(program.log(|l| (l.sent.len(), l.finished)), (1, 0));

    // Never closed: given up after the time limit.
    mgr.state_mut().host_mut().input.clear();
    program.offer(&[("text/plain", b"never sent")]);
    program.hang.store(true, Ordering::SeqCst);
    drag_in(&mut mgr, &mut p, &program, (960, 18));
    wait(&mut mgr, "the program is asked for the bytes", |_| {
        program.log(|l| l.sent.len() == 2)
    });
    let deadline = Instant::now() + DROP_READ_TIMEOUT + WAIT;
    while !(left(&mgr) && program.log(|l| l.cancelled == 2)) {
        assert!(
            Instant::now() < deadline,
            "the read is given up: {:#?}",
            mgr.state().host().input
        );
        mgr.dispatch(Some(Duration::from_millis(20))).unwrap();
    }
    assert!(drops(mgr.state().host()).is_empty());
    assert_eq!(program.log(|l| l.finished), 0);
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

/// Presses on the bar at x 100 with a drag of `node` in flight there (the
/// Router's), and moves below it, so the compositor carries the drag.
fn drag_out_of_bar(mgr: &mut Mgr, p: &mut Pointer, bar: SurfaceId, node: NodeId) {
    p.to(100, 18);
    pump_mgr(mgr, Duration::from_millis(50));
    p.button(wl_pointer::ButtonState::Pressed);
    pump_mgr(mgr, Duration::from_millis(50));
    mgr.state_mut().host_mut().source = Some((bar, node));
    p.to(100, 60);
    wait(mgr, "the drag is handed over", |m| {
        m.state().carrying_drag()
    });
}

/// (M4) A Strand drag out to another program carries its data: text
/// dropped on the other program is read there as UTF-8 text, files as a
/// `text/uri-list` of percent-encoded URIs (what our own drop reading
/// takes back as the same paths); each drop is finished, so the origin's
/// drag ends with the far release. A drag whose value has no form
/// outside Strand offers nothing the program can read: the compositor
/// cancels it.
#[test]
fn a_strand_drag_carries_its_data_to_another_program() {
    let Some((_sway, mut mgr, bar, mut p, program)) =
        desk("a_strand_drag_carries_its_data_to_another_program")
    else {
        return;
    };
    const PIN: NodeId = NodeId::new(42, 7);
    let far = |m: &Mgr| {
        m.state().host().input.iter().any(|e| {
            matches!(
                e,
                InputEvent::PointerButton { surface, position, state: ButtonState::Released, .. }
                    if *surface == bar && position.x < -1000.0
            )
        })
    };
    let cases: [(DropPayload, &str, &[u8]); 2] = [
        (
            DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "pinned \u{2014} note".into(),
                app_id: None,
            },
            "text/plain;charset=utf-8",
            "pinned \u{2014} note".as_bytes(),
        ),
        (
            DropPayload::External {
                kind: DropKind::Files,
                files: vec!["/tmp/a photo.png".into(), "/tmp/b".into()],
                text: String::new(),
                app_id: None,
            },
            "text/uri-list",
            b"file:///tmp/a%20photo.png\r\nfile:///tmp/b\r\n",
        ),
    ];
    for (i, (export, mime, bytes)) in cases.into_iter().enumerate() {
        mgr.state_mut().host_mut().input.clear();
        mgr.state_mut().host_mut().export = Some(export);
        *program.want.lock().unwrap() = Some(mime.to_string());
        drag_out_of_bar(&mut mgr, &mut p, bar, PIN);
        glide(&mut mgr, &mut p, (100, 60), (960, 950));
        p.button(wl_pointer::ButtonState::Released);
        wait(&mut mgr, "the other program reads the drop", |_| {
            program.log(|l| l.received.len() == i + 1)
        });
        wait(&mut mgr, "the origin's drag ends", far);
        let got = program.log(|l| l.received[i].clone());
        assert_eq!(got, (mime.to_string(), bytes.to_vec()));
        assert!(!mgr.state().carrying_drag());
        assert!(drops(mgr.state().host()).is_empty(), "not a drop of ours");
        mgr.state_mut().host_mut().source = None;
    }
    // Nothing to give: the program finds nothing it reads (only our
    // private type is offered), so it never accepts and the compositor
    // cancels the drag.
    mgr.state_mut().host_mut().input.clear();
    mgr.state_mut().host_mut().export = None;
    drag_out_of_bar(&mut mgr, &mut p, bar, PIN);
    glide(&mut mgr, &mut p, (100, 60), (960, 950));
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "the origin's drag ends", far);
    pump_mgr(&mut mgr, Duration::from_millis(200));
    assert_eq!(program.log(|l| l.received.len()), 2, "nothing read");
}

/// (m4-audit) A program that asks for a drag's export and never reads
/// it does not keep the write for the life of the process: past a pipe
/// buffer (about 64 KiB) the write waits, and after
/// `DRAG_WRITE_TIMEOUT` with no progress it is given up and the pipe
/// closed, so the program reads what fitted and then the end of file
/// (before the fix it read what fitted and then waited for good).
#[test]
fn an_unread_drag_export_is_given_up() {
    // dnd.rs's limit (the module is the manager's own, not exported).
    const DRAG_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
    let Some((_sway, mut mgr, bar, mut p, program)) = desk("an_unread_drag_export_is_given_up")
    else {
        return;
    };
    const PIN: NodeId = NodeId::new(42, 7);
    let text = "x".repeat(1 << 20);
    mgr.state_mut().host_mut().export = Some(DropPayload::External {
        kind: DropKind::Text,
        files: vec![],
        text: text.clone(),
        app_id: None,
    });
    *program.want.lock().unwrap() = Some("text/plain;charset=utf-8".into());
    program.hang.store(true, Ordering::SeqCst);
    drag_out_of_bar(&mut mgr, &mut p, bar, PIN);
    glide(&mut mgr, &mut p, (100, 60), (960, 950));
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "the other program asks for the export", |_| {
        program.log(|l| l.unread.len() == 1)
    });
    let mut pipe = program.log(|l| l.unread[0].try_clone().unwrap());
    rustix::io::ioctl_fionbio(&pipe, true).unwrap();
    // Reads what is there; true once the writer has closed its end.
    let mut got = 0usize;
    let mut drain = |pipe: &mut std::io::PipeReader| -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match std::io::Read::read(pipe, &mut buf) {
                Ok(0) => return true,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
                Err(e) => panic!("{e}"),
            }
        }
    };
    // The pipe fills, the write waits on it, and with no progress past
    // the time limit it is given up and the pipe closed. Unread until
    // then: reading would let the write go on.
    pump_mgr(&mut mgr, DRAG_WRITE_TIMEOUT + Duration::from_secs(1));
    let deadline = Instant::now() + DRAG_WRITE_TIMEOUT + WAIT;
    while !drain(&mut pipe) {
        assert!(Instant::now() < deadline, "the write is never given up");
        mgr.dispatch(Some(Duration::from_millis(20))).unwrap();
    }
    assert!(got > 0 && got < text.len(), "{got} of {}", text.len());
}

/// (m4-audit) The other side of the time limit: a large export (1 MiB,
/// many pipe buffers) to a program that does read it, but slowly (it
/// leaves the full pipe for 3 s at a time, longer in all than
/// `DRAG_WRITE_TIMEOUT`), is never cut off: each write that goes through
/// starts the limit again. The pipe stays open (no hang-up) through every
/// pause, and the program reads every byte and then the end of file.
#[test]
fn a_slowly_read_drag_export_is_written_whole() {
    use rustix::event::{PollFd, PollFlags, poll};
    const DRAG_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
    const PAUSE: Duration = Duration::from_secs(3);
    let Some((_sway, mut mgr, bar, mut p, program)) =
        desk("a_slowly_read_drag_export_is_written_whole")
    else {
        return;
    };
    const PIN: NodeId = NodeId::new(42, 7);
    let text = "y".repeat(1 << 20);
    mgr.state_mut().host_mut().export = Some(DropPayload::External {
        kind: DropKind::Text,
        files: vec![],
        text: text.clone(),
        app_id: None,
    });
    *program.want.lock().unwrap() = Some("text/plain;charset=utf-8".into());
    program.hang.store(true, Ordering::SeqCst);
    drag_out_of_bar(&mut mgr, &mut p, bar, PIN);
    glide(&mut mgr, &mut p, (100, 60), (960, 950));
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "the other program asks for the export", |_| {
        program.log(|l| l.unread.len() == 1)
    });
    let mut pipe = program.log(|l| l.unread[0].try_clone().unwrap());
    rustix::io::ioctl_fionbio(&pipe, true).unwrap();
    let mut got = 0usize;
    let drain = |pipe: &mut std::io::PipeReader, got: &mut usize| -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match std::io::Read::read(pipe, &mut buf) {
                Ok(0) => return true,
                Ok(n) => {
                    assert!(buf[..n].iter().all(|b| *b == b'y'));
                    *got += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
                Err(e) => panic!("{e}"),
            }
        }
    };
    // True if the writer has closed its end, without reading. The pipe
    // was drained before the pause, so a writer that stopped without
    // closing its end shows neither: a bounded wait fails then instead
    // of hanging the job.
    let hung_up = |pipe: &std::io::PipeReader, round: usize| -> bool {
        let mut fds = [PollFd::new(pipe, PollFlags::IN)];
        let bound = rustix::event::Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        let n = poll(&mut fds, Some(&bound)).unwrap();
        assert!(
            n > 0,
            "pause {round}: neither data nor a hang-up in 5 s (the writer stopped without closing)"
        );
        fds[0].revents().contains(PollFlags::HUP)
    };
    // Three pauses of 3 s, each with the pipe full and unread, a drain
    // between: 9 s in all, past the limit from the first write.
    let begun = Instant::now();
    for round in 0..3 {
        pump_mgr(&mut mgr, PAUSE);
        assert!(
            !hung_up(&pipe, round),
            "given up in pause {round}, {:?} in, {got} read",
            begun.elapsed()
        );
        assert!(
            !drain(&mut pipe, &mut got),
            "ended early: {got} of {}",
            text.len()
        );
    }
    assert!(begun.elapsed() > DRAG_WRITE_TIMEOUT);
    assert!(got < text.len(), "{got}: the export outlasts the pauses");
    // Read on at speed: the rest, then the end of file.
    let deadline = Instant::now() + WAIT;
    while !drain(&mut pipe, &mut got) {
        assert!(Instant::now() < deadline, "the rest never came: {got}");
        mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
    }
    assert_eq!(got, text.len());
}

/// (M4) A drag from another Strand process (another manager, on its own
/// connection, offering its own private type and what its value gives
/// other programs) is recognised and read like another program's: it
/// enters this one's panel as text and drops as a `Drop` with the text,
/// never as a node of ours. Both managers run on this thread, each
/// dispatched in turn.
#[test]
fn a_drag_from_another_strand_process_is_read_as_its_data() {
    let Some((sway, mut mgr, bar, mut p, _program)) =
        desk("a_drag_from_another_strand_process_is_read_as_its_data")
    else {
        return;
    };
    let mut other =
        SurfaceManager::with_connection(sway.connect(), DndHost::default(), Config::default())
            .expect("the other manager starts");
    const PANEL: NodeId = NodeId::new(5, 0);
    const PIN: NodeId = NodeId::new(42, 7);
    other.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Created(layer_spec(NodeKind::Panel, "Dock", "center", 400.0, 300.0)),
    );
    let panel = shown(&mut other, PANEL);
    other.state_mut().host_mut().accept.insert(panel);
    let both = |mgr: &mut Mgr, other: &mut Mgr, d: Duration| {
        let end = Instant::now() + d;
        while Instant::now() < end {
            mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
            other.dispatch(Some(Duration::from_millis(5))).unwrap();
        }
    };
    both(&mut mgr, &mut other, Duration::from_millis(200));
    mgr.state_mut().host_mut().export = Some(DropPayload::External {
        kind: DropKind::Text,
        files: vec![],
        text: "from the other shell".into(),
        app_id: None,
    });
    p.to(100, 18);
    both(&mut mgr, &mut other, Duration::from_millis(50));
    p.button(wl_pointer::ButtonState::Pressed);
    both(&mut mgr, &mut other, Duration::from_millis(50));
    mgr.state_mut().host_mut().source = Some((bar, PIN));
    p.to(100, 60);
    // The hand-over is a round trip through the compositor: wait for it,
    // bounded, as `drag_out_of_bar` does (a fixed window flakes when the
    // runner is slow).
    let deadline = Instant::now() + WAIT;
    while !mgr.state().carrying_drag() {
        assert!(
            Instant::now() < deadline,
            "the drag is never handed over: {:#?}",
            mgr.state().host().input
        );
        both(&mut mgr, &mut other, Duration::from_millis(20));
    }
    for i in 1..=12u32 {
        let lerp = |a: u32, b: u32| a + (b - a) * i / 12;
        p.to(lerp(100, 960), lerp(60, 540));
        both(&mut mgr, &mut other, Duration::from_millis(25));
    }
    p.button(wl_pointer::ButtonState::Released);
    let deadline = Instant::now() + WAIT;
    while drops(other.state().host()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "never dropped: {:#?}",
            other.state().host().input
        );
        both(&mut mgr, &mut other, Duration::from_millis(20));
    }
    let host = other.state().host();
    assert!(
        host.input.iter().any(|e| matches!(
            e,
            InputEvent::DragEnter { surface, kinds, .. }
                if *surface == panel && kinds == &[DropKind::Text]
        )),
        "{:#?}",
        host.input
    );
    assert_eq!(
        drops(host),
        [(
            panel,
            DropPayload::External {
                kind: DropKind::Text,
                files: vec![],
                text: "from the other shell".into(),
                app_id: None,
            }
        )]
    );
    let deadline = Instant::now() + WAIT;
    while mgr.state().carrying_drag() {
        assert!(Instant::now() < deadline, "the origin's drag never ended");
        both(&mut mgr, &mut other, Duration::from_millis(20));
    }
    assert!(drops(mgr.state().host()).is_empty());
}

/// (M4) While the compositor carries a Strand drag, its icon (the
/// host's `drag_image`: here a 40×20 red box whose corner was at (90, 8)
/// on the bar, grabbed at (100, 18)) follows the pointer, held where it
/// was grabbed: with the pointer at (600, 500) the screen is red at
/// (620, 500), 10 px left of the pointer at (591, 491) and to (629, 509),
/// and not beyond; once let go (cancelled over the desktop) and the
/// pointer moves on, it is gone (headless sway 1.9 repaints the spot only
/// when something there changes).
#[test]
fn a_carried_strand_drag_shows_its_icon_under_the_pointer() {
    let Some((sway, mut mgr, bar, mut p, _program)) =
        desk("a_carried_strand_drag_shows_its_icon_under_the_pointer")
    else {
        return;
    };
    const PIN: NodeId = NodeId::new(42, 7);
    let (w, h) = (40u32, 20u32);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        pixels.extend_from_slice(&[0x00, 0x00, 0xff, 0xff]);
    }
    mgr.state_mut().host_mut().icon = Some(strand_scene::DragImage {
        size: strand_scene::Size::new(w, h),
        scale: strand_scene::Scale::ONE,
        origin: strand_scene::LogicalPoint::new(90.0, 8.0),
        pixels,
    });
    drag_out_of_bar(&mut mgr, &mut p, bar, PIN);
    glide(&mut mgr, &mut p, (100, 60), (600, 500));
    pump_mgr(&mut mgr, Duration::from_millis(200));
    let output = sway.output_names()[0].clone();
    let red = |c: [u8; 3]| c[0] > 200 && c[1] < 60 && c[2] < 60;
    let deadline = Instant::now() + WAIT;
    loop {
        let shot = sway.grim(&output);
        let inside = [(620, 500), (591, 491), (628, 508)].map(|(x, y)| shot.rgb(x, y));
        let outside = [(588, 500), (632, 500), (610, 488), (610, 512)].map(|(x, y)| shot.rgb(x, y));
        if inside.iter().all(|c| red(*c)) && !outside.iter().any(|c| red(*c)) {
            break;
        }
        if Instant::now() >= deadline {
            let mut bbox = (u32::MAX, u32::MAX, 0, 0);
            for y in 400..600 {
                for x in 500..700 {
                    if red(shot.rgb(x, y)) {
                        bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x), bbox.3.max(y));
                    }
                }
            }
            panic!(
                "the icon under the pointer: inside {inside:?}, outside {outside:?}; red in {bbox:?}"
            );
        }
        pump_mgr(&mut mgr, Duration::from_millis(50));
    }
    p.button(wl_pointer::ButtonState::Released);
    wait(&mut mgr, "the drag ends", |m| !m.state().carrying_drag());
    p.to(300, 300);
    let deadline = Instant::now() + WAIT;
    while red(sway.grim(&output).rgb(620, 500)) {
        if Instant::now() >= deadline {
            let shot = sway.grim(&output);
            let mut bbox = (u32::MAX, u32::MAX, 0, 0);
            for y in 0..1080 {
                for x in 0..1920 {
                    if red(shot.rgb(x, y)) {
                        bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x), bbox.3.max(y));
                    }
                }
            }
            panic!("the icon stays after the drag: red in {bbox:?}");
        }
        pump_mgr(&mut mgr, Duration::from_millis(50));
    }
}
