//! (M5) xdg-activation (`xdg_activation_v1`): a token for the press that
//! started a handler, so the app a notification action or a launch
//! raises gets focus (docs/architecture.md, "Handler input context").
//!
//! [`State::activation_token`] asks the compositor for a token with the
//! press's serial, its seat and the surface it landed on, and hands it
//! to the caller once the compositor answers. Without the protocol the
//! answer is `None` at once; a compositor that never answers is the
//! caller's bound to keep (strand-services waits 200 ms, then goes on
//! without one), and the request itself is given up after
//! `FORGET_AFTER` by a timer of its own. Press serials are tracked per seat
//! ([`State::note_press`]): a pointer button's and a key's.
//!
//! This file is part of the manager (`manager/mod.rs` includes it as
//! `manager::activation`), so it reaches `State`'s fields like the
//! manager's other parts do.

use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

use super::*;

/// How many recent presses are remembered for their seat: a handler
/// asks within a frame or two of its press.
const PRESSES: usize = 8;

/// A token asked for and not answered this long is given up (its caller
/// told `None`, its object destroyed) by a timer armed when it is asked:
/// a compositor that never answers keeps nothing alive for good.
const FORGET_AFTER: Duration = Duration::from_secs(5);

/// Told the token, or `None`.
type TokenDone = Box<dyn FnOnce(Option<String>)>;

/// The manager's xdg-activation state.
#[derive(Default)]
pub(super) struct Activation {
    /// `xdg_activation_v1` (none: no tokens).
    global: Option<XdgActivationV1>,
    /// The last presses on our surfaces: serial and seat, newest last.
    presses: std::collections::VecDeque<(u32, wl_seat::WlSeat)>,
    /// Tokens asked for: their object, the timer that gives it up, and
    /// whom to tell.
    waiting: Vec<(XdgActivationTokenV1, Option<RegistrationToken>, TokenDone)>,
    /// Tokens the compositor gave (tests).
    issued: u64,
}

impl Activation {
    pub(super) fn bind<H: SurfaceHost + 'static>(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<State<H>>,
    ) -> Activation {
        Activation {
            global: globals
                .bind::<XdgActivationV1, _, _>(qh, 1..=1, StrandGlobal)
                .ok(),
            ..Activation::default()
        }
    }
}

/// A token object's user data.
#[derive(Debug)]
pub struct TokenTag;

impl<H: SurfaceHost + 'static> State<H> {
    /// A button or key press with `serial` on `seat`: remembered for a
    /// token asked with it ([`State::activation_token`]), and the host
    /// is told before the press itself (`SurfaceHost::press_serial`).
    pub(super) fn note_press(&mut self, seat: &wl_seat::WlSeat, serial: u32) {
        let p = &mut self.activation.presses;
        if p.len() == PRESSES {
            p.pop_front();
        }
        p.push_back((serial, seat.clone()));
        self.host.press_serial(serial);
    }

    /// Whether the compositor offers `xdg_activation_v1`.
    pub fn activation_supported(&self) -> bool {
        self.activation.global.is_some()
    }

    /// Tokens the compositor has given so far (tests).
    pub fn activation_tokens_issued(&self) -> u64 {
        self.activation.issued
    }

    /// Asks the compositor for an xdg-activation token for the press
    /// `serial` on `surface` (its seat is the one that press came from;
    /// the surface is left out when it is gone, as a popup the click
    /// closed may be), and tells `done` the token when it comes. Without
    /// `xdg_activation_v1`, `done` hears `None` before this returns. A
    /// compositor that never answers: `done` hears `None` after
    /// `FORGET_AFTER`, from a timer armed here.
    pub fn activation_token(
        &mut self,
        surface: SurfaceId,
        serial: u32,
        done: impl FnOnce(Option<String>) + 'static,
    ) {
        let Some(global) = &self.activation.global else {
            done(None);
            return;
        };
        let seat = self
            .activation
            .presses
            .iter()
            .rev()
            .find(|(s, _)| *s == serial)
            .map(|(_, seat)| seat.clone())
            .or_else(|| self.last_action.as_ref().map(|a| a.seat.clone()));
        let token = global.get_activation_token(&self.qh, TokenTag);
        if let Some(seat) = &seat {
            token.set_serial(serial, seat);
        }
        if let Some(s) = self.surfaces.get(&surface) {
            token.set_surface(s.wl());
        }
        token.commit();
        let timer = self
            .handle
            .insert_source(Timer::from_duration(FORGET_AFTER), {
                let token = token.clone();
                move |_, _, state: &mut State<H>| {
                    state.give_up_token(&token);
                    TimeoutAction::Drop
                }
            })
            .map_err(|e| {
                log::warn!(
                    "an activation token cannot be given up in time: {}",
                    e.error
                )
            })
            .ok();
        self.activation.waiting.push((token, timer, Box::new(done)));
    }

    /// `FORGET_AFTER` passed with `token` unanswered: its caller hears
    /// `None` and the object goes.
    fn give_up_token(&mut self, token: &XdgActivationTokenV1) {
        let waiting = &mut self.activation.waiting;
        if let Some(i) = waiting.iter().position(|(t, _, _)| t == token) {
            let (t, _, done) = waiting.swap_remove(i);
            t.destroy();
            done(None);
        }
    }

    /// Tokens asked for and not answered or given up yet (tests).
    pub fn activation_tokens_waiting(&self) -> usize {
        self.activation.waiting.len()
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<XdgActivationV1, State<H>> for StrandGlobal {
    fn event(
        &self,
        _: &mut State<H>,
        _: &XdgActivationV1,
        _: wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
    }
}

impl<H: SurfaceHost + 'static> Dispatch2<XdgActivationTokenV1, State<H>> for TokenTag {
    fn event(
        &self,
        state: &mut State<H>,
        proxy: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &Connection,
        _: &QueueHandle<State<H>>,
    ) {
        let xdg_activation_token_v1::Event::Done { token } = event else {
            return;
        };
        let waiting = &mut state.activation.waiting;
        if let Some(i) = waiting.iter().position(|(t, _, _)| t == proxy) {
            let (t, timer, done) = waiting.swap_remove(i);
            t.destroy();
            if let Some(timer) = timer {
                state.handle.remove(timer);
            }
            state.activation.issued += 1;
            done(Some(token));
        }
    }
}
