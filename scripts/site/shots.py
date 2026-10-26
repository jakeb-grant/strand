"""The built site in a real browser (scripts/site/shots.sh): each page at a
desktop's, a tablet's and a phone's width, light and dark, from file://
with no network. Fails on a console error, an uncaught exception, a
request that fails, or a page wider than the screen; saves a full-page
screenshot of each. Then drives the search on the first page, at a
desktop's and a phone's width: "/" focuses it, typing lists results,
the arrows move through them and Enter opens one, a word found nowhere
says so; and on each page, each example player shows another picture at
a later frame than at its first. Each step that does not work is a
problem."""

import argparse
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

WIDTHS = {"desktop": 1440, "tablet": 768, "phone": 390}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", required=True)
    ap.add_argument("--shots", required=True)
    ap.add_argument("--all", action="store_true", help="every page of the site, the home page first")
    ap.add_argument("pages", nargs="*")
    a = ap.parse_args()
    site = Path(a.site)
    shots = Path(a.shots)
    shots.mkdir(parents=True, exist_ok=True)
    pages = a.pages or ["index.html"]
    if a.all:
        rest = sorted(str(p.relative_to(site)) for p in site.rglob("*.html") if str(p.relative_to(site)) != "index.html")
        pages = ["index.html"] + rest
    problems = []

    def watch(page, where):
        page.on("console", lambda m, w=where: m.type == "error" and problems.append(f"{w}: console: {m.text}"))
        page.on("pageerror", lambda e, w=where: problems.append(f"{w}: exception: {e}"))
        page.on("requestfailed", lambda r, w=where: problems.append(f"{w}: failed: {r.url}"))

    def players(browser, rel):
        """Each example player shows another picture at its middle frame
        than at its first, and its moving part stays inside the stage."""
        where = f"{rel} [players]"
        ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1, reduced_motion="reduce")
        page = ctx.new_page()
        watch(page, where)
        page.goto((site / rel).resolve().as_uri(), wait_until="load")
        figs = page.locator(".example:has(.ex-scrub)")
        for i in range(figs.count()):
            fig = figs.nth(i)
            fid = fig.get_attribute("id") or str(i)
            fig.scroll_into_view_if_needed()
            stage = fig.locator(".ex-stage")
            first = stage.screenshot()
            top = int(fig.locator(".ex-scrub").get_attribute("max") or "0")
            fig.locator(".ex-scrub").evaluate(
                "(el, v) => { el.value = String(v); el.dispatchEvent(new Event('input')); }", top // 6)
            page.wait_for_timeout(100)
            name = rel.replace("/", "_").removesuffix(".html")
            fig.screenshot(path=str(shots / f"{name}-{fid}-frame.png"))
            if stage.screenshot() == first:
                problems.append(f"{where}: {fid}: its frame {top // 6} looks like its first")
            inside = page.evaluate(
                """(id) => { const f = document.getElementById(id); const s = f.querySelector('.ex-stage').getBoundingClientRect();
                return [...f.querySelectorAll('.ex-moving')].every(m => { if (!m.offsetParent) return true;
                const r = m.getBoundingClientRect(); return r.left >= s.left - 1 && r.right <= s.right + 1 && r.top >= s.top - 1 && r.bottom <= s.bottom + 1; }); }""",
                fid)
            if not inside:
                problems.append(f"{where}: {fid}: its moving part lies outside its stage")
        ctx.close()

    def search(browser, rel, wname, width):
        where = f"{rel} [search, {wname}]"
        ctx = browser.new_context(viewport={"width": width, "height": 900}, device_scale_factor=1, reduced_motion="reduce")
        page = ctx.new_page()
        watch(page, where)
        page.goto((site / rel).resolve().as_uri(), wait_until="load")
        page.keyboard.press("/")
        if not page.evaluate("document.activeElement && document.activeElement.id === 'search'"):
            problems.append(f"{where}: '/' does not focus the search")
            ctx.close()
            return
        page.keyboard.type("slider value")
        try:
            page.wait_for_selector(".search-hit", timeout=5000)
        except Exception:
            problems.append(f"{where}: no results for 'slider value'")
            ctx.close()
            return
        hits = page.locator(".search-hit")
        if hits.count() < 2:
            problems.append(f"{where}: {hits.count()} result(s) for 'slider value'")
        if hits.nth(0).get_attribute("aria-selected") != "true":
            problems.append(f"{where}: the first result is not selected")
        page.keyboard.press("ArrowDown")
        if hits.count() > 1 and hits.nth(1).get_attribute("aria-selected") != "true":
            problems.append(f"{where}: ArrowDown does not move to the second result")
        over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
        if over > 0:
            problems.append(f"{where}: {over}px wider than the screen with results open")
        name = rel.replace("/", "_").removesuffix(".html")
        page.screenshot(path=str(shots / f"{name}-search-{wname}.png"))
        target = hits.nth(1 if hits.count() > 1 else 0).get_attribute("href") or ""
        page.keyboard.press("Enter")
        try:
            page.wait_for_url(target, timeout=5000)
        except Exception:
            problems.append(f"{where}: Enter did not open {target} (at {page.url})")
        page.keyboard.press("/")
        page.keyboard.type("zzqqxx")
        try:
            page.wait_for_selector(".search-note", timeout=5000)
        except Exception:
            problems.append(f"{where}: a word found nowhere shows no note")
        page.keyboard.press("Escape")
        if page.locator(".search-results").is_visible():
            problems.append(f"{where}: Escape does not close the results")
        ctx.close()
    with sync_playwright() as p:
        browser = p.chromium.launch()
        for scheme in ("light", "dark"):
            for wname, width in WIDTHS.items():
                ctx = browser.new_context(
                    viewport={"width": width, "height": 900},
                    color_scheme=scheme,
                    device_scale_factor=1,
                    reduced_motion="reduce",
                )
                for rel in pages:
                    page = ctx.new_page()
                    where = f"{rel} [{wname} {scheme}]"
                    watch(page, where)
                    page.goto((site / rel).resolve().as_uri(), wait_until="load")
                    page.wait_for_timeout(400)
                    over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
                    if over > 0:
                        problems.append(f"{where}: {over}px wider than the screen")
                    name = rel.replace("/", "_").removesuffix(".html")
                    page.screenshot(path=str(shots / f"{name}-{wname}-{scheme}.png"), full_page=True)
                    page.close()
                ctx.close()
        for rel in pages:
            players(browser, rel)
        for wname in ("desktop", "phone"):
            search(browser, pages[0], wname, WIDTHS[wname])
        browser.close()
    for pr in problems:
        print(pr)
    print(f"shots: {len(pages) * 6} in {shots}; {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
