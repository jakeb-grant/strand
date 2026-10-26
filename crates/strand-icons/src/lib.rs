//! The freedesktop icon theme lookup (the Icon Theme Specification), shared
//! by the renderer (`icon`, `image` of an icon name) and the `apps`
//! service (an app's icon is checked against the same themes), with a
//! cache that live changes invalidate.
//!
//! design.md names `freedesktop-icons` 0.4 for this, "watched live". That
//! crate reads every installed theme once per process into a static that
//! nothing can refresh, and caches every lookup (misses included) the same
//! way, so a theme installed or switched while the shell runs, or an
//! app's icon installed after its first miss, would never be seen
//! (decisions.md, wave4-a3). This crate is the same algorithm with its
//! state behind [`invalidate`]: the watcher's `index.theme` changes
//! (`CacheKind::Icons`) call it, and the next lookup reads the themes
//! afresh.
//!
//! Lookups follow the spec: the theme, then the themes it inherits from
//! (depth first), then `hicolor`; in each theme the first directory whose
//! size matches exactly, else the closest; then the base directories
//! themselves (`/usr/share/pixmaps`). PNG before SVG; XPM is never
//! returned (nothing here draws it).
//!
//! (live-examples) A host without a file system (the web host,
//! strand-web) supplies the theme files it has by path ([`set_files`]):
//! lookups see them as files on disk, and the renderer reads an icon's
//! bytes from them ([`file`]) before the disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock};

/// File extensions an icon may have, in the order they are preferred.
pub const EXTENSIONS: [&str; 2] = ["png", "svg"];

/// Lookups remembered (hits and misses) before the memory starts over.
const MAX_REMEMBERED: usize = 4096;

/// How directories are matched against a requested size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Fixed,
    Scalable,
    Threshold,
}

/// One directory of a theme (`48x48/apps`), from its `index.theme`.
#[derive(Clone, Debug)]
struct Dir {
    path: String,
    size: u32,
    scale: u32,
    kind: Kind,
    min: u32,
    max: u32,
    threshold: u32,
}

impl Dir {
    fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.kind {
            Kind::Fixed => self.size == size,
            Kind::Scalable => self.min <= size && size <= self.max,
            Kind::Threshold => {
                self.size.saturating_sub(self.threshold) <= size
                    && size <= self.size.saturating_add(self.threshold)
            }
        }
    }

    /// How far the directory's size is from `size` at `scale`, in device
    /// pixels. Saturating: the sizes come from an `index.theme` (any
    /// `MaxSize=4294967295`).
    fn distance(&self, size: u32, scale: u32) -> u32 {
        let want = size.saturating_mul(scale);
        let at = |n: u32| n.saturating_mul(self.scale);
        match self.kind {
            Kind::Fixed => at(self.size).abs_diff(want),
            Kind::Scalable => at(self.min)
                .saturating_sub(want)
                .saturating_add(want.saturating_sub(at(self.max))),
            Kind::Threshold => {
                let lo = at(self.size.saturating_sub(self.threshold));
                let hi = at(self.size.saturating_add(self.threshold));
                lo.saturating_sub(want)
                    .saturating_add(want.saturating_sub(hi))
            }
        }
    }
}

/// A theme as its `index.theme` describes it, with the base directories
/// that hold it.
#[derive(Debug)]
struct Theme {
    inherits: Vec<String>,
    dirs: Vec<Dir>,
    /// `<base>/<name>` for every base directory that has it.
    roots: Vec<PathBuf>,
}

