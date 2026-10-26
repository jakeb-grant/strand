//! (replayfix) A packed report (docs/eval.md, "Replaying a glitch
//! report", "A replay's report, packed"): a replay of a minute or more
//! of a real shell paints a thousand frames or more, each repeating
//! almost every node row and almost every pixel of the one before, so
//! its report keeps each only once.
//!
//! - **The trace** (`trace.json`, read back by [`read`] and
//!   [`RunTrace::from_json`]): each frame leaves out what equals the
//!   frame before. Its fields equal to the previous frame's are named
//!   in `"="`, and its node rows `n` are, against the previous frame's
//!   rows, `[i, n]` (its rows `i..i+n` unchanged), `{"i": i, "c": [col,
//!   value, …]}` (its row `i` with those columns changed) or a whole
//!   row (an array whose first item is the key). The values are copied
//!   from the trace's own JSON, never recomputed: unpacked, it is the
//!   same trace. The top-level `packed` says so (`format`) and lists
//!   the patched frames.
//! - **The frames**: each new frame of a surface is stored as a patch
//!   of the frame of that surface stored before it
//!   (`frames/<digest>.patch.png`): the boxes that changed, each a run
//!   of [`TILE`]-px tiles along a band of the frame shrunk to what
//!   changed in it, stacked top to bottom from the image's left edge.
//!   A frame is stored whole (`frames/<digest>.png`) when it is a
//!   surface's first, its size changed, its boxes would cover more than
//!   [`WHOLE_SHARE`] of it, or it is [`CHAIN`] patches from a whole one.
//!   `packed.patches` names each patched frame's base and boxes;
//!   [`Composer`] rebuilds a frame exactly: the base's pixels with each
//!   box's replaced, every byte of it.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde_json::{Map, Value, json};

use crate::Error;
use crate::harness::Image;
use crate::trace::RunTrace;

/// The packed format's version (`packed.format`).
pub const FORMAT: u64 = 1;

/// The most patches a frame is from a whole one: what composing the
/// farthest frame costs (each a small image drawn over the last).
pub const CHAIN: usize = 256;

/// The tile a patch's boxes are cut along, px.
pub const TILE: u32 = 32;

/// A frame whose changed boxes cover more than this share of it is
/// stored whole.
pub const WHOLE_SHARE: f64 = 0.75;

/// The frame fields the viewer draws (eval/viewer/viewer.js): a packed
/// page holds these of each frame, `trace.json` every field.
pub const VIEW_FIELDS: [&str; 12] = [
    "k",
    "t_ns",
    "painted",
    "sizes",
    "png",
    "n",
    "scale",
    // (fidelity) Each surface's scale, with several outputs.
    "scales",
    "held_text",
    "cost",
    // (clip-data) What a clip says of its elements' values and states:
    // the controls' values and the router's flags, and the sliders held
    // in a drag.
    "ui",
    "drags",
];

/// A frame stored as a patch: its base frame (by name) and the boxes
/// (`[x, y, w, h]`, buffer px) whose pixels it replaces, stacked top to
/// bottom in its patch image from its left edge; none when no pixel
/// changed (no image).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub base: String,
    pub boxes: Vec<[u32; 4]>,
}

impl Patch {
    /// The patch as `packed.patches` and the page list it: `[base,
    /// [x, y, w, h], …]`.
    pub fn json(&self) -> Value {
        let mut v = vec![Value::String(self.base.clone())];
        v.extend(self.boxes.iter().map(|b| json!(b)));
        Value::Array(v)
    }

    pub fn from_json(v: &Value) -> Option<Patch> {
        let a = v.as_array()?;
        let base = a.first()?.as_str()?.to_string();
        let mut boxes = Vec::new();
        for b in &a[1..] {
            let b = b.as_array()?;
            if b.len() != 4 {
                return None;
            }
            let n = |i: usize| b[i].as_u64().and_then(|v| u32::try_from(v).ok());
            boxes.push([n(0)?, n(1)?, n(2)?, n(3)?]);
        }
        Some(Patch { base, boxes })
    }
}

