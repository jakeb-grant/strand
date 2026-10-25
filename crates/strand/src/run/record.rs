//! (report) The glitch report's two recorders (docs/architecture.md,
//! "Glitch reports"; design.md, "Reporting a glitch"): [`InputRecorder`]
//! on the main thread, a field of `demo::host::Host`, and
//! [`ChangeRecorder`] on the logic thread, owned by `Shell`. Each owns a
//! `strand_core::flight` ring outright and writes to it on a path that
//! already runs (an input event's routing, a surface callback, a
//! service's pump, a tick's cell writes, a reload), so nothing is
//! shared, locked or woken for it. A report dumps both.
//!
//! **What is never recorded** (decisions.md report). A key that types a
//! character (its text not empty, no ctrl, alt or super) is counted,
//! never named; the release of a key is recorded only when its press was
//! (a named key), so no character leaks through its release. While the
//! focused node of the key's surface is a password input every key,
//! named or not, joins one span with no count. A paste is its count
//! (none into a password), a drop its kinds. Nothing is appended from a
//! lock asked for to the unlock (a gap), nor (services-round) while an
//! admin password dialog or a login screen is up (`polkit.request`, a
//! greeter: its own gap), nor while the inspector picks, and a lock
//! surface's input never (the host does not pass it).
//! `auth` is never recorded; `clipboard`'s texts are lengths; a text of
//! the config's own (a cell, an `rw` write) is kept only when it is a
//! literal of the program; a call's arguments are never kept, and the
//! answer of a call given a text that is not a literal has every text
//! held back.
//!
//! **On and off** (`system.replay`): off drops the ring (its memory is
//! freed), on allocates it anew, empty.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::time::Duration;

use strand_compiler::lower::Program;
use strand_compiler::vm::Value;
use strand_compiler::vm::host::PathSeg;
use strand_core::flight::{
    self, ARef, ArenaWriter, NAME_IN_ARENA, Rec, Ring, RingSpec, flags, gap, kind, modifiers,
};
use strand_render::InputKind;
use strand_scene::{AxisSource, ButtonState, DropKind, DropPayload, InputEvent, NodeId, NodeKind};
use strand_scene::{KeyInput, SurfaceId};
use strand_services::{Change, Data, TapEvent};

/// `BTN_MOUSE`: a button record holds the evdev code less this.
const BTN_MOUSE: u32 = 0x110;
/// The refresh period a surface's motion merges by until presentation
/// says otherwise.
const DEFAULT_PERIOD_NS: u64 = 16_666_667;
/// Named keys held down whose release will be recorded.
const HELD_KEYS: usize = 16;

/// A `NodeId` as a record's u64: generation high, index low.
pub(crate) fn node_bits(n: NodeId) -> u64 {
    (u64::from(n.generation) << 32) | u64::from(n.index)
}

/// The `NodeId` a record's u64 holds.
#[cfg(test)]
pub(crate) fn node_of(bits: u64) -> NodeId {
    NodeId::new(bits as u32, (bits >> 32) as u32)
}

/// A surface root's kind as a mapped record's byte.
pub(crate) fn kind_byte(k: NodeKind) -> u8 {
    match k {
        NodeKind::Bar => 0,
        NodeKind::Panel => 1,
        NodeKind::Osd => 2,
        NodeKind::Popup => 3,
        _ => 4,
    }
}

/// FNV-1a, for the named keys held down.
fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A ring when `on`, none when off.
fn ring_if(on: bool, spec: RingSpec, which: u8) -> Option<Ring> {
    on.then(|| Ring::new(spec, which))
}

/// What the input recorder knows of a surface.
#[derive(Clone, Copy, Debug)]
struct SurfaceSlot {
    slot: u16,
    /// Its refresh period, ns (the last told).
    period: Option<u64>,
    /// When its last frame was painted, and whether it was animating
    /// then (it wanted the next frame).
    painted: Option<(u64, bool)>,
    gpu: bool,
}

/// One mapped surface as the bundle lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SurfaceEntry {
    pub surface: SurfaceId,
    pub slot: u16,
    pub root: NodeId,
    pub output: Option<String>,
    pub gpu: bool,
}

/// An output's logical size, scale (120ths) and position, as its
/// `OUTPUT` record holds them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutputGeometry {
    size: (i32, i32),
    scale: u32,
    position: (i32, i32),
}

/// The main thread's recorder: see the module docs.
#[derive(Debug)]
pub(crate) struct InputRecorder {
    ring: Option<Ring>,
    locked: bool,
    /// (services-round) An admin password dialog or a login screen is up
    /// (logic's `ToMain::Secret`).
    secret: bool,
    picking: bool,
    surfaces: HashMap<SurfaceId, SurfaceSlot>,
    /// Each mapped surface's root, connector, kind and output slot.
    roots: HashMap<SurfaceId, (NodeId, Option<String>, NodeKind, Option<u16>)>,
    next_surface: u16,
    /// Output slots by monitor identity, with the connector and the
    /// geometry last told (logical size, scale in 120ths, position),
    /// written again when recording starts anew.
    outputs: HashMap<String, OutputEntry>,
    next_output: u16,
    /// Named keys pressed while recording and not yet released: the
    /// surface slot and the name's hash.
    held: [(u16, u64); HELD_KEYS],
    held_next: usize,
    /// A frame was painted since [`after_dispatch`] last looked.
    painted_since: bool,
    /// [`after_dispatch`]'s list of surfaces, kept so it allocates once.
    scratch: Vec<SurfaceId>,
}

/// (report) After each dispatch of the main loop: the lock's gap (a lock
/// asked for or held, from strand-surface) and, when a frame was
/// painted, each surface's refresh period from its frame clock.
pub(crate) fn after_dispatch(state: &mut strand_surface::State<crate::demo::host::Host>) {
    let locked = state.lock_active();
    let t = flight::now_ns();
    let r = &mut state.host_mut().recorder;
    r.lock(t, locked);
    if !std::mem::take(&mut r.painted_since) || !r.on() {
        return;
    }
    let mut ids = std::mem::take(&mut r.scratch);
    ids.clear();
    ids.extend(r.surfaces.keys().copied());
    for id in &ids {
        let p = state.refresh(*id);
        state.host_mut().recorder.refresh(t, *id, p);
    }
    state.host_mut().recorder.scratch = ids;
}

impl InputRecorder {
    /// A recorder, recording when `on`.
    pub(crate) fn new(on: bool) -> InputRecorder {
        InputRecorder {
            ring: ring_if(on, flight::INPUT, flight::ring::INPUT),
            locked: false,
            secret: false,
            picking: false,
            surfaces: HashMap::new(),
            roots: HashMap::new(),
            next_surface: 0,
            outputs: HashMap::new(),
            next_output: 0,
            held: [(0, 0); HELD_KEYS],
            held_next: 0,
            painted_since: false,
            scratch: Vec::new(),
        }
    }

    /// Recording is on (`system.replay`).
    pub(crate) fn on(&self) -> bool {
        self.ring.is_some()
    }

    /// `system.replay` changed: off drops the ring, on starts one empty
    /// (paused at once if a lock or pick mode is on).
    pub(crate) fn set_on(&mut self, on: bool, t: u64) {
        if on == self.on() {
            return;
        }
        self.ring = ring_if(on, flight::INPUT, flight::ring::INPUT);
        self.held = [(0, 0); HELD_KEYS];
        if self.ring.is_some() {
            self.restart(t);
        }
        let g = self.gap();
        if let (Some(r), Some(g)) = (&mut self.ring, g) {
            r.pause(t, kind::GAP_BEGINS, g);
        }
    }

    /// The gap the ring is in: a lock, else (services-round) a password
    /// dialog or login screen, else the inspector's pick.
    fn gap(&self) -> Option<u8> {
        if self.locked {
            Some(gap::LOCK)
        } else if self.secret {
            Some(gap::SECRET)
        } else if self.picking {
            Some(gap::PICK)
        } else {
            None
        }
    }

    /// The gap was `before`: it ends and the one now begins.
    fn regap(&mut self, t: u64, before: Option<u8>) {
        let after = self.gap();
        if before == after {
            return;
        }
        let Some(r) = &mut self.ring else {
            return;
        };
        if let Some(g) = before {
            r.resume(t, kind::GAP_ENDS, g);
        }
        if let Some(g) = after {
            r.pause(t, kind::GAP_BEGINS, g);
        }
    }

    /// (report review) A ring started anew by `system.replay` turned on
    /// again: the recorder off ends here (a replay's window starts after
    /// it), and the outputs, the mapped surfaces and their refresh
    /// periods, which the new ring has no record of, are written again
    /// as they are now.
    fn restart(&mut self, t: u64) {
        let Some(r) = &mut self.ring else {
            return;
        };
        r.push(Rec::new(t, kind::GAP_ENDS, 0).u8_at(0, gap::OFF));
        let mut outs: Vec<(&String, &OutputEntry)> = self.outputs.iter().collect();
        outs.sort_by_key(|(_, o)| o.0);
        for (id, (slot, connector, geometry)) in outs {
            if let Some(g) = geometry {
                let name = r.name(connector.as_deref().unwrap_or(id));
                r.push(output_rec(t, *slot, name, *g));
            }
        }
        let mut surfaces: Vec<(&SurfaceId, &SurfaceSlot)> = self.surfaces.iter().collect();
        surfaces.sort_by_key(|(_, s)| s.slot);
        for (id, s) in surfaces {
            if let Some((root, _, k, out)) = self.roots.get(id) {
                r.push(mapped_rec(t, s.slot, *root, *out, *k));
            }
            if let Some(p) = s.period {
                r.push(
                    Rec::new(t, kind::REFRESH, s.slot)
                        .u32_at(0, u32::try_from(p).unwrap_or(u32::MAX)),
                );
            }
        }
    }

    /// The ring (tests).
    #[cfg(test)]
    pub(crate) fn ring(&self) -> Option<&Ring> {
        self.ring.as_ref()
    }

    /// The dump, if recording.
    pub(crate) fn dump(&self) -> Option<Vec<u8>> {
        self.ring.as_ref().map(|r| r.dump(flight::now_ns()))
    }

    /// A lock was asked for or is held (`true`), or is over: a gap from
    /// the first to the second.
    pub(crate) fn lock(&mut self, t: u64, locked: bool) {
        if locked == self.locked {
            return;
        }
        let before = self.gap();
        self.locked = locked;
        self.held = [(0, 0); HELD_KEYS];
        self.regap(t, before);
    }

    /// (services-round) An admin password dialog or a login screen came
    /// or went (`polkit.request`, `greetd`): a gap, as a lock is.
    pub(crate) fn secret(&mut self, t: u64, on: bool) {
        if on == self.secret {
            return;
        }
        let before = self.gap();
        self.secret = on;
        self.held = [(0, 0); HELD_KEYS];
        self.regap(t, before);
    }

    /// The inspector's pick mode began or ended: a gap.
    pub(crate) fn pick(&mut self, t: u64, on: bool) {
        if on == self.picking {
            return;
        }
        let before = self.gap();
        self.picking = on;
        self.regap(t, before);
    }

    fn slot(&self, surface: SurfaceId) -> Option<u16> {
        self.surfaces.get(&surface).map(|s| s.slot)
    }

    /// The named keys held down on surface `slot` are forgotten (its
    /// keyboard focus left, or it went).
    fn forget_held(&mut self, slot: u16) {
        for e in &mut self.held {
            if e.0 == slot {
                *e = (0, 0);
            }
        }
    }

    /// The input event `event`, as routed: `focused` is what has the
    /// keyboard on its surface (`Router::focused_input`), `hit` the
    /// innermost node a press resolved to.
    pub(crate) fn input(
        &mut self,
        t: u64,
        event: &InputEvent,
        focused: Option<InputKind>,
        hit: Option<NodeId>,
    ) {
        let Some(slot) = self.slot(event.surface()) else {
            return;
        };
        let period = self
            .surfaces
            .get(&event.surface())
            .and_then(|s| s.period)
            .unwrap_or(DEFAULT_PERIOD_NS);
        let password = focused == Some(InputKind::Password);
        let Some(r) = &mut self.ring else {
            return;
        };
        if r.paused() {
            return;
        }
        let at = |k: u8, x: f32, y: f32| Rec::new(t, k, slot).f32_at(0, x).f32_at(4, y);
        match event {
            InputEvent::PointerEnter { position, .. } => {
                r.push(at(kind::ENTER, position.x, position.y));
            }
            InputEvent::PointerLeave { .. } => r.push(Rec::new(t, kind::LEAVE, slot)),
            InputEvent::PointerMotion { position, .. } => {
                r.merge_motion(slot, t, position.x, position.y, period);
            }
            InputEvent::PointerButton {
                position,
                button,
                state,
                ..
            } => {
                let pressed = *state == ButtonState::Pressed;
                let b = u16::try_from(button.saturating_sub(BTN_MOUSE)).unwrap_or(u16::MAX);
                r.push(
                    at(kind::BUTTON, position.x, position.y)
                        .u16_at(8, b)
                        .u8_at(10, u8::from(pressed)),
                );
                if pressed && let Some(n) = hit {
                    r.push(Rec::new(t, kind::HIT, slot).u64_at(0, node_bits(n)));
                }
            }
            InputEvent::PointerAxis {
                horizontal,
                vertical,
                source,
                ..
            } => {
                let source = match source {
                    None => 0,
                    Some(AxisSource::Wheel) => 1,
                    Some(AxisSource::Finger) => 2,
                    Some(AxisSource::Continuous) => 3,
                    Some(AxisSource::WheelTilt) => 4,
                };
                // (report review) The vertical axis's notches only: a
                // horizontal scroll is its `dx`, so a replay never reads
                // a tilt as a vertical scroll.
                let discrete = vertical.value120;
                let discrete = discrete.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                r.push(
                    Rec::new(t, kind::AXIS, slot)
                        .f32_at(0, horizontal.pixels as f32)
                        .f32_at(4, vertical.pixels as f32)
                        .u8_at(8, source)
                        .u8_at(9, u8::from(horizontal.stop || vertical.stop))
                        .i16_at(10, discrete),
                );
            }
            InputEvent::KeyboardEnter { .. } => r.push(Rec::new(t, kind::KEYBOARD_ENTER, slot)),
            InputEvent::KeyboardLeave { .. } => {
                r.push(Rec::new(t, kind::KEYBOARD_LEAVE, slot));
                // (report review) Wayland sends no release after `leave`:
                // a named key still down here is forgotten, so a later
                // press of its letter that types cannot end in a release
                // that names it.
                self.forget_held(slot);
            }
            InputEvent::Key { key, .. } => self.key(t, slot, key, password),
            InputEvent::ClickAway { .. } => r.push(Rec::new(t, kind::CLICK_AWAY, slot)),
            InputEvent::DragEnter { at: p, kinds, .. } => {
                let mask = kinds.iter().fold(0u16, |m, k| m | kind_bit(*k));
                r.push(drag(t, slot, 0, p.x, p.y, mask));
            }
            InputEvent::DragMotion { at: p, .. } => r.push(drag(t, slot, 1, p.x, p.y, 0)),
            InputEvent::DragLeave { .. } => r.push(drag(t, slot, 2, 0.0, 0.0, 0)),
            InputEvent::DragDrop { at: p, payload, .. } => {
                let mask = match payload {
                    DropPayload::Node(_) => 0,
                    DropPayload::External { kind, .. } => kind_bit(*kind),
                };
                r.push(drag(t, slot, 3, p.x, p.y, mask));
            }
            InputEvent::Paste { text, .. } => {
                let count = if password {
                    0
                } else {
                    u32::try_from(text.chars().count()).unwrap_or(u32::MAX)
                };
                r.push(
                    Rec::new(t, kind::PASTE, slot)
                        .u32_at(0, count)
                        .u8_at(4, u8::from(password)),
                );
            }
        }
    }

