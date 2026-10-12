//! (M5, S-inspect) The inspector's drawing: the highlight outline and
//! the repaint flash, offline at 1× and 2× against reference PNGs; pick
//! mode following the pointer across surfaces; and, on a Vulkan device
//! (lavapipe in CI), a `GpuPresent` surface drawing both as the CPU does.

mod common;
#[cfg(feature = "gpu")]
mod gpu_host;

use std::time::Duration;

use common::*;
use strand_render::{InputScene, Renderer, Router};
use strand_scene::input::button;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const S2: SurfaceId = SurfaceId(2);

/// A 200×40 bar holding a blue box, a rounded pink box with a label and
/// a small green box inside the pink one. Returns the bar and the
/// boxes: blue, pink, green.
fn scene() -> (SceneDiff, [NodeId; 4]) {
    let mut b = Builder::default();
    let root = b.node(NodeKind::Bar, None, vec![(Prop::Bg, color("#1e1e2e"))]);
    let place = |x: f32, w: f32, bg: &str| {
        vec![
            (Prop::X, num(x)),
            (Prop::Y, num(8.0)),
            (Prop::Width, num(w)),
            (Prop::Height, num(24.0)),
            (Prop::Bg, color(bg)),
        ]
    };
    let blue = b.node(NodeKind::Box, Some(root), place(10.0, 40.0, "#89b4fa"));
    let mut pink_props = place(70.0, 110.0, "#f38ba8");
    pink_props.push((Prop::Radius, num(6.0)));
    let pink = b.node(NodeKind::Box, Some(root), pink_props);
    let green = b.node(
        NodeKind::Box,
        Some(pink),
        vec![
            (Prop::X, num(80.0)),
            (Prop::Y, num(4.0)),
            (Prop::Size, num(16.0)),
            (Prop::Bg, color("#a6e3a1")),
        ],
    );
    b.node(
        NodeKind::Text,
        Some(pink),
        vec![
            (Prop::X, num(8.0)),
            (Prop::Y, num(4.0)),
            (Prop::Text, text("Pick")),
            (Prop::Font, PropValue::Font(font(13.0))),
            (Prop::Color, color("#11111b")),
        ],
    );
    (b.diff, [root, blue, pink, green])
}

/// A renderer showing [`scene`] on `surfaces` (each a copy of the bar,
/// as on two outputs), and its nodes.
fn shown(surfaces: &[SurfaceId]) -> (Renderer, [NodeId; 4]) {
    let mut r = renderer();
    // Colour changes snap: the frames compared are the change's own.
    r.set_reduced_motion(true);
    let (diff, nodes) = scene();
    assert!(r.apply(diff).is_empty());
    for s in surfaces {
        r.attach_surface(*s, nodes[0]);
    }
    (r, nodes)
}

/// Paints until the surface is settled (text shaped inline, nothing
/// wanted), at `time`.
fn settle(r: &mut Renderer, buf: &mut Buffer, s: SurfaceId, time: Duration) {
    buf.paint_at(r, s, 1, time);
    r.update();
    for _ in 0..4 {
        if !r.wants_frame(s) {
            break;
        }
        buf.paint_at(r, s, 1, time);
    }
    assert!(!r.wants_frame(s), "the scene settles");
}

fn buffer(scale: u32) -> Buffer {
    Buffer::new(
        200 * scale,
        40 * scale,
        Scale::from_integer(scale).expect("a scale"),
    )
}

const T0: Duration = Duration::from_secs(1);

fn ms(n: u64) -> Duration {
    T0 + Duration::from_millis(n)
}