/// A report's frames and trace, packed.
#[derive(Clone, Debug, Default)]
pub struct Packed {
    /// The stored file of each frame, by its name: whole, or its patch
    /// ([`patch_file`]) when in `patches`.
    pub files: BTreeMap<String, Vec<u8>>,
    pub patches: BTreeMap<String, Patch>,
    /// `trace.json`, packed.
    pub trace: String,
    /// The page's trace: packed, its frames' [`VIEW_FIELDS`] only, its
    /// patches left to the page's `patches`.
    pub page_trace: String,
}

impl Packed {
    /// Where a frame's stored file goes, relative to its scenario's
    /// directory.
    pub fn path_of(&self, name: &str) -> String {
        if self.patches.contains_key(name) {
            patch_file(name)
        } else {
            name.to_string()
        }
    }

    /// The frames `name` needs to be drawn: itself and its bases, down
    /// to a whole one.
    pub fn chain(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_string()];
        let mut at = name;
        while let Some(p) = self.patches.get(at) {
            if out.len() > CHAIN + 1 {
                break;
            }
            out.push(p.base.clone());
            at = &p.base;
        }
        out
    }
}

/// `frames/<digest>.png` → `frames/<digest>.patch.png`.
pub fn patch_file(name: &str) -> String {
    match name.strip_suffix(".png") {
        Some(stem) => format!("{stem}.patch.png"),
        None => format!("{name}.patch.png"),
    }
}

/// The boxes of the pixels that differ between two RGBA buffers `w` ×
/// `h`: per band of [`TILE`] rows, each run of [`TILE`]-px tiles with a
/// change, shrunk to the changed pixels in it. None when no pixel does.
fn changed(w: u32, h: u32, a: &[u8], b: &[u8]) -> Vec<[u32; 4]> {
    let (wu, row) = (w as usize, w as usize * 4);
    let tiles = w.div_ceil(TILE) as usize;
    let mut out = Vec::new();
    let mut y0 = 0u32;
    while y0 < h {
        let y1 = (y0 + TILE).min(h);
        // Per tile, the changed columns and rows: (x0, x1, y0, y1).
        let mut span: Vec<Option<(u32, u32, u32, u32)>> = vec![None; tiles];
        for y in y0..y1 {
            let at = y as usize * row;
            let (ra, rb) = (&a[at..at + row], &b[at..at + row]);
            if ra == rb {
                continue;
            }
            for x in 0..wu {
                if ra[x * 4..x * 4 + 4] != rb[x * 4..x * 4 + 4] {
                    let x = x as u32;
                    let t = &mut span[(x / TILE) as usize];
                    *t = Some(match *t {
                        None => (x, x, y, y),
                        Some((a0, a1, b0, b1)) => (a0.min(x), a1.max(x), b0.min(y), b1.max(y)),
                    });
                }
            }
        }
        let mut i = 0;
        while i < tiles {
            let Some(first) = span[i] else {
                i += 1;
                continue;
            };
            let mut run = first;
            let mut j = i + 1;
            while j < tiles
                && let Some(t) = span[j]
            {
                run = (
                    run.0.min(t.0),
                    run.1.max(t.1),
                    run.2.min(t.2),
                    run.3.max(t.3),
                );
                j += 1;
            }
            out.push([run.0, run.2, run.1 - run.0 + 1, run.3 - run.2 + 1]);
            i = j;
        }
        y0 = y1;
    }
    out
}

