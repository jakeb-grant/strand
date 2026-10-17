//! `strand-eval promote` (docs/eval.md, "Matrix"): a run's scenario (a
//! matrix cell, or any `runs/<stamp>/<id>`) written out as a regression,
//! fully expanded: its config inline, its steps as they ran, its `about`
//! naming where it came from, and, for a finding accepted for now, an
//! `expect_fail` that turns into a failure the day it passes. It is data
//! only: the gate picks it up on its next run, nothing rebuilds.

use std::path::{Path, PathBuf};

use crate::scenario::{CheckId, Place, Scenario, place_of};
use crate::{Error, report};

/// What to promote and how.
#[derive(Debug, Default)]
pub struct Ask {
    /// The run's scenario directory (`<run>/<id>`).
    pub from: PathBuf,
    /// The new scenario's id (and file name).
    pub name: String,
    /// Checks it is expected to fail, with `note` as the reason.
    pub xfail: Vec<String>,
    pub note: Option<String>,
    /// Where it goes (default eval/scenarios/regressions).
    pub to: Option<PathBuf>,
    /// Overwrite a scenario of that name.
    pub force: bool,
}

/// The header and the rest (`=== …` sections) of a scenario's text.
fn split(text: &str) -> (&str, &str) {
    let mut at = 0;
    for l in text.split_inclusive('\n') {
        if l.starts_with("=== ") {
            return (&text[..at], &text[at..]);
        }
        at += l.len();
    }
    (text, "")
}

/// The scenario directory `from` names: itself when it holds a
/// `scenario.scn`; else its one scenario directory (a matrix cell's
/// `<cell>@<hz>/`); else the one `<from>@<hz>/` beside it (a matrix cell
/// named without its rate).
pub fn resolve(from: &Path) -> Result<PathBuf, Error> {
    if from.join("scenario.scn").is_file() {
        return Ok(from.to_path_buf());
    }
    let inner = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("scenario.scn").is_file())
            .collect()
    };
    let mut found = if from.is_dir() {
        inner(from)
    } else {
        Vec::new()
    };
    if found.is_empty()
        && let (Some(dir), Some(name)) = (from.parent(), from.file_name())
    {
        let name = format!("{}@", name.to_string_lossy());
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        for cell in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if cell.file_name().to_string_lossy().starts_with(&name) {
                found.extend(inner(&cell.path()));
            }
        }
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(Error::Usage(format!(
            "{}: no scenario.scn there (a run's <id>/ or a matrix cell)",
            from.display()
        ))),
        many => Err(Error::Usage(format!(
            "{}: {} scenarios match; name one: {}",
            from.display(),
            many.len(),
            many.iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Writes the promoted scenario; returns its path.
pub fn promote(ask: &Ask) -> Result<PathBuf, Error> {
    let from = resolve(&ask.from)?;
    let ask = &Ask {
        from,
        name: ask.name.clone(),
        xfail: ask.xfail.clone(),
        note: ask.note.clone(),
        to: ask.to.clone(),
        force: ask.force,
    };
    let e = report::read_entry(&ask.from)?;
    // A local scenario is the owner's (its config, its fonts): nothing
    // from it is committed (docs/eval.md, "Local configs and fonts").
    // A finding in one is written up as a new scenario from scratch.
    let file = Path::new(&e.trace.scenario.file);
    if e.trace.scenario.file.is_empty() || place_of(file) == Place::Local {
        return Err(Error::Usage(format!(
            "{}: promote takes committed scenarios and matrix cells only; `{}` is {} \
             (write the regression from scratch, docs/eval.md \"Local configs and fonts\")",
            ask.from.display(),
            e.trace.scenario.file,
            if e.trace.scenario.file.is_empty() {
                "no scenario file"
            } else {
                "a local scenario"
            }
        )));
    }
    if e.source.is_empty() {
        return Err(Error::Usage(format!(
            "{}: no scenario.scn (a report written before promote existed?)",
            ask.from.display()
        )));
    }
    if !ask.xfail.is_empty() && ask.note.as_deref().is_none_or(|n| n.trim().is_empty()) {
        return Err(Error::Usage(
            "promote: --xfail needs --note (the reason expect_fail records)".into(),
        ));
    }
    for c in &ask.xfail {
        if CheckId::parse(c).is_none() {
            return Err(Error::Usage(format!("--xfail {c}: not a check")));
        }
    }
    let (head, rest) = split(&e.source);
    let mut table: toml::Table = toml::from_str(head)
        .map_err(|x| Error::Config(format!("{}/scenario.scn: {x}", ask.from.display())))?;
    let mut rest = rest.to_string();
    // A config named by path is read in, so the regression stands alone.
    if let Some(cfg) = table.remove("config") {
        let rel = cfg.as_str().unwrap_or_default();
        let base = Path::new(&e.trace.scenario.file)
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let p = base.join(rel);
        let found = strand_compiler::source::find_files(&p)
            .map_err(|x| Error::Io(p.display().to_string(), x))?;
        let root = if p.is_file() {
            p.parent().unwrap_or(&p).to_path_buf()
        } else {
            p.clone()
        };
        let mut sections = String::new();
        for f in found.files {
            let text =
                std::fs::read_to_string(&f).map_err(|x| Error::Io(f.display().to_string(), x))?;
            let name = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
            sections += &format!("=== {name}\n{text}");
            if !sections.ends_with('\n') {
                sections.push('\n');
            }
            sections.push('\n');
        }
        rest = sections + &rest;
    }
    table.insert("id".into(), toml::Value::String(ask.name.clone()));
    let about = table
        .get("about")
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();
    let b = &e.trace.build;
    table.insert(
        "about".into(),
        toml::Value::String(format!(
            "Promoted from {} (scenario {}, {}{}). {about}",
            ask.from.display(),
            e.id,
            b.rev,
            if b.dirty { " with changes" } else { "" }
        )),
    );
    if !ask.xfail.is_empty() {
        let mut ef = table
            .get("expect_fail")
            .and_then(|x| x.as_table())
            .cloned()
            .unwrap_or_default();
        for c in &ask.xfail {
            ef.insert(
                c.clone(),
                toml::Value::String(ask.note.clone().unwrap_or_default()),
            );
        }
        table.insert("expect_fail".into(), toml::Value::Table(ef));
    }
    let header = toml::to_string(&table).map_err(|x| Error::Run(format!("promote: {x}")))?;
    let text = format!("{header}\n{rest}");
    let dir = ask.to.clone().unwrap_or_else(|| {
        crate::scenario::eval_root()
            .join("scenarios")
            .join("regressions")
    });
    let path = dir.join(format!("{}.scn", ask.name));
    // It must parse where it goes, by a committed scenario's rules.
    Scenario::parse_in(&text, &path, Place::Committed)?;
    if path.exists() && !ask.force {
        return Err(Error::Usage(format!(
            "{} exists (--force replaces it)",
            path.display()
        )));
    }
    report::write(&path, text.as_bytes())?;
    Ok(path)
}
