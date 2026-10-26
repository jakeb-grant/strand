//! `strand-web-icons EXAMPLES OUT`: copies the icon theme files the docs
//! examples draw (natively, from the icon base directories on this
//! machine: in CI's image, Adwaita and hicolor) into `OUT`, under their
//! paths relative to their base directory, and lists them in
//! `OUT/icons.json`; `EXAMPLES` is the JSON list `strand-site live`
//! prints. A page supplies these to the web host, which has no disk
//! (scripts/wasm/build.sh; docs/live-examples.md).

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use strand_web::live::{Example, icons_drawn};

fn run(examples: &Path, out: &Path) -> Result<usize, String> {
    let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
    let text = std::fs::read_to_string(examples).map_err(|e| at(examples, &e))?;
    let list: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| at(examples, &e))?;
    let examples = list
        .iter()
        .map(|v| Example::from_json(&v.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let font = |p: std::path::PathBuf| std::fs::read(&p).map(Arc::new).map_err(|e| at(&p, &e));
    let fonts = vec![
        font(strand_text::test_font_path())?,
        font(strand_text::test_bold_font_path())?,
    ];
    let files = icons_drawn(&examples, &fonts)?;
    for (rel, abs) in &files {
        let dest = out.join(rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|e| at(dir, &e))?;
        }
        std::fs::copy(abs, &dest).map_err(|e| at(abs, &e))?;
    }
    let list: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
    let json = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())? + "\n";
    let index = out.join("icons.json");
    std::fs::write(&index, json).map_err(|e| at(&index, &e))?;
    Ok(files.len())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [examples, out] = args.as_slice() else {
        eprintln!("usage: strand-web-icons EXAMPLES.json OUT_DIR");
        return ExitCode::from(2);
    };
    match run(Path::new(examples), Path::new(out)) {
        Ok(n) => {
            println!("strand-web-icons: {n} files in {out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
