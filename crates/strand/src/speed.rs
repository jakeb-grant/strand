//! Speed measurements in the shipped binary's link-time optimisation
//! (decisions.md, m5-speed-profile): built only with the `speed-bench`
//! feature, never in a workspace test run.
//!
//! Fat LTO optimises the whole program once more in the session of the
//! crate that links it, at that crate's opt-level. The release binary
//! links in `strand`'s session, so its level (not each library's own)
//! sets the link-time inliner for render, text, layout and every
//! dependency. A library's own integration tests link in that library's
//! session (strand-render's at 3) and so cannot see it. This module
//! links the render benches into `strand`'s library test, which is
//! linked in `strand`'s session, the shipped binary's:
//!
//! ```sh
//! scripts/container/run.sh cargo test --release -p strand \
//!     --no-default-features --features speed-bench --lib speed:: \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The existing render benches are included as they are (their gates
//! still apply), beside measurements of paths none covered: a Lottie
//! frame and first show, image decodes at drawn size, the wallpaper's
//! seed, a `material(seed:)` solve and launcher search per keystroke.
//! Each prints a `speed <name>: <value> us` line for A/B runs.

#[allow(warnings, clippy::all, clippy::pedantic)]
#[path = "../../strand-render/tests/list_scroll_bench.rs"]
mod list_scroll_bench;

#[allow(warnings, clippy::all, clippy::pedantic)]
#[path = "../../strand-render/tests/theme_swap_bench.rs"]
mod theme_swap_bench;

#[allow(warnings, clippy::all, clippy::pedantic)]
#[path = "../../strand-render/tests/motion.rs"]
mod motion;

#[allow(warnings, clippy::all, clippy::pedantic)]
#[path = "../../strand-render/tests/svg.rs"]
mod svg;

#[allow(warnings, clippy::all, clippy::pedantic)]
#[path = "../../strand-render/tests/common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Buffer, Builder, color, num, renderer, text};
use strand_scene::{NodeKind, Prop, Scale, SurfaceId};

const S: SurfaceId = SurfaceId(1);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("strand-speed-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Median and 95th percentile of `samples`, in microseconds.
fn report(name: &str, mut samples: Vec<Duration>) -> f64 {
    samples.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let median = us(samples[samples.len() / 2]);
    let p95 = us(samples[(samples.len() * 95 / 100).min(samples.len() - 1)]);
    println!(
        "speed {name}: {median:.1} us (p95 {p95:.1} us, {} samples)",
        samples.len()
    );
    median
}

/// A deterministic generator (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A photo-like RGB image: smooth gradients, a few soft blobs and fine
/// grain, so a JPEG of it carries AC coefficients as a photo does.
fn photo(w: u32, h: u32) -> Vec<u8> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let blobs: Vec<(f64, f64, f64, [f64; 3])> = (0..24)
        .map(|_| {
            (
                rng.f() * f64::from(w),
                rng.f() * f64::from(h),
                (0.05 + rng.f() * 0.25) * f64::from(w),
                [rng.f() * 255.0, rng.f() * 255.0, rng.f() * 255.0],
            )
        })
        .collect();
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (f64::from(x), f64::from(y));
            let mut c = [
                40.0 + 120.0 * fx / f64::from(w),
                60.0 + 80.0 * fy / f64::from(h),
                120.0,
            ];
            for (bx, by, r, col) in &blobs {
                let d2 = ((fx - bx).powi(2) + (fy - by).powi(2)) / (r * r);
                let k = (-d2).exp();
                for i in 0..3 {
                    c[i] = c[i] * (1.0 - k) + col[i] * k;
                }
            }
            let grain = (rng.f() - 0.5) * 24.0;
            for v in c {
                out.push((v + grain).clamp(0.0, 255.0) as u8);
            }
        }
    }
    out
}

fn rgba(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect()
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    common::write_png(path, w, h, rgba);
}

fn write_jpeg(path: &Path, w: u32, h: u32, rgb: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 88);
    enc.encode(rgb, w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
}

fn write_webp(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    image::codecs::webp::WebPEncoder::new_lossless(file)
        .encode(rgba, w, h, image::ExtendedColorType::Rgba8)
        .unwrap();
}

