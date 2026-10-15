//! `clipboard` (owner-testing gap 30, decisions.md m6-service-gaps)
//! against a real headless sway (sway 1.9 offers wlr-data-control) and
//! another program (a wlr-data-control client, `common::data_control`):
//! the selection followed, the history recorded while read (as a
//! program reading it anywhere holds it), kept on disk and across a
//! restart, the actions, a password manager's secret never read, and a
//! copy outliving the service. Against `strand-fake-wayland`: no data
//! control, and `ext-data-control-v1` preferred where offered.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Sway;
use common::data_control::DataControl;
use strand_core::Runtime;
use strand_fake_wayland::{Cmd, DataControl as Managers, Fake};
use strand_services::clipboard::{self, Clipboard, ClipboardEntry, Config, keeper};
use strand_services::wm::WaylandTarget;
use strand_services::{Applied, Builtin, Buses, Cells, Data, Services, Store, ToData};

/// The tests set the process-wide display and state directory: one at a
/// time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

const TEXT: &str = "text/plain;charset=utf-8";

/// The index of `history` among the store's fields.
fn history_field() -> usize {
    Clipboard::FIELDS
        .iter()
        .position(|f| f.name == "history")
        .unwrap()
}

type Events = Arc<Mutex<Vec<(String, Vec<String>)>>>;

struct Harness {
    rt: Runtime,
    s: Services,
    b: Builtin,
    events: Events,
}

impl Harness {
    /// `clipboard` on the display at `socket`, its history under `state`;
    /// `history`: a reader reads the history (the top-level hold).
    fn start(socket: &Path, state: &Path, history: bool) -> Harness {
        clipboard::configure(Some(Config {
            target: WaylandTarget::Socket(socket.to_path_buf()),
            state_dir: state.to_path_buf(),
        }));
        let rt = Runtime::new();
        let s = Services::new(&rt, Buses::none(), || {});
        let b = Builtin::register(&s, &rt);
        let events: Events = Arc::default();
        let log = events.clone();
        b.clipboard.dynamic().observe(Box::new(move |_, a| {
            if let Applied::Event { event, args } = a {
                let name = <Clipboard as Store>::EVENTS[*event].name;
                let args = args
                    .iter()
                    .map(|d| match d {
                        Data::Text(t) => t.to_string(),
                        d => format!("{d:?}"),
                    })
                    .collect();
                log.lock().unwrap().push((name.to_string(), args));
            }
        }));
        b.clipboard.acquire(&rt);
        if history {
            b.clipboard.acquire_field(history_field());
        }
        assert!(
            s.wait_ready(&rt, Duration::from_secs(10)),
            "the first state"
        );
        rt.flush();
        Harness { rt, s, b, events }
    }

    fn state(&self) -> Clipboard {
        self.b
            .clipboard
            .cells()
            .snapshot(&self.rt)
            .unwrap_or_default()
    }

    fn texts(&self) -> Vec<String> {
        self.state()
            .history
            .iter()
            .map(|e| match &e.image {
                Some(_) => "<image>".to_string(),
                None => e.text.clone(),
            })
            .collect()
    }

