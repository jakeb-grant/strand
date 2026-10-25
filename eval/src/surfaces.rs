//! The surface manager's and the compositor's side (docs/eval.md,
//! "Driver"): every surface the renderer reports is attached, configured
//! at the size the compositor gives it, placed where the compositor puts
//! it, configured or placed again when that changes, and detached when it
//! closes, each with a buffer of its own. Modelled on
//! crates/strand-render/tests/morph.rs `Shell::sync` (lines 201-269),
//! generalised from popups to every surface kind.
//!
//! (eval-false-alarms) Placement is strand-surface's own code, not a
//! copy of it, so the driver cannot drift from the surface manager:
//!
//! - A layer surface (`bar`, `panel`, `osd`) is the state
//!   `placement::layer_config_with` sends (`fit` to the screen), sized
//!   and placed as a wlroots compositor arranges it
//!   (`placement::arranged_area`, `LayerConfig::position_in_rect`, the
//!   rules crates/strand-surface/src/manager/origin.rs places it by):
//!   in the area the exclusive zones of the surfaces before it leave, a
//!   stretched side across that area less its margins, at its margins
//!   from the edges it is anchored to, centred in whole pixels on an axis
//!   anchored on both sides or neither. Every shown layer surface is
//!   arranged again after every change, so a centred content-sized panel
//!   that grows is re-centred, and a bar whose zone comes, goes or
//!   changes moves the surfaces arranged past it.
//! - A popup is the state `placement::popup_config` gives it, its box
//!   where its positioner asks (`popup_request`, moved inside the screen
//!   along its anchor's edge by `popup_shift`, as the manager's
//!   `positioner` asks it, from where its parent lies now), relative to
//!   its parent's window geometry. It is placed again when its state
//!   changes (a new size or anchor: the manager repositions it), and it
//!   moves with its parent, as a configured popup does. It waits for its
//!   parent to be shown, and goes with it.
//! - (window) A `window` is the toplevel `placement::window_config`
//!   asks for, configured as the compositor's [`WindowState`] for it
//!   says (a `window` step; by default floating, focused, decorated as
//!   it asks): its box the size given on an axis where
//!   `placement::window_kept_size` takes it (maximised in the area the
//!   bars' exclusive zones leave, fullscreen over the output), else the
//!   size it asks for; a floating one centred in that area when it first
//!   shows, then kept at its top-left as it grows, as a floating window
//!   is. What the configure says is told to render and logic
//!   ([`Change::Window`]) before its size, as the surface manager's
//!   `SurfaceHost::window_state` is.
//!
//! Render is told where each surface lies (`Renderer::set_surface_origin`,
//! as `Host::send_origin` does) whenever that changes, and the trace
//! records each surface's origin per frame, so the checks see a surface's
//! content where the eye does. Not modelled: a compositor's flip or slide
//! of a popup that does not fit even once shifted, a posed parent's
//! scaled anchor (the manager's `posed_rect`), other programs' exclusive
//! zones.

use std::collections::HashMap;

use strand_render::Renderer;
use strand_scene::{
    Damage, Edge, LogicalPoint, NodeId, NodeKind, PaintTarget, Painter, Scale, Size, SurfaceChange,
    SurfaceId, SurfaceSpec,
};
use strand_surface::ToplevelState;
use strand_surface::placement::{self, LayerConfig, PopupConfig, UsableArea, WindowConfig};

use crate::Error;
use crate::scenario::{Screen, ScreenDef};

/// The name the single screen's output has.
pub const OUTPUT: &str = "DP-1";
/// Its `screens.all` id.
pub const SCREEN_ID: &str = "Mock | DP-1";

/// A shown surface.
#[derive(Debug)]
pub struct Shown {
    pub id: SurfaceId,
    pub node: NodeId,
    pub kind: NodeKind,
    /// Its logical size and its buffer's physical one.
    pub logical: [f32; 2],
    pub size: Size,
    /// Premultiplied BGRA.
    pub pixels: Vec<u8>,
    /// Configured since its last paint: the next paints all of it.
    pub fresh: bool,
    /// The tick it was attached at.
    pub attached: u32,
    /// Where its buffer's top-left lies on the output, logical pixels:
    /// (eval-rules-6) at the margins its compositor pose's offset set.
    pub origin: [f32; 2],
    /// (eval-rules-6) Where it lies at rest: at the margins its state
    /// asks for, no pose's offset in them (`origin` when unposed).
    pub rest: [f32; 2],
    /// (eval-rules-6) The offset of the compositor pose the surface
    /// manager last set (`Painter::surface_pose`, set with the frame's
    /// commit), logical pixels.
    pub pose: [f32; 2],
    /// (fidelity) The output it is on (an index of `Surfaces::outputs`):
    /// a layer surface's is the screen its `screens:` names, else the
    /// focused output when it mapped; a popup's its parent's.
    pub output: usize,
    /// How the compositor places it.
    place: Place,
    /// Configured at least once.
    configured: bool,
}

/// What a surface asked the compositor for.
#[derive(Clone, Debug, PartialEq)]
enum Place {
    /// A layer surface's state.
    Layer(LayerConfig),
    /// A popup's positioner state, its parent's node, and its box's
    /// top-left corner relative to its parent's window geometry, as its
    /// configure gave it.
    Popup {
        config: PopupConfig,
        parent: NodeId,
        at: (i32, i32),
    },
    /// (window) A toplevel's state, where its box's top-left lies while
    /// it floats (once first placed), and what its configure last told.
    Window {
        config: WindowConfig,
        float: Option<[i32; 2]>,
        told: Option<ToplevelState>,
    },
}

/// (window) What the sandbox's compositor says of a window in its
/// configure: a `window` step's, or its own answer to `w.maximize()`
/// and `w.fullscreen()`. The default is a new floating window, focused,
/// decorated as it asks. A size given is the box on that axis; with
/// none, a maximised window takes the area the bars' zones leave and a
/// fullscreen one its output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowState {
    /// The box size it gives on each axis (`None`: the window's own).
    pub size: [Option<u32>; 2],
    pub maximized: bool,
    pub fullscreen: bool,
    pub tiled: bool,
    /// Focused (`activated`): its root's `focused`, and the keyboard.
    pub activated: bool,
    /// Whether the compositor draws its decorations (`None`: as its
    /// `decorations:` asks).
    pub decorated: Option<bool>,
}

