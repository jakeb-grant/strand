//! The pages' HTML around their content: the top bar, the sidebar, the
//! page's contents and the pager. Plain HTML that reads with scripts off;
//! `site.js` adds the theme switch, the search, the copy buttons, the
//! menu on narrow screens and the example players.

use std::fmt::Write as _;

use crate::html::{esc, relative, root};
use crate::{GITHUB, GITHUB_BLOB};

/// A sidebar section, its entries as (label, output page).
#[derive(Debug, Clone)]
pub struct NavSection {
    pub title: String,
    pub items: Vec<(String, String)>,
}

/// What a page is wrapped with.
#[derive(Debug)]
pub struct Page<'a> {
    pub title: &'a str,
    pub out: &'a str,
    pub body: &'a str,
    pub toc: &'a str,
    pub nav: &'a [NavSection],
    pub prev: Option<&'a (String, String)>,
    pub next: Option<&'a (String, String)>,
    /// The source on GitHub, for "Edit this page".
    pub source: Option<&'a str>,
}

/// The theme is chosen before the first paint, so a dark reader never
/// sees a light flash. A stored choice that cannot be read is no choice.
const THEME_BOOT: &str = "<script>try{var t=localStorage.getItem('strand-theme');\
if(t==='light'||t==='dark')document.documentElement.dataset.theme=t}catch(e){}</script>";

const MARK: &str = "<svg class=\"mark\" viewBox=\"0 0 32 32\" aria-hidden=\"true\">\
<path d=\"M4 22c6 0 6-12 12-12s6 12 12 12\"/><path d=\"M4 10c6 0 6 12 12 12s6-12 12-12\"/></svg>";

fn head(o: &mut String, title: &str, out: &str, description: &str) {
    let r = root(out);
    let _ = write!(
        o,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1, viewport-fit=cover\">\n\
         <title>{}</title>\n<meta name=\"description\" content=\"{}\">\n{THEME_BOOT}\n\
         <link rel=\"stylesheet\" href=\"{r}theme.css\">\n<link rel=\"stylesheet\" href=\"{r}assets/site.css\">\n\
         <link rel=\"icon\" href=\"{r}assets/mark.svg\" type=\"image/svg+xml\">\n\
         <script src=\"{r}{manifest}\" defer></script>\n\
         <script src=\"{r}assets/site.js\" defer></script>\n</head>\n",
        esc(title),
        esc(description),
        manifest = crate::example::MANIFEST,
    );
}

fn topbar(o: &mut String, out: &str, current: &str) {
    let r = root(out);
    let link = |o: &mut String, href: &str, label: &str, key: &str| {
        let cur = if key == current {
            " aria-current=\"page\""
        } else {
            ""
        };
        let _ = write!(o, "<a href=\"{href}\"{cur}>{label}</a>");
    };
    let _ = write!(
        o,
        "<header class=\"top\"><div class=\"top-in\">\
         <button type=\"button\" class=\"menu\" aria-label=\"Menu\" aria-expanded=\"false\" aria-controls=\"sidebar\">\
         <svg viewBox=\"0 0 20 20\" aria-hidden=\"true\"><path d=\"M3 6h14M3 10h14M3 14h14\"/></svg></button>\
         <a class=\"brand\" href=\"{r}index.html\">{MARK}<span>strand</span></a><nav class=\"top-nav\" aria-label=\"Site\">"
    );
    link(o, &format!("{r}guide/index.html"), "Guide", "guide");
    link(
        o,
        &format!("{r}concepts/motion.html"),
        "Concepts",
        "concepts",
    );
    link(
        o,
        &format!("{r}reference/elements/index.html"),
        "Reference",
        "reference",
    );
    // The search field works only with the script, which shows it.
    let _ = writeln!(
        o,
        "</nav><div class=\"top-end\"><div class=\"search\" role=\"search\">\
         <svg class=\"search-icon\" viewBox=\"0 0 20 20\" aria-hidden=\"true\"><circle cx=\"8.5\" cy=\"8.5\" r=\"5.5\"/>\
         <path d=\"m13 13 4 4\"/></svg><input type=\"search\" class=\"search-input\" id=\"search\" \
         placeholder=\"Search\" aria-label=\"Search the documentation\" autocomplete=\"off\" spellcheck=\"false\" \
         role=\"combobox\" aria-expanded=\"false\" aria-controls=\"search-results\" aria-autocomplete=\"list\">\
         <kbd class=\"search-key\" aria-hidden=\"true\">/</kbd>\
         <div class=\"search-results\" id=\"search-results\" role=\"listbox\" aria-label=\"Search results\" hidden></div></div>\
         <a class=\"gh\" href=\"{GITHUB}\">GitHub</a>\
         <button type=\"button\" class=\"theme\" aria-label=\"Theme: system\" title=\"Theme\">\
         <svg viewBox=\"0 0 20 20\" aria-hidden=\"true\"><circle cx=\"10\" cy=\"10\" r=\"6.5\"/>\
         <path class=\"theme-half\" d=\"M10 3.5a6.5 6.5 0 0 1 0 13z\"/></svg></button></div></div></header>"
    );
}

