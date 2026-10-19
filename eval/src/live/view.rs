//! What the live window shows (docs/eval.md, "Live sandbox"): the
//! control strip on top, the stage below. The stage is the screen as the
//! sandbox lays it out: each shown surface's buffer where the placement
//! model puts it, the frame of the ring being viewed, with the nodes of
//! that frame's failing findings outlined. The strip is a minimal panel
//! in the reports' bitmap font, drawn by the sandbox, not by the renderer
//! under test (docs/decisions.md eval-live).
//!
//! Everything draws into a premultiplied BGRA buffer (`wl_shm`'s
//! ARGB8888), so it can be checked offline as well as shown.

use crate::live::session::{KnobKind, Session, Verdict};
use crate::report::font;

/// The font's scale on the strip: 10 × 14 px glyphs.
const S: u32 = 2;
const ADV: u32 = (font::W + 1) * S;
const LINE: u32 = font::H * S + 8;
const PAD: u32 = 6;

const STRIP: [u8; 3] = [0x1c, 0x1d, 0x24];
const TEXT: [u8; 3] = [0xe6, 0xe6, 0xe6];
const DIM: [u8; 3] = [0x8a, 0x8c, 0x96];
const BUTTON: [u8; 3] = [0x33, 0x35, 0x40];
const ON: [u8; 3] = [0x2f, 0x6d, 0x4a];
const PASS: [u8; 3] = [0x5c, 0xc9, 0x6a];
const FAIL: [u8; 3] = [0xf0, 0x4a, 0x4a];
const WARN: [u8; 3] = [0xf0, 0xb0, 0x40];
const STAGE: [u8; 3] = [0x10, 0x10, 0x14];
const CHECK: [u8; 3] = [0x18, 0x18, 0x1e];
const SCREEN: [u8; 3] = [0x50, 0x52, 0x5c];

/// What a click on the strip does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    PlayPause,
    Back,
    Step,
    Slower,
    Faster,
    Motion,
    Scale,
    Rate,
    Leave,
    Checks,
    Finding,
    Record,
    Restart,
    Knob(usize),
}

/// The buttons of the transport row: key, label, action.
const BUTTONS: [(&str, &str, Action); 13] = [
    ("SPACE", "PLAY/PAUSE", Action::PlayPause),
    (",", "BACK", Action::Back),
    (".", "STEP", Action::Step),
    ("[", "SLOWER", Action::Slower),
    ("]", "FASTER", Action::Faster),
    ("M", "MOTION", Action::Motion),
    ("S", "SCALE", Action::Scale),
    ("H", "HZ", Action::Rate),
    ("L", "LEAVE", Action::Leave),
    ("C", "CHECKS", Action::Checks),
    ("F", "FINDING", Action::Finding),
    ("W", "RECORD", Action::Record),
    ("R", "RESTART", Action::Restart),
];

/// How the stage is viewed and what the strip is typing.
#[derive(Clone, Debug, Default)]
pub struct ViewState {
    /// Window pixels per logical pixel (`None`: fit what the session has
    /// shown).
    pub zoom: Option<f32>,
    /// The stage's top-left on the screen, logical, when zoomed by hand.
    pub pan: [f32; 2],
    /// The step being typed (`:`), when it is.
    pub command: Option<String>,
}

/// Where things landed in the last frame drawn.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub w: u32,
    pub h: u32,
    /// The strip's height; the stage is below it.
    pub strip: u32,
    pub buttons: Vec<([u32; 4], Action)>,
    pub zoom: f32,
    /// The screen point (logical) at the stage's top-left.
    pub origin: [f32; 2],
}

impl Layout {
    /// The strip's action at window point `(x, y)`.
    pub fn action_at(&self, x: f64, y: f64) -> Option<Action> {
        self.buttons.iter().find_map(|(r, a)| {
            let inside = x >= f64::from(r[0])
                && y >= f64::from(r[1])
                && x < f64::from(r[0] + r[2])
                && y < f64::from(r[1] + r[3]);
            inside.then(|| a.clone())
        })
    }

    /// The screen point (logical) at window point `(x, y)`, when it is on
    /// the stage.
    pub fn screen_at(&self, x: f64, y: f64) -> Option<[f32; 2]> {
        if y < f64::from(self.strip) || self.zoom <= 0.0 {
            return None;
        }
        let lx = self.origin[0] + (x as f32) / self.zoom;
        let ly = self.origin[1] + (y as f32 - self.strip as f32) / self.zoom;
        Some([lx, ly])
    }
}

/// A premultiplied BGRA buffer.
struct Canvas<'a> {
    pub px: &'a mut [u8],
    pub w: u32,
    pub h: u32,
}

