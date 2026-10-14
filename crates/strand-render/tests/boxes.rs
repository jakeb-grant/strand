//! (M6) `id.box` end to end, offline (spec.md 6.3, decisions.md
//! m6-w2-surface, the owner's ruling of 2026-10-14): programs compiled
//! and instantiated, render laying them out and handing the boxes logic
//! reads back to it (`Renderer::take_layout_facts` → `Instance::set_box`),
//! as `strand run` does. The workspace indicator follows the current
//! workspace's box with no offsets computed by hand; the owner's
//! travelling island is one `trigger: hover` popup over the whole status
//! pill, its island springing to the hovered item's box; input outside the
//! drawn island passes through; boxes are logical on every output scale;
//! and a settled shell does no work for them.
//! Regenerate with `STRAND_BLESS=1 cargo test -p strand-render --test boxes`.

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

/// A compiled shell, its instance and a renderer showing its surfaces.
struct Shell {
    inst: Instance,
    r: Renderer,
    /// Each shown surface: its id, node and buffer.
    shown: Vec<(SurfaceId, NodeId, Buffer)>,
    /// Layout facts handed to logic so far.
    facts: usize,
}

impl Shell {
    /// Boots `src` with one screen per `(name, scale)`, each bar shown
    /// `width` × 30 at its scale.
    fn boot(src: &str, screens: &[(&str, f32)], width: u32) -> Shell {
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
        let all = screens
            .iter()
            .map(|(n, _)| {
                host.record(
                    "Screen",
                    &[
                        ("id", Value::text(format!("Mock | {n}"))),
                        ("name", Value::text(*n)),
                    ],
                )
            })
            .collect();
        host.set(&rt, "screens.all", Value::list(all)).unwrap();
        let inst = Instance::new(&rt, program, host.clone(), Storage::none());
        let mut r = renderer();
        r.set_query_wait(Duration::from_secs(30));
        r.set_busy_window(Duration::ZERO);
        assert!(r.apply(inst.flush().diff).is_empty());
        r.take_surface_changes();
        let bars: Vec<NodeId> = r
            .tree()
            .surface_nodes()
            .filter(|n| r.tree().get(*n).is_some_and(|n| n.kind == NodeKind::Bar))
            .collect();
        assert_eq!(bars.len(), screens.len(), "a bar per screen");
        let mut shown = Vec::new();
        for (i, (bar, (name, scale))) in bars.iter().zip(screens).enumerate() {
            let id = SurfaceId(i as u32 + 1);
            let scale = Scale::from_f64(f64::from(*scale)).unwrap();
            r.attach_surface(id, *bar);
            r.configure_surface(id, Size::new(width, 30), scale);
            r.set_surface_origin(
                id,
                Some((Some(name.to_string()), LogicalPoint::new(0.0, 0.0))),
            );
            let f = scale.as_f64();
            let w = (f64::from(width) * f).ceil() as u32;
            let h = (30.0 * f).ceil() as u32;
            shown.push((id, *bar, Buffer::new(w, h, scale)));
        }
        let mut sh = Shell {
            inst,
            r,
            shown,
            facts: 0,
        };
        sh.paint(0);
        sh
    }

    /// Paints every surface that wants a frame at frame `k`, handing the
    /// boxes that changed to logic and its answer back, until nothing more
    /// is held for an answer. Returns whether anything painted.
    fn paint(&mut self, k: u32) -> bool {
        let mut painted = false;
        for _ in 0..8 {
            let mut any = false;
            for (id, _, buf) in &mut self.shown {
                if self.r.wants_frame(*id) || k == 0 {
                    buf.paint_at(&mut self.r, *id, 1, frame(k));
                    any = true;
                }
            }
            painted |= any;
            if !self.answer() {
                break;
            }
        }
        painted
    }

    /// Hands logic the layout facts render has, and its diff back; true
    /// if there were any.
    fn answer(&mut self) -> bool {
        let facts = self.r.take_layout_facts();
        if facts.is_empty() {
            return false;
        }
        self.facts += facts.len();
        for (n, b) in facts {
            self.inst.set_box(n, b);
        }
        let mut diff = self.inst.flush().diff;
        diff.layout_seen = Some(self.r.layout_seq());
        assert!(self.r.apply(diff).is_empty());
        true
    }

    /// Applies what logic has after an event or a service change.
    fn flush(&mut self) {
        assert!(self.r.apply(self.inst.flush().diff).is_empty());
    }

    fn buf(&self, id: SurfaceId) -> &Buffer {
        &self.shown.iter().find(|s| s.0 == id).unwrap().2
    }