impl Default for WindowState {
    fn default() -> Self {
        WindowState {
            size: [None, None],
            maximized: false,
            fullscreen: false,
            tiled: false,
            activated: true,
            decorated: None,
        }
    }
}

/// A surface as `Surfaces::arrange` places it: its id, its size, where
/// it lies and where it lies at rest (without its pose's offset).
type Arranged = (SurfaceId, [u32; 2], [f32; 2], [f32; 2]);

/// What the surface manager does to a surface (`Surfaces::change`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Attached(SurfaceId, NodeId),
    /// Configured at this buffer size and scale (after `Attached` for a
    /// new surface; again when its size or the scale changes).
    Configured(SurfaceId, Size, Scale),
    /// (eval-false-alarms) Moved on the output at the same size: a
    /// layer surface arranged anew (another surface's exclusive zone
    /// changed), a popup repositioned at its size or moved with its
    /// parent. Render is told its new origin.
    Placed(SurfaceId),
    /// (window) A window's configure says something new (its states, the
    /// axes the compositor sizes): told to render and logic before its
    /// `Configured` (`Surfaces::told`).
    Window(SurfaceId),
    Detached(SurfaceId),
}

/// (fidelity) One output: its screen and who it is (`screens.all`'s
/// name and id).
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub screen: Screen,
    pub name: String,
    pub id: String,
}

/// The shown surfaces.
#[derive(Debug)]
pub struct Surfaces {
    pub shown: Vec<Shown>,
    next: u32,
    specs: HashMap<NodeId, SurfaceSpec>,
    /// The main output's screen (`outputs[main]`'s, kept in step).
    pub screen: Screen,
    /// (fidelity) Every output, in `screens.all`'s order.
    pub outputs: Vec<Output>,
    /// The main output's index.
    pub main: usize,
    /// (fidelity) The compositor's focused output (a `screen` step
    /// moves it): where a surface no screen pins maps.
    pub focused: usize,
    /// (eval-surface-leg) With the surface leg, every change the renderer
    /// reported, as it reported it, for the real surface manager.
    pub tee: Option<Vec<(NodeId, SurfaceChange)>>,
    /// (window) The compositor's state of each window, by its node: kept
    /// while it is shown (or before it shows, a replay's configure that
    /// came first), forgotten when it goes.
    windows: HashMap<NodeId, WindowState>,
}

impl Surfaces {
    /// One output, the mock's.
    pub fn new(screen: Screen) -> Surfaces {
        Surfaces::of(
            vec![Output {
                screen,
                name: OUTPUT.into(),
                id: SCREEN_ID.into(),
            }],
            0,
        )
    }

    /// (fidelity) `outputs` (one at least), `main` the main one's index.
    pub fn of(outputs: Vec<Output>, main: usize) -> Surfaces {
        let main = main.min(outputs.len().saturating_sub(1));
        Surfaces {
            shown: Vec::new(),
            next: 1,
            specs: HashMap::new(),
            screen: outputs.get(main).map_or(
                Screen {
                    width: 300,
                    height: 1080,
                    scale: 1.0,
                },
                |o| o.screen,
            ),
            outputs,
            main,
            focused: 0,
            tee: None,
            windows: HashMap::new(),
        }
    }

    /// (fidelity) A scenario's screens as outputs.
    pub fn of_screens(screens: &[ScreenDef], main: usize) -> Surfaces {
        Surfaces::of(
            screens
                .iter()
                .map(|d| Output {
                    screen: d.screen,
                    name: d.identity.name.clone(),
                    id: d.identity.id.clone(),
                })
                .collect(),
            main,
        )
    }

    /// The main output's scale.
    pub fn scale(&self) -> Result<Scale, Error> {
        scale_of(self.screen.scale)
    }

    /// (fidelity) The main output's scale set to `s` (a `scale` step).
    pub fn set_main_scale(&mut self, s: f64) {
        self.screen.scale = s;
        if let Some(o) = self.outputs.get_mut(self.main) {
            o.screen.scale = s;
        }
    }

    /// (fidelity) Output `i`'s screen (the main one's past the end).
    pub fn screen_of(&self, i: usize) -> Screen {
        if i == self.main {
            return self.screen;
        }
        self.outputs.get(i).map_or(self.screen, |o| o.screen)
    }

    /// (fidelity) Output `i`'s name.
    pub fn output_name(&self, i: usize) -> &str {
        self.outputs.get(i).map_or(OUTPUT, |o| o.name.as_str())
    }

    /// (fidelity) Whether there is more than one output: only then does
    /// a trace say which output each surface is on.
    pub fn several(&self) -> bool {
        self.outputs.len() > 1
    }

    /// (fidelity) Surface `id`'s output's scale, as Wayland has it.
    pub fn scale_for(&self, id: SurfaceId) -> Result<Scale, Error> {
        scale_of(self.scale_of_surface(id))
    }

    /// (fidelity) Surface `id`'s output's scale.
    pub fn scale_of_surface(&self, id: SurfaceId) -> f64 {
        self.get(id)
            .map_or(self.screen.scale, |s| self.screen_of(s.output).scale)
    }

    /// (eval-rules-4) Surface `id`'s placement as its spec declares it
    /// (`checks::placement` judges where it lies against it): the
    /// props, the parent surface a popup opens on and the element it
    /// opens from, never the config the surface manager made of them.
    pub fn declared(&self, id: SurfaceId, k: u32) -> Option<(NodeId, crate::trace::PlaceRec)> {
        let sh = self.get(id)?;
        let spec = self.specs.get(&sh.node)?;
        let parent = match &sh.place {
            Place::Popup { parent, .. } => self
                .shown
                .iter()
                .find(|s| s.node == *parent)
                .map(|s| s.id.0),
            Place::Layer(_) | Place::Window { .. } => None,
        };
        let m = spec.margin;
        Some((
            sh.node,
            crate::trace::PlaceRec {
                k,
                layer: spec.layer.map(|l| l.name().to_string()),
                edge: spec.edge.map(|e| e.name().to_string()),
                anchor: spec.anchor.name().to_string(),
                margin: [m.top, m.right, m.bottom, m.left],
                margin_set: spec.margin_set,
                attach: spec.attach.map(|e| e.name().to_string()),
                tooltip: spec.tooltip,
                parent,
                from: None,
                size: [spec.width, spec.height],
            },
        ))
    }