/// The pixels of `rgba` (`w` wide) in `boxes`, stacked top to bottom
/// from the left edge of an image as wide as the widest, as `(width,
/// height, rgba)`.
fn stack(w: u32, rgba: &[u8], boxes: &[[u32; 4]]) -> (u32, u32, Vec<u8>) {
    let sw = boxes.iter().map(|b| b[2]).max().unwrap_or(0);
    let sh: u32 = boxes.iter().map(|b| b[3]).sum();
    let (w, srow) = (w as usize, sw as usize * 4);
    let mut out = vec![0u8; srow * sh as usize];
    let mut sy = 0usize;
    for b in boxes {
        let [x, y, bw, bh] = b.map(|v| v as usize);
        for r in 0..bh {
            let from = ((y + r) * w + x) * 4;
            out[(sy + r) * srow..(sy + r) * srow + bw * 4]
                .copy_from_slice(&rgba[from..from + bw * 4]);
        }
        sy += bh;
    }
    (sw, sh, out)
}

/// Packs the frames of `trace` in `images` (those its frames name; any
/// other image is stored whole) and the trace.
pub fn pack(trace: &RunTrace, images: &BTreeMap<String, Image>) -> Result<Packed, Error> {
    let mut out = Packed::default();
    // Per surface: the frame stored last, its pixels and how many
    // patches it is from a whole one.
    let mut last: BTreeMap<u32, (String, u32, u32, Vec<u8>, usize)> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for f in &trace.frames {
        for (s, name) in &f.png {
            if !seen.insert(name) {
                continue;
            }
            let Some(im) = images.get(name) else {
                continue;
            };
            let (w, h, rgba) = super::png::decode(&im.png)?;
            let base = last
                .get(s)
                .filter(|b| b.1 == w && b.2 == h && b.4 < CHAIN && b.3.len() == rgba.len());
            let mut depth = 0;
            let area = |bs: &[[u32; 4]]| bs.iter().map(|b| u64::from(b[2] * b[3])).sum::<u64>();
            let whole = (u64::from(w) * u64::from(h)) as f64 * WHOLE_SHARE;
            match base.map(|b| (b, changed(w, h, &b.3, &rgba))) {
                Some((b, boxes)) if (area(&boxes) as f64) <= whole => {
                    let bytes = if boxes.is_empty() {
                        Vec::new()
                    } else {
                        let (sw, sh, px) = stack(w, &rgba, &boxes);
                        super::png::encode_with(sw, sh, &px, png::Compression::High)?
                    };
                    out.patches.insert(
                        name.clone(),
                        Patch {
                            base: b.0.clone(),
                            boxes,
                        },
                    );
                    out.files.insert(name.clone(), bytes);
                    depth = b.4 + 1;
                }
                _ => {
                    let bytes = super::png::encode_with(w, h, &rgba, png::Compression::High)?;
                    out.files.insert(name.clone(), bytes);
                }
            }
            last.insert(*s, (name.clone(), w, h, rgba, depth));
        }
    }
    for (name, im) in images {
        out.files
            .entry(name.clone())
            .or_insert_with(|| im.png.clone());
    }
    out.trace = trace_json(trace, &out.patches, false)?;
    out.page_trace = trace_json(trace, &BTreeMap::new(), true)?;
    Ok(out)
}

/// The trace as packed JSON, its patched frames listed; `view`: its
/// frames' [`VIEW_FIELDS`] only (the page's).
pub fn trace_json(
    trace: &RunTrace,
    patches: &BTreeMap<String, Patch>,
    view: bool,
) -> Result<String, Error> {
    let err = |e: serde_json::Error| Error::Run(format!("trace.json: {e}"));
    // Through the trace's own text: each number as `to_json` writes it.
    let mut v: Value = serde_json::from_str(&trace.to_json()).map_err(err)?;
    let Some(o) = v.as_object_mut() else {
        return Err(Error::Run("trace.json: not an object".into()));
    };
    if let Some(Value::Array(frames)) = o.get_mut("frames") {
        if view {
            for f in frames.iter_mut() {
                if let Value::Object(m) = f {
                    m.retain(|k, _| VIEW_FIELDS.contains(&k.as_str()));
                }
            }
        }
        pack_frames(frames);
    }
    let listed: Map<String, Value> = patches.iter().map(|(n, p)| (n.clone(), p.json())).collect();
    o.insert(
        "packed".into(),
        json!({"format": FORMAT, "patches": listed}),
    );
    serde_json::to_string(&v).map_err(err)
}

