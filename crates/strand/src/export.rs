//! `strand export json | base24 | --template <file>` (S-tools; design.md,
//! "Wallpaper palettes"; decisions.md m5-tools, "the CLI"): the palette
//! and the resolved token table as data, a base24 scheme, or a text file
//! of any format filled from the theme. Each goes to stdout, or
//! atomically to `-o <file>`.
//!
//! Where the theme comes from, in order: the running shell over its
//! socket (the live table, overlays included), else an offline mount of
//! the config that writes nothing, else "no palette: run strand once, or
//! fix the theme" with the cause. `--palette '<expr>'` mounts no config.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::theme::is_pending;
use strand_compiler::instantiate::{Instance, Storage};
use strand_compiler::reconcile::Build;
use strand_compiler::source::SourceMap;
use strand_compiler::template::Template;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Num, Value};
use strand_core::Runtime;
use strand_scene::{Color, TokenTable};

pub(crate) const USAGE: &str = "usage: strand export json | base24 | --template <file> \
                     [--palette '<expr>'] [-o <file>]";

/// How long the offline mount waits for a wallpaper to be quantised.
const IMAGE_WAIT: Duration = Duration::from_secs(10);

/// The cause the offline mount gives when no palette can be had.
const NO_PALETTE: &str = "no palette: run strand once, or fix the theme";

/// What is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    Json,
    Base24,
    /// A template file, read by the CLI (its path as diagnostics name it,
    /// and its text).
    Template(PathBuf),
}

/// `strand export`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub format: Format,
    /// `--palette '<expr>'`: no config, this palette (offline only).
    pub palette: Option<String>,
    /// `-o <file>`.
    pub out: Option<PathBuf>,
}

/// Parses the arguments after `export`.
pub fn parse(args: &[String]) -> Result<Ask, String> {
    let mut format = None;
    let mut palette = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs {what}\n{USAGE}"))
        };
        let next = match a.as_str() {
            "json" => Format::Json,
            "base24" => Format::Base24,
            "--template" => Format::Template(value("a file")?.into()),
            "--palette" if palette.is_none() => {
                palette = Some(value("an expression")?);
                continue;
            }
            "-o" if out.is_none() => {
                out = Some(PathBuf::from(value("a file")?));
                continue;
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unexpected `{other}`\n{USAGE}")),
        };
        if format.replace(next).is_some() {
            return Err(format!("one format at a time\n{USAGE}"));
        }
    }
    let format = format.ok_or_else(|| USAGE.to_string())?;
    Ok(Ask {
        format,
        palette,
        out,
    })
}

