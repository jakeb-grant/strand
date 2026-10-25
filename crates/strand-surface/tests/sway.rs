//! Integration tests against a private headless sway (skipped with a
//! message when sway is not installed). See `CLAUDE.md`, "Headless
//! Wayland for tests".

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_scene::{
    Damage, Insets, Layer, LogicalRect, LogicalSize, NodeId, NodeKind, Scale, Size, SurfaceChange,
};
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
    // (M5) `globals()`, `strand report`'s: every global with its version,
    // sorted, one per output.
    let globals = mgr.state().globals();
    let outputs = globals.iter().filter(|(i, _)| i == "wl_output").count();
    assert_eq!(outputs, 2, "{globals:?}");
    assert!(
        globals
            .iter()
            .any(|(i, v)| i == "zwlr_layer_shell_v1" && *v >= 1)
    );
    assert!(globals.is_sorted(), "{globals:?}");
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
    assert_eq!(on_second.monitor.as_ref(), Some(&second_monitor.id));
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
fn replugged_monitor_keeps_its_surface_ids() {
    let Some((sway, mut mgr)) = start("replugged_monitor_keeps_its_surface_ids", Config::default())
    else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let second = sway.create_output();
    wait_for_bars(&mut mgr, 2);
    let monitor = mgr
        .state()
        .monitors()
        .into_iter()
        .find(|m| m.connector.as_deref() == Some(second.as_str()))
        .unwrap();
    assert_eq!(monitor.logical_size, Some((1920, 1080)));
    assert_eq!(monitor.scale, Scale::ONE);
    assert!(monitor.position.is_some());
    let bar = mgr
        .state()
        .surfaces()
        .into_iter()
        .find(|s| s.monitor.as_ref() == Some(&monitor.id))
        .unwrap();

    // A scale change is a monitor change, not a new monitor.
    sway.msg(&["output", &second, "scale", "2"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .monitors_changed
                .iter()
                .any(|m| m.id == monitor.id && m.scale == Scale::new(240).unwrap())
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().host().monitors_changed);
    let changed = mgr.state().host().monitors_changed.last().unwrap().clone();
    assert_eq!(changed.logical_size, Some((960, 540)));
    assert_eq!(mgr.state().host().monitors_added.len(), 2);

    // Disabling the output withdraws its wl_output global; enabling it
    // brings back the same monitor (same make, model, description).
    sway.msg(&["output", &second, "disable"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces().len() == 1 && s.remembered_monitors().len() == 1
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    assert_eq!(mgr.state().remembered_monitors()[0].id, monitor.id);
    assert_eq!(mgr.state().host().detached, vec![bar.id]);

    sway.msg(&["output", &second, "enable"]);
    wait_for_bars(&mut mgr, 2);
    let host = mgr.state().host();
    let (added, reconnected) = host.monitors_added.last().unwrap().clone();
    assert_eq!(added.id, monitor.id);
    assert!(reconnected, "a monitor back within 30 s is reconnected");
    assert!(mgr.state().remembered_monitors().is_empty());
    let again = mgr
        .state()
        .surfaces()
        .into_iter()
        .find(|s| s.monitor.as_ref() == Some(&monitor.id))
        .unwrap();
    assert_eq!(again.id, bar.id, "the same surface id comes back");
    assert_eq!(
        host.attached.last(),
        Some(&(bar.id, BAR, Some(monitor.id.clone())))
    );
    assert_eq!(again.logical_size, (960, 36));
    assert_eq!(again.buffer_size, Size::new(1920, 72));
}

#[test]
fn floating_bar_margins() {
    let Some(sway) = Sway::start("floating_bar_margins") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    // `margin: 8, 8, 0` (design example a, `$space.2`).
    let margin = Insets::from_values(&[8.0, 8.0, 0.0]).unwrap();
    mgr.state_mut().apply_surface_change(
        BAR,
        SurfaceChange::Created(bar_spec_with_margin("Top", 36.0, margin)),
    );
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    assert_eq!(mgr.state().surfaces()[0].logical_size, (1904, 36));
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(8, 8), BLUE);
    assert_eq!(shot.rgb(1911, 43), BLUE);
    assert_ne!(shot.rgb(7, 20), BLUE);
    assert_ne!(shot.rgb(1912, 20), BLUE);
    assert_ne!(shot.rgb(100, 7), BLUE);
    assert_ne!(shot.rgb(100, 44), BLUE);
    // Windows start below the margin plus the bar.
    assert_eq!(sway.workspace_rect("HEADLESS-1").1, 44);
}

#[test]
fn empty_first_paint_does_not_stall() {
    let Some(sway) = Sway::start("empty_first_paint_does_not_stall") else {
        return;
    };
    let mut host = TestHost::default();
    host.empty_first = true;
    let mut mgr =
        strand_surface::SurfaceManager::with_connection(sway.connect(), host, Config::default())
            .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    // The unmapped surface gets no frame callback, so the manager must
    // not wait for one: it paints again and maps.
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let stats = mgr.state().stats();
    assert_eq!(stats.empty_paints, 1, "{stats:?}");
    assert_eq!(stats.frame_requests, stats.frames_done, "{stats:?}");
    assert_eq!(sway.grim("HEADLESS-1").rgb(10, 10), BLUE);
    // And it keeps working afterwards.
    mgr.state_mut()
        .host_mut()
        .set_square(Some(LogicalRect::new(50.0, 8.0, 10.0, 10.0)));
    mgr.state_mut().poll();
    let ok = mgr
        .dispatch_until(WAIT, |s| s.stats().commits > stats.commits)
        .unwrap();
    assert!(ok, "{:?}", mgr.state().stats());
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
        // Exactly the painter's rects went out as damage_buffer.
        assert_eq!(
            mgr.state().surfaces()[0].last_damage,
            paint.damage.rects().to_vec()
        );
        if let Some(old) = previous {
            assert!(paint.damage.covers(Scale::ONE.snap_rect(old)));
        }

        let shot = sway.grim("HEADLESS-1");
        assert_eq!(shot.rgb(sq.x as u32 + 1, sq.y as u32 + 1), RED);
        assert_eq!(shot.rgb(sq.right() as u32 - 1, sq.bottom() as u32 - 1), RED);
        assert_eq!(shot.rgb(sq.right() as u32, sq.y as u32), BLUE);
        if let Some(old) = previous
            && !old.intersect(sq).is_some()
        {
            assert_eq!(shot.rgb(old.x as u32 + 1, old.y as u32 + 1), BLUE);
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
    mgr.state_mut().host_mut().opaque = true;
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let info = mgr.state().surfaces()[0].clone();
    assert!(info.fractional);
    // The opaque region went out in logical pixels: the whole 1920×54
    // buffer is the whole 1280×36 surface.
    let scale = Scale::new(180).unwrap();
    assert_eq!(
        info.opaque_region,
        scale.inner_logical_region(&Damage::full(Size::new(1920, 54)))
    );
    assert_eq!(
        info.opaque_region,
        vec![strand_scene::Rect::new(0, 0, 1280, 36)]
    );
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
    // Each output-scale change reached the host as one configure, at a
    // size that was then painted (never an intermediate one).
    assert_configured_sizes_painted(mgr.state().host());
    assert_eq!(mgr.state().host().configured.len(), 3);
}

/// (m6-integration-w1) A bar the compositor has seen created but not yet
/// mapped when its output's scale changes, as kanshi does right after a
/// hotplug: sway 1.9 sent its preferred scale when the layer surface was
/// made (1.0) and re-sends it on a scale change only to the surfaces it
/// has mapped, so the new scale reaches it only through the output's
/// events. Its first frame and every one after are still at the new
/// scale, on both scale paths.
fn scale_change_before_the_first_frame(test: &str, fractional: bool) {
    let Some(sway) = Sway::start(test) else {
        return;
    };
    let conn = sway.connect();
    let config = Config {
        fractional_scale: fractional,
        ..Config::default()
    };
    let mut mgr =
        strand_surface::SurfaceManager::with_connection(conn.clone(), TestHost::default(), config)
            .unwrap();
    assert_eq!(mgr.state().fractional_available(), fractional);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.monitors().len() == 1)
        .unwrap();
    assert!(ok, "no monitor");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    // sway has the layer surface (and answered with scale 1 and its
    // configure, read but not dispatched): no buffer is committed yet.
    conn.roundtrip().unwrap();
    assert_eq!(mgr.state().surfaces().len(), 1);
    assert_eq!(mgr.state().surfaces()[0].stats.commits, 0);
    sway.msg(&["output", "HEADLESS-1", "scale", "2"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let i = &s.surfaces()[0];
            i.logical_size == (960, 36) && i.stats.commits > 0
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    // Give a compositor that re-sends its preference the time to.
    pump(&mut mgr, Duration::from_millis(300));
    let info = mgr.state().surfaces()[0].clone();
    assert_eq!(info.scale, Scale::from_integer(2).unwrap(), "{info:?}");
    assert_eq!(info.buffer_size, Size::new(1920, 72), "{info:?}");
    assert_eq!(
        mgr.state().host().paints.last().map(|p| (p.size, p.scale)),
        Some((Size::new(1920, 72), Scale::from_integer(2).unwrap()))
    );
    assert_configured_sizes_painted(mgr.state().host());
    settle(&mut mgr);
    // On screen the bar covers 72 physical rows.
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(10, 71), BLUE);
    assert_ne!(shot.rgb(10, 72), BLUE);
}

#[test]
fn a_scale_set_before_the_first_frame_is_kept() {
    scale_change_before_the_first_frame("a_scale_set_before_the_first_frame_is_kept", true);
}

#[test]
fn a_scale_set_before_the_first_frame_is_kept_on_the_integer_path() {
    scale_change_before_the_first_frame(
        "a_scale_set_before_the_first_frame_is_kept_on_the_integer_path",
        false,
    );
}

/// Checks every pixel of the first and last `cols` columns of `rows` rows
/// against the 1-px checkerboard, and that the row below is not the bar.
fn assert_checker(shot: &Image, rows: u32, cols: u32, what: &str) {
    for y in 0..rows {
        for x in (0..cols).chain(shot.width - cols..shot.width) {
            assert_eq!(
                shot.rgb(x, y),
                checker_at(x, y),
                "{what}: pixel ({x}, {y}) was resampled"
            );
        }
    }
    for x in 0..cols {
        let below = shot.rgb(x, rows);
        assert!(
            below != BLUE && below != WHITE,
            "{what}: bar taller than {rows} rows"
        );
    }
}

#[test]
fn fractional_buffers_are_crisp() {
    let Some(sway) = Sway::start("fractional_buffers_are_crisp") else {
        return;
    };
    // 33 × 1.25 = 41.25: the buffer is 41 rows and shown 1:1.
    sway.msg(&["output", "HEADLESS-1", "scale", "1.25"]);
    let mut host = TestHost::default();
    host.checker = true;
    let mut mgr =
        strand_surface::SurfaceManager::with_connection(sway.connect(), host, Config::default())
            .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 33.0)));
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let info = mgr.state().surfaces()[0].clone();
    let scale = Scale::new(150).unwrap();
    assert_eq!(info.scale, scale);
    assert_eq!(info.logical_size, (1536, 33));
    assert_eq!(
        info.buffer_size,
        scale.physical_size(LogicalSize::new(1536.0, 33.0))
    );
    assert_eq!(info.buffer_size, Size::new(1920, 41));
    assert_checker(&sway.grim("HEADLESS-1"), 41, 96, "scale 1.25");

    // 33 × 1.5 = 49.5: 50 rows.
    sway.msg(&["output", "HEADLESS-1", "scale", "1.5"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces()[0].buffer_size == Size::new(1920, 50))
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    settle(&mut mgr);
    assert_eq!(
        mgr.state().surfaces()[0].buffer_size,
        Scale::new(180)
            .unwrap()
            .physical_size(LogicalSize::new(1280.0, 33.0))
    );
    assert_checker(&sway.grim("HEADLESS-1"), 50, 96, "scale 1.5");

    // 33 × 2 = 66 rows, and 33 at 1.0, both through the fractional path.
    for (scale, rows) in [("2", 66), ("1", 33)] {
        sway.msg(&["output", "HEADLESS-1", "scale", scale]);
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.surfaces()[0].buffer_size == Size::new(1920, rows)
            })
            .unwrap();
        assert!(ok, "{:?}", mgr.state().surfaces());
        settle(&mut mgr);
        assert!(mgr.state().surfaces()[0].fractional);
        assert_checker(
            &sway.grim("HEADLESS-1"),
            rows,
            96,
            &format!("scale {scale}"),
        );
    }
    assert_configured_sizes_painted(mgr.state().host());
    assert_eq!(mgr.state().host().age_errors, 0);
}

