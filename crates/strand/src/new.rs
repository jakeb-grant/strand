//! `strand new [dir]` (S-tools; design.md, "`strand new` scaffolds a
//! working shell"): the guide's level-1 hello bar
//! (`docs/guide/template/bar.strand`, S-docs') written as `bar.strand`
//! into `dir`, default `$XDG_CONFIG_HOME/strand`. A directory with
//! anything in it is refused, so nothing of the user's is overwritten.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The starter config, byte for byte the guide's level-1 block.
pub(crate) const TEMPLATE: &str = include_str!("../../../docs/guide/template/bar.strand");

/// The file it writes.
pub(crate) const FILE: &str = "bar.strand";

/// Why `strand new` wrote nothing.
#[derive(Debug)]
pub(crate) enum NewError {
    /// No directory was given and neither `XDG_CONFIG_HOME` nor `HOME`
    /// names one.
    NoDir,
    /// The path is there and is not a directory.
    NotADir(PathBuf),
    /// The directory has something in it.
    NotEmpty(PathBuf),
    /// Creating the directory or writing the file failed.
    Io(PathBuf, std::io::Error),
}

impl fmt::Display for NewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NewError::NoDir => {
                f.write_str("no config directory (give one, or set XDG_CONFIG_HOME or HOME)")
            }
            NewError::NotADir(p) => write!(f, "{} is not a directory", p.display()),
            NewError::NotEmpty(p) => write!(
                f,
                "{} is not empty: `strand new` only writes into an empty or new directory",
                p.display()
            ),
            NewError::Io(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

/// Writes the starter config into `dir` (made if missing); the file
/// written.
pub(crate) fn scaffold(dir: &Path) -> Result<PathBuf, NewError> {
    let io = |p: &Path| {
        let p = p.to_path_buf();
        move |e| NewError::Io(p, e)
    };
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(NewError::NotEmpty(dir.into()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        Err(_) if dir.exists() && !dir.is_dir() => return Err(NewError::NotADir(dir.into())),
        Err(e) => return Err(NewError::Io(dir.into(), e)),
    }
    let file = dir.join(FILE);
    place(&file, |f| f.write_all(TEMPLATE.as_bytes()))?;
    Ok(file)
}

/// Creates `file` (`create_new`: a file that appeared since the check is
/// not replaced) and writes it; a failed write (a full disk) removes the
/// partial file, so the directory is empty again for a retry.
fn place(
    file: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), NewError> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .map_err(|e| NewError::Io(file.into(), e))?;
    write(&mut f).map_err(|e| {
        drop(f);
        let _ = std::fs::remove_file(file);
        NewError::Io(file.into(), e)
    })
}

/// `strand new`'s usage.
pub(crate) const USAGE: &str = "usage: strand new [dir]";

/// `strand new [dir]`.
pub fn new_cli(args: &[String]) -> Result<String, String> {
    let dir = match args {
        [] => crate::check::default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .ok_or(NewError::NoDir),
        [d] if !d.starts_with('-') => Ok(PathBuf::from(d)),
        _ => return Err(USAGE.into()),
    };
    let file = dir.and_then(|d| scaffold(&d)).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {}\nstart it with `strand run{}`\n",
        file.display(),
        match args {
            [d] => format!(" {d}"),
            _ => String::new(),
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("strand-new-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_new_or_empty_dir_gets_the_template() {
        for made in [false, true] {
            let d = temp(&format!("ok-{made}")).join("strand");
            if made {
                std::fs::create_dir_all(&d).unwrap();
            }
            let f = scaffold(&d).unwrap();
            assert_eq!(f, d.join("bar.strand"));
            assert_eq!(std::fs::read_to_string(&f).unwrap(), TEMPLATE);
            let _ = std::fs::remove_dir_all(d.parent().unwrap());
        }
    }

    /// A write that fails part-way (ENOSPC) leaves no partial file: the
    /// directory is empty again and `strand new` succeeds on a retry.
    #[test]
    fn a_failed_write_leaves_the_dir_empty_for_a_retry() {
        let d = temp("enospc");
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join(FILE);
        let got = place(&file, |f| {
            f.write_all(&TEMPLATE.as_bytes()[..10])?;
            Err(std::io::Error::from_raw_os_error(28))
        });
        assert!(matches!(got, Err(NewError::Io(..))));
        assert!(!file.exists(), "a partial bar.strand left behind");
        assert_eq!(scaffold(&d).unwrap(), file);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), TEMPLATE);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_non_empty_dir_or_a_file_is_refused() {
        let d = temp("full");
        std::fs::create_dir_all(&d).unwrap();
        // Even a hidden file counts: nothing is written beside it.
        std::fs::write(d.join(".keep"), "").unwrap();
        assert!(matches!(scaffold(&d), Err(NewError::NotEmpty(_))));
        assert!(!d.join("bar.strand").exists());
        let file = d.join(".keep");
        assert!(matches!(scaffold(&file), Err(NewError::NotADir(_))));
        let _ = std::fs::remove_dir_all(d);
    }
}
