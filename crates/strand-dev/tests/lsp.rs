//! The language server driven in process by a small LSP client over
//! `Connection::memory()`: initialize, open, change, completion, hover,
//! definition, rename, code actions and formatting against design.md's
//! shells (`crates/strand-compiler/tests/fixtures`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::Position;
use serde_json::{Value, json};
use strand_dev::text::{Lines, path_to_uri};

/// design.md's shells, which check clean as one config.
const SHELLS: &[&str] = &["bar", "launcher", "toasts", "osd", "theme", "rice_now"];

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../strand-compiler/tests/fixtures")
        .join(format!("{name}.strand"));
    std::fs::read_to_string(p).unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("strand-lsp-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(dir).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shells' files with (file, from, to) replacements.
fn shell_files(edits: &[(&str, &str, &str)]) -> Vec<(String, String)> {
    SHELLS
        .iter()
        .map(|n| {
            let mut text = fixture(n);
            for (file, from, to) in edits {
                if *file == *n {
                    assert!(text.contains(from), "{from:?} not in {n}");
                    text = text.replacen(from, to, 1);
                }
            }
            (format!("{n}.strand"), text)
        })
        .collect()
}

/// A minimal LSP client.
struct Client {
    conn: Connection,
    server: Option<JoinHandle<()>>,
    next: i32,
    /// Notifications received while waiting for responses.
    queue: VecDeque<Notification>,
    /// Methods of the requests the server sent.
    server_requests: Vec<String>,
    dir: TempDir,
}

const WAIT: Duration = Duration::from_secs(20);

impl Client {
    /// Starts a server on a workspace holding `files` (name, text).
    fn start(files: &[(&str, String)]) -> Self {
        Client::start_with(files, |_| {})
    }

    /// [`Client::start`] with the `initialize` params changed by `init`
    /// (`files` may name sub-directories).
    fn start_with(files: &[(&str, String)], init: impl FnOnce(&mut Value)) -> Self {
        let dir = TempDir::new();
        for (name, text) in files {
            let path = dir.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let (server_conn, conn) = Connection::memory();
        let server = std::thread::spawn(move || {
            strand_dev::serve(&server_conn).unwrap();
        });
        let mut c = Client {
            conn,
            server: Some(server),
            next: 0,
            queue: VecDeque::new(),
            server_requests: Vec::new(),
            dir,
        };
        let root = path_to_uri(&c.dir.0);
        let mut params = json!({
            "processId": null,
            "rootUri": root,
            "workspaceFolders": [{ "uri": root, "name": "strand" }],
            "capabilities": {},
            "initializationOptions": { "debounceMs": 150 },
        });
        init(&mut params);
        let caps = c.request("initialize", params);
        assert_eq!(caps["serverInfo"]["name"], "strand-dev");
        c.notify("initialized", json!({}));
        c
    }

    /// Answers a request the server sent (`client/registerCapability`).
    fn answer(&mut self, r: Request) {
        self.server_requests.push(r.method.clone());
        self.conn
            .sender
            .send(Message::Response(Response::new_ok(r.id, Value::Null)))
            .unwrap();
    }

    fn shells() -> Self {
        Client::shells_edited(&[])
    }

    /// The shells with (file, from, to) replacements.
    fn shells_edited(edits: &[(&str, &str, &str)]) -> Self {
        let files = shell_files(edits);
        let refs: Vec<(&str, String)> =
            files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
        Client::start(&refs)
    }

    fn uri(&self, name: &str) -> String {
        path_to_uri(&self.dir.0.join(name))
    }

    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.0.join(name)).unwrap()
    }

    fn notify(&self, method: &str, params: Value) {
        self.conn
            .sender
            .send(Message::Notification(Notification::new(
                method.into(),
                params,
            )))
            .unwrap();
    }

    fn send(&mut self, method: &str, params: Value) -> RequestId {
        self.next += 1;
        let id = RequestId::from(self.next);
        self.conn
            .sender
            .send(Message::Request(Request::new(
                id.clone(),
                method.into(),
                params,
            )))
            .unwrap();
        id
    }

    fn response(&mut self, id: RequestId) -> Response {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.conn.receiver.recv_timeout(left).expect("no response") {
                Message::Response(r) if r.id == id => return r,
                Message::Notification(n) => self.queue.push_back(n),
                Message::Request(r) => self.answer(r),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// A request's result; panics on an error response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        match self.response(id).response_result {
            Ok(v) => v,
            Err(e) => panic!("{method} failed: {}", e.message),
        }
    }

    /// A request's error message; panics on success.
    fn request_err(&mut self, method: &str, params: Value) -> String {
        let id = self.send(method, params);
        match self.response(id).response_result {
            Ok(v) => panic!("{method} succeeded: {v}"),
            Err(e) => e.message,
        }
    }

    fn open(&self, name: &str) {
        self.open_text(name, &self.text(name));
    }

    fn open_text(&self, name: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": self.uri(name), "languageId": "strand", "version": 1, "text": text,
            }}),
        );
    }

    fn change(&self, name: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": self.uri(name), "version": version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// The next diagnostics published for `name`.
    fn diagnostics(&mut self, name: &str) -> Vec<Value> {
        self.diagnostics_within(name, WAIT)
            .unwrap_or_else(|| panic!("no diagnostics for {name}"))
    }

    fn diagnostics_within(&mut self, name: &str, wait: Duration) -> Option<Vec<Value>> {
        let uri = self.uri(name);
        let is_ours = |n: &Notification| {
            n.method == "textDocument/publishDiagnostics" && n.params["uri"] == uri.as_str()
        };
        if let Some(i) = self.queue.iter().position(is_ours) {
            let n = self.queue.remove(i).unwrap();
            return Some(n.params["diagnostics"].as_array().unwrap().clone());
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.conn.receiver.recv_timeout(left) {
                Ok(Message::Notification(n)) if is_ours(&n) => {
                    return Some(n.params["diagnostics"].as_array().unwrap().clone());
                }
                Ok(Message::Notification(n)) => self.queue.push_back(n),
                Ok(Message::Request(r)) => self.answer(r),
                Ok(other) => panic!("unexpected {other:?}"),
                Err(_) => return None,
            }
        }
    }

    /// The position of the `nth` `needle` in `text`, plus `delta` bytes.
    fn pos(text: &str, needle: &str, nth: usize, delta: usize) -> Position {
        let at = text
            .match_indices(needle)
            .nth(nth)
            .unwrap_or_else(|| panic!("no {needle:?} #{nth}"))
            .0
            + delta;
        Lines::new(text).position(text, at as u32)
    }

    fn at(&self, name: &str, pos: Position) -> Value {
        json!({ "textDocument": { "uri": self.uri(name) }, "position": pos })
    }

    fn completion(&mut self, name: &str, pos: Position) -> Vec<String> {
        let params = self.at(name, pos);
        let r = self.request("textDocument/completion", params);
        let items = match &r {
            Value::Array(a) => a.clone(),
            Value::Object(o) => o["items"].as_array().unwrap().clone(),
            _ => Vec::new(),
        };
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect()
    }

    fn hover(&mut self, name: &str, pos: Position) -> String {
        let params = self.at(name, pos);
        let r = self.request("textDocument/hover", params);
        r["contents"]["value"].as_str().unwrap_or("").to_string()
    }

    /// Applies a workspace edit's changes to this file's text.
    fn applied(&self, edit: &Value, name: &str, text: &str) -> String {
        let Some(edits) = Client::edits_of(edit, &self.uri(name)) else {
            return text.to_string();
        };
        let lines = Lines::new(text);
        let mut spans: Vec<(u32, u32, String)> = edits
            .iter()
            .map(|e| {
                let p = |v: &Value| {
                    Position::new(
                        v["line"].as_u64().unwrap() as u32,
                        v["character"].as_u64().unwrap() as u32,
                    )
                };
                (
                    lines.offset(text, p(&e["range"]["start"])),
                    lines.offset(text, p(&e["range"]["end"])),
                    e["newText"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        spans.sort_by_key(|s| std::cmp::Reverse(s.0));
        let mut out = text.to_string();
        for (s, e, t) in spans {
            out.replace_range(s as usize..e as usize, &t);
        }
        out
    }

    /// A workspace edit's edits for `uri`, from `changes` or
    /// `documentChanges`.
    fn edits_of<'v>(edit: &'v Value, uri: &str) -> Option<&'v Vec<Value>> {
        if let Some(e) = edit["changes"][uri].as_array() {
            return Some(e);
        }
        edit["documentChanges"]
            .as_array()?
            .iter()
            .find(|d| d["textDocument"]["uri"] == uri)?["edits"]
            .as_array()
    }

    fn rename(&mut self, name: &str, pos: Position, new_name: &str) -> Value {
        let mut params = self.at(name, pos);
        params["newName"] = json!(new_name);
        self.request("textDocument/rename", params)
    }

    fn rename_err(&mut self, name: &str, pos: Position, new_name: &str) -> String {
        let mut params = self.at(name, pos);
        params["newName"] = json!(new_name);
        self.request_err("textDocument/rename", params)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let id = self.send("shutdown", Value::Null);
        let _ = self.response(id);
        self.notify("exit", Value::Null);
        if let Some(s) = self.server.take() {
            s.join().unwrap();
        }
    }
}

fn messages(diags: &[Value]) -> Vec<String> {
    diags
        .iter()
        .map(|d| d["message"].as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------

#[test]
fn initialize_advertises_the_features() {
    let mut c = Client::start(&[]);
    // A second look at the capabilities through a fresh handshake is not
    // possible; check the server's own table instead.
    let caps = serde_json::to_value(strand_dev::capabilities()).unwrap();
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!(["$", ".", ">"])
    );
    assert_eq!(caps["renameProvider"]["prepareProvider"], true);
    assert_eq!(caps["hoverProvider"], true);
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["documentFormattingProvider"], true);
    assert_eq!(
        caps["codeActionProvider"]["codeActionKinds"],
        json!(["quickfix"])
    );
    // Unknown requests are errors, not hangs.
    let e = c.request_err("textDocument/frobnicate", json!({}));
    assert!(e.contains("unknown method"), "{e}");
}

#[test]
fn diagnostics_on_open_and_debounced_on_change() {
    let mut c = Client::shells();
    c.open("toasts.strand");
    assert_eq!(c.diagnostics("toasts.strand"), Vec::<Value>::new());

    // Three quick edits publish once, for the last text.
    let good = c.text("toasts.strand");
    let bad = good.replace("n.urgency == critical {", "n.urgency == critcal {");
    let worse = bad.replace("shown.len > 0", "shown.lenn > 0");
    c.change("toasts.strand", 2, &bad);
    c.change("toasts.strand", 3, &good);
    c.change("toasts.strand", 4, &worse);
    let diags = c.diagnostics("toasts.strand");
    let msgs = messages(&diags);
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(
        msgs.iter().any(|m| m.contains("did you mean `critical`?")),
        "{msgs:?}"
    );
    assert!(msgs.iter().any(|m| m.contains("lenn")), "{msgs:?}");
    assert_eq!(diags[0]["source"], "strand");
    assert!(
        c.diagnostics_within("toasts.strand", Duration::from_millis(300))
            .is_none(),
        "published more than once"
    );

    // A syntax error is reported with its range.
    let broken = good.replacen("{", "", 1);
    c.change("toasts.strand", 5, &broken);
    let diags = c.diagnostics("toasts.strand");
    assert!(
        diags
            .iter()
            .any(|d| d["code"].as_str().unwrap().starts_with("syntax::")),
        "{diags:?}"
    );
    c.change("toasts.strand", 6, &good);
    assert_eq!(c.diagnostics("toasts.strand"), Vec::<Value>::new());
}

#[test]
fn diagnostics_reach_other_files_of_the_config() {
    let mut c = Client::shells_edited(&[
        GLOW,
        (
            "launcher",
            "when hover    { bg: $surface.hi }",
            "when hover    { bg: $glow.soft }",
        ),
    ]);
    c.open("theme.strand");
    assert_eq!(c.diagnostics("theme.strand"), Vec::<Value>::new());
    // Renaming the token in theme.strand only breaks its reader elsewhere.
    let theme = c.text("theme.strand");
    let edited = theme.replace("glow { soft:", "glow { dim:");
    c.change("theme.strand", 2, &edited);
    let launcher = c.diagnostics("launcher.strand");
    assert!(
        messages(&launcher).iter().any(|m| m.contains("glow.soft")),
        "{launcher:?}"
    );
    // And clears them when undone.
    c.change("theme.strand", 3, &theme);
    assert_eq!(c.diagnostics("launcher.strand"), Vec::<Value>::new());
}

#[test]
fn completion_after_dollar() {
    let mut c = Client::shells();
    let text = c
        .text("bar.strand")
        .replace("gap: $space.3\n", "gap: $sp\n");
    c.open_text("bar.strand", &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "$sp\n", 0, 3));
    for want in [
        "space.2",
        "surface.hi",
        "radius.lg",
        "accent",
        "motion.bouncy",
    ] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    // A colour token's methods after its path.
    let text = text.replace("color: $fg ", "color: $fg.al ");
    c.change("bar.strand", 2, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "$fg.al", 0, 6));
    assert!(labels.iter().any(|l| l == "alpha"), "{labels:?}");
    assert!(labels.iter().any(|l| l == "fg.muted"), "{labels:?}");
}