#[test]
fn commits_lock_to_the_refresh_rate() {
    // The fake clock records every presentation the compositor reports.
    let clock = FakeClock::new(Duration::from_secs(1));
    let config = Config {
        clock: Box::new(clock.clone()),
        ..Config::default()
    };
    let Some((sway, mut mgr)) = start("commits_lock_to_the_refresh_rate", config) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let before = mgr.state().stats();
    let seen = clock.presentations().len();
    // 100 content changes, each followed by a poll, about every 2 ms: far
    // faster than the 60 Hz output.
    let t = Instant::now();
    let mut last = LogicalRect::new(0.0, 0.0, 0.0, 0.0);
    for i in 0..100 {
        last = LogicalRect::new(10.0 + (i * 7 % 1800) as f32, 8.0, 12.0, 12.0);
        mgr.state_mut().host_mut().set_square(Some(last));
        mgr.state_mut().poll();
        pump(&mut mgr, Duration::from_millis(2));
    }
    let elapsed = t.elapsed();
    settle(&mut mgr);
    let after = mgr.state().stats();
    let commits = after.commits - before.commits;
    eprintln!("{commits} commits for 100 changes in {elapsed:?}: {after:?}");
    // Every commit was presented, each on a later refresh than the one
    // before: at most one frame per refresh. Read from the compositor's
    // presentation timestamps rather than estimated from wall-clock time,
    // which a loaded machine stretches.
    let shown: Vec<_> = clock.presentations()[seen..]
        .iter()
        .map(|(_, p)| *p)
        .collect();
    assert_eq!(shown.len() as u64, commits, "{shown:?} {after:?}");
    for w in shown.windows(2) {
        // Headless sway reports neither a refresh period nor a retrace
        // counter: assume 60 Hz and compare the counter only when it moves.
        let refresh = w[1].refresh.unwrap_or(Duration::from_micros(16_667));
        let gap = w[1].time.saturating_sub(w[0].time);
        let next_retrace = w[1].seq > w[0].seq || w[1].seq == 0;
        assert!(
            next_retrace && gap + Duration::from_millis(2) >= refresh,
            "two frames within one refresh ({gap:?} apart, refresh {refresh:?}): {shown:?}"
        );
    }
    // Coalesced: far fewer commits than changes.
    assert!(commits < 50, "{commits} commits for 100 changes: {after:?}");
    assert!(commits >= 2, "{after:?}");
    assert!(after.throttled > before.throttled, "{after:?}");
    // Nothing is lost: the last change is on screen.
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(last.x as u32 + 1, last.y as u32 + 1), RED);
    assert_eq!(mgr.state().host().age_errors, 0);
}

#[test]
fn focused_surfaces_open_on_the_focused_output() {
    let Some(sway) = Sway::start("focused_surfaces_open_on_the_focused_output") else {
        return;
    };
    let second = sway.create_output();
    sway.msg(&["focus", "output", &second]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(5, 0);
    let spec = layer_spec(NodeKind::Panel, "Launcher", "center", 400.0, 200.0);
    assert_eq!(spec.screens, strand_scene::Screens::Focused);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    // One surface, placed by the compositor on the focused output.
    let on = |s: &strand_surface::State<TestHost>, output: &str| {
        let surfaces = s.surfaces();
        surfaces.len() == 1
            && surfaces[0].stats.commits > 0
            && surfaces[0].monitor.as_ref().is_some_and(|m| {
                s.monitors()
                    .iter()
                    .any(|x| &x.id == m && x.connector.as_deref() == Some(output))
            })
    };
    let ok = mgr.dispatch_until(WAIT, |s| on(s, &second)).unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let info = mgr.state().surfaces()[0].clone();
    assert!(info.focused);
    assert_eq!(info.logical_size, (400, 200));
    let host = mgr.state().host();
    assert_eq!(host.attached, vec![(info.id, PANEL, None)]);
    assert_eq!(host.entered.len(), 1);
    settle(&mut mgr);
    let shot = sway.grim(&second);
    assert_eq!(shot.rgb(960, 540), BLUE, "panel centred on {second}");
    assert_ne!(sway.grim("HEADLESS-1").rgb(960, 540), BLUE);

    // Focus set from outside (a compositor IPC service) moves it.
    let first = mgr
        .state()
        .monitors()
        .into_iter()
        .find(|m| m.connector.as_deref() == Some("HEADLESS-1"))
        .unwrap();
    mgr.state_mut().set_focused_monitor(Some(first.id.clone()));
    let ok = mgr.dispatch_until(WAIT, |s| on(s, "HEADLESS-1")).unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    // Same node, same placement: the same surface id.
    assert_eq!(mgr.state().surfaces()[0].id, info.id);
    assert_eq!(
        mgr.state().host().attached.last(),
        Some(&(info.id, PANEL, Some(first.id.clone())))
    );
    settle(&mut mgr);
    assert_eq!(sway.grim("HEADLESS-1").rgb(960, 540), BLUE);

    // Unplugging its output brings it back on the other one.
    sway.msg(&["output", "HEADLESS-1", "unplug"]);
    let ok = mgr.dispatch_until(WAIT, |s| on(s, &second)).unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
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

    let Some(sway) = Sway::start("pointer_events_arrive_in_surface_coordinates") else {
        return;
    };
    // At 1.5, positions are logical: physical (150, 15) is (100, 10).
    sway.msg(&["output", "HEADLESS-1", "scale", "1.5"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    // An OSD in the middle of the screen, click-through.
    const OSD: NodeId = NodeId::new(9, 0);
    mgr.state_mut().apply_surface_change(
        OSD,
        SurfaceChange::Created(layer_spec(NodeKind::Osd, "Level", "center", 200.0, 100.0)),
    );
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 2);
    let id = mgr.state().surfaces_of(BAR)[0];
    let osd = mgr.state().surfaces_of(OSD)[0];
    assert!(mgr.state().surface(osd).unwrap().click_through);
    assert!(!mgr.state().surface(id).unwrap().click_through);

    // A virtual pointer gives the seat a pointer.
    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));

    use wayland_client::protocol::wl_pointer;
    // A click in the middle of the OSD goes through it.
    pointer.motion_absolute(1, 960, 540, 1920, 1080);
    pointer.frame();
    pointer.button(2, 0x110, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.button(3, 0x110, wl_pointer::ButtonState::Released);
    pointer.frame();
    // Then one on the bar, and a scroll.
    pointer.motion_absolute(4, 150, 15, 1920, 1080);
    pointer.frame();
    pointer.button(5, 0x110, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.axis_source(wl_pointer::AxisSource::Wheel);
    pointer.axis(6, wl_pointer::Axis::VerticalScroll, 15.0);
    pointer.frame();
    pointer.button(7, 0x110, wl_pointer::ButtonState::Released);
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
    assert!(
        (buttons[0].3.x - 100.0).abs() < 1.0 && (buttons[0].3.y - 10.0).abs() < 1.0,
        "{buttons:?}"
    );
    // Nothing for the OSD: clicks pass through it.
    assert!(events.iter().all(|e| e.surface() != osd), "{events:?}");
    let axis = events
        .iter()
        .find_map(|e| match e {
            InputEvent::PointerAxis {
                surface,
                vertical,
                source,
                ..
            } => Some((*surface, *vertical, *source)),
            _ => None,
        })
        .expect("a scroll");
    assert_eq!(axis.0, id);
    assert!((axis.1.pixels - 15.0).abs() < 0.01, "{axis:?}");
    assert_eq!(axis.2, Some(strand_surface::AxisSource::Wheel));
    // The host saw the same events, on the main thread.
    assert_eq!(mgr.state().host().input, events);
    // The cursor was set on enter, and the press serial kept for popups.
    assert!(
        mgr.state().stats().cursor_sets >= 1,
        "{:?}",
        mgr.state().stats()
    );
    assert!(mgr.state().last_button_serial().is_some());
    drop(pointer);
}

/// A shadowed panel: the overhang grows the layer surface and moves its
/// margins (its box stays at margin 40 from the top-left corner), and the
/// input region is the box: a click on the shadow goes past the surface,
/// a click on the box arrives in surface coordinates (overhang included).
#[test]
fn shadow_overhang_grows_the_surface_but_not_its_input() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("shadow_overhang_grows_the_surface_but_not_its_input") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Card", "top_left", 200.0, 100.0);
    spec.margin = Insets::all(40.0);
    spec.overhang = Insets::all(20.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.logical_size, (240, 140));
    assert_eq!(info.input_region, Some(Some((20, 20, 200, 100))));

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // On the shadow (10 px left of the box), then on the box.
    click(1, 30, 60);
    click(10, 60, 60);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && events
            .iter()
            .filter(|e| matches!(e, InputEvent::PointerButton { .. }))
            .count()
            < 2
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    let buttons: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton { position, .. } => Some(*position),
            _ => None,
        })
        .collect();
    assert_eq!(buttons.len(), 2, "only the box takes the click: {events:?}");
    // Box at 40 on screen = 20 into the surface past its 20 px overhang.
    assert!(
        buttons
            .iter()
            .all(|p| (p.x - 40.0).abs() < 1.0 && (p.y - 40.0).abs() < 1.0),
        "{buttons:?}"
    );
    drop(pointer);
}

/// (M6) A painter's input region (`Painter::input_region`, a transparent
/// popup's drawn content) replaces the surface's own: a click elsewhere
/// on the surface passes through.
#[test]
fn a_painted_input_region_lets_the_rest_pass_through() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_painted_input_region_lets_the_rest_pass_through") else {
        return;
    };
    // The painter takes input only on a 50 x 40 box inside the panel
    // (a popup's drawn content), its edges fractional.
    let mut host = TestHost::default();
    host.input_rects = Some(vec![LogicalRect::new(100.2, 30.0, 49.6, 40.0)]);
    let mut mgr =
        strand_surface::SurfaceManager::with_connection(sway.connect(), host, Config::default())
            .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Card", "top_left", 200.0, 100.0);
    spec.margin = Insets::all(40.0);
    spec.overhang = Insets::all(20.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.logical_size, (240, 140));
    // Rounded out to whole pixels, and the overhang's region gives way.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| i.painted_input.is_some())
        })
        .unwrap();
    assert!(ok);
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(
        info.painted_input,
        Some(vec![strand_scene::Rect::new(100, 30, 50, 40)])
    );

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // On the panel's box outside the painted region (it passes
    // through), then inside it (surface (120, 50) = screen (140, 70)).
    click(1, 60, 60);
    click(10, 140, 70);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && events
            .iter()
            .filter(|e| matches!(e, InputEvent::PointerButton { .. }))
            .count()
            < 2
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    let buttons: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton { position, .. } => Some(*position),
            _ => None,
        })
        .collect();
    assert_eq!(
        buttons.len(),
        2,
        "only the painted region takes the click: {events:?}"
    );
    assert!(
        buttons
            .iter()
            .all(|p| (p.x - 120.0).abs() < 1.0 && (p.y - 50.0).abs() < 1.0),
        "{buttons:?}"
    );
    drop(pointer);
}

/// (M6, the review of 66c2f03) The same with a real `xdg_popup` (a
/// `trigger: hover` popup is one): a click in the popup's box outside
/// the painted region passes through it, a click inside reaches it, in
/// the popup's own coordinates.
#[test]
fn a_popups_painted_input_region_lets_the_rest_pass_through() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_popups_painted_input_region_lets_the_rest_pass_through") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const POPUP: NodeId = NodeId::new(8, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);
    let mut popup =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    popup.name = Some("Island".into());
    popup.parent = Some(BAR);
    popup.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
    popup.width = Some(300.0);
    popup.height = Some(100.0);
    popup.hover_trigger = true;
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(popup));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(POPUP)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.origin.is_some() && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let id = mgr.state().surfaces_of(POPUP)[0];
    // The painter takes input only on a 60 x 40 island in the popup.
    let host = mgr.state_mut().host_mut();
    host.input_rects = Some(vec![LogicalRect::new(20.0, 20.0, 60.0, 40.0)]);
    host.input_only = Some(id);
    mgr.state_mut().repaint(id);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| i.painted_input.is_some())
        })
        .unwrap();
    assert!(ok, "the region is applied");
    let (ox, oy) = mgr.state().surface(id).unwrap().origin.unwrap();
    let (ox, oy) = (ox as u32, oy as u32);

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // In the popup's box beside the island (it passes through), then on
    // the island (popup (50, 40)).
    click(1, ox + 200, oy + 60);
    click(10, ox + 50, oy + 40);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && events
            .iter()
            .filter(|e| matches!(e, InputEvent::PointerButton { .. }))
            .count()
            < 2
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    // Anything late from the first click.
    pump(&mut mgr, Duration::from_millis(200));
    events.extend(input.try_iter());
    let buttons: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton {
                surface, position, ..
            } => Some((*surface, *position)),
            _ => None,
        })
        .collect();
    assert_eq!(
        buttons.len(),
        2,
        "only the island takes the click: {events:?}"
    );
    assert!(
        buttons
            .iter()
            .all(|(s, p)| *s == id && (p.x - 50.0).abs() < 1.0 && (p.y - 40.0).abs() < 1.0),
        "{buttons:?}"
    );
    drop(pointer);
}

