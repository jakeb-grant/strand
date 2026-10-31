//! (M4) Surface hand-off against the fake compositor (docs/architecture.md,
//! "`strand-gpu`", "Surface hand-off"): while handed off the manager
//! neither paints nor commits the surface, a configure is still
//! reported, `take_back` paints a full frame, a lent surface takes no
//! buffer until it is handed off or taken back, and a surface destroyed
//! while handed off, or after its handles were lent, keeps its
//! `wl_surface` until `take_back`. On headless sway, a handed-off
//! surface's new size reaches the compositor with the next present.

#![cfg(feature = "gpu")]

mod common;

use std::time::Duration;

use common::{BAR, Sway, TestHost, WAIT, bar_spec, layer_spec, pump, wait_for_bars};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use strand_fake_wayland::{Cmd, Fake, SurfaceGlobals};
use strand_scene::{LogicalRect, NodeId, NodeKind, SurfaceChange};
use strand_surface::{Config, SurfaceManager};
use wayland_client::Connection;

const PANEL: NodeId = NodeId::new(7, 0);

fn manager(fake: &Fake) -> SurfaceManager<TestHost> {
    let conn = Connection::from_socket(fake.connect()).expect("a connection to the fake");
    SurfaceManager::with_connection(conn, TestHost::default(), Config::default())
        .expect("surface manager starts")
}

fn show_panel(fake: &Fake, mgr: &mut SurfaceManager<TestHost>) -> strand_scene::SurfaceId {
    let spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash")
                .first()
                .is_some_and(|s| s.buffer_commits > 0)
        })
        .unwrap();
    assert!(ok, "the panel never painted: {:?}", fake.surfaces());
    mgr.state().surfaces_of(PANEL)[0]
}

fn commits(fake: &Fake) -> usize {
    fake.surfaces()
        .iter()
        .filter(|s| s.namespace.as_deref() == Some("strand-Dash"))
        .map(|s| s.commits)
        .sum()
}

/// New content for the panel (its square moved), as a scene change.
fn change(mgr: &mut SurfaceManager<TestHost>, x: f32) {
    mgr.state_mut()
        .host_mut()
        .set_square(Some(LogicalRect::new(x, 10.0, 20.0, 20.0)));
    mgr.state_mut().poll();
}

#[test]
fn a_handed_off_surface_is_committed_by_nobody_else_until_taken_back() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let id = show_panel(&fake, &mut mgr);
    let handles = mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(matches!(handles.display, RawDisplayHandle::Wayland(_)));
    assert!(matches!(handles.window, RawWindowHandle::Wayland(_)));
    // Let the first frame settle (its callback, its feedback).
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    assert!(mgr.state_mut().hand_off(id));
    assert!(mgr.state().is_handed_off(id));
    let paints = mgr.state().host().paints.len();
    let before = commits(&fake);
    change(&mut mgr, 40.0);
    mgr.dispatch_until(Duration::from_millis(300), |_| false)
        .unwrap();
    assert_eq!(
        mgr.state().host().paints.len(),
        paints,
        "painted while handed off"
    );
    assert_eq!(commits(&fake), before, "committed while handed off");

    // Taken back: a full shm frame, then frame callbacks as before.
    mgr.state_mut().take_back(id);
    assert!(!mgr.state().is_handed_off(id));
    let ok = mgr
        .dispatch_until(WAIT, |s| s.host().paints.len() > paints)
        .unwrap();
    assert!(ok, "no frame after take_back");
    let p = &mgr.state().host().paints[paints];
    assert_eq!(p.age, 0, "the first frame after take_back starts afresh");
    let ok = mgr
        .dispatch_until(WAIT, |_| commits(&fake) > before)
        .unwrap();
    assert!(ok, "the frame was not committed");
    let n = mgr.state().host().paints.len();
    change(&mut mgr, 80.0);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.host().paints.len() > n)
        .unwrap();
    assert!(ok, "frames did not resume");
}

