"""Benchmarks strand-web builds side by side (scripts/wasm/measure.sh):
each round loads every variant in a fresh page, in turn, and records
the module's compile and start, the first frame of the springs example
(load and boot to a drawn frame), and the springs example's 75 ticks
and the hello bar's first tick, each tick and each frame copy timed.
Prints each variant's medians over the rounds."""

import argparse
import functools
import http.server
import json
import statistics
import threading

from playwright.sync_api import sync_playwright

PAGE = """<!doctype html><meta charset=utf-8><title>bench</title><script type=module>
const v = new URLSearchParams(location.search).get("v");
const t0 = performance.now();
const m = await import(`./${v}/strand_web.js`);
await m.default({ module_or_path: `./${v}/strand_web_bg.wasm` });
const t1 = performance.now();
for (const f of ["LiberationSans-Regular.ttf", "LiberationSans-Bold.ttf"])
  m.add_font(new Uint8Array(await (await fetch(`./${v}/${f}`)).arrayBuffer()));
const ex = await (await fetch(`./${v}/examples.json`)).json();
const run = (name) => {
  const e = ex[name];
  const a = performance.now();
  const sh = m.WebShell.load(JSON.stringify({"shell.strand": e.config}), JSON.stringify(e.services), e.screen.width, e.screen.height, e.screen.scale, 60);
  const load = performance.now() - a;
  const ticks = [], copies = [];
  let first = null;
  const tick = () => {
    const b = performance.now(); sh.tick(); const c = performance.now();
    const px = sh.frame_top(sh.content_rows()); const d = performance.now();
    ticks.push(c - b); copies.push(d - c);
    if (first === null) first = d - a;
  };
  for (const s of e.steps) {
    const p = s.split(/\\s+/);
    if (p[0] === "rest") { let i = 0; do { tick(); i++; } while (!sh.at_rest() && i < 600); }
    else if (p[0] === "set") sh.set(p[1], p.slice(2).join(" "));
  }
  sh.free();
  return { load, first, ticks, copies };
};
const springs = run("springs");
const hello = run("hello");
window.result = { compile: t1 - t0, first_frame: performance.now() - t0 - (springs.ticks.length ? 0 : 0), springs, hello, mem: m.memory_bytes() };
</script>"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True)
    ap.add_argument("--rounds", type=int, default=5)
    ap.add_argument("variants", nargs="+")
    a = ap.parse_args()

    class H(http.server.SimpleHTTPRequestHandler):
        extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map, ".wasm": "application/wasm", ".js": "text/javascript"}

        def do_GET(self):
            if self.path.startswith("/bench.html"):
                body = PAGE.encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/html")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            super().do_GET()

        def log_message(self, *args):
            pass

    httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(H, directory=a.dir))
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    port = httpd.server_address[1]
    res = {v: [] for v in a.variants}
    with sync_playwright() as p:
        browser = p.chromium.launch()
        for _ in range(a.rounds):
            for v in a.variants:
                ctx = browser.new_context()
                page = ctx.new_page()
                errors = []
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.goto(f"http://127.0.0.1:{port}/bench.html?v={v}")
                page.wait_for_function("window.result || false", timeout=120000)
                r = page.evaluate("window.result")
                if errors:
                    raise SystemExit(f"{v}: {errors}")
                res[v].append(r)
                ctx.close()
        browser.close()
    med = statistics.median
    out = {}
    print(f"{'variant':24} {'compile':>8} {'1st frame':>9} {'spr tick':>8} {'p95':>6} {'hello tick0':>11} {'copy':>6} {'mem MB':>6}")
    for v, rs in res.items():
        def p95(xs):
            xs = sorted(xs)
            return xs[min(len(xs) - 1, int(0.95 * len(xs)))]
        row = {
            "compile_ms": med(r["compile"] for r in rs),
            "springs_first_frame_ms": med(r["springs"]["first"] for r in rs),
            "springs_tick_ms": med(statistics.mean(r["springs"]["ticks"]) for r in rs),
            "springs_tick_p95_ms": med(p95(r["springs"]["ticks"]) for r in rs),
            "hello_first_tick_ms": med(r["hello"]["ticks"][0] for r in rs),
            "copy_ms": med(statistics.mean(r["springs"]["copies"]) for r in rs),
            "memory_mb": med(r["mem"] for r in rs) / 2**20,
        }
        out[v] = row
        print(f"{v:24} {row['compile_ms']:8.1f} {row['springs_first_frame_ms']:9.1f} {row['springs_tick_ms']:8.2f} {row['springs_tick_p95_ms']:6.2f} {row['hello_first_tick_ms']:11.2f} {row['copy_ms']:6.2f} {row['memory_mb']:6.1f}")
    print(json.dumps(out))


if __name__ == "__main__":
    main()
