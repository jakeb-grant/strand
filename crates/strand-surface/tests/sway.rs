//! Integration tests against a private headless sway (skipped with a
//! message when sway is not installed). See `CLAUDE.md`, "Headless
//! Wayland for tests".

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_scene::{LogicalRect, Scale, Size, SurfaceChange};
use strand_surface::{ButtonState, Config, FakeClock, InputEvent, MAX_BUFFERS, Request};

#[test]
fn bar_on_every_output_with_hotplug() {
    let Some((sway, mut mgr)) = start("bar_on_every_output_with_hotplug", Config::default()) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let first = mgr.state().surfaces()[0].clone();
    assert_eq!(first.namespace, "strand-Top");
    assert_eq!(first.logical_size, (1920, 36));
    assert_eq!(first.buffer_size, Size::new(1920, 36));
    assert_eq!(first.node, BAR);
    // The exclusive zone keeps windows out of the bar.
    pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(sway.workspace_rect("HEADLESS-1").1, 36);

    // A second monitor gets its own bar.
    let second = sway.create_output();
    wait_for_bars(&mut mgr, 2);
    let monitors = mgr.state().monitors();
    assert_eq!(monitors.len(), 2);
    assert_ne!(monitors[0].id, monitors[1].id);
    let on_second = mgr
        .state()
        .surfaces()
        .into_iter()
        .find(|s| s.id != first.id)
        .unwrap();
    let second_monitor = monitors
        .iter()
        .find(|m| m.connector.as_deref() == Some(second.as_str()))
        .unwrap()
        .clone();
    assert_eq!(on_second.monitor, second_monitor.id);
    assert_eq!(on_second.logical_size, (1920, 36));
    assert_eq!(mgr.state().host().attached.len(), 2);

    // Unplugging it destroys that bar and keeps the monitor remembered.
    sway.msg(&["output", &second, "unplug"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces().len() == 1)
        .unwrap();
    assert!(ok, "bar on the unplugged output was not destroyed");
    let host = mgr.state().host();
    assert_eq!(host.detached, vec![on_second.id]);
    // sway closes the layer surface before removing the output.
    assert_eq!(mgr.state().stats().closed, 1);
    assert_eq!(host.monitors_removed.len(), 1);
    assert_eq!(host.monitors_removed[0].id, second_monitor.id);
    assert_eq!(mgr.state().remembered_monitors().len(), 1);
    assert_eq!(mgr.state().surfaces()[0].id, first.id);

    // A spec change reconfigures the surface in place.
    mgr.state_mut().apply_surface_change(
        BAR,
        SurfaceChange::Updated {
            spec: bar_spec("Top", 48.0),
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let i = &s.surfaces()[0];
            i.logical_size == (1920, 48) && i.buffer_size == Size::new(1920, 48)
        })
        .unwrap();
    assert!(
        ok,
        "height change not applied: {:?}",
        mgr.state().surfaces()
    );
    assert_eq!(mgr.state().surfaces()[0].id, first.id);
    pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(sway.workspace_rect("HEADLESS-1").1, 48);

    // Removing the node removes its surfaces.
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Removed);
    assert!(mgr.state().surfaces().is_empty());
    pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(sway.workspace_rect("HEADLESS-1").1, 0);
    assert_eq!(mgr.state().host().age_errors, 0);
}

#[test]
fn bars_on_two_outputs_at_startup() {
    let Some(sway) = Sway::start("bars_on_two_outputs_at_startup") else {
        return;
    };
    let second = sway.create_output();
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 2);
    pump(&mut mgr, Duration::from_millis(150));
    for output in ["HEADLESS-1", second.as_str()] {
        let shot = sway.grim(output);
        assert_eq!(shot.rgb(10, 10), BLUE, "bar on {output}");
        assert_eq!(shot.rgb(1900, 35), BLUE, "bar on {output}");
        assert_ne!(shot.rgb(10, 36), BLUE, "below the bar on {output}");
    }
}

/// Waits until everything committed so far was presented.
fn settle(mgr: &mut strand_surface::SurfaceManager<TestHost>) {
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let st = s.stats();
            st.presented + st.discarded >= st.commits
                && s.surfaces().iter().all(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "frames were not presented: {:?}", mgr.state().stats());
    pump(mgr, Duration::from_millis(50));
}

