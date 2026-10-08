//! The error overlay (design.md, "Errors"): diagnostics that survive
//! 250 ms of quiet open a dismissible panel listing every one of them,
//! with their labels and did-you-mean fixes; clicking a line opens
//! `$EDITOR` at that file and line. A save that is fixed within the
//! quiet period (format-on-save, delete-then-create) never shows it, and
//! the fixing commit takes it away.
//!
//! The panel is made of nodes outside the program
//! ([`Instance::external_create`]): they share the instance's ids and
//! diff, and survive reloads. Layout is placed by hand (`x`/`y`) until
//! flex layout lands (M2).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use strand_compiler::diagnostic::{Diagnostic, Severity};
use strand_compiler::instantiate::Instance;
use strand_compiler::reconcile::{KeptCell, Report};
use strand_compiler::source::SourceMap;
use strand_scene::{Color, Font, NodeId, NodeKind, Prop, PropValue};

/// How long diagnostics must stand before the overlay opens.
pub const QUIET: Duration = Duration::from_millis(250);

/// Rows shown at most (the rest are counted in the header) until the
/// panel can scroll (M2 `scroll`; decisions.md, wave2-runtime).
const MAX_ROWS: usize = 40;
/// Reload notice rows kept at most (the newest).
const MAX_NOTES: usize = MAX_ROWS;
const ROW_H: f32 = 18.0;
const PAD: f32 = 12.0;
const WIDTH: f32 = 960.0;

/// One overlay line: a diagnostic, one of its labels or its help, or a
/// reload notice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub text: String,
    /// Where a click goes (file, 1-based line and column).
    pub at: Option<(PathBuf, u32, u32)>,
    /// An error's own line (drawn in the error colour).
    pub error: bool,
    /// A reload notice (drawn in the warning colour).
    pub notice: bool,
    /// The state cell a click on its `[reset]` resets
    /// (`launcher.query: kept "fir" (default changed) [reset]`).
    pub reset: Option<String>,
    /// The state cell the row is about: a newer row for the same cell
    /// replaces it, and resetting the cell removes it.
    pub cell: Option<String>,
    /// The settings file and field whose runtime overlay a click on its
    /// `[clear]` drops (`accent: file changed but runtime overlay wins
    /// [clear]`).
    pub clear: Option<(String, String)>,
    /// The settings file (or runtime overlay file) whose next read
    /// decides this row (a bad value, a syntax error, an unreadable
    /// file): a read that no longer reports it takes it away.
    pub settings_read: Option<String>,
}

/// The row key of the notice that edits wait for the unlock (not a
/// cell path: removed when the waiting build lands).
pub const WAITS_FOR_UNLOCK: &str = "(waits for the unlock)";

/// A reload notice as an overlay row.
pub fn notice_line(n: &str) -> Line {
    Line {
        text: n.to_string(),
        notice: true,
        ..Line::default()
    }
}

/// A service's notice (another notification server owns the name) as
/// overlay rows, wrapped to the panel's width; the rows are keyed by the
/// service (`service:<name>#<n>`), so [`Overlay::forget_service`] takes
/// them away once the notice is resolved.
pub fn service_lines(service: &str, text: &str) -> Vec<Line> {
    // The panel's text is 12 px monospace, about 7.2 px a character.
    let width = ((WIDTH - PAD * 2.0) / 7.4) as usize;
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        if !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
            rows.push(std::mem::take(&mut row));
            row.push_str("  ");
        } else if !row.is_empty() && row != "  " {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.trim().is_empty() {
        rows.push(row);
    }
    rows.into_iter()
        .enumerate()
        .map(|(i, text)| Line {
            text,
            notice: true,
            cell: Some(format!("{SERVICE_CELL}{service}#{i}")),
            ..Line::default()
        })
        .collect()
}

/// The cell key prefix of a service's rows.
const SERVICE_CELL: &str = "service:";

/// A settings-file notice (a bad value kept at its last good value, a
/// syntax error, a read-only file whose changes go to an overlay, a file
/// change the runtime overlay shadows) as an overlay row. Rows about one
/// file and field replace each other; a shadowed field's row carries its
/// `[clear]`.
pub fn settings_line(n: &strand_core::SettingsNotice) -> Line {
    let key = match &n.field {
        Some(f) => format!("settings:{}#{f}", n.file),
        None => format!("settings:{}", n.file),
    };
    let clear = match (&n.issue, &n.field) {
        (strand_core::SettingsIssue::Shadowed, Some(f)) => {
            Some((n.file.to_string(), f.to_string()))
        }
        _ => None,
    };
    let settings_read = match &n.issue {
        strand_core::SettingsIssue::Syntax(_)
        | strand_core::SettingsIssue::Unreadable(_)
        | strand_core::SettingsIssue::BadValue(_) => Some(n.file.to_string()),
        _ => None,
    };
    Line {
        text: n.to_string(),
        notice: true,
        cell: Some(key),
        clear,
        settings_read,
        ..Line::default()
    }
}

