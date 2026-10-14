//! (M6, owner-testing.md) A popup's card that morphs: its `x`, `width`
//! and `height` spring between an island's size and a dashboard's, as
//! the owner's notch does (target/try/island). Programs compiled and
//! instantiated, render laying them out, and the surface manager's side
//! modelled: a popup is attached and configured when its spec opens,
//! configured again at every new size or overhang, and detached when it
//! closes.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test morph`.

#![allow(dead_code)]

mod common;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;
use strand_render::Renderer;
use strand_scene::*;

const T0: Duration = Duration::from_secs(1);

/// The presentation time of frame `k` after `T0` at 60 Hz.
fn frame(k: u32) -> Duration {
    T0 + Duration::from_micros(16_667 * k as u64)
}

const BAR: SurfaceId = SurfaceId(1);

/// A compiled shell on one screen, its bar `BAR` and its popups shown as
/// the surface manager would show them.
struct Shell {
    inst: Instance,
    r: Renderer,
    scale: Scale,
    /// Each shown surface: its id, node and buffer.
    shown: Vec<(SurfaceId, NodeId, Buffer)>,
    next: u32,
    /// Popup sizes changed after the first configure.
    reconfigured: u32,
    k: u32,
    /// A popup's surface is made again at a new size, not configured.
    remake: bool,
    /// After each frame, the text it asked to have shaped is delivered
    /// before the next (a text worker that keeps up).
    settle_text: bool,
}

impl Shell {
    /// Boots `src` (module `shell`) on one screen at `scale`, its bar
    /// `width` × 30.
    fn boot(src: &str, scale: f32, width: u32) -> Shell {
        Self::boot_with(renderer(), src, scale, width)
    }

