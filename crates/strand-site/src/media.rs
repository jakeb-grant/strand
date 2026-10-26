//! `strand-site media`: an example's frames and timing from eval runs of
//! its scenario (`scripts/site/media.sh` runs them, light and dark). Each
//! distinct frame goes once into a strip (`site/media/<name>-light.png`,
//! one frame under another) and `site/media/<name>.json` keeps, per tick,
//! which frame shows, the tick's time, the steps that acted, and each
//! springing node's value: what the trace recorded, nothing measured.

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

/// The one surface's id (an example draws one).
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
struct Image {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}

fn decode(path: &Path) -> Result<Image, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = File::open(path).map_err(|e| at(&e))?;
    let decoder = png::Decoder::new(BufReader::new(file));
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

/// Writes `frames`, one under another, as one PNG (RGB when every pixel
/// is opaque).
fn write_strip(path: &Path, frames: &[Image]) -> Result<(), String> {
    let first = frames.first().ok_or("no frames")?;
    if frames.iter().any(|f| f.w != first.w || f.h != first.h) {
        return Err(format!("{}: frames of different sizes", path.display()));
    }
    let opaque = frames
        .iter()
        .all(|f| f.rgba.chunks_exact(4).all(|p| p[3] == 255));
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = File::create(path).map_err(|e| at(&e))?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        first.w,
        first.h * frames.len() as u32,
    );
    enc.set_color(if opaque {
        png::ColorType::Rgb
    } else {
        png::ColorType::Rgba
    });
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    let mut w = enc.write_header().map_err(|e| at(&e))?;
    let mut data = Vec::new();
    for f in frames {
        if opaque {
            data.extend(f.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
        } else {
            data.extend_from_slice(&f.rgba);
        }
    }
    w.write_image_data(&data).map_err(|e| at(&e))?;
    w.finish().map_err(|e| at(&e))
}

/// The strip for one mode, and which of its frames each tick shows.
fn strip(run: &Run, out: &Path) -> Result<(usize, Vec<usize>, [f64; 2]), String> {
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
    write_strip(out, &images)?;
    let size = frames(run)?[0]["sizes"][&sid]
        .as_array()
        .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
        .ok_or("a frame with no size")?;
    Ok((order.len(), ticks, size))
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
fn tracks(run: &Run) -> Result<Vec<Value>, String> {
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
        let node = match nodes[&key]["id"].as_str() {
            Some(id) => format!("#{id}"),
            None => nodes[&key]["label"].as_str().unwrap_or(&key).to_string(),
        };
        out.push(json!({ "node": node, "prop": prop, "curve": curve, "values": values }));
    }
    Ok(out)
}

/// The steps that act (not `rest`, `mark` or `expect`), at their ticks.
fn marks(run: &Run) -> Vec<Value> {
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
            .then(|| json!({ "k": e["k"], "text": text }))
        })
        .collect()
}

/// Renders example `name`'s media from its light run (and dark run, when
/// given) into `site/media/`.
pub fn render(root: &Path, name: &str, light: &Path, dark: Option<&Path>) -> Result<(), String> {
    let s = example::load(root, name)?;
    let out = root.join(example::MEDIA);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let light = read_run(light)?;
    let t0 = frames(&light)?[0]["t_ns"].as_f64().unwrap_or(0.0);
    let t: Vec<f64> = frames(&light)?
        .iter()
        .map(|f| ((f["t_ns"].as_f64().unwrap_or(t0) - t0) / 1e6 * 10.0).round() / 10.0)
        .collect();
    let mut modes = serde_json::Map::new();
    let file = format!("{name}-light.png");
    let (count, ticks, size) = strip(&light, &out.join(&file))?;
    modes.insert(
        "light".into(),
        json!({ "sprite": file, "count": count, "ticks": ticks }),
    );
    if let Some(dark) = dark {
        let dark = read_run(dark)?;
        if frames(&dark)?.len() != t.len() {
            return Err(format!(
                "example `{name}`: the dark run has {} ticks, the light {}",
                frames(&dark)?.len(),
                t.len()
            ));
        }
        let file = format!("{name}-dark.png");
        let (count, ticks, _) = strip(&dark, &out.join(&file))?;
        modes.insert(
            "dark".into(),
            json!({ "sprite": file, "count": count, "ticks": ticks }),
        );
    }
    let v = json!({
        "scenario": s.path,
        "source": s.hash,
        "hz": light.trace["scenario"]["hz"],
        "scale": light.trace["scenario"]["scale"],
        "size": size,
        "t": t,
        "marks": marks(&light),
        "tracks": tracks(&light)?,
        "modes": modes,
    });
    let path = out.join(format!("{name}.json"));
    let text = serde_json::to_string(&v).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}
