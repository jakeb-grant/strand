//! `clipboard`: the selection, copies from handlers and the history, over
//! the compositor's data control (`ext-data-control-v1` where offered,
//! else `wlr-data-control`; owner-testing gap 30, decisions.md
//! m6-service-gaps). An `input` copies and pastes without it, through
//! strand-surface's `wl_data_device`.
//!
//! The run lives on a thread of its own (`strand-clipboard`) with its own
//! Wayland connection, asleep in `poll(2)` on its socket, an eventfd the
//! service's messages poke and the one read in flight, as `outputs`'
//! does. It follows the selection: [`offer::kind`] says what it is; a
//! text is read (1 MiB at most, 5 s) into `text`; an image sets
//! `holds_image`.
//! An offer carrying a password manager's hint marked `secret` (or whose
//! hint cannot be read) is read no further: `text` is "", nothing is
//! recorded, nothing is logged. While a reader reads `history` (a
//! `#[store(keyed, stream)]` field, held from the top level of a program
//! that reads it anywhere: spec.md 8.1), each text and each PNG or JPEG
//! image copied is recorded in [`history::History`], kept in
//! `<state>/strand/clipboard/`. The selection the run finds when it binds
//! (Strand starting, or a reconnect) is shown but never recorded: it is
//! no copy, and an entry removed or cleared while it stays the selection
//! must not come back. A program that never reads `history` never writes
//! clipboard data to disk.
//!
//! `copy(value)` and `e.copy()` hand the copy to the [`keeper`], which
//! holds it past the run's end. Without data control `available` is
//! false and they fail as `clipboard.failed(action, error)`, as every
//! refusal does (spec.md 12). A lost connection clears `available` and
//! ends the run with an error, which the contract retries with its
//! backoff.

pub mod history;
pub mod keeper;
pub mod offer;
mod protocol;

use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFlags};

use self::history::{Caps, History, What};
use self::offer::Kind;
use self::protocol::{Ev, NO_DATA_CONTROL, Offer};
use crate::notifications::Date;
use crate::wm::WaylandTarget;
use crate::{Call, Cx, Event, Msg, ServiceError, Store, service};

pub use self::protocol::NO_DATA_CONTROL as UNAVAILABLE;

/// The schema the `clipboard` service serves.
pub const SCHEMA: &str = strand_services_schema::CLIPBOARD;

/// How long a read of the selection may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the service connects and keeps its history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The display.
    pub target: WaylandTarget,
    /// The state directory the history lives under (`<state>/strand/clipboard`).
    pub state_dir: PathBuf,
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Where `clipboard` connects and keeps its history from its next start
/// on (tests: a headless sway's socket and a temporary directory). `None`
/// restores `$WAYLAND_DISPLAY` and `$XDG_STATE_HOME`.
pub fn configure(config: Option<Config>) {
    if let Ok(mut c) = CONFIG.lock() {
        *c = config;
    }
}

/// The display's socket and the history's directory (`None`: no state
/// directory, so no history is kept).
fn config() -> io::Result<(PathBuf, Option<PathBuf>)> {
    let configured = CONFIG.lock().ok().and_then(|c| c.clone());
    let (target, dir) = match configured {
        Some(c) => (c.target, Some(history::dir_in(&c.state_dir))),
        None => (
            WaylandTarget::Env,
            history::default_dir(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")),
        ),
    };
    let socket = match target {
        WaylandTarget::Socket(p) => p,
        WaylandTarget::Env => crate::wm::protocol::display_socket(
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?,
    };
    Ok((socket, dir))
}

/// One entry of the history (the schema's `ClipboardEntry`).
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "ClipboardEntry", key = id)]
pub struct ClipboardEntry {
    pub id: i64,
    pub text: String,
    pub image: Option<String>,
    pub size: i64,
    pub time: Date,
}

