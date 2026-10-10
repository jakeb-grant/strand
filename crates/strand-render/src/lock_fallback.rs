//! The built-in fallback lock (design.md, "Lock screen": "The lock is
//! exempt from reload and fails closed: if anything faults, a built-in
//! password field appears"; docs/architecture.md, "The lock").
//!
//! The binary (`run/lock.rs`) shows it on the session lock's content
//! surface when the config's lock cannot be trusted to come up or to
//! unlock: logic ended, panicked or hung, the lock's component froze, no
//! lock is mounted, the text worker is gone, `auth` is unreachable, or
//! the lock painted no first frame in time. Keys on that surface then come
//! here instead of the `Router`.
//!
//! It needs nothing that can have faulted: no text worker, no layout, no
//! vello context, no allocation per frame. A frame is a dark fill and a
//! rounded field in the middle of the surface, drawn pixel by pixel from
//! signed distances (antialiased, at any scale), holding one dot per
//! character typed; the field's border says idle, checking or refused.
//!
//! The typed bytes live in one buffer sized once to PAM's limit and wiped
//! (volatile writes) whenever they are cleared or handed out and when the
//! fallback is dropped, so no reallocation leaves a copy behind. A
//! password longer than PAM takes is refused, never cut
//! (decisions.md, m4-lock-w1).

use strand_scene::{ButtonState, Color, Damage, KeyInput, PaintTarget};

/// The most bytes a password may have: PAM's response limit less its
/// NUL (`strand_auth::protocol::MAX_PASSWORD`; render does not depend on
/// strand-auth). Typing more marks the input too long, and submitting it
/// is refused.
pub const MAX_PASSWORD: usize = 511;

/// Dots shown at most; more characters keep the field full.
pub const MAX_DOTS: usize = 24;

/// The field's state, shown by its border (and a tint when refused).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum FieldState {
    #[default]
    Idle,
    /// A password is being checked.
    Checking,
    /// The last password was refused or could not be checked.
    Failed,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing changed.
    None,
    /// The field changed: repaint.
    Changed,
    /// Return: check these bytes (taken out of the field, which is now
    /// empty). The caller wipes them once sent (`strand_auth::Password`).
    Submit(Vec<u8>),
}

/// The fallback lock's state: the typed password and the field's look.
pub struct LockFallback {
    secret: Vec<u8>,
    /// More was typed than [`MAX_PASSWORD`] holds.
    too_long: bool,
    state: FieldState,
}

impl std::fmt::Debug for LockFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password, nor its length.
        f.debug_struct("LockFallback")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for LockFallback {
    fn default() -> Self {
        Self::new()
    }
}

