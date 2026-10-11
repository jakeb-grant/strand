//! M5 wave-0 spike 3 (decisions.md m5-infra): does sway 1.9 issue an
//! xdg-activation token for a wlr-layer-shell surface and the serial of
//! a click on it, and does a toplevel activated with that token get
//! focus (or, under sway's default `focus_on_window_activation urgent`,
//! the urgent hint)?
//!
//! Usage: xdg-activation-spike SCENARIO, against WAYLAND_DISPLAY and
//! SWAYSOCK of a private headless sway (run.sh). Scenarios:
//! - `layer`: two toplevels A then B (B focused); a virtual-pointer
//!   click on a layer surface; a token with that serial and the layer
//!   surface; A activated with it.
//! - `bogus`: the same, the token's serial off by 1000.
//! - `none`: the same, the token without serial or surface.
//! - `launch`: a token from the click, then a new toplevel C mapped
//!   and activated with it (an app launched with XDG_ACTIVATION_TOKEN).
//! It prints `TOKEN ...` and the tree's focused/urgent state as
//! `STATE app=… focused=… urgent=…` lines.

use std::fs::File;
use std::os::fd::AsFd;
use std::process::Command;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

#[derive(Default)]
struct App {
    configured: Vec<u32>, // ids (protocol object ids) of configured surfaces
    pointer: Option<wl_pointer::WlPointer>,
    click: Option<(u32, u32)>, // (serial, surface id)
    entered: Option<u32>,
    token: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(App: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(App: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(App: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(App: ignore xdg_toplevel::XdgToplevel);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(_: &mut Self, b: &xdg_wm_base::XdgWmBase, e: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = e {
            b.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, u32> for App {
    fn event(s: &mut Self, x: &xdg_surface::XdgSurface, e: xdg_surface::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = e {
            x.ack_configure(serial);
            s.configured.push(*id);
        }
    }
}
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for App {
    fn event(s: &mut Self, l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, e: zwlr_layer_surface_v1::Event, id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = e {
            l.ack_configure(serial);
            s.configured.push(*id);
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(s: &mut Self, seat: &wl_seat::WlSeat, e: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = e {
            if c.contains(wl_seat::Capability::Pointer) && s.pointer.is_none() {
                s.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(s: &mut Self, _: &wl_pointer::WlPointer, e: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wayland_client::Proxy;
        match e {
            wl_pointer::Event::Enter { surface, .. } => s.entered = Some(surface.id().protocol_id()),
            wl_pointer::Event::Button { serial, state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => {
                if let Some(id) = s.entered {
                    s.click = Some((serial, id));
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
    fn event(s: &mut Self, _: &xdg_activation_token_v1::XdgActivationTokenV1, e: xdg_activation_token_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = e {
            s.token = Some(token);
        }
    }
}

struct G {
    comp: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    wm: xdg_wm_base::XdgWmBase,
    layer: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    act: Option<xdg_activation_v1::XdgActivationV1>,
    seat: wl_seat::WlSeat,
    vptr: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
}

fn buffer(g: &G, qh: &QueueHandle<App>, w: i32, h: i32, argb: u32) -> wl_buffer::WlBuffer {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap();
    let path = format!("{dir}/spike-shm-{argb:08x}-{w}x{h}");
    let bytes: Vec<u8> = std::iter::repeat_n(argb.to_le_bytes(), (w * h) as usize).flatten().collect();
    std::fs::write(&path, &bytes).unwrap();
    let file = File::options().read(true).write(true).open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let pool = g.shm.create_pool(file.as_fd(), w * h * 4, qh, ());
    let b = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    b
}

fn wait(q: &mut EventQueue<App>, app: &mut App, what: &str, mut done: impl FnMut(&App) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(app) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        q.roundtrip(app).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn toplevel(g: &G, q: &mut EventQueue<App>, app: &mut App, app_id: &str, color: u32) -> wl_surface::WlSurface {
    use wayland_client::Proxy;
    let qh = q.handle();
    let s = g.comp.create_surface(&qh, ());
    let id = s.id().protocol_id();
    let xs = g.wm.get_xdg_surface(&s, &qh, id);
    let tl = xs.get_toplevel(&qh, ());
    tl.set_app_id(app_id.into());
    tl.set_title(app_id.into());
    s.commit();
    wait(q, app, "toplevel configure", |a| a.configured.contains(&id));
    s.attach(Some(&buffer(g, &qh, 200, 150, color)), 0, 0);
    s.commit();
    q.roundtrip(app).unwrap();
    std::mem::forget((xs, tl));
    s
}

fn tree_state() {
    let out = Command::new("sh")
        .arg("-c")
        .arg(r#"swaymsg -t get_tree | jq -r '.. | objects | select(.app_id? != null) | "STATE app=\(.app_id) focused=\(.focused) urgent=\(.urgent)"'"#)
        .output()
        .unwrap();
    print!("{}", String::from_utf8_lossy(&out.stdout));
}

fn main() {
    use wayland_client::Proxy;
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "layer".into());
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut q) = registry_queue_init::<App>(&conn).unwrap();
    let qh = q.handle();
    let g = G {
        comp: globals.bind(&qh, 4..=6, ()).unwrap(),
        shm: globals.bind(&qh, 1..=1, ()).unwrap(),
        wm: globals.bind(&qh, 1..=5, ()).unwrap(),
        layer: globals.bind(&qh, 1..=4, ()).unwrap(),
        act: globals.bind(&qh, 1..=1, ()).ok(),
        seat: globals.bind(&qh, 1..=7, ()).unwrap(),
        vptr: globals.bind(&qh, 1..=2, ()).unwrap(),
    };
    println!("xdg_activation_v1 advertised: {}", g.act.is_some());
    let Some(act) = g.act.clone() else { return };
    let mut app = App::default();
    q.roundtrip(&mut app).unwrap();

    let a = toplevel(&g, &mut q, &mut app, "spike.a", 0xff_20_40_c0);
    let _b = toplevel(&g, &mut q, &mut app, "spike.b", 0xff_c0_40_20);
    std::thread::sleep(Duration::from_millis(200));
    println!("--- before");
    tree_state();

    // The layer surface (top layer, no keyboard), 200x40 at the output's
    // top-left corner, and a virtual-pointer click on it.
    let ls = g.comp.create_surface(&qh, ());
    let lid = ls.id().protocol_id();
    let layer = g.layer.get_layer_surface(&ls, None, zwlr_layer_shell_v1::Layer::Top, "spike".into(), &qh, lid);
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
    layer.set_size(200, 40);
    ls.commit();
    wait(&mut q, &mut app, "layer configure", |a| a.configured.contains(&lid));
    ls.attach(Some(&buffer(&g, &qh, 200, 40, 0xff_30_30_30)), 0, 0);
    ls.commit();
    // Headless sway has no pointer until a virtual one exists.
    let vp = g.vptr.create_virtual_pointer(Some(&g.seat), &qh, ());
    wait(&mut q, &mut app, "a pointer", |a| a.pointer.is_some());
    std::thread::sleep(Duration::from_millis(100));
    vp.motion_absolute(1, 50, 20, 1280, 720);
    vp.frame();
    vp.button(2, 0x110, wl_pointer::ButtonState::Pressed);
    vp.frame();
    vp.button(3, 0x110, wl_pointer::ButtonState::Released);
    vp.frame();
    wait(&mut q, &mut app, "the click on the layer surface", |a| a.click.is_some());
    let (serial, on) = app.click.unwrap();
    println!("click serial {serial} on surface {on} (layer surface {lid}: {})", on == lid);

    let tok = act.get_activation_token(&qh, ());
    match scenario.as_str() {
        "none" => {}
        "bogus" => {
            tok.set_serial(serial + 1000, &g.seat);
            tok.set_surface(&ls);
        }
        _ => {
            tok.set_serial(serial, &g.seat);
            tok.set_surface(&ls);
        }
    }
    tok.set_app_id("spike.a".into());
    tok.commit();
    wait(&mut q, &mut app, "the token", |a| a.token.is_some());
    let token = app.token.take().unwrap();
    println!("TOKEN {token:?} ({scenario})");

    let target = if scenario == "launch" {
        // A launched app: maps its toplevel, then activates with the
        // token it got through XDG_ACTIVATION_TOKEN.
        toplevel(&g, &mut q, &mut app, "spike.c", 0xff_20_c0_40)
    } else {
        a
    };
    act.activate(token, &target);
    q.roundtrip(&mut app).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    println!("--- after activate");
    tree_state();
}