/// A cell kept over a changed default: its row, whose `[reset]` resets
/// it.
pub fn kept_line(k: &KeptCell) -> Line {
    Line {
        text: k.notice(),
        notice: true,
        reset: Some(k.path.clone()),
        cell: Some(k.path.clone()),
        ..Line::default()
    }
}

/// The overlay rows of notices that came with `kept` cells: the kept
/// cells' rows (from the structured record, not the text), then the
/// other notices.
pub fn kept_and_notices(kept: &[KeptCell], notices: &[String]) -> Vec<Line> {
    let mut out: Vec<Line> = kept.iter().map(kept_line).collect();
    let texts: Vec<String> = kept.iter().map(KeptCell::notice).collect();
    out.extend(
        notices
            .iter()
            .filter(|n| !texts.contains(n))
            .map(|n| notice_line(n)),
    );
    out
}

/// The overlay rows of a reload's report: its notices (kept over a
/// changed default, ambiguous identities, an `await` cancelled by a
/// handler restart), the cells it reset and why.
pub fn report_lines(r: &Report) -> Vec<Line> {
    let mut out = kept_and_notices(&r.kept_over_default, &r.notices);
    for (cell, why) in &r.reset {
        out.push(Line {
            text: format!("{cell}: reset ({why})"),
            notice: true,
            cell: Some(cell.clone()),
            ..Line::default()
        });
    }
    if r.cancelled > 0 {
        out.push(Line {
            text: format!(
                "{} handler{} restarted: {} in-flight `await` cancelled",
                r.restarted,
                if r.restarted == 1 { "" } else { "s" },
                r.cancelled
            ),
            notice: true,
            ..Line::default()
        });
    }
    out
}

/// The 1-based line and column of byte `offset` in `text`.
pub fn line_col(text: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(text.len());
    let before = text.get(..offset).unwrap_or(text);
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) as u32 + 1;
    (line, col)
}

/// The overlay lines of `diags` (errors and warnings, in order), located
/// in `map`; `unreadable` files first.
pub fn lines(diags: &[Diagnostic], map: &SourceMap, unreadable: &[(PathBuf, String)]) -> Vec<Line> {
    let mut out = Vec::new();
    for (p, why) in unreadable {
        out.push(Line {
            text: format!("{}: cannot read: {why}", p.display()),
            error: true,
            ..Line::default()
        });
    }
    let locate = |file, offset: u32| {
        map.get(file).map(|f| {
            let (l, c) = line_col(&f.text, offset);
            (PathBuf::from(&f.name), l, c)
        })
    };
    for d in diags {
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let at = d
            .primary()
            .and_then(|l| locate(l.file, l.span.start))
            .or_else(|| locate(d.file(), 0));
        let place = at
            .as_ref()
            .map(|(p, l, c)| format!("{}:{l}:{c}: ", short(p)))
            .unwrap_or_default();
        out.push(Line {
            text: format!("{place}{sev}[{}]: {}", d.code, d.message),
            at: at.clone(),
            error: d.severity == Severity::Error,
            ..Line::default()
        });
        for l in &d.labels {
            if l.message.is_empty() || (l.primary && l.message == d.message) {
                continue;
            }
            let at = locate(l.file, l.span.start);
            let place = at
                .as_ref()
                .map(|(p, l, _)| format!("{}:{l}: ", short(p)))
                .unwrap_or_default();
            out.push(Line {
                text: format!("    {place}{}", l.message),
                at,
                ..Line::default()
            });
        }
        if let Some(h) = &d.help {
            out.push(Line {
                text: format!("    help: {h}"),
                at: at.clone(),
                ..Line::default()
            });
        }
    }
    out
}

/// A file name as the overlay shows it: the last two components.
fn short(p: &Path) -> String {
    let parts: Vec<_> = p.components().rev().take(2).collect();
    parts
        .into_iter()
        .rev()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// What a click on the overlay asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Click {
    /// Open this file at this line and column.
    Open(PathBuf, u32, u32),
    /// The close button: hidden until the diagnostics change (and the
    /// reload notices it listed are cleared).
    Dismissed,
    /// A notice's `[reset]`: reset this state cell to its default.
    Reset(String),
    /// A settings notice's `[clear]`: drop the runtime overlay of this
    /// field of this settings file.
    Clear(String, String),
    /// A line with nowhere to go, or the panel itself.
    Nothing,
}

