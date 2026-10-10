//! Seats: pointer and keyboard input, key repeat and the events sent to
//! the host.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    // ---- events ----------------------------------------------------------

    pub(super) fn surface_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.by_wl.get(&wl.id()).copied()
    }

    /// A key event on the surface with keyboard focus.
    pub(super) fn key(&mut self, event: KeyEvent, state: ButtonState, repeat: bool) {
        // During a popup grab, the topmost grabbing popup takes the keys.
        let Some(surface) = self.grab_focus.or(self.keyboard_focus) else {
            return;
        };
        let name = key_name(event.keysym);
        let text = event
            .utf8
            .filter(|t| t.chars().all(|c| !c.is_control()))
            .unwrap_or_default();
        self.send_input(InputEvent::Key {
            surface,
            key: KeyInput {
                name,
                text,
                state,
                repeat,
                modifiers: self.modifiers,
                time: event.time,
            },
        });
    }

    /// Repeats `event` (just pressed on `keyboard`) at the keyboard's
    /// rate after its delay, from a calloop timer of ours; modifiers do
    /// not repeat. Replaces any key repeating before.
    pub(super) fn start_repeat(&mut self, keyboard: &wl_keyboard::WlKeyboard, event: KeyEvent) {
        self.stop_repeat();
        let Some(RepeatInfo::Repeat { rate, delay }) = self.repeat_info.get(&keyboard.id()) else {
            return;
        };
        if event.keysym.is_modifier_key() {
            return;
        }
        let interval = repeat_interval(rate.get());
        let raw = event.raw_code;
        let token = self.handle.insert_source(
            Timer::from_duration(Duration::from_millis(u64::from(*delay))),
            move |_, _, state: &mut State<H>| {
                // Nothing focused (its surface went without a leave): the
                // release will go elsewhere, so stop rather than wake at
                // the repeat rate forever.
                if state.grab_focus.or(state.keyboard_focus).is_none() {
                    state.key_repeat = None;
                    return TimeoutAction::Drop;
                }
                state.key(event.clone(), ButtonState::Pressed, true);
                TimeoutAction::ToDuration(interval)
            },
        );
        match token {
            Ok(t) => self.key_repeat = Some((keyboard.id(), raw, t)),
            Err(e) => log::warn!("cannot repeat a key: {}", e.error),
        }
    }

    /// Stops the key repeating, if any (from event handlers only, never a
    /// `Drop`).
    pub(super) fn stop_repeat(&mut self) {
        if let Some((_, _, token)) = self.key_repeat.take() {
            self.handle.remove(token);
        }
    }

    /// True while a key repeats (tests).
    pub fn key_repeating(&self) -> bool {
        self.key_repeat.is_some()
    }

    /// The surface with keyboard focus, if it is one of ours.
    pub fn keyboard_focus(&self) -> Option<SurfaceId> {
        self.keyboard_focus
    }

    /// True while layer surface `id` is `exclusive` because a grabbing
    /// popup of its is open (see `sync_popup_keyboard`).
    pub fn holds_keyboard_for_popup(&self, id: SurfaceId) -> bool {
        self.grab_keyboard.contains(&id)
    }

    pub(super) fn send_input(&mut self, event: InputEvent) {
        self.host.input(&event);
        if let Some(tx) = &self.input
            && tx.send(event).is_err()
        {
            // Nobody listens any more: stop queueing.
            self.input = None;
        }
    }
}

/// A keysym's xkb name without its `XK_` prefix (`Escape`, `Return`,
/// `Down`, `a`), or its code in hex for one without a name.
pub(super) fn key_name(sym: Keysym) -> String {
    match sym.name() {
        Some(n) => n.strip_prefix("XK_").unwrap_or(n).to_string(),
        None => format!("0x{:x}", sym.raw()),
    }
}

/// The time between repeats at `rate` keys a second, at least 1 ms: the
/// rate is the compositor's (SCTK casts a negative one to a huge `u32`),
/// and a zero interval would fire on every loop iteration.
pub(super) fn repeat_interval(rate: u32) -> Duration {
    Duration::from_micros((1_000_000 / u64::from(rate.max(1))).max(1_000))
}

