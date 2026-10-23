"""The viewer in a real browser (docs/eval.md, "The viewer in a browser").

Run by scripts/eval/viewer-check.sh inside the pinned Playwright image,
with each report mounted read-only. It loads every page from file://
with no network, drives each mode of the viewer (scrubber, filmstrip,
overlays, onion skin, curves, cost lane, A/B against the reference, the
Approve tab, the theme, a phone's width) and of the matrix grid, saves a
screenshot of each, and fails on any console error, uncaught exception,
failed request or failed expectation.

Usage: python3 -I viewer_check.py --shots DIR [--expect KINDS] PAGE...
       PAGE: an index.html (a strand-eval report or a matrix grid).
       KINDS: comma-separated of pass, fail, changed, matrix, replay: each must be
       exercised by some page, so a run on the wrong reports cannot pass
       by finding nothing to check.
"""

import argparse
import hashlib
import os
import re
import sys
import time

from playwright.sync_api import sync_playwright

DESKTOP = {"width": 1280, "height": 900}
PHONE = {"width": 360, "height": 780}

# Records what the page copies (the Approve tab's command) without
# needing a real clipboard.
COPY_HOOK = """
window.__copied = null;
(function () {
  var c = navigator.clipboard;
  var w = function (t) { window.__copied = String(t); return Promise.resolve(); };
  if (c) { try { c.writeText = w; } catch (e) {} }
  else { try { Object.defineProperty(navigator, 'clipboard', { value: { writeText: w } }); } catch (e) {} }
})();
"""

# The toolbar control whose label starts with NAME.
FIELD_JS = """
(name) => {
  var ls = document.querySelectorAll('#toolbar label.field');
  for (var i = 0; i < ls.length; i++) {
    var t = ls[i].textContent.trim();
    if (t === name || t.indexOf(name + ' ') === 0) {
      var c = ls[i].querySelector('select, input');
      if (c) { c.setAttribute('data-vc', name); return true; }
    }
  }
  return false;
}
"""


class Fail(Exception):
    pass


class Checker:
    def __init__(self, shots):
        self.shots = shots
        self.problems = []
        self.kinds = set()
        self.saved = []

    def problem(self, where, what):
        self.problems.append(f"{where}: {what}")
        print(f"  FAIL {where}: {what}", flush=True)

    def expect(self, cond, where, what):
        if not cond:
            self.problem(where, what)
        return cond

    def shot(self, page, name, full=False):
        path = os.path.join(self.shots, re.sub(r"[^A-Za-z0-9_.@+-]", "_", name) + ".png")
        page.screenshot(path=path, full_page=full)
        self.saved.append(path)


def watch(page, chk, where):
    """Console errors, uncaught exceptions and requests that fail."""

    def on_console(m):
        if m.type == "error":
            chk.problem(where(), f"console error: {m.text}")

    page.on("console", on_console)
    page.on("pageerror", lambda e: chk.problem(where(), f"uncaught exception: {e}"))
    page.on("requestfailed", lambda r: chk.problem(where(), f"request failed: {r.url} ({r.failure})"))

    def on_request(r):
        if not (r.url.startswith("file:") or r.url.startswith("data:") or r.url.startswith("blob:")):
            chk.problem(where(), f"loads from elsewhere: {r.url}")

    page.on("request", on_request)


def settle(page, ms=120):
    # Images decode asynchronously and redraw on load.
    page.wait_for_timeout(ms)


def label(page):
    return page.locator("#frame-label").inner_text()


def frame_no(page):
    # A replay's label starts with the time, then the frame.
    m = re.search(r"frame (\d+)/(\d+)", label(page))
    if not m:
        raise Fail(f"frame label reads {label(page)!r}")
    return int(m.group(1)), int(m.group(2))


def stage_hash(page):
    data = page.evaluate(
        "() => Array.from(document.querySelectorAll('#stage canvas')).map(c => c.toDataURL()).join('|')"
    )
    return hashlib.sha256(data.encode()).hexdigest()


def widest_canvas(page):
    return page.evaluate("() => Math.max(0, ...Array.from(document.querySelectorAll('#stage canvas')).map(c => c.width))")


def fits(page):
    """Each stage canvas fits the stage's width (the fitted zoom)."""
    return page.evaluate(
        "() => { var st = document.getElementById('stage'); var w = st.clientWidth;"
        " return Array.from(st.querySelectorAll('canvas')).every(c => c.getBoundingClientRect().width <= w); }"
    )


def field(page, name):
    if not page.evaluate(FIELD_JS, name):
        raise Fail(f"no toolbar control {name!r}")
    return page.locator(f'[data-vc="{name}"]')


def set_check(page, name, on):
    c = field(page, name)
    if c.is_checked() != on:
        c.click()
    settle(page, 40)


def set_select(page, name, value):
    field(page, name).select_option(value)
    settle(page, 60)


def no_page_scroll(page, chk, where):
    w = page.evaluate("() => [document.documentElement.scrollWidth, window.innerWidth]")
    chk.expect(w[0] <= w[1], where, f"the page scrolls sideways ({w[0]} px wide in {w[1]})")


def data_of(page):
    return page.evaluate("() => JSON.parse(document.getElementById('run').textContent)")


def quote(s):
    return "'" + s.replace("'", "'\\''") + "'"


def is_replay(page):
    return page.evaluate("() => document.body.classList.contains('replay')")


def pick(page, sid):
    # (replay-clip) A replay's page shows its one scenario, with no list.
    if is_replay(page):
        settle(page)
        return
    page.locator("#list li", has=page.locator(".name", has_text=re.compile("^" + re.escape(sid) + "$"))).first.click()
    settle(page)


