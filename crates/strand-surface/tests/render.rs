//! End to end: the real `strand-render` renderer as the surface host on a
//! headless sway, wired the way `docs/architecture.md` ("Render loop")
//! describes, checked with grim.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Sway, WAIT};
use strand_render::{Renderer, TextBackend};
use strand_scene::{
    Color, Damage, Font, NodeId, NodeKind, PaintTarget, Painter, Prop, PropValue, Scale, SceneDiff,
    SceneOp, Size, SurfaceId, Transition,
};
use strand_surface::{Config, Monitor, SurfaceHost, SurfaceManager};
use strand_text::{FontConfig, TEST_FONT_FAMILY, TextEngine, TextWorker, test_font_path};

/// What the binary will do: forward lifecycle hooks to the renderer.
struct Host {
    renderer: Renderer,
    /// For each non-empty paint: was text still being shaped?
    text_pending_at_paint: Vec<bool>,
    /// The presentation time of every paint.
    times: Vec<Duration>,
    /// Every paint that drew: its surface, buffer age and the alpha at
    /// the buffer's centre.
    drawn: Vec<(SurfaceId, u8, u8)>,
}

impl Host {
    fn new(renderer: Renderer) -> Self {
        Self {
            renderer,
            text_pending_at_paint: Vec::new(),
            times: Vec::new(),
            drawn: Vec::new(),
        }
    }
}

impl Painter for Host {
    fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage {
        let pending = self.renderer.text_pending();
        self.times.push(target.time);
        let age = target.age;
        let damage = self.renderer.paint(surface, target);
        if !damage.is_empty() {
            self.text_pending_at_paint.push(pending);
            let (w, h) = (target.size.w, target.size.h);
            let i = ((h / 2 * w + w / 2) * 4 + 3) as usize;
            let alpha = target.pixels.get(i).copied().unwrap_or(0);
            self.drawn.push((surface, age, alpha));
        }
        damage
    }

    fn wants_frame(&self, surface: SurfaceId) -> bool {
        self.renderer.wants_frame(surface)
    }

    fn opaque_region(&self, surface: SurfaceId) -> Damage {
        self.renderer.opaque_region(surface)
    }
}

impl SurfaceHost for Host {
    fn surface_attached(&mut self, surface: SurfaceId, node: NodeId, _: Option<&Monitor>) {
        self.renderer.attach_surface(surface, node);
    }

    fn surface_configured(&mut self, surface: SurfaceId, size: Size, scale: Scale) {
        self.renderer.configure_surface(surface, size, scale);
    }

    fn surface_detached(&mut self, surface: SurfaceId) {
        self.renderer.detach_surface(surface);
    }

    fn frame_deadline(&self, surface: SurfaceId) -> Option<Instant> {
        self.renderer.frame_deadline(surface)
    }

    fn frame_dropped(&mut self, surface: SurfaceId) {
        self.renderer.invalidate(surface);
    }
}

const ROOT: NodeId = NodeId::new(0, 0);
const CLOCK: NodeId = NodeId::new(1, 0);
const SQUARE: NodeId = NodeId::new(2, 0);

fn rgb(c: &str) -> [u8; 3] {
    let c = Color::from_hex(c).unwrap();
    let to = |v: f32| (v * 255.0).round() as u8;
    [to(c.r), to(c.g), to(c.b)]
}

