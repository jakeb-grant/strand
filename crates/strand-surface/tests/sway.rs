//! Integration tests against a private headless sway (skipped with a
//! message when sway is not installed). See `CLAUDE.md`, "Headless
//! Wayland for tests".

mod common;

use std::time::{Duration, Instant};

use common::*;
use strand_scene::{
    Damage, Insets, LogicalRect, LogicalSize, NodeId, NodeKind, Scale, Size, SurfaceChange,
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