/// `strand export …`: the text for stdout (empty with `-o`).
pub fn export_cli(args: &[String]) -> Result<String, String> {
    let ask = parse(args)?;
    let template = match &ask.format {
        Format::Template(path) => Some(read_template(path)?),
        _ => None,
    };
    let bytes = match ask.palette.as_deref() {
        Some(expr) => {
            let mount = Mount::palette(expr)?;
            mount.produce(&ask.format, template.as_ref())?
        }
        None if template.as_ref().is_some_and(|t| t.text.is_empty()) => Vec::new(),
        None => match from_shell(&ask.format, template.as_ref())? {
            Some(bytes) => bytes,
            None => {
                let mount = Mount::config()?;
                mount.produce(&ask.format, template.as_ref())?
            }
        },
    };
    match &ask.out {
        Some(path) => {
            // As a `file` element writes: through a link, the mode kept.
            crate::run::write_through(path, &bytes)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(String::new())
        }
        None => Ok(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

/// How long the CLI waits for the shell's answer (a pending other-mode
/// palette is waited for there, up to 30 s).
const SHELL_WAIT: Duration = Duration::from_secs(40);

/// The running shell's answer (step 1), or `None` to mount offline: no
/// shell on the socket, a shell from before `export` (it answers
/// "unknown command"), or a template too large for one request line.
fn from_shell(format: &Format, template: Option<&Source>) -> Result<Option<Vec<u8>>, String> {
    use crate::ipc::{MAX_LINE, Request, encode, request, socket_path};
    let req = Request::Export {
        kind: match format {
            Format::Json => "json",
            Format::Base24 => "base24",
            Format::Template(_) => "template",
        }
        .into(),
        template: template.map(|t| t.text.clone()),
        name: template.map(|t| t.name.clone()),
    };
    if encode(&req).len() > MAX_LINE {
        return Ok(None);
    }
    let Some(conn) = socket_path().and_then(|p| std::os::unix::net::UnixStream::connect(p).ok())
    else {
        return Ok(None);
    };
    let answer = request(&mut std::io::BufReader::new(conn), &req, SHELL_WAIT)?;
    match strand_core::ipc::refusal(&answer) {
        None => Ok(Some(
            answer
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        )),
        Some(why) if why == "unknown command `export`" => Ok(None),
        Some(why) => Err(why),
    }
}

/// A template file as read: its name for diagnostics and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub text: String,
}

fn read_template(path: &Path) -> Result<Source, String> {
    let text = std::fs::read(path)
        .map_err(|e| format!("{}: {e}", path.display()))
        .and_then(|b| {
            String::from_utf8(b).map_err(|_| format!("{}: not valid UTF-8", path.display()))
        })?;
    Ok(Source {
        name: path.display().to_string(),
        text,
    })
}

/// Compiles `source`, its errors rendered with their location.
pub(crate) fn compile_template(source: &Source) -> Result<Template, String> {
    let (map, file) = SourceMap::single(source.name.clone(), source.text.clone());
    Template::compile(file, &source.text).map_err(|d| render(&d, &map, Style::Plain))
}

/// The output of `format` from `inst`'s theme: the shell's handler and
/// the offline mount share it, so IPC and offline agree. `other` is the
/// other mode's table when the template reads it (`None`: absent).
pub(crate) fn produce(
    inst: &Instance,
    format: &Format,
    template: Option<(&Source, &Template)>,
    other: Option<&TokenTable>,
) -> Result<Vec<u8>, String> {
    // The palette with no fallback, for every format: a broken theme is
    // the error, never the built-in or last good palette the token
    // table falls back to (decisions.md m5-tools, "the CLI", step 3).
    let palette = inst.palette().map_err(|e| format!("{NO_PALETTE} ({e})"))?;
    let table = inst
        .tokens_now()
        .map_err(|e| format!("{NO_PALETTE} ({e})"))?;
    match (format, template) {
        (Format::Template(_), Some((source, t))) => {
            let (map, _) = SourceMap::single(source.name.clone(), source.text.clone());
            t.render(&table, other)
                .map_err(|d| render(&d, &map, Style::Plain))
        }
        (Format::Template(_), None) => Err("no template".into()),
        (f, _) => Ok(match f {
            Format::Base24 => strand_theme::export::base24(&palette, &table),
            _ => strand_theme::export::json(&palette, &table),
        }
        .into_bytes()),
    }
}

/// The offline mount (decisions.md m5-tools, "the CLI", step 2).
struct Mount {
    // Dropped before the runtime it lives in.
    inst: Instance,
    _rt: Runtime,
    /// The boot's runtime errors (a failing `use palette` among them),
    /// given as the cause when no palette can be had.
    errors: Vec<String>,
}

impl Mount {
    /// The config in `$XDG_CONFIG_HOME/strand`, mounted on read-only
    /// storage: persisted cells, settings and the token overlays read,
    /// the persisted palette pinned, nothing written.
    fn config() -> Result<Mount, String> {
        let dir = crate::check::default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .ok_or_else(|| {
            format!("{NO_PALETTE} (no config directory: set XDG_CONFIG_HOME or HOME)")
        })?;
        let build = crate::rules::load(&dir).map_err(|e| format!("{NO_PALETTE}:\n{e}"))?;
        let state = strand_core::persist::state_dir().ok_or_else(|| {
            format!(
                "{NO_PALETTE} (no state directory: set STRAND_STATE_DIR, XDG_STATE_HOME or HOME)"
            )
        })?;
        Ok(Mount::boot(
            &build,
            Storage::read_only(&state, &dir),
            Some(&state),
        ))
    }

    /// `--palette '<expr>'`: the base tokens and that palette, no config
    /// (relative imports read from the working directory).
    fn palette(expr: &str) -> Result<Mount, String> {
        let mut map = SourceMap::new();
        map.add("--palette", format!("use palette {expr}\n"));
        let shown = map.clone();
        let build = Build::compile_with(None, map, crate::services::schema())
            .map_err(|d| render(&d, &shown, Style::Plain))?;
        let storage = Storage {
            persist: None,
            settings: None,
            config_dir: std::env::current_dir().ok(),
            tokens: Default::default(),
        };
        Ok(Mount::boot(
            &build,
            storage,
            strand_core::persist::state_dir().as_deref(),
        ))
    }

    fn boot(build: &Build, storage: Storage, state: Option<&Path>) -> Mount {
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::real(&rt, &build.program.types));
        // `system` as the last run kept it (`palettes/system`).
        let last = crate::system::Last::file(state.map(|s| s.join("palettes")))
            .map(|f| crate::system::Last::load(&f))
            .unwrap_or_default();
        let mut sys = strand_services::system::System::default();
        last.seed(&mut sys);
        let accent = sys.accent.map_or(Value::Null, |c| {
            Value::Color(Color::new(c.r, c.g, c.b, c.a))
        });
        for (field, v) in [
            ("system.dark", Value::Bool(sys.dark)),
            ("system.accent", accent),
            ("system.contrast", Value::Num(sys.contrast, Num::Float)),
            ("system.reduced_motion", Value::Bool(sys.reduced_motion)),
        ] {
            let _ = host.set_initial(&rt, field, v);
        }
        let inst = Instance::from_build(&rt, build, host, storage);
        let errors = inst
            .flush()
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.what, e.error))
            .collect();
        inst.wait_images(IMAGE_WAIT);
        Mount {
            inst,
            _rt: rt,
            errors,
        }
    }

    fn produce(&self, format: &Format, template: Option<&Source>) -> Result<Vec<u8>, String> {
        let template = match template {
            Some(s) => Some((s, compile_template(s)?)),
            None => None,
        };
        let other = match &template {
            Some((_, t)) if t.reads_other_mode() => match self.inst.other_table() {
                Ok(o) => o,
                Err(e) if is_pending(&e) => {
                    return Err(format!("the other mode's palette is still loading ({e})"));
                }
                Err(e) => return Err(format!("the other mode has no palette: {e}")),
            },
            _ => None,
        };
        produce(
            &self.inst,
            format,
            template.as_ref().map(|(s, t)| (*s, t)),
            other.as_ref(),
        )
        .map_err(
            |e| match e.starts_with(NO_PALETTE) && !self.errors.is_empty() {
                true => format!("{e}\n{}", self.errors.join("\n")),
                false => e,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse(&args("json")).unwrap(),
            Ask {
                format: Format::Json,
                palette: None,
                out: None
            }
        );
        let a = parse(&args("--template t.css -o out.css --palette x")).unwrap();
        assert_eq!(a.format, Format::Template("t.css".into()));
        assert_eq!(a.out, Some("out.css".into()));
        assert_eq!(a.palette.as_deref(), Some("x"));
        for bad in [
            "",
            "json base24",
            "yaml",
            "--template",
            "json -o",
            "json -o a -o b",
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad}");
        }
    }
}
