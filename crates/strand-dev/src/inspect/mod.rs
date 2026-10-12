//! `strand-dev inspect`: the inspector's terminal client (design.md,
//! "Developer experience"; m5-owner Q9: the panel is a terminal's, so it
//! never weighs on the shell). It runs pick mode over the shell's socket,
//! prints the picked node's pane, keeps it outlined, and offers reset
//! subtree, open in `$EDITOR`, damage flashing and write-back
//! ([`write`]).
//!
//! The terminal stays in line mode: a command is a line. Enter (or
//! Escape, then Enter) ends pick mode; Ctrl-C ends the client, and the
//! shell ends pick mode, the outline and the flash on the disconnect.

pub mod client;
pub mod pane;
pub mod write;

use std::io::{self, BufRead as _, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde_json::Value as Json;
use strand_core::ipc::{Request, refusal};

use client::Connection;

/// A line to the session's terminal (a failed write is not the
/// shell's problem, so it is dropped).
macro_rules! say {
    ($s:expr, $($t:tt)*) => {{
        let _ = writeln!($s.out, $($t)*);
    }};
}

/// How long an answer may take.
const WAIT: Duration = Duration::from_secs(5);

const HELP: &str = "\
  p                   pick a node (click it; Enter or Escape stops picking)
  r                   reset the node's subtree (its state back to defaults)
  e                   open the node's source in $EDITOR
  set <prop> <value>  write a literal prop value back to the node's source
  token <path> <val>  write a token's literal value back to its definition
  f                   damage flashing on or off
  q                   quit (Ctrl-C too)";

/// What the loop waits on.
enum Ev {
    /// A line from the terminal (`None`: end of input).
    Line(Option<String>),
    /// Something arrived on the socket.
    Socket,
}

/// Opens a file at a line and waits for the editor.
pub type Editor = Box<dyn FnMut(&Path, u64) + Send>;

/// The terminal [`drive`] talks to: lines in, text out, and `$EDITOR`.
pub struct Terminal {
    /// The next line typed (`None`: end of input). Called on a thread of
    /// its own, and again only once the last line is handled, so
    /// `$EDITOR` gets the terminal to itself.
    pub read_line: Box<dyn FnMut() -> Option<String> + Send>,
    /// Where the client prints.
    pub out: Box<dyn Write + Send>,
    /// Opens a file at a line and waits for the editor.
    pub editor: Editor,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Terminal")
    }
}

/// Runs the client against the socket at `socket` (default: the
/// shell's, by `strand_core::ipc::socket_path`'s rule), on this
/// process's terminal.
pub fn run(socket: Option<PathBuf>) -> Result<(), String> {
    let path = socket
        .or_else(strand_core::ipc::socket_path)
        .ok_or("no socket: set $STRAND_SOCKET, or $XDG_RUNTIME_DIR and $WAYLAND_DISPLAY")?;
    drive(
        &path,
        Terminal {
            read_line: Box::new(|| {
                let mut line = String::new();
                match io::stdin().lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                }
            }),
            out: Box::new(io::stdout()),
            editor: Box::new(open_editor),
        },
    )
}

