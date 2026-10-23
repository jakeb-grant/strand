// (replay-clip) A clip of a glitch report's replay (docs/eval.md, "A
// clip"): these elements, these frames, where each was drawn and where
// it rests, for this time slice. The one place a clip is made: the
// viewer's page runs it on the trace it holds, `strand-eval replay clip`
// runs it (in QuickJS, eval/src/replay/clip.rs) on trace.json and
// fidelity.json, so both give the same text, JSON and frames for the
// same selection spec. No DOM and nothing loaded: every value comes from
// the recorded trace and the replay's fidelity, never from what a page
// happens to draw.
var StrandClip = (function () {
  'use strict';

  // Logical px kept around the selection when its frames are cropped.
  var MARGIN = 8;
  // A tick is in the slice within half a ms of its ends: the spec keeps
  // times to the ms, and ticks are 4 ms apart or more.
  var SLACK_MS = 0.5;
  // Rest boxes this close to the drawn box (1/128 px, under the trace's
  // quantum) read `same`.
  var SAME = 1 / 128;

  function Refusal(code, message) { this.code = code; this.message = message; }
  function refuse(code, message) { throw new Refusal(code, message); }

  // ---- the packed trace (docs/eval.md, "A replay's report, packed") -------

  // Each frame names the fields equal to the frame before's in `=`, and
  // its rows are runs of the frame before's (`[i, n]`), one of them with
  // columns changed (`{i, c}`) or whole rows. Unpacked in place; an
  // unchanged row is the same array. A trace that is not packed is left.
  function unpack(t) {
    if (!t.packed) return;
    var prev = {}, prevRows = [];
    t.frames.forEach(function (f) {
      (f['='] || []).forEach(function (k) { f[k] = prev[k]; });
      delete f['='];
      var rows = [];
      (f.n || []).forEach(function (e) {
        if (Array.isArray(e) && typeof e[0] === 'string') rows.push(e);
        else if (Array.isArray(e)) { for (var i = 0; i < e[1]; i++) rows.push(prevRows[e[0] + i]); }
        else {
          var r = prevRows[e.i].slice();
          for (var j = 0; j + 1 < e.c.length; j += 2) r[e.c[j]] = e.c[j + 1];
          rows.push(r);
        }
      });
      f.n = rows;
      prev = f; prevRows = rows;
    });
    delete t.packed;
  }

  // ---- small helpers --------------------------------------------------------

  // `v` to `d` decimals (3 by default), trailing zeros dropped.
  function num(v, d) {
    if (v === null || v === undefined || !isFinite(v)) return '-';
    var s = Number(v).toFixed(d === undefined ? 3 : d);
    if (s.indexOf('.') >= 0) s = s.replace(/0+$/, '').replace(/\.$/, '');
    return s === '-0' ? '0' : s;
  }
  // Seconds before the report, to the ms: `-3.200`.
  function secs(v) {
    var s = Number(v).toFixed(3);
    if (s === '-0.000') s = '0.000';
    return (v > 0 && s !== '0.000' ? '+' : '') + s;
  }
  function box(b) { return b.map(function (v) { return num(v); }).join(', '); }
  function quote(s) { return "'" + String(s).replace(/'/g, "'\\''") + "'"; }
  function names(bits, list) {
    var out = [];
    for (var i = 0; i < list.length; i++) if (bits & (1 << i)) out.push(list[i]);
    return out;
  }
  function sameFile(a, b) {
    return a === b || a.slice(-(b.length + 1)) === '/' + b || b.slice(-(a.length + 1)) === '/' + a;
  }
  // Two origins (`file:line:col`) of one place, either may carry a folder.
  function sameOrigin(a, b) { return !!a && !!b && sameFile(a, b); }
  // `notch.strand:241:3` → `notch.strand:241`.
  function sourceLine(origin) {
    var m = /^(.*):(\d+):(\d+)$/.exec(origin || '');
    return m ? m[1] + ':' + m[2] : (origin || '');
  }
  function base(path) {
    var s = String(path || '').replace(/\/+$/, '');
    return s.slice(s.lastIndexOf('/') + 1);
  }
  function pad(s, n) { while (s.length < n) s += ' '; return s; }

  // Where a node comes to rest: its laid box moved by the written `x`
  // and `y`, its own and its ancestors', without what is in flight (the
  // viewer's dashed box; viewer-owner).
  function rest(o) {
    var dx = (o.ix || 0) - (o.fx || 0) + (o.wx || 0), dy = (o.iy || 0) - (o.fy || 0) + (o.wy || 0);
    return [o.lx + dx, o.ly + dy, o.lw, o.lh];
  }

  // ---- the spec ---------------------------------------------------------------

  // A time: seconds before the report (`-3.2`, `-3.2s`, `-3200ms`) or a
  // tick (`k5226`), as seconds before the report to the ms.
  function timeOf(v, what, W, period) {
    var t = null;
    if (typeof v === 'number') t = v;
    else if (typeof v === 'string') {
      var s = v.trim(), m;
      if ((m = /^k(\d+)$/.exec(s))) t = +m[1] * period / 1e9 - W;
      else if ((m = /^([+-]?(?:\d+\.?\d*|\.\d+))(s|ms)?$/.exec(s))) t = m[2] === 'ms' ? +m[1] / 1000 : +m[1];
    }
    if (t === null || !isFinite(t)) {
      refuse('spec', what + ' ' + JSON.stringify(v) + ': seconds before the report (-3.2s, -3200ms) or a tick (k5226)');
    }
    t = Math.round(t * 1000) / 1000;
    return t === 0 ? 0 : t;
  }

  // The nodes a selector names: a key, `#id`, or a config place
  // (`file:line` or `file:line:col`), every node written there.
  function resolve(t, sel) {
    if (Object.prototype.hasOwnProperty.call(t.nodes, sel)) return [sel];
    var keys = Object.keys(t.nodes).sort(), m;
    var out = [];
    if (sel.charAt(0) === '#' && sel.length > 1) {
      var id = sel.slice(1);
      out = keys.filter(function (k) {
        var n = t.nodes[k];
        return n.id === id || (n.also || []).indexOf(id) >= 0;
      });
    } else if ((m = /^(.+?):(\d+)(?::(\d+))?$/.exec(sel))) {
      out = keys.filter(function (k) {
        var o = /^(.*):(\d+):(\d+)$/.exec(t.nodes[k].origin || '');
        return o && sameFile(o[1], m[1]) && o[2] === m[2] && (m[3] === undefined || o[3] === m[3]);
      });
    }
    if (!out.length) {
      refuse('unknown-node', 'no node ' + JSON.stringify(sel) + ' in this replay: name one by its key (the ' +
        'viewer\'s hover card, trace.json), by #id, or by its file:line in the config');
    }
    return out;
  }

  // ---- the walk ----------------------------------------------------------------

  // What shows at each step of the run (a painted frame, or a surface
  // going): each surface's rows by key, its frame's image, its size and
  // scale, as the viewer's timeline holds them (a surface's rows stand
  // until it paints again).
  function walk(t, C, each) {
    var cur = {};
    var gone = t.surfaces.filter(function (s) { return s.detached !== null && s.detached !== undefined; })
      .map(function (s) { return [s.detached, s.id]; })
      .sort(function (a, b) { return a[0] - b[0]; });
    var gi = 0, any = false;
    function detach(k, sid) {
      if (!any) return;
      delete cur[sid];
      each({ k: k, painted: [], gone: sid }, cur);
    }
    t.frames.forEach(function (f) {
      while (gi < gone.length && gone[gi][0] < f.k) { detach(gone[gi][0], gone[gi][1]); gi++; }
      Object.keys(cur).forEach(function (s) { if (!(String(s) in (f.sizes || {}))) delete cur[s]; });
      var scale = f.scale || t.scenario.scale || 1;
      (f.painted || []).forEach(function (s) {
        var prev = cur[s], fresh = f.png && f.png[s];
        var rows = {};
        (f.n || []).forEach(function (r) { if (r[C.s] === s) rows[r[0]] = r; });
        cur[s] = {
          rows: rows,
          png: fresh || (prev ? prev.png : null),
          size: f.sizes[s],
          scale: fresh || !prev ? scale : prev.scale,
          k: f.k
        };
      });
      Object.keys(cur).forEach(function (s) {
        var v = cur[s];
        if (f.sizes[s] && v.size !== f.sizes[s]) cur[s] = { rows: v.rows, png: v.png, size: f.sizes[s], scale: v.scale, k: v.k };
      });
      any = true;
      each({ k: f.k, painted: f.painted || [], gone: null }, cur);
    });
    while (gi < gone.length) { detach(gone[gi][0], gone[gi][1]); gi++; }
  }

  // ---- the clip ------------------------------------------------------------------

  // A source: { trace, checks, fidelity, motions, patches, file(name) }.
  // The viewer's page gives its own (`fromPage`); `strand-eval replay
  // clip` gives trace.json, checks.json and fidelity.json (`cli`).
  function make(src, spec) {
    var t = src.trace;
    unpack(t);
    var F = src.fidelity;
    if (!F || !F.window || !(F.window.seconds > 0)) {
      refuse('no-replay', 'a clip is of a glitch report\'s replay: this run has no fidelity.json window (re-run ' +
        '`strand replay <report>` with this strand-eval)');
    }
    var C = {};
    t.cols.forEach(function (c, i) { C[c] = i; });
    var MOT = src.motions || [];
    var W = F.window.seconds;
    var hz = (t.scenario && t.scenario.hz) || 60;
    var period = Math.floor(1e9 / hz);
    var from = timeOf(spec.from, '--from', W, period);
    var to = spec.to === undefined || spec.to === null || spec.to === '' ? 0 : timeOf(spec.to, '--to', W, period);
    if (from < -W - 0.0005) refuse('spec', '--from ' + secs(from) + 's is before the window, which starts at ' + secs(-W) + ' s');
    if (to > 0.0005) refuse('spec', '--to ' + secs(to) + 's is after the report (0 s)');
    if (!(to > from)) refuse('empty-slice', 'the slice ' + secs(from) + ' s to ' + secs(to) + ' s is empty: --from must come before --to');
    var keys = [];
    (spec.nodes || []).forEach(function (sel) {
      resolve(t, String(sel)).forEach(function (k) { if (keys.indexOf(k) < 0) keys.push(k); });
    });
    var note = String(spec.note || '').trim();
    var fromMs = (from + W) * 1000, toMs = (to + W) * 1000;
    function ms(k) { return k * period / 1e6; }
    function tOf(k) { return Math.round((ms(k) / 1000 - W) * 1e4) / 1e4; }

    // The steps of the slice: the one shown at its start (the last before
    // it, held), then each painted frame and surface gone within it.
    var steps = [], held = null;
    function snap(st, cur) {
      var surf = {}, el = {};
      Object.keys(cur).forEach(function (s) {
        var v = cur[s];
        surf[s] = { png: v.png, size: v.size, scale: v.scale, k: v.k };
      });
      keys.forEach(function (key) {
        var row = null, sid = null;
        Object.keys(cur).forEach(function (s) { if (!row && cur[s].rows[key]) { row = cur[s].rows[key]; sid = +s; } });
        el[key] = row ? valueOf(row, sid) : null;
      });
      return { k: st.k, painted: st.painted, gone: st.gone, surf: surf, el: el };
    }
    function valueOf(r, sid) {
      var o = {};
      t.cols.forEach(function (c, i) { o[c] = r[i]; });
      var own = o.mot || 0;
      return {
        s: sid,
        drawn: [o.vx, o.vy, o.vw, o.vh],
        rest: rest(o),
        op: o.op,
        scale: [o.sx, o.sy],
        rot: o.rot || 0,
        moved: names(own, MOT),
        inherited: names((o.emot || 0) & ~own, MOT),
        hidden: o.hid || null
      };
    }
    walk(t, C, function (st, cur) {
      var at = ms(st.k);
      if (at < fromMs - SLACK_MS) { held = { st: st, cur: cur, snap: null }; held.snap = snap(st, cur); return; }
      if (at > toMs + SLACK_MS) return;
      steps.push(snap(st, cur));
    });
    if (held) { held.snap.held = true; steps.unshift(held.snap); }
    if (!steps.length) {
      refuse('empty-slice', 'nothing was drawn by ' + secs(to) + ' s: the replay\'s first frame comes later');
    }
    var k0 = Math.ceil(fromMs * 1e6 / period - 1e-6), k1 = Math.floor(toMs * 1e6 / period + 1e-6);
    var painted = steps.filter(function (s) { return !s.held && s.painted.length; }).length;
    var kFirst = steps[0].k, kLast = steps[steps.length - 1].k;

    var surfInfo = {};
    t.surfaces.forEach(function (s) { surfInfo[s.id] = s; });
    function surfName(sid) {
      var s = surfInfo[sid];
      return s ? s.label + ' (surface ' + sid + ')' : 'surface ' + sid;
    }
    function rootOrigin(sid) {
      var s = surfInfo[sid], m = s ? t.nodes[s.root] : null;
      return m && m.origin ? m.origin : (s ? (s.root.split('|')[1] || s.root) : null);
    }

    // Each element, frame by frame.
    var elements = keys.map(function (key, n) {
      var m = t.nodes[key] || {};
      var sids = [];
      var frames = steps.map(function (st) {
        var v = st.el[key];
        if (v && sids.indexOf(v.s) < 0) sids.push(v.s);
        var f = { k: st.k, t: tOf(st.k) };
        if (st.held) f.held = true;
        if (!v) { f.drawn = null; return f; }
        f.surface = v.s;
        f.drawn = v.drawn;
        f.rest = v.rest;
        f.opacity = v.op;
        f.scale = v.scale;
        if (v.rot) f.rotate = v.rot;
        f.moved_by = v.moved;
        f.inherited = v.inherited;
        f.hidden = v.hidden;
        return f;
      });
      return {
        n: n + 1,
        key: key,
        label: m.label || key,
        kind: m.kind || null,
        id: m.id || null,
        origin: m.origin || null,
        source: sourceLine(m.origin),
        surfaces: sids.map(function (s) { return { id: s, label: surfInfo[s] ? surfInfo[s].label : null, root: rootOrigin(s) }; }),
        frames: frames
      };
    });

    // The surfaces the clip is of: its elements', or every one shown.
    var sids = [];
    elements.forEach(function (e) { e.surfaces.forEach(function (s) { if (sids.indexOf(s.id) < 0) sids.push(s.id); }); });
    if (!keys.length) {
      steps.forEach(function (st) { Object.keys(st.surf).forEach(function (s) { if (sids.indexOf(+s) < 0) sids.push(+s); }); });
    }
    sids.sort(function (a, b) { return a - b; });

    var fid = fidelityOf(F, steps, sids, rootOrigin, from, to, tOf);
    var hints = hintsOf(src.checks, keys, kFirst, kLast, period, W, F);
    var crop = cropOf(steps, sids, keys, elements);
    var cli = 'strand replay clip ' + quote(F.bundle || '<report>') + ' --from ' + secs(from) + 's --to ' + secs(to) + 's' +
      keys.map(function (k) { return ' --node ' + quote(k); }).join('') + (note ? ' --note ' + quote(note) : '');

    var json = {
      clip: 1,
      spec: { report: base(F.bundle), from: from, to: to, nodes: keys, note: note },
      report: { name: base(F.bundle), words: F.words || '', bundle: F.bundle || null },
      slice: {
        from: from, to: to, from_k: k0, to_k: k1, ticks: Math.max(0, k1 - k0 + 1), period_ns: period,
        held_from: steps[0].held ? steps[0].k : null, painted: painted, steps: steps.map(function (s) { return s.k; })
      },
      elements: elements,
      fidelity: fid.json,
      hints: hints.json,
      crop: crop,
      cli: cli
    };

    // The text: compact, a row per element only where a value changes.
    var L = [];
    L.push('Strand replay clip: ' + JSON.stringify(F.words || '') + ' (report ' + base(F.bundle) + ')');
    L.push('slice: ' + secs(from) + ' s to ' + secs(to) + ' s before the report, ticks k' + k0 + ' to k' + k1 +
      ' (' + num(to - from) + ' s, ' + painted + ' frame' + (painted === 1 ? '' : 's') + ' painted' +
      (steps[0].held ? '; k' + steps[0].k + ', drawn ' + secs(tOf(steps[0].k)) + ' s, held at its start' : '') + ')');
    if (note) L.push('note: ' + note);
    L.push('fidelity:');
    fid.lines.forEach(function (l) { L.push('  ' + l); });
    if (hints.lines.length) {
      L.push(hints.label + ':');
      hints.lines.forEach(function (l) { L.push('  ' + l); });
    }
    if (!elements.length) L.push('elements: none chosen (name some with --node, or click them in the viewer)');
    else L.push('elements (' + elements.length + '): boxes are x, y, w, h in surface logical px; a row per change, its first and last frame');
    elements.forEach(function (e) {
      L.push('[' + e.n + '] ' + e.label + ' · ' + e.source + ' · ' +
        (e.surfaces.length ? 'on ' + e.surfaces.map(function (s) { return surfName(s.id); }).join(', ') : 'not drawn in this slice'));
      L.push('    key ' + e.key);
      tableOf(e).forEach(function (l) { L.push('    ' + l); });
    });
    L.push('regenerate: ' + cli);
    return { text: L.join('\n') + '\n', json: json };
  }

  // One row per run of equal values: frames, time, drawn, rest, opacity,
  // scale, what moves it.
  function tableOf(e) {
    var head = ['frames', 'time (s)', 'drawn', 'rest', 'opacity', 'scale', 'moved by'];
    var rows = [];
    var cur = null;
    function cells(f) {
      if (!f.drawn) return ['not drawn', '', '', '', ''];
      var same = f.rest.every(function (v, i) { return Math.abs(v - f.drawn[i]) <= SAME; });
      var sc = Math.abs(f.scale[0] - f.scale[1]) <= 1e-6 ? num(f.scale[0]) : num(f.scale[0]) + ', ' + num(f.scale[1]);
      var by = f.moved_by.map(function (m) { return m.toLowerCase(); }).join('+');
      var inh = f.inherited.map(function (m) { return m.toLowerCase(); }).join('+');
      var mv = (by || (inh ? '' : '-')) + (inh ? (by ? ' ' : '') + '(' + inh + ')' : '');
      if (f.rotate) mv += ' rotate ' + num(f.rotate);
      if (f.hidden) mv += ' hidden: ' + f.hidden;
      return [box(f.drawn), same ? 'same' : box(f.rest), num(f.opacity), sc, mv];
    }
    e.frames.forEach(function (f) {
      var c = cells(f), sig = JSON.stringify([f.drawn, f.rest, f.opacity, f.scale, f.rotate || 0, f.moved_by, f.inherited, f.hidden, f.surface]);
      if (cur && cur.sig === sig && !f.held) { cur.last = f; cur.n++; return; }
      cur = { sig: sig, first: f, last: f, n: 1, cells: c };
      rows.push(cur);
    });
    var lines = rows.map(function (r) {
      var a = r.first, b = r.last;
      var ks = a.k === b.k ? 'k' + a.k : 'k' + a.k + '..k' + b.k;
      if (a.held) ks += ' (held)';
      var ts = a.k === b.k ? secs(a.t) : secs(a.t) + '..' + secs(b.t);
      return [ks, ts].concat(r.cells);
    });
    var all = [head].concat(lines);
    var w = head.map(function (_, i) { return Math.max.apply(null, all.map(function (l) { return l[i].length; })); });
    return all.map(function (l) {
      return l.map(function (c, i) { return i === l.length - 1 ? c : pad(c, w[i]); }).join('  ').replace(/\s+$/, '');
    });
  }

  // What checked the slice's frames, honestly: the frame the verdict held
  // to the live shell's last frame, when it is in the slice, and how it
  // came out; every other frame is the replay's own, never compared; the
  // session's divergences in the slice; what the replay does not
  // reproduce.
  function fidelityOf(F, steps, sids, rootOrigin, from, to, tOf) {
    var V = F.verdict || null, lines = [], surfaces = [];
    function rect(r) { return '[' + (r || []).map(function (v) { return num(v, 1); }).join(', ') + ']'; }
    function differences(origin) {
      var out = [];
      (V.drifted || []).forEach(function (d) {
        if (!sameOrigin(d.surface, origin)) return;
        out.push(d.node + ': live ' + rect(d.live.box) + ' opacity ' + num(d.live.opacity, 2) + ', replay ' +
          rect(d.replay.box) + ' opacity ' + num(d.replay.opacity, 2) + ': ' + num(d.px, 1) + ' px off');
      });
      (V.live_only || []).forEach(function (d) {
        if (sameOrigin(d.surface, origin)) out.push(d.node + ': drawn live at ' + rect(d.live.box) + ', not in the replay');
      });
      (V.replay_only || []).forEach(function (d) {
        if (sameOrigin(d.surface, origin)) out.push(d.node + ': drawn in the replay at ' + rect(d.replay.box) + ', not live');
      });
      (V.pixels || []).forEach(function (p) {
        if (!sameOrigin(p.surface, origin)) return;
        out.push(p.why ? 'its pixels were not compared: ' + p.why
          : p.over + ' of its pixels differ from the live frame' + (p.bbox ? ', in ' + p.bbox[2] + 'x' + p.bbox[3] + ' at ' + p.bbox[0] + ',' + p.bbox[1] : ''));
      });
      (V.moving_out_of_step || []).forEach(function (d) {
        if (sameOrigin(d.surface, origin)) out.push('moving out of step (not a difference): ' + d.node + ', ' + num(d.px, 1) + ' px off');
      });
      return out;
    }
    function plural(n, w) { return n + ' ' + w + (n === 1 ? '' : 's'); }
    if (!V) {
      lines.push('no verdict: this replay is from a strand-eval before the live boxes were compared; nothing checked its frames ' +
        'against what the live shell drew');
    } else if (V.unchecked) {
      lines.push('NOT CHECKED: nothing compared this replay with what the live shell drew (' + V.unchecked + '); every frame ' +
        'here is the replay\'s own, drawn by the sandbox from the recorded input');
    } else {
      lines.push('the verdict, of the replay\'s last frame before the report: ' + V.line);
    }
    sids.forEach(function (sid) {
      var origin = rootOrigin(sid);
      // The frames of it the slice shows: each paint's tick.
      var shown = [];
      steps.forEach(function (st) { var v = st.surf[sid]; if (v && shown.indexOf(v.k) < 0) shown.push(v.k); });
      var ticks = V && !V.unchecked ? (V.ticks || []).filter(function (x) { return sameOrigin(x.surface, origin); }).map(function (x) { return x.k; }) : [];
      var held = ticks.filter(function (k) { return shown.indexOf(k) >= 0; });
      var s = { surface: sid, origin: origin, frames: shown, compared: ticks, compared_in_slice: held, differences: [] };
      surfaces.push(s);
      if (!V || V.unchecked) return;
      var at = '@' + origin + ': ';
      if (!ticks.length) {
        var why = (V.missing || []).some(function (m) { return sameOrigin(m, origin); }) ? 'the replay drew no frame of it at the report'
          : (V.replay_only_surfaces || []).some(function (m) { return sameOrigin(m, origin); }) ? 'it was shown at the report in the replay only, not live'
            : 'it was not shown at the report';
        lines.push(at + 'none of its ' + plural(shown.length, 'frame') + ' in this slice was compared with what the live shell drew (' +
          why + '): they are the replay\'s own, drawn from the recorded input');
        return;
      }
      if (!held.length) {
        lines.push(at + 'none of its ' + plural(shown.length, 'frame') + ' in this slice was compared with what the live shell ' +
          'drew: they are the replay\'s own, drawn from the recorded input; the verdict held k' + ticks.join(', k') + ' (' +
          secs(tOf(ticks[0])) + ' s), ' + (ticks[0] > shown[shown.length - 1] ? 'after' : 'before') + ' them');
        return;
      }
      var diffs = differences(origin);
      s.differences = diffs;
      var px = (F.frames || []).some(function (p) { return sameOrigin(p.origin, origin) && held.indexOf(p.k) >= 0 && !p.why; });
      var tol = V.tolerance || { px: 1, opacity: 0.02 };
      var head = at + 'of its ' + plural(shown.length, 'frame') + ' in this slice, k' + held.join(', k') + ' was held to the live ' +
        'shell\'s last frame: ';
      if (!diffs.length) {
        lines.push(head + 'VERIFIED, every node of it within ' + num(tol.px) + ' px and ' + num(tol.opacity) + ' opacity of the ' +
          'live boxes' + (px ? ', and its pixels as the live frame\'s' : ' (its pixels were not compared)'));
      } else {
        lines.push(head + 'it differs (' + diffs.length + '):');
        diffs.forEach(function (d) { lines.push('  ' + d); });
      }
      var own = shown.length - held.length;
      if (own > 0) {
        lines.push('  the other ' + plural(own, 'frame') + ' of it are the replay\'s own, drawn from the recorded input, never ' +
          'compared with what the live shell drew');
      }
    });
    var D = F.divergences || [];
    var inSlice = D.filter(function (d) { return d.t >= from - 0.0005 && d.t <= to + 0.0005; });
    var before = D.filter(function (d) { return d.t < from - 0.0005; });
    if (inSlice.length) {
      lines.push('divergences from the recorded session in this slice (' + inSlice.length + '):');
      inSlice.forEach(function (d) { lines.push('  ' + secs(d.t) + ' s ' + d.what + ': ' + d.text); });
    } else {
      lines.push('no divergence from the recorded session (presses, surfaces mapped, cell writes) in this slice');
    }
    if (before.length) {
      var last = before[before.length - 1];
      lines.push(before.length + ' earlier in the window, the last at ' + secs(last.t) + ' s (' + last.what + ': ' + last.text +
        '): the replay may already part from the live shell here');
    }
    var limits = F.limits || [];
    if (limits.length) {
      lines.push('not reproduced by this replay:');
      limits.forEach(function (l) { lines.push('  - ' + l); });
    }
    return {
      lines: lines,
      json: {
        verdict: V ? V.verdict : null,
        line: V ? V.line : null,
        surfaces: surfaces,
        divergences: inSlice,
        divergences_before: before.length,
        limits: limits,
        statement: lines
      }
    };
  }

  // The checks' findings on the clip's elements in the slice: hints.
  function hintsOf(checks, keys, kFirst, kLast, period, W, F) {
    var label = (F.hints && F.hints.label) || 'hints';
    var out = [], lines = [];
    ((checks && checks.results) || []).forEach(function (r) {
      (r.findings || []).forEach(function (f) {
        if (f.sev !== 'fail' && f.sev !== 'warn') return;
        if (!f.key || keys.indexOf(f.key) < 0) return;
        if (f.k === null || f.k === undefined || f.k < kFirst || f.k > kLast) return;
        var t = Math.round((f.k * period / 1e9 - W) * 1e4) / 1e4;
        out.push({ check: r.check, k: f.k, t: t, element: keys.indexOf(f.key) + 1, msg: f.msg });
      });
    });
    out.sort(function (a, b) { return a.k - b.k || a.element - b.element; });
    out.forEach(function (h) { lines.push(h.check + ' k' + h.k + ' (' + secs(h.t) + ' s) [' + h.element + ']: ' + h.msg); });
    return { label: label, lines: lines, json: { label: label, findings: out } };
  }

  // The crop of each surface: the union of the elements' drawn boxes on
  // it over the slice, `MARGIN` px around, in buffer px, clamped to the
  // surface (its whole buffer with no element); and the frames of it the
  // slice shows, each image once.
  function cropOf(steps, sids, keys, elements) {
    return sids.map(function (sid) {
      var x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity, scale = 0, W = 0, H = 0;
      steps.forEach(function (st) {
        var v = st.surf[sid];
        if (!v || !v.size) return;
        var sc = v.scale || 1;
        scale = Math.max(scale, sc);
        W = Math.max(W, Math.ceil(v.size[0] * sc));
        H = Math.max(H, Math.ceil(v.size[1] * sc));
      });
      scale = scale || 1;
      elements.forEach(function (e) {
        e.frames.forEach(function (f) {
          if (!f.drawn || f.surface !== sid) return;
          x0 = Math.min(x0, f.drawn[0]); y0 = Math.min(y0, f.drawn[1]);
          x1 = Math.max(x1, f.drawn[0] + f.drawn[2]); y1 = Math.max(y1, f.drawn[1] + f.drawn[3]);
        });
      });
      var r;
      if (!keys.length || x0 === Infinity) r = [0, 0, W, H];
      else {
        var a = Math.max(0, Math.floor((x0 - MARGIN) * scale)), b = Math.max(0, Math.floor((y0 - MARGIN) * scale));
        var c = Math.min(W, Math.ceil((x1 + MARGIN) * scale)), d = Math.min(H, Math.ceil((y1 + MARGIN) * scale));
        r = [a, b, Math.max(0, c - a), Math.max(0, d - b)];
      }
      var frames = [], last = null;
      steps.forEach(function (st) {
        var v = st.surf[sid];
        if (!v || !v.png || v.png === last) return;
        last = v.png;
        frames.push({ k: st.k, image: v.png, file: 'frames/k' + st.k + '-s' + sid + '.png' });
      });
      return { surface: sid, rect: r, scale: scale, frames: frames };
    });
  }

  // ---- bytes: base64, inflate, PNG, zip -----------------------------------------------

  var B64 = (function () {
    var m = {}, a = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    for (var i = 0; i < a.length; i++) m[a.charCodeAt(i)] = i;
    return m;
  })();
  function b64(s) {
    var n = s.length;
    while (n > 0 && s.charAt(n - 1) === '=') n--;
    var out = new Uint8Array(Math.floor(n * 3 / 4)), o = 0, acc = 0, bits = 0;
    for (var i = 0; i < n; i++) {
      var v = B64[s.charCodeAt(i)];
      if (v === undefined) continue;
      acc = (acc << 6) | v; bits += 6;
      if (bits >= 8) { bits -= 8; out[o++] = (acc >> bits) & 255; }
    }
    return o === out.length ? out : out.subarray(0, o);
  }

  function b64enc(bytes) {
    var a = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/', out = [], chunk = [];
    for (var i = 0; i < bytes.length; i += 3) {
      var n = (bytes[i] << 16) | ((bytes[i + 1] || 0) << 8) | (bytes[i + 2] || 0);
      chunk.push(a.charAt(n >> 18), a.charAt((n >> 12) & 63),
        i + 1 < bytes.length ? a.charAt((n >> 6) & 63) : '=', i + 2 < bytes.length ? a.charAt(n & 63) : '=');
      if (chunk.length >= 8192) { out.push(chunk.join('')); chunk = []; }
    }
    out.push(chunk.join(''));
    return out.join('');
  }

  function utf8(s) {
    var out = [];
    for (var i = 0; i < s.length; i++) {
      var c = s.charCodeAt(i);
      if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length) {
        var d = s.charCodeAt(i + 1);
        if (d >= 0xdc00 && d < 0xe000) { c = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00); i++; }
      }
      if (c < 0x80) out.push(c);
      else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
      else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
      else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
    }
    return new Uint8Array(out);
  }

  var LBASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
  var LEXT = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
  var DBASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
  var DEXT = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
  var ORDER = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

  // A canonical Huffman code from its lengths, as a lookup table of
  // `1 << max` entries: symbol << 4 | length.
  function huff(lengths, n) {
    var max = 0, i;
    for (i = 0; i < n; i++) if (lengths[i] > max) max = lengths[i];
    var table = new Int32Array(1 << Math.max(1, max));
    var count = new Int32Array(16), next = new Int32Array(16);
    for (i = 0; i < n; i++) count[lengths[i]]++;
    count[0] = 0;
    var code = 0;
    for (i = 1; i < 16; i++) { code = (code + count[i - 1]) << 1; next[i] = code; }
    for (i = 0; i < n; i++) {
      var len = lengths[i];
      if (!len) continue;
      var c = next[len]++, rev = 0;
      for (var b = 0; b < len; b++) { rev = (rev << 1) | (c & 1); c >>= 1; }
      for (var j = rev; j < table.length; j += 1 << len) table[j] = (i << 4) | len;
    }
    return { table: table, bits: Math.max(1, max) };
  }

  // A zlib stream's data, inflated.
  function inflate(src) {
    var pos = 2, bit = 0, buf = 0;
    var out = new Uint8Array(Math.max(1024, src.length * 4)), o = 0;
    function need(n) {
      while (bit < n) {
        if (pos >= src.length) throw new Error('inflate: the data ends early');
        buf |= src[pos++] << bit; bit += 8;
      }
    }
    function bits(n) { need(n); var v = buf & ((1 << n) - 1); buf >>>= n; bit -= n; return v; }
    function sym(h) {
      while (bit < h.bits && pos < src.length) { buf |= src[pos++] << bit; bit += 8; }
      var e = h.table[buf & ((1 << h.bits) - 1)], len = e & 15;
      if (!len || len > bit) throw new Error('inflate: a bad code');
      buf >>>= len; bit -= len;
      return e >> 4;
    }
    function room(n) {
      if (o + n <= out.length) return;
      var size = out.length * 2;
      while (size < o + n) size *= 2;
      var g = new Uint8Array(size); g.set(out.subarray(0, o)); out = g;
    }
    var fixedL = null, fixedD = null, last = 0;
    while (!last) {
      last = bits(1);
      var type = bits(2);
      if (type === 0) {
        // To the byte: the whole bytes the bit buffer read ahead go back.
        var drop = bit & 7;
        buf >>>= drop; bit -= drop;
        pos -= bit >> 3;
        buf = 0; bit = 0;
        if (pos + 4 > src.length) throw new Error('inflate: the data ends early');
        var len = src[pos] | (src[pos + 1] << 8);
        pos += 4;
        if (pos + len > src.length) throw new Error('inflate: the data ends early');
        room(len);
        out.set(src.subarray(pos, pos + len), o);
        o += len; pos += len;
        continue;
      }
      var L, D;
      if (type === 1) {
        if (!fixedL) {
          var fl = new Uint8Array(288), fd = new Uint8Array(30), i;
          for (i = 0; i < 144; i++) fl[i] = 8;
          for (; i < 256; i++) fl[i] = 9;
          for (; i < 280; i++) fl[i] = 7;
          for (; i < 288; i++) fl[i] = 8;
          for (i = 0; i < 30; i++) fd[i] = 5;
          fixedL = huff(fl, 288); fixedD = huff(fd, 30);
        }
        L = fixedL; D = fixedD;
      } else if (type === 2) {
        var hl = bits(5) + 257, hd = bits(5) + 1, hc = bits(4) + 4;
        var cl = new Uint8Array(19);
        for (var q = 0; q < hc; q++) cl[ORDER[q]] = bits(3);
        var ch = huff(cl, 19);
        var lens = new Uint8Array(hl + hd), n = 0;
        while (n < hl + hd) {
          var s = sym(ch);
          if (s < 16) lens[n++] = s;
          else {
            var rep, val = 0;
            if (s === 16) { if (!n) throw new Error('inflate: a bad repeat'); val = lens[n - 1]; rep = 3 + bits(2); }
            else if (s === 17) rep = 3 + bits(3);
            else rep = 11 + bits(7);
            if (n + rep > hl + hd) throw new Error('inflate: a bad repeat');
            while (rep--) lens[n++] = val;
          }
        }
        L = huff(lens.subarray(0, hl), hl); D = huff(lens.subarray(hl), hd);
      } else throw new Error('inflate: a bad block type');
      for (;;) {
        var c = sym(L);
        if (c < 256) { room(1); out[o++] = c; continue; }
        if (c === 256) break;
        c -= 257;
        if (c >= 29) throw new Error('inflate: a bad length');
        var ln = LBASE[c] + bits(LEXT[c]);
        var dc = sym(D);
        if (dc >= 30) throw new Error('inflate: a bad distance');
        var dist = DBASE[dc] + bits(DEXT[dc]);
        if (dist > o) throw new Error('inflate: a distance past the start');
        room(ln);
        for (var z = 0; z < ln; z++) { out[o] = out[o - dist]; o++; }
      }
    }
    return out.subarray(0, o);
  }

  var CRC = (function () {
    var t = new Int32Array(256);
    for (var n = 0; n < 256; n++) {
      var c = n;
      for (var k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      t[n] = c;
    }
    return t;
  })();
  function crc32(bytes, from, to) {
    var c = -1;
    for (var i = from || 0, e = to === undefined ? bytes.length : to; i < e; i++) c = CRC[(c ^ bytes[i]) & 255] ^ (c >>> 8);
    return (c ^ -1) >>> 0;
  }

  // A PNG file as `{w, h, px}` (RGBA, 8 bits a channel): RGBA, RGB, grey
  // and grey with alpha, 8 bits deep, not interlaced (what Strand writes).
  function pngDecode(b) {
    var sig = [137, 80, 78, 71, 13, 10, 26, 10];
    for (var i = 0; i < 8; i++) if (b[i] !== sig[i]) throw new Error('png: not a PNG file');
    var p = 8, w = 0, h = 0, depth = 0, color = 0, lace = 0, idat = [], total = 0;
    while (p + 8 <= b.length) {
      var len = ((b[p] << 24) | (b[p + 1] << 16) | (b[p + 2] << 8) | b[p + 3]) >>> 0;
      var type = String.fromCharCode(b[p + 4], b[p + 5], b[p + 6], b[p + 7]);
      var d = p + 8;
      if (d + len > b.length) throw new Error('png: a chunk ends past the file');
      if (type === 'IHDR') {
        w = ((b[d] << 24) | (b[d + 1] << 16) | (b[d + 2] << 8) | b[d + 3]) >>> 0;
        h = ((b[d + 4] << 24) | (b[d + 5] << 16) | (b[d + 6] << 8) | b[d + 7]) >>> 0;
        depth = b[d + 8]; color = b[d + 9]; lace = b[d + 12];
      } else if (type === 'IDAT') { idat.push(b.subarray(d, d + len)); total += len; }
      else if (type === 'IEND') break;
      p = d + len + 4;
    }
    var ch = { 6: 4, 2: 3, 0: 1, 4: 2 }[color];
    if (depth !== 8 || !ch || lace) throw new Error('png: only 8-bit RGBA, RGB and grey files, not interlaced');
    var z = new Uint8Array(total), at = 0;
    idat.forEach(function (c) { z.set(c, at); at += c.length; });
    var raw = inflate(z);
    var stride = w * ch;
    if (raw.length < h * (stride + 1)) throw new Error('png: the image data is short');
    var px = new Uint8Array(w * h * 4), line = new Uint8Array(stride), prev = new Uint8Array(stride);
    for (var y = 0; y < h; y++) {
      var f = raw[y * (stride + 1)], s = y * (stride + 1) + 1;
      for (var x = 0; x < stride; x++) {
        var v = raw[s + x], a = x >= ch ? line[x - ch] : 0, u = prev[x], c = x >= ch ? prev[x - ch] : 0;
        if (f === 1) v += a;
        else if (f === 2) v += u;
        else if (f === 3) v += (a + u) >> 1;
        else if (f === 4) {
          var pp = a + u - c, pa = Math.abs(pp - a), pb = Math.abs(pp - u), pc = Math.abs(pp - c);
          v += pa <= pb && pa <= pc ? a : pb <= pc ? u : c;
        } else if (f !== 0) throw new Error('png: a bad filter');
        line[x] = v & 255;
      }
      var o = y * w * 4;
      for (var q = 0; q < w; q++) {
        if (ch === 4) { px[o] = line[q * 4]; px[o + 1] = line[q * 4 + 1]; px[o + 2] = line[q * 4 + 2]; px[o + 3] = line[q * 4 + 3]; }
        else if (ch === 3) { px[o] = line[q * 3]; px[o + 1] = line[q * 3 + 1]; px[o + 2] = line[q * 3 + 2]; px[o + 3] = 255; }
        else if (ch === 2) { px[o] = px[o + 1] = px[o + 2] = line[q * 2]; px[o + 3] = line[q * 2 + 1]; }
        else { px[o] = px[o + 1] = px[o + 2] = line[q]; px[o + 3] = 255; }
        o += 4;
      }
      var tmp = prev; prev = line; line = tmp;
    }
    return { w: w, h: h, px: px };
  }

  function be32(a, o, v) { a[o] = (v >>> 24) & 255; a[o + 1] = (v >>> 16) & 255; a[o + 2] = (v >>> 8) & 255; a[o + 3] = v & 255; }
  function le16(a, o, v) { a[o] = v & 255; a[o + 1] = (v >>> 8) & 255; }
  function le32(a, o, v) { a[o] = v & 255; a[o + 1] = (v >>> 8) & 255; a[o + 2] = (v >>> 16) & 255; a[o + 3] = (v >>> 24) & 255; }

  // Deflate (RFC 1951), one block of the fixed codes, matches found by
  // a hash of three bytes over a 32 KiB window: small and the same
  // bytes on every engine (the parity test holds the page's and the
  // command line's .zip byte for byte).
  var LBASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
  var LEXTRA = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
  var DBASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
    4097, 6145, 8193, 12289, 16385, 24577];
  var DEXTRA = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
  var LCODE = new Uint8Array(259), DCODE = new Uint8Array(32769);
  (function () {
    var c, v;
    for (c = 0; c < 29; c++) for (v = LBASE[c]; v < LBASE[c] + (1 << LEXTRA[c]) && v <= 258; v++) LCODE[v] = c;
    LCODE[258] = 28;
    for (c = 0; c < 30; c++) for (v = DBASE[c]; v < DBASE[c] + (1 << DEXTRA[c]) && v <= 32768; v++) DCODE[v] = c;
  })();
  function deflate(src) {
    var n = src.length;
    var out = new Uint8Array(Math.max(64, (n >> 3) + 64)), o = 0, buf = 0, bits = 0;
    function grow() { var b = new Uint8Array(out.length * 2); b.set(out); out = b; }
    function put(v, len) { // `len` bits of `v`, least significant first
      buf |= v << bits;
      bits += len;
      while (bits >= 8) { if (o >= out.length) grow(); out[o++] = buf & 255; buf >>>= 8; bits -= 8; }
    }
    function rev(v, len) { var r = 0; for (var i = 0; i < len; i++) { r = (r << 1) | (v & 1); v >>= 1; } return r; }
    function lit(v) { // a fixed literal/length code
      if (v < 144) put(rev(0x30 + v, 8), 8);
      else if (v < 256) put(rev(0x190 + v - 144, 9), 9);
      else if (v < 280) put(rev(v - 256, 7), 7);
      else put(rev(0xc0 + v - 280, 8), 8);
    }
    put(1, 1); put(1, 2); // the last block, the fixed codes
    var WIN = 32768, MASK = WIN - 1, HBITS = 15, HSIZE = 1 << HBITS;
    var head = new Int32Array(HSIZE).fill(-1), prev = new Int32Array(WIN);
    function hash(i) { return ((src[i] << 10) ^ (src[i + 1] << 5) ^ src[i + 2]) & (HSIZE - 1); }
    function insert(i) { var h = hash(i); prev[i & MASK] = head[h]; head[h] = i; }
    var i = 0;
    while (i < n) {
      var best = 0, dist = 0;
      if (i + 2 < n) {
        var cand = head[hash(i)], chain = 24, max = Math.min(258, n - i);
        while (cand >= 0 && i - cand <= WIN && chain-- > 0) {
          if (src[cand + best] === src[i + best]) {
            var l = 0;
            while (l < max && src[cand + l] === src[i + l]) l++;
            if (l > best) { best = l; dist = i - cand; if (l === max) break; }
          }
          var nx = prev[cand & MASK];
          if (nx >= cand) break;
          cand = nx;
        }
      }
      if (best >= 3) {
        var lc = LCODE[best], dc = DCODE[dist];
        lit(257 + lc);
        if (LEXTRA[lc]) put(best - LBASE[lc], LEXTRA[lc]);
        put(rev(dc, 5), 5);
        if (DEXTRA[dc]) put(dist - DBASE[dc], DEXTRA[dc]);
        var end = i + best;
        if (best <= 32) { for (; i < end; i++) if (i + 2 < n) insert(i); }
        else { if (i + 2 < n) insert(i); i = end; }
      } else {
        lit(src[i]);
        if (i + 2 < n) insert(i);
        i++;
      }
    }
    lit(256);
    if (bits > 0) put(0, 8 - bits);
    return out.subarray(0, o);
  }

  // An RGBA image as a PNG file: each row filtered by the left or the
  // above neighbour (whichever leaves less), deflated.
  function pngEncode(w, h, px) {
    var stride = w * 4, raw = new Uint8Array(h * (stride + 1));
    for (var y = 0; y < h; y++) {
      var r = y * stride, q = y * (stride + 1), sub = 0, up = 0, x, d;
      for (x = 0; x < stride; x++) {
        d = (px[r + x] - (x >= 4 ? px[r + x - 4] : 0)) & 255; sub += d < 128 ? d : 256 - d;
        d = (px[r + x] - (y ? px[r + x - stride] : 0)) & 255; up += d < 128 ? d : 256 - d;
      }
      var f = y && up <= sub ? 2 : 1;
      raw[q] = f;
      for (x = 0; x < stride; x++) {
        raw[q + 1 + x] = (px[r + x] - (f === 1 ? (x >= 4 ? px[r + x - 4] : 0) : px[r + x - stride])) & 255;
      }
    }
    var a = 1, b = 0;
    for (var k = 0; k < raw.length; k++) { a = (a + raw[k]) % 65521; b = (b + a) % 65521; }
    var def = deflate(raw);
    var zlen = 2 + def.length + 4;
    var out = new Uint8Array(8 + 25 + 12 + zlen + 12);
    out.set([137, 80, 78, 71, 13, 10, 26, 10], 0);
    var o = 8;
    function chunk(type, len, fill) {
      be32(out, o, len);
      for (var i = 0; i < 4; i++) out[o + 4 + i] = type.charCodeAt(i);
      fill(o + 8);
      be32(out, o + 8 + len, crc32(out, o + 4, o + 8 + len));
      o += 12 + len;
    }
    chunk('IHDR', 13, function (d) { be32(out, d, w); be32(out, d + 4, h); out[d + 8] = 8; out[d + 9] = 6; });
    chunk('IDAT', zlen, function (d) {
      out[d] = 0x78; out[d + 1] = 0x01;
      out.set(def, d + 2);
      be32(out, d + 2 + def.length, ((b << 16) | a) >>> 0);
    });
    chunk('IEND', 0, function () {});
    return out;
  }

  // Files as one .zip, stored (no compression), every date 1980-01-01.
  function zip(files) {
    var size = 22, i;
    files.forEach(function (f) { var n = utf8(f.name).length; size += 30 + n + f.bytes.length + 46 + n; });
    var out = new Uint8Array(size), o = 0, dir = [];
    files.forEach(function (f) {
      var name = utf8(f.name), crc = crc32(f.bytes);
      dir.push({ name: name, crc: crc, len: f.bytes.length, at: o });
      le32(out, o, 0x04034b50); le16(out, o + 4, 20); le16(out, o + 6, 0x0800); le16(out, o + 8, 0);
      le16(out, o + 10, 0); le16(out, o + 12, 0x21); le32(out, o + 14, crc);
      le32(out, o + 18, f.bytes.length); le32(out, o + 22, f.bytes.length); le16(out, o + 26, name.length); le16(out, o + 28, 0);
      out.set(name, o + 30);
      out.set(f.bytes, o + 30 + name.length);
      o += 30 + name.length + f.bytes.length;
    });
    var cd = o;
    dir.forEach(function (d) {
      le32(out, o, 0x02014b50); le16(out, o + 4, 20); le16(out, o + 6, 20); le16(out, o + 8, 0x0800); le16(out, o + 10, 0);
      le16(out, o + 12, 0); le16(out, o + 14, 0x21); le32(out, o + 16, d.crc); le32(out, o + 20, d.len); le32(out, o + 24, d.len);
      le16(out, o + 28, d.name.length);
      for (i = 30; i < 42; i++) out[o + i] = 0;
      le32(out, o + 42, d.at);
      out.set(d.name, o + 46);
      o += 46 + d.name.length;
    });
    le32(out, o, 0x06054b50); le16(out, o + 4, 0); le16(out, o + 6, 0); le16(out, o + 8, dir.length); le16(out, o + 10, dir.length);
    le32(out, o + 12, o - cd); le32(out, o + 16, cd); le16(out, o + 20, 0);
    return out;
  }

  // ---- the frames, cropped ------------------------------------------------------------

  // Frame `name` whole, `{w, h, px}`: its stored image, or a patch's base
  // (rebuilt the same way, or one rebuilt lately) with each of its boxes
  // replaced, as report::pack::Composer rebuilds it. Null when a file it
  // needs is missing or its patches do not fit.
  function framePixels(src, name, cache) {
    var hit = cache.get(name);
    if (hit) return hit;
    // The command line's frames, rebuilt by strand-eval itself.
    if (src.frame) return src.frame(name);
    var patches = src.patches || {}, chain = [], at = name, img = null;
    for (var guard = 0; guard < 4096; guard++) {
      if (at !== name && cache.has(at)) { img = cache.get(at); break; }
      var p = patches[at];
      if (!p) { var b = src.file(at); if (!b) return null; img = pngDecode(b); break; }
      chain.push(at);
      at = p[0];
    }
    if (!img) return null;
    var w = img.w, h = img.h, px = new Uint8Array(img.px);
    for (var c = chain.length - 1; c >= 0; c--) {
      var q = patches[chain[c]];
      if (q.length > 1) {
        var sb = src.file(chain[c]);
        if (!sb) return null;
        var s = pngDecode(sb), sy = 0;
        for (var j = 1; j < q.length; j++) {
          var bx = q[j][0], by = q[j][1], bw = q[j][2], bh = q[j][3];
          if (bx + bw > w || by + bh > h || bw > s.w || sy + bh > s.h) return null;
          for (var r = 0; r < bh; r++) {
            px.set(s.px.subarray((sy + r) * s.w * 4, (sy + r) * s.w * 4 + bw * 4), ((by + r) * w + bx) * 4);
          }
          sy += bh;
        }
      }
    }
    var out = { w: w, h: h, px: px };
    cache.set(name, out);
    if (cache.size > 8) cache.delete(cache.keys().next().value);
    return out;
  }

  // A clip's files: clip.txt, clip.json and each cropped frame, PNG.
  // A frame whose pixels are not there (a page that thinned it) is left
  // out and named in `missing`.
  function filesOf(src, made) {
    var files = [{ name: 'clip.txt', bytes: utf8(made.text) }, { name: 'clip.json', bytes: utf8(JSON.stringify(made.json, null, 1) + '\n') }];
    var cache = new Map(), missing = [];
    made.json.crop.forEach(function (c) {
      var r = c.rect, last = null;
      if (!r[2] || !r[3]) return;
      c.frames.forEach(function (f) {
        var im = null;
        try { im = framePixels(src, f.image, cache); } catch (e) { im = null; }
        if (!im) { missing.push(f.file); return; }
        var out = new Uint8Array(r[2] * r[3] * 4);
        for (var y = 0; y < r[3]; y++) {
          var sy = r[1] + y;
          if (sy >= im.h) break;
          var n = Math.max(0, Math.min(r[2], im.w - r[0]));
          if (n) out.set(im.px.subarray((sy * im.w + r[0]) * 4, (sy * im.w + r[0] + n) * 4), y * r[2] * 4);
        }
        // A crop as the one before (what changed lay outside it): its
        // file again, not encoded again.
        var same = last && last.px.length === out.length;
        for (var i = 0; same && i < out.length; i++) if (last.px[i] !== out[i]) same = false;
        if (!same) last = { px: out, png: pngEncode(r[2], r[3], out) };
        files.push({ name: f.file, bytes: last.png });
      });
    });
    return { files: files, missing: missing };
  }

  // ---- the two hosts ------------------------------------------------------------------

  // The viewer's page: its data (`#run`) and the scenario shown.
  function fromPage(data, sc) {
    return {
      trace: sc.trace,
      checks: sc.checks,
      fidelity: data.replay ? data.replay.record : null,
      motions: data.motions,
      patches: sc.patches || {},
      file: function (name) {
        var u = sc.images && sc.images[name];
        return u ? b64(u.slice(u.indexOf(',') + 1)) : null;
      }
    };
  }

  // The page's or the command line's clip, refusals as a value:
  // {text, json} or {refused: {code, message}}.
  function attempt(fn) {
    try { return fn(); } catch (e) {
      if (e instanceof Refusal) return { refused: { code: e.code, message: e.message } };
      throw e;
    }
  }

  // `strand-eval replay clip` (eval/src/replay/clip.rs): trace.json,
  // checks.json (or null), fidelity.json and the spec, as text; `frame`
  // gives a frame by name as strand-eval rebuilds it ({w, h, px}, RGBA
  // in an ArrayBuffer). Gives {text, json (text), zip (base64)?} or
  // {refused}.
  function cli(traceText, checksText, fidelityText, motionsText, specText, wantZip, frame) {
    return attempt(function () {
      var t = JSON.parse(traceText);
      var patches = (t.packed && t.packed.patches) || {};
      var src = {
        trace: t,
        checks: checksText ? JSON.parse(checksText) : null,
        fidelity: JSON.parse(fidelityText),
        motions: JSON.parse(motionsText),
        patches: patches,
        file: function () { return null; },
        frame: function (name) {
          var f = frame(name);
          return f ? { w: f.w, h: f.h, px: new Uint8Array(f.px) } : null;
        }
      };
      var made = make(src, JSON.parse(specText));
      var out = { text: made.text, json: JSON.stringify(made.json, null, 1) + '\n' };
      if (wantZip) {
        var f = filesOf(src, made);
        out.zip = b64enc(zip(f.files));
        out.missing = f.missing;
      }
      return out;
    });
  }

  // The page's clip of scenario `sc`, as the viewer makes it.
  function page(data, sc, spec, wantZip) {
    return attempt(function () {
      var src = fromPage(data, sc);
      var made = make(src, spec);
      var out = { text: made.text, json: JSON.stringify(made.json, null, 1) + '\n', data: made.json };
      if (wantZip) {
        var f = filesOf(src, made);
        out.zip = zip(f.files);
        out.missing = f.missing;
      }
      return out;
    });
  }

  return {
    unpack: unpack,
    make: make,
    page: page,
    cli: cli,
    fromPage: fromPage,
    resolve: resolve,
    pngDecode: pngDecode,
    pngEncode: pngEncode,
    zip: zip,
    b64: b64,
    b64enc: b64enc,
    rest: rest,
    MARGIN: MARGIN
  };
})();