impl Canvas<'_> {
    fn fill(&mut self, r: [i64; 4], c: [u8; 3]) {
        let x0 = r[0].clamp(0, i64::from(self.w)) as usize;
        let y0 = r[1].clamp(0, i64::from(self.h)) as usize;
        let x1 = (r[0] + r[2]).clamp(0, i64::from(self.w)) as usize;
        let y1 = (r[1] + r[3]).clamp(0, i64::from(self.h)) as usize;
        let px = [c[2], c[1], c[0], 0xff];
        for y in y0..y1 {
            let row = y * self.w as usize * 4;
            for x in x0..x1 {
                let at = row + x * 4;
                self.px[at..at + 4].copy_from_slice(&px);
            }
        }
    }

    fn outline(&mut self, r: [i64; 4], c: [u8; 3], t: i64) {
        self.fill([r[0], r[1], r[2], t], c);
        self.fill([r[0], r[1] + r[3] - t, r[2], t], c);
        self.fill([r[0], r[1], t, r[3]], c);
        self.fill([r[0] + r[2] - t, r[1], t, r[3]], c);
    }

    /// `text` at `(x, y)`, clipped to `max_w`; the width drawn.
    fn text(&mut self, x: u32, y: u32, text: &str, c: [u8; 3], max_w: u32) -> u32 {
        let mut gx = x;
        for ch in text.chars() {
            if gx + ADV > x + max_w {
                break;
            }
            let bits = font::glyph(ch);
            for (col, b) in bits.iter().enumerate() {
                for row in 0..font::H {
                    if b & (1 << row) != 0 {
                        self.fill(
                            [
                                i64::from(gx + col as u32 * S),
                                i64::from(y + row * S),
                                i64::from(S),
                                i64::from(S),
                            ],
                            c,
                        );
                    }
                }
            }
            gx += ADV;
        }
        gx - x
    }
}

fn width(text: &str) -> u32 {
    text.chars().count() as u32 * ADV
}

/// A row of labelled boxes, wrapping at the strip's width.
struct Flow {
    x: u32,
    y: u32,
    w: u32,
}

impl Flow {
    fn place(&mut self, label_w: u32) -> (u32, u32) {
        let bw = label_w + 2 * PAD;
        if self.x > PAD && self.x + bw > self.w.saturating_sub(PAD) {
            self.x = PAD;
            self.y += LINE + 2;
        }
        let at = (self.x, self.y);
        self.x += bw + 4;
        at
    }
}

fn fmt_speed(s: f64) -> String {
    let t = format!("{s:.2}");
    let t = t.trim_end_matches('0').trim_end_matches('.');
    format!("{t}X")
}

