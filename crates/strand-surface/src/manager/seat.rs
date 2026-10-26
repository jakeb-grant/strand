//! Seats: pointer and keyboard input, key repeat and the events sent to
//! the host.

use super::*;

impl<H: SurfaceHost + 'static> State<H> {
    // ---- events ----------------------------------------------------------

    pub(super) fn surface_for(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        self.by_wl.get(&wl.id()).copied()
    }

    /// The Latin keysym's name shortcuts match for `event` on `keyboard`
    /// ([`LatinKeys::shortcut`]).
    fn latin(&self, keyboard: &wl_keyboard::WlKeyboard, event: &KeyEvent) -> Option<String> {
        self.latin_keys
            .get(&keyboard.id())?
            .shortcut(event.raw_code, event.keysym)
            .map(key_name)
    }

    /// A key event on the surface with keyboard focus (`latin`: the
    /// Latin keysym's name shortcuts match, [`LatinKeys`]).
    pub(super) fn key(
        &mut self,
        event: KeyEvent,
        latin: Option<String>,
        state: ButtonState,
        repeat: bool,
    ) {
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
                latin,
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
        let latin = self.latin(keyboard, &event);
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
                state.key(event.clone(), latin.clone(), ButtonState::Pressed, true);
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

    /// (gap 31) Tells the host the focused surface's keyboard state when
    /// it changed: the lock keys and layout of the keyboard that entered
    /// it, or `None` once none of the surfaces has the keyboard (or before
    /// its keyboard reported modifiers).
    pub(super) fn report_keyboard(&mut self) {
        let focused = self.keyboard_focus.or(self.grab_focus).is_some();
        let state = self
            .focus_keyboard
            .as_ref()
            .filter(|_| focused)
            .and_then(|k| {
                let (caps_lock, num_lock, layout, modifiers) = *self.lock_keys.get(k)?;
                let layouts = self.layout_names.get(k).cloned().unwrap_or_default();
                let layout = layouts
                    .get(layout as usize)
                    .filter(|n| !n.is_empty())
                    .cloned();
                Some(KeyboardState {
                    caps_lock,
                    num_lock,
                    layout,
                    layouts,
                    modifiers,
                })
            });
        if state != self.keyboard_reported {
            self.keyboard_reported.clone_from(&state);
            self.host.keyboard_state(state.as_ref());
        }
    }

    /// True while layer surface `id` is `exclusive` because a grabbing
    /// popup of its is open (see `sync_popup_keyboard`).
    pub fn holds_keyboard_for_popup(&self, id: SurfaceId) -> bool {
        self.grab_keyboard.contains(&id)
    }

    /// Hands `event` to the host and the input channel, its position
    /// mapped back from the coordinates of its surface's pose scale to
    /// the content at rest (`pose.rs`).
    pub(super) fn send_input(&mut self, mut event: InputEvent) {
        let factor = self
            .surfaces
            .get(&event.surface())
            .map_or((1.0, 1.0), |s| s.pose_factor());
        if factor != (1.0, 1.0) {
            unpose(&mut event, factor);
        }
        self.host.input(&event);
        self.dnd_input(&event);
        self.clipboard_asks();
        if let Some(tx) = &self.input
            && tx.send(event).is_err()
        {
            // Nobody listens any more: stop queueing.
            self.input = None;
        }
    }
}

/// Maps `event`'s position from a posed surface's coordinates (scaled
/// by `(fx, fy)`, [`Surface::pose_factor`]) back to its content's.
fn unpose(event: &mut InputEvent, (fx, fy): (f64, f64)) {
    let p = match event {
        InputEvent::PointerEnter { position, .. }
        | InputEvent::PointerMotion { position, .. }
        | InputEvent::PointerButton { position, .. }
        | InputEvent::PointerAxis { position, .. } => position,
        InputEvent::DragEnter { at, .. }
        | InputEvent::DragMotion { at, .. }
        | InputEvent::DragDrop { at, .. } => at,
        InputEvent::PointerLeave { .. }
        | InputEvent::KeyboardEnter { .. }
        | InputEvent::KeyboardLeave { .. }
        | InputEvent::Key { .. }
        | InputEvent::ClickAway { .. }
        | InputEvent::DragLeave { .. }
        | InputEvent::Paste { .. } => return,
    };
    p.x = (f64::from(p.x) / fx) as f32;
    p.y = (f64::from(p.y) / fy) as f32;
}

