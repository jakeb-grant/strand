//! `strand-site media`: an example's frames and timing from eval runs of
//! its scenario and its dark twin (`scripts/site/media.sh` runs them,
//! each held to its approved reference recording). Per mode, the first
//! frame is kept whole (`site/media/<name>-light.png`) and the part of
//! the frame that changes, from every distinct frame, goes once into a
//! strip over it (`<name>-light-moving.png`, one under another), both
//! lossless. `site/media/<name>.json` keeps, per tick, which frame shows,
//! the tick's time, the steps that acted, and each springing node's
//! value (what the trace recorded, nothing measured), and per mode each
//! frame's pixel hash, each file's hash and the seal of the reference
//! recording the run matched.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde_json::{Value, json};

use crate::example;

/// One mode's run, read from its `trace.json`.
struct Run {
    trace: Value,
    dir: std::path::PathBuf,
}

fn read_run(dir: &Path) -> Result<Run, String> {
    let path = dir.join("trace.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let trace: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if trace["schema"].as_u64() != Some(1) {
        return Err(format!("{}: not a schema 1 trace", path.display()));
    }
    Ok(Run {
        trace,
        dir: dir.to_path_buf(),
    })
}

fn frames(run: &Run) -> Result<&Vec<Value>, String> {
    run.trace["frames"]
        .as_array()
        .filter(|f| !f.is_empty())
        .ok_or_else(|| format!("{}: no frames", run.dir.display()))
}

/// The one surface's id (of a run that draws one).
fn surface(run: &Run) -> Result<String, String> {
    let s = run.trace["surfaces"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    match s.as_slice() {
        [one] => one["id"]
            .as_u64()
            .map(|i| i.to_string())
            .ok_or_else(|| "a surface with no id".to_string()),
        _ => Err(format!(
            "{}: an example draws one surface, this run drew {}",
            run.dir.display(),
            s.len()
        )),
    }
}

/// An RGBA8 frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Image {
    /// The `w`×`h` part of it at (`x`, `y`).
    fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Image {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let at = ((row * self.w + x) * 4) as usize;
            rgba.extend_from_slice(&self.rgba[at..at + (w * 4) as usize]);
        }
        Image { w, h, rgba }
    }

    /// blake3 of its size and pixels: what the reader sees, whatever the
    /// file's encoding.
    fn hash(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(&self.w.to_le_bytes());
        h.update(&self.h.to_le_bytes());
        h.update(&self.rgba);
        h.finalize().to_hex().to_string()
    }
}

/// A PNG file as RGBA8, whatever its colour type (a palette expanded).
pub fn decode(path: &Path) -> Result<Image, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = File::open(path).map_err(|e| at(&e))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| at(&e))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| at(&"too large"))?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| at(&e))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(at(&"not 8-bit"));
    }
    let px = (info.width * info.height) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => buf[..px * 3]
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        other => return Err(at(&format!("colour type {other:?}"))),
    };
    Ok(Image {
        w: info.width,
        h: info.height,
        rgba,
    })
}

/// Writes `frames`, one under another, as one lossless PNG: indexed when
/// they hold 256 colours or fewer (every browser decodes a palette
/// natively, and a strip of a few flat colours and their edges is a
/// fraction of the size), else RGB when every pixel is opaque, else RGBA.
pub fn write_strip(path: &Path, frames: &[Image]) -> Result<(), String> {
    let first = frames.first().ok_or("no frames")?;
    if frames.iter().any(|f| f.w != first.w || f.h != first.h) {
        return Err(format!("{}: frames of different sizes", path.display()));
    }
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut colours: HashMap<[u8; 4], u8> = HashMap::new();
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut indexed = true;
    'frames: for f in frames {
        for p in f.rgba.chunks_exact(4) {
            let c = [p[0], p[1], p[2], p[3]];
            if let std::collections::hash_map::Entry::Vacant(e) = colours.entry(c) {
                if palette.len() == 256 {
                    indexed = false;
                    break 'frames;
                }
                e.insert(palette.len() as u8);
                palette.push(c);
            }
        }
    }
    let opaque = frames
        .iter()
        .all(|f| f.rgba.chunks_exact(4).all(|p| p[3] == 255));
    let file = File::create(path).map_err(|e| at(&e))?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        first.w,
        first.h * frames.len() as u32,
    );
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    let mut data = Vec::new();
    if indexed {
        enc.set_color(png::ColorType::Indexed);
        // Palette indices do not predict one another: filters only cost.
        enc.set_filter(png::Filter::NoFilter);
        enc.set_palette(
            palette
                .iter()
                .flat_map(|c| [c[0], c[1], c[2]])
                .collect::<Vec<_>>(),
        );
        if !opaque {
            enc.set_trns(palette.iter().map(|c| c[3]).collect::<Vec<_>>());
        }
        for f in frames {
            data.extend(
                f.rgba
                    .chunks_exact(4)
                    .map(|p| colours[&[p[0], p[1], p[2], p[3]]]),
            );
        }
    } else if opaque {
        enc.set_color(png::ColorType::Rgb);
        for f in frames {
            data.extend(f.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
        }
    } else {
        enc.set_color(png::ColorType::Rgba);
        for f in frames {
            data.extend_from_slice(&f.rgba);
        }
    }
    let mut w = enc.write_header().map_err(|e| at(&e))?;
    w.write_image_data(&data).map_err(|e| at(&e))?;
    w.finish().map_err(|e| at(&e))
}