/// The fields a packed frame always keeps.
const KEPT: [&str; 2] = ["k", "t_ns"];

fn pack_frames(frames: &mut [Value]) {
    let mut prev: Map<String, Value> = Map::new();
    let mut prev_rows: Vec<Value> = Vec::new();
    for f in frames.iter_mut() {
        let Value::Object(mut cur) = std::mem::take(f) else {
            continue;
        };
        let rows = match cur.remove("n") {
            Some(Value::Array(r)) => r,
            _ => Vec::new(),
        };
        let mut out = Map::new();
        let mut same = Vec::new();
        for (k, v) in &cur {
            if !KEPT.contains(&k.as_str()) && prev.get(k) == Some(v) {
                same.push(Value::String(k.clone()));
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
        if !same.is_empty() {
            out.insert("=".into(), Value::Array(same));
        }
        out.insert("n".into(), Value::Array(pack_rows(&prev_rows, &rows)));
        prev = cur;
        prev_rows = rows;
        *f = Value::Object(out);
    }
}

fn pack_rows(prev: &[Value], rows: &[Value]) -> Vec<Value> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, r) in prev.iter().enumerate() {
        if let Some(k) = r.get(0).and_then(Value::as_str) {
            index.entry(k).or_insert(i);
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for r in rows {
        let at = r.get(0).and_then(Value::as_str).and_then(|k| index.get(k));
        match at {
            Some(&i) if prev[i] == *r => {
                if let Some(Value::Array(run)) = out.last_mut()
                    && run.len() == 2
                    && let (Some(a), Some(n)) = (run[0].as_u64(), run[1].as_u64())
                    && a + n == i as u64
                {
                    run[1] = json!(n + 1);
                } else {
                    out.push(json!([i, 1]));
                }
            }
            Some(&i) => match (prev[i].as_array(), r.as_array()) {
                (Some(a), Some(b)) if a.len() == b.len() => {
                    let mut c = Vec::new();
                    for (j, (x, y)) in a.iter().zip(b).enumerate() {
                        if x != y {
                            c.push(json!(j));
                            c.push(y.clone());
                        }
                    }
                    out.push(json!({"i": i, "c": c}));
                }
                _ => out.push(r.clone()),
            },
            None => out.push(r.clone()),
        }
    }
    out
}

fn unpack_frames(frames: &mut [Value]) -> Result<(), String> {
    let mut prev: Map<String, Value> = Map::new();
    let mut prev_rows: Vec<Value> = Vec::new();
    for (fi, f) in frames.iter_mut().enumerate() {
        let bad = |what: &str| format!("packed frame {fi}: {what}");
        let Value::Object(mut cur) = std::mem::take(f) else {
            return Err(bad("not an object"));
        };
        if let Some(same) = cur.remove("=") {
            for k in same.as_array().ok_or_else(|| bad("`=` is not a list"))? {
                let k = k.as_str().ok_or_else(|| bad("`=` names a non-field"))?;
                let v = prev
                    .get(k)
                    .ok_or_else(|| bad(&format!("`{k}` is not in the frame before")))?;
                cur.insert(k.to_string(), v.clone());
            }
        }
        let packed = match cur.remove("n") {
            Some(Value::Array(r)) => r,
            _ => Vec::new(),
        };
        let mut rows: Vec<Value> = Vec::with_capacity(packed.len());
        for e in packed {
            match e {
                Value::Array(a) if a.first().is_some_and(Value::is_string) => {
                    rows.push(Value::Array(a));
                }
                Value::Array(a) if a.len() == 2 => {
                    let (Some(i), Some(n)) = (a[0].as_u64(), a[1].as_u64()) else {
                        return Err(bad("a run is not two counts"));
                    };
                    let (i, n) = (i as usize, n as usize);
                    let run = prev_rows
                        .get(i..i + n)
                        .ok_or_else(|| bad("a run past the frame before's rows"))?;
                    rows.extend_from_slice(run);
                }
                Value::Object(o) => {
                    let i = o
                        .get("i")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| bad("a changed row without `i`"))?
                        as usize;
                    let mut row = prev_rows
                        .get(i)
                        .cloned()
                        .ok_or_else(|| bad("a changed row past the frame before's rows"))?;
                    let c = o
                        .get("c")
                        .and_then(Value::as_array)
                        .ok_or_else(|| bad("a changed row without `c`"))?;
                    let Value::Array(cells) = &mut row else {
                        return Err(bad("a row is not a list"));
                    };
                    for pair in c.chunks(2) {
                        let [j, v] = pair else {
                            return Err(bad("a changed row's `c` is odd"));
                        };
                        let j = j.as_u64().ok_or_else(|| bad("a column is not a count"))? as usize;
                        *cells
                            .get_mut(j)
                            .ok_or_else(|| bad("a column past the row"))? = v.clone();
                    }
                    rows.push(row);
                }
                _ => return Err(bad("a row entry of no known form")),
            }
        }
        prev = cur.clone();
        prev_rows = rows.clone();
        cur.insert("n".into(), Value::Array(rows));
        *f = Value::Object(cur);
    }
    Ok(())
}

/// A trace's JSON, packed or not, unpacked (its frames whole, no
/// `packed`), with its patched frames (none when it is not packed).
pub fn unpack_json(text: &str) -> Result<(Value, BTreeMap<String, Patch>), String> {
    let mut v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut patches = BTreeMap::new();
    if let Some(o) = v.as_object_mut()
        && let Some(p) = o.remove("packed")
    {
        let format = p["format"].as_u64().unwrap_or(0);
        if format != FORMAT {
            return Err(format!(
                "a packed trace of format {format}; this strand-eval reads {FORMAT}"
            ));
        }
        if let Some(Value::Array(frames)) = o.get_mut("frames") {
            unpack_frames(frames)?;
        }
        for (name, e) in p["patches"].as_object().into_iter().flatten() {
            let patch = Patch::from_json(e).ok_or_else(|| {
                format!("packed.patches: `{name}` is not [base, [x, y, w, h], …]")
            })?;
            patches.insert(name.clone(), patch);
        }
    }
    Ok((v, patches))
}

/// Reads a trace, packed or not, with its patched frames (none when it
/// is not packed).
pub fn read(text: &str) -> Result<(RunTrace, BTreeMap<String, Patch>), String> {
    let (v, patches) = unpack_json(text)?;
    let trace = serde_json::from_value(v).map_err(|e| e.to_string())?;
    Ok((trace, patches))
}

/// A stored file's bytes by its frame's name.
type Load<'a> = Box<dyn Fn(&str) -> Option<Vec<u8>> + 'a>;

/// Rebuilds patched frames, keeping the last few rebuilt to build the
/// next on.
pub struct Composer<'a> {
    patches: std::borrow::Cow<'a, BTreeMap<String, Patch>>,
    load: Load<'a>,
    recent: VecDeque<(String, u32, u32, Vec<u8>)>,
}