/// `clipboard`'s actions: on a `ClipboardEntry`, and the service's own.
#[derive(Call, Debug, Clone, PartialEq)]
pub enum ClipboardAction {
    /// `e.copy()`.
    #[call(name = "copy")]
    CopyEntry { item: ClipboardEntry },
    /// `e.remove()`.
    Remove { item: ClipboardEntry },
    /// `clipboard.copy(value)`.
    Copy { value: String },
    /// `clipboard.clear()`.
    Clear,
}

impl ClipboardAction {
    /// Its name as the schema spells it.
    pub fn name(&self) -> &'static str {
        match self {
            ClipboardAction::CopyEntry { .. } | ClipboardAction::Copy { .. } => "copy",
            ClipboardAction::Remove { .. } => "remove",
            ClipboardAction::Clear => "clear",
        }
    }
}

/// See the module docs.
#[service(name = "clipboard", action = ClipboardAction, refused = refused, thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Clipboard {
    /// The compositor offers data control: false without it, when `history`
    /// stays as it was and `copy` fails with `clipboard.failed` (an `input`
    /// still copies and pastes).
    pub available: bool,
    /// The selection's text, or "" when it holds none (empty, an image, or a
    /// password manager's secret, which is never read).
    pub text: String,
    /// The selection is an image; once recorded, its file is
    /// `history[0].image`.
    pub holds_image: bool,
    /// What was copied, newest first, each once: texts and images, the newest
    /// 100 within 64 MiB, kept across restarts. Recorded while a program
    /// reads it anywhere (see spec.md 8.1); a secret never enters it.
    #[store(keyed, stream)]
    pub history: Vec<ClipboardEntry>,
    /// An action failed (no data control, an image file gone):
    /// `on clipboard.failed(action, error) { … }`.
    pub failed: Event<(String, String)>,
}

/// An action the service could not take (its run backing off): `failed`,
/// as one the run refuses is.
fn refused(action: &ClipboardAction, error: &str) -> Option<ClipboardEvent> {
    Some(ClipboardEvent::Failed((
        action.name().to_string(),
        error.to_string(),
    )))
}

/// What a read of the selection is for.
#[derive(Debug)]
enum Stage {
    /// The password manager's hint; then the offer, of these types.
    Hint(Vec<String>),
    Text,
    Image(String),
}

/// A read of the selection in flight.
struct Reading {
    pipe: io::PipeReader,
    buf: Vec<u8>,
    max: usize,
    deadline: Instant,
    stage: Stage,
}

impl Reading {
    /// Reads what the pipe holds now: `Some(data)` once it ended (`None`
    /// data: failed, or over `max`); `None` while it goes on.
    fn read(&mut self) -> Option<Option<Vec<u8>>> {
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match self.pipe.read(&mut chunk) {
                Ok(0) => return Some(Some(std::mem::take(&mut self.buf))),
                Ok(n) => {
                    if self.buf.len() + n > self.max {
                        return Some(None);
                    }
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Some(None),
            }
        }
    }
}

/// The run's state beside the store.
struct Run {
    socket: PathBuf,
    /// The history's directory; `None`: no state directory.
    dir: Option<PathBuf>,
    /// Loaded at the first watch or action that needs it.
    history: Option<History>,
    /// The selection's offer, kept until the next.
    selection: Option<Offer>,
    /// The selection is a copy made while the run watched, not the one
    /// the run found when it bound (at Strand's start, or a reconnect):
    /// only a copy is recorded, so an entry removed or cleared while its
    /// selection stays never comes back.
    copied: bool,
    reading: Option<Reading>,
    /// Actions waiting for the registry.
    waiting: Vec<ClipboardAction>,
}

impl Run {
    /// The history, loaded on first use (`None`: no state directory).
    fn history(&mut self, cx: &mut Cx<Clipboard>) -> Option<&mut History> {
        if self.history.is_none() {
            let dir = self.dir.clone()?;
            let (h, warning) = History::load(dir, Caps::DEFAULT);
            if let Some(w) = warning {
                log::warn!("clipboard: {w}");
                cx.warn(w);
            }
            self.history = Some(h);
        }
        self.history.as_mut()
    }

