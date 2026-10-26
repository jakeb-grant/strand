//! The search index: every page's title, its `##` and `###` headings
//! with their anchors, and the text under each ([`Rendered::sections`]),
//! written as `search-index.js`, a script the page loads when the reader
//! first searches. A script, not JSON: `fetch` of a file fails on
//! `file://`, and the site works from disk.

use serde_json::{Value, json};

use crate::markdown::Rendered;

/// Where the index is written.
pub const OUT: &str = "search-index.js";

/// One page's entries: its title, then one per part.
pub fn entries(title: &str, out: &str, r: &Rendered) -> Vec<Value> {
    let mut v = Vec::new();
    for s in &r.sections {
        let text = crate::claims::collapse(&s.text);
        if s.heading.is_empty() && text.is_empty() {
            continue;
        }
        let url = if s.id.is_empty() {
            out.to_string()
        } else {
            format!("{out}#{}", s.id)
        };
        v.push(json!({ "p": title, "h": s.heading, "u": url, "x": text }));
    }
    if v.is_empty() {
        v.push(json!({ "p": title, "h": "", "u": out, "x": "" }));
    }
    v
}

/// The index file's text.
pub fn script(entries: &[Value]) -> Result<String, String> {
    let json = serde_json::to_string(entries).map_err(|e| e.to_string())?;
    // `</script>` could not end anything here (the file is a script of
    // its own), but a `<!--` or `</` in text stays inert either way.
    let json = json.replace("</", "<\\/");
    Ok(format!(
        "/* The site's search index, written by strand-site; do not edit. */\nwindow.STRAND_SEARCH = {json};\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::SearchSection;

    #[test]
    fn a_page_gives_an_entry_per_heading_with_its_anchor() {
        let r = Rendered {
            sections: vec![
                SearchSection {
                    heading: String::new(),
                    id: String::new(),
                    text: "Opening   words ".into(),
                },
                SearchSection {
                    heading: "Tokens".into(),
                    id: "tokens".into(),
                    text: "A token is\nread as $path".into(),
                },
            ],
            ..Rendered::default()
        };
        let e = entries("Level 2", "guide/level-2.html", &r);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["u"], "guide/level-2.html");
        assert_eq!(e[0]["x"], "Opening words");
        assert_eq!(e[1]["u"], "guide/level-2.html#tokens");
        assert_eq!(e[1]["h"], "Tokens");
        assert_eq!(e[1]["x"], "A token is read as $path");
        let s = script(&e).unwrap();
        assert!(s.starts_with("/* The site's search index"));
        assert!(s.contains("window.STRAND_SEARCH = [{"));
        let s = script(&[json!({"x": "a </script> b"})]).unwrap();
        assert!(s.contains("a <\\/script> b"), "{s}");
    }
}
