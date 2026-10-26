"""The built site in a real browser (scripts/site/shots.sh): each page at a
desktop's, a tablet's and a phone's width, light and dark, from file://
with no network. Fails on a console error, an uncaught exception, a
request that fails, or a page wider than the screen; saves a full-page
screenshot of each."""

import argparse
import sys
from pathlib import Path

from playwright.sync_api import sync_playwright

WIDTHS = {"desktop": 1440, "tablet": 768, "phone": 390}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", required=True)
    ap.add_argument("--shots", required=True)
    ap.add_argument("pages", nargs="*")
    a = ap.parse_args()
    site = Path(a.site)
    shots = Path(a.shots)
    shots.mkdir(parents=True, exist_ok=True)
    pages = a.pages or ["index.html"]
    problems = []
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
                    page.on("console", lambda m, w=where: m.type == "error" and problems.append(f"{w}: console: {m.text}"))
                    page.on("pageerror", lambda e, w=where: problems.append(f"{w}: exception: {e}"))
                    page.on("requestfailed", lambda r, w=where: problems.append(f"{w}: failed: {r.url}"))
                    page.goto((site / rel).resolve().as_uri(), wait_until="load")
                    page.wait_for_timeout(400)
                    over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
                    if over > 0:
                        problems.append(f"{where}: {over}px wider than the screen")
                    name = rel.replace("/", "_").removesuffix(".html")
                    page.screenshot(path=str(shots / f"{name}-{wname}-{scheme}.png"), full_page=True)
                    page.close()
                ctx.close()
        browser.close()
    for pr in problems:
        print(pr)
    print(f"shots: {len(pages) * 6} in {shots}; {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
