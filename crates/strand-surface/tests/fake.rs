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
