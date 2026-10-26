// The Strand site's script. Every page reads without it; it adds the
// theme switch, copy buttons, the sidebar on narrow screens, the bands'
// entrance and the example players.
(function () {
  "use strict";
  var root = document.documentElement;
  root.classList.add("js");
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)");

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
    var sprites = Array.prototype.map.call(fig.querySelectorAll(".ex-sprite"), function (el) {
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
      if (!playing && k >= t.length - 1) show(0);
      setPlaying(!playing);
    });
    fig.querySelectorAll(".ex-step").forEach(function (b) {
      b.addEventListener("click", function () {
        setPlaying(false);
        show(k + Number(b.dataset.step));
      });
    });
    scrub.addEventListener("input", function () {
      setPlaying(false);
      show(Number(scrub.value));
    });
    show(0);
    // It plays while it is on screen, unless motion is reduced.
    if (!reduced.matches && "IntersectionObserver" in window) {
      var seen = new IntersectionObserver(function (entries) {
        entries.forEach(function (e) {
          if (e.isIntersecting && !playing && !fig.dataset.touched) setPlaying(true);
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
})();
