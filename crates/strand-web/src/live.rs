//! A docs example run live (docs/live-examples.md): what a figure's live
//! mode runs, as the site writes it into the page (strand-site's
//! `example::live`), booted into a [`Shell`] with its steps in a
//! [`Player`], and the icon theme files a host without a file system
//! supplies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::shell::{LoadError, Options, Shell};
use crate::steps::{self, Player};
use crate::surfaces::Screen;

/// Where the web host's icon theme lies: no disk has it, the host
/// supplies its files ([`install_icons`]).
pub const ICON_BASE: &str = "/strand-web/icons";

/// An example's scenario as its figure carries it: its config files,
/// its `[services]` (each value as eval reads its text), its screen,
/// its clock's rate and its steps section.
#[derive(Clone, Debug, PartialEq)]
pub struct Example {
    pub name: String,
    pub id: String,
    pub files: Vec<(String, String)>,
    pub services: Vec<(String, String)>,
    pub screen: Screen,
    pub hz: u32,
    pub steps: String,
}

fn pairs(v: &Value, key: &str) -> Result<Vec<(String, String)>, String> {
    v[key]
        .as_array()
        .ok_or(format!("no `{key}` list"))?
        .iter()
        .map(|p| match p.as_array().map(Vec::as_slice) {
            Some([Value::String(a), Value::String(b)]) => Ok((a.clone(), b.clone())),
            _ => Err(format!("`{key}`: each is [name, text]")),
        })
        .collect()
}

impl Example {
    /// The JSON the site writes (`{"name", "id", "files": [[name,
    /// text]], "services": [[path, text]], "screen": {"width", "height",
    /// "scale"}, "hz", "steps"}`).
    pub fn from_json(text: &str) -> Result<Example, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let s = &v["screen"];
        let num = |k: &str| s[k].as_f64().ok_or(format!("no screen `{k}`"));
        let text_of = |k: &str| v[k].as_str().map(str::to_string).ok_or(format!("no `{k}`"));
        Ok(Example {
            name: text_of("name")?,
            id: text_of("id")?,
            files: pairs(&v, "files")?,
            services: pairs(&v, "services")?,
            screen: Screen {
                width: num("width")? as u32,
                height: num("height")? as u32,
                scale: num("scale")?,
            },
            hz: v["hz"].as_u64().unwrap_or(60).clamp(1, 1000) as u32,
            steps: text_of("steps")?,
        })
    }

    /// Its services as its run sets them: in dark mode the system's
    /// `dark` first, as the site's dark twin of the scenario sets it
    /// (`strand-site twin`), then the scenario's own; with reduced
    /// motion, the system's `reduced_motion` too.
    pub fn services_for(&self, dark: bool, reduced_motion: bool) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if dark {
            out.push(("system.dark".to_string(), "true".to_string()));
        }
        out.extend(self.services.iter().cloned());
        if reduced_motion {
            out.push(("system.reduced_motion".to_string(), "true".to_string()));
        }
        out
    }

    /// Boots it, with `files` in place of its own when given (the
    /// reader's edit): the shell at its first tick and its steps ready
    /// to play.
    pub fn boot(
        &self,
        files: Option<&[(String, String)]>,
        dark: bool,
        reduced_motion: bool,
        fonts: Vec<Arc<Vec<u8>>>,
    ) -> Result<(Shell, Player), LoadError> {
        let lines = steps::parse(&self.steps).map_err(|e| LoadError {
            short: vec![format!("steps: {e}")],
            text: format!("the example's steps: {e}"),
        })?;
        let shell = Shell::load(
            files.unwrap_or(&self.files),
            &self.services_for(dark, reduced_motion),
            Options {
                screen: self.screen,
                hz: self.hz,
                fonts,
            },
        )?;
        Ok((shell, Player::new(lines)))
    }
}

/// Supplies the icon theme files `files` (paths relative to the icon
/// base, `Adwaita/index.theme`) as the only icons there are: lookups
/// look in [`ICON_BASE`] alone, and find these there.
pub fn install_icons(files: Vec<(String, Vec<u8>)>) {
    let base = PathBuf::from(ICON_BASE);
    let map: HashMap<PathBuf, Arc<[u8]>> = files
        .into_iter()
        .map(|(rel, bytes)| (base.join(rel), Arc::from(bytes)))
        .collect();
    strand_icons::set_base_dirs(Some(vec![base]));
    strand_icons::set_files(map);
}

/// The icon theme files `examples` draw, in both modes, as a lookup in
/// the base directories in use finds them now (natively: the disk's),
/// each with its theme's `index.theme` and hicolor's: (path relative to
/// its base directory, path on disk), sorted, each once. What a host
/// without a file system must supply ([`install_icons`]) to draw them.
pub fn icons_drawn(
    examples: &[Example],
    fonts: &[Arc<Vec<u8>>],
) -> Result<Vec<(String, PathBuf)>, String> {
    let bases = strand_icons::base_dirs();
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |rel: PathBuf, abs: PathBuf| {
        let rel = rel.to_string_lossy().into_owned();
        if !found.iter().any(|(r, _)| *r == rel) {
            found.push((rel, abs));
        }
    };
    for ex in examples {
        for dark in [false, true] {
            let (mut shell, mut player) = ex
                .boot(None, dark, false, fonts.to_vec())
                .map_err(|e| format!("{}: {e}", ex.name))?;
            let mut files = shell.files_drawn();
            while player
                .advance(&mut shell)
                .map_err(|e| format!("{}: {e}", ex.name))?
                != crate::steps::Advance::Done
            {
                files.extend(shell.files_drawn());
            }
            for f in files {
                let Some(base) = bases.iter().find(|b| f.starts_with(b)) else {
                    return Err(format!(
                        "{}: draws {}, which is in no icon base directory",
                        ex.name,
                        f.display()
                    ));
                };
                let rel = f
                    .strip_prefix(base)
                    .map_err(|e| e.to_string())?
                    .to_path_buf();
                if let Some(theme) = rel.components().next() {
                    for t in [PathBuf::from(theme.as_os_str()), PathBuf::from("hicolor")] {
                        let index = t.join("index.theme");
                        if base.join(&index).is_file() {
                            add(index.clone(), base.join(&index));
                        }
                    }
                }
                add(rel, f);
            }
        }
    }
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_example_reads_from_the_json_the_site_writes() {
        let ex = Example::from_json(
            r#"{"name": "t", "id": "docs-t", "files": [["t.strand", "bar B { height: 30 }\n"]],
               "services": [["battery.percent", "0.5"]],
               "screen": {"width": 400, "height": 100, "scale": 2.0}, "hz": 60, "steps": "rest\n"}"#,
        )
        .unwrap();
        assert_eq!(ex.files[0].0, "t.strand");
        assert_eq!(ex.screen.scale, 2.0);
        assert_eq!(
            ex.services_for(true, false),
            vec![
                ("system.dark".to_string(), "true".to_string()),
                ("battery.percent".to_string(), "0.5".to_string())
            ]
        );
        assert!(Example::from_json(r#"{"name": "t"}"#).is_err());
    }
}