#[test]
fn completion_after_dot() {
    let mut c = Client::shells();
    let base = c.text("bar.strand");
    // Nothing typed after the dot: the line does not parse yet.
    let text = base.replace("    icon battery.icon\n", "    icon battery.\n");
    c.open_text("bar.strand", &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "battery.\n", 0, 8));
    for want in ["percent", "charging", "time_left", "icon", "present"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    // Through a field, with a prefix typed.
    let text = base.replace("audio.sink.icon {", "audio.sink.ic {");
    c.change("bar.strand", 2, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "sink.ic {", 0, 7));
    for want in ["volume", "muted", "icon"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    // Methods of a service and of a builtin type.
    let text = base.replace("text clock.format(\"%a %d  %H:%M\") {", "text clock. {");
    c.change("bar.strand", 3, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "clock. {", 0, 6));
    assert!(labels.iter().any(|l| l == "format"), "{labels:?}");
    let text = base.replace(
        "text month.format(\"%B %Y\")",
        "text month.format(\"%B %Y\").",
    );
    c.change("bar.strand", 4, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "%Y\").", 0, 5));
    assert!(labels.iter().any(|l| l == "upper"), "{labels:?}");
    // A list's members, and an enum's variants.
    let text = base.replace(
        "if battery.present { Battery }",
        "if tray.items. { Battery }",
    );
    c.change("bar.strand", 5, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "items. {", 0, 6));
    for want in ["len", "first", "filter", "map"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    let osd = c
        .text("osd.strand")
        .replace("state kind = volume", "state kind = Kind.");
    c.open_text("osd.strand", &osd);
    let labels = c.completion("osd.strand", Client::pos(&osd, "Kind.", 0, 5));
    assert_eq!(labels, ["volume", "brightness"]);
    // Another file's exports.
    let text = base.replace("if battery.present { Battery }", "if launcher. { Battery }");
    c.change("bar.strand", 6, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "launcher. {", 0, 9));
    assert_eq!(labels, ["open"]);
    // Right after the dot, with a name already after the cursor.
    c.change("bar.strand", 7, &base);
    let labels = c.completion("bar.strand", Client::pos(&base, "battery.present", 0, 8));
    for want in ["percent", "present"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
}

