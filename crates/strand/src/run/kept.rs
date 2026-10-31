//! (kept-size) The largest size each panel has had, kept across sessions so
//! a panel maps at the size it will need and never resizes to open
//! (decisions.md kept-size). Tied to the config that measured it: a changed
//! config starts again, so a card made smaller does not stay reserved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE: &str = "panel-sizes.json";

/// The file, and the config it belongs to.
#[derive(Debug)]
pub(crate) struct KeptFile {
    path: PathBuf,
    config: u64,
}

impl KeptFile {
    /// `None` without a state directory: panels then measure each session.
    pub(crate) fn of(config_dir: &Path) -> Option<Self> {
        let path = strand_core::persist::state_dir()?.join(FILE);
        Some(Self {
            path,
            config: config_hash(config_dir),
        })
    }

    /// What this config measured before: nothing for a missing, unreadable
    /// or older file.
    pub(crate) fn load(&self) -> BTreeMap<String, (f32, f32)> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return BTreeMap::new();
        };
        parse(&text, self.config).unwrap_or_default()
    }

    /// Written whole and renamed into place. A failure only costs the next
    /// session a resize.
    pub(crate) fn save(&self, kept: &BTreeMap<String, (f32, f32)>) {
        let text = encode(kept, self.config);
        let tmp = self.path.with_extension("tmp");
        let wrote = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&tmp, text))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = wrote {
            log::warn!("{}: {e}", self.path.display());
        }
    }
}

fn encode(kept: &BTreeMap<String, (f32, f32)>, config: u64) -> String {
    let sizes: serde_json::Map<String, serde_json::Value> = kept
        .iter()
        .map(|(n, (w, h))| (n.clone(), serde_json::json!([w, h])))
        .collect();
    serde_json::json!({"config": config.to_string(), "sizes": sizes}).to_string()
}

fn parse(text: &str, config: u64) -> Option<BTreeMap<String, (f32, f32)>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("config")?.as_str()? != config.to_string() {
        return None;
    }
    let mut out = BTreeMap::new();
    for (name, size) in v.get("sizes")?.as_object()? {
        let (w, h) = (size.get(0)?.as_f64()?, size.get(1)?.as_f64()?);
        out.insert(name.clone(), (w as f32, h as f32));
    }
    Some(out)
}

/// Every `.strand` file under `dir`, names and contents, in name order.
fn config_hash(dir: &Path) -> u64 {
    let mut files = Vec::new();
    let mut walk = vec![dir.to_path_buf()];
    while let Some(d) = walk.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk.push(p);
            } else if p.extension().is_some_and(|x| x == "strand") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut bytes = Vec::new();
    for f in files {
        bytes.extend_from_slice(f.to_string_lossy().as_bytes());
        bytes.extend(std::fs::read(&f).unwrap_or_default());
    }
    strand_core::persist::value_hash(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_come_back_only_for_the_config_that_measured_them() {
        let kept = BTreeMap::from([("strand-Island".to_string(), (0.0, 378.0))]);
        let text = encode(&kept, 7);
        assert_eq!(parse(&text, 7), Some(kept));
        assert_eq!(parse(&text, 8), None);
        assert_eq!(parse("not json", 7), None);
    }
}
