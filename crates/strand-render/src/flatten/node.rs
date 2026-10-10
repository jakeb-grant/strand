//! Flattening one node: its box, background, border, shadows, text,
//! clips and transforms, then its children.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{
    BlurRegion, Border, Color, Font, Insets, LogicalRect, NodeKind, Paint, Prop, PropValue, Rect,
    Shadow, TokenScope,
};
use strand_text::{TextLayout, TextStyle};
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii, Shape};

use super::hash::{hash_color, hash_f32, hash_item, hash_path};
use super::paint::{
    corners_of, cover, kurbo_rect, opaque_bands, opaque_paint, paint_of, radii, radii_zero,
    shape_path, tinted,
};
use super::text::{Shaped, TextSpec, natural_spec, pick, place_text, sane_font, slot_color};
use super::widget::{CaretAt, WidgetCtx, caret_x};
use super::{
    DisplayItem, FillShape, Flattener, GlyphCells, HitBox, Inherited, Item, MAX_BLUR, NodeRecord,
    TOLERANCE, angle, default_color, default_font, finite, finite_or_zero, length, map_rect,
    number,
};
use crate::tree::Node;

impl<'a> Flattener<'a> {
    pub(super) fn push(
        &mut self,
        item: Item,
        bounds: Rect,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let bounds = map_rect(self.xform, bounds);
        hash_item(sig, &item);
        *ink = ink.union(bounds);
        self.out.items.push(DisplayItem { item, bounds });
    }

    /// (M4) `id` is drawn, or hidden only by something that follows
    /// time: its clock (if it has one) runs from this frame, and the
    /// surface keeps it.
    fn run_clock(&mut self, id: strand_scene::NodeId, clock: Option<crate::clock::Clock>) {
        if clock.is_some() {
            self.anim.start_clock(id);
        }
        self.out.clocks.extend(clock);
    }

    /// Pushes a group marker; returns its index so a push marker's bounds
    /// can be set to its group's once known.
    pub(super) fn marker(&mut self, item: Item) -> usize {
        let bounds = self.surface;
        self.out.items.push(DisplayItem { item, bounds });
        self.out.items.len() - 1
    }