    fn until(&self, what: &str, cond: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.s.pump(&self.rt);
            self.rt.flush();
            if cond(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never: {what}: {:#?}",
                self.state()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pumps for `d`, for what must not happen.
    fn settle(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            self.s.pump(&self.rt);
            self.rt.flush();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn act(&self, action: &str, item: Option<&ClipboardEntry>, args: &[Data]) {
        let item = item.map(ToData::to_data);
        self.b
            .clipboard
            .dynamic()
            .action(&self.rt, action, item.as_ref(), args, Default::default())
            .unwrap();
        self.rt.flush();
    }

    fn events(&self) -> Vec<(String, Vec<String>)> {
        self.events.lock().unwrap().clone()
    }
}

/// Another program on `sway`'s seat.
fn other(sway: &Sway) -> DataControl {
    let conn = wayland_client::Connection::from_socket(
        std::os::unix::net::UnixStream::connect(sway.socket()).unwrap(),
    )
    .unwrap();
    DataControl::connect(conn).expect("sway 1.9 offers wlr-data-control")
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in read.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_clipboard_follows_the_selection_and_keeps_a_history() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-follow") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&sway.socket(), state.path(), true);
    assert!(h.state().available, "{:#?}", h.state());
    let other = other(&sway);
    other.copy_text("one");
    h.until("one", |h| h.state().text == "one" && h.texts() == ["one"]);
    let one = h.state().history[0].id;
    other.copy_text("two");
    h.until("two", |h| h.texts() == ["two", "one"]);
    other.copy_text("one");
    h.until("one again", |h| {
        h.state().text == "one" && h.texts() == ["one", "two"]
    });
    assert_eq!(h.state().history[0].id, one, "a repeat keeps its id");
    assert!(!h.state().holds_image);
    // An image: `image`, and its file holds the bytes.
    let png: &[u8] = b"\x89PNG\r\n\x1a\nnot really";
    other.copy(&[("image/png", png)]);
    h.until("the image", |h| {
        h.state().holds_image && h.texts() == ["<image>", "one", "two"]
    });
    let e = h.state().history[0].clone();
    assert_eq!(h.state().text, "");
    assert_eq!(e.size, png.len() as i64);
    let file = PathBuf::from(e.image.unwrap());
    assert!(file.starts_with(state.path().join("strand/clipboard/images")));
    assert_eq!(std::fs::read(&file).unwrap(), png);
    // A text beside an image is a text.
    other.copy(&[("image/png", png), (TEXT, b"https://example.org/a.png")]);
    h.until("the address", |h| {
        h.state().text == "https://example.org/a.png" && !h.state().holds_image
    });
    // An empty selection.
    other.copy(&[("application/x-other", b"x")]);
    h.until("nothing readable", |h| {
        h.state().text.is_empty() && !h.state().holds_image
    });
    assert!(h.events().is_empty(), "{:?}", h.events());
    h.s.shutdown();
}

#[test]
fn copy_and_entry_actions_set_the_selection() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-actions") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&sway.socket(), state.path(), true);
    let other = other(&sway);
    h.act("copy", None, &[Data::text("10.0.0.1")]);
    assert!(
        other.wait(Duration::from_secs(10), |s| s
            .mimes
            .iter()
            .any(|m| m == TEXT)),
        "{:?}",
        other.seen()
    );
    assert_eq!(
        other.read_now(TEXT, Duration::from_secs(5)),
        Some(b"10.0.0.1".to_vec())
    );
    for t in ["UTF8_STRING", "text/plain", "TEXT", "STRING"] {
        assert_eq!(
            other.read_now(t, Duration::from_secs(5)),
            Some(b"10.0.0.1".to_vec()),
            "{t}"
        );
    }
    h.until("our copy is the text and the first entry", |h| {
        h.state().text == "10.0.0.1" && h.texts() == ["10.0.0.1"]
    });
    let ip = h.state().history[0].clone();
    other.copy_text("other");
    h.until("other", |h| h.texts() == ["other", "10.0.0.1"]);
    // `e.copy()` of the older entry: the selection, and the front.
    h.act("copy", Some(&ip), &[]);
    h.until("the entry again", |h| {
        h.state().text == "10.0.0.1" && h.texts() == ["10.0.0.1", "other"]
    });
    assert_eq!(h.state().history[0].id, ip.id);
    assert_eq!(
        other.read_now("text/plain", Duration::from_secs(5)),
        Some(b"10.0.0.1".to_vec())
    );
    // An image entry copied again offers its type; removed, its file goes.
    let png: &[u8] = b"\x89PNG image bytes";
    other.copy(&[("image/png", png)]);
    h.until("the image", |h| {
        h.texts().first().map(String::as_str) == Some("<image>")
    });
    let img = h.state().history[0].clone();
    other.copy_text("after");
    h.until("after", |h| h.texts()[0] == "after");
    h.act("copy", Some(&img), &[]);
    assert!(other.wait(Duration::from_secs(10), |s| s.mimes == ["image/png"]));
    assert_eq!(
        other.read_now("image/png", Duration::from_secs(5)),
        Some(png.to_vec())
    );
    h.until("the image in front", |h| h.state().history[0].id == img.id);
    let file = PathBuf::from(img.image.clone().unwrap());
    assert!(file.is_file());
    h.act("remove", Some(&img), &[]);
    h.until("removed", |h| {
        h.state().history.iter().all(|e| e.id != img.id)
    });
    assert!(!file.exists(), "its file is deleted");
    let saved = state.path().join("strand/clipboard/history.json");
    assert!(saved.is_file());
    // `clear()`: nothing kept, nothing on disk; the selection stays.
    h.act("clear", None, &[]);
    h.until("cleared", |h| h.state().history.is_empty());
    assert!(!saved.exists());
    assert_eq!(
        files(&state.path().join("strand/clipboard")),
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        other.read_now("image/png", Duration::from_secs(5)),
        Some(png.to_vec()),
        "the selection stays"
    );
    // An entry no longer in the history fails as an event.
    h.act("copy", Some(&ip), &[]);
    h.until("failed", |h| !h.events().is_empty());
    assert_eq!(h.events()[0].0, "failed");
    assert_eq!(h.events()[0].1[0], "copy");
    h.s.shutdown();
}

#[test]
fn a_secret_never_enters_history_or_reaches_disk() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-secret") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&sway.socket(), state.path(), true);
    let other = other(&sway);
    other.copy_text("before");
    h.until("before", |h| h.texts() == ["before"]);
    let on_disk = files(state.path());
    other.copy(&[
        (TEXT, b"hunter2-secret"),
        ("x-kde-passwordManagerHint", b"secret"),
    ]);
    h.until("the secret is no text", |h| h.state().text.is_empty());
    h.settle(Duration::from_millis(300));
    assert_eq!(h.texts(), ["before"]);
    assert!(!h.state().holds_image);
    assert_eq!(files(state.path()), on_disk, "no new file");
    for f in files(state.path()) {
        assert!(
            !contains(&std::fs::read(&f).unwrap(), b"hunter2"),
            "{} holds the secret",
            f.display()
        );
    }
    // A hint that says otherwise is no secret.
    other.copy(&[(TEXT, b"public"), ("x-kde-passwordManagerHint", b"public")]);
    h.until("public", |h| h.texts() == ["public", "before"]);
    h.s.shutdown();
}

#[test]
fn history_survives_a_restart() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-restart") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let other = other(&sway);
    let before = {
        let h = Harness::start(&sway.socket(), state.path(), true);
        other.copy_text("first");
        h.until("first", |h| h.texts() == ["first"]);
        other.copy(&[("image/jpeg", b"\xff\xd8 jpeg")]);
        h.until("the image", |h| h.texts() == ["<image>", "first"]);
        other.copy_text("second");
        h.until("second", |h| h.texts() == ["second", "<image>", "first"]);
        let s = h.state().history;
        h.s.shutdown();
        s
    };
    let h = Harness::start(&sway.socket(), state.path(), true);
    h.until("the history is back", |h| h.state().history.len() == 3);
    let after = h.state().history;
    assert_eq!(
        after
            .iter()
            .map(|e| (e.id, &e.text, &e.image))
            .collect::<Vec<_>>(),
        before
            .iter()
            .map(|e| (e.id, &e.text, &e.image))
            .collect::<Vec<_>>()
    );
    assert!(after[1].image.as_deref().unwrap().ends_with(".jpg"));
    h.s.shutdown();
}

/// (review r1) The selection a run finds when it binds is shown, never
/// recorded: an entry removed, or a history cleared, while its selection
/// stays does not come back at the next start, and an entry still kept
/// keeps its time.
#[test]
fn a_cleared_or_removed_selection_does_not_come_back_at_a_restart() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-unresurrected") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().join("strand/clipboard");
    let other = other(&sway);
    // Cleared while it is the selection.
    {
        let h = Harness::start(&sway.socket(), state.path(), true);
        other.copy_text("token");
        h.until("token", |h| h.texts() == ["token"]);
        assert!(dir.join("history.json").is_file());
        h.act("clear", None, &[]);
        h.until("cleared", |h| h.state().history.is_empty());
        assert_eq!(files(&dir), Vec::<PathBuf>::new());
        h.s.shutdown();
    }
    let h = Harness::start(&sway.socket(), state.path(), true);
    h.until("the selection is shown", |h| h.state().text == "token");
    h.settle(Duration::from_millis(300));
    assert!(h.state().history.is_empty(), "{:#?}", h.state());
    assert_eq!(files(&dir), Vec::<PathBuf>::new(), "nothing written back");
    // Removed while it is the selection, an older entry kept.
    other.copy_text("older");
    h.until("older", |h| h.texts() == ["older"]);
    other.copy_text("removed");
    h.until("removed", |h| h.texts() == ["removed", "older"]);
    let removed = h.state().history[0].clone();
    let older = h.state().history[1].clone();
    h.act("remove", Some(&removed), &[]);
    h.until("gone", |h| h.texts() == ["older"]);
    h.s.shutdown();
    drop(h);
    let h = Harness::start(&sway.socket(), state.path(), true);
    h.until("the selection is shown", |h| h.state().text == "removed");
    h.settle(Duration::from_millis(300));
    assert_eq!(h.texts(), ["older"]);
    assert_eq!(h.state().history[0].time, older.time);
    let saved = std::fs::read(dir.join("history.json")).unwrap();
    assert!(!contains(&saved, b"removed"), "not written back");
    // A copy made after the start is recorded.
    other.copy_text("next");
    h.until("next", |h| h.texts() == ["next", "older"]);
    h.s.shutdown();
}

