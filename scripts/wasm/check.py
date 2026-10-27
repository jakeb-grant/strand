"""The playground in headless Chromium (scripts/wasm/check-browser.sh;
docs/live-examples.md): the built site, with the live examples added
(scripts/wasm/site.sh), served on the container's loopback (a browser
loads no WebAssembly from file://), no other network. A problem is a
console error, an uncaught exception, a failed request, or any check
below that does not hold:

- no page but the playground loads WebAssembly, and no figure offers
  to run live;
- each docs example, in the playground, in light and in dark mode:
  every frame its steps paint has the time and the pixel hash of the
  frame eval drew for the site (media/<name>.json, held to the approved
  reference recordings), frame for frame, as tests/live.rs holds them
  natively;
- Next step names the step it runs and runs a step that changes the
  example, Play plays the steps again (and says Playing while they
  run);
- an edit with an error shows its diagnostic, a fixed one runs again,
  Reset puts the config back;
- from file:// the playground says why it cannot run;
- no page is wider than a phone's screen with the playground running.

Prints one JSON object (timings among it) and saves screenshots."""

import argparse
import functools
import http.server
import json
import re
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright


def serve(root):
    class Quiet(http.server.SimpleHTTPRequestHandler):
        extensions_map = {
            **http.server.SimpleHTTPRequestHandler.extensions_map,
            ".wasm": "application/wasm",
            ".js": "text/javascript",
        }

        def log_message(self, *args):
            pass

    handler = functools.partial(Quiet, directory=root)
    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return httpd


def eval_frames(media):
    """Each frame eval drew, as (time ms to a tenth, pixel hash), per mode."""
    out = {}
    for mode in ("light", "dark"):
        m = media["modes"][mode]
        out[mode] = [(t, m["frames"][i]) for t, i in zip(media["t"], m["ticks"])]
    return out


def live_frames(paints):
    if not paints:
        return []
    t0 = paints[0][1]
    return [(round((p[1] - t0) * 10) / 10, p[4]) for p in paints]