/// The outlined node gets a ring inside its box, drawn over everything
/// it covers; only the ring's pixels repaint when it appears and when it
/// goes, and clearing it leaves the frame as it was.
#[test]
fn the_picked_node_is_outlined() {
    for scale in [1, 2] {
        let (mut r, [_, _, pink, _]) = shown(&[S]);
        let mut buf = buffer(scale);
        settle(&mut r, &mut buf, S, T0);
        let plain = buf.pixels.clone();
        r.set_highlight(Some(pink));
        assert!(r.wants_frame(S), "a highlight repaints");
        let d = buf.paint_at(&mut r, S, 1, ms(16));
        let k = scale as i32;
        assert_eq!(
            d.bounds(),
            Some(Rect::new(70 * k, 8 * k, 110 * scale, 24 * scale)),
            "only the box the ring is in repaints"
        );
        let name = match scale {
            1 => "inspect_outline",
            _ => "inspect_outline_2x",
        };
        assert_matches_ref(name, &buf, 1);
        // The ring is 2 logical pixels inside the box: its edge pixel is
        // the outline's blue, the box's middle is still pink.
        let px = |b: &Buffer, x: i32, y: i32| b.px((x * k) as u32, (y * k) as u32);
        assert_eq!(px(&buf, 100, 8), [0xf3, 0x96, 0x21, 0xff]);
        assert_eq!(px(&buf, 130, 20), [0xa8, 0x8b, 0xf3, 0xff]);
        // Steady: nothing more to draw.
        assert!(!r.wants_frame(S));
        assert_eq!(
            r.node_layout(S, pink).rect,
            Some(LogicalRect::new(70.0, 8.0, 110.0, 24.0))
        );
        r.set_highlight(None);
        let d = buf.paint_at(&mut r, S, 1, ms(32));
        assert_eq!(
            d.bounds(),
            Some(Rect::new(70 * k, 8 * k, 110 * scale, 24 * scale))
        );
        assert_eq!(buf.pixels, plain, "the frame without the outline");
    }
}

/// With the flash on, each frame's own repaint is tinted, fading over
/// 300 ms: only the box that changed, never the rest of the bar, and the
/// frames repainting the fading tint add no tint of their own (no
/// feedback). Once it has faded the frame is as it would be without it,
/// and nothing more is wanted.
#[test]
fn repaint_flash_tints_only_the_damage() {
    for scale in [1, 2] {
        let (mut r, [_, blue, _, _]) = shown(&[S]);
        let mut buf = buffer(scale);
        settle(&mut r, &mut buf, S, T0);
        r.set_damage_flash(true);
        // Turning it on repaints nothing.
        assert!(!r.wants_frame(S));
        let mut d = SceneDiff::new();
        d.set(blue, Prop::Bg, color("#fab387"));
        assert!(r.apply(d).is_empty());
        let k = scale as i32;
        let blue_box = Rect::new(10 * k, 8 * k, 40 * scale, 24 * scale);
        let damage = buf.paint_at(&mut r, S, 1, ms(16));
        assert_eq!(damage.bounds(), Some(blue_box), "the change alone");
        let name = match scale {
            1 => "inspect_flash",
            _ => "inspect_flash_2x",
        };
        assert_matches_ref(name, &buf, 1);
        let px = |b: &Buffer, x: i32, y: i32| b.px((x * k) as u32, (y * k) as u32);
        let tinted = px(&buf, 30, 20);
        assert_ne!(tinted, [0x87, 0xb3, 0xfa, 0xff], "tinted orange");
        assert_eq!(
            px(&buf, 130, 20),
            [0xa8, 0x8b, 0xf3, 0xff],
            "pink untouched"
        );
        // The tint fades: each frame repaints only the tinted box, and
        // the tint never spreads past it.
        let mut t = 16;
        let mut last = tinted;
        while r.wants_frame(S) {
            t += 16;
            assert!(t < 1000, "the flash never ended");
            let damage = buf.paint_at(&mut r, S, 1, ms(t));
            assert_eq!(damage.bounds(), Some(blue_box), "at {t} ms");
            let now = px(&buf, 30, 20);
            assert_ne!(now, last, "fading at {t} ms");
            last = now;
            assert_eq!(px(&buf, 130, 20), [0xa8, 0x8b, 0xf3, 0xff]);
        }
        assert!(t >= 300, "faded after {t} ms");
        assert_eq!(last, [0x87, 0xb3, 0xfa, 0xff], "plain orange after it");
        // A frame painted from scratch matches.
        let (mut fresh, [_, fblue, _, _]) = shown(&[S]);
        let mut d = SceneDiff::new();
        d.set(fblue, Prop::Bg, color("#fab387"));
        assert!(fresh.apply(d).is_empty());
        let mut want = buffer(scale);
        settle(&mut fresh, &mut want, S, T0);
        assert_eq!(buf.pixels, want.pixels);
        assert!(buf.paint_at(&mut r, S, 1, ms(t + 500)).is_empty());
    }
}