#[test]
fn completion_after_two_way_offers_only_writable_places() {
    let mut c = Client::shells();
    let base = c.text("bar.strand");
    let text = base.replace("value: <-> audio.sink.volume", "value: <-> ");
    c.open_text("bar.strand", &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "value: <-> ", 0, 11));
    for want in [
        "audio.sink.volume",
        "audio.sink.muted",
        "launcher.open",
        "theme.look",
    ] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    for never in ["battery.percent", "clock", "Dot", "hits"] {
        assert!(
            !labels.iter().any(|l| l == never),
            "{never} offered: {labels:?}"
        );
    }
    // `open` is the Clock's own state; not visible from Volume.
    assert!(!labels.iter().any(|l| l == "open"), "{labels:?}");
    let text = base.replace(
        "popup { open: <-> open; Calendar }",
        "popup { open: <->  ; Calendar }",
    );
    c.change("bar.strand", 2, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "<->  ;", 0, 4));
    assert!(labels.iter().any(|l| l == "open"), "{labels:?}");
    // After a dot inside `<->`: writable members only.
    let text = base.replace("value: <-> audio.sink.volume", "value: <-> audio.sink.");
    c.change("bar.strand", 3, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "<-> audio.sink.", 0, 15));
    assert!(labels.iter().any(|l| l == "volume"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "icon"), "{labels:?}");
}

#[test]
fn completion_of_elements_props_and_events() {
    let mut c = Client::shells();
    let base = c.text("bar.strand");
    // A new line in a `row` block.
    let text = base.replace("    icon battery.icon\n", "    icon battery.icon\n    \n");
    c.open_text("bar.strand", &text);
    let labels = c.completion(
        "bar.strand",
        Client::pos(&text, "battery.icon\n    \n", 0, 17),
    );
    for want in [
        "text", "slider", "Dot", "Volume", "when", "for", "gap", "bg", "on",
    ] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    // `start` belongs in a `split` only; surfaces at the top level only.
    assert!(
        !labels.iter().any(|l| l == "start" || l == "bar"),
        "{labels:?}"
    );
    // A `when` block holds props only.
    let text = base.replace(
        "when ws.urgent   { bg: $error }",
        "when ws.urgent   { bg: $error; o }",
    );
    c.change("bar.strand", 2, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "$error; o }", 0, 9));
    assert!(labels.iter().any(|l| l == "opacity"), "{labels:?}");
    assert!(!labels.iter().any(|l| l == "text"), "{labels:?}");
    // A component call's block takes its parameters.
    let text = base.replace("{ Dot ws }", "{ Dot ws { w } }");
    c.change("bar.strand", 3, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "{ w }", 0, 3));
    assert!(labels.iter().any(|l| l == "ws"), "{labels:?}");
    // `Dot` has no `slot`, so no children are offered.
    assert!(
        !labels
            .iter()
            .any(|l| l == "text" || l == "box" || l == "if" || l == "Dot"),
        "{labels:?}"
    );
    // Events after `on`.
    let text = base.replace(
        "on secondary { item.menu.open() }",
        "on  { item.menu.open() }",
    );
    c.change("bar.strand", 4, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "on  {", 0, 3));
    for want in ["click", "secondary", "scroll", "change"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
    // A prop's enum value.
    let text = base.replace("edge: top;", "edge: ;");
    c.change("bar.strand", 5, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "edge: ;", 0, 6));
    assert_eq!(labels, ["top", "bottom", "left", "right"]);
    // Top-level keywords.
    let text = format!("{base}\n");
    c.change("bar.strand", 6, &text);
    let labels = c.completion("bar.strand", Client::pos(&text, "\n\n", 0, 1));
    for want in ["component", "bar", "state", "tokens"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
}

