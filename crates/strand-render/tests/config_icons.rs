//! (M6, the owner's ruling of 2026-10-14) The config's `icons/` folder:
//! an icon name is looked up there before the icon theme
//! (`strand_icons::set_config_icons`), a `-symbolic` file there is
//! recoloured by `color` and any other drawn as authored, a name the
//! folder lacks falls back to the theme, and a file added or removed
//! while running is drawn once the icons are reported changed (the
//! watcher's `CacheKind::Icons`, `Renderer::icons_changed`).
//!
//! A binary of its own: the folder and the base directories are
//! process-wide, set once here before the first lookup.

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::*;
use strand_render::Renderer;
use strand_scene::*;

/// A 24 px square of one colour.
fn square(fill: &str) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\">\
         <rect width=\"24\" height=\"24\" fill=\"{fill}\"/></svg>"
    )
}

const GREEN: &str = "#00ff00";
const RED: &str = "#ff0000";
const BLACK: &str = "#000000";

/// The theme (`CfgTest`, every name green) and the config folder.
struct Dirs {
    theme: PathBuf,
    config: PathBuf,
}

fn dirs() -> &'static Dirs {
    static DIRS: OnceLock<Dirs> = OnceLock::new();
    DIRS.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("strand-cfg-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let theme = root.join("icons/CfgTest");
        std::fs::create_dir_all(theme.join("scalable/apps")).unwrap();
        std::fs::write(
            theme.join("index.theme"),
            "[Icon Theme]\nName=CfgTest\nDirectories=scalable/apps\n\n\
             [scalable/apps]\nSize=24\nMinSize=8\nMaxSize=512\nType=Scalable\n",
        )
        .unwrap();
        for name in [
            "strand-cfg-wins",
            "strand-cfg-sym",
            "strand-cfg-theme-only",
            "strand-cfg-later",
        ] {
            std::fs::write(
                theme.join(format!("scalable/apps/{name}.svg")),
                square(GREEN),
            )
            .unwrap();
        }
        let config = root.join("config/icons");
        std::fs::create_dir_all(&config).unwrap();
        // `strand-cfg-wins`: the folder's red square over the theme's.
        std::fs::write(config.join("strand-cfg-wins.svg"), square(RED)).unwrap();
        // `strand-cfg-sym`: the theme has the plain name, the folder its
        // `-symbolic` variant (a black square, recoloured).
        std::fs::write(config.join("strand-cfg-sym-symbolic.svg"), square(BLACK)).unwrap();
        // A plain file is drawn as authored, `color` or not; a PNG too.
        std::fs::write(config.join("strand-cfg-plain.svg"), square(RED)).unwrap();
        write_png(
            &config.join("strand-cfg-raster.png"),
            4,
            4,
            &[0xff, 0, 0, 0xff].repeat(16),
        );
        strand_icons::set_base_dirs(Some(vec![root.join("icons")]));
        strand_icons::set_config_icons(Some(config.clone()));
        Dirs { theme, config }
    })
}

/// A 24 px `icon` per name in a row on black, each with `color: #0000ff`
/// (set up at age 0; later ages paint the same scene again).
fn paint(r: &mut Renderer, names: &[&str], age: u8, buf: &mut Buffer) {
    if age > 0 {
        buf.paint(r, SurfaceId(1), age);
        return;
    }
    let mut b = Builder::default();
    let w = 24.0 * names.len() as f32;
    let root = b.node(
        NodeKind::Panel,
        None,
        vec![
            (Prop::Width, num(w)),
            (Prop::Height, num(24.0)),
            (Prop::Bg, color(BLACK)),
        ],
    );
    let row = b.node(NodeKind::Row, Some(root), Vec::new());
    for n in names {
        b.node(
            NodeKind::Icon,
            Some(row),
            vec![
                (Prop::Source, text(n)),
                (Prop::Size, num(24.0)),
                (Prop::Color, color("#0000ff")),
            ],
        );
    }
    assert!(r.apply(b.diff).is_empty());
    r.attach_surface(SurfaceId(1), r.tree().roots()[0]);
    r.configure_surface(SurfaceId(1), Size::new(w as u32, 24), Scale::ONE);
    buf.paint(r, SurfaceId(1), 0);
}