/// Pixels kept around the part that changes: unchanged ones, so the
/// moving strip's edge lies over pixels the still frame shows alike,
/// however the browser scales the two.
const MARGIN: u32 = 8;

/// How far inside the moving box the still frame's hole starts, in the
/// frames' px: the width of the band both draw (`moving_box`).
pub const RING: u32 = 3;

/// The box (x, y, w, h) holding every pixel in which any frame differs
/// from the first, widened by [`MARGIN`] within the frame; `None` when
/// none differs.
pub fn moving_box(frames: &[Image]) -> Option<(u32, u32, u32, u32)> {
    let base = frames.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for f in &frames[1..] {
        for (i, (a, b)) in f
            .rgba
            .chunks_exact(4)
            .zip(base.rgba.chunks_exact(4))
            .enumerate()
        {
            if a != b {
                let (x, y) = (i as u32 % base.w, i as u32 / base.w);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 == u32::MAX {
        return None;
    }
    let mut x0 = x0.saturating_sub(MARGIN);
    let mut y0 = y0.saturating_sub(MARGIN);
    let mut x1 = (x1 + MARGIN).min(base.w - 1);
    let mut y1 = (y1 + MARGIN).min(base.h - 1);
    // The page draws the moving part over the still frame, which has a
    // hole RING px inside the box's edge (example.rs, `hole`): both are
    // scaled to the page's pixels, so the moving part's own edge is soft,
    // and the still under that ring covers it. The ring is drawn twice,
    // which is seamless where its pixels are clear or solid in every
    // frame, so the box grows until they are (a bar's shadow, a glass
    // surface: the box goes past them). At the frame's edge there is no
    // ring and nothing to cover.
    let soft = |x: u32, y: u32| {
        frames.iter().any(|f| {
            let a = f.rgba[((y * f.w + x) * 4 + 3) as usize];
            a != 0 && a != 255
        })
    };
    let ring = |from: u32, to: u32| from..=to.min(from + RING - 1);
    let ring_back = |from: u32, to: u32| from.saturating_sub(RING - 1).max(to)..=from;
    loop {
        let mut grew = false;
        if x0 > 0 && ring(x0, x1).any(|x| (y0..=y1).any(|y| soft(x, y))) {
            x0 -= 1;
            grew = true;
        }
        if x1 + 1 < base.w && ring_back(x1, x0).any(|x| (y0..=y1).any(|y| soft(x, y))) {
            x1 += 1;
            grew = true;
        }
        if y0 > 0 && ring(y0, y1).any(|y| (x0..=x1).any(|x| soft(x, y))) {
            y0 -= 1;
            grew = true;
        }
        if y1 + 1 < base.h && ring_back(y1, y0).any(|y| (x0..=x1).any(|x| soft(x, y))) {
            y1 += 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    Some((x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

fn file_hash(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// One mode's media: the first frame whole (`<name>-<mode>.png`) and,
/// when anything moves, the part that changes of every distinct frame,
/// one under another (`<name>-<mode>-moving.png`), with which of them
/// each tick shows, each frame's pixel hash and each file's hash.
/// Each tick's frame of a run that draws one surface: its buffers as
/// they are. Its ticks index the distinct buffers, and the size is the
/// surface's, in logical px.
#[allow(clippy::type_complexity)]
fn one(run: &Run) -> Result<(Vec<usize>, Vec<Image>, [f64; 2]), String> {
    let sid = surface(run)?;
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut ticks = Vec::new();
    let mut last = None;
    for f in frames(run)? {
        let here = match f["png"][&sid].as_str() {
            Some(p) => {
                let next = order.len();
                let i = *index.entry(p.to_string()).or_insert_with(|| {
                    order.push(p.to_string());
                    next
                });
                Some(i)
            }
            None => last,
        };
        let i = here.ok_or_else(|| {
            format!(
                "{}: the first tick has no frame; run with --frames all",
                run.dir.display()
            )
        })?;
        ticks.push(i);
        last = Some(i);
    }
    let images: Vec<Image> = order
        .iter()
        .map(|p| decode(&run.dir.join(p)))
        .collect::<Result<_, _>>()?;
    let size = frames(run)?[0]["sizes"][&sid]
        .as_array()
        .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
        .ok_or("a frame with no size")?;
    Ok((ticks, images, size))
}

/// Whether a run's surfaces drew buffers of more than one size.
fn resizes(run: &Run) -> Result<bool, String> {
    let mut seen: HashMap<String, Value> = HashMap::new();
    for f in frames(run)? {
        for (sid, size) in f["sizes"].as_object().into_iter().flatten() {
            if seen.entry(sid.clone()).or_insert_with(|| size.clone()) != size {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Each tick's frame of a run that draws several surfaces (a bar and
/// its popup): the shown surfaces' latest buffers, each where it lies on
/// the output (`origins`), in the order the run made them (a popup over
/// its parent), on one transparent picture as large as every place they
/// held. Its ticks index the distinct pictures, and the size is in
/// logical px.
#[allow(clippy::type_complexity)]
fn composed(run: &Run) -> Result<(Vec<usize>, Vec<Image>, [f64; 2]), String> {
    let scale = run.trace["scenario"]["scale"].as_f64().unwrap_or(1.0);
    let mut cache: HashMap<String, Image> = HashMap::new();
    let mut latest: HashMap<String, String> = HashMap::new();
    // Per tick, the shown surfaces as (id, buffer, x, y) in device px.
    let mut ticks: Vec<Vec<(u64, String, i64, i64)>> = Vec::new();
    for f in frames(run)? {
        if let Some(png) = f["png"].as_object() {
            for (sid, p) in png {
                if let Some(p) = p.as_str() {
                    latest.insert(sid.clone(), p.to_string());
                }
            }
        }
        let mut shown = Vec::new();
        for (sid, _) in f["sizes"].as_object().into_iter().flatten() {
            let Some(p) = latest.get(sid) else { continue };
            if !cache.contains_key(p) {
                cache.insert(p.clone(), decode(&run.dir.join(p))?);
            }
            let o = &f["origins"][sid];
            let at = |i: usize| (o[i].as_f64().unwrap_or(0.0) * scale).round() as i64;
            shown.push((sid.parse().unwrap_or(0), p.clone(), at(0), at(1)));
        }
        shown.sort();
        ticks.push(shown);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for (_, p, x, y) in ticks.iter().flatten() {
        let img = &cache[p];
        x0 = x0.min(*x);
        y0 = y0.min(*y);
        x1 = x1.max(x + i64::from(img.w));
        y1 = y1.max(y + i64::from(img.h));
    }
    if x0 >= x1 || y0 >= y1 {
        return Err(format!(
            "{}: the first tick has no frame; run with --frames all",
            run.dir.display()
        ));
    }
    let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let mut index: HashMap<Vec<(u64, String, i64, i64)>, usize> = HashMap::new();
    let mut images = Vec::new();
    let mut at = Vec::new();
    for shown in ticks {
        let next = images.len();
        let i = match index.get(&shown) {
            Some(&i) => i,
            None => {
                let mut pic = Image {
                    w,
                    h,
                    rgba: vec![0; (w * h * 4) as usize],
                };
                for (_, p, x, y) in &shown {
                    over(&mut pic, &cache[p], (x - x0) as u32, (y - y0) as u32);
                }
                images.push(pic);
                index.insert(shown, next);
                next
            }
        };
        at.push(i);
    }
    Ok((at, images, [f64::from(w) / scale, f64::from(h) / scale]))
}

/// `top` drawn over `pic` at (`x`, `y`), straight alpha, source over.
fn over(pic: &mut Image, top: &Image, x: u32, y: u32) {
    for row in 0..top.h {
        for col in 0..top.w {
            let s = ((row * top.w + col) * 4) as usize;
            let d = (((y + row) * pic.w + x + col) * 4) as usize;
            let sa = f32::from(top.rgba[s + 3]) / 255.0;
            if sa == 0.0 {
                continue;
            }
            let da = f32::from(pic.rgba[d + 3]) / 255.0;
            if sa == 1.0 || da == 0.0 {
                pic.rgba[d..d + 4].copy_from_slice(&top.rgba[s..s + 4]);
                continue;
            }
            let oa = sa + da * (1.0 - sa);
            for c in 0..3 {
                let v = (f32::from(top.rgba[s + c]) * sa
                    + f32::from(pic.rgba[d + c]) * da * (1.0 - sa))
                    / oa;
                pic.rgba[d + c] = v.round().clamp(0.0, 255.0) as u8;
            }
            pic.rgba[d + 3] = (oa * 255.0).round() as u8;
        }
    }
}

/// The ticks a run painted (`k`), one per frame of its trace.
fn painted(run: &Run) -> Result<Vec<u64>, String> {
    Ok(frames(run)?
        .iter()
        .map(|f| f["k"].as_u64().unwrap_or(0))
        .collect())
}

/// `per_frame` (one value per frame of `run`) on the `timeline`'s ticks:
/// at each, the value of the run's last frame at or before it, since a
/// tick it did not paint shows what it painted last. The two modes'
/// runs share the steps' ticks, but a spring may settle a tick or two
/// sooner in one (a colour nearer its target in the dark).
fn on_timeline<T: Clone>(run: &Run, per_frame: &[T], timeline: &[u64]) -> Result<Vec<T>, String> {
    let ks = painted(run)?;
    let mut j = 0;
    let mut out = Vec::with_capacity(timeline.len());
    for &k in timeline {
        while j + 1 < ks.len() && ks[j + 1] <= k {
            j += 1;
        }
        out.push(per_frame.get(j).cloned().ok_or("no frames")?);
    }
    Ok(out)
}

fn strip(
    run: &Run,
    timeline: &[u64],
    out: &Path,
    name: &str,
    mode: &str,
) -> Result<(Value, [f64; 2]), String> {
    let several = run.trace["surfaces"]
        .as_array()
        .is_some_and(|s| s.len() > 1);
    // A surface that grows or shrinks (a launcher's card fitting its
    // rows) is composed at its origins too, on a picture as large as
    // every place it held, so every frame of the strip has one size.
    let (ticks, images, size) = if several || resizes(run)? {
        composed(run)?
    } else {
        one(run)?
    };
    let ticks = on_timeline(run, &ticks, timeline)?;
    let first = images.first().ok_or("no frames")?;
    if images.iter().any(|f| f.w != first.w || f.h != first.h) {
        return Err(format!("{}: frames of different sizes", run.dir.display()));
    }
    let base = format!("{name}-{mode}.png");
    write_strip(&out.join(&base), std::slice::from_ref(first))?;
    let mut files = serde_json::Map::new();
    files.insert(base.clone(), json!(file_hash(&out.join(&base))?));
    let mut v = json!({
        "base": base,
        "frame": [first.w, first.h],
        "count": images.len(),
        "ticks": ticks,
        "frames": images.iter().map(Image::hash).collect::<Vec<_>>(),
    });
    if let Some((x, y, w, h)) = moving_box(&images) {
        let crops: Vec<Image> = images.iter().map(|f| f.crop(x, y, w, h)).collect();
        let sprite = format!("{name}-{mode}-moving.png");
        write_strip(&out.join(&sprite), &crops)?;
        files.insert(sprite.clone(), json!(file_hash(&out.join(&sprite))?));
        v["sprite"] = json!(sprite);
        v["box"] = json!([x, y, w, h]);
    }
    v["files"] = Value::Object(files);
    Ok((v, size))
}

/// A transition as the trace has it, written as Strand writes it.
fn curve_text(c: &Value) -> String {
    if let Some(s) = c.get("spring") {
        return format!(
            "spring({}, {})",
            num(s["k"].as_f64().unwrap_or(0.0)),
            num(s["z"].as_f64().unwrap_or(0.0))
        );
    }
    if let Some(t) = c.get("timed") {
        let ms = t["d"].as_f64().unwrap_or(0.0) / 1e6;
        let ease = t["ease"]["bezier"]
            .as_array()
            .map(|b| {
                let b: Vec<String> = b.iter().filter_map(Value::as_f64).map(num).collect();
                format!(", bezier({})", b.join(", "))
            })
            .unwrap_or_default();
        return format!("{}ms{ease}", num(ms));
    }
    c.to_string()
}

fn num(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if (r - r.round()).abs() < 1e-9 {
        format!("{}", r as i64)
    } else {
        format!("{r}")
    }
}

/// Each node whose `x` or `y` springs: its value per tick (written plus
/// the spring's part, as the trace records them), and its curve.
fn tracks(run: &Run, timeline: &[u64]) -> Result<Vec<Value>, String> {
    let cols: Vec<&str> = run.trace["cols"]
        .as_array()
        .ok_or("no cols")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let col = |n: &str| {
        cols.iter()
            .position(|c| *c == n)
            .ok_or(format!("no `{n}` column"))
    };
    let (key_c, wx, wy, spx, spy) = (
        col("key")?,
        col("wx")?,
        col("wy")?,
        col("spx")?,
        col("spy")?,
    );
    // (key, prop) in the order they first spring, with their curve.
    let mut found: Vec<(String, String, String)> = Vec::new();
    for f in frames(run)? {
        for sp in f["sp"].as_array().into_iter().flatten() {
            let (Some(key), Some(of)) = (sp["key"].as_str(), sp["of"].as_str()) else {
                continue;
            };
            if (of == "x" || of == "y") && !found.iter().any(|(k, p, _)| k == key && p == of) {
                found.push((key.to_string(), of.to_string(), curve_text(&sp["c"])));
            }
        }
    }
    let nodes = &run.trace["nodes"];
    let line_of = |key: &str| -> (u64, u64) {
        let origin = nodes[key]["origin"].as_str().unwrap_or("");
        let mut it = origin.rsplit(':');
        let col = it.next().and_then(|c| c.parse().ok()).unwrap_or(0);
        let line = it.next().and_then(|c| c.parse().ok()).unwrap_or(0);
        (line, col)
    };
    found.sort_by_key(|(k, p, _)| (line_of(k), p.clone()));
    let mut out = Vec::new();
    for (key, prop, curve) in found {
        let (wc, sc) = if prop == "x" { (wx, spx) } else { (wy, spy) };
        let mut values = Vec::new();
        let mut last = 0.0;
        for f in frames(run)? {
            let row = f["n"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r[key_c].as_str() == Some(key.as_str()));
            if let Some(r) = row {
                last = r[wc].as_f64().unwrap_or(0.0) + r[sc].as_f64().unwrap_or(0.0);
            }
            values.push((last * 100.0).round() / 100.0);
        }
        let values = on_timeline(run, &values, timeline)?;
        let node = match nodes[&key]["id"].as_str() {
            Some(id) => format!("#{id}"),
            None => nodes[&key]["label"].as_str().unwrap_or(&key).to_string(),
        };
        out.push(json!({ "node": node, "prop": prop, "curve": curve, "values": values }));
    }
    Ok(out)
}

/// The steps that act (not `rest`, `mark` or `expect`), each at the
/// first frame of the `timeline` from its tick on.
fn marks(run: &Run, timeline: &[u64]) -> Vec<Value> {
    run.trace["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let text = e["text"].as_str()?;
            let word = text.split_whitespace().next()?;
            (!matches!(
                word,
                "rest" | "mark" | "expect" | "intend" | "frames" | "wait"
            ))
            .then(|| {
                let k = e["k"].as_u64().unwrap_or(0);
                let at = timeline.partition_point(|&t| t < k);
                json!({ "k": at, "text": text })
            })
        })
        .collect()
}

/// Renders example `name`'s media from its light run and its dark
/// twin's run into `out` (`site/media/`, or elsewhere to compare), each
/// mode with the seal of the approved reference recording its run was
/// held to (eval/refs/<id>.ref), and returns the media's JSON.
pub fn render_into(
    root: &Path,
    name: &str,
    light: &Path,
    dark: &Path,
    out: &Path,
) -> Result<Value, String> {
    let s = example::load(root, name)?;
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let light = read_run(light)?;
    let dark = read_run(dark)?;
    // The timeline: every tick either mode painted, in order, at its
    // time on the shared virtual clock.
    let mut when: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    for run in [&light, &dark] {
        for f in frames(run)? {
            when.entry(f["k"].as_u64().unwrap_or(0))
                .or_insert(f["t_ns"].as_f64().unwrap_or(0.0));
        }
    }
    let timeline: Vec<u64> = when.keys().copied().collect();
    let t0 = when.values().next().copied().unwrap_or(0.0);
    let t: Vec<f64> = when
        .values()
        .map(|ns| ((ns - t0) / 1e6 * 10.0).round() / 10.0)
        .collect();
    let mut modes = serde_json::Map::new();
    let mut size = [0.0, 0.0];
    for (mode, run, id) in [
        ("light", &light, s.id.clone()),
        ("dark", &dark, example::dark_id(&s.id)),
    ] {
        let (mut v, sz) = strip(run, &timeline, out, name, mode)?;
        if mode == "light" {
            size = sz;
        }
        let seal = example::approved_seal(root, &id)?;
        v["reference"] = json!({ "id": id, "seal": seal });
        modes.insert(mode.into(), v);
    }
    let v = json!({
        "scenario": s.path,
        "source": s.hash,
        "hz": light.trace["scenario"]["hz"],
        "scale": light.trace["scenario"]["scale"],
        "size": size,
        "t": t,
        "marks": marks(&light, &timeline),
        "tracks": tracks(&light, &timeline)?,
        "modes": modes,
    });
    let path = out.join(format!("{name}.json"));
    let text = serde_json::to_string(&v).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(v)
}

/// Renders example `name`'s media into `site/media/`.
pub fn render(root: &Path, name: &str, light: &Path, dark: &Path) -> Result<(), String> {
    render_into(root, name, light, dark, &root.join(example::MEDIA)).map(|_| ())
}

/// Renders example `name`'s media again from fresh runs, elsewhere, and
/// says how they differ from the committed ones: a renderer that now
/// draws other pixels, or other ticks, fails here.
pub fn compare(root: &Path, name: &str, light: &Path, dark: &Path) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("strand-site-media-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let fresh = render_into(root, name, light, dark, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    let fresh = fresh?;
    let path = root.join(example::MEDIA).join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let kept: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut diffs = Vec::new();
    for key in ["size", "t", "marks", "tracks", "hz", "scale"] {
        if fresh[key] != kept[key] {
            diffs.push(format!("its `{key}`"));
        }
    }
    for mode in ["light", "dark"] {
        let (f, k) = (&fresh["modes"][mode], &kept["modes"][mode]);
        for key in ["frames", "ticks", "box", "frame"] {
            if f[key] != k[key] {
                diffs.push(format!("its {mode} `{key}`"));
            }
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "example `{name}`: the renderer now draws it otherwise than its committed media ({}); \
             look at the runs, and once they are right run scripts/site/media.sh {name}",
            diffs.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Image {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend(f(x, y));
            }
        }
        Image { w, h, rgba }
    }

    #[test]
    fn a_strip_reads_back_pixel_for_pixel_in_every_encoding() {
        let dir = std::env::temp_dir().join(format!("strand-site-strip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Few colours (indexed), many opaque ones (RGB), many with alpha.
        let few = image(5, 3, |x, y| [x as u8 * 40, y as u8 * 60, 9, 255]);
        let many = image(40, 10, |x, y| {
            [x as u8 * 6, y as u8 * 20, (x * y) as u8, 255]
        });
        let clear = image(40, 10, |x, y| [x as u8 * 6, y as u8 * 20, 1, (x * 6) as u8]);
        let few_clear = image(4, 2, |x, _| [1, 2, 3, x as u8 * 80]);
        for (n, frames) in [
            ("few", vec![few.clone(), few.clone()]),
            ("many", vec![many]),
            ("clear", vec![clear]),
            ("few-clear", vec![few_clear]),
        ] {
            let path = dir.join(format!("{n}.png"));
            write_strip(&path, &frames).unwrap();
            let back = decode(&path).unwrap();
            let mut want = Vec::new();
            for f in &frames {
                want.extend_from_slice(&f.rgba);
            }
            assert_eq!(back.rgba, want, "{n}");
            assert_eq!(back.h, frames[0].h * frames.len() as u32, "{n}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (docs-launcher) One surface whose buffer shrinks, and whose
    /// origin moves as it stays centred, is composed at its origins on
    /// one picture as large as every place it held: each tick's frame
    /// the same size, each buffer where it lay.
    #[test]
    fn a_surface_that_shrinks_is_composed_at_its_origins() {
        let dir = std::env::temp_dir().join(format!("strand-site-shrink-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        let tall = image(6, 8, |_, _| [200, 0, 0, 255]);
        let short = image(6, 4, |_, _| [0, 0, 200, 255]);
        write_strip(&dir.join("frames/tall.png"), std::slice::from_ref(&tall)).unwrap();
        write_strip(&dir.join("frames/short.png"), std::slice::from_ref(&short)).unwrap();
        let run = Run {
            trace: json!({
                "schema": 1,
                "scenario": {"scale": 2.0},
                "surfaces": [{"id": 1}],
                "frames": [
                    {"k": 1, "sizes": {"1": [3.0, 4.0]}, "origins": {"1": [1.0, 0.0]},
                     "png": {"1": "frames/tall.png"}},
                    {"k": 2, "sizes": {"1": [3.0, 2.0]}, "origins": {"1": [1.0, 1.0]},
                     "png": {"1": "frames/short.png"}},
                    {"k": 3, "sizes": {"1": [3.0, 2.0]}, "origins": {"1": [1.0, 1.0]},
                     "png": {}},
                ],
            }),
            dir: dir.clone(),
        };
        assert!(resizes(&run).unwrap());
        let (ticks, images, size) = composed(&run).unwrap();
        assert_eq!(ticks, vec![0, 1, 1]);
        assert_eq!(size, [3.0, 4.0]);
        assert!(images.iter().all(|i| (i.w, i.h) == (6, 8)));
        let px = |i: &Image, y: u32| i.rgba[(y * i.w * 4) as usize..][..4].to_vec();
        assert_eq!(px(&images[1], 0), vec![0, 0, 0, 0], "above the short one");
        assert_eq!(px(&images[1], 2), vec![0, 0, 200, 255]);
        assert_eq!(px(&images[1], 6), vec![0, 0, 0, 0], "below it");
        let still = Run {
            trace: json!({"schema": 1, "surfaces": [{"id": 1}], "frames": [
                {"k": 1, "sizes": {"1": [3.0, 4.0]}}, {"k": 2, "sizes": {"1": [3.0, 4.0]}},
            ]}),
            dir: dir.clone(),
        };
        assert!(!resizes(&still).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_moving_box_ends_on_clear_or_solid_pixels() {
        // A solid bar over a soft shadow (rows 10 to 13, half alpha), on
        // clear: a change on the bar grows the box past the shadow's
        // rows and across it, so no ring of the box lies on it.
        let draw = |dot: bool| {
            image(60, 40, move |x, y| match y {
                0..10 if dot && (28..32).contains(&x) && (4..6).contains(&y) => [255, 0, 0, 255],
                0..10 => [20, 20, 20, 255],
                10..14 => [0, 0, 0, 128],
                _ => [0, 0, 0, 0],
            })
        };
        let (x, y, w, h) = moving_box(&[draw(false), draw(true)]).unwrap();
        assert_eq!((x, y), (0, 0));
        // Below the shadow, RING clear rows end the box.
        assert_eq!((x + w, y + h), (60, 14 + RING), "{:?}", (x, y, w, h));
    }

    #[test]
    fn the_moving_box_holds_every_changed_pixel_and_a_margin() {
        let still = image(60, 40, |_, _| [0, 0, 0, 255]);
        let moved = image(60, 40, |x, y| {
            if (20..25).contains(&x) && (30..32).contains(&y) {
                [255, 0, 0, 255]
            } else {
                [0, 0, 0, 255]
            }
        });
        assert_eq!(moving_box(&[still.clone(), still.clone()]), None);
        let (x, y, w, h) = moving_box(&[still.clone(), moved.clone()]).unwrap();
        assert_eq!((x, y), (20 - MARGIN, 30 - MARGIN));
        assert_eq!((x + w, y + h), (25 + MARGIN, 40), "kept within the frame");
        // Outside the box every frame is the first one: the still frame
        // with the moving part over it is each frame, pixel for pixel.
        for f in [&still, &moved] {
            let c = f.crop(x, y, w, h);
            let mut drawn = still.clone();
            for row in 0..h {
                let at = (((y + row) * drawn.w + x) * 4) as usize;
                let from = (row * w * 4) as usize;
                drawn.rgba[at..at + (w * 4) as usize]
                    .copy_from_slice(&c.rgba[from..from + (w * 4) as usize]);
            }
            assert_eq!(&drawn, f);
        }
    }
}
