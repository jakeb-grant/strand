//! (flex) A whole shell offline: a config compiled and instantiated on
//! the mock services, its surfaces painted by a real `Renderer`, input
//! routed by a real `Router` through the host loop (`host_loop::input`,
//! so render hears the pointer as the shell's main thread gives it), and
//! what routing and render report handed to logic as the logic thread
//! does: flags, node events, two-way writes, layout and pointer facts.
//!
//! It stands in for the compositor too: surfaces lie on one output at
//! the place their margins give them (a `top_left` panel at its left
//! and top margins, a bar at 0, 0), and pointer input is given in output
//! coordinates and handed to the surface under it in its own, so a
//! surface that moves itself moves under the pointer as on a real
//! screen.

#![allow(dead_code)]

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::instantiate::{Instance, NodeFlag, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::host_loop::{self, Cause, LoopHost, LoopSync};
use strand_render::{Flag, Intent, NodeEvent, Renderer, Router, SceneError, TextBackend};
use strand_scene::input::button;
use strand_scene::{
    AxisDelta, AxisSource, ButtonState, InputEvent, KeyInput, LogicalPoint, Modifiers, NodeId,
    NodeKind, PaintTarget, Painter, Scale, Size, SurfaceChange, SurfaceId, SurfaceSpec,
};
use strand_text::{FontConfig, TextEngine, test_font_path};

/// The output every surface is on.
pub const SCREEN: Size = Size { w: 1280, h: 720 };

pub struct Attached {
    pub id: SurfaceId,
    pub node: NodeId,
    pub spec: SurfaceSpec,
    pub size: Size,
    /// Its place on the output.
    pub origin: (f32, f32),
}

pub struct Shell {
    pub rt: Runtime,
    pub host: Rc<SchemaHost>,
    pub inst: Instance,
    pub r: Renderer,
    pub router: Router,
    pub surfaces: Vec<Attached>,
    /// The paint clock.
    pub now: Duration,
    /// Specs of surfaces not attached (closed, or created this flush).
    pending: Vec<(NodeId, SurfaceSpec)>,
    next: u32,
    /// The pointer on the output.
    pub pointer: Option<(f32, f32)>,
    on: Option<SurfaceId>,
    pub held: bool,
    /// The surface with the keyboard.
    pub keyboard: Option<SurfaceId>,
    /// The runtime's diagnostics so far (a throttled writer warns).
    pub diags: Vec<strand_core::Diagnostic>,
    /// How many pointer values logic was handed.
    pub pointer_facts: usize,
}

fn renderer() -> Renderer {
    let data = std::fs::read(test_font_path()).unwrap();
    let engine = TextEngine::new(FontConfig::isolated(vec![Arc::new(data)]));
    let mut r = Renderer::new(TextBackend::Inline(Box::new(engine)));
    r.set_query_wait(Duration::ZERO);
    r.set_resize_wait(Duration::ZERO);
    r.set_busy_window(Duration::ZERO);
    r
}

impl Shell {
    /// `src` as `shell.strand`, on one monitor `DP-1`.
    pub fn new(src: &str) -> Shell {
        Self::with_files(&[("shell.strand", src)])
    }

    pub fn with_files(files: &[(&str, &str)]) -> Shell {
        let mut map = SourceMap::new();
        for (n, t) in files {
            map.add(n.to_string(), t.to_string());
        }
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
        let program = Arc::new(lower::lower(
            &compiled.program,
            strand_compiler::schema::Schema::builtin(),
        ));
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &program.types));
        let screen = host.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1")),
                ("name", Value::text("DP-1")),
                ("width", Value::float(SCREEN.w as f64)),
                ("height", Value::float(SCREEN.h as f64)),
            ],
        );
        host.set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        let mut sh = Shell {
            rt,
            host,
            inst,
            r: renderer(),
            router: Router::new(),
            surfaces: Vec::new(),
            now: Duration::from_secs(1),
            pending: Vec::new(),
            next: 1,
            pointer: None,
            on: None,
            held: false,
            keyboard: None,
            diags: Vec::new(),
            pointer_facts: 0,
        };
        let diff = sh.inst.tick(sh.now).diff;
        host_loop::apply(&mut sh, diff).unwrap();
        sh.settle();
        sh
    }

    /// The attached surface named `name`.
    pub fn surface(&self, name: &str) -> &Attached {
        self.surfaces
            .iter()
            .find(|a| a.spec.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no surface {name} attached"))
    }

    /// A top-level value of `shell.strand` by name.
    pub fn get(&self, name: &str) -> Value {
        self.inst.value_of("shell", name).unwrap()
    }

    pub fn num(&self, path: &str) -> f64 {
        self.get(path)
            .as_f64()
            .unwrap_or_else(|| panic!("{path} is {:?}", self.get(path)))
    }

    /// Paints every surface a frame at a time, handing logic what each
    /// reports and applying its answer, until nothing wants a frame (at
    /// most `frames` frames, 16 ms apart).
    pub fn run(&mut self, frames: u32) {
        for _ in 0..frames {
            let mut painted = false;
            let ids: Vec<SurfaceId> = self.surfaces.iter().map(|a| a.id).collect();
            for id in ids {
                if !self.r.wants_frame(id) {
                    continue;
                }
                painted = true;
                let size = self.size_of(id);
                let mut px = vec![0u8; (size.w * size.h * 4) as usize];
                let t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 0).unwrap();
                self.r.paint(id, &mut t.at(self.now));
                host_loop::painted(self, id).unwrap();
            }
            let answered = self.answer();
            if !painted && !answered {
                return;
            }
            self.now += Duration::from_millis(16);
        }
    }

    /// [`Shell::run`] long enough for springs to settle.
    pub fn settle(&mut self) {
        self.run(400);
    }

    /// Logic's answer to what it was handed (its diff applied); whether
    /// there was one.
    fn answer(&mut self) -> bool {
        let u = self.inst.tick(self.now);
        if let Some(e) = u.errors.first() {
            panic!("runtime error: {}", e.message());
        }
        self.diags.extend(u.diagnostics);
        let mut diff = u.diff;
        if diff.ops.is_empty() && diff.layout_seen.is_none() {
            return false;
        }
        diff.layout_seen = Some(self.r.layout_seq());
        host_loop::apply(self, diff).unwrap();
        true
    }

    fn size_of(&self, id: SurfaceId) -> Size {
        self.surfaces.iter().find(|a| a.id == id).unwrap().size
    }

    /// The surface under the output point `(x, y)`, and the point on it.
    fn under(&self, x: f32, y: f32) -> Option<(SurfaceId, LogicalPoint)> {
        self.surfaces.iter().rev().find_map(|a| {
            let (lx, ly) = (x - a.origin.0, y - a.origin.1);
            (lx >= 0.0 && ly >= 0.0 && lx < a.size.w as f32 && ly < a.size.h as f32)
                .then(|| (a.id, LogicalPoint::new(lx, ly)))
        })
    }

    fn local(&self, id: SurfaceId, x: f32, y: f32) -> LogicalPoint {
        let a = self.surfaces.iter().find(|a| a.id == id).unwrap();
        LogicalPoint::new(x - a.origin.0, y - a.origin.1)
    }

    pub fn send(&mut self, e: InputEvent) {
        let mut idle = Vec::new();
        host_loop::input(self, &e, &mut idle).unwrap();
        self.answer();
    }

    /// The pointer to the output point `(x, y)`: it enters and leaves
    /// surfaces as a compositor sends it, and a held button keeps it on
    /// the surface it went down on (an implicit grab).
    pub fn move_to(&mut self, x: f32, y: f32) {
        self.pointer = Some((x, y));
        let target = if self.held {
            self.on.map(|s| (s, self.local(s, x, y)))
        } else {
            self.under(x, y)
        };
        let time = self.now.as_millis() as u32;
        match (self.on, target) {
            (Some(a), Some((b, at))) if a == b => self.send(InputEvent::PointerMotion {
                surface: b,
                position: at,
                time,
            }),
            (from, to) => {
                if let Some(a) = from {
                    self.send(InputEvent::PointerLeave { surface: a });
                }
                if let Some((b, at)) = to {
                    self.send(InputEvent::PointerEnter {
                        surface: b,
                        position: at,
                    });
                }
                self.on = to.map(|t| t.0);
            }
        }
        // One frame a move: a drag at 60 Hz.
        self.run(1);
    }

    /// A wheel where the pointer is: `dx`, `dy` notches (15 px each).
    pub fn wheel(&mut self, dx: f64, dy: f64) {
        let delta = |n: f64| AxisDelta {
            pixels: n * 15.0,
            value120: (n * 120.0) as i32,
            stop: false,
        };
        self.axis(delta(dx), delta(dy), AxisSource::Wheel);
    }

    /// Fingers on a touchpad move the content by `dx`, `dy` pixels where
    /// the pointer is, then a frame passes.
    pub fn touch(&mut self, dx: f64, dy: f64) {
        let delta = |px: f64| AxisDelta {
            pixels: px,
            value120: 0,
            stop: false,
        };
        self.axis(delta(dx), delta(dy), AxisSource::Finger);
    }

    /// The fingers lift from the touchpad.
    pub fn lift(&mut self) {
        let stop = AxisDelta {
            stop: true,
            ..AxisDelta::default()
        };
        self.axis(stop, stop, AxisSource::Finger);
    }

    fn axis(&mut self, horizontal: AxisDelta, vertical: AxisDelta, source: AxisSource) {
        let Some(s) = self.on else {
            return;
        };
        let (x, y) = self.pointer.unwrap();
        let position = self.local(s, x, y);
        self.send(InputEvent::PointerAxis {
            surface: s,
            position,
            horizontal,
            vertical,
            source: Some(source),
            time: self.now.as_millis() as u32,
        });
        self.run(1);
    }

    /// The keyboard enters the surface named `name`.
    pub fn keyboard_to(&mut self, name: &str) {
        let s = self.surface(name).id;
        self.keyboard = Some(s);
        self.send(InputEvent::KeyboardEnter { surface: s });
        self.run(2);
    }

    /// A key typed on the surface that has the keyboard (`shift` held).
    pub fn key(&mut self, name: &str, shift: bool) {
        let Some(s) = self.keyboard else {
            return;
        };
        let key = KeyInput {
            name: name.into(),
            text: String::new(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            time: self.now.as_millis() as u32,
            latin: None,
        };
        self.send(InputEvent::Key { surface: s, key });
        self.run(2);
    }

    /// Sets the top-level `state` `name` of `shell.strand` and lets the
    /// shell answer.
    pub fn set(&mut self, name: &str, value: Value) {
        self.inst.set_value("shell", name, value).unwrap();
        self.answer();
        self.settle();
    }

    /// The nodes of `kind` on the surface named `name`, in tree order.
    pub fn nodes(&self, name: &str, kind: NodeKind) -> Vec<NodeId> {
        let tree = self.r.tree();
        let mut out = Vec::new();
        let mut stack = vec![self.surface(name).node];
        while let Some(n) = stack.pop() {
            let Some(node) = tree.get(n) else { continue };
            if node.kind == kind {
                out.push(n);
            }
            stack.extend(node.children.iter().rev());
        }
        out
    }

    /// The specs of the popups, as render last gave them.
    pub fn popups(&self) -> Vec<&SurfaceSpec> {
        self.pending
            .iter()
            .map(|(_, s)| s)
            .filter(|s| s.kind == NodeKind::Popup)
            .collect()
    }

    /// The left button down or up where the pointer is.
    pub fn button(&mut self, down: bool) {
        let Some(s) = self.on else {
            return;
        };
        let (x, y) = self.pointer.unwrap();
        let position = self.local(s, x, y);
        self.held = down;
        self.send(InputEvent::PointerButton {
            surface: s,
            position,
            button: button::LEFT,
            state: if down {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
            time: self.now.as_millis() as u32,
        });
        self.run(2);
    }

    /// Press at `from`, drag through `steps` points to `to`, release.
    pub fn drag(&mut self, from: (f32, f32), to: (f32, f32), steps: u32) {
        self.move_to(from.0, from.1);
        self.button(true);
        for i in 1..=steps {
            let f = i as f32 / steps as f32;
            self.move_to(from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f);
        }
        self.button(false);
    }

    /// Whether the runtime warned of a writer over the write rate.
    pub fn throttled(&self) -> bool {
        self.diags
            .iter()
            .any(|d| matches!(d, strand_core::Diagnostic::WriteRate { .. }))
    }

    /// A click at `(x, y)`.
    pub fn click(&mut self, x: f32, y: f32) {
        self.move_to(x, y);
        self.button(true);
        self.button(false);
    }

    /// `name`'s surface drawn from scratch now, as premultiplied BGRA.
    pub fn shot(&mut self, name: &str) -> (Size, Vec<u8>) {
        self.settle();
        let id = self.surface(name).id;
        let size = self.size_of(id);
        self.r.invalidate(id);
        let mut px = vec![0u8; (size.w * size.h * 4) as usize];
        let t = PaintTarget::new(&mut px, size, size.w * 4, Scale::ONE, 0).unwrap();
        self.r.paint(id, &mut t.at(self.now));
        host_loop::painted(self, id).unwrap();
        (size, px)
    }

    /// Attaches the open surfaces not attached yet, re-places and
    /// resizes those whose spec changed, and lets go of closed ones.
    fn place(&mut self) {
        let changes = self.r.take_surface_changes();
        for (node, c) in changes {
            match c {
                SurfaceChange::Created(spec) | SurfaceChange::Updated { spec, .. } => {
                    self.pending.retain(|(n, _)| *n != node);
                    self.pending.push((node, spec));
                }
                SurfaceChange::Removed => {
                    self.pending.retain(|(n, _)| *n != node);
                    if let Some(i) = self.surfaces.iter().position(|a| a.node == node) {
                        let a = self.surfaces.remove(i);
                        host_loop::detached(self, a.id).unwrap();
                    }
                }
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for (node, spec) in pending {
            if spec.kind == strand_scene::NodeKind::Popup {
                self.pending.push((node, spec));
                continue;
            }
            let at = self.surfaces.iter().position(|a| a.node == node);
            if !spec.open {
                if let Some(i) = at {
                    let a = self.surfaces.remove(i);
                    host_loop::detached(self, a.id).unwrap();
                }
                self.pending.push((node, spec));
                continue;
            }
            let size = size_for(&spec);
            let origin = origin_for(&spec, size);
            match at {
                Some(i) => {
                    let id = self.surfaces[i].id;
                    let resized = self.surfaces[i].size != size;
                    self.surfaces[i].spec = spec;
                    self.surfaces[i].size = size;
                    if self.surfaces[i].origin != origin {
                        self.surfaces[i].origin = origin;
                        self.r.set_surface_origin(
                            id,
                            Some((Some("DP-1".into()), LogicalPoint::new(origin.0, origin.1))),
                        );
                    }
                    if resized {
                        self.r.configure_surface(id, size, Scale::ONE);
                    }
                }
                None => {
                    let id = SurfaceId(self.next);
                    self.next += 1;
                    self.surfaces.push(Attached {
                        id,
                        node,
                        spec,
                        size,
                        origin,
                    });
                    host_loop::attached(
                        self,
                        id,
                        node,
                        Some(strand_scene::LogicalSize::new(
                            SCREEN.w as f32,
                            SCREEN.h as f32,
                        )),
                    )
                    .unwrap();
                    self.r.set_surface_origin(
                        id,
                        Some((Some("DP-1".into()), LogicalPoint::new(origin.0, origin.1))),
                    );
                    self.r.configure_surface(id, size, Scale::ONE);
                }
            }
        }
    }
}

/// A surface's size: as its spec asks, a bar across its edge, the whole
/// output where nothing is asked.
fn size_for(spec: &SurfaceSpec) -> Size {
    use strand_scene::{Edge, NodeKind};
    let (sw, sh) = (SCREEN.w as f32, SCREEN.h as f32);
    let (w, h) = match (spec.kind, spec.edge) {
        (NodeKind::Bar, Some(Edge::Left | Edge::Right)) => (spec.width.unwrap_or(32.0), sh),
        (NodeKind::Bar, _) => (sw, spec.height.unwrap_or(32.0)),
        _ => (spec.width.unwrap_or(sw), spec.height.unwrap_or(sh)),
    };
    Size::new(w.ceil() as u32, h.ceil() as u32)
}

/// Where a surface of `size` lies on the output: from the edges its
/// anchor names, its margins in.
fn origin_for(spec: &SurfaceSpec, size: Size) -> (f32, f32) {
    use strand_scene::{Anchor, Edge, NodeKind};
    let (sw, sh) = (SCREEN.w as f32, SCREEN.h as f32);
    let (w, h) = (size.w as f32, size.h as f32);
    let m = spec.margin;
    if spec.kind == NodeKind::Bar {
        return match spec.edge {
            Some(Edge::Bottom) => (0.0, sh - h),
            Some(Edge::Right) => (sw - w, 0.0),
            _ => (0.0, 0.0),
        };
    }
    let x = match spec.anchor {
        Anchor::Left | Anchor::TopLeft | Anchor::BottomLeft => m.left,
        Anchor::Right | Anchor::TopRight | Anchor::BottomRight => sw - w - m.right,
        _ => (sw - w) / 2.0 + m.left - m.right,
    };
    let y = match spec.anchor {
        Anchor::Top | Anchor::TopLeft | Anchor::TopRight => m.top,
        Anchor::Bottom | Anchor::BottomLeft | Anchor::BottomRight => sh - h - m.bottom,
        _ => (sh - h) / 2.0 + m.top - m.bottom,
    };
    (x, y)
}

impl LoopHost for Shell {
    type Error = std::convert::Infallible;

    fn parts(&mut self) -> (&mut Renderer, Option<&mut Router>) {
        (&mut self.r, Some(&mut self.router))
    }

    fn shown(&self, out: &mut Vec<SurfaceId>) {
        out.extend(self.surfaces.iter().map(|a| a.id));
    }

    fn applied(&mut self, errors: Vec<SceneError>) -> Result<(), Self::Error> {
        assert!(errors.is_empty(), "{errors:?}");
        Ok(())
    }

    fn intents(&mut self, _cause: Cause<'_>, intents: Vec<Intent>) -> Result<(), Self::Error> {
        for i in intents {
            match i {
                Intent::Flag { node, flag, on } => {
                    let flag = match flag {
                        Flag::Hover => NodeFlag::Hover,
                        Flag::Pressed => NodeFlag::Pressed,
                        Flag::Focused => NodeFlag::Focused,
                        Flag::Selected => NodeFlag::Selected,
                    };
                    self.inst.set_flag(node, flag, on);
                }
                Intent::Event { node, event } => {
                    let (name, args) = match event {
                        NodeEvent::Click => ("click", vec![]),
                        NodeEvent::Secondary => ("secondary", vec![]),
                        NodeEvent::Middle => ("middle", vec![]),
                        NodeEvent::Activate => ("activate", vec![]),
                        NodeEvent::Dismiss => ("dismiss", vec![]),
                        NodeEvent::Scroll { dy, dx } => {
                            ("scroll", vec![Value::float(dy), Value::float(dx)])
                        }
                        NodeEvent::Key {
                            name,
                            text,
                            modifiers: m,
                        } => (
                            "key",
                            vec![self.host.record(
                                "Key",
                                &[
                                    ("name", Value::text(name.as_str())),
                                    ("text", Value::text(text.as_str())),
                                    ("ctrl", Value::Bool(m.ctrl)),
                                    ("shift", Value::Bool(m.shift)),
                                    ("alt", Value::Bool(m.alt)),
                                    ("logo", Value::Bool(m.logo)),
                                ],
                            )],
                        ),
                        _ => continue,
                    };
                    self.inst.event(node, name, args);
                }
                Intent::Write { node, prop, value } => {
                    let _ = self.inst.write(node, prop, value);
                }
                Intent::Clipboard(_) => {}
            }
        }
        Ok(())
    }

    fn facts(&mut self) -> Result<(), Self::Error> {
        for (n, b) in self.r.take_layout_facts() {
            self.inst.set_box(n, b);
        }
        for (n, p) in self.r.take_pointer_facts() {
            self.pointer_facts += 1;
            self.inst.set_pointer(n, p);
        }
        for (n, w) in self.r.take_list_windows() {
            self.inst.set_list_window(n, w.start, w.end - w.start);
        }
        Ok(())
    }

    fn wake(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl LoopSync for Shell {
    fn sync(&mut self) -> Result<(), Self::Error> {
        self.place();
        self.facts()
    }
}

/// Reads `refs/<name>.png` (RGBA) and compares it with `pixels`
/// (premultiplied BGRA) within `tolerance` per channel; `STRAND_BLESS=1`
/// writes it instead. On a mismatch the frame is written beside it as
/// `<name>.actual.png`.
pub fn assert_matches_ref(name: &str, size: Size, pixels: &[u8], tolerance: u8) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/refs");
    let path = dir.join(format!("{name}.png"));
    let rgba: Vec<u8> = pixels
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u32;
            let un = |c: u8| {
                (c as u32 * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8)
            };
            [un(p[2]), un(p[1]), un(p[0]), p[3]]
        })
        .collect();
    let save = |path: &std::path::Path| {
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbaImage::from_raw(size.w, size.h, rgba.clone())
            .unwrap()
            .save(path)
            .unwrap();
    };
    if std::env::var_os("STRAND_BLESS").is_some() {
        save(&path);
        return;
    }
    let want = image::open(&path)
        .unwrap_or_else(|e| panic!("{path:?}: {e}; run with STRAND_BLESS=1"))
        .to_rgba8();
    assert_eq!(want.dimensions(), (size.w, size.h), "{name}: size differs");
    let bad = want
        .as_raw()
        .chunks_exact(4)
        .zip(pixels.chunks_exact(4))
        .filter(|(w, got)| {
            let a = w[3] as u32;
            let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            let exp = [pm(w[2]), pm(w[1]), pm(w[0]), w[3]];
            got.iter().zip(exp).any(|(g, e)| g.abs_diff(e) > tolerance)
        })
        .count();
    if bad > 0 {
        save(&dir.join(format!("{name}.actual.png")));
        panic!("{name}: {bad} pixels differ by more than {tolerance}");
    }
}

/// The premultiplied BGRA pixel at `(x, y)` of a shot.
pub fn px(shot: &(Size, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let i = ((y * shot.0.w + x) * 4) as usize;
    [shot.1[i], shot.1[i + 1], shot.1[i + 2], shot.1[i + 3]]
}
