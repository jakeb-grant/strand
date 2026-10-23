//! (growth) A content-sized panel, OSD or popup springs its box over a
//! step of its content's size (a row mounted with no pose that springs
//! its size, a text grown), growing as it shrinks: its content is laid
//! out at the size in flight, its buffer follows with the edge it is
//! anchored by kept, and nothing is drawn past its box or jumps on
//! screen. The escape hatches: `width: auto ~ …` (`height`, `size`)
//! names the curve, `~ instant` snaps, the `$motion.spatial` token
//! reaches every growth, and reduced motion makes it immediate
//! (design.md "Springs everywhere", decisions.md growth).

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

fn pose(props: Vec<(Prop, PropValue)>) -> PropValue {
    PropValue::Pose(props)
}

fn kw(k: &str) -> PropValue {
    PropValue::Keyword(k.into())
}

/// The output the compositor places the surfaces on.
const OUTPUT: (i64, i64) = (1920, 1080);

/// What one painted frame of the watched surface showed.
#[derive(Clone, Debug)]
struct Shot {
    /// Its buffer's size.
    buf: Size,
    /// Each watched node's box in the buffer.
    rects: HashMap<NodeId, LogicalRect>,
    /// No pixel past the root's box (rounded out to whole pixels) is
    /// drawn.
    clean: bool,
    /// The red pixels drawn: their bounds in the buffer, left, top,
    /// right and bottom (exclusive).
    red: Option<[i64; 4]>,
}

impl Shot {
    /// The red pixels' bounds on screen, the buffer anchored at `at`.
    fn red_on_screen(&self, at: (f32, f32)) -> [i64; 4] {
        let (ox, oy) = origin(self.buf, at);
        let [l, t, r, b] = self.red.expect("red is drawn");
        [ox + l, oy + t, ox + r, oy + b]
    }
}

/// The bounds of `buf`'s opaque red pixels.
fn red_bounds(buf: &Buffer) -> Option<[i64; 4]> {
    let mut out: Option<[i64; 4]> = None;
    for y in 0..buf.size.h {
        for x in 0..buf.size.w {
            let p = buf.px(x, y);
            let [b, g, r, a] = p;
            if r > 200 && g < 60 && b < 60 && a > 200 {
                let (x, y) = (x as i64, y as i64);
                let o = out.get_or_insert([x, y, x + 1, y + 1]);
                *o = [o[0].min(x), o[1].min(y), o[2].max(x + 1), o[3].max(y + 1)];
            }
        }
    }
    out
}

/// Content-sized surfaces under the host's order: the diff's preview,
/// then the surface manager's configure at the size each spec asks for.
/// Every surface is painted; `watch` is read.
struct Host {
    r: Renderer,
    watch: NodeId,
    surfaces: HashMap<NodeId, (SurfaceId, Buffer, u8)>,
    next: u32,
    k: u32,
}

impl Host {
    fn new(diff: SceneDiff, watch: NodeId) -> Host {
        Host::with(renderer(), diff, watch)
    }

    fn with(r: Renderer, diff: SceneDiff, watch: NodeId) -> Host {
        let mut h = Host {
            r,
            watch,
            surfaces: HashMap::new(),
            next: 100,
            k: 1,
        };
        h.apply(diff);
        h
    }

    fn apply(&mut self, d: SceneDiff) {
        assert!(self.r.apply(d).is_empty());
        self.r.update();
        self.sync();
    }