#[test]
fn a_surface_destroyed_while_handed_off_waits_for_take_back() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let id = show_panel(&fake, &mut mgr);
    assert!(mgr.state_mut().hand_off(id));
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Removed);
    assert_eq!(mgr.state().host().gpu_released, [id]);
    assert!(mgr.state().surfaces_of(PANEL).is_empty());
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    let alive = |fake: &Fake| {
        fake.surfaces()
            .iter()
            .any(|s| s.namespace.as_deref() == Some("strand-Dash") && !s.destroyed)
    };
    assert!(alive(&fake), "the wl_surface went before the swapchain");
    mgr.state_mut().take_back(id);
    let ok = mgr.dispatch_until(WAIT, |_| !alive(&fake)).unwrap();
    assert!(ok, "the wl_surface was not destroyed at take_back");
}

/// (gpu-pace) A presented surface's next frame waits for the frame
/// callback of its last present, so its FIFO wait never holds the one GPU
/// thread; given up on only where Mesa does not wait for it too; and a
/// surface that sends no frame asks for nothing.
#[test]
fn a_presented_frame_waits_for_its_callback_or_gives_up() {
    use wayland_client::Proxy;
    use wayland_client::backend::ObjectId;
    use wayland_client::protocol::wl_surface::WlSurface;

    let fake = Fake::compositor(SurfaceGlobals::default());
    let conn = Connection::from_socket(fake.connect()).expect("a connection to the fake");
    let mut mgr =
        SurfaceManager::with_connection(conn.clone(), TestHost::default(), Config::default())
            .expect("surface manager starts");
    let id = show_panel(&fake, &mut mgr);
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    // The hand-off comes with an shm frame's callback still to come.
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();
    let painted = mgr.state().surface(id).unwrap().stats.commits;
    change(&mut mgr, 40.0);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| i.stats.commits > painted)
        })
        .unwrap();
    assert!(ok, "the shm frame was not committed");
    let handles = mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(mgr.state_mut().hand_off(id));
    let RawWindowHandle::Wayland(w) = handles.window else {
        panic!("not a Wayland handle");
    };
    // SAFETY: the handle names the manager's live wl_surface on `conn`.
    let obj = unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }
        .expect("the wl_surface's id");
    let wsi = WlSurface::from_id(&conn, obj).expect("the wl_surface");

    assert!(mgr.state().present_due(id), "nothing sent yet");
    let idle = mgr.state().surface(id).unwrap().stats.frame_requests;
    mgr.dispatch_until(Duration::from_millis(100), |_| false)
        .unwrap();
    assert_eq!(
        mgr.state().surface(id).unwrap().stats.frame_requests,
        idle,
        "a surface that sends no frame asks for no callback"
    );

    // A frame goes: not due until its present has been answered. The
    // shm frame's callback coming now (late) does not answer it: it would
    // make every later present wait for its own callback in the WSI, on
    // the GPU thread, for good.
    mgr.state_mut().ask_present_callback(id);
    assert!(!mgr.state().present_due(id), "its callback has not come");
    let done = mgr.state().surface(id).unwrap().stats.frames_done;
    fake.cmd(Cmd::HoldFrames(false));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| i.stats.frames_done > done)
        })
        .unwrap();
    assert!(ok, "the shm frame's callback never came");
    assert!(
        !mgr.state().present_due(id),
        "the shm frame's callback was taken for the present's"
    );
    mgr.dispatch_until(Duration::from_millis(100), |_| false)
        .unwrap();
    assert!(
        !mgr.state().present_due(id),
        "no commit yet, so no callback: still waiting"
    );
    wsi.commit();
    conn.flush().unwrap();
    let ok = mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap();
    assert!(ok, "the present's callback never made it due");

    // A surface the compositor does not draw (an output powered off): its
    // next frame waits. Sent, Mesa's present would wait for this callback
    // with no timeout and stop every other surface.
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();
    mgr.state_mut().ask_present_callback(id);
    wsi.commit();
    conn.flush().unwrap();
    mgr.dispatch_until(Duration::from_millis(1200), |_| false)
        .unwrap();
    assert!(!mgr.state().present_due(id), "given up without a FIFO");
    fake.cmd(Cmd::HoldFrames(false));
    let ok = mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap();
    assert!(ok, "drawn again: the held callback came");

    // With `wp_fifo_manager_v1` Mesa does not wait for it, so it is given
    // up on, and the give-up's timer wakes the loop by itself.
    mgr.state_mut().set_wsi_fifo(true);
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();
    let asked = std::time::Instant::now();
    mgr.state_mut().ask_present_callback(id);
    wsi.commit();
    conn.flush().unwrap();
    mgr.dispatch_until(Duration::from_millis(300), |_| false)
        .unwrap();
    assert!(!mgr.state().present_due(id), "the callback is held");
    let ok = mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap();
    assert!(ok, "never given up on");
    assert!(
        asked.elapsed() >= Duration::from_millis(900),
        "given up early"
    );
    fake.cmd(Cmd::HoldFrames(false));
    mgr.state_mut().set_wsi_fifo(false);

    // A frame that failed: no present will answer it.
    mgr.state_mut().ask_present_callback(id);
    mgr.state_mut().forget_present_callback(id);
    assert!(mgr.state().present_due(id));

    // Taken back: the CPU's frames do not wait for a callback no frame
    // will answer.
    mgr.state_mut().ask_present_callback(id);
    mgr.state_mut().take_back(id);
    assert!(mgr.state().present_due(id));
}

