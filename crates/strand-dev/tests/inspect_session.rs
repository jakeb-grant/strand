//! (M5 wave 3, S-inspect; inspect-r1-3) `strand-dev inspect`'s terminal
//! session, driven line by line against a scripted shell on a Unix
//! socket: a pick ends pick mode before the pane is shown and outlined;
//! `r` resets and shows the pane again; `f` toggles flashing; an event
//! arriving while the client waits for an answer is handled once the
//! answer is in (`pick_ended`, then Enter sends nothing; a `picked`);
//! `e` opens the node's origin; a read-only token write-back offers
//! `$EDITOR` at the token's line, and the next line answers the offer.

use std::fs;
use std::io::{BufRead as _, BufReader, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use strand_dev::inspect::write::hash;
use strand_dev::inspect::{Terminal, drive};

/// What the session printed.
#[derive(Clone, Default)]
struct Out(Arc<Mutex<Vec<u8>>>);

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Out {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    /// Waits until the session has printed `want` (after `from` bytes).
    fn wait_for(&self, from: usize, want: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = self.text();
            if let Some(i) = t[from.min(t.len())..].find(want) {
                return from + i + want.len();
            }
            assert!(Instant::now() < deadline, "never printed {want:?}:\n{t}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The shell's end of the connection.
struct Shell {
    r: BufReader<UnixStream>,
    w: UnixStream,
}

impl Shell {
    fn expect(&mut self, want: Json) {
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let mut got: Json = serde_json::from_str(&line).unwrap_or(Json::Null);
        // The protocol's version rides on every request.
        assert_eq!(got["v"], 1, "{got}");
        if let Some(m) = got.as_object_mut() {
            m.remove("v");
        }
        assert_eq!(got, want, "the client's request");
    }
    fn send(&mut self, v: Json) {
        self.w.write_all(format!("{v}\n").as_bytes()).unwrap();
    }
    fn ok(&mut self) {
        self.send(json!({"ok": true}));
    }
    /// The client's request, answered `answer`.
    fn answer(&mut self, want: Json, answer: Json) {
        self.expect(want);
        self.send(answer);
    }
}

#[test]
fn the_terminal_session_picks_shows_resets_flashes_and_offers_editor() {
    let d = std::env::temp_dir().join(format!("strand-dev-session-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let bar = d.join("bar.strand");
    let theme = d.join("theme.strand");
    let bar_text = "bar Top {\n    box { id: t; bg: $probe.c }\n}\n";
    let theme_text = "tokens base {\n    probe {\n        h: 24\n        c: #112233\n    }\n}\n";
    fs::write(&bar, bar_text).unwrap();
    fs::write(&theme, theme_text).unwrap();
    // A Nix store file: the token's definition cannot be written.
    fs::set_permissions(&theme, fs::Permissions::from_mode(0o444)).unwrap();
    let pane = json!({
        "ok": true,
        "node": {
            "id": "3.0", "kind": "box",
            "origin": {"file": bar.display().to_string(), "line": 40, "col": 5,
                       "span": [14, 47]},
            "identity": {"id": "t", "sibling_index": 0, "scope": "Top"},
            "props": {"bg": "#112233"},
            "tokens": [{"path": "probe.c", "chain": [
                {"path": "probe.c", "origin": "tokens base", "value": "#112233"}
            ]}],
        },
        "files": {
            bar.display().to_string(): hash(bar_text.as_bytes()),
            theme.display().to_string(): hash(theme_text.as_bytes()),
        },
        "layout": [{"surface": 1, "x": 0.0, "y": 0.0, "width": 20.0, "height": 30.0}],
    });

    let socket = d.join("shell.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (lines, typed) = mpsc::channel::<String>();
    let typed = Mutex::new(typed);
    let out = Out::default();
    let opened = Arc::new(Mutex::new(Vec::<(PathBuf, u64)>::new()));
    let term = Terminal {
        read_line: Box::new(move || typed.lock().unwrap().recv().ok()),
        out: Box::new(out.clone()),
        editor: Box::new({
            let opened = opened.clone();
            move |f, line| opened.lock().unwrap().push((f.to_path_buf(), line))
        }),
    };
    let client = std::thread::spawn({
        let socket = socket.clone();
        move || drive(&socket, term)
    });
    let (s, _) = listener.accept().unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sh = Shell {
        r: BufReader::new(s.try_clone().unwrap()),
        w: s,
    };
    let type_line = |l: &str| lines.send(format!("{l}\n")).unwrap();
    let pick = |on: bool| json!({"cmd": "inspect.pick", "on": on});
    let node = json!({"cmd": "inspect.node", "id": "3.0"});
    let highlight = json!({"cmd": "inspect.highlight", "id": "3.0"});
    let flash = |on: bool| json!({"cmd": "inspect.flash", "on": on});

    // The session starts picking.
    sh.answer(pick(true), json!({"ok": true}));
    let mut at = out.wait_for(0, "picking: click a node");
    // A pick: pick mode ends first (the shell's input is its own again),
    // then the pane is asked for and the node outlined.
    sh.send(json!({"event": "picked", "surface": 1, "node": "3.0"}));
    sh.answer(pick(false), json!({"ok": true}));
    sh.answer(node.clone(), pane.clone());
    sh.answer(highlight.clone(), json!({"ok": true}));
    at = out.wait_for(at, "box 3.0");
    // `r`: the subtree reset, the pane shown and outlined again.
    type_line("r");
    sh.answer(
        json!({"cmd": "reset", "node": "3.0"}),
        json!({"ok": true, "reset": 1}),
    );
    sh.answer(node.clone(), pane.clone());
    sh.answer(highlight.clone(), json!({"ok": true}));
    at = out.wait_for(at, "reset 3.0's subtree");
    at = out.wait_for(at, "box 3.0");
    // `f` toggles flashing.
    type_line("f");
    sh.answer(flash(true), json!({"ok": true}));
    at = out.wait_for(at, "damage flashing on");
    type_line("f");
    sh.answer(flash(false), json!({"ok": true}));
    at = out.wait_for(at, "damage flashing off");
    // An event that comes while the client waits for an answer is
    // handled after it: pick mode ended by the shell (a lock)...
    type_line("p");
    sh.expect(pick(true));
    sh.send(json!({"event": "pick_ended"}));
    sh.ok();
    at = out.wait_for(at, "pick mode ended by the shell");
    // ... so Enter no longer ends pick mode: the next request is `f`'s.
    type_line("");
    type_line("f");
    sh.answer(flash(true), json!({"ok": true}));
    at = out.wait_for(at, "damage flashing on");
    // A pick that comes while the client waits is shown once it is in.
    type_line("p");
    sh.expect(pick(true));
    sh.send(json!({"event": "picked", "surface": 2, "node": "3.0"}));
    sh.ok();
    sh.answer(pick(false), json!({"ok": true}));
    sh.answer(node.clone(), pane.clone());
    sh.answer(highlight.clone(), json!({"ok": true}));
    at = out.wait_for(at, "box 3.0");
    // `e`: the node's source at its origin line.
    type_line("e");
    // A read-only token definition: refused, offering `$EDITOR` at the
    // token's own line (line 4 of the theme), not the node's (40); the
    // next line answers the offer and is not a command.
    type_line("token probe.c #ffffff");
    at = out.wait_for(at, &format!("open {} in $EDITOR? [y/N]", theme.display()));
    type_line("y");
    type_line("q");
    assert_eq!(client.join().unwrap(), Ok(()));
    let _ = at;
    assert_eq!(
        *opened.lock().unwrap(),
        [(bar.clone(), 40), (theme.clone(), 4)]
    );
    assert_eq!(fs::read_to_string(&theme).unwrap(), theme_text);
    fs::set_permissions(&theme, fs::Permissions::from_mode(0o644)).unwrap();
    let _ = fs::remove_dir_all(&d);
}
