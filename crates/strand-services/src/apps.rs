//! `apps`: installed apps from desktop entries, fuzzy search with match
//! ranges and frecency, and launching.
//!
//! - **Entries.** Every `*.desktop` file under each `applications/`
//!   directory (`$XDG_DATA_HOME`, then each of `$XDG_DATA_DIRS`; the
//!   first directory holding an id wins, as the Desktop Entry spec says),
//!   parsed with `freedesktop-desktop-entry`. An entry is listed when its
//!   `Type` is `Application`, it is not `NoDisplay` or `Hidden` (a
//!   `Hidden` entry also hides the same id further down), `OnlyShowIn` /
//!   `NotShowIn` admit `$XDG_CURRENT_DESKTOP`, and its `TryExec` program
//!   exists. Names, comments and keywords are in the user's language.
//! - **Icons** are checked against the icon theme the renderer draws from
//!   ([`strand_icons`]): a name the theme lacks (or a missing file)
//!   becomes `application-x-executable`; `foo.png` as a name loses its
//!   extension (the spec says names have none).
//! - **Search** ([`search`]) is nucleo's matcher behind our own API
//!   (design.md: "wrap it"): the query against each app's name (its
//!   match positions are the hit's `ranges`, character indices; a
//!   matched grapheme cluster covers all its characters), else
//!   against its generic name and keywords (half the score, no ranges),
//!   plus a frecency bonus. It runs only when asked: `apps.search(q)` is
//!   an async method, so a closed launcher never searches.
//! - **Frecency** ([`Frecency`]): launches per app and the last one,
//!   persisted through `strand-core`'s persist store
//!   (`services:apps.frecency` under `$XDG_STATE_HOME/strand/persist`).
//! - **Launch** ([`command`]): the `Exec` line split as the spec quotes
//!   it, field codes expanded (no files or URLs are passed, so `%f %F %u
//!   %U` go; `%i`, `%c`, `%k`, `%%`), in a terminal for `Terminal=true`
//!   (`xdg-terminal-exec`, else `$TERMINAL -e`), from `Path`, detached
//!   (its own session, reparented to init, never a zombie of the shell).
//! - **Live.** [`changed`] (the binary calls it when the watcher reports
//!   an `applications/` directory or the icon theme changed) makes a
//!   running service read the entries again; nothing polls.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use freedesktop_desktop_entry::DesktopEntry;
use nucleo::Matcher;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};

use crate::{Call, Cx, Msg, ServiceError, Store, service};

/// The schema the `apps` service serves.
pub const SCHEMA: &str = strand_services_schema::APPS;

/// The icon an app gets when its entry names none the theme has.
pub const FALLBACK_ICON: &str = "application-x-executable";

/// Where the frecency record is kept in the persist store.
pub const FRECENCY_PATH: &str = "services:apps.frecency";

/// Most results a search returns.
pub const MAX_HITS: usize = 200;

/// How long an app no longer installed keeps its frecency (90 days, in
/// seconds).
pub const FORGET_AFTER: u64 = 90 * 86_400;

/// An installed app.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "App", key = id)]
pub struct App {
    /// Its desktop entry id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its comment.
    pub comment: Option<String>,
    /// Its icon: a theme icon name or a path.
    pub icon: String,
    /// Its categories.
    pub categories: Vec<String>,
}

/// A span of characters of a name.
#[derive(crate::Data, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[data(name = "Range")]
pub struct Range {
    pub start: i64,
    pub end: i64,
}

/// A search result.
#[derive(crate::Data, Clone, Debug, Default, PartialEq)]
#[data(name = "Hit")]
pub struct Hit {
    pub app: App,
    pub score: f64,
    pub ranges: Vec<Range>,
}

/// `h.app.launch()`.
#[derive(Call, Debug)]
pub enum AppsAction {
    Launch { item: App },
}

/// `apps.search(q)`.
#[derive(Call, Debug)]
pub enum AppsCall {
    Search { query: String },
}

/// See the module docs.
#[service(name = "apps", action = AppsAction, call = AppsCall)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Apps {
    /// Every installed app, keyed by `id`, by name.
    #[store(keyed)]
    pub all: Vec<App>,
}