/// With the flash off nothing is kept and nothing asks for frames: a
/// change paints once and the surface is idle. Turning it off while
/// tints show repaints them away once, then nothing.
#[test]
fn flash_off_wakes_nothing() {
    let (mut r, [_, blue, _, _]) = shown(&[S]);
    let mut buf = buffer(1);
    settle(&mut r, &mut buf, S, T0);
    let mut d = SceneDiff::new();
    d.set(blue, Prop::Bg, color("#fab387"));
    assert!(r.apply(d).is_empty());
    assert!(!buf.paint_at(&mut r, S, 1, ms(16)).is_empty());
    assert!(!r.wants_frame(S), "no flash, no frames after the change");
    assert_eq!(r.frame_deadline(S), None);

    r.set_damage_flash(true);
    let mut d = SceneDiff::new();
    d.set(blue, Prop::Bg, color("#89b4fa"));
    assert!(r.apply(d).is_empty());
    buf.paint_at(&mut r, S, 1, ms(32));
    assert!(r.wants_frame(S), "fading");
    r.set_damage_flash(false);
    assert!(r.wants_frame(S), "the tint is repainted away");
    let damage = buf.paint_at(&mut r, S, 1, ms(48));
    assert_eq!(damage.bounds(), Some(Rect::new(10, 8, 40, 24)));
    assert_eq!(buf.px(30, 20), [0xfa, 0xb4, 0x89, 0xff], "no tint");
    assert!(!r.wants_frame(S));
    assert!(buf.paint_at(&mut r, S, 1, ms(64)).is_empty());
}

/// Pick mode: the router hands pointer input to no handler, the node
/// under the pointer is outlined on whichever surface the pointer is
/// over (here one bar on two outputs, each showing the outline of the
/// node hovered on either), a left press picks the node under it on its
/// surface, and ending pick mode clears the outline and gives the
/// pointer back to the handlers.
#[test]
fn pick_follows_the_pointer_across_surfaces() {
    let (mut r, [root, blue, pink, green]) = shown(&[S, S2]);
    let mut one = buffer(1);
    let mut two = buffer(2);
    settle(&mut r, &mut one, S, T0);
    settle(&mut r, &mut two, S2, T0);
    let mut router = Router::new();
    router.attached(S, root);
    router.attached(S2, root);
    // A hover before pick mode is ended by it.
    let motion = |surface, x: f32, y: f32| InputEvent::PointerMotion {
        surface,
        position: LogicalPoint::new(x, y),
        time: 0,
    };
    let before = router.handle(&motion(S, 20.0, 20.0), &mut r);
    assert!(
        before.iter().any(
            |i| matches!(i, strand_render::Intent::Flag { node, on: true, .. } if *node == blue)
        )
    );
    let off = router.set_picking(true, &mut r);
    assert!(
        off.iter().any(
            |i| matches!(i, strand_render::Intent::Flag { node, on: false, .. } if *node == blue)
        ),
        "pick mode ends the hover: {off:?}"
    );
    assert!(router.picking());

    let blue_ring = |b: &Buffer, k: u32| b.px(30 * k, 8 * k);
    let green_ring = |b: &Buffer, k: u32| b.px(158 * k, 12 * k);
    const RING: [u8; 4] = [0xf3, 0x96, 0x21, 0xff];
    let feed = |r: &mut Renderer, router: &mut Router, e: InputEvent| {
        assert!(router.handle(&e, r).is_empty(), "no handler hears {e:?}");
        r.pick_input(&e)
    };
    // Over the blue box on output 1.
    assert_eq!(feed(&mut r, &mut router, motion(S, 20.0, 20.0)), None);
    one.paint_at(&mut r, S, 1, ms(16));
    two.paint_at(&mut r, S2, 1, ms(16));
    assert_eq!(blue_ring(&one, 1), RING);
    assert_eq!(blue_ring(&two, 2), RING, "the same node on output 2");
    // Over the green box on output 2: the outline follows.
    let leave = InputEvent::PointerLeave { surface: S };
    assert_eq!(feed(&mut r, &mut router, leave), None);
    let enter = InputEvent::PointerEnter {
        surface: S2,
        position: LogicalPoint::new(160.0, 18.0),
    };
    assert_eq!(feed(&mut r, &mut router, enter), None);
    one.paint_at(&mut r, S, 1, ms(32));
    two.paint_at(&mut r, S2, 1, ms(32));
    assert_ne!(blue_ring(&one, 1), RING, "blue no longer outlined");
    assert_eq!(green_ring(&two, 2), RING);
    assert_eq!(green_ring(&one, 1), RING);
    assert_eq!(
        r.node_layout(S2, green).rect,
        Some(LogicalRect::new(150.0, 12.0, 16.0, 16.0))
    );
    // A left press picks on output 2; its release clicks nothing.
    let press = |state| InputEvent::PointerButton {
        surface: S2,
        position: LogicalPoint::new(73.0, 10.0),
        button: button::LEFT,
        state,
        time: 1,
    };
    assert_eq!(
        feed(&mut r, &mut router, press(ButtonState::Pressed)),
        Some((S2, Some(pink)))
    );
    assert_eq!(
        feed(&mut r, &mut router, press(ButtonState::Released)),
        None
    );
    // A right press is not a pick.
    let right = InputEvent::PointerButton {
        surface: S2,
        position: LogicalPoint::new(100.0, 20.0),
        button: button::RIGHT,
        state: ButtonState::Pressed,
        time: 2,
    };
    assert_eq!(feed(&mut r, &mut router, right), None);
    // Off the bar's shapes nothing is picked.
    assert_eq!(r.pick(S, LogicalPoint::new(500.0, 20.0)), None);
    assert_eq!(r.pick(S, LogicalPoint::new(5.0, 2.0)), Some(root));

    // Ending pick mode: the outline goes, and the pointer reaches the
    // handlers again.
    assert!(router.set_picking(false, &mut r).is_empty());
    r.pick_ended();
    two.paint_at(&mut r, S2, 1, ms(48));
    assert_ne!(green_ring(&two, 2), RING);
    let back = router.handle(&motion(S2, 20.0, 20.0), &mut r);
    assert!(
        back.iter().any(
            |i| matches!(i, strand_render::Intent::Flag { node, on: true, .. } if *node == blue)
        ),
        "hover again: {back:?}"
    );
}