/// (M4) A panel the compositor holds at half size (a delegated scale
/// pose): its input region shrinks with it, and a click arrives in the
/// coordinates of the content at rest, not the destination's, so the
/// node under the pointer is the one hit.
#[test]
fn a_posed_scale_maps_input_back_to_the_content() {
    use strand_scene::SurfacePose;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_posed_scale_maps_input_back_to_the_content") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    let half = SurfacePose {
        scale: 0.5,
        ..SurfacePose::IDENTITY
    };
    mgr.state_mut().host_mut().poses = [half].into_iter().collect();
    let mut spec = layer_spec(NodeKind::Panel, "Card", "top_left", 200.0, 100.0);
    spec.margin = Insets::all(40.0);
    spec.overhang = Insets::all(20.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);
    pump(&mut mgr, Duration::from_millis(200));
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.logical_size, (240, 140));
    assert_eq!(info.pose, half);
    // The box (20, 20, 200, 100) at half size.
    assert_eq!(info.input_region, Some(Some((10, 10, 100, 50))));

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // The surface lies at 20 (the box's margin 40 less its overhang),
    // drawn 120 × 70: the half-size box covers 30..130 × 30..80 on
    // screen. A click at 35 is 15 into the surface, outside the box at
    // rest but inside the half-size one; at 135 it is 115 in, inside
    // the box at rest and the drawn surface but past the half-size box;
    // then 100, 70.
    click(1, 35, 35);
    click(10, 135, 50);
    click(20, 100, 70);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && events
            .iter()
            .filter(|e| matches!(e, InputEvent::PointerButton { .. }))
            .count()
            < 4
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    pump(&mut mgr, Duration::from_millis(100));
    events.extend(input.try_iter());
    let presses: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton {
                position,
                state: ButtonState::Pressed,
                ..
            } => Some(*position),
            _ => None,
        })
        .collect();
    assert_eq!(presses.len(), 2, "the half-size box takes two: {events:?}");
    // In the content at rest: 15 and 80, 50 into the surface, doubled.
    let near = |p: strand_scene::LogicalPoint, x: f32, y: f32| {
        (p.x - x).abs() < 2.5 && (p.y - y).abs() < 2.5
    };
    assert!(near(presses[0], 30.0, 30.0), "{presses:?}");
    assert!(near(presses[1], 160.0, 100.0), "{presses:?}");
    drop(pointer);
}

/// (M4) A popup opened from a panel the compositor holds at half size
/// is anchored where the panel's content is drawn: its anchor rect is
/// scaled with the parent's surface-local coordinates.
#[test]
fn a_popup_of_a_posed_panel_opens_at_its_drawn_anchor() {
    use strand_scene::SurfacePose;
    let Some(sway) = Sway::start("a_popup_of_a_posed_panel_opens_at_its_drawn_anchor") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    const POPUP: NodeId = NodeId::new(8, 0);
    let half = SurfacePose {
        scale: 0.5,
        ..SurfacePose::IDENTITY
    };
    mgr.state_mut().host_mut().poses = [half].into_iter().collect();
    let spec = layer_spec(NodeKind::Panel, "Card", "top_left", 400.0, 300.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    wait_for_bars(&mut mgr, 1);
    pump(&mut mgr, Duration::from_millis(200));
    let panel = mgr.state().surfaces_of(PANEL)[0];
    assert_eq!(mgr.state().surface(panel).unwrap().pose, half);
    // A 100 × 50 popup under a 60 × 20 button at (200, 100) in the
    // panel, drawn at (100, 50, 30, 10).
    let mut popup =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    popup.name = Some("Menu".into());
    popup.parent = Some(PANEL);
    popup.anchor_rect = Some(LogicalRect::new(200.0, 100.0, 60.0, 20.0));
    popup.width = Some(100.0);
    popup.height = Some(50.0);
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(popup));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(POPUP)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.origin.is_some())
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let id = mgr.state().surfaces_of(POPUP)[0];
    let (x, y) = mgr.state().surface(id).unwrap().origin.unwrap();
    // Centred under the drawn button (115) and below it (60 plus the
    // gap), not under the button at rest (230, 120).
    assert_eq!(x, 65, "origin ({x}, {y})");
    assert!((60..=72).contains(&y), "origin ({x}, {y})");
}

/// An open `keyboard: exclusive` panel whose `open` is two-way (the
/// design's launcher) gets a transparent catcher under it: a click
/// outside it is `ClickAway` on the panel, a click inside is the panel's
/// own button event; closing the panel takes the catcher away, and a
/// panel whose `open` is one-way gets none.
#[test]
fn a_click_outside_an_exclusive_panel_is_click_away() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_click_outside_an_exclusive_panel_is_click_away") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    const PLAIN: NodeId = NodeId::new(8, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    spec.open_two_way = true;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec.clone()));
    let input = mgr.take_input().unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.click_away && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // Outside (top left of the output), then inside (its middle).
    click(1, 100, 100);
    click(10, 960, 540);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    let done = |events: &[InputEvent]| {
        events
            .iter()
            .any(|e| matches!(e, InputEvent::ClickAway { .. }))
            && events
                .iter()
                .filter(|e| matches!(e, InputEvent::PointerButton { .. }))
                .count()
                >= 2
    };
    while Instant::now() < deadline && !done(&events) {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    let id = mgr.state().surfaces_of(PANEL)[0];
    let away: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, InputEvent::ClickAway { .. }))
        .collect();
    assert_eq!(away, [&InputEvent::ClickAway { surface: id }], "{events:?}");
    let inside: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::PointerButton {
                surface, position, ..
            } => Some((*surface, *position)),
            _ => None,
        })
        .collect();
    assert_eq!(inside.len(), 2, "press and release inside: {events:?}");
    assert!(
        inside
            .iter()
            .all(|(s, p)| *s == id && (p.x - 100.0).abs() < 1.0 && (p.y - 50.0).abs() < 1.0),
        "{inside:?}"
    );

    // Closed: the catcher goes with it; a one-way `open` gets none.
    spec.open = false;
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: spec.clone(),
            recreate: false,
        },
    );
    let mut plain = spec.clone();
    plain.name = Some("Plain".into());
    plain.open = true;
    plain.open_two_way = false;
    mgr.state_mut()
        .apply_surface_change(PLAIN, SurfaceChange::Created(plain));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(PANEL).is_empty()
                && s.surfaces()
                    .iter()
                    .any(|i| i.node == PLAIN && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    assert!(mgr.state().surfaces().iter().all(|i| !i.click_away));
    events.clear();
    click(20, 100, 100);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(300));
    events.extend(input.try_iter());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, InputEvent::ClickAway { .. })),
        "{events:?}"
    );
    drop(pointer);
}

/// With two outputs, an open `keyboard: exclusive` panel on one gets a
/// catcher over the other too (the whole output, bars included): a click
/// there is a click away from it.
#[test]
fn a_click_on_another_output_is_click_away() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_click_on_another_output_is_click_away") else {
        return;
    };
    let second = sway.create_output();
    sway.msg(&["output", "HEADLESS-1", "position", "0", "0"]);
    sway.msg(&["output", &second, "position", "1920", "0"]);
    sway.msg(&["focus", "output", "HEADLESS-1"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    spec.open_two_way = true;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let input = mgr.take_input().unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.click_away && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    pump(&mut mgr, Duration::from_millis(300));
    let id = mgr.state().surfaces_of(PANEL)[0];

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    // The middle of the second output, in a 3840 × 1080 layout.
    pointer.motion_absolute(1, 1920 + 960, 540, 3840, 1080);
    pointer.frame();
    pointer.button(2, 0x110, wl_pointer::ButtonState::Pressed);
    pointer.frame();
    pointer.button(3, 0x110, wl_pointer::ButtonState::Released);
    pointer.frame();
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline
        && !events
            .iter()
            .any(|e| matches!(e, InputEvent::ClickAway { .. }))
    {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    assert!(
        events.contains(&InputEvent::ClickAway { surface: id }),
        "{events:?}"
    );
    drop(pointer);
}

/// `popup`s are anchored `xdg_popup`s that nest (design.md, "Input and
/// structure"): one in a top bar opens below the bar under its anchor,
/// its box (the window geometry) placed and its shadow overhang around
/// it; one nested in it opens below its own anchor; a click away ends
/// the grab and both are dismissed (`ClickAway` on each, then gone),
/// and they are not shown again until their specs close and reopen.
#[test]
fn popups_nest_under_their_anchors_and_close_on_click_away() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("popups_nest_under_their_anchors_and_close_on_click_away") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const BAR: NodeId = NodeId::new(1, 0);
    const POPUP: NodeId = NodeId::new(2, 0);
    const MENU: NodeId = NodeId::new(3, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);

    // A press on the bar: the serial the popup's grab uses.
    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    click(1, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .iter()
                .any(|e| matches!(e, InputEvent::PointerButton { .. }))
        })
        .unwrap();
    assert!(ok, "the press reached the bar");

    // The calendar: 200 × 120 with a 10 px shadow, anchored to a 60 × 20
    // clock at (100, 8) in the bar.
    let mut spec =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    spec.name = Some("Calendar".into());
    spec.parent = Some(BAR);
    spec.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
    spec.width = Some(200.0);
    spec.height = Some(120.0);
    spec.overhang = Insets::all(10.0);
    spec.open_two_way = true;
    // Both opened by the click (`SceneDiff::press`).
    let serial = *mgr.state().host().press_serials.last().unwrap();
    mgr.state_mut().set_popup_cause(POPUP, Some(serial));
    mgr.state_mut().set_popup_cause(MENU, Some(serial));
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(spec.clone()));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(POPUP)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let popup = mgr.state().surfaces_of(POPUP)[0];
    let info = mgr.state().surface(popup).unwrap();
    assert_eq!(info.kind, NodeKind::Popup);
    assert_eq!(info.logical_size, (220, 140), "box plus overhang");
    assert_eq!(info.input_region, Some(Some((10, 10, 200, 120))));
    pump(&mut mgr, Duration::from_millis(100));
    let shot = sway.grim("HEADLESS-1");
    // Below the bar (36) and a 6 px gap, centred under the clock (130):
    // the box spans x 30..230, y 42..162, its shadow 10 px around it.
    assert_eq!(shot.rgb(130, 100), BLUE, "the popup's box");
    assert_eq!(shot.rgb(25, 100), BLUE, "its overhang");
    assert_ne!(shot.rgb(130, 175), BLUE, "nothing past the overhang");
    assert_ne!(shot.rgb(10, 100), BLUE);
    // (M4) Where each lies on the output, as the host is told: the bar
    // at the top-left corner, the popup's buffer 10 px outside its box.
    let bar = mgr.state().surfaces_of(BAR)[0];
    assert_eq!(mgr.state().surface(bar).unwrap().origin, Some((0, 0)));
    assert_eq!(info.origin, Some((20, 32)));
    assert!(
        mgr.state().host().placed.contains(&(popup, (20, 32))),
        "{:?}",
        mgr.state().host().placed
    );

    // A menu nested in it, anchored to (20, 30, 40, 20) in its buffer.
    let mut menu = spec.clone();
    menu.name = Some("Menu".into());
    menu.parent = Some(POPUP);
    menu.anchor_rect = Some(LogicalRect::new(20.0, 30.0, 40.0, 20.0));
    menu.width = Some(80.0);
    menu.height = Some(60.0);
    menu.overhang = Insets::default();
    mgr.state_mut()
        .apply_surface_change(MENU, SurfaceChange::Created(menu.clone()));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(MENU)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "the nested popup maps: {:?}", mgr.state().surfaces());
    // (M4) A submenu: to the right of its anchor, 6 px away, level with
    // its top. The anchor is (10, 20, 40, 20) in the calendar's box at
    // (30, 42), so the menu's box starts at (30 + 50 + 6, 42 + 20), as
    // sway's configure placed it.
    let nested = mgr.state().surfaces_of(MENU)[0];
    assert_eq!(
        mgr.state().surface(nested).unwrap().origin,
        Some((86, 62)),
        "{:?}",
        mgr.state().host().placed
    );
    // `anchor: left` opens it to the left instead, repositioned in
    // place: from a row at (140, 20) in the calendar's box, it ends 6 px
    // before the row (30 + 140 - 6 - 80).
    let mut left = menu.clone();
    left.anchor = strand_scene::Anchor::Left;
    left.anchor_rect = Some(LogicalRect::new(150.0, 30.0, 40.0, 20.0));
    mgr.state_mut().apply_surface_change(
        MENU,
        SurfaceChange::Updated {
            spec: left,
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(MENU)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.origin == Some((84, 62)))
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().host().placed);

    // A click on the desktop ends the grab: both are dismissed.
    let before = mgr.state().host().input.len();
    click(10, 1500, 800);
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(POPUP).is_empty() && s.surfaces_of(MENU).is_empty()
        })
        .unwrap();
    assert!(ok, "dismissed: {:?}", mgr.state().surfaces());
    let away: Vec<_> = mgr.state().host().input[before..]
        .iter()
        .filter_map(|e| match e {
            InputEvent::ClickAway { surface } => Some(*surface),
            _ => None,
        })
        .collect();
    assert!(away.contains(&popup), "{away:?}");
    assert!(!mgr.state().surfaces_of(BAR).is_empty(), "the bar stays");
    // Still open in its spec (logic has not answered): not shown again.
    mgr.state_mut().apply_surface_change(
        POPUP,
        SurfaceChange::Updated {
            spec: spec.clone(),
            recreate: false,
        },
    );
    pump(&mut mgr, Duration::from_millis(200));
    assert!(mgr.state().surfaces_of(POPUP).is_empty());
    // Closed and opened again: it comes back.
    let mut closed = spec.clone();
    closed.open = false;
    for s in [closed, spec] {
        mgr.state_mut().apply_surface_change(
            POPUP,
            SurfaceChange::Updated {
                spec: s,
                recreate: false,
            },
        );
    }
    let ok = mgr
        .dispatch_until(WAIT, |s| !s.surfaces_of(POPUP).is_empty())
        .unwrap();
    assert!(ok);
    drop(input);
    drop(pointer);
}