    fn key(&mut self, t: u64, slot: u16, key: &KeyInput, password: bool) {
        let Some(r) = &mut self.ring else {
            return;
        };
        let m = key.modifiers;
        let pressed = key.state == ButtonState::Pressed;
        if password {
            if pressed {
                r.merge_text(slot, t, true);
            }
            return;
        }
        let types = !key.text.is_empty() && !m.ctrl && !m.alt && !m.logo;
        let h = hash(&key.name);
        if !pressed {
            // A release is recorded only for a named key whose press was.
            let Some(i) = self.held.iter().position(|e| *e == (slot, h) && h != 0) else {
                return;
            };
            self.held[i] = (0, 0);
        } else if types {
            r.merge_text(slot, t, false);
            // (report review) A key that types now is no named key held:
            // its release, whenever it comes, is not recorded.
            for e in &mut self.held {
                if *e == (slot, h) {
                    *e = (0, 0);
                }
            }
            return;
        } else if !key.repeat {
            self.held[self.held_next] = (slot, h);
            self.held_next = (self.held_next + 1) % HELD_KEYS;
        }
        let bits = (if m.shift { modifiers::SHIFT } else { 0 })
            | (if m.ctrl { modifiers::CTRL } else { 0 })
            | (if m.alt { modifiers::ALT } else { 0 })
            | (if m.logo { modifiers::SUPER } else { 0 });
        let state = match (pressed, key.repeat) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        let name = r.name(&key.name);
        let mut rec = Rec::new(t, kind::KEY, slot)
            .u16_at(0, name)
            .u16_at(2, bits)
            .u8_at(4, state);
        if name == NAME_IN_ARENA {
            let a = r.value(|w| {
                w.put(key.name.as_bytes());
                w.put(&[0]);
            });
            rec = rec.with_value(a);
        }
        r.push(rec);
    }

    /// `surface` was attached showing `root` on `output` (a monitor's
    /// identity and connector): a new slot.
    pub(crate) fn mapped(
        &mut self,
        t: u64,
        surface: SurfaceId,
        root: NodeId,
        output: Option<(&str, Option<&str>)>,
        kind: NodeKind,
    ) {
        let slot = self.next_surface;
        self.next_surface = self.next_surface.wrapping_add(1);
        self.surfaces.insert(
            surface,
            SurfaceSlot {
                slot,
                period: None,
                painted: None,
                gpu: false,
            },
        );
        let out = output.map(|(id, _)| self.output_slot(id));
        self.roots.insert(
            surface,
            (
                root,
                output.and_then(|(_, c)| c.map(str::to_string)),
                kind,
                out,
            ),
        );
        if let Some(r) = &mut self.ring {
            r.push(mapped_rec(t, slot, root, out, kind));
        }
    }

    fn output_slot(&mut self, id: &str) -> u16 {
        if let Some((s, _, _)) = self.outputs.get(id) {
            return *s;
        }
        let s = self.next_output;
        self.next_output = self.next_output.wrapping_add(1);
        self.outputs.insert(id.to_string(), (s, None, None));
        s
    }

    /// `surface` was configured: its logical size and scale (120ths).
    pub(crate) fn configured(&mut self, t: u64, surface: SurfaceId, w: u32, h: u32, scale: u32) {
        let Some(slot) = self.slot(surface) else {
            return;
        };
        if let Some(r) = &mut self.ring {
            let c = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            r.push(
                Rec::new(t, kind::CONFIGURED, slot)
                    .u16_at(0, c(w))
                    .u16_at(2, c(h))
                    .u16_at(4, c(scale)),
            );
        }
    }

    /// `surface`'s buffer lies at `(x, y)` on its output.
    pub(crate) fn placed(&mut self, t: u64, surface: SurfaceId, x: i32, y: i32) {
        let Some(slot) = self.slot(surface) else {
            return;
        };
        if let Some(r) = &mut self.ring {
            r.push(Rec::new(t, kind::PLACED, slot).i32_at(0, x).i32_at(4, y));
        }
    }

    /// `surface` went.
    pub(crate) fn unmapped(&mut self, t: u64, surface: SurfaceId) {
        let Some(s) = self.surfaces.remove(&surface) else {
            return;
        };
        self.roots.remove(&surface);
        self.forget_held(s.slot);
        if let Some(r) = &mut self.ring {
            r.push(Rec::new(t, kind::UNMAPPED, s.slot));
        }
    }

    /// An output was added or changed: its identity, connector, logical
    /// size, scale (120ths) and position.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn output(
        &mut self,
        t: u64,
        id: &str,
        connector: Option<&str>,
        size: (i32, i32),
        scale: u32,
        position: (i32, i32),
    ) {
        let slot = self.output_slot(id);
        let g = OutputGeometry {
            size,
            scale,
            position,
        };
        if let Some(e) = self.outputs.get_mut(id) {
            e.1 = connector.map(str::to_string);
            e.2 = Some(g);
        }
        let Some(r) = &mut self.ring else {
            return;
        };
        let name = r.name(connector.unwrap_or(id));
        r.push(output_rec(t, slot, name, g));
    }

    /// An output was unplugged.
    pub(crate) fn output_gone(&mut self, t: u64, id: &str) {
        let Some(slot) = self.outputs.get(id).map(|(s, ..)| *s) else {
            return;
        };
        if let Some(r) = &mut self.ring {
            r.push(Rec::new(t, kind::OUTPUT_GONE, slot));
        }
    }

    /// The frame clock's refresh period of `surface` (presentation
    /// feedback): recorded when first known or changed.
    pub(crate) fn refresh(&mut self, t: u64, surface: SurfaceId, period: Option<Duration>) {
        let Some(p) = period.map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)) else {
            return;
        };
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        if s.period == Some(p) || p == 0 {
            return;
        }
        s.period = Some(p);
        let slot = s.slot;
        if let Some(r) = &mut self.ring {
            r.push(
                Rec::new(t, kind::REFRESH, slot).u32_at(0, u32::try_from(p).unwrap_or(u32::MAX)),
            );
        }
    }

    /// A frame of `surface` was painted at `t` on the GPU or not;
    /// `animating`: it wants the next frame. A frame two or more refresh
    /// periods after the one before, while that one was animating, is
    /// late; a backend switch is recorded.
    pub(crate) fn painted(&mut self, t: u64, surface: SurfaceId, gpu: bool, animating: bool) {
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        self.painted_since = true;
        let period = s.period.unwrap_or(DEFAULT_PERIOD_NS).max(1);
        let late = match s.painted {
            Some((before, true)) if t.saturating_sub(before) >= 2 * period => {
                Some(t.saturating_sub(before) / period - 1)
            }
            _ => None,
        };
        s.painted = Some((t, animating));
        let switched = s.gpu != gpu;
        s.gpu = gpu;
        let slot = s.slot;
        let Some(r) = &mut self.ring else {
            return;
        };
        if let Some(missed) = late {
            r.push(
                Rec::new(t, kind::LATE, slot).u16_at(0, u16::try_from(missed).unwrap_or(u16::MAX)),
            );
        }
        if switched {
            r.push(Rec::new(t, kind::BACKEND, slot).u8_at(0, u8::from(gpu)));
        }
    }

    /// The mapped surfaces, by slot.
    pub(crate) fn surfaces(&self) -> Vec<SurfaceEntry> {
        let mut out: Vec<SurfaceEntry> = self
            .surfaces
            .iter()
            .filter_map(|(id, s)| {
                let (root, output, ..) = self.roots.get(id)?.clone();
                Some(SurfaceEntry {
                    surface: *id,
                    slot: s.slot,
                    root,
                    output,
                    gpu: s.gpu,
                })
            })
            .collect();
        out.sort_by_key(|e| e.slot);
        out
    }
}

/// An `OUTPUT` record of output `slot`, its name at `name`.
fn output_rec(t: u64, slot: u16, name: u16, g: OutputGeometry) -> Rec {
    let c16 = |v: i32| u16::try_from(v.max(0)).unwrap_or(u16::MAX);
    let i16_ = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    Rec::new(t, kind::OUTPUT, slot)
        .u16_at(0, name)
        .u16_at(2, c16(g.size.0))
        .u16_at(4, c16(g.size.1))
        .u16_at(6, u16::try_from(g.scale).unwrap_or(u16::MAX))
        .i16_at(8, i16_(g.position.0))
        .i16_at(10, i16_(g.position.1))
}

/// (replayfix) A root record's name index: its origin's, or
/// [`NAME_IN_ARENA`] with no arena value, which reads back as no name,
/// for a root with no origin or one the name table cannot hold (a root
/// record's payload has no room for an arena reference).
fn root_name(r: &mut Ring, origin: Option<&str>) -> u16 {
    origin.map_or(NAME_IN_ARENA, |o| r.name(o))
}

/// A `ROOT` record of surface root `node`, its origin at name `slot`.
fn root_rec(t: u64, slot: u16, node: NodeId, k: NodeKind) -> Rec {
    Rec::new(t, kind::ROOT, slot)
        .u64_at(0, node_bits(node))
        .u8_at(8, kind_byte(k))
}

/// A `MAPPED` record of surface `slot` showing `root` on output `out`.
fn mapped_rec(t: u64, slot: u16, root: NodeId, out: Option<u16>, k: NodeKind) -> Rec {
    Rec::new(t, kind::MAPPED, slot)
        .u64_at(0, node_bits(root))
        .u8_at(8, out.map_or(u8::MAX, |o| o as u8))
        .u8_at(9, kind_byte(k))
}

fn kind_bit(k: DropKind) -> u16 {
    DropKind::ALL
        .iter()
        .position(|d| *d == k)
        .map_or(0, |i| 1 << i)
}

fn drag(t: u64, slot: u16, phase: u8, x: f32, y: f32, mask: u16) -> Rec {
    Rec::new(t, kind::DRAG, slot)
        .f32_at(0, x)
        .f32_at(4, y)
        .u16_at(8, mask)
        .u8_at(10, phase)
}

/// A writer that keeps nothing: what [`ChangeRecorder::tainted`] walks
/// a value with.
struct Sink;

impl fmt::Write for Sink {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        Ok(())
    }
}

/// What a recorded call keeps until its answer arrives.
pub(crate) struct Call {
    id: u32,
    name: String,
    tainted: bool,
}

/// An output as the input recorder knows it: its slot, connector name
/// and (report review) geometry, to write again when recording restarts.
type OutputEntry = (u16, Option<String>, Option<OutputGeometry>);

/// The logic thread's recorder: see the module docs.
pub(crate) struct ChangeRecorder {
    ring: Option<Ring>,
    /// A lock is asked for or shown: the ring is paused.
    pub(super) locked: bool,
    /// (services-round) An admin password dialog or a login screen is
    /// up: the ring is paused.
    secret: bool,
    program: Arc<Program>,
    /// Surface roots recorded, by node: their origin (none for a
    /// surface Strand makes itself) and kind.
    roots: HashMap<NodeId, (Option<String>, NodeKind)>,
    next_call: u32,
    /// The config's declared services (`service mood from file …`): their
    /// fields' names in declaration order, by service. Such a service is
    /// one keyed list of its values by field index
    /// (`strand_services::custom`); it is recorded and snapshotted by its
    /// declared fields, as a builtin's.
    declared: HashMap<String, Vec<String>>,
    /// The last value recorded of each declared field (`mood.level`), as
    /// JSON: the old half of its next `FIELD` record.
    declared_last: HashMap<String, String>,
    /// (report review) The nodes the newest presses hit, as they were
    /// when pressed: `nodes.json` names one a press removed (a menu row
    /// that closes its menu) from here. At most [`HITS_KEPT`].
    hits: std::collections::VecDeque<(NodeId, HitIdentity)>,
    /// (replayfix) The builtin services' fields the program reads: a
    /// field it never reads is not recorded.
    reads: ServiceReads,
}

/// (report review) How many pressed nodes' identities are kept.
pub(crate) const HITS_KEPT: usize = 256;

/// (report review) A pressed node as `nodes.json` names it: its source
/// place, and its instance identity.
#[derive(Clone, Debug)]
pub(crate) struct HitIdentity {
    pub file: strand_compiler::source::FileId,
    pub span: strand_compiler::syntax::Span,
    pub ident: Option<strand_compiler::instantiate::inspect::Identity>,
}

/// The fields' names of each service `program` declares.
fn declared_of(program: &Program) -> HashMap<String, Vec<String>> {
    program
        .services
        .values()
        .map(|s| {
            let fields = s.fields.iter().map(|f| f.name.clone()).collect();
            (s.name.clone(), fields)
        })
        .collect()
}

/// One item of a declared service's `values` list: its field's index and
/// its value.
fn declared_item(d: &Data) -> Option<(usize, &Data)> {
    let Some(Data::Int(i)) = d.field("index") else {
        return None;
    };
    Some((usize::try_from(*i).ok()?, d.field("value")?))
}

impl fmt::Debug for ChangeRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangeRecorder")
            .field("ring", &self.ring)
            .field("locked", &self.locked)
            .finish_non_exhaustive()
    }
}