/// (review r1) The same against strand-fake-wayland, whose selection is
/// set before the service starts: shown, not recorded, nothing written;
/// the next copy is.
#[test]
fn the_selection_found_at_start_is_shown_but_not_recorded() {
    let _g = serial();
    let fake = Fake::builder()
        .toplevel_list(false)
        .seats(1)
        .data_control(Managers::Wlr)
        .start();
    fake.cmd(Cmd::SetSelection(vec![(
        TEXT.into(),
        b"from before".to_vec(),
    )]));
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&fake.socket, state.path(), true);
    h.until("the selection is shown", |h| {
        h.state().text == "from before"
    });
    h.settle(Duration::from_millis(200));
    assert!(h.state().history.is_empty(), "{:#?}", h.state());
    assert!(!state.path().join("strand").exists(), "nothing written");
    fake.cmd(Cmd::SetSelection(vec![(TEXT.into(), b"copied".to_vec())]));
    h.until("the copy is kept", |h| h.texts() == ["copied"]);
    h.s.shutdown();
}

#[test]
fn history_is_recorded_only_while_read() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-unread") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&sway.socket(), state.path(), false);
    let other = other(&sway);
    other.copy_text("unread");
    h.until("the text", |h| h.state().text == "unread");
    other.copy(&[("image/png", b"png")]);
    h.until("the image", |h| h.state().holds_image);
    h.settle(Duration::from_millis(200));
    assert!(h.state().history.is_empty());
    assert!(
        !state.path().join("strand").exists(),
        "nothing written: {:?}",
        files(state.path())
    );
    // A reader comes: what is copied from now on is kept.
    h.b.clipboard.acquire_field(history_field());
    other.copy_text("read");
    h.until("kept", |h| h.texts() == ["read"]);
    assert!(state.path().join("strand/clipboard/history.json").is_file());
    h.s.shutdown();
}

