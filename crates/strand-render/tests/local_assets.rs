//! (M6, the owner's approval of 2026-10-14) A local file an `image`,
//! `svg` or `lottie` draws, saved while the shell runs, is drawn anew
//! once `strand run`'s watcher reports it (`Role::Asset`,
//! [`Renderer::assets_changed`]): no reload, nothing else changed, and
//! with a worker the old pixels stay until the new ones are in.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

const S: SurfaceId = SurfaceId(1);
const T0: Duration = Duration::from_secs(1);

/// A 24 px square of one colour.
fn square(fill: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\">\
         <rect width=\"24\" height=\"24\" fill=\"{fill}\"/></svg>"
    )
}

/// A still 24 × 24 Lottie of one colour (`[r, g, b]` in 0..1).
fn lottie(rgb: [u8; 3]) -> String {
    let [r, g, b] = rgb;
    format!(
        r#"{{"v":"5.7.0","fr":30,"ip":0,"op":60,"w":24,"h":24,"layers":[
 {{"ty":4,"ind":1,"ip":0,"op":60,"st":0,
  "ks":{{"o":{{"a":0,"k":100}},"r":{{"a":0,"k":0}},"p":{{"a":0,"k":[12,12,0]}},
        "a":{{"a":0,"k":[0,0,0]}},"s":{{"a":0,"k":[100,100,100]}}}},
  "shapes":[{{"ty":"gr","it":[
    {{"ty":"rc","p":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[24,24]}},"r":{{"a":0,"k":0}}}},
    {{"ty":"fl","c":{{"a":0,"k":[{r},{g},{b},1]}},"o":{{"a":0,"k":100}}}},
    {{"ty":"tr","p":{{"a":0,"k":[0,0]}},"a":{{"a":0,"k":[0,0]}},"s":{{"a":0,"k":[100,100]}},"r":{{"a":0,"k":0}},"o":{{"a":0,"k":100}}}}]}}]}}]}}"#
    )
}

fn png(path: &Path, rgba: [u8; 4]) {
    write_png(path, 4, 4, &rgba.repeat(16));
}

/// Saves `text` at `path` as an editor does: a temporary file renamed
/// over it.
fn save(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

struct Files {
    image: PathBuf,
    svg: PathBuf,
    lottie: PathBuf,
}

fn files(name: &str) -> Files {
    let dir = std::env::temp_dir().join(format!("strand-assets-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let f = Files {
        image: dir.join("a.png"),
        svg: dir.join("b.svg"),
        lottie: dir.join("c.json"),
    };
    png(&f.image, [0xff, 0, 0, 0xff]);
    std::fs::write(&f.svg, square("#ff0000")).unwrap();
    std::fs::write(&f.lottie, lottie([1, 0, 0])).unwrap();
    f
}

/// An `image`, an `svg` and a `lottie` of `f`, 24 px each, in a row.
fn scene(r: &mut Renderer, f: &Files) -> Buffer {
    let mut b = Builder::default();
    let root = b.node(
        NodeKind::Bar,
        None,
        vec![
            (Prop::Width, num(72.0)),
            (Prop::Height, num(24.0)),
            (Prop::Bg, color("#000000")),
        ],
    );
    let row = b.node(NodeKind::Row, Some(root), Vec::new());
    for (kind, path) in [
        (NodeKind::Image, &f.image),
        (NodeKind::Svg, &f.svg),
        (NodeKind::Lottie, &f.lottie),
    ] {
        b.node(
            kind,
            Some(row),
            vec![
                (Prop::Source, text(path.to_str().unwrap())),
                (Prop::Size, num(24.0)),
                // Still: no clock asks for frames of its own.
                (Prop::Speed, num(0.0)),
            ],
        );
    }
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(S, r.tree().roots()[0]);
    let mut buf = Buffer::new(72, 24, Scale::ONE);
    buf.paint_at(r, S, 0, T0);
    buf
}

/// Straight RGB at the centre of the `i`th node.
fn at(buf: &Buffer, i: u32) -> (u8, u8, u8) {
    let [b, g, r, _] = buf.px(i * 24 + 12, 12);
    (r, g, b)
}

const RED: (u8, u8, u8) = (0xff, 0, 0);
const GREEN: (u8, u8, u8) = (0, 0xff, 0);

fn edit(f: &Files) {
    let tmp = f.image.with_extension("tmp.png");
    png(&tmp, [0, 0xff, 0, 0xff]);
    std::fs::rename(&tmp, &f.image).unwrap();
    save(&f.svg, &square("#00ff00"));
    save(&f.lottie, &lottie([0, 1, 0]));
}

/// Inline (offline): an edit reported is drawn at the next frame, as
/// damage at buffer age 1; a path nothing draws repaints nothing.
#[test]
fn an_edited_local_file_is_drawn_without_a_reload() {
    let f = files("inline");
    let mut r = renderer();
    let mut buf = scene(&mut r, &f);
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [RED; 3]);
    edit(&f);
    // Not reported yet: the decodes held are drawn.
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [RED; 3]);
    {
        use strand_scene::Painter;
        r.assets_changed(&[PathBuf::from("/nonexistent/other.svg")]);
        assert!(!r.wants_frame(S), "a file nothing draws repaints nothing");
        // What the watcher's `Role::Asset` for the three does.
        r.assets_changed(&[f.image.clone(), f.svg.clone(), f.lottie.clone()]);
        assert!(r.wants_frame(S), "the surface showing them repaints");
    }
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(33));
    assert_eq!(
        [at(&buf, 0), at(&buf, 1), at(&buf, 2)],
        [GREEN; 3],
        "image, svg and lottie read again"
    );
    // One file alone: the others are not read again.
    save(&f.svg, &square("#0000ff"));
    r.assets_changed(std::slice::from_ref(&f.svg));
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(50));
    assert_eq!(
        [at(&buf, 0), at(&buf, 1), at(&buf, 2)],
        [GREEN, (0, 0, 0xff), GREEN]
    );
}

