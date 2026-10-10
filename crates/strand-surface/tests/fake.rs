//! The surface manager against the fake compositor (strand-fake-wayland),
//! for the protocols sway 1.9 in CI lacks (the alpha modifier,
//! `ext-background-effect-v1`) and for capability combinations no one
//! compositor has. Real compositors are covered by tests/sway.rs and the
//! compositor matrix (crates/strand/tests/compositor_matrix.rs).

mod common;

use std::time::{Duration, Instant};

use common::{TestHost, WAIT, layer_spec};
use strand_fake_wayland::{Cmd, Fake, SurfaceGlobals};
use strand_scene::{CompositorCaps, NodeId, NodeKind, SurfaceChange, SurfaceId};
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
        hyprland: false,
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

/// The pose `k` of `n` on its way from `from` to rest.
fn pose_at(from: strand_scene::SurfacePose, k: u32, n: u32) -> strand_scene::SurfacePose {
    let f = 1.0 - k as f32 / n as f32;
    strand_scene::SurfacePose {
        opacity: 1.0 + (from.opacity - 1.0) * f,
        scale: 1.0 + (from.scale - 1.0) * f,
        offset: strand_scene::LogicalPoint::new(from.offset.x * f, from.offset.y * f),
    }
}

/// Compositor-animated poses (M4): a pose the painter reports goes to
/// the compositor as surface state, the alpha multiplier, the viewport's
/// destination and the layer surface's margins, frame by frame; frames
/// that drew nothing commit it with no buffer; it ends at rest (full
/// opacity, the logical size, the placed margins).
#[test]
fn poses_go_to_the_compositor_without_buffers() {
    use strand_scene::{LogicalPoint, SurfacePose};
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let from = SurfacePose {
        opacity: 0.0,
        scale: 0.5,
        offset: LogicalPoint::new(100.0, 20.0),
    };
    let n = 8;
    mgr.state_mut().host_mut().poses = (0..=n).map(|k| pose_at(from, k, n)).collect();
    show_panel(&fake, &mut mgr);
    let id = mgr.state().surfaces_of(PANEL)[0];
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host().poses.is_empty()
                && fake
                    .layer("strand-Dash")
                    .first()
                    .is_some_and(|r| r.alpha == Some(u32::MAX))
        })
        .unwrap();
    assert!(
        ok,
        "the pose never settled: {:?}",
        fake.layer("strand-Dash")
    );
    common::pump(&mut mgr, Duration::from_millis(100));
    let rec = fake.layer("strand-Dash")[0].clone();
    // One buffer (the content at rest): every later pose was a bare
    // commit.
    assert_eq!(rec.buffer_commits, 1, "{rec:?}");
    assert!(rec.commits > n as usize, "{rec:?}");
    // The first frame went out with the start pose, opacity rising after.
    assert_eq!(rec.alpha_sets.first(), Some(&0), "{rec:?}");
    assert!(
        rec.alpha_sets.windows(2).all(|w| w[1] > w[0]),
        "{:?}",
        rec.alpha_sets
    );
    assert_eq!(rec.alpha, Some(u32::MAX));
    // Half size first; at rest the logical size on the fractional path,
    // unset (the buffer scale sizes it) on the integer one.
    assert_eq!(
        rec.viewport_sets.first(),
        Some(&Some((200, 150))),
        "{rec:?}"
    );
    let fractional = mgr.state().surface(id).unwrap().fractional;
    let rest = fractional.then_some((400, 300));
    assert_eq!(rec.viewport, rest, "fractional: {fractional}");
    // `top_right`: x moves by the right margin, y by the top one; back
    // at the placed margins (0) at rest.
    assert_eq!(rec.margin, Some([0, 0, 0, 0]), "{rec:?}");
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.pose, SurfacePose::IDENTITY);
    assert_eq!(info.stats.poses, n as u64 + 1, "{:?}", info.stats);
    assert_eq!(info.stats.commits, 1);
}

