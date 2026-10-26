//! `strand-site build [--out DIR]` builds the site (into `target/site`);
//! `strand-site media <name> <light-run> [<dark-run>]` renders an
//! example's media from eval runs of its scenario (scripts/site/media.sh).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: strand-site build [--out DIR]\n       strand-site media <name> <light-run-dir> [<dark-run-dir>]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo();
    match args.first().map(String::as_str) {
        Some("build") => {
            let out = match args.get(1).map(String::as_str) {
                Some("--out") => match args.get(2) {
                    Some(o) => PathBuf::from(o),
                    None => return usage(),
                },
                Some(_) => return usage(),
                None => root.join("target/site"),
            };
            match strand_site::build(&root, &out) {
                Ok(r) => {
                    println!("strand-site: {} files in {}", r.pages.len(), out.display());
                    ExitCode::SUCCESS
                }
                Err(errors) => {
                    for e in &errors {
                        eprintln!("error: {e}");
                    }
                    eprintln!("strand-site: {} problem(s); nothing written", errors.len());
                    ExitCode::FAILURE
                }
            }
        }
        Some("media") => {
            let (Some(name), Some(light)) = (args.get(1), args.get(2)) else {
                return usage();
            };
            let dark = args.get(3).map(PathBuf::from);
            match strand_site::media::render(&root, name, Path::new(light), dark.as_deref()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => usage(),
    }
}