    /// The node of the first text reading `t`.
    fn text(&self, t: &str) -> NodeId {
        let tree = self.r.tree();
        let mut work: Vec<NodeId> = tree.surface_nodes().collect();
        while let Some(n) = work.pop() {
            let node = tree.get(n).unwrap();
            if matches!(node.get(Prop::Text), Some(PropValue::Text(s)) if s.as_str() == t) {
                return n;
            }
            work.extend(node.children.iter().copied());
        }
        panic!("no text {t:?}");
    }
}

/// Where the first run of pixels whose red channel is over 200 starts and
/// ends on row `y` of `buf` (physical pixels).
fn red_run(buf: &Buffer, y: u32) -> Option<(u32, u32)> {
    let red = |x: u32| {
        let p = buf.px(x, y);
        p[2] > 200 && p[1] < 80 && p[0] < 80
    };
    let start = (0..buf.size.w).find(|x| red(*x))?;
    let end = (start..buf.size.w).find(|x| !red(*x)).unwrap_or(buf.size.w);
    Some((start, end))
}

/// The owner's workspace indicator, as the spec writes it: a `place:
/// absolute` box in the workspace row whose `x` and `width` spring to
/// the current workspace's box (`cur`, a conditional id: the `for` copy
/// whose `n == current`), relative to the row (`ws`). Clicking `next`
/// moves `current` on; clicking `grow` widens the second workspace.
const WORKSPACES: &str = "
state current = 1
state wide = false
bar Top {
  height: 30; bg: #1e1e2e
  row {
    gap: 0
    box { width: 13; height: 30 }
    row {
      id: ws
      gap: 6; pad: 0, 8
      for n in [1, 2, 3] key n {
        box {
          height: 30; width: n == 2 && wide ? 50 : 24
          when n == current { id: cur }
        }
      }
      box {
        place: absolute; height: 4; bg: #ff0000
        x: cur.box.x - ws.box.x ~ spring(400, 1)
        width: cur.box.width ~ spring(400, 1)
      }
    }
    text \"next\" { width: 30; on click { current = current % 3 + 1 } }
    text \"grow\" { width: 30; on click { wide = !wide } }
  }
}
";

/// Runs frames from `k` until no surface wants one; returns the next
/// frame number and, per frame, the red run on row `y` of `surface`.
fn settle(sh: &mut Shell, surface: SurfaceId, y: u32, mut k: u32) -> (u32, Vec<(u32, u32)>) {
    let mut seen = Vec::new();
    // Facts a frame held for (a first box) are answered first.
    while sh.answer() || sh.shown.iter().any(|(id, _, _)| sh.r.wants_frame(*id)) {
        sh.paint(k);
        seen.extend(red_run(sh.buf(surface), y));
        k += 1;
        assert!(k < 2000, "never settled");
    }
    (k, seen)
}

/// The workspace indicator sits under the current workspace from the
/// first frame (its first box snaps), and when `current` moves it springs
/// there: every frame between is between the two boxes, and it settles
/// exactly under the new one. Logical numbers: the row starts at 13, pads
/// 8 and gaps 6, so workspace `n` (24 wide) is at 21 + 30 (n − 1).
#[test]
fn the_workspace_indicator_follows_the_current_box() {
    let mut sh = Shell::boot(WORKSPACES, &[("DP-1", 1.0)], 300);
    let (mut k, _) = settle(&mut sh, BAR, 1, 1);
    assert_eq!(red_run(sh.buf(BAR), 1), Some((21, 45)), "under the first");

    sh.inst.event(sh.text("next"), "click", Vec::new());
    sh.flush();
    let (k2, seen) = settle(&mut sh, BAR, 1, k);
    k = k2;
    assert!(seen.len() > 5, "it travels: {seen:?}");
    assert!(
        seen.iter()
            .all(|(a, b)| (21..=51).contains(a) && b - a == 24),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|(a, _)| (30..=42).contains(a)),
        "a frame mid-way: {seen:?}"
    );
    assert_eq!(red_run(sh.buf(BAR), 1), Some((51, 75)), "under the second");

    // (A spring following a `.box` change.) The second workspace widens:
    // its box changes, the indicator's width springs after it.
    sh.inst.event(sh.text("grow"), "click", Vec::new());
    sh.flush();
    let (_, seen) = settle(&mut sh, BAR, 1, k);
    assert!(seen.len() > 5, "it springs: {seen:?}");
    assert!(
        seen.iter().any(|(a, b)| b - a > 30 && b - a < 44),
        "a frame mid-way: {seen:?}"
    );
    assert_eq!(red_run(sh.buf(BAR), 1), Some((51, 101)), "the new box");
}