    fn sync(&mut self) {
        while self.r.has_surface_changes() {
            for (node, change) in self.r.take_surface_changes() {
                let spec = match change {
                    SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => s,
                    _ => {
                        self.surfaces.remove(&node);
                        continue;
                    }
                };
                if !spec.open {
                    self.surfaces.remove(&node);
                    continue;
                }
                let o = spec.overhang;
                let size = Size::new(
                    (spec.width.unwrap_or(1.0) + o.left + o.right) as u32,
                    (spec.height.unwrap_or(1.0) + o.top + o.bottom) as u32,
                );
                if !self.surfaces.contains_key(&node) {
                    self.next += 1;
                    let id = SurfaceId(self.next);
                    self.r.attach_surface(id, node);
                    self.surfaces
                        .insert(node, (id, Buffer::new(size.w, size.h, Scale::ONE), 0));
                    self.r.configure_surface(id, size, Scale::ONE);
                    continue;
                }
                let (id, buf, age) = self.surfaces.get_mut(&node).unwrap();
                if buf.size != size {
                    *buf = Buffer::new(size.w, size.h, Scale::ONE);
                    *age = 0;
                    self.r.configure_surface(*id, size, Scale::ONE);
                }
            }
        }
    }

    /// Paints the next frame of every surface that wants one; returns
    /// the watched surface's, read before a resize it asks for replaces
    /// its buffer, or `None` once nothing wants a frame.
    fn frame(&mut self) -> Option<Option<Shot>> {
        let t = frame(self.k);
        let mut any = false;
        let mut shot = None;
        let mut nodes: Vec<NodeId> = self.surfaces.keys().copied().collect();
        nodes.sort_by_key(|n| (*n != self.watch, n.index));
        for node in nodes {
            let Some((id, buf, age)) = self.surfaces.get_mut(&node) else {
                continue;
            };
            if !self.r.wants_frame(*id) {
                continue;
            }
            any = true;
            buf.paint_at(&mut self.r, *id, *age, t);
            *age = 1;
            if node == self.watch {
                let boxes = self.r.boxes(*id).expect("laid out");
                let rects: HashMap<NodeId, LogicalRect> =
                    boxes.rects.iter().map(|(n, r)| (*n, *r)).collect();
                let root = rects[&node];
                shot = Some(Shot {
                    buf: buf.size,
                    clean: clean_outside(buf, root),
                    red: red_bounds(buf),
                    rects,
                });
            }
        }
        if !any {
            return None;
        }
        self.k += 1;
        if self.r.has_surface_changes() {
            self.r.update();
            self.sync();
        }
        Some(shot)
    }

    /// The watched surface's frames until nothing wants one.
    fn shots(&mut self) -> Vec<Shot> {
        let mut out = Vec::new();
        let mut n = 0;
        while let Some(s) = self.frame() {
            out.extend(s);
            n += 1;
            assert!(n < 400, "never settled");
        }
        out
    }

    fn run(&mut self) {
        self.shots();
    }

    /// The watched surface's last painted buffer and boxes.
    fn now(&self) -> (Size, LogicalRect) {
        let (id, buf, _) = &self.surfaces[&self.watch];
        let b = self.r.boxes(*id).expect("laid out");
        (buf.size, b.rects[&self.watch])
    }
}

/// Every pixel outside `r`, rounded out to whole pixels, is clear.
fn clean_outside(buf: &Buffer, r: LogicalRect) -> bool {
    let (x0, y0) = (r.x.floor() as i64, r.y.floor() as i64);
    let (x1, y1) = ((r.x + r.w).ceil() as i64, (r.y + r.h).ceil() as i64);
    for y in 0..buf.size.h as i64 {
        for x in 0..buf.size.w as i64 {
            if (x < x0 || x >= x1 || y < y0 || y >= y1) && buf.px(x as u32, y as u32)[3] != 0 {
                return false;
            }
        }
    }
    true
}

/// Where the compositor puts a buffer of `size` on the output, anchored
/// at `(fx, fy)` of it (0 the left or top edge, 0.5 the centre), in
/// whole pixels.
fn origin(size: Size, (fx, fy): (f32, f32)) -> (i64, i64) {
    let place = |out: i64, have: u32, f: f32| {
        if f == 0.0 {
            0
        } else if f == 1.0 {
            out - have as i64
        } else {
            (out - have as i64) / 2
        }
    };
    (place(OUTPUT.0, size.w, fx), place(OUTPUT.1, size.h, fy))
}