    /// (report) How surface `id` takes the keyboard (its spec's
    /// `keyboard:`; `none` for a surface not shown).
    pub fn keyboard(&self, id: SurfaceId) -> strand_scene::Keyboard {
        self.get(id).and_then(|sh| self.specs.get(&sh.node)).map_or(
            strand_scene::Keyboard::None,
            |s| match s.kind {
                // (window) A press focuses a window, as a compositor does.
                NodeKind::Window => strand_scene::Keyboard::OnDemand,
                _ => s.keyboard,
            },
        )
    }

    /// (window) The compositor's state of `node`'s window (its default
    /// before any was set).
    pub fn window_state(&self, node: NodeId) -> WindowState {
        self.windows.get(&node).copied().unwrap_or_default()
    }

    /// (window) The compositor configures `node`'s window as `state`
    /// says (a `window` step, or its answer to an ask): kept for it, and
    /// the window, if shown, sized and placed so.
    pub fn set_window(&mut self, node: NodeId, state: WindowState) -> Result<Vec<Change>, Error> {
        self.windows.insert(node, state);
        let mut out = Vec::new();
        self.arrange(&mut out)?;
        Ok(out)
    }

    /// (window) Window `id`'s node and what its last configure told
    /// (`None`: not a window, or not configured).
    pub fn told(&self, id: SurfaceId) -> Option<(NodeId, ToplevelState)> {
        let sh = self.get(id)?;
        match &sh.place {
            Place::Window { told, .. } => Some((sh.node, (*told)?)),
            _ => None,
        }
    }

    /// The surface `id`.
    pub fn get(&self, id: SurfaceId) -> Option<&Shown> {
        self.shown.iter().find(|s| s.id == id)
    }

    /// The main screen's logical size, as the output describes it.
    fn full(&self) -> (u32, u32) {
        self.full_of(self.main)
    }

    /// (fidelity) Output `i`'s logical size.
    fn full_of(&self, i: usize) -> (u32, u32) {
        let s = self.screen_of(i);
        (s.width.max(1), s.height.max(1))
    }

    /// (fidelity) The output a layer surface of `spec` maps on: the
    /// first of the screens its `screens:` names (by id or connector, as
    /// the surface manager's `wants` matches them), else the focused
    /// output; with one output, that one whatever it names (as before
    /// there were several).
    fn output_for(&self, spec: &SurfaceSpec) -> usize {
        if self.outputs.len() <= 1 {
            return 0;
        }
        if let strand_scene::Screens::Named(names) = &spec.screens
            && let Some(i) = self
                .outputs
                .iter()
                .position(|o| names.iter().any(|n| *n == o.id || *n == o.name))
        {
            return i;
        }
        self.focused.min(self.outputs.len() - 1)
    }

    /// (eval-false-alarms) A shown layer surface whose configured size is
    /// not the one it would have over the whole screen: another surface's
    /// exclusive zone sized it (a left bar under a top bar's zone). The
    /// surface leg's fake compositor arranges no zones, so it would
    /// configure that surface at another size than the one painted.
    pub fn sized_by_a_zone(&self) -> Option<SurfaceId> {
        let full = UsableArea::full(self.full());
        self.shown.iter().find_map(|s| match &s.place {
            Place::Layer(c) if s.configured => {
                let (w, h) = configured_size(c, full);
                (s.logical != [w as f32, h as f32]).then_some(s.id)
            }
            _ => None,
        })
    }

    /// True if a popup nested in layer-surface node `node` declares a
    /// scrim, open or not (the manager's `has_nested_scrim`): such a
    /// surface rises a layer (`placement::layer_config_with`).
    fn has_nested_scrim(&self, node: NodeId) -> bool {
        self.specs.values().any(|s| {
            s.kind == NodeKind::Popup && s.scrim.is_some() && self.root_node(s) == Some(node)
        })
    }

    /// The layer-surface node a popup spec is nested in (the manager's
    /// `root_node`).
    fn root_node(&self, spec: &SurfaceSpec) -> Option<NodeId> {
        let mut node = spec.parent?;
        for _ in 0..64 {
            let s = self.specs.get(&node)?;
            if s.kind != NodeKind::Popup {
                return Some(node);
            }
            node = s.parent?;
        }
        None
    }

    /// What the surface manager does with one of the renderer's surface
    /// changes (`State::apply_surface_change`): the surfaces to attach,
    /// configure, place and detach, in order, for the driver to run
    /// through the host loop (`strand_render::host_loop::{attached,
    /// configured, detached}`). A surface it attaches is shown from now
    /// on. Every shown surface is arranged again after it, as the
    /// compositor arranges an output's layer surfaces when one of them
    /// changes.
    pub fn change(
        &mut self,
        node: NodeId,
        change: SurfaceChange,
        k: u32,
    ) -> Result<Vec<Change>, Error> {
        let mut out = Vec::new();
        if let Some(tee) = self.tee.as_mut() {
            tee.push((node, change.clone()));
        }
        match change {
            SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => {
                self.specs.insert(node, s);
            }
            SurfaceChange::Removed => {
                self.specs.remove(&node);
            }
        }
        self.reconcile(node, k, &mut out)?;
        // Arranged before its popups ask where it lies.
        self.arrange(&mut out)?;
        // The popups that waited for this surface (the manager's
        // `reconcile_children`), in a fixed order.
        let mut kids: Vec<NodeId> = self
            .specs
            .iter()
            .filter(|(n, s)| {
                s.kind == NodeKind::Popup
                    && s.parent == Some(node)
                    && s.open
                    && !self.shown.iter().any(|x| x.node == **n)
            })
            .map(|(n, _)| *n)
            .collect();
        kids.sort();
        if !kids.is_empty() {
            for kid in kids {
                self.reconcile(kid, k, &mut out)?;
            }
            self.arrange(&mut out)?;
        }
        Ok(out)
    }