    /// Flattens `node` and its subtree; returns the subtree's ink bounds
    /// (clipped by ancestors).
    pub(super) fn node(
        &mut self,
        node: &'a Node,
        parent: LogicalRect,
        inh: &Inherited<'a>,
        root: bool,
    ) -> Rect {
        let s = self.scale.as_f64();
        // Token references resolve once per node, against the global table
        // and the `tokens` overrides of this node and its ancestors.
        let mut tokens = inh.tokens.clone();
        if let Some(PropValue::Tokens(t)) = node.get(Prop::Tokens) {
            tokens.push(t);
        }
        // Time-bound props (M4) are evaluated at this node's own time.
        let global = &self.tree.tokens;
        let timed_scope = inh.timed || crate::time::overrides_read_time(node, global);
        // (M4) A `text`'s `letters` child is drawn by the text, at its time.
        let letters = crate::effects::letters::child(self.tree, node);
        let timed = timed_scope
            || crate::time::reads_time(node, global)
            || letters.is_some_and(|l| crate::time::reads_time(l, global));
        // Its clock: the rate its time props and its own animation run at.
        // (A source built from props follows a clock that time-bound
        // props run at refresh: `crate::effects::raster`.)
        let raster = self
            .extras
            .rasters
            .rate(node.id)
            .or_else(|| crate::effects::raster::rate(node).filter(|_| !timed));
        let rate = crate::clock::rate(node, timed, raster);
        let (time, next) = match rate {
            Some(rate) => {
                let (cx, next) = self.anim.time_of(node.id, rate);
                (Some(cx), next)
            }
            None => (None, None),
        };
        let clock = rate.map(|_| crate::clock::Clock { next });
        let scope = TokenScope::new(&tokens).with_time(time);
        let mut props: Vec<(Prop, Cow<'a, PropValue>)> = node
            .props
            .iter()
            .filter(|e| e.prop != Prop::Tokens)
            .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
            .collect();
        // A node layout did not place (a list row out of view) draws
        // nothing.
        let laid = if root {
            parent
        } else {
            match self.boxes.rects.get(&node.id) {
                Some(r) => *r,
                None => return Rect::default(),
            }
        };
        // Springs: this frame's values of the props in flight.
        let inherited = inh.color.unwrap_or_else(|| default_color(&scope));
        // (M4) A transition mask, decided before the springs so a ghost
        // it needs is kept (`crate::effects::transition`).
        let transition_mask = self.anim.reveal(self.tree, node, &scope);
        self.anim.stagger(self.tree, node, &scope);
        self.anim
            .paint(node, &mut props, &scope, inherited, Some(laid), parent);
        self.anim
            .keyframes(node, &mut props, inherited, Some(laid), parent);
        self.anim.lean(
            node,
            &mut props,
            &scope,
            inherited,
            self.pointer,
            laid,
            self.logical,
        );
        let inert = inh.inert || self.tree.is_ghost(node.id);
        let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());

        // Inherited props, as the children see them.
        let own_color = match get(Prop::Color) {
            Some(PropValue::Color(c)) => Some(*c),
            _ => inh.color,
        };
        let (own_font, mut weight) = match get(Prop::Font) {
            Some(PropValue::Font(f)) => (Some(sane_font(f.clone())), None),
            _ => (inh.font.clone(), inh.weight),
        };
        if let Some(w) = number(get(Prop::Weight)) {
            weight = Some(w.clamp(1.0, 1000.0) as u16);
        }
        // What this node draws with; theme defaults come from its scope
        // (text, the widgets that draw labels, tracks and fills, and a
        // symbolic icon, which an `image` of an icon name can resolve to
        // too: freedesktop symbolic icons are always drawn in the
        // foreground colour).
        let is_text = matches!(
            node.kind,
            NodeKind::Text | NodeKind::Button | NodeKind::Input
        );
        let themed = is_text
            || matches!(
                node.kind,
                NodeKind::Segmented
                    | NodeKind::Meter
                    | NodeKind::Arc
                    | NodeKind::Effect
                    | NodeKind::Particles
                    | NodeKind::Slider
                    | NodeKind::Icon
                    | NodeKind::Image
            );
        let color = if themed {
            own_color.unwrap_or_else(|| default_color(&scope))
        } else {
            Color::BLACK
        };
        let mut font = if themed {
            own_font.clone().unwrap_or_else(|| default_font(&scope))
        } else {
            Font::default()
        };
        if let Some(w) = weight {
            font.weight = w;
        }

        // Geometry: the laid-out box, moved by the paint offsets (`x`,
        // `y`, and a FLIP glide) of this node and its ancestors. A
        // percentage is of the parent's box, as CSS insets are.
        let glide = self.anim.offset(node.id);
        let own = (
            length(get(Prop::X), parent.w).unwrap_or(0.0) + glide.0,
            length(get(Prop::Y), parent.h).unwrap_or(0.0) + glide.1,
        );
        let offset = (inh.offset.0 + own.0, inh.offset.1 + own.1);
        let rect = LogicalRect::new(
            laid.x + offset.0,
            laid.y + offset.1,
            laid.w.max(0.0),
            laid.h.max(0.0),
        );

        // An `input` shows its `text` (bullets for `type: password`), or
        // its `placeholder` in `$fg.muted` while that is empty (plain
        // text: no markup or marks), with its caret and selection while
        // it has focus.
        let input = node.kind == NodeKind::Input;
        let button = node.kind == NodeKind::Button;
        let widgets = &self.extras.widgets;
        let own_text = match get(Prop::Text) {
            Some(PropValue::Text(t)) => t.as_str(),
            _ => "",
        };
        let placeholder = input && own_text.is_empty();
        let password =
            input && matches!(get(Prop::InputType), Some(PropValue::Keyword(k)) if k == "password");
        // Only a password input copies its text (into bullets); the map
        // from text to shown offsets is the identity otherwise (a boxed
        // zero-sized closure: no allocation).
        let (masked, mask_map) = crate::widgets::shown_text(own_text, password);
        let masked = masked.map(PropValue::Text);
        let caret = (input && widgets.focused.contains(&node.id)).then(|| {
            widgets
                .carets
                .get(&node.id)
                .copied()
                .unwrap_or(crate::widgets::Caret::at(own_text.len()))
                .clamped(own_text)
        });
        let text_color = color;
        let color = if placeholder {
            match scope.lookup("fg.muted") {
                Some(PropValue::Color(c)) => c,
                _ => color.with_alpha(color.a * 0.6),
            }
        } else {
            color
        };
        let centre = PropValue::Keyword("center".into());
        let text_get = |p: Prop| match p {
            Prop::Text if placeholder => get(Prop::Placeholder),
            Prop::Text if password => masked.as_ref(),
            Prop::Markup | Prop::Marks | Prop::Ellipsis | Prop::MaxLines if input => None,
            // A button's label is centred unless it says otherwise.
            Prop::Align if button && get(Prop::Align).is_none() => Some(&centre),
            _ => get(p),
        };
        // Text sits in the content box (inside `pad`; a button pads its
        // label unless it says otherwise).
        let pad = if is_text {
            get(Prop::Pad)
                .and_then(PropValue::insets)
                .or(button.then_some(crate::widgets::BUTTON_PAD))
                .unwrap_or_default()
        } else {
            Insets::default()
        };
        let content = LogicalRect::new(
            rect.x + pad.left,
            rect.y + pad.top,
            (rect.w - pad.left - pad.right).max(0.0),
            (rect.h - pad.top - pad.bottom).max(0.0),
        );
        // Text: the unbounded layout (layout measures with it) and, for a
        // box narrower than it, one shaped for the box's width.
        let mut layout = None;
        // Its span colours (marks, markup links), by slot.
        let mut span_colors = Vec::new();
        // An `input`'s text that is wider than its box is clipped to it.
        let mut clip_text = false;
        // An `input`'s caret and selection: their x in its text layout
        // (logical), and the layout's shift in the box.
        let mut caret_at: Option<(Arc<TextLayout>, f32)> = None;
        if is_text
            && let Some((natural, align, colors)) =
                natural_spec(&text_get, &scope, &font, self.scale)
        {
            let shaped: &[Shaped] = self.layouts.get(&node.id).map_or(&[], Vec::as_slice);
            let (fit, placed) = if input {
                // One line, never wrapped: wider than the box, it is
                // clipped and shifted so the caret stays in view (its end,
                // where typing happens, without focus).
                let placed = pick(shaped, self.scale, None)
                    .or_else(|| {
                        shaped
                            .iter()
                            .find(|c| c.part == 0)
                            .map(|c| c.layout.clone())
                    })
                    .map(|l| {
                        let end = (content.w - l.size.w).min(0.0);
                        let dx = match caret.filter(|_| !placeholder) {
                            Some(c) => {
                                let x = caret_x(&l, mask_map(c.pos));
                                (content.w - crate::widgets::CARET_WIDTH - x)
                                    .min(0.0)
                                    .max(end - crate::widgets::CARET_WIDTH)
                                    .min(0.0)
                            }
                            None => end,
                        };
                        clip_text = dx < 0.0;
                        caret_at = Some((l.clone(), dx));
                        (l, dx, 0.0)
                    });
                (None, placed)
            } else {
                place_text(shaped, self.scale, content, align)
            };
            self.out.text.push((node.id, natural.clone()));
            if let Some(w) = fit {
                self.out.text.push((
                    node.id,
                    TextSpec {
                        style: TextStyle {
                            align,
                            ..natural.style
                        },
                        max_width: Some(w),
                        ..natural
                    },
                ));
            }
            layout = placed.map(|(l, dx, dy)| (l, dx + pad.left, dy + pad.top));
            span_colors = colors;
        }
        // A focused `input` with nothing typed still shows its caret.
        if input
            && !placeholder
            && let Some((l, dx)) = &caret_at
        {
            self.out
                .inputs
                .insert(node.id, (l.clone(), rect.x + pad.left + dx));
        }
        let caret_at: Option<CaretAt> = caret_at
            .map(|(l, dx)| (Some(l), dx + pad.left, pad.top))
            .or_else(|| caret.is_some().then_some((None, pad.left, pad.top)));

        let phys = self.scale.snap_rect(rect);
        let frame = kurbo_rect(phys);
        // A hidden node's clock stops (its subtree is not visited, so
        // theirs do too), unless what hides it follows time.
        let follows = |p: Prop| {
            node.get(p).is_some_and(|v| {
                v.reads_time_with(&|t| global.time_reads(t)) || timed_scope && v.has_tokens()
            })
        };
        let opacity = number(get(Prop::Opacity)).unwrap_or(1.0).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            if follows(Prop::Opacity) {
                self.run_clock(node.id, clock);
            }
            return Rect::default();
        }

