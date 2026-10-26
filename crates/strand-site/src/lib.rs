//! `strand-site`: the documentation site, built from markdown (docs/
//! architecture.md, "strand-site"; docs/decisions.md, docs-site). Pages
//! are listed in `site/SUMMARY.md`; the guide is read where it lives
//! (`docs/guide/`), the reference is generated from the schema and the
//! CLI page from the binary's help, every Strand example is checked, and
//! every claim of the ledger is held to its page and its sources, so the
//! site cannot say what the code does not do. The output is a folder of static files that works
//! from disk (`target/site/index.html`) and from a server alike.

pub mod check;
pub mod claims;
pub mod cli;
pub mod example;
pub mod highlight;
pub mod html;
pub mod markdown;
pub mod media;
pub mod nav;
pub mod page;
pub mod reference;
pub mod search;
pub mod theme;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use highlight::Highlighter;
use markdown::{Ctx, Rendered};
use nav::{Target, output_of};
use page::NavSection;

/// The repository, for links to files the site does not publish.
pub const GITHUB: &str = "https://github.com/jakeb-grant/strand";
pub const GITHUB_BLOB: &str = "https://github.com/jakeb-grant/strand/blob/main/";

/// The home page's source: its own layout, not in the sidebar.
const HOME: &str = "site/index.md";

/// What a build wrote.
#[derive(Debug, Default)]
pub struct Report {
    /// Output pages, by path in the site.
    pub pages: Vec<String>,
    /// Bytes of each output page's HTML.
    pub sizes: HashMap<String, usize>,
}

/// What a build checks beyond the repository.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// The `strand` binary the CLI page is held to ([`cli::check`]);
    /// `None` leaves that check out.
    pub strand: Option<PathBuf>,
}

/// One page to render.
struct Source {
    out: String,
    /// From the repository's root; `None` for a generated page.
    source: Option<String>,
    markdown: String,
    /// Where its relative links resolve from.
    resolve_from: String,
    label: String,
}

impl Source {
    fn generated(p: &reference::RefPage, label: &str) -> Source {
        Source {
            out: p.out.clone(),
            source: None,
            markdown: p.markdown.clone(),
            resolve_from: p.source.to_string(),
            label: label.to_string(),
        }
    }
}

fn read(root: &Path, rel: &str) -> Result<String, String> {
    std::fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel}: {e}"))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for e in entries {
        let e = e.map_err(|e| e.to_string())?;
        let p = e.path();
        let dest = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(&p, &dest)?;
        } else {
            std::fs::copy(&p, &dest).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    Ok(())
}

/// Builds the site from the repository at `root` into `out`, with no
/// `strand` binary to hold the CLI page to.
pub fn build(root: &Path, out: &Path) -> Result<Report, Vec<String>> {
    build_with(root, out, &Options::default())
}