fn section_of(out: &str) -> &str {
    out.split('/').next().unwrap_or("")
}

/// A documentation page.
pub fn doc(p: &Page) -> String {
    let mut o = String::new();
    head(
        &mut o,
        &format!("{} · Strand", p.title),
        p.out,
        &format!("{} — Strand, a toolkit for Wayland shells.", p.title),
    );
    o.push_str("<body class=\"doc\">\n");
    topbar(&mut o, p.out, section_of(p.out));
    o.push_str(
        "<div class=\"layout\"><nav class=\"sidebar\" id=\"sidebar\" aria-label=\"Documentation\">",
    );
    for s in p.nav {
        let _ = write!(o, "<p class=\"side-title\">{}</p><ul>", esc(&s.title));
        for (label, out) in &s.items {
            let cur = if out == p.out {
                " aria-current=\"page\""
            } else {
                ""
            };
            let _ = write!(
                o,
                "<li><a href=\"{}\"{cur}>{}</a></li>",
                relative(p.out, out),
                esc(label)
            );
        }
        o.push_str("</ul>");
    }
    o.push_str("</nav><main class=\"main\" id=\"main\"><article class=\"prose\">");
    o.push_str(p.body);
    o.push_str("</article><footer class=\"pager\">");
    if let Some((label, out)) = p.prev {
        let _ = write!(
            o,
            "<a class=\"prev\" href=\"{}\"><span>Previous</span>{}</a>",
            relative(p.out, out),
            esc(label)
        );
    }
    if let Some((label, out)) = p.next {
        let _ = write!(
            o,
            "<a class=\"next\" href=\"{}\"><span>Next</span>{}</a>",
            relative(p.out, out),
            esc(label)
        );
    }
    o.push_str("</footer>");
    if let Some(src) = p.source {
        let _ = write!(
            o,
            "<p class=\"edit\"><a href=\"{GITHUB_BLOB}{src}\">This page on GitHub: {}</a></p>",
            esc(src)
        );
    }
    let _ = write!(
        o,
        "</main><aside class=\"aside\">{}</aside></div>\n</body>\n</html>\n",
        p.toc
    );
    o
}

/// The home page: no sidebar, the content wide.
pub fn home(body: &str, out: &str) -> String {
    let mut o = String::new();
    head(
        &mut o,
        "Strand",
        out,
        "Strand is a toolkit for Wayland shells: bars, launchers, lock screens and panels in one small typed language that animates, reloads live and stays idle.",
    );
    o.push_str("<body class=\"home\">\n");
    topbar(&mut o, out, "");
    let _ = write!(
        o,
        "<main class=\"home-main\" id=\"main\">{body}</main>\n<footer class=\"foot\"><div class=\"foot-in\">\
         <span>{MARK}strand</span><span>Built from <a href=\"{GITHUB}\">its repository</a> by strand-site; \
         every example on these pages is checked by its build.</span></div></footer>\n</body>\n</html>\n"
    );
    o
}