/// The strip's rows, laid out and drawn (`draw` false: laid out only),
/// returning its height.
fn strip(c: &mut Canvas<'_>, s: &Session, v: &ViewState, lay: &mut Layout, draw: bool) -> u32 {
    let w = c.w;
    let mut y = PAD;
    // Status.
    let d = s.driver();
    let head_k = d.k().saturating_sub(1);
    let shown = s.shown();
    let view = match (s.at_head(), shown) {
        (true, _) => "HEAD".to_string(),
        (false, Some(f)) => format!("VIEW K{} OF {}", f.k, head_k),
        (false, None) => "VIEW ?".to_string(),
    };
    let t = match (s.at_head(), shown) {
        (false, Some(f)) => f.t,
        _ => s.head_time(),
    };
    let status = format!(
        "{} {}  K{}  T {:.3}S  {}  {}HZ  SCALE {}  MOTION {}  LOG {}  QUEUE {}  RELOADS {}  {}",
        if s.paused { "PAUSED" } else { "PLAYING" },
        fmt_speed(s.speed),
        head_k,
        t.as_secs_f64(),
        view,
        d.hz(),
        d.surfaces().screen.scale,
        if d.reduced() { "REDUCED" } else { "FULL" },
        s.log().len(),
        s.queued(),
        s.reloads,
        s.opts
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
    );
    if draw {
        c.text(
            PAD,
            y + 3,
            &status,
            if s.paused { WARN } else { PASS },
            w - 2 * PAD,
        );
    }
    y += LINE;
    // Transport.
    let mut flow = Flow { x: PAD, y, w };
    for (key, label, action) in BUTTONS.iter() {
        let text = format!("{key} {label}");
        let tw = width(&text);
        let (bx, by) = flow.place(tw);
        let r = [bx, by, tw + 2 * PAD, LINE];
        let lit = match action {
            Action::PlayPause => !s.paused,
            Action::Motion => d.reduced(),
            _ => false,
        };
        if draw {
            c.fill(
                [bx.into(), by.into(), r[2].into(), r[3].into()],
                if lit { ON } else { BUTTON },
            );
            c.text(bx + PAD, by + 4, &text, TEXT, tw);
        }
        lay.buttons.push((r, action.clone()));
    }
    y = flow.y + LINE + 4;
    // Knobs.
    let mut flow = Flow { x: PAD, y, w };
    if s.knobs().is_empty() && draw {
        c.text(
            PAD,
            y + 4,
            "NO KNOBS: THE CONFIG DECLARES NO STATE, HOVER OR INPUT",
            DIM,
            w,
        );
    }
    let places = s.analysis().map(|a| &a.places);
    for (i, k) in s.knobs().iter().enumerate() {
        let key = if i < 9 {
            format!("{} ", i + 1)
        } else {
            String::new()
        };
        let mut label = k.label.clone();
        if let KnobKind::Toggle(p) | KnobKind::Cycle(p, _) = &k.kind
            && let Some(v) = d.value_text(p)
        {
            label = format!("{label} = {v}");
        }
        let on_screen = match &k.kind {
            KnobKind::Hover(o) | KnobKind::Input(o, _) => {
                let spot = places.and_then(|p| p.get(o));
                if let Some(sp) = spot {
                    label = label.replace(o.as_str(), &sp.label);
                }
                spot.is_some()
            }
            _ => true,
        };
        let text = format!("{key}{label}");
        let tw = width(&text).min(w.saturating_sub(4 * PAD));
        let (bx, by) = flow.place(tw);
        let r = [bx, by, tw + 2 * PAD, LINE];
        if draw {
            c.fill([bx.into(), by.into(), r[2].into(), r[3].into()], BUTTON);
            c.text(
                bx + PAD,
                by + 4,
                &text,
                if on_screen { TEXT } else { DIM },
                tw,
            );
        }
        lay.buttons.push((r, Action::Knob(i)));
    }
    y = flow.y + LINE + 4;
    // Verdicts.
    let mut x = PAD;
    let head = match s.analysis() {
        Some(a) => format!(
            "CHECKS AT K{}{}:",
            a.k.saturating_sub(1),
            if s.stale() { "*" } else { "" }
        ),
        None => "CHECKS: C RUNS THEM".to_string(),
    };
    if draw {
        x += c.text(x, y + 3, &head, DIM, w - x) + ADV;
    }
    // Failures first: they stay on the strip however narrow it is.
    let mut verdicts = s.verdicts();
    verdicts.sort_by_key(|(_, v)| !matches!(v, Verdict::Fail(_)));
    for (name, verdict) in verdicts {
        let (t, col) = match verdict {
            Verdict::Pass => (format!("{name} OK"), PASS),
            Verdict::Fail(n) => (format!("{name} FAIL {n}"), FAIL),
            Verdict::Xfail => (format!("{name} XFAIL"), WARN),
        };
        if draw && x + width(&t) < w {
            x += c.text(x, y + 3, &t, col, w - x) + ADV;
        }
    }
    y += LINE;
    // Findings at this frame, else the last message.
    let marks = s.marks();
    let (line, col) = match (marks.first(), &s.msg) {
        (Some(m), _) => (
            format!("{} AT THIS FRAME: {}: {}", marks.len(), m.check, m.msg),
            FAIL,
        ),
        (None, Some((m, err))) => (m.clone(), if *err { FAIL } else { TEXT }),
        (None, None) => (
            "HOVER AND CLICK THE STAGE, : TYPES A STEP, 1-9 RUN KNOBS, Q QUITS".into(),
            DIM,
        ),
    };
    if draw {
        c.text(PAD, y + 3, &line, col, w - 2 * PAD);
    }
    y += LINE;
    if let Some(cmd) = &v.command {
        if draw {
            c.fill([0, y.into(), w.into(), LINE.into()], BUTTON);
            c.text(PAD, y + 3, &format!(": {cmd}_"), TEXT, w - 2 * PAD);
        }
        y += LINE;
    }
    y + PAD
}