/// Zeroes `bytes` in a way the compiler keeps.
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, exclusive reference to a byte.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl LockFallback {
    pub fn new() -> Self {
        Self {
            // Sized once: pushes never reallocate (and leave no copy).
            secret: Vec::with_capacity(MAX_PASSWORD + 4),
            too_long: false,
            state: FieldState::Idle,
        }
    }

    pub fn state(&self) -> FieldState {
        self.state
    }

    /// Sets the field's state (the binary: checking while its client
    /// runs, failed on a refusal). True when it changed.
    pub fn set_state(&mut self, state: FieldState) -> bool {
        std::mem::replace(&mut self.state, state) != state
    }

    /// Characters typed (what the dots count).
    pub fn chars(&self) -> usize {
        if self.too_long {
            return MAX_DOTS;
        }
        self.secret.iter().filter(|b| (**b & 0xc0) != 0x80).count()
    }

    fn clear(&mut self) -> bool {
        let had = !self.secret.is_empty() || self.too_long;
        wipe(&mut self.secret);
        self.secret.clear();
        self.too_long = false;
        had
    }

    /// A key on the lock surface. Return submits (unless a check runs or
    /// nothing is typed: a Return to wake the screen is not a password
    /// attempt, and PAM would count it against `pam_faillock`),
    /// BackSpace deletes a character, Escape and Ctrl+U clear, and a key
    /// that types text without Ctrl, Alt or Logo adds it. Typing after a
    /// refusal puts the field back to idle.
    pub fn key(&mut self, key: &KeyInput) -> Action {
        if key.state != ButtonState::Pressed {
            return Action::None;
        }
        let m = key.modifiers;
        match key.name.as_str() {
            "Return" | "KP_Enter" => {
                if self.state == FieldState::Checking {
                    return Action::None;
                }
                if self.secret.is_empty() && !self.too_long {
                    return Action::None;
                }
                if self.too_long {
                    // Refused, never cut to a prefix.
                    self.clear();
                    self.state = FieldState::Failed;
                    return Action::Changed;
                }
                self.state = FieldState::Checking;
                let mut out = Vec::with_capacity(MAX_PASSWORD + 4);
                out.extend_from_slice(&self.secret);
                self.clear();
                return Action::Submit(out);
            }
            "BackSpace" => {
                if self.too_long {
                    let had = self.clear();
                    return self.changed(had);
                }
                let mut changed = false;
                while let Some(b) = self.secret.pop() {
                    changed = true;
                    // Wipe the byte left in the spare capacity.
                    let len = self.secret.len();
                    // SAFETY: `len` < capacity: the byte just popped.
                    unsafe { std::ptr::write_volatile(self.secret.as_mut_ptr().add(len), 0) };
                    if (b & 0xc0) != 0x80 {
                        break;
                    }
                }
                return self.changed(changed);
            }
            "Escape" => {
                let had = self.clear();
                return self.changed(had);
            }
            "u" | "U" if m.ctrl => {
                let had = self.clear();
                return self.changed(had);
            }
            _ => {}
        }
        if m.ctrl
            || m.alt
            || m.logo
            || key.text.is_empty()
            || key.text.chars().any(char::is_control)
        {
            return Action::None;
        }
        if self.secret.len() + key.text.len() > MAX_PASSWORD {
            self.too_long = true;
        } else {
            self.secret.extend_from_slice(key.text.as_bytes());
        }
        if self.state == FieldState::Failed {
            self.state = FieldState::Idle;
        }
        Action::Changed
    }

    fn changed(&mut self, changed: bool) -> Action {
        if !changed {
            return Action::None;
        }
        if self.state == FieldState::Failed {
            self.state = FieldState::Idle;
        }
        Action::Changed
    }

    /// Paints the whole frame into `target` and returns its damage (all
    /// of it).
    pub fn paint(&self, target: &mut PaintTarget<'_>) -> Damage {
        paint(target, self.chars(), self.state)
    }
}

impl Drop for LockFallback {
    fn drop(&mut self) {
        self.clear();
    }
}

/// An opaque colour from its sRGB bytes.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
}

/// The fallback's colours (all opaque: the lock hides the desktop).
const BACKGROUND: Color = rgb(0x11, 0x11, 0x1b);
const FIELD: Color = rgb(0x1e, 0x1e, 0x2e);
const FIELD_FAILED: Color = rgb(0x3b, 0x20, 0x2c);
const BORDER_IDLE: Color = rgb(0x58, 0x5b, 0x70);
const BORDER_CHECKING: Color = rgb(0x89, 0xb4, 0xfa);
const BORDER_FAILED: Color = rgb(0xf3, 0x8b, 0xa8);
const DOT: Color = rgb(0xcd, 0xd6, 0xf4);

/// The field's size, border, dot radius and dot pitch in logical pixels.
const FIELD_W: f32 = 320.0;
const FIELD_H: f32 = 48.0;
const BORDER: f32 = 2.0;
const DOT_R: f32 = 5.0;
const DOT_PITCH: f32 = 12.0;