#[derive(Debug)]
struct Shown {
    panel: NodeId,
    close: NodeId,
    rows: Vec<(NodeId, Line)>,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Overlay {
    /// The diagnostics of the last attempt.
    lines: Vec<Line>,
    /// Reload notices, kept until dismissed (or reset, one by one).
    notes: Vec<Line>,
    /// When the current lines were set (the quiet period runs from here).
    since: Option<Instant>,
    shown: Option<Shown>,
    dismissed: bool,
    /// A last good config is running (else the header says nothing
    /// runs yet).
    running: bool,
}

impl Overlay {
    /// The diagnostics now (empty: everything committed cleanly). New
    /// diagnostics restart the quiet period; none take the overlay away.
    pub fn set(&mut self, lines: Vec<Line>, now: Instant, inst: &Instance) {
        if lines == self.lines {
            return;
        }
        self.lines = lines;
        self.changed(now, inst);
    }

    /// Whether a last good config is running (the header's wording).
    pub fn set_running(&mut self, running: bool) {
        self.running = running;
    }

    /// Reload notices to list (after the errors) until dismissed; they
    /// open the overlay after the same quiet period. A row about a cell
    /// replaces the older row about it; the newest [`MAX_NOTES`] stay.
    pub fn note(&mut self, notes: Vec<Line>, now: Instant, inst: &Instance) {
        let mut any = false;
        for n in notes {
            if self.notes.contains(&n) {
                continue;
            }
            if let Some(c) = &n.cell {
                self.notes.retain(|o| o.cell.as_ref() != Some(c));
            }
            self.notes.push(n);
            any = true;
        }
        if self.notes.len() > MAX_NOTES {
            let extra = self.notes.len() - MAX_NOTES;
            self.notes.drain(..extra);
        }
        if any {
            self.changed(now, inst);
        }
    }

    /// Settings files were read again (`reread`: each file and runtime
    /// overlay file as notices name them) and reported `rows`: the rows
    /// a read decides (a bad value, a syntax error) for those files go
    /// unless reported again, so a fixed file takes its notice away. A
    /// notice that was already listed does not reopen a dismissed
    /// overlay.
    pub fn settings_read(
        &mut self,
        reread: &[String],
        rows: Vec<Line>,
        now: Instant,
        inst: &Instance,
    ) {
        let before = self.notes.clone();
        self.notes
            .retain(|n| n.settings_read.as_ref().is_none_or(|f| !reread.contains(f)));
        let mut fresh = false;
        for n in rows {
            if self.notes.contains(&n) {
                continue;
            }
            if let Some(c) = &n.cell {
                self.notes.retain(|o| o.cell.as_ref() != Some(c));
            }
            fresh |= !before.contains(&n);
            self.notes.push(n);
        }
        if self.notes.len() > MAX_NOTES {
            let extra = self.notes.len() - MAX_NOTES;
            self.notes.drain(..extra);
        }
        if fresh {
            self.changed(now, inst);
        } else if self.notes != before {
            self.refresh(inst);
        }
    }

    /// The cell at `path` was reset (IPC `reset`, a `[reset]` click):
    /// its rows no longer apply.
    pub fn forget_cell(&mut self, path: &str, inst: &Instance) {
        let before = self.notes.len();
        self.notes.retain(|n| n.cell.as_deref() != Some(path));
        if self.notes.len() != before {
            self.refresh(inst);
        }
    }

    /// Service `service`'s notice was resolved (the notification server
    /// took the name over): its rows go.
    pub fn forget_service(&mut self, service: &str, inst: &Instance) {
        let prefix = format!("{SERVICE_CELL}{service}#");
        let before = self.notes.len();
        self.notes
            .retain(|n| n.cell.as_deref().is_none_or(|c| !c.starts_with(&prefix)));
        if self.notes.len() != before {
            self.refresh(inst);
        }
    }

    /// Show the current list again (or hide it when empty).
    fn refresh(&mut self, inst: &Instance) {
        if self.lines.is_empty() && self.notes.is_empty() {
            self.since = None;
            self.hide(inst);
        } else if self.shown.is_some() {
            self.hide(inst);
            self.show(inst);
        }
    }