fn scene() -> SceneDiff {
    let mut d = SceneDiff::default();
    d.create(ROOT, NodeKind::Bar, None, 0);
    d.set(ROOT, Prop::Name, PropValue::Text("Top".into()));
    d.set(ROOT, Prop::Edge, PropValue::Keyword("top".into()));
    d.set(ROOT, Prop::Height, PropValue::Number(36.0));
    d.set(
        ROOT,
        Prop::Bg,
        PropValue::Color(Color::from_hex("#1e1e2e").unwrap()),
    );
    d.set(
        ROOT,
        Prop::Color,
        PropValue::Color(Color::from_hex("#ffffff").unwrap()),
    );
    d.set(
        ROOT,
        Prop::Font,
        PropValue::Font(Font {
            family: TEST_FONT_FAMILY.into(),
            size: 16.0,
            weight: 400,
        }),
    );
    d.create(CLOCK, NodeKind::Text, Some(ROOT), 0);
    d.set(CLOCK, Prop::Text, PropValue::Text("12:34".into()));
    d.set(CLOCK, Prop::X, PropValue::Number(20.0));
    d.set(CLOCK, Prop::Y, PropValue::Number(8.0));
    d.create(SQUARE, NodeKind::Box, Some(ROOT), 1);
    d.set(SQUARE, Prop::X, PropValue::Number(300.0));
    d.set(SQUARE, Prop::Y, PropValue::Number(8.0));
    d.set(SQUARE, Prop::Size, PropValue::Number(20.0));
    d.set(
        SQUARE,
        Prop::Bg,
        PropValue::Color(Color::from_hex("#f38ba8").unwrap()),
    );
    d
}

/// Applies a diff the way the binary's diff-channel callback will.
fn apply(mgr: &mut SurfaceManager<Host>, diff: SceneDiff) {
    let state = mgr.state_mut();
    let errors = state.host_mut().renderer.apply(diff);
    assert!(errors.is_empty(), "{errors:?}");
    let changes = state.host_mut().renderer.take_surface_changes();
    for (node, change) in changes {
        state.apply_surface_change(node, change);
    }
    state.poll();
}

#[test]
fn renderer_paints_a_bar_through_the_surface_manager() {
    let Some(sway) = Sway::start("renderer_paints_a_bar_through_the_surface_manager") else {
        return;
    };
    let font = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let host = Host::new(Renderer::new(TextBackend::Inline(Box::new(engine))));
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default()).unwrap();
    apply(&mut mgr, scene());
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let st = s.stats();
            st.commits > 0 && st.presented >= st.commits
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let info = mgr.state().surfaces()[0].clone();
    assert_eq!(info.namespace, "strand-Top");
    assert_eq!(info.buffer_size, Size::new(1920, 36));
    // The opaque background was declared opaque.
    assert_eq!(info.stats.opaque_updates, 1);

    let shot = sway.grim("HEADLESS-1");
    let bg = rgb("#1e1e2e");
    assert_eq!(shot.rgb(1000, 18), bg);
    assert_eq!(shot.rgb(310, 18), rgb("#f38ba8"));
    // The clock's glyphs are drawn: some pixel in its box is light.
    let lit = (20..80).any(|x| (8..30).any(|y| shot.rgb(x, y)[0] > 0xc0));
    assert!(lit, "no text drawn");

    // Clock-tick-sized changes repaint only what moved, from the first
    // change on: the second buffer starts as a copy of the first frame
    // (copy-forward), so it is not painted in full.
    // Instant moves (`~ instant`): one frame each.
    for x in [500.0, 700.0, 900.0] {
        let mut d = SceneDiff::default();
        d.push(SceneOp::SetProp {
            id: SQUARE,
            prop: Prop::X,
            value: PropValue::Number(x),
            transition: Transition::Instant,
        });
        let commits = mgr.state().stats().commits;
        apply(&mut mgr, d);
        let ok = mgr
            .dispatch_until(WAIT, |s| {
                let st = s.stats();
                st.commits > commits && st.presented >= st.commits
            })
            .unwrap();
        assert!(ok);
        let damage = mgr.state().host().renderer.last_damage(info.id).unwrap();
        assert!(damage.area() <= 4 * 20 * 20, "move to {x}: {damage:?}");
    }
    std::thread::sleep(Duration::from_millis(50));
    let shot = sway.grim("HEADLESS-1");
    for x in [310, 510, 710] {
        assert_eq!(shot.rgb(x, 18), bg, "old square at {x}");
    }
    assert_eq!(shot.rgb(910, 18), rgb("#f38ba8"));
    assert!((20..80).any(|x| (8..30).any(|y| shot.rgb(x, y)[0] > 0xc0)));

    // Then it is idle: no frame callbacks pending, nothing painted.
    let before = mgr.state().stats();
    let t = Instant::now();
    mgr.dispatch(Some(Duration::from_millis(700))).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(650), "woke while idle");
    assert_eq!(mgr.state().stats(), before);
}

