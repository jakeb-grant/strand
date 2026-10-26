//! The whole site builds from the repository as it stands: every link and
//! anchor resolves, every config on it checks clean and is formatted,
//! every example's media matches its scenario and its approved reference
//! recordings, every claim of the ledger is on its page with real
//! sources, every text colour has its contrast. A docs change that breaks
//! any of them fails here.

use std::path::Path;

#[test]
fn the_site_builds_with_no_problems() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("site");
    let _ = std::fs::remove_dir_all(&out);
    let report = match strand_site::build(&root, &out) {
        Ok(r) => r,
        Err(errors) => panic!("{} problem(s):\n{}", errors.len(), errors.join("\n")),
    };
    for page in [
        "index.html",
        "guide/index.html",
        "guide/level-1.html",
        "concepts/motion.html",
        "reference/elements/index.html",
        "reference/elements/slider.html",
        "reference/elements/node.html",
        "reference/services/index.html",
        "reference/services/audio.html",
        "reference/types.html",
        "reference/functions.html",
        "reference/tokens.html",
        "tools/cli.html",
        "search-index.js",
    ] {
        assert!(report.pages.iter().any(|p| p == page), "no {page}");
    }
    let home = std::fs::read_to_string(out.join("index.html")).unwrap();
    // The check on the home page is computed, not pasted.
    assert!(home.contains("did you mean `percent`"));
    assert!(home.contains("id=\"example-springs\""));
    assert!(out.join("media/springs-light.png").exists());
    assert!(out.join("assets/fonts/inter.woff2").exists());
    assert!(out.join("theme.css").exists());
    // A type in a signature links to its entry, an element's group to
    // its page.
    let spectrum = std::fs::read_to_string(out.join("reference/elements/spectrum.html")).unwrap();
    assert!(spectrum.contains("<a href=\"../types.html#audiodevice\">AudioDevice</a>"));
    assert!(spectrum.contains("<a href=\"node.html\">node</a>"));
    // The CLI page lists the binary's commands, from its own help.
    let cli = std::fs::read_to_string(out.join("tools/cli.html")).unwrap();
    for cmd in ["strand check", "strand compositor-rules", "strand replay"] {
        assert!(cli.contains(cmd), "no {cmd}");
    }
    // The search index has every page, its headings with their anchors.
    let index = std::fs::read_to_string(out.join("search-index.js")).unwrap();
    assert!(index.contains("\"u\":\"guide/level-2.html#"));
    assert!(index.contains("\"u\":\"reference/services/audio.html\""));
}