impl<H: SurfaceHost + 'static> SeatHandler for State<H> {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && !self.pointers.iter().any(|p| p.seat == seat) {
            // A themed pointer sets the cursor through wp_cursor_shape_v1
            // when the compositor has it, else from the cursor theme.
            let cursor_surface = self.compositor.create_surface(qh);
            match self.seat_state.get_pointer_with_theme::<_, ()>(
                qh,
                &seat,
                self.shm.wl_shm(),
                cursor_surface,
                ThemeSpec::default(),
            ) {
                Ok(pointer) => self.pointers.push(SeatPointer {
                    seat: seat.clone(),
                    pointer,
                    button_serial: None,
                    enter_serial: None,
                }),
                Err(e) => log::warn!("cannot get the pointer: {e}"),
            }
        }
        if capability == Capability::Keyboard && !self.keyboards.iter().any(|(s, _)| *s == seat) {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboards.push((seat, k)),
                Err(e) => log::warn!("cannot get the keyboard: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            // Dropping a themed pointer releases it.
            self.pointers.retain(|p| p.seat != seat);
        }
        if capability == Capability::Keyboard {
            let gone: Vec<ObjectId> = self
                .keyboards
                .iter()
                .filter(|(s, _)| *s == seat)
                .map(|(_, k)| k.id())
                .collect();
            if self
                .key_repeat
                .as_ref()
                .is_some_and(|(k, _, _)| gone.contains(k))
            {
                self.stop_repeat();
            }
            for k in &gone {
                self.repeat_info.remove(k);
                self.raw_modifiers.remove(k);
            }
            self.keyboards.retain(|(s, k)| {
                let keep = *s != seat;
                if !keep && k.version() >= 3 {
                    k.release();
                }
                keep
            });
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

pub(super) fn axis_delta(a: &smithay_client_toolkit::seat::pointer::AxisScroll) -> AxisDelta {
    AxisDelta {
        pixels: a.absolute,
        value120: if a.value120 != 0 {
            a.value120
        } else {
            a.discrete.saturating_mul(120)
        },
        stop: a.stop,
    }
}

impl<H: SurfaceHost + 'static> PointerHandler for State<H> {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let seat = self
            .pointers
            .iter()
            .position(|p| p.pointer.pointer() == pointer);
        for e in events {
            if let Some(surface) = self.catcher_for(&e.surface) {
                // Only a press means anything on a catcher: a click
                // outside the surface it serves.
                match &e.kind {
                    PointerEventKind::Enter { .. } => {
                        if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                            let _ = p.pointer.set_cursor(conn, CursorIcon::Default);
                        }
                    }
                    PointerEventKind::Press { serial, .. } => {
                        if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                            p.button_serial = Some(*serial);
                            self.last_action = Some(UserAction {
                                seat: p.seat.clone(),
                                serial: *serial,
                                at: Instant::now(),
                            });
                        }
                        self.send_input(InputEvent::ClickAway { surface });
                    }
                    _ => {}
                }
                continue;
            }
            let Some(surface) = self.surface_for(&e.surface) else {
                continue;
            };
            let position = LogicalPoint::new(e.position.0 as f32, e.position.1 as f32);
            let event = match &e.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.enter_serial = Some(*serial);
                        match p.pointer.set_cursor(conn, CursorIcon::Default) {
                            Ok(()) => {
                                self.stats.cursor_sets += 1;
                                if let Some(s) = self.surfaces.get_mut(&surface) {
                                    s.stats.cursor_sets += 1;
                                }
                            }
                            Err(e) => log::debug!("cannot set the cursor: {e}"),
                        }
                    }
                    InputEvent::PointerEnter { surface, position }
                }
                PointerEventKind::Leave { .. } => InputEvent::PointerLeave { surface },
                PointerEventKind::Motion { time } => InputEvent::PointerMotion {
                    surface,
                    position,
                    time: *time,
                },
                PointerEventKind::Press {
                    time,
                    button,
                    serial,
                } => {
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.button_serial = Some(*serial);
                        self.last_action = Some(UserAction {
                            seat: p.seat.clone(),
                            serial: *serial,
                            at: Instant::now(),
                        });
                    }
                    self.last_pressed = Some(surface);
                    InputEvent::PointerButton {
                        surface,
                        position,
                        button: *button,
                        state: ButtonState::Pressed,
                        time: *time,
                    }
                }
                PointerEventKind::Release { time, button, .. } => InputEvent::PointerButton {
                    surface,
                    position,
                    button: *button,
                    state: ButtonState::Released,
                    time: *time,
                },
                PointerEventKind::Axis {
                    time,
                    horizontal,
                    vertical,
                    source,
                } => InputEvent::PointerAxis {
                    surface,
                    position,
                    horizontal: axis_delta(horizontal),
                    vertical: axis_delta(vertical),
                    source: source.map(|s| match s {
                        wl_pointer::AxisSource::Finger => AxisSource::Finger,
                        wl_pointer::AxisSource::Continuous => AxisSource::Continuous,
                        wl_pointer::AxisSource::WheelTilt => AxisSource::WheelTilt,
                        _ => AxisSource::Wheel,
                    }),
                    time: *time,
                },
            };
            self.send_input(event);
        }
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// Ends a held leave ([`State::held_leave`]) once the dispatch that
    /// brought it is over: no enter for its layer surface followed, so
    /// the keyboard really left, and the grabbing popup loses it too.
    pub(super) fn resolve_held_leave(&mut self) {
        let Some(layer) = self.held_leave.take() else {
            return;
        };
        if self.keyboard_focus == Some(layer) {
            return;
        }
        log::trace!("keyboard leave {layer:?} held, not stale");
        if let Some(p) = self.grab_focus.take()
            && self.surfaces.contains_key(&p)
        {
            self.send_input(InputEvent::KeyboardLeave { surface: p });
        }
    }
}