/// With the real text worker, the first frame waits for its text (up to
/// the renderer's first-frame wait) instead of going out without it.
#[test]
fn first_frame_waits_for_its_text() {
    let Some(sway) = Sway::start("first_frame_waits_for_its_text") else {
        return;
    };
    let font = std::fs::read(test_font_path()).unwrap();
    let (ping, ping_source) = calloop::ping::make_ping().unwrap();
    let worker = TextWorker::spawn_with_waker(
        FontConfig::isolated(vec![Arc::new(font)]),
        Some(Box::new(move || ping.ping())),
    )
    .unwrap();
    let mut renderer = Renderer::new(TextBackend::Worker(worker));
    // Long enough that only the text's arrival can end the hold here.
    let wait = Duration::from_millis(800);
    renderer.set_first_frame_wait(wait);
    let mut mgr =
        SurfaceManager::with_connection(sway.connect(), Host::new(renderer), Config::default())
            .unwrap();
    // The binary's wiring: text delivered → `update`, then `poll`.
    mgr.loop_handle()
        .insert_source(ping_source, |_, _, state| {
            state.host_mut().renderer.update();
            state.poll();
        })
        .unwrap();
    let t = Instant::now();
    apply(&mut mgr, scene());
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let st = s.stats();
            st.commits > 0 && st.presented + st.discarded >= st.commits
        })
        .unwrap();
    assert!(ok, "{:?}", mgr.state().surfaces());
    let first_commit = t.elapsed();
    let host = mgr.state().host();
    assert_eq!(
        host.text_pending_at_paint.first(),
        Some(&false),
        "the first frame went out while its text was being shaped"
    );
    assert!(
        first_commit < wait,
        "held until the deadline: {first_commit:?}"
    );
    assert_eq!(mgr.state().stats().commits, 1, "{:?}", mgr.state().stats());
    let shot = sway.grim("HEADLESS-1");
    let lit = (20..80).any(|x| (8..30).any(|y| shot.rgb(x, y)[0] > 0xc0));
    assert!(lit, "no text in the first frame");

    // The hold's deadline timer was cancelled by the paint: nothing wakes
    // the loop when it would have fired.
    let before = mgr.state().stats();
    let t = Instant::now();
    mgr.dispatch(Some(wait + Duration::from_millis(200)))
        .unwrap();
    assert!(
        t.elapsed() >= wait + Duration::from_millis(150),
        "woke after {:?}",
        t.elapsed()
    );
    assert_eq!(mgr.state().stats(), before);
}

