//! The surface manager against the fake compositor (strand-fake-wayland),
//! for the protocols sway 1.9 in CI lacks (the alpha modifier,
//! `ext-background-effect-v1`) and for capability combinations no one
//! compositor has. Real compositors are covered by tests/sway.rs and the
//! compositor matrix (crates/strand/tests/compositor_matrix.rs).

mod common;

use std::time::Duration;

use common::{TestHost, WAIT, layer_spec};
use strand_fake_wayland::{Cmd, Fake, SurfaceGlobals};
use strand_scene::{CompositorCaps, NodeId, NodeKind, SurfaceChange};
use strand_surface::{Config, SurfaceManager};
use wayland_client::Connection;

const PANEL: NodeId = NodeId::new(7, 0);

/// A surface manager connected to `fake`.
fn manager(fake: &Fake) -> SurfaceManager<TestHost> {
    let conn = Connection::from_socket(fake.connect()).expect("a connection to the fake");
    SurfaceManager::with_connection(conn, TestHost::default(), Config::default())
        .expect("surface manager starts")
}

/// Shows `panel Dash` (400×300, top right) and waits for its first frame.
fn show_panel(fake: &Fake, mgr: &mut SurfaceManager<TestHost>) {
    let spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surfaces().iter().any(|i| i.stats.commits > 0))
        .unwrap();
    assert!(ok, "the panel never painted: {:?}", mgr.state().surfaces());
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash")
                .first()
                .is_some_and(|s| s.buffer_commits > 0)
        })
        .unwrap();
    assert!(
        ok,
        "the fake never saw the panel's buffer: {:?}",
        fake.surfaces()
    );
}

/// The manager binds the M4 globals and tells the host what the
/// compositor offers once, before any surface is configured; the
/// background effect counts only with its blur capability, and a change
/// of that capability is reported again.
#[test]
fn capabilities_are_reported_once_the_globals_are_bound() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let ok = mgr
        .dispatch_until(WAIT, |s| !s.host().caps.is_empty())
        .unwrap();
    assert!(ok, "no capabilities reported");
    // The blur capability may come in the wakeup after the first report.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().caps.last().is_some_and(|c| c.background_effect)
        })
        .unwrap();
    assert!(ok, "blur never reported: {:?}", mgr.state().host().caps);
    let want = CompositorCaps {
        alpha_modifier: true,
        viewporter: true,
        single_pixel_buffer: true,
        background_effect: true,
        session_lock: false,
        data_device: false,
    };
    assert_eq!(mgr.state().host().caps.last(), Some(&want));
    assert_eq!(mgr.state().compositor_caps(), want);
    assert!(want.delegates_poses());
    // Reported only when they change.
    let n = mgr.state().host().caps.len();
    show_panel(&fake, &mut mgr);
    assert_eq!(mgr.state().host().caps.len(), n, "no change, no report");
    // The compositor stops blurring (a setting turned off): reported.
    fake.cmd(Cmd::SetEffectCapabilities(0));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().caps.last().is_some_and(|c| !c.background_effect)
        })
        .unwrap();
    assert!(ok, "the lost capability was not reported");
    assert!(!mgr.state().compositor_caps().background_effect);
    assert_eq!(mgr.state().host().caps.len(), n + 1);
}

/// A compositor with none of the optional protocols reports none, and a
/// background effect manager that cannot blur does not count.
#[test]
fn a_bare_compositor_reports_no_capabilities() {
    let bare = SurfaceGlobals {
        viewporter: false,
        single_pixel_buffer: false,
        alpha_modifier: false,
        background_effect: None,
        ..SurfaceGlobals::default()
    };
    let fake = Fake::compositor(bare);
    let mut mgr = manager(&fake);
    let ok = mgr
        .dispatch_until(WAIT, |s| !s.host().caps.is_empty())
        .unwrap();
    assert!(ok, "no capabilities reported");
    assert_eq!(mgr.state().host().caps, [CompositorCaps::default()]);
    // A panel still maps, on shm alone.
    show_panel(&fake, &mut mgr);

    let no_blur = SurfaceGlobals {
        background_effect: Some(0),
        ..SurfaceGlobals::default()
    };
    let fake = Fake::compositor(no_blur);
    let mut mgr = manager(&fake);
    let ok = mgr
        .dispatch_until(WAIT, |s| !s.host().caps.is_empty())
        .unwrap();
    assert!(ok, "no capabilities reported");
    common::pump(&mut mgr, Duration::from_millis(100));
    let caps = mgr.state().host().caps.clone();
    assert!(
        caps.iter()
            .all(|c| !c.background_effect && c.alpha_modifier),
        "{caps:?}"
    );
}

/// Paints the panel once more (new content) and waits for its commit.
fn repaint(mgr: &mut SurfaceManager<TestHost>) {
    let id = mgr.state().surfaces_of(PANEL)[0];
    let before = mgr.state().surface(id).unwrap().stats.commits;
    // A square moved: new content to paint.
    let x = before as f32 * 3.0 % 300.0;
    mgr.state_mut()
        .host_mut()
        .set_square(Some(strand_scene::LogicalRect::new(x, 10.0, 20.0, 20.0)));
    mgr.state_mut().poll();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| i.stats.commits > before)
        })
        .unwrap();
    assert!(ok, "the panel did not paint again");
}