/// `node`'s box on screen in `shot`, the buffer anchored at `at`.
fn on_screen(shot: &Shot, node: NodeId, at: (f32, f32)) -> (f32, f32, f32, f32) {
    let (ox, oy) = origin(shot.buf, at);
    let r = shot.rects[&node];
    (ox as f32 + r.x, oy as f32 + r.y, r.w, r.h)
}

/// A spring's path from `from` to `to`: it starts off `from`, ends at
/// `to`, never runs back past where it was nor past `to` by more than
/// `$motion.spatial`'s overshoot (2.84%, and a pixel of rounding), and
/// takes several frames.
fn springs(path: &[f32], from: f32, to: f32) {
    let up = to > from;
    let over = (to - from).abs() * 0.0304 + 1.0;
    assert!(path.len() >= 4, "springs over several frames: {path:?}");
    let first = path[0];
    assert!(
        (first - from).abs() > 0.5 && (first - to).abs() > 0.5,
        "the first frame is partway from {from} to {to}: {path:?}"
    );
    assert!(
        (path.last().unwrap() - to).abs() < 1e-3,
        "ends at {to}: {path:?}"
    );
    for v in path {
        let past = if up { v - to } else { to - v };
        assert!(
            past <= over,
            "overshoots {to} by no more than the spring: {path:?}"
        );
    }
    // Monotone up to the overshoot's turn.
    let turn = path
        .iter()
        .position(|v| if up { *v > to } else { *v < to })
        .unwrap_or(path.len());
    assert!(
        path[..turn]
            .windows(2)
            .all(|w| if up { w[1] >= w[0] } else { w[1] <= w[0] }),
        "heads for {to} without turning back: {path:?}"
    );
}

/// A centred content-sized panel: a column with a red row 100 by 30.
fn centred_panel(extra: Vec<(Prop, PropValue)>) -> (SceneDiff, NodeId, NodeId, NodeId) {
    let mut b = Builder::default();
    let mut props = vec![(Prop::Bg, color("#1e1e2e")), (Prop::Anchor, kw("center"))];
    props.extend(extra);
    let root = b.node(NodeKind::Panel, None, props);
    let col = b.node(NodeKind::Col, Some(root), vec![(Prop::Gap, num(4.0))]);
    let red = b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    (b.diff, root, col, red)
}

/// Mounts a green row 140 by 20 at `index` of `col`, entering on a fade
/// alone (nothing that springs its size).
fn mount(col: NodeId, id: u32, index: u32) -> (SceneDiff, NodeId) {
    let new = NodeId::new(id, 0);
    let mut d = SceneDiff::new();
    d.create(new, NodeKind::Box, Some(col), index)
        .set(new, Prop::Width, num(140.0))
        .set(new, Prop::Height, num(20.0))
        .set(new, Prop::Bg, color("#00ff00"))
        .set(new, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]));
    (d, new)
}

/// The same row with no pose at all.
fn mount_plain(col: NodeId, id: u32, index: u32) -> (SceneDiff, NodeId) {
    let new = NodeId::new(id, 0);
    let mut d = SceneDiff::new();
    d.create(new, NodeKind::Box, Some(col), index)
        .set(new, Prop::Width, num(140.0))
        .set(new, Prop::Height, num(20.0))
        .set(new, Prop::Bg, color("#00ff00"));
    (d, new)
}

const CENTRE: (f32, f32) = (0.5, 0.5);