def slide(page, v):
    page.locator("#slider").evaluate(
        "(s, v) => { s.value = String(v); s.dispatchEvent(new Event('input', {bubbles: true})); }", v)
    settle(page, 40)


def seconds(page):
    """A replay's scrubbed time, seconds before the report, from its label."""
    m = re.match(r"([+-]?\d+\.\d+) s", label(page))
    if not m:
        raise Fail(f"a replay's label reads {label(page)!r}")
    return float(m.group(1))


def exercise_time(page, chk, tl, where, tag, kind):
    """(replayfix) A replay's scrubber spans its window in real time:
    the report marked, the spans with no repaint said so, the live input
    and changes marked, the live pointer named and drawn, buttons, keys
    and play stepping through time."""
    length = tl["seconds"]
    slider = page.locator("#slider")
    top = int(slider.get_attribute("max") or "0")
    chk.expect(abs(top - round(length * 1000)) <= 1, where, f"the slider spans {top} ms, the window {length} s")
    chk.expect("(the report)" in label(page), where, f"a replay does not open at the report: {label(page)!r}")
    chk.expect(page.locator("#track .m.report").count() == 1, where, "the report is not marked")
    chk.expect(page.locator("#track-legend span").count() >= 6, where, "the timeline has no legend")
    if tl.get("events"):
        marks = page.locator("#track .m.in, #track .m.chg, #track .m.map, #track .m.late").count()
        chk.expect(marks >= 1, where, "the live events are not marked")
    slide(page, 0)
    chk.expect(abs(seconds(page) + length) < 0.01, where, f"the slider's start is not the window's: {label(page)!r}")
    # A span with no repaint, scrubbed into: said so, the frame held.
    gap = page.evaluate("""() => {
      var d = JSON.parse(document.getElementById('run').textContent), t = d.replay.timeline;
      var sc = d.runs[0].scenarios[0], ms = sc.trace.frames.map(function (f) { return f.k * t.period_ns / 1e6; });
      ms.push(t.seconds * 1000);
      for (var i = 0; i + 1 < ms.length; i++) if (ms[i + 1] - ms[i] >= t.idle_s * 1000) return (ms[i] + ms[i + 1]) / 2;
      return null; }""")
    if gap is not None:
        slide(page, round(gap))
        chk.expect("nothing changed: no repaint for" in label(page), where, f"an idle span reads {label(page)!r}")
        chk.expect(page.locator("#track .b.idle").count() >= 1, where, "no idle span on the timeline")
        chk.shot(page, f"{tag}-{kind}-idle")
    # The live pointer at a time it was on a surface: named, drawn.
    on = [p for p in tl.get("pointer", []) if p.get("on")]
    if on:
        slide(page, round((on[-1]["t"] + length) * 1000) + 1)
        chk.expect("on @" + on[-1]["on"] in label(page), where, f"the pointer is not named: {label(page)!r}")
        chk.shot(page, f"{tag}-{kind}-pointer")
    page.locator("#first").click()
    chk.expect(abs(seconds(page) + length) < 0.01, where, "|< does not go to the window's start")
    page.locator("#next").click()
    chk.expect(seconds(page) > -length, where, "> does not step on in time")
    page.locator("#last").click()
    chk.expect("(the report)" in label(page), where, ">| does not go to the report")
    page.locator("#prev").click()
    chk.expect(seconds(page) < 0, where, "< does not step back in time")
    page.keyboard.press("Home")
    chk.expect(abs(seconds(page) + length) < 0.01, where, "Home does not go to the window's start")
    page.keyboard.press("ArrowRight")
    chk.expect(seconds(page) > -length, where, "Right does not step on in time")
    page.keyboard.press("End")
    chk.expect("(the report)" in label(page), where, "End does not go to the report")
    page.keyboard.press("Home")
    page.locator("#play").click()
    # A span with nothing to show plays in a second at most.
    page.wait_for_timeout(1300)
    if page.locator("#play").inner_text() == "Pause":
        page.locator("#play").click()
    chk.expect(seconds(page) > -length, where, "Play does not advance in time")
    exercise_packed(page, chk, tl, where, tag, kind)
    slide(page, top)


def details(page, chk, where, on):
    """(replay-clip) A replay's dense views behind Details, remembered."""
    b = page.locator("#details-bar button")
    if (b.get_attribute("aria-pressed") == "true") != on:
        b.click()
        settle(page, 80)
    shown = page.locator("#tabs").is_visible()
    chk.expect(shown == on, where, f"Details {'on' if on else 'off'} {'shows' if shown else 'hides'} the tabs")
    stored = page.evaluate("() => { try { return localStorage.getItem('strand-eval:details'); } catch (e) { return 'x'; } }")
    chk.expect(stored == ("1" if on else "0"), where, f"Details is not remembered: {stored!r}")


def exercise_simple(page, chk, where, tag, kind):
    """(replay-clip) A replay's page by default: the frame, the timeline,
    the two overlays and the clip; the dense views behind Details."""
    chk.expect(not page.locator("#list").is_visible(), where, "a replay's page shows the scenario list")
    chk.expect(page.locator("#clip").is_visible(), where, "a replay's page has no clip panel")
    chk.expect(page.locator("#details-bar button").is_visible(), where, "no Details toggle")
    if page.locator("#details-bar button").get_attribute("aria-pressed") == "true":
        page.locator("#details-bar button").click()
        settle(page, 60)
    for sel in ("#tabs", "#film", "#toolbar label.field"):
        chk.expect(not page.locator(sel).first.is_visible(), where, f"{sel} shows without Details")
    big = page.evaluate("""() => Array.from(document.querySelectorAll('.controls .btn, #toolbar .btn, #details-bar .btn, #clip .btn'))
      .filter(b => b.offsetParent).map(b => [b.textContent, b.getBoundingClientRect().height])""")
    small = [b for b in big if b[1] < 36]
    chk.expect(bool(big) and not small, where, f"buttons under 36 px tall: {small[:4]}")
    for name in ("Where it rests", "Onion skin"):
        b = page.locator("#toolbar button", has_text=name)
        chk.expect(b.count() == 1, where, f"no {name!r} toggle")
        if not b.count():
            continue
        was = b.get_attribute("aria-pressed")
        b.click()
        settle(page, 60)
        b = page.locator("#toolbar button", has_text=name)
        chk.expect(b.get_attribute("aria-pressed") != was, where, f"{name!r} does not toggle")
        # (Nothing need change on the stage: at rest, a node rests where
        # it is drawn.)
        b.click()
        settle(page, 60)
    chk.shot(page, f"{tag}-{kind}-simple", full=True)