def compare(name, mode, got, want, problems):
    """Frames equal, in order; each difference is a problem."""
    if len(got) != len(want):
        problems.append(f"{name} {mode}: {len(got)} frames live, eval drew {len(want)}")
    for j, (g, w) in enumerate(zip(got, want)):
        if g[0] != w[0] or g[1] != w[1]:
            problems.append(f"{name} {mode}: frame {j} at {g[0]} ms {g[1][:12]}, eval's at {w[0]} ms {w[1][:12]}")
            return j
    return min(len(got), len(want))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", required=True)
    ap.add_argument("--shots", required=True)
    a = ap.parse_args()
    site = Path(a.site)
    shots = Path(a.shots)
    shots.mkdir(parents=True, exist_ok=True)
    httpd = serve(a.site)
    base = f"http://127.0.0.1:{httpd.server_address[1]}/"
    problems = []
    out = {"examples": {}}

    # Each example's first page, the home page first: a figure there
    # is its frames, and the page fetches no WebAssembly.
    pages = {}
    for p in sorted(site.rglob("*.html"), key=lambda p: (str(p.relative_to(site)) != "index.html", str(p))):
        for name in re.findall(r'<figure class="example[^"]*" id="example-([a-z0-9-]+)"', p.read_text()):
            pages.setdefault(name, str(p.relative_to(site)))
    if not pages:
        problems.append("no page has an example figure")
    if not (site / "playground.html").is_file():
        problems.append("the site has no playground")

    def watch(page, where, requests=None):
        page.on("console", lambda m, w=where: m.type == "error" and problems.append(f"{w}: console: {m.text}"))
        page.on("pageerror", lambda e, w=where: problems.append(f"{w}: exception: {e}"))
        page.on("requestfailed", lambda r, w=where: problems.append(f"{w}: failed: {r.url}"))
        if requests is not None:
            page.on("request", lambda r: requests.append(r.url))

    RUN = "window.StrandLive.run"
    ready = f"window.StrandLive && {RUN} && {RUN}.firstDrawn !== undefined"

    def run(page, expr):
        """`expr` of the playground's run (`r`)."""
        return page.evaluate(f"(() => {{ const r = window.StrandLive.run; return {expr}; }})()")

    def open_playground(browser, scheme="light", width=1440, scale=1):
        ctx = browser.new_context(viewport={"width": width, "height": 900}, color_scheme=scheme, device_scale_factor=scale)
        page = ctx.new_page()
        watch(page, f"playground [{scheme}, {width}px]")
        page.goto(base + "playground.html", wait_until="load")
        page.wait_for_function(ready, timeout=60000)
        return ctx, page

    def pick(page, i):
        """Picks the `i`th example; the ms to its first frame."""
        t0 = page.evaluate("performance.now()")
        page.locator(".pg-choice").nth(i).click()
        page.wait_for_function(f"{ready} && {RUN}.data.name === {json.dumps(page.locator('.pg-choice').nth(i).inner_text().strip())}", timeout=10000)
        return page.evaluate(f"(t0) => {RUN}.firstDrawn - t0", t0)

    with sync_playwright() as p:
        browser = p.chromium.launch()

        def guard(what, f, *args):
            try:
                f(*args)
            except Exception as e:  # a check that could not finish is a problem
                problems.append(f"{what}: {type(e).__name__}: {str(e).splitlines()[0] if str(e) else ''}")

        def sec_pages():
            # A page with an example fetches no WebAssembly and offers
            # nothing live.
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            for rel in sorted(set(pages.values())):
                page = ctx.new_page()
                requests = []
                watch(page, rel, requests)
                page.goto(base + rel, wait_until="load")
                page.wait_for_timeout(300)
                if any("/wasm/strand_web" in u or u.endswith(".wasm") or u.endswith("/live.js") for u in requests):
                    problems.append(f"{rel}: the page fetched the playground's code")
                if page.locator(".lv-canvas, .ex-live-try").count():
                    problems.append(f"{rel}: a figure offers to run live")
                page.close()
            ctx.close()

        guard("pages", sec_pages)

        def sec_examples():
            # Each example in turn: eval's frames, then its buttons.
            ctx, page = open_playground(browser)
            names = [c.strip() for c in page.locator(".pg-choice").all_inner_texts()]
            for name in pages:
                if name not in names:
                    problems.append(f"{name}: the playground does not offer it")
            for i, name in enumerate(names):
                guard(f"example {name}", one_example, page, i, name)
            out["boot_ms"] = page.evaluate("window.StrandLive.boot(window.STRAND_WASM, null).then(l => l.timing)")
            out["memory_bytes"] = page.evaluate("window.StrandLive.boot(window.STRAND_WASM, null).then(l => l.memory.buffer.byteLength)")
            ctx.close()

        def one_example(page, i, name):
            res = {"first_frame_ms": round(pick(page, i), 1)}
            media = site / "media" / f"{name}.json"
            if not media.is_file():
                problems.append(f"{name}: no media/{name}.json to hold it to")
                return
            want = eval_frames(json.loads(media.read_text()))
            for mode, dark in (("light", False), ("dark", True)):
                r = page.evaluate("(dark) => window.StrandLive.run.check(dark)", dark)
                got = live_frames(r["paints"])
                res[f"{mode}_frames_equal"] = compare(name, mode, got, want[mode], problems)
                res[f"{mode}_frames"] = len(want[mode])
                res[f"{mode}_ticks"] = r["ticks"]
                res[f"{mode}_run_ms"] = round(r["ms"], 1)
            # Played in real time to its end: what a tick and a draw cost.
            page.wait_for_function(f"!{RUN}.playing && {RUN}.live.at_rest()", timeout=30000)
            cost = run(page, "r.cost")
            res["live_tick_ms"] = round(cost["tickMs"] / max(1, cost["ticks"]), 3)
            res["live_draw_ms"] = round(cost["frameMs"] / max(1, cost["frames"]), 3)
            res["live_ticks"] = cost["ticks"]
            res["live_frames"] = cost["frames"]
            page.locator(".playground").screenshot(path=str(shots / f"playground-{name}.png"))
            # At the steps' end, at rest, Next step runs the steps that act
            # one at a time, naming the one it runs: pressed in turn, one
            # of them changes the example (the last one may set what the
            # steps left), which paints.
            nxt = page.locator(".playground .lv-next")
            n_acts = run(page, "r.acts.length")
            acted = n_acts == 0
            if n_acts and not nxt.is_visible():
                problems.append(f"{name}: no Next step for its {n_acts} steps")
                n_acts = 0
            for _ in range(n_acts):
                painted0 = run(page, "r.live.painted()")
                if not (nxt.get_attribute("title") or "") or not nxt.locator(".lv-next-what").inner_text().strip():
                    problems.append(f"{name}: Next step does not name the step it runs")
                nxt.click()
                page.wait_for_timeout(600)
                if run(page, "r.live.painted()") > painted0:
                    acted = True
                    break
            if not acted:
                problems.append(f"{name}: no step Next step ran changed the example")
            # Play boots the example again and plays its steps, and says
            # so until they end.
            if n_acts:
                starts = run(page, "r.starts")
                play = page.locator(".playground .lv-replay")
                play.click()
                if run(page, "r.starts") != starts + 1:
                    problems.append(f"{name}: Play does not start the example again")
                if run(page, "r.playing") and play.inner_text().strip() != "Playing":
                    problems.append(f"{name}: Play does not say the steps are playing")
                page.wait_for_function(f"!{RUN}.playing", timeout=30000)
                if play.inner_text().strip() != "Play" or play.is_disabled():
                    problems.append(f"{name}: Play does not come back when the steps end")
            out["examples"][name] = res

        guard("examples", sec_examples)

        def sec_edit():
            # An edit with an error, then a fix, then Reset (on springs, or
            # the first example).
            ctx, page = open_playground(browser)
            names = [c.strip() for c in page.locator(".pg-choice").all_inner_texts()]
            pick(page, names.index("springs") if "springs" in names else 0)
            area = page.locator(".playground .lv-code").first
            original = area.input_value()
            area.fill(original + "\nlet nope = battery.nope\n")
            try:
                page.wait_for_function("document.querySelector('.playground').classList.contains('lv-failed')", timeout=5000)
                err = page.text_content(".playground .lv-out") or ""
                out["error_shown"] = err.splitlines()[:6]
                if "nope" not in err:
                    problems.append(f"edit: the broken config shows no diagnostic: {err!r}")
            except Exception:
                problems.append("edit: a config with an error shows no failure")
            page.locator(".playground").screenshot(path=str(shots / "playground-error.png"))
            area.fill(original + "\n// edited\n")
            page.wait_for_timeout(800)
            if page.evaluate("document.querySelector('.playground').classList.contains('lv-failed')"):
                problems.append("edit: the fixed config still shows as failed")
            out["edit_boot_ms"] = round(run(page, "r.loadMs"), 1)
            page.locator(".playground .lv-reset").click()
            if area.input_value() != original:
                problems.append("edit: Reset does not put the config back")
            ctx.close()

        guard("edit", sec_edit)

        def sec_file():
            # From a file the playground says why it cannot run.
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            page = ctx.new_page()
            watch(page, "file://")
            page.goto((site / "playground.html").resolve().as_uri(), wait_until="load")
            note = page.text_content(".pg-note") or ""
            if "http" not in note:
                problems.append(f"file:// playground: its note does not say why: {note!r}")
            ctx.close()

        guard("file", sec_file)

        def sec_dark():
            ctx, page = open_playground(browser, "dark")
            page.screenshot(path=str(shots / "playground-dark.png"), full_page=True)
            ctx.close()

        guard("dark", sec_dark)

        def sec_phone():
            # A phone's width, the playground running each example: no
            # sideways overflow.
            ctx, page = open_playground(browser, width=390, scale=2)
            for i in range(page.locator(".pg-choice").count()):
                pick(page, i)
                page.wait_for_timeout(300)
                over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
                if over > 0:
                    problems.append(f"phone [playground {i}]: {over}px wider than the screen")
            page.locator(".playground").screenshot(path=str(shots / "playground-phone.png"))
            ctx.close()

        guard("phone", sec_phone)

        browser.close()
    httpd.shutdown()
    out["problems"] = problems
    print(json.dumps(out, indent=1))
    for pr in problems:
        print(pr, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