/// Where apps come from (tests point it at directories of their own).
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The `applications/` directories, most important first.
    pub dirs: Vec<PathBuf>,
    /// `$XDG_CURRENT_DESKTOP`'s names.
    pub desktops: Vec<String>,
    /// The persist store's directory for frecency (`None`: frecency is
    /// counted but not kept).
    pub state: Option<PathBuf>,
}

impl Config {
    /// What a service started now uses: [`set_config`]'s, else
    /// [`Config::from_env`].
    pub fn current() -> Config {
        config()
    }

    /// From the environment: `$XDG_DATA_HOME/applications`, then each
    /// `$XDG_DATA_DIRS/applications`; `$XDG_CURRENT_DESKTOP`; the persist
    /// store's directory.
    pub fn from_env() -> Config {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut dirs = Vec::new();
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        if let Some(d) = data_home {
            dirs.push(d.join("applications"));
        }
        let data = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for d in data.split(':').filter(|d| d.starts_with('/')) {
            let p = Path::new(d).join("applications");
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let state = strand_core::PersistStore::from_env()
            .ok()
            .map(|s| s.dir().to_path_buf());
        Config {
            dirs,
            desktops,
            state,
        }
    }
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

/// Use `config` instead of [`Config::from_env`] for services started
/// from now on (`None`: the environment again).
pub fn set_config(config: Option<Config>) {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = config;
}

fn config() -> Config {
    CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or_else(Config::from_env)
}

/// Bumped by [`changed`]; a running service reads the entries again.
static CHANGES: LazyLock<tokio::sync::watch::Sender<u64>> =
    LazyLock::new(|| tokio::sync::watch::Sender::new(0));

/// The desktop entries or the icon theme changed (the watcher's
/// `CacheKind::Apps` or `CacheKind::Icons`): a running `apps` service
/// reads them again (a stopped one reads them when it next starts).
pub fn changed() {
    CHANGES.send_modify(|n| *n += 1);
}

/// One listed entry: the app and how to start it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub app: App,
    /// The `Exec` line as written (quoted, with field codes).
    pub exec: String,
    pub terminal: bool,
    /// `Path`: the working directory.
    pub path: Option<PathBuf>,
    /// The entry's file (`%k`).
    pub file: PathBuf,
    /// The icon as the entry names it (`%i`).
    pub icon_key: Option<String>,
    /// Generic name and keywords: matched when the name is not.
    pub also: Vec<String>,
}

/// The `.desktop` files under `dir`, with their ids (the path below `dir`,
/// `/` as `-`, without `.desktop`), at most 4 directories deep.
fn files_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0usize)];
    while let Some((d, prefix, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if path.is_dir() {
                if depth < 4 {
                    stack.push((path, format!("{prefix}{name}-"), depth + 1));
                }
            } else if let Some(stem) = name.strip_suffix(".desktop") {
                out.push((format!("{prefix}{stem}"), path));
            }
        }
    }
    out
}

/// Whether `program` (a name looked up in `$PATH`, or a path) can run.
fn executable(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let runs = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return runs(Path::new(program));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| runs(&d.join(program))))
}

/// The icon an app shows: see the module docs.
pub fn resolve_icon(icon: Option<&str>) -> String {
    let Some(icon) = icon.map(str::trim).filter(|i| !i.is_empty()) else {
        return FALLBACK_ICON.into();
    };
    if icon.starts_with('/') {
        return if Path::new(icon).is_file() {
            icon.to_string()
        } else {
            FALLBACK_ICON.into()
        };
    }
    let name = ["png", "svg", "xpm"]
        .iter()
        .find_map(|ext| icon.strip_suffix(&format!(".{ext}")))
        .unwrap_or(icon);
    if strand_icons::exists(name) {
        name.to_string()
    } else {
        FALLBACK_ICON.into()
    }
}

