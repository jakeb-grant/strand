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
use strand_compiler::source::SourceMap;
use strand_scene::{Color, Font, NodeId, NodeKind, Prop, PropValue};

/// How long diagnostics must stand before the overlay opens.
pub const QUIET: Duration = Duration::from_millis(250);

/// Rows shown at most (the rest are counted in the header).
const MAX_ROWS: usize = 40;
const ROW_H: f32 = 18.0;
const PAD: f32 = 12.0;
const WIDTH: f32 = 960.0;

/// One overlay line: a diagnostic, one of its labels or its help.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    /// Where a click goes (file, 1-based line and column).
    pub at: Option<(PathBuf, u32, u32)>,
    /// An error's own line (drawn in the error colour).
    pub error: bool,
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
            at: None,
            error: true,
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
                error: false,
            });
        }
        if let Some(h) = &d.help {
            out.push(Line {
                text: format!("    help: {h}"),
                at: at.clone(),
                error: false,
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
    /// The close button: hidden until the diagnostics change.
    Dismissed,
    /// A line with nowhere to go, or the panel itself.
    Nothing,
}

#[derive(Debug)]
struct Shown {
    panel: NodeId,
    close: NodeId,
    rows: Vec<(NodeId, usize)>,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct Overlay {
    lines: Vec<Line>,
    /// When the current lines were set (the quiet period runs from here).
    since: Option<Instant>,
    shown: Option<Shown>,
    dismissed: bool,
}

impl Overlay {
    /// The diagnostics now (empty: everything committed cleanly). New
    /// diagnostics restart the quiet period; none take the overlay away.
    pub fn set(&mut self, lines: Vec<Line>, now: Instant, inst: &Instance) {
        if lines == self.lines {
            return;
        }
        self.lines = lines;
        self.dismissed = false;
        if self.lines.is_empty() {
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
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// A click on `node`: `None` if it is not the overlay's.
    pub fn click(&mut self, node: NodeId, inst: &Instance) -> Option<Click> {
        let shown = self.shown.as_ref()?;
        if node == shown.close {
            self.hide(inst);
            self.dismissed = true;
            return Some(Click::Dismissed);
        }
        if let Some((_, i)) = shown.rows.iter().find(|(n, _)| *n == node) {
            return Some(match &self.lines[*i].at {
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
        if self.lines.is_empty() {
            return;
        }
        let rows = self.lines.len().min(MAX_ROWS);
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
        let errors = self.lines.iter().filter(|l| l.error).count();
        let header = inst.external_create(NodeKind::Text, Some(panel), 0);
        let more = self.lines.len().saturating_sub(rows);
        let title = format!(
            "strand: {errors} error{} — the last good config is running; click a line to open it{}",
            if errors == 1 { "" } else { "s" },
            if more > 0 {
                format!(" ({more} more lines)")
            } else {
                String::new()
            }
        );
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
        for (i, l) in self.lines.iter().take(rows).enumerate() {
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
            }
            row_nodes.push((n, i));
        }
        self.shown = Some(Shown {
            panel,
            close,
            rows: row_nodes,
        });
    }
}

/// The command that opens `file` at `line`: `$STRAND_EDITOR` as a
/// template (`{file}`, `{line}`, `{col}`, split on spaces, e.g. `foot -e
/// nvim +{line} {file}`), else `$VISUAL` or `$EDITOR` as `<editor>
/// +<line> <file>` (vi, vim, nvim, emacs, nano, micro, kakoune), else
/// `xdg-open <file>`.
pub fn editor_command(
    file: &Path,
    line: u32,
    col: u32,
    template: Option<&str>,
    editor: Option<&str>,
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
        cmd.push(f);
        return cmd;
    }
    vec!["xdg-open".into(), f]
}

/// Open the editor (detached; its exit is reaped on a thread of its own).
pub fn open_editor(file: &Path, line: u32, col: u32) {
    let template = std::env::var("STRAND_EDITOR").ok();
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok());
    let cmd = editor_command(file, line, col, template.as_deref(), editor.as_deref());
    let Some((prog, args)) = cmd.split_first() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    match std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
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

    #[test]
    fn the_editor_command() {
        let f = Path::new("/c/bar.strand");
        assert_eq!(
            editor_command(f, 3, 7, Some("foot -e nvim +{line} {file}"), Some("vim")),
            ["foot", "-e", "nvim", "+3", "/c/bar.strand"]
        );
        assert_eq!(
            editor_command(f, 3, 7, None, Some("emacsclient -n")),
            ["emacsclient", "-n", "+3", "/c/bar.strand"]
        );
        assert_eq!(
            editor_command(f, 3, 7, Some(" "), None),
            ["xdg-open", "/c/bar.strand"]
        );
    }
}