/// An animated GIF of `frames` frames, a blob moving over noise.
fn write_gif(path: &Path, w: u16, h: u16, frames: u16) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = gif::Encoder::new(file, w, h, &[]).unwrap();
    enc.set_repeat(gif::Repeat::Infinite).unwrap();
    let base = photo(u32::from(w), u32::from(h));
    for f in 0..frames {
        let mut px = base.clone();
        let cx = f32::from(f) / f32::from(frames) * f32::from(w);
        for y in 0..h {
            for x in 0..w {
                let d = (f32::from(x) - cx).powi(2) + (f32::from(y) - f32::from(h) / 2.0).powi(2);
                if d < 400.0 {
                    let i = (usize::from(y) * usize::from(w) + usize::from(x)) * 3;
                    px[i..i + 3].copy_from_slice(&[250, 80, 40]);
                }
            }
        }
        let mut frame = gif::Frame::from_rgb_speed(w, h, &px, 10);
        frame.delay = 4;
        enc.write_frame(&frame).unwrap();
    }
}

/// Image decodes at drawn size (P27 in the speed survey): what a
/// launcher's icons, album art, an `image` node and an animated GIF's
/// frames cost on the decode worker, and the wallpaper's seed (decode
/// and quantise) for `material(image:)`.
#[test]
#[ignore = "measurement, not a gate"]
fn decode_cost() {
    use strand_render::image::{Fit, IconTheme, ImageKey, load};
    let dir = scratch("decode");
    let icon = photo(256, 256);
    let icon_png = dir.join("icon.png");
    write_png(&icon_png, 256, 256, &rgba(&icon));
    let art = photo(1200, 1200);
    let art_jpg = dir.join("art.jpg");
    write_jpeg(&art_jpg, 1200, 1200, &art);
    let art_png = dir.join("art.png");
    write_png(&art_png, 1200, 1200, &rgba(&art));
    let art_webp = dir.join("art.webp");
    write_webp(&art_webp, 1200, 1200, &rgba(&art));
    let anim = dir.join("anim.gif");
    write_gif(&anim, 320, 240, 24);
    let wall = photo(1920, 1080);
    let wall_jpg = dir.join("wall.jpg");
    write_jpeg(&wall_jpg, 1920, 1080, &wall);
    let theme = IconTheme::named("hicolor");
    let key = |p: &Path, w: u32, frame: u32| ImageKey {
        source: p.to_str().unwrap().to_string(),
        icon: false,
        w,
        h: w,
        fit: Fit::default(),
        scale: 1,
        frame,
    };
    let rounds: usize = std::env::var("SPEED_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    let time = |k: &ImageKey, n: usize| -> Vec<Duration> {
        (0..n)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(load(k, &theme).unwrap());
                t.elapsed()
            })
            .collect()
    };
    report(
        "decode png icon 256->48",
        time(&key(&icon_png, 48, 0), rounds * 8),
    );
    report("decode png 1200->300", time(&key(&art_png, 300, 0), rounds));
    report(
        "decode jpeg 1200->300",
        time(&key(&art_jpg, 300, 0), rounds),
    );
    report(
        "decode jpeg 1200->1200",
        time(&key(&art_jpg, 1200, 0), rounds),
    );
    report(
        "decode webp-lossless 1200->300",
        time(&key(&art_webp, 300, 0), rounds),
    );
    // An animated GIF's 24 frames streamed through one player, as the
    // worker decodes them in turn.
    let gif_frames: Vec<Duration> = (0..rounds)
        .map(|_| {
            let mut players = strand_render::image::Players::default();
            let t = Instant::now();
            for f in 0..24 {
                std::hint::black_box(
                    strand_render::image::load_with(&key(&anim, 240, f), &theme, &mut players)
                        .unwrap(),
                );
            }
            t.elapsed() / 24
        })
        .collect();
    report("decode gif frame 320x240", gif_frames);
    let bytes = std::fs::read(&wall_jpg).unwrap();
    let seeds: Vec<Duration> = (0..rounds)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(strand_theme::image::seed_from_bytes(&bytes).unwrap());
            t.elapsed()
        })
        .collect();
    report("wallpaper seed jpeg 1920x1080", seeds);
}

/// A `material(seed:)` solve (P17): the palette a swap to a seed not
/// shown before pays for, both schemes.
#[test]
#[ignore = "measurement, not a gate"]
fn palette_cost() {
    use strand_theme::material::{Options, Variant, forget_kept_palettes, from_seed};
    let mut rng = Rng(7);
    let samples: Vec<Duration> = (0..200)
        .map(|i| {
            forget_kept_palettes();
            let seed = strand_scene::Color::from_rgba8(
                (rng.next() & 255) as u8,
                (rng.next() & 255) as u8,
                (rng.next() & 255) as u8,
                255,
            );
            let opts = Options {
                variant: Variant::default(),
                dark: i % 2 == 0,
                contrast: 0.0,
            };
            let t = Instant::now();
            std::hint::black_box(from_seed(seed, opts));
            t.elapsed()
        })
        .collect();
    report("material(seed:) solve", samples);
}