/// (gpu-pace) Where a callback is given up on, a surface nobody draws must not
/// go on filling the compositor's queue at the first give-up's pace: each
/// present still unanswered doubles the next wait, the timer's too, and the
/// callbacks' coming starts it again.
#[test]
fn a_give_up_doubles_while_presents_stay_unanswered() {
    use wayland_client::Proxy;
    use wayland_client::backend::ObjectId;
    use wayland_client::protocol::wl_surface::WlSurface;

    let fake = Fake::compositor(SurfaceGlobals::default());
    let conn = Connection::from_socket(fake.connect()).expect("a connection to the fake");
    let mut mgr =
        SurfaceManager::with_connection(conn.clone(), TestHost::default(), Config::default())
            .expect("surface manager starts");
    let id = show_panel(&fake, &mut mgr);
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    let handles = mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(mgr.state_mut().hand_off(id));
    let RawWindowHandle::Wayland(w) = handles.window else {
        panic!("not a Wayland handle");
    };
    // SAFETY: the handle names the manager's live wl_surface on `conn`.
    let obj = unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }
        .expect("the wl_surface's id");
    let wsi = WlSurface::from_id(&conn, obj).expect("the wl_surface");
    mgr.state_mut().set_wsi_fifo(true);
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();

    let present = |mgr: &mut SurfaceManager<TestHost>| {
        mgr.state_mut().ask_present_callback(id);
        wsi.commit();
        conn.flush().unwrap();
        std::time::Instant::now()
    };
    // The first, at the first give-up's second; the wake is the timer's.
    let asked = present(&mut mgr);
    assert!(mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap());
    let first = asked.elapsed();
    assert!(first >= Duration::from_millis(950), "{first:?}");
    assert!(first < Duration::from_millis(1900), "{first:?}");
    // One unanswered: twice as long, and the timer wakes the loop then.
    let asked = present(&mut mgr);
    mgr.dispatch_until(Duration::from_millis(1400), |_| false)
        .unwrap();
    assert!(!mgr.state().present_due(id), "gave up at the first's pace");
    assert!(mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap());
    let second = asked.elapsed();
    assert!(second >= Duration::from_millis(1950), "{second:?}");
    assert!(second < Duration::from_millis(3900), "{second:?}");
    // Two unanswered: not at the second's pace either.
    present(&mut mgr);
    mgr.dispatch_until(Duration::from_millis(2400), |_| false)
        .unwrap();
    assert!(!mgr.state().present_due(id), "gave up at the second's pace");
    // Drawn again: every callback comes, and the next starts from the first.
    fake.cmd(Cmd::HoldFrames(false));
    assert!(mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap());
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();
    let asked = present(&mut mgr);
    assert!(mgr.dispatch_until(WAIT, |s| s.present_due(id)).unwrap());
    let again = asked.elapsed();
    assert!(again < Duration::from_millis(1900), "{again:?}");
}

