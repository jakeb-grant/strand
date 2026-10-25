//! (window) `window` surfaces on a private headless sway (decisions.md
//! `## window`): an `xdg_toplevel` sway manages, with the title and app
//! id its spec names, tiled at the size sway gives and relaid out when
//! the layout changes, floating at its own size, server-side decorated
//! when it asks, closed by sway as a click away for the host, and the
//! parent of the popups declared in it.

mod common;

use std::time::Duration;

use common::*;
use serde_json::Value;
use strand_scene::{Insets, LogicalRect, NodeId, NodeKind, SurfaceChange, SurfaceSpec, WindowAsk};
use strand_surface::{Config, InputEvent, SurfaceManager, ToplevelState};

const WIN: NodeId = NodeId::new(10, 0);
const OTHER: NodeId = NodeId::new(11, 0);
const MENU: NodeId = NodeId::new(12, 0);

fn manager(sway: &Sway) -> SurfaceManager<TestHost> {
    SurfaceManager::with_connection(sway.connect(), TestHost::default(), Config::default())
        .expect("surface manager starts")
}

/// The views in sway's tree whose app id is `app_id`.
fn views(sway: &Sway, app_id: &str) -> Vec<Value> {
    let tree: Value = serde_json::from_str(&sway.msg(&["-t", "get_tree", "-r"])).unwrap();
    let mut out = Vec::new();
    let mut walk = vec![tree];
    while let Some(n) = walk.pop() {
        if n["app_id"] == app_id {
            out.push(n.clone());
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(kids) = n[key].as_array() {
                walk.extend(kids.iter().cloned());
            }
        }
    }
    out
}

/// A view's content size, `window_rect`'s width and height.
fn content(view: &Value) -> (u32, u32) {
    let r = &view["window_rect"];
    (
        r["width"].as_u64().unwrap() as u32,
        r["height"].as_u64().unwrap() as u32,
    )
}

/// Dispatches until `node`'s window was configured in a state `want`
/// holds of and painted a frame at the size that configure gave; that
/// state.
fn wait_state(
    mgr: &mut SurfaceManager<TestHost>,
    node: NodeId,
    want: impl Fn(&ToplevelState) -> bool,
) -> ToplevelState {
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(node)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| {
                    i.stats.commits > 0
                        && i.toplevel
                            .is_some_and(|t| want(&t) && t.size == i.logical_size)
                        && i.buffer_size.w == i.logical_size.0
                        && i.buffer_size.h == i.logical_size.1
                })
        })
        .unwrap();
    assert!(
        ok,
        "the window paints: {:?}",
        mgr.state()
            .surfaces_of(node)
            .first()
            .and_then(|id| mgr.state().surface(*id))
    );
    mgr.state().toplevel_state(node).unwrap()
}

fn spec_change(mgr: &mut SurfaceManager<TestHost>, node: NodeId, spec: SurfaceSpec) {
    mgr.state_mut().apply_surface_change(
        node,
        SurfaceChange::Updated {
            spec,
            recreate: false,
        },
    );
}