/// (m6-owner-fixes review) A source written with a `.` component
/// (`<dir>/./b.svg`) is the file the watcher reports without it (`Path`
/// equality drops a `.` past the first component): its change is drawn.
#[test]
fn a_source_with_a_dot_component_follows_its_file() {
    use strand_scene::Painter;
    let mut f = files("dot");
    let dir = f.svg.parent().unwrap().to_path_buf();
    let reported = [f.image.clone(), f.svg.clone(), f.lottie.clone()];
    for p in [&mut f.image, &mut f.svg, &mut f.lottie] {
        *p = dir.join(".").join(p.file_name().unwrap());
    }
    let mut r = renderer();
    let mut buf = scene(&mut r, &f);
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [RED; 3]);
    edit(&f);
    r.assets_changed(&reported);
    assert!(r.wants_frame(S), "the surface showing them repaints");
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [GREEN; 3]);
}

/// With an image worker the files are read off the render thread: the
/// frame right after the report still draws the old pixels (no blank
/// frame), and the new ones arrive and repaint.
#[test]
fn with_a_worker_the_old_pixels_stay_until_the_new_are_in() {
    use strand_scene::Painter;
    use strand_text::{FontConfig, TextWorker, test_font_path};
    let f = files("worker");
    let data = std::fs::read(test_font_path()).unwrap();
    let worker =
        TextWorker::spawn_with_waker(FontConfig::isolated(vec![std::sync::Arc::new(data)]), None)
            .unwrap();
    let mut r = Renderer::new(strand_render::TextBackend::Worker(worker));
    let mut buf = scene(&mut r, &f);
    let mut t = T0;
    let mut settle = |r: &mut Renderer, buf: &mut Buffer, until: &dyn Fn(&Buffer) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !until(buf) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            r.update();
            if r.wants_frame(S) {
                t += Duration::from_millis(16);
                buf.paint_at(r, S, 1, t);
            }
        }
    };
    let all = |c: (u8, u8, u8)| move |b: &Buffer| [at(b, 0), at(b, 1), at(b, 2)] == [c; 3];
    settle(&mut r, &mut buf, &all(RED));
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [RED; 3]);
    edit(&f);
    r.assets_changed(&[f.image.clone(), f.svg.clone(), f.lottie.clone()]);
    assert!(r.wants_frame(S));
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_secs(2));
    for i in 0..3 {
        assert_ne!(at(&buf, i), (0, 0, 0), "node {i}: no blank frame");
    }
    settle(&mut r, &mut buf, &all(GREEN));
    assert_eq!([at(&buf, 0), at(&buf, 1), at(&buf, 2)], [GREEN; 3]);
}

/// (report) The files a glitch report copies: each local file an
/// `image`, `svg` or `lottie` on screen draws, once each, sorted; none
/// before anything is drawn.
#[test]
fn image_sources_name_the_files_drawn() {
    let f = files("sources");
    let mut r = renderer();
    assert!(r.image_sources().files().is_empty());
    let mut buf = scene(&mut r, &f);
    let mut want = vec![f.image.clone(), f.svg.clone(), f.lottie.clone()];
    want.sort();
    assert_eq!(r.image_sources().files(), want);
    buf.paint_at(&mut r, S, 1, T0 + Duration::from_millis(16));
    assert_eq!(r.image_sources().files(), want, "each once");
}