/// A popup grabs only with the serial of a recent press (within
/// `GRAB_WINDOW`): one opened by a timer or IPC seconds after the last
/// click has no grab (compositors that check the serial would end it at
/// once), and stays shown. A second grabbing popup outside the first's
/// chain dismisses the first (xdg-shell's topmost grab rule).
#[test]
fn popups_grab_only_right_after_a_press() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("popups_grab_only_right_after_a_press") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const BAR: NodeId = NodeId::new(1, 0);
    const CALENDAR: NodeId = NodeId::new(2, 0);
    const VOLUME: NodeId = NodeId::new(3, 0);
    const LATE: NodeId = NodeId::new(4, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    // A first click on the desktop wakes the new virtual pointer up.
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    click(1, 1500, 800);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(100));
    click(10, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .iter()
                .any(|e| matches!(e, InputEvent::PointerButton { .. }))
        })
        .unwrap();
    assert!(ok, "the press reached the bar");

    let popup =
        |name: &str, x: f32| {
            let mut spec = strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| {
                None::<&strand_scene::PropValue>
            });
            spec.name = Some(name.into());
            spec.parent = Some(BAR);
            spec.anchor_rect = Some(LogicalRect::new(x, 8.0, 60.0, 20.0));
            spec.width = Some(200.0);
            spec.height = Some(120.0);
            spec.open_two_way = true;
            spec
        };
    let mapped = |mgr: &mut strand_surface::SurfaceManager<TestHost>, node: NodeId| {
        mgr.dispatch_until(WAIT, |s| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap()
    };

    // Opened by the click: it grabs.
    let serial = *mgr.state().host().press_serials.last().unwrap();
    for node in [CALENDAR, VOLUME, LATE] {
        mgr.state_mut().set_popup_cause(node, Some(serial));
    }
    mgr.state_mut()
        .apply_surface_change(CALENDAR, SurfaceChange::Created(popup("Calendar", 100.0)));
    assert!(mapped(&mut mgr, CALENDAR));
    assert_eq!(mgr.state().stats().grabs, 1);
    let calendar = mgr.state().surfaces_of(CALENDAR)[0];

    // A sibling grabbing popup, still within the window: the calendar
    // goes first, told as a click away.
    let before = mgr.state().host().input.len();
    mgr.state_mut()
        .apply_surface_change(VOLUME, SurfaceChange::Created(popup("Volume", 600.0)));
    assert!(mapped(&mut mgr, VOLUME));
    assert_eq!(mgr.state().stats().grabs, 2);
    assert!(mgr.state().surfaces_of(CALENDAR).is_empty());
    assert!(
        mgr.state().host().input[before..].contains(&InputEvent::ClickAway { surface: calendar }),
        "{:?}",
        &mgr.state().host().input[before..]
    );
    let bar = mgr.state().surfaces_of(BAR)[0];
    assert!(mgr.state().holds_keyboard_for_popup(bar));
    let volume = mgr.state().surfaces_of(VOLUME)[0];

    // Long after the click (a timer, IPC): no grab, and it stays shown,
    // as does the grabbing volume menu (a popup with no grab is not
    // bound by the topmost-grab rule).
    pump(
        &mut mgr,
        strand_surface::GRAB_WINDOW + Duration::from_millis(700),
    );
    let before = mgr.state().host().input.len();
    mgr.state_mut()
        .apply_surface_change(LATE, SurfaceChange::Created(popup("Late", 300.0)));
    assert!(mapped(&mut mgr, LATE));
    assert_eq!(mgr.state().stats().grabs, 2, "no grab with a stale serial");
    pump(&mut mgr, Duration::from_millis(600));
    assert_eq!(mgr.state().surfaces_of(LATE).len(), 1, "still shown");
    assert_eq!(
        mgr.state().surfaces_of(VOLUME),
        vec![volume],
        "the grabbing sibling stays open"
    );
    assert!(
        !mgr.state().host().input[before..]
            .iter()
            .any(|e| matches!(e, InputEvent::ClickAway { .. })),
        "{:?}",
        &mgr.state().host().input[before..]
    );

    // The volume menu closes: the late popup alone does not hold the
    // keyboard.
    let mut closed = popup("Volume", 600.0);
    closed.open = false;
    mgr.state_mut().apply_surface_change(
        VOLUME,
        SurfaceChange::Updated {
            spec: closed,
            recreate: false,
        },
    );
    pump(&mut mgr, Duration::from_millis(100));
    assert!(mgr.state().surfaces_of(VOLUME).is_empty());
    assert_eq!(mgr.state().surfaces_of(LATE).len(), 1);
    assert!(
        !mgr.state().holds_keyboard_for_popup(bar),
        "a popup with no grab takes no keyboard"
    );
    let shot = sway.grim("HEADLESS-1");
    // Centred under its anchor (330), below the bar.
    assert_eq!(shot.rgb(330, 100), BLUE, "the late popup is drawn");
    drop(input);
    drop(pointer);
}

/// Escape reaches a grabbing popup over a `keyboard: none` bar on a real
/// compositor (a virtual keyboard, `zwp_virtual_keyboard_v1`): the popup
/// gets keyboard focus from its grab, and the key arrives on it (the
/// router turns it into `open: false`, tested offline in
/// strand-render's `popups.rs::escape_closes_a_popup`).
#[test]
fn escape_reaches_a_grabbing_popup() {
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };
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
    delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Client: ignore wl_seat::WlSeat);

    let Some(sway) = Sway::start("escape_reaches_a_grabbing_popup") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const BAR: NodeId = NodeId::new(1, 0);
    const POPUP: NodeId = NodeId::new(2, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    // A keyboard with one key, Escape (keycode 9 = evdev 1 + 8), its
    // keymap self-contained (no xkeyboard-config includes).
    let kbd_manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let keyboard = kbd_manager.create_virtual_keyboard(&seat, &qh, ());
    let keymap = "xkb_keymap {\n\
        xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <ESC> = 9; };\n\
        xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
        xkb_compatibility \"strand\" { };\n\
        xkb_symbols \"strand\" { key <ESC> { [ Escape ] }; };\n\
        };\n";
    let path = std::env::temp_dir().join(format!("strand-keymap-{}", std::process::id()));
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(keymap.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    file.flush().unwrap();
    let file = std::fs::File::open(&path).unwrap();
    keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
    let ptr_manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = ptr_manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(300));
    let _ = std::fs::remove_file(&path);

    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // Wake the new pointer on the desktop, then click the bar.
    click(1, 1500, 800);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(100));
    click(10, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .iter()
                .any(|e| matches!(e, InputEvent::PointerButton { .. }))
        })
        .unwrap();
    assert!(ok, "the press reached the bar");

    let mut spec =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    spec.name = Some("Calendar".into());
    spec.parent = Some(BAR);
    spec.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
    spec.width = Some(200.0);
    spec.height = Some(120.0);
    spec.open_two_way = true;
    // Opened by the click (`SceneDiff::press`).
    let serial = *mgr.state().host().press_serials.last().unwrap();
    mgr.state_mut().set_popup_cause(POPUP, Some(serial));
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(POPUP)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "the popup maps");
    assert_eq!(mgr.state().stats().grabs, 1);
    let popup = mgr.state().surfaces_of(POPUP)[0];
    // The bar holds the keyboard while its grabbing popup is open, and
    // the popup is told it has it.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .contains(&InputEvent::KeyboardEnter { surface: popup })
        })
        .unwrap();
    assert!(
        ok,
        "the grabbing popup has the keyboard: {:?} {:?}",
        mgr.state().keyboard_focus(),
        mgr.state().host().input
    );

    // Escape, pressed and released (evdev KEY_ESC = 1).
    keyboard.key(100, 1, 1);
    keyboard.key(101, 1, 0);
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().input.iter().any(|e| {
                matches!(e, InputEvent::Key { surface, key }
                    if *surface == popup && key.name == "Escape"
                        && key.state == ButtonState::Pressed)
            })
        })
        .unwrap();
    assert!(
        ok,
        "Escape reached the popup: {:?}",
        mgr.state().host().input
    );
    let bar = mgr.state().surfaces_of(BAR)[0];
    assert!(mgr.state().holds_keyboard_for_popup(bar));
    // Logic answers `open: false`: the popup goes, and the bar gives the
    // keyboard back (`keyboard: none` again; with no window to focus,
    // headless sway leaves the focus where it was).
    let mut closed =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    closed.name = Some("Calendar".into());
    closed.parent = Some(BAR);
    closed.open = false;
    let before = mgr.state().host().input.len();
    mgr.state_mut().apply_surface_change(
        POPUP,
        SurfaceChange::Updated {
            spec: closed,
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces_of(POPUP).is_empty())
        .unwrap();
    assert!(ok, "closed");
    assert!(!mgr.state().holds_keyboard_for_popup(bar));
    // Keys go back to the surface with focus, which is told so (a
    // launcher's input takes its caret back after its menu closes).
    assert_eq!(mgr.state().keyboard_focus(), Some(bar));
    assert!(
        mgr.state().host().input[before..].contains(&InputEvent::KeyboardEnter { surface: bar }),
        "{:?}",
        &mgr.state().host().input[before..]
    );
    drop(input);
    drop(pointer);
    drop(keyboard);
}

/// A keyboard that goes away after a key press (a USB keyboard unplugged,
/// a KVM switch) leaves the shell running: key repeat is strand's own
/// calloop timer, stopped from the seat's events, so nothing removes a
/// source while calloop's sources are borrowed (SCTK's repeat did, from
/// a `Drop`, and aborted the process). A held key repeats at the seat's
/// rate first; the surface still paints after the keyboard is gone.
#[test]
fn a_keyboard_going_away_after_a_press_leaves_the_shell_running() {
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
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
    delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Client: ignore wl_seat::WlSeat);

    let Some(sway) = Sway::start("a_keyboard_going_away_after_a_press_leaves_the_shell_running")
    else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    // A `keyboard: exclusive` panel: it has the keyboard once one exists.
    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec.clone()));
    let input = mgr.take_input().unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces().iter().any(|i| i.stats.commits > 0))
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let id = mgr.state().surfaces_of(PANEL)[0];

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let kbd_manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let keyboard = kbd_manager.create_virtual_keyboard(&seat, &qh, ());
    let keymap = "xkb_keymap {\n\
        xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <AC01> = 38; };\n\
        xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
        xkb_compatibility \"strand\" { };\n\
        xkb_symbols \"strand\" { key <AC01> { [ a ] }; };\n\
        };\n";
    let path = std::env::temp_dir().join(format!("strand-keymap-unplug-{}", std::process::id()));
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(keymap.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    file.flush().unwrap();
    let file = std::fs::File::open(&path).unwrap();
    keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
    queue.roundtrip(&mut Client).unwrap();
    let _ = std::fs::remove_file(&path);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .contains(&InputEvent::KeyboardEnter { surface: id })
        })
        .unwrap();
    assert!(
        ok,
        "the panel has the keyboard: {:?}",
        mgr.state().host().input
    );

    // `a` held (evdev KEY_A = 30): it repeats.
    keyboard.key(100, 30, 1);
    queue.roundtrip(&mut Client).unwrap();
    let mut events = Vec::new();
    let deadline = Instant::now() + WAIT;
    let repeats = |events: &[InputEvent]| {
        events
            .iter()
            .filter(|e| matches!(e, InputEvent::Key { key, .. } if key.repeat && key.text == "a"))
            .count()
    };
    while Instant::now() < deadline && repeats(&events) < 2 {
        mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
        events.extend(input.try_iter());
    }
    assert!(repeats(&events) >= 2, "a held key repeats: {events:?}");
    assert!(mgr.state().key_repeating());

    // Released: the repeat stops.
    keyboard.key(200, 30, 0);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    assert!(!mgr.state().key_repeating());
    events.clear();
    events.extend(input.try_iter());
    pump(&mut mgr, Duration::from_millis(300));
    events.extend(input.try_iter());
    assert_eq!(
        repeats(&events[events
            .iter()
            .position(|e| matches!(e, InputEvent::Key { key, .. } if key.state == ButtonState::Released))
            .map_or(0, |i| i + 1)..]),
        0,
        "repeats after the release: {events:?}"
    );

    // Pressed again, and the keyboard goes away while it is held.
    keyboard.key(300, 30, 1);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(100));
    keyboard.destroy();
    queue.roundtrip(&mut Client).unwrap();
    // (Before the fix this dispatch panicked inside calloop and aborted.)
    pump(&mut mgr, Duration::from_millis(800));
    assert!(
        !mgr.state().key_repeating(),
        "the repeat outlived its keyboard"
    );

    // The panel still paints: a new size is configured and committed.
    let commits = mgr.state().surface(id).unwrap().stats.commits;
    spec.height = Some(120.0);
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec,
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.logical_size == (200, 120) && i.stats.commits > commits)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    drop(input);
}