/// Paints the fallback with `dots` characters typed in `state` into
/// `target`: the background over every pixel, the field centred.
pub fn paint(target: &mut PaintTarget<'_>, dots: usize, state: FieldState) -> Damage {
    let (w, h) = (target.size.w as usize, target.size.h as usize);
    let stride = target.stride as usize;
    let s = target.scale.as_f32();
    let bg = bgra(BACKGROUND);
    for y in 0..h {
        let row = &mut target.pixels[y * stride..y * stride + w * 4];
        for px in row.chunks_exact_mut(4) {
            px.copy_from_slice(&bg);
        }
    }
    let (fw, fh) = ((FIELD_W * s).min(w as f32 - 2.0), FIELD_H * s);
    if fw <= 0.0 || fh <= 0.0 {
        return Damage::full(target.size);
    }
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let radius = fh / 2.0;
    let border = (BORDER * s).max(1.0);
    let (fill, edge) = match state {
        FieldState::Idle => (FIELD, BORDER_IDLE),
        FieldState::Checking => (FIELD, BORDER_CHECKING),
        FieldState::Failed => (FIELD_FAILED, BORDER_FAILED),
    };
    let n = dots.min(MAX_DOTS);
    let pitch = DOT_PITCH * s * 1.5;
    let first = cx - pitch * (n as f32 - 1.0) / 2.0;
    let dot_r = DOT_R * s;
    let x0 = ((cx - fw / 2.0).floor().max(0.0)) as usize;
    let x1 = ((cx + fw / 2.0).ceil() as usize).min(w);
    let y0 = ((cy - fh / 2.0).floor().max(0.0)) as usize;
    let y1 = ((cy + fh / 2.0).ceil() as usize).min(h);
    for y in y0..y1 {
        let py = y as f32 + 0.5 - cy;
        for x in x0..x1 {
            let px = x as f32 + 0.5 - cx;
            let outer = coverage(round_rect(px, py, fw / 2.0, fh / 2.0, radius));
            if outer <= 0.0 {
                continue;
            }
            let inner = coverage(round_rect(
                px,
                py,
                fw / 2.0 - border,
                fh / 2.0 - border,
                (radius - border).max(0.0),
            ));
            let i = y * stride + x * 4;
            let p = &mut target.pixels[i..i + 4];
            over(p, edge, outer);
            over(p, fill, inner);
            if n > 0 {
                // The nearest dot only: they never overlap.
                let k = ((x as f32 + 0.5 - first) / pitch)
                    .round()
                    .clamp(0.0, n as f32 - 1.0);
                let dx = x as f32 + 0.5 - (first + k * pitch);
                let d = (dx * dx + py * py).sqrt() - dot_r;
                over(p, DOT, coverage(d) * inner);
            }
        }
    }
    Damage::full(target.size)
}