    /// Shows, keeps or hides `node`'s surface for its spec now, with the
    /// state it asks the compositor for; `arrange` sizes and places it.
    fn reconcile(&mut self, node: NodeId, k: u32, out: &mut Vec<Change>) -> Result<(), Error> {
        let at = self.shown.iter().position(|s| s.node == node);
        let Some(spec) = self.specs.get(&node).filter(|s| s.open).cloned() else {
            self.detach(node, out);
            return Ok(());
        };
        let place = match spec.kind {
            NodeKind::Lock => {
                return Err(Error::Run(
                    "a `lock` surface runs only in the lock VM (scripts/container/lockvm.sh), \
                     never in the sandbox"
                        .into(),
                ));
            }
            NodeKind::Popup => {
                // It waits for its parent to be shown, and for its size
                // and anchor (`AutoSize`).
                let Some(parent) = spec.parent else {
                    return Ok(());
                };
                let (Some(ps), Some(p)) = (
                    self.specs.get(&parent),
                    self.shown.iter().find(|s| s.node == parent),
                ) else {
                    return Ok(());
                };
                let Ok(config) = placement::popup_config(&spec, ps) else {
                    return Ok(());
                };
                if let Some(Place::Popup { config: had, .. }) = at.map(|i| &self.shown[i].place)
                    && *had == config
                {
                    return Ok(());
                }
                Place::Popup {
                    at: self.popup_at(&config, p),
                    config,
                    parent,
                }
            }
            NodeKind::Window => {
                // It waits for a size, as the manager's
                // `reconcile_window` does.
                let Ok(config) = placement::window_config(&spec) else {
                    return Ok(());
                };
                let (float, told) = match at.map(|i| &self.shown[i].place) {
                    Some(Place::Window { float, told, .. }) => (*float, *told),
                    _ => (None, None),
                };
                Place::Window {
                    config,
                    float,
                    told,
                }
            }
            _ => {
                let Ok(mut config) =
                    placement::layer_config_with(&spec, self.has_nested_scrim(node))
                else {
                    // No size yet, or not a layer surface: none (yet),
                    // as the manager's `create_surface` leaves it.
                    return Ok(());
                };
                // (fidelity) Fitted to the output it is on, or will map on.
                let on = at.map_or_else(|| self.output_for(&spec), |i| self.shown[i].output);
                let (w, h) = self.full_of(on);
                config.fit((w as i32, h as i32));
                Place::Layer(config)
            }
        };
        // (fidelity) A popup is on its parent's output.
        let output = match &place {
            Place::Popup { parent, .. } => self
                .shown
                .iter()
                .find(|s| s.node == *parent)
                .map_or(0, |p| p.output),
            Place::Layer(_) | Place::Window { .. } => {
                at.map_or_else(|| self.output_for(&spec), |i| self.shown[i].output)
            }
        };
        // (window) A new window takes the compositor's focus from the
        // others, unless a configure already said otherwise of it.
        if at.is_none() && spec.kind == NodeKind::Window && !self.windows.contains_key(&node) {
            let others: Vec<NodeId> = self
                .shown
                .iter()
                .filter(|s| matches!(s.place, Place::Window { .. }))
                .map(|s| s.node)
                .collect();
            for n in others {
                self.windows.entry(n).or_default().activated = false;
            }
        }
        match at {
            Some(i) => self.shown[i].place = place,
            None => {
                let id = SurfaceId(self.next);
                self.next += 1;
                self.shown.push(Shown {
                    id,
                    node,
                    kind: spec.kind,
                    logical: [0.0, 0.0],
                    size: Size::new(1, 1),
                    pixels: Vec::new(),
                    fresh: true,
                    attached: k,
                    origin: [0.0, 0.0],
                    rest: [0.0, 0.0],
                    pose: [0.0, 0.0],
                    output,
                    place,
                    configured: false,
                });
                out.push(Change::Attached(id, node));
            }
        }
        Ok(())
    }

    /// Hides `node`'s surface, and the popups nested in it before it (a
    /// popup goes before its parent, as the manager destroys them).
    fn detach(&mut self, node: NodeId, out: &mut Vec<Change>) {
        let kids: Vec<NodeId> = self
            .shown
            .iter()
            .filter(|s| matches!(&s.place, Place::Popup { parent, .. } if *parent == node))
            .map(|s| s.node)
            .collect();
        for kid in kids {
            self.detach(kid, out);
        }
        if let Some(i) = self.shown.iter().position(|s| s.node == node) {
            let gone = self.shown.remove(i);
            if matches!(gone.place, Place::Window { .. }) {
                self.windows.remove(&node);
            }
            out.push(Change::Detached(gone.id));
        }
    }

    /// Where popup `c`'s box lands relative to its parent `p`'s window
    /// geometry: as the manager's `positioner` asks for it, moved inside
    /// the screen along its anchor's edge from where its parent lies.
    fn popup_at(&self, c: &PopupConfig, p: &Shown) -> (i32, i32) {
        let (gx, gy) = geometry(p);
        let (sw, sh) = self.full_of(p.output);
        let within = (
            -(whole(p.origin[0]) + gx),
            -(whole(p.origin[1]) + gy),
            sw as i32,
            sh as i32,
        );
        let rect = c.anchor_rect;
        let shift = placement::popup_shift(c, rect, within);
        let (x, y, _, _) = placement::popup_request(c, rect, shift);
        (x, y)
    }