fn alive(fake: &Fake) -> bool {
    fake.surfaces()
        .iter()
        .any(|s| s.namespace.as_deref() == Some("strand-Dash") && !s.destroyed)
}

/// Handles lent for an `Attach` keep the `wl_surface` alive before the
/// hand-off too: the GPU thread may be building a swapchain on it when
/// the surface is closed (its device takes 100 ms or more to start).
/// Until `take_back` a destroy goes through `gpu_release`, and the late
/// `Attached { Present }` hands nothing off. Once taken back (read back
/// after all), a destroy is immediate again.
#[test]
fn a_surface_destroyed_after_its_handles_were_lent_waits_for_take_back() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let id = show_panel(&fake, &mut mgr);
    mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(!mgr.state().is_handed_off(id));

    // Closed before `Attached` came back.
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Removed);
    assert_eq!(mgr.state().host().gpu_released, [id]);
    assert!(mgr.state().surfaces_of(PANEL).is_empty());
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    assert!(alive(&fake), "the wl_surface went while the GPU had it");
    assert!(
        !mgr.state_mut().hand_off(id),
        "a destroyed surface was handed off"
    );
    mgr.state_mut().take_back(id);
    let ok = mgr.dispatch_until(WAIT, |_| !alive(&fake)).unwrap();
    assert!(ok, "the wl_surface was not destroyed at take_back");

    // Lent and given back (attached for readback): destroyed at once.
    let id = show_panel(&fake, &mut mgr);
    mgr.state_mut().raw_handles(id).expect("raw handles");
    mgr.state_mut().take_back(id);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Removed);
    assert_eq!(
        mgr.state().host().gpu_released.len(),
        1,
        "no second release"
    );
    let ok = mgr.dispatch_until(WAIT, |_| !alive(&fake)).unwrap();
    assert!(ok, "a surface given back was kept");
}

fn buffer_commits(fake: &Fake) -> usize {
    fake.surfaces()
        .iter()
        .filter(|s| s.namespace.as_deref() == Some("strand-Dash"))
        .map(|s| s.buffer_commits)
        .sum()
}

/// (syncobj-lend) From the moment its handles are lent the manager
/// attaches no buffer to the surface: the GPU thread may be building a
/// swapchain on it, and a WSI with explicit sync then owns its buffers
/// (Hyprland ends the connection on an shm buffer with no acquire point,
/// owner-testing.md bug 22). A new frame waits; given back (attached for
/// readback), the surface paints it.
#[test]
fn a_lent_surface_takes_no_buffer_until_it_is_taken_back() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let id = show_panel(&fake, &mut mgr);
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    mgr.state_mut().raw_handles(id).expect("raw handles");
    let paints = mgr.state().host().paints.len();
    let before = buffer_commits(&fake);
    change(&mut mgr, 40.0);
    mgr.dispatch_until(Duration::from_millis(300), |_| false)
        .unwrap();
    assert_eq!(
        buffer_commits(&fake),
        before,
        "a buffer was committed to a lent surface"
    );
    assert_eq!(
        mgr.state().host().paints.len(),
        paints,
        "painted while lent"
    );

    // Given back: the frame it skipped is drawn and committed.
    mgr.state_mut().take_back(id);
    let ok = mgr
        .dispatch_until(WAIT, |_| buffer_commits(&fake) > before)
        .unwrap();
    assert!(ok, "the skipped frame was not drawn after take_back");
    assert!(mgr.state().host().paints.len() > paints);
}