/// A spring runs one frame per refresh, sampled at predicted presentation
/// times, and once it settles the surface is truly idle: no frame
/// callbacks, no commits, no wakeups.
#[test]
fn springs_run_at_the_refresh_rate_then_the_surface_is_idle() {
    let Some(sway) = Sway::start("springs_run_at_the_refresh_rate_then_the_surface_is_idle") else {
        return;
    };
    let font = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let host = Host::new(Renderer::new(TextBackend::Inline(Box::new(engine))));
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default()).unwrap();
    apply(&mut mgr, scene());
    let ok = mgr
        .dispatch_until(WAIT, |s| {
            let st = s.stats();
            st.commits > 0 && st.presented >= st.commits
        })
        .unwrap();
    assert!(ok);
    let id = mgr.state().surfaces()[0].id;
    let before = mgr.state().stats();
    let painted = mgr.state().host().times.len();
    // `$motion.spatial` (the design's spring: no theme here) moves the
    // square and `$motion.effects` fades its colour.
    let mut d = SceneDiff::default();
    d.set(SQUARE, Prop::X, PropValue::Number(1200.0)).set(
        SQUARE,
        Prop::Bg,
        PropValue::Color(Color::from_hex("#89b4fa").unwrap()),
    );
    apply(&mut mgr, d);
    assert!(
        mgr.state().host().renderer.animating(id) || mgr.state().host().renderer.wants_frame(id)
    );
    let ok = mgr
        .dispatch_until(Duration::from_secs(5), |s| {
            !s.host().renderer.animating(id) && s.stats().presented >= s.stats().commits
        })
        .unwrap();
    assert!(ok, "never settled");
    let after = mgr.state().stats();
    let frames = after.commits - before.commits;
    assert!((8..=90).contains(&frames), "{frames} frames for one spring");
    // Each frame waited for the previous one's callback.
    assert!(after.frame_requests - before.frame_requests >= frames - 1);
    // Springs sampled increasing presentation times, about a refresh
    // apart (headless sway runs at 60 Hz): most frame-to-frame steps are
    // within a quarter of a refresh of it.
    let times = &mgr.state().host().times[painted..];
    assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    let refresh = Duration::from_micros(16_667);
    let steps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let near = steps
        .iter()
        .filter(|d| d.abs_diff(refresh) <= refresh / 4)
        .count();
    assert!(
        near * 4 >= steps.len() * 3,
        "{near} of {} steps near {refresh:?}: {steps:?}",
        steps.len()
    );
    let span = *times.last().unwrap() - times[0];
    assert!(
        span >= Duration::from_millis(100) && span <= Duration::from_secs(3),
        "{span:?} over {} frames",
        times.len()
    );
    std::thread::sleep(Duration::from_millis(50));
    let shot = sway.grim("HEADLESS-1");
    assert_eq!(shot.rgb(1210, 18), rgb("#89b4fa"));
    assert_eq!(shot.rgb(310, 18), rgb("#1e1e2e"));

    // Settled: true idle. Nothing is requested, committed or painted, and
    // the loop does not wake.
    let idle = mgr.state().stats();
    let paints = mgr.state().host().times.len();
    let t = Instant::now();
    mgr.dispatch(Some(Duration::from_millis(700))).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(650), "woke while idle");
    assert_eq!(mgr.state().stats(), idle);
    assert_eq!(mgr.state().host().times.len(), paints);
    assert!(!mgr.state().host().renderer.wants_frame(id));
}

const PANEL: NodeId = NodeId::new(20, 0);

/// Dispatches until `done`, waking the way the binary does: when a
/// paint leaves surface changes behind (`has_surface_changes`), the
/// host runs `update` and hands them to the manager.
fn run_until(
    mgr: &mut SurfaceManager<Host>,
    timeout: Duration,
    mut done: impl FnMut(&strand_surface::State<Host>) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let state = mgr.state_mut();
        if state.host().renderer.has_surface_changes() {
            state.host_mut().renderer.update();
            for (node, change) in state.host_mut().renderer.take_surface_changes() {
                state.apply_surface_change(node, change);
            }
            state.poll();
        }
        if done(mgr.state()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        mgr.dispatch(Some(Duration::from_millis(5))).unwrap();
    }
}