/// A keysym's xkb name without its `XK_` prefix (`Escape`, `Return`,
/// `Down`, `a`), or its code in hex for one without a name.
pub(super) fn key_name(sym: Keysym) -> String {
    match sym.name() {
        Some(n) => n.strip_prefix("XK_").unwrap_or(n).to_string(),
        None => format!("0x{:x}", sym.raw()),
    }
}

/// (M6, review r1) The Latin keysym of each key of a keymap with several
/// layouts: the first layout's (in the keymap's order) whose first level
/// gives the key a printable ASCII keysym. A shortcut matches it when the
/// active layout gives the key a non-ASCII character, as GTK and Qt match
/// theirs: with `us,ru` and `ru` active, Ctrl+V is `Cyrillic_em`, and
/// matches as `v`.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct LatinKeys(HashMap<u32, Keysym>);

/// A keymap in xkb's text format, compiled once for both what is read
/// from it ([`LatinKeys::of`], [`layout_names_of`]); `None` when it holds
/// a NUL or does not compile. A `wl_keyboard` keymap is complete, so no
/// include path or environment is consulted.
pub(super) fn compile_keymap(text: &str) -> Option<xkbcommon::xkb::Keymap> {
    use xkbcommon::xkb;
    if text.contains('\0') {
        return None;
    }
    let context =
        xkb::Context::new(xkb::CONTEXT_NO_DEFAULT_INCLUDES | xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    xkb::Keymap::new_from_string(
        &context,
        text.to_string(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

impl LatinKeys {
    /// From a keymap in xkb's text format (`None`: it does not compile).
    /// A keymap with one layout has none: its keys are what they are.
    #[cfg(test)]
    pub(super) fn from_text(text: &str) -> Option<LatinKeys> {
        compile_keymap(text).map(|keymap| LatinKeys::of(&keymap))
    }

    /// From a compiled keymap.
    pub(super) fn of(keymap: &xkbcommon::xkb::Keymap) -> LatinKeys {
        let mut keys = HashMap::new();
        if keymap.num_layouts() < 2 {
            return LatinKeys(keys);
        }
        keymap.key_for_each(|keymap, code| {
            let latin = (0..keymap.num_layouts_for_key(code)).find_map(|layout| {
                match keymap.key_get_syms_by_level(code, layout, 0) {
                    [sym] if (0x21..=0x7e).contains(&sym.raw()) => Some(*sym),
                    _ => None,
                }
            });
            // xkb's keycodes are the evdev codes `wl_keyboard` sends, plus 8.
            if let (Some(sym), Some(raw)) = (latin, code.raw().checked_sub(8)) {
                keys.insert(raw, sym);
            }
        });
        LatinKeys(keys)
    }

    /// The Latin keysym a shortcut matches for key `raw` typed as `sym`:
    /// `None` unless `sym` is a non-ASCII character and the key has one.
    pub(super) fn shortcut(&self, raw: u32, sym: Keysym) -> Option<Keysym> {
        if sym.key_char().is_none_or(|c| c.is_ascii()) {
            return None;
        }
        self.0.get(&raw).copied()
    }
}

/// (gap 31) The layout names of a keymap's text, by layout index (`""`
/// for one it does not name). A keymap that does not compile names none.
#[cfg(test)]
pub(super) fn layout_names(text: &str) -> Vec<String> {
    compile_keymap(text)
        .map(|keymap| layout_names_of(&keymap))
        .unwrap_or_default()
}

/// (gap 31) The layout names of a compiled keymap, by layout index.
pub(super) fn layout_names_of(keymap: &xkbcommon::xkb::Keymap) -> Vec<String> {
    (0..keymap.num_layouts())
        .map(|i| keymap.layout_get_name(i).to_string())
        .collect()
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
        // (M6) A keyboard's seat gets one too: an `input` copies and
        // pastes through its selection.
        if capability == Capability::Pointer || capability == Capability::Keyboard {
            self.dnd_seat(&seat);
        }
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
                    on: None,
                    shape: None,
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
                self.latin_keys.remove(k);
                self.layout_names.remove(k);
                self.lock_keys.remove(k);
                if self.focus_keyboard.as_ref() == Some(k) {
                    self.focus_keyboard = None;
                }
            }
            self.keyboards.retain(|(s, k)| {
                let keep = *s != seat;
                if !keep && k.version() >= 3 {
                    k.release();
                }
                keep
            });
            self.report_keyboard();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.dnd_seat_gone(&seat);
    }
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
                            p.on = None;
                            p.shape = None;
                        }
                    }
                    PointerEventKind::Press { serial, .. } => {
                        let on = self.root_layer(surface);
                        if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                            p.button_serial = Some(*serial);
                            let seat = p.seat.clone();
                            self.note_press(&seat, *serial);
                            self.last_action = Some(UserAction {
                                seat,
                                serial: *serial,
                                at: Instant::now(),
                                on,
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
                    // The cursor is set once the host has routed the
                    // enter (`sync_cursors`, below): a new surface shows
                    // none until a client sets one.
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.enter_serial = Some(*serial);
                        p.on = Some(surface);
                        p.shape = None;
                    }
                    InputEvent::PointerEnter { surface, position }
                }
                PointerEventKind::Leave { .. } => {
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i))
                        && p.on == Some(surface)
                    {
                        p.on = None;
                    }
                    InputEvent::PointerLeave { surface }
                }
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
                    let on = self.root_layer(surface);
                    if let Some(p) = seat.and_then(|i| self.pointers.get_mut(i)) {
                        p.button_serial = Some(*serial);
                        let seat = p.seat.clone();
                        self.note_press(&seat, *serial);
                        self.last_action = Some(UserAction {
                            seat,
                            serial: *serial,
                            at: Instant::now(),
                            on,
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
            // `STRAND_LOG=debug`: what the compositor told of the pointer,
            // its moves left out (the compositor matrix reads it).
            if !matches!(event, InputEvent::PointerMotion { .. }) {
                log::debug!("pointer: {event:?}");
            }
            self.send_input(event);
        }
        self.sync_cursors();
    }
}