/// The bar on each screen is its own instance: `.box` is in logical
/// pixels whatever the output's scale, so the same program gives the same
/// boxes on a 1× and a 1.5× output (the facts logic gets are equal), and
/// the indicator is drawn 1.5 times as far in on the scaled one.
#[test]
fn boxes_are_logical_on_mixed_scale_outputs() {
    let mut sh = Shell::boot(WORKSPACES, &[("DP-1", 1.0), ("DP-2", 1.5)], 300);
    settle(&mut sh, BAR, 1, 1);
    // What the workspaces' boxes read on each bar.
    let mut per_bar: Vec<Vec<LogicalRect>> = Vec::new();
    for (id, bar, _) in &sh.shown {
        let row = sh.r.tree().get(*bar).unwrap().children[0];
        let ws = sh.r.tree().get(row).unwrap().children[1];
        let kids = sh.r.tree().get(ws).unwrap().children.clone();
        let root = sh.r.node_rect(*id, *bar).unwrap();
        per_bar.push(
            kids[..3]
                .iter()
                .map(|n| {
                    let r = sh.r.node_rect(*id, *n).unwrap();
                    LogicalRect::new(r.x - root.x, r.y - root.y, r.w, r.h)
                })
                .collect(),
        );
    }
    assert_eq!(per_bar[0], per_bar[1], "the same logical boxes");
    assert_eq!(per_bar[0][1], LogicalRect::new(51.0, 0.0, 24.0, 30.0));
    assert_eq!(red_run(sh.buf(BAR), 1), Some((21, 45)));
    // 21 × 1.5 = 31.5 and 45 × 1.5 = 67.5: the half pixels are edges.
    let (a, b) = red_run(sh.buf(SurfaceId(2)), 2).expect("drawn at 1.5×");
    assert!((31..=32).contains(&a) && (67..=68).contains(&b), "{a}, {b}");
}

/// A shell whose boxes are settled does no work for them: no facts go to
/// logic, logic sends nothing, no surface wants a frame and nothing wakes
/// render, however much time passes.
#[test]
fn a_settled_box_wakes_nothing() {
    let mut sh = Shell::boot(WORKSPACES, &[("DP-1", 1.0)], 300);
    let (k, _) = settle(&mut sh, BAR, 1, 1);
    let facts = sh.facts;
    assert!(facts > 0, "boxes were reported");
    assert!(sh.r.take_layout_facts().is_empty());
    assert!(sh.inst.flush().diff.is_empty(), "logic has nothing to send");
    assert!(!sh.r.wants_frame(BAR));
    assert_eq!(sh.r.next_wake(), None);
    // Painted again much later: nothing moved, nothing reported.
    let (id, _, buf) = &mut sh.shown[0];
    let damage = buf.paint_at(&mut sh.r, *id, 1, frame(k + 600));
    assert!(damage.is_empty(), "{damage:?}");
    assert!(sh.r.take_layout_facts().is_empty());
    assert!(sh.inst.flush().diff.is_empty());
    assert_eq!(sh.facts, facts);
}

