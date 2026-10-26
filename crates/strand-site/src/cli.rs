//! The CLI reference page (`tools/cli.html`): made from what the `strand`
//! binary itself prints, `strand --help` and the usage of each command
//! that has one, so it cannot say what the binary does not. The help is
//! kept in [`CAPTURE`], written by `strand-site cli <strand>`
//! (scripts/site/cli.sh); `strand-site build --strand <strand>` runs the
//! binary again and fails when it prints anything else, so a new,
//! renamed or removed command or flag fails the build until the page is
//! made again.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The binary's help as last captured, from the repository's root.
pub const CAPTURE: &str = "crates/strand-site/cli-help.txt";
/// Where the page is written.
pub const OUT: &str = "tools/cli.html";

/// The commands whose own usage is asked for (`strand <cmd> --help`):
/// those whose code answers a leading `-` with their usage before it does
/// anything (read in crates/strand: `run`, `new` and `report` refuse it
/// with their usage line; `check`, `fmt`, `export` and
/// `compositor-rules` take `--help`). The socket's commands would ask a
/// running shell, and `replay` hands its arguments to strand-eval.
pub const OWN_HELP: &[&str] = &[
    "run",
    "check",
    "fmt",
    "new",
    "export",
    "compositor-rules",
    "report",
];

const NOTE: &str =
    "# Captured by `strand-site cli` from the strand binary (scripts/site/cli.sh); do not edit.\n";

/// One invocation and what it printed.
#[derive(Debug, Clone, PartialEq)]
pub struct Help {
    /// The arguments after `strand`.
    pub args: String,
    pub text: String,
}

/// What `bin args` prints, stdout then stderr, trailing blank lines
/// dropped. It runs in an empty directory with no shell to talk to and
/// is stopped after ten seconds.
fn run(bin: &Path, args: &[&str]) -> Result<String, String> {
    // The child runs elsewhere: a relative path is the caller's.
    let bin = &std::fs::canonicalize(bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("strand-site-cli-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(&dir)
        .env_remove("STRAND_SOCKET")
        .env_remove("STRAND_STATE_DIR")
        .env_remove("WAYLAND_DISPLAY")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`strand {}` did not finish", args.join(" ")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("{}: {e}", bin.display())),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    let _ = std::fs::remove_dir_all(&dir);
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim_end().to_string();
    if text.is_empty() {
        return Err(format!("`strand {}` printed nothing", args.join(" ")));
    }
    if text.lines().any(|l| l.starts_with("$ strand")) {
        return Err(format!(
            "`strand {}` printed a line starting `$ strand`, which the capture cannot keep",
            args.join(" ")
        ));
    }
    Ok(text)
}

/// The help `bin` prints now.
pub fn capture(bin: &Path) -> Result<Vec<Help>, String> {
    let mut out = vec![Help {
        args: "--help".into(),
        text: run(bin, &["--help"])?,
    }];
    for cmd in OWN_HELP {
        out.push(Help {
            args: format!("{cmd} --help"),
            text: run(bin, &[cmd, "--help"])?,
        });
    }
    Ok(out)
}

/// The capture file's text.
pub fn write(helps: &[Help]) -> String {
    let mut o = String::from(NOTE);
    for h in helps {
        let _ = write!(o, "$ strand {}\n{}\n", h.args, h.text);
    }
    o
}

/// The capture file's invocations.
pub fn parse(text: &str) -> Result<Vec<Help>, String> {
    let mut out: Vec<Help> = Vec::new();
    for line in text.lines() {
        if let Some(args) = line.strip_prefix("$ strand ") {
            out.push(Help {
                args: args.to_string(),
                text: String::new(),
            });
        } else if let Some(h) = out.last_mut() {
            if !h.text.is_empty() {
                h.text.push('\n');
            }
            h.text.push_str(line);
        } else if !line.starts_with('#') && !line.is_empty() {
            return Err(format!("{CAPTURE}: text before the first `$ strand`"));
        }
    }
    for h in &mut out {
        h.text = h.text.trim_end().to_string();
    }
    if !out.iter().any(|h| h.args == "--help") {
        return Err(format!("{CAPTURE}: no `$ strand --help`"));
    }
    Ok(out)
}

/// Why the capture at `root` is not what `bin` prints, if it is not.
pub fn check(root: &Path, bin: &Path) -> Result<(), String> {
    let now = write(&capture(bin)?);
    let kept = std::fs::read_to_string(root.join(CAPTURE)).unwrap_or_default();
    if now == kept {
        return Ok(());
    }
    let (n, a, b) = now
        .lines()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(kept.lines().map(Some).chain(std::iter::repeat(None)))
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(n, (a, b))| (n + 1, a.unwrap_or("(nothing)"), b.unwrap_or("(nothing)")))
        .unwrap_or((0, "", ""));
    Err(format!(
        "{CAPTURE} is not what {} prints (line {n}: the binary has `{a}`, the page `{b}`); \
         run scripts/site/cli.sh and commit the page's capture",
        bin.display()
    ))
}

/// A help section's rows: (name, what it does), a row's name ending at
/// its first run of two spaces.
fn rows<'t>(text: &'t str, title: &str) -> Vec<(&'t str, &'t str)> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            inside = line.starts_with(title);
            continue;
        }
        if inside {
            let l = line.trim_start();
            match l.split_once("  ") {
                Some((name, about)) => out.push((name.trim(), about.trim())),
                None => out.push((l.trim(), "")),
            }
        }
    }
    out
}

