//! (replay-clip) `strand-eval replay clip <report> --from T [--to T]
//! [--node SEL]… [--note TEXT] [--json] [--save FILE]`: a clip of a
//! glitch report's replay, "these elements, these frames, where each was
//! drawn and where it rests, for this time slice", as text to paste to
//! an agent, as JSON, or as a .zip with both and the slice's frames
//! cropped around the selection (docs/eval.md, "A clip").
//!
//! A clip is made in one place, `eval/viewer/clip.js`: the viewer's
//! replay page runs it on the trace it holds, and this runs the same
//! file in QuickJS on the replay's `trace.json`, `checks.json` and
//! `fidelity.json`, so the page and the command line give the same
//! text, JSON and frames for the same selection spec (report, slice,
//! element keys, note). Its data always comes from the recorded trace
//! and the replay's fidelity, never from what a page draws.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use rquickjs::{ArrayBuffer, Context, Ctx, Function, Object, Runtime, Value};
use serde_json::Value as Json;

use crate::report::explain::MOTIONS;
use crate::report::pack::{Composer, Patch, patch_file};

/// The clip's maker, as the viewer embeds it.
pub const CLIP_JS: &str = include_str!("../../viewer/clip.js");

/// What a clip asks for: the time slice, as given (`-3.2s`, `-3200ms`,
/// `k5226`; `to` the report when `None`), the elements (each a node's
/// key, `#id` or `file:line[:col]`), and a note.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub from: String,
    pub to: Option<String>,
    pub nodes: Vec<String>,
    pub note: String,
}

impl Spec {
    /// The spec as clip.js reads it.
    pub fn json(&self) -> String {
        serde_json::json!({
            "from": self.from,
            "to": self.to,
            "nodes": self.nodes,
            "note": self.note,
        })
        .to_string()
    }
}

/// A clip: its text, its JSON (as text), and with `--save` its .zip.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Made {
    pub text: String,
    pub json: String,
    pub zip: Option<Vec<u8>>,
    /// Frames whose pixels the source did not hold (left out of the zip).
    pub missing: Vec<String>,
}

/// Why a clip was not made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipError {
    /// A selector names no node of the replay.
    UnknownNode(String),
    /// The slice holds nothing: `--from` not before `--to`, or nothing
    /// drawn by its end.
    EmptySlice(String),
    /// A time that does not read, or one outside the window.
    Spec(String),
    /// No replay to clip: a report not replayed yet, or a directory that
    /// is neither a report nor a replay.
    NoReplay(String),
    /// A file of the replay could not be read.
    Io(String),
    /// clip.js failed (a bug in it, or a trace it cannot read).
    Engine(String),
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipError::UnknownNode(m)
            | ClipError::EmptySlice(m)
            | ClipError::Spec(m)
            | ClipError::NoReplay(m)
            | ClipError::Io(m) => write!(f, "clip: {m}"),
            ClipError::Engine(m) => write!(f, "clip: clip.js failed (a bug in strand-eval): {m}"),
        }
    }
}

impl std::error::Error for ClipError {}

impl From<ClipError> for crate::Error {
    fn from(e: ClipError) -> Self {
        match e {
            ClipError::Engine(_) => crate::Error::Run(e.to_string()),
            _ => crate::Error::Usage(e.to_string()),
        }
    }
}

/// A replay's report on disk: its directory (`fidelity.json`) and its
/// scenario's (`trace.json`, the frames).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    pub dir: PathBuf,
    pub entry: PathBuf,
}

impl Replay {
    /// `path`: a report (its `replay/`), or a replay's report itself (a
    /// `--out` directory).
    pub fn find(path: &Path) -> Result<Replay, ClipError> {
        let dir = if path.join("bundle.json").is_file() {
            let r = path.join("replay");
            if !r.join("fidelity.json").is_file() {
                return Err(ClipError::NoReplay(format!(
                    "{} has not been replayed yet (no replay/fidelity.json): run `strand replay {}` first",
                    path.display(),
                    path.display()
                )));
            }
            r
        } else if path.join("fidelity.json").is_file() {
            path.to_path_buf()
        } else {
            return Err(ClipError::NoReplay(format!(
                "{} is neither a glitch report nor a replay's report (no bundle.json, no fidelity.json)",
                path.display()
            )));
        };
        let entries =
            crate::report::entry_dirs(&dir).map_err(|e| ClipError::NoReplay(e.to_string()))?;
        let [entry] = entries.as_slice() else {
            return Err(ClipError::NoReplay(format!(
                "{} holds {} scenarios; a replay's report holds one",
                dir.display(),
                entries.len()
            )));
        };
        Ok(Replay {
            dir,
            entry: entry.clone(),
        })
    }
}

