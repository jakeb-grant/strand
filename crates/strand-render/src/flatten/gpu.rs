//! (M4, `gpu` builds) The flattener's side of the bundled GPU effects
//! (`crate::effects::gpu`): the pass wants a node adds while it is drawn,
//! and the GPU's pixels it draws in place of the CPU's once they are
//! back.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{Bundled, NodeId, Rect, ShaderInput, ShaderPass, ShaderRef, TimeContext};
use vello_cpu::Pixmap;
use vello_cpu::kurbo;

use super::{Flattener, Item, hash_item};
use crate::backdrop::GLASS_REFRACTION;
use crate::effects::builtin::{Builtin, Kind};
use crate::effects::gpu::{LARGE_BLUR, aurora_uniforms, glass_uniforms};
use crate::effects::raster::Built;
use crate::offscreen::Drawn;
use crate::renderer::backend::{PassId, PassInput, Slot, bundled_want, gpu_filter};

impl Flattener<'_> {
    /// The pointer as a pass reads it (`strand.pointer`): buffer pixels
    /// relative to `rect`, a box in the current group's space (`-1, -1`
    /// off the surface or outside it).
    pub(super) fn pass_pointer(&self, rect: kurbo::Rect) -> [f32; 2] {
        let Some(p) = self.pointer.filter(|p| p.x.is_finite() && p.y.is_finite()) else {
            return [-1.0, -1.0];
        };
        let s = self.scale.as_f64();
        let at = kurbo::Point::new(p.x as f64 * s, p.y as f64 * s);
        let local = if self.xform.determinant().abs() > 1e-12 {
            self.xform.inverse() * at
        } else {
            return [-1.0, -1.0];
        };
        if !rect.contains(local) {
            return [-1.0, -1.0];
        }
        [(local.x - rect.x0) as f32, (local.y - rect.y0) as f32]
    }

    /// True if `node`'s `tilt` is the GPU's 3-D pass: a GPU may draw
    /// (none failed under 30 s ago) and has answered for it (a first
    /// tilt turns in 2-D while [`Self::warm_tilt`] gets the device up).
    pub(super) fn tilts_in_3d(&self, node: NodeId) -> bool {
        self.extras.gpu_ok
            && self
                .extras
                .shaders
                .get(PassId::new(node, Slot::Filter))
                .is_some()
    }

    /// Asks for a one-pixel tilt pass for `node`, turning in 2-D: it
    /// starts the device, and once it is back the node turns in 3-D.
    pub(super) fn warm_tilt(&mut self, node: NodeId) {
        let pass = ShaderPass {
            code: ShaderRef::Bundled(Bundled::Tilt),
            uniforms: Arc::from([0.0, 0.0]),
            input: ShaderInput::Content,
        };
        let one = Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        let scale = self.scale.as_f32();
        if let Some(w) = bundled_want(
            PassId::new(node, Slot::Filter),
            vec![pass],
            one,
            PassInput::None,
            0,
            scale,
            0.0,
            [-1.0, -1.0],
        ) {
            self.out.passes.push(w);
        }
    }

    /// The pointer relative to `region`, in surface buffer pixels.
    fn region_pointer(&self, region: Rect) -> [f32; 2] {
        let Some(p) = self.pointer.filter(|p| p.x.is_finite() && p.y.is_finite()) else {
            return [-1.0, -1.0];
        };
        let s = self.scale.as_f32();
        let (x, y) = (p.x * s - region.x as f32, p.y * s - region.y as f32);
        if x < 0.0 || y < 0.0 || x >= region.w as f32 || y >= region.h as f32 {
            return [-1.0, -1.0];
        }
        [x, y]
    }

    /// The GPU's pixels for raster source `built` of `node` (an aurora,
    /// or particles past the CPU's cap) laid out at `frame`, at `time`;
    /// `None` when the CPU draws it (no GPU, no pixels yet, anything
    /// else). Asks for them either way while a GPU can draw them.
    pub(super) fn generator_pass(
        &mut self,
        node: NodeId,
        built: &Built,
        frame: kurbo::Rect,
        time: TimeContext,
    ) -> Option<(u64, Arc<Pixmap>)> {
        let (w, h) = (frame.width().round() as u32, frame.height().round() as u32);
        let scale = self.scale.as_f32();
        let (code, uniforms) = match built {
            Built::Effect(
                b @ Builtin {
                    kind: Kind::Aurora, ..
                },
            ) => (Bundled::Aurora, aurora_uniforms(b)),
            Built::Particles(p) if p.capped() => {
                (Bundled::Particles, p.gpu_uniforms(w, h, scale, time.t))
            }
            _ => return None,
        };
        let pass = ShaderPass {
            code: ShaderRef::Bundled(code),
            uniforms: uniforms.into(),
            input: ShaderInput::None,
        };
        let region = Rect {
            x: frame.x0.round() as i32,
            y: frame.y0.round() as i32,
            w,
            h,
        };
        let want = bundled_want(
            PassId::new(node, Slot::Node),
            vec![pass],
            region,
            PassInput::None,
            0,
            scale,
            time.t,
            [-1.0, -1.0],
        )?;
        let got = self.extras.shaders.usable(&want).cloned();
        self.out.passes.push(want);
        got
    }

    /// Asks for the bundled passes of the layer pushed at item `i` (by
    /// `node`, its subtree flattened after it): its group, CPU-filtered,
    /// is their input; once their pixels are back they are the layer's
    /// offscreen group, and its node's record follows them.
    pub(super) fn filter_pass(&mut self, node: NodeId, i: usize, time: f32) {
        let layer = match self.out.items.get(i).map(|d| &d.item) {
            Some(Item::PushLayer(l)) => l.clone(),
            _ => return,
        };
        let passes: Vec<ShaderPass> = layer
            .effects
            .iter()
            .filter_map(gpu_filter)
            .cloned()
            .collect();
        if passes.is_empty() {
            return;
        }
        let Some(region) = self.out.items[i]
            .bounds
            .intersect(self.surface)
            .filter(|r| !r.is_empty())
        else {
            return;
        };
        // What the input's pixels depend on: the group's region, effects,
        // transform and items.
        let mut h = DefaultHasher::new();
        (region.x, region.y, region.w, region.h).hash(&mut h);
        crate::layers::hash_effects(&mut h, &layer.effects);
        for v in layer.xform.as_coeffs() {
            v.to_bits().hash(&mut h);
        }
        for d in &self.out.items[i + 1..] {
            hash_item(&mut h, &d.item);
        }
        let Some(want) = bundled_want(
            PassId::new(node, Slot::Filter),
            passes,
            region,
            PassInput::Group(i),
            h.finish(),
            self.scale.as_f32(),
            time,
            [-1.0, -1.0],
        ) else {
            return;
        };
        if let Some((key, pixmap)) = self.extras.shaders.usable(&want).cloned() {
            let mut with = (*layer).clone();
            with.gpu = Some(Drawn {
                pixmap,
                x: region.x,
                y: region.y,
            });
            self.out.items[i].item = Item::PushLayer(Arc::new(with));
            if let Some(rec) = self.out.records.get_mut(&node) {
                let mut h = DefaultHasher::new();
                (rec.sig, key).hash(&mut h);
                rec.sig = h.finish();
                rec.bounds = rec.bounds.union(region);
            }
        }
        self.out.passes.push(want);
    }

    /// The GPU's pixels for backdrop `e` of `node` over its box `boxed`
    /// (surface buffer pixels; `read` with the CPU's reach), the backdrop
    /// layer to be pushed at item `layer` with corners of `radius`:
    /// glass, or a blur over a large box. `None` while the CPU draws it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn backdrop_pass(
        &mut self,
        node: NodeId,
        e: &strand_scene::Effect,
        boxed: Rect,
        read: Rect,
        layer: usize,
        radius: f64,
        time: f32,
    ) -> Option<Drawn> {
        if boxed.is_empty() {
            return None;
        }
        let strand_scene::Effect::Shader(ShaderPass {
            code: ShaderRef::Bundled(b),
            uniforms,
            ..
        }) = e
        else {
            return None;
        };
        let scale = self.scale.as_f32();
        let (pass, region, pointer) = match b {
            Bundled::Glass => {
                let refraction = uniforms
                    .first()
                    .copied()
                    .unwrap_or(GLASS_REFRACTION * scale);
                let u = glass_uniforms(refraction, radius as f32, scale);
                let pass = ShaderPass {
                    code: ShaderRef::Bundled(Bundled::Glass),
                    uniforms: u.into(),
                    input: ShaderInput::Content,
                };
                (pass, boxed, self.region_pointer(boxed))
            }
            Bundled::BackdropBlur if u64::from(boxed.w) * u64::from(boxed.h) >= LARGE_BLUR => {
                let pass = ShaderPass {
                    code: ShaderRef::Bundled(Bundled::BackdropBlur),
                    uniforms: uniforms.clone(),
                    input: ShaderInput::Content,
                };
                (pass, read, [-1.0, -1.0])
            }
            _ => return None,
        };
        let mut h = DefaultHasher::new();
        crate::backdrop::hash_behind(&self.out.items, self.out.items.len(), region, &mut h);
        let want = bundled_want(
            PassId::new(node, Slot::Backdrop),
            vec![pass],
            region,
            PassInput::Behind(layer),
            h.finish(),
            scale,
            time,
            pointer,
        )?;
        let got = self.extras.shaders.usable(&want).map(|(_, p)| Drawn {
            pixmap: p.clone(),
            x: region.x,
            y: region.y,
        });
        self.out.passes.push(want);
        got
    }
}