/// A pose's offset reaches the margins: mid-way through a slide the
/// compositor has the right margin shrunk by the offset and the top one
/// grown, and a spec change meanwhile (a new size) keeps them.
#[test]
fn a_pose_offset_moves_the_margins_and_survives_a_reconfigure() {
    use strand_scene::{LogicalPoint, SurfacePose};
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let held = SurfacePose {
        offset: LogicalPoint::new(30.0, 12.0),
        ..SurfacePose::IDENTITY
    };
    mgr.state_mut().host_mut().poses = [held].into_iter().collect();
    show_panel(&fake, &mut mgr);
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash")
                .first()
                .is_some_and(|r| r.margin == Some([12, -30, 0, 0]))
        })
        .unwrap();
    assert!(ok, "{:?}", fake.layer("strand-Dash"));
    let spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 300.0, 200.0);
    mgr.state_mut().apply_surface_change(
        PANEL,
        SurfaceChange::Updated {
            spec,
            recreate: false,
        },
    );
    let ok = mgr
        .dispatch_until(WAIT, |_| {
            fake.layer("strand-Dash")
                .first()
                .is_some_and(|r| r.configured == Some((300, 200)))
        })
        .unwrap();
    assert!(ok, "{:?}", fake.layer("strand-Dash"));
    common::pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(
        fake.layer("strand-Dash")[0].margin,
        Some([12, -30, 0, 0]),
        "the reconfigure kept the pose's margins"
    );
}

/// Without the alpha modifier no multiplier is ever set, though the
/// rest of a pose still goes out (render delegates nothing there, as
/// `CompositorCaps::delegates_poses` is false; this is the manager's
/// side only).
#[test]
fn no_alpha_modifier_no_multiplier() {
    use strand_scene::SurfacePose;
    let fake = Fake::compositor(SurfaceGlobals {
        alpha_modifier: false,
        ..SurfaceGlobals::default()
    });
    let mut mgr = manager(&fake);
    let half = SurfacePose {
        opacity: 0.5,
        ..SurfacePose::IDENTITY
    };
    mgr.state_mut().host_mut().poses = [half].into_iter().collect();
    show_panel(&fake, &mut mgr);
    common::pump(&mut mgr, Duration::from_millis(100));
    let rec = &fake.layer("strand-Dash")[0];
    assert_eq!(rec.alpha, None, "{rec:?}");
    assert!(rec.alpha_sets.is_empty());
}

/// An opaque region that changes on a frame that drew nothing (a
/// delegated fade settling: render claims the box only at full opacity)
/// goes out with that frame's bare commit, not only with a buffer.
#[test]
fn an_opaque_region_rides_a_bare_commit() {
    use strand_scene::SurfacePose;
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    show_panel(&fake, &mut mgr);
    common::pump(&mut mgr, Duration::from_millis(100));
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert!(info.opaque_region.is_empty());
    assert_eq!(info.stats.commits, 1);
    let bare = fake.layer("strand-Dash")[0].commits;
    // The fade ends: no new content, but the box is opaque now.
    let host = mgr.state_mut().host_mut();
    host.opaque = true;
    host.poses = [SurfacePose::IDENTITY].into_iter().collect();
    mgr.state_mut().poll();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| !i.opaque_region.is_empty())
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    common::pump(&mut mgr, Duration::from_millis(100));
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.stats.commits, 1, "no buffer: {:?}", info.stats);
    assert_eq!(info.stats.opaque_updates, 1, "{:?}", info.stats);
    let rec = &fake.layer("strand-Dash")[0];
    assert_eq!(rec.buffer_commits, 1, "{rec:?}");
    assert!(rec.commits > bare, "a bare commit carried it: {rec:?}");
}

/// While the compositor scales a surface, its surface-local coordinates
/// are the destination's: the opaque and blur regions go out scaled
/// with the pose (a 400 × 300 panel at half size claims and blurs only
/// its 200 × 150), and back at full size at rest.
#[test]
fn regions_follow_a_delegated_scale() {
    use strand_scene::SurfacePose;
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    let half = SurfacePose {
        scale: 0.5,
        ..SurfacePose::IDENTITY
    };
    {
        let host = mgr.state_mut().host_mut();
        host.opaque = true;
        host.blur = vec![rounded(400, 300, 16.0)];
        host.poses = [half].into_iter().collect();
    }
    show_panel(&fake, &mut mgr);
    wait_blur_sets(&fake, &mut mgr, 1);
    common::pump(&mut mgr, Duration::from_millis(100));
    let id = mgr.state().surfaces_of(PANEL)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.pose, half);
    assert_eq!(
        info.opaque_region,
        vec![strand_scene::Rect::new(0, 0, 200, 150)],
        "{info:?}"
    );
    let rec = &fake.layer("strand-Dash")[0];
    assert_eq!(rec.viewport, Some((200, 150)), "{rec:?}");
    let blur = rec.blur.clone().expect("a blur region");
    assert!(blur.contains(100, 75) && blur.contains(100, 0) && blur.contains(0, 75));
    assert!(!blur.contains(0, 0) && !blur.contains(201, 75) && !blur.contains(100, 151));
    let corners = 4.0 * (8.0f64 * 8.0) * (1.0 - std::f64::consts::PI / 4.0);
    let missing = (200 * 150) as f64 - blur.area() as f64;
    assert!(
        (missing - corners).abs() < 4.0 * 8.0,
        "the half-size corners are left out: {missing} px (about {corners})"
    );
    // At rest: the whole box again.
    mgr.state_mut().host_mut().poses = [SurfacePose::IDENTITY].into_iter().collect();
    mgr.state_mut().poll();
    wait_blur_sets(&fake, &mut mgr, 2);
    common::pump(&mut mgr, Duration::from_millis(100));
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(
        info.opaque_region,
        vec![strand_scene::Rect::new(0, 0, 400, 300)]
    );
    let blur = fake.layer("strand-Dash")[0].blur.clone().unwrap();
    assert!(blur.contains(399, 150) && blur.contains(200, 299));
}

