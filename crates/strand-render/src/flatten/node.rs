//! Flattening one node: its box, background, border, shadows, text,
//! clips and transforms, then its children.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use strand_scene::{
    BlurRegion, Border, Color, Edge, Font, Insets, LogicalRect, NodeKind, Paint, Prop, PropValue,
    Rect, Shadow, TokenScope,
};
use strand_text::{TextLayout, TextStyle};
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii, Shape};

use super::hash::{hash_color, hash_f32, hash_item, hash_path};
use super::paint::{
    corners_of, cover, kurbo_rect, opaque_bands, opaque_paint, paint_of, radii, radii_zero,
    shape_path, tinted,
};
use super::text::{
    Shaped, TextSpec, natural_spec, pick, pick_part, place_text, sane_font, slot_color,
};
use super::widget::{CaretAt, WidgetCtx, caret_x};
use super::{
    DisplayItem, FillShape, Flattener, Geometry, GlyphCells, HitBox, Inherited, Item, MAX_BLUR,
    NodeRecord, RingCells, RoundBox, TOLERANCE, angle, default_color, default_font, finite,
    finite_or_zero, length, map_rect, number, painted, painted_cover,
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
        // (damage-fraction) Every pixel it paints, as far as the clips
        // it is drawn under let it.
        let reach = painted(&item).map(|p| match self.item_clips.last() {
            Some(c) => p.intersect(*c),
            None => p,
        });
        let bounds = match reach {
            Some(p) if p.width() > 0.0 && p.height() > 0.0 => bounds.union(painted_cover(p)),
            _ => bounds,
        };
        match &item {
            Item::PushClip(p) => {
                let c = p.bounding_box();
                let c = self.item_clips.last().map_or(c, |o| o.intersect(c));
                self.item_clips.push(c);
            }
            Item::PopClip => {
                self.item_clips.pop();
            }
            _ => {}
        }
        let bounds = map_rect(self.xform, bounds);
        hash_item(sig, &item);
        *ink = ink.union(bounds);
        self.out.items.push(DisplayItem { item, bounds });
    }

    /// (M4) `id` is drawn, or hidden only by something that follows
    /// time: its clock (if it has one) runs from this frame, and the
    /// surface keeps it; so does its `play` while `playing`.
    fn run_clock(
        &mut self,
        id: strand_scene::NodeId,
        clock: Option<crate::clock::Clock>,
        playing: bool,
    ) {
        if clock.is_some() {
            self.anim.start_clock(id);
        }
        if playing {
            self.anim.play_drawn(id);
        }
        self.out.clocks.extend(clock);
    }

    /// (cross-surface-morph) Sets the bounds of the group marker `i` to
    /// `local`, in the node's own space as its items' are given to
    /// [`Flattener::push`]: drawn under the node's transform, the group
    /// lies where `push` puts its items, so the painter does not pass it
    /// over where the damage holds what it draws.
    pub(super) fn group_bounds(&mut self, i: usize, local: Rect) {
        self.out.items[i].bounds = map_rect(self.xform, local);
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
            || letters.is_some_and(|l| crate::time::reads_time(l, global))
            || node.kind == NodeKind::Svg && self.svg_parts_read_time(node);
        // Its clock: the rate its time props and its own animation run at.
        // (M4) A `shader` node whose code reads `strand.time` has a clock.
        #[cfg(feature = "gpu")]
        let timed = timed
            || (node.kind == NodeKind::Shader
                && matches!(node.get(Prop::Shader),
                    Some(PropValue::Shader(c)) if crate::renderer::backend::reads_time(c)))
            // (M4) A `filter: wobble(…)` the GPU draws moves with time
            // (not once its pass failed: the CPU draws it unfiltered).
            || (self.gpu_draws(node.id, crate::renderer::backend::Slot::Filter)
                && crate::effects::gpu::moves(node));
        // (M4) An animated image's frames run on a clock of their own.
        // (A source built from props follows a clock that time-bound
        // props run at refresh: `crate::effects::raster`.)
        let anim = &mut *self.anim;
        let images = &self.extras.images;
        let own = self.extras.rasters.rate(node.id);
        let frames = own.is_none().then(|| {
            images.frame_rate(node, || {
                anim.time_of(node.id, crate::clock::Rate::Refresh).0.t
            })
        });
        let raster = own
            .or(frames.flatten())
            .or_else(|| crate::effects::raster::rate(node).filter(|_| !timed));
        // (M4) The GPU's aurora is animated (its CPU fallback, also after
        // its pass failed, is still).
        #[cfg(feature = "gpu")]
        let raster = raster.or_else(|| {
            crate::effects::gpu::rate(node)
                .filter(|_| self.gpu_draws(node.id, crate::renderer::backend::Slot::Node) && !timed)
        });
        let rate = crate::clock::rate(node, timed, raster);
        let (time, next) = match rate {
            Some(rate) => {
                let (cx, mut next) = self.anim.time_of(node.id, rate);
                // Its frames' clock wakes at frame changes only.
                if let (Some(Some(crate::clock::Rate::Every(tick))), false, Some(n)) =
                    (frames, timed, next)
                {
                    next = Some(images.frame_wake(node, cx.t, tick, n));
                }
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
        // (M4) A shared-element morph: drawn from where its name was
        // last drawn, moved and scaled (`crate::anim::morph`); decided
        // before the springs, as it replaces the enter pose. Its box is
        // the laid-out one with the paint offsets logic set.
        let shared = self.tree.root_of(node.id).and_then(|root| {
            let raw = |p: Prop, of: f32| {
                length(
                    props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref()),
                    of,
                )
                .unwrap_or(0.0)
            };
            // Where the ancestors are drawn: at rest and in flight.
            let at = LogicalRect::new(
                laid.x + inh.offset.0 + inh.flight.0 + raw(Prop::X, parent.w),
                laid.y + inh.offset.1 + inh.flight.1 + raw(Prop::Y, parent.h),
                laid.w.max(0.0),
                laid.h.max(0.0),
            );
            self.anim.shared_morph(
                node,
                &scope,
                (root, at),
                &self.extras.origins,
                &mut self.out.morphs,
            )
        });
        // (M4) A transition mask, decided before the springs so a ghost
        // it needs is kept (`crate::effects::transition`).
        let transition_mask = self.anim.reveal(self.tree, node, &scope);
        self.anim.stagger(self.tree, node, &scope);
        // The paint offsets as written, and as their springs have them.
        let offsets = |props: &[(Prop, Cow<'a, PropValue>)]| {
            let of = |p: Prop, along: f32| {
                length(
                    props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref()),
                    along,
                )
                .unwrap_or(0.0)
            };
            (of(Prop::X, parent.w), of(Prop::Y, parent.h))
        };
        let written = offsets(&props);
        let opacity_of = |props: &[(Prop, Cow<'a, PropValue>)]| {
            number(
                props
                    .iter()
                    .find(|(q, _)| *q == Prop::Opacity)
                    .map(|(_, v)| v.as_ref()),
            )
            .unwrap_or(1.0)
            .clamp(0.0, 1.0)
        };
        let written_opacity = opacity_of(&props);
        if self.anim.enter_starts(node.id) && self.late_text_under(node) {
            self.anim.hold_enter(node.id);
        }
        self.anim
            .paint(node, &mut props, &scope, inherited, Some(laid), parent);
        let sprung = offsets(&props);
        let sprung_opacity = opacity_of(&props);
        let play = self
            .anim
            .keyframes(node, &mut props, inherited, Some(laid), parent);
        // A prop its `play` sets follows time while it plays.
        let playing = play.as_ref().is_some_and(|(_, moving)| *moving);
        let plays = |p: Prop| {
            play.as_ref()
                .is_some_and(|(k, moving)| *moving && crate::anim::keyframes::sets(k, p))
        };
        // (M4) With a GPU, `tilt` turns the node in 3-D (a bundled pass
        // over its subtree, below) rather than in its plane, once the
        // GPU has answered for it (`Flattener::tilts_in_3d`).
        #[cfg(feature = "gpu")]
        let three_d = self.tilts_in_3d(node.id);
        #[cfg(not(feature = "gpu"))]
        let three_d = false;
        let tilt = self.anim.lean(
            node,
            &mut props,
            &scope,
            inherited,
            self.pointer,
            laid,
            self.logical,
            three_d,
        );
        // (M4) The root paints at rest what the compositor applies.
        if root && self.extras.delegates_pose(node.id) {
            let scale = !self.extras.compositor_pose_scale_off;
            self.out.pose = crate::pose::delegate(node.kind, &mut props, laid, scale);
        }
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
        // foreground colour; a `graph`'s line and a `spectrum`'s bars too).
        let is_text = matches!(
            node.kind,
            NodeKind::Text | NodeKind::Button | NodeKind::Input
        );
        let themed = is_text
            || matches!(
                node.kind,
                NodeKind::Segmented
                    | NodeKind::Canvas
                    | NodeKind::Meter
                    | NodeKind::Arc
                    | NodeKind::Effect
                    | NodeKind::Particles
                    | NodeKind::Slider
                    | NodeKind::Icon
                    | NodeKind::Image
                    | NodeKind::Graph
                    | NodeKind::Spectrum
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
        // A paint offset in flight (a spring's way to its rest, a glide)
        // is drawn whole logical pixels from where it rests, as layout
        // places boxes: a box whose offset springs with its size
        // (centred as it grows) keeps its content still, its content
        // moves as one, and a spring's tail between pixels draws its
        // rest (never a pixel off for a frame as it settles). (seam-snap)
        // Each edge is rounded from where it falls, the box's exact edge
        // (`Boxes::residues`) moved by everything in flight above and
        // here, never the offset and the size apart: two boxes that meet
        // exactly (a card and the fillets springing beside it) meet on
        // every frame. A drag follows the pointer as it is.
        let raw_glide = self.anim.offset(node.id);
        let lifted = self.anim.lifted(node.id);
        let glide = if lifted {
            raw_glide
        } else {
            (raw_glide.0.round(), raw_glide.1.round())
        };
        let flown = |p: Prop, d: f32| if get(p).is_some() { d } else { 0.0 };
        let (fx, fy) = (
            flown(Prop::X, sprung.0 - written.0),
            flown(Prop::Y, sprung.1 - written.1),
        );
        let (gx, gy) = if lifted { (0.0, 0.0) } else { raw_glide };
        let flight = (inh.flight.0 + fx + gx, inh.flight.1 + fy + gy);
        let mut own = (
            length(get(Prop::X), parent.w).unwrap_or(0.0) - fx + (raw_glide.0 - gx),
            length(get(Prop::Y), parent.h).unwrap_or(0.0) - fy + (raw_glide.1 - gy),
        );
        if let Some(m) = shared {
            own = (own.0 + m[0], own.1 + m[1]);
        }
        let (morph_sx, morph_sy) = shared.map_or((1.0, 1.0), |m| (m[2] as f64, m[3] as f64));
        // (M4) A dragged node's squash and stretch, from its drawn offset.
        let jelly = number(get(Prop::Jelly))
            .filter(|a| *a > 0.0)
            .and_then(|a| self.anim.jelly(node, a, glide, &scope))
            .and_then(crate::effects::jelly::matrix);
        let offset = (inh.offset.0 + own.0, inh.offset.1 + own.1);
        let [rl, rt, rr, rb] = self
            .boxes
            .residues
            .get(&node.id)
            .copied()
            .unwrap_or_default();
        let (x, w) = drawn_span(laid.x + offset.0, laid.w.max(0.0), rl, rr, flight.0);
        let (y, h) = drawn_span(laid.y + offset.1, laid.h.max(0.0), rt, rb, flight.1);
        let rect = LogicalRect::new(x, y, w, h);

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
        #[cfg(feature = "eval-hooks")]
        let (mut probe_text, mut probe_fit, mut probe_cut) = (None, None, None);
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
                // (bar-blank-on-exit) A box an `enter` or `exit` pose
                // squeezes keeps its text's line box: its lines are drawn
                // for the width it has at rest, cut to the box, as layout
                // measured them ([`crate::anim::Forced`]'s `hold`).
                let held = self.anim.line_hold(node.id).map(|h| LogicalRect {
                    w: (h - pad.left.max(0.0) - pad.right.max(0.0)).max(0.0),
                    ..content
                });
                let (fit, placed, clip) = place_text(
                    shaped,
                    self.scale,
                    held.unwrap_or(content),
                    align,
                    natural.style.ellipsis.is_some(),
                );
                clip_text = clip || held.is_some();
                (fit, placed)
            };
            #[cfg(feature = "eval-hooks")]
            {
                probe_text = Some(super::probe::text_drawn(shaped, self.scale, fit));
                probe_fit = fit;
                if self.trace.is_some()
                    && let (Some(w), Some((l, _, _))) = (fit, &placed)
                {
                    probe_cut =
                        super::probe::cut(shaped, self.scale, w, l, &natural.text, &natural.style);
                }
            }
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
            plays(p)
                || node.get(p).is_some_and(|v| {
                    v.reads_time_with(&|t| global.time_reads(t)) || timed_scope && v.has_tokens()
                })
        };
        // (eval-sandbox) What moves it this frame, from the locals above.
        #[cfg(feature = "eval-hooks")]
        let mut probe = self.trace.is_some().then(|| {
            super::probe::Seen {
                node: node.id,
                parent: node.parent,
                kind: node.kind,
                laid,
                drawn: rect,
                after: self.anim.motions(node.id),
                written,
                sprung,
                written_opacity,
                sprung_opacity,
                plays: super::probe::axes(&plays),
                playing,
                shared,
                tilt: tilt.is_some(),
                jelly: jelly.is_some(),
                raw_glide,
                lifted,
                reveal: transition_mask.is_some(),
                inert,
                follows: super::probe::axes(&follows),
                inherited: (inh.offset.0 + inh.flight.0, inh.offset.1 + inh.flight.1),
                text: probe_text,
                fit: probe_fit,
            }
            .trace()
        });
        let opacity = number(get(Prop::Opacity)).unwrap_or(1.0).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            if follows(Prop::Opacity) {
                self.run_clock(node.id, clock, playing);
            }
            #[cfg(feature = "eval-hooks")]
            if let Some(t) = probe {
                let look = (opacity, [1.0, 1.0], 0.0);
                let hidden = Some(crate::probe::Hidden::Opacity0);
                self.probe_push(t, look, frame, self.xform, hidden, inh.clip);
            }
            self.hidden_texts(node, own_font.as_ref(), weight, &tokens);
            return Rect::default();
        }

        // `scale` and `rotate` about the box's centre: the subtree is drawn
        // under a transform, and its damage is the transformed bounds.
        let zoom = number(get(Prop::Scale)).unwrap_or(1.0).clamp(0.0, 1000.0);
        if zoom <= 0.0 {
            if follows(Prop::Scale) {
                self.run_clock(node.id, clock, playing);
            }
            #[cfg(feature = "eval-hooks")]
            if let Some(t) = probe {
                let look = (opacity, [0.0, 0.0], 0.0);
                let hidden = Some(crate::probe::Hidden::Scale0);
                self.probe_push(t, look, frame, self.xform, hidden, inh.clip);
            }
            self.hidden_texts(node, own_font.as_ref(), weight, &tokens);
            return Rect::default();
        }
        let turn = angle(get(Prop::Rotate)).unwrap_or(0.0) % 360.0;
        let saved = self.xform;
        let morphs = morph_sx != 1.0 || morph_sy != 1.0;
        let transform_group =
            (zoom != 1.0 || turn != 0.0 || morphs || jelly.is_some()).then(|| {
                let c = frame.center();
                let [ja, jb, jc, jd] = jelly.unwrap_or([1.0, 0.0, 0.0, 1.0]);
                let local = kurbo::Affine::translate(c.to_vec2())
                    * kurbo::Affine::rotate((turn as f64).to_radians())
                    * kurbo::Affine::scale(zoom as f64)
                    * kurbo::Affine::scale_non_uniform(morph_sx, morph_sy)
                    * kurbo::Affine::new([ja, jb, jc, jd, 0.0, 0.0])
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

        // (m6-owner-fixes, bug 17) Its opacity or paint offset in flight:
        // the raster may draw the group once into a fade layer from its
        // first frame (`crate::offscreen`).
        let moving = sprung_opacity != written_opacity
            || plays(Prop::Opacity)
            || sprung != written
            || glide != (0.0, 0.0);
        let opacity_group = (opacity < 1.0).then(|| {
            self.marker(Item::PushOpacity(
                opacity,
                Some(super::Fade {
                    node: node.id,
                    moving,
                }),
            ))
        });
        // (M4) Group effects: a layer around the node and its subtree,
        // whose damage grows by their reach.
        let effects = crate::effects::group(get, rect.w, rect.h, self.scale.as_f32());
        // (M4) Some of it shows: a GPU pass is asked for only then (the
        // device starts only while an effect is visible).
        #[cfg(feature = "gpu")]
        let shows = !phys.is_empty()
            && map_rect(self.xform, phys)
                .intersect(inh.clip)
                .is_some_and(|r| !r.is_empty());
        #[cfg(feature = "gpu")]
        let effects = match tilt {
            Some(turn) if three_d => Some(crate::effects::with_tilt(effects, turn)),
            Some(_) => {
                if shows {
                    self.warm_tilt(node.id);
                }
                effects
            }
            None => effects,
        };
        #[cfg(not(feature = "gpu"))]
        let _ = tilt;
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
                mosaic: None,
                gpu: None,
            })))
        });
        // (M4) The transition mask over the node and its subtree.
        let mask_group = transition_mask.map(|m| {
            let mut h = DefaultHasher::new();
            format!("{m:?}").hash(&mut h);
            h.finish().hash(&mut sig);
            let (push, pop) = m.group(frame, self.scale.as_f32(), self.xform);
            (self.marker(push), pop)
        });
        let mask_hash = transition_mask.map(|m| format!("{m:?}"));
        // What the node itself draws starts here (`NodeRecord::geom`).
        let own_start = self.out.items.len();
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
        // A lone radius glows in the node's colour (its own, else the
        // inherited one), on a box too, as its springs do.
        let glow_color = own_color.unwrap_or_else(|| default_color(&scope));
        let glow = crate::effects::light::Glow::of(get(Prop::Glow), glow_color);
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
            // (M4) Glass, and a large blur, drawn by the GPU at full
            // resolution once it has (the layer is pushed after its clip).
            #[cfg(feature = "gpu")]
            let gpu = if !shows {
                None
            } else {
                let boxed = map_rect(self.xform, phys)
                    .intersect(self.surface)
                    .unwrap_or_default();
                let t = time.map_or(0.0, |t| t.t);
                self.backdrop_pass(node.id, &e, boxed, read, end + 1, r.top_left, t)
            };
            #[cfg(not(feature = "gpu"))]
            let gpu = None;
            self.push(Item::PushClip(box_path.clone()), phys, &mut sig, &mut ink);
            self.push(
                Item::PushLayer(Arc::new(crate::layers::Layer {
                    effects: Arc::from([e]),
                    frame,
                    scale: self.scale.as_f32(),
                    xform: self.xform,
                    mosaic: None,
                    gpu,
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
        // (M6) A blur behind the box draws for input too (`input_region`).
        let blur_start = self.out.blur.len();
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
            NodeKind::Bar | NodeKind::Button | NodeKind::Segmented => {
                super::default_fill(node.kind, root, &scope, text_color).map(Paint::Solid)
            }
            NodeKind::Meter => {
                Some(paint_of(get(Prop::Track)).unwrap_or(Paint::Solid(text_color.alpha(0.15))))
            }
            _ => None,
        };
        // Where its fillets show past the clips above it.
        let mut fillets = Rect::default();
        if has_area
            && let Some(paint) = paint_of(get(Prop::Bg))
                .or(default_bg)
                .map(|p| if tint { tinted(p) } else { p })
        {
            // A root the compositor fades (a delegated opacity below 1,
            // `pose::delegate` took it out of the props) claims nothing
            // either.
            if root
                && opacity >= 1.0
                && self.out.pose.is_none_or(|p| p.opacity >= 1.0)
                && layer_group.is_none()
                && saved == self.xform
                && self.xform == kurbo::Affine::IDENTITY
                && opaque_paint(&paint)
            {
                self.out.opaque = opaque_bands(phys, &r).clipped(self.surface);
            }
            let fillet = self.fillet.filter(|f| f.node == node.id);
            let (shape, reach) = if let Some(f) = fillet {
                // `attach:`: square on the edge, with its fillets, which
                // the clips above it let through (fillet-clip).
                let path = crate::fillet::path(frame, r, f.edge);
                let reach = cover(path.bounding_box()).union(phys);
                fillets = self.let_fillets_through(frame, r, f.edge);
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
        // (M4) An `svg`'s `#id { … }` parts, resolved for this frame.
        let parts = if has_area && node.kind == NodeKind::Svg {
            self.svg_parts(node, &tokens, time, inherited, laid)
        } else {
            Vec::new()
        };
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
            // (M4) A merge's goo is drawn after its children.
            None if node.kind == NodeKind::Merge => None,
            Some(b) => {
                // (M4) Aurora and particles past the CPU's cap: the GPU's
                // pixels once it has drawn them.
                #[cfg(feature = "gpu")]
                let gpu = if shows {
                    self.generator_pass(node.id, b, frame, time_now)
                } else {
                    None
                };
                #[cfg(not(feature = "gpu"))]
                let gpu: Option<(u64, Arc<vello_cpu::Pixmap>)> = None;
                match gpu {
                    Some(drawn) => Some(drawn),
                    None => {
                        #[cfg(feature = "gpu")]
                        let says = !self.gpu_draws(node.id, crate::renderer::backend::Slot::Node);
                        #[cfg(not(feature = "gpu"))]
                        let says = true;
                        if says {
                            self.extras.fallbacks.note(b);
                        }
                        self.extras.rasters.pixmap_from(
                            node.id,
                            b,
                            b.config(),
                            pw,
                            ph,
                            self.scale.as_f32(),
                            b.time(time_now),
                        )
                    }
                }
            }
            None => {
                self.extras.rasters.unused(node.id);
                self.extras.rasters.pixmap(
                    node.id,
                    pw,
                    ph,
                    self.scale.as_f32(),
                    time_now,
                    &crate::offscreen::RasterProps {
                        get: &get,
                        color: text_color,
                        parts: &parts,
                        files: Some(&self.extras.images),
                    },
                )
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
        // (M4) A `shader` node: its pass's last pixels (a transparent
        // pixel until it has some, and with no device), and the pass this
        // frame wants.
        #[cfg(feature = "gpu")]
        if shows
            && node.kind == NodeKind::Shader
            && let Some(PropValue::Shader(code)) = get(Prop::Shader)
        {
            let (w, h) = (frame.width().round() as u32, frame.height().round() as u32);
            let (x, y) = (frame.x0.round(), frame.y0.round());
            let pointer = self.pass_pointer(kurbo::Rect::new(x, y, x + w as f64, y + h as f64));
            if let Some(want) = crate::renderer::backend::pass_want(
                node.id,
                code,
                get(Prop::Uniforms),
                (x as i32, y as i32),
                w,
                h,
                self.scale.as_f32(),
                time.map_or(0.0, |t| t.t),
                pointer,
            ) {
                let id = crate::renderer::backend::PassId::new(
                    node.id,
                    crate::renderer::backend::Slot::Node,
                );
                let (key, pixmap) = match self.extras.shaders.get(id) {
                    Some((k, p)) => (*k, p.clone()),
                    None => (0, crate::renderer::backend::empty_pixmap()),
                };
                let rect = kurbo::Rect::new(x, y, x + w as f64, y + h as f64);
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
                // (m4-audit) A file that lost a device is never run again.
                if !self.extras.shaders.lost(code) {
                    self.out.passes.push(want);
                }
            }
        }
        // (M4) A `canvas`: what its `draw:` recorded (`canvas.rs`).
        if has_area
            && node.kind == NodeKind::Canvas
            && let Some(PropValue::DrawList(ops)) = get(Prop::Draw)
        {
            let themed = |v: &PropValue| scope.resolve(v).and_then(|v| paint_of(Some(&v)));
            // Its texts: one request each (parts 1..), drawn once shaped.
            let shaped: &[Shaped] = self.layouts.get(&node.id).map_or(&[], Vec::as_slice);
            let scale = self.scale;
            let mut specs = Vec::new();
            let mut text = |i: usize, t: &str, x: f64, y: f64| {
                let part = i as u8 + 1;
                specs.push(TextSpec {
                    text: t.to_string(),
                    style: TextStyle {
                        font: font.clone(),
                        ..TextStyle::default()
                    },
                    max_width: None,
                    scale,
                    part,
                });
                let l = pick_part(shaped, part, scale, None).or_else(|| {
                    shaped
                        .iter()
                        .find(|c| c.part == part)
                        .map(|c| c.layout.clone())
                })?;
                let k = scale.as_f64() / l.scale.as_f64();
                let (x, y) = (x.round() as i32, (y - l.baseline as f64 * s).round() as i32);
                let ink = kurbo::Rect::new(
                    x as f64 + l.ink.left() as f64 * k,
                    y as f64 + l.ink.top() as f64 * k,
                    x as f64 + l.ink.right() as f64 * k,
                    y as f64 + l.ink.bottom() as f64 * k,
                );
                let item = Item::Glyphs {
                    x,
                    y,
                    layout: l,
                    color: text_color,
                    spans: Vec::new(),
                    fill: None,
                };
                Some((item, ink))
            };
            let items = crate::canvas::items(ops, frame, s, &themed, &mut text);
            for (item, reach) in items {
                self.push(item, cover(reach), &mut sig, &mut ink);
            }
            for spec in specs {
                self.out.text.push((node.id, spec));
            }
        }
        // Border, drawn inside the box. Along the box (no `shape:`), it
        // is hashed apart from the rest, with the strips it lies in: a
        // change of the border alone damages only its ring.
        let mut ring: Option<(u64, [Rect; 4])> = None;
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
            // (eval-rules-6) The ring as drawn, and what it was drawn
            // from.
            #[cfg(feature = "eval-hooks")]
            if let Some(t) = probe.as_mut() {
                t.ring = Some(crate::probe::RingDrawn {
                    frame: [frame.x0, frame.y0, frame.x1, frame.y1],
                    path: path.clone(),
                    width,
                    radius: get(Prop::Radius).or(default_radius.as_ref()).cloned(),
                    size: [rect.w, rect.h],
                    shaped: outline.is_some() || squircle,
                    scale: s,
                });
            }
            let item = Item::Border {
                path,
                paint: paint.clone(),
                frame,
            };
            // The ring lies in two columns as wide as its corners reach
            // (or the border, if wider) and, between them, two rows as tall
            // as the border, each 2 px more for antialiasing: a pill's
            // round ends are its columns, and its middle is left out.
            let corner = [r.top_left, r.top_right, r.bottom_right, r.bottom_left]
                .into_iter()
                .fold(0.0f64, f64::max);
            // A squircle's corner eases further along its edges.
            let corner = if squircle {
                (corner * super::paint::SQUIRCLE_REACH)
                    .min(frame.width().min(frame.height()) / 2.0)
                    .max(corner)
            } else {
                corner
            };
            let col = (bw.max(corner).ceil() as i32).saturating_add(2);
            let row = (bw.ceil() as i32).saturating_add(2);
            let outer = phys.inflate(1);
            let strips = (outline.is_none()
                && i64::from(outer.w) > 2 * i64::from(col)
                && i64::from(outer.h) > 2 * i64::from(row))
            .then(|| {
                let (w, h, c, b) = (outer.w, outer.h, col as u32, row as u32);
                let mid = w - 2 * c;
                [
                    Rect::new(outer.x + col, outer.y, mid, b),
                    Rect::new(outer.x + col, outer.y + (h - b) as i32, mid, b),
                    Rect::new(outer.x, outer.y, c, h),
                    Rect::new(outer.x + (w - c) as i32, outer.y, c, h),
                ]
                .map(|c| {
                    map_rect(self.xform, c)
                        .intersect(inh.clip)
                        .unwrap_or_default()
                })
            });
            match strips {
                Some(cells) => {
                    let mut h = DefaultHasher::new();
                    self.push(item, phys, &mut h, &mut ink);
                    ring = Some((h.finish(), cells));
                }
                None => self.push(item, phys, &mut sig, &mut ink),
            }
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
                node, &get, frame, phys, &box_path, &r, text_color, &scope, time, &mut sig,
                &mut ink,
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
            // (eval-sandbox) The glyphs' ink as this frame draws it.
            #[cfg(feature = "eval-hooks")]
            if let Some(t) = probe.as_mut()
                && !bounds.is_empty()
            {
                let lg = |v: f64| (v / s) as f32;
                t.ink = Some(LogicalRect::new(
                    lg(f64::from(bounds.x)),
                    lg(f64::from(bounds.y)),
                    lg(f64::from(bounds.w)),
                    lg(f64::from(bounds.h)),
                ));
                // (eval-r3) The layout's own ink past its own box.
                let ls = l.scale.as_f64();
                let (il, ir) = (l.ink.left() as f64 / ls, l.ink.right() as f64 / ls);
                t.overhang = Some([
                    (-il).max(0.0) as f32,
                    (ir - f64::from(l.size.w)).max(0.0) as f32,
                ]);
            }
            let clip = (clip_text && !bounds.is_empty())
                .then(|| self.marker(Item::PushClip(frame.to_path(0.1))));
            if let Some(i) = clip {
                self.group_bounds(i, bounds);
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
                // (eval-rules-2) A roll in flight, and the one a change
                // interrupted.
                #[cfg(feature = "eval-hooks")]
                if let Some(t) = probe.as_mut() {
                    let was = self.anim.roll_interrupted(node.id);
                    let texts = &self.extras.texts;
                    let name =
                        |l: &strand_text::TextLayout| texts.get(&l.key).map(|p| p.text.clone());
                    t.roll = roll.as_ref().map(|(a, b, p)| crate::probe::RollDrawn {
                        from: name(a),
                        to: name(b),
                        p: *p,
                        was: was.as_ref().map(|(a, b, p)| (name(a), name(b), *p)),
                    });
                }
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
        let before_light = self.out.items.len();
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
        self.shift_fillet_fill(content_start, self.out.items.len() - before_light);
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

        let mut bounds = ink.intersect(inh.clip).unwrap_or_default().union(fillets);
        if reach > 0 && !bounds.is_empty() {
            bounds = bounds
                .inflate(reach)
                .intersect(self.surface)
                .unwrap_or_default();
        }
        // (M6) On a hover popup that draws nothing itself, each child of its
        // root that draws anything (a blur behind it included) takes input
        // in its whole box, as drawn (below, once its subtree is drawn).
        if root {
            self.out.root_paints = (!ink.is_empty() || self.out.blur.len() > blur_start) && !inert;
        }
        // (m6-owner-fixes, G-c) `hit: none`: no input in its own box;
        // its children take theirs, and stand for it in a hover popup's
        // region.
        let no_hit = matches!(get(Prop::Hit), Some(PropValue::Keyword(k)) if k == "none");
        let takes_input = !root && !inh.input_done && !inert && !no_hit;
        let input_done = inh.input_done || (!root && !no_hit);
        // The border, hashed apart, still counts in the whole and in what
        // the glyphs leave out.
        // A box that draws only itself (a solid fill, border and rim,
        // untransformed, no effects): its geometry apart from the rest.
        let own = if has_area
            && !themed
            && reach == 0
            && self.xform == kurbo::Affine::IDENTITY
            && outline.is_none()
            && !squircle
            && transition_mask.is_none()
            && layer_group.is_none()
            && get(Prop::Stroke).is_none()
            // An inner shadow's ring lies inside the box, past the edge
            // band a moving edge repaints, and its offset and spread are
            // not the box's geometry (m6-owner-fixes review).
            && get(Prop::InnerShadow).is_none()
            && self.fillet.is_none_or(|f| f.node != node.id)
        {
            let mut h = DefaultHasher::new();
            (inh.ctx, node.kind, node.epoch).hash(&mut h);
            hash_f32(&mut h, opacity);
            format!("{:?}{:?}", get(Prop::Border), get(Prop::Rim)).hash(&mut h);
            let plain = self.out.items[own_start..].iter().all(|i| {
                let (tag, paint) = match &i.item {
                    Item::Fill { paint, .. } => (0u8, Some(paint)),
                    Item::Border { paint, .. } => (1, Some(paint)),
                    Item::PushClip(_) => (2, None),
                    Item::PopClip => (3, None),
                    _ => return false,
                };
                tag.hash(&mut h);
                match paint {
                    Some(Paint::Solid(c)) => hash_color(&mut h, c),
                    Some(_) => return false,
                    None => {}
                }
                true
            });
            let border = match get(Prop::Border) {
                Some(PropValue::Border(Border { width, .. })) => finite(*width)
                    .filter(|w| *w > 0.0)
                    .map_or(0.0, |w| (w as f64 * s).round().max(1.0)),
                _ => 0.0,
            };
            let rim = if get(Prop::Rim).is_some() {
                s.round().max(1.0)
            } else {
                0.0
            };
            plain.then(|| {
                let edge = border.max(rim).ceil() as u32 + 1;
                (round_box(phys, &r, edge), h.finish())
            })
        } else {
            None
        };
        // Clips whose geometry decides damage (`NodeRecord::geom`) cannot
        // be told apart under effects that spread pixels.
        if reach > 0 {
            for c in inh.clips.iter() {
                (c.rect, c.radii).hash(&mut sig);
            }
        }
        let rest = sig.finish();
        let whole = match ring {
            Some((r, _)) => {
                let mut h = DefaultHasher::new();
                (rest, r).hash(&mut h);
                h.finish()
            }
            None => rest,
        };
        self.out.order.push(node.id);
        self.out.records.insert(
            node.id,
            NodeRecord {
                bounds,
                sig: whole,
                glyphs: glyph_cells.map(|(mut glyph_rest, cells)| {
                    if let Some((r, _)) = ring {
                        r.hash(&mut glyph_rest);
                    }
                    Arc::new(GlyphCells {
                        rest: glyph_rest.finish(),
                        cells,
                    })
                }),
                ring: ring.map(|(_, cells)| Arc::new(RingCells { rest, cells })),
                geom: Some(Arc::new(Geometry {
                    rest: own.map_or(whole, |(_, h)| h),
                    ink,
                    own: own.map(|(b, _)| b),
                    clips: inh.clips.clone(),
                })),
            },
        );
        #[cfg(feature = "eval-hooks")]
        if let Some(mut t) = probe {
            t.paint = super::probe::paint(&self.out.items[own_start..], s, &self.extras.texts);
            t.gap = number(get(Prop::Gap)).filter(|g| g.is_finite());
            t.drag = self.extras.widgets.drags.get(&node.id).copied();
            t.cut = probe_cut;
            let (own, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.probe_images)
                .into_iter()
                .partition(|(n, _)| *n == node.id);
            self.probe_images = rest;
            t.images = own.into_iter().map(|(_, i)| i).collect();
            let (sx, sy) = (zoom * morph_sx as f32, zoom * morph_sy as f32);
            let hidden = inert.then_some(crate::probe::Hidden::Inert);
            self.probe_push(
                t,
                (opacity, [sx, sy], turn),
                frame,
                self.xform,
                hidden,
                inh.clip,
            );
        }

        // Hit shape: the rounded box, grown by `hit: grow(n)`; none with
        // `hit: none`.
        let grow = match get(Prop::Hit) {
            Some(PropValue::Call { name, args }) if name == "grow" => {
                args.first().and_then(|a| number(Some(a))).unwrap_or(0.0)
            }
            _ => 0.0,
        }
        .clamp(0.0, 1000.0) as f64
            * s;
        if !inert && !no_hit {
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
                grow,
            });
        }

        // Children. A `scroll`, `list` or `pages` always clips its content
        // (a page slides in from outside it), and so
        // does a node whose size springs (a toast collapsing to `height:
        // 0`), or that exits to a pose setting its size.
        let clips = matches!(get(Prop::Clip), Some(PropValue::Bool(true)))
            || matches!(
                node.kind,
                NodeKind::Scroll | NodeKind::List | NodeKind::Pages
            )
            || self.anim.sizing(self.tree, node);
        let mut ctx = DefaultHasher::new();
        (inh.ctx, node.epoch).hash(&mut ctx);
        hash_f32(&mut ctx, opacity);
        if let Some(i) = layer_group
            && let Item::PushLayer(l) = &self.out.items[i].item
        {
            crate::layers::hash_effects(&mut ctx, &l.effects);
        }
        mask_hash.hash(&mut ctx);
        // A mask (`mask:`, or a transition's) is drawn across the node's
        // frame, so where it lies is part of how every child draws: a
        // resized masked box repaints its children whole, not only along
        // a clip's edges (m6-owner-fixes review).
        let masked = mask_group.is_some()
            || layer_group.is_some_and(|i| {
                matches!(&self.out.items[i].item, Item::PushLayer(l)
                    if l.effects.iter().any(|e| matches!(e, strand_scene::Effect::Mask(_))))
            });
        if masked {
            [frame.x0, frame.y0, frame.x1, frame.y1]
                .map(f64::to_bits)
                .hash(&mut ctx);
        }
        let mut child_clip = inh.clip;
        let mut clip_group = None;
        let mut child_clips = inh.clips.clone();
        if clips {
            // An untransformed rounded clip's geometry goes in the records
            // below (`NodeRecord::geom`), so its moving edge damages only
            // what it uncovers or covers.
            if self.xform == kurbo::Affine::IDENTITY && outline.is_none() && !squircle {
                (2u8, node.id).hash(&mut ctx);
                let mut all = inh.clips.to_vec();
                all.push(round_box(phys, &r, 1));
                child_clips = Arc::from(all);
            } else {
                hash_path(&mut ctx, &box_path);
            }
            // What the clip lets through: its outline, which a shape's
            // passes outside the box by a fraction of a pixel
            // (damage-fraction).
            let through = painted_cover(box_path.bounding_box()).union(phys);
            child_clip = map_rect(self.xform, through)
                .intersect(inh.clip)
                .unwrap_or_default();
            let i = self.marker(Item::PushClip(box_path));
            clip_group = Some(i);
            self.open_clips.push(super::OpenClip {
                item: i,
                xform: self.xform,
                frame,
                radius: [r.top_left, r.top_right, r.bottom_right, r.bottom_left]
                    .into_iter()
                    .fold(0.0, f64::max),
                held: None,
            });
        }
        // (M4) A merge's goo goes under its children, once they are
        // drawn (`crate::effects::goo`): its distance and their colours.
        let goo_at = (node.kind == NodeKind::Merge && has_area).then(|| {
            let distance = node
                .get(Prop::Value)
                .and_then(|v| scope.resolve(v))
                .and_then(|v| number(Some(&v)))
                .unwrap_or(0.0);
            let colors: Vec<(strand_scene::NodeId, Color)> = node
                .children
                .iter()
                .filter_map(|c| {
                    let bg = self.tree.get(*c)?.get(Prop::Bg)?;
                    match scope.resolve(bg).as_deref() {
                        Some(PropValue::Color(col)) => Some((*c, *col)),
                        Some(PropValue::Paint(strand_scene::Paint::Solid(col))) => Some((*c, *col)),
                        _ => None,
                    }
                })
                .collect();
            (self.out.items.len(), distance, colors)
        });
        let child_inh = Inherited {
            color: own_color,
            font: own_font,
            weight,
            tokens,
            ctx: ctx.finish(),
            clip: child_clip,
            offset,
            flight,
            inert,
            timed: timed_scope,
            reach,
            input_done,
            clips: child_clips,
        };
        let mut children = Rect::default();
        if !(clips && child_clip.is_empty()) {
            // (M4) A child lifted by a drag paints above its siblings,
            // one springing back below the one a drag holds (dock-drag).
            let mut order = Cow::Borrowed(node.children.as_slice());
            if order.iter().any(|c| self.anim.lift_rank(*c) > 0) {
                order.to_mut().sort_by_key(|c| self.anim.lift_rank(*c));
            }
            for &c in order.iter() {
                // A nested surface (a popup) paints on its own surface.
                if let Some(child) = self
                    .tree
                    .get(c)
                    .filter(|n| !crate::layout::out_of_flow(n.kind))
                {
                    children = children.union(self.node(child, rect, &child_inh, false));
                }
            }
        } else {
            // Clipped to nothing (a box its content has not sized yet, or
            // one collapsed to `height: 0` or scrolled out of its clip):
            // its subtree is not visited, but layout still places what it
            // holds, and a text with no layout takes no room there, so its
            // texts are asked for as a hidden node's are, and as a visited
            // text out of view is. Else a box sized by its text alone would
            // never show it, and a collapsing exit's texts would be pruned
            // and placed by their estimates (exit-height0, late-text-dip).
            self.hidden_texts(
                node,
                child_inh.font.as_ref(),
                child_inh.weight,
                &child_inh.tokens,
            );
        }
        if let Some((at, distance, colors)) = goo_at {
            self.goo(node, at, distance, &colors, (frame, phys), inh.clip);
        }
        if let Some(i) = clip_group {
            if let Some(open) = self.open_clips.pop() {
                self.hold_to_box(open);
            }
            self.out.items[i].bounds = children;
            self.marker(Item::PopClip);
        }
        let subtree = bounds.union(children);
        if takes_input && (!subtree.is_empty() || self.out.blur.len() > blur_start) {
            self.out.input.push(rect);
        }
        if let Some(i) = layer_group {
            self.out.items[i].bounds = subtree;
            // (M4) Its bundled passes, drawn by the GPU.
            #[cfg(feature = "gpu")]
            self.filter_pass(node.id, i, time.map_or(0.0, |t| t.t));
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
            self.run_clock(node.id, clock, playing);
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

    /// (fillet-clip) The `attach:` target about to draw its fill `fill`
    /// in `frame` with radii `r`: each clip above it (open, between its
    /// `PushClip` and `PopClip`) takes in its fillets and the square
    /// corners they meet at the target's corners its box reaches, bounded
    /// by that box ([`crate::fillet::corners`]), so its fill shows
    /// there past a clip that holds it to its box; once the clip's group
    /// is drawn, the rest of it is held to the box (`hold_to_box`).
    /// Returns the pixels they cover (none under no clip, where its
    /// fill's own ink is enough).
    fn let_fillets_through(&mut self, frame: kurbo::Rect, r: RoundedRectRadii, edge: Edge) -> Rect {
        let mut covered: Option<kurbo::Rect> = None;
        if self.xform.determinant().abs() <= 1e-12 {
            return Rect::default();
        }
        // The fill's item: the next one pushed.
        let at = self.out.items.len();
        for open in &mut self.open_clips {
            // Each clip's path is in its own group's space.
            if open.xform.determinant().abs() <= 1e-12 {
                continue;
            }
            // Its box and radius in the target's space: what it lets
            // through is bounded by its box.
            let to_target = self.xform.inverse() * open.xform;
            let clip_box = to_target.transform_rect_bbox(open.frame);
            let side = open.radius * to_target.determinant().abs().sqrt();
            let corners = crate::fillet::corners(frame, r, edge, side, clip_box);
            for c in &corners {
                let b = c.bounding_box();
                covered = Some(covered.map_or(b, |a| a.union(b)));
            }
            let to = open.xform.inverse() * self.xform;
            let Item::PushClip(clip) = &mut self.out.items[open.item].item else {
                continue;
            };
            if corners.is_empty() {
                continue;
            }
            open.held.get_or_insert_with(|| clip.clone());
            // Wound like the clip's own box, so the union has no seam
            // where a corner meets it.
            let along = clip.area() >= 0.0;
            for c in corners {
                let mut c = to * c;
                if (c.area() >= 0.0) != along {
                    c = c.reverse_subpaths();
                }
                clip.extend(c.iter());
            }
        }
        let Some(covered) = covered else {
            return Rect::default();
        };
        self.fillet_fill = Some(at);
        map_rect(self.xform, cover(covered))
            .intersect(self.surface)
            .unwrap_or_default()
    }

    /// (fillet-clip) `n` items went in before item `at`: the fillet
    /// target's fill, when it is at or past `at`, moved with them.
    fn shift_fillet_fill(&mut self, at: usize, n: usize) {
        if let Some(k) = &mut self.fillet_fill
            && at <= *k
        {
            *k += n;
        }
    }

    /// (fillet-clip) The clip group `open` closing: when the fillet
    /// target's fill widened it ([`Self::let_fillets_through`]),
    /// everything else the group draws is held to the clip's box again,
    /// so only that fill passes it. Each run of items around the fill
    /// that opens no group the fill is drawn in (what is drawn before
    /// the fill, the target's own content after it, what follows) is
    /// wrapped in a clip of the box, in the transform it is drawn under;
    /// the groups the fill is drawn in (its ancestors' and its own
    /// opacity, transform, layers, clips) keep their order.
    fn hold_to_box(&mut self, open: super::OpenClip) {
        let Some(held) = open.held else {
            return;
        };
        let start = open.item + 1;
        let end = self.out.items.len();
        let Some(fill) = self.fillet_fill.filter(|&k| {
            (start..end).contains(&k)
                && matches!(
                    self.out.items[k].item,
                    Item::Fill {
                        shape: FillShape::Path(_),
                        ..
                    }
                )
        }) else {
            return;
        };
        // The runs to hold: (first item, end, the transform in force).
        let mut runs: Vec<(usize, usize, kurbo::Affine)> = Vec::new();
        // The groups open at the fill, each with the transform before it.
        let mut path: Vec<(usize, kurbo::Affine)> = Vec::new();
        let mut cur = open.xform;
        for k in start..fill {
            match &self.out.items[k].item {
                Item::PushTransform(a) => {
                    path.push((k, cur));
                    cur = *a;
                }
                Item::PushClip(_) | Item::PushOpacity(..) | Item::PushLayer(_) => {
                    path.push((k, cur));
                }
                Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer => {
                    if let Some((_, before)) = path.pop() {
                        cur = before;
                    }
                }
                _ => {}
            }
        }
        let mut from = start;
        for &(k, before) in &path {
            runs.push((from, k, before));
            from = k + 1;
        }
        runs.push((from, fill, cur));
        // After the fill: each pop at its depth closes a group it is
        // drawn in, innermost first.
        let mut from = fill + 1;
        let mut depth = 0usize;
        for k in fill + 1..end {
            match &self.out.items[k].item {
                Item::PushClip(_)
                | Item::PushOpacity(..)
                | Item::PushTransform(_)
                | Item::PushLayer(_) => depth += 1,
                Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer => {
                    if depth > 0 {
                        depth -= 1;
                    } else {
                        runs.push((from, k, cur));
                        if let Some((_, before)) = path.pop() {
                            cur = before;
                        }
                        from = k + 1;
                    }
                }
                _ => {}
            }
        }
        runs.push((from, end, cur));
        // The clips to insert, by the item they go before.
        let mut inserts: Vec<(usize, DisplayItem)> = Vec::new();
        for (a, b, at) in runs {
            if a >= b || at.determinant().abs() <= 1e-12 {
                continue;
            }
            let bounds = self.out.items[a..b]
                .iter()
                .filter(|d| {
                    !matches!(
                        d.item,
                        Item::PopClip | Item::PopOpacity | Item::PopTransform | Item::PopLayer
                    )
                })
                .fold(Rect::default(), |u, d| u.union(d.bounds));
            let to = at.inverse() * open.xform;
            let clip = if to == kurbo::Affine::IDENTITY {
                held.clone()
            } else {
                to * held.clone()
            };
            inserts.push((
                a,
                DisplayItem {
                    item: Item::PushClip(clip),
                    bounds,
                },
            ));
            inserts.push((
                b,
                DisplayItem {
                    item: Item::PopClip,
                    bounds: self.surface,
                },
            ));
        }
        if inserts.is_empty() {
            return;
        }
        // (The runs are in order, so the inserts are.)
        #[cfg(feature = "gpu")]
        for want in &mut self.out.passes {
            use crate::renderer::backend::PassInput;
            if let PassInput::Group(k) | PassInput::Behind(k) = &mut want.input {
                let old = *k;
                *k += inserts.iter().filter(|(at, _)| *at <= old).count();
            }
        }
        if let Some(k) = &mut self.fillet_fill {
            let old = *k;
            *k += inserts.iter().filter(|(at, _)| *at <= old).count();
        }
        let tail: Vec<DisplayItem> = self.out.items.drain(start..).collect();
        let mut inserts = inserts.into_iter().peekable();
        for (k, d) in (start..).zip(tail) {
            while let Some((_, i)) = inserts.next_if(|(at, _)| *at == k) {
                self.out.items.push(i);
            }
            self.out.items.push(d);
        }
        self.out.items.extend(inserts.map(|(_, i)| i));
    }

    /// (M4) Inserts `merge` node `node`'s goo at item `at` (under its
    /// children, drawn by now): the smooth union of their drawn boxes,
    /// within the merge distance (`crate::effects::goo`). `frame` is its
    /// box (physical, and as a pixel rect), `clip` its ancestors' clip.
    fn goo(
        &mut self,
        node: &Node,
        at: usize,
        distance: f32,
        colors: &[(strand_scene::NodeId, Color)],
        (frame, phys): (kurbo::Rect, Rect),
        clip: Rect,
    ) {
        use crate::effects::goo::{Blob, Goo, MAX_DISTANCE};
        let distance = distance.clamp(0.0, MAX_DISTANCE) as f64 * self.scale.as_f64();
        let origin = kurbo::Vec2::new(frame.x0.round(), frame.y0.round());
        let mut blobs = Vec::new();
        for &(c, color) in colors {
            let c = &c;
            let Some(hit) = self.out.hits.iter().rev().find(|h| h.node == *c) else {
                continue;
            };
            // Its box as drawn (through its transform, if any).
            let rect = match hit.inverse {
                Some(inv) if inv.determinant().abs() > 1e-12 => {
                    inv.inverse().transform_rect_bbox(hit.rect)
                }
                Some(_) => continue,
                None => hit.rect,
            };
            blobs.push(Blob {
                rect: rect - origin,
                radius: hit.radii.top_left,
                color,
            });
        }
        let goo = Goo { blobs, distance };
        let (pw, ph) = (phys.w, phys.h);
        let Some((key, pixmap)) = self.extras.rasters.pixmap_from(
            node.id,
            &goo,
            goo.config(),
            pw,
            ph,
            self.scale.as_f32(),
            strand_scene::TimeContext::at(0.0),
        ) else {
            return;
        };
        let rect = kurbo::Rect::new(
            origin.x,
            origin.y,
            origin.x + pixmap.width() as f64,
            origin.y + pixmap.height() as f64,
        );
        let bounds = map_rect(self.xform, phys);
        self.shift_fillet_fill(at, 1);
        self.out.items.insert(
            at,
            DisplayItem {
                item: Item::Raster {
                    node: node.id,
                    key,
                    pixmap,
                    rect,
                },
                bounds,
            },
        );
        if let Some(rec) = self.out.records.get_mut(&node.id) {
            let mut h = DefaultHasher::new();
            (rec.sig, key).hash(&mut h);
            rec.sig = h.finish();
            rec.bounds = rec.bounds.union(bounds.intersect(clip).unwrap_or_default());
            rec.drop_geometry();
        }
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

    /// (late-text-dip) [`late_text_under`] for `node` as this frame
    /// lays it out.
    fn late_text_under(&self, node: &Node) -> bool {
        late_text_under(
            self.tree,
            &self.boxes.rects,
            self.layouts,
            self.scale,
            node.id,
        )
    }

    /// (owner-testing.md bug 25) The texts under `node`, which is drawn
    /// hidden (`opacity` or `scale` 0, as an `enter` pose starts) or
    /// clips its children to nothing (exit-height0): its subtree is not
    /// visited, but layout still places it, and a text with no layout
    /// takes no room there until it arrives (late-text-dip; before, it
    /// was placed by its estimate, a ligature's name wrapped into lines).
    /// So their unbounded layouts are asked for
    /// and waited on here as when they are drawn, with no other part of
    /// the visit (no item, clock, hit or animation state): only what
    /// the text's spec reads, its props in its token scope and the
    /// font it inherits.
    fn hidden_texts(
        &mut self,
        node: &'a Node,
        font: Option<&Font>,
        weight: Option<u16>,
        tokens: &[&'a strand_scene::TokenTable],
    ) {
        for c in &node.children {
            let Some(child) = self
                .tree
                .get(*c)
                .filter(|n| !crate::layout::out_of_flow(n.kind))
                .filter(|n| self.boxes.rects.contains_key(&n.id))
            else {
                continue;
            };
            let mut tokens = tokens.to_vec();
            if let Some(PropValue::Tokens(t)) = child.get(Prop::Tokens) {
                tokens.push(t);
            }
            let scope = TokenScope::new(&tokens);
            let props: Vec<(Prop, Cow<'a, PropValue>)> = child
                .props
                .iter()
                .filter(|e| e.prop != Prop::Tokens)
                .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
                .collect();
            let get = |p: Prop| props.iter().find(|(q, _)| *q == p).map(|(_, v)| v.as_ref());
            let (own_font, mut own_weight) = match get(Prop::Font) {
                Some(PropValue::Font(f)) => (Some(sane_font(f.clone())), None),
                _ => (font.cloned(), weight),
            };
            if let Some(w) = number(get(Prop::Weight)) {
                own_weight = Some(w.clamp(1.0, 1000.0) as u16);
            }
            // As `node` reads them for a text and a button's label (an
            // `input`'s text depends on its focus and is left to its
            // estimate).
            if matches!(child.kind, NodeKind::Text | NodeKind::Button) {
                let mut f = own_font.clone().unwrap_or_else(|| default_font(&scope));
                if let Some(w) = own_weight {
                    f.weight = w;
                }
                let centre = PropValue::Keyword("center".into());
                let button = child.kind == NodeKind::Button;
                let text_get = |p: Prop| match p {
                    Prop::Align if button && get(Prop::Align).is_none() => Some(&centre),
                    _ => get(p),
                };
                if let Some((natural, ..)) = natural_spec(&text_get, &scope, &f, self.scale) {
                    self.out.text.push((child.id, natural));
                }
            }
            self.hidden_texts(child, own_font.as_ref(), own_weight, &tokens);
        }
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

/// (M4) An `svg`'s `#id { … }` parts (`svg_part` children): what its
/// raster source applies to each layer.
type SvgParts = Vec<(String, Vec<(Prop, PropValue)>)>;

impl<'a> Flattener<'a> {
    /// True if a part of `svg` reads time: the `svg` then has a clock.
    fn svg_parts_read_time(&self, svg: &Node) -> bool {
        svg.children.iter().any(|c| {
            self.tree.get(*c).is_some_and(|n| {
                n.kind == NodeKind::SvgPart && crate::time::reads_time(n, &self.tree.tokens)
            })
        })
    }

    /// The parts of `svg` with their props resolved at the `svg`'s time,
    /// springs included.
    fn svg_parts(
        &mut self,
        svg: &Node,
        tokens: &[&strand_scene::TokenTable],
        time: Option<strand_scene::TimeContext>,
        inherited: Color,
        laid: LogicalRect,
    ) -> SvgParts {
        let scope = TokenScope::new(tokens).with_time(time);
        let mut parts = Vec::new();
        for c in &svg.children {
            let Some(part) = self.tree.get(*c).filter(|n| n.kind == NodeKind::SvgPart) else {
                continue;
            };
            let Some(PropValue::Text(name)) = part.get(Prop::Name) else {
                continue;
            };
            let mut props: Vec<(Prop, Cow<'_, PropValue>)> = part
                .props
                .iter()
                .filter(|e| !matches!(e.prop, Prop::Tokens | Prop::Name))
                .filter_map(|e| scope.resolve(&e.value).map(|v| (e.prop, v)))
                .collect();
            self.anim
                .paint(part, &mut props, &scope, inherited, None, laid);
            parts.push((
                name.clone(),
                props
                    .into_iter()
                    .map(|(p, v)| (p, v.into_owned()))
                    .collect(),
            ));
        }
        parts
    }
}

/// `phys` with the corners `r` (buffer pixels) and `edge`, as damage
/// diffing compares boxes (`NodeRecord::geom`).
fn round_box(phys: Rect, r: &RoundedRectRadii, edge: u32) -> RoundBox {
    RoundBox {
        rect: phys,
        radii: [r.top_left, r.top_right, r.bottom_right, r.bottom_left].map(f64::to_bits),
        edge,
    }
}

/// (seam-snap) Where a box is drawn along one axis: from `start` (its
/// laid-out, snapped edge moved by the paint offsets at rest) for `len`,
/// each edge moved by `flight` (the offsets in flight, unrounded) rounded
/// from where it falls exactly (`lo` and `hi`, its edges' residues from
/// layout's snapping). Nothing in flight: the laid-out box, unchanged.
pub(crate) fn drawn_span(start: f32, len: f32, lo: f32, hi: f32, flight: f32) -> (f32, f32) {
    if flight == 0.0 || !flight.is_finite() {
        return (start, len);
    }
    // Half up, the same at every whole-pixel origin.
    let snap = |v: f32| (v + 0.5).floor();
    let a = start + snap(lo + flight);
    let b = start + len + snap(hi + flight);
    (a, (b - a).max(0.0))
}

/// (late-text-dip) True if `id` or a node laid out under it (in
/// `rects`) is a text (or a button's label) with something to show and
/// no layout at `scale` or any width yet in `layouts`: it is drawn as
/// nothing and takes no room until its layout arrives. Only a text
/// written as text counts (one a token gives is left out: it may resolve
/// to nothing, which never gets a layout). Flattening holds an `enter`
/// pose on it; the layout step holds the pose's size spring
/// (unshaped-enter).
pub(crate) fn late_text_under(
    tree: &crate::tree::SceneTree,
    rects: &std::collections::HashMap<strand_scene::NodeId, LogicalRect>,
    layouts: &std::collections::HashMap<strand_scene::NodeId, Vec<Shaped>>,
    scale: strand_scene::Scale,
    id: strand_scene::NodeId,
) -> bool {
    let mut work = vec![id];
    while let Some(n) = work.pop() {
        let Some(at) = tree.get(n) else { continue };
        if n != id && (crate::layout::out_of_flow(at.kind) || !rects.contains_key(&n)) {
            continue;
        }
        if matches!(at.kind, NodeKind::Text | NodeKind::Button)
            && matches!(at.get(Prop::Text), Some(PropValue::Text(t)) if !t.is_empty())
            && pick(layouts.get(&n).map_or(&[], Vec::as_slice), scale, None).is_none()
        {
            return true;
        }
        work.extend(at.children.iter().copied());
    }
    false
}