impl<H: SurfaceHost + 'static> State<H> {
    /// (flex) Sets on each seat's pointer the shape the host asks for on
    /// the surface it is on, when it changed: `wp_cursor_shape_v1` where
    /// the compositor has it, else the cursor theme (`ThemedPointer`).
    pub(super) fn sync_cursors(&mut self) {
        for i in 0..self.pointers.len() {
            let Some(surface) = self.pointers[i].on else {
                continue;
            };
            let want = self.host.cursor(surface);
            if self.pointers[i].shape == Some(want) {
                continue;
            }
            // Not asked again for this shape whether or not it took: a
            // theme without it would fail on every frame.
            self.pointers[i].shape = Some(want);
            match self.pointers[i]
                .pointer
                .set_cursor(&self.conn, cursor_icon(want))
            {
                Ok(()) => {
                    self.stats.cursor_sets += 1;
                    if let Some(s) = self.surfaces.get_mut(&surface) {
                        s.stats.cursor_sets += 1;
                    }
                }
                Err(e) => log::debug!("cannot set the cursor {want}: {e}"),
            }
        }
    }

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
        // (gap 31) No Strand surface has the keys now: the leave's own
        // report still counted the grabbing popup as focused.
        self.report_keyboard();
    }
}

impl<H: SurfaceHost + 'static> KeyboardHandler for State<H> {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(id) = self.keyboard_target(surface) {
            self.focus_keyboard = Some(keyboard.id());
            log::trace!("keyboard enter {id:?} (grab focus {:?})", self.grab_focus);
            self.releasing.remove(&id);
            if self.held_leave == Some(id) {
                // The held leave was stale: the grabbing popup keeps
                // the keys.
                self.held_leave = None;
            }
            // (gap 31) The lock keys this keyboard reported are from the
            // last time Strand had it (wl_keyboard.modifiers goes only to
            // the focused client): stale, so they are dropped, and the
            // modifiers the compositor must send after an enter report
            // its state. Until then what was reported stands: none, or
            // this same keyboard's still current state when the enter
            // ends a leave held or sent in the same dispatch.
            self.lock_keys.remove(&keyboard.id());
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
        // (lockmon) A solid lock surface's keys went to the content
        // focused then, which the focused output no longer names.
        let id = self.surface_for(surface).or(self.keyboard_focus);
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
        self.report_keyboard();
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
            let seat = seat.clone();
            // (M5) Key serials are tracked beside the pointer's, for
            // xdg-activation tokens (`activation.rs`).
            self.note_press(&seat, serial);
            let on = self
                .grab_focus
                .or(self.keyboard_focus)
                .and_then(|f| self.root_layer(f));
            self.last_action = Some(UserAction {
                seat,
                serial,
                at: Instant::now(),
                on,
            });
        }
        let latin = self.latin(keyboard, &event);
        self.key(event.clone(), latin, ButtonState::Pressed, false);
        self.start_repeat(keyboard, event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let latin = self.latin(keyboard, &event);
        self.key(event, latin, ButtonState::Pressed, true);
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
        let latin = self.latin(keyboard, &event);
        self.key(event, latin, ButtonState::Released, false);
    }

    fn update_keymap(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        keyboard: &wl_keyboard::WlKeyboard,
        keymap: Keymap<'_>,
    ) {
        // SCTK hands the keymap only for the call: it is compiled again
        // from its text, once, for the Latin keys and the layout names.
        let (latin, names) = match compile_keymap(&keymap.as_string()) {
            Some(keymap) => (LatinKeys::of(&keymap), layout_names_of(&keymap)),
            None => {
                log::warn!(
                    "cannot read the keymap's layouts: shortcuts match the active layout's keys"
                );
                (LatinKeys::default(), Vec::new())
            }
        };
        self.latin_keys.insert(keyboard.id(), latin);
        self.layout_names.insert(keyboard.id(), names);
        self.report_keyboard();
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
        self.lock_keys
            .insert(id, (m.caps_lock, m.num_lock, layout, self.modifiers));
        self.report_keyboard();
    }
}

