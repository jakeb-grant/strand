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

/// `panel Dash` with `scrim:` in `color` (and, with `clicks`, `keyboard:
/// exclusive` and a two-way `open`, as the launcher).
fn scrim_spec(color: Option<strand_scene::Color>, clicks: bool) -> strand_scene::SurfaceSpec {
    let mut spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    spec.scrim = color;
    if clicks {
        spec.keyboard = strand_scene::Keyboard::Exclusive;
        spec.open_two_way = true;
    }
    spec
}

const DIM: strand_scene::Color = strand_scene::Color::new(0.0, 0.0, 0.0, 0.3);

/// Waits until the fake has a live layer surface `ns` matching `want`.
fn wait_layer(
    fake: &Fake,
    mgr: &mut SurfaceManager<TestHost>,
    ns: &str,
    want: impl Fn(&strand_fake_wayland::SurfaceRecord) -> bool,
) -> strand_fake_wayland::SurfaceRecord {
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer(ns).iter().any(|s| !s.destroyed && want(s))
        })
        .unwrap();
    assert!(ok, "{ns}: {:?}", fake.surfaces());
    fake.layer(ns)
        .into_iter()
        .find(|s| !s.destroyed && want(s))
        .unwrap()
}

/// `scrim:` on a panel is one single-pixel buffer the viewporter scales
/// over the usable area, on its own layer surface with an empty input
/// region (clicks pass through a scrim alone), on the layer below the
/// panel's: a `top` panel rises to `overlay`, its scrim on `top` (two
/// layer surfaces on one layer stack in an order the protocol leaves
/// open). A new colour is a new pixel on the same surface (the panel
/// stays), and taking the scrim away destroys it.
#[test]
fn a_scrim_is_one_single_pixel_under_the_panel() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(scrim_spec(Some(DIM), false)));
    let rec = wait_layer(&fake, &mut mgr, "strand-Dash-scrim", |s| s.buffer.is_some());
    let a = (0.3 * f64::from(u32::MAX)).round() as u32;
    let Some(strand_fake_wayland::BufferKind::SinglePixel([0, 0, 0, got])) = rec.buffer else {
        panic!("not a black single pixel: {rec:?}");
    };
    assert!(got.abs_diff(a) < 300, "{got} vs {a}");
    assert_eq!(rec.viewport, Some((1920, 1080)));
    assert!(
        rec.input.as_ref().is_some_and(|r| r.area() == 0),
        "clicks pass through: {rec:?}"
    );
    // zwlr_layer_shell_v1: top 2, overlay 3.
    assert_eq!(rec.layer, Some(2), "the scrim on top");
    assert_eq!(
        fake.layer("strand-Dash")[0].layer,
        Some(3),
        "the panel above it"
    );
    assert!(fake.layer("strand-Dash-click-away").is_empty());
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.scrim, Some(DIM));
    assert!(!info.click_away);

    // A new colour: the same surfaces, a new pixel.
    let red = strand_scene::Color::new(1.0, 0.0, 0.0, 0.5);
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: scrim_spec(Some(red), false),
            recreate: false,
        },
    );
    let rec = wait_layer(
        &fake,
        &mut mgr,
        "strand-Dash-scrim",
        |s| matches!(s.buffer, Some(strand_fake_wayland::BufferKind::SinglePixel([r, 0, 0, _])) if r > 0),
    );
    let Some(strand_fake_wayland::BufferKind::SinglePixel([r, _, _, a])) = rec.buffer else {
        unreachable!()
    };
    assert!(r.abs_diff(a) <= 1, "premultiplied: {r} vs {a}");
    assert_eq!(
        fake.layer("strand-Dash-scrim").len(),
        1,
        "recoloured in place"
    );
    assert_eq!(mgr.state().surfaces_of(PANEL), [id], "the panel stays");
    assert_eq!(mgr.state().surface(id).unwrap().scrim, Some(red));

    // No scrim: it goes, and the panel is back on `top`.
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: scrim_spec(None, false),
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash-scrim").is_empty()
                && fake
                    .layer("strand-Dash")
                    .iter()
                    .any(|s| s.layer == Some(2) && s.buffer.is_some())
        })
        .unwrap();
    assert!(ok, "{:?}", fake.surfaces());
    let id = mgr.state().surfaces_of(PANEL)[0];
    assert_eq!(mgr.state().surface(id).unwrap().scrim, None);
}