/// Read every listed entry under `config`'s directories, by name.
pub fn load(config: &Config) -> Vec<Entry> {
    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for dir in &config.dirs {
        for (id, file) in files_in(dir) {
            // The first directory holding an id that reads decides it,
            // even when it hides it (`Hidden=true`); a file that does not
            // read or parse leaves the id to the next directory's.
            if seen.contains(&id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Ok(entry) = DesktopEntry::from_str(&file, &text, Some(&locales)) else {
                continue;
            };
            seen.insert(id.clone());
            if let Some(e) = listed(&entry, id, file, &locales, &config.desktops) {
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| {
        a.app
            .name
            .to_lowercase()
            .cmp(&b.app.name.to_lowercase())
            .then_with(|| a.app.id.cmp(&b.app.id))
    });
    out
}

fn listed(
    entry: &DesktopEntry,
    id: String,
    file: PathBuf,
    locales: &[String],
    desktops: &[String],
) -> Option<Entry> {
    if entry.type_() != Some("Application") || entry.no_display() || entry.hidden() {
        return None;
    }
    let shown_in = |list: &[&str]| {
        list.iter()
            .any(|d| desktops.iter().any(|c| c.eq_ignore_ascii_case(d)))
    };
    if let Some(only) = entry.only_show_in()
        && !shown_in(&only)
    {
        return None;
    }
    if let Some(not) = entry.not_show_in()
        && shown_in(&not)
    {
        return None;
    }
    if let Some(t) = entry.try_exec()
        && !t.trim().is_empty()
        && !executable(t.trim())
    {
        return None;
    }
    let exec = entry.exec()?.to_string();
    let name = entry.name(locales)?.to_string();
    let mut also: Vec<String> = Vec::new();
    if let Some(g) = entry.generic_name(locales) {
        also.push(g.to_string());
    }
    if let Some(k) = entry.keywords(locales) {
        also.extend(
            k.into_iter()
                .map(|k| k.to_string())
                .filter(|k| !k.is_empty()),
        );
    }
    let icon_key = entry.icon().map(str::to_string);
    Some(Entry {
        app: App {
            id,
            name,
            comment: entry
                .comment(locales)
                .map(|c| c.to_string())
                .filter(|c| !c.is_empty()),
            icon: resolve_icon(icon_key.as_deref()),
            categories: entry
                .categories()
                .unwrap_or_default()
                .into_iter()
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect(),
        },
        exec,
        terminal: entry.terminal(),
        path: entry
            .path()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from),
        file,
        icon_key,
        also,
    })
}

// --- Frecency ---------------------------------------------------------------

/// Launches per app: how many and the last one (Unix seconds).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frecency {
    pub uses: HashMap<String, (u32, u64)>,
}

impl Frecency {
    /// From its persisted text: one `<last> <count> <id>` line per app.
    pub fn parse(text: &str) -> Frecency {
        let mut uses = HashMap::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(last), Some(count), Some(id)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if let (Ok(last), Ok(count)) = (last.parse::<u64>(), count.parse::<u32>()) {
                uses.insert(id.to_string(), (count, last));
            }
        }
        Frecency { uses }
    }

    /// Its persisted text, by id.
    pub fn text(&self) -> String {
        let mut ids: Vec<&String> = self.uses.keys().collect();
        ids.sort();
        ids.iter()
            .map(|id| {
                let (count, last) = self.uses[*id];
                format!("{last} {count} {id}\n")
            })
            .collect()
    }

    /// `id` was launched at `now` (Unix seconds).
    pub fn record(&mut self, id: &str, now: u64) {
        let e = self.uses.entry(id.to_string()).or_insert((0, now));
        e.0 = e.0.saturating_add(1).min(10_000);
        e.1 = now;
    }

    /// How much `id` is used: its launches weighted by how recent the
    /// last one is (100 within 4 days, 70 within 2 weeks, 50 within a
    /// month, 30 within 3 months, 10 after), as Firefox's frecency
    /// buckets do.
    pub fn points(&self, id: &str, now: u64) -> f64 {
        let Some(&(count, last)) = self.uses.get(id) else {
            return 0.0;
        };
        let days = now.saturating_sub(last) / 86_400;
        let weight = match days {
            0..4 => 100.0,
            4..14 => 70.0,
            14..31 => 50.0,
            31..90 => 30.0,
            _ => 10.0,
        };
        f64::from(count) * weight
    }

    /// Forgets apps no longer installed (`installed` says no) whose last
    /// launch is older than [`FORGET_AFTER`]: the table does not grow
    /// with every app ever removed, and an app removed for a while (an
    /// update, a moved entry) keeps its count meanwhile.
    pub fn prune(&mut self, installed: impl Fn(&str) -> bool, now: u64) {
        self.uses
            .retain(|id, &mut (_, last)| installed(id) || now.saturating_sub(last) < FORGET_AFTER);
    }

    /// The score frecency adds to a match.
    pub fn bonus(&self, id: &str, now: u64) -> f64 {
        10.0 * (1.0 + self.points(id, now)).ln()
    }

    fn load(state: Option<&Path>) -> Frecency {
        let Some(dir) = state else {
            return Frecency::default();
        };
        match strand_core::PersistStore::new(dir).load(FRECENCY_PATH) {
            Ok(Some(stored)) => Frecency::parse(&String::from_utf8_lossy(&stored.value)),
            Ok(None) => Frecency::default(),
            Err(e) => {
                log::warn!("apps: frecency not read: {e}");
                Frecency::default()
            }
        }
    }

    /// Keeps snapshot `generation` unless a later one is kept already.
    /// Saves run one at a time (`written` is held while writing), so two
    /// quick launches whose saves finish out of order never leave the
    /// older table on disk.
    pub fn save_latest(&self, written: &Mutex<u64>, generation: u64, state: Option<&Path>) {
        let mut written = written.lock().unwrap_or_else(PoisonError::into_inner);
        if *written >= generation {
            return;
        }
        self.save(state);
        *written = generation;
    }

    fn save(&self, state: Option<&Path>) {
        let Some(dir) = state else {
            return;
        };
        if let Err(e) =
            strand_core::PersistStore::new(dir).save(FRECENCY_PATH, b"", self.text().as_bytes())
        {
            log::warn!("apps: frecency not kept: {e}");
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// --- Search -----------------------------------------------------------------

/// Consecutive character indices as `[start, end)` ranges.
fn ranges_of(mut indices: Vec<u32>) -> Vec<Range> {
    indices.sort_unstable();
    indices.dedup();
    let mut out: Vec<Range> = Vec::new();
    for i in indices {
        let i = i64::from(i);
        match out.last_mut() {
            Some(r) if r.end == i => r.end = i + 1,
            _ => out.push(Range {
                start: i,
                end: i + 1,
            }),
        }
    }
    out
}

/// The fuzzy matcher (nucleo's), wrapped: one per search.
pub struct Fuzzy {
    pattern: Pattern,
    matcher: Matcher,
    buf: Vec<char>,
    /// The character offset where each grapheme of the text in `buf`
    /// starts, and the text's length in characters last.
    starts: Vec<u32>,
}

impl std::fmt::Debug for Fuzzy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Fuzzy")
    }
}

impl Fuzzy {
    /// A matcher for `query` (smart case: lower case matches either
    /// case; accents ignored unless the query has them; words separated
    /// by spaces must all match).
    pub fn new(query: &str) -> Fuzzy {
        Fuzzy {
            pattern: Pattern::parse(query, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(nucleo::Config::DEFAULT),
            buf: Vec::new(),
            starts: Vec::new(),
        }
    }

    /// `text` as nucleo matches it: ASCII as is, anything else one
    /// character per grapheme cluster (its first, as nucleo's own
    /// `Utf32Str::new` does; but never its ASCII shortcut over the bytes
    /// of a text whose clusters all start in ASCII, `e` + U+0301 say,
    /// which would match the combining mark's bytes as characters).
    /// `starts` maps the clusters back to characters.
    fn hay<'a>(
        text: &'a str,
        buf: &'a mut Vec<char>,
        starts: &mut Vec<u32>,
    ) -> nucleo::Utf32Str<'a> {
        use unicode_segmentation::UnicodeSegmentation;
        starts.clear();
        if text.is_ascii() {
            return nucleo::Utf32Str::Ascii(text.as_bytes());
        }
        buf.clear();
        let mut at = 0u32;
        for g in text.graphemes(true) {
            if let Some(c) = g.chars().next() {
                buf.push(c);
                starts.push(at);
            }
            at = at.saturating_add(u32::try_from(g.chars().count()).unwrap_or(u32::MAX));
        }
        starts.push(at);
        nucleo::Utf32Str::Unicode(buf)
    }

    /// `text`'s score and the matched characters as ranges (character
    /// indices: a matched grapheme cluster covers all its characters);
    /// `None` when it does not match.
    pub fn matches(&mut self, text: &str) -> Option<(u32, Vec<Range>)> {
        let hay = Self::hay(text, &mut self.buf, &mut self.starts);
        let mut indices = Vec::new();
        let score = self.pattern.indices(hay, &mut self.matcher, &mut indices)?;
        if !self.starts.is_empty() {
            let starts = &self.starts;
            indices = indices
                .iter()
                .filter_map(|&g| {
                    let (a, b) = (starts.get(g as usize)?, starts.get(g as usize + 1)?);
                    Some(*a..*b)
                })
                .flatten()
                .collect();
        }
        Some((score, ranges_of(indices)))
    }

    /// `text`'s score, without ranges.
    pub fn score(&mut self, text: &str) -> Option<u32> {
        let hay = Self::hay(text, &mut self.buf, &mut self.starts);
        self.pattern.score(hay, &mut self.matcher)
    }
}