/// A window maps as a toplevel sway names by its title and app id, is
/// tiled at sway's size and laid out again when a second window splits
/// the workspace, decorated by sway, fullscreened when it asks, and
/// closed by sway as a click away (it goes when its spec closes).
#[test]
fn a_window_is_a_toplevel_sway_tiles_and_closes() {
    let Some(sway) = Sway::start("a_window_is_a_toplevel_sway_tiles_and_closes") else {
        return;
    };
    let mut mgr = manager(&sway);
    let input = mgr.take_input().unwrap();
    let spec = window_spec("Settings", "Preferences", 400.0, 300.0);
    mgr.state_mut()
        .apply_surface_change(WIN, SurfaceChange::Created(spec.clone()));
    // sway's first configure leaves the size to it; once mapped, it is
    // tiled.
    let state = wait_state(&mut mgr, WIN, |t| t.tiled);
    let id = mgr.state().surfaces_of(WIN)[0];
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.kind, NodeKind::Window);
    assert_eq!(info.layer, None);
    assert_eq!(mgr.state().host().attached, vec![(id, WIN, None)]);
    // In sway's tree, by its title and app id.
    let v = views(&sway, "strand-Settings");
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0]["name"], "Preferences");
    // Tiled at sway's size (the whole workspace less its border), not
    // the 400 × 300 it asked for, and decorated by sway.
    assert!(state.sized.0 && state.sized.1, "{state:?}");
    assert!(state.tiled, "{state:?}");
    assert!(state.decorated, "sway draws its title bar: {state:?}");
    assert_eq!(state.size, content(&v[0]));
    assert!(state.size.0 > 1800, "{state:?}");
    assert_eq!(info.logical_size, state.size);
    // Its title changes in place.
    let mut renamed = spec.clone();
    renamed.title = Some("Settings — Display".into());
    spec_change(&mut mgr, WIN, renamed);
    pump(&mut mgr, Duration::from_millis(200));
    assert_eq!(
        views(&sway, "strand-Settings")[0]["name"],
        "Settings — Display"
    );
    assert_eq!(mgr.state().surfaces_of(WIN), vec![id], "the same window");

    // A second window splits the workspace: the first is configured at
    // its half and painted at it.
    mgr.state_mut().apply_surface_change(
        OTHER,
        SurfaceChange::Created(window_spec("Other", "Other", 300.0, 200.0)),
    );
    wait_state(&mut mgr, OTHER, |t| t.tiled);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.logical_size.0 < 1000 && i.buffer_size.w == i.logical_size.0)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    pump(&mut mgr, Duration::from_millis(200));
    let half = mgr.state().surface(id).unwrap().logical_size;
    assert_eq!(half, content(&views(&sway, "strand-Settings")[0]));
    assert_configured_sizes_painted(mgr.state().host());

    // Fullscreen when it asks (sway offers it), and back.
    mgr.state_mut().window_ask(WIN, WindowAsk::Fullscreen, None);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.toplevel_state(WIN)
                .is_some_and(|t| t.fullscreen && t.size == (1920, 1080))
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().toplevel_state(WIN));
    mgr.state_mut().window_ask(WIN, WindowAsk::Fullscreen, None);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.toplevel_state(WIN)
                .is_some_and(|t| !t.fullscreen && t.size == half)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().toplevel_state(WIN));

    // sway's close (`kill`) is a click away for the host; the window
    // stays until its spec closes.
    sway.msg(&["[app_id=strand-Settings]", "kill"]);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.host()
                .input
                .contains(&InputEvent::ClickAway { surface: id })
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().host().input);
    assert_eq!(mgr.state().surfaces_of(WIN), vec![id]);
    // `w.close()` says the same.
    let before = mgr.state().host().input.len();
    mgr.state_mut().window_ask(WIN, WindowAsk::Close, None);
    assert!(mgr.state().host().input[before..].contains(&InputEvent::ClickAway { surface: id }));
    let mut closed = spec;
    closed.open = false;
    spec_change(&mut mgr, WIN, closed);
    assert!(mgr.state().surfaces_of(WIN).is_empty());
    pump(&mut mgr, Duration::from_millis(200));
    assert!(views(&sway, "strand-Settings").is_empty());
    assert_eq!(mgr.state().host().detached, vec![id]);
    drop(input);
}