/// Draws the window: the strip, then the stage at the frame viewed.
pub fn draw(s: &Session, v: &ViewState, px: &mut [u8], w: u32, h: u32) -> Layout {
    let mut c = Canvas { px, w, h };
    let mut lay = Layout {
        w,
        h,
        ..Layout::default()
    };
    // Lay the strip out first for its height.
    let sh = strip(&mut c, s, v, &mut lay, false).min(h);
    lay.buttons.clear();
    lay.strip = sh;
    // The stage: a checkerboard behind the screen's area.
    let stage_h = h.saturating_sub(sh);
    let screen = s.driver().surfaces().screen;
    let (zoom, origin) = match v.zoom {
        Some(z) => (z, v.pan),
        None => fit(
            s.seen,
            [screen.width as f32, screen.height as f32],
            w,
            stage_h,
        ),
    };
    lay.zoom = zoom;
    lay.origin = origin;
    for ty in (0..stage_h).step_by(8) {
        for tx in (0..w).step_by(8) {
            let col = if (tx / 8 + ty / 8) % 2 == 0 {
                STAGE
            } else {
                CHECK
            };
            c.fill([tx.into(), (sh + ty).into(), 8, 8], col);
        }
    }
    let to_win = |p: [f32; 2]| -> [i64; 2] {
        [
            ((p[0] - origin[0]) * zoom).round() as i64,
            ((p[1] - origin[1]) * zoom).round() as i64 + i64::from(sh),
        ]
    };
    let tl = to_win([0.0, 0.0]);
    let br = to_win([screen.width as f32, screen.height as f32]);
    c.outline(
        [tl[0] - 1, tl[1] - 1, br[0] - tl[0] + 2, br[1] - tl[1] + 2],
        SCREEN,
        1,
    );
    if let Some(shot) = s.shown() {
        for sf in &shot.surfaces {
            blit(&mut c, sf, origin, zoom, sh);
        }
    }
    for m in s.marks() {
        if m.rect[2] <= 0.0 && m.rect[3] <= 0.0 {
            continue;
        }
        let a = to_win([m.rect[0], m.rect[1]]);
        let b = to_win([m.rect[0] + m.rect[2], m.rect[1] + m.rect[3]]);
        c.outline(
            [a[0] - 2, a[1] - 2, b[0] - a[0] + 4, b[1] - a[1] + 4],
            FAIL,
            2,
        );
    }
    // The strip last, over anything drawn past the stage's top.
    c.fill([0, 0, w.into(), sh.into()], STRIP);
    strip(&mut c, s, v, &mut lay, true);
    lay
}

/// The zoom and top-left that fit `seen` (else the screen) in the
/// stage: whole zooms up to 4 when it is smaller, a fraction when not.
fn fit(seen: Option<[f32; 4]>, screen: [f32; 2], w: u32, h: u32) -> (f32, [f32; 2]) {
    let b = seen.unwrap_or([0.0, 0.0, screen[0], screen[1]]);
    let margin = 16.0;
    let fw = (w as f32 - 2.0 * margin).max(1.0) / b[2].max(1.0);
    let fh = (h as f32 - 2.0 * margin).max(1.0) / b[3].max(1.0);
    let f = fw.min(fh);
    let z = if f >= 1.0 { f.floor().min(4.0) } else { f };
    (z, [b[0] - margin / z, b[1] - margin / z])
}

/// One surface's buffer onto the stage, nearest-sampled, blended over
/// what is there (premultiplied).
fn blit(
    c: &mut Canvas<'_>,
    sf: &crate::live::session::ShotSurface,
    origin: [f32; 2],
    zoom: f32,
    top: u32,
) {
    let (bw, bh) = (sf.size.w as usize, sf.size.h as usize);
    if bw == 0 || bh == 0 || sf.pixels.len() < bw * bh * 4 {
        return;
    }
    let sx = sf.size.w as f32 / sf.logical[0].max(1.0);
    let sy = sf.size.h as f32 / sf.logical[1].max(1.0);
    let x0 = ((sf.origin[0] - origin[0]) * zoom).floor().max(0.0) as u32;
    let y0 = (((sf.origin[1] - origin[1]) * zoom).floor().max(0.0) as u32).saturating_add(top);
    let x1 = (((sf.origin[0] + sf.logical[0] - origin[0]) * zoom)
        .ceil()
        .max(0.0) as u32)
        .min(c.w);
    let y1 = ((((sf.origin[1] + sf.logical[1] - origin[1]) * zoom)
        .ceil()
        .max(0.0) as u32)
        .saturating_add(top))
    .min(c.h);
    for wy in y0.max(top)..y1 {
        let ly = origin[1] + (wy - top) as f32 / zoom - sf.origin[1];
        let by = (ly * sy).floor();
        if by < 0.0 || by as usize >= bh {
            continue;
        }
        let srow = by as usize * bw * 4;
        let drow = wy as usize * c.w as usize * 4;
        for wx in x0..x1 {
            let lx = origin[0] + wx as f32 / zoom - sf.origin[0];
            let bx = (lx * sx).floor();
            if bx < 0.0 || bx as usize >= bw {
                continue;
            }
            let s = srow + bx as usize * 4;
            let a = u32::from(sf.pixels[s + 3]);
            if a == 0 {
                continue;
            }
            let d = drow + wx as usize * 4;
            for i in 0..3 {
                let src = u32::from(sf.pixels[s + i]);
                let dst = u32::from(c.px[d + i]);
                c.px[d + i] = (src + (dst * (255 - a) + 127) / 255).min(255) as u8;
            }
            c.px[d + 3] = 0xff;
        }
    }
}
