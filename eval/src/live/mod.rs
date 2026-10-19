//! `strand-eval live <file>` (docs/eval.md, "Live sandbox";
//! docs/decisions.md eval-live): a config (or a `.scn`) in a window, run
//! by the sandbox's driver on its virtual clock at a chosen speed, to
//! build and judge animation and effect interactions by eye.
//!
//! - [`session`]: the session, a scenario written as it runs (steps,
//!   the clock between them, the ring of frames to step back through,
//!   reloads, the checks' verdicts, the recording);
//! - [`view`]: the control strip and the stage, drawn into a buffer;
//! - [`window`]: the Wayland client that shows them and takes the
//!   owner's keys and pointer;
//! - [`Live`]: the commands every input comes down to, also read one per
//!   line from stdin (how its tests drive it).
//!
//! It is the eval workspace's own: nothing of it is in the shipped
//! binary, and its renderer's trace hooks stay behind `eval-hooks`.

pub mod session;
pub mod view;
pub mod window;

use std::path::PathBuf;

use serde_json::{Value as Json, json};

use crate::Error;
use crate::scenario::{Fonts, Screen};
use session::{Options, Session};
use view::{Action, ViewState};

/// A key, as the window hands it over.
#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    Char(char),
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Escape,
    Return,
    Backspace,
    Reload,
}

/// The session and how the window views it.
#[derive(Debug)]
pub struct Live {
    pub session: Session,
    pub view: ViewState,
    /// Where the window drew things last (`status` reports the stage).
    pub layout: view::Layout,
    /// The window has the keyboard.
    pub focused: bool,
    pub quit: bool,
}

impl Live {
    pub fn new(session: Session) -> Live {
        Live {
            session,
            view: ViewState::default(),
            layout: view::Layout::default(),
            focused: false,
            quit: false,
        }
    }

    /// An error as the strip shows it.
    fn show(&mut self, r: Result<(), Error>) {
        match r {
            Ok(()) => {}
            Err(e) => self.session.msg = Some((e.to_string(), true)),
        }
    }

    /// A key pressed (`shift` held).
    pub fn key(&mut self, key: Key, shift: bool) {
        if let Some(cmd) = self.view.command.as_mut() {
            match key {
                Key::Escape => self.view.command = None,
                Key::Return => {
                    let text = std::mem::take(cmd);
                    self.view.command = None;
                    let r = self.session.act(&text);
                    self.show(r);
                }
                Key::Backspace => {
                    cmd.pop();
                }
                Key::Space => cmd.push(' '),
                Key::Char(c) => cmd.push(c),
                _ => {}
            }
            return;
        }
        let pan = |l: &mut Live, dx: f32, dy: f32| {
            let z = l.view.zoom.unwrap_or(1.0);
            l.view.zoom = Some(z);
            l.view.pan[0] += dx * 64.0 / z;
            l.view.pan[1] += dy * 64.0 / z;
        };
        match key {
            Key::Left if shift => pan(self, -1.0, 0.0),
            Key::Right if shift => pan(self, 1.0, 0.0),
            Key::Up if shift => pan(self, 0.0, -1.0),
            Key::Down if shift => pan(self, 0.0, 1.0),
            Key::Space => self.action(Action::PlayPause),
            Key::Right | Key::Char('.') => self.action(Action::Step),
            Key::Left | Key::Char(',') => self.action(Action::Back),
            Key::End => self.session.head(),
            Key::Home => self.session.back(u32::MAX),
            Key::Char('[') => self.action(Action::Slower),
            Key::Char(']') => self.action(Action::Faster),
            Key::Char('m') => self.action(Action::Motion),
            Key::Char('s') => self.action(Action::Scale),
            Key::Char('h') => self.action(Action::Rate),
            Key::Char('l') => self.action(Action::Leave),
            Key::Char('c') => self.action(Action::Checks),
            Key::Char('f') => self.action(Action::Finding),
            Key::Char('w') => self.action(Action::Record),
            Key::Char('r') => self.action(Action::Restart),
            Key::Reload => {
                let r = self.session.reload();
                self.show(r);
            }
            Key::Char(':') => self.view.command = Some(String::new()),
            Key::Char(c @ '1'..='9') => self.action(Action::Knob(c as usize - '1' as usize)),
            Key::Char('-') => {
                let z = self.view.zoom.unwrap_or(1.0);
                self.zoom_to(z / 2.0);
            }
            Key::Char('=' | '+') => {
                let z = self.view.zoom.unwrap_or(1.0);
                self.zoom_to(z * 2.0);
            }
            Key::Char('0') => self.view.zoom = None,
            Key::Char('q') | Key::Escape => self.quit = true,
            _ => {}
        }
    }