fn renderer() -> Renderer {
    dirs();
    let mut r = common::renderer();
    r.set_icon_theme_inline("CfgTest");
    r
}

/// Straight RGB at the centre of the `i`th icon.
fn at(buf: &Buffer, i: u32) -> (u8, u8, u8) {
    let [b, g, r, _] = buf.px(i * 24 + 12, 12);
    (r, g, b)
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
    common::write_png(path, w, h, rgba);
}

#[test]
fn a_config_icon_wins_over_the_theme_and_a_missing_one_falls_back() {
    let mut r = renderer();
    let mut buf = Buffer::new(24 * 3, 24, Scale::ONE);
    paint(
        &mut r,
        &["strand-cfg-wins", "strand-cfg-sym", "strand-cfg-theme-only"],
        0,
        &mut buf,
    );
    assert_eq!(
        at(&buf, 0),
        (0xff, 0, 0),
        "the folder's file, not the theme's"
    );
    assert_eq!(
        at(&buf, 1),
        (0, 0, 0xff),
        "the folder's `-symbolic` variant before the theme's plain name, in `color`"
    );
    assert_eq!(at(&buf, 2), (0, 0xff, 0), "not in the folder: the theme's");
    // The `apps` service's question, through the same chain.
    assert!(strand_icons::exists("strand-cfg-plain"));
    assert_eq!(
        strand_icons::resolve("strand-cfg-sym", 24, 1, Some("CfgTest")),
        Some(dirs().config.join("strand-cfg-sym-symbolic.svg"))
    );
}

#[test]
fn a_symbolic_config_icon_is_recoloured_and_a_plain_one_is_not() {
    let mut r = renderer();
    let mut buf = Buffer::new(24 * 3, 24, Scale::ONE);
    paint(
        &mut r,
        &[
            "strand-cfg-sym-symbolic",
            "strand-cfg-plain-symbolic",
            "strand-cfg-raster",
        ],
        0,
        &mut buf,
    );
    assert_eq!(at(&buf, 0), (0, 0, 0xff), "black recoloured to `color`");
    // Asked for as `-symbolic`, found as the plain file: drawn as
    // authored, since the file's own name is not symbolic.
    assert_eq!(at(&buf, 1), (0xff, 0, 0), "a plain file as authored");
    assert_eq!(at(&buf, 2), (0xff, 0, 0), "a PNG, scaled, as authored");
}

#[test]
fn a_file_added_to_the_folder_while_running_is_drawn() {
    let mut r = renderer();
    let mut buf = Buffer::new(24, 24, Scale::ONE);
    paint(&mut r, &["strand-cfg-later"], 0, &mut buf);
    assert_eq!(at(&buf, 0), (0, 0xff, 0), "the theme's at first");
    let file = dirs().config.join("strand-cfg-later.svg");
    std::fs::write(&file, square(RED)).unwrap();
    // What the watcher's `CacheKind::Icons` for the folder does.
    r.icons_changed();
    assert_eq!(
        strand_icons::resolve("strand-cfg-later", 24, 1, Some("CfgTest")),
        Some(file.clone())
    );
    {
        use strand_scene::Painter;
        assert!(r.wants_frame(SurfaceId(1)), "the icon's surface repaints");
    }
    // Age 1, as a compositor hands buffers back: the change is damage.
    paint(&mut r, &["strand-cfg-later"], 1, &mut buf);
    assert_eq!(at(&buf, 0), (0xff, 0, 0), "the added file");
    std::fs::remove_file(&file).unwrap();
    r.icons_changed();
    paint(&mut r, &["strand-cfg-later"], 1, &mut buf);
    assert_eq!(at(&buf, 0), (0, 0xff, 0), "removed: the theme's again");
    assert!(dirs().theme.is_dir());
}