def exercise_clip(page, chk, tl, where, tag, kind):
    """(replay-clip) The clip: a slice dragged on the timeline and set
    with [ and ], elements clicked and dragged over on the frame, a
    note, Copy for agent (the text on the clipboard, its command the
    one shown), Save clip (one .zip), the clipboard refused (the text
    left to copy by hand) and both cleared."""
    length = tl["seconds"] * 1000
    tb = page.locator("#track").bounding_box()
    y = tb["y"] + tb["height"] / 2
    page.mouse.move(tb["x"] + tb["width"] * 0.8, y)
    page.mouse.down()
    page.mouse.move(tb["x"] + tb["width"] * 0.9, y, steps=4)
    page.mouse.move(tb["x"] + tb["width"] * 0.999, y, steps=4)
    page.mouse.up()
    settle(page, 80)
    chk.expect(page.locator("#track .b.clip").count() == 1, where, "a drag on the timeline marks no slice")
    when = page.locator("#clip .clip-row").first.inner_text()
    chk.expect(" to " in when, where, f"the slice reads {when!r}")
    # [ and ]: the slice from the time shown to the report.
    at = round(length * 0.9)
    slide(page, at)
    page.keyboard.press("[")
    page.keyboard.press("End")
    page.keyboard.press("]")
    settle(page, 60)
    spec_from = "%.3fs" % (at / 1000 - tl["seconds"])
    # Elements: a click on the frame, then a rectangle over it.
    box = page.locator("#stage canvas").first.bounding_box()
    chk.expect(box is not None, where, "no frame to pick elements on")
    if box is None:
        return
    page.mouse.click(box["x"] + box["width"] / 2, box["y"] + min(box["height"] - 2, 10))
    settle(page, 80)
    one = page.locator("#clip .clip-list li").count()
    chk.expect(one >= 1, where, "a click on the frame picks no element")
    page.mouse.move(box["x"] + 1, box["y"] + 1)
    page.mouse.down()
    page.mouse.move(box["x"] + box["width"] / 3, box["y"] + box["height"] / 2, steps=3)
    page.mouse.move(box["x"] + box["width"] * 2 / 3, box["y"] + box["height"] - 1, steps=3)
    page.mouse.up()
    settle(page, 80)
    many = page.locator("#clip .clip-list li").count()
    chk.expect(many >= one, where, "a rectangle over the frame drops elements")
    chk.shot(page, f"{tag}-{kind}-clip-picked", full=True)
    if many > 1:
        page.locator("#clip .clip-list li .remove").last.click()
        settle(page, 60)
        chk.expect(page.locator("#clip .clip-list li").count() == many - 1, where, "Remove takes no element out")
    labels = page.locator("#clip .clip-list li .clip-label").all_inner_texts()
    chk.expect(all(labels), where, f"the clip lists an element with no label: {labels}")
    page.locator("#clip textarea").first.fill("the card's ears lag")
    settle(page, 400)
    cmd = page.locator("#clip pre.cmd").first.inner_text()
    chk.expect(cmd.startswith("strand replay clip ") and f"--from {spec_from}" in cmd and "--note 'the card" in cmd, where,
               f"the command line reads {cmd!r}")
    # Copy for agent.
    page.evaluate("() => { window.__copied = null; }")
    page.locator("#clip button", has_text="Copy for agent").click()
    settle(page, 150)
    copied = page.evaluate("() => window.__copied")
    chk.expect(bool(copied) and copied.startswith("Strand replay clip: "), where, f"Copy for agent copied {str(copied)[:80]!r}")
    if copied:
        chk.expect("note: the card's ears lag" in copied and ("regenerate: " + cmd) in copied, where,
                   "the copied clip lacks its note or its command")
        chk.expect("http" not in copied, where, "the copied clip names a URL")
    # Save clip: one .zip with the text, the JSON and the frames.
    with page.expect_download() as dl:
        page.locator("#clip button", has_text="Save clip").click()
    d = dl.value
    path = d.path()
    raw = open(path, "rb").read() if path else b""
    chk.expect(raw[:4] == b"PK\x03\x04" and d.suggested_filename.endswith(".zip"), where,
               f"Save clip gave {d.suggested_filename!r} ({len(raw)} bytes)")
    chk.expect(b"clip.txt" in raw and b"clip.json" in raw and b"frames/k" in raw, where,
               "the .zip lacks the text, the JSON or a frame")
    if copied:
        chk.expect(copied.encode() in raw, where, "the .zip's text is not the copied clip")
    # The clipboard refused both ways: the clip left selected in a box.
    page.evaluate("""() => { navigator.clipboard.writeText = function () { return Promise.reject(new Error('no')); };
      document.execCommand = function () { return false; }; }""")
    page.locator("#clip button", has_text="Copy for agent").click()
    settle(page, 150)
    hand = page.locator("#clip textarea.byhand")
    chk.expect(hand.count() == 1 and hand.input_value() == copied, where, "a refused clipboard leaves no clip to copy by hand")
    chk.shot(page, f"{tag}-{kind}-clip", full=True)
    page.locator("#clip button", has_text="Clear slice").click()
    settle(page, 60)
    chk.expect(page.locator("#track .b.clip").count() == 0, where, "Clear slice leaves the slice")
    page.locator("#clip button", has_text="Clear elements").click()
    settle(page, 60)
    chk.expect(page.locator("#clip .clip-list li").count() == 0, where, "Clear elements leaves elements")
    page.locator("#clip textarea").first.fill("")


