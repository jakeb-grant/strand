//! (M4) Surface hand-off against the fake compositor (docs/architecture.md,
//! "`strand-gpu`", "Surface hand-off"): while handed off the manager
//! neither paints nor commits the surface, a configure is still
//! reported, `take_back` paints a full frame, and a surface destroyed
//! while handed off keeps its `wl_surface` until `take_back`. On headless sway, a handed-off
//! surface's new size reaches the compositor with the next present.

#![cfg(feature = "gpu")]

mod common;

use std::time::Duration;

use common::{BAR, Sway, TestHost, WAIT, bar_spec, layer_spec, pump, wait_for_bars};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use strand_fake_wayland::{Fake, SurfaceGlobals};
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
