//! What widgets draw over their background: buttons, sliders, inputs
//! and their carets, segmented controls, meters (wavy too) and `arc`
//! gauges.

use std::collections::hash_map::DefaultHasher;
use std::sync::Arc;

use strand_scene::{Color, Font, NodeKind, Paint, Prop, PropValue, Rect, Scale, TokenScope};
use strand_text::{TextLayout, TextStyle};
use vello_cpu::kurbo::{self, BezPath, RoundedRectRadii, Shape};

use super::paint::{cover, paint_of, shape_path};
use super::text::{Shaped, TextSpec, pick_part};
use super::{FillShape, Flattener, Item, TOLERANCE, number};
use crate::tree::Node;

/// Where an `input`'s caret stops are drawn: its text layout (none while
/// nothing is typed) and the layout's logical offset in the box.
pub(super) type CaretAt = (Option<Arc<TextLayout>>, f32, f32);

/// The x of byte offset `byte` in `l`'s first line, logical pixels (the
/// nearest stop at or before it).
pub(super) fn caret_x(l: &TextLayout, byte: usize) -> f32 {
    let byte = byte as u32;
    let mut best: Option<(u32, f32)> = None;
    for c in l.carets.iter().filter(|c| c.line == 0) {
        if c.byte == byte {
            return c.x;
        }
        if c.byte < byte && best.is_none_or(|(b, _)| c.byte > b) {
            best = Some((c.byte, c.x));
        }
    }
    best.map_or(0.0, |(_, x)| x)
}

/// The byte offset of the caret stop nearest to `x` (logical pixels in
/// `l`'s first line).
pub(crate) fn caret_index(l: &TextLayout, x: f32) -> usize {
    l.carets
        .iter()
        .filter(|c| c.line == 0)
        .min_by(|a, b| (a.x - x).abs().total_cmp(&(b.x - x).abs()))
        .map_or(0, |c| c.byte as usize)
}

/// What a widget draws with.
pub(super) struct WidgetCtx<'n, 's> {
    pub(super) node: &'n Node,
    pub(super) frame: kurbo::Rect,
    pub(super) radii: RoundedRectRadii,
    pub(super) box_path: &'s BezPath,
    /// The colour its labels use (inherited or its own `color`).
    pub(super) color: Color,
    pub(super) scope: &'s TokenScope<'s>,
    pub(super) font: &'s Font,
}

impl WidgetCtx<'_, '_> {
    fn token_color(&self, path: &str) -> Option<Color> {
        match self.scope.lookup(path) {
            Some(PropValue::Color(c)) => Some(c),
            _ => None,
        }
    }

    /// `$accent`, or the label colour without one.
    fn accent(&self) -> Color {
        self.token_color("accent").unwrap_or(self.color)
    }
}

/// The text on `bg`: `$on_accent` when it is the accent, else black or
/// white, whichever contrasts more.
pub(super) fn on(bg: Color) -> Color {
    let l = bg.to_oklab().l;
    if l > 0.62 { Color::BLACK } else { Color::WHITE }
}