/// Writes a change record whose value `write` puts in the arena (it
/// returns whether it held a text back): the name interned, or at the
/// value's head ended by a 0 byte.
fn push_value(
    r: &mut Ring,
    t: u64,
    k: u8,
    name: &str,
    inline: &[u8],
    rec_flags: u8,
    write: impl FnOnce(&mut ArenaWriter<'_>) -> bool,
) {
    if r.paused() {
        return;
    }
    let slot = r.name(name);
    let mut rec = Rec::new(t, k, slot).flagged(rec_flags);
    let n = inline.len().min(6);
    rec.payload[..n].copy_from_slice(&inline[..n]);
    let mut held = false;
    // (fidelity) A value over the cap keeps its name.
    let a: ARef = r.named_value((slot == NAME_IN_ARENA).then_some(name), |w| {
        held = write(w);
    });
    rec = rec.with_value(a);
    if held {
        rec = rec.flagged(flags::REDACTED);
    }
    r.push(rec);
}

impl ChangeRecorder {
    /// A recorder of `program`'s changes, recording when `on`.
    pub(crate) fn new(on: bool, program: Arc<Program>) -> ChangeRecorder {
        ChangeRecorder {
            ring: ring_if(on, flight::CHANGES, flight::ring::CHANGES),
            locked: false,
            secret: false,
            declared: declared_of(&program),
            declared_last: HashMap::new(),
            reads: ServiceReads::of(&program),
            program,
            roots: HashMap::new(),
            next_call: 0,
            hits: std::collections::VecDeque::new(),
        }
    }

    /// (report review) A press hit `node`, now `who`: kept while
    /// recording, the oldest let go past [`HITS_KEPT`].
    pub(crate) fn hit(&mut self, node: NodeId, who: HitIdentity) {
        if self.ring.is_none() || self.locked || self.secret {
            return;
        }
        self.hits.retain(|(n, _)| *n != node);
        if self.hits.len() == HITS_KEPT {
            self.hits.pop_front();
        }
        self.hits.push_back((node, who));
    }

    /// (report review) The identity `node` had when last pressed.
    pub(crate) fn hit_identity(&self, node: NodeId) -> Option<&HitIdentity> {
        self.hits
            .iter()
            .rev()
            .find(|(n, _)| *n == node)
            .map(|(_, w)| w)
    }

    /// The program changed (a reload): its literals and types are the
    /// new one's.
    pub(crate) fn set_program(&mut self, program: Arc<Program>) {
        self.declared = declared_of(&program);
        self.reads = ServiceReads::of(&program);
        let declared = &self.declared;
        self.declared_last.retain(|k, _| {
            k.split_once('.')
                .is_some_and(|(s, _)| declared.contains_key(s))
        });
        self.program = program;
    }

    /// Recording is on.
    pub(crate) fn on(&self) -> bool {
        self.ring.is_some()
    }

    /// `system.replay` changed: see [`InputRecorder::set_on`].
    pub(crate) fn set_on(&mut self, on: bool, t: u64) {
        if on == self.on() {
            return;
        }
        self.ring = ring_if(on, flight::CHANGES, flight::ring::CHANGES);
        self.declared_last.clear();
        self.hits.clear();
        // (report review) Started anew: the recorder off ends here, and
        // the surface roots shown now, which the new ring has no record
        // of, are written again.
        if let Some(r) = &mut self.ring {
            r.push(Rec::new(t, kind::CHANGE_GAP_ENDS, 0).u8_at(0, gap::OFF));
            let mut roots: Vec<(&NodeId, &(Option<String>, NodeKind))> =
                self.roots.iter().collect();
            roots.sort_by_key(|(n, _)| node_bits(**n));
            for (node, (origin, k)) in roots {
                let slot = root_name(r, origin.as_deref());
                r.push(root_rec(t, slot, *node, *k));
            }
        }
        let g = self.gap();
        if let (Some(r), Some(g)) = (&mut self.ring, g) {
            r.pause(t, kind::CHANGE_GAP_BEGINS, g);
        }
    }

    /// The gap the ring is in: a lock, else (services-round) a password
    /// dialog or login screen.
    fn gap(&self) -> Option<u8> {
        if self.locked {
            Some(gap::LOCK)
        } else if self.secret {
            Some(gap::SECRET)
        } else {
            None
        }
    }

    /// The gap was `before`: it ends and the one now begins.
    fn regap(&mut self, t: u64, before: Option<u8>) {
        let after = self.gap();
        if before == after {
            return;
        }
        let Some(r) = &mut self.ring else {
            return;
        };
        if let Some(g) = before {
            r.resume(t, kind::CHANGE_GAP_ENDS, g);
        }
        if let Some(g) = after {
            r.pause(t, kind::CHANGE_GAP_BEGINS, g);
        }
    }

    /// (services-round) An admin password dialog or a login screen came
    /// or went: a gap, as a lock is.
    pub(crate) fn secret(&mut self, t: u64, on: bool) {
        if on == self.secret {
            return;
        }
        let before = self.gap();
        self.secret = on;
        self.regap(t, before);
    }

    /// The ring (tests).
    #[cfg(test)]
    pub(crate) fn ring(&self) -> Option<&Ring> {
        self.ring.as_ref()
    }

    /// The dump, if recording.
    pub(crate) fn dump(&self) -> Option<Vec<u8>> {
        self.ring.as_ref().map(|r| r.dump(flight::now_ns()))
    }

    /// A lock asked for or shown (`true`), or over: a gap.
    pub(crate) fn lock(&mut self, t: u64, locked: bool) {
        if locked == self.locked {
            return;
        }
        let before = self.gap();
        self.locked = locked;
        self.regap(t, before);
    }

    /// Whether a config text is kept: a literal of the program.
    fn literal(&self, s: &str) -> bool {
        self.program.is_text_literal(s)
    }

    /// A service's patch, from `Services::tap`.
    pub(crate) fn service(&mut self, t: u64, ev: &TapEvent<'_>) {
        if strand_services::secret(ev.service) {
            return;
        }
        let clip = ev.service == "clipboard";
        let Some(r) = &mut self.ring else {
            return;
        };
        if r.paused() {
            return;
        }
        if let Some(fields) = self.declared.get(ev.service) {
            return declared_change(r, &mut self.declared_last, t, ev, fields);
        }
        let mut name = String::with_capacity(ev.service.len() + 24);
        name.push_str(ev.service);
        name.push('.');
        let keep = |s: &str| !clip || s.is_empty();
        match &ev.change {
            Change::Field {
                field,
                old,
                new,
                initial,
                local,
            } => {
                if !self.reads.reads(ev.service, field) {
                    return;
                }
                name.push_str(field);
                let f = change_flags(*initial, *local);
                // (replayfix) What changed in it, when that is shorter
                // than the value before and after it whole.
                let edits = field_edits(old, new, &keep);
                push_value(r, t, kind::FIELD, &name, &[], f, |w| {
                    let mut held = false;
                    let mut k = |s: &str| {
                        let kept = keep(s);
                        held |= !kept;
                        kept
                    };
                    match &edits {
                        Some(e) => {
                            let _ = write_edits(w, e, &mut k);
                        }
                        None => {
                            let _ = w.write_char('[');
                            let _ = old.write_json(w, &mut k);
                            let _ = w.write_char(',');
                            let _ = new.write_json(w, &mut k);
                            let _ = w.write_char(']');
                        }
                    }
                    held
                });
            }
            Change::Keyed {
                field,
                diffs,
                removed,
                initial,
                local,
            } => {
                if !self.reads.reads(ev.service, field) {
                    return;
                }
                // (replayfix) Each item a diff replaced as what changed
                // in it, when shorter; an item replaced by its equal is
                // no change, and a patch of nothing else is not recorded.
                let Some(plan) = keyed_plan(diffs, removed, &keep) else {
                    return;
                };
                name.push_str(field);
                let f = change_flags(*initial, *local);
                push_value(r, t, kind::KEYED, &name, &[], f, |w| {
                    let mut held = false;
                    let mut k = |s: &str| {
                        let kept = keep(s);
                        held |= !kept;
                        kept
                    };
                    let _ = match &plan {
                        KeyedPlan::Whole => write_diffs(w, diffs, removed, &mut k),
                        KeyedPlan::Parts(parts) => write_parts(w, parts, &mut k),
                    };
                    held
                });
            }
            Change::Event { event, args } => {
                name.push_str(event);
                push_value(r, t, kind::EVENT, &name, &[], 0, |w| {
                    let mut held = false;
                    let mut k = |s: &str| {
                        let kept = keep(s);
                        held |= !kept;
                        kept
                    };
                    let _ = w.write_char('[');
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            let _ = w.write_char(',');
                        }
                        let _ = a.write_json(w, &mut k);
                    }
                    let _ = w.write_char(']');
                    held
                });
            }
        }
    }

    /// A cell write, from `Instance::set_cell_tap`.
    pub(crate) fn cell(&mut self, t: u64, path: &str, old: &Value, new: &Value) {
        let program = self.program.clone();
        let Some(r) = &mut self.ring else {
            return;
        };
        // A token overlay (`$accent`) is set from the command line, not
        // typed into the shell: its text is kept.
        let overlay = path.starts_with('$');
        push_value(r, t, kind::CELL, path, &[], 0, |w| {
            let mut held = false;
            let mut k = |s: &str| {
                let kept = overlay || program.is_text_literal(s);
                held |= !kept;
                kept
            };
            let _ = w.write_char('[');
            let _ = strand_compiler::vm::json::to_json(&program.types, old, w, &mut k);
            let _ = w.write_char(',');
            let _ = strand_compiler::vm::json::to_json(&program.types, new, w, &mut k);
            let _ = w.write_char(']');
            held
        });
    }

    /// (report review) A cell written by `strand set`, `toggle` or
    /// `reset` (`verb`, [`flight::request`]): what a replay repeats at
    /// its time. `new` is the cell's value after it, held back as a
    /// cell's is.
    pub(crate) fn request(&mut self, t: u64, verb: u8, path: &str, new: &Value) {
        let program = self.program.clone();
        let Some(r) = &mut self.ring else {
            return;
        };
        let overlay = path.starts_with('$');
        push_value(r, t, kind::REQUEST, path, &[verb], 0, |w| {
            let mut held = false;
            let mut k = |s: &str| {
                let kept = overlay || program.is_text_literal(s);
                held |= !kept;
                kept
            };
            let _ = strand_compiler::vm::json::to_json(&program.types, new, w, &mut k);
            held
        });
    }

    /// Whether any text in `args` is not a literal of the program.
    fn tainted(&self, args: &[Value]) -> bool {
        let mut tainted = false;
        for a in args {
            let _ = strand_compiler::vm::json::to_json(
                &self.program.types,
                a,
                &mut Sink,
                &mut |s: &str| {
                    tainted |= !self.literal(s);
                    true
                },
            );
        }
        tainted
    }

    /// A call `service.method(args)` began (an async one, a `fetch`):
    /// what its answer is recorded with. `None` while not recording or
    /// for a secret service (`auth`, `polkit`, `greetd`).
    pub(crate) fn call(&mut self, service: &str, method: &str, args: &[Value]) -> Option<Call> {
        if strand_services::secret(service) || self.ring.as_ref().is_none_or(Ring::paused) {
            return None;
        }
        let id = self.next_call;
        self.next_call = self.next_call.wrapping_add(1);
        Some(Call {
            id,
            name: format!("{service}.{method}"),
            tainted: self.tainted(args) || service == "clipboard",
        })
    }

    /// `call`'s answer arrived at `t`.
    pub(crate) fn answered(&mut self, t: u64, call: &Call, answer: &Result<Value, String>) {
        let program = self.program.clone();
        let Some(r) = &mut self.ring else {
            return;
        };
        let inline = call.id.to_le_bytes();
        push_value(r, t, kind::FETCH, &call.name, &inline, 0, |w| {
            let mut held = false;
            let mut k = |_: &str| {
                held |= call.tainted;
                !call.tainted
            };
            match answer {
                Ok(v) => {
                    let _ = w.write_str("{\"ok\":");
                    let _ = strand_compiler::vm::json::to_json(&program.types, v, w, &mut k);
                }
                Err(e) => {
                    let _ = w.write_str("{\"err\":");
                    if call.tainted {
                        held = true;
                        let _ = flight::json_held(w, e);
                    } else {
                        let _ = flight::json_text(w, e);
                    }
                }
            }
            let _ = w.write_char('}');
            held
        });
    }

    /// The config wrote `value` to an `rw` field `service.path`.
    pub(crate) fn wrote(&mut self, t: u64, service: &str, path: &[PathSeg], value: &Value) {
        if strand_services::secret(service) {
            return;
        }
        let program = self.program.clone();
        let Some(r) = &mut self.ring else {
            return;
        };
        let mut name = String::from(service);
        for seg in path {
            let _ = write!(name, "{seg}");
        }
        push_value(r, t, kind::WRITE, &name, &[], 0, |w| {
            let mut held = false;
            let mut k = |s: &str| {
                let kept = service != "clipboard" && program.is_text_literal(s);
                held |= !kept;
                kept
            };
            let _ = strand_compiler::vm::json::to_json(&program.types, value, w, &mut k);
            held
        });
    }

    /// A declared action's command started (`started`) with `argc`
    /// arguments, or ended with `exit` (0: success).
    pub(crate) fn action(&mut self, t: u64, action: &str, argc: usize, started: bool, exit: i32) {
        let Some(r) = &mut self.ring else {
            return;
        };
        if r.paused() {
            return;
        }
        let slot = r.name(action);
        let mut rec = Rec::new(t, kind::ACTION, slot)
            .i32_at(0, exit)
            .u8_at(4, u8::try_from(argc).unwrap_or(u8::MAX))
            .u8_at(5, u8::from(started));
        if slot == NAME_IN_ARENA {
            let a = r.value(|w| {
                w.put(action.as_bytes());
                w.put(&[0]);
            });
            rec = rec.with_value(a);
        }
        r.push(rec);
    }

    /// A reload committed: its counts (files, cells kept, cells reset)
    /// and each file's hash.
    pub(crate) fn reload(&mut self, t: u64, counts: [u16; 3], files: &[(String, String)]) {
        let Some(r) = &mut self.ring else {
            return;
        };
        let mut inline = [0u8; 6];
        for (i, c) in counts.iter().enumerate() {
            inline[i * 2..i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        push_value(r, t, kind::RELOAD, "reload", &inline, 0, |w| {
            let _ = w.write_char('{');
            for (i, (f, h)) in files.iter().enumerate() {
                if i > 0 {
                    let _ = w.write_char(',');
                }
                let _ = flight::json_text(w, f);
                let _ = w.write_char(':');
                let _ = flight::json_text(w, h);
            }
            let _ = w.write_char('}');
            false
        });
    }

    /// A surface root `node` of `kind` was created; `origin` is
    /// `file.strand:LINE:COL`, `None` for a surface with no place in the
    /// config (Strand's own error panel), which is recorded with no name.
    /// A `lock` root is never recorded.
    pub(crate) fn root(&mut self, t: u64, node: NodeId, kind: NodeKind, origin: Option<&str>) {
        if kind == NodeKind::Lock {
            return;
        }
        self.roots.insert(node, (origin.map(str::to_string), kind));
        let Some(r) = &mut self.ring else {
            return;
        };
        if r.paused() {
            return;
        }
        let slot = root_name(r, origin);
        r.push(root_rec(t, slot, node, kind));
    }

    /// `node` was removed: a root gone if it was one.
    pub(crate) fn removed(&mut self, t: u64, node: NodeId) {
        let Some((origin, _)) = self.roots.remove(&node) else {
            return;
        };
        let Some(r) = &mut self.ring else {
            return;
        };
        if r.paused() {
            return;
        }
        let slot = root_name(r, origin.as_deref());
        r.push(Rec::new(t, kind::ROOT_GONE, slot).u64_at(0, node_bits(node)));
    }

    /// Whether `node` is a surface root recorded.
    pub(crate) fn is_root(&self, node: NodeId) -> bool {
        self.roots.contains_key(&node)
    }

    /// A declared service's fields by their declared names, from its
    /// snapshot (`None` for a builtin service).
    pub(crate) fn declared_fields(
        &self,
        service: &str,
        snapshot: &[(&'static str, Data)],
    ) -> Option<Vec<(String, Data)>> {
        let names = self.declared.get(service)?;
        let mut out: Vec<(String, Data)> = names.iter().map(|n| (n.clone(), Data::Null)).collect();
        if let Some((_, Data::List(items))) = snapshot.iter().find(|(f, _)| *f == "values") {
            for (i, v) in items.iter().filter_map(declared_item) {
                if let Some(slot) = out.get_mut(i) {
                    slot.1 = v.clone();
                }
            }
        }
        Some(out)
    }

    /// A service's value as the bundle keeps it, under the same rules.
    pub(crate) fn service_json(service: &str, d: &Data) -> String {
        let clip = service == "clipboard";
        let mut s = String::new();
        let _ = d.write_json(&mut s, &mut |t: &str| !clip || t.is_empty());
        s
    }

    /// The cell at `path`'s value as the bundle keeps it.
    pub(crate) fn cell_json(&self, path: &str, v: &Value) -> String {
        let overlay = path.starts_with('$');
        let mut s = String::new();
        let _ = strand_compiler::vm::json::to_json(&self.program.types, v, &mut s, &mut |t| {
            overlay || self.literal(t)
        });
        s
    }
}

/// A service change's flags: a boot value's, and (services) a local
/// write's, which a replay's own handler makes again.
fn change_flags(initial: bool, local: bool) -> u8 {
    let mut f = 0;
    if initial {
        f |= flags::BOOT;
    }
    if local {
        f |= flags::LOCAL;
    }
    f
}

/// A declared service's patch: each field it set, as a `FIELD` record of
/// its declared name (`mood.level`), its old value the last one recorded
/// (`null` before any). Its values are kept as reported.
fn declared_change(
    r: &mut Ring,
    last: &mut HashMap<String, String>,
    t: u64,
    ev: &TapEvent<'_>,
    fields: &[String],
) {
    let Change::Keyed {
        field: "values",
        diffs,
        initial,
        local,
        ..
    } = &ev.change
    else {
        return;
    };
    let mut set: Vec<(usize, &Data)> = Vec::new();
    for d in diffs.iter() {
        match d {
            strand_core::VecDiff::Reset { items } => {
                set.extend(items.iter().filter_map(|(_, v)| declared_item(v)));
            }
            strand_core::VecDiff::Insert { value, .. }
            | strand_core::VecDiff::Update { value, .. } => {
                set.extend(declared_item(value));
            }
            _ => {}
        }
    }
    let f = change_flags(*initial, *local);
    for (i, v) in set {
        let Some(field) = fields.get(i) else {
            continue;
        };
        let name = format!("{}.{field}", ev.service);
        let mut new = String::new();
        let _ = v.write_json(&mut new, &mut |_| true);
        let old = last.get(&name).map_or("null", String::as_str);
        if old == new && !*initial {
            continue;
        }
        let both = format!("[{old},{new}]");
        push_value(r, t, kind::FIELD, &name, &[], f, |w| {
            let _ = w.write_str(&both);
            false
        });
        last.insert(name, new);
    }
}

/// (replayfix) What a program reads of each builtin service, from its
/// chunks' read sets (`Program::reads`): a field read by name
/// (`workspaces.all`, `windows.focused?.title`), or the service whole
/// when a method is called on it (`workspaces.slots(screen)` reads
/// whatever the method reads, so every field of it counts as read).
#[derive(Debug, Default)]
pub(crate) struct ServiceReads(HashMap<String, Option<std::collections::HashSet<String>>>);

impl ServiceReads {
    pub(crate) fn of(program: &Program) -> ServiceReads {
        let mut m: HashMap<String, Option<std::collections::HashSet<String>>> = HashMap::new();
        for r in &program.reads {
            for (svc, field) in &r.services {
                match field {
                    None => {
                        m.insert(svc.clone(), None);
                    }
                    Some(f) => {
                        if let Some(set) = m
                            .entry(svc.clone())
                            .or_insert_with(|| Some(Default::default()))
                        {
                            set.insert(f.clone());
                        }
                    }
                }
            }
        }
        // `system` is read past the program's chunks too: the theme's
        // light or dark mode, reduced motion and the contrast are the
        // renderer's and the palette's, so it is always recorded.
        m.insert("system".into(), None);
        ServiceReads(m)
    }

    /// Whether the program can read `service.field`.
    pub(crate) fn reads(&self, service: &str, field: &str) -> bool {
        match self.0.get(service) {
            Some(None) => true,
            Some(Some(set)) => set.contains(field),
            None => false,
        }
    }
}

/// (replayfix) A place in a value: a record's field or a list's index.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Seg<'a> {
    Field(&'a str),
    Index(usize),
}

/// (replayfix) One edit of a value: at `path` (empty: the value whole),
/// `old` became `new`.
#[derive(Clone, Debug, PartialEq)]
struct Edit<'a> {
    path: Vec<Seg<'a>>,
    old: &'a Data,
    new: &'a Data,
}

/// Counts the bytes written to it.
struct Count(usize);

impl fmt::Write for Count {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.len();
        Ok(())
    }
}

/// The bytes of `d` as JSON, its texts kept or held back by `keep`.
fn json_len(d: &Data, keep: &dyn Fn(&str) -> bool) -> usize {
    let mut c = Count(0);
    let _ = d.write_json(&mut c, &mut |s| keep(s));
    c.0
}

/// A field name an edit's path can hold: not empty, no `.` (the path's
/// separator), not starting with a digit (an index).
fn plain_field(name: &str) -> bool {
    !name.is_empty() && !name.contains('.') && !name.starts_with(|c: char| c.is_ascii_digit())
}

/// The bytes of one edit as [`write_edit`] writes it.
fn edit_len(e: &Edit<'_>, keep: &dyn Fn(&str) -> bool) -> usize {
    let mut c = Count(0);
    let _ = write_path(&mut c, &e.path);
    // `[` path `,` old `,` new `]`
    c.0 + 3 + json_len(e.old, keep) + json_len(e.new, keep)
}

/// The edits that take `old` to `new`, pushed to `out`: a record of the
/// same type and fields field by field, a list of the same length item
/// by item when that is shorter than the list whole, anything else
/// whole.
fn diff<'a>(
    old: &'a Data,
    new: &'a Data,
    path: &mut Vec<Seg<'a>>,
    out: &mut Vec<Edit<'a>>,
    keep: &dyn Fn(&str) -> bool,
) {
    if old == new {
        return;
    }
    match (old, new) {
        (Data::Record { ty: a, fields: fa }, Data::Record { ty: b, fields: fb })
            if a == b
                && fa.len() == fb.len()
                && fa
                    .iter()
                    .zip(fb)
                    .all(|((x, _), (y, _))| x == y && plain_field(x)) =>
        {
            for ((name, o), (_, n)) in fa.iter().zip(fb) {
                path.push(Seg::Field(name));
                diff(o, n, path, out, keep);
                path.pop();
            }
        }
        (Data::List(a), Data::List(b)) if a.len() == b.len() => {
            let mut inner = Vec::new();
            for (i, (o, n)) in a.iter().zip(b).enumerate() {
                path.push(Seg::Index(i));
                diff(o, n, path, &mut inner, keep);
                path.pop();
            }
            let whole = Edit {
                path: path.clone(),
                old,
                new,
            };
            let parts: usize = inner.iter().map(|e| edit_len(e, keep) + 1).sum();
            if parts < edit_len(&whole, keep) + 1 {
                out.extend(inner);
            } else {
                out.push(whole);
            }
        }
        _ => out.push(Edit {
            path: path.clone(),
            old,
            new,
        }),
    }
}