/// A popup grab's keyboard over a `keyboard: none` bar on a compositor
/// that keeps the bar focused when it gives `exclusive` back, as the
/// fake does (sway does too, but sends the leave it owes with the next
/// grab's enter). A real focus loss after the next grab (a lock,
/// another exclusive surface) still reaches the grabbing popup; a
/// leave and enter for the bar arriving together (sway's late leave)
/// does not.
#[test]
fn a_grabbing_popup_loses_the_keyboard_after_an_earlier_release() {
    use strand_scene::{InputEvent, LogicalRect, SurfaceSpec};
    const BAR: NodeId = NodeId::new(1, 0);
    const POPUP: NodeId = NodeId::new(2, 0);
    let fake = Fake::builder()
        .toplevel_list(false)
        .surfaces(SurfaceGlobals::default())
        .seats(1)
        .start();
    let mut mgr = manager(&fake);
    mgr.state_mut()
        .apply_surface_change(BAR, SurfaceChange::Created(common::bar_spec("Top", 36.0)));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(BAR)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0)
        })
        .unwrap();
    assert!(ok, "the bar maps");
    let bar = mgr.state().surfaces_of(BAR)[0];
    fake.cmd(Cmd::KeyboardEnter("strand-Top"));
    let ok = mgr
        .dispatch_until(WAIT, |s| s.keyboard_focus() == Some(bar))
        .unwrap();
    assert!(ok, "the bar has keyboard focus");

    let popup = |open: bool| {
        let mut spec = SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
        spec.name = Some("Calendar".into());
        spec.parent = Some(BAR);
        spec.anchor_rect = Some(LogicalRect::new(100.0, 8.0, 60.0, 20.0));
        spec.width = Some(200.0);
        spec.height = Some(120.0);
        spec.open_two_way = true;
        spec.open = open;
        spec
    };
    // A key press, then a grabbing popup: it has the keys.
    let open = |mgr: &mut SurfaceManager<TestHost>, first: bool| {
        fake.cmd(Cmd::Key(1, true));
        fake.cmd(Cmd::Key(1, false));
        let n = mgr.state().host().input.len();
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.host().input[n..]
                    .iter()
                    .any(|e| matches!(e, InputEvent::Key { .. }))
            })
            .unwrap();
        assert!(ok, "the press reached the bar");
        let change = if first {
            SurfaceChange::Created(popup(true))
        } else {
            SurfaceChange::Updated {
                spec: popup(true),
                recreate: false,
            }
        };
        mgr.state_mut().apply_surface_change(POPUP, change);
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                s.surfaces_of(POPUP).first().is_some_and(|p| {
                    s.host()
                        .input
                        .contains(&InputEvent::KeyboardEnter { surface: *p })
                        && s.host()
                            .input
                            .iter()
                            .rposition(|e| *e == InputEvent::KeyboardEnter { surface: *p })
                            >= Some(n)
                })
            })
            .unwrap();
        assert!(ok, "the popup has the keys: {:?}", mgr.state().host().input);
        assert!(mgr.state().holds_keyboard_for_popup(bar));
        mgr.state().surfaces_of(POPUP)[0]
    };
    let close = |mgr: &mut SurfaceManager<TestHost>| {
        mgr.state_mut().apply_surface_change(
            POPUP,
            SurfaceChange::Updated {
                spec: popup(false),
                recreate: false,
            },
        );
        let ok = mgr
            .dispatch_until(WAIT, |s| s.surfaces_of(POPUP).is_empty())
            .unwrap();
        assert!(ok, "closed");
        assert!(!mgr.state().holds_keyboard_for_popup(bar));
    };
    fn leaves_in(host: &TestHost, p: strand_scene::SurfaceId) -> usize {
        host.input
            .iter()
            .filter(|e| **e == InputEvent::KeyboardLeave { surface: p })
            .count()
    }
    let leaves = |mgr: &SurfaceManager<TestHost>, p| leaves_in(mgr.state().host(), p);

    // Opened and closed: the bar gives `exclusive` back while focused,
    // and the fake sends no leave for it.
    let _ = open(&mut mgr, true);
    close(&mut mgr);
    common::pump(&mut mgr, Duration::from_millis(50));
    assert_eq!(mgr.state().keyboard_focus(), Some(bar));

    // Sway's late leave: a leave and an enter for the bar together,
    // right after the next grab. The popup keeps the keys.
    let p = open(&mut mgr, false);
    let before = leaves(&mgr, p);
    fake.cmd(Cmd::KeyboardEnter("strand-Top"));
    common::pump(&mut mgr, Duration::from_millis(100));
    assert_eq!(mgr.state().keyboard_focus(), Some(bar));
    assert_eq!(leaves(&mgr, p), before, "{:?}", mgr.state().host().input);
    close(&mut mgr);

    // No leave owed: the next grab comes with no leave or enter, and a
    // real focus loss later reaches the grabbing popup.
    let p = open(&mut mgr, false);
    common::pump(&mut mgr, Duration::from_millis(50));
    let before = leaves(&mgr, p);
    fake.cmd(Cmd::KeyboardLeave);
    let ok = mgr
        .dispatch_until(WAIT, |s| leaves_in(s.host(), p) > before)
        .unwrap();
    assert!(
        ok,
        "the grabbing popup lost the keyboard: {:?}",
        mgr.state().host().input
    );
    assert_eq!(mgr.state().keyboard_focus(), None);
}