    fn boot_with(mut r: Renderer, src: &str, scale: f32, width: u32) -> Shell {
        let mut map = SourceMap::new();
        map.add("shell.strand".to_string(), src.to_string());
        let compiled = strand_compiler::compile(&map);
        assert_eq!(compiled.errors(), 0, "{:?}", compiled.diagnostics);
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
            ],
        );
        host.set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        r.set_query_wait(Duration::from_secs(30));
        r.set_busy_window(Duration::ZERO);
        assert!(r.apply(inst.flush().diff).is_empty());
        let bar = r
            .tree()
            .surface_nodes()
            .find(|n| r.tree().get(*n).is_some_and(|n| n.kind == NodeKind::Bar))
            .expect("a bar");
        let scale = Scale::from_f64(f64::from(scale)).unwrap();
        r.attach_surface(BAR, bar);
        r.configure_surface(BAR, Size::new(width, 30), scale);
        r.set_surface_origin(
            BAR,
            Some((Some("DP-1".to_string()), LogicalPoint::new(0.0, 0.0))),
        );
        let f = scale.as_f64();
        let buf = Buffer::new(
            (f64::from(width) * f).ceil() as u32,
            (30.0 * f).ceil() as u32,
            scale,
        );
        let mut sh = Shell {
            inst,
            r,
            scale,
            shown: vec![(BAR, bar, buf)],
            next: 2,
            reconfigured: 0,
            k: 0,
            remake: false,
            settle_text: false,
        };
        sh.sync();
        sh.paint();
        sh
    }

    /// The surface manager's side: popups opened are attached and
    /// configured at their spec's size (with its overhang), configured
    /// again when that changes (or, with `remake`, made again, as on an
    /// `xdg_wm_base` older than version 3: sway 1.9), detached when they
    /// close.
    fn sync(&mut self) {
        while self.r.has_surface_changes() {
            for (node, change) in self.r.take_surface_changes() {
                let spec = match change {
                    SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } => Some(s),
                    SurfaceChange::Removed => None,
                };
                if self
                    .r
                    .tree()
                    .get(node)
                    .is_none_or(|n| n.kind != NodeKind::Popup)
                {
                    continue;
                }
                let mut at = self.shown.iter().position(|s| s.1 == node);
                let Some(s) = spec.filter(|s| s.open) else {
                    if let Some(i) = at {
                        let (id, ..) = self.shown.remove(i);
                        self.r.detach_surface(id);
                    }
                    continue;
                };
                let o = s.overhang;
                let f = self.scale.as_f64();
                let px = |v: f32| (f64::from(v) * f).ceil() as u32;
                let w = s.width.unwrap_or(1.0) + o.left + o.right;
                let h = s.height.unwrap_or(1.0) + o.top + o.bottom;
                let size = Size::new(px(w), px(h));
                if let Some(i) = at {
                    if self.shown[i].2.size == size {
                        continue;
                    }
                    self.reconfigured += 1;
                    if self.remake {
                        let (id, ..) = self.shown.remove(i);
                        self.r.detach_surface(id);
                        at = None;
                    }
                }
                let id = match at {
                    Some(i) => self.shown[i].0,
                    None => {
                        let id = SurfaceId(self.next);
                        self.next += 1;
                        self.r.attach_surface(id, node);
                        let r = s.anchor_rect.unwrap_or_default();
                        self.r.set_surface_origin(
                            id,
                            Some((None, LogicalPoint::new(r.x - o.left, r.y + r.h - o.top))),
                        );
                        self.shown.push((id, node, Buffer::new(1, 1, self.scale)));
                        id
                    }
                };
                let i = self.shown.iter().position(|s| s.0 == id).unwrap();
                self.shown[i].2 = Buffer::new(size.w, size.h, self.scale);
                self.r.configure_surface(id, size, self.scale);
            }
        }
    }

    /// Paints the next frame of every surface that wants one (all of them
    /// on the first), handing layout facts to logic and its answer back.
    fn paint(&mut self) {
        let k = self.k;
        self.k += 1;
        for _ in 0..8 {
            for (id, _, buf) in &mut self.shown {
                if k == 0 || self.r.wants_frame(*id) {
                    buf.paint_at(&mut self.r, *id, 1, frame(k));
                }
            }
            self.sync();
            if !self.answer() {
                break;
            }
        }
        if self.settle_text {
            assert!(self.r.wait_for_text(Duration::from_secs(10)));
            self.sync();
        }
    }

    fn answer(&mut self) -> bool {
        let facts = self.r.take_layout_facts();
        if facts.is_empty() {
            return false;
        }
        for (n, b) in facts {
            self.inst.set_box(n, b);
        }
        let mut diff = self.inst.flush().diff;
        diff.layout_seen = Some(self.r.layout_seq());
        assert!(self.r.apply(diff).is_empty());
        self.sync();
        true
    }

    fn wants(&self) -> bool {
        self.shown.iter().any(|(id, ..)| self.r.wants_frame(*id))
    }

    /// Sets state `name` and applies what logic sends.
    fn set(&mut self, name: &str, v: Value) {
        self.inst.set_value("shell", name, v).unwrap();
        assert!(self.r.apply(self.inst.flush().diff).is_empty());
        self.sync();
    }

    /// The popup's surface, if shown.
    fn popup(&self) -> Option<SurfaceId> {
        self.shown.get(1).map(|s| s.0)
    }

    fn buf(&self, id: SurfaceId) -> &Buffer {
        &self.shown.iter().find(|s| s.0 == id).unwrap().2
    }

    /// The node of the first `kind` under `under`, depth first.
    fn find(&self, under: NodeId, kind: NodeKind) -> NodeId {
        let tree = self.r.tree();
        let mut work = vec![under];
        while let Some(n) = work.pop() {
            let node = tree.get(n).unwrap();
            if n != under && node.kind == kind {
                return n;
            }
            work.extend(node.children.iter().rev().copied());
        }
        panic!("no {kind:?}");
    }

    fn popup_node(&self) -> NodeId {
        self.find(self.shown[0].1, NodeKind::Popup)
    }

    /// `node`'s laid-out box on surface `id`.
    fn rect(&self, id: SurfaceId, node: NodeId) -> Option<LogicalRect> {
        self.r.boxes(id)?.rects.get(&node).copied()
    }
}