impl<H: SurfaceHost + 'static> KeyboardHandler for State<H> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(id) = self.keyboard_target(surface) {
            log::trace!("keyboard enter {id:?} (grab focus {:?})", self.grab_focus);
            self.releasing.remove(&id);
            if self.held_leave == Some(id) {
                // The held leave was stale: the grabbing popup keeps
                // the keys.
                self.held_leave = None;
            }
            self.keyboard_focus = Some(id);
            self.send_input(InputEvent::KeyboardEnter { surface: id });
            self.sync_popup_keyboard();
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.stop_repeat();
        let id = self.keyboard_target(surface).or(self.keyboard_focus);
        log::trace!("keyboard leave {id:?} (grab focus {:?})", self.grab_focus);
        if self.keyboard_focus == id {
            self.keyboard_focus = None;
        }
        // Maybe the leave for a grab given back, arriving after a new
        // grab took the keyboard again (sway sends it with the new grab's
        // enter, which gives the keys back): held until this dispatch
        // ends, when an enter for the same surface has made it stale or
        // its absence makes it a real focus loss. A compositor that kept
        // focus through the release sends no such leave, and a real one
        // later is told then all the same.
        let suspect = id.filter(|id| self.releasing.remove(id) && self.grab_keyboard.contains(id));
        if let Some(l) = suspect {
            self.held_leave = Some(l);
            self.handle
                .insert_idle(|state: &mut State<H>| state.resolve_held_leave());
        }
        // The keyboard left the layer surface: its grabbing popup loses it
        // too.
        if suspect.is_none()
            && let Some(p) = self.grab_focus.take()
            && self.surfaces.contains_key(&p)
        {
            self.send_input(InputEvent::KeyboardLeave { surface: p });
        }
        if let Some(id) = id {
            self.send_input(InputEvent::KeyboardLeave { surface: id });
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        if let Some((seat, _)) = self.keyboards.iter().find(|(_, k)| k == keyboard) {
            self.last_action = Some(UserAction {
                seat: seat.clone(),
                serial,
                at: Instant::now(),
            });
        }
        self.key(event.clone(), ButtonState::Pressed, false);
        self.start_repeat(keyboard, event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event, ButtonState::Pressed, true);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self
            .key_repeat
            .as_ref()
            .is_some_and(|(k, raw, _)| *k == keyboard.id() && *raw == event.raw_code)
        {
            self.stop_repeat();
        }
        self.key(event, ButtonState::Released, false);
    }

    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        info: RepeatInfo,
    ) {
        self.repeat_info.insert(keyboard.id(), info);
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        m: XkbModifiers,
        raw: RawModifiers,
        layout: u32,
    ) {
        let raw = (raw.depressed, raw.latched, raw.locked, layout);
        let id = keyboard.id();
        let changed = self.raw_modifiers.insert(id.clone(), raw) != Some(raw);
        if changed && self.key_repeat.as_ref().is_some_and(|(k, _, _)| *k == id) {
            // The repeating key's text was computed under its keyboard's
            // old modifiers; repeating it under the new ones would send
            // e.g. "a" with Shift held. Another seat's keyboard leaves it.
            self.stop_repeat();
        }
        self.modifiers = Modifiers {
            ctrl: m.ctrl,
            alt: m.alt,
            shift: m.shift,
            logo: m.logo,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repeat_interval_never_busy_loops() {
        assert_eq!(repeat_interval(25), Duration::from_millis(40));
        assert_eq!(repeat_interval(1_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(2_000_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(u32::MAX), Duration::from_millis(1));
        assert_eq!(repeat_interval(0), Duration::from_secs(1));
    }
}
