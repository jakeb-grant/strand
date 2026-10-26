"""The playground in headless Chromium (scripts/wasm/check-browser.sh):
the built site served on the container's loopback (a browser loads no
WebAssembly from file://), no other network. Fails on a console error,
an uncaught exception or a failed request. Then, in the page:

- each docs example run as its scenario's steps say (rest, set, rest
  ...), every tick's frame compared with the frame eval drew for the
  site (media/<name>-light.png and .json);
- the time each tick and each frame's copy took, the time to the first
  frame drawn, the module's memory;
- a config with an error shows its diagnostic, and a fixed one runs.

Prints one JSON object and saves screenshots."""

import argparse
import functools
import http.server
import json
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

RUN = r"""
async ([name, frames]) => {
  const { lib } = window.strandPlayground;
  const ex = lib.examples[name];
  const meta = await (await fetch(`media/${name}.json`)).json();
  const img = await (await fetch(`media/${name}-light.png`)).blob();
  const bmp = await createImageBitmap(img, { colorSpaceConversion: "none", premultiplyAlpha: "none" });
  const count = meta.modes.light.count;
  const w = bmp.width, h = bmp.height / count;
  const cv = new OffscreenCanvas(bmp.width, bmp.height);
  const cx = cv.getContext("2d", { colorSpace: "srgb" });
  cx.drawImage(bmp, 0, 0);
  const sprite = cx.getImageData(0, 0, bmp.width, bmp.height).data;
  const services = Object.assign({}, ex.services, { "system.dark": false });
  const t0 = performance.now();
  const sh = lib.m.WebShell.load(JSON.stringify({ "shell.strand": ex.config }), JSON.stringify(services),
    ex.screen.width, ex.screen.height, ex.screen.scale, 60);
  const loadMs = performance.now() - t0;
  const tickMs = [], frameMs = [], diffs = [];
  const tick = () => {
    const a = performance.now();
    sh.tick();
    const b = performance.now();
    const px = sh.frame_top(h);
    const c = performance.now();
    tickMs.push(b - a);
    frameMs.push(c - b);
    const k = diffs.length;
    const f = meta.modes.light.ticks[k];
    let n = 0, max = 0;
    if (f === undefined) { diffs.push([-1, 0]); return; }
    const off = f * w * h * 4;
    for (let i = 0; i < w * h * 4; i += 4) {
      let d = 0;
      for (let j = 0; j < 4; j++) d = Math.max(d, Math.abs(px[i + j] - sprite[off + i + j]));
      if (d) { n++; max = Math.max(max, d); }
    }
    diffs.push([n, max]);
  };
  for (const s of ex.steps) {
    const p = s.split(/\s+/);
    if (p[0] === "rest") {
      let i = 0;
      do { tick(); i++; } while (!sh.at_rest() && i < 600);
    } else if (p[0] === "set") {
      sh.set(p[1], p.slice(2).join(" "));
    }
  }
  const sum = (a) => a.reduce((x, y) => x + y, 0);
  const sorted = (a) => [...a].sort((x, y) => x - y);
  const pct = (a, q) => sorted(a)[Math.min(a.length - 1, Math.floor(q * a.length))];
  sh.free();
  return {
    ticks: diffs.length, eval_ticks: meta.modes.light.ticks.length,
    frames_equal: diffs.filter((d) => d[0] === 0).length,
    worst: diffs.reduce((m, d, k) => (d[0] > m[1] ? [k, d[0], d[1]] : m), [-1, 0, 0]),
    load_ms: loadMs,
    tick_ms_mean: sum(tickMs) / tickMs.length, tick_ms_p95: pct(tickMs, 0.95), tick_ms_max: Math.max(...tickMs),
    copy_ms_mean: sum(frameMs) / frameMs.length,
    frame_px: [w, h],
  };
}
"""


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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", required=True)
    ap.add_argument("--shots", required=True)
    a = ap.parse_args()
    shots = Path(a.shots)
    shots.mkdir(parents=True, exist_ok=True)
    httpd = serve(a.site)
    url = f"http://127.0.0.1:{httpd.server_address[1]}/playground.html"
    problems = []
    out = {}
    with sync_playwright() as p:
        browser = p.chromium.launch()
        for scheme in ("light", "dark"):
            ctx = browser.new_context(viewport={"width": 1440, "height": 900}, color_scheme=scheme, device_scale_factor=1)
            page = ctx.new_page()
            page.on("console", lambda m, s=scheme: m.type == "error" and problems.append(f"{s}: console: {m.text}"))
            page.on("pageerror", lambda e, s=scheme: problems.append(f"{s}: exception: {e}"))
            page.on("requestfailed", lambda r, s=scheme: problems.append(f"{s}: failed: {r.url}"))
            page.goto(url, wait_until="load")
            page.wait_for_function("window.strandPlayground && window.strandPlayground.examples.every(e => e.firstDrawn !== undefined && e.shell && e.shell.at_rest())", timeout=60000)
            if scheme == "light":
                out["first_frame_ms"] = page.evaluate("window.strandPlayground.examples.map(e => [e.ex.id, Math.round(e.firstDrawn)])")
                out["boot_ms"] = page.evaluate("window.strandPlayground.lib.timing")
                out["dom_content_loaded_ms"] = page.evaluate("performance.getEntriesByType('navigation')[0].domContentLoadedEventEnd")
                for name in ("hello", "springs"):
                    out[name] = page.evaluate(RUN, [name, True])
                out["memory_bytes"] = page.evaluate("window.strandPlayground.lib.m.memory_bytes()")
                out["js_heap_bytes"] = page.evaluate("performance.memory ? performance.memory.usedJSHeapSize : null")
                # A live run in the page: flip `on` and let it settle.
                page.click(".pg[data-example=springs] .pg-flip")
                page.wait_for_timeout(300)
                page.screenshot(path=str(shots / "playground-moving-light.png"), full_page=True)
                page.wait_for_function("window.strandPlayground.examples[1].shell.at_rest()", timeout=10000)
                ex = page.evaluate("(() => { const e = window.strandPlayground.examples[1]; return {ticks: e.cost.ticks, tick_ms: e.cost.tickMs / e.cost.ticks, frames: e.cost.frames, draw_ms: e.cost.frameMs / Math.max(1, e.cost.frames)}; })()")
                out["springs_live"] = ex
                # An error, then a fix.
                hello = ".pg[data-example=hello]"
                page.fill(f"{hello} .pg-code", "bar Top {\n  edge: top; height: 32\n  text battery.nope\n}\n")
                page.click(f"{hello} .pg-run")
                err = page.text_content(f"{hello} .pg-out")
                out["error_shown"] = err.splitlines()[:6]
                if "nope" not in err:
                    problems.append(f"the broken config shows no diagnostic: {err!r}")
                page.fill(f"{hello} .pg-code", "bar Top {\n  edge: top; height: 32\n  text \"edited\"\n}\n")
                page.click(f"{hello} .pg-run")
                page.wait_for_timeout(300)
                if page.evaluate("document.querySelector('.pg[data-example=hello]').classList.contains('pg-failed')"):
                    problems.append("the fixed config still shows as failed")
            page.screenshot(path=str(shots / f"playground-{scheme}.png"), full_page=True)
            ctx.close()
        # A phone's width: no sideways overflow.
        ctx = browser.new_context(viewport={"width": 390, "height": 900}, device_scale_factor=2)
        page = ctx.new_page()
        page.on("pageerror", lambda e: problems.append(f"phone: exception: {e}"))
        page.goto(url, wait_until="load")
        page.wait_for_function("window.strandPlayground && window.strandPlayground.examples.every(e => e.firstDrawn !== undefined)", timeout=60000)
        over = page.evaluate("document.documentElement.scrollWidth - window.innerWidth")
        if over > 0:
            problems.append(f"phone: {over}px wider than the screen")
        page.screenshot(path=str(shots / "playground-phone.png"), full_page=True)
        ctx.close()
        browser.close()
    httpd.shutdown()
    out["problems"] = problems
    print(json.dumps(out, indent=1))
    for name in ("hello", "springs"):
        r = out.get(name, {})
        if r.get("frames_equal") != r.get("ticks") or r.get("ticks") != r.get("eval_ticks"):
            problems.append(f"{name}: {r.get('frames_equal')} of {r.get('ticks')} frames equal eval's ({r.get('eval_ticks')} ticks)")
    for pr in problems:
        print(pr, file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