#[test]
fn hover_shows_types_and_schema_docs() {
    let mut c = Client::shells();
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    let h = c.hover("bar.strand", Client::pos(&bar, "battery.present", 0, 2));
    assert!(h.contains("service battery"), "{h}");
    assert!(h.len() > "service battery".len() + 20, "no schema doc: {h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "battery.percent", 0, 10));
    assert!(h.contains("percent: float"), "{h}");
    // Service fields and record members carry the schema's docs.
    assert!(h.contains("Charge, 0 to 1"), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "audio.sink.volume", 0, 13));
    assert!(
        h.contains("volume: float rw") && h.contains("Volume, 0 to 1"),
        "{h}"
    );
    let h = c.hover("bar.strand", Client::pos(&bar, "open = !open", 0, 1));
    assert!(h.contains("state open: bool"), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "$accent }", 0, 2));
    assert!(h.contains("$accent: color"), "{h}");
    assert!(h.contains("Material 3 `primary`"), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "edge: top", 0, 1));
    assert!(h.contains("edge: Edge"), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "Dot ws }", 0, 1));
    assert!(h.contains("component Dot(ws: Workspace)"), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "pct(battery", 0, 1));
    assert!(h.contains("fn pct("), "{h}");
    let h = c.hover("bar.strand", Client::pos(&bar, "slider {", 0, 1));
    assert!(h.contains("element slider"), "{h}");
    // A method, with its signature.
    let h = c.hover("bar.strand", Client::pos(&bar, "clock.format(\"%a", 0, 7));
    assert!(h.contains("format(") && h.contains("-> text"), "{h}");
    // A token defined in another file shows its value.
    let launcher = c.text("launcher.strand");
    c.open("launcher.strand");
    let h = c.hover(
        "launcher.strand",
        Client::pos(&launcher, "$surface.hi", 0, 3),
    );
    assert!(
        h.contains("$surface.hi: color") && h.contains("base: $surface.mix($fg, 8%)"),
        "{h}"
    );
    // An overridden token names each value's token set.
    let h = c.hover("bar.strand", Client::pos(&bar, "$space.2", 0, 3));
    assert!(
        h.contains("base: 8px") && h.contains("compact (override): 4px"),
        "{h}"
    );
    // A base-tier token reads its tier's doc.
    assert!(h.contains("The spacing scale"), "{h}");
    // A file's header comment is not the doc of its first declaration.
    let h = c.hover(
        "launcher.strand",
        Client::pos(&launcher, "state open", 0, 7),
    );
    assert!(h.contains("state open: bool"), "{h}");
    assert!(!h.contains("Bind a key"), "{h}");
}

#[test]
fn list_members_come_from_the_schema() {
    // Completion and hover read the checker's own member table
    // (`schema::members_of`), so a list's methods carry their docs and a
    // keyed list offers the keyed mutations.
    let src = "type Pin { id: int; label: text }\nstate pins: [Pin] key id = []\nlet few = pins.take(3)\n";
    let mut c = Client::start(&[("a.strand", src.into())]);
    c.open("a.strand");
    assert_eq!(c.diagnostics("a.strand"), Vec::<Value>::new());
    let h = c.hover("a.strand", Client::pos(src, "take(3)", 0, 1));
    assert!(h.contains("take(count: int)"), "{h}");
    assert!(h.contains("The first `count` items."), "{h}");
    let text = src.replace("pins.take(3)", "pins.");
    c.change("a.strand", 2, &text);
    let labels = c.completion("a.strand", Client::pos(&text, "pins.\n", 0, 5));
    for want in ["len", "first", "take", "filter", "remove_key", "update"] {
        assert!(
            labels.iter().any(|l| l == want),
            "{want} missing: {labels:?}"
        );
    }
}

#[test]
fn definition_across_files() {
    let mut c = Client::shells();
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    let params = c.at("bar.strand", Client::pos(&bar, "$space.2", 0, 3));
    let r = c.request("textDocument/definition", params);
    // Defined in `base` and overridden in `compact`.
    let locs = r.as_array().unwrap();
    assert_eq!(locs.len(), 2, "{r}");
    let theme = c.text("theme.strand");
    for (loc, at) in locs.iter().zip(["2: 8px", "2: 4px"]) {
        assert_eq!(loc["uri"], c.uri("theme.strand"));
        assert_eq!(loc["range"]["start"], json!(Client::pos(&theme, at, 0, 0)));
    }
    // A component, in the same file.
    let params = c.at("bar.strand", Client::pos(&bar, "Dot ws }", 0, 1));
    let r = c.request("textDocument/definition", params);
    assert_eq!(
        r[0]["range"]["start"],
        json!(Client::pos(&bar, "Dot(ws", 0, 0))
    );
    // A user enum's variant.
    let osd = c.text("osd.strand");
    c.open("osd.strand");
    let params = c.at("osd.strand", Client::pos(&osd, "kind = brightness", 0, 8));
    let r = c.request("textDocument/definition", params);
    assert_eq!(
        r[0]["range"]["start"],
        json!(Client::pos(&osd, "brightness }", 0, 0))
    );
}

#[test]
fn rename_state_let_component_and_token() {
    let mut c = Client::shells();
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    // The Clock's `open`, not the launcher's, nor the `open` prop or the
    // tray menu's `open()` method.
    let edit = c.rename(
        "bar.strand",
        Client::pos(&bar, "open = !open", 0, 0),
        "shown",
    );
    let out = c.applied(&edit, "bar.strand", &bar);
    assert!(out.contains("state shown = false"), "{out}");
    assert!(out.contains("on click { shown = !shown }"), "{out}");
    assert!(out.contains("popup { open: <-> shown; Calendar }"), "{out}");
    assert!(out.contains("item.menu.open()"), "{out}");
    assert!(
        edit["changes"].get(c.uri("launcher.strand")).is_none(),
        "{edit}"
    );

    // A component, at its declaration.
    let edit = c.rename("bar.strand", Client::pos(&bar, "Dot(ws", 0, 1), "Pip");
    let out = c.applied(&edit, "bar.strand", &bar);
    assert!(
        out.contains("component Pip(ws: Workspace)") && out.contains("{ Pip ws }"),
        "{out}"
    );

    // A let.
    let osd = c.text("osd.strand");
    c.open("osd.strand");
    let edit = c.rename(
        "osd.strand",
        Client::pos(&osd, "meter level", 0, 6),
        "amount",
    );
    let out = c.applied(&edit, "osd.strand", &osd);
    assert!(
        out.contains("let amount = kind") && out.contains("meter amount {"),
        "{out}"
    );
    assert!(out.contains("text pct(amount)"), "{out}");
}

