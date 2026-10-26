//! The compositor's side, cut down from eval/src/surfaces.rs: one
//! output; layer surfaces (`bar`, `panel`, `osd`) sized and placed as a
//! wlroots compositor arranges them, and popups where their positioner
//! asks, both through strand-surface's own placement code
//! (`crate::placement`). Windows, several outputs and compositor poses
//! are not modelled: a config that opens a window gets a note, not a
//! surface.

use std::collections::HashMap;

use strand_render::Renderer;
use strand_scene::{
    Damage, LogicalPoint, NodeId, NodeKind, PaintTarget, Painter, Scale, Size, SurfaceChange,
    SurfaceId, SurfaceSpec,
};

use crate::placement::{self, LayerConfig, PopupConfig};

/// The screen surfaces are arranged on: logical size and scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// A shown surface.
#[derive(Debug)]
pub struct Shown {
    pub id: SurfaceId,
    pub node: NodeId,
    pub kind: NodeKind,
    /// Its logical size and its buffer's physical one.
    pub logical: [f32; 2],
    pub size: Size,
    /// Premultiplied BGRA, as the renderer paints it.
    pub pixels: Vec<u8>,
    /// Configured since its last paint: the next paints all of it.
    pub fresh: bool,
    /// Where its buffer's top-left lies on the screen, logical pixels.
    pub origin: [f32; 2],
    place: Place,
    configured: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Place {
    Layer(LayerConfig),
    Popup {
        config: PopupConfig,
        parent: NodeId,
        at: (i32, i32),
    },
}

/// What happened to a surface, for the host loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Attached(SurfaceId, NodeId),
    Configured(SurfaceId, Size, Scale),
    Placed(SurfaceId),
    Detached(SurfaceId),
}

/// The shown surfaces, in the order they were shown (a popup after its
/// parent, so later ones lie on top).
#[derive(Debug)]
pub struct Surfaces {
    pub shown: Vec<Shown>,
    pub screen: Screen,
    next: u32,
    specs: HashMap<NodeId, SurfaceSpec>,
    /// Kinds asked for that are not modelled (a `window`), once each.
    pub unsupported: Vec<NodeKind>,
}

impl Surfaces {
    pub fn new(screen: Screen) -> Surfaces {
        Surfaces {
            shown: Vec::new(),
            screen,
            next: 1,
            specs: HashMap::new(),
            unsupported: Vec::new(),
        }
    }

    pub fn scale(&self) -> Result<Scale, String> {
        scale_of(self.screen.scale)
    }

    pub fn get(&self, id: SurfaceId) -> Option<&Shown> {
        self.shown.iter().find(|s| s.id == id)
    }

    /// How far down the screen the shown surfaces reach, logical
    /// pixels (0 with none).
    pub fn bottom(&self) -> f32 {
        self.shown
            .iter()
            .map(|s| s.origin[1] + s.logical[1])
            .fold(0.0, f32::max)
    }

    /// Where render is told `id` lies (output name, logical origin).
    pub fn origin(&self, id: SurfaceId) -> Option<(Option<String>, LogicalPoint)> {
        let s = self.get(id)?;
        Some((None, LogicalPoint::new(s.origin[0], s.origin[1])))
    }

    fn full(&self) -> (u32, u32) {
        (self.screen.width.max(1), self.screen.height.max(1))
    }

