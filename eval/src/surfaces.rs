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
use strand_surface::placement::{self, LayerConfig, PopupConfig, UsableArea};

use crate::Error;
use crate::scenario::Screen;

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
    Detached(SurfaceId),
}

/// The shown surfaces.
#[derive(Debug)]
pub struct Surfaces {
    pub shown: Vec<Shown>,
    next: u32,
    specs: HashMap<NodeId, SurfaceSpec>,
    pub screen: Screen,
    /// (eval-surface-leg) With the surface leg, every change the renderer
    /// reported, as it reported it, for the real surface manager.
    pub tee: Option<Vec<(NodeId, SurfaceChange)>>,
}

impl Surfaces {
    pub fn new(screen: Screen) -> Surfaces {
        Surfaces {
            shown: Vec::new(),
            next: 1,
            specs: HashMap::new(),
            screen,
            tee: None,
        }
    }

    pub fn scale(&self) -> Result<Scale, Error> {
        Scale::from_f64(self.screen.scale).ok_or_else(|| {
            Error::Run(format!(
                "scale {} is not one Wayland has",
                self.screen.scale
            ))
        })
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
            Place::Layer(_) => None,
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

    /// The surface `id`.
    pub fn get(&self, id: SurfaceId) -> Option<&Shown> {
        self.shown.iter().find(|s| s.id == id)
    }

    /// The screen's logical size, as the output describes it.
    fn full(&self) -> (u32, u32) {
        (self.screen.width.max(1), self.screen.height.max(1))
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
            _ => {
                let Ok(mut config) =
                    placement::layer_config_with(&spec, self.has_nested_scrim(node))
                else {
                    // No size yet, or not a layer surface: none (yet),
                    // as the manager's `create_surface` leaves it.
                    return Ok(());
                };
                let (w, h) = self.full();
                config.fit((w as i32, h as i32));
                Place::Layer(config)
            }
        };
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
            out.push(Change::Detached(gone.id));
        }
    }

    /// Where popup `c`'s box lands relative to its parent `p`'s window
    /// geometry: as the manager's `positioner` asks for it, moved inside
    /// the screen along its anchor's edge from where its parent lies.
    fn popup_at(&self, c: &PopupConfig, p: &Shown) -> (i32, i32) {
        let (gx, gy) = geometry(p);
        let (sw, sh) = self.full();
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
        let scale = self.scale()?;
        let full = self.full();
        let layers: Vec<(SurfaceId, LayerConfig)> = self
            .shown
            .iter()
            .filter_map(|s| match &s.place {
                Place::Layer(c) => Some((s.id, c.clone())),
                Place::Popup { .. } => None,
            })
            .collect();
        let list: Vec<(SurfaceId, &LayerConfig)> = layers.iter().map(|(i, c)| (*i, c)).collect();
        let mut placed: Vec<Arranged> = Vec::new();
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
        for (id, size, origin, rest) in placed {
            let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
                continue;
            };
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
            Place::Popup { .. } => [0.0, 0.0],
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
            Place::Popup { .. } => None,
        }
    }

    /// Where surface `id` lies, for render (`Renderer::set_surface_origin`,
    /// as `Host::send_origin` sends it): a layer surface on the one
    /// output, a popup on its parent's.
    pub fn origin(&self, id: SurfaceId) -> Option<(Option<String>, LogicalPoint)> {
        let sh = self.get(id)?;
        let output = (sh.kind != NodeKind::Popup).then(|| OUTPUT.to_string());
        Some((output, LogicalPoint::new(sh.origin[0], sh.origin[1])))
    }

    /// (eval-rules-2) The sides of surface `id`'s buffer a shadow may be
    /// cut by, as bits (1 top, 2 right, 4 bottom, 8 left): a side that
    /// lies on or past the output's edge, where the compositor's placement
    /// (`arrange`) put it (past it nothing shows: render keeps an anchored
    /// side's overhang within its margin, decisions.md m6-w2-surface), and
    /// an `attach:` side, past which there is no overhang (m4-surface-w1).
    /// Read from the modelled buffer, not from render's own caps, so a
    /// side render cuts that is not at the edge stays a finding
    /// (integration-r4).
    pub fn cuts(&self, id: SurfaceId) -> u8 {
        let Some(sh) = self.get(id) else {
            return 0;
        };
        let (sw, shh) = self.full();
        let [x, y] = sh.rest;
        let [w, h] = sh.logical;
        let at = |gap: f32| gap <= 0.5;
        let mut bits = 0;
        if at(y) {
            bits |= 1;
        }
        if at(sw as f32 - (x + w)) {
            bits |= 2;
        }
        if at(shh as f32 - (y + h)) {
            bits |= 4;
        }
        if at(x) {
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

    /// Every shown surface at the screen's (new) scale: the surfaces to
    /// configure again.
    pub fn rescale(&mut self) -> Result<Vec<Change>, Error> {
        let scale = self.scale()?;
        let mut out = Vec::new();
        for sh in &mut self.shown {
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
        let scale = self.scale()?;
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
        s.change(b, SurfaceChange::Created(bar), 0).unwrap();
        s.change(p, SurfaceChange::Created(panel(150.0)), 0)
            .unwrap();
        assert_eq!(at(&s), [125.0, 30.0]);
        // (integration-r4) The cut sides are where the buffer meets the
        // output's edge: the bar's top, left and right, none of the
        // panel's under the bar's zone.
        let id = |n: NodeId| s.shown.iter().find(|x| x.node == n).unwrap().id;
        assert_eq!(s.cuts(id(b)), 1 | 2 | 8);
        assert_eq!(s.cuts(id(p)), 0);
    }
}