    /// Arranges every shown surface as the compositor does after a
    /// change: each layer surface sized and placed in the area the
    /// exclusive zones before it leave, then each popup where its
    /// configure put it on its parent. A surface given a new size is
    /// configured at it; one only moved is placed.
    fn arrange(&mut self, out: &mut Vec<Change>) -> Result<(), Error> {
        let mut scales = Vec::with_capacity(self.outputs.len());
        for i in 0..self.outputs.len().max(1) {
            scales.push(scale_of(self.screen_of(i).scale)?);
        }
        let mut placed: Vec<Arranged> = Vec::new();
        // (fidelity) Each output's layer surfaces in the area its own
        // exclusive zones leave.
        for o in 0..scales.len() {
            self.arrange_layers(o, &mut placed);
        }
        let told = self.arrange_windows(&mut placed);
        self.arrange_popups(&mut placed);
        for (id, size, origin, rest) in placed {
            // (window) What its configure says, before its size.
            if told.contains(&id) {
                out.push(Change::Window(id));
            }
            let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
                continue;
            };
            let scale = scales.get(sh.output).copied().unwrap_or(scales[0]);
            let logical = [size[0] as f32, size[1] as f32];
            let moved = sh.origin != origin;
            sh.origin = origin;
            sh.rest = rest;
            if !sh.configured || sh.logical != logical {
                let size = physical(logical, scale);
                sh.configured = true;
                sh.logical = logical;
                sh.size = size;
                sh.pixels = vec![0; (size.w * size.h * 4) as usize];
                sh.fresh = true;
                out.push(Change::Configured(id, size, scale));
            } else if moved {
                out.push(Change::Placed(id));
            }
        }
        Ok(())
    }

    /// (fidelity) Output `o`'s layer surfaces, sized and placed.
    fn arrange_layers(&self, o: usize, placed: &mut Vec<Arranged>) {
        let full = self.full_of(o);
        let layers: Vec<(SurfaceId, LayerConfig)> = self
            .shown
            .iter()
            .filter(|s| s.output == o)
            .filter_map(|s| match &s.place {
                Place::Layer(c) => Some((s.id, c.clone())),
                Place::Popup { .. } | Place::Window { .. } => None,
            })
            .collect();
        let list: Vec<(SurfaceId, &LayerConfig)> = layers.iter().map(|(i, c)| (*i, c)).collect();
        for (id, c) in &layers {
            let area = placement::arranged_area(full, &list, id);
            let size = configured_size(c, area);
            let (x, y) = c.position_in_rect(size, area);
            // (eval-rules-6) At the margins the manager sends for its
            // compositor pose's offset (`State::sync_pose`, through
            // `placement::posed_margin`), as the compositor places it.
            let pose = self
                .shown
                .iter()
                .find(|s| s.id == *id)
                .map_or([0.0, 0.0], |s| s.pose);
            let mut posed = c.clone();
            posed.margin = placement::posed_margin(c, LogicalPoint::new(pose[0], pose[1]));
            let (px, py) = posed.position_in_rect(size, area);
            placed.push((
                *id,
                [size.0, size.1],
                [px as f32, py as f32],
                [x as f32, y as f32],
            ));
        }
    }

    /// (window) The windows, each configured as its compositor state
    /// says (see the module docs); the ids whose configure tells
    /// something new.
    fn arrange_windows(&mut self, placed: &mut Vec<Arranged>) -> Vec<SurfaceId> {
        let mut told_now = Vec::new();
        for i in 0..self.shown.len() {
            let Place::Window { config, float, .. } = &self.shown[i].place else {
                continue;
            };
            let (config, float) = (config.clone(), *float);
            let sh = &self.shown[i];
            let st = self.window_state(sh.node);
            let full = self.full_of(sh.output);
            // The area the bars' exclusive zones leave: a window is
            // arranged after every one of them.
            let layers: Vec<(SurfaceId, LayerConfig)> = self
                .shown
                .iter()
                .filter(|s| s.output == sh.output)
                .filter_map(|s| match &s.place {
                    Place::Layer(c) => Some((s.id, c.clone())),
                    _ => None,
                })
                .chain(std::iter::once((sh.id, config.as_layer())))
                .collect();
            let list: Vec<(SurfaceId, &LayerConfig)> =
                layers.iter().map(|(i, c)| (*i, c)).collect();
            let area = placement::arranged_area(full, &list, &sh.id);
            // A size given wins (a replay's, as the live compositor gave
            // it); else a fullscreen window fills its output and a
            // maximised one the area the zones leave.
            let given = match st.size {
                [None, None] if st.fullscreen => [Some(full.0), Some(full.1)],
                [None, None] if st.maximized => [Some(area.w), Some(area.h)],
                size => size,
            };
            let constrained = st.maximized || st.fullscreen || st.tiled;
            let (sw, w) = placement::window_kept_size(given[0], config.width, constrained);
            let (shh, h) = placement::window_kept_size(given[1], config.height, constrained);
            let at = if st.fullscreen {
                [0, 0]
            } else if st.maximized || st.tiled {
                [area.x, area.y]
            } else {
                float.unwrap_or([
                    area.x + ((area.w as i32 - w as i32) / 2).max(0),
                    area.y + ((area.h as i32 - h as i32) / 2).max(0),
                ])
            };
            let state = ToplevelState {
                sized: (sw, shh),
                size: (w, h),
                maximized: st.maximized,
                fullscreen: st.fullscreen,
                tiled: st.tiled,
                activated: st.activated,
                decorated: st
                    .decorated
                    .unwrap_or(config.decorations == strand_scene::Decorations::Server),
                bounds: Some((area.w, area.h)),
                window_menu: true,
                can_maximize: true,
                can_fullscreen: true,
                can_minimize: true,
            };
            let [t, _, _, l] = config.overhang;
            let (bw, bh) = config.buffer((w, h));
            let o = [(at[0] - l) as f32, (at[1] - t) as f32];
            placed.push((sh.id, [bw.max(1), bh.max(1)], o, o));
            let id = sh.id;
            let floating = !(st.fullscreen || st.maximized || st.tiled);
            if let Place::Window { float, told, .. } = &mut self.shown[i].place {
                if floating {
                    *float = Some(at);
                }
                if *told != Some(state) {
                    *told = Some(state);
                    told_now.push(id);
                }
            }
        }
        told_now
    }

    /// The popups, each where its configure put it on its parent.
    fn arrange_popups(&self, placed: &mut Vec<Arranged>) {
        // Popups in the order they were shown: a parent before its
        // popups.
        for i in 0..self.shown.len() {
            let Place::Popup { config, parent, at } = &self.shown[i].place else {
                continue;
            };
            let Some(p) = self.shown.iter().find(|s| s.node == *parent) else {
                continue;
            };
            let parent_at = placed
                .iter()
                .find(|(id, _, _, _)| *id == p.id)
                .map_or(p.origin, |(_, _, o, _)| *o);
            let (gx, gy) = geometry(p);
            let [t, _, _, l] = config.overhang;
            let (w, h) = config.buffer();
            let o = [
                parent_at[0] + (gx + at.0 - l) as f32,
                parent_at[1] + (gy + at.1 - t) as f32,
            ];
            // A popup of this popup reads where it lies now, from here.
            placed.push((self.shown[i].id, [w.max(1), h.max(1)], o, o));
        }
    }

    /// (eval-rules-6) Surface `id`'s frame commits with a compositor
    /// pose of `offset` (`None`: none): a layer surface's margins move by
    /// it, as the manager sets them (`State::sync_pose`), and it and its
    /// popups are placed again where the compositor puts them; a popup
    /// takes no offset (the manager sends none). The surfaces placed anew.
    pub fn set_pose(
        &mut self,
        id: SurfaceId,
        offset: Option<[f32; 2]>,
    ) -> Result<Vec<Change>, Error> {
        let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
            return Ok(Vec::new());
        };
        let offset = match sh.place {
            Place::Layer(_) => offset.unwrap_or([0.0, 0.0]),
            Place::Popup { .. } | Place::Window { .. } => [0.0, 0.0],
        };
        if sh.pose == offset {
            return Ok(Vec::new());
        }
        sh.pose = offset;
        let mut out = Vec::new();
        self.arrange(&mut out)?;
        Ok(out)
    }

    /// (eval-rules-6) The state layer surface `id` asks for at rest (its
    /// margins without a pose's offset), for the surface leg.
    pub fn layer_config(&self, id: SurfaceId) -> Option<&LayerConfig> {
        match &self.get(id)?.place {
            Place::Layer(c) => Some(c),
            Place::Popup { .. } | Place::Window { .. } => None,
        }
    }

    /// Where surface `id` lies, for render (`Renderer::set_surface_origin`,
    /// as `Host::send_origin` sends it): a layer surface on the one
    /// output, a popup on its parent's.
    pub fn origin(&self, id: SurfaceId) -> Option<(Option<String>, LogicalPoint)> {
        let sh = self.get(id)?;
        let output = (sh.kind != NodeKind::Popup).then(|| self.output_name(sh.output).to_string());
        Some((output, LogicalPoint::new(sh.origin[0], sh.origin[1])))
    }

    /// (eval-rules-2) The sides of surface `id`'s buffer a shadow may be
    /// cut by, as bits (1 top, 2 right, 4 bottom, 8 left): a side that
    /// lies on or past the output's edge, where the compositor's placement
    /// (`arrange`) put it (past it nothing shows: render keeps an anchored
    /// side's overhang within its margin, decisions.md m6-w2-surface), an
    /// anchored side on or past the edge of the area other surfaces'
    /// exclusive zones leave it (eval-rules-7: its margin counts from
    /// there and is never negative), and an `attach:` side, past which
    /// there is no overhang (m4-surface-w1).
    /// Read from the modelled buffer, not from render's own caps, so a
    /// side render cuts that is not at the edge stays a finding
    /// (integration-r4).
    pub fn cuts(&self, id: SurfaceId) -> u8 {
        let Some(sh) = self.get(id) else {
            return 0;
        };
        let (sw, shh) = self.full_of(sh.output);
        let [x, y] = sh.rest;
        let [w, h] = sh.logical;
        let at = |gap: f32| gap <= 0.5;
        // (eval-rules-7) A layer surface's margin on an edge it is
        // anchored to counts from the edge of the area the exclusive
        // zones arranged before it leave (layer shell: "the exclusive
        // zone includes the margin"; spec.md 5.1), and an anchored side's
        // margin is never negative (m6-w2-surface), so on such a side
        // its buffer ends at that area's edge at the furthest: a panel
        // anchored below a bar's zone has its shadow cut where the zone
        // begins (set I's media card), as one at the output's edge has.
        let area = match &sh.place {
            Place::Layer(c) => {
                let layers: Vec<(SurfaceId, &LayerConfig)> = self
                    .shown
                    .iter()
                    .filter(|s| s.output == sh.output)
                    .filter_map(|s| match &s.place {
                        Place::Layer(c) => Some((s.id, c)),
                        Place::Popup { .. } | Place::Window { .. } => None,
                    })
                    .collect();
                let a = placement::arranged_area(self.full_of(sh.output), &layers, &id);
                let edges = c.anchors;
                Some((a, edges))
            }
            Place::Popup { .. } | Place::Window { .. } => None,
        };
        let zone = |side: u8| {
            area.is_some_and(|(a, e)| {
                let (ax, ay) = (a.x as f32, a.y as f32);
                let (ar, ab) = (ax + a.w as f32, ay + a.h as f32);
                match side {
                    1 => e.top && at(y - ay),
                    2 => e.right && at(ar - (x + w)),
                    4 => e.bottom && at(ab - (y + h)),
                    _ => e.left && at(x - ax),
                }
            })
        };
        let mut bits = 0;
        if at(y) || zone(1) {
            bits |= 1;
        }
        if at(sw as f32 - (x + w)) || zone(2) {
            bits |= 2;
        }
        if at(shh as f32 - (y + h)) || zone(4) {
            bits |= 4;
        }
        if at(x) || zone(8) {
            bits |= 8;
        }
        let attach = self.specs.get(&sh.node).and_then(|s| s.attach);
        bits | attach.map_or(0, |e| match e {
            Edge::Top => 1,
            Edge::Right => 2,
            Edge::Bottom => 4,
            Edge::Left => 8,
        })
    }

    /// Every shown surface on the main screen at its (new) scale: the
    /// surfaces to configure again.
    pub fn rescale(&mut self) -> Result<Vec<Change>, Error> {
        let scale = self.scale()?;
        let main = self.main;
        let mut out = Vec::new();
        for sh in self.shown.iter_mut().filter(|s| s.output == main) {
            let size = physical(sh.logical, scale);
            sh.size = size;
            sh.pixels = vec![0; (size.w * size.h * 4) as usize];
            sh.fresh = true;
            out.push(Change::Configured(sh.id, size, scale));
        }
        Ok(out)
    }

    /// Paints surface `id`'s frame presented at `time`; its damage.
    pub fn paint(
        &mut self,
        r: &mut Renderer,
        id: SurfaceId,
        time: std::time::Duration,
    ) -> Result<Damage, Error> {
        let scale = scale_of(self.scale_of_surface(id))?;
        let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
            return Ok(Damage::new());
        };
        let age = if sh.fresh { 0 } else { 1 };
        sh.fresh = false;
        let t = PaintTarget::new(&mut sh.pixels, sh.size, sh.size.w * 4, scale, age)
            .map_err(|e| Error::Run(format!("surface {}: {e:?}", id.0)))?;
        let mut t = t.at(time);
        Ok(r.paint(id, &mut t))
    }

    /// Paints surface `id`'s frame presented at `time` on the GPU leg:
    /// the frame `Renderer::paint_gpu` lowers, drawn by `dev` into the
    /// surface's pixels. Whether there was one.
    pub fn paint_gpu(
        &mut self,
        r: &mut Renderer,
        dev: &mut crate::gpu::Device,
        id: SurfaceId,
        time: std::time::Duration,
    ) -> Result<bool, Error> {
        let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        sh.fresh = false;
        let Some(frame) = r.paint_gpu(id, time) else {
            return Ok(false);
        };
        if frame.size != sh.size {
            return Err(Error::Run(format!(
                "surface {}: a GPU frame of {}x{} for a {}x{} buffer",
                id.0, frame.size.w, frame.size.h, sh.size.w, sh.size.h
            )));
        }
        dev.draw(frame, &mut sh.pixels, sh.size)?;
        Ok(true)
    }
}