/// A lock surface is never pickable, and the outline is never drawn on
/// one.
#[test]
fn a_lock_surface_is_never_picked_or_outlined() {
    let mut b = Builder::default();
    let lock = b.node(NodeKind::Lock, None, vec![(Prop::Bg, color("#11111b"))]);
    let field = b.node(
        NodeKind::Box,
        Some(lock),
        vec![
            (Prop::X, num(10.0)),
            (Prop::Y, num(8.0)),
            (Prop::Width, num(40.0)),
            (Prop::Height, num(24.0)),
            (Prop::Bg, color("#1e1e2e")),
        ],
    );
    let mut r = renderer();
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, lock);
    let mut buf = buffer(1);
    settle(&mut r, &mut buf, S, T0);
    let plain = buf.pixels.clone();
    assert_eq!(r.pick(S, LogicalPoint::new(20.0, 20.0)), None);
    r.set_highlight(Some(field));
    buf.paint_at(&mut r, S, 1, ms(16));
    assert_eq!(buf.pixels, plain);
}

/// A `GpuPresent` surface draws the outline and the flash as the CPU
/// does (within the GPU tests' tolerance).
#[cfg(feature = "gpu")]
#[test]
fn a_presented_surface_shows_the_outline_and_the_flash() {
    use gpu_host::device;
    use std::sync::mpsc;
    use std::time::Instant;
    use strand_gpu::{Gpu, GpuReply, GpuRequest};

    let Some(opts) = device() else { return };
    let inspected = |r: &mut Renderer, blue: NodeId, pink: NodeId| {
        r.set_highlight(Some(pink));
        r.set_damage_flash(true);
        let mut d = SceneDiff::new();
        d.set(blue, Prop::Bg, color("#fab387"));
        assert!(r.apply(d).is_empty());
    };
    // The CPU's frame.
    let (mut cpu, [_, blue, pink, _]) = shown(&[S]);
    let mut want = buffer(1);
    settle(&mut cpu, &mut want, S, T0);
    inspected(&mut cpu, blue, pink);
    want.paint_at(&mut cpu, S, 0, ms(16));

    // The same frame presented.
    let (mut r, [_, blue, pink, _]) = shown(&[S]);
    let mut buf = buffer(1);
    settle(&mut r, &mut buf, S, T0);
    r.promote_now(S);
    let _ = r.take_backend_changes();
    r.set_backend(S, Backend::GpuPresent);
    let (ping, pings) = mpsc::channel();
    let mut gpu = Gpu::spawn(
        Box::new(move || {
            let _ = ping.send(());
        }),
        opts,
    );
    gpu.send(GpuRequest::Attach {
        surface: S,
        handles: None,
        size: Size::new(200, 40),
        scale: Scale::ONE,
        opaque: false,
    });
    let _ = r.paint_gpu(S, T0);
    inspected(&mut r, blue, pink);
    let frame = r.paint_gpu(S, ms(16)).expect("the change repaints");
    let id = frame.id;
    gpu.send(GpuRequest::Frame(frame));
    let deadline = Instant::now() + gpu_host::WAIT;
    let px = 'got: loop {
        while let Some(reply) = gpu.try_recv() {
            match reply {
                GpuReply::Pixels { frame, pixels, .. } if frame == id => {
                    let mut out = Vec::new();
                    for y in 0..pixels.height {
                        out.extend_from_slice(pixels.row(y));
                    }
                    break 'got out;
                }
                GpuReply::Failed { error, .. } => panic!("frame {id} failed: {error}"),
                _ => {}
            }
        }
        assert!(Instant::now() < deadline, "no pixels for frame {id}");
        let _ = pings.recv_timeout(Duration::from_millis(50));
    };
    let mut bad = 0;
    let mut worst = 0;
    for (a, b) in px.chunks_exact(4).zip(want.pixels.chunks_exact(4)) {
        let d = a
            .iter()
            .zip(b)
            .map(|(p, q)| p.abs_diff(*q))
            .max()
            .unwrap_or(0);
        worst = worst.max(d);
        if d > 6 {
            bad += 1;
        }
    }
    let at = |x: u32, y: u32| {
        let i = ((y * 200 + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    };
    assert!(
        bad as f64 / (200.0 * 40.0) <= 0.02,
        "{bad} pixels past 6 of the CPU's (worst {worst})"
    );
    let ring = at(100, 8);
    assert!(ring[0] > 0xe0 && ring[2] < 0x40, "the outline: {ring:?}");
    assert_ne!(at(30, 20), [0x87, 0xb3, 0xfa, 0xff], "the flash's tint");
    assert!(r.wants_frame(S), "the flash fades on the GPU too");
    drop(gpu);
}

/// The router's pick mode is reachable from `InputScene` users other
/// than the renderer (tests' hit functions): pointer input reaches no
/// handler while it is on.
#[test]
fn pick_mode_holds_pointer_input_from_the_handlers() {
    let mut router = Router::new();
    let root = NodeId::new(1, 0);
    let node = NodeId::new(2, 0);
    router.attached(S, root);
    let mut scene = strand_render::HitOnly(move |_: SurfaceId, _: LogicalPoint| vec![node, root]);
    let scene: &mut dyn InputScene = &mut scene;
    assert!(router.set_picking(true, scene).is_empty());
    for e in [
        InputEvent::PointerEnter {
            surface: S,
            position: LogicalPoint::new(1.0, 1.0),
        },
        InputEvent::PointerButton {
            surface: S,
            position: LogicalPoint::new(1.0, 1.0),
            button: button::LEFT,
            state: ButtonState::Pressed,
            time: 0,
        },
        InputEvent::PointerButton {
            surface: S,
            position: LogicalPoint::new(1.0, 1.0),
            button: button::LEFT,
            state: ButtonState::Released,
            time: 1,
        },
        InputEvent::ClickAway { surface: S },
    ] {
        assert!(router.handle(&e, scene).is_empty(), "{e:?}");
    }
    assert_eq!(router.pointer(S), Some(LogicalPoint::new(1.0, 1.0)));
}