/// Tokens the config declares (the schema's own names cannot be renamed).
const GLOW: (&str, &str, &str) = (
    "theme",
    "  border:           oklch(from $surface, l: l + 0.12)\n",
    "  border:           oklch(from $surface, l: l + 0.12)\n  glow { soft: $accent.alpha(0.4); hard: $accent }\n  card.fill: $surface.alpha(0.9)\n",
);

#[test]
fn rename_tokens_across_files() {
    let mut c = Client::shells_edited(&[
        GLOW,
        (
            "launcher",
            "when hover    { bg: $surface.hi }",
            "when hover    { bg: $glow.soft }",
        ),
        (
            "toasts",
            "when hover { bg: $surface.hi }",
            "when hover { bg: $card.fill; glow: 4, $glow.soft }",
        ),
    ]);
    let launcher = c.text("launcher.strand");
    let theme = c.text("theme.strand");
    let toasts = c.text("toasts.strand");
    c.open("launcher.strand");
    assert_eq!(c.diagnostics("launcher.strand"), Vec::<Value>::new());
    // A token inside a group: the key in the group and every reader.
    let edit = c.rename(
        "launcher.strand",
        Client::pos(&launcher, "$glow.soft", 0, 2),
        "$glow.dim",
    );
    assert!(
        c.applied(&edit, "theme.strand", &theme)
            .contains("glow { dim: $accent.alpha(0.4); hard")
    );
    assert!(
        c.applied(&edit, "launcher.strand", &launcher)
            .contains("bg: $glow.dim }")
    );
    assert!(
        c.applied(&edit, "toasts.strand", &toasts)
            .contains("glow: 4, $glow.dim }")
    );
    // From the key in its group, a bare name stays in the group.
    c.open("theme.strand");
    let edit = c.rename("theme.strand", Client::pos(&theme, "soft:", 0, 1), "dim");
    assert!(
        c.applied(&edit, "theme.strand", &theme)
            .contains("glow { dim: $accent.alpha(0.4); hard")
    );
    assert!(
        c.applied(&edit, "launcher.strand", &launcher)
            .contains("bg: $glow.dim }")
    );
    // The new name must stay in the group.
    let e = c.rename_err(
        "launcher.strand",
        Client::pos(&launcher, "$glow.soft", 0, 2),
        "halo.soft",
    );
    assert!(e.contains("inside the `glow` group"), "{e}");
    // A dotted key outside a group, renamed from its definition.
    let edit = c.rename(
        "theme.strand",
        Client::pos(&theme, "card.fill", 0, 1),
        "card.bg",
    );
    assert!(
        c.applied(&edit, "theme.strand", &theme)
            .contains("  card.bg: $surface.alpha(0.9)")
    );
    assert!(
        c.applied(&edit, "toasts.strand", &toasts)
            .contains("bg: $card.bg;")
    );
    // The schema's tokens stay.
    let e = c.rename_err(
        "launcher.strand",
        Client::pos(&launcher, "$surface.alpha", 0, 2),
        "base",
    );
    assert!(e.contains("cannot be renamed"), "{e}");
}

#[test]
fn rename_refuses_what_would_break_or_is_built_in() {
    let mut c = Client::shells();
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    let e = c.rename_err("bar.strand", Client::pos(&bar, "Dot(ws", 0, 1), "Volume");
    assert!(e.contains("would break the config"), "{e}");
    let e = c.rename_err(
        "bar.strand",
        Client::pos(&bar, "Dot(ws", 0, 1),
        "not a name",
    );
    assert!(e.contains("not a name"), "{e}");
    let params = c.at("bar.strand", Client::pos(&bar, "battery.present", 0, 1));
    let e = c.request_err("textDocument/prepareRename", params);
    assert!(e.contains("built in"), "{e}");
    let e = c.rename_err("bar.strand", Client::pos(&bar, "$accent }", 0, 2), "brand");
    assert!(e.contains("cannot be renamed"), "{e}");
    // prepareRename gives the name's range and text.
    let params = c.at("bar.strand", Client::pos(&bar, "Dot ws }", 0, 1));
    let r = c.request("textDocument/prepareRename", params);
    assert_eq!(r["placeholder"], "Dot");
}

#[test]
fn rename_refuses_to_capture_another_name() {
    let src = "state level = 1\ncomponent A {\n  let amount = 2\n  text pct(level) { opacity: amount }\n}\n";
    let mut c = Client::start(&[("a.strand", src.into())]);
    c.open("a.strand");
    assert_eq!(c.diagnostics("a.strand"), Vec::<Value>::new());
    // `let level` inside A would quietly take over `pct(level)`.
    let e = c.rename_err("a.strand", Client::pos(src, "amount", 0, 1), "level");
    assert!(e.contains("would change what other names refer to"), "{e}");
    let edit = c.rename("a.strand", Client::pos(src, "amount", 0, 1), "alpha");
    assert!(c.applied(&edit, "a.strand", src).contains("opacity: alpha"));
}

/// design.md, "What you see" #2: renaming a component's prop renames the
/// parameter, its reads, and the prop at every call site in other files.
#[test]
fn rename_a_component_prop_across_files() {
    let card =
        "component Card(expanded: bool = false) {\n  box { when expanded { height: 200 } }\n}\n";
    let side = "bar Side {\n  edge: left\n  Card { expanded: true }\n}\n";
    let mut c = Client::start(&[("card.strand", card.into()), ("side.strand", side.into())]);
    c.open("side.strand");
    assert_eq!(c.diagnostics("side.strand"), Vec::<Value>::new());
    let edit = c.rename("side.strand", Client::pos(side, "expanded", 0, 2), "open");
    assert_eq!(
        c.applied(&edit, "card.strand", card),
        "component Card(open: bool = false) {\n  box { when open { height: 200 } }\n}\n"
    );
    assert_eq!(
        c.applied(&edit, "side.strand", side),
        "bar Side {\n  edge: left\n  Card { open: true }\n}\n"
    );
    // Hover and definition on the call-site prop reach the parameter.
    let h = c.hover("side.strand", Client::pos(side, "expanded", 0, 1));
    assert!(h.contains("expanded: bool"), "{h}");
    let params = c.at("side.strand", Client::pos(side, "expanded", 0, 1));
    let r = c.request("textDocument/definition", params);
    assert_eq!(r[0]["uri"], c.uri("card.strand"));
}