/// The signed distance from (`x`, `y`) to a rounded rectangle centred at
/// the origin with half-size (`hw`, `hh`) and corner radius `r`.
fn round_rect(x: f32, y: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let r = r.min(hw).min(hh).max(0.0);
    let qx = x.abs() - (hw - r);
    let qy = y.abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// A pixel's coverage at signed distance `d` (negative inside).
fn coverage(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// `c` as wl_shm ARGB8888 bytes (B, G, R, A), premultiplied.
fn bgra(c: Color) -> [u8; 4] {
    let [a, r, g, b] = argb(c);
    [b, g, r, a]
}

fn argb(c: Color) -> [u8; 4] {
    let c = c.clamped();
    let q = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    [q(c.a), q(c.r * c.a), q(c.g * c.a), q(c.b * c.a)]
}

/// Composites `c` at coverage `k` over pixel `p` (premultiplied BGRA).
fn over(p: &mut [u8], c: Color, k: f32) {
    if k <= 0.0 {
        return;
    }
    let src = bgra(c);
    let a = f32::from(src[3]) / 255.0 * k;
    for i in 0..4 {
        let s = f32::from(src[i]) * k;
        let d = f32::from(p[i]);
        p[i] = (s + d * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_scene::Modifiers;

    fn press(name: &str, text: &str) -> KeyInput {
        KeyInput {
            name: name.into(),
            text: text.into(),
            state: ButtonState::Pressed,
            repeat: false,
            modifiers: Modifiers::default(),
            time: 0,
        }
    }

    #[test]
    fn typing_counts_characters_and_return_hands_the_bytes_out() {
        let mut f = LockFallback::new();
        for c in ["p", "é", "w"] {
            assert_eq!(f.key(&press(c, c)), Action::Changed);
        }
        assert_eq!(f.chars(), 3);
        assert_eq!(f.key(&press("BackSpace", "")), Action::Changed);
        assert_eq!(f.chars(), 2, "BackSpace takes the whole é");
        match f.key(&press("Return", "\r")) {
            Action::Submit(b) => assert_eq!(b, "pé".as_bytes()),
            a => panic!("{a:?}"),
        }
        assert_eq!(f.chars(), 0);
        assert_eq!(f.state(), FieldState::Checking);
        assert_eq!(
            f.key(&press("Return", "\r")),
            Action::None,
            "one check at a time"
        );
        f.set_state(FieldState::Failed);
        assert_eq!(f.key(&press("a", "a")), Action::Changed);
        assert_eq!(
            f.state(),
            FieldState::Idle,
            "typing again clears the refusal"
        );
    }

    /// (m4-audit) Return on an empty field checks nothing: no empty
    /// password reaches PAM (where `pam_faillock` would count it), and the
    /// field stays as it was.
    #[test]
    fn return_on_an_empty_field_submits_nothing() {
        let mut f = LockFallback::new();
        assert_eq!(f.key(&press("Return", "\r")), Action::None);
        assert_eq!(f.key(&press("KP_Enter", "\r")), Action::None);
        assert_eq!(f.state(), FieldState::Idle);
        f.set_state(FieldState::Failed);
        assert_eq!(f.key(&press("Return", "\r")), Action::None);
        assert_eq!(f.state(), FieldState::Failed);
        f.key(&press("x", "x"));
        f.key(&press("BackSpace", ""));
        assert_eq!(
            f.key(&press("Return", "\r")),
            Action::None,
            "typed, then erased"
        );
        f.key(&press("x", "x"));
        assert!(matches!(f.key(&press("Return", "\r")), Action::Submit(b) if b == b"x"));
    }

    #[test]
    fn control_keys_and_releases_type_nothing() {
        let mut f = LockFallback::new();
        let mut k = press("a", "a");
        k.state = ButtonState::Released;
        assert_eq!(f.key(&k), Action::None);
        let mut k = press("c", "c");
        k.modifiers.ctrl = true;
        assert_eq!(f.key(&k), Action::None);
        assert_eq!(f.key(&press("Tab", "\t")), Action::None);
        assert_eq!(f.key(&press("Shift_L", "")), Action::None);
        f.key(&press("x", "x"));
        let mut k = press("u", "u");
        k.modifiers.ctrl = true;
        assert_eq!(f.key(&k), Action::Changed);
        assert_eq!(f.chars(), 0);
    }

    /// More than PAM takes is refused on Return, never cut to a prefix.
    #[test]
    fn an_overlong_password_is_refused_not_cut() {
        let mut f = LockFallback::new();
        for _ in 0..MAX_PASSWORD {
            f.key(&press("a", "a"));
        }
        assert_eq!(f.chars(), MAX_PASSWORD);
        f.key(&press("a", "a"));
        assert_eq!(f.chars(), MAX_DOTS, "too long");
        assert_eq!(f.key(&press("Return", "\r")), Action::Changed);
        assert_eq!(f.state(), FieldState::Failed);
        assert_eq!(f.chars(), 0);
        // Exactly the limit is submitted whole.
        for _ in 0..MAX_PASSWORD {
            f.key(&press("a", "a"));
        }
        match f.key(&press("Return", "\r")) {
            Action::Submit(b) => assert_eq!(b.len(), MAX_PASSWORD),
            a => panic!("{a:?}"),
        }
    }

    /// The buffer never reallocates, and cleared bytes are zero in its
    /// spare capacity.
    #[test]
    fn cleared_bytes_are_wiped_in_place() {
        let mut f = LockFallback::new();
        let ptr = f.secret.as_ptr();
        for c in "secret".chars() {
            let s = c.to_string();
            f.key(&press(&s, &s));
        }
        f.key(&press("BackSpace", ""));
        f.key(&press("Escape", ""));
        assert_eq!(f.secret.as_ptr(), ptr, "no reallocation");
        // SAFETY: within the capacity the vector allocated.
        let spare = unsafe { std::slice::from_raw_parts(ptr, 6) };
        assert_eq!(spare, [0; 6]);
        assert!(!format!("{f:?}").contains("secret"));
    }
}