#[test]
fn renders_pixels_with_exact_damage() {
    let Some((sway, mut mgr)) = start("renders_pixels_with_exact_damage", Config::default()) else {
        return;
    };
    mgr.state_mut().host_mut().opaque = true;
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert_eq!((shot.width, shot.height), (1920, 1080));
    assert_eq!(shot.rgb(0, 0), BLUE);
    assert_eq!(shot.rgb(1919, 35), BLUE);
    assert_ne!(shot.rgb(0, 36), BLUE);

    // Move a square around; each step is requested from another thread
    // through the repaint channel.
    let handle = mgr.repaint_handle();
    let steps = [
        LogicalRect::new(100.0, 8.0, 60.0, 20.0),
        LogicalRect::new(400.0, 8.0, 60.0, 20.0),
        LogicalRect::new(700.0, 4.0, 60.0, 28.0),
        LogicalRect::new(130.0, 8.0, 60.0, 20.0),
    ];
    let mut previous: Option<LogicalRect> = None;
    for (step, sq) in steps.into_iter().enumerate() {
        let paints_before = mgr.state().host().paints.len();
        mgr.state_mut().host_mut().set_square(Some(sq));
        let h = handle.clone();
        std::thread::spawn(move || assert!(h.send(Request::Poll)))
            .join()
            .unwrap();
        let ok = mgr
            .dispatch_until(WAIT, |s| s.host().paints.len() > paints_before)
            .unwrap();
        assert!(ok, "no paint after the repaint request");
        settle(&mut mgr);

        let paint = mgr.state().host().paints.last().unwrap().clone();
        // The first step needs a second buffer (the first is on screen),
        // painted in full; after that buffers are reused with their age
        // and the damage covers the old and new square plus what the age
        // adds, never the whole bar.
        if step > 0 {
            assert!(paint.age > 0, "a steady-state frame reuses a buffer");
            let area: u64 = paint.damage.area();
            assert!(area < 1920 * 36 / 4, "damage {:?}", paint.damage);
        }
        let new = Scale::ONE.snap_rect(sq);
        assert!(paint.damage.covers(new));
        if let Some(old) = previous {
            assert!(paint.damage.covers(Scale::ONE.snap_rect(old)));
        }

        let shot = sway.grim("HEADLESS-1");
        assert_eq!(shot.rgb(sq.x as u32 + 1, sq.y as u32 + 1), RED);
        assert_eq!(shot.rgb(sq.right() as u32 - 1, sq.bottom() as u32 - 1), RED);
        assert_eq!(shot.rgb(sq.right() as u32, sq.y as u32), BLUE);
        if let Some(old) = previous {
            if !old.intersect(sq).is_some() {
                assert_eq!(shot.rgb(old.x as u32 + 1, old.y as u32 + 1), BLUE);
            }
        }
        previous = Some(sq);
    }
    let info = &mgr.state().surfaces()[0];
    assert!(info.buffers >= 2 && info.buffers <= MAX_BUFFERS);
    // The opaque bar declared its region once; it never changed after.
    assert_eq!(info.stats.opaque_updates, 1);
    assert_eq!(mgr.state().host().age_errors, 0, "buffer ages were wrong");
}