/// A centred panel a row grows springs its box from 100 by 30 to 140 by
/// 54 along `$motion.spatial`. Its buffer is configured at the larger
/// size at once and the box placed where the compositor keeps it, so the
/// red row stays where it was on screen and glides; no row is squeezed,
/// and nothing is drawn past the box. Before, the box took its new size
/// at once and its rows glided (`glide_origin`), the new row drawn past
/// the old box's edge.
#[test]
fn a_centred_panel_springs_its_box_as_it_grows() {
    let (diff, root, col, red) = centred_panel(vec![]);
    let mut h = Host::new(diff, root);
    h.run();
    let (buf, r) = h.now();
    assert_eq!((buf.w, buf.h, r.w, r.h), (100, 30, 100.0, 30.0));
    let before = {
        let (ox, oy) = origin(buf, CENTRE);
        let [l, t, _, _] = red_bounds(&h.surfaces[&root].1).expect("red");
        (ox + l, oy + t)
    };
    let (d, green) = mount(col, 60, 1);
    h.apply(d);
    let shots = h.shots();
    assert!(
        shots.iter().all(|s| s.buf == Size::new(140, 54)),
        "the buffer is the larger size from the first frame"
    );
    let ws: Vec<f32> = shots.iter().map(|s| s.rects[&root].w).collect();
    let hs: Vec<f32> = shots.iter().map(|s| s.rects[&root].h).collect();
    springs(&ws, 100.0, 140.0);
    springs(&hs, 30.0, 54.0);
    for s in &shots {
        assert!(s.clean, "nothing past the box: {:?}", s.rects[&root]);
        assert_eq!(s.rects[&red].h, 30.0, "the red row is never squeezed");
        assert_eq!(s.rects[&green].h, 20.0, "nor the new one");
        assert_eq!(s.rects[&green].w, 140.0, "laid out at its size, clipped");
    }
    // The red row on screen, as drawn: where it was, a frame along the
    // spring, and gliding to its new place (20 px left, 12 up).
    let path: Vec<(i64, i64)> = shots
        .iter()
        .map(|s| {
            let [l, t, _, _] = s.red_on_screen(CENTRE);
            (l, t)
        })
        .collect();
    assert!(
        (path[0].0 - before.0).abs() <= 6 && (path[0].1 - before.1).abs() <= 4,
        "stays where it was: {before:?} then {path:?}"
    );
    assert_eq!(*path.last().unwrap(), (before.0 - 20, before.1 - 12));
    // At most the spring's fastest frame (40% of the way at 60 Hz).
    assert!(
        path.windows(2)
            .all(|w| (w[1].0 - w[0].0).abs() <= 9 && (w[1].1 - w[0].1).abs() <= 6),
        "glides, never jumps: {path:?}"
    );
}

/// The same panel shrinking as its row leaves with no pose: held at its
/// larger buffer while its box springs back to 100 by 30, then the
/// buffer shrinks; the red row stays put on screen as it does.
#[test]
fn a_centred_panel_springs_its_box_as_it_shrinks() {
    let (diff, root, col, red) = centred_panel(vec![]);
    let mut h = Host::new(diff, root);
    h.run();
    // No pose: it leaves at once.
    let (d, green) = mount_plain(col, 60, 1);
    h.apply(d);
    h.run();
    let (buf, r) = h.now();
    assert_eq!((buf.w, buf.h, r.w, r.h), (140, 54, 140.0, 54.0));
    let mut d = SceneDiff::new();
    d.remove(green);
    h.apply(d);
    let shots = h.shots();
    assert_eq!(shots[0].buf, Size::new(140, 54), "held at its larger size");
    assert_eq!(
        shots.last().unwrap().buf,
        Size::new(100, 30),
        "then shrinks"
    );
    let moving: Vec<&Shot> = shots
        .iter()
        .filter(|s| s.buf == Size::new(140, 54))
        .collect();
    let ws: Vec<f32> = moving.iter().map(|s| s.rects[&root].w).collect();
    let hs: Vec<f32> = moving.iter().map(|s| s.rects[&root].h).collect();
    springs(&ws, 140.0, 100.0);
    springs(&hs, 54.0, 30.0);
    for s in &shots {
        assert!(s.clean, "nothing past the box: {:?}", s.rects[&root]);
        assert_eq!(s.rects[&red].h, 30.0, "the red row is never squeezed");
    }
    // The box's centre stays the screen's (in whole-pixel halves).
    for s in &shots {
        let (x, y, w, hh) = on_screen(s, root, CENTRE);
        let (cx, cy) = (x + w / 2.0, y + hh / 2.0);
        assert!(
            (cx - 960.0).abs() <= 1.0 && (cy - 540.0).abs() <= 1.0,
            "shrinks about its centre: {cx},{cy} in {:?}",
            s.buf
        );
    }
    // The red row, as drawn, glides back to where it was alone, and the
    // buffer's shrink moves nothing on screen.
    let path: Vec<(i64, i64)> = shots
        .iter()
        .map(|s| {
            let [l, t, _, _] = s.red_on_screen(CENTRE);
            (l, t)
        })
        .collect();
    assert_eq!(*path.last().unwrap(), (910, 525), "{path:?}");
    assert!(
        path.windows(2)
            .all(|w| (w[1].0 - w[0].0).abs() <= 9 && (w[1].1 - w[0].1).abs() <= 6),
        "glides back, never jumps: {path:?}"
    );
}