/// (flex) A [`strand_scene::CursorShape`] as the cursor-icon name both
/// `wp_cursor_shape_v1` and cursor themes know.
fn cursor_icon(shape: strand_scene::CursorShape) -> CursorIcon {
    use strand_scene::CursorShape as S;
    match shape {
        S::Default => CursorIcon::Default,
        S::Pointer => CursorIcon::Pointer,
        S::Text => CursorIcon::Text,
        S::Crosshair => CursorIcon::Crosshair,
        S::Grab => CursorIcon::Grab,
        S::Grabbing => CursorIcon::Grabbing,
        S::Move => CursorIcon::Move,
        S::NotAllowed => CursorIcon::NotAllowed,
        S::Wait => CursorIcon::Wait,
        S::Progress => CursorIcon::Progress,
        S::Help => CursorIcon::Help,
        S::ContextMenu => CursorIcon::ContextMenu,
        S::Cell => CursorIcon::Cell,
        S::Copy => CursorIcon::Copy,
        S::Alias => CursorIcon::Alias,
        S::NoDrop => CursorIcon::NoDrop,
        S::AllScroll => CursorIcon::AllScroll,
        S::ZoomIn => CursorIcon::ZoomIn,
        S::ZoomOut => CursorIcon::ZoomOut,
        S::ColResize => CursorIcon::ColResize,
        S::RowResize => CursorIcon::RowResize,
        S::EwResize => CursorIcon::EwResize,
        S::NsResize => CursorIcon::NsResize,
        S::NeswResize => CursorIcon::NeswResize,
        S::NwseResize => CursorIcon::NwseResize,
        S::NResize => CursorIcon::NResize,
        S::EResize => CursorIcon::EResize,
        S::SResize => CursorIcon::SResize,
        S::WResize => CursorIcon::WResize,
        S::NeResize => CursorIcon::NeResize,
        S::NwResize => CursorIcon::NwResize,
        S::SeResize => CursorIcon::SeResize,
        S::SwResize => CursorIcon::SwResize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keymap with `groups` (each a list of `(key, keycode, keysym)`
    /// on one level).
    fn keymap(groups: &[&[(&str, u32, &str)]]) -> String {
        let mut codes = Vec::new();
        let mut keys: Vec<(String, Vec<String>)> = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            for (name, code, sym) in *group {
                if !codes.iter().any(|(n, _)| n == name) {
                    codes.push((name.to_string(), *code));
                    keys.push((name.to_string(), Vec::new()));
                }
                let k = keys.iter_mut().find(|(n, _)| n == name).unwrap();
                k.1.push(format!("symbols[Group{}] = [ {sym} ]", g + 1));
            }
        }
        let codes: String = codes
            .iter()
            .map(|(n, c)| format!("<{n}> = {c}; "))
            .collect();
        let keys: String = keys
            .iter()
            .map(|(n, s)| format!("key <{n}> {{ {} }};\n", s.join(", ")))
            .collect();
        format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"t\" {{ minimum = 8; maximum = 255; {codes}}};\n\
             xkb_types \"t\" {{ type \"ONE_LEVEL\" {{ modifiers = none; level_name[Level1] = \"Any\"; }}; }};\n\
             xkb_compatibility \"t\" {{ }};\n\
             xkb_symbols \"t\" {{\n{keys}}};\n}};\n"
        )
    }

    const C: u32 = 46;
    const V: u32 = 47;
    const ONE: u32 = 2;

    fn sym(name: &str) -> Keysym {
        let s = xkbcommon::xkb::keysym_from_name(name, xkbcommon::xkb::KEYSYM_NO_FLAGS);
        assert_ne!(s.raw(), 0, "{name}");
        s
    }

    #[test]
    fn a_non_latin_layouts_keys_match_shortcuts_as_their_latin_keysyms() {
        // `us,ru`: the second layout's keys stand for the first's.
        let us_ru = LatinKeys::from_text(&keymap(&[
            &[
                ("AB03", C + 8, "c"),
                ("AB04", V + 8, "v"),
                ("AE01", ONE + 8, "1"),
            ],
            &[
                ("AB03", C + 8, "Cyrillic_es"),
                ("AB04", V + 8, "Cyrillic_em"),
                ("AE01", ONE + 8, "1"),
            ],
        ]))
        .unwrap();
        assert_eq!(us_ru.shortcut(V, sym("Cyrillic_em")), Some(sym("v")));
        assert_eq!(us_ru.shortcut(C, sym("Cyrillic_es")), Some(sym("c")));
        assert_eq!(
            key_name(us_ru.shortcut(V, sym("Cyrillic_em")).unwrap()),
            "v"
        );
        // A Latin or non-character keysym is itself.
        assert_eq!(us_ru.shortcut(V, sym("v")), None);
        assert_eq!(us_ru.shortcut(ONE, sym("1")), None);
        assert_eq!(us_ru.shortcut(V, sym("Return")), None);
        // A key no layout gives a Latin keysym has none.
        assert_eq!(us_ru.shortcut(99, sym("Cyrillic_em")), None);
        // `ru,us`: the first layout that has one, wherever it is.
        let ru_us = LatinKeys::from_text(&keymap(&[
            &[("AB04", V + 8, "Cyrillic_em")],
            &[("AB04", V + 8, "v")],
        ]))
        .unwrap();
        assert_eq!(ru_us.shortcut(V, sym("Cyrillic_em")), Some(sym("v")));
        // One layout: nothing stands in.
        let ru = LatinKeys::from_text(&keymap(&[&[("AB04", V + 8, "Cyrillic_em")]])).unwrap();
        assert_eq!(ru, LatinKeys::default());
        // A keymap that does not compile, or holds a NUL, is none.
        assert_eq!(LatinKeys::from_text("not a keymap"), None);
        assert_eq!(LatinKeys::from_text("xkb_keymap {\0"), None);
    }

    /// (gap 31) A keymap's layouts by index, named or not; text that is
    /// no keymap names none.
    #[test]
    fn a_keymaps_layouts_are_named_by_index() {
        let keymap = "xkb_keymap {\n\
            xkb_keycodes \"t\" { minimum = 8; maximum = 255; <ESC> = 9; };\n\
            xkb_types \"t\" { type \"ONE_LEVEL\" { modifiers = none; level_name[Level1] = \"Any\"; }; };\n\
            xkb_compatibility \"t\" { };\n\
            xkb_symbols \"t\" { name[Group1] = \"English (US)\"; name[Group2] = \"German\";\n\
              key <ESC> { symbols[Group1] = [ Escape ], symbols[Group2] = [ Escape ] }; };\n\
            };\n";
        assert_eq!(layout_names(keymap), ["English (US)", "German"]);
        let unnamed = keymap
            .replace("name[Group1] = \"English (US)\"; ", "")
            .replace("name[Group2] = \"German\";", "");
        assert_eq!(layout_names(&unnamed), ["", ""]);
        assert!(layout_names("not a keymap").is_empty());
        assert!(layout_names("xkb_keymap {\0").is_empty());
    }

    #[test]
    fn the_repeat_interval_never_busy_loops() {
        assert_eq!(repeat_interval(25), Duration::from_millis(40));
        assert_eq!(repeat_interval(1_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(2_000_000), Duration::from_millis(1));
        assert_eq!(repeat_interval(u32::MAX), Duration::from_millis(1));
        assert_eq!(repeat_interval(0), Duration::from_secs(1));
    }

    #[test]
    fn a_posed_surface_hands_back_content_positions() {
        let surface = SurfaceId(3);
        let mut press = InputEvent::PointerButton {
            surface,
            position: LogicalPoint::new(80.0, 40.0),
            button: 0x110,
            state: ButtonState::Pressed,
            time: 0,
        };
        // At 0.8 the content under destination (80, 40) is at (100, 50).
        unpose(&mut press, (0.8, 0.8));
        let InputEvent::PointerButton { position, .. } = press else {
            unreachable!()
        };
        assert!((position.x - 100.0).abs() < 1e-3 && (position.y - 50.0).abs() < 1e-3);
        let mut key = InputEvent::KeyboardLeave { surface };
        unpose(&mut key, (0.5, 0.5));
        assert_eq!(key, InputEvent::KeyboardLeave { surface });
    }
}