impl std::fmt::Debug for Composer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composer")
            .field("patches", &self.patches.len())
            .field("recent", &self.recent.len())
            .finish()
    }
}

impl<'a> Composer<'a> {
    pub fn new(
        patches: &'a BTreeMap<String, Patch>,
        load: impl Fn(&str) -> Option<Vec<u8>> + 'a,
    ) -> Composer<'a> {
        Composer {
            patches: std::borrow::Cow::Borrowed(patches),
            load: Box::new(load),
            recent: VecDeque::new(),
        }
    }

    /// (replay-clip) One that holds its patches (`strand-eval replay
    /// clip` hands its frames to clip.js as it asks for them).
    pub fn owned(
        patches: BTreeMap<String, Patch>,
        load: impl Fn(&str) -> Option<Vec<u8>> + 'a,
    ) -> Composer<'a> {
        Composer {
            patches: std::borrow::Cow::Owned(patches),
            load: Box::new(load),
            recent: VecDeque::new(),
        }
    }

    /// Frame `name` as `(width, height, rgba)`; `None` when a file it
    /// needs is missing or unreadable, or its patches do not fit.
    pub fn frame(&mut self, name: &str) -> Option<(u32, u32, Vec<u8>)> {
        if let Some((_, w, h, px)) = self.recent.iter().find(|r| r.0 == name) {
            return Some((*w, *h, px.clone()));
        }
        // The patched frames to apply, newest first, back to a whole
        // frame or one rebuilt lately.
        let mut chain: Vec<String> = Vec::new();
        let mut at = name.to_string();
        let (w, h, mut px) = loop {
            if at != name
                && let Some((_, w, h, px)) = self.recent.iter().find(|r| r.0 == at)
            {
                break (*w, *h, px.clone());
            }
            match self.patches.get(&at) {
                Some(p) => {
                    if chain.len() > CHAIN {
                        return None;
                    }
                    chain.push(at.clone());
                    at = p.base.clone();
                }
                None => {
                    let bytes = (self.load)(&at)?;
                    break super::png::decode(&bytes).ok()?;
                }
            }
        };
        if chain.is_empty() {
            self.remember(name, w, h, &px);
        }
        for n in chain.iter().rev() {
            let boxes = &self.patches.get(n)?.boxes;
            if !boxes.is_empty() {
                let bytes = (self.load)(n)?;
                let (sw, sh, strip) = super::png::decode(&bytes).ok()?;
                let (row, srow) = (w as usize * 4, sw as usize * 4);
                let mut sy = 0usize;
                for &[x, y, bw, bh] in boxes {
                    if x + bw > w || y + bh > h || bw > sw || sy + bh as usize > sh as usize {
                        return None;
                    }
                    let n = bw as usize * 4;
                    for r in 0..bh as usize {
                        let o = (y as usize + r) * row + x as usize * 4;
                        let from = (sy + r) * srow;
                        px[o..o + n].copy_from_slice(&strip[from..from + n]);
                    }
                    sy += bh as usize;
                }
            }
            self.remember(n, w, h, &px);
        }
        Some((w, h, px))
    }

    fn remember(&mut self, name: &str, w: u32, h: u32, px: &[u8]) {
        self.recent.push_back((name.to_string(), w, h, px.to_vec()));
        if self.recent.len() > 4 {
            self.recent.pop_front();
        }
    }
}