/// (replayfix) A field's change as its edits (`{"e": [[path, old,
/// new], …]}`), when they are shorter than `[old, new]`; `None` when
/// not.
fn field_edits<'a>(
    old: &'a Data,
    new: &'a Data,
    keep: &dyn Fn(&str) -> bool,
) -> Option<Vec<Edit<'a>>> {
    let mut out = Vec::new();
    diff(old, new, &mut Vec::new(), &mut out, keep);
    if out.is_empty() || out.iter().any(|e| e.path.is_empty()) {
        return None;
    }
    let edits: usize = 7 + out.iter().map(|e| edit_len(e, keep) + 1).sum::<usize>();
    let whole = 3 + json_len(old, keep) + json_len(new, keep);
    (edits < whole).then_some(out)
}

/// Writes an edit's path: its segments joined by `.`, an index as its
/// number (`windows.1.title`), as a JSON string.
fn write_path(w: &mut dyn fmt::Write, path: &[Seg<'_>]) -> fmt::Result {
    w.write_char('"')?;
    for (i, s) in path.iter().enumerate() {
        if i > 0 {
            w.write_char('.')?;
        }
        match s {
            // A plain field (`plain_field`) needs no escaping but for
            // the JSON string's own characters.
            Seg::Field(f) => {
                let mut q = String::new();
                flight::json_text(&mut q, f)?;
                w.write_str(&q[1..q.len() - 1])?;
            }
            Seg::Index(n) => write!(w, "{n}")?,
        }
    }
    w.write_char('"')
}

/// Writes `edits` as a list of `[path, old, new]`.
fn write_edit_list(
    w: &mut dyn fmt::Write,
    edits: &[Edit<'_>],
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    w.write_char('[')?;
    for (i, e) in edits.iter().enumerate() {
        if i > 0 {
            w.write_char(',')?;
        }
        w.write_char('[')?;
        write_path(w, &e.path)?;
        w.write_char(',')?;
        e.old.write_json(w, keep)?;
        w.write_char(',')?;
        e.new.write_json(w, keep)?;
        w.write_char(']')?;
    }
    w.write_char(']')
}

/// Writes a field's edits: `{"e": [[path, old, new], …]}`.
fn write_edits(
    w: &mut dyn fmt::Write,
    edits: &[Edit<'_>],
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    w.write_str("{\"e\":")?;
    write_edit_list(w, edits, keep)?;
    w.write_char('}')
}

/// (replayfix) How a keyed patch is written: as it came, or diff by
/// diff with the items it replaced patched.
enum KeyedPlan<'a> {
    Whole,
    Parts(Vec<Part<'a>>),
}

/// One diff of a keyed patch as written.
enum Part<'a> {
    /// The diff as it came, with the item it removed or replaced (if
    /// any), which goes into `r`.
    Diff(&'a strand_core::VecDiff<Data, Data>, Option<&'a Data>),
    /// An item that changed in place: `{"patch": [index, key, edits]}`.
    Patch(usize, &'a Data, Vec<Edit<'a>>),
}

/// The plan of a keyed patch: `None` when nothing in it changes (every
/// diff an item replaced by its equal). A patch with a reset, or whose
/// removed items do not pair with its diffs, is written whole.
fn keyed_plan<'a>(
    diffs: &'a [strand_core::VecDiff<Data, Data>],
    removed: &'a [Data],
    keep: &dyn Fn(&str) -> bool,
) -> Option<KeyedPlan<'a>> {
    use strand_core::VecDiff;
    if diffs.is_empty() || diffs.iter().any(|d| matches!(d, VecDiff::Reset { .. })) {
        return Some(KeyedPlan::Whole);
    }
    let mut parts = Vec::with_capacity(diffs.len());
    let mut gone = removed.iter();
    for d in diffs {
        match d {
            VecDiff::Update { index, key, value } => {
                let Some(old) = gone.next() else {
                    return Some(KeyedPlan::Whole);
                };
                if old == value {
                    continue;
                }
                let mut edits = Vec::new();
                diff(old, value, &mut Vec::new(), &mut edits, keep);
                let whole_item = edits.iter().any(|e| e.path.is_empty());
                let patch = 16
                    + json_len(key, keep)
                    + edits.iter().map(|e| edit_len(e, keep) + 1).sum::<usize>();
                let update = 16 + json_len(key, keep) + json_len(value, keep) + json_len(old, keep);
                if !whole_item && patch < update {
                    parts.push(Part::Patch(*index, key, edits));
                } else {
                    parts.push(Part::Diff(d, Some(old)));
                }
            }
            VecDiff::Remove { .. } => {
                let Some(old) = gone.next() else {
                    return Some(KeyedPlan::Whole);
                };
                parts.push(Part::Diff(d, Some(old)));
            }
            _ => parts.push(Part::Diff(d, None)),
        }
    }
    if gone.next().is_some() {
        return Some(KeyedPlan::Whole);
    }
    (!parts.is_empty()).then_some(KeyedPlan::Parts(parts))
}