#[test]
fn a_copy_outlives_the_service() {
    let _g = serial();
    let Some(sway) = Sway::start("clip-keep") else {
        return;
    };
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&sway.socket(), state.path(), false);
    let other = other(&sway);
    h.act("copy", None, &[Data::text("kept")]);
    assert!(other.wait(Duration::from_secs(10), |s| s.mimes.len() == 5));
    h.until("the text", |h| h.state().text == "kept");
    h.s.shutdown();
    drop(h);
    assert!(keeper::running());
    assert_eq!(
        other.read_now(TEXT, Duration::from_secs(5)),
        Some(b"kept".to_vec()),
        "the copy is still served"
    );
    // Another program takes the selection: the keeper ends.
    other.copy_text("mine");
    let deadline = Instant::now() + Duration::from_secs(10);
    while keeper::running() {
        assert!(Instant::now() < deadline, "the keeper did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!state.path().join("strand").exists());
}

#[test]
fn without_data_control_the_clipboard_is_unavailable_and_copy_fails_as_an_event() {
    let _g = serial();
    let fake = Fake::builder().toplevel_list(false).seats(1).start();
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&fake.socket, state.path(), true);
    assert!(!h.state().available);
    h.act("copy", None, &[Data::text("x")]);
    h.until("failed", |h| !h.events().is_empty());
    let (name, args) = h.events()[0].clone();
    assert_eq!(name, "failed");
    assert_eq!(args[0], "copy");
    assert_eq!(args[1], clipboard::UNAVAILABLE);
    assert!(fake.selection().is_none());
    h.s.shutdown();
}

#[test]
fn ext_data_control_is_used_where_offered() {
    let _g = serial();
    let fake = Fake::builder()
        .toplevel_list(false)
        .seats(1)
        .data_control(Managers::Both)
        .start();
    let state = tempfile::tempdir().unwrap();
    let h = Harness::start(&fake.socket, state.path(), true);
    assert!(h.state().available);
    // Another program copies: followed and kept.
    fake.cmd(Cmd::SetSelection(vec![(
        TEXT.into(),
        b"from the fake".to_vec(),
    )]));
    h.until("the fake's copy", |h| {
        h.state().text == "from the fake" && h.texts() == ["from the fake"]
    });
    // Our copy goes through ext-data-control.
    h.act("copy", None, &[Data::text("ours")]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some((mimes, protocol)) = fake.selection()
            && protocol != "fake"
        {
            assert_eq!(protocol, "ext");
            assert_eq!(mimes[0], TEXT);
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", fake.selection());
        std::thread::sleep(Duration::from_millis(10));
    }
    h.until("ours", |h| {
        h.state().text == "ours" && h.texts() == ["ours", "from the fake"]
    });
    // On wlr-data-control alone it works too.
    h.s.shutdown();
    drop(h);
    let wlr = Fake::builder()
        .toplevel_list(false)
        .seats(1)
        .data_control(Managers::Wlr)
        .start();
    let h = Harness::start(&wlr.socket, state.path(), true);
    assert!(h.state().available);
    h.act("copy", None, &[Data::text("over wlr")]);
    h.until("over wlr", |h| h.state().text == "over wlr");
    assert_eq!(wlr.selection().map(|s| s.1), Some("wlr".to_string()));
    h.s.shutdown();
}