/// Launcher search per keystroke (P7): `apps.search` over 1,000
/// entries for each prefix of a typed query, as the launcher runs it.
#[test]
#[ignore = "measurement, not a gate"]
fn search_cost() {
    use strand_services::apps::{App, Entry, Frecency, search};
    let syllables = [
        "fi", "re", "fox", "term", "in", "al", "code", "nau", "ti", "lus", "gim", "p", "set",
        "tings", "mu", "sic", "vi", "de", "o", "pho", "to", "con", "so", "le", "mail", "chat",
        "edit", "or", "calc", "map", "draw", "net", "work",
    ];
    let mut rng = Rng(42);
    let mut word = |n: usize| -> String {
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(syllables[(rng.next() % syllables.len() as u64) as usize]);
        }
        s
    };
    let mut entries: Vec<Entry> = (0..1000)
        .map(|i| {
            let name = format!("{} {}", capital(&word(2)), word(3));
            Entry {
                app: App {
                    id: format!("org.example.App{i}"),
                    name,
                    comment: Some(word(6)),
                    icon: "application-x-executable".into(),
                    categories: vec!["Utility".into()],
                },
                exec: "true".into(),
                terminal: false,
                path: None,
                file: PathBuf::from("/dev/null"),
                icon_key: None,
                also: vec![word(3), word(2), word(4)],
            }
        })
        .collect();
    entries.sort_by(|a, b| a.app.name.cmp(&b.app.name));
    let frecency = Frecency::default();
    let mut per_key = Vec::new();
    for _ in 0..40 {
        for query in [
            "c", "co", "con", "cons", "conso", "consol", "t", "te", "ter", "term",
        ] {
            let t = Instant::now();
            std::hint::black_box(search(&entries, &frecency, query, 0));
            per_key.push(t.elapsed());
        }
    }
    report("launcher search keystroke (1000 entries)", per_key);
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// A Lottie of `layers` shape layers, each a closed path whose eight
/// vertices are keyframed, filled and stroked, rotating: about what a
/// loader or weather animation holds. 240 × 240 at 60 fps, 120 frames.
fn lottie(layers: usize) -> String {
    let mut rng = Rng(99);
    let mut shape = |r: f64| -> String {
        let mut v = Vec::new();
        let mut io = Vec::new();
        for k in 0..8 {
            let a = k as f64 / 8.0 * std::f64::consts::TAU;
            let rr = r * (0.6 + 0.4 * rng.f());
            v.push(format!("[{:.2},{:.2}]", rr * a.cos(), rr * a.sin()));
            io.push(format!(
                "[{:.2},{:.2}]",
                -rr * 0.3 * a.sin(),
                rr * 0.3 * a.cos()
            ));
        }
        let neg: Vec<String> = io
            .iter()
            .map(|p| {
                let t = p.trim_matches(|c| c == '[' || c == ']');
                let (x, y) = t.split_once(',').unwrap();
                let x: f64 = x.parse().unwrap();
                let y: f64 = y.parse().unwrap();
                format!("[{:.2},{:.2}]", -x, -y)
            })
            .collect();
        format!(
            r#"{{"i":[{}],"o":[{}],"v":[{}],"c":true}}"#,
            neg.join(","),
            io.join(","),
            v.join(",")
        )
    };
    let ease = r#""i":{"x":[0.5],"y":[0.5]},"o":{"x":[0.5],"y":[0.5]}"#;
    let mut out = Vec::new();
    for l in 0..layers {
        let (a, b, c) = (shape(60.0), shape(60.0), shape(60.0));
        let (x, y) = (
            60.0 + (l % 4) as f64 * 40.0,
            60.0 + (l / 4 % 4) as f64 * 40.0,
        );
        let col = [(l * 37 % 255) as f64 / 255.0, 0.4, 0.8];
        out.push(format!(
            r#"{{"ty":4,"ind":{ind},"ip":0,"op":120,"st":0,
 "ks":{{"o":{{"a":0,"k":80}},"r":{{"a":1,"k":[{{"t":0,"s":[0],{ease}}},{{"t":120,"s":[360]}}]}},
       "p":{{"a":0,"k":[{x},{y},0]}},"a":{{"a":0,"k":[0,0,0]}},"s":{{"a":0,"k":[60,60,100]}}}},
 "shapes":[{{"ty":"gr","it":[
   {{"ty":"sh","ks":{{"a":1,"k":[{{"t":0,"s":[{a}],{ease}}},{{"t":60,"s":[{b}],{ease}}},{{"t":120,"s":[{c}]}}]}}}},
   {{"ty":"fl","c":{{"a":0,"k":[{r},{g},{bb},1]}},"o":{{"a":0,"k":100}}}},
   {{"ty":"st","c":{{"a":0,"k":[0.1,0.1,0.1,1]}},"o":{{"a":0,"k":100}},"w":{{"a":0,"k":3}}}},
   {{"ty":"tr","p":{{"a":0,"k":[0,0]}},"a":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[100,100]}},"r":{{"a":0,"k":0}},"o":{{"a":0,"k":100}}}}]}}]}}"#,
            ind = l + 1,
            r = col[0],
            g = col[1],
            bb = col[2],
        ));
    }
    format!(
        r#"{{"v":"5.7.0","fr":60,"ip":0,"op":120,"w":240,"h":240,"layers":[{}]}}"#,
        out.join(",")
    )
}