fn read(p: &Path) -> Result<String, ClipError> {
    std::fs::read_to_string(p).map_err(|e| ClipError::Io(format!("{}: {e}", p.display())))
}

/// A stored file of `entry`: only a plain relative path under the
/// scenario's directory.
fn stored(entry: &Path, path: &str) -> Option<Vec<u8>> {
    let rel = Path::new(path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    std::fs::read(entry.join(rel)).ok()
}

/// The frames of the replay's report, rebuilt as Rust's own report
/// reader rebuilds them ([`Composer`]): what clip.js crops on the
/// command line. (The page decodes its own; the parity test holds the
/// two .zips equal, so the two decodings are.)
fn frames_of(trace: &str, entry: PathBuf) -> Result<Composer<'static>, ClipError> {
    let v: Json = serde_json::from_str(trace)
        .map_err(|e| ClipError::Io(format!("{}/trace.json: {e}", entry.display())))?;
    let mut patches: BTreeMap<String, Patch> = BTreeMap::new();
    for (name, p) in v["packed"]["patches"].as_object().into_iter().flatten() {
        let p = Patch::from_json(p).ok_or_else(|| {
            ClipError::Io(format!(
                "{}/trace.json: packed.patches `{name}` does not read",
                entry.display()
            ))
        })?;
        patches.insert(name.clone(), p);
    }
    let patched: BTreeSet<String> = patches.keys().cloned().collect();
    Ok(Composer::owned(patches, move |name: &str| {
        let path = if patched.contains(name) {
            patch_file(name)
        } else {
            name.to_string()
        };
        stored(&entry, &path)
    }))
}

/// The clip `spec` of the replay `r`, made by clip.js from its files;
/// `zip`: its .zip too.
pub fn make(r: &Replay, spec: &Spec, zip: bool) -> Result<Made, ClipError> {
    let trace = read(&r.entry.join("trace.json"))?;
    let checks = match std::fs::read_to_string(r.entry.join("checks.json")) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(ClipError::Io(format!(
                "{}/checks.json: {e}",
                r.entry.display()
            )));
        }
    };
    let fidelity = read(&r.dir.join("fidelity.json"))?;
    let motions = serde_json::to_string(&MOTIONS).unwrap_or_else(|_| "[]".into());
    let frames = if zip {
        Some(frames_of(&trace, r.entry.clone())?)
    } else {
        None
    };
    let out = run(
        vec![
            ("TRACE", Some(trace)),
            ("CHECKS", checks),
            ("FIDELITY", Some(fidelity)),
            ("MOTIONS", Some(motions)),
            ("SPEC", Some(spec.json())),
        ],
        zip,
        frames,
        "JSON.stringify(StrandClip.cli(TRACE, CHECKS, FIDELITY, MOTIONS, SPEC, WANT_ZIP, __clipFrame))",
    )?;
    made_of(out)
}

/// The clip `spec_json` as the viewer's page makes it, from the page's
/// own data (`html`: the page): what the parity test holds the command
/// line to. `spec_json` is the page's spec (`from`, `to`, `nodes`,
/// `note`).
pub fn of_page(html: &str, spec_json: &str, zip: bool) -> Result<Made, ClipError> {
    let data = crate::report::html::embedded(html)
        .ok_or_else(|| ClipError::NoReplay("the page holds no #run data".into()))?;
    let out = run(
        vec![
            ("PAGE", Some(data.to_string())),
            ("SPEC", Some(spec_json.to_string())),
        ],
        zip,
        None,
        "(function () { var D = JSON.parse(PAGE); \
         var r = StrandClip.page(D, D.runs[0].scenarios[0], JSON.parse(SPEC), WANT_ZIP); \
         if (r.zip) r.zip = StrandClip.b64enc(r.zip); delete r.data; return JSON.stringify(r); })()",
    )?;
    made_of(out)
}