/// Parses an `index.theme`'s text. `None` without an `[Icon Theme]`
/// group.
fn parse_index(text: &str) -> Option<(Vec<String>, Vec<Dir>)> {
    let mut groups: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Some(g.to_string());
            groups.entry(g.to_string()).or_default();
            continue;
        }
        let (Some(g), Some((k, v))) = (&current, line.split_once('=')) else {
            continue;
        };
        if let Some(group) = groups.get_mut(g) {
            group
                .entry(k.trim().to_string())
                .or_insert_with(|| v.trim().to_string());
        }
    }
    let head = groups.get("Icon Theme")?;
    let list = |key: &str| -> Vec<String> {
        head.get(key)
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let inherits = list("Inherits");
    let mut names = list("Directories");
    for d in list("ScaledDirectories") {
        if !names.contains(&d) {
            names.push(d);
        }
    }
    let dirs = names
        .into_iter()
        .filter_map(|name| {
            let g = groups.get(&name)?;
            let num = |k: &str| g.get(k).and_then(|v| v.parse::<u32>().ok());
            let size = num("Size")?;
            let kind = match g.get("Type").map(String::as_str) {
                Some("Fixed") => Kind::Fixed,
                Some("Scalable") => Kind::Scalable,
                _ => Kind::Threshold,
            };
            Some(Dir {
                path: name,
                size,
                scale: num("Scale").unwrap_or(1).max(1),
                kind,
                min: num("MinSize").unwrap_or(size),
                max: num("MaxSize").unwrap_or(size),
                threshold: num("Threshold").unwrap_or(2),
            })
        })
        .collect();
    Some((inherits, dirs))
}

