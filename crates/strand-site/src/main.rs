//! `strand-site build [--out DIR] [--strand BIN]` builds the site (into
//! `target/site`), holding its CLI page to `BIN` when given;
//! `strand-site cli BIN` captures `BIN`'s help for that page
//! (scripts/site/cli.sh); `strand-site media <name> <light-run>
//! [<dark-run>]` renders an example's media from eval runs of its
//! scenario (scripts/site/media.sh); `strand-site live` prints every
//! docs example as the playground runs it (scripts/wasm/build.sh).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: strand-site build [--out DIR] [--strand BIN]\n       strand-site cli BIN\n       \
         strand-site twin <name>\n       strand-site live\n       strand-site media [--check] <name> <light-run-dir> <dark-run-dir>"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo();
    match args.first().map(String::as_str) {
        Some("build") => {
            let mut out = root.join("target/site");
            let mut opts = strand_site::Options::default();
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                match (a.as_str(), rest.next()) {
                    ("--out", Some(o)) => out = PathBuf::from(o),
                    ("--strand", Some(b)) => opts.strand = Some(PathBuf::from(b)),
                    _ => return usage(),
                }
            }
            if opts.strand.is_none() {
                eprintln!(
                    "strand-site: the CLI page is not held to a binary (--strand BIN holds it)"
                );
            }
            match strand_site::build_with(&root, &out, &opts) {
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
        Some("cli") => {
            let Some(bin) = args.get(1) else {
                return usage();
            };
            let path = root.join(strand_site::cli::CAPTURE);
            match strand_site::cli::capture(Path::new(bin)) {
                Ok(h) => match std::fs::write(&path, strand_site::cli::write(&h)) {
                    Ok(()) => {
                        println!("strand-site: {}", strand_site::cli::CAPTURE);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {}: {e}", path.display());
                        ExitCode::FAILURE
                    }
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("media") => {
            let check = args.get(1).is_some_and(|a| a == "--check");
            let rest = &args[if check { 2 } else { 1 }..];
            let [name, light, dark] = rest else {
                return usage();
            };
            let (light, dark) = (Path::new(light), Path::new(dark));
            let done = if check {
                strand_site::media::compare(&root, name, light, dark)
            } else {
                strand_site::media::render(&root, name, light, dark)
            };
            match done {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("live") => {
            // Every docs example as the playground runs it, for
            // the tools that build the live examples (scripts/wasm/build.sh).
            match strand_site::example::all_live(&root) {
                Ok(v) => {
                    println!("{v}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("twin") => {
            let Some(name) = args.get(1) else {
                return usage();
            };
            let ex = strand_site::example::DIR;
            let twins = strand_site::example::TWINS;
            let made = std::fs::read_to_string(root.join(format!("{ex}/{name}.scn")))
                .map_err(|e| format!("{ex}/{name}.scn: {e}"))
                .and_then(|t| strand_site::example::dark_twin(name, &t))
                .and_then(|t| {
                    std::fs::create_dir_all(root.join(twins))
                        .map_err(|e| format!("{twins}: {e}"))?;
                    std::fs::write(root.join(format!("{twins}/{name}.scn")), t)
                        .map_err(|e| format!("{twins}/{name}.scn: {e}"))
                });
            match made {
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
