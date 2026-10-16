//! `apps` on desktop entries in temporary directories: which entries are
//! listed (precedence, `NoDisplay`, `Hidden`, `OnlyShowIn`/`NotShowIn`,
//! `TryExec`, ids of subdirectories, the user's language), icons checked
//! against the icon theme, search with ranges and frecency, launching
//! detached with frecency persisted, and entries read again when they
//! change, all through the service registry.

use std::path::Path;
use std::time::{Duration, Instant};

use strand_core::Runtime;
use strand_services::apps::{self, App, AppsAction, Config};
use strand_services::{Builtin, Buses, Data, DynService, FromData, Services};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn desktop(name: &str, exec: &str, extra: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{extra}")
}

fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn ids(rt: &Runtime, d: &dyn DynService) -> Vec<String> {
    let field = d.fields().iter().position(|f| f.name == "all").unwrap();
    d.keyed_items(rt, field)
        .unwrap()
        .iter()
        .map(|a| App::from_data(a).unwrap().id)
        .collect()
}

/// The tests here set `apps`' process-wide config: one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn apps_lists_searches_launches_and_follows_entries() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("strand-apps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let user = root.join("home/applications");
    let system = root.join("system/applications");
    let marker = root.join("launched");
    // An icon theme with one app icon, the renderer's lookup's.
    let icons = root.join("icons");
    write(
        &icons.join("hicolor/index.theme"),
        "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\nType=Fixed\n",
    );
    write(&icons.join("hicolor/48x48/apps/strand-editor.png"), "png");
    // Only a symbolic variant: the renderer draws it, so the app keeps
    // the name (one lookup chain, strand_icons::resolve).
    write(
        &icons.join("hicolor/48x48/apps/strand-term-symbolic.png"),
        "png",
    );
    strand_icons::set_base_dirs(Some(vec![icons.clone()]));

    write(
        &system.join("editor.desktop"),
        &desktop(
            "Text Editor",
            "true",
            "Name[de]=Texteditor\nIcon=strand-editor.png\nComment=Edit text\nCategories=Utility;TextEditor;\nKeywords=notes;write;\n",
        ),
    );
    write(
        &system.join("hidden-later.desktop"),
        &desktop("Hidden Later", "true", ""),
    );
    // The user's copy of an id hides the system's.
    write(
        &user.join("hidden-later.desktop"),
        &desktop("Hidden Later", "true", "Hidden=true\n"),
    );
    write(
        &system.join("nodisplay.desktop"),
        &desktop("No Display", "true", "NoDisplay=true\n"),
    );
    write(
        &system.join("kde-only.desktop"),
        &desktop("KDE Only", "true", "OnlyShowIn=KDE;\n"),
    );
    write(
        &system.join("not-here.desktop"),
        &desktop("Not Here", "true", "NotShowIn=Strand;\n"),
    );
    write(
        &system.join("missing.desktop"),
        &desktop("Missing", "nope", "TryExec=strand-no-such-program\n"),
    );
    write(
        &system.join("link.desktop"),
        "[Desktop Entry]\nType=Link\nName=A Link\nURL=https://x\n",
    );
    let script = root.join("run.sh");
    write(
        &script,
        &format!(
            "#!/bin/sh\necho \"$@\" > {}.tmp\nmv {0}.tmp {0}\n",
            marker.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        &system.join("kde/viewer.desktop"),
        &desktop(
            "Viewer",
            &format!("{} %c %U", script.display()),
            "Icon=no-such-icon\n",
        ),
    );
    let state = root.join("state");
    apps::set_config(Some(Config {
        dirs: vec![user.clone(), system.clone()],
        desktops: vec!["Strand".into()],
        state: Some(state.clone()),
    }));

    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let d = b.apps.dynamic();
    // Nothing runs until something reads it.
    assert!(!b.apps.running());
    b.apps.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    assert_eq!(ids(&rt, &*d), ["editor", "kde-viewer"], "by name");
    let all = d
        .keyed_items(&rt, 0)
        .unwrap()
        .into_iter()
        .map(|a| App::from_data(&a).unwrap())
        .collect::<Vec<_>>();
    let editor = all.iter().find(|a| a.id == "editor").unwrap();
    assert_eq!(
        editor.icon, "strand-editor",
        "extension dropped, in the theme"
    );
    assert_eq!(editor.comment.as_deref(), Some("Edit text"));
    assert_eq!(editor.categories, ["Utility", "TextEditor"]);
    let viewer = all.iter().find(|a| a.id == "kde-viewer").unwrap();
    assert_eq!(viewer.icon, apps::FALLBACK_ICON, "not in the theme");

    // Search: by name with ranges, by keyword without.
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let search = |q: &str| -> Vec<(String, Vec<(i64, i64)>)> {
        let fut = d.fetch(&rt, "search", &[Data::text(q)]);
        let hits = tokio.block_on(fut).unwrap();
        let Data::List(hits) = hits else {
            panic!("{hits:?}")
        };
        hits.iter()
            .map(|h| {
                let id = App::from_data(h.field("app").unwrap()).unwrap().id;
                let ranges = match h.field("ranges") {
                    Some(Data::List(r)) => r
                        .iter()
                        .map(|r| {
                            let n = |f| i64::from_data(r.field(f).unwrap()).unwrap();
                            (n("start"), n("end"))
                        })
                        .collect(),
                    other => panic!("{other:?}"),
                };
                (id, ranges)
            })
            .collect()
    };
    assert_eq!(search("text"), [("editor".to_string(), vec![(0, 4)])]);
    assert_eq!(search("notes"), [("editor".to_string(), vec![])]);
    assert!(search("zzzz").is_empty());
    assert_eq!(search("").len(), 2);

    // Launch: detached, field codes expanded, counted for frecency and
    // kept.
    let before = apps::launches();
    b.apps
        .act(
            &rt,
            AppsAction::Launch {
                item: viewer.clone(),
            },
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().trim(),
        "Viewer",
        "%c is the name, %U nothing"
    );
    assert_eq!(apps::launches(), before + 1);
    // The used app now comes first for an empty query, and the count is
    // on disk.
    let deadline = Instant::now() + Duration::from_secs(10);
    let stored = loop {
        let got = strand_core::PersistStore::new(&state)
            .load(apps::FRECENCY_PATH)
            .unwrap();
        if let Some(s) = got {
            break String::from_utf8(s.value).unwrap();
        }
        assert!(Instant::now() < deadline, "frecency never kept");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(stored.ends_with(" 1 kde-viewer\n"), "{stored:?}");
    assert_eq!(search("")[0].0, "kde-viewer");
    // Through the dynamic view, as the language calls it.
    assert!(
        d.action(&rt, "launch", Some(&Data::Null), &[], Default::default())
            .is_err()
    );

    // A new entry, a removed one: read again once told.
    write(
        &user.join("new.desktop"),
        &desktop("Another", "true", "Icon=strand-term\n"),
    );
    std::fs::remove_file(system.join("editor.desktop")).unwrap();
    apps::changed();
    until(&rt, &s, "the entries are read again", || {
        ids(&rt, &*d) == ["new", "kde-viewer"]
    });
    let another = App::from_data(&d.keyed_items(&rt, 0).unwrap()[0]).unwrap();
    assert_eq!(
        another.icon, "strand-term",
        "drawn from its symbolic variant"
    );

    s.shutdown();
    apps::set_config(None);
    strand_icons::set_base_dirs(None);
    let _ = std::fs::remove_dir_all(&root);
}

/// (M5, S-platform) A launch from a handler a press started gives the
/// app the press's xdg-activation token, in `XDG_ACTIVATION_TOKEN` and
/// `DESKTOP_STARTUP_ID`; a launch with no token (no press) gives it
/// neither, not even the token strand itself was started with.
#[test]
fn a_launch_carries_the_token_and_not_an_inherited_one() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the tests of this binary run one at a time (`SERIAL`), and
    // nothing else here reads the environment while this one sets it.
    unsafe { std::env::set_var("XDG_ACTIVATION_TOKEN", "strand-own-stale") };
    let root = std::env::temp_dir().join(format!("strand-apps-token-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    let marker = root.join("env");
    let script = root.join("env.sh");
    write(
        &script,
        &format!(
            "#!/bin/sh\necho \"[$XDG_ACTIVATION_TOKEN] [$DESKTOP_STARTUP_ID]\" > {0}.tmp\nmv {0}.tmp {0}\n",
            marker.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        &dir.join("env.desktop"),
        &desktop("Env", &script.display().to_string(), ""),
    );
    apps::set_config(Some(Config {
        dirs: vec![dir],
        desktops: vec![],
        state: None,
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    s.set_token_source(Some(std::sync::Arc::new(|surface, serial| {
        Box::pin(async move { Some(format!("tok-{surface}-{serial}")) })
    })));
    let b = Builtin::register(&s, &rt);
    let d = b.apps.dynamic();
    b.apps.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    let app = d.keyed_items(&rt, 0).unwrap()[0].clone();
    let launched = |ctx: strand_core::ActionCtx| -> String {
        let _ = std::fs::remove_file(&marker);
        d.action(&rt, "launch", Some(&app), &[], ctx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "never launched");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read_to_string(&marker).unwrap().trim().to_string()
    };
    let pressed = strand_core::ActionCtx {
        input: Some(strand_core::InputCtx {
            surface: 4,
            serial: 321,
        }),
        invocation: 1,
    };
    assert_eq!(launched(pressed), "[tok-4-321] [tok-4-321]");
    assert_eq!(launched(strand_core::ActionCtx::default()), "[] []");
    s.shutdown();
    apps::set_config(None);
    // SAFETY: as above.
    unsafe { std::env::remove_var("XDG_ACTIVATION_TOKEN") };
    let _ = std::fs::remove_dir_all(&root);
}

/// (launch-env) A launched app gets strand's environment but for the
/// variables that address this shell: a second instance's
/// `STRAND_SOCKET` and `STRAND_STATE_DIR` stay with it, so `strand` in a
/// terminal it opened reaches the display's shell and the app's own
/// variables (here `XDG_STATE_HOME`) are the session's.
#[test]
fn a_launch_never_inherits_the_variables_that_address_this_shell() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: the tests of this binary run one at a time (`SERIAL`), and
    // nothing else here reads the environment while this one sets it.
    unsafe {
        std::env::set_var("STRAND_SOCKET", "/run/strand-second.sock");
        std::env::set_var("STRAND_STATE_DIR", "/tmp/strand-second-state");
        std::env::set_var("STRAND_LAUNCH_ENV_KEPT", "kept");
    }
    let root = std::env::temp_dir().join(format!("strand-apps-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    let marker = root.join("env");
    let script = root.join("env.sh");
    write(
        &script,
        &format!(
            "#!/bin/sh\necho \"[${{STRAND_SOCKET-unset}}] [${{STRAND_STATE_DIR-unset}}] \
             [${{STRAND_LAUNCH_ENV_KEPT-unset}}]\" > {0}.tmp\nmv {0}.tmp {0}\n",
            marker.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        &dir.join("env.desktop"),
        &desktop("Env", &script.display().to_string(), ""),
    );
    apps::set_config(Some(Config {
        dirs: vec![dir],
        desktops: vec![],
        state: None,
    }));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    let d = b.apps.dynamic();
    b.apps.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)));
    rt.flush();
    let app = d.keyed_items(&rt, 0).unwrap()[0].clone();
    d.action(
        &rt,
        "launch",
        Some(&app),
        &[],
        strand_core::ActionCtx::default(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "never launched");
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = std::fs::read_to_string(&marker).unwrap();
    s.shutdown();
    apps::set_config(None);
    // SAFETY: as above.
    unsafe {
        std::env::remove_var("STRAND_SOCKET");
        std::env::remove_var("STRAND_STATE_DIR");
        std::env::remove_var("STRAND_LAUNCH_ENV_KEPT");
    }
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(got.trim(), "[unset] [unset] [kept]");
}

/// Bugs 3 and gaps 9 (m6-w2-services): a window finds its desktop entry
/// by its app id (the entry's id, ignoring case, `StartupWMClass`, a
/// reverse-DNS id's last part, the name, a dash-separated head), and
/// draws the entry's icon, else its app id's (lowercased when only that
/// is in the theme). The owner's Spotify (`Spotify`, XWayland) and
/// Chromium (`Chromium-browser`) are among them.
#[test]
fn windows_find_their_desktop_entries() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = std::env::temp_dir().join(format!("strand-apps-win-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("applications");
    let icons = root.join("icons");
    write(
        &icons.join("hicolor/index.theme"),
        "[Icon Theme]\nDirectories=48x48/apps\n[48x48/apps]\nSize=48\nType=Fixed\n",
    );
    for icon in [
        "spotify-client",
        "chromium",
        "org.gnome.Nautilus",
        "google-chrome",
        "spotify",
    ] {
        write(&icons.join(format!("hicolor/48x48/apps/{icon}.png")), "png");
    }
    strand_icons::set_base_dirs(Some(vec![icons.clone()]));
    write(
        &dir.join("spotify.desktop"),
        &desktop(
            "Spotify",
            "spotify %U",
            "Icon=spotify-client\nCategories=Audio;Music;Player;AudioVideo;\n",
        ),
    );
    write(
        &dir.join("chromium.desktop"),
        &desktop(
            "Chromium",
            "chromium %U",
            "Icon=chromium\nStartupWMClass=chromium\nCategories=Network;WebBrowser;\n",
        ),
    );
    write(
        &dir.join("org.gnome.Nautilus.desktop"),
        &desktop(
            "Files",
            "nautilus --new-window %U",
            "Icon=org.gnome.Nautilus\nCategories=GNOME;Utility;FileManager;\n",
        ),
    );
    write(
        &dir.join("google-chrome.desktop"),
        &desktop(
            "Google Chrome",
            "google-chrome %U",
            "Icon=google-chrome\nStartupWMClass=Google-chrome-stable\n",
        ),
    );
    write(
        &dir.join("vlc.desktop"),
        &desktop("VLC media player", "vlc", "Icon=vlc\n"),
    );
    let index = apps::AppIndex::load(&Config {
        dirs: vec![dir.clone()],
        ..Config::default()
    });
    let found = |app_id: &str| index.find(app_id).map(|e| e.app.id.clone());
    assert_eq!(found("spotify").as_deref(), Some("spotify"), "its id");
    assert_eq!(
        found("Spotify").as_deref(),
        Some("spotify"),
        "ignoring case"
    );
    assert_eq!(
        found("Google-chrome-stable").as_deref(),
        Some("google-chrome"),
        "StartupWMClass"
    );
    assert_eq!(
        found("nautilus").as_deref(),
        Some("org.gnome.Nautilus"),
        "a reverse-DNS id's last part"
    );
    assert_eq!(
        found("org.chromium.Chromium").as_deref(),
        Some("chromium"),
        "the app id's last part"
    );
    assert_eq!(
        found("vlc media player").as_deref(),
        Some("vlc"),
        "the name"
    );
    assert_eq!(
        found("Chromium-browser").as_deref(),
        Some("chromium"),
        "a dash-separated head"
    );
    assert_eq!(found("foot"), None);
    assert_eq!(found(""), None);

    // The icon a window draws, and the app it links to.
    let (icon, app) = index.window("Spotify");
    assert_eq!(icon, "spotify-client");
    let app = app.unwrap();
    assert_eq!(app.id, "spotify");
    assert!(app.categories.contains(&"Music".to_string()));
    assert_eq!(index.window("Chromium-browser").0, "chromium");
    // An entry whose icon the theme lacks: the fallback, as `apps.all`.
    assert_eq!(index.window("vlc").0, apps::FALLBACK_ICON);
    // No entry: the app id lowercased when only that is in the theme,
    // else the app id as it is.
    let bare = apps::AppIndex::default();
    assert_eq!(bare.window("Spotify"), ("spotify".to_string(), None));
    assert_eq!(bare.window("foot"), ("foot".to_string(), None));
    strand_icons::set_base_dirs(None);
    let _ = std::fs::remove_dir_all(&root);
}