/// Runs frames until nothing moves; returns each frame's box of `node`
/// on the popup.
fn run(sh: &mut Shell, node: NodeId) -> Vec<LogicalRect> {
    let mut seen = Vec::new();
    let mut n = 0;
    while sh.wants() {
        sh.paint();
        if let Some(p) = sh.popup() {
            seen.extend(sh.rect(p, node));
        }
        n += 1;
        assert!(n < 600, "never settled");
    }
    seen
}

/// A renderer whose text is shaped on the text worker, as `strand run`
/// shapes it: a layout asked for in a frame arrives after that frame.
fn worker_renderer() -> Renderer {
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let data = std::fs::read(test_font_path()).unwrap();
    let worker = TextWorker::spawn(FontConfig::isolated(vec![Arc::new(data)])).unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    r.set_first_frame_wait(Duration::ZERO);
    r.set_new_text_wait(Duration::ZERO);
    r
}

/// The owner's window pill, reduced (owner-testing.md bug 9): a title
/// cut with `ellipsis: end` in a row whose width springs, room after it
/// (where the owner's pill has its app's name).
const TITLE: &str = "
state grown = false
bar Top {
  height: 30
  box { width: 100; height: 30
    popup { id: big; open: true; width: 420; height: 40; margin: 0
      row { bg: #202020; clip: true; height: 40; align: center
            width: grown ? 400 : 90 ~ spring(320, 0.62)
        text \"Firefox: a window title much too long for the pill it is in\" {
          shrink: 1; min_width: 0; ellipsis: end; color: #ffffff }
        box { width: 30; height: 20; shrink: 0 }
      }
    }
  }
}
";

/// (owner-testing.md bug 9) Text cut with `ellipsis` in a box whose
/// width springs stays cut on every frame. Each frame asks for a layout
/// of the new width, which the text worker delivers after that frame;
/// meanwhile a layout of another width stands in, never the whole text
/// spilling past its box (into the room beside it), which was drawn
/// on every frame whose width had changed: the text flickered between
/// cut and whole as the card morphed. At scales 1 and 1.5, growing and
/// shrinking; a frame midway matches the reference (pixel test).
#[test]
fn cut_text_stays_cut_while_its_box_springs() {
    for scale in [1.0, 1.5] {
        let mut sh = Shell::boot_with(worker_renderer(), TITLE, scale, 800);
        sh.settle_text = true;
        run(&mut sh, NodeId::new(0, 0));
        let pop = sh.popup().expect("the popup is shown");
        let text = sh.find(sh.popup_node(), NodeKind::Text);
        let f = f64::from(scale);
        for (i, grown) in [true, false, true].into_iter().enumerate() {
            sh.set("grown", Value::Bool(grown));
            let mut frames = 0;
            let mut n = 0;
            while sh.wants() {
                sh.paint();
                n += 1;
                assert!(n < 600, "never settled");
                let Some(r) = sh.rect(pop, text) else {
                    continue;
                };
                let buf = sh.buf(pop);
                let y = (f64::from(r.y + r.h / 2.0) * f) as u32;
                let right = (f64::from(r.x + r.w) * f).ceil() as u32;
                // White glyph ink.
                let rows = y.saturating_sub(4)..(y + 4).min(buf.size.h);
                let ink = (0..buf.size.w)
                    .rev()
                    .find(|x| rows.clone().any(|yy| buf.px(*x, yy)[1] > 120));
                if scale == 1.5 && i == 0 && n == 4 {
                    // Midway through the first growth: cut, with its
                    // ellipsis, inside its box.
                    assert_matches_ref("morph_cut_text_midway", buf, 2);
                }
                if let Some(ink) = ink {
                    assert!(
                        ink <= right,
                        "scale {scale}, grown {grown}, frame {n}: ink to {ink}, its box ends at {right}"
                    );
                    frames += 1;
                }
            }
            assert!(frames > 10, "it sprang: {frames} frames");
        }
    }
}