/// `apps.search(query)` over `entries`: see the module docs. Best first
/// (ties by name), at most [`MAX_HITS`].
pub fn search(entries: &[Entry], frecency: &Frecency, query: &str, now: u64) -> Vec<Hit> {
    let query = query.trim();
    let mut hits: Vec<Hit> = Vec::new();
    if query.is_empty() {
        hits = entries
            .iter()
            .map(|e| Hit {
                app: e.app.clone(),
                score: frecency.bonus(&e.app.id, now),
                ranges: Vec::new(),
            })
            .collect();
    } else {
        let mut fuzzy = Fuzzy::new(query);
        for e in entries {
            let (score, ranges) = match fuzzy.matches(&e.app.name) {
                Some((s, r)) => (f64::from(s), r),
                None => {
                    let best = e.also.iter().filter_map(|t| fuzzy.score(t)).max();
                    match best {
                        Some(s) => (f64::from(s) / 2.0, Vec::new()),
                        None => continue,
                    }
                }
            };
            hits.push(Hit {
                app: e.app.clone(),
                score: score + frecency.bonus(&e.app.id, now),
                ranges,
            });
        }
    }
    // A stable sort: entries come by name, so ties stay by name.
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(MAX_HITS);
    hits
}

// --- Launch -----------------------------------------------------------------