/// Writes a planned keyed patch as `{"d": diffs, "r": removed}`, a
/// patched item as `{"patch": [index, key, [[path, old, new], …]]}`
/// with nothing of it in `r`.
fn write_parts(
    w: &mut dyn fmt::Write,
    parts: &[Part<'_>],
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    w.write_str("{\"d\":[")?;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            w.write_char(',')?;
        }
        match p {
            Part::Diff(d, _) => write_diff(w, d, keep)?,
            Part::Patch(index, key, edits) => {
                write!(w, "{{\"patch\":[{index},")?;
                key.write_json(w, keep)?;
                w.write_char(',')?;
                write_edit_list(w, edits, keep)?;
                w.write_str("]}")?;
            }
        }
    }
    w.write_str("],\"r\":[")?;
    let mut first = true;
    for p in parts {
        if let Part::Diff(_, Some(old)) = p {
            if !first {
                w.write_char(',')?;
            }
            first = false;
            old.write_json(w, keep)?;
        }
    }
    w.write_str("]}")
}

/// A keyed patch as `{"d": diffs, "r": removed}`: each diff one of
/// `{"reset": [[key, value], …]}`, `{"insert": [index, key, value]}`,
/// `{"update": [index, key, value]}`, `{"remove": [index, key]}` and
/// `{"move": [from, to, key]}`.
fn write_diffs(
    w: &mut dyn fmt::Write,
    diffs: &[strand_core::VecDiff<Data, Data>],
    removed: &[Data],
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    w.write_str("{\"d\":[")?;
    for (i, d) in diffs.iter().enumerate() {
        if i > 0 {
            w.write_char(',')?;
        }
        write_diff(w, d, keep)?;
    }
    w.write_str("],\"r\":[")?;
    for (i, d) in removed.iter().enumerate() {
        if i > 0 {
            w.write_char(',')?;
        }
        d.write_json(w, keep)?;
    }
    w.write_str("]}")
}

/// One diff of a keyed patch (see [`write_diffs`]).
fn write_diff(
    w: &mut dyn fmt::Write,
    d: &strand_core::VecDiff<Data, Data>,
    keep: &mut dyn FnMut(&str) -> bool,
) -> fmt::Result {
    use strand_core::VecDiff;
    {
        match d {
            VecDiff::Reset { items } => {
                w.write_str("{\"reset\":[")?;
                for (j, (k, v)) in items.iter().enumerate() {
                    if j > 0 {
                        w.write_char(',')?;
                    }
                    w.write_char('[')?;
                    k.write_json(w, keep)?;
                    w.write_char(',')?;
                    v.write_json(w, keep)?;
                    w.write_char(']')?;
                }
                w.write_str("]}")?;
            }
            VecDiff::Insert { index, key, value } | VecDiff::Update { index, key, value } => {
                let tag = if matches!(d, VecDiff::Insert { .. }) {
                    "insert"
                } else {
                    "update"
                };
                write!(w, "{{\"{tag}\":[{index},")?;
                key.write_json(w, keep)?;
                w.write_char(',')?;
                value.write_json(w, keep)?;
                w.write_str("]}")?;
            }
            VecDiff::Remove { index, key } => {
                write!(w, "{{\"remove\":[{index},")?;
                key.write_json(w, keep)?;
                w.write_str("]}")?;
            }
            VecDiff::Move { from, to, key } => {
                write!(w, "{{\"move\":[{from},{to},")?;
                key.write_json(w, keep)?;
                w.write_str("]}")?;
            }
        }
    }
    Ok(())
}

/// The composite's calls and writes, told to the recorder.
struct RecorderTap(std::rc::Rc<std::cell::RefCell<ChangeRecorder>>);

impl crate::services::CallTap for RecorderTap {
    fn fetch(
        &self,
        service: &str,
        method: &str,
        args: &[Value],
    ) -> Option<crate::services::FetchDone> {
        let call = self.0.try_borrow_mut().ok()?.call(service, method, args)?;
        let rec = self.0.clone();
        Some(Box::new(move |answer| {
            let answer = match answer {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.to_string()),
            };
            if let Ok(mut r) = rec.try_borrow_mut() {
                r.answered(flight::now_ns(), &call, &answer);
            }
        }))
    }

    fn wrote(&self, service: &str, path: &[PathSeg], value: &Value) {
        if let Ok(mut r) = self.0.try_borrow_mut() {
            r.wrote(flight::now_ns(), service, path, value);
        }
    }
}

/// (report) The logic thread's recorder for `program`, its taps
/// installed on `inst` and the real services.
pub(super) fn install(
    inst: &strand_compiler::instantiate::Instance,
    real: Option<&crate::services::Real>,
    program: Arc<Program>,
) -> std::rc::Rc<std::cell::RefCell<ChangeRecorder>> {
    let rec = std::rc::Rc::new(std::cell::RefCell::new(ChangeRecorder::new(true, program)));
    taps(inst, real, &rec, true);
    rec
}

/// Installs the recorder's taps (`on`), or takes them away: while off,
/// no service reads an old value for it and no cell's last value is
/// kept.
fn taps(
    inst: &strand_compiler::instantiate::Instance,
    real: Option<&crate::services::Real>,
    rec: &std::rc::Rc<std::cell::RefCell<ChangeRecorder>>,
    on: bool,
) {
    if let Some(r) = real {
        if on {
            let c = rec.clone();
            r.services.tap(Some(Box::new(move |ev: &TapEvent<'_>| {
                if let Ok(mut x) = c.try_borrow_mut() {
                    x.service(flight::now_ns(), ev);
                }
            })));
            r.host
                .set_tap(Some(std::rc::Rc::new(RecorderTap(rec.clone()))));
        } else {
            r.services.tap(None);
            r.host.set_tap(None);
        }
    }
    if on {
        let c = rec.clone();
        inst.set_cell_tap(Some(Box::new(
            move |w: strand_compiler::instantiate::CellWrite<'_>| {
                if let Ok(mut x) = c.try_borrow_mut() {
                    x.cell(flight::now_ns(), w.path, w.old, w.new);
                }
            },
        )));
    } else {
        inst.set_cell_tap(None);
    }
}

impl super::shell::Shell {
    /// (report review) A press hit `node` (main's `ToLogic::Hit`, sent
    /// before the press's events): its identity is kept for
    /// `nodes.json`, in case the press removes it.
    pub(super) fn record_hit(&self, node: NodeId) {
        let Ok(mut r) = self.recorder.try_borrow_mut() else {
            return;
        };
        if !r.on() {
            return;
        }
        if let Some((file, _, span)) = self.inst.origin(node) {
            let ident = self.inst.identity(node);
            r.hit(node, HitIdentity { file, span, ident });
        }
    }

    /// (report review) `path` was written by a request (`verb`): its
    /// value now is recorded for the replay. A service's field is not:
    /// its patch is recorded as the service's.
    /// A token overlay's (`$accent`) is the text it was set to, `token`.
    pub(super) fn record_request(&self, verb: u8, path: &str, token: Option<&str>) {
        let value = match token {
            Some(text) => Value::text(text),
            // A reset names a cell as the inspector does
            // (`TopBar[DP-1].expanded`), not always an export.
            None => match self.inst.get(path) {
                Ok(v) => v,
                Err(_) => match self
                    .inst
                    .cells_snapshot()
                    .into_iter()
                    .find(|(p, _)| p == path)
                {
                    Some((_, v)) => v,
                    None => return,
                },
            },
        };
        if let Ok(mut r) = self.recorder.try_borrow_mut() {
            r.request(flight::now_ns(), verb, path, &value);
        }
    }

    /// (report) A surface root a diff creates is recorded with its
    /// origin, one it removes as gone; a `lock` root created, or a
    /// surface opened or closed, checks whether a lock is now asked for
    /// (the ring pauses) or over.
    pub(super) fn record_diff(&mut self, diff: &strand_scene::SceneDiff) {
        use strand_scene::SceneOp;
        let t = flight::now_ns();
        let mut lock = false;
        for op in &diff.ops {
            match op {
                SceneOp::Create { id, kind, .. } if kind.is_surface() => {
                    if *kind == NodeKind::Lock {
                        lock = true;
                        continue;
                    }
                    let origin = self.origin_of(*id);
                    self.recorder
                        .borrow_mut()
                        .root(t, *id, *kind, origin.as_deref());
                }
                SceneOp::Remove { id, .. } => {
                    let mut r = self.recorder.borrow_mut();
                    if r.is_root(*id) {
                        r.removed(t, *id);
                    } else if r.locked {
                        lock = true;
                    }
                }
                // A surface opened or closed: a `lock` kept mounted
                // (`open: <-> locked`) is asked for this way.
                SceneOp::SetProp {
                    prop: strand_scene::Prop::Open,
                    ..
                } => lock = true,
                _ => {}
            }
        }
        if lock {
            self.record_lock();
        }
    }

    /// `node`'s origin as `file.strand:LINE:COL`, the file under the
    /// config directory as `nodes.json` names it; `None` for a node with
    /// no place in the config's source (Strand's own error panel).
    /// (replayfix) Never a placeholder: a replay reads the name as a
    /// selector.
    fn origin_of(&self, node: NodeId) -> Option<String> {
        let (f, _, sp) = self.inst.origin(node)?;
        self.origin_text(f, sp, self.config_dir.as_deref())
    }

    /// (services-round) An admin password dialog or a login screen is up
    /// (`Real::secret_shown`).
    pub(super) fn secret_shown(&self) -> bool {
        self.real
            .as_ref()
            .is_some_and(|r| r.secret_shown(self.inst.runtime()))
    }

    /// (services-round) Both rings pause while [`Self::secret_shown`],
    /// as they do for a lock (main's through `ToMain::Secret`).
    pub(super) fn record_secret(&mut self) {
        let shown = self.secret_shown();
        if shown == self.secret_sent {
            return;
        }
        self.secret_sent = shown;
        self.recorder.borrow_mut().secret(flight::now_ns(), shown);
        if let Some(tx) = &self.to_main {
            let _ = tx.send(super::ToMain::Secret(shown));
        }
    }

    /// (report) The changes ring pauses while a lock is asked for or
    /// shown, and appends again after.
    pub(super) fn record_lock(&mut self) {
        let locked = self.inst.lock_shown();
        self.recorder.borrow_mut().lock(flight::now_ns(), locked);
    }