impl Flattener<'_> {
    /// Draws what a widget adds over its background (see
    /// `crate::widgets`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn widget<'v>(
        &mut self,
        w: &WidgetCtx<'_, '_>,
        get: &impl Fn(Prop) -> Option<&'v PropValue>,
        caret: Option<crate::widgets::Caret>,
        caret_at: &Option<CaretAt>,
        mask: &dyn Fn(usize) -> usize,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        let id = w.node.id;
        let widgets = &self.extras.widgets;
        let (hovered, pressed) = (widgets.hovered.contains(&id), widgets.pressed.contains(&id));
        let f = w.frame;
        let phys = cover(f);
        match w.node.kind {
            NodeKind::Button => {
                // A state layer of the label colour: 8 % hovered, 12 %
                // pressed, over whatever background it has.
                let a = if pressed {
                    0.12
                } else if hovered {
                    0.08
                } else {
                    0.0
                };
                if a > 0.0 {
                    self.push(
                        Item::Fill {
                            shape: FillShape::Path(w.box_path.clone()),
                            paint: Paint::Solid(w.color.alpha(a)),
                            frame: f,
                        },
                        phys,
                        sig,
                        ink,
                    );
                }
            }
            NodeKind::Meter => {
                // The fill: `color` (its own, else `$accent`) up to
                // `value`, cut to the track's shape.
                let v = number(get(Prop::Value)).unwrap_or(0.0).clamp(0.0, 1.0) as f64;
                if v <= 0.0 {
                    return;
                }
                let fill = match w.node.get(Prop::Color).and(get(Prop::Color)) {
                    Some(PropValue::Color(c)) => *c,
                    _ => w.accent(),
                };
                // (M4) `wave: amplitude`: the fill is a round-capped wavy
                // line along the middle, as thick as the meter (it
                // flattens as `wave` springs to 0).
                let amp = number(get(Prop::Wave)).unwrap_or(0.0);
                if amp.is_finite() && amp != 0.0 {
                    let h = f.height();
                    let (x0, cy) = (f.x0 + h / 2.0, f.center().y);
                    let x1 = x0 + (f.width() - h).max(0.0) * v;
                    let mut line = BezPath::new();
                    line.move_to((x0, cy));
                    line.line_to((x1.max(x0 + 0.01), cy));
                    let style = crate::shapes::stroke::Style {
                        wave: Some((
                            amp.clamp(-1000.0, 1000.0) as f64 * s,
                            crate::widgets::METER_WAVELENGTH as f64 * s,
                        )),
                        ..crate::shapes::stroke::Style::plain(h, crate::shapes::stroke::Cap::Round)
                    };
                    if let Some(path) = crate::shapes::stroke::outline(&line, &style) {
                        let b = path.bounding_box();
                        self.push(
                            Item::Fill {
                                shape: FillShape::Path(path),
                                paint: Paint::Solid(fill),
                                frame: f,
                            },
                            cover(b),
                            sig,
                            ink,
                        );
                    }
                    return;
                }
                let r = kurbo::Rect::new(f.x0, f.y0, f.x0 + f.width() * v, f.y1);
                let clip = self.marker(Item::PushClip(w.box_path.clone()));
                self.out.items[clip].bounds = phys;
                self.push(
                    Item::Fill {
                        shape: FillShape::Path(shape_path(r, w.radii, false)),
                        paint: Paint::Solid(fill),
                        frame: r,
                    },
                    cover(r),
                    sig,
                    ink,
                );
                self.marker(Item::PopClip);
            }
            NodeKind::Arc => {
                // (M4) A gauge: the track over the whole sweep, the value
                // over its fraction, round-capped unless `cap:` says
                // otherwise; `width` is the line's (design.md: `arc {
                // value: cpu.usage; sweep: 270deg; width: 4 }`).
                let v = number(get(Prop::Value)).unwrap_or(0.0) as f64;
                let sweep = get(Prop::Sweep)
                    .and_then(crate::effects::degrees)
                    .unwrap_or(crate::widgets::ARC_SWEEP)
                    .clamp(0.0, 360.0) as f64;
                let width = get(Prop::Width)
                    .and_then(crate::effects::number)
                    .unwrap_or(crate::widgets::ARC_WIDTH)
                    .clamp(0.0, 1000.0) as f64
                    * s;
                let Some((track, value)) = crate::widgets::arc_lines(f, v, sweep, width) else {
                    return;
                };
                let style = crate::shapes::stroke::style_of(
                    get,
                    width,
                    s,
                    crate::shapes::stroke::Cap::Round,
                );
                let rest = paint_of(get(Prop::Track)).unwrap_or(Paint::Solid(w.color.alpha(0.15)));
                let fill = match w.node.get(Prop::Color).and(get(Prop::Color)) {
                    Some(PropValue::Color(c)) => Paint::Solid(*c),
                    _ => Paint::Solid(w.accent()),
                };
                for (line, paint) in [(Some(track), rest), (value, fill)] {
                    let Some(path) = line.and_then(|l| crate::shapes::stroke::outline(&l, &style))
                    else {
                        continue;
                    };
                    let b = path.bounding_box();
                    self.push(
                        Item::Fill {
                            shape: FillShape::Path(path),
                            paint,
                            frame: f,
                        },
                        cover(b),
                        sig,
                        ink,
                    );
                }
            }
            NodeKind::Slider => {
                let v = widgets
                    .drags
                    .get(&id)
                    .copied()
                    .or(number(get(Prop::Value)))
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0) as f64;
                let active = hovered || pressed;
                let knob = crate::widgets::slider_knob_radius(active) * s;
                let track = (crate::widgets::SLIDER_TRACK as f64 * s).max(1.0);
                let cy = f.center().y;
                let (x0, x1) = crate::widgets::slider_span(f.x0, f.x1, active, s);
                let x = x0 + (x1 - x0) * v;
                let bar =
                    |a: f64, b: f64| kurbo::Rect::new(a, cy - track / 2.0, b, cy + track / 2.0);
                let pill = |r: kurbo::Rect| {
                    let rr = r.height() / 2.0;
                    shape_path(r, RoundedRectRadii::from_single_radius(rr), false)
                };
                let accent = match w.node.get(Prop::Color).and(get(Prop::Color)) {
                    Some(PropValue::Color(c)) => *c,
                    _ => w.accent(),
                };
                let rest = paint_of(get(Prop::Track)).unwrap_or(Paint::Solid(w.color.alpha(0.2)));
                for (r, paint) in [
                    (bar(f.x0, f.x1), rest),
                    (bar(f.x0, x), Paint::Solid(accent)),
                ] {
                    if r.width() > 0.0 {
                        self.push(
                            Item::Fill {
                                shape: FillShape::Path(pill(r)),
                                paint,
                                frame: r,
                            },
                            cover(r),
                            sig,
                            ink,
                        );
                    }
                }
                let k = kurbo::Rect::new(x - knob, cy - knob, x + knob, cy + knob);
                self.push(
                    Item::Fill {
                        shape: FillShape::Path(kurbo::Ellipse::from_rect(k).to_path(TOLERANCE)),
                        paint: Paint::Solid(accent),
                        frame: k,
                    },
                    cover(k),
                    sig,
                    ink,
                );
            }
            NodeKind::Segmented => self.segmented(w, get, sig, ink),
            NodeKind::Input => {
                // The selection, under the text.
                let (Some(c), Some((Some(l), dx, dy))) = (caret, caret_at) else {
                    return;
                };
                let sel = c.selection();
                if sel.is_empty() {
                    return;
                }
                let (a, b) = (caret_x(l, mask(sel.start)), caret_x(l, mask(sel.end)));
                let color = w
                    .token_color("accent.container")
                    .unwrap_or(w.accent().alpha(0.3));
                let r = kurbo::Rect::new(
                    f.x0 + (*dx + a) as f64 * s,
                    f.y0 + *dy as f64 * s,
                    f.x0 + (*dx + b) as f64 * s,
                    f.y0 + (*dy + l.size.h) as f64 * s,
                )
                .intersect(f);
                if r.width() > 0.0 && r.height() > 0.0 {
                    self.push(
                        Item::Fill {
                            shape: FillShape::Rect(r),
                            paint: Paint::Solid(color),
                            frame: r,
                        },
                        cover(r),
                        sig,
                        ink,
                    );
                }
            }
            _ => {}
        }
    }

    /// A focused `input`'s caret: `$accent` (the text colour without
    /// it), `CARET_WIDTH` wide, as tall as its line, at byte `at` of the
    /// shown text, kept inside the box.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn caret(
        &mut self,
        f: kurbo::Rect,
        at: &CaretAt,
        byte: usize,
        scope: &TokenScope<'_>,
        color: Color,
        font: &Font,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        let (l, dx, dy) = at;
        let (x, h) = match l {
            Some(l) => (caret_x(l, byte), l.size.h),
            None => (0.0, (font.size * 1.2).ceil()),
        };
        let accent = match scope.lookup("accent") {
            Some(PropValue::Color(c)) => c,
            _ => color,
        };
        let cw = crate::widgets::CARET_WIDTH as f64 * s;
        let x = (f.x0 + (*dx + x) as f64 * s).clamp(f.x0, (f.x1 - cw).max(f.x0));
        let r = kurbo::Rect::new(
            x,
            f.y0 + *dy as f64 * s,
            x + cw,
            (f.y0 + (*dy + h) as f64 * s).min(f.y1),
        );
        if r.height() > 0.0 {
            self.push(
                Item::Fill {
                    shape: FillShape::Rect(r),
                    paint: Paint::Solid(accent),
                    frame: r,
                },
                cover(r),
                sig,
                ink,
            );
        }
    }

    /// A `segmented` control: its options in equal segments, the chosen
    /// one on `$accent` (inset 2 px, its radius less 2) with its label in
    /// `$on_accent`, the others in the label colour.
    pub(super) fn segmented<'v>(
        &mut self,
        w: &WidgetCtx<'_, '_>,
        get: &impl Fn(Prop) -> Option<&'v PropValue>,
        sig: &mut DefaultHasher,
        ink: &mut Rect,
    ) {
        let s = self.scale.as_f64();
        let opts = crate::widgets::options(get(Prop::Options));
        if opts.is_empty() {
            return;
        }
        let value = get(Prop::Value);
        let f = w.frame;
        let n = opts.len() as f64;
        let seg = f.width() / n;
        let accent = w.accent();
        let on_accent = w.token_color("on_accent").unwrap_or(on(accent));
        let shaped: &[Shaped] = self.layouts.get(&w.node.id).map_or(&[], Vec::as_slice);
        for (i, opt) in opts.iter().enumerate() {
            let x0 = f.x0 + seg * i as f64;
            let cell = kurbo::Rect::new(x0, f.y0, x0 + seg, f.y1);
            let chosen = value.is_some_and(|v| crate::widgets::same_option(v, opt));
            if chosen {
                let inset = 2.0 * s;
                let r = cell.inflate(-inset, -inset);
                let rr = (w.radii.top_left - inset).max(0.0);
                self.push(
                    Item::Fill {
                        shape: FillShape::Path(shape_path(
                            r,
                            RoundedRectRadii::from_single_radius(rr),
                            false,
                        )),
                        paint: Paint::Solid(accent),
                        frame: r,
                    },
                    cover(r),
                    sig,
                    ink,
                );
            }
            let part = i as u8 + 1;
            let spec = TextSpec {
                text: crate::widgets::option_label(opt),
                style: TextStyle {
                    font: w.font.clone(),
                    ..TextStyle::default()
                },
                max_width: None,
                scale: self.scale,
                part,
            };
            self.out.text.push((w.node.id, spec));
            let Some(l) = pick_part(shaped, part, self.scale, None).or_else(|| {
                shaped
                    .iter()
                    .find(|c| c.part == part)
                    .map(|c| c.layout.clone())
            }) else {
                continue;
            };
            let k = self.scale.as_f64() / l.scale.as_f64();
            let tw = l.size.w as f64 * s;
            let th = l.size.h as f64 * s;
            let x = (cell.x0 + ((cell.width() - tw) / 2.0).max(0.0)).round() as i32;
            let y = (cell.y0 + ((cell.height() - th) / 2.0).max(0.0)).round() as i32;
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
            self.push(
                Item::Glyphs {
                    x,
                    y,
                    layout: l,
                    color: if chosen { on_accent } else { w.color },
                    spans: Vec::new(),
                    fill: None,
                },
                bounds,
                sig,
                ink,
            );
        }
    }
}

/// The text requests of a `segmented`'s labels (parts 1..), shaped with
/// `font` at `scale`.
pub(super) fn segment_specs(
    options: Option<&PropValue>,
    font: &Font,
    scale: Scale,
) -> Vec<TextSpec> {
    crate::widgets::options(options)
        .iter()
        .enumerate()
        .map(|(i, o)| TextSpec {
            text: crate::widgets::option_label(o),
            style: TextStyle {
                font: font.clone(),
                ..TextStyle::default()
            },
            max_width: None,
            scale,
            part: i as u8 + 1,
        })
        .collect()
}