/// Every frame `trace` names, rebuilt from a packed report's stored
/// files (`load` reads one by its path): whole PNGs, as an unpacked
/// report holds them.
pub fn unpack_images(
    trace: &RunTrace,
    patches: &BTreeMap<String, Patch>,
    load: impl Fn(&str) -> Option<Vec<u8>>,
) -> Result<BTreeMap<String, Image>, Error> {
    let mut out = BTreeMap::new();
    let mut c = Composer::new(patches, |name: &str| {
        let path = if patches.contains_key(name) {
            patch_file(name)
        } else {
            name.to_string()
        };
        load(&path)
    });
    for f in &trace.frames {
        for name in f.png.values() {
            if out.contains_key(name) {
                continue;
            }
            if !patches.contains_key(name) {
                if let Some(bytes) = load(name) {
                    let (width, height) = super::png_size(&bytes).unwrap_or((0, 0));
                    out.insert(
                        name.clone(),
                        Image {
                            width,
                            height,
                            png: bytes,
                        },
                    );
                }
                continue;
            }
            if let Some((w, h, px)) = c.frame(name) {
                out.insert(
                    name.clone(),
                    Image {
                        width: w,
                        height: h,
                        png: super::png::encode(w, h, &px)?,
                    },
                );
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Frame;

    /// Frames of a 100 × 70 surface, each a few scattered changes from
    /// the one before (across tile edges, at the edges of the frame,
    /// alpha alone), one of a new size and one changed all over.
    fn frames() -> Vec<(u32, u32, Vec<u8>)> {
        let (w, h) = (100u32, 70u32);
        let mut px: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        let mut out = vec![(w, h, px.clone())];
        let mut seed = 7u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            seed >> 8
        };
        for n in 0..40 {
            for _ in 0..(1 + n % 5) {
                let (x, y) = (next() % w, next() % h);
                let c = ((y * w + x) * 4 + next() % 4) as usize;
                px[c] = px[c].wrapping_add(1 + (next() % 200) as u8);
            }
            if n == 10 {
                // The corners and a run across a tile's edge.
                for (x, y) in [(0, 0), (w - 1, h - 1), (31, 5), (32, 5), (33, 5)] {
                    px[((y * w + x) * 4 + 3) as usize] ^= 0x55;
                }
            }
            if n == 20 {
                px.iter_mut().for_each(|v| *v = v.wrapping_add(3));
            }
            out.push((w, h, px.clone()));
            if n == 30 {
                out.push((40, 30, vec![9; 40 * 30 * 4]));
            }
        }
        out
    }

    #[test]
    fn every_frame_is_rebuilt_exactly() {
        let mut trace = RunTrace::default();
        let mut images = BTreeMap::new();
        for (k, (w, h, px)) in frames().into_iter().enumerate() {
            let name = format!("frames/{k:04}.png");
            images.insert(
                name.clone(),
                Image {
                    width: w,
                    height: h,
                    png: super::super::png::encode(w, h, &px).unwrap(),
                },
            );
            trace.frames.push(Frame {
                k: k as u32,
                png: BTreeMap::from([(1, name)]),
                ..Frame::default()
            });
        }
        let p = pack(&trace, &images).unwrap();
        assert!(p.patches.len() > 30, "{} patched", p.patches.len());
        assert!(
            p.patches.values().any(|q| q.boxes.len() > 1),
            "no patch of more than one box"
        );
        let stored: usize = p.files.values().map(Vec::len).sum();
        let whole: usize = images.values().map(|i| i.png.len()).sum();
        assert!(stored < whole, "{stored} of {whole}");
        let (back, patches) = read(&p.trace).unwrap();
        assert_eq!(back, trace);
        assert_eq!(patches, p.patches);
        let rebuilt = unpack_images(&trace, &patches, |path| {
            let name = path.replace(".patch.png", ".png");
            p.files.get(&name).cloned()
        })
        .unwrap();
        for (name, im) in &images {
            assert_eq!(
                super::super::png::decode(&rebuilt[name].png).unwrap(),
                super::super::png::decode(&im.png).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn changed_boxes_are_runs_of_changed_tiles_shrunk_to_their_changes() {
        // Tiles 0-31, 32-63, 64-95, 96-99 across; bands 0-31, 32-39.
        let (w, h) = (100u32, 40u32);
        let a = vec![0u8; (w * h * 4) as usize];
        let mut b = a.clone();
        for (x, y) in [(1, 1), (70, 1), (71, 2), (99, 39), (31, 33), (32, 33)] {
            b[((y * w + x) * 4) as usize] = 1;
        }
        // A run of changed tiles is one box, shrunk to its changes.
        let boxes = changed(w, h, &a, &b);
        assert_eq!(
            boxes,
            vec![[1, 1, 1, 1], [70, 1, 2, 2], [31, 33, 2, 1], [99, 39, 1, 1]]
        );
        assert!(changed(w, h, &a, &a).is_empty());
    }
}