    /// Shows the history as it is now.
    fn publish(&mut self, cx: &mut Cx<Clipboard>) {
        if let Some(h) = &self.history {
            let entries = h.entries();
            cx.update(|s| s.history = entries);
        }
    }

    /// Saves the history and shows it.
    fn changed(&mut self, cx: &mut Cx<Clipboard>) {
        if let Some(h) = &self.history
            && let Err(e) = h.save()
        {
            let w = format!(
                "cannot save the clipboard history in {}: {e}",
                h.dir().display()
            );
            log::warn!("clipboard: {w}");
            cx.warn(w);
        }
        self.publish(cx);
    }

    fn act(&mut self, cx: &mut Cx<Clipboard>, available: bool, action: ClipboardAction) {
        let name = action.name();
        if let Err(e) = self.try_act(cx, available, action) {
            log::warn!("clipboard: {name} failed: {e}");
            cx.emit(ClipboardEvent::Failed((name.to_string(), e)));
        }
    }

    fn try_act(
        &mut self,
        cx: &mut Cx<Clipboard>,
        available: bool,
        action: ClipboardAction,
    ) -> Result<(), String> {
        const NO_STATE: &str = "no state directory ($XDG_STATE_HOME or $HOME) to keep a history in";
        match action {
            ClipboardAction::Copy { value } => {
                if !available {
                    return Err(NO_DATA_CONTROL.into());
                }
                keeper::copy(&self.socket, text_offers(&value))
            }
            ClipboardAction::CopyEntry { item } => {
                if !available {
                    return Err(NO_DATA_CONTROL.into());
                }
                let socket = self.socket.clone();
                let h = self.history(cx).ok_or(NO_STATE)?;
                let s = h
                    .get(item.id)
                    .ok_or_else(|| format!("entry {} is no longer in the history", item.id))?;
                let offers = match &s.what {
                    What::Text(t) => text_offers(t),
                    What::Image { file, mime } => {
                        let path = h.image_path(file);
                        let bytes =
                            std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                        vec![(mime.clone(), Arc::from(bytes))]
                    }
                };
                keeper::copy(&socket, offers)?;
                h.touch(item.id);
                self.changed(cx);
                Ok(())
            }
            ClipboardAction::Remove { item } => {
                let h = self.history(cx).ok_or(NO_STATE)?;
                if h.remove(item.id) {
                    self.changed(cx);
                }
                Ok(())
            }
            ClipboardAction::Clear => {
                let h = self.history(cx).ok_or(NO_STATE)?;
                let r = h.clear().map_err(|e| e.to_string());
                self.publish(cx);
                r
            }
        }
    }

    /// The selection changed (`at_bind`: it is the one the run found).
    fn selection(
        &mut self,
        cx: &mut Cx<Clipboard>,
        sel: Option<(Offer, Vec<String>)>,
        at_bind: bool,
    ) {
        self.reading = None;
        self.copied = !at_bind;
        if let Some(old) = self.selection.take() {
            old.destroy();
        }
        let Some((offer, mimes)) = sel else {
            cx.update(|s| {
                s.text.clear();
                s.holds_image = false;
            });
            return;
        };
        self.selection = Some(offer);
        if offer::has_hint(&mimes) {
            self.read(
                cx,
                offer::PASSWORD_HINT,
                offer::HINT_BYTES,
                Stage::Hint(mimes),
            );
        } else {
            self.classify(cx, &mimes);
        }
    }

    /// What the selection is, once it is known not to be secret.
    fn classify(&mut self, cx: &mut Cx<Clipboard>, mimes: &[String]) {
        match offer::kind(mimes) {
            Kind::Text(mime) => {
                self.read(cx, &mime, history::Caps::DEFAULT.text_bytes, Stage::Text);
            }
            Kind::Image(mime) => {
                cx.update(|s| {
                    s.text.clear();
                    s.holds_image = true;
                });
                if self.copied && cx.watched("history") {
                    self.read(
                        cx,
                        &mime,
                        Caps::DEFAULT.image_bytes,
                        Stage::Image(mime.clone()),
                    );
                }
            }
            Kind::OtherImage => {
                cx.update(|s| {
                    s.text.clear();
                    s.holds_image = true;
                });
            }
            Kind::Empty => {
                cx.update(|s| {
                    s.text.clear();
                    s.holds_image = false;
                });
            }
        }
    }