#[test]
fn quick_fix_for_did_you_mean() {
    let mut c = Client::shells();
    let text = c
        .text("toasts.strand")
        .replace("n.urgency == critical {", "n.urgency == critcal {");
    c.open_text("toasts.strand", &text);
    let diags = c.diagnostics("toasts.strand");
    assert_eq!(diags.len(), 1, "{diags:?}");
    let range = diags[0]["range"].clone();
    let r = c.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": c.uri("toasts.strand") },
            "range": range,
            "context": { "diagnostics": diags },
        }),
    );
    let actions = r.as_array().unwrap();
    assert_eq!(actions.len(), 1, "{r}");
    assert_eq!(actions[0]["title"], "Change to `critical`");
    assert_eq!(actions[0]["kind"], "quickfix");
    let fixed = c.applied(&actions[0]["edit"], "toasts.strand", &text);
    assert_eq!(fixed, c.text("toasts.strand"));
}

/// The code actions offered for the diagnostics of `name`, opened with
/// `text`, asked for the whole text.
fn fixes_for(c: &mut Client, name: &str, text: &str) -> Vec<Value> {
    c.open_text(name, text);
    let diags = c.diagnostics(name);
    assert!(!diags.is_empty(), "no diagnostics in {text}");
    let r = c.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": c.uri(name) },
            "range": { "start": Client::pos(text, "", 0, 0), "end": Client::pos(text, "", 0, text.len()) },
            "context": { "diagnostics": diags },
        }),
    );
    r.as_array().unwrap().clone()
}

#[test]
fn quick_fix_for_a_renamed_parameter_and_a_misplaced_span() {
    let cal = "component Calendar(open: bool = false, day: int = 1) {\n  text \"x\"\n}\n";
    // design.md "What you see" #2: the call site still says `expanded`.
    let side = "component Side {\n  Calendar { day: 2; expanded: true }\n}\n";
    let mut c = Client::start(&[
        ("cal.strand", cal.to_string()),
        ("side.strand", side.to_string()),
        ("a.strand", String::new()),
    ]);
    let actions = fixes_for(&mut c, "side.strand", side);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["title"], "Change to `open`");
    assert_eq!(actions[0]["isPreferred"], true);
    assert_eq!(
        c.applied(&actions[0]["edit"], "side.strand", side),
        "component Side {\n  Calendar { day: 2; open: true }\n}\n"
    );
    // Two parameters left: both offered, neither preferred.
    let side = "component Side {\n  Calendar { expanded: true }\n}\n";
    c.change("side.strand", 2, side);
    let _ = c.diagnostics("side.strand");
    let actions = fixes_for(&mut c, "side.strand", side);
    let titles: Vec<&str> = actions
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Change to `open`", "Change to `day`"]);
    assert!(actions.iter().all(|a| a["isPreferred"] == false));
    // An unknown named argument of a function call, inside a component
    // call: the parameters left are the function's, not the component's.
    let side = "fn f(a: int, b: int) -> int { a + b }\n\
                component Side {\n  Calendar { day: f(a: 1, zzz: 2) }\n}\n";
    c.change("side.strand", 3, side);
    let _ = c.diagnostics("side.strand");
    let actions = fixes_for(&mut c, "side.strand", side);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["title"], "Change to `b`");
    assert_eq!(actions[0]["isPreferred"], true);
    assert_eq!(
        c.applied(&actions[0]["edit"], "side.strand", side),
        side.replace("zzz", "b")
    );
    // The parser points past the misspelt keyword; the fix still
    // replaces the keyword.
    let a = "component A {\n  on chnage a, b { x = 1 }\n}\n";
    let actions = fixes_for(&mut c, "a.strand", a);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["title"], "Change to `change`");
    assert_eq!(
        c.applied(&actions[0]["edit"], "a.strand", a),
        "component A {\n  on change a, b { x = 1 }\n}\n"
    );
}

#[test]
fn formatting_whole_documents() {
    let mut c = Client::shells();
    let messy = "state  open=false\nbar Top{edge:top;text \"x\"}\n";
    c.open_text("bar.strand", messy);
    let r = c.request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": c.uri("bar.strand") },
            "options": { "tabSize": 2, "insertSpaces": true },
        }),
    );
    let edits = r.as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(
        edits[0]["newText"],
        "state open = false\nbar Top { edge: top; text \"x\" }\n"
    );
    // Formatted already: no edits. Broken: none either.
    let theme = c.text("theme.strand");
    c.open("theme.strand");
    let params = json!({
        "textDocument": { "uri": c.uri("theme.strand") },
        "options": { "tabSize": 2, "insertSpaces": true },
    });
    assert_eq!(
        c.request("textDocument/formatting", params.clone()),
        json!([])
    );
    c.change("theme.strand", 2, &theme.replacen('}', "", 1));
    assert_eq!(c.request("textDocument/formatting", params), Value::Null);
}

#[test]
fn a_file_outside_the_workspace_is_checked_alone() {
    let c = Client::start(&[]);
    let other = TempDir::new();
    let path = other.0.join(".hidden.strand");
    std::fs::write(&path, "state a = b\n").unwrap();
    let uri = path_to_uri(&path);
    c.notify(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri, "languageId": "strand", "version": 1, "text": "state a = b\n",
        }}),
    );
    let n = loop {
        match c.conn.receiver.recv_timeout(WAIT).unwrap() {
            Message::Notification(n) if n.params["uri"] == uri.as_str() => break n,
            _ => {}
        }
    };
    let msgs = n.params["diagnostics"].as_array().unwrap();
    assert!(
        msgs[0]["message"].as_str().unwrap().contains("`b`"),
        "{msgs:?}"
    );
}