/// The Desktop Entry spec's string escapes (`\s`, `\n`, `\t`, `\r`,
/// `\\`), which apply before an `Exec` line is split; others are kept.
fn unescape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// An `Exec` line's arguments, split as the spec quotes them: by spaces,
/// a double-quoted argument taking `\"`, `` \` ``, `\$` and `\\`
/// escapes. A field code stays in its argument (as `%`-prefixed text) for
/// [`command`] to expand; `quoted` says which arguments were quoted
/// (field codes are not expanded inside quotes).
fn split_exec(exec: &str) -> Result<Vec<(String, bool)>, String> {
    let exec = unescape_string(exec);
    let mut args = Vec::new();
    let mut chars = exec.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        let mut arg = String::new();
        if first == '"' {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c @ ('"' | '`' | '$' | '\\')) => arg.push(c),
                        Some(c) => {
                            arg.push('\\');
                            arg.push(c);
                        }
                        None => return Err("unterminated quote".into()),
                    },
                    Some(c) => arg.push(c),
                    None => return Err("unterminated quote".into()),
                }
            }
            args.push((arg, true));
        } else {
            while let Some(&c) = chars.peek() {
                if c == ' ' || c == '\t' {
                    break;
                }
                arg.push(c);
                chars.next();
            }
            args.push((arg, false));
        }
    }
    Ok(args)
}

/// The program and arguments that start `entry` (no files or URLs: the
/// launcher passes none), with the terminal around it when it asks for
/// one. Field codes: `%f %F %u %U` (and the deprecated `%d %D %n %N %v
/// %m`) are dropped, `%i` is `--icon <Icon>`, `%c` the name, `%k` the
/// entry's file, `%%` a `%`.
pub fn command(entry: &Entry) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for (arg, quoted) in split_exec(&entry.exec)? {
        if quoted {
            out.push(arg);
            continue;
        }
        match arg.as_str() {
            "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
            "%i" => {
                if let Some(icon) = &entry.icon_key {
                    out.push("--icon".into());
                    out.push(icon.clone());
                }
                continue;
            }
            _ => {}
        }
        let mut s = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                s.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => s.push('%'),
                Some('c') => s.push_str(&entry.app.name),
                Some('k') => s.push_str(&entry.file.to_string_lossy()),
                // Files, URLs and the deprecated codes inside an argument
                // expand to nothing.
                Some(_) | None => {}
            }
        }
        out.push(s);
    }
    if out.is_empty() {
        return Err(format!("`{}` has an empty Exec line", entry.app.id));
    }
    if entry.terminal {
        let mut wrapped = if executable("xdg-terminal-exec") {
            vec!["xdg-terminal-exec".to_string()]
        } else {
            match std::env::var("TERMINAL")
                .ok()
                .filter(|t| !t.trim().is_empty())
            {
                Some(t) => vec![t, "-e".into()],
                None => {
                    return Err(format!(
                        "`{}` runs in a terminal, and there is no xdg-terminal-exec or $TERMINAL",
                        entry.app.id
                    ));
                }
            }
        };
        wrapped.extend(out);
        out = wrapped;
    }
    Ok(out)
}

