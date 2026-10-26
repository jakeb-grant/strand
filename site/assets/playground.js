// The playground's live examples (docs/wasm-spike.md): Strand's compiler
// and CPU renderer, built to WebAssembly by scripts/wasm/build.sh into
// assets/wasm/, run in the page. Each `.pg[data-example]` gets the
// example's config in a text box; Run compiles it and shows the bar it
// draws, or the compiler's errors. The clock is virtual: one tick per
// 1/60 s of the page's animation frames, so motion runs at its real
// speed and stops when nothing moves.
//
// A classic script, not a module, so a page opened from file:// (where
// a browser loads no module and no WebAssembly) says why instead of
// failing; the wasm-bindgen glue is imported only over http.
(function () {
  "use strict";
  const BASE = new URL("./wasm/", document.currentScript.src);
  const FONTS = ["LiberationSans-Regular.ttf", "LiberationSans-Bold.ttf"];
  let booted = null;

  /** Loads the module, its fonts and the examples once. */
  function boot() {
    if (!booted) {
      booted = (async () => {
        const t0 = performance.now();
        const m = await import(new URL("strand_web.js", BASE).href);
        await m.default({ module_or_path: new URL("strand_web_bg.wasm", BASE) });
        const t1 = performance.now();
        for (const f of FONTS) {
          const r = await fetch(new URL(f, BASE));
          if (!r.ok) throw new Error(`${f}: ${r.status}`);
          m.add_font(new Uint8Array(await r.arrayBuffer()));
        }
        const r = await fetch(new URL("examples.json", BASE));
        if (!r.ok) throw new Error(`examples.json: ${r.status}`);
        const examples = await r.json();
        return { m, examples, timing: { module: t1 - t0, fonts: performance.now() - t1 } };
      })();
    }
    return booted;
  }

  function dark() {
    const t = document.documentElement.dataset.theme;
    if (t) return t === "dark";
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  }

  function el(tag, attrs, ...kids) {
    const e = document.createElement(tag);
    Object.assign(e, attrs || {});
    for (const k of kids) e.append(k);
    return e;
  }

  /** The `set <path> <value>` steps of an example: what its buttons flip. */
  function toggles(ex) {
    const seen = new Map();
    for (const s of ex.steps) {
      const m = /^set\s+(\S+)\s+(true|false)$/.exec(s);
      if (m && !seen.has(m[1])) seen.set(m[1], true);
    }
    return [...seen.keys()];
  }

  class Example {
    constructor(root, lib, ex) {
      this.root = root;
      this.lib = lib;
      this.ex = ex;
      this.shell = null;
      this.rows = 0;
      this.drawn = -1;
      this.raf = 0;
      this.cost = { ticks: 0, tickMs: 0, frames: 0, frameMs: 0 };

      this.canvas = el("canvas", { className: "pg-canvas" });
      this.stage = el("div", { className: "pg-stage" }, this.canvas);
      this.run = el("button", { type: "button", className: "pg-run", textContent: "Run" });
      const controls = el("div", { className: "pg-controls" }, this.run);
      this.flips = toggles(ex).map((path) => {
        const b = el("button", { type: "button", className: "pg-flip", textContent: `${path}: false` });
        b.dataset.path = path;
        b.addEventListener("click", () => this.flip(path, b));
        controls.append(b);
        return b;
      });
      this.status = el("span", { className: "pg-status" });
      controls.append(this.status);
      this.code = el("textarea", {
        className: "pg-code",
        spellcheck: false,
        value: ex.config,
        rows: Math.min(28, ex.config.split("\n").length + 1),
      });
      this.code.setAttribute("aria-label", "The example's config, shell.strand");
      this.services = el("textarea", {
        className: "pg-code pg-services",
        spellcheck: false,
        value: JSON.stringify(ex.services, null, 1),
        rows: 4,
      });
      this.services.setAttribute("aria-label", "Mock service values, JSON");
      const svc = el("details", { className: "pg-more" }, el("summary", { textContent: "Service values" }), this.services);
      this.out = el("pre", { className: "pg-out" });
      const file = el("div", { className: "pg-file", textContent: "shell.strand" });
      root.replaceChildren(this.stage, controls, el("div", { className: "pg-edit" }, file, this.code), svc, this.out);

      this.run.addEventListener("click", () => this.load());
      this.code.addEventListener("keydown", (e) => {
        if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
          e.preventDefault();
          this.load();
        }
      });
      const at = (e) => {
        const s = this.ex.screen;
        const r = this.canvas.getBoundingClientRect();
        return [((e.clientX - r.left) * s.width) / r.width, ((e.clientY - r.top) * s.width) / r.width];
      };
      const input = (f) => (e) => {
        if (!this.shell) return;
        try {
          f(e);
        } catch (err) {
          this.fail(String(err));
        }
        this.wake();
      };
      this.canvas.addEventListener("pointermove", input((e) => this.shell.pointer_move(...at(e))));
      this.canvas.addEventListener("pointerdown", input((e) => {
        this.shell.pointer_move(...at(e));
        this.shell.pointer_button(true);
      }));
      this.canvas.addEventListener("pointerup", input(() => this.shell.pointer_button(false)));
      this.canvas.addEventListener("pointerleave", input(() => this.shell.pointer_leave()));
      this.load();
    }

    load() {
      cancelAnimationFrame(this.raf);
      this.raf = 0;
      if (this.shell) this.shell.free();
      this.shell = null;
      this.rows = 0;
      this.drawn = -1;
      let services;
      try {
        services = JSON.parse(this.services.value || "{}");
      } catch (e) {
        this.fail(`Service values: ${e.message}`);
        return;
      }
      services["system.dark"] = dark();
      const s = this.ex.screen;
      const t0 = performance.now();
      try {
        this.shell = this.lib.m.WebShell.load(
          JSON.stringify({ "shell.strand": this.code.value }),
          JSON.stringify(services),
          s.width,
          s.height,
          s.scale,
          60,
        );
      } catch (e) {
        this.fail(String(e));
        return;
      }
      this.loadMs = performance.now() - t0;
      for (const b of this.flips) b.textContent = `${b.dataset.path}: false`;
      this.root.classList.remove("pg-failed");
      this.out.textContent = this.shell.warnings();
      this.start = null;
      this.wake();
    }

    fail(text) {
      this.root.classList.add("pg-failed");
      this.out.textContent = text;
      this.status.textContent = "";
    }

    flip(path, b) {
      if (!this.shell) return;
      try {
        const now = this.shell.get(path) === "true";
        this.shell.set(path, now ? "false" : "true");
        b.textContent = `${path}: ${!now}`;
      } catch (e) {
        this.fail(String(e));
        return;
      }
      this.wake();
    }

    setDark() {
      if (!this.shell) return;
      try {
        this.shell.set("system.dark", String(dark()));
      } catch (e) {
        return;
      }
      this.wake();
    }

    wake() {
      if (!this.raf && this.shell) this.raf = requestAnimationFrame((ts) => this.frame(ts));
    }

    frame(ts) {
      this.raf = 0;
      const sh = this.shell;
      if (!sh) return;
      // The ticks due by this animation frame, at most 4 behind (a tab in
      // the background does not run the clock fast when it comes back);
      // the clock starts half a tick early, so a 60 Hz screen runs one
      // tick a frame whatever its jitter.
      if (this.start === null) {
        this.start = ts - 1500 / 60;
        this.done = 0;
      }
      const due = Math.floor((ts - this.start) / (1000 / 60));
      const n = Math.min(4, due - this.done);
      this.done = Math.max(this.done, due - 4) + Math.max(0, n);
      if (n <= 0) {
        this.wake();
        return;
      }
      try {
        const t0 = performance.now();
        for (let i = 0; i < n; i++) sh.tick();
        const t1 = performance.now();
        this.cost.ticks += n;
        this.cost.tickMs += t1 - t0;
        if (sh.painted() !== this.drawn) {
          this.draw();
          this.cost.frames += 1;
          this.cost.frameMs += performance.now() - t1;
        }
      } catch (e) {
        this.fail(String(e));
        return;
      }
      const notes = sh.notes();
      if (notes) this.out.textContent = [sh.warnings(), notes].filter(Boolean).join("\n");
      this.status.textContent = `tick ${sh.k()} · ${(this.cost.tickMs / Math.max(1, this.cost.ticks)).toFixed(2)} ms a tick`;
      if (!sh.at_rest()) this.wake();
    }

    draw() {
      const sh = this.shell;
      this.drawn = sh.painted();
      if (this.firstDrawn === undefined) this.firstDrawn = performance.now();
      const w = sh.frame_width();
      this.rows = Math.max(this.rows, Math.min(sh.frame_height(), sh.content_rows()));
      if (this.rows === 0) return;
      const px = sh.frame_top(this.rows);
      if (this.canvas.width !== w || this.canvas.height !== this.rows) {
        this.canvas.width = w;
        this.canvas.height = this.rows;
        this.canvas.style.aspectRatio = `${w} / ${this.rows}`;
        this.canvas.style.maxWidth = `${this.ex.screen.width}px`;
      }
      this.canvas.getContext("2d").putImageData(new ImageData(new Uint8ClampedArray(px.buffer, px.byteOffset, px.length), w, this.rows), 0, 0);
    }
  }

  async function main() {
    const roots = document.querySelectorAll(".pg[data-example]");
    if (!roots.length) return;
    if (location.protocol === "file:") {
      for (const r of roots) r.textContent = "The live examples need the site served over http (a browser does not load WebAssembly from a file).";
      return;
    }
    let lib;
    try {
      lib = await boot();
    } catch (e) {
      for (const r of roots) r.textContent = `The live examples did not load: ${e.message}. Build them with scripts/wasm/build.sh.`;
      return;
    }
    const all = [];
    for (const r of roots) {
      const ex = lib.examples[r.dataset.example];
      if (!ex) {
        r.textContent = `No example ${r.dataset.example}.`;
        continue;
      }
      all.push(new Example(r, lib, ex));
    }
    new MutationObserver(() => all.forEach((e) => e.setDark())).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-theme"],
    });
    window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => all.forEach((e) => e.setDark()));
    window.strandPlayground = { lib, examples: all };
  }

  main();
})();