    /// Everything listed: the errors, then the notices.
    fn all(&self) -> Vec<Line> {
        self.lines.iter().chain(&self.notes).cloned().collect()
    }

    fn changed(&mut self, now: Instant, inst: &Instance) {
        self.dismissed = false;
        if self.lines.is_empty() && self.notes.is_empty() {
            self.since = None;
            self.hide(inst);
            return;
        }
        if self.shown.is_some() {
            // Already open: show the new list at once.
            self.hide(inst);
            self.show(inst);
        } else {
            self.since = Some(now);
        }
    }

    /// When [`Overlay::tick`] should run to open it.
    pub fn deadline(&self) -> Option<Instant> {
        if self.shown.is_some() || self.dismissed {
            return None;
        }
        self.since.map(|t| t + QUIET)
    }

    /// Open it if its diagnostics stood for the quiet period.
    pub fn tick(&mut self, now: Instant, inst: &Instance) {
        if self.deadline().is_some_and(|d| now >= d) {
            self.show(inst);
        }
    }

    #[cfg(test)]
    pub fn is_shown(&self) -> bool {
        self.shown.is_some()
    }

    /// The lines it lists (or would list).
    #[cfg(test)]
    pub fn lines(&self) -> Vec<Line> {
        self.all()
    }

    /// A click on `node`: `None` if it is not the overlay's.
    pub fn click(&mut self, node: NodeId, inst: &Instance) -> Option<Click> {
        let shown = self.shown.as_ref()?;
        if node == shown.close {
            self.hide(inst);
            self.notes.clear();
            self.since = None;
            self.dismissed = true;
            return Some(Click::Dismissed);
        }
        if let Some((_, line)) = shown.rows.iter().find(|(n, _)| *n == node) {
            let line = line.clone();
            if let Some((file, field)) = &line.clear {
                // Done with: its row goes.
                self.notes.retain(|n| *n != line);
                self.refresh(inst);
                return Some(Click::Clear(file.clone(), field.clone()));
            }
            if let Some(path) = &line.reset {
                // Done with: its rows go.
                self.notes
                    .retain(|n| *n != line && n.cell.as_deref() != Some(path));
                self.refresh(inst);
                return Some(Click::Reset(path.clone()));
            }
            return Some(match &line.at {
                Some((p, l, c)) => Click::Open(p.clone(), *l, *c),
                None => Click::Nothing,
            });
        }
        (node == shown.panel).then_some(Click::Nothing)
    }

    fn hide(&mut self, inst: &Instance) {
        if let Some(s) = self.shown.take() {
            inst.external_remove(s.panel);
        }
    }