/// Start `argv` detached: its own session, reparented to init (a double
/// fork, so the shell never holds a zombie), stdio on `/dev/null`, from
/// `dir` when given. Returns once it was executed (an `execvp` failure is
/// the error).
pub fn spawn_detached(argv: &[String], dir: Option<&Path>) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("nothing to run"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(d) = dir.filter(|d| d.is_dir()) {
        cmd.current_dir(d);
    }
    // SAFETY: only async-signal-safe calls between fork and exec
    // (`setsid`, `fork`, `_exit`), as `pre_exec` requires.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut child = cmd.spawn()?;
    // The intermediate process exits at once: reap it.
    child.wait()?;
    Ok(())
}

/// How many launches this process made (tests).
static LAUNCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Launches made by every `apps` service of this process.
pub fn launches() -> u64 {
    LAUNCHES.load(std::sync::atomic::Ordering::Relaxed)
}

static SEARCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Searches run by every `apps` service of this process (tests: a closed
/// launcher never searches).
pub fn searches() -> u64 {
    SEARCHES.load(std::sync::atomic::Ordering::Relaxed)
}

impl Apps {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        let mut changes = CHANGES.subscribe();
        changes.mark_unchanged();
        let config = config();
        let read = |config: Config| async move {
            tokio::task::spawn_blocking(move || load(&config))
                .await
                .map_err(|e| ServiceError(format!("reading desktop entries: {e}")))
        };
        let mut entries = read(config.clone()).await?;
        let state = config.state.clone();
        let mut frecency = {
            let s = state.clone();
            tokio::task::spawn_blocking(move || Frecency::load(s.as_deref()))
                .await
                .unwrap_or_default()
        };
        // The snapshots saved: the latest one wins (`save_latest`).
        let written = Arc::new(Mutex::new(0u64));
        let mut generation = 0u64;
        cx.update(|s| s.all = entries.iter().map(|e| e.app.clone()).collect());
        cx.ready();
        // Launches in flight (each on the blocking pool).
        let mut launching: tokio::task::JoinSet<Result<String, String>> =
            tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                m = cx.recv() => match m {
                    None => return Ok(()),
                    Some(Msg::Call(AppsCall::Search { query }, reply)) => {
                        if reply.is_closed() {
                            continue;
                        }
                        SEARCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        reply.send(Ok::<_, String>(search(&entries, &frecency, &query, unix_now())));
                    }
                    Some(Msg::Action(AppsAction::Launch { item })) => {
                        let Some(entry) = entries.iter().find(|e| e.app.id == item.id) else {
                            log::warn!("apps: no app `{}` to launch", item.id);
                            continue;
                        };
                        // Forking, waiting for the exec and reaping the
                        // intermediate process block: off the shared
                        // runtime, so no service waits on a launch.
                        let (id, entry) = (entry.app.id.clone(), entry.clone());
                        launching.spawn_blocking(move || {
                            command(&entry).and_then(|argv| {
                                spawn_detached(&argv, entry.path.as_deref())
                                    .map_err(|e| format!("cannot start `{}`: {e}", argv[0]))
                            })?;
                            Ok::<_, String>(id)
                        });
                    }
                    Some(_) => {}
                },
                Some(done) = launching.join_next(), if !launching.is_empty() => {
                    match done {
                        Ok(Ok(id)) => {
                            LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let now = unix_now();
                            frecency.record(&id, now);
                            frecency.prune(|id| entries.iter().any(|e| e.app.id == id), now);
                            generation += 1;
                            let (f, s, w) = (frecency.clone(), state.clone(), written.clone());
                            tokio::task::spawn_blocking(move || {
                                f.save_latest(&w, generation, s.as_deref());
                            });
                        }
                        Ok(Err(e)) => log::warn!("apps: {e}"),
                        Err(e) => log::warn!("apps: a launch failed: {e}"),
                    }
                }
                r = changes.changed() => {
                    if r.is_err() {
                        continue;
                    }
                    entries = read(config.clone()).await?;
                    if !cx.update(|s| s.all = entries.iter().map(|e| e.app.clone()).collect()) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exec: &str) -> Entry {
        Entry {
            app: App {
                id: "x".into(),
                name: "X Files".into(),
                ..App::default()
            },
            exec: exec.into(),
            terminal: false,
            path: None,
            file: PathBuf::from("/a/x.desktop"),
            icon_key: Some("x-icon".into()),
            also: Vec::new(),
        }
    }

    #[test]
    fn exec_lines_are_split_and_field_codes_expanded() {
        let cmd = |e: &str| command(&entry(e)).unwrap();
        assert_eq!(cmd("foot"), ["foot"]);
        assert_eq!(cmd("firefox %u"), ["firefox"]);
        assert_eq!(cmd("gimp-2.10 %U --new"), ["gimp-2.10", "--new"]);
        assert_eq!(
            cmd("app %i --title=%c"),
            ["app", "--icon", "x-icon", "--title=X Files"]
        );
        assert_eq!(cmd("app %k 100%%"), ["app", "/a/x.desktop", "100%"]);
        assert_eq!(
            cmd(r#"sh -c "echo \"a b\" \$HOME %f""#),
            ["sh", "-c", r#"echo "a b" $HOME %f"#],
            "quoted: escapes, no field codes"
        );
        // The file's own string escapes come first (`\s` is a space, so
        // it separates unless quoted).
        assert_eq!(cmd(r"my\sapp --flag"), ["my", "app", "--flag"]);
        assert_eq!(cmd(r#""my\sapp" --flag"#), ["my app", "--flag"]);
        assert_eq!(cmd(r#""/opt/My App/run" --x"#), ["/opt/My App/run", "--x"]);
        assert!(command(&entry("\"unterminated")).is_err());
        assert!(command(&entry("%f")).is_err(), "nothing left to run");
    }

    /// Ranges count characters, as the schema's `Range` and the
    /// renderer's marks do, though nucleo matches grapheme clusters: a
    /// matched cluster covers all its characters, and the characters
    /// after a combining mark keep their places.
    #[test]
    fn match_ranges_are_characters_not_clusters() {
        let r = |s, e| Range { start: s, end: e };
        // `e` + U+0301: the clusters all start in ASCII.
        let (_, ranges) = Fuzzy::new("fil").matches("Cafe\u{301} Files").unwrap();
        assert_eq!(ranges, vec![r(6, 9)]);
        let (_, ranges) = Fuzzy::new("cafe").matches("Cafe\u{301} Files").unwrap();
        assert_eq!(ranges, vec![r(0, 5)], "the mark goes with its letter");
        // Devanagari: फ़ा इ लें, clusters of three, one and three characters.
        let (_, ranges) = Fuzzy::new("\u{932}")
            .matches("\u{92b}\u{93c}\u{93e}\u{907}\u{932}\u{947}\u{902}")
            .unwrap();
        assert_eq!(ranges, vec![r(4, 7)]);
        // A cluster-free text is unchanged.
        let (_, ranges) = Fuzzy::new("efo").matches("Firefox").unwrap();
        assert_eq!(ranges, vec![r(3, 6)]);
        let (_, ranges) = Fuzzy::new("öl").matches("Größe öl").unwrap();
        assert_eq!(ranges, vec![r(6, 8)]);
    }

    #[test]
    fn search_matches_names_with_ranges_then_keywords_and_frecency() {
        let mk = |id: &str, name: &str, also: &[&str]| Entry {
            app: App {
                id: id.into(),
                name: name.into(),
                ..App::default()
            },
            also: also.iter().map(|s| s.to_string()).collect(),
            ..entry("x")
        };
        // By name, as `load` gives them.
        let entries = vec![
            mk("files", "Files", &["File Manager"]),
            mk("firefox", "Firefox", &["Web Browser", "internet"]),
            mk("foot", "Foot", &["Terminal"]),
        ];
        let mut f = Frecency::default();
        let now = 1_000_000_000;
        let hits = search(&entries, &f, "fi", now);
        let ids: Vec<&str> = hits.iter().map(|h| h.app.id.as_str()).collect();
        assert!(ids.contains(&"firefox") && ids.contains(&"files"));
        assert!(!ids.contains(&"foot"));
        let firefox = hits.iter().find(|h| h.app.id == "firefox").unwrap();
        assert_eq!(firefox.ranges, [Range { start: 0, end: 2 }]);
        // A keyword match: no ranges, a lower score.
        let web = search(&entries, &f, "browser", now);
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].app.id, "firefox");
        assert!(web[0].ranges.is_empty());
        // Frecency lifts the used app above an equal match.
        let before = search(&entries, &f, "f", now);
        f.record("foot", now);
        f.record("foot", now);
        let after = search(&entries, &f, "f", now);
        assert_eq!(after[0].app.id, "foot", "{before:?} -> {after:?}");
        // An empty query lists everything, the used first.
        let all = search(&entries, &f, "  ", now);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].app.id, "foot");
        assert_eq!(all[1].app.id, "files", "then by name");
        // Unicode: ranges count characters.
        let entries = vec![mk("e", "Éditeur de texte", &[])];
        let h = search(&entries, &f, "tex", now);
        assert_eq!(h[0].ranges, [Range { start: 11, end: 14 }]);
    }

    /// An app gone for longer than [`FORGET_AFTER`] is forgotten; one
    /// still installed, or gone only lately, keeps its count.
    #[test]
    fn frecency_forgets_long_uninstalled_apps() {
        let mut f = Frecency::default();
        let now = 1_000 * 86_400;
        f.record("gone-long", now - FORGET_AFTER - 1);
        f.record("gone-lately", now - 86_400);
        f.record("installed-old", now - FORGET_AFTER - 1);
        f.prune(|id| id == "installed-old", now);
        let mut kept: Vec<&str> = f.uses.keys().map(String::as_str).collect();
        kept.sort();
        assert_eq!(kept, ["gone-lately", "installed-old"]);
    }

    /// Two saves finishing out of order leave the later snapshot on disk.
    #[test]
    fn frecency_saves_keep_the_latest_snapshot() {
        let dir = std::env::temp_dir().join(format!("strand-apps-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = Mutex::new(0);
        let mut first = Frecency::default();
        first.record("a", 1);
        let mut second = first.clone();
        second.record("a", 2);
        // The second launch's save runs first; the first's comes late.
        second.save_latest(&written, 2, Some(&dir));
        first.save_latest(&written, 1, Some(&dir));
        assert_eq!(Frecency::load(Some(&dir)), second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A user entry that does not read leaves its id to the system's; a
    /// hidden one still claims it.
    #[test]
    fn unreadable_entries_do_not_hide_the_next_directorys() {
        let root = std::env::temp_dir().join(format!("strand-apps-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (user, system) = (root.join("user"), root.join("system"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        let ok =
            |name: &str| format!("[Desktop Entry]\nType=Application\nName={name}\nExec=true\n");
        std::fs::write(
            user.join("broken.desktop"),
            b"[Desktop Entry]\nName=\xff\xfe\n",
        )
        .unwrap();
        std::fs::write(system.join("broken.desktop"), ok("System Copy")).unwrap();
        std::fs::write(
            user.join("hidden.desktop"),
            format!("{}Hidden=true\n", ok("Hidden")),
        )
        .unwrap();
        std::fs::write(system.join("hidden.desktop"), ok("Hidden")).unwrap();
        let entries = load(&Config {
            dirs: vec![user, system],
            desktops: vec![],
            state: None,
        });
        let names: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.app.id.as_str(), e.app.name.as_str()))
            .collect();
        assert_eq!(names, [("broken", "System Copy")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frecency_round_trips_and_decays() {
        let mut f = Frecency::default();
        f.record("a b", 100);
        f.record("a b", 200);
        f.record("c", 300);
        let g = Frecency::parse(&f.text());
        assert_eq!(f, g);
        assert_eq!(g.uses["a b"], (2, 200));
        let day = 86_400;
        assert_eq!(g.points("a b", 200 + day), 200.0);
        assert_eq!(g.points("a b", 200 + 20 * day), 100.0);
        assert_eq!(g.points("a b", 200 + 365 * day), 20.0);
        assert_eq!(g.points("nothing", 0), 0.0);
        assert!(Frecency::parse("junk\n1 x\n").uses.is_empty());
    }
}