    /// Starts reading the selection as `mime`.
    fn read(&mut self, cx: &mut Cx<Clipboard>, mime: &str, max: usize, stage: Stage) {
        let Some(offer) = &self.selection else {
            return;
        };
        let started = io::pipe().and_then(|(r, w)| {
            rustix::io::ioctl_fionbio(&r, true).map_err(io::Error::from)?;
            offer.receive(mime, w.as_fd());
            drop(OwnedFd::from(w));
            Ok(r)
        });
        match started {
            Ok(pipe) => {
                self.reading = Some(Reading {
                    pipe,
                    buf: Vec::new(),
                    max,
                    deadline: Instant::now() + READ_TIMEOUT,
                    stage,
                });
            }
            Err(e) => {
                log::warn!("clipboard: cannot read the selection: {e}");
                self.done(cx, stage, None);
            }
        }
    }

    /// A read ended with `data` (`None`: failed, too long or too slow).
    fn done(&mut self, cx: &mut Cx<Clipboard>, stage: Stage, data: Option<Vec<u8>>) {
        match stage {
            Stage::Hint(mimes) => {
                if offer::hint_is_secret(data.as_deref()) {
                    cx.update(|s| {
                        s.text.clear();
                        s.holds_image = false;
                    });
                } else {
                    self.classify(cx, &mimes);
                }
            }
            Stage::Text => {
                let text = data
                    .map(|d| String::from_utf8_lossy(&d).into_owned())
                    .unwrap_or_else(|| {
                        log::debug!(
                            "clipboard: the selection's text was not read (too long or too slow)"
                        );
                        String::new()
                    });
                let record = self.copied && cx.watched("history") && !text.is_empty();
                cx.update(|s| {
                    s.text.clone_from(&text);
                    s.holds_image = false;
                });
                if record && let Some(h) = self.history(cx) {
                    h.push_text(&text);
                    self.changed(cx);
                }
            }
            Stage::Image(mime) => {
                let Some(bytes) = data else {
                    log::debug!(
                        "clipboard: the selection's image was not read (too large or too slow)"
                    );
                    return;
                };
                if !self.copied || !cx.watched("history") {
                    return;
                }
                let Some(h) = self.history(cx) else {
                    return;
                };
                if let Err(e) = h.push_image(&bytes, &mime) {
                    let w = format!("cannot keep a copied image: {e}");
                    log::warn!("clipboard: {w}");
                    cx.warn(w);
                }
                self.changed(cx);
            }
        }
    }
}

/// `value` as every text type.
fn text_offers(value: &str) -> keeper::Offers {
    let bytes: Arc<[u8]> = Arc::from(value.as_bytes());
    offer::TEXT
        .iter()
        .map(|t| ((*t).to_string(), bytes.clone()))
        .collect()
}