/// A bottom-anchored OSD a row grows above its last: the box springs
/// taller from the buffer's bottom edge, which the compositor keeps, so
/// the last row never moves on screen.
#[test]
fn a_bottom_osd_springs_up_from_its_edge() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Osd,
        None,
        vec![(Prop::Bg, color("#1e1e2e")), (Prop::Anchor, kw("bottom"))],
    );
    let col = b.node(
        NodeKind::Col,
        Some(root),
        vec![(Prop::Width, num(140.0)), (Prop::Gap, num(8.0))],
    );
    let red = b.node(
        NodeKind::Box,
        Some(col),
        vec![(Prop::Height, num(20.0)), (Prop::Bg, color("#ff0000"))],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.now().0, Size::new(140, 20));
    let (d, _) = mount(col, 60, 0);
    h.apply(d);
    let shots = h.shots();
    assert!(shots.iter().all(|s| s.buf == Size::new(140, 48)));
    let hs: Vec<f32> = shots.iter().map(|s| s.rects[&root].h).collect();
    springs(&hs, 20.0, 48.0);
    let at = (0.5, 1.0);
    for s in &shots {
        assert!(s.clean, "nothing past the box: {:?}", s.rects[&root]);
        let r = s.rects[&root];
        assert!(
            (r.y + r.h - s.buf.h as f32).abs() < 1e-3,
            "its bottom edge is the buffer's: {r:?}"
        );
        assert_eq!(s.rects[&red].h, 20.0, "the last row keeps its height");
        let [_, t, _, b] = s.red_on_screen(at);
        assert_eq!((t, b), (OUTPUT.1 - 20, OUTPUT.1), "and stays on the edge");
    }
}

/// A popup centred under a bar item: as its content grows the box
/// springs wider about its centre and taller from its top edge, the one
/// facing its anchor, which the compositor keeps.
#[test]
fn a_centred_popup_springs_from_its_anchor() {
    let mut b = Builder::default();
    let bar = b.node(NodeKind::Bar, None, vec![(Prop::Height, num(30.0))]);
    let item = b.node(
        NodeKind::Box,
        Some(bar),
        vec![(Prop::Width, num(40.0)), (Prop::Height, num(30.0))],
    );
    let popup = b.node(
        NodeKind::Popup,
        Some(item),
        vec![(Prop::Bg, color("#1e1e2e"))],
    );
    let col = b.node(NodeKind::Col, Some(popup), vec![(Prop::Gap, num(4.0))]);
    let red = b.node(
        NodeKind::Box,
        Some(col),
        vec![
            (Prop::Width, num(100.0)),
            (Prop::Height, num(30.0)),
            (Prop::Bg, color("#ff0000")),
        ],
    );
    let mut h = Host::new(b.diff, popup);
    h.run();
    assert_eq!(h.now().0, Size::new(100, 30));
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let shots = h.shots();
    assert!(shots.iter().all(|s| s.buf == Size::new(140, 54)));
    let ws: Vec<f32> = shots.iter().map(|s| s.rects[&popup].w).collect();
    let hs: Vec<f32> = shots.iter().map(|s| s.rects[&popup].h).collect();
    springs(&ws, 100.0, 140.0);
    springs(&hs, 30.0, 54.0);
    for s in &shots {
        assert!(s.clean, "nothing past the box: {:?}", s.rects[&popup]);
        let r = s.rects[&popup];
        assert_eq!(r.y, 0.0, "its top edge stays the buffer's");
        let centre = r.x + r.w / 2.0;
        assert!((centre - 70.0).abs() <= 0.5, "centred: {r:?}");
        assert_eq!(s.rects[&red].y, 0.0, "the first row stays put");
    }
}