/// A panel whose `open` toggles plays its poses on screen: each open
/// makes a new surface (a fresh buffer: age 0) that fades and scales in
/// over several frames; each close plays the exit (the mirrored enter)
/// and then the surface is destroyed, with no other input.
#[test]
fn a_toggling_panel_plays_its_poses_and_goes_away() {
    let Some(sway) = Sway::start("a_toggling_panel_plays_its_poses_and_goes_away") else {
        return;
    };
    let font = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(font)]));
    let host = Host::new(Renderer::new(TextBackend::Inline(Box::new(engine))));
    let mut mgr = SurfaceManager::with_connection(sway.connect(), host, Config::default()).unwrap();
    let mut d = SceneDiff::default();
    d.create(PANEL, NodeKind::Panel, None, 0)
        .set(PANEL, Prop::Name, PropValue::Text("Pop".into()))
        .set(PANEL, Prop::Width, PropValue::Number(240.0))
        .set(PANEL, Prop::Height, PropValue::Number(120.0))
        .set(
            PANEL,
            Prop::Bg,
            PropValue::Color(Color::from_hex("#89b4fa").unwrap()),
        )
        .set(PANEL, Prop::Open, PropValue::Bool(false))
        .set(
            PANEL,
            Prop::Enter,
            PropValue::Pose(vec![
                (Prop::Opacity, PropValue::Number(0.0)),
                (Prop::Scale, PropValue::Number(0.9)),
            ]),
        );
    apply(&mut mgr, d);
    mgr.dispatch(Some(Duration::from_millis(50))).unwrap();
    assert!(mgr.state().surfaces_of(PANEL).is_empty(), "closed");
    for round in 0..2 {
        let opened = mgr.state().host().drawn.len();
        let mut d = SceneDiff::default();
        d.set(PANEL, Prop::Open, PropValue::Bool(true));
        apply(&mut mgr, d);
        let mut id = None;
        let ok = run_until(&mut mgr, Duration::from_secs(5), |s| {
            id = s.surfaces_of(PANEL).first().copied();
            id.is_some_and(|id| {
                s.host().drawn[opened..].iter().any(|p| p.0 == id)
                    && !s.host().renderer.wants_frame(id)
                    && s.stats().presented >= s.stats().commits
            })
        });
        assert!(ok, "round {round}: never settled open");
        // (The manager keeps a node's surface id across re-creations.)
        let id = id.unwrap();
        let frames: Vec<(u8, u8)> = mgr.state().host().drawn[opened..]
            .iter()
            .filter(|p| p.0 == id)
            .map(|p| (p.1, p.2))
            .collect();
        assert!(frames.len() >= 5, "round {round}: enter frames {frames:?}");
        assert_eq!(frames[0].0, 0, "a fresh buffer: {frames:?}");
        assert!(frames[0].1 < 128, "starts near transparent: {frames:?}");
        assert!(
            frames[..5].windows(2).all(|w| w[1].1 > w[0].1),
            "fades in frame by frame: {frames:?}"
        );
        assert_eq!(frames.last().unwrap().1, 255, "{frames:?}");
        std::thread::sleep(Duration::from_millis(50));
        let shot = sway.grim("HEADLESS-1");
        assert_eq!(
            shot.rgb(960, 540),
            rgb("#89b4fa"),
            "round {round}: on screen"
        );

        let before = mgr.state().host().drawn.len();
        let mut d = SceneDiff::default();
        d.set(PANEL, Prop::Open, PropValue::Bool(false));
        apply(&mut mgr, d);
        assert_eq!(
            mgr.state().surfaces_of(PANEL),
            vec![id],
            "stays for its exit"
        );
        let ok = run_until(&mut mgr, Duration::from_secs(5), |s| {
            s.surfaces_of(PANEL).is_empty()
        });
        assert!(ok, "round {round}: never destroyed after its exit");
        let exit: Vec<u8> = mgr.state().host().drawn[before..]
            .iter()
            .filter(|p| p.0 == id)
            .map(|p| p.2)
            .collect();
        assert!(exit.len() >= 3, "round {round}: exit frames {exit:?}");
        assert!(
            exit.windows(2).all(|w| w[1] <= w[0]) && exit[0] < 255,
            "fades out: {exit:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        mgr.dispatch(Some(Duration::from_millis(20))).unwrap();
        let shot = sway.grim("HEADLESS-1");
        assert_ne!(shot.rgb(960, 540), rgb("#89b4fa"), "round {round}: gone");
    }
}