    fn show(&mut self, inst: &Instance) {
        let all = self.all();
        if all.is_empty() {
            return;
        }
        let rows = all.len().min(MAX_ROWS);
        let height = PAD * 2.0 + ROW_H * (rows as f32 + 1.0);
        let panel = inst.external_create(NodeKind::Panel, None, 0);
        let color = |hex: &str| Color::from_hex(hex).map_or(PropValue::Unset, PropValue::Color);
        for (p, v) in [
            (Prop::Name, PropValue::Text("StrandErrors".into())),
            (Prop::Layer, PropValue::Keyword("overlay".into())),
            (Prop::Anchor, PropValue::Keyword("top".into())),
            (Prop::Keyboard, PropValue::Keyword("none".into())),
            (Prop::Margin, PropValue::Number(48.0)),
            (Prop::Width, PropValue::Number(WIDTH)),
            (Prop::Height, PropValue::Number(height)),
            (Prop::Radius, PropValue::Number(10.0)),
            (Prop::Bg, color("#1e1e2ef0")),
            (Prop::Color, color("#cdd6f4")),
            (
                Prop::Font,
                PropValue::Font(Font {
                    family: "monospace".into(),
                    size: 12.0,
                    weight: 400,
                }),
            ),
        ] {
            inst.external_set(panel, p, v);
        }
        let errors = all.iter().filter(|l| l.error).count();
        let header = inst.external_create(NodeKind::Text, Some(panel), 0);
        let more = all.len().saturating_sub(rows);
        let more = if more > 0 {
            format!(" ({more} more lines)")
        } else {
            String::new()
        };
        let title = if errors > 0 {
            format!(
                "strand: {errors} error{} — {}; click a line to open it{more}",
                if errors == 1 { "" } else { "s" },
                if self.running {
                    "the last good config is running"
                } else {
                    "nothing is running yet"
                },
            )
        } else if all.iter().any(|l| l.reset.is_some()) {
            format!("strand: reloaded with notices — click [reset] to go back to a default{more}")
        } else if all.iter().all(|l| {
            l.cell
                .as_deref()
                .is_some_and(|c| c.starts_with(SERVICE_CELL))
        }) {
            format!("strand: services{more}")
        } else if all.iter().all(|l| {
            l.cell
                .as_deref()
                .is_some_and(|c| c.starts_with("settings:"))
        }) {
            if all.iter().any(|l| l.clear.is_some()) {
                format!("strand: settings files — click [clear] to use the file's value{more}")
            } else {
                format!("strand: settings files{more}")
            }
        } else {
            format!("strand: reloaded with notices{more}")
        };
        for (p, v) in [
            (Prop::Text, PropValue::Text(title)),
            (Prop::X, PropValue::Number(PAD)),
            (Prop::Y, PropValue::Number(PAD)),
            (Prop::Weight, PropValue::Number(700.0)),
        ] {
            inst.external_set(header, p, v);
        }
        let close = inst.external_create(NodeKind::Text, Some(panel), 1);
        for (p, v) in [
            (Prop::Text, PropValue::Text("×".into())),
            (Prop::X, PropValue::Number(WIDTH - PAD - 12.0)),
            (Prop::Y, PropValue::Number(PAD)),
        ] {
            inst.external_set(close, p, v);
        }
        let mut row_nodes = Vec::new();
        for (i, l) in all.into_iter().take(rows).enumerate() {
            let n = inst.external_create(NodeKind::Text, Some(panel), i + 2);
            inst.external_set(n, Prop::Text, PropValue::Text(l.text.clone()));
            inst.external_set(n, Prop::X, PropValue::Number(PAD));
            inst.external_set(
                n,
                Prop::Y,
                PropValue::Number(PAD + ROW_H * (i as f32 + 1.0)),
            );
            inst.external_set(n, Prop::MaxWidth, PropValue::Number(WIDTH - PAD * 2.0));
            if l.error {
                inst.external_set(n, Prop::Color, color("#f38ba8"));
            } else if l.notice {
                inst.external_set(n, Prop::Color, color("#f9e2af"));
            }
            row_nodes.push((n, l));
        }
        self.shown = Some(Shown {
            panel,
            close,
            rows: row_nodes,
        });
    }
}

/// Editors that need a terminal (run from a shell with no tty they exit
/// at once or hang).
const TERMINAL_EDITORS: &[&str] = &[
    "vi", "vim", "nvim", "nano", "micro", "kak", "hx", "helix", "ne", "joe", "mg",
];

/// The command that opens `file` at `line`: `$STRAND_EDITOR` as a
/// template (`{file}`, `{line}`, `{col}`, split on spaces, e.g. `foot -e
/// nvim +{line} {file}`), else `$VISUAL` or `$EDITOR` as `<editor>
/// +<line> <file>`, run inside `terminal` (the command prefix that
/// starts a terminal: `xdg-terminal-exec`, or `$TERMINAL -e`) when it is
/// a terminal editor (vi, vim, nvim, nano, micro, kakoune, helix), else
/// `xdg-open <file>` (a terminal editor with no terminal to run in
/// included: the click then opens the desktop's editor for the file).
pub fn editor_command(
    file: &Path,
    line: u32,
    col: u32,
    template: Option<&str>,
    editor: Option<&str>,
    terminal: Option<&[String]>,
) -> Vec<String> {
    let f = file.display().to_string();
    if let Some(t) = template.filter(|t| !t.trim().is_empty()) {
        return t
            .split_whitespace()
            .map(|w| {
                w.replace("{file}", &f)
                    .replace("{line}", &line.to_string())
                    .replace("{col}", &col.to_string())
            })
            .collect();
    }
    if let Some(e) = editor.filter(|e| !e.trim().is_empty()) {
        let mut cmd: Vec<String> = e.split_whitespace().map(String::from).collect();
        cmd.push(format!("+{line}"));
        cmd.push(f.clone());
        let name = Path::new(&cmd[0])
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !TERMINAL_EDITORS.contains(&name.as_str()) {
            return cmd;
        }
        if let Some(term) = terminal.filter(|t| !t.is_empty()) {
            let mut wrapped = term.to_vec();
            wrapped.extend(cmd);
            return wrapped;
        }
    }
    vec!["xdg-open".into(), f]
}

/// `name` is an executable on `$PATH`.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| {
            let p = d.join(name);
            std::fs::metadata(&p).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            })
        })
    })
}