/// A text that grows springs its panel wider, laid out at its new width
/// (never wrapped anew by the motion) and clipped by the box.
#[test]
fn a_growing_text_springs_its_panel() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Bg, color("#1e1e2e")), (Prop::Anchor, kw("top"))],
    );
    let row = b.node(NodeKind::Row, Some(root), vec![(Prop::Pad, num(6.0))]);
    let label = b.node(
        NodeKind::Text,
        Some(row),
        vec![
            (Prop::Text, text("vol")),
            (Prop::Font, PropValue::Font(font(14.0))),
            (Prop::Color, color("#ffffff")),
        ],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    let (_, r0) = h.now();
    let mut d = SceneDiff::new();
    d.set(label, Prop::Text, text("volume 40% on speakers"));
    h.apply(d);
    let shots = h.shots();
    let last = shots.last().unwrap();
    let (w1, text_w) = (last.rects[&root].w, last.rects[&label].w);
    assert!(w1 > r0.w + 60.0, "it grew: {} to {w1}", r0.w);
    let ws: Vec<f32> = shots.iter().map(|s| s.rects[&root].w).collect();
    springs(&ws, r0.w, w1);
    for s in &shots {
        assert!(s.clean, "nothing past the box: {:?}", s.rects[&root]);
        assert_eq!(s.rects[&label].w, text_w, "laid out at its new width");
        assert_eq!(s.rects[&label].h, last.rects[&label].h, "on one line");
    }
}

/// `width: auto ~ instant` and `height: auto ~ instant` on the panel
/// restore the snap: the box takes its new size on the first frame.
#[test]
fn auto_instant_snaps_the_growth() {
    let (mut diff, root, col, _) = centred_panel(vec![]);
    for p in [Prop::Width, Prop::Height] {
        diff.push(SceneOp::SetProp {
            id: root,
            prop: p,
            value: kw("auto"),
            transition: Transition::Instant,
        });
    }
    let mut h = Host::new(diff, root);
    h.run();
    assert_eq!(h.now().1.w, 100.0, "`auto` is content-sized");
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let shots = h.shots();
    let r = shots[0].rects[&root];
    assert_eq!((r.w, r.h), (140.0, 54.0), "snaps: {r:?}");
}

/// A shrink under `~ instant` snaps what the box holds too: as the green
/// row snaps to nothing, the red row is drawn at its new place on screen
/// from the first frame, never glided from where the larger box held it (a step on an instant curve
/// opens no FLIP of everything).
#[test]
fn auto_instant_snaps_what_a_shrink_moves() {
    let (mut diff, root, col, _) = centred_panel(vec![]);
    for p in [Prop::Width, Prop::Height] {
        diff.push(SceneOp::SetProp {
            id: root,
            prop: p,
            value: kw("auto"),
            transition: Transition::Instant,
        });
    }
    let mut h = Host::new(diff, root);
    h.run();
    let (d, green) = mount_plain(col, 60, 1);
    h.apply(d);
    h.run();
    assert_eq!(h.now().0, Size::new(140, 54));
    // Narrower and shorter at once, with no structural change (a removal
    // glides its siblings by FLIP, growth or not).
    let mut d = SceneDiff::new();
    for p in [Prop::Width, Prop::Height] {
        d.push(SceneOp::SetProp {
            id: green,
            prop: p,
            value: num(0.0),
            transition: Transition::Instant,
        });
    }
    h.apply(d);
    let shots = h.shots();
    assert_eq!(shots.last().unwrap().buf, Size::new(100, 34), "it shrinks");
    let path: Vec<(i64, i64)> = shots
        .iter()
        .map(|s| {
            let [l, t, _, _] = s.red_on_screen(CENTRE);
            (l, t)
        })
        .collect();
    assert!(path.iter().all(|p| *p == (910, 523)), "snaps: {path:?}");
}