/// Sway 1.9 offers the viewporter, single-pixel buffers, the session
/// lock and the data device, and no background effect: the manager
/// reports exactly that once its globals are bound (the alpha modifier
/// came with sway 1.10, so it is not asserted).
#[test]
fn sway_reports_its_capabilities() {
    let Some((_sway, mut mgr)) = start("sway_reports_its_capabilities", Config::default()) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let caps = mgr.state().host().caps.clone();
    assert_eq!(caps.len(), 1, "reported once: {caps:?}");
    let c = caps[0];
    assert!(c.viewporter && c.single_pixel_buffer, "{c:?}");
    assert!(c.session_lock && c.data_device, "{c:?}");
    assert!(!c.background_effect, "{c:?}");
    assert_eq!(mgr.state().compositor_caps(), c);
}

/// `scrim:` on sway 1.9 (single-pixel buffers and the viewporter): a
/// panel's scrim dims the usable area beneath it, under the panel (its
/// box keeps its colour: sway 1.9 stacks the older of two layer surfaces
/// on one layer on top, so the scrim is on the layer below the panel's)
/// and not over the bar; a popup's goes on its bar's layer and output,
/// under the popup. Taking a scrim away undims. A popup of a panel puts
/// its scrim on the layer below the panel, which rises to `overlay` for
/// it, so the scrim dims neither the panel nor the popup (sway 1.9 would
/// show the panel above a scrim on its own layer anyway; the matrix runs
/// the compositors that would not). A bar whose shadow reaches past its
/// exclusive zone is a panel to its popup's scrim: it rises, its shadow
/// undimmed.
#[test]
fn scrims_dim_beneath_panels_and_popups() {
    let Some(sway) = Sway::start("scrims_dim_beneath_panels_and_popups") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    assert!(mgr.state().compositor_caps().single_pixel_buffer);
    const PANEL: NodeId = NodeId::new(7, 0);
    const POPUP: NodeId = NodeId::new(2, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    settle(&mut mgr);
    let bg = sway.grim("HEADLESS-1").rgb(500, 600);
    assert_ne!(bg, BLUE);
    let dimmed = bg.map(|c| (f32::from(c) * 0.7).round() as u8);
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2);
    let dim = strand_scene::Color::new(0.0, 0.0, 0.0, 0.3);

    let mut spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    spec.scrim = Some(dim);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec.clone()));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == PANEL && i.scrim.is_some() && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert!(
        near(shot.rgb(500, 600), dimmed),
        "dimmed: {:?} vs {dimmed:?}",
        shot.rgb(500, 600)
    );
    assert_eq!(shot.rgb(1720, 186), BLUE, "the panel is above its scrim");
    assert_eq!(shot.rgb(10, 10), BLUE, "the bar is not dimmed");

    // No scrim: undimmed.
    spec.scrim = None;
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: spec.clone(),
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces().iter().all(|i| i.scrim.is_none()))
        .unwrap();
    assert!(ok);
    settle(&mut mgr);
    assert_eq!(sway.grim("HEADLESS-1").rgb(500, 600), bg);

    // A popup of the bar with a scrim.
    let mut popup =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    popup.name = Some("Calendar".into());
    popup.parent = Some(BAR);
    popup.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
    popup.width = Some(200.0);
    popup.height = Some(120.0);
    popup.scrim = Some(dim);
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(popup));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == POPUP && i.scrim.is_some() && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert!(
        near(shot.rgb(500, 600), dimmed),
        "dimmed: {:?}",
        shot.rgb(500, 600)
    );
    assert_eq!(shot.rgb(130, 100), BLUE, "the popup is above its scrim");
    assert_eq!(shot.rgb(10, 10), BLUE, "the bar is not dimmed");
    let bar_popup = mgr.state().surfaces_of(POPUP)[0];
    assert_eq!(
        mgr.state().surface(bar_popup).unwrap().under_layer,
        Some(Layer::Top),
        "beside the bar, outside its exclusive zone"
    );

    // A popup of the panel (no scrim of its own) with a scrim.
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Removed);
    let mut popup =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    popup.name = Some("Menu".into());
    popup.parent = Some(PANEL);
    popup.anchor_rect = Some(LogicalRect::new(100.0, 200.0, 60.0, 20.0));
    popup.width = Some(200.0);
    popup.height = Some(120.0);
    popup.scrim = Some(dim);
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(popup));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == POPUP && i.scrim.is_some() && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let panel = mgr.state().surfaces_of(PANEL)[0];
    let menu = mgr.state().surfaces_of(POPUP)[0];
    assert_eq!(
        mgr.state().surface(panel).unwrap().layer,
        Some(Layer::Overlay),
        "the panel rises for its popup's scrim"
    );
    assert_eq!(
        mgr.state().surface(menu).unwrap().under_layer,
        Some(Layer::Top),
        "the scrim below the panel"
    );
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert!(
        near(shot.rgb(500, 600), dimmed),
        "dimmed: {:?}",
        shot.rgb(500, 600)
    );
    assert_eq!(shot.rgb(1720, 100), BLUE, "the panel is above the scrim");
    assert_eq!(shot.rgb(1650, 340), BLUE, "the popup is above the scrim");
    assert_eq!(shot.rgb(10, 10), BLUE, "the bar is not dimmed");

    // The popup goes: the panel goes back to `top`, undimmed.
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Removed);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == PANEL && i.layer == Some(Layer::Top) && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    settle(&mut mgr);
    assert_eq!(sway.grim("HEADLESS-1").rgb(500, 600), bg);

    // The bar gains a 10 px shadow below it, past its exclusive zone, in
    // the usable area a popup's scrim covers: with a scrimmed popup it
    // rises to `overlay`, the scrim on `top` below it, so the shadow
    // strip is not dimmed on a compositor that draws the newer surface
    // of one layer on top either.
    let mut shaded = bar_spec("Top", 36.0);
    shaded.overhang.bottom = 10.0;
    mgr.state_mut().apply_surface_change(
        BAR,
        SurfaceChange::Updated {
            spec: shaded,
            recreate: false,
        },
    );
    let mut popup =
        strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    popup.name = Some("Calendar".into());
    popup.parent = Some(BAR);
    popup.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
    popup.width = Some(200.0);
    popup.height = Some(120.0);
    popup.scrim = Some(dim);
    mgr.state_mut()
        .apply_surface_change(POPUP, SurfaceChange::Created(popup));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces()
                .iter()
                .any(|i| i.node == POPUP && i.scrim.is_some() && i.stats.commits > 0)
                && s.surfaces()
                    .iter()
                    .any(|i| i.node == BAR && i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let bar = mgr.state().surfaces_of(BAR)[0];
    let tip = mgr.state().surfaces_of(POPUP)[0];
    assert_eq!(
        mgr.state().surface(bar).unwrap().layer,
        Some(Layer::Overlay),
        "a bar shadowing the usable area rises for its popup's scrim"
    );
    assert_eq!(
        mgr.state().surface(tip).unwrap().under_layer,
        Some(Layer::Top),
        "the scrim below the bar"
    );
    settle(&mut mgr);
    let shot = sway.grim("HEADLESS-1");
    assert!(
        near(shot.rgb(500, 600), dimmed),
        "dimmed: {:?}",
        shot.rgb(500, 600)
    );
    assert_eq!(shot.rgb(1000, 10), BLUE, "the bar is not dimmed");
    assert_eq!(shot.rgb(1000, 41), BLUE, "nor is its shadow");
    assert_eq!(shot.rgb(130, 100), BLUE, "the popup is above its scrim");
}

/// (M4) A corner panel's place (what `surface_placed` says, the tray's
/// click point) includes the offset its compositor pose moved its
/// margins by (a root's static `x`/`y` is delegated for good), and
/// follows its output's logical size.
#[test]
fn a_posed_panel_is_placed_where_its_margins_put_it() {
    use strand_scene::{LogicalPoint, SurfacePose};
    let Some(sway) = Sway::start("a_posed_panel_is_placed_where_its_margins_put_it") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const PANEL: NodeId = NodeId::new(7, 0);
    let held = SurfacePose {
        offset: LogicalPoint::new(30.0, 12.0),
        ..SurfacePose::IDENTITY
    };
    mgr.state_mut().host_mut().poses = [held].into_iter().collect();
    let spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let placed = |mgr: &mut strand_surface::SurfaceManager<TestHost>, want: (i32, i32)| {
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.surfaces_of(PANEL)
                    .first()
                    .and_then(|id| s.surface(*id))
                    .is_some_and(|i| i.origin == Some(want))
            })
            .unwrap();
        assert!(ok, "placed at {want:?}: {:?}", mgr.state().host().placed);
        let id = mgr.state().surfaces_of(PANEL)[0];
        assert_eq!(
            mgr.state()
                .host()
                .placed
                .iter()
                .rev()
                .find(|(s, _)| *s == id)
                .map(|(_, o)| *o),
            Some(want),
            "told to the host"
        );
    };
    // Top right of 1920 × 1080: x 1520, moved 30 right (the right margin
    // shrunk by 30) and 12 down.
    placed(&mut mgr, (1550, 12));
    // Scale 2: 960 × 540 logical, the panel keeps its size.
    sway.msg(&["output", "HEADLESS-1", "scale", "2"]);
    placed(&mut mgr, (590, 12));
    // The pose dropped (a GPU hand-off): back at its configured margins
    // at once, and the host is told.
    let id = mgr.state().surfaces_of(PANEL)[0];
    mgr.state_mut().clear_pose(id);
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.origin, Some((560, 0)));
    assert_eq!(mgr.state().host().placed.last(), Some(&(id, (560, 0))));
}

/// (M4) A layer surface is placed where sway arranges it, in the
/// compositor's logical layout: a corner panel past our own bar's
/// exclusive zone (not over it), at the top while the live bar's zone is
/// none and below it once it is back, again at the top once the bar
/// goes, and a bar on a second output at that output's position in the
/// layout.
#[test]
fn layer_surfaces_are_placed_past_our_zones_in_layout_coordinates() {
    let Some((sway, mut mgr)) = start(
        "layer_surfaces_are_placed_past_our_zones_in_layout_coordinates",
        Config::default(),
    ) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    // Every surface paints a red square in its top-left corner.
    mgr.state_mut()
        .host_mut()
        .set_square(Some(LogicalRect::new(0.0, 0.0, 10.0, 10.0)));
    const PANEL: NodeId = NodeId::new(7, 0);
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Created(layer_spec(
            NodeKind::Panel,
            "Dash",
            "top_right",
            400.0,
            300.0,
        )),
    );
    let origin_of = |mgr: &strand_surface::SurfaceManager<TestHost>, node: NodeId| {
        mgr.state()
            .surfaces_of(node)
            .iter()
            .map(|id| mgr.state().surface(*id).and_then(|i| i.origin))
            .collect::<Vec<_>>()
    };
    let wait_origin = |mgr: &mut strand_surface::SurfaceManager<TestHost>,
                       node: NodeId,
                       want: Vec<Option<(i32, i32)>>| {
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.surfaces_of(node)
                    .iter()
                    .map(|id| s.surface(*id).and_then(|i| i.origin))
                    .collect::<Vec<_>>()
                    == want
            })
            .unwrap();
        assert!(
            ok,
            "{node:?} placed at {want:?}: {:?}, told {:?}",
            origin_of(mgr, node),
            mgr.state().host().placed
        );
    };
    // Top right of 1920 × 1080, below the 36 px bar: where sway draws it.
    wait_origin(&mut mgr, PANEL, vec![Some((1520, 36))]);
    pump(&mut mgr, Duration::from_millis(200));
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(
        shot.rgb(1525, 36 + 5),
        RED,
        "the panel's corner is drawn there"
    );
    assert_eq!(
        shot.rgb(1525, 5),
        BLUE,
        "the bar, not the panel, at the top"
    );
    // A second output, right of the first: its bar is at its position.
    let second = sway.create_output();
    wait_for_bars(&mut mgr, 2);
    let position = |mgr: &strand_surface::SurfaceManager<TestHost>| {
        mgr.state()
            .monitors()
            .into_iter()
            .find(|m| m.connector.as_deref() == Some(second.as_str()))
            .and_then(|m| m.position)
    };
    let _ = mgr.dispatch_until(WAIT, |s| {
        s.monitors()
            .iter()
            .any(|m| m.connector.as_deref() == Some(second.as_str()) && m.position.is_some())
    });
    let pos = position(&mgr)
        .unwrap_or_else(|| panic!("the second output's position: {:?}", mgr.state().monitors()));
    assert!(pos.0 >= 1920, "{pos:?}");
    let want = [Some((0, 0)), Some(pos)];
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let mut got: Vec<_> = s
                .surfaces_of(BAR)
                .iter()
                .map(|id| s.surface(*id).and_then(|i| i.origin))
                .collect();
            got.sort();
            got == want
        })
        .unwrap();
    assert!(
        ok,
        "the bars, in layout coordinates: {:?}, monitors {:?}",
        origin_of(&mgr, BAR),
        mgr.state().monitors()
    );
    // The live bar's zone drops to none (`height: 0`), and comes back:
    // the panel, which gets no configure of its own, follows each time.
    for (height, y) in [(0.0, 0), (36.0, 36)] {
        mgr.state_mut().apply_surface_change(
            BAR,
            SurfaceChange::Updated {
                spec: bar_spec("Top", height),
                recreate: false,
            },
        );
        wait_origin(&mut mgr, PANEL, vec![Some((1520, y))]);
    }
    // The bar gone: the panel moves up to the top, as sway arranges it.
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Removed);
    wait_origin(&mut mgr, PANEL, vec![Some((1520, 0))]);
}