/// (M4) The fake configures a surface only when its own request changes
/// (as a compositor that does not configure siblings again may): a
/// panel beside our bar is placed again when the live bar's exclusive
/// zone drops to none (`height: 0`) and when it comes back, though the
/// panel gets no configure of its own (`origin.rs`, `Surface::zone_on`).
#[test]
fn a_zone_that_drops_to_none_places_the_others_again() {
    let fake = Fake::compositor(SurfaceGlobals {
        xdg_output: true,
        ..SurfaceGlobals::default()
    });
    let mut mgr = manager(&fake);
    mgr.state_mut().apply_surface_change(
        common::BAR,
        SurfaceChange::Created(common::bar_spec("Top", 36.0)),
    );
    // On every screen: the fake sends no `wl_surface.enter`, so a
    // focused panel would never learn its output.
    let mut spec = layer_spec(NodeKind::Panel, "Dash", "top_right", 400.0, 300.0);
    spec.screens = strand_scene::Screens::All;
    mgr.state_mut()
        .apply_surface_change(PANEL, SurfaceChange::Created(spec));
    let origin = |s: &strand_surface::State<TestHost>| {
        s.surfaces_of(PANEL)
            .first()
            .and_then(|id| s.surface(*id))
            .and_then(|i| i.origin)
    };
    let wait = |mgr: &mut SurfaceManager<TestHost>, want: (i32, i32)| {
        let ok = mgr
            .dispatch_until(WAIT, |s| origin(s) == Some(want))
            .unwrap();
        assert!(
            ok,
            "the panel placed at {want:?}: {:?}, told {:?}",
            origin(mgr.state()),
            mgr.state().host().placed
        );
    };
    wait(&mut mgr, (1520, 36));
    let configures = |mgr: &SurfaceManager<TestHost>| {
        let s = mgr.state();
        s.surfaces_of(PANEL)
            .first()
            .and_then(|id| s.surface(*id))
            .map(|i| i.stats.configures)
    };
    let before = configures(&mgr);
    for (height, y) in [(0.0, 0), (36.0, 36)] {
        mgr.state_mut().apply_surface_change(
            common::BAR,
            SurfaceChange::Updated {
                spec: common::bar_spec("Top", height),
                recreate: false,
            },
        );
        wait(&mut mgr, (1520, y));
    }
    assert_eq!(configures(&mgr), before, "the panel got no configure");
}