    /// One of the renderer's surface changes, as the surface manager
    /// takes it: the surfaces attached, configured, placed or detached.
    pub fn change(&mut self, node: NodeId, change: SurfaceChange) -> Result<Vec<Change>, String> {
        let mut out = Vec::new();
        match change {
            SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => {
                self.specs.insert(node, s);
            }
            SurfaceChange::Removed => {
                self.specs.remove(&node);
            }
        }
        self.reconcile(node, &mut out)?;
        self.arrange(&mut out)?;
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
                self.reconcile(kid, &mut out)?;
            }
            self.arrange(&mut out)?;
        }
        Ok(out)
    }

    fn reconcile(&mut self, node: NodeId, out: &mut Vec<Change>) -> Result<(), String> {
        let at = self.shown.iter().position(|s| s.node == node);
        let Some(spec) = self.specs.get(&node).filter(|s| s.open).cloned() else {
            self.detach(node, out);
            return Ok(());
        };
        let place = match spec.kind {
            NodeKind::Lock | NodeKind::Window => {
                if !self.unsupported.contains(&spec.kind) {
                    self.unsupported.push(spec.kind);
                }
                return Ok(());
            }
            NodeKind::Popup => {
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
                    origin: [0.0, 0.0],
                    place,
                    configured: false,
                });
                out.push(Change::Attached(id, node));
            }
        }
        Ok(())
    }

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

    fn has_nested_scrim(&self, node: NodeId) -> bool {
        self.specs.values().any(|s| {
            s.kind == NodeKind::Popup && s.scrim.is_some() && self.root_node(s) == Some(node)
        })
    }

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

    /// Every shown surface sized and placed: layer surfaces in the area
    /// the exclusive zones before them leave, popups on their parents.
    fn arrange(&mut self, out: &mut Vec<Change>) -> Result<(), String> {
        let scale = self.scale()?;
        let full = self.full();
        let mut placed: Vec<(SurfaceId, [u32; 2], [f32; 2])> = Vec::new();
        let layers: Vec<(SurfaceId, LayerConfig)> = self
            .shown
            .iter()
            .filter_map(|s| match &s.place {
                Place::Layer(c) => Some((s.id, c.clone())),
                Place::Popup { .. } => None,
            })
            .collect();
        let list: Vec<(SurfaceId, &LayerConfig)> = layers.iter().map(|(i, c)| (*i, c)).collect();
        for (id, c) in &layers {
            let area = placement::arranged_area(full, &list, id);
            let size = configured_size(c, area);
            let (x, y) = c.position_in_rect(size, area);
            placed.push((*id, [size.0, size.1], [x as f32, y as f32]));
        }
        for i in 0..self.shown.len() {
            let Place::Popup { config, parent, at } = &self.shown[i].place else {
                continue;
            };
            let Some(p) = self.shown.iter().find(|s| s.node == *parent) else {
                continue;
            };
            let parent_at = placed
                .iter()
                .find(|(id, _, _)| *id == p.id)
                .map_or(p.origin, |(_, _, o)| *o);
            let (gx, gy) = geometry(p);
            let [t, _, _, l] = config.overhang;
            let (w, h) = config.buffer();
            let o = [
                parent_at[0] + (gx + at.0 - l) as f32,
                parent_at[1] + (gy + at.1 - t) as f32,
            ];
            placed.push((self.shown[i].id, [w.max(1), h.max(1)], o));
        }
        for (id, size, origin) in placed {
            let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
                continue;
            };
            let logical = [size[0] as f32, size[1] as f32];
            let moved = sh.origin != origin;
            sh.origin = origin;
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

    /// Paints surface `id`'s frame presented at `time`; its damage.
    pub fn paint(
        &mut self,
        r: &mut Renderer,
        id: SurfaceId,
        time: std::time::Duration,
    ) -> Result<Damage, String> {
        let scale = self.scale()?;
        let Some(sh) = self.shown.iter_mut().find(|s| s.id == id) else {
            return Ok(Damage::new());
        };
        let age = if sh.fresh { 0 } else { 1 };
        sh.fresh = false;
        let t = PaintTarget::new(&mut sh.pixels, sh.size, sh.size.w * 4, scale, age)
            .map_err(|e| format!("surface {}: {e:?}", id.0))?;
        let mut t = t.at(time);
        Ok(r.paint(id, &mut t))
    }

    /// The topmost surface under `at` (screen logical pixels) and the
    /// point in its own coordinates.
    pub fn hit(&self, at: [f32; 2]) -> Option<(SurfaceId, LogicalPoint)> {
        self.shown.iter().rev().find_map(|s| {
            let (x, y) = (at[0] - s.origin[0], at[1] - s.origin[1]);
            (x >= 0.0 && y >= 0.0 && x < s.logical[0] && y < s.logical[1])
                .then(|| (s.id, LogicalPoint::new(x, y)))
        })
    }

    /// The whole screen at its physical size, every surface laid over
    /// the last in the order shown (premultiplied "over"), as straight
    /// RGBA for a canvas's `ImageData`.
    pub fn composite(&self) -> (u32, u32, Vec<u8>) {
        self.composite_top(u32::MAX)
    }

    /// [`Surfaces::composite`] of the screen's top `rows` physical rows
    /// only (a bar's example need not composite the empty screen under
    /// it).
    pub fn composite_top(&self, rows: u32) -> (u32, u32, Vec<u8>) {
        let s = self.screen.scale;
        let w = (f64::from(self.screen.width) * s).round().max(1.0) as u32;
        let h = (f64::from(self.screen.height) * s).round().max(1.0) as u32;
        let h = h.min(rows.max(1));
        // Premultiplied RGBA, then made straight at the end.
        let mut acc = vec![0u8; (w * h * 4) as usize];
        for sh in &self.shown {
            let ox = (f64::from(sh.origin[0]) * s).round() as i64;
            let oy = (f64::from(sh.origin[1]) * s).round() as i64;
            for y in 0..sh.size.h as i64 {
                let dy = oy + y;
                if dy < 0 || dy >= i64::from(h) {
                    continue;
                }
                for x in 0..sh.size.w as i64 {
                    let dx = ox + x;
                    if dx < 0 || dx >= i64::from(w) {
                        continue;
                    }
                    let si = ((y * i64::from(sh.size.w) + x) * 4) as usize;
                    let di = ((dy * i64::from(w) + dx) * 4) as usize;
                    let Some(p) = sh.pixels.get(si..si + 4) else {
                        continue;
                    };
                    let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
                    if a == 0 && b == 0 && g == 0 && r == 0 {
                        continue;
                    }
                    let inv = 255 - u32::from(a);
                    let over = |src: u8, dst: u8| -> u8 {
                        (u32::from(src) + (u32::from(dst) * inv + 127) / 255).min(255) as u8
                    };
                    acc[di] = over(r, acc[di]);
                    acc[di + 1] = over(g, acc[di + 1]);
                    acc[di + 2] = over(b, acc[di + 2]);
                    acc[di + 3] = over(a, acc[di + 3]);
                }
            }
        }
        for p in acc.chunks_exact_mut(4) {
            let a = u32::from(p[3]);
            let un = |c: u8| {
                (u32::from(c) * 255 + a / 2)
                    .checked_div(a)
                    .unwrap_or(0)
                    .min(255) as u8
            };
            p[0] = un(p[0]);
            p[1] = un(p[1]);
            p[2] = un(p[2]);
        }
        (w, h, acc)
    }
}

/// Straight-alpha RGBA of a premultiplied BGRA buffer, as eval's
/// `surfaces::to_rgba` makes it for its PNGs.
pub fn to_rgba(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len());
    for p in pixels.chunks_exact(4) {
        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        let un = |c: u32| (c * 255 + a / 2).checked_div(a).unwrap_or(0).min(255) as u8;
        out.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
    }
    out
}

fn scale_of(s: f64) -> Result<Scale, String> {
    Scale::from_f64(s).ok_or_else(|| format!("scale {s} is not one Wayland has"))
}

fn configured_size(c: &LayerConfig, area: placement::UsableArea) -> (u32, u32) {
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

fn geometry(s: &Shown) -> (i32, i32) {
    match &s.place {
        Place::Popup { config, .. } => (config.overhang[3], config.overhang[0]),
        Place::Layer(_) => (0, 0),
    }
}

fn whole(v: f32) -> i32 {
    if v.is_finite() { v.round() as i32 } else { 0 }
}

fn physical(logical: [f32; 2], scale: Scale) -> Size {
    let s = scale.physical_size(strand_scene::LogicalSize::new(logical[0], logical[1]));
    Size::new(s.w.max(1), s.h.max(1))
}