def exercise_clip_phone(page, chk, where):
    """(replay-clip) The clip at a phone's width, with no sideways page
    scroll."""
    page.keyboard.press("Home")
    page.keyboard.press("[")
    page.keyboard.press("End")
    page.keyboard.press("]")
    box = page.locator("#stage canvas").first.bounding_box()
    if box:
        page.mouse.click(box["x"] + box["width"] / 2, box["y"] + min(box["height"] - 2, 10))
    settle(page, 150)
    page.locator("#clip details summary").click()
    settle(page, 60)
    no_page_scroll(page, chk, where + " clip")
    page.locator("#clip button", has_text="Clear slice").click()
    settle(page, 40)

def exercise_packed(page, chk, tl, where, tag, kind):
    """(replayfix) A replay's page is packed: its trace unpacks (the
    frames' rows whole, each frame's fields), and every painted frame is
    drawn from its pixels, a patch over its base, never a wireframe:
    the first, a middle one, one rebuilt from a long chain, the last."""
    info = page.evaluate("""() => {
      var d = JSON.parse(document.getElementById('run').textContent), t = d.replay.timeline;
      var sc = d.runs[0].scenarios[0], fr = sc.trace.frames, p = sc.patches || {};
      var depth = function (n) { var k = 0; while (n && p[n] && k < 999) { n = p[n][0]; k++; } return k; };
      var deepest = 0, at = 0;
      fr.forEach(function (f, i) { for (var s in f.png) { var k = depth(f.png[s]); if (k > deepest) { deepest = k; at = i; } } });
      var ms = function (i) { return Math.round(fr[i].k * t.period_ns / 1e6); };
      return { packed: !!sc.trace.packed, patches: Object.keys(p).length, deepest: deepest,
               times: fr.length ? [ms(0), ms(fr.length >> 1), ms(at), ms(fr.length - 1)] : [] };
    }""")
    if not info["packed"]:
        return
    chk.expect(info["patches"] > 0, where, "a packed replay page has no patched frame")
    for i, v in enumerate(info["times"]):
        slide(page, v)
        settle(page, 250)
        labels = page.locator("#stage .surface-label").all_inner_texts()
        chk.expect(bool(labels), where, f"no surface drawn at {v} ms")
        chk.expect(not any("(wireframe)" in l for l in labels), where,
                   f"a replay's frame at {v} ms is a wireframe: {labels}")
        rows = page.evaluate("() => document.querySelectorAll('#stage canvas').length")
        chk.expect(rows >= 1, where, f"no stage canvas at {v} ms")
        if i == 2:
            chk.shot(page, f"{tag}-{kind}-patched-{info['deepest']}")


def exercise_scenario(page, chk, data, sc, kind, tag):
    """Every mode of the viewer on one scenario."""
    where = f"{tag}/{sc['id']}"
    pick(page, sc["id"])
    chk.expect(page.locator("#head h1").inner_text() == sc["id"], where, "the header names another scenario")
    i, n = frame_no(page)
    chk.expect(n >= 1, where, "no frames")
    chk.shot(page, f"{tag}-{kind}-desktop-light", full=True)
    slider = page.locator("#slider")
    tl = (data.get("replay") or {}).get("timeline")
    if tl and len(data["runs"]) == 1:
        exercise_simple(page, chk, where, tag, kind)
        exercise_time(page, chk, tl, where, tag, kind)
        exercise_clip(page, chk, tl, where, tag, kind)
        details(page, chk, where, True)
        exercise_rest(page, chk, data, sc, kind, tag, where, i, lambda: slide(page, int(slider.get_attribute("max") or "0")))
        details(page, chk, where, False)
        return

    # The scrubber: slider, buttons, keys.
    mid = (n - 1) // 2
    slider.evaluate("(s, v) => { s.value = String(v); s.dispatchEvent(new Event('input', {bubbles: true})); }", mid)
    settle(page, 40)
    chk.expect(frame_no(page)[0] == mid + 1, where, f"slider to {mid} shows {label(page)!r}")
    page.locator("#first").click()
    chk.expect(frame_no(page)[0] == 1, where, "|< does not go to the first frame")
    page.locator("#last").click()
    chk.expect(frame_no(page)[0] == n, where, ">| does not go to the last frame")
    page.locator("#prev").click()
    chk.expect(frame_no(page)[0] == max(1, n - 1), where, "< does not step back")
    page.locator("#next").click()
    chk.expect(frame_no(page)[0] == n, where, "> does not step on")
    page.locator("#first").click()
    page.locator("body").press("ArrowRight")
    chk.expect(frame_no(page)[0] == min(n, 2), where, "Right does not step on")
    page.keyboard.press("Shift+ArrowRight")
    chk.expect(frame_no(page)[0] == min(n, 12), where, "Shift+Right does not step ten")
    page.keyboard.press("End")
    chk.expect(frame_no(page)[0] == n, where, "End does not go to the last frame")
    page.keyboard.press("Home")
    chk.expect(frame_no(page)[0] == 1, where, "Home does not go to the first frame")
    if n > 2:
        page.locator("#play").click()
        page.wait_for_timeout(400)
        chk.expect(page.locator("#play").inner_text() in ("Pause", "Play"), where, "the play button has no label")
        if page.locator("#play").inner_text() == "Pause":
            page.locator("#play").click()
        chk.expect(frame_no(page)[0] > 1, where, "Play does not advance")

    # Back to the frame the viewer opened on (a failing finding's).
    slider.evaluate("(s, v) => { s.value = String(v); s.dispatchEvent(new Event('input', {bubbles: true})); }", i - 1)
    settle(page)
    exercise_rest(page, chk, data, sc, kind, tag, where, i, lambda: slide(page, i - 1))