fn made_of(out: Json) -> Result<Made, ClipError> {
    if let Some(r) = out.get("refused") {
        let m = r["message"].as_str().unwrap_or("refused").to_string();
        return Err(match r["code"].as_str() {
            Some("unknown-node") => ClipError::UnknownNode(m),
            Some("empty-slice") => ClipError::EmptySlice(m),
            Some("no-replay") => ClipError::NoReplay(m),
            _ => ClipError::Spec(m),
        });
    }
    let text = |k: &str| out[k].as_str().map(str::to_string);
    let zip = match out["zip"].as_str() {
        Some(b) => Some(
            base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|e| ClipError::Engine(format!("the .zip: {e}")))?,
        ),
        None => None,
    };
    Ok(Made {
        text: text("text").ok_or_else(|| ClipError::Engine("no text".into()))?,
        json: text("json").ok_or_else(|| ClipError::Engine("no JSON".into()))?,
        zip,
        missing: out["missing"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The message of a JavaScript exception.
fn exception(ctx: &Ctx<'_>) -> String {
    let v = ctx.catch();
    if let Some(e) = v.as_exception() {
        return format!(
            "{}{}",
            e.message().unwrap_or_default(),
            e.stack().map(|s| format!("\n{s}")).unwrap_or_default()
        );
    }
    v.as_string()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| "an exception".into())
}

/// `__clipFrame(name)`: frame `name` of `frames` as `{w, h, px}` (RGBA
/// in an ArrayBuffer), or null.
fn frame_fn<'js>(
    ctx: &Ctx<'js>,
    frames: Option<Composer<'static>>,
) -> rquickjs::Result<Function<'js>> {
    let frames = RefCell::new(frames);
    Function::new(
        ctx.clone(),
        move |c: Ctx<'js>, name: String| -> rquickjs::Result<Value<'js>> {
            let got = frames.borrow_mut().as_mut().and_then(|f| f.frame(&name));
            match got {
                Some((w, h, px)) => {
                    let o = Object::new(c.clone())?;
                    o.set("w", w)?;
                    o.set("h", h)?;
                    o.set("px", ArrayBuffer::new(c.clone(), px)?)?;
                    Ok(o.into_value())
                }
                None => Ok(Value::new_null(c)),
            }
        },
    )
}

/// Runs clip.js with `globals` (strings, or null), `WANT_ZIP` and
/// `__clipFrame` (a frame by name, from `frames`), then `driver`, an
/// expression giving the result as JSON text.
fn run(
    globals: Vec<(&str, Option<String>)>,
    zip: bool,
    frames: Option<Composer<'static>>,
    driver: &str,
) -> Result<Json, ClipError> {
    let engine = |e: rquickjs::Error| ClipError::Engine(e.to_string());
    let rt = Runtime::new().map_err(engine)?;
    rt.set_max_stack_size(8 << 20);
    let cx = Context::full(&rt).map_err(engine)?;
    let text: String = cx.with(|ctx| -> Result<String, ClipError> {
        let js = |ctx: &Ctx<'_>, e: rquickjs::Error| match e {
            rquickjs::Error::Exception => ClipError::Engine(exception(ctx)),
            e => ClipError::Engine(e.to_string()),
        };
        ctx.eval::<(), _>(CLIP_JS).map_err(|e| js(&ctx, e))?;
        let g = ctx.globals();
        for (k, v) in globals {
            g.set(k, v).map_err(|e| js(&ctx, e))?;
        }
        g.set("WANT_ZIP", zip).map_err(|e| js(&ctx, e))?;
        let f = frame_fn(&ctx, frames).map_err(|e| js(&ctx, e))?;
        g.set("__clipFrame", f).map_err(|e| js(&ctx, e))?;
        ctx.eval::<String, _>(driver).map_err(|e| js(&ctx, e))
    })?;
    serde_json::from_str(&text).map_err(|e| ClipError::Engine(format!("its result: {e}")))
}

/// (replay-clip) The options `replay clip` takes a value with; `strand
/// replay clip` keeps the same list (crates/strand/src/report/replay.rs
/// `CLIP_VALUED`), held to it by eval/tests/clip.rs.
pub const CLIP_VALUED: [&str; 5] = ["--from", "--to", "--node", "--note", "--save"];

/// The usage line.
pub const USAGE: &str = "usage: strand-eval replay clip <report> --from T [--to T] [--node SEL]... \
     [--note TEXT] [--json] [--save FILE.zip]\n  T: seconds before the report (-3.2s, -3200ms) or a \
     tick (k5226); --to defaults to the report\n  SEL: a node's key, #id, or file:line[:col] in the \
     config\n  <report>: a glitch report (its replay/), or a replay's report (`--out` of `replay`)";