/// (M5, S-platform) Sway 1.9 issues an xdg-activation token for a click
/// on a layer surface (the m5-infra spike): the token is asked with the
/// press's serial, which the host was told before the press.
#[test]
fn an_activation_token_comes_from_a_click_on_sway() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let Some(sway) = Sway::start("an_activation_token_comes_from_a_click_on_sway") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    assert!(
        mgr.state().activation_supported(),
        "sway offers xdg_activation_v1"
    );
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    let id = mgr.state().surfaces_of(BAR)[0];
    let mut pointer = VirtualPointer::new(&sway);
    pump(&mut mgr, Duration::from_millis(200));
    pointer.click(150, 15, 1920, 1080);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().input.iter().any(|e| {
                matches!(
                    e,
                    InputEvent::PointerButton {
                        state: ButtonState::Released,
                        ..
                    }
                )
            })
        })
        .unwrap();
    assert!(
        ok,
        "the click reached the bar: {:?}",
        mgr.state().host().input
    );
    let serial = *mgr
        .state()
        .host()
        .press_serials
        .last()
        .expect("the press's serial");
    assert_eq!(mgr.state().last_button_serial(), Some(serial));
    let got = Rc::new(RefCell::new(None));
    let put = got.clone();
    mgr.state_mut()
        .activation_token(id, serial, move |t| *put.borrow_mut() = Some(t));
    let ok = mgr
        .dispatch_until(WAIT, |_| got.borrow().is_some())
        .unwrap();
    assert!(ok, "sway answered the token request");
    let token = got.borrow_mut().take().flatten();
    assert!(token.as_deref().is_some_and(|t| !t.is_empty()), "{token:?}");
    assert_eq!(mgr.state().activation_tokens_issued(), 1);
}

/// (flex) The cursor follows what the host asks for on the surface the
/// pointer is on: set when it enters, set again when the host's answer
/// changes (after a poll: a scene diff changed a node's `cursor:` under
/// a still pointer, or after a motion onto another node), never twice
/// for the same shape, and forgotten when it leaves. Sway 1.9 has
/// `wp_cursor_shape_v1`, so a shape that sets is one the protocol took
/// (an unknown one would be a protocol error that ends the connection).
#[test]
fn the_cursor_follows_the_hosts_shape() {
    use strand_scene::CursorShape;
    let Some(sway) = Sway::start("the_cursor_follows_the_hosts_shape") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    let id = mgr.state().surfaces_of(BAR)[0];
    let mut pointer = VirtualPointer::new(&sway);
    pump(&mut mgr, Duration::from_millis(200));
    assert_eq!(mgr.state().cursor_shape(), None);
    // Onto the bar: the arrow.
    pointer.motion(100, 10, 1920, 1080);
    pump(&mut mgr, Duration::from_millis(200));
    assert_eq!(mgr.state().cursor_shape(), Some((id, CursorShape::Default)));
    let sets = mgr.state().stats().cursor_sets;
    assert!(sets >= 1, "{:?}", mgr.state().stats());
    // The host's answer changes under a still pointer: a poll shows it.
    mgr.state_mut().host_mut().cursor = Some(CursorShape::Grab);
    mgr.state_mut().poll();
    pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(mgr.state().cursor_shape(), Some((id, CursorShape::Grab)));
    assert_eq!(mgr.state().stats().cursor_sets, sets + 1);
    // Every shape is one the protocol takes (an unknown one would end
    // the connection with a protocol error).
    for shape in CursorShape::ALL {
        mgr.state_mut().host_mut().cursor = Some(*shape);
        mgr.state_mut().poll();
        pump(&mut mgr, Duration::from_millis(30));
        assert_eq!(mgr.state().cursor_shape(), Some((id, *shape)));
    }
    let sets = mgr.state().stats().cursor_sets;
    // The same shape is not set again however the pointer moves.
    for x in 0..5 {
        pointer.motion(120 + x * 10, 10, 1920, 1080);
    }
    pump(&mut mgr, Duration::from_millis(200));
    assert_eq!(mgr.state().stats().cursor_sets, sets);
    // Off the bar: no surface of ours, no shape.
    pointer.motion(900, 900, 1920, 1080);
    pump(&mut mgr, Duration::from_millis(200));
    assert_eq!(mgr.state().cursor_shape(), None);
    assert!(mgr.dispatch(Some(Duration::from_millis(10))).is_ok());
}

/// (flex) `exclusive:` as sway applies it: a bar keeps its thickness by
/// default, nothing with `false`, exactly a length with one, changed in
/// place; a panel anchored to one edge keeps its thickness with `true`
/// or a length, along that edge, and a panel at a corner keeps nothing.
#[test]
fn exclusive_keeps_that_much_from_windows() {
    use strand_scene::Exclusive;
    let Some((sway, mut mgr)) = start("exclusive_keeps_that_much_from_windows", Config::default())
    else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let usable = |mgr: &mut strand_surface::SurfaceManager<TestHost>| {
        pump(mgr, Duration::from_millis(150));
        sway.workspace_rect("HEADLESS-1")
    };
    assert_eq!(usable(&mut mgr), (0, 36, 1920, 1044));
    let bar = |e: Exclusive| {
        let mut spec = bar_spec("Top", 36.0);
        spec.exclusive = e;
        SurfaceChange::Updated {
            spec,
            recreate: false,
        }
    };
    mgr.state_mut()
        .apply_surface_change(BAR, bar(Exclusive::None));
    assert_eq!(usable(&mut mgr), (0, 0, 1920, 1080), "overlaps windows");
    mgr.state_mut()
        .apply_surface_change(BAR, bar(Exclusive::Length(10.0)));
    assert_eq!(usable(&mut mgr), (0, 10, 1920, 1070), "keeps exactly 10");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Removed);
    assert_eq!(usable(&mut mgr), (0, 0, 1920, 1080));
    // A panel on the right edge keeping 280 of its 300.
    const SIDE: NodeId = NodeId::new(5, 0);
    let mut side = layer_spec(NodeKind::Panel, "Side", "right", 300.0, 600.0);
    side.exclusive = Exclusive::Length(280.0);
    mgr.state_mut()
        .apply_surface_change(SIDE, SurfaceChange::Created(side.clone()));
    assert_eq!(usable(&mut mgr), (0, 0, 1640, 1080));
    // `true`: its width.
    side.exclusive = Exclusive::Auto;
    mgr.state_mut().apply_surface_change(
        SIDE,
        SurfaceChange::Updated {
            spec: side.clone(),
            recreate: false,
        },
    );
    assert_eq!(usable(&mut mgr), (0, 0, 1620, 1080));
    // At a corner: nothing (layer-shell keeps space along one edge).
    side.anchor = strand_scene::Anchor::TopRight;
    mgr.state_mut().apply_surface_change(
        SIDE,
        SurfaceChange::Updated {
            spec: side,
            recreate: false,
        },
    );
    assert_eq!(usable(&mut mgr), (0, 0, 1920, 1080));
}

/// (flex) A panel on every screen (`panel Side { screens: all }`) is one
/// panel node per monitor, each pinned by its monitor's identity as a
/// bar's instances are: each shows on its own output at its own size
/// (from `screen.height`), and keeping a length from windows there only.
#[test]
fn a_panel_per_screen_shows_on_its_own_output() {
    use strand_scene::Exclusive;
    let Some(sway) = Sway::start("a_panel_per_screen_shows_on_its_own_output") else {
        return;
    };
    sway.msg(&["output", "HEADLESS-1", "position", "0", "0"]);
    let second = sway.create_output();
    sway.msg(&["output", &second, "position", "1920", "0"]);
    sway.msg(&["output", &second, "resolution", "1280x720@60Hz"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let monitors = mgr.state().monitors();
    assert_eq!(monitors.len(), 2);
    let mut nodes = Vec::new();
    for (i, m) in monitors.iter().enumerate() {
        let node = NodeId::new(10 + i as u32, 0);
        let (_, h) = m.logical_size.unwrap();
        let mut spec = layer_spec(NodeKind::Panel, "Side", "left", 200.0, h as f32);
        spec.screens = strand_scene::Screens::Named(vec![m.id.as_str().to_string()]);
        spec.exclusive = Exclusive::Length(120.0);
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(spec));
        nodes.push((node, m.clone()));
    }
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            nodes.iter().all(|(n, _)| {
                s.surfaces_of(*n)
                    .first()
                    .and_then(|id| s.surface(*id))
                    .is_some_and(|i| i.configured)
            })
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    for (node, m) in &nodes {
        let ids = mgr.state().surfaces_of(*node);
        assert_eq!(ids.len(), 1, "one surface for {node:?}");
        let info = mgr.state().surface(ids[0]).unwrap();
        assert_eq!(info.monitor.as_ref(), Some(&m.id));
        assert_eq!(info.logical_size, (200, m.logical_size.unwrap().1 as u32));
    }
    pump(&mut mgr, Duration::from_millis(150));
    assert_eq!(sway.workspace_rect("HEADLESS-1"), (120, 0, 1800, 1080));
    assert_eq!(sway.workspace_rect(&second).0, 1920 + 120);
}

/// A virtual pointer on sway's seat (`zwlr_virtual_pointer_v1`).
struct VirtualPointer {
    queue: wayland_client::EventQueue<VpClient>,
    pointer: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    time: u32,
}

struct VpClient;

impl
    wayland_client::Dispatch<
        wayland_client::protocol::wl_registry::WlRegistry,
        wayland_client::globals::GlobalListContents,
    > for VpClient
{
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(VpClient: ignore wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(VpClient: ignore wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

impl VirtualPointer {
    fn new(sway: &Sway) -> Self {
        use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
        let conn = sway.connect();
        let (globals, mut queue) =
            wayland_client::globals::registry_queue_init::<VpClient>(&conn).unwrap();
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        queue.roundtrip(&mut VpClient).unwrap();
        VirtualPointer {
            queue,
            pointer,
            time: 0,
        }
    }

    /// The pointer to layout position (`x`, `y`) of a `w`×`h` layout.
    fn motion(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.time += 10;
        self.pointer.motion_absolute(self.time, x, y, w, h);
        self.pointer.frame();
        self.queue.roundtrip(&mut VpClient).unwrap();
    }

    /// A left click at layout position (`x`, `y`) of a `w`×`h` layout.
    fn click(&mut self, x: u32, y: u32, w: u32, h: u32) {
        use wayland_client::protocol::wl_pointer::ButtonState as B;
        self.time += 10;
        self.pointer.motion_absolute(self.time, x, y, w, h);
        self.pointer.frame();
        for state in [B::Pressed, B::Released] {
            self.time += 10;
            self.pointer.button(self.time, 0x110, state);
            self.pointer.frame();
        }
        self.queue.roundtrip(&mut VpClient).unwrap();
    }
}

/// (M6, decisions.md m6-w2-surface) A popup grabs by its cause: one the
/// press opened (`State::set_popup_cause`, from `SceneDiff::press`)
/// grabs with that press's serial even when the pointer moved well away
/// before it was created; one no press opened (a hover island under
/// the next bar item) takes no grab within `GRAB_WINDOW` of a click, so
/// it neither takes the keyboard nor latches the pointer. A popup in a
/// parent popup without a grab takes none either (xdg-shell allows a
/// grab only under a toplevel or a grabbing popup). A `trigger: hover`
/// popup never grabs, even marked with a fresh press, nor does a popup
/// opened from it.
#[test]
fn a_popup_grabs_only_for_the_press_that_opened_it() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_popup_grabs_only_for_the_press_that_opened_it") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const BAR: NodeId = NodeId::new(1, 0);
    const HOVERED: NodeId = NodeId::new(2, 0);
    const CLICKED: NodeId = NodeId::new(3, 0);
    const NESTED: NodeId = NodeId::new(4, 0);
    const TRIGGERED: NodeId = NodeId::new(5, 0);
    const IN_TRIGGERED: NodeId = NodeId::new(6, 0);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 1);

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 1920, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // A first click on the desktop wakes the new virtual pointer up.
    click(1, 1500, 800);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(100));
    let pressed = |mgr: &mut strand_surface::SurfaceManager<TestHost>, n: usize| {
        mgr.dispatch_until(WAIT, |s| {
            s.host()
                .input
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        InputEvent::PointerButton {
                            state: ButtonState::Pressed,
                            ..
                        }
                    )
                })
                .count()
                >= n
        })
        .unwrap()
    };
    let popup =
        |name: &str, x: f32| {
            let mut spec = strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| {
                None::<&strand_scene::PropValue>
            });
            spec.name = Some(name.into());
            spec.parent = Some(BAR);
            spec.anchor_rect = Some(LogicalRect::new(x, 8.0, 60.0, 20.0));
            spec.width = Some(200.0);
            spec.height = Some(120.0);
            spec.open_two_way = true;
            spec
        };
    let mapped = |mgr: &mut strand_surface::SurfaceManager<TestHost>, node: NodeId| {
        mgr.dispatch_until(WAIT, |s| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap()
    };

    // A click on one item, then the pointer goes on along the bar, more
    // than 8 px, to the next (well within the grab window).
    click(10, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    assert!(pressed(&mut mgr, 1), "the press reached the bar");
    let serial = *mgr.state().host().press_serials.last().unwrap();
    pointer.motion_absolute(20, 600, 18, 1920, 1080);
    pointer.frame();
    queue.roundtrip(&mut Client).unwrap();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().input.iter().any(
                |e| matches!(e, InputEvent::PointerMotion { position, .. } if position.x > 500.0),
            )
        })
        .unwrap();
    assert!(ok, "the motion reached the bar");
    // The island the hover opened there: no press opened it, no grab.
    mgr.state_mut().set_popup_cause(HOVERED, None);
    mgr.state_mut()
        .apply_surface_change(HOVERED, SurfaceChange::Created(popup("Hovered", 600.0)));
    assert!(mapped(&mut mgr, HOVERED));
    assert_eq!(
        mgr.state().stats().grabs,
        0,
        "a hover-opened popup never grabs"
    );
    let bar = mgr.state().surfaces_of(BAR)[0];
    assert!(!mgr.state().holds_keyboard_for_popup(bar));

    // A popup the click opened, created only now (its handler was slow,
    // the pointer long gone from where it pressed): it grabs.
    mgr.state_mut().set_popup_cause(CLICKED, Some(serial));
    mgr.state_mut()
        .apply_surface_change(CLICKED, SurfaceChange::Created(popup("Clicked", 300.0)));
    assert!(mapped(&mut mgr, CLICKED));
    assert_eq!(mgr.state().stats().grabs, 1, "the click's own menu grabs");
    assert!(mgr.state().holds_keyboard_for_popup(bar));

    // A popup nested in the hover island, opened by the same press: its
    // parent has no grab, so neither does it.
    let mut nested = popup("Nested", 10.0);
    nested.parent = Some(HOVERED);
    mgr.state_mut().set_popup_cause(NESTED, Some(serial));
    mgr.state_mut()
        .apply_surface_change(NESTED, SurfaceChange::Created(nested));
    assert!(mapped(&mut mgr, NESTED));
    assert_eq!(
        mgr.state().stats().grabs,
        1,
        "no grab under a popup without one"
    );

    // The popups closed, a fresh click, and a `trigger: hover` popup
    // marked with it: no grab; nor for a popup in it marked with the
    // same press.
    for node in [NESTED, CLICKED, HOVERED] {
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Removed);
    }
    pump(&mut mgr, Duration::from_millis(100));
    click(20, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    assert!(pressed(&mut mgr, 2), "the second press reached the bar");
    let serial = *mgr.state().host().press_serials.last().unwrap();
    let mut island = popup("Island", 130.0);
    island.hover_trigger = true;
    mgr.state_mut().set_popup_cause(TRIGGERED, Some(serial));
    mgr.state_mut()
        .apply_surface_change(TRIGGERED, SurfaceChange::Created(island));
    assert!(mapped(&mut mgr, TRIGGERED));
    let mut inner = popup("InIsland", 10.0);
    inner.parent = Some(TRIGGERED);
    mgr.state_mut().set_popup_cause(IN_TRIGGERED, Some(serial));
    mgr.state_mut()
        .apply_surface_change(IN_TRIGGERED, SurfaceChange::Created(inner));
    assert!(mapped(&mut mgr, IN_TRIGGERED));
    assert_eq!(
        mgr.state().stats().grabs,
        1,
        "a `trigger: hover` popup, and one in it, never grab"
    );
    drop(input);
    drop(pointer);
}