/// A handed-off surface resized: the manager sets the new viewport
/// destination as pending state, and the GPU thread's next present (here
/// a bare commit on the handle it was given, as the WSI's would be)
/// shows the surface at its new size. At 1.5× the 36 px bar is 54 rows;
/// at 48 px it must cover 72, its old buffer stretched until the new
/// one comes.
#[test]
fn a_handed_off_surface_shows_its_new_size_at_the_next_present() {
    use raw_window_handle::RawWindowHandle;
    use wayland_client::Proxy;
    use wayland_client::backend::ObjectId;
    use wayland_client::protocol::wl_surface::WlSurface;

    let Some(sway) = Sway::start("a_handed_off_surface_shows_its_new_size_at_the_next_present")
    else {
        return;
    };
    sway.msg(&["output", "HEADLESS-1", "scale", "1.5"]);
    let conn = sway.connect();
    let mut mgr =
        SurfaceManager::with_connection(conn.clone(), TestHost::default(), Config::default())
            .expect("surface manager starts");
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(bar_spec("Top", 36.0)));
    wait_for_bars(&mut mgr, 1);
    pump(&mut mgr, Duration::from_millis(300));
    let info = mgr.state().surfaces()[0].clone();
    assert!(info.fractional, "sway offers fractional scale");
    let id = info.id;
    let handles = mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(mgr.state_mut().hand_off(id));
    let RawWindowHandle::Wayland(w) = handles.window else {
        panic!("not a Wayland handle");
    };
    // SAFETY: the handle names the manager's live wl_surface on `conn`.
    let obj = unsafe { ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr().cast()) }
        .expect("the wl_surface's id");
    let wsi = WlSurface::from_id(&conn, obj).expect("the wl_surface");
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(10, 53), common::BLUE);
    assert_ne!(shot.rgb(10, 69), common::BLUE);

    mgr.state_mut().apply_surface_change(
        BAR,
        SurfaceChange::Updated {
            spec: bar_spec("Top", 48.0),
            recreate: false,
        },
    );
    let end = std::time::Instant::now() + WAIT;
    loop {
        // A present: the layer size, the configure's ack and the new
        // geometry go out with it.
        wsi.commit();
        conn.flush().unwrap();
        pump(&mut mgr, Duration::from_millis(100));
        let resized = mgr.state().surfaces()[0].logical_size == (1280, 48);
        if resized && sway.grim("HEADLESS-1").rgb(10, 69) == common::BLUE {
            break;
        }
        assert!(
            std::time::Instant::now() < end,
            "the new size never showed: {:?}",
            mgr.state().surfaces()
        );
    }
    assert!(mgr.state().is_handed_off(id));
    let shot = sway.grim("HEADLESS-1");
    // Its old buffer stretched: the last row or so is filtered.
    assert_eq!(shot.rgb(10, 69), common::BLUE);
    assert_ne!(shot.rgb(10, 73), common::BLUE, "past the new size");
}

/// (m4-integration-w2) Handing a posed surface to the GPU thread drops
/// its compositor pose (render paints poses into the GPU's frames): the
/// surface goes back to identity as pending state, committed by the next
/// present.
#[test]
fn a_handed_off_surface_drops_its_compositor_pose() {
    use strand_scene::{LogicalPoint, SurfacePose};
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let posed = SurfacePose {
        opacity: 0.5,
        scale: 0.8,
        offset: LogicalPoint::new(40.0, 0.0),
    };
    mgr.state_mut().host_mut().poses = [posed].into_iter().collect();
    let id = show_panel(&fake, &mut mgr);
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surface(id).is_some_and(|i| i.pose == posed))
        .unwrap();
    assert!(ok, "the pose never reached the surface");
    assert!(mgr.state_mut().hand_off(id));
    assert_eq!(
        mgr.state().surface(id).map(|i| i.pose),
        Some(SurfacePose::IDENTITY),
        "the GPU's frames carry the pose, not the compositor"
    );
}

