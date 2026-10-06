//! The portal's appearance settings as the `system` service: `system.dark`,
//! `system.accent` and `system.contrast` (design.md, "Change sources").
//!
//! The portal follower (`strand_watch::PortalSettings`) runs on a thread
//! of its own and sends [`SystemBatch`]es; the logic thread writes them
//! into the host. The boot read is written as initial values (`on change`
//! does not fire for them); later batches are ordinary writes. The last
//! values are kept in the state directory and written before the first
//! frame, so a dark desktop never boots into a light frame while the
//! portal answers (it may take up to `BOOT_READ_TIMEOUT`).

use std::path::{Path, PathBuf};

use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::Color;
use strand_watch::{Contrast, SystemBatch, SystemSetting};

/// The graph value a setting writes.
pub fn value(s: &SystemSetting) -> Value {
    match s {
        SystemSetting::Dark { dark, .. } => Value::Bool(*dark),
        SystemSetting::Accent(Some([r, g, b])) => {
            Value::Color(Color::rgb(*r as f32, *g as f32, *b as f32))
        }
        SystemSetting::Accent(None) => Value::Null,
        SystemSetting::Contrast(Contrast::High) => Value::float(1.0),
        SystemSetting::Contrast(Contrast::Normal) => Value::float(0.0),
    }
}

/// Writes `settings` into the host; `initial`: as boot values.
pub fn apply(rt: &Runtime, host: &SchemaHost, settings: &[SystemSetting], initial: bool) {
    for s in settings {
        let r = if initial {
            host.set_initial(rt, s.path(), value(s))
        } else {
            host.set(rt, s.path(), value(s))
        };
        if let Err(e) = r {
            log::warn!("{}: {e}", s.path());
        }
    }
}

/// The last portal values, kept across restarts.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Last {
    pub settings: Vec<SystemSetting>,
}

impl Last {
    /// `<dir>/system`, if a state directory exists.
    pub fn file(dir: Option<PathBuf>) -> Option<PathBuf> {
        dir.map(|d| d.join("system"))
    }

    /// Reads the kept values (missing or broken lines are skipped).
    pub fn load(path: &Path) -> Last {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Last::default();
        };
        let mut settings = Vec::new();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let s = match (k.trim(), v.trim()) {
                // `dark=<bool>,<scheme>` (an older file has no scheme).
                ("dark", v) => {
                    use strand_watch::ColorScheme;
                    let (dark, scheme) = v.split_once(',').unwrap_or((v, ""));
                    let dark = dark.trim() == "true";
                    let scheme = match scheme.trim() {
                        "prefer-dark" => ColorScheme::PreferDark,
                        "prefer-light" => ColorScheme::PreferLight,
                        "none" => ColorScheme::NoPreference,
                        _ if dark => ColorScheme::PreferDark,
                        _ => ColorScheme::NoPreference,
                    };
                    SystemSetting::Dark { dark, scheme }
                }
                ("accent", "none") => SystemSetting::Accent(None),
                ("accent", v) => match Color::from_hex(v) {
                    Some(c) => SystemSetting::Accent(Some([c.r as f64, c.g as f64, c.b as f64])),
                    None => continue,
                },
                ("contrast", "high") => SystemSetting::Contrast(Contrast::High),
                ("contrast", _) => SystemSetting::Contrast(Contrast::Normal),
                _ => continue,
            };
            settings.push(s);
        }
        Last { settings }
    }

    /// Takes in a batch's values (each setting replaces the kept one).
    /// Returns whether anything changed.
    pub fn merge(&mut self, batch: &SystemBatch) -> bool {
        let mut changed = false;
        for s in &batch.settings {
            match self.settings.iter_mut().find(|o| o.path() == s.path()) {
                Some(o) if o == s => {}
                Some(o) => {
                    *o = *s;
                    changed = true;
                }
                None => {
                    self.settings.push(*s);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Writes the kept values (temp file plus rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut out = String::new();
        for s in &self.settings {
            match s {
                SystemSetting::Dark { dark, scheme } => {
                    let scheme = match scheme {
                        strand_watch::ColorScheme::PreferDark => "prefer-dark",
                        strand_watch::ColorScheme::PreferLight => "prefer-light",
                        strand_watch::ColorScheme::NoPreference => "none",
                    };
                    out.push_str(&format!("dark={dark},{scheme}\n"));
                }
                SystemSetting::Accent(None) => out.push_str("accent=none\n"),
                SystemSetting::Accent(Some([r, g, b])) => {
                    let [r, g, b, _] = Color::rgb(*r as f32, *g as f32, *b as f32).to_rgba8();
                    out.push_str(&format!("accent=#{r:02x}{g:02x}{b:02x}\n"));
                }
                SystemSetting::Contrast(Contrast::High) => out.push_str("contrast=high\n"),
                SystemSetting::Contrast(Contrast::Normal) => out.push_str("contrast=normal\n"),
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Per process: two `strand run`s sharing the state directory
        // never write the same temp file.
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn last_values_round_trip_and_merge() {
        let dir = std::env::temp_dir().join(format!("strand-system-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = Last::file(Some(dir.clone())).unwrap();
        assert_eq!(Last::load(&path), Last::default());
        let mut last = Last::default();
        let batch = SystemBatch {
            settings: vec![
                SystemSetting::Dark {
                    dark: true,
                    scheme: strand_watch::ColorScheme::PreferDark,
                },
                SystemSetting::Accent(Some([1.0, 0.0, 0.0])),
                SystemSetting::Contrast(Contrast::High),
            ],
            at_boot: true,
            received: Instant::now(),
        };
        assert!(last.merge(&batch));
        assert!(!last.merge(&batch), "the same values change nothing");
        last.save(&path).unwrap();
        assert_eq!(Last::load(&path), last);
        assert_eq!(
            value(&last.settings[1]),
            Value::Color(Color::rgb(1.0, 0.0, 0.0))
        );
        assert_eq!(value(&last.settings[2]), Value::float(1.0));
        // An explicit light preference survives a restart as such.
        let light = SystemBatch {
            settings: vec![SystemSetting::Dark {
                dark: false,
                scheme: strand_watch::ColorScheme::PreferLight,
            }],
            at_boot: false,
            received: Instant::now(),
        };
        assert!(last.merge(&light));
        last.save(&path).unwrap();
        assert_eq!(Last::load(&path), last);
        // The older form still reads.
        std::fs::write(&path, "dark=true\n").unwrap();
        assert_eq!(
            Last::load(&path).settings,
            [SystemSetting::Dark {
                dark: true,
                scheme: strand_watch::ColorScheme::PreferDark
            }]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