/// (m4-audit) A frame whose callback never comes (a compositor that lost
/// it: GitHub run 38064533227's replugged bar, which logic updated and
/// never painted again) does not stop the surface for good: new content
/// waits while the frame may still come, and is painted once the wait
/// has lasted `THROTTLE_GIVE_UP` (1 s), with the give-up counted. When
/// the callbacks come again (late), painting goes on as before, with no
/// further give-up.
#[test]
fn a_lost_frame_callback_does_not_stop_the_surface() {
    let fake = Fake::compositor(SurfaceGlobals::default());
    let mut mgr = manager(&fake);
    show_panel(&fake, &mut mgr);
    repaint(&mut mgr);
    let id = mgr.state().surfaces_of(PANEL)[0];
    // Every callback asked for so far done, so the fake holds only the
    // next frame's.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.frames_done == i.stats.frame_requests)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    fake.cmd(Cmd::HoldFrames(true));
    // The fake takes the command before the next commit.
    fake.sync();
    // This frame's callback is held by the fake.
    repaint(&mut mgr);
    let stats = mgr.state().surface(id).unwrap().stats;
    assert_eq!(stats.throttle_given_up, 0, "{stats:?}");
    // New content while its callback is awaited: held, not painted.
    mgr.state_mut()
        .host_mut()
        .set_square(Some(strand_scene::LogicalRect::new(
            200.0, 50.0, 20.0, 20.0,
        )));
    mgr.state_mut().poll();
    let _ = mgr
        .dispatch_until(Duration::from_millis(200), |_| false)
        .unwrap();
    let held = mgr.state().surface(id).unwrap().stats;
    assert_eq!(held.commits, stats.commits, "{held:?}");
    assert_eq!(held.frames_done, stats.frames_done, "the callback was held");
    assert!(held.throttled > stats.throttled, "{held:?}");
    assert_eq!(held.throttle_given_up, 0);
    // No callback ever comes; the surface paints anyway.
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.commits > stats.commits)
        })
        .unwrap();
    let after = mgr.state().surface(id).unwrap().stats;
    assert!(ok, "never painted again: {after:?}");
    assert_eq!(after.throttle_given_up, 1, "{after:?}");
    assert_eq!(after.frames_done, stats.frames_done);
    // Callbacks come again (the held ones late): painting goes on as
    // before, each paint on its own callback, never another give-up.
    fake.cmd(Cmd::HoldFrames(false));
    for _ in 0..3 {
        repaint(&mut mgr);
    }
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.frames_done == i.stats.frame_requests)
        })
        .unwrap();
    let last = mgr.state().surface(id).unwrap().stats;
    assert!(ok, "a callback never came: {last:?}");
    assert!(last.frames_done >= after.frames_done + 3, "{last:?}");
    assert!(last.commits >= after.commits + 3, "{last:?}");
    assert_eq!(last.throttle_given_up, 1, "no further give-up: {last:?}");
    assert!(!mgr.state().give_up_armed(id), "{last:?}");
}

/// A fake that offers `wp_presentation`: while nothing animates the
/// manager asks for no frame callback and waits on presentation feedback
/// (`in_flight`), the path a real compositor (sway) takes.
fn presenting() -> Fake {
    Fake::compositor(SurfaceGlobals {
        presentation: true,
        ..SurfaceGlobals::default()
    })
}

/// Shows the panel on a presenting fake, paints it once more and waits
/// until every commit's feedback came; the panel's surface.
fn presented_panel(fake: &Fake, mgr: &mut SurfaceManager<TestHost>) -> SurfaceId {
    show_panel(fake, mgr);
    repaint(mgr);
    let id = mgr.state().surfaces_of(PANEL)[0];
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.presented + i.stats.discarded == i.stats.commits)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    id
}

/// New content for the panel while its frame is in flight: waits until
/// its paint is refused (throttled) or made.
fn new_content(mgr: &mut SurfaceManager<TestHost>, id: SurfaceId) {
    let before = mgr.state().surface(id).unwrap().stats;
    mgr.state_mut()
        .host_mut()
        .set_square(Some(strand_scene::LogicalRect::new(
            200.0, 50.0, 20.0, 20.0,
        )));
    mgr.state_mut().poll();
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id).is_some_and(|i| {
                i.stats.throttled > before.throttled || i.stats.commits > before.commits
            })
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
}