/// The base directories icons are looked up in, in the spec's order:
/// `~/.icons`, `$XDG_DATA_HOME/icons`, each `$XDG_DATA_DIRS/icons`, then
/// `/usr/share/pixmaps`.
pub fn default_base_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut out = Vec::new();
    if let Some(h) = &home {
        out.push(h.join(".icons"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    if let Some(d) = data_home {
        out.push(d.join("icons"));
    }
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    for d in dirs.split(':').filter(|d| d.starts_with('/')) {
        out.push(Path::new(d).join("icons"));
    }
    out.push(PathBuf::from("/usr/share/pixmaps"));
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// The files that name the desktop's icon theme (GTK's settings): a
/// change to them switches the theme ([`system_theme`]).
pub fn theme_setting_files() -> Vec<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let Some(config) = config else {
        return Vec::new();
    };
    ["gtk-3.0", "gtk-4.0"]
        .iter()
        .map(|v| config.join(v).join("settings.ini"))
        .collect()
}

/// The desktop's icon theme: `$STRAND_ICON_THEME`, else the settings
/// portal's `org.gnome.desktop.interface` `icon-theme` when a portal
/// answered ([`set_desktop_theme`]; what GTK itself follows on Wayland),
/// else `gtk-icon-theme-name` in GTK's settings
/// ([`theme_setting_files`]), else Adwaita. Read again after
/// [`invalidate`].
pub fn system_theme() -> String {
    let (desktop, epoch) = {
        let s = state();
        if let Some(t) = &s.system {
            return t.clone();
        }
        (s.desktop.clone(), generation())
    };
    // The settings files are read without the lock held.
    let t = read_system_theme(desktop);
    let mut s = state();
    if generation() == epoch {
        s.system = Some(t.clone());
    }
    t
}

/// The icon theme the desktop's settings portal names (`None`: no portal,
/// or it names none), which [`system_theme`] prefers to GTK's settings
/// files. `true` when it differs from the one set before: the caches were
/// invalidated ([`invalidate`]), and holders of icons must look them up
/// again.
pub fn set_desktop_theme(name: Option<String>) -> bool {
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    {
        let mut s = state();
        if s.desktop == name {
            return false;
        }
        s.desktop = name;
    }
    invalidate();
    true
}

fn read_system_theme(desktop: Option<String>) -> String {
    if let Ok(t) = std::env::var("STRAND_ICON_THEME")
        && !t.trim().is_empty()
    {
        return t.trim().to_string();
    }
    if let Some(t) = desktop {
        return t;
    }
    for path in theme_setting_files() {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=')
                && k.trim() == "gtk-icon-theme-name"
            {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    "Adwaita".into()
}

#[derive(Default)]
struct State {
    /// Set by [`set_base_dirs`]; else [`default_base_dirs`].
    bases: Option<Vec<PathBuf>>,
    themes: HashMap<String, Option<Arc<Theme>>>,
    found: HashMap<(String, String, u16, u16), Option<PathBuf>>,
    system: Option<String>,
    /// The portal's theme ([`set_desktop_theme`]); kept by [`invalidate`].
    desktop: Option<String>,
    /// The config's `icons/` folder ([`set_config_icons`]); kept by
    /// [`invalidate`].
    config: Option<PathBuf>,
    /// Its files by icon name, listed once per [`generation`].
    config_files: Option<Arc<HashMap<String, PathBuf>>>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Forget every theme read and every lookup made (and the desktop's
/// theme name): the next lookup reads them afresh. What a change under
/// an icon base directory (`index.theme`, `icon-theme.cache`, a new
/// theme) or to GTK's settings calls.
pub fn invalidate() {
    let mut s = state();
    s.themes.clear();
    s.found.clear();
    s.system = None;
    s.config_files = None;
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// (M6, the owner's ruling of 2026-10-14) Look icon names up in `dir`
/// first (the config's `icons/` folder; `None`: no folder). This crate
/// does not know where the config is: `strand run` sets it at boot, and
/// watches the folder so that a change there calls [`invalidate`].
/// Invalidates when it changes.
///
/// The folder is flat: `<name>.svg`, else `<name>.png`, nothing in its
/// subdirectories. [`resolve`] tries it before the theme for the name and
/// its `-symbolic` variant, then the same for each generic fallback, so
/// a file there wins over the theme for the name asked for, and a theme
/// icon of a more specific name wins over a config file of a generic one.
pub fn set_config_icons(dir: Option<PathBuf>) {
    let changed = {
        let mut s = state();
        let changed = s.config != dir;
        s.config = dir;
        changed
    };
    if changed {
        invalidate();
    }
}

/// The folder [`set_config_icons`] set.
pub fn config_icons() -> Option<PathBuf> {
    state().config.clone()
}

/// The config folder's icons by name (`wifi` for `wifi.svg`, an SVG
/// before a PNG of the same name), listed without the lock held and
/// remembered for this [`generation`].
fn config_files() -> Option<Arc<HashMap<String, PathBuf>>> {
    let (dir, epoch) = {
        let s = state();
        if let Some(f) = &s.config_files {
            return Some(f.clone());
        }
        (s.config.clone()?, generation())
    };
    let mut files: HashMap<String, PathBuf> = HashMap::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = e.path();
        let (Some(stem), Some(ext)) = (
            path.file_stem().and_then(|s| s.to_str()),
            path.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        if stem.is_empty() || stem.starts_with('.') || !EXTENSIONS.contains(&ext) {
            continue;
        }
        // A link is followed; a directory or a FIFO is no icon.
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        if ext == "svg" || !files.contains_key(stem) {
            files.insert(stem.to_string(), path);
        }
    }
    let files = Arc::new(files);
    let mut s = state();
    if generation() == epoch && s.config.as_ref() == Some(&dir) {
        s.config_files = Some(files.clone());
    }
    Some(files)
}

/// How many times [`invalidate`] ran: a holder of lookups made earlier
/// compares it to know they may be stale.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Look in `dirs` instead of [`default_base_dirs`] (`None`: the
/// defaults again). Invalidates. For tests and for hosts with a layout of
/// their own.
pub fn set_base_dirs(dirs: Option<Vec<PathBuf>>) {
    state().bases = dirs;
    invalidate();
}

/// The base directories in use.
pub fn base_dirs() -> Vec<PathBuf> {
    state().bases.clone().unwrap_or_else(default_base_dirs)
}

/// (live-examples) The files a host supplies by path, with no file
/// system to read them from.
type Supplied = Arc<HashMap<PathBuf, Arc<[u8]>>>;

static SUPPLIED: LazyLock<RwLock<Option<Supplied>>> = LazyLock::new(|| RwLock::new(None));

/// (live-examples) The files a host without a file system supplies, by
/// absolute path (the web host: the theme files its examples draw from,
/// `index.theme` included): from now on a lookup sees each as a file and
/// each folder above one as a folder, before the disk, and [`file`]
/// gives its bytes. An empty map supplies none. Invalidates.
pub fn set_files(files: HashMap<PathBuf, Arc<[u8]>>) {
    let files = (!files.is_empty()).then(|| Arc::new(files));
    *SUPPLIED.write().unwrap_or_else(PoisonError::into_inner) = files;
    invalidate();
}

/// The bytes of `path` if a host supplied it ([`set_files`]).
pub fn file(path: &Path) -> Option<Arc<[u8]>> {
    SUPPLIED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()?
        .get(path)
        .cloned()
}

fn supplied_dir(path: &Path) -> bool {
    SUPPLIED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|f| f.keys().any(|k| k != path && k.starts_with(path)))
}

fn is_file(path: &Path) -> bool {
    file(path).is_some() || path.is_file()
}

fn is_dir(path: &Path) -> bool {
    supplied_dir(path) || path.is_dir()
}

fn read_text(path: &Path) -> Option<String> {
    match file(path) {
        Some(b) => String::from_utf8(b.to_vec()).ok(),
        None => std::fs::read_to_string(path).ok(),
    }
}

fn load_theme(bases: &[PathBuf], name: &str) -> Option<Theme> {
    let roots: Vec<PathBuf> = bases
        .iter()
        .map(|b| b.join(name))
        .filter(|r| is_dir(r))
        .collect();
    // The first `index.theme` found describes it (spec: "the first one
    // found in the base directories").
    let (inherits, dirs) = roots
        .iter()
        .find_map(|r| read_text(&r.join("index.theme")))
        .and_then(|t| parse_index(&t))?;
    Some(Theme {
        inherits,
        dirs,
        roots,
    })
}

/// The theme `name`, read once per [`generation`]. Its `index.theme` is
/// read without the lock held (a slow file system blocks only this
/// lookup); it is remembered only if no [`invalidate`] ran meanwhile
/// (`epoch`, the generation the lookup began in).
fn theme(bases: &[PathBuf], name: &str, epoch: u64) -> Option<Arc<Theme>> {
    if let Some(t) = state().themes.get(name) {
        return t.clone();
    }
    let t = load_theme(bases, name).map(Arc::new);
    let mut s = state();
    if generation() != epoch {
        return t;
    }
    // Another lookup may have read it meanwhile: keep the first.
    s.themes.entry(name.to_string()).or_insert(t).clone()
}

/// Whether the theme `name` is installed (it has an `index.theme`).
pub fn theme_exists(name: &str) -> bool {
    let epoch = generation();
    theme(&base_dirs(), name, epoch).is_some()
}

/// The themes a lookup in `name` visits: it, its parents depth first,
/// then `hicolor`.
fn chain(bases: &[PathBuf], name: &str, epoch: u64) -> Vec<Arc<Theme>> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    fn visit(
        bases: &[PathBuf],
        name: &str,
        epoch: u64,
        seen: &mut Vec<String>,
        out: &mut Vec<Arc<Theme>>,
    ) {
        if seen.iter().any(|n| n == name) || seen.len() > 32 {
            return;
        }
        seen.push(name.to_string());
        let Some(t) = theme(bases, name, epoch) else {
            return;
        };
        out.push(t.clone());
        for parent in &t.inherits {
            if parent != "hicolor" {
                visit(bases, parent, epoch, seen, out);
            }
        }
    }
    visit(bases, name, epoch, &mut seen, &mut out);
    visit(bases, "hicolor", epoch, &mut seen, &mut out);
    out
}

fn file_in(root: &Path, dir: &str, name: &str) -> Option<PathBuf> {
    EXTENSIONS.iter().find_map(|ext| {
        let p = root.join(dir).join(format!("{name}.{ext}"));
        is_file(&p).then_some(p)
    })
}

fn lookup_in(t: &Theme, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
    for d in t.dirs.iter().filter(|d| d.matches(size, scale)) {
        for root in &t.roots {
            if let Some(p) = file_in(root, &d.path, name) {
                return Some(p);
            }
        }
    }
    let mut best: Option<(u32, PathBuf)> = None;
    for d in &t.dirs {
        let dist = d.distance(size, scale);
        if best.as_ref().is_some_and(|(b, _)| *b <= dist) {
            continue;
        }
        for root in &t.roots {
            if let Some(p) = file_in(root, &d.path, name) {
                best = Some((dist, p));
                break;
            }
        }
    }
    best.map(|(_, p)| p)
}

/// The file of icon `name` at `size` logical pixels and integer `scale`
/// in `theme` (the desktop's, [`system_theme`], when `None`), as the Icon
/// Theme Specification finds it. Hits and misses are remembered until
/// [`invalidate`]. The files are probed without the cache's lock held,
/// so lookups (the renderer's, the `apps` service's) never wait on each
/// other's file system work.
pub fn lookup(name: &str, size: u16, scale: u16, theme: Option<&str>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let theme_name = match theme {
        Some(t) => t.to_string(),
        None => system_theme(),
    };
    let (size, scale) = (size.max(1), scale.max(1));
    let key = (theme_name.clone(), name.to_string(), size, scale);
    let bases = base_dirs();
    let epoch = {
        let s = state();
        if let Some(found) = s.found.get(&key) {
            return found.clone();
        }
        generation()
    };
    let themes = chain(&bases, &theme_name, epoch);
    let found = themes
        .iter()
        .find_map(|t| lookup_in(t, name, u32::from(size), u32::from(scale)))
        .or_else(|| {
            // Unthemed icons in the base directories themselves.
            bases.iter().find_map(|b| file_in(b, "", name))
        });
    let mut s = state();
    // An answer from before an invalidation is not remembered.
    if generation() != epoch {
        return found;
    }
    if s.found.len() >= MAX_REMEMBERED {
        s.found.clear();
    }
    s.found.insert(key, found.clone());
    found
}

/// The names an icon lookup tries, in order: the name, then its other
/// variant (`-symbolic` added, or removed for a symbolic name), then the
/// same for each generic fallback with a trailing `-segment` stripped
/// (the freedesktop icon naming spec: `network-wireless-signal-good`,
/// `network-wireless-signal`, `network-wireless`, `network`). GTK 4 does
/// both; current themes (Adwaita) ship mostly symbolic icons, so a tray
/// item's `network-wireless` finds `network-wireless-symbolic`.
pub fn candidates(name: &str) -> Vec<String> {
    let (base, symbolic) = match name.strip_suffix("-symbolic") {
        Some(b) if !b.is_empty() => (b, true),
        _ => (name, false),
    };
    let mut out = Vec::new();
    let mut g = base;
    loop {
        let sym = format!("{g}-symbolic");
        if symbolic {
            out.push(sym);
            out.push(g.to_string());
        } else {
            out.push(g.to_string());
            out.push(sym);
        }
        match g.rfind('-') {
            Some(i) if i > 0 => g = &g[..i],
            _ => break,
        }
    }
    out
}

/// The file icon `name` draws from: the first of its [`candidates`] the
/// config's `icons/` folder ([`set_config_icons`]) or the theme
/// ([`lookup`]) has. [`candidates`] come in pairs, a name and its other
/// `-symbolic` variant, from the name asked for to its most generic
/// fallback; for each pair the folder is tried before the theme, so
/// `wifi` finds `icons/wifi.svg`, then `icons/wifi-symbolic.svg`, then
/// the theme's `wifi` and `wifi-symbolic`, and only then `wifi`'s
/// fallbacks. What the renderer draws, and what the `apps` service checks
/// an app's icon against: both agree on which names draw.
pub fn resolve(name: &str, size: u16, scale: u16, theme: Option<&str>) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let own = config_files();
    candidates(name).chunks(2).find_map(|pair| {
        own.as_ref()
            .and_then(|own| pair.iter().find_map(|n| own.get(n).cloned()))
            .or_else(|| pair.iter().find_map(|n| lookup(n, size, scale, theme)))
    })
}

/// Whether `name` draws from the desktop's theme (or its parents, or
/// `hicolor`, or a base directory) at any size: [`resolve`] finds it,
/// through the same fallbacks the renderer takes.
pub fn exists(name: &str) -> bool {
    resolve(name, 48, 1, None).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const INDEX: &str = "[Icon Theme]\nName=T\nInherits=Parent\nDirectories=16x16/apps,48x48/apps,scalable/apps\n\n\
        [16x16/apps]\nSize=16\nType=Fixed\n\n[48x48/apps]\nSize=48\nType=Fixed\n\n\
        [scalable/apps]\nSize=48\nType=Scalable\nMinSize=8\nMaxSize=512\n";

    /// Tests that set the process-wide base directories or desktop theme
    /// run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[test]
    fn the_spec_lookup_sizes_parents_hicolor_and_invalidation() {
        let _serial = serial();
        let base = std::env::temp_dir().join(format!("strand-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let icons = base.join("icons");
        let pixmaps = base.join("pixmaps");
        write(&icons.join("T/index.theme"), INDEX);
        write(&icons.join("T/16x16/apps/small.png"), "png");
        write(&icons.join("T/48x48/apps/small.png"), "png");
        write(&icons.join("T/scalable/apps/vector.svg"), "svg");
        write(
            &icons.join("Parent/index.theme"),
            "[Icon Theme]\nDirectories=32x32/apps\n[32x32/apps]\nSize=32\n",
        );
        write(&icons.join("Parent/32x32/apps/inherited.png"), "png");
        write(
            &icons.join("hicolor/index.theme"),
            "[Icon Theme]\nDirectories=64x64/apps\n[64x64/apps]\nSize=64\nType=Fixed\n",
        );
        write(&icons.join("hicolor/64x64/apps/colour.png"), "png");
        write(&pixmaps.join("legacy.png"), "png");
        write(&icons.join("T/48x48/apps/terminal-symbolic.png"), "png");
        set_base_dirs(Some(vec![icons.clone(), pixmaps.clone()]));

        let t = Some("T");
        // The renderer's chain (resolve, and `exists` the apps service
        // asks): a name the theme has only as `-symbolic` draws, and so
        // does a generic fallback (`small-thing` → `small`).
        assert_eq!(lookup("terminal", 48, 1, t), None);
        assert_eq!(
            resolve("terminal", 48, 1, t),
            Some(icons.join("T/48x48/apps/terminal-symbolic.png"))
        );
        assert_eq!(
            resolve("small-thing", 48, 1, t),
            Some(icons.join("T/48x48/apps/small.png"))
        );
        assert_eq!(resolve("nothing", 48, 1, t), None);
        // Exact sizes first, else the closest.
        assert_eq!(
            lookup("small", 16, 1, t),
            Some(icons.join("T/16x16/apps/small.png"))
        );
        assert_eq!(
            lookup("small", 40, 1, t),
            Some(icons.join("T/48x48/apps/small.png"))
        );
        assert_eq!(
            lookup("vector", 128, 2, t),
            Some(icons.join("T/scalable/apps/vector.svg"))
        );
        // Parents, then hicolor, then the base directories.
        assert_eq!(
            lookup("inherited", 16, 1, t),
            Some(icons.join("Parent/32x32/apps/inherited.png"))
        );
        assert_eq!(
            lookup("colour", 16, 1, t),
            Some(icons.join("hicolor/64x64/apps/colour.png"))
        );
        assert_eq!(lookup("legacy", 16, 1, t), Some(pixmaps.join("legacy.png")));
        assert_eq!(lookup("nothing", 16, 1, t), None);
        assert_eq!(lookup("../etc/passwd", 16, 1, t), None);

        // A miss is remembered until invalidated: an icon installed later
        // is found once the cache is told.
        write(&icons.join("T/48x48/apps/nothing.png"), "png");
        assert_eq!(lookup("nothing", 16, 1, t), None, "remembered");
        let g = generation();
        invalidate();
        assert!(generation() > g);
        assert_eq!(
            lookup("nothing", 16, 1, t),
            Some(icons.join("T/48x48/apps/nothing.png"))
        );
        // A theme installed while running is read after invalidation.
        assert!(!theme_exists("New"));
        write(
            &icons.join("New/index.theme"),
            INDEX.replace("Parent", "T").as_str(),
        );
        write(&icons.join("New/48x48/apps/fresh.png"), "png");
        invalidate();
        assert!(theme_exists("New"));
        assert_eq!(
            lookup("small", 48, 1, Some("New")),
            Some(icons.join("T/48x48/apps/small.png")),
            "New inherits T"
        );
        set_base_dirs(None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// (live-examples) Files a host supplies are found as files on disk
    /// are, in a base directory that does not exist: the theme's index,
    /// its folders and its icons; and their bytes are given back. With
    /// none supplied, the same lookup finds nothing.
    #[test]
    fn supplied_files_are_found_where_no_disk_has_them() {
        let _serial = serial();
        let base = PathBuf::from(format!("/nonexistent-strand-icons-{}", std::process::id()));
        let svg: Arc<[u8]> = Arc::from(&b"<svg/>"[..]);
        let files: HashMap<PathBuf, Arc<[u8]>> = [
            (base.join("T/index.theme"), Arc::from(INDEX.as_bytes())),
            (
                base.join("T/scalable/apps/vector-symbolic.svg"),
                svg.clone(),
            ),
        ]
        .into_iter()
        .collect();
        set_base_dirs(Some(vec![base.clone()]));
        assert_eq!(
            resolve("vector", 24, 2, Some("T")),
            None,
            "nothing supplied yet"
        );
        set_files(files);
        assert!(theme_exists("T"));
        let found = resolve("vector", 24, 2, Some("T"));
        assert_eq!(
            found,
            Some(base.join("T/scalable/apps/vector-symbolic.svg"))
        );
        assert_eq!(file(&found.unwrap()).as_deref(), Some(&svg[..]));
        assert_eq!(
            file(&base.join("T/scalable/apps")),
            None,
            "a folder has no bytes"
        );
        set_files(HashMap::new());
        assert_eq!(
            resolve("vector", 24, 2, Some("T")),
            None,
            "none supplied again"
        );
        set_base_dirs(None);
    }

    /// A lookup whose `index.theme` read hangs (a slow file system; here
    /// a FIFO nobody writes yet) does not hold the cache's lock: a lookup
    /// in another theme answers meanwhile.
    #[test]
    fn a_slow_theme_read_blocks_no_other_lookup() {
        let _serial = serial();
        let base = std::env::temp_dir().join(format!("strand-icons-slow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let icons = base.join("icons");
        write(&icons.join("T/index.theme"), INDEX);
        write(&icons.join("T/48x48/apps/small.png"), "png");
        std::fs::create_dir_all(icons.join("Slow")).unwrap();
        let fifo = icons.join("Slow/index.theme");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: no mkfifo");
            return;
        }
        set_base_dirs(Some(vec![icons.clone()]));
        let slow = std::thread::spawn(|| lookup("small", 48, 1, Some("Slow")));
        // Let it reach the read.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(lookup("small", 48, 1, Some("T")));
        });
        let other = rx.recv_timeout(std::time::Duration::from_secs(3));
        // Release the slow read (opening blocks until it reads), then
        // check.
        std::fs::write(&fifo, INDEX.replace("Parent", "T")).unwrap();
        let slow = slow.join().unwrap();
        assert_eq!(
            other,
            Ok(Some(icons.join("T/48x48/apps/small.png"))),
            "a lookup in T answered while Slow's index was being read"
        );
        assert_eq!(
            slow,
            Some(icons.join("T/48x48/apps/small.png")),
            "Slow inherits T"
        );
        set_base_dirs(None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The portal's theme name wins over GTK's settings files (not over
    /// `$STRAND_ICON_THEME`); setting a different one invalidates, the
    /// same one does nothing, and `None` falls back to the files.
    #[test]
    fn the_portal_theme_is_preferred_and_invalidates_on_change() {
        let _serial = serial();
        let files = read_system_theme(None);
        let pinned = std::env::var("STRAND_ICON_THEME").is_ok_and(|t| !t.trim().is_empty());
        let g = generation();
        assert!(set_desktop_theme(Some("PortalTheme".into())));
        assert!(generation() > g, "a switch invalidates");
        if !pinned {
            assert_eq!(system_theme(), "PortalTheme");
        }
        let g = generation();
        assert!(!set_desktop_theme(Some(" PortalTheme ".into())));
        assert_eq!(generation(), g, "the same theme again changes nothing");
        assert!(set_desktop_theme(Some(String::new())), "empty: no theme");
        assert_eq!(system_theme(), files);
        assert!(!set_desktop_theme(None));
        assert_eq!(system_theme(), files);
    }

    #[test]
    fn index_files_parse_types_and_defaults() {
        let (inherits, dirs) = parse_index(INDEX).unwrap();
        assert_eq!(inherits, ["Parent"]);
        assert_eq!(dirs.len(), 3);
        assert_eq!(dirs[2].kind, Kind::Scalable);
        assert!(dirs[2].matches(300, 1) && !dirs[2].matches(300, 2));
        assert!(dirs[0].matches(16, 1) && !dirs[0].matches(17, 1));
        let threshold = Dir {
            path: String::new(),
            size: 24,
            scale: 1,
            kind: Kind::Threshold,
            min: 24,
            max: 24,
            threshold: 2,
        };
        assert!(threshold.matches(22, 1) && threshold.matches(26, 1) && !threshold.matches(27, 1));
        assert_eq!(threshold.distance(30, 1), 4);
        assert!(parse_index("[Other]\nx=1").is_none());
    }

    /// Sizes an odd `index.theme` declares at the edge of `u32` saturate
    /// instead of overflowing (a panic in debug builds).
    #[test]
    fn huge_index_sizes_saturate() {
        let text = "[Icon Theme]\nDirectories=a,b,c\n\
                    [a]\nSize=4294967295\nScale=2\nType=Fixed\n\
                    [b]\nSize=16\nMinSize=4294967295\nMaxSize=4294967295\nScale=4294967295\nType=Scalable\n\
                    [c]\nSize=4294967295\nThreshold=4294967295\nScale=3\n";
        let (_, dirs) = parse_index(text).unwrap();
        for d in &dirs {
            for (size, scale) in [(1, 1), (48, 2), (65535, 65535)] {
                let _ = d.matches(size, scale);
                let _ = d.distance(size, scale);
            }
        }
        assert_eq!(dirs[0].distance(48, 2), u32::MAX - 96);
        assert!(dirs[2].matches(4294967295, 3));
    }

    /// (M6, the owner's ruling of 2026-10-14) The config's `icons/`
    /// folder comes before the theme for each pair of candidates: the
    /// name and its `-symbolic` variant there beat the theme's name, a
    /// theme icon of a more specific name beats a generic file there, an
    /// SVG beats a PNG of the same name, its subdirectories are not
    /// looked in, and a file added is found after [`invalidate`].
    #[test]
    fn the_config_folder_comes_first_for_each_name_and_its_variant() {
        let _serial = serial();
        let base = std::env::temp_dir().join(format!("strand-icons-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let icons = base.join("icons");
        write(&icons.join("T/index.theme"), INDEX);
        for n in ["wifi", "network-wireless", "both", "theme-only"] {
            write(&icons.join(format!("T/scalable/apps/{n}.svg")), "svg");
        }
        let own = base.join("config/icons");
        write(&own.join("wifi-symbolic.svg"), "svg");
        write(&own.join("network.svg"), "svg");
        write(&own.join("both.png"), "png");
        write(&own.join("both.svg"), "svg");
        write(&own.join("raster.png"), "png");
        write(&own.join("sub/nested.svg"), "svg");
        write(&own.join("notes.txt"), "txt");
        set_base_dirs(Some(vec![icons.clone()]));
        let t = Some("T");
        assert_eq!(
            resolve("wifi", 48, 1, t),
            Some(icons.join("T/scalable/apps/wifi.svg"))
        );
        set_config_icons(Some(own.clone()));
        assert_eq!(config_icons(), Some(own.clone()));
        let r = |n: &str| resolve(n, 48, 1, t);
        assert_eq!(
            r("wifi"),
            Some(own.join("wifi-symbolic.svg")),
            "the variant first"
        );
        assert_eq!(r("wifi-symbolic"), Some(own.join("wifi-symbolic.svg")));
        assert_eq!(
            r("network-wireless"),
            Some(icons.join("T/scalable/apps/network-wireless.svg")),
            "the theme's specific name over the folder's generic one"
        );
        assert_eq!(
            r("network-wired"),
            Some(own.join("network.svg")),
            "a generic file"
        );
        assert_eq!(r("both"), Some(own.join("both.svg")), "SVG before PNG");
        assert_eq!(r("raster"), Some(own.join("raster.png")));
        assert_eq!(
            r("theme-only"),
            Some(icons.join("T/scalable/apps/theme-only.svg"))
        );
        assert_eq!(r("nested"), None, "flat");
        assert_eq!(r("notes"), None, "SVG and PNG only");
        assert!(exists("raster"), "the apps service's question too");
        // Listed once per generation: a file added is found after an
        // invalidation, as the watcher's report gives.
        write(&own.join("theme-only.svg"), "svg");
        invalidate();
        assert_eq!(r("theme-only"), Some(own.join("theme-only.svg")));
        set_config_icons(None);
        assert_eq!(r("wifi"), Some(icons.join("T/scalable/apps/wifi.svg")));
        set_base_dirs(None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