def exercise_rest(page, chk, data, sc, kind, tag, where, i, back):
    """The filmstrip, overlays and panels, from the frame the viewer
    opened on (`back` returns to it)."""
    # The filmstrip.
    film = page.locator("#film canvas")
    count = film.count()
    chk.expect(count >= 1, where, "no filmstrip")
    sizes = page.evaluate("() => Array.from(document.querySelectorAll('#film canvas')).map(c => [c.width, c.height])")
    chk.expect(all(w <= 240 and h <= 36 for w, h in sizes), where, f"filmstrip frames larger than 240 by 36: {sizes[:3]}")
    if count > 1:
        target = 0 if film.nth(0).get_attribute("class") != "cur" else 1
        before = label(page)
        film.nth(target).click()
        settle(page, 60)
        chk.expect(label(page) != before, where, "clicking a filmstrip frame does not move the scrubber")
        chk.expect(page.locator("#film canvas.cur").count() == 1, where, "the filmstrip marks no current frame")
        back()
        settle(page)

    # Overlays.
    h0 = stage_hash(page)
    set_check(page, "every node", True)
    h1 = stage_hash(page)
    chk.expect(h1 != h0 or page.evaluate("() => document.querySelectorAll('#stage canvas').length") == 0, where,
               "'every node' draws nothing new")
    chk.shot(page, f"{tag}-{kind}-overlay-every-node")
    set_check(page, "rest", False)
    set_check(page, "boxes", False)
    h2 = stage_hash(page)
    chk.expect(h2 != h1, where, "turning boxes and rest boxes off changes nothing")
    set_check(page, "rest", True)
    h3 = stage_hash(page)
    chk.expect(h3 != h2, where, "rest boxes alone draw nothing")
    set_check(page, "boxes", True)
    set_check(page, "every node", False)
    chk.expect(stage_hash(page) == h0, where, "the overlays do not return to how they were")
    chk.expect(fits(page), where, "the fitted stage is wider than the stage")
    set_select(page, "zoom", "2")
    w0 = widest_canvas(page)
    set_select(page, "zoom", "1")
    w1 = widest_canvas(page)
    chk.expect(abs(w1 * 2 - w0) <= 1, where, f"zoom 1x is not half of 2x ({w1} against {w0})")
    set_select(page, "zoom", "4")
    chk.shot(page, f"{tag}-{kind}-zoom4")
    set_select(page, "zoom", "fit")

    # Onion skin, both ways, and the diff to the previous frame.
    if i > 3:
        set_select(page, "onion", "3")
        ho = stage_hash(page)
        if kind == "fail":
            chk.expect(ho != h0, where, "onion skin at a failing frame draws nothing")
        chk.shot(page, f"{tag}-{kind}-onion-outlines")
        set_select(page, "as", "ghost")
        chk.shot(page, f"{tag}-{kind}-onion-ghost")
        set_select(page, "as", "outline")
        set_select(page, "onion", "0")
    set_check(page, "diff to previous", True)
    settle(page)
    chk.shot(page, f"{tag}-{kind}-diff-previous")
    set_check(page, "diff to previous", False)

    # The tooltip of the node under the pointer.
    cv = page.locator("#stage canvas").first
    if cv.count():
        box = cv.bounding_box()
        if box:
            page.mouse.move(box["x"] + box["width"] / 2, box["y"] + min(box["height"] - 2, 10))
            settle(page, 60)
            if page.locator("#tip").is_visible():
                chk.expect("drawn" in page.locator("#tip").inner_text(), where, "the tooltip shows no box")
                chk.shot(page, f"{tag}-{kind}-tooltip")
            page.mouse.move(2, 2)

    # The panels.
    tabs = [t.strip() for t in page.locator("#tabs button").all_inner_texts()]
    want_tabs = ["Findings", "Nodes", "Curves", "Cost"] + (["A/B"] if len(data["runs"]) == 2 else [])
    # (viewer-owner) A replay has no reference to approve.
    if not data.get("replay"):
        want_tabs.append("Approve")
    chk.expect(tabs == want_tabs, where, f"tabs {tabs}, not {want_tabs}")

    def tab(name):
        page.locator("#tabs button", has_text=re.compile("^" + re.escape(name) + "$")).click()
        settle(page, 60)
        chk.expect(page.locator(f'#tabs button[aria-selected="true"]').inner_text() == name, where, f"tab {name} not selected")

    tab("Findings")
    results = (sc.get("checks") or {}).get("results", [])
    if results:
        rows = page.locator("#panel table tr").count()
        chk.expect(rows == len(results), where, f"findings table has {rows} rows for {len(results)} checks")
    fails = page.locator("#panel .find.st-fail")
    if kind == "changed" and sc["status"] == "changed":
        chk.expect(fails.count() == 0 and page.locator("#panel .find.st-changed").count() >= 1, where,
                   "a CHANGED scenario's reference finding does not read as CHANGED")
    if kind == "fail":
        chk.expect(fails.count() >= 1, where, "a failing scenario lists no failing finding")
        if fails.count():
            fails.first.click()
            settle(page, 60)
            k = page.locator("#frame-label").inner_text()
            chk.expect(re.search(r"k\d+", k) is not None, where, "a finding does not jump to a frame")
    sheets = page.evaluate("() => Array.from(document.querySelectorAll('#panel .find a')).map(a => a.href)")
    for href in sheets:
        ok = page.evaluate(
            "(u) => new Promise(r => { var i = new Image(); i.onload = () => r(i.naturalWidth > 0); i.onerror = () => r(false); i.src = u; })",
            href)
        chk.expect(ok, where, f"contact sheet {href} does not load")
    chk.shot(page, f"{tag}-{kind}-findings", full=True)

    tab("Nodes")
    lines = page.locator("#panel .tree div")
    chk.expect(lines.count() >= 1, where, "the node tree is empty")
    if lines.count():
        lines.nth(min(1, lines.count() - 1)).click()
        settle(page, 40)
        chk.expect(page.locator("#panel .tree div.sel").count() >= 1, where, "clicking a node does not select it")
    chk.shot(page, f"{tag}-{kind}-nodes", full=True)

    tab("Curves")
    charts = page.locator("#panel svg.chart")
    chk.expect(charts.count() >= 1, where, "no curves for the selected node")
    if charts.count():
        page.locator("#panel select").select_option("run")
        settle(page, 40)
        charts = page.locator("#panel svg.chart")
        chk.expect(charts.count() >= 1, where, "no curves over the whole run")
        charts.first.scroll_into_view_if_needed()
        b = charts.first.bounding_box()
        before = label(page)
        page.mouse.click(b["x"] + b["width"] * 0.8, b["y"] + b["height"] / 2)
        settle(page, 40)
        chk.expect(label(page) != before or frame_no(page)[1] < 3, where, "clicking a curve does not move the scrubber")
        size = page.evaluate("() => { var t = document.querySelector('#panel svg.chart text');"
                             " return t ? t.getBoundingClientRect().height : 0; }")
        chk.expect(size >= 9, where, f"the curves' labels are {size:.1f} px tall")
    chk.shot(page, f"{tag}-{kind}-curves", full=True)

    tab("Cost")
    chk.expect(page.locator("#panel svg.chart").count() == 4, where, "the cost lane does not have four charts")
    chk.expect(page.locator("#panel table tr").count() >= 2, where, "the cost table has no surface")
    chk.shot(page, f"{tag}-{kind}-cost", full=True)

    run_out = data["runs"][0]["out"]
    if len(data["runs"]) == 2:
        exercise_ab(page, chk, data, sc, kind, tag, where, tab)

    # The viewer keeps a verdict's draft in this browser, per run and
    # scenario, until it is copied (eval/viewer/viewer.js). One page can
    # show one scenario under two kinds (a mutant's lone scenario is both
    # its failing and its CHANGED one; an A/B page's is its passing and
    # its A/B one), and the exercise before left Good with a cleared note:
    # each exercise starts from no draft.
    if data.get("replay"):
        return
    page.evaluate("(k) => { try { window.localStorage.removeItem(k); } catch (e) {} }",
                  f"strand-eval:verdict:{run_out}/{sc['id']}")
    tab("Approve")
    exercise_approve(page, chk, sc, kind, tag, where, run_out)