/// `s` as markdown text: every character markdown could read as markup
/// escaped, but `` `code` ``, which stays code (only a `|` is escaped in
/// it in a table cell, which needs it).
fn md_text(s: &str, table: bool) -> String {
    let mut o = String::with_capacity(s.len());
    let code = s.matches('`').count().is_multiple_of(2) && s.contains('`');
    let mut in_code = false;
    for c in s.chars() {
        if c == '`' && code {
            in_code = !in_code;
            o.push(c);
            continue;
        }
        let special = matches!(
            c,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '#'
                | '+'
                | '!'
                | '|'
                | '~'
                | '&'
        );
        if (table && in_code && c == '|') || (!in_code && special) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// The page's markdown, from the capture.
pub fn page(helps: &[Help]) -> Result<String, String> {
    let top = helps
        .iter()
        .find(|h| h.args == "--help")
        .ok_or_else(|| format!("{CAPTURE}: no `$ strand --help`"))?;
    let own: Vec<(&str, &Help)> = helps
        .iter()
        .filter_map(|h| Some((h.args.strip_suffix(" --help")?, h)))
        .collect();
    let commands = rows(&top.text, "commands:");
    if commands.is_empty() {
        return Err(format!("{CAPTURE}: `strand --help` lists no commands"));
    }
    let mut o = String::from(
        "# The `strand` command\n\n\
         Everything `strand` does is one of its commands: `strand <command> [args]`. \
         This page is made from what the binary prints for `strand --help` and for each \
         command's own `--help`, and the site's build checks it against the binary, so it \
         lists exactly the commands and flags `strand` has.\n\n## Commands\n\n\
         | Command | What it does |\n| --- | --- |\n",
    );
    for (name, about) in &commands {
        let cell = if own.iter().any(|(c, _)| c == name) {
            format!("[`strand {name}`](#strand-{name})")
        } else {
            format!("`strand {name}`")
        };
        let _ = writeln!(o, "| {cell} | {} |", md_text(about, true));
    }
    for (title, head, col) in [
        ("options:", "Options", "Option"),
        ("environment", "Environment", "Variable"),
    ] {
        let r = rows(&top.text, title);
        if r.is_empty() {
            continue;
        }
        let heading = top
            .text
            .lines()
            .find(|l| l.starts_with(title))
            .unwrap_or("");
        let _ = write!(o, "\n## {head}\n\n");
        // `environment (not passed to programs the shell starts):` says
        // something of its own.
        if let Some(said) = heading
            .split_once('(')
            .and_then(|(_, r)| r.strip_suffix("):"))
        {
            let mut said = said.to_string();
            if let Some(f) = said.get(..1) {
                said.replace_range(..1, &f.to_uppercase());
            }
            let _ = write!(o, "{}.\n\n", md_text(&said, false));
        }
        let _ = writeln!(o, "| {col} | |\n| --- | --- |");
        for (name, about) in r {
            let names: Vec<String> = name.split(", ").map(|n| format!("`{n}`")).collect();
            let _ = writeln!(o, "| {} | {} |", names.join(", "), md_text(about, true));
        }
    }
    // A command's usage line as code, what it says after it as text.
    for (cmd, h) in &own {
        let _ = write!(o, "\n## `strand {cmd}`\n\n");
        let mut paras = h.text.split("\n\n");
        let usage = paras.next().unwrap_or("");
        let _ = write!(o, "```text\n{usage}\n```\n");
        for p in paras {
            let _ = write!(o, "\n{}\n", md_text(&p.replace('\n', " "), false));
        }
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOP: &str = "usage: strand <command> [args]\n\ncommands:\n  run               run the config [dir]\n  \
                       check             check the config <dir> | file\n\noptions:\n  -V, --version     print the version\n\n\
                       environment (not passed to programs the shell starts):\n  STRAND_SOCKET     the shell's socket";

    fn helps() -> Vec<Help> {
        vec![
            Help {
                args: "--help".into(),
                text: TOP.into(),
            },
            Help {
                args: "check --help".into(),
                text: "usage: strand check [dir | file]\n\nChecks.".into(),
            },
        ]
    }

    #[test]
    fn the_capture_reads_back_as_written() {
        let h = helps();
        assert_eq!(parse(&write(&h)).unwrap(), h);
        assert!(parse("# note\nloose\n").is_err());
        assert!(parse("$ strand check --help\nx\n").is_err());
    }

    #[test]
    fn the_page_lists_every_command_option_and_variable() {
        let md = page(&helps()).unwrap();
        assert!(
            md.contains("| `strand run` | run the config \\[dir\\] |"),
            "{md}"
        );
        assert!(
            md.contains(
                "| [`strand check`](#strand-check) | check the config \\<dir\\> \\| file |"
            ),
            "{md}"
        );
        assert!(
            md.contains("| `-V`, `--version` | print the version |"),
            "{md}"
        );
        assert!(
            md.contains("Not passed to programs the shell starts."),
            "{md}"
        );
        assert!(
            md.contains("| `STRAND_SOCKET` | the shell's socket |"),
            "{md}"
        );
        assert!(
            md.contains(
                "## `strand check`\n\n```text\nusage: strand check [dir | file]\n```\n\nChecks.\n"
            ),
            "{md}"
        );
    }

    /// A stand-in binary: a shell script printing `top` for `--help` and
    /// a usage for each command.
    fn fake(top: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!(
            "strand-site-fake-{}-{:x}",
            std::process::id(),
            blake3::hash(top.as_bytes()).as_bytes()[0]
        ));
        std::fs::create_dir_all(&d).unwrap();
        let bin = d.join("strand");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --help ]; then cat <<'EOF'\n{top}\nEOF\nelse echo \"usage: strand $1\" >&2; exit 1; fi\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[test]
    fn the_check_fails_when_the_binary_says_something_else() {
        let root =
            std::env::temp_dir().join(format!("strand-site-cli-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join("crates/strand-site")).unwrap();
        let bin = fake(TOP);
        let kept = write(&capture(&bin).unwrap());
        assert!(
            kept.contains("$ strand export --help\nusage: strand export\n"),
            "{kept}"
        );
        std::fs::write(root.join(CAPTURE), &kept).unwrap();
        assert_eq!(check(&root, &bin), Ok(()));
        // A command added to the binary: the page no longer says it all.
        let renamed = fake(&TOP.replace("  check  ", "  verify "));
        let e = check(&root, &renamed).unwrap_err();
        assert!(e.contains("line 7"), "{e}");
        assert!(e.contains("run scripts/site/cli.sh"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
