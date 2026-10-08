//! Reading `/proc` and `/sys` for the `cpu` and `memory` services.

use std::path::{Path, PathBuf};

/// One line of `/proc/stat`: busy and total jiffies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Times {
    pub idle: u64,
    pub total: u64,
}

/// The aggregate `cpu` line, then each `cpuN`, of `/proc/stat`.
pub(crate) fn parse_stat(text: &str) -> (Option<Times>, Vec<Times>) {
    let mut all = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_ascii_whitespace();
        let Some(name) = parts.next() else { continue };
        if !name.starts_with("cpu") {
            continue;
        }
        // user nice system idle iowait irq softirq steal (guest and
        // guest_nice are already counted in user and nice).
        let n: Vec<u64> = parts.take(8).filter_map(|p| p.parse().ok()).collect();
        if n.len() < 4 {
            continue;
        }
        let idle = n[3] + n.get(4).copied().unwrap_or(0);
        let t = Times {
            idle,
            total: n.iter().sum(),
        };
        if name == "cpu" {
            all = Some(t);
        } else {
            cores.push(t);
        }
    }
    (all, cores)
}

/// The busy fraction between two readings (0 when nothing elapsed).
pub(crate) fn usage(prev: Times, cur: Times) -> f64 {
    let total = cur.total.saturating_sub(prev.total);
    if total == 0 {
        return 0.0;
    }
    let idle = cur.idle.saturating_sub(prev.idle).min(total);
    (total - idle) as f64 / total as f64
}

/// `MemTotal` and `MemAvailable` of `/proc/meminfo`, in bytes.
pub(crate) fn parse_meminfo(text: &str) -> Option<(f64, f64)> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kib = || -> Option<f64> {
            let n: f64 = rest.split_ascii_whitespace().next()?.parse().ok()?;
            Some(n * 1024.0)
        };
        match key {
            "MemTotal" => total = kib(),
            "MemAvailable" => available = kib(),
            _ => {}
        }
    }
    Some((total?, available?))
}

/// The hwmon chips that report a processor temperature as `temp1`.
const CPU_HWMON: [&str; 5] = [
    "coretemp",
    "k10temp",
    "zenpower",
    "cpu_thermal",
    "cpu-thermal",
];
/// Thermal zone types of a processor sensor.
const CPU_ZONES: [&str; 4] = ["x86_pkg_temp", "cpu-thermal", "cpu_thermal", "soc_thermal"];

/// The file holding the processor's temperature in millidegrees
/// Celsius, if a known sensor exists under `sys` (`/sys`).
pub(crate) fn find_temperature(sys: &Path) -> Option<PathBuf> {
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut chips: Vec<PathBuf> = std::fs::read_dir(sys.join("class/hwmon"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    chips.sort();
    for chip in chips {
        if read(chip.join("name")).is_some_and(|n| CPU_HWMON.contains(&n.as_str())) {
            let input = chip.join("temp1_input");
            if input.exists() {
                return Some(input);
            }
        }
    }
    let mut zones: Vec<PathBuf> = std::fs::read_dir(sys.join("class/thermal"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone"))
        })
        .collect();
    zones.sort();
    zones
        .into_iter()
        .find(|z| read(z.join("type")).is_some_and(|t| CPU_ZONES.contains(&t.as_str())))
        .map(|z| z.join("temp"))
}

/// Degrees Celsius from a millidegree sensor file.
pub(crate) fn read_temperature(path: &Path) -> Option<f64> {
    let text = std::fs::read_to_string(path).ok()?;
    let milli: f64 = text.trim().parse().ok()?;
    Some(milli / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 0 100 700 100 0 0 0 0 0\n\
                        cpu0 50 0 50 350 50 0 0 0 0 0\n\
                        cpu1 50 0 50 350 50 0 0 0 0 0\n\
                        intr 1 2 3\nctxt 42\n";

    #[test]
    fn stat_lines_are_parsed_and_compared() {
        let (all, cores) = parse_stat(STAT);
        let all = all.unwrap();
        assert_eq!(
            all,
            Times {
                idle: 800,
                total: 1000
            }
        );
        assert_eq!(cores.len(), 2);
        let later = Times {
            idle: 850,
            total: 1100,
        };
        assert!((usage(all, later) - 0.5).abs() < 1e-9);
        assert_eq!(usage(all, all), 0.0);
        // A counter going backwards (a CPU hot-unplugged) is not negative.
        assert_eq!(usage(later, all), 0.0);
    }

    #[test]
    fn meminfo_is_bytes() {
        let text = "MemTotal:       16000000 kB\nMemFree: 1 kB\nMemAvailable:    4000000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some((16_000_000.0 * 1024.0, 4_000_000.0 * 1024.0))
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn a_hwmon_chip_or_a_thermal_zone_is_found() {
        let dir = std::env::temp_dir().join(format!("strand-sys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let zone = dir.join("class/thermal/thermal_zone3");
        std::fs::create_dir_all(&zone).unwrap();
        std::fs::write(zone.join("type"), "x86_pkg_temp\n").unwrap();
        std::fs::write(zone.join("temp"), "51000\n").unwrap();
        let found = find_temperature(&dir).unwrap();
        assert_eq!(read_temperature(&found), Some(51.0));
        let chip = dir.join("class/hwmon/hwmon1");
        std::fs::create_dir_all(&chip).unwrap();
        std::fs::write(chip.join("name"), "k10temp\n").unwrap();
        std::fs::write(chip.join("temp1_input"), "47500\n").unwrap();
        let found = find_temperature(&dir).unwrap();
        assert_eq!(read_temperature(&found), Some(47.5));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(find_temperature(&dir), None);
    }
}