/// (grab-scope, owner-testing.md bug 21) A press grabs only for popups
/// on its own monitor. Everything a logic step does carries the press
/// that started it, `on change` handlers in every monitor's instance of
/// a bar included: a brightness slider dragged in one monitor's card
/// made each other monitor's bar open a popup marked with that press,
/// whose grab took the pointer from the slider and shut the card. Two
/// outputs, a bar on each; a click on the first's: a popup on the
/// second's bar marked with it takes no grab, one on the first's does.
#[test]
fn a_press_grabs_only_for_popups_on_its_monitor() {
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_pointer, wl_registry};
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

    let Some(sway) = Sway::start("a_press_grabs_only_for_popups_on_its_monitor") else {
        return;
    };
    sway.msg(&["output", "HEADLESS-1", "position", "0", "0"]);
    let second = sway.create_output();
    sway.msg(&["output", &second, "position", "1920", "0"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    const HERE: NodeId = NodeId::new(1, 0);
    const THERE: NodeId = NodeId::new(2, 0);
    const MENU: NodeId = NodeId::new(3, 0);
    const PEEK: NodeId = NodeId::new(4, 0);
    for (node, output) in [(HERE, "HEADLESS-1".to_string()), (THERE, second.clone())] {
        let mut spec = bar_spec("Top", 36.0);
        spec.screens = strand_scene::Screens::Named(vec![output]);
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(spec));
    }
    let input = mgr.take_input().unwrap();
    wait_for_bars(&mut mgr, 2);

    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
    let pointer = manager.create_virtual_pointer(None, &qh, ());
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    // The layout is both outputs side by side, 3840 × 1080.
    let click = |t: u32, x: u32, y: u32| {
        pointer.motion_absolute(t, x, y, 3840, 1080);
        pointer.frame();
        pointer.button(t + 1, 0x110, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(t + 2, 0x110, wl_pointer::ButtonState::Released);
        pointer.frame();
    };
    // A first click on the desktop wakes the new virtual pointer up.
    click(1, 1500, 800);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(100));
    click(10, 130, 18);
    queue.roundtrip(&mut Client).unwrap();
    let pressed = mgr
        .dispatch_until(WAIT, |s| {
            s.host().input.iter().any(|e| {
                matches!(
                    e,
                    InputEvent::PointerButton {
                        state: ButtonState::Pressed,
                        ..
                    }
                )
            })
        })
        .unwrap();
    assert!(pressed, "the press reached the first bar");
    let serial = *mgr.state().host().press_serials.last().unwrap();
    let popup =
        |parent: NodeId, name: &str| {
            let mut spec = strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| {
                None::<&strand_scene::PropValue>
            });
            spec.name = Some(name.into());
            spec.parent = Some(parent);
            spec.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
            spec.width = Some(200.0);
            spec.height = Some(120.0);
            spec.open_two_way = true;
            spec
        };
    let mapped = |mgr: &mut strand_surface::SurfaceManager<TestHost>, node: NodeId| {
        mgr.dispatch_until(WAIT, |s| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap()
    };

    // The other monitor's bar opens a popup in the same logic step.
    mgr.state_mut().set_popup_cause(PEEK, Some(serial));
    mgr.state_mut()
        .apply_surface_change(PEEK, SurfaceChange::Created(popup(THERE, "Peek")));
    assert!(mapped(&mut mgr, PEEK));
    assert_eq!(
        mgr.state().stats().grabs,
        0,
        "a popup on another monitor never grabs with this press"
    );
    let there = mgr.state().surfaces_of(THERE)[0];
    assert!(!mgr.state().holds_keyboard_for_popup(there));

    // The pressed bar's own menu grabs.
    mgr.state_mut().set_popup_cause(MENU, Some(serial));
    mgr.state_mut()
        .apply_surface_change(MENU, SurfaceChange::Created(popup(HERE, "Menu")));
    assert!(mapped(&mut mgr, MENU));
    assert_eq!(mgr.state().stats().grabs, 1, "the pressed bar's menu grabs");
    let here = mgr.state().surfaces_of(HERE)[0];
    assert!(mgr.state().holds_keyboard_for_popup(here));
    drop(input);
    drop(pointer);
}

/// (owner-testing.md bug 6) A popup wider than the room its anchor
/// leaves at an output's edge is moved inside the output by the manager
/// itself, so the compositor never has to slide it (Hyprland 0.56
/// slides a repositioned popup with a shadow overhang past the edge). On
/// outputs of different widths and places: HEADLESS-1 is 2560 logical
/// pixels wide at 1920 (3840×2160 at 1.5), HEADLESS-2 1920 wide at 0.
/// A 400-wide popup under the bar's last 20 pixels, then its first,
/// each grown while open (`xdg_popup.reposition`, or made again where
/// the compositor's `xdg_wm_base` is older than version 3), lands whole
/// inside its output, where its positioner asked (`Stats::popups_moved`).
#[test]
fn a_popup_at_an_outputs_edge_is_placed_inside_it() {
    let Some(sway) = Sway::start("a_popup_at_an_outputs_edge_is_placed_inside_it") else {
        return;
    };
    sway.msg(&[
        "output",
        "HEADLESS-1",
        "resolution",
        "3840x2160@60Hz",
        "position",
        "1920",
        "0",
        "scale",
        "1.5",
    ]);
    let second = sway.create_output();
    sway.msg(&["output", &second, "position", "0", "0"]);
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 2);
    // Both bars placed, on outputs whose place is known.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.monitors()
                .iter()
                .all(|m| m.position.is_some() && m.logical_size.is_some())
                && s.surfaces_of(BAR)
                    .iter()
                    .all(|b| s.host().placed.iter().any(|(id, _)| id == b))
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().monitors());
    // The popup nests in the bar's first surface.
    let parent = *mgr.state().surfaces_of(BAR).iter().min().unwrap();
    let monitor = mgr.state().surface(parent).unwrap().monitor.unwrap();
    let monitor = mgr
        .state()
        .monitors()
        .into_iter()
        .find(|m| m.id == monitor)
        .unwrap();
    let (mx, _) = monitor.position.unwrap();
    let (mw, _) = monitor.logical_size.unwrap();
    assert_eq!(
        (mx, mw),
        (1920, 2560),
        "the first bar is on HEADLESS-1: {monitor:?}"
    );

    let popup =
        |anchor_x: f32, width: f32| {
            let mut spec = strand_scene::SurfaceSpec::resolve(NodeKind::Popup, |_| {
                None::<&strand_scene::PropValue>
            });
            spec.name = Some("edge".into());
            spec.parent = Some(BAR);
            spec.anchor_rect = Some(LogicalRect::new(anchor_x, 8.0, 20.0, 20.0));
            spec.width = Some(width);
            spec.height = Some(120.0);
            spec
        };
    // Waits for `node`'s popup to be configured `width` wide and placed
    // since `since` placements; returns its box's left and right edges
    // in the layout.
    let placed = |mgr: &mut strand_surface::SurfaceManager<TestHost>,
                  node: NodeId,
                  width: u32,
                  since: usize| {
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.surfaces_of(node).first().is_some_and(|id| {
                    s.surface(*id)
                        .is_some_and(|i| i.configured && i.logical_size.0 == width)
                        && s.host().placed[since..].iter().any(|(p, _)| p == id)
                })
            })
            .unwrap();
        assert!(ok, "{node:?} configured {width} wide");
        let id = mgr.state().surfaces_of(node)[0];
        let (x, _) = mgr
            .state()
            .host()
            .placed
            .iter()
            .rev()
            .find(|(p, _)| *p == id)
            .unwrap()
            .1;
        (x, x + width as i32)
    };
    let cases = [
        ("the right end", NodeId::new(20, 0), (mw - 20) as f32),
        ("the left end", NodeId::new(21, 0), 0.0),
    ];
    for (what, node, at) in cases {
        let moved = mgr.state().stats().popups_moved;
        let since = mgr.state().host().placed.len();
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(popup(at, 400.0)));
        let (x0, x1) = placed(&mut mgr, node, 400, since);
        assert!(
            mx <= x0 && x1 <= mx + mw,
            "{what}: the popup's box {x0}..{x1} is not inside {mx}..{}",
            mx + mw
        );
        assert_eq!(
            mgr.state().stats().popups_moved,
            moved,
            "{what}: the compositor had to move it"
        );
        // Grown while open: still inside, where it asked.
        let since = mgr.state().host().placed.len();
        mgr.state_mut().apply_surface_change(
            node,
            SurfaceChange::Updated {
                spec: popup(at, 480.0),
                recreate: false,
            },
        );
        let (x0, x1) = placed(&mut mgr, node, 480, since);
        assert!(
            mx <= x0 && x1 <= mx + mw,
            "{what}, grown: the popup's box {x0}..{x1} is not inside {mx}..{}",
            mx + mw
        );
        assert_eq!(
            mgr.state().stats().popups_moved,
            moved,
            "{what}, grown: the compositor had to move it"
        );
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Removed);
        pump(&mut mgr, Duration::from_millis(50));
    }
}