        // `scale` and `rotate` about the box's centre: the subtree is drawn
        // under a transform, and its damage is the transformed bounds.
        let zoom = number(get(Prop::Scale)).unwrap_or(1.0).clamp(0.0, 1000.0);
        if zoom <= 0.0 {
            if follows(Prop::Scale) {
                self.run_clock(node.id, clock);
            }
            return Rect::default();
        }
        let turn = angle(get(Prop::Rotate)).unwrap_or(0.0) % 360.0;
        let saved = self.xform;
        let transform_group = (zoom != 1.0 || turn != 0.0).then(|| {
            let c = frame.center();
            let local = kurbo::Affine::translate(c.to_vec2())
                * kurbo::Affine::rotate((turn as f64).to_radians())
                * kurbo::Affine::scale(zoom as f64)
                * kurbo::Affine::translate(-c.to_vec2());
            self.xform = saved * local;
            self.marker(Item::PushTransform(self.xform))
        });

        let mut sig = DefaultHasher::new();
        (inh.ctx, node.kind, node.epoch).hash(&mut sig);
        hash_f32(&mut sig, opacity);
        for v in self.xform.as_coeffs() {
            v.to_bits().hash(&mut sig);
        }
        let mut ink = Rect::default();

        let opacity_group = (opacity < 1.0).then(|| self.marker(Item::PushOpacity(opacity)));
        // (M4) Group effects: a layer around the node and its subtree,
        // whose damage grows by their reach.
        let effects = crate::effects::group(get, rect.w, rect.h, self.scale.as_f32());
        let own_reach = effects
            .as_deref()
            .map_or(0, |e| crate::layers::reach_px(e, self.scale.as_f32()));
        let reach = inh.reach.saturating_add(own_reach);
        if let Some(e) = &effects {
            crate::layers::hash_effects(&mut sig, e);
        }
        let layer_group = effects.map(|effects| {
            self.marker(Item::PushLayer(Arc::new(crate::layers::Layer {
                effects,
                frame,
                scale: self.scale.as_f32(),
                xform: self.xform,
            })))
        });
        // (M4) The transition mask over the node and its subtree.
        let mask_group = transition_mask.map(|m| {
            let mut h = DefaultHasher::new();
            format!("{m:?}").hash(&mut h);
            h.finish().hash(&mut sig);
            match m.drawn(frame, s) {
                crate::effects::transition::Drawn::Clip(path) => {
                    (self.marker(Item::PushClip(path)), Item::PopClip)
                }
                crate::effects::transition::Drawn::Blur { radius, opacity } => {
                    let mut effects = vec![strand_scene::Effect::Opacity(opacity)];
                    if radius > 0.0 {
                        effects.push(strand_scene::Effect::Blur { radius });
                    }
                    (
                        self.marker(Item::PushLayer(Arc::new(crate::layers::Layer {
                            effects: effects.into(),
                            frame,
                            scale: self.scale.as_f32(),
                            xform: self.xform,
                        }))),
                        Item::PopLayer,
                    )
                }
            }
        });
        let mask_hash = transition_mask.map(|m| format!("{m:?}"));
        // Widgets' default radius: `$radius.md` for buttons and segmented
        // controls, a pill for meters.
        let default_radius = match node.kind {
            NodeKind::Button | NodeKind::Segmented => Some(
                scope
                    .lookup("radius.md")
                    .filter(|v| v.as_number().is_some())
                    .unwrap_or(PropValue::Number(8.0)),
            ),
            NodeKind::Meter => Some(PropValue::Keyword("full".into())),
            _ => None,
        };
        let r = radii(
            corners_of(
                get(Prop::Radius).or(default_radius.as_ref()),
                rect.w,
                rect.h,
            ),
            frame.width(),
            frame.height(),
            s,
        );
        let squircle = matches!(get(Prop::Corners), Some(PropValue::Keyword(k)) if k == "squircle");
        // (M4) `shape:` draws the box as a shape of the library, morphing
        // by spring when it changes; hit testing and shadows round the
        // box by the shape's corner (`crate::shapes`).
        let outline = self.shape_outline(node, &get, &scope, frame);
        let r = match &outline {
            Some(o) => {
                RoundedRectRadii::from_single_radius(o.corner(frame.width(), frame.height()))
            }
            None => r,
        };
        let box_path = match &outline {
            Some(o) => crate::shapes::path(o, frame),
            None => shape_path(frame, r, squircle),
        };
        let has_area = !phys.is_empty();

