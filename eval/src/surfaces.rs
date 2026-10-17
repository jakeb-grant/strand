//! The surface manager's side (docs/eval.md, "Driver"): every surface
//! the renderer reports is attached and configured at the size its spec
//! asks (its shadow overhang included), configured again when that
//! changes, and detached when it closes, each with a buffer of its own.
//! Modelled on crates/strand-render/tests/morph.rs `Shell::sync` (lines
//! 201-269), generalised from popups to every surface kind.

use std::collections::HashMap;

use strand_render::{Renderer, Router};
use strand_scene::{
    Edge, LogicalPoint, NodeId, NodeKind, PaintTarget, Painter, Scale, Size, SurfaceChange,
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
}

/// What `sync` did to a surface.
#[derive(Debug, PartialEq)]
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

    /// Hands the renderer's surface changes to the surfaces, as the
    /// surface manager would.
    pub fn sync(
        &mut self,
        r: &mut Renderer,
        router: &mut Router,
        k: u32,
    ) -> Result<Vec<Change>, Error> {
        let mut out = Vec::new();
        let scale = self.scale()?;
        let mut rounds = 0;
        while r.has_surface_changes() {
            rounds += 1;
            if rounds > 64 {
                return Err(Error::Run("surface changes never settle".into()));
            }
            for (node, change) in r.take_surface_changes() {
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
                        r.detach_surface(gone.id);
                        router.detached(gone.id);
                        out.push(Change::Detached(gone.id));
                    }
                    continue;
                };
                // A popup waits for its anchor's box.
                if s.kind == NodeKind::Popup && s.anchor_rect.is_none() {
                    continue;
                }
                let logical = self.logical(&s)?;
                let size = physical(logical, scale);
                if let Some(i) = at
                    && self.shown[i].size == size
                {
                    continue;
                }
                let id = match at {
                    Some(i) => self.shown[i].id,
                    None => {
                        let id = SurfaceId(self.next);
                        self.next += 1;
                        r.attach_surface(id, node);
                        router.attached(id, node);
                        let origin = match (s.kind, s.anchor_rect) {
                            (NodeKind::Popup, Some(a)) => {
                                let o = s.overhang;
                                (None, LogicalPoint::new(a.x - o.left, a.y + a.h - o.top))
                            }
                            _ => (Some(OUTPUT.to_string()), LogicalPoint::new(0.0, 0.0)),
                        };
                        r.set_surface_origin(id, Some(origin));
                        self.shown.push(Shown {
                            id,
                            node,
                            kind: s.kind,
                            logical,
                            size,
                            pixels: Vec::new(),
                            fresh: true,
                            attached: k,
                        });
                        out.push(Change::Attached(id, node));
                        id
                    }
                };
                let Some(sh) = self.shown.iter_mut().find(|x| x.id == id) else {
                    continue;
                };
                sh.logical = logical;
                sh.size = size;
                sh.pixels = vec![0; (size.w * size.h * 4) as usize];
                sh.fresh = true;
                r.configure_surface(id, size, scale);
                out.push(Change::Configured(id, size, scale));
            }
        }
        Ok(out)
    }

    /// Every shown surface configured again at the screen's (new) scale.
    pub fn rescale(&mut self, r: &mut Renderer) -> Result<Vec<Change>, Error> {
        let scale = self.scale()?;
        let mut out = Vec::new();
        for sh in &mut self.shown {
            let size = physical(sh.logical, scale);
            sh.size = size;
            sh.pixels = vec![0; (size.w * size.h * 4) as usize];
            sh.fresh = true;
            r.configure_surface(sh.id, size, scale);
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