/// `size: auto ~ instant` snaps both axes too.
#[test]
fn size_auto_instant_snaps_both_axes() {
    let (mut diff, root, col, _) = centred_panel(vec![]);
    diff.push(SceneOp::SetProp {
        id: root,
        prop: Prop::Size,
        value: kw("auto"),
        transition: Transition::Instant,
    });
    let mut h = Host::new(diff, root);
    h.run();
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let r = h.shots()[0].rects[&root];
    assert_eq!((r.w, r.h), (140.0, 54.0), "snaps: {r:?}");
}

/// A curve of the node's own (`height: auto ~ 200ms`, linear) moves the
/// growth along it: evenly, over 200 ms; the width, unnamed, still
/// springs on `$motion.spatial`.
#[test]
fn auto_with_a_curve_grows_along_it() {
    let (mut diff, root, col, _) = centred_panel(vec![]);
    diff.push(SceneOp::SetProp {
        id: root,
        prop: Prop::Height,
        value: kw("auto"),
        transition: Transition::Duration {
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        },
    });
    let mut h = Host::new(diff, root);
    h.run();
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let shots = h.shots();
    let hs: Vec<f32> = shots.iter().map(|s| s.rects[&root].h).collect();
    assert_eq!(*hs.last().unwrap(), 54.0, "{hs:?}");
    let steps: Vec<f32> = hs.windows(2).map(|w| w[1] - w[0]).collect();
    let moving: Vec<f32> = steps.iter().copied().filter(|d| *d > 0.0).collect();
    // 24 px over 200 ms at 60 Hz: 2 px a frame, evenly.
    assert!(
        (10..=13).contains(&moving.len()) && moving.iter().all(|d| (d - 2.0).abs() < 0.1),
        "linear over 200 ms: {hs:?}"
    );
    let ws: Vec<f32> = shots.iter().map(|s| s.rects[&root].w).collect();
    springs(&ws, 100.0, 140.0);
}

/// The theme's `$motion.spatial` reaches every growth: swapped for a
/// linear 100 ms, the box grows evenly over 100 ms.
#[test]
fn the_spatial_token_reaches_the_growth() {
    let (diff, root, col, _) = centred_panel(vec![]);
    let mut h = Host::new(diff, root);
    h.run();
    let mut table = TokenTable::default();
    table.tokens.insert(
        "motion.spatial".into(),
        PropValue::Transition(Transition::Duration {
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        }),
    );
    let mut d = SceneDiff::new();
    d.set_tokens(table, Transition::Instant);
    h.apply(d);
    h.run();
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let hs: Vec<f32> = h.shots().iter().map(|s| s.rects[&root].h).collect();
    let steps: Vec<f32> = hs
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0.0)
        .collect();
    // 24 px over 100 ms at 60 Hz: 4 px a frame.
    assert!(
        (4..=7).contains(&steps.len()) && steps.iter().all(|d| (d - 4.0).abs() < 0.2),
        "linear over 100 ms: {hs:?}"
    );
    assert_eq!(*hs.last().unwrap(), 54.0);
}

/// Under reduced motion growth and shrink are immediate.
#[test]
fn reduced_motion_makes_growth_immediate() {
    let (diff, root, col, _) = centred_panel(vec![]);
    let mut r = renderer();
    r.set_reduced_motion(true);
    let mut h = Host::with(r, diff, root);
    h.run();
    let (d, green) = mount(col, 60, 1);
    h.apply(d);
    let shots = h.shots();
    let r = shots[0].rects[&root];
    assert_eq!((r.w, r.h), (140.0, 54.0), "grows at once: {r:?}");
    let mut d = SceneDiff::new();
    d.remove(green);
    h.apply(d);
    let shots = h.shots();
    let r = shots[0].rects[&root];
    assert_eq!(shots[0].buf, Size::new(100, 30), "not held");
    assert_eq!((r.w, r.h), (100.0, 30.0), "shrinks at once: {r:?}");
}