#[test]
fn fractional_scale_buffers_and_viewport() {
    let Some(sway) = Sway::start("fractional_scale_buffers_and_viewport") else {
        return;
    };
    sway.msg(&["output", "HEADLESS-1", "scale", "1.5"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    assert!(mgr.state().fractional_available());
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let info = mgr.state().surfaces()[0].clone();
    assert!(info.fractional);
    assert_eq!(info.scale, Scale::new(180).unwrap());
    assert_eq!(info.logical_size, (1280, 36));
    // 1280 × 1.5 and 36 × 1.5 are exact.
    assert_eq!(info.buffer_size, Size::new(1920, 54));
    // The first frame was already painted at the right size and scale.
    let host = mgr.state().host();
    assert!(
        host.paints
            .iter()
            .all(|p| p.size == Size::new(1920, 54) && p.scale.numerator() == 180),
        "{:?}",
        host.paints
    );
    // On screen the bar covers exactly 54 physical rows.
    let shot = sway.grim("HEADLESS-1");
    assert_eq!((shot.width, shot.height), (1920, 1080));
    assert_eq!(shot.rgb(10, 53), BLUE);
    assert_eq!(shot.rgb(1919, 0), BLUE);
    assert_ne!(shot.rgb(10, 54), BLUE);

    // Changing the output scale at runtime resizes the buffers.
    sway.msg(&["output", "HEADLESS-1", "scale", "1.25"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let i = &s.surfaces()[0];
            i.scale.numerator() == 150 && i.stats.commits >= 2
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let info = mgr.state().surfaces()[0].clone();
    assert_eq!(info.logical_size, (1536, 36));
    assert_eq!(info.buffer_size, Size::new(1920, 45));
    let last = mgr.state().host().paints.last().unwrap().clone();
    assert_eq!(last.size, Size::new(1920, 45));
    assert_eq!(last.age, 0, "new size, new buffers");
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(10, 44), BLUE);
    assert_ne!(shot.rgb(10, 45), BLUE);

    // And at 2.0 through the same fractional path.
    sway.msg(&["output", "HEADLESS-1", "scale", "2"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let i = &s.surfaces()[0];
            i.scale.numerator() == 240 && i.buffer_size == Size::new(1920, 72)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    assert_eq!(mgr.state().surfaces()[0].logical_size, (960, 36));
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(10, 71), BLUE);
    assert_ne!(shot.rgb(10, 72), BLUE);
    assert_eq!(
        mgr.state().host().paints.last().unwrap().size,
        Size::new(1920, 72)
    );
    assert_eq!(mgr.state().host().age_errors, 0);
}

#[test]
fn integer_scale_fallback() {
    let Some(sway) = Sway::start("integer_scale_fallback") else {
        return;
    };
    sway.msg(&["output", "HEADLESS-1", "scale", "1.5"]);
    let config = Config {
        fractional_scale: false,
        ..Config::default()
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        config,
    )
    .unwrap();
    assert!(!mgr.state().fractional_available());
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces()[0].buffer_size == Size::new(2560, 72))
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let info = mgr.state().surfaces()[0].clone();
    assert!(!info.fractional);
    assert_eq!(info.scale, Scale::from_integer(2).unwrap());
    assert_eq!(info.logical_size, (1280, 36));
    settle(&mut mgr);
    // Downscaled by the compositor to 54 physical rows.
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(10, 52), BLUE);
    assert_ne!(shot.rgb(10, 55), BLUE);
}

#[test]
fn idle_requests_no_frames_and_commits_nothing() {
    let Some((_sway, mut mgr)) = start(
        "idle_requests_no_frames_and_commits_nothing",
        Config::default(),
    ) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let id = mgr.state().surfaces()[0].id;
    // A first frame alone asks for no frame callback.
    assert_eq!(mgr.state().stats().frame_requests, 0);

    // An animation drives frame callbacks while it runs...
    {
        let host = mgr.state_mut().host_mut();
        host.set_square(Some(LogicalRect::new(10.0, 10.0, 8.0, 8.0)));
        host.animate = 12;
    }
    mgr.state_mut().poll();
    let ok = mgr.dispatch_until(WAIT, |s| s.host().animate == 0).unwrap();
    assert!(ok, "animation stalled: {:?}", mgr.state().stats());
    settle(&mut mgr);
    let stats = mgr.state().stats();
    assert!(stats.frame_requests >= 11, "{stats:?}");
    assert_eq!(stats.frame_requests, stats.frames_done, "{stats:?}");
    let times: Vec<Duration> = mgr
        .state()
        .host()
        .paints_of(id)
        .iter()
        .map(|p| p.time)
        .collect();
    assert!(
        times.windows(2).all(|w| w[1] > w[0]),
        "frame times must advance: {times:?}"
    );
    assert!(mgr.state().surfaces()[0].buffers <= MAX_BUFFERS);

    // ...and then stops: an idle shell sleeps in poll with nothing armed.
    let before = mgr.state().stats();
    let handle = mgr.repaint_handle();
    let sleeper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        handle.send(Request::Poll);
    });
    let t = Instant::now();
    mgr.dispatch(None).unwrap();
    let slept = t.elapsed();
    sleeper.join().unwrap();
    assert!(
        slept >= Duration::from_millis(1400),
        "woke after {slept:?} while idle"
    );
    pump(&mut mgr, Duration::from_millis(500));
    let after = mgr.state().stats();
    assert_eq!(after.commits, before.commits, "commits while idle");
    assert_eq!(
        after.bare_commits, before.bare_commits,
        "commits while idle"
    );
    assert_eq!(
        after.frame_requests, before.frame_requests,
        "frame callbacks while idle"
    );
    assert_eq!(after.frames_done, before.frames_done);
    assert_eq!(after.paints, before.paints, "paints while idle");
    assert_eq!(mgr.state().host().age_errors, 0);
}

