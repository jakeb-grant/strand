//! The last values of the `system` service (the portal's appearance
//! settings: `system.dark`, `.accent`, `.contrast`, `.reduced_motion`),
//! kept across restarts (design.md, "Wallpaper palettes": boot never
//! flashes default colours).
//!
//! The service itself (`strand_services::system`) follows the portal on
//! the shared services runtime. `strand run` seeds it with these values
//! before the first frame (`Client::seed`: boot values, so `on change`
//! does not fire for them), and writes them back off the logic thread
//! whenever the service reports new ones, so a dark desktop never boots
//! into a light frame while the portal answers.

use std::path::{Path, PathBuf};

use strand_scene::Color;
use strand_services::system::System;
use strand_watch::{ColorScheme, Contrast, SystemSetting};

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
                ("reduced_motion", v) => SystemSetting::ReducedMotion(v == "true"),
                _ => continue,
            };
            settings.push(s);
        }
        Last { settings }
    }

    /// The values `system` holds now.
    pub fn of(s: &System) -> Last {
        let scheme = if s.dark {
            ColorScheme::PreferDark
        } else {
            ColorScheme::NoPreference
        };
        let contrast = if s.contrast >= 0.5 {
            Contrast::High
        } else {
            Contrast::Normal
        };
        Last {
            settings: vec![
                SystemSetting::Dark {
                    dark: s.dark,
                    scheme,
                },
                SystemSetting::Accent(s.accent.map(|c| [c.r as f64, c.g as f64, c.b as f64])),
                SystemSetting::Contrast(contrast),
                SystemSetting::ReducedMotion(s.reduced_motion),
            ],
        }
    }

    /// Put the kept values into `s` (the service's seed).
    pub fn seed(&self, s: &mut System) {
        for setting in &self.settings {
            s.apply_setting(setting);
        }
    }

    /// Writes the kept values (temp file plus rename; tests). `strand
    /// run` queues [`Last::to_text`] on a [`strand_theme::FileWriter`]
    /// instead, off the logic thread.
    #[cfg(test)]
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        strand_theme::writer::write_atomic(path, self.to_text().as_bytes())
    }

    /// The kept values as the file holds them.
    pub fn to_text(&self) -> String {
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
                SystemSetting::ReducedMotion(on) => {
                    out.push_str(&format!("reduced_motion={on}\n"));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_services::Rgba;

    #[test]
    fn last_values_round_trip_through_the_service_state() {
        let dir = std::env::temp_dir().join(format!("strand-system-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = Last::file(Some(dir.clone())).unwrap();
        assert_eq!(Last::load(&path), Last::default());
        let state = System {
            dark: true,
            accent: Some(Rgba::rgb(1.0, 0.0, 0.0)),
            contrast: 1.0,
            reduced_motion: true,
            hostname: "box".into(),
        };
        let last = Last::of(&state);
        last.save(&path).unwrap();
        let back = Last::load(&path);
        assert_eq!(back, last);
        let mut seeded = System::default();
        back.seed(&mut seeded);
        assert_eq!(
            seeded,
            System {
                hostname: String::new(),
                ..state
            },
            "the host name is not kept"
        );
        // Light, no accent.
        let mut light = System::default();
        Last::load(&{
            Last::of(&light).save(&path).unwrap();
            path.clone()
        })
        .seed(&mut light);
        assert_eq!(light, System::default());
        // The older form still reads.
        std::fs::write(&path, "dark=true\n").unwrap();
        assert_eq!(
            Last::load(&path).settings,
            [SystemSetting::Dark {
                dark: true,
                scheme: ColorScheme::PreferDark
            }]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