/// Builds the site from the repository at `root` into `out`. Every
/// problem found is returned together, and nothing is written unless
/// there are none.
pub fn build_with(root: &Path, out: &Path, opts: &Options) -> Result<Report, Vec<String>> {
    let mut errors = Vec::new();
    let hl = Highlighter::new().map_err(|e| vec![e])?;
    let summary = read(root, "site/SUMMARY.md").map_err(|e| vec![e])?;
    let sections = nav::parse(&summary).map_err(|e| vec![e])?;
    let schema = strand_dev::reference::schema().map_err(|e| vec![e])?;
    let links = reference::Links::new(&schema);

    let mut sources: Vec<Source> = Vec::new();
    // Pages a set leads to, beyond the one the sidebar links.
    let mut more: Vec<Source> = Vec::new();
    // The generator's pages the site publishes, for links to them.
    let mut generated_outputs: Vec<(String, String)> = Vec::new();
    let mut nav: Vec<NavSection> = Vec::new();
    for s in &sections {
        let mut items = Vec::new();
        for e in &s.entries {
            match &e.target {
                Target::Page(src) => {
                    let Some(out_path) = output_of(src) else {
                        errors.push(format!(
                            "site/SUMMARY.md: `{src}` is not a page the site publishes"
                        ));
                        continue;
                    };
                    match read(root, src) {
                        Ok(md) => sources.push(Source {
                            out: out_path.clone(),
                            source: Some(src.clone()),
                            markdown: md,
                            resolve_from: src.clone(),
                            label: e.label.clone(),
                        }),
                        Err(err) => errors.push(err),
                    }
                    items.push((e.label.clone(), out_path));
                }
                Target::Generated(set) if set == "tools/cli" => {
                    match read(root, cli::CAPTURE)
                        .and_then(|t| cli::parse(&t))
                        .and_then(|h| cli::page(&h))
                    {
                        Ok(md) => sources.push(Source {
                            out: cli::OUT.into(),
                            source: None,
                            markdown: md,
                            resolve_from: cli::CAPTURE.into(),
                            label: e.label.clone(),
                        }),
                        Err(err) => errors.push(err),
                    }
                    items.push((e.label.clone(), cli::OUT.into()));
                }
                Target::Generated(set) => match reference::set(&schema, set) {
                    Some(g) => {
                        generated_outputs.push((g.index.source.to_string(), g.index.out.clone()));
                        sources.push(Source::generated(&g.index, &e.label));
                        items.push((e.label.clone(), g.index.out.clone()));
                        for p in &g.pages {
                            more.push(Source::generated(p, &p.name));
                        }
                    }
                    None => errors.push(format!("site/SUMMARY.md: no generated set `@{set}`")),
                },
            }
        }
        nav.push(NavSection {
            title: s.title.clone(),
            items,
        });
    }
    sources.extend(more);
    match read(root, HOME) {
        Ok(md) => sources.push(Source {
            out: "index.html".into(),
            source: Some(HOME.into()),
            markdown: md,
            resolve_from: HOME.into(),
            label: "Home".into(),
        }),
        Err(e) => errors.push(e),
    }

    let mut outputs: HashMap<String, String> = generated_outputs.into_iter().collect();
    for s in &sources {
        if let Some(src) = &s.source {
            outputs.insert(src.clone(), s.out.clone());
        }
    }
    let mut seen = HashSet::new();
    for s in &sources {
        if !seen.insert(s.out.clone()) {
            errors.push(format!("two pages are written to {}", s.out));
        }
    }

    // An anchor of a page the site splits goes to its own page, but only
    // where that page is published.
    let mut anchor_pages = reference::anchor_pages(&schema);
    let published: HashSet<&str> = sources.iter().map(|s| s.out.as_str()).collect();
    anchor_pages.retain(|_, page| published.contains(page.as_str()));
    let mut rendered: Vec<(usize, Rendered)> = Vec::new();
    for (i, s) in sources.iter().enumerate() {
        let is_reference = s.out.starts_with("reference/");
        let ctx = Ctx {
            root,
            source: &s.resolve_from,
            out: &s.out,
            outputs: &outputs,
            hl: &hl,
            anchor_pages: &anchor_pages,
            links: is_reference.then_some(&links),
        };
        let r = markdown::render(&s.markdown, &ctx, &mut errors);
        if !r.config.is_empty() {
            match check::config(&s.out.replace('/', "-"), &r.config) {
                Ok(c) => {
                    if c.errors + c.warnings > 0 {
                        errors.push(format!(
                            "{}: its config does not check clean:\n{}",
                            s.resolve_from, c.text
                        ));
                    }
                    for f in c.unformatted {
                        errors.push(format!(
                            "{}: `strand fmt` would change its `{f}`",
                            s.resolve_from
                        ));
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", s.resolve_from)),
            }
        }
        rendered.push((i, r));
    }

    // Every link to a page of the site reaches a page and an anchor on it.
    let anchors: HashMap<&str, &HashSet<String>> = rendered
        .iter()
        .map(|(i, r)| (sources[*i].out.as_str(), &r.anchors))
        .collect();
    for (_, r) in &rendered {
        for l in &r.links {
            match anchors.get(l.page.as_str()) {
                None => errors.push(format!(
                    "{}: link to {}, which the site has no page for",
                    l.from, l.page
                )),
                Some(a) if !l.fragment.is_empty() && !a.contains(&l.fragment) => {
                    errors.push(format!(
                        "{}: link to {}#{}, which has no such heading",
                        l.from, l.page, l.fragment
                    ))
                }
                _ => {}
            }
        }
    }
    // So does every `href` the pages end up with, raw HTML's included: to
    // a page and its anchor, or to a file the site copies.
    for (i, r) in &rendered {
        let from = sources[*i].out.as_str();
        for href in hrefs(&r.html) {
            if href.contains("://") || href.starts_with("mailto:") || href.starts_with('#') {
                continue;
            }
            let (path, fragment) = href.split_once('#').unwrap_or((href, ""));
            let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
            let target = match nav::normalise(&format!("{dir}/{path}")) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("{}: href `{href}`: {e}", sources[*i].resolve_from));
                    continue;
                }
            };
            let ok = match anchors.get(target.as_str()) {
                Some(a) => fragment.is_empty() || a.contains(fragment),
                None => fragment.is_empty() && root.join("site").join(&target).is_file(),
            };
            if !ok {
                errors.push(format!(
                    "{}: href `{href}` reaches nothing in the site",
                    sources[*i].resolve_from
                ));
            }
        }
    }
    errors.extend(theme::contrast_misses());
    errors.extend(claims::check(root));
    if let Some(bin) = &opts.strand
        && let Err(e) = cli::check(root, bin)
    {
        errors.push(e);
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Prev and next follow the sidebar.
    let order: Vec<(String, String)> = nav.iter().flat_map(|s| s.items.iter().cloned()).collect();
    let mut report = Report::default();
    let write = |rel: &str, text: &str, report: &mut Report| -> Result<(), String> {
        let path = out.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        report.pages.push(rel.to_string());
        report.sizes.insert(rel.to_string(), text.len());
        Ok(())
    };
    let mut fail = Vec::new();
    let mut index = Vec::new();
    for (i, r) in &rendered {
        let s = &sources[*i];
        let title = r.title.clone().unwrap_or_else(|| s.label.clone());
        index.extend(search::entries(
            if s.out == "index.html" {
                "Strand"
            } else {
                &title
            },
            &s.out,
            r,
        ));
        let html = if s.out == "index.html" {
            page::home(&r.html, &s.out)
        } else {
            let at = order.iter().position(|(_, o)| *o == s.out);
            let toc = markdown::toc(r);
            page::doc(&page::Page {
                title: &title,
                out: &s.out,
                body: &r.html,
                toc: &toc,
                nav: &nav,
                prev: at.and_then(|a| a.checked_sub(1)).and_then(|a| order.get(a)),
                next: at.and_then(|a| order.get(a + 1)),
                source: s.source.as_deref(),
            })
        };
        if let Err(e) = write(&s.out, &html, &mut report) {
            fail.push(e);
        }
    }
    if let Err(e) = write("theme.css", &theme::css(), &mut report) {
        fail.push(e);
    }
    match search::script(&index) {
        Ok(js) => {
            if let Err(e) = write(search::OUT, &js, &mut report) {
                fail.push(e);
            }
        }
        Err(e) => fail.push(e),
    }
    for (from, to) in [("site/assets", "assets"), (example::MEDIA, "media")] {
        if root.join(from).is_dir()
            && let Err(e) = copy_dir(&root.join(from), &out.join(to))
        {
            fail.push(e);
        }
    }
    if fail.is_empty() {
        Ok(report)
    } else {
        Err(fail)
    }
}

/// The `href`s in a page's HTML.
fn hrefs(html: &str) -> impl Iterator<Item = &str> {
    html.split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split_once('"').map(|(h, _)| h))
}
