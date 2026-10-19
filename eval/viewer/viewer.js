// strand-eval's viewer (docs/eval.md, "The viewer"). Vanilla, no
// network: everything it shows is in the page's `#run` JSON.
(function () {
  'use strict';

  var DATA = JSON.parse(document.getElementById('run').textContent);
  var MOT = DATA.motions;
  var AX = DATA.axes;
  var AB = DATA.runs.length === 2;
  // The B side is the run's approved references (a check report whose
  // runs left them), not a second run.
  var REF = AB && !!DATA.runs[1].reference;
  var GLIDE = 1 << MOT.indexOf('GLIDE');

  // ---- small helpers ------------------------------------------------------

  function $(id) { return document.getElementById(id); }
  function el(tag, attrs) {
    var e = document.createElement(tag);
    if (attrs) {
      for (var k in attrs) {
        if (!Object.prototype.hasOwnProperty.call(attrs, k)) continue;
        var v = attrs[k];
        if (v === null || v === undefined || v === false) continue;
        if (k === 'text') e.textContent = v;
        else if (k === 'cls') e.className = v;
        else if (k.slice(0, 2) === 'on') e.addEventListener(k.slice(2), v);
        else e.setAttribute(k, v === true ? '' : v);
      }
    }
    for (var i = 2; i < arguments.length; i++) {
      var c = arguments[i];
      if (c === null || c === undefined) continue;
      e.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
    }
    return e;
  }
  function esc(t) {
    return String(t).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }
  var store = {
    get: function (k) { try { return window.localStorage.getItem(k); } catch (e) { return null; } },
    set: function (k, v) { try { window.localStorage.setItem(k, v); } catch (e) { /* storage off */ } }
  };
  function css(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || '#888';
  }
  function fmt(v, d) {
    if (v === null || v === undefined) return '-';
    return Number(v).toFixed(d === undefined ? 2 : d);
  }
  function names(bits, list) {
    var out = [];
    for (var i = 0; i < list.length; i++) if (bits & (1 << i)) out.push(list[i]);
    return out;
  }

  // ---- the data, prepared ---------------------------------------------------

  // What shows at each frame: a surface's rows stand until it paints
  // again; a detached surface leaves at its tick (checks::Timeline).
  function timeline(t, C) {
    var ks = [], screens = [], frames = [];
    var cur = new Map();
    var gone = t.surfaces.filter(function (s) { return s.detached !== null && s.detached !== undefined; })
      .map(function (s) { return [s.detached, s.id]; })
      .sort(function (a, b) { return a[0] - b[0]; });
    var gi = 0;
    function snap() { return new Map(cur); }
    function detach(d, sid) {
      if (!frames.length) return;
      cur.delete(sid);
      ks.push(d); screens.push(snap()); frames.push(frames[frames.length - 1]);
    }
    t.frames.forEach(function (f) {
      while (gi < gone.length && gone[gi][0] < f.k) { detach(gone[gi][0], gone[gi][1]); gi++; }
      Array.from(cur.keys()).forEach(function (s) {
        if (!(String(s) in f.sizes)) cur.delete(s);
      });
      // A frame's own scale (a `scale` step changes it); a trace from
      // before it was recorded has the scenario's.
      var fscale = f.scale || t.scenario.scale || 1;
      f.painted.forEach(function (s) {
        var prev = cur.get(s);
        var fresh = f.png && f.png[s];
        var png = fresh || (prev ? prev.png : null);
        var scale = fresh || !prev ? fscale : prev.scale;
        cur.set(s, { rows: f.n.filter(function (r) { return r[C.s] === s; }), png: png, size: f.sizes[s], scale: scale });
      });
      Array.from(cur.keys()).forEach(function (s) {
        var v = cur.get(s);
        if (f.sizes[s] && v.size !== f.sizes[s]) cur.set(s, { rows: v.rows, png: v.png, size: f.sizes[s], scale: v.scale });
      });
      ks.push(f.k); screens.push(snap()); frames.push(f);
    });
    while (gi < gone.length) { detach(gone[gi][0], gone[gi][1]); gi++; }
    return { ks: ks, screens: screens, frames: frames };
  }

  function prep(sc) {
    if (sc.C) return sc;
    var t = sc.trace;
    var C = {};
    t.cols.forEach(function (c, i) { C[c] = i; });
    sc.C = C;
    sc.tl = timeline(t, C);
    sc.fails = [];
    if (sc.checks) {
      sc.checks.results.forEach(function (r) {
        r.findings.forEach(function (f) { if (f.sev === 'fail') sc.fails.push(f); });
      });
    }
    sc.failTicks = new Set();
    sc.failKeys = new Set();
    sc.fails.forEach(function (f) {
      if (f.k !== null && f.k !== undefined) sc.failTicks.add(f.k);
      if (f.key) sc.failKeys.add(f.key);
    });
    sc.imgs = {};
    return sc;
  }

  function at(sc, k) {
    var ks = sc.tl.ks, lo = 0, hi = ks.length;
    while (lo < hi) { var mid = (lo + hi) >> 1; if (ks[mid] <= k) lo = mid + 1; else hi = mid; }
    return lo - 1;
  }

  function obj(sc, r) {
    var o = {};
    sc.trace.cols.forEach(function (c, i) { o[c] = r[i]; });
    return o;
  }

  function label(sc, key) {
    var m = sc.trace.nodes[key];
    return m ? m.label : key;
  }

  function rowsAt(sc, i) {
    var out = [];
    if (i < 0 || i >= sc.tl.screens.length) return out;
    sc.tl.screens[i].forEach(function (v) { v.rows.forEach(function (r) { out.push(r); }); });
    return out;
  }

  function rowOf(sc, i, key) {
    if (i < 0 || i >= sc.tl.screens.length) return null;
    var found = null;
    sc.tl.screens[i].forEach(function (v) {
      if (found) return;
      for (var j = 0; j < v.rows.length; j++) if (v.rows[j][0] === key) { found = v.rows[j]; return; }
    });
    return found;
  }

  // ---- state --------------------------------------------------------------

  var ids = [];
  DATA.runs.forEach(function (run) {
    run.scenarios.forEach(function (s) { if (ids.indexOf(s.id) < 0) ids.push(s.id); });
  });

  var S = {
    id: ids[0] || null,
    i: 0,
    // 'fit': the largest whole zoom (a fraction below 1) at which the
    // widest surface fits the stage; or a fixed one.
    zoom: 'fit',
    boxes: true,
    laid: true,
    all: false,
    onion: 0,
    onionMode: 'outline',
    diff: false,
    view: AB ? 'side' : 'a',
    flickB: false,
    speed: 1,
    playing: false,
    sel: new Set(),
    tab: store.get('strand-eval:tab') || 'findings',
    scope: 'segment',
    failOnly: false
  };

  function scOf(run, id) {
    var r = DATA.runs[run];
    if (!r) return null;
    for (var i = 0; i < r.scenarios.length; i++) if (r.scenarios[i].id === id) return prep(r.scenarios[i]);
    return null;
  }
  function A() { return scOf(0, S.id) || scOf(1, S.id); }
  // The zoom the stage is drawn at (S.zoom resolved; see fitZoom).
  var Z = 2;
  function B() { return AB ? scOf(1, S.id) : null; }

  // ---- theme --------------------------------------------------------------

  var themes = ['auto', 'light', 'dark'];
  function applyTheme(t) {
    if (t === 'auto') delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = t;
    $('theme').textContent = 'Theme: ' + t;
  }
  var theme = store.get('strand-eval:theme') || 'auto';
  applyTheme(theme);
  $('theme').addEventListener('click', function () {
    theme = themes[(themes.indexOf(theme) + 1) % themes.length];
    store.set('strand-eval:theme', theme);
    applyTheme(theme);
    draw();
  });

  // ---- scenario list --------------------------------------------------------

  function statusOf(id) {
    var worst = null, order = ['pass', 'run', 'changed', 'xfail', 'fail', 'error'];
    DATA.runs.forEach(function (run) {
      run.scenarios.forEach(function (s) {
        if (s.id !== id) return;
        if (worst === null || order.indexOf(s.status) > order.indexOf(worst)) worst = s.status;
      });
    });
    return worst || 'run';
  }

  function renderList() {
    var q = $('filter').value.toLowerCase();
    var ul = $('list');
    ul.textContent = '';
    ids.forEach(function (id) {
      var st = statusOf(id);
      var any = DATA.runs[0].scenarios.some(function (s) { return s.id === id && s.refs && s.refs.status === 'missing'; });
      if (q && (id + ' ' + st).toLowerCase().indexOf(q) < 0) return;
      var li = el('li', { cls: id === S.id ? 'cur' : '', title: id + ': ' + st, onclick: function () { pick(id); } },
        el('span', { cls: 'dot st-' + st }),
        el('span', { cls: 'name', text: id }),
        el('span', { cls: 'note st-' + st, text: st.toUpperCase() }),
        any ? el('span', { cls: 'note', text: 'no reference' }) : null);
      ul.appendChild(li);
    });
  }
  $('filter').addEventListener('input', renderList);

  function pick(id) {
    S.id = id;
    S.i = 0;
    S.sel = new Set();
    var a = A();
    if (a && a.fails.length) {
      a.fails.slice(0, 3).forEach(function (f) { if (f.key) S.sel.add(f.key); });
      var f0 = a.fails[0];
      if (f0.k !== null && f0.k !== undefined) S.i = Math.max(0, at(a, f0.k));
    }
    renderAll();
  }

  // ---- header ---------------------------------------------------------------

  function renderHead() {
    var h = $('head');
    h.textContent = '';
    var a = A();
    if (!a) return;
    var sc = a.trace.scenario, b = a.trace.build;
    h.appendChild(el('h1', { text: a.id }));
    var bits = [
      AB ? DATA.runs.map(function (r) { return r.label; }).join(' vs ') : b.rev + (b.dirty ? ' (with changes)' : ''),
      sc.hz + ' Hz', 'scale ' + sc.scale, 'text ' + sc.text, 'backend ' + sc.backend,
      sc.reduced_motion ? 'motion reduced' : null, a.file
    ].filter(function (x) { return x; });
    h.appendChild(el('div', { cls: 'meta', text: bits.join(' · ') }));
    var chips = el('div', { cls: 'chips' });
    // Two runs: each side's status, as the list's worst of them would
    // otherwise disagree with A's alone.
    var bs = AB && !REF ? scOf(1, a.id) : null;
    chips.appendChild(el('span', { cls: 'chip st-' + a.status, text: (bs ? 'A ' : '') + a.status.toUpperCase() }));
    if (bs) chips.appendChild(el('span', { cls: 'chip st-' + bs.status, text: 'B ' + bs.status.toUpperCase() }));
    if (a.checks) {
      a.checks.results.forEach(function (r) {
        var st = r.status.status;
        if (st === 'pass' || st === 'skip') return;
        chips.appendChild(el('button', {
          type: 'button', cls: 'chip st-' + st, text: r.check + ' ' + st.toUpperCase(),
          title: r.status.why || '',
          onclick: function () {
            var f = r.findings.filter(function (x) { return x.sev === 'fail' || x.sev === 'warn'; })[0];
            if (f) jump(a, f);
            setTab('findings');
          }
        }));
      });
    }
    h.appendChild(chips);
    if (a.error) h.appendChild(el('div', { cls: 'st-error', text: 'error: ' + a.error }));
    if (a.thinned) h.appendChild(el('div', { cls: 'warn', text: 'Some frames are thinned to keep the page small: they are drawn as wireframes of the trace.' }));
    a.notes.forEach(function (n) { h.appendChild(el('div', { cls: 'note', text: 'note: ' + n })); });
  }

  // ---- toolbar ------------------------------------------------------------------

  function renderToolbar() {
    var t = $('toolbar');
    t.textContent = '';
    function sel(labelText, value, opts, set) {
      var s = el('select');
      opts.forEach(function (o) {
        var op = el('option', { value: String(o[0]), text: o[1] });
        if (String(o[0]) === String(value)) op.selected = true;
        s.appendChild(op);
      });
      s.addEventListener('change', function () { set(s.value); draw(); });
      return el('label', { cls: 'field' }, labelText + ' ', s);
    }
    function chk(labelText, value, set, title) {
      var c = el('input', { type: 'checkbox' });
      c.checked = value;
      c.addEventListener('change', function () { set(c.checked); draw(); });
      return el('label', { cls: 'field', title: title || null }, c, ' ' + labelText);
    }
    t.appendChild(sel('zoom', S.zoom, [['fit', 'fit'], [1, '1×'], [2, '2×'], [3, '3×'], [4, '4×'], [6, '6×'], [8, '8×']], function (v) { S.zoom = v === 'fit' ? 'fit' : +v; }));
    t.appendChild(chk('boxes', S.boxes, function (v) { S.boxes = v; }, 'Drawn boxes solid: blue authored, orange derived, red failing'));
    t.appendChild(chk('laid', S.laid, function (v) { S.laid = v; }, 'Laid boxes dashed'));
    t.appendChild(chk('every node', S.all, function (v) { S.all = v; }, 'Boxes of nodes that do not move too'));
    t.appendChild(sel('onion', S.onion, [[0, 'off'], [1, '1'], [2, '2'], [3, '3'], [4, '4'], [6, '6'], [8, '8']], function (v) { S.onion = +v; }));
    t.appendChild(sel('as', S.onionMode, [['outline', 'outlines'], ['ghost', 'ghost pixels']], function (v) { S.onionMode = v; }));
    t.appendChild(chk('diff to previous', S.diff, function (v) { S.diff = v; }, 'Pixels that changed since the frame before, in red'));
    if (AB) {
      // A reference holds no pixels: its difference is of boxes.
      t.appendChild(sel('A/B', S.view, [['a', 'A: ' + DATA.runs[0].label], ['b', 'B: ' + DATA.runs[1].label], ['side', 'side by side'], ['flicker', 'flicker'], ['difference', REF ? 'difference (boxes)' : 'difference']], function (v) {
        var was = S.view;
        S.view = v;
        // The verdict panel is of the run shown (B only on `b`): it
        // follows the select. Its draft is kept per run, so nothing typed
        // is lost.
        if (S.tab === 'verdict' && (was === 'b') !== (v === 'b')) renderPanel();
      }));
    }
  }

  // ---- images -------------------------------------------------------------------

  function img(sc, name) {
    if (!name || !sc.images[name]) return null;
    var c = sc.imgs[name];
    if (!c) {
      c = new Image();
      c.onload = function () { draw(); };
      c.src = sc.images[name];
      sc.imgs[name] = c;
    }
    return c.complete && c.naturalWidth ? c : null;
  }

  function pixels(image) {
    var c = document.createElement('canvas');
    c.width = image.naturalWidth; c.height = image.naturalHeight;
    var x = c.getContext('2d');
    x.drawImage(image, 0, 0);
    return x.getImageData(0, 0, c.width, c.height);
  }

  // ---- the stage ------------------------------------------------------------------

  function moving(sc, o) {
    return o.mot || o.emot || Math.abs(o.vx - o.lx) > 0.01 || Math.abs(o.vy - o.ly) > 0.01 ||
      Math.abs(o.vw - o.lw) > 0.01 || Math.abs(o.vh - o.lh) > 0.01;
  }

  function colorOf(sc, o) {
    if (sc.failKeys.has(o.key)) return css('--fail');
    if (o.auth) return css('--auth');
    if ((o.emot & GLIDE) || moving(sc, o)) return css('--derived');
    return '#9a9aa6';
  }

  // Draws surface `sid` of `sc` at frame `i` into a new canvas.
  function surfaceCanvas(sc, i, sid, other) {
    var scr = sc.tl.screens[i].get(sid);
    var scale = scr.scale || 1;
    var z = Z;
    var size = scr.size || [1, 1];
    var W = Math.max(1, Math.ceil(size[0] * scale)), H = Math.max(1, Math.ceil(size[1] * scale));
    var cv = el('canvas', { width: Math.max(1, Math.round(W * z)), height: Math.max(1, Math.round(H * z)) });
    var x = cv.getContext('2d');
    x.imageSmoothingEnabled = false;
    var im = img(sc, scr.png);
    var refDiff = S.view === 'difference' && other && REF;
    if (S.view === 'difference' && other && !REF) {
      // A against B, pixel by pixel.
      var j = at(other, sc.tl.ks[i]);
      var os = j >= 0 ? other.tl.screens[j].get(sid) : null;
      var im2 = os ? img(other, os.png) : null;
      if (im && im2) {
        var pa = pixels(im), pb = pixels(im2);
        var out = x.createImageData(pa.width, pa.height);
        for (var p = 0; p < pa.data.length; p += 4) {
          var d = Math.max(Math.abs(pa.data[p] - pb.data[p]), Math.abs(pa.data[p + 1] - pb.data[p + 1]),
            Math.abs(pa.data[p + 2] - pb.data[p + 2]), Math.abs(pa.data[p + 3] - pb.data[p + 3]));
          out.data[p] = d ? 255 : 30; out.data[p + 1] = d ? 64 : 30; out.data[p + 2] = d ? 64 : 34;
          out.data[p + 3] = 255;
        }
        var tmp = document.createElement('canvas');
        tmp.width = pa.width; tmp.height = pa.height;
        tmp.getContext('2d').putImageData(out, 0, 0);
        x.drawImage(tmp, 0, 0, W * z, H * z);
      } else {
        x.fillStyle = '#333'; x.fillRect(0, 0, W * z, H * z);
        x.fillStyle = '#ccc'; x.font = '11px sans-serif';
        x.fillText('no images for both runs at this tick', 6, 14);
      }
      return cv;
    }
    if (im) {
      x.drawImage(im, 0, 0, W * z, H * z);
    } else {
      x.strokeStyle = '#9a9aa6';
      x.lineWidth = 1;
      scr.rows.forEach(function (r) {
        var o = obj(sc, r);
        if (o.hid === 'opacity0' || o.hid === 'scale0') return;
        x.globalAlpha = Math.max(0.25, Math.min(1, o.op));
        x.strokeRect(o.vx * scale * z + 0.5, o.vy * scale * z + 0.5, Math.max(1, o.vw * scale * z - 1), Math.max(1, o.vh * scale * z - 1));
      });
      x.globalAlpha = 1;
    }
    // Onion skin.
    for (var b = S.onion; b >= 1; b--) {
      var pi = i - b;
      if (pi < 0) continue;
      var ps = sc.tl.screens[pi].get(sid);
      if (!ps) continue;
      var alpha = 0.45 * (1 - (b - 1) / (S.onion + 1));
      if (S.onionMode === 'ghost') {
        var pim = img(sc, ps.png);
        if (pim) { x.globalAlpha = alpha * 0.6; x.drawImage(pim, 0, 0, W * z, H * z); x.globalAlpha = 1; }
      } else {
        x.globalAlpha = alpha;
        ps.rows.forEach(function (r) {
          var o = obj(sc, r);
          if (!(S.sel.has(o.key) || sc.failKeys.has(o.key) || moving(sc, o))) return;
          x.strokeStyle = colorOf(sc, o);
          x.setLineDash([]);
          x.strokeRect(o.vx * scale * z + 0.5, o.vy * scale * z + 0.5, Math.max(1, o.vw * scale * z - 1), Math.max(1, o.vh * scale * z - 1));
        });
        x.globalAlpha = 1;
      }
    }
    // Diff to the previous frame.
    if (S.diff && i > 0) {
      var prev = sc.tl.screens[i - 1].get(sid);
      var pim2 = prev ? img(sc, prev.png) : null;
      if (im && pim2 && pim2.naturalWidth === im.naturalWidth && pim2.naturalHeight === im.naturalHeight) {
        var c1 = pixels(im), c0 = pixels(pim2);
        var heat = x.createImageData(c1.width, c1.height);
        for (var q = 0; q < c1.data.length; q += 4) {
          var dd = Math.max(Math.abs(c1.data[q] - c0.data[q]), Math.abs(c1.data[q + 1] - c0.data[q + 1]),
            Math.abs(c1.data[q + 2] - c0.data[q + 2]), Math.abs(c1.data[q + 3] - c0.data[q + 3]));
          heat.data[q] = 255; heat.data[q + 1] = 0; heat.data[q + 2] = 0;
          heat.data[q + 3] = dd ? Math.min(220, 60 + dd) : 0;
        }
        var hc = document.createElement('canvas');
        hc.width = c1.width; hc.height = c1.height;
        hc.getContext('2d').putImageData(heat, 0, 0);
        x.drawImage(hc, 0, 0, W * z, H * z);
      }
    }
    if (refDiff) refBoxes(x, sc, i, sid, other, scale, z);
    // Boxes.
    scr.rows.forEach(function (r) {
      if (refDiff) return;
      var o = obj(sc, r);
      var show = S.all || S.sel.has(o.key) || sc.failKeys.has(o.key) || moving(sc, o);
      if (!show) return;
      var col = colorOf(sc, o);
      x.strokeStyle = col;
      x.lineWidth = S.sel.has(o.key) ? 2 : 1;
      if (S.laid) {
        x.setLineDash([4, 3]);
        x.globalAlpha = 0.8;
        x.strokeRect(o.lx * scale * z + 0.5, o.ly * scale * z + 0.5, Math.max(1, o.lw * scale * z - 1), Math.max(1, o.lh * scale * z - 1));
      }
      if (S.boxes) {
        x.setLineDash([]);
        x.globalAlpha = 1;
        x.strokeRect(o.vx * scale * z + 0.5, o.vy * scale * z + 0.5, Math.max(1, o.vw * scale * z - 1), Math.max(1, o.vh * scale * z - 1));
      }
      x.globalAlpha = 1;
      x.setLineDash([]);
    });
    cv.addEventListener('mousemove', function (ev) { hover(ev, sc, i, sid, cv); });
    cv.addEventListener('mouseleave', function () { $('tip').hidden = true; });
    cv.addEventListener('click', function (ev) {
      var o = hit(ev, sc, i, sid, cv);
      if (!o) return;
      if (S.sel.has(o.key)) S.sel.delete(o.key); else S.sel.add(o.key);
      draw();
    });
    return cv;
  }

  // A against its reference, which holds boxes and no pixels: each node
  // drawn other than the reference drew it at this tick, A's box solid
  // red and the reference's dashed; a node only one side drew, its box
  // alone.
  function refBoxes(x, sc, i, sid, ref, scale, z) {
    var j = at(ref, sc.tl.ks[i]);
    var rs = j >= 0 ? ref.tl.screens[j].get(sid) : null;
    var mine = {}, theirs = {};
    sc.tl.screens[i].get(sid).rows.forEach(function (r) { var o = obj(sc, r); mine[o.key] = o; });
    if (rs) rs.rows.forEach(function (r) { var o = obj(ref, r); theirs[o.key] = o; });
    function shown(o) { return o && o.hid !== 'opacity0' && o.hid !== 'scale0'; }
    function differs(a, b) {
      if (shown(a) !== shown(b)) return true;
      if (!shown(a)) return false;
      return Math.abs(a.vx - b.vx) > 0.5 || Math.abs(a.vy - b.vy) > 0.5 || Math.abs(a.vw - b.vw) > 0.5 ||
        Math.abs(a.vh - b.vh) > 0.5 || Math.abs(a.op - b.op) > 1 / 255;
    }
    function box(o, col, dash) {
      x.strokeStyle = col;
      x.lineWidth = 2;
      x.setLineDash(dash);
      x.strokeRect(o.vx * scale * z + 1, o.vy * scale * z + 1, Math.max(1, o.vw * scale * z - 2), Math.max(1, o.vh * scale * z - 2));
    }
    var keys = {};
    Object.keys(mine).forEach(function (k) { keys[k] = 1; });
    Object.keys(theirs).forEach(function (k) { keys[k] = 1; });
    Object.keys(keys).forEach(function (k) {
      var a = mine[k], b = theirs[k];
      if (!differs(a, b)) return;
      if (shown(b)) box(b, css('--changed'), [4, 3]);
      if (shown(a)) box(a, css('--fail'), []);
    });
    x.setLineDash([]);
    x.lineWidth = 1;
  }

  function hit(ev, sc, i, sid, cv) {
    var rect = cv.getBoundingClientRect();
    var scale = sc.tl.screens[i].get(sid).scale || 1;
    var lx = (ev.clientX - rect.left) * (cv.width / rect.width) / Z / scale;
    var ly = (ev.clientY - rect.top) * (cv.height / rect.height) / Z / scale;
    var best = null, area = Infinity;
    sc.tl.screens[i].get(sid).rows.forEach(function (r) {
      var o = obj(sc, r);
      if (lx < o.vx || ly < o.vy || lx > o.vx + o.vw || ly > o.vy + o.vh) return;
      var a = o.vw * o.vh;
      if (a < area) { area = a; best = o; }
    });
    return best;
  }

  function parts(sc, o) {
    var out = [];
    var parent = sc.trace.nodes[o.key] && sc.trace.nodes[o.key].parent;
    function add(px, py, what) {
      if (px) out.push('x ' + (px > 0 ? '+' : '') + fmt(px) + ' px ' + what);
      if (py) out.push('y ' + (py > 0 ? '+' : '') + fmt(py) + ' px ' + what);
    }
    add(o.ix, o.iy, 'inherited' + (parent ? ' from ' + label(sc, parent) : ''));
    add(o.fx, o.fy, 'of it in flight');
    add(o.wx, o.wy, 'written (own)');
    add(o.spx, o.spy, 'spring (own)');
    add(o.gx, o.gy, 'GLIDE (own)');
    add(o.mx, o.my, 'morph (own)');
    return out;
  }

  function hover(ev, sc, i, sid, cv) {
    var o = hit(ev, sc, i, sid, cv);
    var tip = $('tip');
    if (!o) { tip.hidden = true; return; }
    var m = sc.trace.nodes[o.key] || {};
    tip.textContent = '';
    tip.appendChild(el('div', null, el('b', { text: m.label || o.key }), m.origin ? ' ' + m.origin : ''));
    tip.appendChild(el('div', { cls: 'mono', text: o.key }));
    tip.appendChild(el('div', { text: 'drawn ' + [o.vx, o.vy, o.vw, o.vh].map(function (v) { return fmt(v); }).join(', ') +
      ' · laid ' + [o.lx, o.ly, o.lw, o.lh].map(function (v) { return fmt(v); }).join(', ') }));
    tip.appendChild(el('div', { text: 'opacity ' + fmt(o.op, 3) + ' (own ' + fmt(o.oo, 3) + ') · scale ' + fmt(o.sx, 3) + ', ' + fmt(o.sy, 3) +
      (o.rot ? ' · rotate ' + fmt(o.rot) : '') + (o.hid ? ' · hidden: ' + o.hid : '') + (o.txt ? ' · text ' + ({ S: 'shaped', E: 'by its estimate', I: 'stand-in' }[o.txt] || o.txt) : '') }));
    var own = names(o.mot, MOT), inh = names(o.emot & ~o.mot, MOT);
    tip.appendChild(el('div', { text: 'motions: ' + (own.join(' ') || '-') + (inh.length ? ' · inherited: ' + inh.join(' ') : '') }));
    tip.appendChild(el('div', { text: 'authored: ' + (names(o.auth, AX).join(' ') || '-') }));
    var ps = parts(sc, o);
    if (ps.length) tip.appendChild(el('div', { text: 'offset: ' + ps.join('; ') }));
    tip.hidden = false;
    var x = ev.clientX + 14, y = ev.clientY + 14;
    var w = tip.offsetWidth, hgt = tip.offsetHeight;
    if (x + w > window.innerWidth - 8) x = Math.max(8, ev.clientX - w - 14);
    if (y + hgt > window.innerHeight - 8) y = Math.max(8, ev.clientY - hgt - 14);
    tip.style.left = x + 'px';
    tip.style.top = y + 'px';
  }

  function stageOf(sc, i, other, title) {
    var box = el('div');
    if (title) box.appendChild(el('div', { cls: 'surface-label', text: title }));
    if (!sc || i < 0 || i >= sc.tl.screens.length) {
      box.appendChild(el('div', { cls: 'surface-label', text: 'no frame' }));
      return box;
    }
    var info = {};
    sc.trace.surfaces.forEach(function (s) { info[s.id] = s; });
    sc.tl.screens[i].forEach(function (v, sid) {
      var s = info[sid];
      box.appendChild(el('div', { cls: 'surface-label', text: (s ? s.label : 'surface ' + sid) + (v.png && !sc.images[v.png] ? ' (wireframe)' : '') }));
      box.appendChild(surfaceCanvas(sc, i, sid, other));
    });
    return box;
  }

  var flickTimer = null;
  function renderStage() {
    var st = $('stage');
    st.textContent = '';
    var a = A(), b = B();
    if (!a) return;
    var ka = a.tl.ks[S.i];
    var ib = b ? at(b, ka) : -1;
    // A scenario B does not have (a run that matches its reference) shows A.
    var view = b ? S.view : 'a';
    Z = fitZoom(st, a, b, view);
    if (flickTimer && view !== 'flicker') { clearInterval(flickTimer); flickTimer = null; }
    if (!AB || view === 'a') st.appendChild(stageOf(a, S.i, b, AB && b ? 'A: ' + DATA.runs[0].label : null));
    else if (view === 'b') st.appendChild(stageOf(b, ib, a, 'B: ' + DATA.runs[1].label));
    else if (view === 'side') {
      var cols = el('div', { cls: 'cols' });
      cols.appendChild(stageOf(a, S.i, b, 'A: ' + DATA.runs[0].label));
      cols.appendChild(stageOf(b, ib, a, 'B: ' + DATA.runs[1].label));
      st.appendChild(cols);
    } else if (view === 'flicker') {
      st.appendChild(S.flickB ? stageOf(b, ib, a, 'B: ' + DATA.runs[1].label) : stageOf(a, S.i, b, 'A: ' + DATA.runs[0].label));
      if (!flickTimer) flickTimer = setInterval(function () { S.flickB = !S.flickB; renderStage(); }, 600);
    } else if (view === 'difference') {
      st.appendChild(stageOf(a, S.i, b, REF ? 'A against B: where A (solid red) drew a node other than the reference (dashed)' : 'A against B: changed pixels red'));
    }
  }

  // The widest surface of a scenario over its whole run, in pixels, so
  // a fitted zoom does not change while scrubbing.
  function widest(sc) {
    if (!sc) return 1;
    if (sc.widest) return sc.widest;
    var w = 1;
    sc.tl.screens.forEach(function (scr) {
      scr.forEach(function (v) { if (v.size) w = Math.max(w, Math.ceil(v.size[0] * (v.scale || 1))); });
    });
    sc.widest = w;
    return w;
  }

  // S.zoom, with 'fit' resolved against the stage's width. Side by side
  // puts the two in columns when each still gets 2x; otherwise (a wide
  // bar) they stack, each at the whole width.
  function fitZoom(st, a, b, view) {
    if (S.zoom !== 'fit') return S.zoom;
    var room = Math.max(120, st.clientWidth - 22);
    var w = Math.max(widest(a), b ? widest(b) : 1);
    var z = room / w;
    if (view === 'side' && b && (room - 14) / 2 / w >= 2) z = (room - 14) / 2 / w;
    return z >= 1 ? Math.min(8, Math.floor(z)) : Math.max(0.25, z);
  }

  // ---- scrubber -------------------------------------------------------------------

  function renderScrub() {
    var a = A();
    var n = a ? a.tl.ks.length : 0;
    var sl = $('slider');
    sl.max = String(Math.max(0, n - 1));
    sl.value = String(S.i);
    var tr = $('track');
    tr.textContent = '';
    if (!a || !n) { $('frame-label').textContent = 'no frames'; return; }
    function pos(i) { return (n > 1 ? i / (n - 1) * 100 : 0) + '%'; }
    function mark(i, cls, title) {
      if (i < 0) i = 0;
      tr.appendChild(el('span', { cls: 'm ' + cls, title: title, style: 'left:' + pos(i), onclick: function () { setFrame(i); } }));
    }
    a.trace.segments.forEach(function (s) {
      if (s.s !== null && s.s !== undefined) mark(at(a, s.s) + 1, 'seg', 'segment ' + s.name + ' (k' + s.s + ' to k' + s.e + ')');
    });
    a.trace.events.forEach(function (e) { mark(at(a, e.k), 'ev', 'k' + e.k + ' line ' + e.line + ': ' + e.text); });
    a.failTicks.forEach(function (k) { mark(at(a, k), 'bad', 'failing at k' + k); });
    mark(S.i, 'cur', 'this frame');
    var f = a.tl.frames[S.i];
    var k = a.tl.ks[S.i];
    var seg = segmentOf(a, k);
    $('frame-label').textContent = 'frame ' + (S.i + 1) + '/' + n + ' · k' + k + ' · ' +
      fmt((f.t_ns - 1e9) / 1e6, 1) + ' ms' + (seg ? ' · ' + seg.name : '') +
      (f.k !== k ? ' · surface detached' : '') + (f.held_text && f.held_text.length ? ' · text held' : '');
  }

  function segmentOf(sc, k) {
    var segs = sc.trace.segments, found = null;
    segs.forEach(function (s) {
      var lo = s.s === null || s.s === undefined ? -1 : s.s;
      if (k > lo && k <= s.e) found = s;
    });
    return found;
  }

  // ---- filmstrip --------------------------------------------------------------------

  function renderFilm() {
    var film = $('film');
    film.textContent = '';
    var a = A();
    if (!a || !a.tl.ks.length) return;
    var seg = segmentOf(a, a.tl.ks[S.i]);
    var lo = 0, hi = a.tl.ks.length - 1;
    if (seg) {
      lo = seg.s === null || seg.s === undefined ? 0 : Math.max(0, at(a, seg.s));
      hi = at(a, seg.e);
    }
    if (hi - lo > 160) { lo = Math.max(lo, S.i - 80); hi = Math.min(hi, lo + 160); }
    for (var i = lo; i <= hi; i++) {
      (function (i) {
        var scr = null, sid = null;
        a.tl.screens[i].forEach(function (v, s) { if (scr === null) { scr = v; sid = s; } });
        var c = el('canvas', { title: 'k' + a.tl.ks[i] });
        var hgt = 36;
        if (!scr) { c.width = 40; c.height = hgt; film.appendChild(c); return; }
        var scale = scr.scale || 1;
        var W = scr.size[0] * scale, H = scr.size[1] * scale;
        // The whole surface, at most 240 by 36: a wide bar is shrunk,
        // not cut off on the right.
        var f = Math.min(hgt / Math.max(1, H), 240 / Math.max(1, W));
        c.width = Math.max(8, Math.round(W * f));
        c.height = Math.max(4, Math.round(H * f));
        var x = c.getContext('2d');
        x.imageSmoothingEnabled = false;
        var im = img(a, scr.png);
        if (im) x.drawImage(im, 0, 0, W * f, H * f);
        else {
          x.strokeStyle = '#777';
          scr.rows.forEach(function (r) {
            var o = obj(a, r);
            x.strokeRect(o.vx * scale * f, o.vy * scale * f, o.vw * scale * f, o.vh * scale * f);
          });
        }
        if (a.failTicks.has(a.tl.ks[i])) c.classList.add('bad');
        if (i === S.i) c.classList.add('cur');
        c.addEventListener('click', function () { setFrame(i); });
        film.appendChild(c);
        void sid;
      })(i);
    }
    var cur = film.querySelector('.cur');
    if (cur && cur.scrollIntoView) cur.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }

  // ---- panels ---------------------------------------------------------------------------

  var TABS = [['findings', 'Findings'], ['nodes', 'Nodes'], ['curves', 'Curves'], ['cost', 'Cost']];
  if (AB) TABS.push(['ab', 'A/B']);
  TABS.push(['verdict', 'Approve']);
  if (!TABS.some(function (t) { return t[0] === S.tab; })) S.tab = 'findings';

  function setTab(t) {
    S.tab = t;
    store.set('strand-eval:tab', t);
    renderTabs();
    renderPanel();
  }

  function renderTabs() {
    var tabs = $('tabs');
    tabs.textContent = '';
    TABS.forEach(function (t) {
      tabs.appendChild(el('button', { type: 'button', role: 'tab', 'aria-selected': S.tab === t[0] ? 'true' : 'false', text: t[1], onclick: function () { setTab(t[0]); } }));
    });
  }

  function describe(sc, f) {
    var s = f.check;
    if (f.segment) s += ' [' + f.segment + ']';
    if (f.key) {
      var m = sc.trace.nodes[f.key];
      s += ' ' + (m ? m.label : f.key);
      if (m && m.origin) s += ' (' + m.origin + ')';
    }
    if (f.k !== null && f.k !== undefined) s += ' k' + f.k;
    if (f.axis) s += ' ' + f.axis;
    if (f.got !== null && f.got !== undefined && f.bound) s += ' ' + fmt(f.got) + ' not in [' + fmt(f.bound[0]) + ', ' + fmt(f.bound[1]) + ']';
    if (f.more) s += ' (+' + f.more + ' ticks)';
    return s + ': ' + f.msg;
  }

  function jump(sc, f) {
    if (f.key) S.sel.add(f.key);
    if (f.k !== null && f.k !== undefined) setFrame(Math.max(0, at(sc, f.k)));
    else draw();
  }

  function findingsPanel(p) {
    var a = A();
    if (!a) return;
    if (!a.checks) { p.appendChild(el('div', { cls: 'note', text: 'Traced, not checked (strand-eval run).' })); }
    else {
      var t = el('table');
      a.checks.results.forEach(function (r) {
        var st = r.status.status;
        t.appendChild(el('tr', null, el('td', { text: r.check }), el('td', { cls: 'st-' + st, text: st.toUpperCase() }), el('td', { cls: 'note', text: r.status.why || '' })));
      });
      p.appendChild(t);
      var list = el('div');
      a.checks.results.forEach(function (r) {
        r.findings.forEach(function (f) {
          if (f.sev === 'info') return;
          var sheet = (a.sheets || []).filter(function (s) { return s.check === f.check && s.k === f.k && s.key === f.key; })[0];
          // A reference finding reads as CHANGED (its chip's colour), not as a check's failure.
          var tone = f.sev === 'fail' && f.check !== 'reference' ? 'fail' : 'changed';
          var d = el('div', { cls: 'find st-' + tone, onclick: function () { jump(a, f); } }, describe(a, f));
          if (sheet) {
            d.appendChild(document.createTextNode(' '));
            d.appendChild(el('a', { href: sheet.path, target: '_blank', text: 'contact sheet', onclick: function (e) { e.stopPropagation(); } }));
          }
          list.appendChild(d);
        });
      });
      if (list.childNodes.length) { p.appendChild(el('h3', { text: 'Findings' })); p.appendChild(list); }
    }
    if (a.trace.expects.length) {
      p.appendChild(el('h3', { text: 'expect steps' }));
      a.trace.expects.forEach(function (e) { p.appendChild(el('div', { cls: 'mono', text: 'line ' + e.line + ' (k' + e.k + '): ' + e.text })); });
    }
    if (a.trace.commands.length) {
      p.appendChild(el('h3', { text: 'Commands the shell ran (recorded, not executed)' }));
      a.trace.commands.forEach(function (c) { p.appendChild(el('div', { cls: 'mono', text: 'k' + c.k + ': ' + c.text })); });
    }
  }

  function nodesPanel(p) {
    var a = A();
    if (!a) return;
    var c = el('input', { type: 'checkbox' });
    c.checked = S.failOnly;
    c.addEventListener('change', function () { S.failOnly = c.checked; renderPanel(); });
    p.appendChild(el('label', { cls: 'field' }, c, ' failing nodes only'));
    p.appendChild(el('div', { cls: 'note', text: 'Click a node to select it (its curves and outline). Tags: hidden reason, own motions, text drawn shaped (S), by its estimate (E) or a stand-in (I).' }));
    var tree = el('div', { cls: 'tree' });
    var rows = rowsAt(a, S.i).map(function (r) { return obj(a, r); });
    var present = {};
    rows.forEach(function (o) { present[o.key] = o; });
    function depth(key) {
      var d = 0, k = key, seen = {};
      while (a.trace.nodes[k] && a.trace.nodes[k].parent && !seen[k]) { seen[k] = 1; k = a.trace.nodes[k].parent; if (present[k]) d++; }
      return d;
    }
    rows.forEach(function (o) {
      if (S.failOnly && !a.failKeys.has(o.key)) return;
      var tags = [];
      if (o.hid) tags.push(o.hid);
      var ms = names(o.mot, MOT);
      if (ms.length) tags.push(ms.join('+'));
      if (o.txt) tags.push(o.txt);
      if (o.op < 0.999) tags.push('op ' + fmt(o.op, 2));
      var line = el('div', {
        cls: (S.sel.has(o.key) ? 'sel ' : '') + (a.failKeys.has(o.key) ? 'bad' : ''),
        title: o.key,
        onclick: function () { if (S.sel.has(o.key)) S.sel.delete(o.key); else S.sel.add(o.key); draw(); }
      }, ' '.repeat(2 * depth(o.key)) + label(a, o.key) + ' ', el('span', { cls: 'tag', text: tags.join(' · ') }));
      tree.appendChild(line);
    });
    p.appendChild(tree);
  }

  var AXES_CURVE = [['x', 'vx', 'lx'], ['y', 'vy', 'ly'], ['w', 'vw', 'lw'], ['h', 'vh', 'lh'], ['opacity', 'op', null], ['scale', 'sx', null]];
  // The series colours, theme tokens (--c1 to --c6) so dark mode lightens them.
  function hues() { return [1, 2, 3, 4, 5, 6].map(function (n) { return css('--c' + n); }); }
  // A chart's width in its own units: the panel's, within 300 to 640, so
  // its 10 px labels are not shrunk on a phone.
  function chartWidth(p) { return Math.round(Math.min(640, Math.max(300, p.clientWidth || 640))); }

  function curvesPanel(p) {
    var a = A(), b = B();
    if (!a || !a.tl.ks.length) return;
    var keys = Array.from(S.sel);
    if (!keys.length) keys = Array.from(a.failKeys).slice(0, 3);
    var scope = el('select');
    [['segment', 'this segment'], ['run', 'the whole run']].forEach(function (o) {
      var op = el('option', { value: o[0], text: o[1] });
      if (o[0] === S.scope) op.selected = true;
      scope.appendChild(op);
    });
    scope.addEventListener('change', function () { S.scope = scope.value; renderPanel(); });
    p.appendChild(el('label', { cls: 'field' }, 'over ', scope));
    if (!keys.length) { p.appendChild(el('div', { cls: 'note', text: 'Select a node (click it on the stage or in Nodes) to draw its curves.' })); return; }
    var lo = 0, hi = a.tl.ks.length - 1;
    var seg = segmentOf(a, a.tl.ks[S.i]);
    if (S.scope === 'segment' && seg) {
      lo = seg.s === null || seg.s === undefined ? 0 : Math.max(0, at(a, seg.s));
      hi = Math.max(lo, at(a, seg.e));
    }
    var HUES = hues();
    var legend = el('div', { cls: 'legend' });
    keys.forEach(function (k, n) {
      legend.appendChild(el('span', { style: 'color:' + HUES[n % HUES.length], text: '■ ' + label(a, k) }));
    });
    legend.appendChild(el('span', { cls: 'note', text: 'solid drawn · dashed laid' + (AB ? ' · B dotted' : '') + ' · red band: the bound a failing finding held it to · grey: text held' }));
    p.appendChild(legend);
    var W = chartWidth(p), H = 110, L = 44, R = 8, T = 8, Bm = 18;
    function xs(i) { return L + (hi > lo ? (i - lo) / (hi - lo) : 0) * (W - L - R); }
    AXES_CURVE.forEach(function (ax) {
      var series = [];
      var vmin = Infinity, vmax = -Infinity;
      function take(sc, key, col, iOf) {
        var pts = [], lpts = [];
        for (var i = lo; i <= hi; i++) {
          var j = iOf(i);
          var r = j >= 0 ? rowOf(sc, j, key) : null;
          if (!r) { pts.push(null); lpts.push(null); continue; }
          var v = r[sc.C[col[1]]];
          pts.push([i, v]);
          vmin = Math.min(vmin, v); vmax = Math.max(vmax, v);
          if (col[2]) { var lv = r[sc.C[col[2]]]; lpts.push([i, lv]); vmin = Math.min(vmin, lv); vmax = Math.max(vmax, lv); } else lpts.push(null);
        }
        return { pts: pts, lpts: lpts };
      }
      keys.forEach(function (k, n) {
        series.push({ s: take(a, k, ax, function (i) { return i; }), hue: HUES[n % HUES.length], dash: '' });
        if (b) series.push({ s: take(b, k, ax, function (i) { return at(b, a.tl.ks[i]); }), hue: HUES[n % HUES.length], dash: '1 3' });
      });
      var bands = [];
      a.fails.forEach(function (f) {
        if (!f.bound || keys.indexOf(f.key) < 0 || f.axis !== ax[0]) return;
        var sg = null;
        a.trace.segments.forEach(function (s) { if (s.name === f.segment) sg = s; });
        var i0 = sg && sg.s !== null && sg.s !== undefined ? at(a, sg.s) : lo;
        var i1 = sg ? at(a, sg.e) : hi;
        bands.push({ i0: Math.max(lo, i0), i1: Math.min(hi, i1), b: f.bound, k: f.k, got: f.got });
        vmin = Math.min(vmin, f.bound[0], f.got); vmax = Math.max(vmax, f.bound[1], f.got);
      });
      if (!isFinite(vmin)) return;
      if (vmax - vmin < 1e-6) { vmin -= 1; vmax += 1; }
      var pad = (vmax - vmin) * 0.08;
      vmin -= pad; vmax += pad;
      function ys(v) { return T + (1 - (v - vmin) / (vmax - vmin)) * (H - T - Bm); }
      var svg = '<svg class="chart" viewBox="0 0 ' + W + ' ' + H + '" role="img" aria-label="' + esc(ax[0]) + ' against frame">';
      // Held text.
      for (var i = lo; i <= hi; i++) {
        var fr = a.tl.frames[i];
        if (fr.held_text && fr.held_text.length) {
          svg += '<rect x="' + (xs(i) - 2) + '" y="' + T + '" width="4" height="' + (H - T - Bm) + '" style="fill:var(--held)"/>';
        }
      }
      bands.forEach(function (bd) {
        var y0 = ys(bd.b[1]), y1 = ys(bd.b[0]);
        svg += '<rect x="' + xs(bd.i0) + '" y="' + y0 + '" width="' + Math.max(1, xs(bd.i1) - xs(bd.i0)) + '" height="' + Math.max(1, y1 - y0) + '" style="fill:var(--band)"/>';
      });
      svg += '<line x1="' + L + '" y1="' + (H - Bm) + '" x2="' + (W - R) + '" y2="' + (H - Bm) + '" style="stroke:var(--line)"/>';
      svg += '<text x="2" y="' + (T + 8) + '">' + esc(fmt(vmax - pad, 1)) + '</text>';
      svg += '<text x="2" y="' + (H - Bm) + '">' + esc(fmt(vmin + pad, 1)) + '</text>';
      svg += '<text x="2" y="' + (H / 2) + '">' + esc(ax[0]) + '</text>';
      svg += '<text x="' + L + '" y="' + (H - 4) + '">k' + a.tl.ks[lo] + '</text>';
      svg += '<text x="' + (W - R - 40) + '" y="' + (H - 4) + '">k' + a.tl.ks[hi] + '</text>';
      function path(pts) {
        var d = '', pen = false;
        pts.forEach(function (p) {
          if (!p) { pen = false; return; }
          d += (pen ? 'L' : 'M') + xs(p[0]).toFixed(1) + ' ' + ys(p[1]).toFixed(1) + ' ';
          pen = true;
        });
        return d;
      }
      series.forEach(function (s) {
        var d = path(s.s.pts);
        if (d) svg += '<path d="' + d + '" fill="none" stroke="' + s.hue + '" stroke-width="1.5"' + (s.dash ? ' stroke-dasharray="' + s.dash + '"' : '') + '/>';
        var ld = path(s.s.lpts);
        if (ld && !s.dash) svg += '<path d="' + ld + '" fill="none" stroke="' + s.hue + '" stroke-width="1" stroke-dasharray="4 3" opacity="0.7"/>';
      });
      bands.forEach(function (bd) {
        if (bd.k === null || bd.k === undefined) return;
        var i = at(a, bd.k);
        if (i < lo || i > hi) return;
        svg += '<circle cx="' + xs(i) + '" cy="' + ys(bd.got) + '" r="3" fill="none" style="stroke:var(--fail)" stroke-dasharray="1 1.5"/>';
      });
      if (S.i >= lo && S.i <= hi) svg += '<line x1="' + xs(S.i) + '" y1="' + T + '" x2="' + xs(S.i) + '" y2="' + (H - Bm) + '" style="stroke:var(--ink)" opacity="0.5"/>';
      svg += '</svg>';
      var holder = el('div');
      holder.innerHTML = svg;
      var node = holder.firstChild;
      node.addEventListener('click', function (ev) {
        var r = node.getBoundingClientRect();
        var fx = (ev.clientX - r.left) / r.width * W;
        var i = Math.round(lo + (fx - L) / (W - L - R) * (hi - lo));
        setFrame(Math.max(lo, Math.min(hi, i)));
      });
      p.appendChild(node);
    });
  }

  function costPanel(p) {
    var a = A();
    if (!a || !a.trace.frames.length) return;
    p.appendChild(el('div', { cls: 'note', text: 'Per painted tick, summed over surfaces. The deterministic proxies (damage, raster, items, layout passes) can gate through [budget]; wall times are advisory and never gate.' }));
    var frames = a.trace.frames;
    function sum(f, k) {
      var t = 0;
      for (var s in f.cost) if (Object.prototype.hasOwnProperty.call(f.cost, s)) t += f.cost[s][k] || 0;
      return t;
    }
    var metrics = [['damage_px', 'damage px'], ['raster_px', 'raster px'], ['items', 'items'], ['layout_passes', 'layout passes']];
    var W = chartWidth(p), H = 46, L = 92;
    var curK = a.tl.ks[S.i];
    metrics.forEach(function (m) {
      var vals = frames.map(function (f) { return sum(f, m[0]); });
      var max = Math.max.apply(null, vals.concat([1]));
      var bw = (W - L) / Math.max(1, frames.length);
      var svg = '<svg class="chart" viewBox="0 0 ' + W + ' ' + H + '" role="img" aria-label="' + esc(m[1]) + '">';
      svg += '<text x="2" y="14">' + esc(m[1]) + '</text><text x="2" y="28">max ' + max + '</text>';
      vals.forEach(function (v, i) {
        var bh = v / max * (H - 6);
        var cur = frames[i].k === curK;
        svg += '<rect x="' + (L + i * bw).toFixed(1) + '" y="' + (H - 2 - bh).toFixed(1) + '" width="' + Math.max(0.6, bw - 0.4).toFixed(1) + '" height="' + bh.toFixed(1) + '" style="fill:' + (cur ? 'var(--ink)' : 'var(--accent)') + '" opacity="' + (cur ? 1 : 0.65) + '"/>';
      });
      if (m[0] === 'damage_px') {
        var wall = frames.map(function (f) { return sum(f, 'flatten_us') + sum(f, 'raster_us'); });
        var wmax = Math.max.apply(null, wall.concat([1]));
        var d = '';
        wall.forEach(function (v, i) { d += (i ? 'L' : 'M') + (L + (i + 0.5) * bw).toFixed(1) + ' ' + (H - 2 - v / wmax * (H - 6)).toFixed(1) + ' '; });
        svg += '<path d="' + d + '" fill="none" style="stroke:var(--muted)" stroke-width="1" opacity="0.45"/>';
        svg += '<text x="2" y="42">wall (advisory)</text>';
      }
      svg += '</svg>';
      var holder = el('div');
      holder.innerHTML = svg;
      p.appendChild(holder.firstChild);
    });
    var f = a.tl.frames[S.i];
    var t = el('table');
    t.appendChild(el('tr', null, el('th', { text: 'surface' }), el('th', { text: 'paints' }), el('th', { text: 'damage' }), el('th', { text: 'raster' }), el('th', { text: 'items' }), el('th', { text: 'records' }), el('th', { text: 'passes' }), el('th', { text: 'layout nodes' }), el('th', { text: 'flatten µs*' }), el('th', { text: 'raster µs*' })));
    for (var s in f.cost) {
      if (!Object.prototype.hasOwnProperty.call(f.cost, s)) continue;
      var c = f.cost[s];
      t.appendChild(el('tr', null, el('td', { text: s }), el('td', { text: String(c.paints) }), el('td', { text: String(c.damage_px) }), el('td', { text: String(c.raster_px) }), el('td', { text: String(c.items) }), el('td', { text: String(c.records) }), el('td', { text: String(c.layout_passes) }), el('td', { text: String(c.layout_nodes) }), el('td', { text: String(c.flatten_us) }), el('td', { text: String(c.raster_us) })));
    }
    p.appendChild(el('h3', { text: 'This tick (k' + f.k + '); * advisory' }));
    // Ten columns: on a phone the table scrolls in its own box, not the page.
    p.appendChild(el('div', { cls: 'scrollx' }, t));
  }

  function abPanel(p) {
    var a = scOf(0, S.id), b = scOf(1, S.id);
    var info = DATA.ab && DATA.ab.scenarios ? DATA.ab.scenarios[S.id] : null;
    // The reference side has no status of its own: it is not a run.
    var bSays = REF ? (b ? '' : ' (none differs)') : (b ? ' (' + b.status + ')' : ' (not run)');
    p.appendChild(el('div', { text: 'A: ' + DATA.runs[0].label + (a ? ' (' + a.status + ')' : ' (not run)') + ' · B: ' + DATA.runs[1].label + bSays }));
    if (!info) { p.appendChild(el('div', { cls: 'note', text: REF ? 'The run matches its approved reference, or has none: there is nothing to put beside it.' : 'Only one side ran this scenario.' })); return; }
    p.appendChild(el('div', { cls: 'note', text: info.same ? 'The two runs drew the same frames (equal digests).' : 'The first frame each node drew differently (beyond 0.5 px or 1/255 opacity), by key:' }));
    // Equal digests list nothing: no empty table under the note.
    if (!info.diverge.length) return;
    var t = el('table');
    t.appendChild(el('tr', null, el('th', { text: 'node' }), el('th', { text: 'k' }), el('th', { text: 'what' }), el('th', { text: 'A' }), el('th', { text: 'B' })));
    info.diverge.forEach(function (d) {
      var tr = el('tr', { cls: 'find', onclick: function () { S.sel.add(d.key); if (a && d.k !== null) setFrame(Math.max(0, at(a, d.k))); else draw(); } },
        el('td', { title: d.key, text: d.label + (d.uncertain ? ' (matched by sibling index: uncertain)' : '') }),
        el('td', { text: d.k === null ? '-' : 'k' + d.k }),
        el('td', { text: d.what }),
        el('td', { cls: 'mono', text: d.a }),
        el('td', { cls: 'mono', text: d.b }));
      t.appendChild(tr);
    });
    p.appendChild(t);
  }

  function quote(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'"; }

  // The approval step: the run's reference state and the command that
  // records it, Good as its approved reference, Bad as a known-bad
  // recording. A note is required either way.
  function verdictPanel(p) {
    var r = AB && !REF && S.view === 'b' ? 1 : 0;
    var sc = scOf(r, S.id) || A();
    if (!sc) return;
    var run = DATA.runs[DATA.runs[r] && scOf(r, S.id) ? r : 0];
    var refs = sc.refs;
    if (refs && refs.good) {
      p.appendChild(el('div', { text: 'Approved reference (' + refs.file + '): ' + refs.good.date + ', ' + refs.good.rev + ', by ' + refs.good.approver + ': ' + refs.good.note }));
    } else {
      p.appendChild(el('div', { cls: 'note', text: 'No approved reference for ' + sc.id + (refs ? ' (' + refs.file + ')' : '') + '.' }));
    }
    (refs ? refs.bad : []).forEach(function (b) {
      p.appendChild(el('div', { cls: 'note', text: 'Known bad: ' + b.date + ', ' + b.rev + ', by ' + b.approver + ': ' + b.note }));
    });
    var said = {
      match: 'The run matches it (within a quantum).',
      changed: 'The run left it (CHANGED). The A/B view puts the reference (B, wireframes) beside the run.',
      missing: 'The gate fails until it is approved.',
      bad: 'The run reproduces a known-bad recording.',
      skip: 'This run was not compared with it.'
    };
    if (refs && said[refs.status]) p.appendChild(el('div', { cls: refs.status === 'match' ? 'note' : 'warn', text: said[refs.status] }));
    if (refs && refs.diff.length) p.appendChild(el('pre', { cls: 'cmd', text: refs.diff.join('\n') }));
    var key = 'strand-eval:verdict:' + run.out + '/' + sc.id;
    var draft = {};
    try { draft = JSON.parse(store.get(key) || '{}') || {}; } catch (e) { draft = {}; }
    var cmd = el('pre', { cls: 'cmd' });
    var note = el('textarea', { rows: 3, placeholder: 'Why this motion is right (Good) or wrong (Bad): required, kept in this browser' });
    note.value = draft.note || '';
    // Who looked: kept per browser, not per run. Empty leaves it to
    // $STRAND_EVAL_APPROVER; `unknown` names nobody, as on the command line.
    var who = el('input', { type: 'text', cls: 'who', placeholder: 'Approver: who looked (empty: $STRAND_EVAL_APPROVER)', 'aria-label': 'Approver' });
    who.value = store.get('strand-eval:approver') || '';
    var whoNote = el('div', { cls: 'note' });
    function update() {
      store.set(key, JSON.stringify(draft));
      var n = (draft.note || '').trim();
      var w = who.value.trim();
      var named = w && w.toLowerCase() !== 'unknown';
      var by = named ? ' --approver ' + quote(w) : '';
      var at = run.out + '/' + sc.id;
      if (!draft.v) cmd.textContent = 'Pick Good (approve it as the reference) or Bad (record it as known bad).';
      else if (!n) cmd.textContent = 'Write a note: approving needs one, and the reference keeps it.';
      else if (draft.v === 'good') cmd.textContent = 'scripts/container/run.sh eval approve ' + at + ' --note ' + quote(n) + by;
      else cmd.textContent = 'scripts/container/run.sh eval verdict ' + at + ' bad --note ' + quote(n) + by;
      whoNote.textContent = named ? '' : (w ? '"unknown" names nobody: the command takes $STRAND_EVAL_APPROVER, and writes nothing without it.'
        : 'No approver typed: the command takes $STRAND_EVAL_APPROVER, and writes nothing without it.');
      copy.disabled = !(draft.v && n);
      good.className = 'btn' + (draft.v === 'good' ? ' on' : '');
      bad.className = 'btn' + (draft.v === 'bad' ? ' on' : '');
    }
    var good = el('button', { type: 'button', text: 'Good', title: 'Approve this run as the reference', onclick: function () { draft.v = 'good'; update(); } });
    var bad = el('button', { type: 'button', text: 'Bad', title: 'Record this run as a known-bad recording', onclick: function () { draft.v = 'bad'; update(); } });
    note.addEventListener('input', function () { draft.note = note.value; update(); });
    who.addEventListener('input', function () { store.set('strand-eval:approver', who.value); update(); });
    var copy = el('button', {
      type: 'button', cls: 'btn', text: 'Copy command', onclick: function () {
        var text = cmd.textContent;
        function fallback() {
          var ta = el('textarea');
          ta.value = text; document.body.appendChild(ta); ta.select();
          try { document.execCommand('copy'); } catch (e) { /* the user copies by hand */ }
          document.body.removeChild(ta);
        }
        if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).catch(fallback);
        else fallback();
        copy.textContent = 'Copied';
        setTimeout(function () { copy.textContent = 'Copy command'; }, 1200);
      }
    });
    p.appendChild(el('div', { cls: 'controls' }, good, bad));
    p.appendChild(note);
    p.appendChild(who);
    p.appendChild(cmd);
    p.appendChild(whoNote);
    p.appendChild(copy);
    p.appendChild(el('div', { cls: 'note', text: 'Good writes the run as the approved reference in ' + (refs ? refs.file : 'eval/refs/') + ' with the note, the date, the revision and the approver; a later run that leaves it is CHANGED and fails the gate. Bad records it as known bad: a run that comes back to it fails.' }));
    update();
  }

  function renderPanel() {
    var p = $('panel');
    p.textContent = '';
    ({ findings: findingsPanel, nodes: nodesPanel, curves: curvesPanel, cost: costPanel, ab: abPanel, verdict: verdictPanel }[S.tab] || findingsPanel)(p);
  }

  // ---- frames and playing -------------------------------------------------------------------

  function setFrame(i) {
    var a = A();
    if (!a) return;
    S.i = Math.max(0, Math.min(a.tl.ks.length - 1, i));
    draw();
  }

  function draw() {
    renderStage();
    renderScrub();
    renderFilm();
    if (S.tab !== 'verdict') renderPanel();
  }

  function renderAll() {
    $('run-title').textContent = DATA.title;
    renderList();
    renderHead();
    renderToolbar();
    renderTabs();
    renderPanel();
    draw();
  }

  var playTimer = null;
  function play(on) {
    S.playing = on;
    $('play').textContent = on ? 'Pause' : 'Play';
    if (playTimer) { clearTimeout(playTimer); playTimer = null; }
    if (!on) return;
    (function step() {
      var a = A();
      if (!a || S.i >= a.tl.ks.length - 1) { play(false); return; }
      var t0 = a.tl.frames[S.i].t_ns, t1 = a.tl.frames[S.i + 1].t_ns;
      var ms = Math.max(4, (t1 - t0) / 1e6 / S.speed);
      playTimer = setTimeout(function () { setFrame(S.i + 1); step(); }, ms);
    })();
  }

  // A fitted stage follows the window's width.
  var resizeTimer = null;
  window.addEventListener('resize', function () {
    if (S.zoom !== 'fit') return;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(function () { renderStage(); if (S.tab === 'curves' || S.tab === 'cost') renderPanel(); }, 120);
  });

  $('slider').addEventListener('input', function () { setFrame(+$('slider').value); });
  $('first').addEventListener('click', function () { setFrame(0); });
  $('last').addEventListener('click', function () { var a = A(); if (a) setFrame(a.tl.ks.length - 1); });
  $('prev').addEventListener('click', function (e) { setFrame(S.i - (e.shiftKey ? 10 : 1)); });
  $('next').addEventListener('click', function (e) { setFrame(S.i + (e.shiftKey ? 10 : 1)); });
  $('play').addEventListener('click', function () { play(!S.playing); });
  $('speed').addEventListener('change', function () { S.speed = +$('speed').value; });
  document.addEventListener('keydown', function (e) {
    var tag = (e.target && e.target.tagName) || '';
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
    var a = A();
    if (!a) return;
    if (e.key === 'ArrowLeft') { setFrame(S.i - (e.shiftKey ? 10 : 1)); e.preventDefault(); }
    else if (e.key === 'ArrowRight') { setFrame(S.i + (e.shiftKey ? 10 : 1)); e.preventDefault(); }
    else if (e.key === 'Home') { setFrame(0); e.preventDefault(); }
    else if (e.key === 'End') { setFrame(a.tl.ks.length - 1); e.preventDefault(); }
    else if (e.key === ' ') { play(!S.playing); e.preventDefault(); }
  });

  // Start on the first scenario that did not pass, else the first.
  var first = ids.filter(function (id) { return ['fail', 'error', 'xfail', 'changed'].indexOf(statusOf(id)) >= 0; })[0] || ids[0];
  if (first) pick(first); else renderAll();
})();
