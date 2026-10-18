//! The surface manager's side (docs/eval.md, "Driver"): every surface
//! the renderer reports is attached and configured at the size its spec
//! asks (its shadow overhang included), configured again when that
//! changes, and detached when it closes, each with a buffer of its own.
//! Modelled on crates/strand-render/tests/morph.rs `Shell::sync` (lines
//! 201-269), generalised from popups to every surface kind.
//!
//! A popup is placed where the surface manager's positioner asks the
//! compositor to put it (crates/strand-surface/src/placement.rs
//! `popup_config` and `popup_request`: across a bar from its edge, below
//! its anchor otherwise, centred on it, a submenu level with its row),
//! placed again whenever it is configured at a new size (a content-sized
//! popup that grows is re-centred), and render is told where
//! (`Renderer::set_surface_origin`, as `Host::send_origin` does). The
//! trace records each surface's origin per frame, so the checks see a
//! popup's content where the eye does.

use std::collections::HashMap;

use strand_render::Renderer;
use strand_scene::{
    Anchor, Edge, LogicalPoint, NodeId, NodeKind, PaintTarget, Painter, Scale, Size, SurfaceChange,
    SurfaceId, SurfaceSpec,
};

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
    /// Where its buffer's top-left lies on the output, logical pixels.
    pub origin: [f32; 2],
}

/// What the surface manager does to a surface (`Surfaces::change`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Attached(SurfaceId, NodeId),
    /// Configured at this buffer size and scale (after `Attached` for a
    /// new surface; again when its size or the scale changes).
    Configured(SurfaceId, Size, Scale),
    Detached(SurfaceId),
}

/// The shown surfaces.
#[derive(Debug)]
pub struct Surfaces {
    pub shown: Vec<Shown>,
    next: u32,
    specs: HashMap<NodeId, SurfaceSpec>,
    pub screen: Screen,
}