def exercise_ab(page, chk, data, sc, kind, tag, where, tab):
    ref = bool(data["runs"][1].get("reference"))
    has_b = any(s["id"] == sc["id"] for s in data["runs"][1]["scenarios"])
    tab("A/B")
    text = page.locator("#panel").inner_text()
    if has_b:
        chk.expect("A:" in text and "B:" in text, where, "the A/B panel names no sides")
        rows = page.locator("#panel table tr")
        info = (data.get("ab") or {}).get("scenarios", {}).get(sc["id"])
        if info and not info.get("same"):
            chk.expect(rows.count() >= 2, where, "the A/B panel lists no divergence")
            rows.nth(1).click()
            settle(page, 60)
    chk.shot(page, f"{tag}-{kind}-ab-panel", full=True)
    if not has_b:
        return
    views = page.evaluate(
        "() => { var c = document.querySelector('[data-vc=\"A/B\"]'); return c ? Array.from(c.options).map(o => o.value) : []; }"
    ) if page.evaluate(FIELD_JS, "A/B") else []
    chk.expect(views == ["a", "b", "side", "flicker", "difference"], where, f"A/B views {views}")
    set_select(page, "A/B", "side")
    labels = page.locator("#stage .cols > div > .surface-label:first-child").all_inner_texts()
    chk.expect(len(labels) == 2 and labels[0].startswith("A:") and labels[1].startswith("B:"), where,
               f"side by side shows {labels}")
    if ref:
        chk.expect("reference" in labels[1].lower() if len(labels) == 2 else False, where,
                   f"the B side does not say it is the reference: {labels}")
    chk.shot(page, f"{tag}-{kind}-ab-side", full=True)
    set_select(page, "A/B", "a")
    ha = stage_hash(page)
    set_select(page, "A/B", "difference")
    chk.expect(page.locator("#stage .surface-label").first.inner_text().startswith("A against B"), where,
               "difference view is not labelled")
    if ref and kind in ("changed", "fail"):
        # The viewer opens a CHANGED run at its first difference.
        chk.expect(stage_hash(page) != ha, where, "the difference from the reference draws nothing")
        chk.expect("no images" not in page.locator("#stage").inner_text(), where,
                   "the difference from the reference asks for pixels a reference does not hold")
    if not ref and kind == "ab":
        # Two runs with frames: their pixel difference, at the frame the
        # panel's first divergence put the scrubber on.
        chk.expect("no images" not in page.locator("#stage").inner_text(), where,
                   "the difference of two runs with frames has no images")
        chk.expect(stage_hash(page) != ha, where, "the difference of two runs that differ draws nothing")
    chk.shot(page, f"{tag}-{kind}-ab-difference")
    set_select(page, "A/B", "b")
    chk.expect(page.locator("#stage .surface-label").first.inner_text().startswith("B:"), where, "B view is not B")
    chk.shot(page, f"{tag}-{kind}-ab-b")
    set_select(page, "A/B", "flicker")
    first = page.locator("#stage .surface-label").first.inner_text()
    page.wait_for_timeout(700)
    second = page.locator("#stage .surface-label").first.inner_text()
    chk.expect(first[:2] != second[:2], where, f"flicker does not alternate ({first!r}, {second!r})")
    set_select(page, "A/B", "side")