/// A scale Wayland has, else why not.
fn scale_of(s: f64) -> Result<Scale, Error> {
    Scale::from_f64(s).ok_or_else(|| Error::Run(format!("scale {s} is not one Wayland has")))
}

/// The size a compositor configures a layer surface of `c` at in `area`
/// (logical px): what it asked for, and on a side it stretches (0)
/// across the area less its margins (wlroots'
/// `wlr_scene_layer_surface_v1_configure`).
fn configured_size(c: &LayerConfig, area: UsableArea) -> (u32, u32) {
    let [mt, mr, mb, ml] = c.margin;
    let stretch = |asked: u32, room: u32, a: i32, b: i32| -> u32 {
        if asked != 0 {
            return asked;
        }
        (i64::from(room) - i64::from(a) - i64::from(b)).clamp(1, i64::from(u32::MAX)) as u32
    };
    (
        stretch(c.width, area.w, ml, mr),
        stretch(c.height, area.h, mt, mb),
    )
}

/// A surface's window geometry inside its buffer: a popup's is its box,
/// inside its overhang; a layer surface's is its whole buffer.
fn geometry(s: &Shown) -> (i32, i32) {
    match &s.place {
        Place::Popup { config, .. } => (config.overhang[3], config.overhang[0]),
        // (window) A window's is its box too.
        Place::Window { config, .. } => (config.overhang[3], config.overhang[0]),
        Place::Layer(_) => (0, 0),
    }
}