/// Every request at every few bytes of the shells answers without a
/// server panic, including inside half-typed text.
#[test]
fn requests_anywhere_never_fail() {
    let mut c = Client::shells();
    for name in ["bar", "osd", "theme"] {
        let file = format!("{name}.strand");
        let text = c.text(&file);
        // Also a damaged copy: every `.` followed by a gap.
        let damaged = text.replace('.', ". ");
        for (version, t) in [(1, &text), (2, &damaged)] {
            if version == 1 {
                c.open_text(&file, t);
            } else {
                c.change(&file, version, t);
            }
            let lines = Lines::new(t);
            for at in (0..t.len()).step_by(5) {
                let pos = lines.position(t, at as u32);
                let params = c.at(&file, pos);
                for method in [
                    "textDocument/completion",
                    "textDocument/hover",
                    "textDocument/definition",
                ] {
                    let id = c.send(method, params.clone());
                    let r = c.response(id);
                    assert!(r.response_result.is_ok(), "{method} at {pos:?} in {file}");
                }
                let id = c.send("textDocument/prepareRename", params);
                let r = c.response(id);
                if let Err(e) = r.response_result {
                    assert_eq!(e.code, -32803, "{}", e.message);
                }
            }
        }
    }
}

const READS_LAUNCHER: (&str, &str, &str) = (
    "bar",
    "if battery.present { Battery }",
    "if launcher.open { Battery } else if extra.on { Battery }",
);

/// Files changed on disk behind the editor's back (by `strand fmt`, git, or
/// another editor) are read again before a request is answered.
#[test]
fn files_changed_on_disk_are_seen() {
    let mut c = Client::shells_edited(&[READS_LAUNCHER]);
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    let diags = c.diagnostics("bar.strand");
    assert!(
        messages(&diags).iter().any(|m| m.contains("extra")),
        "{diags:?}"
    );
    let at = Client::pos(&bar, "launcher.open", 0, 10);
    assert!(c.hover("bar.strand", at).contains("state open"));
    // Two lines added at the top of an unopened file.
    let launcher = format!("// one\n// two\n{}", c.text("launcher.strand"));
    std::fs::write(c.dir.0.join("launcher.strand"), &launcher).unwrap();
    let edit = c.rename("bar.strand", at, "shown");
    let renamed = c.applied(&edit, "launcher.strand", &launcher);
    assert!(
        renamed.starts_with("// one\n// two\n// launcher.strand."),
        "{renamed}"
    );
    assert!(
        renamed.contains("\nexport state shown = false\n"),
        "{renamed}"
    );
    assert!(renamed.contains("open: <-> shown"), "{renamed}");
    assert!(!renamed.contains("export state open"), "{renamed}");
    // A file added next to the others joins the config.
    std::fs::write(c.dir.0.join("extra.strand"), "export state on = true\n").unwrap();
    let h = c.hover("bar.strand", Client::pos(&bar, "extra.on", 0, 7));
    assert!(h.contains("state on: bool"), "{h}");
    // What is shown catches up with what requests see, though the client
    // said nothing: the error about `extra` is cleared.
    loop {
        let diags = c.diagnostics("bar.strand");
        if !messages(&diags).iter().any(|m| m.contains("extra")) {
            break;
        }
    }
}

/// A change on disk the client never reported is still shown when an edit
/// to a document of another config came in between: the edit moves the
/// workspace on, and the stale config must not be rebuilt silently.
#[test]
fn disk_changes_survive_edits_elsewhere() {
    let mut c = Client::shells_edited(&[READS_LAUNCHER]);
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    let diags = c.diagnostics("bar.strand");
    assert!(
        messages(&diags).iter().any(|m| m.contains("extra")),
        "{diags:?}"
    );
    let untitled = "untitled:x";
    c.notify(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": untitled, "languageId": "strand", "version": 1, "text": "state a = 1\n",
        }}),
    );
    std::fs::write(c.dir.0.join("extra.strand"), "export state on = true\n").unwrap();
    c.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": untitled, "version": 2 },
            "contentChanges": [{ "text": "state a = 2\n" }],
        }),
    );
    let h = c.hover("bar.strand", Client::pos(&bar, "extra.on", 0, 7));
    assert!(h.contains("state on: bool"), "{h}");
    loop {
        let diags = c.diagnostics("bar.strand");
        if !messages(&diags).iter().any(|m| m.contains("extra")) {
            break;
        }
    }
}