/// The terminal a terminal editor runs in: `xdg-terminal-exec` when it
/// is installed, else `$TERMINAL -e`.
fn terminal() -> Option<Vec<String>> {
    if on_path("xdg-terminal-exec") {
        return Some(vec!["xdg-terminal-exec".into()]);
    }
    let t = std::env::var("TERMINAL")
        .ok()
        .filter(|t| !t.trim().is_empty())?;
    let mut cmd: Vec<String> = t.split_whitespace().map(String::from).collect();
    cmd.push("-e".into());
    Some(cmd)
}

/// Open the editor (detached; its exit is reaped on a thread of its own).
pub fn open_editor(file: &Path, line: u32, col: u32) {
    let template = std::env::var("STRAND_EDITOR").ok();
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok());
    let term = terminal();
    let cmd = editor_command(
        file,
        line,
        col,
        template.as_deref(),
        editor.as_deref(),
        term.as_deref(),
    );
    if cmd.first().map(String::as_str) == Some("xdg-open") && editor.is_some() {
        log::info!(
            "opening {} with xdg-open: $EDITOR needs a terminal and none was found \
             (install xdg-terminal-exec, set $TERMINAL, or set $STRAND_EDITOR, \
             e.g. `foot -e nvim +{{line}} {{file}}`)",
            file.display()
        );
    }
    let Some((prog, args)) = cmd.split_first() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .process_group(0);
    // SAFETY: `restore_in_child` makes only async-signal-safe calls, as
    // `pre_exec` requires: the editor gets the THP setting strand had.
    unsafe {
        cmd.pre_exec(|| {
            strand_services::child::restore_in_child();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            let _ = std::thread::Builder::new()
                .name("strand-editor".into())
                .stack_size(64 * 1024)
                .spawn(move || child.wait());
        }
        Err(e) => log::warn!("cannot open {}: {prog}: {e}", file.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_columns_are_one_based() {
        let t = "ab\ncdé\n";
        assert_eq!(line_col(t, 0), (1, 1));
        assert_eq!(line_col(t, 3), (2, 1));
        assert_eq!(line_col(t, 5), (2, 3));
        assert_eq!(line_col(t, 99), (3, 1));
    }

    #[test]
    fn diagnostics_list_with_labels_and_help() {
        let mut map = SourceMap::new();
        let f = map.add("/c/strand/bar.strand", "bar Top {\n  txet \"a\"\n}\n");
        let d = Diagnostic::error("check::unknown", "unknown element `txet`")
            .with_label_in(
                f,
                strand_compiler::syntax::Span::new(12, 16),
                "not an element",
            )
            .with_help("did you mean `text`?");
        let l = lines(
            &[d],
            &map,
            &[(PathBuf::from("/c/strand/x.strand"), "not UTF-8".into())],
        );
        let texts: Vec<&str> = l.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "/c/strand/x.strand: cannot read: not UTF-8",
                "strand/bar.strand:2:3: error[check::unknown]: unknown element `txet`",
                "    strand/bar.strand:2: not an element",
                "    help: did you mean `text`?",
            ]
        );
        assert_eq!(l[1].at, Some((PathBuf::from("/c/strand/bar.strand"), 2, 3)));
        assert!(l[1].error && !l[2].error);
    }

    fn instance() -> (strand_core::Runtime, Instance) {
        let mut map = SourceMap::new();
        map.add("bar.strand", "panel P { text \"hi\" }\n");
        let b = strand_compiler::reconcile::Build::compile(None, map).unwrap();
        let rt = strand_core::Runtime::new();
        let host = std::rc::Rc::new(strand_compiler::vm::schema_host::SchemaHost::mock(
            &rt,
            &b.program.types,
        ));
        let inst =
            Instance::from_build(&rt, &b, host, strand_compiler::instantiate::Storage::none());
        (rt, inst)
    }

    fn line(t: &str) -> Line {
        Line {
            text: t.into(),
            at: Some((PathBuf::from("/c/bar.strand"), 2, 3)),
            error: true,
            ..Line::default()
        }
    }

    /// Errors fixed within the quiet period (format-on-save, a
    /// delete-then-create save) never show it; errors that stand 250 ms
    /// do; the fix takes it away; dismissed, it stays away until the
    /// diagnostics change.
    #[test]
    fn the_quiet_period_and_dismissal() {
        let (_rt, inst) = instance();
        let mut o = Overlay::default();
        let t0 = Instant::now();
        o.set(vec![line("a")], t0, &inst);
        o.tick(t0 + Duration::from_millis(100), &inst);
        o.set(Vec::new(), t0 + Duration::from_millis(120), &inst);
        o.tick(t0 + Duration::from_secs(1), &inst);
        assert!(!o.is_shown(), "fixed within the quiet period");
        o.set(vec![line("a")], t0, &inst);
        assert_eq!(o.deadline(), Some(t0 + QUIET));
        o.tick(t0 + Duration::from_millis(249), &inst);
        assert!(!o.is_shown());
        o.tick(t0 + QUIET, &inst);
        assert!(o.is_shown());
        assert_eq!(o.lines().len(), 1);
        // Clicks: a row opens its place; the close button dismisses.
        let u = inst.flush();
        let mut m = strand_compiler::instantiate::SceneMirror::new();
        m.apply(&u.diff).unwrap();
        let row = m.find_text("a").unwrap();
        assert_eq!(
            o.click(row, &inst),
            Some(Click::Open(PathBuf::from("/c/bar.strand"), 2, 3))
        );
        let hi = m.find_text("hi").unwrap();
        assert_eq!(o.click(hi, &inst), None, "not the overlay's");
        let close = m.find_text("×").unwrap();
        assert_eq!(o.click(close, &inst), Some(Click::Dismissed));
        assert!(!o.is_shown() && o.deadline().is_none());
        o.set(vec![line("a")], t0, &inst);
        assert!(o.deadline().is_none(), "the same errors stay dismissed");
        o.set(vec![line("b")], t0, &inst);
        assert!(o.deadline().is_some(), "new ones show again");
        m.apply(&inst.flush().diff).unwrap();
        assert_eq!(m.find_text("a"), None);
    }

    /// Settings-file notices are rows too: one per file and field (a
    /// newer one replaces it), and a shadowed field's `[clear]` asks for
    /// its runtime overlay to go.
    #[test]
    fn settings_notices_and_their_clear() {
        use std::sync::Arc;
        use strand_core::{SettingsIssue, SettingsNotice};
        let (_rt, inst) = instance();
        let mut o = Overlay::default();
        let t0 = Instant::now();
        let notice = |field: &str, issue| SettingsNotice {
            file: Arc::from("/c/prefs.toml"),
            field: Some(Arc::from(field)),
            issue,
        };
        let bad = settings_line(&notice("gap", SettingsIssue::BadValue("not an int".into())));
        assert!(bad.clear.is_none());
        assert!(
            bad.text.contains("keeping its last good value"),
            "{}",
            bad.text
        );
        let shadowed = settings_line(&notice("accent", SettingsIssue::Shadowed));
        assert_eq!(
            shadowed.text,
            "accent: file changed but runtime overlay wins [clear]"
        );
        assert_eq!(
            shadowed.clear,
            Some(("/c/prefs.toml".into(), "accent".into()))
        );
        let read_only = settings_line(&SettingsNotice {
            file: Arc::from("/nix/store/x/prefs.toml"),
            field: None,
            issue: SettingsIssue::ReadOnly {
                overlay: PathBuf::from("/s/settings/prefs.toml"),
            },
        });
        assert!(
            read_only
                .text
                .contains("is read-only; changes are kept in /s/settings/prefs.toml")
        );
        o.note(vec![bad, shadowed, read_only], t0, &inst);
        // A newer notice about the same field replaces its row.
        let worse = settings_line(&notice(
            "gap",
            SettingsIssue::BadValue("not a number".into()),
        ));
        o.note(vec![worse], t0, &inst);
        assert_eq!(o.lines().len(), 3);
        o.tick(t0 + QUIET, &inst);
        assert!(o.is_shown());
        let mut m = strand_compiler::instantiate::SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        let row = m
            .find_text("accent: file changed but runtime overlay wins [clear]")
            .unwrap();
        assert_eq!(
            o.click(row, &inst),
            Some(Click::Clear("/c/prefs.toml".into(), "accent".into()))
        );
        assert_eq!(o.lines().len(), 2, "its row goes");
    }

    /// Reload notices are listed after the quiet period as warning rows;
    /// a `[reset]` row asks for its cell and goes; clean reloads leave
    /// them; dismissal clears them.
    #[test]
    fn reload_notices_and_their_reset() {
        let (_rt, inst) = instance();
        let mut o = Overlay::default();
        let t0 = Instant::now();
        let mut report = Report {
            reset: vec![("t.b".into(), "renamed".into())],
            ..Report::default()
        };
        report.kept_over(KeptCell {
            path: "launcher.query".into(),
            shown: "\"fi\"".into(),
        });
        report.notice("x: ambiguous".into());
        let rows = report_lines(&report);
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows[0].reset.as_deref(), Some("launcher.query"));
        assert_eq!(rows[1].text, "x: ambiguous");
        assert_eq!(rows[2].text, "t.b: reset (renamed)");
        assert!(rows[1].reset.is_none() && rows[2].reset.is_none());
        assert!(rows.iter().all(|l| l.notice));
        o.note(rows, t0, &inst);
        // A newer value kept for the same cell replaces its row.
        let mut newer = Report::default();
        newer.kept_over(KeptCell {
            path: "launcher.query".into(),
            shown: "\"fir\"".into(),
        });
        o.note(report_lines(&newer), t0, &inst);
        assert_eq!(o.lines().len(), 3, "{:?}", o.lines());
        // A clean reload in the meantime keeps them.
        o.set(Vec::new(), t0, &inst);
        o.tick(t0 + Duration::from_millis(100), &inst);
        assert!(!o.is_shown());
        o.tick(t0 + QUIET, &inst);
        assert!(o.is_shown());
        let mut m = strand_compiler::instantiate::SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        let row = m
            .find_text("launcher.query: kept \"fir\" (default changed) [reset]")
            .unwrap();
        assert_eq!(
            o.click(row, &inst),
            Some(Click::Reset("launcher.query".into()))
        );
        m.apply(&inst.flush().diff).unwrap();
        assert!(m.find_text("t.b: reset (renamed)").is_some());
        assert_eq!(o.lines().len(), 2, "the reset row is gone");
        // A cell reset over IPC loses its rows.
        o.forget_cell("t.b", &inst);
        m.apply(&inst.flush().diff).unwrap();
        assert!(m.find_text("t.b: reset (renamed)").is_none());
        assert_eq!(o.lines().len(), 1);
        // Notices are capped (the newest stay).
        o.note(
            (0..MAX_NOTES + 5)
                .map(|i| notice_line(&format!("n{i}")))
                .collect(),
            t0,
            &inst,
        );
        assert_eq!(o.lines().len(), MAX_NOTES);
        assert_eq!(
            o.lines().last().map(|l| l.text.clone()),
            Some(format!("n{}", MAX_NOTES + 4))
        );
        m.apply(&inst.flush().diff).unwrap();
        let close = m.find_text("×").unwrap();
        assert_eq!(o.click(close, &inst), Some(Click::Dismissed));
        assert!(o.lines().is_empty());
        m.apply(&inst.flush().diff).unwrap();
        assert!(m.find_text("x: ambiguous").is_none());
    }

    #[test]
    fn the_editor_command() {
        let f = Path::new("/c/bar.strand");
        let term = ["foot".to_string(), "-e".to_string()];
        assert_eq!(
            editor_command(
                f,
                3,
                7,
                Some("foot -e nvim +{line} {file}"),
                Some("vim"),
                None
            ),
            ["foot", "-e", "nvim", "+3", "/c/bar.strand"]
        );
        // A GUI editor runs as it is.
        assert_eq!(
            editor_command(f, 3, 7, None, Some("emacsclient -n"), Some(&term)),
            ["emacsclient", "-n", "+3", "/c/bar.strand"]
        );
        // A terminal editor runs in the terminal, or not at all.
        assert_eq!(
            editor_command(f, 3, 7, None, Some("/usr/bin/nvim"), Some(&term)),
            ["foot", "-e", "/usr/bin/nvim", "+3", "/c/bar.strand"]
        );
        assert_eq!(
            editor_command(f, 3, 7, None, Some("nano"), None),
            ["xdg-open", "/c/bar.strand"]
        );
        assert_eq!(
            editor_command(f, 3, 7, Some(" "), None, None),
            ["xdg-open", "/c/bar.strand"]
        );
    }

    #[test]
    fn service_notices_wrap_and_are_keyed_by_service() {
        let text = "word ".repeat(80);
        let rows = service_lines("notifications", &text);
        assert!(rows.len() >= 3, "{rows:?}");
        let width = ((WIDTH - PAD * 2.0) / 7.4) as usize;
        for (i, r) in rows.iter().enumerate() {
            assert!(r.text.chars().count() <= width, "{r:?}");
            assert!(r.notice);
            assert_eq!(
                r.cell.as_deref(),
                Some(&*format!("service:notifications#{i}"))
            );
            if i > 0 {
                assert!(r.text.starts_with("  word"), "continuations indent: {r:?}");
            }
        }
        let words: usize = rows.iter().map(|r| r.text.split_whitespace().count()).sum();
        assert_eq!(words, 80, "no word lost");
    }
}