/// The owner's status row, reduced (owner-testing.md bug 10): the
/// music bars (red) at the start of a row at the bar's end, and a
/// reading mounted after them. Its text is far narrower than its length
/// says (an icon font's ligature, `battery_5_bar`, draws one glyph);
/// here a run of narrow letters.
const READING: &str = "
state big = false
bar Top {
  height: 30
  row { width: 100%; height: 30; justify: end; align: center
    row { gap: 6; align: center
      box { width: 12; height: 12; bg: #ff0000 }
      if big { text \"iiiiiiiiiiiiiiiiiiii\" { color: #ffffff } }
    }
  }
}
";

/// Where the red box starts on row `y` of `buf`, in physical pixels.
fn red_from(buf: &Buffer, y: u32) -> Option<u32> {
    (0..buf.size.w).find(|x| {
        let p = buf.px(*x, y);
        p[2] > 200 && p[1] < 80
    })
}

/// The red box's place on each frame from `big` turning true until
/// nothing moves, on `r`.
fn bars_track(r: Renderer, settle_text: bool, scale: f32) -> Vec<u32> {
    let mut sh = Shell::boot_with(r, READING, scale, 400);
    sh.settle_text = settle_text;
    run(&mut sh, NodeId::new(0, 0));
    let y = (15.0 * scale) as u32;
    assert_eq!(
        red_from(sh.buf(BAR), y),
        Some(((400.0 - 12.0) * scale) as u32)
    );
    sh.set("big", Value::Bool(true));
    let mut seen = Vec::new();
    let mut n = 0;
    while sh.wants() {
        sh.paint();
        seen.extend(red_from(sh.buf(BAR), y));
        n += 1;
        assert!(n < 600, "never settled");
    }
    seen
}

/// (owner-testing.md bug 10) A text mounted beside siblings moves them
/// on the text worker as it does when shaped at once: no frame lays it
/// out by an estimate of its width (its length in ems) and then moves
/// everything again when its shaped size arrives. The music bars of the
/// owner's crown slid away and back as the readings after them mounted.
#[test]
fn a_mounted_text_moves_its_siblings_once() {
    for scale in [1.0, 1.5] {
        let inline = bars_track(renderer(), false, scale);
        let worker = bars_track(worker_renderer(), true, scale);
        assert!(inline.len() > 5, "the bars glide: {inline:?}");
        assert_eq!(worker, inline, "scale {scale}");
    }
}

/// A popup opened with its card (owner-testing.md bug 12, B-c): the
/// card mounts as the popup opens, its `enter` pose at the island's size.
const OPENING: &str = "
state shown = false
bar Top {
  height: 30
  box { width: 100; height: 30
    popup { id: big; open: shown; width: 500; height: 228; margin: 0; align: start
      if shown {
        col { bg: #ff0000; width: 400; height: 200; x: 50
              enter { width: 100; height: 30; x: 200 } }
      }
    }
  }
}
";

/// (owner-testing.md bug 12, B-c) A size in the `enter` pose of content
/// mounted as its popup opens plays from the popup's first frame, as
/// the pose's `x` does: the popup is laid out at rest before its first
/// frame (`configure_surface`, with no clock yet), and that preview took
/// the size springs waiting to start and dropped them. The card's first
/// frame is at the pose's size and it springs to its own.
#[test]
fn a_size_in_an_enter_pose_plays_as_its_popup_opens() {
    let mut sh = Shell::boot(OPENING, 1.0, 800);
    run(&mut sh, NodeId::new(0, 0));
    assert!(sh.popup().is_none());
    sh.set("shown", Value::Bool(true));
    let card = sh.find(sh.popup_node(), NodeKind::Col);
    let mut seen = Vec::new();
    let mut n = 0;
    while sh.wants() {
        sh.paint();
        let pop = sh.popup().expect("the popup is shown");
        seen.extend(sh.rect(pop, card).map(|r| (r.w, r.h)));
        n += 1;
        assert!(n < 600, "never settled");
    }
    let first = seen.first().copied().expect("frames");
    assert!(
        first.0 < 150.0 && first.1 < 60.0,
        "starts at the pose's size: {seen:?}"
    );
    assert!(
        seen.iter()
            .filter(|(w, _)| *w > 120.0 && *w < 380.0)
            .count()
            >= 2,
        "springs: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&(400.0, 200.0)));
}