impl Surfaces {
    pub fn new(screen: Screen) -> Surfaces {
        Surfaces {
            shown: Vec::new(),
            next: 1,
            specs: HashMap::new(),
            screen,
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

    /// The surface `id`.
    pub fn get(&self, id: SurfaceId) -> Option<&Shown> {
        self.shown.iter().find(|s| s.id == id)
    }

    /// The logical size a surface of `spec` is configured at.
    fn logical(&self, spec: &SurfaceSpec) -> Result<[f32; 2], Error> {
        let o = spec.overhang;
        let m = spec.margin;
        let sw = self.screen.width as f32;
        let sh = self.screen.height as f32;
        let (w, h) = match spec.kind {
            NodeKind::Bar => match spec.edge {
                Some(Edge::Left | Edge::Right) => (
                    spec.width.unwrap_or(1.0),
                    spec.height.unwrap_or(sh - m.top - m.bottom),
                ),
                _ => (
                    spec.width.unwrap_or(sw - m.left - m.right),
                    spec.height.unwrap_or(1.0),
                ),
            },
            NodeKind::Lock => {
                return Err(Error::Run(
                    "a `lock` surface runs only in the lock VM (scripts/container/lockvm.sh), \
                     never in the sandbox"
                        .into(),
                ));
            }
            _ => (spec.width.unwrap_or(1.0), spec.height.unwrap_or(1.0)),
        };
        Ok([
            (w + o.left + o.right).max(1.0),
            (h + o.top + o.bottom).max(1.0),
        ])
    }

    /// Where a surface of `spec` lies on the output: a layer surface at
    /// the output's corner (the sandbox's one screen), a popup where the
    /// surface manager's positioner puts it (`popup_config` and
    /// `popup_request` in crates/strand-surface/src/placement.rs, before
    /// the compositor constrains it to the output), from its parent's
    /// origin.
    fn origin_of(&self, spec: &SurfaceSpec) -> [f32; 2] {
        let (NodeKind::Popup, Some(a)) = (spec.kind, spec.anchor_rect) else {
            return [0.0, 0.0];
        };
        let px = |v: f32| if v.is_finite() { v.round() as i32 } else { 0 };
        let parent = spec.parent.and_then(|p| self.specs.get(&p));
        let parent_at = spec
            .parent
            .and_then(|p| self.shown.iter().find(|s| s.node == p))
            .map_or([0.0, 0.0], |s| s.origin);
        let (pt, pl) = parent.map_or((0, 0), |p| {
            (px(p.overhang.top).max(0), px(p.overhang.left).max(0))
        });
        let parent_popup = parent.is_some_and(|p| p.kind == NodeKind::Popup);
        // The parent's window geometry: a popup's is its box, a layer
        // surface's its whole buffer.
        let (gx, gy) = if parent_popup { (pl, pt) } else { (0, 0) };
        let (mut x, mut y, mut aw, mut ah) = (px(a.x) - gx, px(a.y) - gy, px(a.w), px(a.h));
        let bar_edge = parent
            .filter(|p| p.kind == NodeKind::Bar)
            .map(|p| p.edge.unwrap_or(Edge::Top));
        #[derive(Clone, Copy, PartialEq)]
        enum Side {
            Below,
            Above,
            Right,
            Left,
        }
        let side = match spec.attach.or(bar_edge) {
            Some(Edge::Top) => Side::Below,
            Some(Edge::Bottom) => Side::Above,
            Some(Edge::Left) => Side::Right,
            Some(Edge::Right) => Side::Left,
            None if parent_popup => match spec.anchor {
                Anchor::Left | Anchor::TopLeft | Anchor::BottomLeft => Side::Left,
                Anchor::Top => Side::Above,
                Anchor::Bottom => Side::Below,
                _ => Side::Right,
            },
            None => Side::Below,
        };
        let side = if spec.tooltip { Side::Below } else { side };
        let aligned = parent_popup
            && spec.attach.is_none()
            && !spec.tooltip
            && matches!(side, Side::Right | Side::Left);
        if let Some(edge) = bar_edge
            && !spec.tooltip
        {
            let thick = parent
                .and_then(|p| p.exclusive_zone())
                .map_or(0, |t| px(t).max(0));
            match edge {
                Edge::Top | Edge::Bottom => (y, ah) = (pt, thick),
                Edge::Left | Edge::Right => (x, aw) = (pl, thick),
            }
        }
        let m = spec.margin;
        let gap = match side {
            Side::Below => m.top,
            Side::Above => m.bottom,
            Side::Right => m.left,
            Side::Left => m.right,
        };
        let gap = match (spec.tooltip, spec.margin_set, spec.attach) {
            (true, _, _) => 4,
            (false, true, _) => px(gap),
            (false, false, Some(_)) => 0,
            (false, false, None) => 6,
        };
        let (w, h) = (
            px(spec.width.unwrap_or(1.0)).max(1),
            px(spec.height.unwrap_or(1.0)).max(1),
        );
        let (aw, ah) = (aw.max(1), ah.max(1));
        let (bx, by) = match (side, aligned) {
            (Side::Below, _) => (x + aw / 2 - w / 2, y + ah + gap),
            (Side::Above, _) => (x + aw / 2 - w / 2, y - h - gap),
            (Side::Right, false) => (x + aw + gap, y + ah / 2 - h / 2),
            (Side::Left, false) => (x - w - gap, y + ah / 2 - h / 2),
            (Side::Right, true) => (x + aw + gap, y),
            (Side::Left, true) => (x - w - gap, y),
        };
        let o = spec.overhang;
        [
            parent_at[0] + (gx + bx) as f32 - o.left,
            parent_at[1] + (gy + by) as f32 - o.top,
        ]
    }

    /// What the surface manager does with one of the renderer's surface
    /// changes (`State::apply_surface_change`): the surfaces to attach,
    /// configure and detach, in order, for the driver to run through the
    /// host loop (`strand_render::host_loop::{attached, configured,
    /// detached}`). A surface it attaches is shown from now on.
    pub fn change(
        &mut self,
        node: NodeId,
        change: SurfaceChange,
        k: u32,
    ) -> Result<Vec<Change>, Error> {
        let mut out = Vec::new();
        let scale = self.scale()?;
        let spec = match change {
            SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => Some(s),
            SurfaceChange::Removed => None,
        };
        if let Some(s) = &spec {
            self.specs.insert(node, s.clone());
        } else {
            self.specs.remove(&node);
        }
        let at = self.shown.iter().position(|s| s.node == node);
        let Some(s) = spec.filter(|s| s.open) else {
            if let Some(i) = at {
                let gone = self.shown.remove(i);
                out.push(Change::Detached(gone.id));
            }
            return Ok(out);
        };
        // A popup waits for its anchor's box.
        if s.kind == NodeKind::Popup && s.anchor_rect.is_none() {
            return Ok(out);
        }
        let logical = self.logical(&s)?;
        let size = physical(logical, scale);
        if let Some(i) = at
            && self.shown[i].size == size
        {
            return Ok(out);
        }
        let origin = self.origin_of(&s);
        let id = match at {
            Some(i) => self.shown[i].id,
            None => {
                let id = SurfaceId(self.next);
                self.next += 1;
                self.shown.push(Shown {
                    id,
                    node,
                    kind: s.kind,
                    logical,
                    size,
                    pixels: Vec::new(),
                    fresh: true,
                    attached: k,
                    origin,
                });
                out.push(Change::Attached(id, node));
                id
            }
        };
        let Some(sh) = self.shown.iter_mut().find(|x| x.id == id) else {
            return Ok(out);
        };
        sh.logical = logical;
        sh.size = size;
        sh.pixels = vec![0; (size.w * size.h * 4) as usize];
        sh.fresh = true;
        sh.origin = origin;
        out.push(Change::Configured(id, size, scale));
        Ok(out)
    }

    /// Where surface `id` lies, for render (`Renderer::set_surface_origin`,
    /// as `Host::send_origin` sends it): a layer surface on the one
    /// output, a popup on its parent's.
    pub fn origin(&self, id: SurfaceId) -> Option<(Option<String>, LogicalPoint)> {
        let sh = self.get(id)?;
        let output = (sh.kind != NodeKind::Popup).then(|| OUTPUT.to_string());
        Some((output, LogicalPoint::new(sh.origin[0], sh.origin[1])))
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

    /// Paints surface `id`'s frame presented at `time`; its damage area.
    pub fn paint(
        &mut self,
        r: &mut Renderer,
        id: SurfaceId,
        time: std::time::Duration,
    ) -> Result<u64, Error> {
        let scale = self.scale()?;
        let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
            return Ok(0);
        };
        let age = if sh.fresh { 0 } else { 1 };
        sh.fresh = false;
        let t = PaintTarget::new(&mut sh.pixels, sh.size, sh.size.w * 4, scale, age)
            .map_err(|e| Error::Run(format!("surface {}: {e:?}", id.0)))?;
        let mut t = t.at(time);
        Ok(r.paint(id, &mut t).area())
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

/// The physical size of a logical one at `scale`.
fn physical(logical: [f32; 2], scale: Scale) -> Size {
    let f = scale.as_f64();
    let px = |v: f32| ((f64::from(v) * f).ceil() as u32).max(1);
    Size::new(px(logical[0]), px(logical[1]))
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