/// A Lottie's frames and its first show (P26, P13): each 60 Hz frame
/// builds velato's scene and rasterises it; the first paint also reads
/// and parses the file.
#[test]
#[ignore = "measurement, not a gate"]
fn lottie_cost() {
    let dir = scratch("lottie");
    let path = dir.join("anim.json");
    std::fs::write(&path, lottie(32)).unwrap();
    let big = dir.join("big.json");
    std::fs::write(&big, lottie(400)).unwrap();
    let scene = |file: &Path| {
        let mut b = Builder::default();
        let root = b.node(
            NodeKind::Bar,
            None,
            vec![
                (Prop::Width, num(240.0)),
                (Prop::Height, num(240.0)),
                (Prop::Bg, color("#1e1e2e")),
            ],
        );
        b.node(
            NodeKind::Lottie,
            Some(root),
            vec![
                (Prop::Source, text(file.to_str().unwrap())),
                (Prop::Size, num(240.0)),
                (Prop::Speed, num(1.0)),
            ],
        );
        let mut r = renderer();
        assert!(r.apply(b.diff).is_empty());
        r.attach_surface(S, r.tree().roots()[0]);
        (r, Buffer::new(240, 240, Scale::ONE))
    };
    let t0 = Duration::from_secs(1);
    let firsts: Vec<Duration> = (0..10)
        .map(|_| {
            let (mut r, mut buf) = scene(&big);
            let t = Instant::now();
            buf.paint_at(&mut r, S, 0, t0);
            t.elapsed()
        })
        .collect();
    println!(
        "lottie first show: {} kB of JSON",
        std::fs::metadata(&big).unwrap().len() / 1000
    );
    report("lottie first show (400 layers)", firsts);
    let (mut r, mut buf) = scene(&path);
    let mut at = t0;
    buf.paint_at(&mut r, S, 0, at);
    let mut frames = Vec::new();
    for i in 0..1200 {
        at += Duration::from_micros(16_667);
        let t = Instant::now();
        let damage = buf.paint_at(&mut r, S, 1, at);
        let dt = t.elapsed();
        if i >= 60 {
            assert!(!damage.is_empty(), "frame {i} drew nothing");
            frames.push(dt);
        }
    }
    report("lottie frame (32 layers, 240x240)", frames);
}

/// The system fonts' first lookup and a fallback (P18, P20): what the
/// text worker pays at startup and after the installed fonts change
/// (`reload_fonts`), fontique scanning fontconfig's directories and
/// parsing its XML, then shaping a line that needs Greek and Cyrillic.
#[test]
#[ignore = "measurement, not a gate"]
fn fonts_cost() {
    use strand_text::{FontConfig, TextAlign, TextEngine, TextKey, TextRequest, TextStyle};
    let req = TextRequest {
        key: TextKey(1),
        text: "Hello 12:00 — Ελληνικά, Кириллица".into(),
        style: TextStyle {
            font: strand_scene::Font {
                family: "sans-serif".into(),
                size: 13.0,
                weight: 400,
            },
            line_height: None,
            align: TextAlign::default(),
            ellipsis: None,
            max_lines: None,
            spans: vec![],
        },
        max_width: None,
        scale: Scale::ONE,
    };
    let mut first = Vec::new();
    let mut reload = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let mut e = TextEngine::new(FontConfig::default());
        let l = e.layout(&req);
        first.push(t.elapsed());
        assert!(!l.runs.is_empty());
        let t = Instant::now();
        e.reload_fonts();
        std::hint::black_box(e.layout(&req));
        reload.push(t.elapsed());
    }
    report("system fonts first lookup + fallback", first);
    report("reload_fonts + fallback", reload);
}