impl Clipboard {
    fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let wake = Arc::new(
            rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .map_err(io::Error::from)?,
        );
        let poke = wake.clone();
        cx.set_notify(move || {
            let _ = rustix::io::write(&*poke, &1u64.to_ne_bytes());
        });
        let (socket, dir) = config()?;
        let mut run = Run {
            socket,
            dir,
            history: None,
            selection: None,
            copied: false,
            reading: None,
            waiting: Vec::new(),
        };
        let r = Self::serve(&mut cx, &wake, &mut run);
        if let Err(e) = &r {
            cx.update(|s| {
                s.available = false;
                s.text.clear();
                s.holds_image = false;
            });
            let error = format!("the compositor's connection failed: {e}");
            let mut lost = std::mem::take(&mut run.waiting);
            while let Ok(m) = cx.try_recv() {
                if let Msg::Action(a) = m {
                    lost.push(a);
                }
            }
            for a in lost {
                cx.emit(ClipboardEvent::Failed((
                    a.name().to_string(),
                    error.clone(),
                )));
            }
        }
        r
    }

    fn serve(cx: &mut Cx<Self>, wake: &OwnedFd, run: &mut Run) -> Result<(), ServiceError> {
        let (conn, mut queue, mut wl) = protocol::connect(&run.socket).map_err(ServiceError)?;
        let qh = queue.handle();
        loop {
            loop {
                match cx.try_recv() {
                    Ok(Msg::Action(a)) => run.waiting.push(a),
                    Ok(Msg::Watch {
                        field: "history",
                        on: true,
                    }) => {
                        if run.history(cx).is_some() {
                            run.publish(cx);
                        }
                    }
                    Ok(_) => {}
                    Err(false) => break,
                    Err(true) => return Ok(()),
                }
            }
            let available = wl.bind(&qh);
            // Ready once the selection found at bind is told, so an action
            // never races it.
            if wl.settled() {
                if !cx.is_ready() || cx.state().available != available {
                    cx.update(|s| s.available = available);
                    if !cx.is_ready() {
                        if cx.watched("history") && run.history(cx).is_some() {
                            run.publish(cx);
                        }
                        cx.ready();
                    }
                }
                for a in std::mem::take(&mut run.waiting) {
                    run.act(cx, available, a);
                }
            }
            for ev in std::mem::take(&mut wl.events) {
                if let Ev::Selection { offer, at_bind } = ev {
                    run.selection(cx, offer, at_bind);
                }
            }
            if wl.lost {
                return Err(ServiceError(
                    "the compositor's data control went away".into(),
                ));
            }
            let deadline = run.reading.as_ref().map(|r| r.deadline);
            let mut extra = vec![protocol::polled(wake, PollFlags::IN)];
            if let Some(r) = &run.reading {
                extra.push((r.pipe.as_fd(), PollFlags::IN));
            }
            let revents = protocol::wait(&conn, &mut queue, &mut wl, &extra, deadline)
                .map_err(ServiceError)?;
            drop(extra);
            if revents.first().is_some_and(|r| !r.is_empty()) {
                let mut buf = [0u8; 8];
                let _ = rustix::io::read(wake, &mut buf);
            }
            let ended = match &mut run.reading {
                Some(r) if revents.get(1).is_some_and(|f| !f.is_empty()) => r.read(),
                Some(r) if Instant::now() >= r.deadline => Some(None),
                _ => None,
            };
            if let Some(data) = ended
                && let Some(r) = run.reading.take()
            {
                run.done(cx, r.stage, data);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Keyed, Service};

    #[test]
    fn the_store_is_the_schema() {
        assert_eq!(<Clipboard as Service>::NAME, "clipboard");
        let names: Vec<&str> = Clipboard::FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, ["available", "text", "holds_image", "history"]);
        assert_eq!((Clipboard::FIELDS[3].ty)(), "[ClipboardEntry]");
        assert_eq!(ClipboardEntry::KEY_FIELD, "id");
    }

    #[test]
    fn a_refused_action_is_the_failed_event() {
        let a = ClipboardAction::Copy { value: "x".into() };
        assert_eq!(
            refused(&a, "no run"),
            Some(ClipboardEvent::Failed(("copy".into(), "no run".into())))
        );
        let e = ClipboardAction::CopyEntry {
            item: ClipboardEntry::default(),
        };
        assert_eq!(e.name(), "copy");
        assert_eq!(ClipboardAction::Clear.name(), "clear");
    }

    #[test]
    fn a_copy_offers_every_text_type() {
        let offers = text_offers("10.0.0.1");
        let types: Vec<&str> = offers.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(types, offer::TEXT);
        assert!(offers.iter().all(|(_, b)| &b[..] == b"10.0.0.1"));
    }
}