/// The owner's travelling island with no group primitive: one `trigger:
/// hover` popup anchored to the whole status pill, sized from the pill
/// (`width: pill.box.width + 16`, a box on another surface), drawing
/// nothing itself; inside it one island whose `x` and `width` spring to
/// the hovered item's box (`it`, a conditional id on each item), its
/// content chosen by which item it is. Clicking an item stands in for
/// hovering it.
const ISLAND: &str = "
state hovered = 1
bar Top {
  height: 30; bg: #1e1e2e
  row {
    gap: 0
    box { width: 40; height: 30 }
    row {
      id: pill
      gap: 20
      box { width: 30; height: 30; bg: #444444; on click { hovered = 1 }
            when hovered == 1 { id: it } }
      box { width: 60; height: 30; bg: #444444; on click { hovered = 2 }
            when hovered == 2 { id: it } }
      box { width: 40; height: 30; bg: #444444; on click { hovered = 3 }
            when hovered == 3 { id: it } }
      popup {
        id: pop
        trigger: hover
        width: pill.box.width + 16; height: 50
        col {
          place: absolute; height: 40; radius: 8; pad: 8
          bg: #ff0000
          x: it.box.x - pop.box.x - 8 ~ spring(300, 1)
          width: it.box.width + 16 ~ spring(300, 1)
          if hovered == 3 { box { width: 12; height: 12; bg: #0000ff } }
          else { box { width: 12; height: 12; bg: #00ff00 } }
        }
      }
    }
  }
}
";

const BAR: SurfaceId = SurfaceId(1);
const POP: SurfaceId = SurfaceId(2);

/// The pill row of the island shell and its items.
fn pill(sh: &Shell) -> (NodeId, Vec<NodeId>) {
    let bar = sh.shown[0].1;
    let row = sh.r.tree().get(bar).unwrap().children[0];
    let pill = sh.r.tree().get(row).unwrap().children[1];
    (pill, sh.r.tree().get(pill).unwrap().children.clone())
}

/// `src` (the island shell, or a variant) with its popup open and shown
/// on `POP`, placed centred under the pill (its box's top-left at
/// (32, 30) on the bar), settled. Returns it and the popup's spec.
fn island_shell(src: &str) -> (Shell, SurfaceSpec) {
    let mut sh = Shell::boot(src, &[("DP-1", 1.0)], 400);
    settle(&mut sh, BAR, 1, 1);
    let (_, items) = pill(&sh);
    let popup = *items.last().unwrap();
    assert_eq!(sh.r.tree().get(popup).unwrap().kind, NodeKind::Popup);
    sh.inst
        .write(popup, Prop::Open, PropValue::Bool(true))
        .unwrap();
    sh.flush();
    let spec = sh
        .r
        .take_surface_changes()
        .into_iter()
        .rev()
        .find_map(|(n, c)| match c {
            SurfaceChange::Created(s) | SurfaceChange::Updated { spec: s, .. } if n == popup => {
                Some(s)
            }
            _ => None,
        })
        .expect("the popup's spec");
    let o = spec.overhang;
    let (w, h) = (spec.width.unwrap(), spec.height.unwrap());
    let size = Size::new(
        (w + o.left + o.right).ceil() as u32,
        (h + o.top + o.bottom).ceil() as u32,
    );
    sh.r.attach_surface(POP, popup);
    sh.r.configure_surface(POP, size, Scale::ONE);
    sh.r.set_surface_origin(
        POP,
        Some((None, LogicalPoint::new(32.0 - o.left, 30.0 - o.top))),
    );
    sh.shown
        .push((POP, popup, Buffer::new(size.w, size.h, Scale::ONE)));
    settle(&mut sh, POP, 20, 100);
    (sh, spec)
}

/// The island opens under the first item (its first box snaps: no travel
/// from 0, 0), travels to the third when it is hovered, and the frame
/// nearest halfway shows it between the two, already holding the third's
/// content (pixel test). Popup-relative numbers: the items are at 8, 58
/// and 138, the island 8 wider each side: 0..46, then 130..186.
#[test]
fn the_island_travels_between_items() {
    let (mut sh, spec) = island_shell(ISLAND);
    // As wide as the pill (30 + 20 + 60 + 20 + 40) and its margins.
    assert_eq!((spec.width, spec.height), (Some(186.0), Some(50.0)));
    assert_eq!(red_run(sh.buf(POP), 20), Some((0, 46)), "under the first");

    let (_, items) = pill(&sh);
    sh.inst.event(items[2], "click", Vec::new());
    sh.flush();
    let mut k = 600;
    let mut best: Option<(u32, Buffer)> = None;
    while sh.answer() || sh.r.wants_frame(POP) || sh.r.wants_frame(BAR) {
        sh.paint(k);
        if let Some((a, _)) = red_run(sh.buf(POP), 20) {
            let off = a.abs_diff(65);
            if best.as_ref().is_none_or(|(b, _)| off < *b) {
                best = Some((off, sh.buf(POP).clone()));
            }
        }
        k += 1;
        assert!(k < 2000, "never settled");
    }
    assert_eq!(
        red_run(sh.buf(POP), 20),
        Some((130, 186)),
        "under the third"
    );
    let (off, mid) = best.expect("frames between");
    assert!(off < 10, "a frame near halfway: {off}");
    assert_matches_ref("box_island_midpoint", &mid, 3);
}

/// The popup draws nothing itself: it takes input only where its island
/// is drawn (`Painter::input_region`), so a press beside the island goes
/// through to whatever is under the popup. With a background of its own
/// it takes its whole box (the surface manager's region); a bar keeps
/// its own.
#[test]
fn input_passes_through_outside_the_island() {
    let (sh, _) = island_shell(ISLAND);
    let region = Painter::input_region(&sh.r, POP).expect("the island's region");
    assert_eq!(region, vec![LogicalRect::new(0.0, 0.0, 46.0, 40.0)]);
    assert_eq!(Painter::input_region(&sh.r, BAR), None);

    let painted = ISLAND.replace(
        "        id: pop\n",
        "        id: pop\n        bg: #101010\n",
    );
    let (sh, _) = island_shell(&painted);
    assert_eq!(Painter::input_region(&sh.r, POP), None, "its whole box");
}