def exercise_approve(page, chk, sc, kind, tag, where, run_out):
    panel = page.locator("#panel")
    copy = panel.locator("button", has_text="Copy command")
    good = panel.locator("button", has_text=re.compile("^Good$"))
    bad = panel.locator("button", has_text=re.compile("^Bad$"))
    cmd = panel.locator("pre.cmd").last
    note = panel.locator("textarea")
    chk.expect(copy.is_disabled(), where, "Copy is enabled before a verdict and a note")
    chk.expect(cmd.inner_text().startswith("Pick Good"), where, f"the command reads {cmd.inner_text()!r} before a verdict")
    good.click()
    chk.expect(copy.is_disabled(), where, "Copy is enabled before a note is written")
    chk.expect(cmd.inner_text().startswith("Write a note"), where, f"Good without a note reads {cmd.inner_text()!r}")
    note.fill("   ")
    chk.expect(copy.is_disabled(), where, "Copy is enabled by a blank note")
    text = "it's the intended motion; checked frame by frame"
    note.fill(text)
    at = f"{run_out}/{sc['id']}"
    want = f"scripts/container/run.sh eval approve {at} --note {quote(text)}"
    chk.expect(copy.is_enabled(), where, "Copy stays disabled with a verdict and a note")
    chk.expect(cmd.inner_text() == want, where, f"approve command {cmd.inner_text()!r}, not {want!r}")
    copy.click()
    page.wait_for_timeout(50)
    chk.expect(page.evaluate("() => window.__copied") == want, where,
               f"Copy copied {page.evaluate('() => window.__copied')!r}")
    chk.shot(page, f"{tag}-{kind}-approve", full=True)
    bad.click()
    want_bad = f"scripts/container/run.sh eval verdict {at} bad --note {quote(text)}"
    chk.expect(cmd.inner_text() == want_bad, where, f"bad command {cmd.inner_text()!r}, not {want_bad!r}")
    good.click()
    who = panel.locator("input.who")
    chk.expect(who.count() == 1, where, "no approver field")
    if who.count():
        who.fill("unknown")
        chk.expect(cmd.inner_text() == want, where, f"'unknown' is passed as an approver: {cmd.inner_text()!r}")
        who.fill("claude")
        want_by = want + " --approver 'claude'"
        chk.expect(cmd.inner_text() == want_by, where, f"approve command {cmd.inner_text()!r}, not {want_by!r}")
        copy.click()
        page.wait_for_timeout(50)
        chk.expect(page.evaluate("() => window.__copied") == want_by, where, "Copy does not copy the approver")
        chk.shot(page, f"{tag}-{kind}-approve-approver", full=True)
        who.fill("")
    note.fill("")
    chk.expect(copy.is_disabled(), where, "Copy stays enabled once the note is cleared")


def themes_and_widths(browser, url, chk, tag, sid, kind):
    """Each theme at a desktop's and a phone's width, the system's and the toggle's."""
    for scheme in ("light", "dark"):
        for size, wname in ((DESKTOP, "desktop"), (PHONE, "phone")):
            if wname == "desktop" and scheme == "light":
                continue  # exercise_scenario's first shot
            ctx = browser.new_context(viewport=size, color_scheme=scheme)
            page = ctx.new_page()
            where = lambda: f"{tag}/{sid} {wname} {scheme}"
            watch(page, chk, where)
            page.goto(url)
            page.wait_for_selector("#list li", state="attached")
            pick(page, sid)
            settle(page)
            no_page_scroll(page, chk, where())
            chk.expect(fits(page), where(), "the fitted stage is wider than the stage")
            chk.shot(page, f"{tag}-{kind}-{wname}-{scheme}", full=True)
            if wname == "phone":
                if is_replay(page):
                    exercise_clip_phone(page, chk, where())
                    details(page, chk, where(), True)
                page.locator("#tabs button", has_text="Curves").click()
                settle(page)
                no_page_scroll(page, chk, where() + " curves")
                chk.shot(page, f"{tag}-{kind}-{wname}-{scheme}-curves", full=True)
                if not is_replay(page):
                    page.locator("#tabs button", has_text="Approve").click()
                    settle(page)
                    no_page_scroll(page, chk, where() + " approve")
                else:
                    details(page, chk, where(), False)
            ctx.close()
    # The toggle overrides the system: auto, light, dark.
    ctx = browser.new_context(viewport=DESKTOP, color_scheme="light")
    page = ctx.new_page()
    watch(page, chk, lambda: f"{tag} theme toggle")
    page.goto(url)
    page.wait_for_selector("#list li", state="attached")
    seen = []
    for _ in range(3):
        page.locator("#theme").click()
        settle(page, 40)
        seen.append((page.locator("#theme").inner_text(), page.evaluate("() => document.documentElement.dataset.theme || 'auto'"),
                     page.evaluate("() => getComputedStyle(document.body).backgroundColor")))
    names = [s[0] for s in seen]
    chk.expect(names == ["Theme: light", "Theme: dark", "Theme: auto"], f"{tag} theme", f"the toggle reads {names}")
    chk.expect(seen[0][2] != seen[1][2], f"{tag} theme", "light and dark have the same background")
    ctx.close()