/// (m4-audit) The give-up on the presentation path: a frame whose
/// presentation feedback never comes (no frame callback was asked for,
/// nothing animating) stops new content only for `THROTTLE_GIVE_UP`,
/// counted once. The lost feedback arriving late, for a commit no longer
/// in flight, changes nothing; later paints go on through feedback alone.
#[test]
fn a_lost_presentation_feedback_does_not_stop_the_surface() {
    let fake = presenting();
    let mut mgr = manager(&fake);
    let id = presented_panel(&fake, &mut mgr);
    let before = mgr.state().surface(id).unwrap().stats;
    fake.cmd(Cmd::HoldFeedback(true));
    fake.sync();
    // This frame's feedback is held; no callback is asked for.
    repaint(&mut mgr);
    let stats = mgr.state().surface(id).unwrap().stats;
    assert_eq!(stats.frame_requests, before.frame_requests, "{stats:?}");
    new_content(&mut mgr, id);
    let held = mgr.state().surface(id).unwrap().stats;
    assert_eq!(held.presented, stats.presented, "the feedback was held");
    assert_eq!(
        held.commits, stats.commits,
        "held while in flight: {held:?}"
    );
    assert!(held.throttled > stats.throttled, "{held:?}");
    assert!(
        mgr.state().give_up_armed(id),
        "a refused paint arms the give-up"
    );
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.commits > stats.commits)
        })
        .unwrap();
    let after = mgr.state().surface(id).unwrap().stats;
    assert!(ok, "never painted again: {after:?}");
    assert_eq!(after.throttle_given_up, 1, "{after:?}");
    assert_eq!(after.presented, stats.presented);
    assert_eq!(after.frame_requests, before.frame_requests);
    // The held feedback comes late (the lost commit's, and the give-up
    // paint's, which settles it); painting goes on with no give-up.
    fake.cmd(Cmd::HoldFeedback(false));
    for _ in 0..3 {
        repaint(&mut mgr);
    }
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.presented + i.stats.discarded == i.stats.commits)
        })
        .unwrap();
    let last = mgr.state().surface(id).unwrap().stats;
    assert!(ok, "feedback never came: {last:?}");
    assert!(last.commits >= after.commits + 3, "{last:?}");
    assert_eq!(last.throttle_given_up, 1, "no further give-up: {last:?}");
    assert_eq!(last.frame_requests, before.frame_requests, "{last:?}");
    assert!(!mgr.state().give_up_armed(id), "{last:?}");
}

/// (m4-audit) Feedback that comes late but within `THROTTLE_GIVE_UP`
/// settles the frame: the content held meanwhile is painted then, and
/// the give-up timer the refused paint armed is cancelled, so no give-up
/// is counted and nothing is painted again at the 1 s mark.
#[test]
fn late_presentation_feedback_cancels_the_give_up() {
    let fake = presenting();
    let mut mgr = manager(&fake);
    let id = presented_panel(&fake, &mut mgr);
    fake.cmd(Cmd::HoldFeedback(true));
    fake.sync();
    repaint(&mut mgr);
    let painted = Instant::now();
    let stats = mgr.state().surface(id).unwrap().stats;
    new_content(&mut mgr, id);
    let held = mgr.state().surface(id).unwrap().stats;
    assert_eq!(
        held.commits, stats.commits,
        "held while in flight: {held:?}"
    );
    assert!(held.throttled > stats.throttled, "{held:?}");
    assert!(
        mgr.state().give_up_armed(id),
        "a refused paint arms the give-up"
    );
    fake.cmd(Cmd::HoldFeedback(false));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.stats.commits > stats.commits)
        })
        .unwrap();
    let after = mgr.state().surface(id).unwrap().stats;
    assert!(ok, "the held content was never painted: {after:?}");
    assert!(after.presented > stats.presented, "{after:?}");
    if after.throttle_given_up == 0 {
        // The feedback settled the frame before the give-up: its timer is
        // gone, not left to fire.
        assert!(!mgr.state().give_up_armed(id), "{after:?}");
        common::pump(&mut mgr, Duration::from_millis(1200));
        let quiet = mgr.state().surface(id).unwrap().stats;
        assert_eq!(quiet.throttle_given_up, 0, "{quiet:?}");
        assert_eq!(quiet.commits, after.commits, "nothing painted at 1 s");
    } else {
        // A runner stalled past the give-up before the feedback was
        // read: the give-up painted instead, which is the other test's.
        eprintln!(
            "the give-up came first ({:?} after the paint); the cancel was not checked",
            painted.elapsed()
        );
    }
}