    /// (report) `system.replay` as the host reads it now: a change turns
    /// both rings off (dropped) or on (empty), main's through
    /// `ToMain::Recording`.
    pub(super) fn record_replay(&mut self) {
        let on = {
            let rt = self.inst.runtime();
            let host = super::logic::live_host(&self.host, &self.real);
            rt.untrack(|rt| host.read_builtin(rt, "system", "replay"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        };
        if on == self.replay_on {
            return;
        }
        self.replay_on = on;
        log::info!("glitch-report recording {}", if on { "on" } else { "off" });
        self.recorder.borrow_mut().set_on(on, flight::now_ns());
        taps(&self.inst, self.real.as_ref(), &self.recorder, on);
        if let Some(tx) = &self.to_main {
            let _ = tx.send(super::ToMain::Recording(on));
        }
    }

    /// (report) A reload committed: its counts and its files' hashes, and
    /// the new program's literals from now on.
    pub(super) fn record_reload(&mut self, report: &strand_compiler::reconcile::Report) {
        let files: Vec<(String, String)> = self
            .build
            .sources
            .iter()
            .map(|(_, f)| {
                (
                    f.name.clone(),
                    blake3::hash(f.text.as_bytes()).to_hex().to_string(),
                )
            })
            .collect();
        let c = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        let counts = [c(files.len()), c(report.kept.len()), c(report.reset.len())];
        let mut r = self.recorder.borrow_mut();
        r.set_program(self.build.program.clone());
        r.reload(flight::now_ns(), counts, &files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_core::flight::Entry;
    use strand_scene::{AxisDelta, LogicalPoint, Modifiers};

    const S: SurfaceId = SurfaceId(7);

    fn mapped() -> InputRecorder {
        let mut r = InputRecorder::new(true);
        r.mapped(
            1,
            S,
            NodeId::new(3, 1),
            Some(("out-1", Some("DP-1"))),
            NodeKind::Bar,
        );
        r
    }

    fn entries(r: &Ring) -> Vec<Entry> {
        flight::read(&r.dump(0)).unwrap().entries()
    }

    fn kinds(r: &Ring) -> Vec<u8> {
        entries(r).iter().map(|e| e.rec.kind).collect()
    }

    fn p(x: f32, y: f32) -> LogicalPoint {
        LogicalPoint { x, y }
    }

    fn key(name: &str, text: &str, pressed: bool, m: Modifiers) -> InputEvent {
        InputEvent::Key {
            surface: S,
            key: KeyInput {
                name: name.into(),
                text: text.into(),
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
                repeat: false,
                modifiers: m,
                time: 0,
                latin: None,
            },
        }
    }

    /// Every bit of every dump, its names and its arena: what a test
    /// searches for a secret.
    fn bytes(r: &Ring) -> Vec<u8> {
        r.dump(0)
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn each_input_event_maps_to_its_record() {
        let mut r = mapped();
        let events = [
            InputEvent::PointerEnter {
                surface: S,
                position: p(1.0, 2.0),
            },
            InputEvent::PointerMotion {
                surface: S,
                position: p(3.0, 4.0),
                time: 0,
            },
            InputEvent::PointerButton {
                surface: S,
                position: p(3.0, 4.0),
                button: 0x111,
                state: ButtonState::Released,
                time: 0,
            },
            InputEvent::PointerAxis {
                surface: S,
                position: p(3.0, 4.0),
                horizontal: AxisDelta::default(),
                vertical: AxisDelta {
                    pixels: 15.0,
                    value120: 120,
                    stop: false,
                },
                source: Some(AxisSource::Wheel),
                time: 0,
            },
            InputEvent::KeyboardEnter { surface: S },
            key("Escape", "", true, Modifiers::default()),
            InputEvent::KeyboardLeave { surface: S },
            InputEvent::ClickAway { surface: S },
            InputEvent::DragEnter {
                surface: S,
                at: p(5.0, 6.0),
                kinds: vec![DropKind::Files, DropKind::Text],
            },
            InputEvent::DragLeave { surface: S },
            InputEvent::PointerLeave { surface: S },
        ];
        for (i, e) in events.iter().enumerate() {
            r.input(100 * (i as u64 + 1) * 1_000_000, e, None, None);
        }
        let ring = r.ring().unwrap();
        assert_eq!(
            kinds(ring),
            [
                kind::MAPPED,
                kind::ENTER,
                kind::MOTION,
                kind::BUTTON,
                kind::AXIS,
                kind::KEYBOARD_ENTER,
                kind::KEY,
                kind::KEYBOARD_LEAVE,
                kind::CLICK_AWAY,
                kind::DRAG,
                kind::DRAG,
                kind::LEAVE,
            ]
        );
        let e = entries(ring);
        assert_eq!((e[1].rec.f32(0), e[1].rec.f32(4)), (1.0, 2.0));
        assert_eq!(e[3].rec.u16(8), 1, "BTN_RIGHT less BTN_MOUSE");
        assert_eq!(e[3].rec.u8(10), 0, "released");
        assert_eq!(
            (e[4].rec.f32(4), e[4].rec.u8(8), e[4].rec.i16(10)),
            (15.0, 1, 120)
        );
        let ring = r.ring().unwrap();
        assert_eq!(ring.name_at(e[6].rec.u16(0)), Some("Escape"));
        assert_eq!(e[6].rec.u8(4), 1, "pressed");
        assert_eq!(e[9].rec.u16(8), 0b101, "files and text offered");
    }

    #[test]
    fn a_press_is_followed_by_its_hit() {
        let mut r = mapped();
        let press = |state| InputEvent::PointerButton {
            surface: S,
            position: p(10.0, 5.0),
            button: 0x110,
            state,
            time: 0,
        };
        let n = NodeId::new(42, 3);
        r.input(5, &press(ButtonState::Pressed), None, Some(n));
        r.input(6, &press(ButtonState::Released), None, Some(n));
        let e = entries(r.ring().unwrap());
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::MAPPED, kind::BUTTON, kind::HIT, kind::BUTTON],
            "a release has no hit"
        );
        assert_eq!(node_of(e[2].rec.u64(0)), n);
        assert_eq!(e[2].rec.t, 5);
    }

    #[test]
    fn surfaces_and_outputs_get_slots() {
        let mut r = InputRecorder::new(true);
        r.output(1, "id-a", Some("DP-1"), (1920, 1080), 120, (0, 0));
        r.output(2, "id-b", Some("HDMI-A-1"), (1280, 720), 180, (1920, 0));
        r.mapped(
            3,
            SurfaceId(1),
            NodeId::new(1, 0),
            Some(("id-b", Some("HDMI-A-1"))),
            NodeKind::Bar,
        );
        r.mapped(
            4,
            SurfaceId(9),
            NodeId::new(2, 0),
            Some(("id-a", Some("DP-1"))),
            NodeKind::Popup,
        );
        r.configured(5, SurfaceId(9), 300, 200, 120);
        r.placed(6, SurfaceId(9), 10, 40);
        r.unmapped(7, SurfaceId(1));
        r.output_gone(8, "id-b");
        let ring = r.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e.iter()
                .map(|e| (e.rec.kind, e.rec.slot))
                .collect::<Vec<_>>(),
            [
                (kind::OUTPUT, 0),
                (kind::OUTPUT, 1),
                (kind::MAPPED, 0),
                (kind::MAPPED, 1),
                (kind::CONFIGURED, 1),
                (kind::PLACED, 1),
                (kind::UNMAPPED, 0),
                (kind::OUTPUT_GONE, 1),
            ]
        );
        assert_eq!(ring.name_at(e[1].rec.u16(0)), Some("HDMI-A-1"));
        assert_eq!(
            (e[1].rec.u16(2), e[1].rec.u16(6), e[1].rec.i16(8)),
            (1280, 180, 1920)
        );
        assert_eq!(e[2].rec.u8(8), 1, "the bar is on the second output");
        assert_eq!(e[3].rec.u8(9), kind_byte(NodeKind::Popup));
        assert_eq!((e[4].rec.u16(0), e[4].rec.u16(2)), (300, 200));
        assert_eq!((e[5].rec.i32(0), e[5].rec.i32(4)), (10, 40));
        let left = r.surfaces();
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].slot, left[0].root), (1, NodeId::new(2, 0)));
    }

    #[test]
    fn refresh_lateness_and_backend_are_recorded() {
        let mut r = mapped();
        let ms = 1_000_000;
        r.refresh(10, S, Some(Duration::from_nanos(8_333_333)));
        r.refresh(11, S, Some(Duration::from_nanos(8_333_333)));
        r.painted(100 * ms, S, false, true);
        r.painted(100 * ms + 8_333_333, S, false, true);
        r.painted(100 * ms + 8_333_333 * 4, S, true, false);
        r.painted(200 * ms, S, true, false);
        let e = entries(r.ring().unwrap());
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::MAPPED, kind::REFRESH, kind::LATE, kind::BACKEND],
            "the refresh once, a frame three periods late, not one after an idle one"
        );
        assert_eq!(e[1].rec.u32(0), 8_333_333);
        assert_eq!(e[2].rec.u16(0), 2, "two frames missed");
        assert_eq!(e[3].rec.u8(0), 1, "to the GPU");
    }

    #[test]
    fn a_text_key_is_counted_not_named() {
        let mut r = mapped();
        let m = Modifiers::default();
        let shift = Modifiers { shift: true, ..m };
        for (i, (name, text, mm)) in [("h", "h", m), ("Q", "Q", shift), ("eacute", "é", m)]
            .into_iter()
            .enumerate()
        {
            r.input(
                10 + i as u64,
                &key(name, text, true, mm),
                Some(InputKind::Text),
                None,
            );
            r.input(
                11 + i as u64,
                &key(name, "", false, mm),
                Some(InputKind::Text),
                None,
            );
        }
        let ring = r.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::MAPPED, kind::TEXT],
            "one count; no key and no release"
        );
        assert_eq!(e[1].rec.u32(0), 3);
        let all = bytes(ring);
        for s in ["eacute", "Q\0", "é"] {
            assert!(!contains(&all, s.as_bytes()), "{s} is in the dump");
        }
        assert!(ring.find_name("h").is_none() && ring.find_name("Q").is_none());
    }

    #[test]
    fn ctrl_keys_are_named() {
        let mut r = mapped();
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        r.input(1, &key("c", "c", true, ctrl), Some(InputKind::Text), None);
        r.input(2, &key("c", "", false, ctrl), Some(InputKind::Text), None);
        r.input(
            3,
            &key("Return", "", true, Modifiers::default()),
            None,
            None,
        );
        let ring = r.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e[1..]
                .iter()
                .map(|e| (e.rec.kind, e.rec.u8(4)))
                .collect::<Vec<_>>(),
            [(kind::KEY, 1), (kind::KEY, 0), (kind::KEY, 1)],
            "pressed, released, pressed"
        );
        assert_eq!(ring.name_at(e[1].rec.u16(0)), Some("c"));
        assert_eq!(e[1].rec.u16(2), modifiers::CTRL);
        assert_eq!(ring.name_at(e[3].rec.u16(0)), Some("Return"));
    }

    /// (report review) A named key whose release never came (keyboard
    /// focus left while it was down, and Wayland sends no release after
    /// `leave`) does not make a later typed press of its letter recorded
    /// by its release: after ctrl+a, a leave and an enter, a plain `a`
    /// typed into the same surface is counted and nothing names it.
    #[test]
    fn a_key_left_down_does_not_name_a_later_typed_letter() {
        let m = Modifiers::default();
        let ctrl = Modifiers { ctrl: true, ..m };
        for leave in [true, false] {
            let mut r = mapped();
            r.input(1, &key("a", "a", true, ctrl), None, None);
            if leave {
                r.input(2, &InputEvent::KeyboardLeave { surface: S }, None, None);
                r.input(3, &InputEvent::KeyboardEnter { surface: S }, None, None);
            }
            r.input(4, &key("a", "a", true, m), Some(InputKind::Text), None);
            r.input(5, &key("a", "", false, m), Some(InputKind::Text), None);
            let e = entries(r.ring().unwrap());
            let after: Vec<&Entry> = e.iter().filter(|e| e.rec.t >= 2).collect();
            assert!(
                after.iter().all(|e| e.rec.kind != kind::KEY),
                "leave {leave}: {after:?}"
            );
            assert!(
                after
                    .iter()
                    .any(|e| e.rec.kind == kind::TEXT && e.rec.u32(0) == 1),
                "leave {leave}: the letter is counted: {after:?}"
            );
        }
    }

    #[test]
    fn a_password_span_has_no_count() {
        let mut r = mapped();
        let m = Modifiers::default();
        for (name, text) in [("h", "h"), ("BackSpace", ""), ("2", "2"), ("Return", "")] {
            r.input(
                1,
                &key(name, text, true, m),
                Some(InputKind::Password),
                None,
            );
            r.input(2, &key(name, "", false, m), Some(InputKind::Password), None);
        }
        let ring = r.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::MAPPED, kind::TEXT]
        );
        assert_eq!(
            (e[1].rec.u32(0), e[1].rec.u8(8)),
            (0, 1),
            "no count, a password span"
        );
        assert!(ring.find_name("BackSpace").is_none() && ring.find_name("Return").is_none());
        // A release whose press was in the password span is not
        // recorded once the field loses focus.
        r.input(3, &key("x", "", false, m), None, None);
        assert_eq!(entries(r.ring().unwrap()).len(), 2);
    }

    #[test]
    fn a_paste_is_a_count() {
        let mut r = mapped();
        let paste = InputEvent::Paste {
            surface: S,
            text: "hunter2-Z9q".into(),
        };
        r.input(1, &paste, Some(InputKind::Text), None);
        r.input(2, &paste, Some(InputKind::Password), None);
        let ring = r.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e[1..]
                .iter()
                .map(|e| (e.rec.kind, e.rec.u32(0), e.rec.u8(4)))
                .collect::<Vec<_>>(),
            [(kind::PASTE, 11, 0), (kind::PASTE, 0, 1)]
        );
        assert!(!contains(&bytes(ring), b"hunter2"));
        // A drop is its kinds.
        r.input(
            3,
            &InputEvent::DragDrop {
                surface: S,
                at: p(1.0, 1.0),
                payload: DropPayload::External {
                    kind: DropKind::Text,
                    files: vec![],
                    text: "hunter2-Z9q".into(),
                    app_id: None,
                },
            },
            None,
            None,
        );
        assert!(!contains(&bytes(r.ring().unwrap()), b"hunter2"));
    }

    #[test]
    fn a_password_dialog_or_login_screen_is_a_gap_of_its_own() {
        let mut r = mapped();
        let motion = |x| InputEvent::PointerMotion {
            surface: S,
            position: p(x, 1.0),
            time: 0,
        };
        r.secret(10, true);
        r.input(11, &motion(1.0), None, None);
        // A lock over it: its gap ends and the lock's begins.
        r.lock(12, true);
        r.lock(13, false);
        r.input(14, &motion(2.0), None, None);
        r.secret(15, false);
        r.input(16, &motion(3.0), None, None);
        let e = entries(r.ring().unwrap());
        assert_eq!(
            e.iter()
                .map(|e| (e.rec.kind, e.rec.t, e.rec.u8(0)))
                .filter(|(k, ..)| *k != kind::MAPPED)
                .collect::<Vec<_>>(),
            [
                (kind::GAP_BEGINS, 10, gap::SECRET),
                (kind::GAP_ENDS, 12, gap::SECRET),
                (kind::GAP_BEGINS, 12, gap::LOCK),
                (kind::GAP_ENDS, 13, gap::LOCK),
                (kind::GAP_BEGINS, 13, gap::SECRET),
                (kind::GAP_ENDS, 15, gap::SECRET),
                (kind::MOTION, 16, e.last().unwrap().rec.u8(0)),
            ]
        );
        // Turned on while it shows, a ring starts in its gap.
        r.secret(17, true);
        r.set_on(false, 18);
        r.set_on(true, 19);
        assert!(r.ring().unwrap().paused());
        // The logic side's ring pauses alike.
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        c.secret(1, true);
        c.cell(2, "a", &Value::int(1), &Value::int(2));
        c.secret(3, false);
        let e = entries(c.ring().unwrap());
        assert_eq!(
            e.iter()
                .map(|e| (e.rec.kind, e.rec.u8(0)))
                .collect::<Vec<_>>(),
            [
                (kind::CHANGE_GAP_BEGINS, gap::SECRET),
                (kind::CHANGE_GAP_ENDS, gap::SECRET)
            ]
        );
    }

    #[test]
    fn the_ring_pauses_from_lock_asked_to_unlock() {
        let mut r = mapped();
        let motion = |x| InputEvent::PointerMotion {
            surface: S,
            position: p(x, 1.0),
            time: 0,
        };
        let ms = 1_000_000;
        r.input(10 * ms, &motion(1.0), None, None);
        r.lock(20 * ms, true);
        r.input(30 * ms, &motion(2.0), None, None);
        r.input(
            31 * ms,
            &key("Return", "", true, Modifiers::default()),
            None,
            None,
        );
        r.mapped(
            32 * ms,
            SurfaceId(99),
            NodeId::new(9, 0),
            None,
            NodeKind::Lock,
        );
        r.lock(40 * ms, false);
        r.input(50 * ms, &motion(3.0), None, None);
        let e = entries(r.ring().unwrap());
        assert_eq!(
            e.iter()
                .map(|e| (e.rec.kind, e.rec.t / ms))
                .collect::<Vec<_>>(),
            [
                (kind::MAPPED, 0),
                (kind::MOTION, 10),
                (kind::GAP_BEGINS, 20),
                (kind::GAP_ENDS, 40),
                (kind::MOTION, 50),
            ]
        );
        assert_eq!(e[2].rec.u8(0), gap::LOCK);
        // The logic side's ring pauses alike.
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        c.lock(1, true);
        c.cell(2, "a", &Value::int(1), &Value::int(2));
        c.action(3, "go", 0, true, 0);
        c.lock(4, false);
        let kinds: Vec<u8> = entries(c.ring().unwrap())
            .iter()
            .map(|e| e.rec.kind)
            .collect();
        assert_eq!(kinds, [kind::CHANGE_GAP_BEGINS, kind::CHANGE_GAP_ENDS]);
    }

    #[test]
    fn pick_mode_is_a_gap_and_off_drops_the_ring() {
        let mut r = mapped();
        r.pick(1, true);
        r.input(2, &InputEvent::ClickAway { surface: S }, None, None);
        r.pick(3, false);
        assert_eq!(
            kinds(r.ring().unwrap()),
            [kind::MAPPED, kind::GAP_BEGINS, kind::GAP_ENDS]
        );
        r.output(4, "out-1", Some("DP-1"), (1920, 1080), 120, (0, 0));
        r.refresh(4, S, Some(Duration::from_nanos(6_944_444)));
        r.set_on(false, 5);
        assert!(r.ring().is_none() && r.dump().is_none());
        r.set_on(true, 6);
        // (report review) On again: the recorder off ends there, and the
        // output, the mapped surface and its refresh period are written
        // again, so a replay's window starts after it with its surfaces.
        let e = entries(r.ring().unwrap());
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::GAP_ENDS, kind::OUTPUT, kind::MAPPED, kind::REFRESH],
            "on again: {e:?}"
        );
        assert_eq!(e[0].rec.u8(0), gap::OFF);
        assert_eq!(
            (
                e[1].rec.u16(2),
                e[1].rec.u16(4),
                e[2].rec.u64(0),
                e[2].rec.u8(8)
            ),
            (1920, 1080, node_bits(NodeId::new(3, 1)), 0)
        );
        assert_eq!(e[3].rec.u32(0), 6_944_444);
        r.input(7, &InputEvent::ClickAway { surface: S }, None, None);
        assert_eq!(
            kinds(r.ring().unwrap()).last(),
            Some(&kind::CLICK_AWAY),
            "slots are kept"
        );
    }

    /// (fidelity) A cell kept per screen has a name too long for the
    /// ring's table; a write of it over the value cap still says whose
    /// it was (a version-1 recorder lost the name with the value).
    #[test]
    fn a_long_named_cell_over_the_cap_keeps_its_name() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        let path = "Notch[Dell Inc. | DELL U2720Q | Dell Inc. DELL U2720Q HQ1MTS2].history";
        let big = Value::list(vec![Value::int(1); flight::VALUE_CAP]);
        c.cell(1, path, &Value::int(0), &big);
        c.cell(2, path, &big, &Value::int(1));
        let e = entries(c.ring().unwrap());
        let cells: Vec<&Entry> = e.iter().filter(|e| e.rec.kind == kind::CELL).collect();
        assert_eq!(cells.len(), 2);
        for c in &cells {
            assert_eq!(c.rec.slot, NAME_IN_ARENA);
            assert_eq!(c.name.as_deref(), Some(path));
            assert_ne!(c.rec.flags & flags::TRUNCATED, 0);
            assert_eq!(c.value, None);
        }
    }

    fn program(src: &str) -> Arc<Program> {
        let (map, _) = strand_compiler::source::SourceMap::single("t.strand", src.to_string());
        let build =
            strand_compiler::reconcile::Build::compile_with(None, map, crate::services::schema())
                .unwrap_or_else(|d| panic!("{d:?}"));
        build.program.clone()
    }

    fn value_of(e: &Entry) -> serde_json::Value {
        let v = e.value.as_ref().unwrap();
        serde_json::from_slice(v).unwrap()
    }

    /// (report review) The changes ring turned on again starts with the
    /// recorder off's end and the surface roots shown, written again.
    #[test]
    fn the_changes_ring_on_again_writes_its_roots_again() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        c.root(
            1,
            NodeId::new(4, 1),
            NodeKind::Panel,
            Some("shell.strand:3:1"),
        );
        c.set_on(false, 2);
        assert!(c.ring().is_none());
        c.set_on(true, 3);
        let ring = c.ring().unwrap();
        let e = entries(ring);
        assert_eq!(
            e.iter().map(|e| e.rec.kind).collect::<Vec<_>>(),
            [kind::CHANGE_GAP_ENDS, kind::ROOT]
        );
        assert_eq!(e[0].rec.u8(0), gap::OFF);
        assert_eq!(e[1].rec.u64(0), node_bits(NodeId::new(4, 1)));
        assert_eq!(ring.name_at(e[1].rec.slot), Some("shell.strand:3:1"));
    }

    /// (replayfix) A surface root with no place in the config (Strand's
    /// own error panel) is recorded with no name, never `?`, when it
    /// comes, goes and is written again; one beside it keeps its origin.
    #[test]
    fn a_root_with_no_origin_is_recorded_with_no_name() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        c.root(
            1,
            NodeId::new(4, 1),
            NodeKind::Bar,
            Some("shell.strand:1:1"),
        );
        c.root(2, NodeId::new(77, 0), NodeKind::Panel, None);
        assert!(c.is_root(NodeId::new(77, 0)));
        c.set_on(false, 3);
        c.set_on(true, 4);
        c.removed(5, NodeId::new(77, 0));
        let ring = c.ring().unwrap();
        let e = entries(ring);
        let roots: Vec<(u64, Option<String>)> = e
            .iter()
            .filter(|e| e.rec.kind == kind::ROOT)
            .map(|e| (e.rec.u64(0), e.name.clone()))
            .collect();
        assert_eq!(
            roots,
            // Written again in node order.
            [
                (node_bits(NodeId::new(77, 0)), None),
                (
                    node_bits(NodeId::new(4, 1)),
                    Some("shell.strand:1:1".into())
                ),
            ]
        );
        assert_eq!(e.last().map(|e| e.rec.kind), Some(kind::ROOT_GONE));
        assert_eq!(e.last().map(|e| e.rec.slot), Some(NAME_IN_ARENA));
        let dump = flight::read(&ring.dump(0)).unwrap();
        assert_eq!(dump.names, ["shell.strand:1:1"], "no placeholder name");
    }

    #[test]
    fn auth_is_never_recorded() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        let old = Data::text("");
        let new = Data::text("Password:");
        c.service(
            1,
            &TapEvent {
                service: "auth",
                change: Change::Field {
                    field: "prompt",
                    old: &old,
                    new: &new,
                    initial: false,
                    local: false,
                },
            },
        );
        c.wrote(2, "auth", &[PathSeg::Field("x".into())], &Value::text("y"));
        assert!(c.call("auth", "check", &[]).is_none());
        assert!(c.ring().unwrap().is_empty());
        assert!(!contains(&c.dump().unwrap(), b"auth"));
    }

    /// (services-round) Nor are `polkit` and `greetd`, whose actions
    /// carry an admin password and a login's answers.
    #[test]
    fn polkit_and_greetd_are_never_recorded() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        let old = Data::text("");
        let new = Data::text("Password:");
        for service in ["polkit", "greetd"] {
            c.service(
                1,
                &TapEvent {
                    service,
                    change: Change::Field {
                        field: "prompt",
                        old: &old,
                        new: &new,
                        initial: false,
                        local: false,
                    },
                },
            );
            c.wrote(2, service, &[PathSeg::Field("x".into())], &Value::text("y"));
            assert!(c.call(service, "submit", &[]).is_none());
        }
        assert!(c.ring().unwrap().is_empty());
        let dump = c.dump().unwrap();
        assert!(!contains(&dump, b"polkit") && !contains(&dump, b"greetd"));
    }

    #[test]
    fn clipboard_texts_are_lengths() {
        let mut c = ChangeRecorder::new(true, program("let x = clipboard.text"));
        let old = Data::Null;
        let new = Data::Record {
            ty: "ClipEntry".into(),
            fields: vec![
                ("text".into(), Data::text("hunter2-Z9q")),
                ("width".into(), Data::Int(64)),
            ],
        };
        c.service(
            1,
            &TapEvent {
                service: "clipboard",
                change: Change::Field {
                    field: "text",
                    old: &old,
                    new: &new,
                    initial: false,
                    local: false,
                },
            },
        );
        let ring = c.ring().unwrap();
        let e = entries(ring);
        assert_eq!(e[0].name.as_deref(), Some("clipboard.text"));
        assert_eq!(
            value_of(&e[0]),
            serde_json::json!([null, {"text": {"~text": 11}, "width": 64}])
        );
        assert_ne!(e[0].rec.flags & flags::REDACTED, 0);
        assert!(!contains(&c.dump().unwrap(), b"hunter2"));
        assert_eq!(
            ChangeRecorder::service_json("clipboard", &new),
            r#"{"text":{"~text":11},"width":64}"#
        );
        // Another service's texts are kept as reported.
        assert_eq!(
            ChangeRecorder::service_json("windows", &Data::text("Helix")),
            "\"Helix\""
        );
    }

    /// A declared service's item of field `i`.
    fn declared(i: i64, v: Data) -> (Data, Data) {
        let item = Data::Record {
            ty: "CustomValue".into(),
            fields: vec![("index".into(), Data::Int(i)), ("value".into(), v)],
        };
        (Data::Int(i), item)
    }

    #[test]
    fn a_declared_service_is_recorded_by_its_fields() {
        let src =
            "service mood from file \"/m.json\" { level: int; name: text }\nlet x = mood.level";
        let mut c = ChangeRecorder::new(true, program(src));
        let tell = |c: &mut ChangeRecorder,
                    t: u64,
                    diffs: &[strand_core::VecDiff<Data, Data>],
                    initial: bool| {
            c.service(
                t,
                &TapEvent {
                    service: "mood",
                    change: Change::Keyed {
                        field: "values",
                        diffs,
                        removed: &[],
                        initial,
                        local: false,
                    },
                },
            );
        };
        let boot = [strand_core::VecDiff::Reset {
            items: vec![declared(0, Data::Int(1)), declared(1, Data::text("calm"))],
        }];
        tell(&mut c, 1, &boot, true);
        let (k, item) = declared(0, Data::Int(7));
        let up = [strand_core::VecDiff::Update {
            index: 0,
            key: k,
            value: item,
        }];
        tell(&mut c, 2, &up, false);
        let e = entries(c.ring().unwrap());
        let got: Vec<_> = e
            .iter()
            .map(|e| {
                (
                    e.rec.kind,
                    e.name.clone().unwrap(),
                    value_of(e),
                    e.rec.flags & flags::BOOT != 0,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    kind::FIELD,
                    "mood.level".into(),
                    serde_json::json!([null, 1]),
                    true
                ),
                (
                    kind::FIELD,
                    "mood.name".into(),
                    serde_json::json!([null, "calm"]),
                    true
                ),
                (
                    kind::FIELD,
                    "mood.level".into(),
                    serde_json::json!([1, 7]),
                    false
                ),
            ]
        );
        // The snapshot by the declared names too.
        let snap = [
            ("spec", Data::Int(3)),
            ("values", Data::List(vec![declared(0, Data::Int(7)).1])),
        ];
        assert_eq!(
            c.declared_fields("mood", &snap),
            Some(vec![
                ("level".into(), Data::Int(7)),
                ("name".into(), Data::Null)
            ])
        );
        assert_eq!(c.declared_fields("battery", &snap), None);
    }

    #[test]
    fn a_non_literal_cell_text_is_a_length() {
        let mut c = ChangeRecorder::new(true, program("state query = \"\"\nlet x = \"open\""));
        c.cell(1, "query", &Value::text(""), &Value::text("hunter2-Z9q"));
        let e = entries(c.ring().unwrap());
        assert_eq!(value_of(&e[0]), serde_json::json!(["", {"~text": 11}]));
        assert_ne!(e[0].rec.flags & flags::REDACTED, 0);
        assert!(!contains(&c.dump().unwrap(), b"hunter2"));
        assert_eq!(
            c.cell_json("query", &Value::text("hunter2-Z9q")),
            r#"{"~text":11}"#
        );
        assert_eq!(
            c.cell_json("$accent", &Value::text("#ff0000")),
            r##""#ff0000""##
        );
    }

    /// (report review) `strand set`, `toggle` and `reset`: the verb and
    /// the value after, held back as a cell's text is; a token overlay's
    /// text kept.
    #[test]
    fn a_request_records_its_verb_and_the_value_after() {
        let mut c = ChangeRecorder::new(true, program("state q = \"\"\nstate on = false"));
        c.request(1, flight::request::TOGGLE, "shell.on", &Value::Bool(true));
        c.request(
            2,
            flight::request::SET,
            "shell.q",
            &Value::text("hunter2-Z9q"),
        );
        c.request(3, flight::request::SET, "$accent", &Value::text("#ff8800"));
        let e = entries(c.ring().unwrap());
        assert!(e.iter().all(|e| e.rec.kind == kind::REQUEST));
        assert_eq!(e[0].rec.u8(0), flight::request::TOGGLE);
        assert_eq!(e[0].name.as_deref(), Some("shell.on"));
        assert_eq!(value_of(&e[0]), serde_json::json!(true));
        assert_eq!(e[1].rec.u8(0), flight::request::SET);
        assert_eq!(value_of(&e[1]), serde_json::json!({"~text": 11}));
        assert_ne!(e[1].rec.flags & flags::REDACTED, 0);
        assert_eq!(value_of(&e[2]), serde_json::json!("#ff8800"));
        assert!(!contains(&c.dump().unwrap(), b"hunter2"));
    }

    /// (report review) The identity a press found is kept while
    /// recording, the newest [`HITS_KEPT`], and dropped with the ring.
    #[test]
    fn pressed_identities_are_kept_while_recording() {
        let mut c = ChangeRecorder::new(true, program("let x = 1"));
        let who = || HitIdentity {
            file: strand_compiler::source::FileId::default(),
            span: strand_compiler::syntax::Span::default(),
            ident: None,
        };
        for i in 0..=HITS_KEPT as u32 {
            c.hit(NodeId::new(i, 1), who());
        }
        assert!(
            c.hit_identity(NodeId::new(0, 1)).is_none(),
            "the oldest went"
        );
        assert!(c.hit_identity(NodeId::new(HITS_KEPT as u32, 1)).is_some());
        c.set_on(false, 5);
        c.set_on(true, 6);
        assert!(c.hit_identity(NodeId::new(HITS_KEPT as u32, 1)).is_none());
        c.lock(7, true);
        c.hit(NodeId::new(1, 1), who());
        assert!(
            c.hit_identity(NodeId::new(1, 1)).is_none(),
            "nothing while locked"
        );
    }

    #[test]
    fn a_literal_cell_text_is_kept() {
        let mut c = ChangeRecorder::new(true, program("state mode = \"open\"\nlet x = \"closed\""));
        c.cell(1, "mode", &Value::text("open"), &Value::text("closed"));
        let e = entries(c.ring().unwrap());
        assert_eq!(e[0].name.as_deref(), Some("mode"));
        assert_eq!(value_of(&e[0]), serde_json::json!(["open", "closed"]));
        assert_eq!(e[0].rec.flags & flags::REDACTED, 0);
    }

    #[test]
    fn a_call_given_typed_text_keeps_no_text_of_its_answer() {
        let mut c = ChangeRecorder::new(true, program("let q = \"firefox\""));
        let answer = Ok(Value::list(vec![Value::text("hunter2-Z9q result")]));
        let typed = c
            .call("apps", "search", &[Value::text("hunter2-Z9q")])
            .unwrap();
        c.answered(5, &typed, &answer);
        let literal = c.call("apps", "search", &[Value::text("firefox")]).unwrap();
        c.answered(6, &literal, &Ok(Value::list(vec![Value::text("Firefox")])));
        let ring = c.ring().unwrap();
        let e = entries(ring);
        assert_eq!(e[0].name.as_deref(), Some("apps.search"));
        assert_eq!(value_of(&e[0]), serde_json::json!({"ok": [{"~text": 18}]}));
        assert_eq!((e[0].rec.u32(0), e[1].rec.u32(0)), (0, 1), "call ids");
        assert_eq!(value_of(&e[1]), serde_json::json!({"ok": ["Firefox"]}));
        assert!(!contains(&c.dump().unwrap(), b"hunter2"));
    }

    #[test]
    fn keyed_patches_keep_their_diffs_and_removed_items() {
        let mut c = ChangeRecorder::new(true, program("let x = windows.all.len"));
        let diffs = [
            strand_core::VecDiff::Insert {
                index: 1,
                key: Data::text("w2"),
                value: Data::Int(2),
            },
            strand_core::VecDiff::Remove {
                index: 0,
                key: Data::text("w1"),
            },
        ];
        let removed = [Data::Int(1)];
        c.service(
            1,
            &TapEvent {
                service: "windows",
                change: Change::Keyed {
                    field: "all",
                    diffs: &diffs,
                    removed: &removed,
                    initial: true,
                    local: false,
                },
            },
        );
        let e = entries(c.ring().unwrap());
        assert_eq!(e[0].rec.kind, kind::KEYED);
        assert_ne!(e[0].rec.flags & flags::BOOT, 0);
        assert_eq!(
            value_of(&e[0]),
            serde_json::json!({"d": [{"insert": [1, "w2", 2]}, {"remove": [0, "w1"]}], "r": [1]})
        );
    }

    /// A record of `ty` with `fields`.
    fn rec(ty: &'static str, fields: Vec<(&'static str, Data)>) -> Data {
        Data::Record {
            ty: ty.into(),
            fields: fields.into_iter().map(|(n, d)| (n.into(), d)).collect(),
        }
    }

    /// A workspace of `id` holding windows titled `titles`.
    fn workspace(id: i64, focused: bool, titles: &[&str]) -> Data {
        let windows = titles
            .iter()
            .enumerate()
            .map(|(i, t)| {
                rec(
                    "Window",
                    vec![
                        ("id", Data::text(format!("w{id}-{i}").as_str())),
                        ("title", Data::text(*t)),
                        ("app_id", Data::text("busy-term")),
                        (
                            "app",
                            rec(
                                "App",
                                vec![
                                    ("id", Data::text("busy-term")),
                                    ("name", Data::text("Terminal")),
                                    (
                                        "categories",
                                        Data::List(vec![
                                            Data::text("System"),
                                            Data::text("TerminalEmulator"),
                                        ]),
                                    ),
                                ],
                            ),
                        ),
                    ],
                )
            })
            .collect();
        rec(
            "Workspace",
            vec![
                ("id", Data::Int(id)),
                ("name", Data::text(id.to_string().as_str())),
                ("focused", Data::Bool(focused)),
                ("windows", Data::List(windows)),
            ],
        )
    }

    fn field_change(
        c: &mut ChangeRecorder,
        t: u64,
        service: &str,
        field: &'static str,
        old: &Data,
        new: &Data,
    ) {
        c.service(
            t,
            &TapEvent {
                service,
                change: Change::Field {
                    field,
                    old,
                    new,
                    initial: false,
                    local: false,
                },
            },
        );
    }

    fn keyed_change(
        c: &mut ChangeRecorder,
        t: u64,
        service: &str,
        field: &'static str,
        diffs: &[strand_core::VecDiff<Data, Data>],
        removed: &[Data],
    ) {
        c.service(
            t,
            &TapEvent {
                service,
                change: Change::Keyed {
                    field,
                    diffs,
                    removed,
                    initial: false,
                    local: false,
                },
            },
        );
    }

    /// (services) A config's write is recorded as the change it made,
    /// flagged `LOCAL`, with what it replaced: a field's (here a leaf
    /// below it, as its edits) and an item of a keyed list's; the
    /// service's answer after it is an ordinary change from the value
    /// the write left.
    #[test]
    fn a_local_write_is_recorded_with_what_it_replaced() {
        use strand_core::VecDiff;
        let mut c = ChangeRecorder::new(
            true,
            program(
                "let a = notifications.dnd\nlet b = workspaces.focused\nlet c = workspaces.all.len",
            ),
        );
        let tell = |c: &mut ChangeRecorder, t: u64, svc: &str, change: Change<'_>| {
            c.service(
                t,
                &TapEvent {
                    service: svc,
                    change,
                },
            );
        };
        tell(
            &mut c,
            1,
            "notifications",
            Change::Field {
                field: "dnd",
                old: &Data::Bool(false),
                new: &Data::Bool(true),
                initial: false,
                local: true,
            },
        );
        let (w, written) = (
            workspace(2, true, &["a", "b"]),
            workspace(2, true, &["a", "x"]),
        );
        tell(
            &mut c,
            2,
            "workspaces",
            Change::Field {
                field: "focused",
                old: &w,
                new: &written,
                initial: false,
                local: true,
            },
        );
        let answered = workspace(2, false, &["a", "x"]);
        field_change(&mut c, 3, "workspaces", "focused", &written, &answered);
        let up = [VecDiff::Update {
            index: 0,
            key: Data::Int(2),
            value: written.clone(),
        }];
        tell(
            &mut c,
            4,
            "workspaces",
            Change::Keyed {
                field: "all",
                diffs: &up,
                removed: std::slice::from_ref(&w),
                initial: false,
                local: true,
            },
        );
        let e = entries(c.ring().unwrap());
        let local: Vec<bool> = e.iter().map(|e| e.rec.flags & flags::LOCAL != 0).collect();
        assert_eq!(local, [true, true, false, true]);
        assert_eq!(e[0].name.as_deref(), Some("notifications.dnd"));
        assert_eq!(value_of(&e[0]), serde_json::json!([false, true]));
        assert_eq!(
            value_of(&e[1]),
            serde_json::json!({"e": [["windows.1.title", "b", "x"]]})
        );
        assert_eq!(
            value_of(&e[2]),
            serde_json::json!({"e": [["focused", true, false]]})
        );
        assert_eq!(e[3].rec.kind, kind::KEYED);
        assert_eq!(
            value_of(&e[3])["d"],
            serde_json::json!([{"patch": [0, 2, [["windows.1.title", "b", "x"]]]}])
        );
    }

    /// (replayfix) A field's change is what changed in it (`{"e": [[path,
    /// old, new]]}`) when that is shorter than the value before and
    /// after; a number, or a value of another shape, is `[old, new]`.
    #[test]
    fn a_field_change_is_its_edits_when_shorter() {
        let mut c = ChangeRecorder::new(
            true,
            program("let a = workspaces.focused\nlet b = cpu.usage"),
        );
        let before = workspace(2, true, &["~", "build 1%"]);
        let after = workspace(2, true, &["~", "build 2%"]);
        field_change(&mut c, 1, "workspaces", "focused", &before, &after);
        field_change(
            &mut c,
            2,
            "cpu",
            "usage",
            &Data::Float(0.25),
            &Data::Float(0.5),
        );
        // Another workspace: what differs, its windows whole (a list of
        // another length); then none, `[old, null]`.
        let other = workspace(3, true, &["notes"]);
        field_change(&mut c, 3, "workspaces", "focused", &after, &other);
        field_change(&mut c, 4, "workspaces", "focused", &other, &Data::Null);
        let e = entries(c.ring().unwrap());
        assert_eq!(
            value_of(&e[0]),
            serde_json::json!({"e": [["windows.1.title", "build 1%", "build 2%"]]})
        );
        assert_eq!(value_of(&e[1]), serde_json::json!([0.25, 0.5]));
        let paths: Vec<String> = value_of(&e[2])["e"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x[0].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, ["id", "name", "windows"]);
        assert_eq!(value_of(&e[3])[1], serde_json::Value::Null);
        assert!(e[0].value.as_ref().unwrap().len() < 60);
    }

    /// (replayfix) A keyed item changed in place is written as its edits
    /// (`{"patch": [index, key, edits]}`, nothing of it in `r`); an item
    /// replaced by its equal is no change; a removed item is kept whole.
    /// A patch that changes nothing is not recorded at all.
    #[test]
    fn a_keyed_update_is_patched_and_an_equal_one_dropped() {
        use strand_core::VecDiff;
        let mut c = ChangeRecorder::new(true, program("let a = workspaces.all.len"));
        let (w1, w1b) = (workspace(1, false, &["~"]), workspace(1, true, &["~"]));
        let w2 = workspace(2, false, &["a", "b"]);
        let w3 = workspace(3, false, &[]);
        let diffs = [
            VecDiff::Update {
                index: 0,
                key: Data::Int(1),
                value: w1b.clone(),
            },
            VecDiff::Update {
                index: 1,
                key: Data::Int(2),
                value: w2.clone(),
            },
            VecDiff::Remove {
                index: 2,
                key: Data::Int(3),
            },
        ];
        keyed_change(
            &mut c,
            1,
            "workspaces",
            "all",
            &diffs,
            &[w1, w2.clone(), w3],
        );
        // Only an equal item: nothing changed.
        let same = [VecDiff::Update {
            index: 1,
            key: Data::Int(2),
            value: w2.clone(),
        }];
        keyed_change(
            &mut c,
            2,
            "workspaces",
            "all",
            &same,
            std::slice::from_ref(&w2),
        );
        let e = entries(c.ring().unwrap());
        assert_eq!(e.len(), 1, "the change of nothing is not recorded: {e:?}");
        assert_eq!(
            value_of(&e[0]),
            serde_json::json!({
                "d": [{"patch": [0, 1, [["focused", false, true]]]}, {"remove": [2, 3]}],
                "r": [{"id": 3, "name": "3", "focused": false, "windows": []}]
            })
        );
        // A patch with a reset is written as it came.
        let reset = [VecDiff::Reset {
            items: vec![(Data::Int(2), w2.clone())],
        }];
        keyed_change(&mut c, 3, "workspaces", "all", &reset, &[]);
        let e = entries(c.ring().unwrap());
        assert!(value_of(&e[1])["d"][0].get("reset").is_some());
    }

    /// (replayfix) A field the config never reads is not recorded: not
    /// by name, nor through a method of its service. A service whose
    /// method the config calls (`workspaces.slots(screen)`) counts as
    /// read whole. A service's events are recorded.
    #[test]
    fn a_field_the_config_never_reads_is_not_recorded() {
        use strand_core::VecDiff;
        let src = "let t = windows.focused?.title ?? \"\"\nbar B {\n  row { for ws in workspaces.slots(screen) { text ws.name } }\n}";
        let mut c = ChangeRecorder::new(true, program(src));
        let w = rec(
            "Window",
            vec![("id", Data::text("1")), ("title", Data::text("a"))],
        );
        let w2 = rec(
            "Window",
            vec![("id", Data::text("1")), ("title", Data::text("b"))],
        );
        let up = [VecDiff::Update {
            index: 0,
            key: Data::text("1"),
            value: w2.clone(),
        }];
        keyed_change(&mut c, 1, "windows", "all", &up, std::slice::from_ref(&w));
        field_change(&mut c, 2, "windows", "focused", &w, &w2);
        let ws = workspace(4, false, &[]);
        let ins = [VecDiff::Insert {
            index: 0,
            key: Data::Int(4),
            value: ws,
        }];
        keyed_change(&mut c, 3, "workspaces", "configured", &ins, &[]);
        field_change(
            &mut c,
            4,
            "cpu",
            "usage",
            &Data::Float(0.1),
            &Data::Float(0.2),
        );
        // `system` is read by the theme and the renderer, so it is kept.
        field_change(
            &mut c,
            4,
            "system",
            "dark",
            &Data::Bool(false),
            &Data::Bool(true),
        );
        c.service(
            5,
            &TapEvent {
                service: "notifications",
                change: Change::Event {
                    event: "received",
                    args: &[Data::Int(7)],
                },
            },
        );
        let names: Vec<String> = entries(c.ring().unwrap())
            .into_iter()
            .filter_map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            [
                "windows.focused",
                "workspaces.configured",
                "system.dark",
                "notifications.received"
            ]
        );
        // A reload that reads `cpu.usage` records it from then on.
        c.set_program(program("let u = cpu.usage"));
        field_change(
            &mut c,
            6,
            "cpu",
            "usage",
            &Data::Float(0.2),
            &Data::Float(0.3),
        );
        field_change(&mut c, 7, "windows", "focused", &w2, &w);
        let e = entries(c.ring().unwrap());
        assert_eq!(e.last().and_then(|e| e.name.as_deref()), Some("cpu.usage"));
    }

    /// (replayfix) An edit holds its texts back by the same rules as a
    /// whole value: clipboard's are lengths.
    #[test]
    fn an_edit_holds_texts_back_as_a_value_does() {
        let mut c = ChangeRecorder::new(true, program("let x = clipboard.text"));
        let entry = |t: &str| {
            rec(
                "ClipEntry",
                vec![
                    ("text", Data::text(t)),
                    ("mime", Data::text("text/plain;charset=utf-8")),
                    ("width", Data::Int(64)),
                ],
            )
        };
        field_change(
            &mut c,
            1,
            "clipboard",
            "text",
            &entry("abc"),
            &entry("hunter2-Z9q"),
        );
        let e = entries(c.ring().unwrap());
        assert_eq!(
            value_of(&e[0]),
            serde_json::json!({"e": [["text", {"~text": 3}, {"~text": 11}]]})
        );
        assert_ne!(e[0].rec.flags & flags::REDACTED, 0);
        assert!(!contains(&c.dump().unwrap(), b"hunter2"));
    }
}
