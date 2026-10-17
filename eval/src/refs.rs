//! Verdicts (docs/eval.md, "Verdicts and references"): the owner marks a
//! run good or bad, and `strand-eval verdict` records it as
//! `eval/refs/<id>.ref.json` with the run's digest and every node's
//! quantised path (no pixels). The `reference` check reads it back.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::checks::Reference;
use crate::checks::reference::paths;
use crate::report;

/// Today's date as `YYYY-MM-DD` (UTC), from the system clock.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil(secs / 86_400)
}

/// The civil date `days` after 1970-01-01 (Howard Hinnant's algorithm).
pub fn civil(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Where scenario `id`'s reference lives.
pub fn ref_path(id: &str) -> PathBuf {
    crate::scenario::eval_root()
        .join("refs")
        .join(format!("{id}.ref.json"))
}

/// Records `verdict` (`good` or `bad`) on the scenario run in `dir`
/// (`<run>/<id>`), into `refs` (eval/refs/ by default); returns the file
/// written.
pub fn record(
    dir: &Path,
    verdict: &str,
    note: &str,
    refs: Option<&Path>,
) -> Result<PathBuf, Error> {
    if !matches!(verdict, "good" | "bad") {
        return Err(Error::Usage(format!("verdict `{verdict}`: good or bad")));
    }
    let e = report::read_entry(dir)?;
    let r = Reference {
        verdict: verdict.into(),
        note: note.into(),
        date: today(),
        rev: format!(
            "{}{}",
            e.trace.build.rev,
            if e.trace.build.dirty { "+changes" } else { "" }
        ),
        digest: e.trace.digest.clone(),
        path: paths(&e.trace),
    };
    let out = match refs {
        Some(d) => d.join(format!("{}.ref.json", e.id)),
        None => ref_path(&e.id),
    };
    let json = serde_json::to_string_pretty(&r).map_err(|x| Error::Run(format!("ref: {x}")))?;
    report::write(&out, (json + "\n").as_bytes())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_become_dates() {
        assert_eq!(super::civil(0), "1970-01-01");
        assert_eq!(super::civil(20_743), "2026-10-17");
        assert_eq!(super::civil(11_016), "2000-02-29");
    }
}