    fn zoom_to(&mut self, z: f32) {
        let z = z.clamp(0.125, 16.0);
        if self.view.zoom.is_none() {
            self.view.pan = self
                .session
                .seen
                .map_or([0.0, 0.0], |b| [b[0] - 8.0, b[1] - 8.0]);
        }
        self.view.zoom = Some(z);
    }

    /// A strip button (or its key).
    pub fn action(&mut self, a: Action) {
        let r = match a {
            Action::PlayPause => {
                self.session.paused = !self.session.paused;
                if !self.session.paused {
                    self.session.head();
                }
                Ok(())
            }
            Action::Step => self.session.step(1),
            Action::Back => {
                self.session.back(1);
                Ok(())
            }
            Action::Slower => {
                self.session.slower();
                Ok(())
            }
            Action::Faster => {
                self.session.faster();
                Ok(())
            }
            Action::Motion => self.session.toggle_motion(),
            Action::Scale => self.session.next_scale(),
            Action::Rate => self.session.next_rate(),
            Action::Leave => self.session.act("leave"),
            Action::Checks => {
                self.session.analyse();
                Ok(())
            }
            Action::Finding => {
                if !self.session.next_finding() {
                    self.session.msg = Some(("no failing frame to show".into(), false));
                }
                Ok(())
            }
            Action::Record => self.session.record(None).map(|r| {
                self.session.msg = Some((
                    format!(
                        "recorded {} ({} steps); check it: scripts/container/run.sh eval {} {}",
                        r.path.display(),
                        r.steps,
                        if crate::scenario::place_of(&r.path) == crate::scenario::Place::Local {
                            "local"
                        } else {
                            "check"
                        },
                        r.path.display()
                    ),
                    false,
                ));
            }),
            Action::Restart => self.session.restart(),
            Action::Knob(i) => match self.session.knobs().get(i).cloned() {
                Some(k) => match self.session.knob_step(&k) {
                    Ok(session::KnobStep::Prefill(t)) => {
                        self.view.command = Some(t);
                        Ok(())
                    }
                    Ok(session::KnobStep::Steps(steps)) => {
                        steps.iter().try_for_each(|s| self.session.act(s))
                    }
                    Err(e) => Err(e),
                },
                None => Ok(()),
            },
        };
        self.show(r);
    }

    /// The session's state, for `status`.
    pub fn status(&self) -> Json {
        let s = &self.session;
        let d = s.driver();
        json!({
            "k": d.k().saturating_sub(1),
            "view": s.shown().map(|f| f.k),
            "head": s.at_head(),
            "paused": s.paused,
            "speed": s.speed,
            "hz": d.hz(),
            "scale": d.surfaces().screen.scale,
            "reduced": d.reduced(),
            "log": s.log().len(),
            "queued": s.queued(),
            "reloads": s.reloads,
            "frames": d.trace().frames.len(),
            "ring": s.ring().len(),
            "surfaces": d.surfaces().shown.len(),
            "pointer": d.pointer_on(),
            "knobs": s.knobs().iter().map(|k| k.label.clone()).collect::<Vec<_>>(),
            "msg": s.msg.as_ref().map(|(m, _)| m.clone()),
            "error": s.msg.as_ref().is_some_and(|(_, e)| *e),
            "command": self.view.command.clone(),
            "window": [self.layout.w, self.layout.h],
            "focused": self.focused,
            "stage": {
                "top": self.layout.strip,
                "zoom": self.layout.zoom,
                "origin": self.layout.origin,
            },
        })
    }