def check_viewer(browser, url, chk, tag):
    ctx = browser.new_context(viewport=DESKTOP, color_scheme="light")
    ctx.add_init_script(COPY_HOOK)
    page = ctx.new_page()
    current = {"at": tag}
    watch(page, chk, lambda: current["at"])
    page.goto(url)
    page.wait_for_selector("#list li", state="attached")
    data = data_of(page)
    a = data["runs"][0]["scenarios"]
    items = page.locator("#list li").count()
    ids = []
    for r in data["runs"]:
        for s in r["scenarios"]:
            if s["id"] not in ids:
                ids.append(s["id"])
    chk.expect(items == len(ids), tag, f"the list has {items} scenarios, the page {len(ids)}")
    # The filter (a replay's page has none: one scenario, no list).
    if ids and not is_replay(page):
        page.locator("#filter").fill(ids[-1])
        chk.expect(page.locator("#list li").count() >= 1, tag, "the filter hides the scenario it names")
        page.locator("#filter").fill("")
    picked = {}
    for s in a:
        refs = s.get("refs") or {}
        if s["status"] == "pass" and "pass" not in picked:
            picked["pass"] = s
        if s["status"] == "fail" and "fail" not in picked:
            picked["fail"] = s
        if refs.get("status") == "changed" and "changed" not in picked and s is not picked.get("fail"):
            picked["changed"] = s
    if "changed" not in picked:
        for s in a:
            if (s.get("refs") or {}).get("status") == "changed":
                picked["changed"] = s
                break
    # An A/B page of two runs (two revisions, a mutant against HEAD):
    # a scenario the B run fails, where the two sides differ.
    if len(data["runs"]) > 1 and not data["runs"][1].get("reference"):
        for s in data["runs"][1]["scenarios"]:
            if s["status"] == "fail":
                same = next((x for x in a if x["id"] == s["id"]), None)
                if same is not None:
                    picked["ab"] = same
                break
    # (replayfix) A glitch report's replay: its one scenario, scrubbed by
    # time.
    if (data.get("replay") or {}).get("timeline") and a:
        picked = {"replay": a[0]}
    for kind, s in picked.items():
        current["at"] = f"{tag}/{s['id']}"
        print(f"  {kind}: {s['id']}", flush=True)
        try:
            exercise_scenario(page, chk, data, s, kind, tag)
        except Fail as e:
            chk.problem(current["at"], str(e))
        chk.kinds.add(kind)
    ctx.close()
    for kind, s in picked.items():
        themes_and_widths(browser, url, chk, tag, s["id"], kind)


def check_grid(browser, url, chk, tag):
    for scheme in ("light", "dark"):
        for size, wname in ((DESKTOP, "desktop"), (PHONE, "phone")):
            ctx = browser.new_context(viewport=size, color_scheme=scheme)
            page = ctx.new_page()
            where = f"{tag} {wname} {scheme}"
            watch(page, chk, lambda: where)
            page.goto(url)
            page.wait_for_selector("table.grid")
            meta = page.locator(".meta").first.inner_text()
            m = re.match(r"(\d+) cells", meta)
            cells = page.locator("table.grid td:not(.off)").count()
            chk.expect(m is not None and int(m.group(1)) == cells, where, f"the head says {meta!r}, the grids hold {cells} cells")
            no_page_scroll(page, chk, where)
            chk.shot(page, f"{tag}-matrix-{wname}-{scheme}", full=True)
            if wname == "desktop" and scheme == "light":
                links = page.locator("table.grid td a")
                if links.count():
                    title = links.first.locator("xpath=..").get_attribute("title") or ""
                    chk.expect("failed:" in title or "changed" in title or "known" in title, where,
                               f"a linked cell's title says nothing of why: {title!r}")
                    links.first.click()
                    page.wait_for_selector("#list li", state="attached")
                    settle(page)
                    chk.expect(page.locator("#head h1").count() == 1, where, "a cell's page shows no scenario")
                    chk.shot(page, f"{tag}-matrix-cell-page", full=True)
            ctx.close()
    chk.kinds.add("matrix")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shots", required=True)
    ap.add_argument("--expect", default="")
    ap.add_argument("pages", nargs="+")
    args = ap.parse_args()
    os.makedirs(args.shots, exist_ok=True)
    chk = Checker(args.shots)
    t0 = time.time()
    with sync_playwright() as p:
        browser = p.chromium.launch()
        try:
            for n, path in enumerate(args.pages):
                tag = os.environ.get(f"VIEWER_TAG_{n}") or f"page{n}"
                url = "file://" + os.path.abspath(path)
                print(f"== {tag}: {path}", flush=True)
                ctx = browser.new_context()
                page = ctx.new_page()
                page.goto(url)
                kind = page.evaluate(
                    "() => document.getElementById('run') ? 'viewer' : (document.querySelector('table.grid') ? 'grid' : 'other')"
                )
                ctx.close()
                if kind == "viewer":
                    check_viewer(browser, url, chk, tag)
                elif kind == "grid":
                    check_grid(browser, url, chk, tag)
                else:
                    chk.problem(tag, "neither a strand-eval report nor a matrix grid")
        finally:
            browser.close()
    for k in [k for k in args.expect.split(",") if k]:
        chk.expect(k in chk.kinds, "expect", f"no page had a {k} scenario to exercise")
    print(f"{len(chk.saved)} screenshots in {args.shots} ({time.time() - t0:.0f} s)")
    if chk.problems:
        print(f"viewer-check: {len(chk.problems)} problems")
        for p in chk.problems:
            print(f"  {p}")
        return 1
    print("viewer-check: every mode works, no console error")
    return 0


if __name__ == "__main__":
    sys.exit(main())