fn input_requests(fake: &Fake) -> Vec<Option<strand_fake_wayland::Region>> {
    fake.surfaces()
        .iter()
        .filter(|s| s.namespace.as_deref() == Some("strand-Dash"))
        .flat_map(|s| s.input_requests.clone())
        .collect()
}

/// (gpu-regions, owner-testing.md bug 23) A hover popup that grew on the
/// GPU takes the pointer where it now draws: the input region its frame
/// asks for is set as pending state for the present, without a commit
/// of the manager's. Without it the region of the last shm frame stood,
/// and the island lost hover when the pointer left its old size.
#[test]
fn a_handed_off_surface_takes_input_where_its_gpu_frame_drew() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    mgr.state_mut().host_mut().input_rects = Some(vec![LogicalRect::new(10.0, 10.0, 40.0, 20.0)]);
    let id = show_panel(&fake, &mut mgr);
    mgr.state_mut().raw_handles(id).expect("raw handles");
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    assert!(mgr.state_mut().hand_off(id));
    let sent = input_requests(&fake).len();
    let before = commits(&fake);

    // The GPU frame drew it wider.
    let grown = LogicalRect::new(10.0, 10.0, 300.0, 200.0);
    mgr.state_mut().host_mut().input_rects = Some(vec![grown]);
    mgr.state_mut().sync_presented(id);
    mgr.dispatch_until(Duration::from_millis(100), |_| false)
        .unwrap();
    let requests = input_requests(&fake);
    assert_eq!(requests.len(), sent + 1, "the new input region was not set");
    let region = requests[sent]
        .clone()
        .expect("a region, not the whole surface");
    assert!(
        region.contains(250, 150),
        "the region is not the grown one: {region:?}"
    );
    assert_eq!(
        commits(&fake),
        before,
        "the manager committed a handed-off surface"
    );

    // The same frame again sets nothing.
    mgr.state_mut().sync_presented(id);
    mgr.dispatch_until(Duration::from_millis(100), |_| false)
        .unwrap();
    assert_eq!(
        input_requests(&fake).len(),
        sent + 1,
        "an unchanged region was sent again"
    );
}

/// (gpu-pace) An animating CPU surface whose callback was given up on and
/// which is then handed off: the old callback, when it comes, must not pass
/// for the present's, or the next frame goes while that present's own callback
/// is owed, into Mesa's wait.
#[test]
fn a_given_up_shm_callback_does_not_answer_the_present() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let id = show_panel(&fake, &mut mgr);
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    mgr.state_mut().host_mut().animate = 1000;
    change(&mut mgr, 20.0);
    fake.cmd(Cmd::HoldFrames(true));
    fake.sync();
    // Two frames with callbacks asked, the first given up on after 1 s.
    let commits = mgr.state().surface(id).unwrap().stats.commits;
    mgr.dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    change(&mut mgr, 30.0);
    mgr.dispatch_until(Duration::from_millis(1800), |s| {
        s.surface(id).unwrap().stats.commits >= commits + 2
    })
    .unwrap();
    assert!(
        mgr.state().surface(id).unwrap().stats.throttle_given_up >= 1,
        "the give-up did not run"
    );

    mgr.state_mut().raw_handles(id).expect("raw handles");
    assert!(mgr.state_mut().hand_off(id));
    mgr.state_mut().host_mut().animate = 0;
    mgr.state_mut().ask_present_callback(id);
    assert!(!mgr.state().present_due(id));
    // The output is back: the old callbacks come, and no present committed
    // the one just asked for.
    fake.cmd(Cmd::HoldFrames(false));
    mgr.dispatch_until(Duration::from_millis(500), |_| false)
        .unwrap();
    assert!(
        !mgr.state().present_due(id),
        "an shm callback passed for the present's, with no present committed"
    );
}
