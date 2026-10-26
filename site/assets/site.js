// The Strand site's script. Every page reads without it; it adds the
// theme switch, the search, copy buttons, the sidebar on narrow screens,
// the bands' entrance, the example players and, where the site has
// their WebAssembly build, the offer of each example live.
(function () {
  "use strict";
  var root = document.documentElement;
  root.classList.add("js");
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
  // The site's root, from this script's own URL (assets/site.js): it
  // works from disk as from a server.
  var siteRoot = document.currentScript ? new URL("../", document.currentScript.src) : new URL("./", location.href);

  // ---- Theme: system, light, dark, in turn. A choice that cannot be
  // stored is still made for this page.
  var themeButton = document.querySelector(".theme");
  function themeLabel() {
    var t = root.dataset.theme || "system";
    if (themeButton) themeButton.setAttribute("aria-label", "Theme: " + t);
  }
  if (themeButton) {
    themeLabel();
    themeButton.addEventListener("click", function () {
      var next = { system: "light", light: "dark", dark: "system" }[root.dataset.theme || "system"];
      if (next === "system") delete root.dataset.theme;
      else root.dataset.theme = next;
      try {
        if (next === "system") localStorage.removeItem("strand-theme");
        else localStorage.setItem("strand-theme", next);
      } catch (e) {}
      themeLabel();
    });
  }

  // ---- The sidebar on narrow screens.
  var menu = document.querySelector(".menu");
  var sidebar = document.getElementById("sidebar");
  if (menu && sidebar) {
    menu.addEventListener("click", function () {
      var open = sidebar.classList.toggle("open");
      menu.setAttribute("aria-expanded", open ? "true" : "false");
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && sidebar.classList.contains("open")) {
        sidebar.classList.remove("open");
        menu.setAttribute("aria-expanded", "false");
        menu.focus();
      }
    });
  }

  // ---- Search: the index (search-index.js, written by the build) is a
  // script loaded the first time the reader searches, since a fetch of a
  // file fails on file://. Every word typed must be found; a heading's
  // match counts most, then the page's title, then the text.
  var field = document.getElementById("search");
  var list = document.getElementById("search-results");
  if (field && list) setupSearch(field, list);

  function setupSearch(field, list) {
    var index = null;
    var state = "idle"; // idle, loading, ready, failed
    var hits = [];
    var active = -1;

    function load() {
      if (state !== "idle") return;
      state = "loading";
      var s = document.createElement("script");
      s.src = new URL("search-index.js", siteRoot).href;
      s.onload = function () {
        index = (window.STRAND_SEARCH || []).map(function (e) {
          return { e: e, p: e.p.toLowerCase(), h: e.h.toLowerCase(), x: e.x.toLowerCase() };
        });
        state = "ready";
        update();
      };
      s.onerror = function () {
        state = "failed";
        update();
      };
      document.head.appendChild(s);
    }

    function esc(t) {
      return t.replace(/[&<>"']/g, function (c) {
        return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
      });
    }

    // `text` with each word marked, escaped.
    function marked(text, words) {
      var lower = text.toLowerCase();
      var spans = [];
      words.forEach(function (w) {
        var at = 0;
        while ((at = lower.indexOf(w, at)) !== -1) {
          spans.push([at, at + w.length]);
          at += w.length;
        }
      });
      spans.sort(function (a, b) { return a[0] - b[0]; });
      var out = "";
      var pos = 0;
      spans.forEach(function (sp) {
        if (sp[0] < pos) return;
        out += esc(text.slice(pos, sp[0])) + "<mark>" + esc(text.slice(sp[0], sp[1])) + "</mark>";
        pos = sp[1];
      });
      return out + esc(text.slice(pos));
    }

    // The text around the first word found, cut at spaces.
    function snippet(e, x, words) {
      var at = -1;
      for (var i = 0; i < words.length && at < 0; i++) at = x.indexOf(words[i]);
      if (at < 0) at = 0;
      var from = Math.max(0, at - 50);
      var to = Math.min(e.x.length, at + 110);
      if (from > 0) from = e.x.indexOf(" ", from) + 1 || from;
      var sp = e.x.lastIndexOf(" ", to);
      if (to < e.x.length && sp > at) to = sp;
      return (from > 0 ? "… " : "") + marked(e.x.slice(from, to), words) + (to < e.x.length ? " …" : "");
    }

    function score(item, words) {
      var total = 0;
      for (var i = 0; i < words.length; i++) {
        var w = words[i];
        var s = 0;
        var h = item.h.indexOf(w);
        if (h === 0 || (h > 0 && /\W/.test(item.h[h - 1]))) s += 12;
        else if (h > 0) s += 8;
        if (item.p.indexOf(w) >= 0) s += 5;
        // Each time the text says it counts, up to five.
        for (var at = item.x.indexOf(w), n = 0; at >= 0 && n < 5; at = item.x.indexOf(w, at + w.length), n++) s += 1;
        if (!s) return 0;
        total += s;
      }
      // The words as typed, together.
      var phrase = words.join(" ");
      if (words.length > 1 && (item.h + " " + item.x).indexOf(phrase) >= 0) total += 6;
      // A page's own entry, then shorter headings, first.
      return total + (item.h ? 0 : 0.5) - item.h.length / 1000;
    }

    function render(q) {
      var html = "";
      if (state === "loading") html = '<p class="search-note">Loading the index…</p>';
      else if (state === "failed") html = '<p class="search-note">The search index could not be loaded.</p>';
      else if (!hits.length) html = '<p class="search-note">Nothing found for “' + esc(q) + '”.</p>';
      else {
        var words = q.toLowerCase().split(/\s+/).filter(Boolean);
        hits.forEach(function (it, i) {
          var e = it.e;
          html += '<a class="search-hit" role="option" id="search-hit-' + i + '" aria-selected="' + (i === active) +
            '" href="' + esc(new URL(e.u, siteRoot).href) + '">' +
            '<span class="search-hit-title">' + marked(e.h || e.p, words) + "</span>" +
            (e.h ? '<span class="search-hit-page">' + esc(e.p) + "</span>" : "") +
            (e.x ? '<span class="search-hit-text">' + snippet(e, it.x, words) + "</span>" : "") +
            "</a>";
        });
      }
      list.innerHTML = html;
      list.hidden = false;
      field.setAttribute("aria-expanded", "true");
      setActive(active);
    }

    function update() {
      var q = field.value.trim();
      if (!q) return close();
      load();
      hits = [];
      if (state === "ready") {
        var words = q.toLowerCase().split(/\s+/).filter(Boolean);
        var scored = [];
        index.forEach(function (it) {
          var s = score(it, words);
          if (s > 0) scored.push([s, it]);
        });
        scored.sort(function (a, b) { return b[0] - a[0]; });
        hits = scored.slice(0, 10).map(function (s) { return s[1]; });
      }
      active = hits.length ? 0 : -1;
      render(q);
    }

    function setActive(i) {
      var items = list.querySelectorAll(".search-hit");
      active = items.length ? Math.max(0, Math.min(items.length - 1, i)) : -1;
      Array.prototype.forEach.call(items, function (el, n) {
        el.setAttribute("aria-selected", n === active ? "true" : "false");
      });
      if (active >= 0) {
        field.setAttribute("aria-activedescendant", items[active].id);
        items[active].scrollIntoView({ block: "nearest" });
      } else field.removeAttribute("aria-activedescendant");
    }

    function close() {
      list.hidden = true;
      list.innerHTML = "";
      hits = [];
      active = -1;
      field.setAttribute("aria-expanded", "false");
      field.removeAttribute("aria-activedescendant");
    }

    field.addEventListener("focus", function () {
      load();
      if (field.value.trim()) update();
    });
    field.addEventListener("input", update);
    field.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        if (list.hidden) return update();
        e.preventDefault();
        setActive(active + (e.key === "ArrowDown" ? 1 : -1));
      } else if (e.key === "Enter") {
        var items = list.querySelectorAll(".search-hit");
        if (items[active]) {
          e.preventDefault();
          location.href = items[active].href;
        }
      } else if (e.key === "Escape") {
        e.stopPropagation();
        if (!list.hidden) close();
        else {
          field.value = "";
          field.blur();
        }
      }
    });
    // A press on a result keeps the field focused until the link opens.
    list.addEventListener("mousedown", function (e) { e.preventDefault(); });
    list.addEventListener("mousemove", function (e) {
      var hit = e.target.closest && e.target.closest(".search-hit");
      if (!hit) return;
      var items = Array.prototype.indexOf.call(list.querySelectorAll(".search-hit"), hit);
      if (items !== active) setActive(items);
    });
    field.addEventListener("blur", close);
    // "/" anywhere but in a field goes to the search.
    document.addEventListener("keydown", function (e) {
      if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey) return;
      var t = e.target;
      if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return;
      e.preventDefault();
      field.focus();
      field.select();
    });
  }

  // ---- Copy buttons on code blocks.
  document.querySelectorAll(".code").forEach(function (box) {
    var pre = box.querySelector("pre");
    if (!pre) return;
    var head = box.querySelector(".code-head");
    var b = document.createElement("button");
    b.type = "button";
    b.className = "copy";
    b.textContent = "Copy";
    b.addEventListener("click", function () {
      var text = pre.innerText;
      function done(label) {
        b.textContent = label;
        setTimeout(function () { b.textContent = "Copy"; }, 1400);
      }
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(function () { done("Copied"); }, function () {
          select(pre);
          done("Selected");
        });
      } else {
        select(pre);
        done("Selected");
      }
    });
    if (head) head.appendChild(b);
    else box.insertBefore(b, pre);
  });
  function select(el) {
    var r = document.createRange();
    r.selectNodeContents(el);
    var s = window.getSelection();
    s.removeAllRanges();
    s.addRange(r);
  }

  // ---- The home page's bands rise in, their parts one after another.
  if ("IntersectionObserver" in window && document.body.classList.contains("home")) {
    var parts = [];
    document.querySelectorAll(".band, .closing").forEach(function (band) {
      var i = 0;
      band.querySelectorAll("h2, .band-copy p, .figures > div, .ex-view, .code, .check-out, .compositors li, .compositors-note, .closing p")
        .forEach(function (el) {
          if (el.closest(".rise")) return;
          var box = el.getBoundingClientRect();
          if (box.top < window.innerHeight) return; // already in view: shown as is
          el.classList.add("rise");
          el.style.setProperty("--delay", Math.min(i, 6) * 75 + "ms");
          i++;
          parts.push(el);
        });
    });
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("risen");
          io.unobserve(e.target);
        }
      });
    }, { rootMargin: "0px 0px -8% 0px" });
    parts.forEach(function (el) { io.observe(el); });
  }

  // ---- Example players: the frames Strand drew, by tick.
  document.querySelectorAll(".example").forEach(function (fig) {
    var controls = fig.querySelector(".ex-controls");
    if (!controls) return;
    var t = controls.dataset.t.split(",").map(Number);
    // Each mode's moving part: a strip of every distinct frame's, over
    // the still first frame.
    var sprites = Array.prototype.map.call(fig.querySelectorAll(".ex-moving"), function (el) {
      return { el: el, count: Number(el.dataset.count), ticks: el.dataset.ticks.split(",").map(Number) };
    });
    var scrub = fig.querySelector(".ex-scrub");
    var out = fig.querySelector(".ex-time");
    var play = fig.querySelector(".ex-play");
    var cursor = fig.querySelector(".ex-cursor");
    var k = 0;
    var playing = false;
    var started = 0;
    var raf = 0;
    var tEnd = t[t.length - 1] || 1;

    function show(next) {
      k = Math.max(0, Math.min(t.length - 1, next));
      sprites.forEach(function (s) {
        // Background positions in % place the strip so frame i shows.
        var i = s.ticks[k] || 0;
        var y = s.count > 1 ? (100 * i) / (s.count - 1) : 0;
        s.el.style.backgroundPosition = "0 " + y + "%";
      });
      scrub.value = String(k);
      out.textContent = "frame " + k + " · " + Math.round(t[k]) + " ms";
      if (cursor) {
        var x0 = Number(cursor.dataset.x0), x1 = Number(cursor.dataset.x1);
        var x = x0 + (x1 - x0) * t[k] / tEnd;
        cursor.setAttribute("x1", x.toFixed(1));
        cursor.setAttribute("x2", x.toFixed(1));
      }
    }
    function frame(now) {
      if (!playing) return;
      // Real time: the tick whose time has come, held a moment at the end.
      var ms = now - started;
      var hold = 900;
      if (ms > tEnd + hold) {
        started = now;
        ms = 0;
      }
      var next = k;
      if (ms <= tEnd) {
        next = 0;
        while (next + 1 < t.length && t[next + 1] <= ms) next++;
      }
      if (next !== k) show(next);
      raf = requestAnimationFrame(frame);
    }
    function setPlaying(on) {
      playing = on;
      play.setAttribute("aria-pressed", on ? "true" : "false");
      play.setAttribute("aria-label", on ? "Pause" : "Play");
      cancelAnimationFrame(raf);
      if (on) {
        started = performance.now() - t[k];
        raf = requestAnimationFrame(frame);
      }
    }
    play.addEventListener("click", function () {
      if (!playing) begin();
      if (!playing && k >= t.length - 1) show(0);
      setPlaying(!playing);
    });
    fig.querySelectorAll(".ex-step").forEach(function (b) {
      b.addEventListener("click", function () {
        fresh = false;
        setPlaying(false);
        show(k + Number(b.dataset.step));
      });
    });
    scrub.addEventListener("input", function () {
      fresh = false;
      setPlaying(false);
      show(Number(scrub.value));
    });
    // Before it plays, the scene as its first step left it; playing
    // starts from the beginning.
    var fresh = true;
    show(Number(controls.dataset.poster || 0));
    function begin() {
      if (fresh) {
        fresh = false;
        show(0);
      }
    }
    // Live mode (assets/live.js) takes the figure's place while it runs.
    fig.addEventListener("strand-live", function (e) {
      if (e.detail) {
        fig.dataset.touched = "1";
        setPlaying(false);
      }
    });
    // It plays while it is on screen, unless motion is reduced.
    if (!reduced.matches && "IntersectionObserver" in window) {
      var seen = new IntersectionObserver(function (entries) {
        entries.forEach(function (e) {
          if (e.isIntersecting && !playing && !fig.dataset.touched) {
            begin();
            setPlaying(true);
          }
          if (!e.isIntersecting && playing) setPlaying(false);
        });
      }, { threshold: 0.4 });
      seen.observe(fig);
      [scrub, play].concat(Array.prototype.slice.call(fig.querySelectorAll(".ex-step"))).forEach(function (c) {
        c.addEventListener("pointerdown", function () { fig.dataset.touched = "1"; });
        c.addEventListener("keydown", function () { fig.dataset.touched = "1"; });
      });
    }
  });

  // ---- Live examples (docs/live-examples.md). Offered only where they
  // can run: the site has their WebAssembly build (wasm/manifest.js
  // names it), it is served over http (a browser loads no WebAssembly
  // from a file) and the browser runs WebAssembly. Nothing is fetched
  // until the reader asks; the player stays the default and the
  // fallback.
  var wasm = window.STRAND_WASM;
  var canLive = !!wasm && /^https?:$/.test(location.protocol) && typeof WebAssembly === "object";
  var liveScript = null;
  function loadLive() {
    if (!liveScript) {
      liveScript = new Promise(function (resolve, reject) {
        var s = document.createElement("script");
        s.src = new URL("assets/live.js", siteRoot).href;
        s.onload = function () { resolve(window.StrandLive); };
        s.onerror = function () { reject(new Error("assets/live.js did not load")); };
        document.head.appendChild(s);
      });
      liveScript.catch(function () { liveScript = null; });
    }
    return liveScript;
  }
  function megabytes(n) {
    return (n / 1e6).toFixed(1) + " MB";
  }
  if (canLive) {
    document.querySelectorAll(".example").forEach(function (fig) {
      if (!fig.querySelector(".ex-live-data")) return;
      var offer = document.createElement("div");
      offer.className = "ex-live-offer";
      var b = document.createElement("button");
      b.type = "button";
      b.className = "ex-live-try";
      b.textContent = "Try it live";
      var note = document.createElement("span");
      note.className = "ex-live-note";
      note.textContent = "Runs Strand in your browser; edit the config and it runs again" +
        (wasm.download ? " (" + megabytes(wasm.download) + ", once)" : "");
      offer.appendChild(b);
      offer.appendChild(note);
      fig.querySelector(".ex-view").appendChild(offer);
      b.addEventListener("click", function () {
        b.disabled = true;
        b.textContent = "Loading…";
        loadLive()
          .then(function (L) { return L.figure(fig, wasm, siteRoot); })
          .then(function () {
            b.disabled = false;
            b.textContent = "Try it live";
          })
          .catch(function (e) {
            b.disabled = false;
            b.textContent = "Try it live";
            note.textContent = "The live example did not load (" + (e && e.message ? e.message : e) + "); the frames above are Strand's own.";
          });
      });
    });
  }
  var pg = document.querySelector(".playground");
  if (pg) {
    var pgNote = pg.querySelector(".pg-note");
    if (!wasm) pgNote.textContent = "This copy of the site was built without the playground's WebAssembly build (scripts/wasm/site.sh adds it).";
    else if (!/^https?:$/.test(location.protocol)) pgNote.textContent = "The playground needs the site served over http: a browser loads no WebAssembly from a file.";
    else if (!canLive) pgNote.textContent = "The playground needs a browser that runs WebAssembly.";
    else {
      pgNote.textContent = "Loading the playground" + (wasm.download ? " (" + megabytes(wasm.download) + ")" : "") + "…";
      loadLive()
        .then(function (L) { return L.playground(pg, wasm, siteRoot); })
        .catch(function (e) { pgNote.textContent = "The playground did not load: " + (e && e.message ? e.message : e) + "."; });
    }
  }
})();
