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

use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value as Json;
use strand_core::ipc::{Request, refusal};

use client::Connection;

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

/// Runs the client against the socket at `socket` (default: the
/// shell's, by `strand_core::ipc::socket_path`'s rule).
pub fn run(socket: Option<PathBuf>) -> Result<(), String> {
    let path = socket
        .or_else(strand_core::ipc::socket_path)
        .ok_or("no socket: set $STRAND_SOCKET, or $XDG_RUNTIME_DIR and $WAYLAND_DISPLAY")?;
    let (tx, rx) = mpsc::channel();
    let notify = tx.clone();
    let conn = Connection::connect(
        &path,
        Some(Box::new(move || {
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
            let stdin = io::stdin();
            loop {
                let mut line = String::new();
                let got = match stdin.lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => Some(line),
                };
                let end = got.is_none();
                if tx.send(Ev::Line(got)).is_err() || end || acked.recv().is_err() {
                    return;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let mut s = Session {
        conn,
        node: None,
        picking: false,
        flash: false,
        offer: None,
    };
    println!(
        "strand-dev inspect: connected to {}\n{HELP}",
        path.display()
    );
    s.pick(true)?;
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
    /// The pane shown: an `inspect.node` answer.
    node: Option<Json>,
    picking: bool,
    flash: bool,
    /// A read-only file write-back refused, offered to `$EDITOR`.
    offer: Option<(PathBuf, u64)>,
}

impl Session {
    fn ask(&mut self, req: &Request) -> Result<Json, String> {
        let a = self.conn.ask(req, WAIT).map_err(|e| e.to_string())?;
        match refusal(&a) {
            Some(why) => Err(format!("{}: {why}", req.cmd())),
            None => Ok(a),
        }
    }

    fn pick(&mut self, on: bool) -> Result<(), String> {
        if let Err(e) = self.ask(&Request::InspectPick { on }) {
            println!("{e}");
            return Ok(());
        }
        self.picking = on;
        if on {
            println!("picking: click a node on any output (Enter stops)");
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
                    None => println!("nothing pickable there"),
                },
                Some("pick_ended") if self.picking => {
                    self.picking = false;
                    println!("pick mode ended by the shell (a lock, or the surface went away)");
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
                println!("{e}");
                return Ok(());
            }
        };
        let _ = self.ask(&Request::InspectHighlight {
            id: Some(id.into()),
        });
        print!("\n{}", pane::pane(&answer));
        let _ = io::stdout().flush();
        self.node = Some(answer);
        Ok(())
    }

    /// Handles one line; `false` quits.
    fn command(&mut self, line: &str) -> Result<bool, String> {
        if let Some((file, at)) = self.offer.take() {
            if matches!(line, "y" | "Y" | "yes") {
                open_editor(&file, at);
            }
            return Ok(true);
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match cmd {
            "" | "\u{1b}" if self.picking => self.pick(false)?,
            "" => {}
            "q" | "quit" => return Ok(false),
            "?" | "h" | "help" => println!("{HELP}"),
            "p" | "pick" => self.pick(true)?,
            "f" | "flash" => {
                let on = !self.flash;
                match self.ask(&Request::InspectFlash { on }) {
                    Ok(_) => {
                        self.flash = on;
                        println!("damage flashing {}", if on { "on" } else { "off" });
                    }
                    Err(e) => println!("{e}"),
                }
            }
            _ if self.node.is_none() => println!("pick a node first (p)"),
            "r" | "reset" => {
                let id = self.node_id();
                match self.ask(&Request::ResetNode { node: id.clone() }) {
                    Ok(_) => {
                        println!("reset {id}'s subtree");
                        self.show(&id)?;
                    }
                    Err(e) => println!("{e}"),
                }
            }
            "e" | "edit" => match self.origin() {
                Some((file, line)) => open_editor(&file, line),
                None => println!("the node has no source file"),
            },
            "set" | "token" => {
                let Some((what, value)) = rest.split_once(' ') else {
                    println!(
                        "usage: {cmd} <{}> <value>",
                        if cmd == "set" { "prop" } else { "path" }
                    );
                    return Ok(true);
                };
                let edit = match cmd {
                    "set" => self.prop_edit(what, value.trim()),
                    _ => self.token_edit(what.trim_start_matches('$'), value.trim()),
                };
                match edit.and_then(|e| write::write(&e)) {
                    Ok(file) => println!("wrote {}: the shell reloads it", file.display()),
                    Err(r) => {
                        println!("{r}");
                        if let Some(f) = r.editor_offer() {
                            print!("open {} in $EDITOR? [y/N] ", f.display());
                            let _ = io::stdout().flush();
                            self.offer = Some((f.to_path_buf(), self.origin().map_or(1, |o| o.1)));
                        }
                    }
                }
            }
            _ => println!("unknown command `{cmd}` (? for help)"),
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