/// Without single-pixel buffers the scrim is one shm pixel the viewporter
/// scales; without the viewporter too, a buffer as large as the area.
#[test]
fn a_scrim_falls_back_to_shm() {
    let fake = Fake::compositor(SurfaceGlobals {
        single_pixel_buffer: false,
        ..SurfaceGlobals::default()
    });
    let mut mgr = manager(&fake);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(scrim_spec(Some(DIM), false)));
    let rec = wait_layer(&fake, &mut mgr, "strand-Dash-scrim", |s| s.buffer.is_some());
    assert_eq!(
        rec.buffer,
        Some(strand_fake_wayland::BufferKind::Shm {
            width: 1,
            height: 1
        })
    );
    assert_eq!(rec.viewport, Some((1920, 1080)));

    let fake = Fake::compositor(SurfaceGlobals {
        single_pixel_buffer: false,
        viewporter: false,
        ..SurfaceGlobals::default()
    });
    let mut mgr = manager(&fake);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(scrim_spec(Some(DIM), false)));
    let rec = wait_layer(&fake, &mut mgr, "strand-Dash-scrim", |s| s.buffer.is_some());
    assert_eq!(
        rec.buffer,
        Some(strand_fake_wayland::BufferKind::Shm {
            width: 1920,
            height: 1080
        })
    );
}

/// A panel with a scrim and click-away gets one surface for both: the
/// click-away catcher, coloured, with its hole for the panel, on the
/// layer below it.
#[test]
fn the_scrim_is_the_click_away_catcher() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(scrim_spec(Some(DIM), true)));
    let rec = wait_layer(&fake, &mut mgr, "strand-Dash-click-away", |s| {
        s.buffer.is_some() && s.input.is_some()
    });
    assert!(
        matches!(
            rec.buffer,
            Some(strand_fake_wayland::BufferKind::SinglePixel([0, 0, 0, a])) if a > 0
        ),
        "{rec:?}"
    );
    let input = rec.input.unwrap();
    assert!(
        input.contains(10, 10) && !input.contains(1900, 100),
        "a hole for the panel"
    );
    assert_eq!(rec.layer, Some(2));
    assert!(
        fake.layer("strand-Dash-scrim").is_empty(),
        "one surface, not two"
    );
    let id = mgr.state().surfaces_of(PANEL)[0];
    let ok = mgr
        .dispatch_until(WAIT, |s| s.surface(id).is_some_and(|i| i.click_away))
        .unwrap();
    assert!(ok);
    assert_eq!(mgr.state().surface(id).unwrap().scrim, Some(DIM));
}

/// A click-away panel the user put on `overlay` keeps its layer when it
/// gains a scrim, so its catcher moves: from the panel's layer (a
/// transparent catcher) to the one below (the scrim), and back when the
/// scrim goes. The panel itself is never made again. On sway 1.9 a
/// coloured catcher left on the panel's own layer would dim the panel.
#[test]
fn a_scrim_toggled_on_an_overlay_panel_moves_its_catcher() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let spec = |color| {
        let mut s = scrim_spec(color, true);
        s.layer = Some(strand_scene::Layer::Overlay);
        s
    };
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec(None)));
    // zwlr_layer_shell_v1: bottom 1, top 2, overlay 3.
    let rec = wait_layer(&fake, &mut mgr, "strand-Dash-click-away", |s| {
        s.configured.is_some()
    });
    assert_eq!(rec.layer, Some(3), "transparent, on the panel's layer");
    let panel = wait_layer(&fake, &mut mgr, "strand-Dash", |s| s.buffer.is_some());
    assert_eq!(panel.layer, Some(3));
    let id = mgr.state().surfaces_of(PANEL)[0];
    let panels = fake
        .surfaces()
        .iter()
        .filter(|s| s.namespace.as_deref() == Some("strand-Dash"))
        .count();

    // A scrim: the catcher goes one layer down, coloured.
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: spec(Some(DIM)),
            recreate: false,
        },
    );
    let rec = wait_layer(
        &fake,
        &mut mgr,
        "strand-Dash-click-away",
        |s| matches!(s.buffer, Some(strand_fake_wayland::BufferKind::SinglePixel([0, 0, 0, a])) if a > 0),
    );
    assert_eq!(rec.layer, Some(2), "the scrim below the panel");
    assert_eq!(
        fake.layer("strand-Dash-click-away").len(),
        1,
        "{:?}",
        fake.surfaces()
    );
    assert_eq!(
        fake.layer("strand-Dash")[0].layer,
        Some(3),
        "the panel stays"
    );
    assert_eq!(mgr.state().surfaces_of(PANEL), [id]);

    // No scrim: back on the panel's layer, transparent.
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec: spec(None),
            recreate: false,
        },
    );
    wait_layer(&fake, &mut mgr, "strand-Dash-click-away", |s| {
        s.layer == Some(3)
            && matches!(
                s.buffer,
                Some(strand_fake_wayland::BufferKind::SinglePixel([_, _, _, 0]))
            )
    });
    assert_eq!(
        fake.layer("strand-Dash-click-away").len(),
        1,
        "{:?}",
        fake.surfaces()
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.click_away && i.scrim.is_none())
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    assert_eq!(
        fake.surfaces()
            .iter()
            .filter(|s| s.namespace.as_deref() == Some("strand-Dash"))
            .count(),
        panels,
        "the panel was never made again"
    );
}