        // Shadows, under the box.
        if has_area && let Some(PropValue::Shadow(list)) = get(Prop::Shadow) {
            for sh in list {
                self.shadow(sh, frame, &r, &box_path, &mut sig, &mut ink);
            }
        }
        // (M4) `glow:`: a box glows as a shadow under it; text, icons and
        // images glow their own pixels (`crate::effects::light`).
        let glow = crate::effects::light::Glow::of(get(Prop::Glow), color);
        let glows_content = matches!(node.kind, NodeKind::Text | NodeKind::Icon | NodeKind::Image);
        // (`particles` glow their sprites: `crate::effects::particles`.)
        let glows_box = !glows_content && node.kind != NodeKind::Particles;
        if has_area
            && glows_box
            && let Some(g) = glow
        {
            self.shadow(&g.shadow(), frame, &r, &box_path, &mut sig, &mut ink);
        }
        // (M4) `backdrop:`: what is drawn behind the box, filtered, in its
        // outline under its background (`crate::backdrop`); its damage
        // follows what is behind it.
        if has_area
            && !inert
            && let Some(e) =
                get(Prop::Backdrop).and_then(|v| crate::backdrop::effect(v, self.scale.as_f32()))
        {
            let read = map_rect(self.xform, phys)
                .inflate(crate::backdrop::reach_of(&e, self.scale))
                .intersect(self.surface)
                .unwrap_or_default();
            let end = self.out.items.len();
            crate::backdrop::hash_behind(&self.out.items, end, read, &mut sig);
            crate::layers::hash_effects(&mut sig, std::slice::from_ref(&e));
            self.push(Item::PushClip(box_path.clone()), phys, &mut sig, &mut ink);
            self.push(
                Item::PushLayer(Arc::new(crate::layers::Layer {
                    effects: Arc::from([e]),
                    frame,
                    scale: self.scale.as_f32(),
                    xform: self.xform,
                })),
                phys,
                &mut sig,
                &mut ink,
            );
            self.push(Item::PopLayer, phys, &mut sig, &mut ink);
            self.push(Item::PopClip, phys, &mut sig, &mut ink);
        }
        let light = crate::effects::light::BoxLight {
            frame,
            path: &box_path,
            scale: s,
            xform: self.xform,
        };
        // `blur: N` asks the compositor to blur behind the box. Until a
        // compositor does (M4), the tint fallback raises the background's
        // alpha by 0.15 so text over it stays readable (`blur_fallback:
        // none` keeps it as written).
        let blur = number(get(Prop::Blur)).filter(|b| *b > 0.0);
        let tint = blur.is_some()
            && !self.extras.compositor_blur
            && !matches!(get(Prop::BlurFallback), Some(PropValue::Keyword(k)) if k == "none");
        if let Some(radius) = blur
            && has_area
            && !inert
        {
            self.out.blur.push(BlurRegion {
                rect: map_rect(self.xform, phys)
                    .intersect(inh.clip)
                    .unwrap_or_default(),
                radii: [
                    r.top_left as f32,
                    r.top_right as f32,
                    r.bottom_right as f32,
                    r.bottom_left as f32,
                ],
                radius: radius.min(MAX_BLUR),
            });
        }
        // Background. A button and a segmented control without one get
        // `$surface.hi` (the label colour at 10 % without it); a meter's
        // is its track.
        // A bar that names none is themed: `$surface` (an edge strip;
        // panels, OSDs and popups stay clear around their content, which
        // carries its own `bg`).
        let default_bg = match node.kind {
            NodeKind::Bar if root => match scope.lookup("surface") {
                Some(PropValue::Color(c)) => Some(Paint::Solid(c)),
                _ => None,
            },
            NodeKind::Button | NodeKind::Segmented => {
                Some(Paint::Solid(match scope.lookup("surface.hi") {
                    Some(PropValue::Color(c)) => c,
                    _ => text_color.alpha(0.1),
                }))
            }
            NodeKind::Meter => {
                Some(paint_of(get(Prop::Track)).unwrap_or(Paint::Solid(text_color.alpha(0.15))))
            }
            _ => None,
        };
        if has_area
            && let Some(paint) = paint_of(get(Prop::Bg))
                .or(default_bg)
                .map(|p| if tint { tinted(p) } else { p })
        {
            if root
                && opacity >= 1.0
                && layer_group.is_none()
                && saved == self.xform
                && self.xform == kurbo::Affine::IDENTITY
                && opaque_paint(&paint)
            {
                self.out.opaque = opaque_bands(phys, &r).clipped(self.surface);
            }
            let fillet = self.fillet.filter(|f| f.node == node.id);
            let (shape, reach) = if let Some(f) = fillet {
                // `attach:`: square on the edge, with its fillets.
                let path = crate::fillet::path(frame, r, f.edge);
                let reach = cover(path.bounding_box()).union(phys);
                (FillShape::Path(path), reach)
            } else if radii_zero(&r) {
                (FillShape::Rect(frame), phys)
            } else {
                (FillShape::Path(box_path.clone()), phys)
            };
            self.push(
                Item::Fill {
                    shape,
                    paint,
                    frame,
                },
                reach,
                &mut sig,
                &mut ink,
            );
        }
        // (M4) A CPU raster node's pixels at its clock's tick, over its
        // background: a source of its own, or one its props build
        // (`grain:`), clipped to its shape.
        let source_color = match get(Prop::Color) {
            Some(PropValue::Color(c)) => *c,
            _ => match scope.lookup("accent") {
                Some(PropValue::Color(c)) => c,
                _ => text_color,
            },
        };
        let built = crate::effects::raster::built(node.kind, get, source_color);
        let (pw, ph) = (frame.width().round() as u32, frame.height().round() as u32);
        let time_now = time.unwrap_or_default();
        let raster = match &built {
            _ if !has_area => None,
            Some(b) => self.extras.rasters.pixmap_from(
                node.id,
                b,
                b.config(),
                pw,
                ph,
                self.scale.as_f32(),
                b.time(time_now),
            ),
            None => {
                self.extras.rasters.unused(node.id);
                self.extras
                    .rasters
                    .pixmap(node.id, pw, ph, self.scale.as_f32(), time_now)
            }
        };
        if let Some((key, pixmap)) = raster {
            let clipped = !radii_zero(&r) || outline.is_some();
            if clipped {
                self.push(Item::PushClip(box_path.clone()), phys, &mut sig, &mut ink);
            }
            let rect = kurbo::Rect::new(
                frame.x0.round(),
                frame.y0.round(),
                frame.x0.round() + pixmap.width() as f64,
                frame.y0.round() + pixmap.height() as f64,
            );
            self.push(
                Item::Raster {
                    node: node.id,
                    key,
                    pixmap,
                    rect,
                },
                phys,
                &mut sig,
                &mut ink,
            );
            if clipped {
                self.push(Item::PopClip, phys, &mut sig, &mut ink);
            }
        }
        // (M4) Inner shadows, over the background.
        if has_area && let Some(PropValue::Shadow(list)) = get(Prop::InnerShadow) {
            for (item, bounds) in crate::effects::light::inner_shadows(list, light) {
                self.push(item, bounds, &mut sig, &mut ink);
            }
        }
        // Border, drawn inside the box.
        if has_area
            && let Some(PropValue::Border(Border { width, paint })) = get(Prop::Border)
            && let Some(width) = finite(*width)
            && width > 0.0
        {
            let bw = (width as f64 * s).round().max(1.0);
            let inner = frame.inflate(-bw, -bw);
            let mut path = box_path.clone();
            if inner.width() > 0.0 && inner.height() > 0.0 {
                let ir = RoundedRectRadii::new(
                    (r.top_left - bw).max(0.0),
                    (r.top_right - bw).max(0.0),
                    (r.bottom_right - bw).max(0.0),
                    (r.bottom_left - bw).max(0.0),
                );
                match &outline {
                    Some(o) => path.extend(crate::shapes::path(o, inner)),
                    None => path.extend(shape_path(inner, ir, squircle)),
                }
            }
            self.push(
                Item::Border {
                    path,
                    paint: paint.clone(),
                    frame,
                },
                phys,
                &mut sig,
                &mut ink,
            );
        }
        // (M4) `stroke:` with its styles (`crate::shapes::stroke`), along
        // the outline inset by half its width, so it is drawn inside the
        // box as a border is.
        if has_area
            && node.kind != NodeKind::Arc
            && let Some(PropValue::Border(Border { width, paint })) = get(Prop::Stroke)
            && let Some(width) = finite(*width)
            && width > 0.0
        {
            let sw = (width as f64 * s).min(frame.width().min(frame.height()));
            let half = sw / 2.0;
            let mid = frame.inflate(-half, -half);
            let mr = RoundedRectRadii::new(
                (r.top_left - half).max(0.0),
                (r.top_right - half).max(0.0),
                (r.bottom_right - half).max(0.0),
                (r.bottom_left - half).max(0.0),
            );
            let center = match &outline {
                Some(o) => crate::shapes::path(o, mid),
                None => shape_path(mid, mr, squircle),
            };
            let style =
                crate::shapes::stroke::style_of(get, sw, s, crate::shapes::stroke::Cap::Butt);
            if let Some(path) = crate::shapes::stroke::outline(&center, &style) {
                let b = cover(path.bounding_box());
                self.push(
                    Item::Fill {
                        shape: FillShape::Path(path),
                        paint: paint.clone(),
                        frame,
                    },
                    b,
                    &mut sig,
                    &mut ink,
                );
            }
        }
        // (M4) A rim light, over the border.
        if has_area {
            for (item, bounds) in crate::effects::light::rim(get(Prop::Rim), light) {
                self.push(item, bounds, &mut sig, &mut ink);
            }
        }
        // Widgets: a button's hover and press state layer, a meter's fill,
        // a slider's track and knob, a segmented control's options, an
        // input's selection.
        if has_area {
            let wctx = WidgetCtx {
                node,
                frame,
                radii: r,
                box_path: &box_path,
                color: text_color,
                scope: &scope,
                font: &font,
            };
            self.widget(&wctx, &get, caret, &caret_at, &mask_map, &mut sig, &mut ink);
        }
        // What text, an icon or an image draws from here glows.
        let content_start = self.out.items.len();
        // An `icon` or `image`: decoded at the box's size.
        if has_area && matches!(node.kind, NodeKind::Icon | NodeKind::Image) {
            self.image(
                node, &get, frame, phys, &box_path, &r, text_color, &mut sig, &mut ink,
            );
        }
        // Text. (M4) `fill:` paints its glyphs, `text_stroke:` outlines
        // them (an offscreen group under them), and a `letters` child
        // draws each letter on its own (`crate::effects::letters`).
        let mut glyph_cells: Option<(DefaultHasher, Vec<(Rect, u64)>)> = None;
        let text_fill = if is_text {
            paint_of(get(Prop::Fill))
        } else {
            None
        };
        let text_stroke = if is_text {
            crate::effects::light::text_stroke(get(Prop::TextStroke))
        } else {
            None
        };
        if let Some((l, dx, dy)) = layout {
            // A layout from another scale is drawn resampled (see raster).
            let x = phys.x + (dx as f64 * s).round().clamp(-1e7, 1e7) as i32;
            let y = phys.y + (dy as f64 * s).round().clamp(-1e7, 1e7) as i32;
            let k = self.scale.as_f64() / l.scale.as_f64();
            let bounds = if k == 1.0 {
                l.ink.translate(x, y)
            } else {
                cover(kurbo::Rect::new(
                    x as f64 + l.ink.left() as f64 * k,
                    y as f64 + l.ink.top() as f64 * k,
                    x as f64 + l.ink.right() as f64 * k,
                    y as f64 + l.ink.bottom() as f64 * k,
                ))
                .inflate(1)
            };
            let bounds = if clip_text {
                bounds.intersect(phys).unwrap_or_default()
            } else {
                bounds
            };
            let clip = (clip_text && !bounds.is_empty())
                .then(|| self.marker(Item::PushClip(frame.to_path(0.1))));
            if let Some(i) = clip {
                self.out.items[i].bounds = bounds;
            }
            if !bounds.is_empty() {
                // (M4) `roll: true`: changed letters roll
                // (`crate::effects::roll`).
                let roll = if letters.is_none()
                    && matches!(get(Prop::Roll), Some(PropValue::Bool(true)))
                {
                    let transition = node
                        .props
                        .iter()
                        .find(|e| e.prop == Prop::Roll)
                        .map_or(strand_scene::Transition::Default, |e| e.transition.clone());
                    let curve = strand_scene::Curve::of(&scope.transition(&transition, Prop::Y));
                    self.anim.roll(node.id, &l, curve)
                } else {
                    self.anim.forget_roll(node.id);
                    None
                };
                let lines: Vec<(Rect, Color)> = l
                    .runs
                    .iter()
                    .filter_map(|r| Some((r.underline?, slot_color(r.color, &span_colors, color))))
                    .collect();
                // Glyph cells, when the glyphs are the last thing drawn
                // and land on buffer pixels as they are.
                if lines.is_empty()
                    && caret.is_none()
                    && k == 1.0
                    && self.xform == kurbo::Affine::IDENTITY
                    && text_fill.is_none()
                    && text_stroke.is_none()
                    && letters.is_none()
                    && roll.is_none()
                {
                    let mut rest = sig.clone();
                    (x, y).hash(&mut rest);
                    let cells = l
                        .runs
                        .iter()
                        .flat_map(|r| {
                            let c = slot_color(r.color, &span_colors, color);
                            r.glyphs.iter().map(move |g| (g, c))
                        })
                        .map(|(g, c)| {
                            let b = Rect::new(
                                x.saturating_add(g.x).saturating_sub(1),
                                y.saturating_add(g.y).saturating_sub(1),
                                u32::from(g.slot.w) + 3,
                                u32::from(g.slot.h) + 3,
                            );
                            let mut h = DefaultHasher::new();
                            (g.slot.page, g.slot.x, g.slot.y, g.slot.w, g.slot.h).hash(&mut h);
                            hash_color(&mut h, &c);
                            (b.intersect(bounds).unwrap_or_default(), h.finish())
                        })
                        .collect();
                    glyph_cells = Some((rest, cells));
                }
                let fill = text_fill.clone().map(|paint| {
                    Arc::new(super::GlyphFill {
                        paint,
                        frame,
                        area: kurbo_rect(bounds).union(frame),
                    })
                });
                if let Some((old, new, p)) = &roll {
                    let clip = kurbo_rect(phys).to_path(TOLERANCE);
                    self.push(Item::PushClip(clip), phys, &mut sig, &mut ink);
                    let h = (new.size.h as f64 * s).max(1.0);
                    let each =
                        crate::effects::roll::items(old, new, *p, (x, y), h, color, &span_colors);
                    for (item, b) in each {
                        let b = b.intersect(phys).unwrap_or_default();
                        self.push(item, b, &mut sig, &mut ink);
                    }
                    self.push(Item::PopClip, phys, &mut sig, &mut ink);
                }
                match letters {
                    _ if roll.is_some() => {}
                    None => self.push(
                        Item::Glyphs {
                            x,
                            y,
                            layout: l,
                            color,
                            spans: span_colors,
                            fill,
                        },
                        bounds,
                        &mut sig,
                        &mut ink,
                    ),
                    Some(lt) => {
                        let each = crate::effects::letters::items(
                            lt,
                            &scope,
                            &l,
                            (x, y),
                            color,
                            &span_colors,
                            fill,
                            s,
                            self.xform,
                        );
                        for (item, b) in each {
                            self.push(item, b, &mut sig, &mut ink);
                        }
                    }
                }
                for (u, c) in lines {
                    let r = kurbo::Rect::new(
                        x as f64 + u.left() as f64 * k,
                        y as f64 + u.top() as f64 * k,
                        x as f64 + u.right() as f64 * k,
                        y as f64 + u.bottom() as f64 * k,
                    );
                    self.push(
                        Item::Fill {
                            shape: FillShape::Rect(r),
                            paint: Paint::Solid(c),
                            frame: r,
                        },
                        cover(r),
                        &mut sig,
                        &mut ink,
                    );
                }
            }
            if clip.is_some() {
                self.marker(Item::PopClip);
            }
        }
        if let Some(effects) = text_stroke
            && let Some((bounds, effects)) = crate::effects::light::under_content(
                &mut self.out.items,
                content_start,
                effects,
                (frame, self.scale.as_f32(), self.xform),
                self.surface,
            )
        {
            crate::layers::hash_effects(&mut sig, &effects);
            ink = ink.union(bounds);
        }
        if glows_content
            && let Some(g) = glow
            && let Some((bounds, effects)) = crate::effects::light::under_content(
                &mut self.out.items,
                content_start,
                g.effects(),
                (frame, self.scale.as_f32(), self.xform),
                self.surface,
            )
        {
            // The glyphs are no longer the last thing drawn.
            glyph_cells = None;
            crate::layers::hash_effects(&mut sig, &effects);
            ink = ink.union(bounds);
        }
        // An input's caret, over its text.
        if has_area && let (Some(c), Some(at)) = (caret, &caret_at) {
            self.caret(
                frame,
                at,
                mask_map(c.pos),
                &scope,
                text_color,
                &font,
                &mut sig,
                &mut ink,
            );
        }

