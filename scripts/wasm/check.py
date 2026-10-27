"""The live examples in headless Chromium (scripts/wasm/check-browser.sh;
docs/live-examples.md): the built site, with the live examples added
(scripts/wasm/site.sh), served on the container's loopback (a browser
loads no WebAssembly from file://), no other network. A problem is a
console error, an uncaught exception, a failed request, or any check
below that does not hold:

- a page loads no WebAssembly until the reader asks;
- each docs example, live in its figure, in light and in dark mode:
  every frame its steps paint has the time and the pixel hash of the
  frame eval drew for the site (media/<name>.json, held to the approved
  reference recordings), frame for frame, as tests/live.rs holds them
  natively;
- Next step runs a step that changes the example, Play plays the steps
  again, Restart starts over without them, Back to recording brings the
  player back;
- an edit with an error shows its diagnostic, a fixed one runs again,
  Reset puts the config back;
- from file:// no figure offers live mode, and the playground says why;
- the playground runs each example; no page is wider than a phone's
  screen with live mode open.

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

    # Each example's first page, the home page first.
    pages = {}
    for p in sorted(site.rglob("*.html"), key=lambda p: (str(p.relative_to(site)) != "index.html", str(p))):
        for name in re.findall(r'<figure class="example[^"]*" id="example-([a-z0-9-]+)"', p.read_text()):
            pages.setdefault(name, str(p.relative_to(site)))
    if not pages:
        problems.append("no page has an example figure")

    def watch(page, where, requests=None):
        page.on("console", lambda m, w=where: m.type == "error" and problems.append(f"{w}: console: {m.text}"))
        page.on("pageerror", lambda e, w=where: problems.append(f"{w}: exception: {e}"))
        page.on("requestfailed", lambda r, w=where: problems.append(f"{w}: failed: {r.url}"))
        if requests is not None:
            page.on("request", lambda r: requests.append(r.url))

    def go_live(page, fid):
        fig = page.locator(f"#{fid}")
        fig.scroll_into_view_if_needed()
        t0 = page.evaluate("performance.now()")
        fig.locator(".ex-live-try").click()
        page.wait_for_function(
            "(id) => { const r = window.StrandLive && window.StrandLive.figures.get(id); return r && r.firstDrawn !== undefined; }",
            arg=fid, timeout=60000)
        return page.evaluate("([id, t0]) => window.StrandLive.figures.get(id).firstDrawn - t0", [fid, t0])

    with sync_playwright() as p:
        browser = p.chromium.launch()
        ready = "window.StrandLive && window.StrandLive.figures.get('playground') && window.StrandLive.figures.get('playground').firstDrawn !== undefined"

        def guard(what, f, *args):
            try:
                f(*args)
            except Exception as e:  # a check that could not finish is a problem
                problems.append(f"{what}: {type(e).__name__}: {str(e).splitlines()[0] if str(e) else ''}")

        def one_example(name, rel):
            fid = f"example-{name}"
            res = {"page": rel}
            ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
            page = ctx.new_page()
            requests = []
            watch(page, f"{rel} [{name}]", requests)
            page.goto(base + rel, wait_until="load")
            page.wait_for_timeout(300)
            if any("/wasm/strand_web" in u or u.endswith(".wasm") for u in requests):
                problems.append(f"{rel}: the page fetched the WebAssembly before the reader asked")
            if page.locator(f"#{fid} .ex-live-try").count() != 1:
                problems.append(f"{rel}: {fid} offers no live mode")
                ctx.close()
                return
            res["first_frame_ms"] = round(go_live(page, fid), 1)
            res["boot_ms"] = page.evaluate("window.StrandLive.boot(window.STRAND_WASM, null).then(l => l.timing)")
            media = json.loads((site / "media" / f"{name}.json").read_text())
            want = eval_frames(media)
            for mode, dark in (("light", False), ("dark", True)):
                r = page.evaluate("([id, dark]) => window.StrandLive.figures.get(id).check(dark)", [fid, dark])
                got = live_frames(r["paints"])
                res[f"{mode}_frames_equal"] = compare(name, mode, got, want[mode], problems)
                res[f"{mode}_frames"] = len(want[mode])
                res[f"{mode}_ticks"] = r["ticks"]
                res[f"{mode}_run_ms"] = round(r["ms"], 1)
            # Played in real time to its end: what a tick and a draw cost.
            page.wait_for_function(
                "(id) => { const r = window.StrandLive.figures.get(id); return !r.playing && r.live.at_rest(); }",
                arg=fid, timeout=30000)
            cost = page.evaluate("(id) => window.StrandLive.figures.get(id).cost", fid)
            res["live_tick_ms"] = round(cost["tickMs"] / max(1, cost["ticks"]), 3)
            res["live_draw_ms"] = round(cost["frameMs"] / max(1, cost["frames"]), 3)
            res["live_ticks"] = cost["ticks"]
            res["live_frames"] = cost["frames"]
            res["memory_bytes"] = page.evaluate("window.StrandLive.boot(window.STRAND_WASM, null).then(l => l.memory.buffer.byteLength)")
            page.locator(f"#{fid}").screenshot(path=str(shots / f"live-{name}.png"))
            # At the steps' end, at rest, Next step runs the steps that act
            # one at a time, its tooltip naming the one it runs: pressed
            # in turn, one of them changes the example (the last one may
            # set what the steps left), which paints.
            nxt = page.locator(f"#{fid} .lv-next")
            n_acts = page.evaluate("(id) => window.StrandLive.figures.get(id).acts.length", fid)
            painted = "(id) => window.StrandLive.figures.get(id).live.painted()"
            acted = n_acts == 0
            if n_acts and not nxt.is_visible():
                problems.append(f"{name}: no Next step for its {n_acts} steps")
                n_acts = 0
            for i in range(n_acts):
                painted0 = page.evaluate(painted, fid)
                if not (nxt.get_attribute("title") or ""):
                    problems.append(f"{name}: Next step does not name the step it runs")
                nxt.click()
                page.wait_for_timeout(600)
                if page.evaluate(painted, fid) > painted0:
                    acted = True
                    break
            if not acted:
                problems.append(f"{name}: no step Next step ran changed the example")
            # Play boots the example again and plays its steps; Restart
            # boots it again without them.
            for button, what in (("lv-replay", "Play"), ("lv-restart", "Restart")):
                starts = page.evaluate("(id) => window.StrandLive.figures.get(id).starts", fid)
                page.locator(f"#{fid} .{button}").click()
                if page.evaluate("(id) => window.StrandLive.figures.get(id).starts", fid) != starts + 1:
                    problems.append(f"{name}: {what} does not start the example again")
            if page.evaluate("(id) => window.StrandLive.figures.get(id).playing", fid):
                problems.append(f"{name}: Restart plays the steps")
            # Back to recording: the player again, the canvas gone.
            page.locator(f"#{fid} .lv-back").click()
            if page.locator(f"#{fid} .lv-canvas").count() or not page.locator(f"#{fid} .ex-stage .ex-sprite").first.is_visible():
                problems.append(f"{name}: Back to recording does not bring the player back")
            out["examples"][name] = res
            ctx.close()


        for name, rel in pages.items():
            guard(f"example {name}", one_example, name, rel)

        def sec_edit():
            # An edit with an error, then a fix, then Reset (on springs, or
            # the first example).
            name = "springs" if "springs" in pages else next(iter(pages), None)
            if name:
                fid = f"example-{name}"
                ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
                page = ctx.new_page()
                watch(page, f"{pages[name]} [edit]")
                page.goto(base + pages[name], wait_until="load")
                go_live(page, fid)
                area = page.locator(f"#{fid} .lv-code").first
                original = area.input_value()
                area.fill(original + "\nlet nope = battery.nope\n")
                try:
                    page.wait_for_function("(id) => document.getElementById(id).classList.contains('lv-failed')", arg=fid, timeout=5000)
                    err = page.text_content(f"#{fid} .lv-out") or ""
                    out["error_shown"] = err.splitlines()[:6]
                    if "nope" not in err:
                        problems.append(f"edit: the broken config shows no diagnostic: {err!r}")
                except Exception:
                    problems.append("edit: a config with an error shows no failure")
                page.locator(f"#{fid}").screenshot(path=str(shots / "live-error.png"))
                area.fill(original + "\n// edited\n")
                page.wait_for_timeout(800)
                if page.evaluate("(id) => document.getElementById(id).classList.contains('lv-failed')", fid):
                    problems.append("edit: the fixed config still shows as failed")
                out["edit_boot_ms"] = round(page.evaluate("(id) => window.StrandLive.figures.get(id).loadMs", fid), 1)
                page.locator(f"#{fid} .lv-reset").click()
                if area.input_value() != original:
                    problems.append("edit: Reset does not put the config back")
                ctx.close()

        guard('edit', sec_edit)

        def sec_file():
            # From a file, nothing is offered, and the playground says why.
            ctx = browser.new_context(viewport={"width": 1440, "height": 900})
            page = ctx.new_page()
            watch(page, "file://")
            for rel in list(pages.values())[:1]:
                page.goto((Path(a.site) / rel).resolve().as_uri(), wait_until="load")
                if page.locator(".ex-live-try").count():
                    problems.append(f"file://{rel}: a figure offers live mode from a file")
            if (site / "playground.html").is_file():
                page.goto((Path(a.site) / "playground.html").resolve().as_uri(), wait_until="load")
                note = page.text_content(".pg-note") or ""
                if "http" not in note:
                    problems.append(f"file:// playground: its note does not say why: {note!r}")
            ctx.close()

        guard('file', sec_file)

        def sec_playground():
            # The playground, each example in turn, light and dark.
            if (site / "playground.html").is_file():
                for scheme in ("light", "dark"):
                    ctx = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme, device_scale_factor=1)
                    page = ctx.new_page()
                    watch(page, f"playground [{scheme}]")
                    page.goto(base + "playground.html", wait_until="load")
                    page.wait_for_function(ready, timeout=60000)
                    choices = page.locator(".pg-choice")
                    for i in range(choices.count()):
                        choices.nth(i).click()
                        page.wait_for_function("(() => { const r = window.StrandLive.figures.get('playground'); return r && r.live && r.live.painted() > 0; })()", timeout=10000)
                        page.wait_for_timeout(250)
                        if scheme == "light":
                            page.locator(".playground").screenshot(path=str(shots / f"playground-{i}.png"))
                    page.screenshot(path=str(shots / f"playground-{scheme}.png"), full_page=True)
                    ctx.close()

        guard('playground', sec_playground)

        def sec_phone():
            # A phone's width, live mode open: no sideways overflow.
            phone = list(pages)[:2] + (["playground"] if (site / "playground.html").is_file() else [])
            for name in phone:
                ctx = browser.new_context(viewport={"width": 390, "height": 900}, device_scale_factor=2)
                page = ctx.new_page()
                watch(page, f"phone [{name}]")
                if name == "playground":
                    page.goto(base + "playground.html", wait_until="load")
                    page.wait_for_function(ready, timeout=60000)
                    target = page.locator(".playground")
                else:
                    page.goto(base + pages[name], wait_until="load")
                    go_live(page, f"example-{name}")
                    target = page.locator(f"#example-{name}")
                page.wait_for_timeout(300)
                over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
                if over > 0:
                    problems.append(f"phone [{name}]: {over}px wider than the screen")
                target.screenshot(path=str(shots / f"live-phone-{name}.png"))
                ctx.close()
        guard('phone', sec_phone)

        browser.close()
    httpd.shutdown()
    out["problems"] = problems
    print(json.dumps(out, indent=1))
    for pr in problems:
        print(pr, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
