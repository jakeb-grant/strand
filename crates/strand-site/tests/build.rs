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
    // The menu on a narrow screen opens the sidebar here too, its links
    // relative to the root.
    assert!(home.contains("<nav class=\"sidebar\" id=\"sidebar\""));
    assert!(home.contains("<a href=\"guide/index.html\">"));
    assert!(out.join("media/springs-light.png").exists());
    assert!(out.join("assets/fonts/inter.woff2").exists());
    assert!(out.join("theme.css").exists());
    // For language models: llms.txt lists the pages' markdown, which
    // exists, links to the other pages' markdown, shows an example as
    // its config and leaves out what is repo-only.
    let llms = std::fs::read_to_string(out.join("llms.txt")).unwrap();
    assert!(llms.starts_with("# Strand\n\n> "), "{llms}");
    for line in llms.lines().filter(|l| l.starts_with("- [")) {
        let md = line.split_once("](").unwrap().1.split_once(')').unwrap().0;
        assert!(md.starts_with('/'), "links are from the root: {md}");
        assert!(
            out.join(&md[1..]).is_file(),
            "llms.txt lists {md}, which is not written"
        );
    }
    let osd = std::fs::read_to_string(out.join("how-to/osd.html.md")).unwrap();
    assert!(osd.contains("```strand\n// osd.strand"), "{osd}");
    assert!(!osd.contains("```example"), "{osd}");
    let level1 = std::fs::read_to_string(out.join("guide/level-1.html.md")).unwrap();
    assert!(
        level1.contains(".html.md"),
        "links go to markdown: {level1}"
    );
    let perf = std::fs::read_to_string(out.join("concepts/performance.html.md")).unwrap();
    assert!(
        !perf.contains("repo-only") && !perf.contains("What CI holds"),
        "{perf}"
    );
    let full = std::fs::read_to_string(out.join("llms-full.txt")).unwrap();
    assert!(full.contains("<!-- page: reference/elements/slider.html -->"));
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
    // (live examples) Only the playground runs an example live: a
    // figure is its frames, with nothing for a script to run. The
    // playground carries every example named, and the site needs no
    // wasm build: its manifest names none until scripts/wasm/site.sh
    // adds one.
    for page in ["index.html", "concepts/motion.html", "how-to/osd.html"] {
        let html = std::fs::read_to_string(out.join(page)).unwrap();
        assert!(html.contains("class=\"example"), "{page} has no example");
        assert!(!html.contains("ex-live-data"), "{page} offers live mode");
    }
    assert!(home.contains("<script src=\"wasm/manifest.js\" defer></script>"));
    assert_eq!(
        std::fs::read_to_string(out.join("wasm/manifest.js")).unwrap(),
        strand_site::example::manifest_none()
    );
    let playground = std::fs::read_to_string(out.join("playground.html")).unwrap();
    let data = playground
        .split("class=\"pg-data\">")
        .nth(1)
        .and_then(|r| r.split("</script>").next())
        .unwrap();
    let names: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(data)
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["hello", "springs", "osd", "toasts", "swap"]);
    // The search index has every page, its headings with their anchors.
    let index = std::fs::read_to_string(out.join("search-index.js")).unwrap();
    assert!(index.contains("\"u\":\"guide/level-2.html#"));
    assert!(index.contains("\"u\":\"reference/services/audio.html\""));
}
