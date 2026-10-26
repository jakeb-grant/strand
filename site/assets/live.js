// Live examples (docs/live-examples.md): Strand's compiler and CPU
// renderer, built to WebAssembly, running an example's scenario in the
// page. site.js loads this file only when a reader asks for a live
// example (or opens the playground), and only on a site that has the
// WebAssembly build (wasm/manifest.js names it) served over http; the
// frame-by-frame player stays the figure's default and its fallback.
//
// A run is the example's own scenario: its config, its mock services,
// its screen and its steps, played at the page's frame rate on Strand's
// virtual clock (one tick each 1/hz s, at most four a frame behind). The
// reader can press each step that acts, replay the steps, point and
// click, and edit the config; an edit runs again, or shows the
// compiler's diagnostics as `strand check` prints them.
(function () {
  "use strict";

  let lib = null;

  /** The module, its fonts and its icons, loaded once per page. */
  function boot(wasm, root) {
    if (!lib) {
      lib = (async () => {
        const t0 = performance.now();
        const base = new URL("wasm/", root);
        const m = await import(new URL(wasm.glue, base).href);
        const exports = await m.default({ module_or_path: new URL(wasm.module, base) });
        const t1 = performance.now();
        const get = async (name) => {
          const r = await fetch(new URL(name, base));
          if (!r.ok) throw new Error(`${name}: ${r.status}`);
          return r;
        };
        for (const f of wasm.fonts) m.add_font(new Uint8Array(await (await get(f)).arrayBuffer()));
        const icons = await (await get(wasm.icons)).json();
        for (const [path, b64] of Object.entries(icons)) {
          const bin = atob(b64);
          const bytes = new Uint8Array(bin.length);
          for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
          m.add_icon(path, bytes);
        }
        m.icons_ready();
        return { m, memory: exports.memory, timing: { module: t1 - t0, assets: performance.now() - t1 } };
      })();
      lib.catch(() => {
        lib = null;
      });
    }
    return lib;
  }

  function dark() {
    const t = document.documentElement.dataset.theme;
    if (t) return t === "dark";
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  }

  const reducedQuery = window.matchMedia("(prefers-reduced-motion: reduce)");

  function el(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  }

  function button(cls, text, label) {
    const b = el("button", cls, text);
    b.type = "button";
    if (label) b.setAttribute("aria-label", label);
    return b;
  }

  /** A step's label, cut as the curve's are (26 characters). */
  function short(text) {
    return text.length > 26 ? text.slice(0, 25) + "…" : text;
  }

  // ---- A light editor: a textarea over the same text, highlighted as
  // the site highlights Strand (its hl- classes), by a small scanner
  // rather than the grammar the build uses.
  const KEYWORDS = new Set(
    ("action after await bar change component dbus else enter enum every exit export extends fn for from if in key " +
      "keyframes let listen lock match on osd override palette panel permit persist play poll rw service session set " +
      "state system tokens type use when while window").split(" "),
  );
  const BOOLS = new Set(["true", "false", "null"]);

  function esc(t) {
    return t.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
  }

  function highlight(src) {
    const re =
      /(\/\/[^\n]*)|("(?:[^"\\\n]|\\.)*"?)|(\$[A-Za-z_][\w.-]*)|(\b\d+(?:\.\d+)?(?:ms|s|px|ch|%)?\b)|([A-Za-z_][\w-]*)(\s*:(?!:))?|([\s\S])/g;
    let out = "";
    let m;
    while ((m = re.exec(src))) {
      if (m[1]) out += `<span class="hl-comment">${esc(m[1])}</span>`;
      else if (m[2]) out += `<span class="hl-string">${esc(m[2])}</span>`;
      else if (m[3]) out += `<span class="hl-property">${esc(m[3])}</span>`;
      else if (m[4]) out += `<span class="hl-number">${esc(m[4])}</span>`;
      else if (m[5]) {
        const w = m[5];
        let cls = "";
        if (m[6]) cls = "hl-property";
        else if (KEYWORDS.has(w)) cls = "hl-keyword";
        else if (BOOLS.has(w)) cls = "hl-boolean";
        else if (/^[A-Z]/.test(w)) cls = "hl-type";
        out += cls ? `<span class="${cls}">${esc(w)}</span>` : esc(w);
        if (m[6]) out += esc(m[6]);
      } else out += esc(m[7]);
    }
    // A trailing newline keeps the last line's height.
    return out + "\n";
  }

  function editor(name, text, onEdit) {
    const box = el("div", "code lv-file");
    const head = el("div", "code-head");
    head.append(el("span", "code-file", name));
    const wrap = el("div", "lv-editor");
    const pre = el("pre", "lv-hl");
    pre.setAttribute("aria-hidden", "true");
    const code = el("code", "language-strand");
    pre.append(code);
    const area = el("textarea", "lv-code");
    area.spellcheck = false;
    area.setAttribute("autocapitalize", "off");
    area.setAttribute("autocomplete", "off");
    area.setAttribute("aria-label", `${name}, editable: the example runs again as you type`);
    area.wrap = "off";
    const paint = () => {
      code.innerHTML = highlight(area.value);
      area.rows = Math.max(3, area.value.split("\n").length);
    };
    area.addEventListener("input", () => {
      paint();
      onEdit();
    });
    area.addEventListener("scroll", () => {
      pre.scrollLeft = area.scrollLeft;
      pre.scrollTop = area.scrollTop;
    });
    area.value = text;
    paint();
    wrap.append(pre, area);
    box.append(head, wrap);
    return {
      box,
      head,
      get value() {
        return area.value;
      },
      set value(v) {
        area.value = v;
        paint();
      },
      area,
    };
  }

  /**
   * One example running live: a canvas where its frames were, its steps
   * as buttons, its config in editors, and what went wrong under them.
   * `parts` places them: `stage` (the canvas's box), `controls`, `edit`.
   */
  class Run {
    constructor(lib, data, parts) {
      this.lib = lib;
      this.data = data;
      this.parts = parts;
      this.live = null;
      this.raf = 0;
      this.playing = false;
      this.rect = null;
      this.drawn = -1;
      this.cost = { ticks: 0, tickMs: 0, frames: 0, frameMs: 0 };
      this.steps = JSON.parse(lib.m.parse_steps(data.steps));

      this.canvas = el("canvas", "lv-canvas");
      this.canvas.setAttribute("role", "img");
      this.canvas.setAttribute("aria-label", `The ${data.name} example, running live`);
      parts.stage.append(this.canvas);

      const c = parts.controls;
      this.replay = button("lv-replay", "Replay", "Replay the example's steps");
      this.replay.addEventListener("click", () => this.start());
      c.append(this.replay);
      this.acts = [];
      this.steps.forEach((s, i) => {
        if (!s.acts) return;
        const b = button("lv-act", short(s.text));
        b.title = s.text;
        b.addEventListener("click", () => this.act(i, b));
        c.append(b);
        this.acts.push(b);
      });
      this.status = el("span", "lv-status");
      this.status.setAttribute("aria-live", "polite");
      c.append(this.status);

      this.editors = data.files.map(([name, text]) =>
        editor(name, text, () => {
          clearTimeout(this.edited);
          this.edited = setTimeout(() => this.start(), 350);
        }),
      );
      this.reset = button("lv-reset", "Reset", "Put the example's config back");
      this.reset.addEventListener("click", () => {
        this.editors.forEach((e, i) => (e.value = data.files[i][1]));
        this.start();
      });
      if (this.editors[0]) this.editors[0].head.append(this.reset);
      this.out = el("pre", "lv-out");
      this.out.hidden = true;
      this.outBox = el("div", "check-out lv-out-box");
      this.outBox.append(this.out);
      this.outBox.hidden = true;
      parts.edit.append(...this.editors.map((e) => e.box), this.outBox);

      const at = (e) => {
        const r = this.canvas.getBoundingClientRect();
        const s = this.live.scale();
        const [vx, vy, vw, vh] = this.rect || [0, 0, 1, 1];
        return [(vx + ((e.clientX - r.left) / r.width) * vw) / s, (vy + ((e.clientY - r.top) / r.height) * vh) / s];
      };
      const input = (f) => (e) => {
        if (!this.live) return;
        try {
          f(e);
        } catch (err) {
          this.fail(String(err));
        }
        this.wake(2);
      };
      this.canvas.addEventListener("pointermove", input((e) => this.live.pointer_move(...at(e))));
      this.canvas.addEventListener("pointerdown", input((e) => {
        this.live.pointer_move(...at(e));
        this.live.pointer_button(true);
      }));
      this.canvas.addEventListener("pointerup", input(() => this.live.pointer_button(false)));
      this.canvas.addEventListener("pointerleave", input(() => this.live.pointer_leave()));
      this.onTheme = () => {
        if (!this.live) return;
        try {
          this.live.set("system.dark", String(dark()));
        } catch (e) {
          return;
        }
        this.wake(2);
      };
      this.themeWatch = new MutationObserver(this.onTheme);
      this.themeWatch.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
      window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", this.onTheme);
    }

    /** The reader's files when they differ from the example's, else none. */
    files() {
      const now = this.editors.map((e, i) => [this.data.files[i][0], e.value]);
      const same = now.every((f, i) => f[1] === this.data.files[i][1]);
      return same ? "" : JSON.stringify(now);
    }

    /** Boots the run and plays its steps from the beginning. */
    start() {
      clearTimeout(this.edited);
      cancelAnimationFrame(this.raf);
      this.raf = 0;
      let live;
      const t0 = performance.now();
      try {
        live = this.lib.m.Live.boot(JSON.stringify(this.data), this.files(), dark(), reducedQuery.matches);
      } catch (e) {
        this.fail(String(e));
        return false;
      }
      if (this.live) this.live.free();
      this.live = live;
      this.loadMs = performance.now() - t0;
      this.starts = (this.starts || 0) + 1;
      this.drawn = -1;
      this.owed = 0;
      this.playing = true;
      this.clock = null;
      this.parts.root.classList.remove("lv-failed");
      this.note([live.warnings(), reducedQuery.matches ? "Reduced motion is on, as on your desktop: motion snaps." : ""]);
      this.wake();
      return true;
    }

    note(lines) {
      const text = lines.filter(Boolean).join("\n");
      this.out.textContent = text;
      this.out.hidden = this.outBox.hidden = !text;
    }

    fail(text) {
      cancelAnimationFrame(this.raf);
      this.raf = 0;
      this.parts.root.classList.add("lv-failed");
      this.out.textContent = text;
      this.out.hidden = this.outBox.hidden = false;
      this.status.textContent = this.live ? "The last config that ran is shown" : "";
    }

    act(index, b) {
      if (!this.live) return;
      this.playing = false;
      try {
        this.live.act(index);
      } catch (e) {
        this.fail(String(e));
        return;
      }
      this.acts.forEach((a) => a.classList.toggle("lv-done", a === b));
      this.wake(2);
    }

    /**
     * Asks for a frame; `owe` ticks are run even if the shell looks at
     * rest (a value just set or a pointer just moved reaches logic at the
     * next tick, which is what wakes it).
     */
    wake(owe = 0) {
      this.owed = Math.max(this.owed || 0, owe);
      if (!this.raf && this.live) this.raf = requestAnimationFrame((ts) => this.frame(ts));
    }

    frame(ts) {
      this.raf = 0;
      const live = this.live;
      if (!live) return;
      const period = 1000 / this.data.hz;
      // The ticks due by this frame, at most four behind (a tab in the
      // background does not run the clock fast when it returns); the
      // clock starts half a tick early so a screen at the same rate runs
      // one tick a frame whatever its jitter.
      if (this.clock === null) this.clock = { start: ts - period * 1.5, done: 0 };
      const due = Math.floor((ts - this.clock.start) / period);
      const n = Math.min(4, due - this.clock.done);
      this.clock.done = Math.max(this.clock.done, due - 4) + Math.max(0, n);
      try {
        const t0 = performance.now();
        for (let i = 0; i < n; i++) {
          if (this.playing) {
            const at = live.advance();
            if (at < 0) {
              this.playing = false;
              if (!live.at_rest()) live.tick();
            } else this.mark(at);
          } else if (this.owed > 0 || !live.at_rest()) {
            live.tick();
            if (this.owed > 0) this.owed -= 1;
          }
        }
        const t1 = performance.now();
        if (n > 0) {
          this.cost.ticks += n;
          this.cost.tickMs += t1 - t0;
        }
        if (live.painted() !== this.drawn) {
          this.draw();
          this.cost.frames += 1;
          this.cost.frameMs += performance.now() - t1;
        }
      } catch (e) {
        this.fail(String(e));
        return;
      }
      const notes = live.notes();
      if (notes) this.note([live.warnings(), notes]);
      if (this.playing || this.owed > 0 || !live.at_rest()) this.wake();
      else this.status.textContent = `${(this.cost.tickMs / Math.max(1, this.cost.ticks)).toFixed(2)} ms a tick`;
    }

    /** Shows which step is playing. */
    mark(at) {
      const s = this.steps[at];
      if (s) this.status.textContent = s.text.length > 40 ? s.text.slice(0, 39) + "…" : s.text;
    }

    draw() {
      const live = this.live;
      this.drawn = live.painted();
      if (this.firstDrawn === undefined) this.firstDrawn = performance.now();
      const v = live.view();
      if (v.length === 4) this.rect = Array.from(v);
      const cx = this.canvas.getContext("2d");
      if (!this.rect) return;
      const [x, y, w, h] = this.rect;
      if (this.canvas.width !== w || this.canvas.height !== h) {
        this.canvas.width = w;
        this.canvas.height = h;
        this.parts.fit(w, h, live.scale());
      }
      if (v.length !== 4) {
        cx.clearRect(0, 0, w, h);
        return;
      }
      live.compose(x, y, w, h);
      // A view on the module's memory: the frame is not copied out.
      const px = new Uint8ClampedArray(this.lib.memory.buffer, live.frame_ptr(), live.frame_len());
      cx.putImageData(new ImageData(px, w, h), 0, 0);
    }

    /**
     * (the browser check) The example run headless in `mode`, its steps
     * to their end: each frame painted, as `[surface, t_ms, w, h, hash]`.
     */
    check(isDark) {
      const live = this.lib.m.Live.boot(JSON.stringify(this.data), "", isDark, false);
      live.record(true);
      const t0 = performance.now();
      let ticks = 0;
      while (live.advance() >= 0) ticks++;
      const ms = performance.now() - t0;
      const paints = JSON.parse(live.paints());
      live.free();
      return { paints, ticks, ms };
    }

    stop() {
      cancelAnimationFrame(this.raf);
      clearTimeout(this.edited);
      this.raf = 0;
      this.themeWatch.disconnect();
      window.matchMedia("(prefers-color-scheme: dark)").removeEventListener("change", this.onTheme);
      if (this.live) this.live.free();
      this.live = null;
    }
  }

  /** A figure's live mode, in place of its player. */
  async function figure(fig, wasm, root) {
    const data = JSON.parse(fig.querySelector(".ex-live-data").textContent);
    const l = await boot(wasm, root);
    const view = fig.querySelector(".ex-view");
    const stage = fig.querySelector(".ex-stage");
    const controls = el("div", "lv-controls");
    const edit = el("div", "lv-edit");
    const back = button("lv-back", "Frames", "Back to the frames Strand drew");
    const parts = {
      root: fig,
      stage,
      controls,
      edit,
      fit(w, h, scale) {
        stage.style.aspectRatio = `${w} / ${h}`;
        stage.style.maxWidth = `${(w / scale) * 1.5}px`;
      },
    };
    const was = { aspect: stage.style.aspectRatio, max: stage.style.maxWidth };
    const run = new Run(l, data, parts);
    controls.append(back);
    view.append(controls);
    fig.querySelector("figcaption").before(edit);
    fig.classList.add("is-live");
    fig.dispatchEvent(new CustomEvent("strand-live", { detail: true }));
    back.addEventListener("click", () => {
      run.stop();
      run.canvas.remove();
      controls.remove();
      edit.remove();
      stage.style.aspectRatio = was.aspect;
      stage.style.maxWidth = was.max;
      fig.classList.remove("is-live", "lv-failed");
      figures.delete(fig.id);
      fig.dispatchEvent(new CustomEvent("strand-live", { detail: false }));
    });
    figures.set(fig.id, run);
    run.start();
    return run;
  }

  /** The playground page: every example in turn, larger, to edit. */
  async function playground(box, wasm, root) {
    const all = JSON.parse(box.querySelector(".pg-data").textContent);
    const l = await boot(wasm, root);
    const pick = el("div", "pg-pick");
    pick.setAttribute("role", "group");
    pick.setAttribute("aria-label", "Example");
    const view = el("div", "ex-view pg-view");
    const stage = el("div", "ex-stage pg-stage");
    const controls = el("div", "lv-controls");
    const edit = el("div", "lv-edit pg-edit");
    view.append(stage, controls);
    const note = box.querySelector(".pg-note");
    note.textContent = "Pick an example; edit its config and it runs again.";
    box.append(pick, view, edit);
    let run = null;
    const show = (i) => {
      if (run) {
        run.stop();
        run.canvas.remove();
      }
      controls.replaceChildren();
      edit.replaceChildren();
      stage.style.aspectRatio = "";
      Array.from(pick.children).forEach((b, j) => b.setAttribute("aria-pressed", String(i === j)));
      run = new Run(l, all[i], {
        root: box,
        stage,
        controls,
        edit,
        fit(w, h, scale) {
          stage.style.aspectRatio = `${w} / ${h}`;
          stage.style.maxWidth = `${(w / scale) * 1.5}px`;
        },
      });
      figures.set("playground", run);
      run.start();
    };
    all.forEach((ex, i) => {
      const b = button("pg-choice", ex.name);
      b.addEventListener("click", () => show(i));
      pick.append(b);
    });
    show(0);
    return run;
  }

  const figures = new Map();
  window.StrandLive = { figure, playground, figures, boot: (w, r) => boot(w, r) };
})();