/// A whole logical pixel (origins are whole: the compositor places
/// boxes on its integer grid).
fn whole(v: f32) -> i32 {
    if v.is_finite() { v.round() as i32 } else { 0 }
}

/// The physical size of a logical one at `scale`: its buffer's, as the
/// surface manager sizes it (manager/mod.rs `buffer_size`: at a
/// fractional scale rounded, half away from zero, as
/// `wp_fractional_scale_v1` says; at an integer one the logical size
/// times the scale). (eval-coverage-2: it was rounded up, so at 1.25 a
/// 221 px surface had a 277 px buffer, the renderer read it back as
/// 221.6 px, and a centred shrink was placed a pixel off what the
/// compositor did.)
fn physical(logical: [f32; 2], scale: Scale) -> Size {
    let s = scale.physical_size(strand_scene::LogicalSize::new(logical[0], logical[1]));
    Size::new(s.w.max(1), s.h.max(1))
}

/// Straight-alpha RGBA of a premultiplied BGRA buffer (for PNG), as
/// crates/strand-render/tests/common/mod.rs `Buffer::to_rgba` makes it.
pub fn to_rgba(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for p in pixels.chunks_exact(4) {
        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
        out.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::{Prop, PropValue};

    fn spec(kind: NodeKind, props: &[(Prop, PropValue)]) -> SurfaceSpec {
        let map: HashMap<Prop, PropValue> = props.iter().cloned().collect();
        SurfaceSpec::resolve(kind, |p| map.get(&p))
    }

    fn node(index: u32) -> NodeId {
        NodeId {
            index,
            generation: 0,
        }
    }

    /// (eval-coverage-2) A buffer is the configured size at the scale as
    /// the surface manager sizes it: rounded half away from zero at a
    /// fractional scale (221 px at 1.25 is 276 px, not 277), times the
    /// scale at an integer one.
    #[test]
    fn a_buffer_is_sized_as_the_surface_manager_sizes_it() {
        let s = |f: f64| Scale::from_f64(f).unwrap();
        assert_eq!(physical([221.0, 60.0], s(1.25)), Size::new(276, 75));
        assert_eq!(physical([221.0, 60.0], s(1.5)), Size::new(332, 90));
        assert_eq!(physical([221.0, 60.0], s(2.0)), Size::new(442, 120));
        assert_eq!(physical([0.2, 0.2], s(1.0)), Size::new(1, 1));
    }

    /// (eval-coverage-2) A panel anchored to the top edge only is centred
    /// across as wlroots arranges it (`area / 2 - size / 2`, whole
    /// pixels: 221 px on a 400 px screen at 90, 150 px at 125), and a top
    /// bar's exclusive zone shown before it moves it down.
    #[test]
    fn a_centred_panel_lies_where_the_compositor_puts_it() {
        let mut s = Surfaces::new(Screen {
            width: 400,
            height: 300,
            scale: 1.0,
        });
        let kw = |k: &str| PropValue::Keyword(k.into());
        let panel = |w: f32| {
            spec(
                NodeKind::Panel,
                &[
                    (Prop::Anchor, kw("top")),
                    (Prop::Width, PropValue::Number(w)),
                    (Prop::Height, PropValue::Number(60.0)),
                ],
            )
        };
        let p = node(2);
        let at = |s: &Surfaces| s.shown.iter().find(|x| x.node == p).unwrap().origin;
        s.change(p, SurfaceChange::Created(panel(221.0)), 0)
            .unwrap();
        assert_eq!(at(&s), [90.0, 0.0]);
        s.change(
            p,
            SurfaceChange::Updated {
                spec: panel(150.0),
                recreate: false,
            },
            1,
        )
        .unwrap();
        assert_eq!(at(&s), [125.0, 0.0]);
        // A top bar's zone, shown from here on, moves it down; the panel
        // made first is arranged first, so the bar is made first here.
        let mut s = Surfaces::new(s.screen);
        let b = node(1);
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(30.0))]);
        s.change(b, SurfaceChange::Created(bar.clone()), 0).unwrap();
        s.change(p, SurfaceChange::Created(panel(150.0)), 0)
            .unwrap();
        assert_eq!(at(&s), [125.0, 30.0]);
        // (integration-r4) The cut sides are where the buffer meets the
        // output's edge: the bar's top, left and right. (eval-rules-7)
        // And an anchored side where it meets the edge of the area the
        // zones before it leave: the panel's top, under the bar's zone,
        // which its margin counts from; none of its other sides.
        let id = |n: NodeId| s.shown.iter().find(|x| x.node == n).unwrap().id;
        assert_eq!(s.cuts(id(b)), 1 | 2 | 8);
        assert_eq!(s.cuts(id(p)), 1);
        // A margin keeps it off that edge: nothing cuts it there.
        let mut s2 = Surfaces::new(s.screen);
        let spaced = spec(
            NodeKind::Panel,
            &[
                (Prop::Anchor, kw("top")),
                (Prop::Width, PropValue::Number(150.0)),
                (Prop::Height, PropValue::Number(60.0)),
                (Prop::Margin, PropValue::Number(8.0)),
            ],
        );
        s2.change(b, SurfaceChange::Created(bar), 0).unwrap();
        s2.change(p, SurfaceChange::Created(spaced), 0).unwrap();
        assert_eq!(at(&s2), [125.0, 38.0]);
        let spaced_id = s2.shown.iter().find(|x| x.node == p).unwrap().id;
        assert_eq!(s2.cuts(spaced_id), 0);
    }

    /// (window) The sandbox's compositor configures a window: floating
    /// at the size it asks for, centred in the area a bar's zone leaves
    /// and kept at its top-left as it grows; a size given wins on its
    /// axis; maximised it takes that area, fullscreen the output; and
    /// each configure that says something new is told before its size.
    /// A second window takes the focus from the first.
    #[test]
    fn a_window_is_configured_as_its_compositor_says() {
        let mut s = Surfaces::new(Screen {
            width: 400,
            height: 300,
            scale: 1.0,
        });
        let window = |w: f32, h: f32| {
            let mut sp = spec(NodeKind::Window, &[]);
            (sp.width, sp.height) = (Some(w), Some(h));
            sp
        };
        let (b, w) = (node(1), node(2));
        let bar = spec(NodeKind::Bar, &[(Prop::Height, PropValue::Number(30.0))]);
        s.change(b, SurfaceChange::Created(bar), 0).unwrap();
        let out = s
            .change(w, SurfaceChange::Created(window(200.0, 100.0)), 0)
            .unwrap();
        let id = s.shown.iter().find(|x| x.node == w).unwrap().id;
        assert_eq!(
            out.iter()
                .filter(|c| !matches!(c, Change::Placed(_)))
                .copied()
                .collect::<Vec<_>>(),
            [
                Change::Attached(id, w),
                Change::Window(id),
                Change::Configured(id, Size::new(200, 100), Scale::ONE),
            ],
            "told before its size"
        );
        let at = |s: &Surfaces| s.get(id).unwrap().origin;
        let size = |s: &Surfaces| s.get(id).unwrap().logical;
        assert_eq!((at(&s), size(&s)), ([100.0, 115.0], [200.0, 100.0]));
        let told = s.told(id).unwrap().1;
        // Decorated as it asks (`decorations: server`, the default).
        assert!(told.activated && told.decorated && told.sized == (false, false));
        assert_eq!(told.bounds, Some((400, 270)), "the area the bar leaves");
        // Its content grows: the top-left stays.
        s.change(
            w,
            SurfaceChange::Updated {
                spec: window(240.0, 120.0),
                recreate: false,
            },
            1,
        )
        .unwrap();
        assert_eq!((at(&s), size(&s)), ([100.0, 115.0], [240.0, 120.0]));
        // Tiled at a size: that size, at the area's corner.
        let tiled = WindowState {
            size: [Some(380), Some(260)],
            tiled: true,
            ..WindowState::default()
        };
        let out = s.set_window(w, tiled).unwrap();
        assert!(out.contains(&Change::Window(id)));
        assert_eq!((at(&s), size(&s)), ([0.0, 30.0], [380.0, 260.0]));
        assert_eq!(s.told(id).unwrap().1.sized, (true, true));
        // Maximised with no size: the area; fullscreen: the output.
        let max = WindowState {
            maximized: true,
            ..WindowState::default()
        };
        s.set_window(w, max).unwrap();
        assert_eq!((at(&s), size(&s)), ([0.0, 30.0], [400.0, 270.0]));
        s.set_window(
            w,
            WindowState {
                fullscreen: true,
                ..WindowState::default()
            },
        )
        .unwrap();
        assert_eq!((at(&s), size(&s)), ([0.0, 0.0], [400.0, 300.0]));
        // Floating again: its own size where it floated.
        s.set_window(w, WindowState::default()).unwrap();
        assert_eq!((at(&s), size(&s)), ([100.0, 115.0], [240.0, 120.0]));
        assert_eq!(s.told(id).unwrap().1.sized, (false, false));
        // Configured the same again: nothing new to tell.
        let out = s.set_window(w, WindowState::default()).unwrap();
        assert!(!out.contains(&Change::Window(id)), "{out:?}");
        // A second window takes the focus.
        let w2 = node(3);
        s.change(w2, SurfaceChange::Created(window(100.0, 50.0)), 2)
            .unwrap();
        assert!(!s.told(id).unwrap().1.activated);
        let id2 = s.shown.iter().find(|x| x.node == w2).unwrap().id;
        assert!(s.told(id2).unwrap().1.activated);
        // Closed, its state is forgotten.
        s.change(w, SurfaceChange::Removed, 3).unwrap();
        assert_eq!(s.window_state(w), WindowState::default());
    }
}