/// Runs the client against the socket at `path` on `term` until it
/// quits (`q`, or the end of input).
pub fn drive(path: &Path, term: Terminal) -> Result<(), String> {
    let Terminal {
        mut read_line,
        out,
        editor,
    } = term;
    let (tx, rx) = mpsc::channel();
    let notify = tx.clone();
    let conn = Connection::connect(
        path,
        Some(Arc::new(move || {
            let _ = notify.send(Ev::Socket);
        })),
    )
    .map_err(|e| format!("{}: {e} (is `strand run` running?)", path.display()))?;
    // The terminal is read a line at a time, and the next line only once
    // the last is handled, so `$EDITOR` gets the terminal to itself.
    let (ack, acked) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("inspect-stdin".into())
        .spawn(move || {
            loop {
                let got = read_line();
                let end = got.is_none();
                if tx.send(Ev::Line(got)).is_err() || end || acked.recv().is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let mut s = Session {
        conn,
        out,
        editor,
        node: None,
        picking: false,
        flash: false,
        offer: None,
    };
    say!(
        s,
        "strand-dev inspect: connected to {}\n{HELP}",
        path.display()
    );
    s.pick(true)?;
    if s.conn.closed() {
        return Err("the shell closed the connection".into());
    }
    for ev in rx {
        match ev {
            Ev::Socket => s.events()?,
            Ev::Line(None) => break,
            Ev::Line(Some(l)) => {
                if !s.command(l.trim())? {
                    break;
                }
                let _ = ack.send(());
            }
        }
        if s.conn.closed() {
            return Err("the shell closed the connection".into());
        }
    }
    Ok(())
}

/// One client's state.
struct Session {
    conn: Connection,
    out: Box<dyn Write + Send>,
    editor: Editor,
    /// The pane shown: an `inspect.node` answer.
    node: Option<Json>,
    picking: bool,
    flash: bool,
    /// A read-only file write-back refused, offered to `$EDITOR`.
    offer: Option<(PathBuf, u64)>,
}

impl Session {
    fn ask(&mut self, req: &Request) -> Result<Json, String> {
        let a = match self.conn.ask(req, WAIT) {
            Ok(a) => a,
            Err(e) if e.kind() == io::ErrorKind::TimedOut && !self.conn.closed() => {
                return Err(format!("{e}{}", self.restore()));
            }
            Err(e) => return Err(e.to_string()),
        };
        match refusal(&a) {
            Some(why) => Err(format!("{}: {why}", req.cmd())),
            None => Ok(a),
        }
    }

    /// After the connection started again (a request timed out, so the
    /// shell ended the old one's pick mode, outline and flash): asks for
    /// them again on the new one, and says which are off now. Pick mode
    /// and flashing are on here only if they were before the request
    /// that timed out and it did not turn them off; the outline is on
    /// the node shown.
    fn restore(&mut self) -> String {
        let want = [
            (self.picking, "pick mode", Request::InspectPick { on: true }),
            (
                self.node.is_some(),
                "the outline",
                Request::InspectHighlight {
                    id: Some(self.node_id()),
                },
            ),
            (self.flash, "flashing", Request::InspectFlash { on: true }),
        ];
        (self.picking, self.flash) = (false, false);
        let mut off = Vec::new();
        let mut gone = false;
        for (i, (on, what, req)) in want.into_iter().enumerate() {
            if !on {
                continue;
            }
            // A second timeout started yet another connection: the rest
            // stays off rather than wait again.
            let back = !gone
                && match self.conn.ask(&req, WAIT) {
                    Ok(a) => refusal(&a).is_none(),
                    Err(_) => {
                        gone = true;
                        false
                    }
                };
            match (back, i) {
                (true, 0) => self.picking = true,
                (true, 2) => self.flash = true,
                (true, _) => {}
                (false, _) => off.push(what),
            }
        }
        match off.is_empty() {
            true => String::new(),
            false => format!(" ({} off now)", off.join(", ")),
        }
    }

    fn pick(&mut self, on: bool) -> Result<(), String> {
        // Off is off on this side whatever the answer: a timeout's new
        // connection ends pick mode too, and must not turn it back on.
        if !on {
            self.picking = false;
        }
        if let Err(e) = self.ask(&Request::InspectPick { on }) {
            say!(self, "{e}");
            return Ok(());
        }
        // A `pick_ended` that came before the answer ends the pick mode
        // before this one: the shell answers `inspect.pick` as it acts
        // on it, and ends the new mode only after (inspect-r2-1).
        self.conn.forget("pick_ended");
        self.picking = on;
        if on {
            say!(self, "picking: click a node on any output (Enter stops)");
        }
        Ok(())
    }

    /// The events that came on the socket.
    fn events(&mut self) -> Result<(), String> {
        while let Some(e) = self.conn.event(Duration::ZERO).map_err(|e| e.to_string())? {
            match e["event"].as_str() {
                Some("picked") => match e["node"].as_str() {
                    Some(id) => {
                        let id = id.to_string();
                        // One pick, then the shell's input is its own again.
                        self.pick(false)?;
                        self.show(&id)?;
                    }
                    None => say!(self, "nothing pickable there"),
                },
                Some("pick_ended") if self.picking => {
                    self.picking = false;
                    say!(
                        self,
                        "pick mode ended by the shell (a lock, or the surface went away)"
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn show(&mut self, id: &str) -> Result<(), String> {
        let answer = match self.ask(&Request::InspectNode { id: id.into() }) {
            Ok(a) => a,
            Err(e) => {
                say!(self, "{e}");
                return Ok(());
            }
        };
        let _ = write!(self.out, "\n{}", pane::pane(&answer));
        let _ = self.out.flush();
        // Shown before it is outlined, so a timeout's new connection
        // outlines this node, not the one before.
        self.node = Some(answer);
        if let Err(e) = self.ask(&Request::InspectHighlight {
            id: Some(id.into()),
        }) {
            say!(self, "{e}");
        }
        Ok(())
    }

    /// Handles one line; `false` quits.
    fn command(&mut self, line: &str) -> Result<bool, String> {
        if let Some((file, at)) = self.offer.take() {
            if matches!(line, "y" | "Y" | "yes") {
                (self.editor)(&file, at);
            }
            return Ok(true);
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match cmd {
            "" | "\u{1b}" if self.picking => self.pick(false)?,
            "" => {}
            "q" | "quit" => return Ok(false),
            "?" | "h" | "help" => say!(self, "{HELP}"),
            "p" | "pick" => self.pick(true)?,
            "f" | "flash" => {
                let on = !self.flash;
                // As for pick mode: off is off here whatever the answer.
                self.flash = false;
                match self.ask(&Request::InspectFlash { on }) {
                    Ok(_) => {
                        self.flash = on;
                        say!(self, "damage flashing {}", if on { "on" } else { "off" });
                    }
                    Err(e) => say!(self, "{e}"),
                }
            }
            _ if self.node.is_none() => say!(self, "pick a node first (p)"),
            "r" | "reset" => {
                let id = self.node_id();
                match self.ask(&Request::ResetNode { node: id.clone() }) {
                    Ok(_) => {
                        say!(self, "reset {id}'s subtree");
                        self.show(&id)?;
                    }
                    Err(e) => say!(self, "{e}"),
                }
            }
            "e" | "edit" => match self.origin() {
                Some((file, line)) => (self.editor)(&file, line),
                None => say!(self, "the node has no source file"),
            },
            "set" | "token" => {
                let Some((what, value)) = rest.split_once(' ') else {
                    say!(
                        self,
                        "usage: {cmd} <{}> <value>",
                        if cmd == "set" { "prop" } else { "path" }
                    );
                    return Ok(true);
                };
                let edit = match cmd {
                    "set" => self.prop_edit(what, value.trim()),
                    _ => self.token_edit(what.trim_start_matches('$'), value.trim()),
                };
                let edit = match edit {
                    Ok(e) => e,
                    Err(r) => {
                        say!(self, "{r}");
                        return Ok(true);
                    }
                };
                match write::write(&edit) {
                    Ok(file) => say!(self, "wrote {}: the shell reloads it", file.display()),
                    Err(r) => {
                        say!(self, "{r}");
                        if let Some(f) = r.editor_offer() {
                            let _ = write!(self.out, "open {} in $EDITOR? [y/N] ", f.display());
                            let _ = self.out.flush();
                            // At the edited value's line: the token's
                            // definition, not the node's origin.
                            self.offer = Some((f.to_path_buf(), line_of(&edit)));
                        }
                    }
                }
            }
            _ => say!(self, "unknown command `{cmd}` (? for help)"),
        }
        Ok(true)
    }

    fn node_id(&self) -> String {
        let n = self.node.as_ref().map(|a| &a["node"]["id"]);
        n.and_then(Json::as_str).unwrap_or_default().to_string()
    }

    fn origin(&self) -> Option<(PathBuf, u64)> {
        let o = &self.node.as_ref()?["node"]["origin"];
        Some((
            PathBuf::from(o["file"].as_str()?),
            o["line"].as_u64().unwrap_or(1),
        ))
    }

    fn prop_edit(&self, prop: &str, value: &str) -> Result<write::Edit, write::Refusal> {
        let a = self
            .node
            .as_ref()
            .ok_or_else(|| write::Refusal::NotFound("a node".into()))?;
        write::prop_edit(&a["node"], &a["files"], prop, value)
    }

    fn token_edit(&self, path: &str, value: &str) -> Result<write::Edit, write::Refusal> {
        let a = self
            .node
            .as_ref()
            .ok_or_else(|| write::Refusal::NotFound("a node".into()))?;
        // Where the node's chain says the token is defined.
        let origin = a["node"]["tokens"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|t| t["chain"].as_array().into_iter().flatten())
            .filter(|s| s["path"] == path)
            .find_map(|s| s["origin"].as_str())
            .ok_or_else(|| {
                write::Refusal::NotFound(format!("`${path}` among the tokens the node reads"))
            })?;
        write::token_edit(&a["files"], path, origin, value)
    }
}

/// The 1-based line of the file `edit` changes where its span starts
/// (1 when the file cannot be read).
fn line_of(edit: &write::Edit) -> u64 {
    std::fs::read(&edit.file).map_or(1, |b| {
        let upto = &b[..edit.span.start.min(b.len())];
        1 + upto.iter().filter(|c| **c == b'\n').count() as u64
    })
}

/// Opens `file` at `line` in `$VISUAL` or `$EDITOR` (`vi` without
/// either), in this terminal, and waits for it.
fn open_editor(file: &Path, line: u64) {
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    let mut words = editor.split_whitespace();
    let Some(prog) = words.next() else { return };
    let status = std::process::Command::new(prog)
        .args(words)
        .arg(format!("+{line}"))
        .arg(file)
        .status();
    if let Err(e) = status {
        println!("{editor}: {e}");
    }
}
