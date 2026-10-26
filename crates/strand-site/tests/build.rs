//! The whole site builds from the repository as it stands: every link and
//! anchor resolves, every config on it checks clean and is formatted,
//! every example's media matches its scenario, every text colour has its
//! contrast. A docs change that breaks any of them fails here.

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
}