/// A floating window keeps its own size: what it asks for, and a new
/// size as its content changes; with `decorations: none` sway does not
/// decorate it (one of a fixed size, which sway floats), and it draws
/// its own.
#[test]
fn a_floating_window_takes_its_own_size() {
    let Some(sway) = Sway::start("a_floating_window_takes_its_own_size") else {
        return;
    };
    let mut mgr = manager(&sway);
    let mut spec = window_spec("Picker", "Picker", 400.0, 300.0);
    spec.decorations = strand_scene::Decorations::None;
    // A window of one size (sway floats it and honours the decorations
    // it asks for then; a window sway tiles, or floats by a rule, it
    // decorates whatever it asks).
    spec.min_size = (Some(400.0), Some(300.0));
    spec.max_size = (Some(400.0), Some(300.0));
    mgr.state_mut()
        .apply_surface_change(WIN, SurfaceChange::Created(spec.clone()));
    pump(&mut mgr, Duration::from_millis(300));
    let state = wait_state(&mut mgr, WIN, |_| true);
    assert_eq!(state.size, (400, 300), "{state:?}");
    assert_eq!(state.sized, (false, false), "{state:?}");
    assert!(!state.tiled && !state.decorated, "{state:?}");
    pump(&mut mgr, Duration::from_millis(200));
    let v = views(&sway, "strand-Picker");
    assert_eq!(v[0]["type"], "floating_con", "{v:?}");
    assert_eq!(content(&v[0]), (400, 300));
    // Its content grew: it asks for the new size, and sway takes it.
    let taller = taller_spec(spec.clone());
    spec_change(&mut mgr, WIN, taller);
    let id = mgr.state().surfaces_of(WIN)[0];
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.logical_size == (400, 360) && i.buffer_size.h == 360)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    pump(&mut mgr, Duration::from_millis(300));
    assert_eq!(content(&views(&sway, "strand-Picker")[0]), (400, 360));
    // A shadow past its box: the buffer grows, the window geometry
    // (what sway places and sizes) stays the box.
    let mut shadowed = taller_spec(spec);
    shadowed.overhang = Insets::all(12.0);
    spec_change(&mut mgr, WIN, shadowed);
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surface(id)
                .is_some_and(|i| i.logical_size == (424, 384) && i.buffer_size.h == 384)
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surface(id));
    let info = mgr.state().surface(id).unwrap();
    assert_eq!(info.input_region, Some(Some((12, 12, 400, 360))));
    pump(&mut mgr, Duration::from_millis(300));
    assert_eq!(content(&views(&sway, "strand-Picker")[0]), (400, 360));
}

fn taller_spec(mut spec: SurfaceSpec) -> SurfaceSpec {
    spec.height = Some(360.0);
    spec.min_size.1 = Some(360.0);
    spec.max_size.1 = Some(360.0);
    spec
}

/// A popup declared in a window nests in its `xdg_surface`.
#[test]
fn a_popup_in_a_window_nests_in_it() {
    let Some(sway) = Sway::start("a_popup_in_a_window_nests_in_it") else {
        return;
    };
    sway.msg(&[
        "for_window",
        "[app_id=strand-Settings]",
        "floating",
        "enable",
    ]);
    let mut mgr = manager(&sway);
    mgr.state_mut().apply_surface_change(
        WIN,
        SurfaceChange::Created(window_spec("Settings", "Settings", 400.0, 300.0)),
    );
    wait_state(&mut mgr, WIN, |_| true);
    let mut menu = SurfaceSpec::resolve(NodeKind::Popup, |_| None::<&strand_scene::PropValue>);
    menu.name = Some("Menu".into());
    menu.parent = Some(WIN);
    menu.anchor_rect = Some(LogicalRect::new(20.0, 20.0, 80.0, 24.0));
    menu.width = Some(160.0);
    menu.height = Some(120.0);
    menu.open_two_way = true;
    mgr.state_mut()
        .apply_surface_change(MENU, SurfaceChange::Created(menu.clone()));
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            s.surfaces_of(MENU)
                .first()
                .and_then(|id| s.surface(*id))
                .is_some_and(|i| i.stats.commits > 0 && i.logical_size == (160, 120))
        })
        .unwrap();
    assert!(ok, "the menu maps: {:?}", mgr.state().surfaces());
    // It goes with its window.
    let mut closed = window_spec("Settings", "Settings", 400.0, 300.0);
    closed.open = false;
    spec_change(&mut mgr, WIN, closed);
    assert!(mgr.state().surfaces_of(MENU).is_empty());
    assert!(mgr.state().surfaces().is_empty());
}