    /// One command line (stdin's): `{"ok": …}` and what it returns.
    pub fn command(&mut self, line: &str) -> Json {
        match self.command_inner(line.trim()) {
            Ok(v) => {
                let mut v = if v.is_object() { v } else { json!({}) };
                v["ok"] = json!(true);
                v
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    fn command_inner(&mut self, line: &str) -> Result<Json, Error> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let count = |r: &str| -> Result<u32, Error> {
            if r.is_empty() {
                return Ok(1);
            }
            r.parse::<u32>()
                .map_err(|_| Error::Usage(format!("`{r}` is not a count")))
        };
        let point = |r: &str| -> Result<[f32; 2], Error> {
            let (x, y) = r
                .split_once(',')
                .ok_or_else(|| Error::Usage(format!("`{r}` is not X,Y")))?;
            let n = |t: &str| {
                t.trim()
                    .parse::<f32>()
                    .map_err(|_| Error::Usage(format!("`{r}` is not X,Y")))
            };
            Ok([n(x)?, n(y)?])
        };
        match verb {
            "" => Ok(json!({})),
            "status" => Ok(self.status()),
            "pause" => {
                self.session.paused = true;
                Ok(json!({}))
            }
            "play" => {
                self.session.paused = false;
                self.session.head();
                Ok(json!({}))
            }
            "toggle" => {
                self.action(Action::PlayPause);
                Ok(json!({}))
            }
            "speed" => {
                let s = rest
                    .trim_end_matches(['x', 'X'])
                    .parse::<f64>()
                    .map_err(|_| Error::Usage(format!("speed `{rest}`: 0.1 to 1")))?;
                self.session.set_speed(s)?;
                Ok(json!({"speed": self.session.speed}))
            }
            "slower" => {
                self.session.slower();
                Ok(json!({"speed": self.session.speed}))
            }
            "faster" => {
                self.session.faster();
                Ok(json!({"speed": self.session.speed}))
            }
            "step" => {
                self.session.step(count(rest)?)?;
                Ok(self.status())
            }
            "back" => {
                self.session.back(count(rest)?);
                Ok(self.status())
            }
            "head" => {
                self.session.head();
                Ok(self.status())
            }
            "finding" => Ok(json!({"found": self.session.next_finding()})),
            "do" => {
                self.session.act(rest)?;
                Ok(json!({}))
            }
            "knob" => {
                let n = count(rest)?;
                self.session.knob(n.saturating_sub(1) as usize)?;
                Ok(json!({}))
            }
            "knobs" => Ok(
                json!({"knobs": self.session.knobs().iter().map(|k| k.label.clone()).collect::<Vec<_>>()}),
            ),
            "motion" => self.session.toggle_motion().map(|()| json!({})),
            "scale" => self.session.next_scale().map(|()| json!({})),
            "hz" => self.session.next_rate().map(|()| json!({})),
            "leave" => self.session.act("leave").map(|()| json!({})),
            "hover" => {
                self.session.aim(Some(point(rest)?));
                Ok(json!({}))
            }
            "press" | "release" => {
                let b = if rest.is_empty() { "left" } else { rest };
                self.session.button(b, verb == "press")?;
                Ok(json!({}))
            }
            "scroll" => {
                let dy = rest
                    .parse::<f64>()
                    .map_err(|_| Error::Usage(format!("scroll `{rest}`: a number")))?;
                self.session.scroll(dy)?;
                Ok(json!({}))
            }
            "record" => {
                let to = (!rest.is_empty()).then(|| PathBuf::from(rest));
                let r = self.session.record(to)?;
                Ok(
                    json!({"path": r.path.display().to_string(), "digest": r.digest, "steps": r.steps}),
                )
            }
            "scenario" => Ok(json!({"text": self.session.scenario_text()?})),
            "digest" => Ok(json!({"digest": self.session.driver().snapshot().digest})),
            "reload" => {
                self.session.reload()?;
                Ok(self.status())
            }
            "restart" => {
                self.session.restart()?;
                Ok(self.status())
            }
            "checks" => {
                self.session.analyse();
                let a = self
                    .session
                    .analysis()
                    .map(|a| serde_json::to_value(&a.report).unwrap_or(Json::Null));
                Ok(
                    json!({"report": a, "marks": self.session.marks().iter().map(|m| json!({"check": m.check, "msg": m.msg, "rect": m.rect})).collect::<Vec<_>>()}),
                )
            }
            "zoom" => {
                if rest == "fit" {
                    self.view.zoom = None;
                } else {
                    let z = rest
                        .parse::<f32>()
                        .map_err(|_| Error::Usage(format!("zoom `{rest}`: a number or fit")))?;
                    self.zoom_to(z);
                }
                Ok(json!({}))
            }
            "shot" => {
                let (path, size) = match rest.split_once(' ') {
                    Some((p, s)) => (p, Some(s)),
                    None => (rest, None),
                };
                if path.is_empty() {
                    return Err(Error::Usage("shot PATH [WxH]".into()));
                }
                let (w, h) = match size.and_then(|s| s.split_once('x')) {
                    Some((w, h)) => (
                        w.parse()
                            .map_err(|_| Error::Usage("shot PATH [WxH]".into()))?,
                        h.parse()
                            .map_err(|_| Error::Usage("shot PATH [WxH]".into()))?,
                    ),
                    None => (1280u32, 800u32),
                };
                let mut px = vec![0u8; (w * h * 4) as usize];
                view::draw(&self.session, &self.view, &mut px, w, h);
                let rgba = crate::surfaces::to_rgba(&px);
                let png = crate::report::png::encode(w, h, &rgba)?;
                std::fs::write(path, png).map_err(|e| Error::Io(path.to_string(), e))?;
                Ok(json!({"path": path}))
            }
            "quit" => {
                self.quit = true;
                Ok(json!({}))
            }
            other => Err(Error::Usage(format!(
                "unknown command `{other}` (status, pause, play, speed F, step [N], back [N], head, \
                 do STEP, knob N, knobs, motion, scale, hz, leave, hover X,Y, press, release, \
                 scroll DY, record [PATH], scenario, digest, reload, restart, checks, finding, \
                 zoom F|fit, shot PATH [WxH], quit)"
            ))),
        }
    }
}

/// `strand-eval live <file> [--screen WxH[@S]] [--hz N] [--fonts
/// bundled|local] [--speed F] [--paused] [--record-to FILE] [--ring N]
/// [--max-frames N]`.
pub fn main(args: &[String]) -> Result<(), Error> {
    let usage = || {
        Error::Usage(
            "usage: strand-eval live <file.strand|dir|file.scn> [--screen WxH[@S]] [--hz N] \
             [--fonts bundled|local] [--speed F] [--paused] [--record-to FILE] [--ring N] \
             [--max-frames N]"
                .into(),
        )
    };
    let mut opts = Options::default();
    let mut path = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or_else(usage);
        match a.as_str() {
            "--screen" => opts.screen = screen(&val()?)?,
            "--hz" => {
                opts.hz = val()?
                    .parse()
                    .ok()
                    .filter(|h| (1..=1000).contains(h))
                    .ok_or_else(|| Error::Usage("--hz: 1 to 1000".into()))?;
            }
            "--fonts" => {
                opts.fonts = match val()?.as_str() {
                    "bundled" => Fonts::Bundled,
                    "local" => Fonts::Local,
                    o => return Err(Error::Usage(format!("--fonts {o}: bundled or local"))),
                }
            }
            "--speed" => {
                opts.speed = val()?
                    .trim_end_matches(['x', 'X'])
                    .parse()
                    .ok()
                    .filter(|s: &f64| (0.1..=1.0).contains(s))
                    .ok_or_else(|| Error::Usage("--speed: 0.1 to 1".into()))?;
            }
            "--paused" => opts.paused = true,
            "--record-to" => opts.record_to = Some(PathBuf::from(val()?)),
            "--ring" => {
                opts.ring = val()?
                    .parse()
                    .ok()
                    .filter(|n| *n >= 2)
                    .ok_or_else(|| Error::Usage("--ring: 2 or more frames".into()))?;
            }
            "--max-frames" => {
                opts.max_frames = val()?
                    .parse()
                    .map_err(|_| Error::Usage("--max-frames: a count".into()))?;
            }
            "-h" | "--help" => return Err(usage()),
            x if x.starts_with('-') => {
                return Err(Error::Usage(format!("live: unknown option `{x}`")));
            }
            x if path.is_none() => path = Some(PathBuf::from(x)),
            _ => return Err(usage()),
        }
    }
    opts.path = path.ok_or_else(usage)?;
    let session = Session::open(opts)?;
    window::run(Live::new(session))
}

/// `WxH` or `WxH@S`.
fn screen(s: &str) -> Result<Screen, Error> {
    let bad = || Error::Usage(format!("--screen {s}: WxH or WxH@S (scale up to 4)"));
    let (size, scale) = match s.split_once('@') {
        Some((a, b)) => (a, b.parse::<f64>().map_err(|_| bad())?),
        None => (s, 1.0),
    };
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let (w, h) = (
        w.parse::<u32>().map_err(|_| bad())?,
        h.parse::<u32>().map_err(|_| bad())?,
    );
    if w == 0 || h == 0 || !(scale > 0.0 && scale <= 4.0) {
        return Err(bad());
    }
    Ok(Screen {
        width: w,
        height: h,
        scale,
    })
}