/// An open file deleted from a config directory is checked alone from the
/// next watched-files event: what it shows ends up being its own errors,
/// not nothing.
#[test]
fn an_open_file_deleted_from_a_config_is_checked_alone() {
    let files = shell_files(&[]);
    let refs: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let mut c = Client::start_with(&refs, |p| {
        p["capabilities"] =
            json!({ "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } });
    });
    let text = "export let shown_too = launcher.open\n";
    std::fs::write(c.dir.0.join("new.strand"), text).unwrap();
    c.open_text("new.strand", text);
    assert_eq!(c.diagnostics("new.strand"), Vec::<Value>::new());
    std::fs::remove_file(c.dir.0.join("new.strand")).unwrap();
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": c.uri("new.strand"), "type": 3 }] }),
    );
    let mut last = None;
    while let Some(d) = c.diagnostics_within("new.strand", Duration::from_millis(1500)) {
        last = Some(d);
    }
    let last = last.expect("new.strand was never re-published");
    assert!(
        messages(&last).iter().any(|m| m.contains("`launcher`")),
        "{last:?}"
    );
}

/// A document opened before its file exists is checked alone; saved into
/// a config directory, it moves to that config, and a later watched-file
/// event never brings back what it showed alone.
#[test]
fn a_file_saved_into_a_config_moves_to_it() {
    let files = shell_files(&[]);
    let refs: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let mut c = Client::start_with(&refs, |p| {
        p["capabilities"] =
            json!({ "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } });
    });
    let text = "export let shown_too = launcher.open\n";
    c.open_text("new.strand", text);
    let diags = c.diagnostics("new.strand");
    assert!(
        messages(&diags).iter().any(|m| m.contains("`launcher`")),
        "{diags:?}"
    );
    std::fs::write(c.dir.0.join("new.strand"), text).unwrap();
    c.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": c.uri("new.strand") } }),
    );
    assert_eq!(c.diagnostics("new.strand"), Vec::<Value>::new());
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": c.uri("new.strand"), "type": 1 }] }),
    );
    let mut seen = 0;
    while let Some(d) = c.diagnostics_within("new.strand", Duration::from_millis(1500)) {
        assert_eq!(d, Vec::<Value>::new());
        seen += 1;
    }
    assert!(seen >= 1, "the watched-file event re-checks the config");
}

/// A client that watches files gets a registration for `.strand` files,
/// and a change it reports is re-checked.
#[test]
fn watched_files_are_registered_and_rechecked() {
    let files = shell_files(&[]);
    let refs: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let mut c = Client::start_with(&refs, |p| {
        p["capabilities"] =
            json!({ "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } });
    });
    c.open("bar.strand");
    assert_eq!(c.diagnostics("bar.strand"), Vec::<Value>::new());
    assert_eq!(c.server_requests, ["client/registerCapability"]);
    let theme = format!("{}state broken = nope\n", c.text("theme.strand"));
    std::fs::write(c.dir.0.join("theme.strand"), theme).unwrap();
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": c.uri("theme.strand"), "type": 2 }] }),
    );
    let diags = c.diagnostics("theme.strand");
    assert!(
        messages(&diags).iter().any(|m| m.contains("`nope`")),
        "{diags:?}"
    );
}

/// With a client that takes `documentChanges`, edits to open documents
/// carry their version, so stale edits are refused by the client.
#[test]
fn rename_edits_are_versioned() {
    let files = shell_files(&[READS_LAUNCHER]);
    let refs: Vec<(&str, String)> = files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
    let mut c = Client::start_with(&refs, |p| {
        p["capabilities"] =
            json!({ "workspace": { "workspaceEdit": { "documentChanges": true } } });
    });
    let bar = c.text("bar.strand");
    c.open("bar.strand");
    c.change("bar.strand", 3, &bar);
    let edit = c.rename(
        "bar.strand",
        Client::pos(&bar, "launcher.open", 0, 10),
        "shown",
    );
    assert!(edit.get("changes").is_none_or(Value::is_null), "{edit}");
    let docs = edit["documentChanges"].as_array().unwrap();
    let version = |name: &str| {
        docs.iter()
            .find(|d| d["textDocument"]["uri"] == c.uri(name))
            .map(|d| d["textDocument"]["version"].clone())
    };
    assert_eq!(version("bar.strand"), Some(json!(3)));
    assert_eq!(version("launcher.strand"), Some(Value::Null));
    assert!(
        c.applied(&edit, "bar.strand", &bar)
            .contains("launcher.shown")
    );
}

/// A notification with bad params is logged; the server carries on.
#[test]
fn bad_notifications_do_not_stop_the_server() {
    let mut c = Client::shells();
    let bar = c.text("bar.strand");
    c.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": c.uri("bar.strand"), "version": 1, "text": bar } }),
    );
    c.notify("textDocument/didChange", json!({ "textDocument": 7 }));
    let h = c.hover("bar.strand", Client::pos(&bar, "open = !open", 0, 1));
    assert!(h.contains("state open: bool"), "{h}");
    let logged: Vec<&Notification> = c
        .queue
        .iter()
        .filter(|n| n.method == "window/logMessage")
        .collect();
    assert_eq!(logged.len(), 2, "{logged:?}");
}

/// Configs are found as `strand check <file>` finds them: sibling
/// configs in one workspace folder stay apart, and a file deep in the
/// default config directory is checked with all of it.
#[test]
fn configs_are_found_as_strand_check_finds_them() {
    let mut c = Client::start(&[
        ("a/one.strand", "let x = two.shared\n".into()),
        ("b/two.strand", "export let shared = 1\n".into()),
    ]);
    c.open("a/one.strand");
    let diags = c.diagnostics("a/one.strand");
    assert!(
        messages(&diags).iter().any(|m| m.contains("`two`")),
        "{diags:?}"
    );

    let mut c = Client::start_with(
        &[
            ("cfg/root.strand", "export let shared = 1\n".into()),
            ("cfg/widgets/clock.strand", "let x = root.shared\n".into()),
        ],
        |p| {
            let root = strand_dev::text::uri_to_path(p["rootUri"].as_str().unwrap()).unwrap();
            p["rootUri"] = Value::Null;
            p["workspaceFolders"] = Value::Null;
            p["initializationOptions"]["configDir"] = json!(root.join("cfg"));
        },
    );
    c.open("cfg/widgets/clock.strand");
    assert_eq!(
        c.diagnostics("cfg/widgets/clock.strand"),
        Vec::<Value>::new()
    );
}

/// Closing the last open file of a config outside the workspace clears
/// its diagnostics, and a pending debounce does not bring them back.
#[test]
fn closing_clears_diagnostics_outside_the_workspace() {
    let c = Client::start(&[]);
    let other = TempDir::new();
    std::fs::write(other.0.join("x.strand"), "state a = b\n").unwrap();
    std::fs::write(other.0.join("y.strand"), "state y = 1\n").unwrap();
    let uri = path_to_uri(&other.0.join("x.strand"));
    let doc = |text: &str, version: i32| {
        json!({ "textDocument": {
            "uri": uri, "languageId": "strand", "version": version, "text": text,
        }})
    };
    c.notify("textDocument/didOpen", doc("state a = b\n", 1));
    c.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "state a = c\n" }],
        }),
    );
    c.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    // Every diagnostics notification for it, until none comes for a while.
    let mut last = None;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match c.conn.receiver.recv_timeout(left) {
            Ok(Message::Notification(n)) if n.params["uri"] == uri.as_str() => {
                last = Some(n.params["diagnostics"].clone());
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(last, Some(json!([])));
}

/// `strand-dev lsp` speaks the protocol on stdin and stdout.
#[test]
fn the_binary_serves_stdio() {
    use std::io::{BufReader, Write};
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_strand-dev"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let send = |w: &mut std::process::ChildStdin, m: Message| {
        m.write(w).unwrap();
        w.flush().unwrap();
    };
    send(
        &mut stdin,
        Message::Request(Request::new(
            1.into(),
            "initialize".into(),
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        )),
    );
    let Some(Message::Response(r)) = Message::read(&mut stdout).unwrap() else {
        panic!("no initialize response")
    };
    assert_eq!(
        r.response_result.unwrap()["serverInfo"]["name"],
        "strand-dev"
    );
    send(
        &mut stdin,
        Message::Notification(Notification::new("initialized".into(), json!({}))),
    );
    send(
        &mut stdin,
        Message::Request(Request::new(2.into(), "shutdown".into(), Value::Null)),
    );
    let Some(Message::Response(r)) = Message::read(&mut stdout).unwrap() else {
        panic!("no shutdown response")
    };
    assert_eq!(r.id, 2.into());
    send(
        &mut stdin,
        Message::Notification(Notification::new("exit".into(), Value::Null)),
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