#[test]
fn presentation_feedback_feeds_the_frame_clock() {
    let clock = FakeClock::new(Duration::from_secs(1));
    let config = Config {
        clock: Box::new(clock.clone()),
        ..Config::default()
    };
    let Some((_sway, mut mgr)) = start("presentation_feedback_feeds_the_frame_clock", config)
    else {
        return;
    };
    assert!(mgr.state().presentation_available());
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let id = mgr.state().surfaces()[0].id;
    // Before any feedback, the frame time is the clock's "now".
    assert_eq!(mgr.state().host().paints[0].time, Duration::from_secs(1));
    let presented = clock.presentations();
    assert!(!presented.is_empty());
    let (surface, last) = *presented.last().unwrap();
    assert_eq!(surface, id);

    // The next frame targets the first refresh boundary after "now" (or
    // "now" itself while the compositor reports no refresh period, as
    // headless wlroots does).
    clock.set(last.time + Duration::from_millis(5));
    mgr.state_mut()
        .host_mut()
        .set_square(Some(LogicalRect::new(5.0, 5.0, 4.0, 4.0)));
    mgr.state_mut().poll();
    let ok = mgr
        .dispatch_until(WAIT, |s| s.host().paints.len() >= 2)
        .unwrap();
    assert!(ok);
    let expected = match last.refresh {
        Some(r) if r > Duration::from_millis(5) => last.time + r,
        Some(r) => last.time + r * (Duration::from_millis(5).as_nanos() / r.as_nanos() + 1) as u32,
        None => last.time + Duration::from_millis(5),
    };
    assert_eq!(mgr.state().host().paints.last().unwrap().time, expected);
}

#[test]
fn pointer_events_arrive_in_surface_coordinates() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_registry;
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

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

    let Some((sway, mut mgr)) = start(
        "pointer_events_arrive_in_surface_coordinates",
        Config::default(),
    ) else {
        return;
    };
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);
    let id = mgr.state().surfaces()[0].id;

    // A virtual pointer gives the seat a pointer.
    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));

    pointer.motion_absolute(1, 100, 10, 1920, 1080);
    pointer.frame();
    pointer.button(
        2,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Pressed,
    );
    pointer.frame();
    pointer.button(
        3,
        0x110,
        wayland_client::protocol::wl_pointer::ButtonState::Released,
    );
    pointer.frame();
    queue.roundtrip(&mut Client).unwrap();

    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && !events.iter().any(|e| {
            matches!(
                e,
                InputEvent::PointerButton {
                    state: ButtonState::Released,
                    ..
                }
            )
        })
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    let enter = events
        .iter()
        .find_map(|e| match e {
            InputEvent::PointerEnter { surface, position } => Some((*surface, *position)),
            _ => None,
        })
        .expect("pointer enter");
    assert_eq!(enter.0, id);
    assert!(
        (enter.1.x - 100.0).abs() < 1.0 && (enter.1.y - 10.0).abs() < 1.0,
        "{enter:?}"
    );
    let buttons: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton {
                surface,
                button,
                state,
                position,
                ..
            } => Some((*surface, *button, *state, *position)),
            _ => None,
        })
        .collect();
    assert_eq!(buttons.len(), 2, "{events:?}");
    assert_eq!(buttons[0].1, strand_surface::input::button::LEFT);
    assert_eq!(buttons[0].2, ButtonState::Pressed);
    assert_eq!(buttons[1].2, ButtonState::Released);
    assert!(buttons.iter().all(|b| b.0 == id));
    assert!((buttons[0].3.x - 100.0).abs() < 1.0);
    drop(pointer);
}