        let mut bounds = ink.intersect(inh.clip).unwrap_or_default();
        if reach > 0 && !bounds.is_empty() {
            bounds = bounds
                .inflate(reach)
                .intersect(self.surface)
                .unwrap_or_default();
        }
        self.out.records.insert(
            node.id,
            NodeRecord {
                bounds,
                sig: sig.finish(),
                glyphs: glyph_cells.map(|(rest, cells)| {
                    Arc::new(GlyphCells {
                        rest: rest.finish(),
                        cells,
                    })
                }),
            },
        );

        // Hit shape: the rounded box, grown by `hit: grow(n)`.
        let grow = match get(Prop::Hit) {
            Some(PropValue::Call { name, args }) if name == "grow" => {
                args.first().and_then(|a| number(Some(a))).unwrap_or(0.0)
            }
            _ => 0.0,
        }
        .clamp(0.0, 1000.0) as f64
            * s;
        if !inert {
            let grown = frame.inflate(grow, grow);
            let radii = RoundedRectRadii::new(
                r.top_left + grow,
                r.top_right + grow,
                r.bottom_right + grow,
                r.bottom_left + grow,
            );
            // Transformed: the untransformed shape, hit through the
            // inverse (a degenerate transform is never hit).
            let inverse = (self.xform != kurbo::Affine::IDENTITY).then(|| {
                if self.xform.determinant().abs() > 1e-12 {
                    self.xform.inverse()
                } else {
                    kurbo::Affine::translate((f64::INFINITY, f64::INFINITY))
                }
            });
            self.out.hits.push(HitBox {
                node: node.id,
                rect: grown,
                radii,
                inverse,
                clip: inh.clip,
            });
        }