/// A second row mounted mid-growth retargets the spring with its
/// velocity kept: the box keeps growing, never pausing or turning back,
/// to the new size.
#[test]
fn a_retarget_mid_growth_keeps_velocity() {
    let (diff, root, col, _) = centred_panel(vec![]);
    let mut h = Host::new(diff, root);
    h.run();
    let (d, _) = mount(col, 60, 1);
    h.apply(d);
    let mut hs = Vec::new();
    for _ in 0..2 {
        let s = h.frame().expect("moving").expect("painted");
        hs.push(s.rects[&root].h);
    }
    let (d, _) = mount(col, 61, 2);
    h.apply(d);
    let rest: Vec<Shot> = h.shots();
    hs.extend(rest.iter().map(|s| s.rects[&root].h));
    assert!(rest.iter().all(|s| s.clean));
    assert_eq!(*hs.last().unwrap(), 78.0, "{hs:?}");
    springs(&hs, 30.0, 78.0);
    let step = |i: usize| hs[i + 1] - hs[i];
    assert!(
        step(1) >= step(0) * 0.8,
        "the step after the retarget keeps the speed it had: {hs:?}"
    );
}

/// A stack crossfading from a large card to a small one: the leaving
/// card holds the box at its size while it fades, then the box springs
/// down to the small card. The small card is alone in the stack at rest
/// but did not fill it when the last frame drew it, so it keeps its size
/// at rest and its place on screen: it never stretches to the box in
/// flight and jumps (held-grow-glide's teleport; it did when a box
/// alone in its parent at rest was taken to fill it).
#[test]
fn a_card_a_crossfade_leaves_alone_keeps_its_size() {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![(Prop::Anchor, kw("center")), (Prop::Bg, color("#1e1e2e"))],
    );
    let stack = b.node(
        NodeKind::Stack,
        Some(root),
        vec![(Prop::Pad, num(10.0)), (Prop::Align, kw("center"))],
    );
    let big = b.node(
        NodeKind::Box,
        Some(stack),
        vec![
            (Prop::Width, num(160.0)),
            (Prop::Height, num(40.0)),
            (Prop::Bg, color("#00ff00")),
            (Prop::Exit, pose(vec![(Prop::Opacity, num(0.0))])),
        ],
    );
    let mut h = Host::new(b.diff, root);
    h.run();
    assert_eq!(h.now().0, Size::new(180, 60));
    // A row sized by what it holds, as an `else` card is.
    let (small, dot) = (NodeId::new(70, 0), NodeId::new(71, 0));
    let mut d = SceneDiff::new();
    d.remove(big);
    d.create(small, NodeKind::Row, Some(stack), 1)
        .set(small, Prop::Bg, color("#ff0000"))
        .set(small, Prop::Enter, pose(vec![(Prop::Opacity, num(0.0))]));
    d.create(dot, NodeKind::Box, Some(small), 0)
        .set(dot, Prop::Width, num(40.0))
        .set(dot, Prop::Height, num(15.0));
    h.apply(d);
    let shots = h.shots();
    assert_eq!(shots.last().unwrap().buf, Size::new(60, 35), "then shrinks");
    let hs: Vec<f32> = shots
        .iter()
        .filter(|s| s.rects[&root].w < 179.5)
        .map(|s| s.rects[&root].w)
        .collect();
    assert!(hs.len() > 3, "the box springs down: {hs:?}");
    for s in &shots {
        // Once it fills the box it follows it, the spring's undershoot
        // too, but it is never stretched to the box in flight.
        let r = s.rects[&small];
        assert!(r.w <= 40.0 && r.h <= 15.0, "never stretched: {r:?}");
        assert_eq!((s.rects[&dot].w, s.rects[&dot].h), (40.0, 15.0));
        let (x, y, _, _) = on_screen(s, dot, CENTRE);
        assert!(
            (x - 940.0).abs() <= 3.0 && (y - 532.5).abs() <= 3.0,
            "stays centred on screen, but for the undershoot it rides: {x},{y} in {:?}",
            s.buf
        );
    }
}
