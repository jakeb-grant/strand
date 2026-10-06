//! Importers, `material()` and the wallpaper quantiser.

use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_scene::Color;
use strand_theme::contrast::{self, MIN_CONTRAST};
use strand_theme::image::{Lookup, Quantiser, seed_from_bytes};
use strand_theme::{ImportError, Options, Palette, Role, Variant, from_seed, import};

fn hex(s: &str) -> Color {
    Color::from_hex(s).unwrap()
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "strand-theme-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Every role is a real colour, the pairs are readable.
fn whole(p: &Palette) {
    for (r, c) in p.iter() {
        assert!(c.in_gamut(1e-4) && c.a == 1.0, "{r}: {c:?}");
    }
    assert!(
        contrast::worst(p) >= MIN_CONTRAST,
        "worst {}",
        contrast::worst(p)
    );
}

#[test]
fn catppuccin_flavours_fill_the_palette() {
    let mocha = import("catppuccin:mocha", None).unwrap();
    whole(&mocha);
    assert!(mocha.is_dark());
    assert_eq!(mocha.get(Role::Accent), hex("#cba6f7"));
    assert_eq!(mocha.get(Role::Surface), hex("#1e1e2e"));
    assert_eq!(mocha.get(Role::Fg), hex("#cdd6f4"));
    assert_eq!(mocha.get(Role::SurfaceLow), hex("#181825"));
    assert_eq!(mocha.get(Role::Error), hex("#f38ba8"));
    let blue = import("catppuccin:mocha:blue", None).unwrap();
    assert_eq!(blue.get(Role::Accent), hex("#89b4fa"));
    for f in ["macchiato", "frappe", "latte"] {
        let p = import(&format!("catppuccin:{f}"), None).unwrap();
        whole(&p);
        assert_eq!(p.is_dark(), f != "latte");
    }
    assert!(matches!(
        import("catppuccin:espresso", None),
        Err(ImportError::UnknownName { .. })
    ));
    assert!(matches!(
        import("solarized", None),
        Err(ImportError::UnknownSource(_))
    ));
}

const BASE16: &str = r##"
system: "base16"
name: "Tokyo Night"
variant: "dark"
palette:
  base00: "#1a1b26" # background
  base01: "#16161e"
  base02: "#2f3549"
  base03: "#444b6a"
  base04: "#787c99"
  base05: "#a9b1d6"
  base06: "#cbccd1"
  base07: "#d5d6db"
  base08: "#c0caf5"
  base09: "#a9b1d6"
  base0A: "#0db9d7"
  base0B: "#9ece6a"
  base0C: "#b4f9f8"
  base0D: "#2ac3de"
  base0E: "#bb9af7"
  base0F: "#f7768e"
"##;

#[test]
fn base16_and_base24_schemes_fill_the_palette() {
    let dir = temp("base16");
    std::fs::write(dir.join("tokyo.yaml"), BASE16).unwrap();
    let p = import("base16:tokyo.yaml", Some(&dir)).unwrap();
    whole(&p);
    assert!(p.is_dark());
    assert_eq!(p.get(Role::Surface), hex("#1a1b26"));
    assert_eq!(p.get(Role::Accent), hex("#2ac3de"));
    assert_eq!(p.get(Role::Tertiary), hex("#bb9af7"));
    assert_eq!(p.get(Role::Outline), hex("#444b6a"));
    // The legacy flat form, without `#`, light, plus base24's extra
    // backgrounds.
    let mut legacy = String::from("scheme: \"Light\"\n");
    for (i, c) in [
        "fafafa", "f0f0f0", "e0e0e0", "a0a0a0", "606060", "202020", "101010", "000000", "d00000",
        "e08000", "c0a000", "00a000", "008080", "0050d0", "8000c0", "804000", "f8f8f8", "ffffff",
        "ff0000", "ffff00", "00ff00", "00ffff", "4070ff", "ff00ff",
    ]
    .iter()
    .enumerate()
    {
        let key = if i < 16 {
            format!("base{i:02X}")
        } else {
            format!("base{:X}", 0x10 + i - 16)
        };
        legacy.push_str(&format!("{key}: \"{c}\"\n"));
    }
    std::fs::write(dir.join("light.yaml"), legacy).unwrap();
    let p = import("base24:light.yaml", Some(&dir)).unwrap();
    whole(&p);
    assert!(!p.is_dark());
    assert_eq!(p.get(Role::SurfaceLowest), hex("#ffffff"));
    assert_eq!(p.get(Role::SurfaceDim), hex("#f8f8f8"));
    assert!(matches!(
        import("base16:missing.yaml", Some(&dir)),
        Err(ImportError::Io { .. })
    ));
    std::fs::write(dir.join("bad.yaml"), "base05: \"#ffffff\"\n").unwrap();
    assert!(matches!(
        import("base16:bad.yaml", Some(&dir)),
        Err(ImportError::Parse { .. })
    ));
    let _ = std::fs::remove_dir_all(dir);
}

/// matugen's JSON fills roles by their Material 3 names; both layouts.
#[test]
fn matugen_output_fills_the_palette() {
    let dir = temp("matugen");
    let seed = from_seed(
        hex("#7aa2f7"),
        Options {
            dark: true,
            ..Options::default()
        },
    );
    // colors.<mode>.<role>, as `matugen --json hex` writes it.
    let mut dark = serde_json::Map::new();
    for (r, c) in seed.iter() {
        let [x, y, z, _] = c.to_rgba8();
        dark.insert(r.m3().to_string(), format!("#{x:02x}{y:02x}{z:02x}").into());
    }
    let doc = serde_json::json!({ "colors": { "dark": dark, "light": {} }, "mode": "dark" });
    std::fs::write(dir.join("colors.json"), doc.to_string()).unwrap();
    let p = import("matugen:colors.json", Some(&dir)).unwrap();
    whole(&p);
    assert_eq!(p, seed, "a full matugen scheme comes back role for role");
    // colors.<role>.<mode>, with `{ "color": … }` values; partial, so the
    // derivation table fills the rest.
    let doc = serde_json::json!({
        "colors": {
            "primary": { "light": { "color": "#3a5f8f" }, "dark": { "color": "#a4c9fe" } },
            "surface": { "light": { "color": "#f9f9ff" }, "dark": { "color": "#111318" } },
            "on_surface": { "light": { "color": "#191c20" }, "dark": { "color": "#e1e2e8" } }
        },
        "mode": "light"
    });
    std::fs::write(dir.join("v3.json"), doc.to_string()).unwrap();
    let p = import("matugen:v3.json", Some(&dir)).unwrap();
    whole(&p);
    assert!(!p.is_dark());
    assert_eq!(p.get(Role::Accent), hex("#3a5f8f"));
    assert_eq!(p.get(Role::Fg), hex("#191c20"));
    let _ = std::fs::remove_dir_all(dir);
}

/// W3C design-token JSON: groups, `$type` inheritance, aliases and the
/// 2025 colour object.
#[test]
fn w3c_design_tokens_fill_the_palette() {
    let dir = temp("w3c");
    let doc = serde_json::json!({
        "base": {
            "$type": "color",
            "blue": { "$value": "#0057b7" },
            "night": { "$value": { "colorSpace": "srgb", "components": [0.05, 0.06, 0.08], "alpha": 1 } }
        },
        "color": {
            "$type": "color",
            "accent": { "$value": "{base.blue}" },
            "surface": { "$value": "{base.night}" },
            "on-surface": { "$value": "#e8e8f0" },
            "md": { "sys": { "color": { "on-primary": { "$value": "#ffffff" } } } }
        },
        "size": { "$type": "dimension", "accent": { "$value": "4px" } }
    });
    std::fs::write(dir.join("tokens.json"), doc.to_string()).unwrap();
    let p = import("w3c:tokens.json", Some(&dir)).unwrap();
    whole(&p);
    assert!(p.is_dark());
    assert_eq!(p.get(Role::Accent), hex("#0057b7"));
    assert_eq!(p.get(Role::Fg), hex("#e8e8f0"));
    assert_eq!(p.get(Role::Surface).to_rgba8(), [13, 15, 20, 255]);
    std::fs::write(dir.join("none.json"), "{\"x\": {\"$value\": 1}}").unwrap();
    assert!(import("w3c:none.json", Some(&dir)).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

/// Component groups do not overwrite the system roles, whatever their
/// order in the file; out-of-gamut and translucent role values are
/// brought into a usable palette.
#[test]
fn w3c_component_tokens_do_not_claim_roles() {
    let dir = temp("w3c-components");
    let doc = serde_json::json!({
        "color": {
            "$type": "color",
            "background": { "$value": "#ffffff" },
            "surface": { "$value": "#ffffff" },
            "primary": { "$value": "#0057b7" },
            "on-surface": { "$value": { "colorSpace": "srgb", "components": [0.1, 0.1, 0.12], "alpha": 0.1 } },
            "error": { "$value": { "colorSpace": "srgb", "components": [1.6, -0.3, 0.2] } }
        },
        "component": {
            "$type": "color",
            "tooltip": {
                "background": { "$value": "#222222" },
                "surface": { "$value": "#333333" }
            },
            "button": { "primary": { "$value": "#ff0000" } }
        }
    });
    std::fs::write(dir.join("tokens.json"), doc.to_string()).unwrap();
    let p = import("w3c:tokens.json", Some(&dir)).unwrap();
    whole(&p);
    assert!(!p.is_dark());
    assert_eq!(p.get(Role::Bg), hex("#ffffff"));
    assert_eq!(p.get(Role::Surface), hex("#ffffff"));
    assert_eq!(p.get(Role::Accent), hex("#0057b7"));
    assert_eq!(p.get(Role::Fg).a, 1.0);
    let _ = std::fs::remove_dir_all(dir);
}

/// `material(seed:)` is Material 3's algorithm: material-color-utilities'
/// reference scheme for blue, every variant whole, the same seed always
/// the same palette.
#[test]
fn material_seed_is_exact_and_deterministic() {
    // material-color-utilities `scheme_tonal_spot_test`: seed 0xff0000ff,
    // light, standard contrast.
    let blue = from_seed(hex("#0000ff"), Options::default());
    assert_eq!(blue.get(Role::Accent), hex("#555992"));
    assert_eq!(blue.get(Role::Surface), hex("#fbf8ff"));
    for &variant in Variant::ALL {
        for dark in [false, true] {
            for contrast in [-1.0, 0.0, 1.0] {
                let o = Options {
                    variant,
                    dark,
                    contrast,
                };
                let p = from_seed(hex("#7aa2f7"), o);
                whole(&p);
                assert_eq!(p, from_seed(hex("#7aa2f7"), o));
            }
        }
    }
    // High contrast pulls text further from its surface.
    let o = Options {
        dark: true,
        ..Options::default()
    };
    let normal = from_seed(hex("#7aa2f7"), o);
    let high = from_seed(hex("#7aa2f7"), Options { contrast: 1.0, ..o });
    let r = |p: &Palette| contrast::ratio(p.get(Role::FgVariant), p.get(Role::Surface));
    assert!(r(&high) > r(&normal));
}

/// A PNG made of two colour fields, mostly `major`.
fn png(major: [u8; 3], minor: [u8; 3], w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, _| {
        image::Rgb(if x < w * 3 / 4 { major } else { minor })
    });
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn wait_ready(q: &mut Quantiser, path: &Path) -> Color {
    assert!(q.wait(Duration::from_secs(30)));
    match q.lookup(path) {
        Lookup::Ready(c) => c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn material_image_is_deterministic_and_cached_by_content() {
    let dir = temp("image");
    let cache = dir.join("cache");
    let wall = dir.join("wall.png");
    let bytes = png([30, 90, 200], [240, 200, 40], 640, 360);
    std::fs::write(&wall, &bytes).unwrap();
    // Straight from bytes: the same image, the same seed; the seed is the
    // dominant blue.
    let seed = seed_from_bytes(&bytes).unwrap();
    assert_eq!(seed, seed_from_bytes(&bytes).unwrap());
    let lch = seed.to_oklch();
    assert!((lch.h - 260.0).abs() < 25.0, "{lch:?}");
    assert!(seed_from_bytes(b"not an image").is_err());

    let mut q = Quantiser::new(Some(cache.clone())).unwrap();
    assert_eq!(q.lookup(&wall), Lookup::Pending { last: None });
    assert_eq!(wait_ready(&mut q, &wall), seed);
    assert_eq!(q.quantised(), 1);
    // A copy under another name has the same content: no decode.
    let copy = dir.join("copy.png");
    std::fs::copy(&wall, &copy).unwrap();
    assert!(matches!(q.lookup(&copy), Lookup::Pending { last: Some(_) }));
    assert_eq!(wait_ready(&mut q, &copy), seed);
    assert_eq!(q.quantised(), 1, "content hash hit");
    // A symlink to it: the link resolves to the same file.
    let link = dir.join("link.png");
    std::os::unix::fs::symlink(&wall, &link).unwrap();
    q.lookup(&link);
    assert_eq!(wait_ready(&mut q, &link), seed);
    assert_eq!(q.quantised(), 1);
    // Replacing the file re-quantises; the old seed holds meanwhile.
    let red = png([200, 40, 40], [20, 20, 20], 320, 200);
    std::fs::write(dir.join("next.png"), &red).unwrap();
    std::fs::rename(dir.join("next.png"), &wall).unwrap();
    assert!(matches!(q.lookup(&wall), Lookup::Pending { last: Some(c) } if c == seed));
    let new = wait_ready(&mut q, &wall);
    assert_ne!(new, seed);
    assert_eq!(q.quantised(), 2);
    // A swapped link follows its new target.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&copy, &link).unwrap();
    q.lookup(&link);
    assert_eq!(wait_ready(&mut q, &link), seed);
    drop(q);

    // A new run (a reboot): the unchanged wallpaper is answered at once
    // from the persisted index, nothing decoded.
    let mut q = Quantiser::new(Some(cache.clone())).unwrap();
    assert_eq!(q.lookup(&wall), Lookup::Ready(new));
    assert_eq!(q.quantised(), 0);
    assert!(q.last().is_some(), "the last seed survives a restart");
    // Missing files fail with the last seed to hold.
    assert!(matches!(
        q.lookup(&dir.join("gone.png")),
        Lookup::Failed { last: Some(_), .. }
    ));
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}

/// A wallpaper copied over in place keeping its size and modification
/// time (`cp -p`) is a new file (its change time moved); one replaced by
/// delete-then-create holds its seed through the gap; one that stays
/// missing fails once the grace is over.
#[test]
fn replaced_and_briefly_missing_wallpapers() {
    use strand_theme::image::{MISSING_GRACE, Stamp};
    let dir = temp("replace");
    let wall = dir.join("wall.png");
    let mut blue = png([30, 90, 200], [240, 200, 40], 64, 64);
    let mut red = png([200, 40, 40], [20, 20, 20], 64, 64);
    // Same size (decoders stop at IEND), so only the times tell.
    let n = blue.len().max(red.len());
    blue.resize(n, 0);
    red.resize(n, 0);
    std::fs::write(&wall, &blue).unwrap();
    let mut q = Quantiser::new(Some(dir.join("cache"))).unwrap();
    q.lookup(&wall);
    let first = wait_ready(&mut q, &wall);
    // Same size, same mtime, other content.
    let before = Stamp::of(&wall).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&wall, &red).unwrap();
    let f = std::fs::File::options().write(true).open(&wall).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + Duration::from_nanos(before.mtime_ns as u64))
        .unwrap();
    drop(f);
    let after = Stamp::of(&wall).unwrap();
    assert_eq!((after.len, after.mtime_ns), (before.len, before.mtime_ns));
    assert!(matches!(q.lookup(&wall), Lookup::Pending { .. }));
    let second = wait_ready(&mut q, &wall);
    assert_ne!(first, second);
    // Delete, look up in the gap (the watcher saw the delete), create.
    std::fs::remove_file(&wall).unwrap();
    q.invalidate(&wall);
    assert_eq!(q.lookup(&wall), Lookup::Pending { last: Some(second) });
    std::fs::write(&wall, &blue).unwrap();
    q.invalidate(&wall);
    q.lookup(&wall);
    assert_eq!(wait_ready(&mut q, &wall), first);
    // Gone for good: failed after the grace, the waker asked to look
    // again.
    let (tx, rx) = std::sync::mpsc::channel();
    q.set_waker(move || {
        let _ = tx.send(());
    });
    std::fs::remove_file(&wall).unwrap();
    let gone = std::time::Instant::now();
    assert!(matches!(q.lookup(&wall), Lookup::Pending { .. }));
    // (An earlier gap's timer may wake us first: look again until the
    // answer changes.)
    loop {
        // The grace running out wakes the owner to look again.
        rx.recv_timeout(MISSING_GRACE * 4).unwrap();
        q.poll();
        match q.lookup(&wall) {
            Lookup::Failed { .. } => break,
            Lookup::Pending { .. } => assert!(gone.elapsed() < MISSING_GRACE * 2),
            other => panic!("{other:?}"),
        }
    }
    assert!(gone.elapsed() >= MISSING_GRACE);
    // Never there: failed at once.
    assert!(matches!(
        q.lookup(&dir.join("nope.png")),
        Lookup::Failed { .. }
    ));
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}

/// A lookup set off by something else (a portal change) while the
/// wallpaper is being copied over in place reads a torn file: its seed
/// holds through the grace instead of failing, the finished write is
/// read, and a file that stays broken fails after one more read.
#[test]
fn a_torn_wallpaper_holds_its_seed() {
    use strand_theme::image::MISSING_GRACE;
    let dir = temp("torn");
    let wall = dir.join("wall.png");
    let blue = png([30, 90, 200], [240, 200, 40], 64, 64);
    let red = png([200, 40, 40], [20, 20, 20], 64, 64);
    std::fs::write(&wall, &blue).unwrap();
    let mut q = Quantiser::new(Some(dir.join("cache"))).unwrap();
    q.lookup(&wall);
    let first = wait_ready(&mut q, &wall);
    // Half of the new file written.
    std::fs::write(&wall, &red[..red.len() / 2]).unwrap();
    assert!(matches!(q.lookup(&wall), Lookup::Pending { .. }));
    assert!(q.wait(Duration::from_secs(30)));
    assert_eq!(q.lookup(&wall), Lookup::Pending { last: Some(first) });
    // The write finishes; the watcher says so.
    std::fs::write(&wall, &red).unwrap();
    q.invalidate(&wall);
    q.lookup(&wall);
    let second = wait_ready(&mut q, &wall);
    assert_ne!(first, second);
    // Broken for good: held once, then failed.
    std::fs::write(&wall, b"not an image").unwrap();
    q.lookup(&wall);
    assert!(q.wait(Duration::from_secs(30)));
    let broke = std::time::Instant::now();
    assert!(matches!(q.lookup(&wall), Lookup::Pending { .. }));
    std::thread::sleep(MISSING_GRACE + Duration::from_millis(50));
    q.poll();
    q.lookup(&wall);
    assert!(q.wait(Duration::from_secs(30)));
    assert!(matches!(q.lookup(&wall), Lookup::Failed { .. }));
    assert!(broke.elapsed() >= MISSING_GRACE);
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}

/// Two runs sharing one cache directory keep each other's wallpapers in
/// the index (and their seeds): a restart of either answers both at once.
#[test]
fn two_runs_share_the_wallpaper_cache() {
    let dir = temp("shared-cache");
    let cache = dir.join("cache");
    let (a_wall, b_wall) = (dir.join("a.png"), dir.join("b.png"));
    std::fs::write(&a_wall, png([30, 90, 200], [240, 200, 40], 32, 32)).unwrap();
    std::fs::write(&b_wall, png([200, 40, 40], [20, 20, 20], 32, 32)).unwrap();
    let mut a = Quantiser::new(Some(cache.clone())).unwrap();
    let mut b = Quantiser::new(Some(cache.clone())).unwrap();
    a.lookup(&a_wall);
    let a_seed = wait_ready(&mut a, &a_wall);
    b.lookup(&b_wall);
    let b_seed = wait_ready(&mut b, &b_wall);
    // `a` writes again after `b` (another wallpaper): `b`'s entry stays.
    let c_wall = dir.join("c.png");
    std::fs::write(&c_wall, png([40, 200, 40], [20, 20, 20], 32, 32)).unwrap();
    a.lookup(&c_wall);
    wait_ready(&mut a, &c_wall);
    drop((a, b));
    let seeds = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".seed"))
        .count();
    assert_eq!(seeds, 3);
    let mut q = Quantiser::new(Some(cache)).unwrap();
    assert_eq!(q.lookup(&a_wall), Lookup::Ready(a_seed));
    assert_eq!(q.lookup(&b_wall), Lookup::Ready(b_seed));
    assert!(matches!(q.lookup(&c_wall), Lookup::Ready(_)));
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}

/// A slideshow through 300 wallpapers leaves a bounded index, seed cache
/// and lookup table.
#[test]
fn many_wallpapers_leave_a_bounded_cache() {
    use strand_theme::image::MAX_REMEMBERED;
    let dir = temp("slideshow");
    let cache = dir.join("cache");
    let mut q = Quantiser::new(Some(cache.clone())).unwrap();
    for i in 0..300u32 {
        let p = dir.join(format!("w{i}.png"));
        std::fs::write(
            &p,
            png(
                [(i % 251) as u8, (i / 251) as u8 * 60, 90],
                [10, 10, 10],
                4,
                4,
            ),
        )
        .unwrap();
        q.lookup(&p);
        if i % 50 == 49 {
            assert!(q.wait(Duration::from_secs(60)));
        }
    }
    assert!(q.wait(Duration::from_secs(60)));
    assert!(q.remembered() <= MAX_REMEMBERED, "{}", q.remembered());
    let index = std::fs::read_to_string(cache.join("index")).unwrap();
    let entries = index.lines().filter(|l| l.starts_with("entry ")).count();
    assert!(entries <= MAX_REMEMBERED, "{entries}");
    let seeds = std::fs::read_dir(&cache)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".seed"))
        .count();
    assert!(seeds <= MAX_REMEMBERED, "{seeds}");
    // The most recent ones are the ones kept: a reboot answers the last
    // wallpaper at once.
    drop(q);
    let mut q = Quantiser::new(Some(cache)).unwrap();
    assert!(matches!(q.lookup(&dir.join("w299.png")), Lookup::Ready(_)));
    drop(q);
    let _ = std::fs::remove_dir_all(dir);
}

/// An imported file must be a regular file of at most a megabyte: a FIFO
/// (which would block the logic thread), a device and an oversized file
/// are refused with an `Io` error, without reading them.
#[test]
fn imports_refuse_fifos_devices_and_huge_files() {
    let dir = temp("import-kinds");
    let fifo = dir.join("pipe.yaml");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|s| s.success());
    if made {
        let started = std::time::Instant::now();
        let r = import(&format!("base16:{}", fifo.display()), None);
        assert!(matches!(r, Err(ImportError::Io { .. })), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    let r = import("base16:/dev/zero", None);
    assert!(matches!(r, Err(ImportError::Io { .. })), "{r:?}");
    let huge = dir.join("huge.json");
    std::fs::write(&huge, vec![b' '; 2 * 1024 * 1024]).unwrap();
    let r = import(&format!("w3c:{}", huge.display()), None);
    assert!(matches!(r, Err(ImportError::Io { .. })), "{r:?}");
    let _ = std::fs::remove_dir_all(dir);
}