/// (m6-owner-fixes, owner-testing.md gap 24) A popup's `margin` on the
/// side facing its anchor is the gap between them (spec.md 5.1): 6 px
/// only while `margin` is unset, `margin: 0` touches the anchor, and a
/// negative margin overlaps it. Under a 36 px top bar, as sway places
/// them: below 42, at 36, and 10 px over the bar at 26.
#[test]
fn a_popups_margin_on_its_anchors_side_is_its_gap() {
    use std::collections::HashMap;
    use strand_scene::{Prop, PropValue, SurfaceSpec};
    let Some((_sway, mut mgr)) = start(
        "a_popups_margin_on_its_anchors_side_is_its_gap",
        Config::default(),
    ) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let popup = |name: &str, x: f32, margin: Option<PropValue>| {
        let props: HashMap<Prop, PropValue> =
            margin.map(|m| (Prop::Margin, m)).into_iter().collect();
        let mut spec = SurfaceSpec::resolve(NodeKind::Popup, |p| props.get(&p));
        spec.name = Some(name.into());
        spec.parent = Some(BAR);
        spec.anchor_rect = Some(LogicalRect::new(x, 8.0, 60.0, 20.0));
        spec.width = Some(100.0);
        spec.height = Some(50.0);
        spec
    };
    // `margin: n, 0, 0`: only its top faces a top bar's items.
    let three = |n: f32| {
        PropValue::List(vec![
            PropValue::Number(n),
            PropValue::Number(0.0),
            PropValue::Number(0.0),
        ])
    };
    for (i, (name, x, margin, y)) in [
        ("Unset", 100.0, None, 42),
        ("Zero", 400.0, Some(PropValue::Number(0.0)), 36),
        ("Over", 700.0, Some(three(-10.0)), 26),
    ]
    .into_iter()
    .enumerate()
    {
        let node = NodeId::new(10 + i as u32, 0);
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(popup(name, x, margin)));
        let placed = |s: &strand_surface::State<TestHost>| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .filter(|i| i.stats.commits > 0)
                .and_then(|i| i.origin)
        };
        let ok = mgr.dispatch_until(WAIT, |s| placed(s).is_some()).unwrap();
        assert!(ok, "{name} maps: {:?}", mgr.state().surfaces());
        // Centred under its 60 px anchor: x + 30 - 50.
        assert_eq!(
            placed(mgr.state()),
            Some((x as i32 - 20, y)),
            "{name}: {:?}",
            mgr.state().host().placed
        );
    }
}

/// (flex) `anchor: pointer`: sway places the popup down and to the
/// right of the 1 × 1 anchor at the press (in the 36 px top bar, so the
/// popup's top is just under the press, not under the bar), slid back
/// inside the output where it would leave it, as any popup is.
#[test]
fn a_popup_at_the_pointer_opens_at_the_press() {
    use std::collections::HashMap;
    use strand_scene::{Prop, PropValue, SurfaceSpec};
    let Some((_sway, mut mgr)) = start(
        "a_popup_at_the_pointer_opens_at_the_press",
        Config::default(),
    ) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let popup = |name: &str, x: f32| {
        let props: HashMap<Prop, PropValue> =
            [(Prop::Anchor, PropValue::Keyword("pointer".into()))].into();
        let mut spec = SurfaceSpec::resolve(NodeKind::Popup, |p| props.get(&p));
        spec.name = Some(name.into());
        spec.parent = Some(BAR);
        spec.anchor_rect = Some(LogicalRect::new(x, 12.0, 1.0, 1.0));
        spec.width = Some(100.0);
        spec.height = Some(50.0);
        spec
    };
    // At 300: from 301, 13. At 1900, 100 px would leave the 1920 px
    // output: it ends at the output's edge.
    for (i, (name, x, want)) in [("Here", 300.0, (301, 13)), ("Inside", 1900.0, (1820, 13))]
        .into_iter()
        .enumerate()
    {
        let node = NodeId::new(10 + i as u32, 0);
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(popup(name, x)));
        let placed = |s: &strand_surface::State<TestHost>| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .filter(|i| i.stats.commits > 0)
                .and_then(|i| i.origin)
        };
        let ok = mgr.dispatch_until(WAIT, |s| placed(s).is_some()).unwrap();
        assert!(ok, "{name} maps: {:?}", mgr.state().surfaces());
        assert_eq!(
            placed(mgr.state()),
            Some(want),
            "{name}: {:?}",
            mgr.state().host().placed
        );
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Removed);
    }
}

/// (m6-owner-fixes, owner-testing.md gap 26) `attach:` keeps a set
/// margin on its side (spec.md 5.1): 0 while unset, so an attached
/// popup touches its bar and an attached panel is flush past the bar's
/// exclusive zone, and a negative one overlaps, so `margin: -36, 0, 0`
/// takes either over a 36 px top bar to the screen's edge. As sway
/// places them, with render's 14 px fillet overhang on both sides of
/// the box along the edge: the popups' boxes at y 36 and 0, the panels'
/// at 36 and 0 (before, `attach:` made both 36).
#[test]
fn an_attached_surface_keeps_its_margin_on_its_attached_side() {
    use std::collections::HashMap;
    use strand_scene::{Prop, PropValue, SurfaceSpec};
    let Some((_sway, mut mgr)) = start(
        "an_attached_surface_keeps_its_margin_on_its_attached_side",
        Config::default(),
    ) else {
        return;
    };
    wait_for_bars(&mut mgr, 1);
    let fillets = Insets {
        top: 0.0,
        right: 14.0,
        bottom: 0.0,
        left: 14.0,
    };
    let over = PropValue::List(vec![
        PropValue::Number(-36.0),
        PropValue::Number(0.0),
        PropValue::Number(0.0),
    ]);
    let props = |name: &str, margin: Option<&PropValue>| -> HashMap<Prop, PropValue> {
        [
            Some((Prop::Name, PropValue::Text(name.into()))),
            Some((Prop::Attach, PropValue::Keyword("top".into()))),
            margin.map(|m| (Prop::Margin, m.clone())),
        ]
        .into_iter()
        .flatten()
        .collect()
    };
    let popup = |name: &str, x: f32, margin: Option<&PropValue>| {
        let props = props(name, margin);
        let mut spec = SurfaceSpec::resolve(NodeKind::Popup, |p| props.get(&p));
        spec.parent = Some(BAR);
        spec.anchor_rect = Some(LogicalRect::new(x, 8.0, 60.0, 20.0));
        spec.width = Some(200.0);
        spec.height = Some(80.0);
        spec.overhang = fillets;
        spec
    };
    let panel = |name: &str, margin: Option<&PropValue>| {
        let mut props = props(name, margin);
        props.insert(Prop::Anchor, PropValue::Keyword("top".into()));
        props.insert(Prop::Layer, PropValue::Keyword("overlay".into()));
        props.insert(Prop::Width, PropValue::Number(200.0));
        props.insert(Prop::Height, PropValue::Number(80.0));
        let mut spec = SurfaceSpec::resolve(NodeKind::Panel, |p| props.get(&p));
        spec.overhang = fillets;
        spec
    };
    // Each box's top-left corner on the output: the buffer's origin
    // plus the overhang.
    for (i, (name, spec, x, y)) in [
        (
            "Touching",
            popup("Touching", 100.0, None),
            100 + 30 - 100,
            36,
        ),
        ("Over", popup("Over", 700.0, Some(&over)), 700 + 30 - 100, 0),
        ("Flush", panel("Flush", None), 960 - 100, 36),
        ("Covering", panel("Covering", Some(&over)), 960 - 100, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let node = NodeId::new(10 + i as u32, 0);
        mgr.state_mut()
            .apply_surface_change(node, SurfaceChange::Created(spec));
        let placed = |s: &strand_surface::State<TestHost>| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .filter(|i| i.stats.commits > 0)
                .and_then(|i| i.origin)
        };
        let ok = mgr.dispatch_until(WAIT, |s| placed(s).is_some()).unwrap();
        assert!(ok, "{name} maps: {:?}", mgr.state().surfaces());
        assert_eq!(
            placed(mgr.state()),
            Some((x - 14, y)),
            "{name}: {:?}",
            mgr.state().host().placed
        );
        if name == "Flush" {
            // Removed, so the next panel's place is not shared.
            mgr.state_mut()
                .apply_surface_change(node, SurfaceChange::Removed);
            pump(&mut mgr, Duration::from_millis(50));
        }
    }
}

/// (gap 31) A surface with keyboard focus reports its keyboard's lock
/// keys and the keymap's name for the active layout, exactly as the
/// keyboard's modifier state changes: a virtual keyboard whose keymap
/// names two layouts locks Caps Lock (`Lock`) and Num Lock (`Mod2`),
/// unlocks Caps Lock, switches to the second layout; the panel gone, the
/// report is `None`. This is a `keyboard: exclusive` panel, not the
/// session lock (whose code runs only in the lock VM).
#[test]
fn a_focused_surface_reports_its_lock_keys_and_layout() {
    use std::io::Write as _;
    use std::os::fd::AsFd;
    use strand_surface::KeyboardState;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{wl_registry, wl_seat};
    use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
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
    delegate_noop!(Client: ignore ZwpVirtualKeyboardManagerV1);
    delegate_noop!(Client: ignore ZwpVirtualKeyboardV1);
    delegate_noop!(Client: ignore wl_seat::WlSeat);

    let Some(sway) = Sway::start("a_focused_surface_reports_its_lock_keys_and_layout") else {
        return;
    };
    let mut mgr = strand_surface::SurfaceManager::with_connection(
        sway.connect(),
        TestHost::default(),
        Config::default(),
    )
    .unwrap();

    // The keyboard first, so the seat has one when the panel maps.
    let conn = sway.connect();
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).unwrap();
    let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let keymap = "xkb_keymap {\n\
        xkb_keycodes \"strand\" { minimum = 8; maximum = 255; <ESC> = 9; };\n\
        xkb_types \"strand\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
        xkb_compatibility \"strand\" { };\n\
        xkb_symbols \"strand\" {\n\
          name[Group1] = \"Strand One\";\n\
          name[Group2] = \"Strand Two\";\n\
          key <ESC> { symbols[Group1] = [ Escape ], symbols[Group2] = [ Escape ] };\n\
        };\n\
        };\n";
    let path = std::env::temp_dir().join(format!("strand-keymap-locks-{}", std::process::id()));
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(keymap.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    file.flush().unwrap();
    let file = std::fs::File::open(&path).unwrap();
    keyboard.keymap(1, file.as_fd(), keymap.len() as u32 + 1);
    queue.roundtrip(&mut Client).unwrap();
    pump(&mut mgr, Duration::from_millis(200));
    let _ = std::fs::remove_file(&path);
    assert!(
        mgr.state().host().keyboard_states.is_empty(),
        "no surface has the keyboard yet"
    );

    const PANEL: NodeId = NodeId::new(7, 0);
    let mut spec = layer_spec(NodeKind::Panel, "Launcher", "center", 200.0, 100.0);
    spec.keyboard = strand_scene::Keyboard::Exclusive;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let input = mgr.take_input().unwrap();
    let state = |caps_lock: bool, num_lock: bool, layout: &str| {
        Some(KeyboardState {
            caps_lock,
            num_lock,
            layout: Some(layout.to_string()),
        })
    };
    let wait_for = |mgr: &mut strand_surface::SurfaceManager<TestHost>,
                    want: Option<KeyboardState>,
                    what: &str| {
        let ok = mgr
            .dispatch_until(WAIT, |s| s.host().keyboard_states.last() == Some(&want))
            .unwrap();
        assert!(ok, "{what}: {:?}", mgr.state().host().keyboard_states);
    };
    wait_for(&mut mgr, state(false, false, "Strand One"), "focused");
    assert!(mgr.state().keyboard_focus().is_some());

    const LOCK: u32 = 1 << 1;
    const MOD2: u32 = 1 << 4;
    keyboard.modifiers(0, 0, LOCK | MOD2, 0);
    queue.roundtrip(&mut Client).unwrap();
    wait_for(&mut mgr, state(true, true, "Strand One"), "both locked");
    keyboard.modifiers(0, 0, MOD2, 0);
    queue.roundtrip(&mut Client).unwrap();
    wait_for(&mut mgr, state(false, true, "Strand One"), "caps unlocked");
    keyboard.modifiers(0, 0, MOD2, 1);
    queue.roundtrip(&mut Client).unwrap();
    wait_for(
        &mut mgr,
        state(false, true, "Strand Two"),
        "the second layout",
    );
    // Only changes are told.
    let states = mgr.state().host().keyboard_states.clone();
    assert!(
        states.windows(2).all(|w| w[0] != w[1]),
        "repeated: {states:?}"
    );

    // The panel goes: no surface has the keyboard.
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Removed);
    wait_for(&mut mgr, None, "unfocused");
    drop(input);
    drop(keyboard);
}