fn rounded(w: u32, h: u32, r: f32) -> strand_scene::BlurRegion {
    strand_scene::BlurRegion {
        rect: strand_scene::Rect::new(0, 0, w, h),
        radii: [r; 4],
        radius: 24.0,
    }
}

/// Waits until the fake's panel has had `n` blur regions committed.
fn wait_blur_sets(fake: &Fake, mgr: &mut SurfaceManager<TestHost>, n: usize) {
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash")
                .first()
                .is_some_and(|s| s.blur_sets.len() >= n)
        })
        .unwrap();
    assert!(
        ok,
        "expected {n} blur regions: {:?}",
        fake.layer("strand-Dash")
    );
}

/// The blur ladder's first rung: a node with `blur` asks the compositor
/// to blur behind its rounded box. The region goes with the frame's
/// commit, follows the rounded corners (the corner pixel is out, the
/// edges' middles are in), is sent again only when the shape changes
/// (frames with the same shape send nothing), and is set to null once
/// nothing asks for blur.
#[test]
fn the_blur_region_follows_the_rounded_shape_and_changes_only_with_it() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    mgr.state_mut().host_mut().blur = vec![rounded(400, 300, 16.0)];
    show_panel(&fake, &mut mgr);
    wait_blur_sets(&fake, &mut mgr, 1);
    let rec = &fake.layer("strand-Dash")[0];
    let region = rec.blur.clone().expect("a blur region");
    assert!(region.contains(200, 150) && region.contains(200, 0) && region.contains(0, 150));
    assert!(!region.contains(0, 0) && !region.contains(399, 299) && !region.contains(2, 2));
    let full = 400 * 300;
    let corners = 4.0 * (16.0f64 * 16.0) * (1.0 - std::f64::consts::PI / 4.0);
    let missing = full as f64 - region.area() as f64;
    assert!(
        missing >= corners && missing < corners + 4.0 * 2.0 * 16.0,
        "the region leaves out about the corners: {missing} px (the corners are {corners})"
    );
    // The commit that carried it attached a buffer (it rode the frame).
    assert!(rec.buffer_commits >= 1);
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert!(info.blur_region.as_ref().is_some_and(|r| !r.is_empty()));
    // Frames with the same shape send nothing.
    for _ in 0..3 {
        repaint(&mut mgr);
    }
    common::pump(&mut mgr, Duration::from_millis(50));
    assert_eq!(fake.layer("strand-Dash")[0].blur_sets.len(), 1);
    assert_eq!(mgr.state().surface(id).unwrap().stats.blur_updates, 1);
    // A new radius is a new shape.
    mgr.state_mut().host_mut().blur = vec![rounded(400, 300, 4.0)];
    repaint(&mut mgr);
    wait_blur_sets(&fake, &mut mgr, 2);
    let region = fake.layer("strand-Dash")[0].blur.clone().unwrap();
    assert!(region.contains(2, 2) && !region.contains(0, 0));
    // Nothing asks for blur: null.
    mgr.state_mut().host_mut().blur.clear();
    repaint(&mut mgr);
    wait_blur_sets(&fake, &mut mgr, 3);
    let rec = &fake.layer("strand-Dash")[0];
    assert_eq!(rec.blur_sets.last(), Some(&None), "a null region");
    assert!(rec.blur.is_none());
    repaint(&mut mgr);
    common::pump(&mut mgr, Duration::from_millis(50));
    assert_eq!(fake.layer("strand-Dash")[0].blur_sets.len(), 3);
}

/// Without the blur capability nothing is sent (render draws the tint);
/// when the compositor starts blurring, the region of the last frame is
/// sent at once, with a commit of its own.
#[test]
fn no_blur_region_until_the_compositor_blurs() {
    let fake = Fake::compositor(SurfaceGlobals {
        background_effect: Some(0),
        ..SurfaceGlobals::default()
    });
    let mut mgr = manager(&fake);
    mgr.state_mut().host_mut().blur = vec![rounded(400, 300, 12.0)];
    show_panel(&fake, &mut mgr);
    repaint(&mut mgr);
    common::pump(&mut mgr, Duration::from_millis(50));
    assert!(fake.layer("strand-Dash")[0].blur_sets.is_empty());
    fake.cmd(Cmd::SetEffectCapabilities(1));
    wait_blur_sets(&fake, &mut mgr, 1);
    assert!(fake.layer("strand-Dash")[0].blur.is_some());
    // A surface whose frames ask for no blur never sends a region.
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    show_panel(&fake, &mut mgr);
    repaint(&mut mgr);
    common::pump(&mut mgr, Duration::from_millis(50));
    let rec = &fake.layer("strand-Dash")[0];
    assert!(rec.blur_sets.is_empty(), "{rec:?}");
    assert_eq!(mgr.state().stats().blur_updates, 0);
}