        // Children. A `scroll`, `list` or `pages` always clips its content
        // (a page slides in from outside it), and so
        // does a node whose size springs (a toast collapsing to `height:
        // 0`).
        let clips = matches!(get(Prop::Clip), Some(PropValue::Bool(true)))
            || matches!(
                node.kind,
                NodeKind::Scroll | NodeKind::List | NodeKind::Pages
            )
            || self.anim.sizing(node.id);
        let mut ctx = DefaultHasher::new();
        (inh.ctx, node.epoch).hash(&mut ctx);
        hash_f32(&mut ctx, opacity);
        if let Some(i) = layer_group
            && let Item::PushLayer(l) = &self.out.items[i].item
        {
            crate::layers::hash_effects(&mut ctx, &l.effects);
        }
        mask_hash.hash(&mut ctx);
        let mut child_clip = inh.clip;
        let mut clip_group = None;
        if clips {
            hash_path(&mut ctx, &box_path);
            child_clip = map_rect(self.xform, phys)
                .intersect(inh.clip)
                .unwrap_or_default();
            clip_group = Some(self.marker(Item::PushClip(box_path)));
        }
        let child_inh = Inherited {
            color: own_color,
            font: own_font,
            weight,
            tokens,
            ctx: ctx.finish(),
            clip: child_clip,
            offset,
            inert,
            timed: timed_scope,
            reach,
        };
        let mut children = Rect::default();
        if !(clips && child_clip.is_empty()) {
            for c in &node.children {
                // A nested surface (a popup) paints on its own surface.
                if let Some(child) = self
                    .tree
                    .get(*c)
                    .filter(|n| !crate::layout::out_of_flow(n.kind))
                {
                    children = children.union(self.node(child, rect, &child_inh, false));
                }
            }
        }
        if let Some(i) = clip_group {
            self.out.items[i].bounds = children;
            self.marker(Item::PopClip);
        }
        let subtree = bounds.union(children);
        if let Some(i) = layer_group {
            self.out.items[i].bounds = subtree;
            self.marker(Item::PopLayer);
        }
        if let Some((i, pop)) = mask_group {
            self.out.items[i].bounds = subtree;
            self.marker(pop);
        }
        // Drawn: its clock runs, unless all it draws is outside the clip
        // and nothing that places it follows time (it stays out). A
        // built-in `effect` or `particles` draws its raster in its box
        // (`crate::effects::raster`), so it counts as drawn only when that
        // is inside the clip.
        let moves = [Prop::X, Prop::Y, Prop::Scale, Prop::Rotate, Prop::Shadow]
            .into_iter()
            .any(follows);
        if !subtree.is_empty() || moves {
            self.run_clock(node.id, clock);
        }
        if let Some(i) = opacity_group {
            self.out.items[i].bounds = subtree;
            self.marker(Item::PopOpacity);
        }
        if let Some(i) = transform_group {
            self.out.items[i].bounds = subtree;
            self.marker(Item::PopTransform);
            self.xform = saved;
        }
        subtree
    }

    /// (M4) The outline `node` draws its box as, when it has a `shape:`
    /// (`crate::shapes`); a node without one forgets any it had.
    fn shape_outline<'v>(
        &mut self,
        node: &Node,
        get: &impl Fn(Prop) -> Option<&'v PropValue>,
        scope: &TokenScope,
        frame: kurbo::Rect,
    ) -> Option<crate::shapes::Outline> {
        let shape = match get(Prop::Shape) {
            Some(PropValue::Keyword(k) | PropValue::Text(k)) => crate::shapes::Shape::from_name(k),
            _ => None,
        };
        let Some(shape) = shape else {
            self.anim.forget_shape(node.id);
            return None;
        };
        let transition = node
            .props
            .iter()
            .find(|e| e.prop == Prop::Shape)
            .map_or(strand_scene::Transition::Default, |e| e.transition.clone());
        let curve = strand_scene::Curve::of(&scope.transition(&transition, Prop::Shape));
        Some(
            self.anim
                .shape_outline(node.id, shape, crate::shapes::aspect(frame), curve),
        )
    }

    pub(super) fn shadow(
        &mut self,
        sh: &Shadow,
        frame: kurbo::Rect,
        r: &RoundedRectRadii,
        box_path: &BezPath,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        if sh.color.a.is_nan() || sh.color.a <= 0.0 {
            return;
        }
        let spread = finite_or_zero(sh.spread) as f64 * s;
        let (dx, dy) = (
            finite_or_zero(sh.x) as f64 * s,
            finite_or_zero(sh.y) as f64 * s,
        );
        let rect = frame
            .with_origin((frame.x0 + dx, frame.y0 + dy))
            .inflate(spread, spread);
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        let limit = rect.width().min(rect.height()) / 2.0;
        let corner = |v: f64| (v + spread).min(limit).max(0.0) as f32;
        let radii = [
            corner(r.top_left),
            corner(r.top_right),
            corner(r.bottom_right),
            corner(r.bottom_left),
        ];
        // CSS blur radius is twice the Gaussian standard deviation.
        let blur = finite_or_zero(sh.blur).clamp(0.0, MAX_BLUR) as f64;
        let std_dev = blur * s / 2.0;
        let reach = (3.0 * std_dev).ceil() + 1.0;
        let extent = rect.inflate(reach, reach);
        let mut clip = extent.to_path(TOLERANCE);
        clip.extend(box_path.iter());
        self.push(
            Item::Shadow {
                rect,
                radii,
                std_dev: std_dev as f32,
                color: sh.color,
                clip,
                extent,
            },
            cover(extent),
            sig,
            ink,
        );
    }
}
