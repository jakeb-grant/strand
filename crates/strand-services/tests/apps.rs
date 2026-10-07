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

#[test]
fn apps_lists_searches_launches_and_follows_entries() {
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
    assert!(d.action(&rt, "launch", Some(&Data::Null), &[]).is_err());

    // A new entry, a removed one: read again once told.
    write(
        &user.join("new.desktop"),
        &desktop("Another", "true", "Icon=strand-editor\n"),
    );
    std::fs::remove_file(system.join("editor.desktop")).unwrap();
    apps::changed();
    until(&rt, &s, "the entries are read again", || {
        ids(&rt, &*d) == ["new", "kde-viewer"]
    });

    s.shutdown();
    apps::set_config(None);
    strand_icons::set_base_dirs(None);
    let _ = std::fs::remove_dir_all(&root);
}
