//! The logic side of virtualised lists: render asks for the rows a
//! `list` shows (its view plus overscan, `ToLogic::ListWindow`), and the
//! instance mounts them by key (`Instance::set_list_window`). Drag and
//! drop lands here too: a drop's arguments (`on drop(p, at)`).

use strand_compiler::instantiate::{Instance, MAX_LIST_WINDOW};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_render::HiddenRows;
use strand_scene::{DropPayload, NodeId};

/// Mounts rows `first .. first + count` of virtualised list `list` (at
/// most [`MAX_LIST_WINDOW`] of them). A list that is gone (unmounted
/// since render asked) is ignored.
pub(super) fn set_window(inst: &Instance, list: NodeId, first: u32, count: u32) {
    let count = count.min(MAX_LIST_WINDOW as u32);
    if !inst.set_list_window(list, first, count) {
        log::debug!("list window for {list:?}: not a mounted virtualised list");
    }
}

/// The arguments of `on drop(p, at)`: a `drag:` source's value as its
/// binding last held it (`None`, nothing to deliver, when the source is
/// gone or holds no value now), or a `Drop` record for another
/// program's drop, whose `app` is the installed `App` with the dropped
/// id (null when there is none, or no `apps` service runs). `at` is the
/// insertion index render worked out (a global row index in a `list`),
/// less one when (M6) a copy of a `drag:` source's value is among the
/// windowed list's rows render could not see (`hidden`;
/// `Instance::drop_at`).
pub(super) fn drop_args(
    inst: &Instance,
    host: &SchemaHost,
    payload: &DropPayload,
    at: u32,
    hidden: Option<HiddenRows>,
) -> Option<Vec<Value>> {
    let mut at = at;
    let value = match payload {
        DropPayload::Node(source) => {
            let v = inst.drag_value(*source);
            if v.is_none() {
                log::debug!("drop of {source:?}: the source holds no value now");
            }
            let v = v?;
            if let Some(h) = hidden {
                at = inst.drop_at(&v, at, h.list, h.before);
            }
            v
        }
        DropPayload::External {
            kind,
            files,
            text,
            app_id,
        } => {
            let app = app_id
                .as_deref()
                .and_then(|id| installed_app(inst, host, id))
                .unwrap_or(Value::Null);
            let files = files
                .iter()
                .map(|f| Value::text(f.to_string_lossy().as_ref()))
                .collect();
            host.record(
                "Drop",
                &[
                    ("kind", host.variant("DropKind", kind.name())),
                    ("files", Value::list(files)),
                    ("app", app),
                    ("text", Value::text(text.as_str())),
                ],
            )
        }
    };
    Some(vec![value, Value::int(i64::from(at))])
}

/// The `App` in `apps.all` whose `id` is `id`.
fn installed_app(inst: &Instance, host: &SchemaHost, id: &str) -> Option<Value> {
    let all = host.get(inst.runtime(), "apps.all").ok()?;
    let types = host.types();
    all.as_list()?
        .iter()
        .find(|a| a.field(types, "id").and_then(Value::as_text) == Some(id))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use strand_compiler::instantiate::{SceneMirror, Storage};
    use strand_compiler::reconcile::Build;
    use strand_compiler::source::SourceMap;
    use strand_core::Runtime;
    use strand_scene::{DropKind, NodeKind};

    const DOCK: &str = r#"type Pin { app: text }
export state pins: [Pin] key app = [Pin(app: "a"), Pin(app: "b"), Pin(app: "c")]
export state got = ""
export state app = ""
export state files = 0
export state at = -1
panel P {
  row {
    on drop(p: Pin, i: int) { pins.move(p.app, i) }
    on drop(d: Drop, i: int) { got = d.text; app = d.app?.name ?? "-"; files = d.files.len; at = i }
    for p in pins { box { drag: p } }
  }
}
"#;

    fn boot() -> (Runtime, Rc<SchemaHost>, Instance, SceneMirror) {
        let mut map = SourceMap::new();
        map.add("dock.strand", DOCK);
        let b = Build::compile(None, map).unwrap();
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
        let firefox = host.record(
            "App",
            &[
                ("id", Value::text("firefox.desktop")),
                ("name", Value::text("Firefox")),
            ],
        );
        host.set(&rt, "apps.all", Value::list(vec![firefox]))
            .unwrap();
        let inst = Instance::from_build(&rt, &b, host.clone(), Storage::none());
        let mut m = SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        (rt, host, inst, m)
    }

    fn texts(inst: &Instance, name: &str) -> Value {
        inst.get(&format!("dock.{name}")).unwrap()
    }

    /// A Strand drag's drop hands `on drop` the source's own value and
    /// the index (here a keyed move of pin `c` to the front); another
    /// program's drop is a `Drop` record with its files, text and the
    /// installed `App` its id names (null for an unknown one), which
    /// only the `d: Drop` handler takes. A source that is gone delivers
    /// nothing.
    #[test]
    fn drops_deliver_the_source_value_or_a_drop_record() {
        let (_rt, host, inst, mut m) = boot();
        let row = m.of_kind(NodeKind::Row)[0];
        let boxes = m.children(row).to_vec();
        let args = drop_args(&inst, &host, &DropPayload::Node(boxes[2]), 0, None).unwrap();
        assert_eq!(args[1], Value::int(0));
        assert!(inst.event(row, "drop", args));
        m.apply(&inst.flush().diff).unwrap();
        assert_eq!(m.children(row)[0], boxes[2], "moved by key");
        assert_eq!(texts(&inst, "at"), Value::int(-1), "not a Drop");
        // Another program's files.
        let files = DropPayload::External {
            kind: DropKind::Files,
            files: vec!["/tmp/a.png".into(), "/tmp/b.txt".into()],
            text: String::new(),
            app_id: None,
        };
        let args = drop_args(&inst, &host, &files, 2, None).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "files"), Value::int(2));
        assert_eq!(texts(&inst, "app"), Value::text("-"));
        assert_eq!(texts(&inst, "at"), Value::int(2));
        // An app, resolved through `apps.all`; an unknown one is null.
        let app = |id: &str| DropPayload::External {
            kind: DropKind::App,
            files: vec![],
            text: String::new(),
            app_id: Some(id.into()),
        };
        let args = drop_args(&inst, &host, &app("firefox.desktop"), 1, None).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "app"), Value::text("Firefox"));
        let args = drop_args(&inst, &host, &app("nope.desktop"), 1, None).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "app"), Value::text("-"));
        // Text.
        let text = DropPayload::External {
            kind: DropKind::Text,
            files: vec![],
            text: "hello".into(),
            app_id: None,
        };
        let args = drop_args(&inst, &host, &text, 3, None).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "got"), Value::text("hello"));
        // A source that is no node of ours.
        let gone = NodeId::new(9999, 0);
        assert_eq!(
            drop_args(&inst, &host, &DropPayload::Node(gone), 0, None),
            None
        );
    }

    /// (M6) A drop on a windowed `list` whose rows before its mounted
    /// window render could not see (`HiddenRows`): `at` is one less when
    /// a copy of the dragged value is among them (as `pins.move` takes
    /// it out), as it is when there is none, or nothing is hidden. Two
    /// lists of the same 60 pins: the first shows rows 0.., the second is
    /// moved to rows 40..60.
    #[test]
    fn a_drop_leaves_out_a_copy_render_could_not_see() {
        let mut src = String::from("type Pin { app: text }\nexport state pins: [Pin] key app = [");
        for i in 0..60 {
            src.push_str(&format!("Pin(app: \"p{i}\"), "));
        }
        src.push_str(
            "]\npanel P {\n  col {\n    list { for p in pins { box { drag: p } } }\n    list { on drop(p: Pin, i: int) { pins.move(p.app, i) }\n      for p in pins { box { drag: p } } }\n  }\n}\n",
        );
        let mut map = SourceMap::new();
        map.add("dock.strand", src);
        let b = Build::compile(None, map).unwrap();
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
        let inst = Instance::from_build(&rt, &b, host.clone(), Storage::none());
        let mut m = SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        let lists = m.of_kind(NodeKind::List);
        let (one, two) = (lists[0], lists[1]);
        set_window(&inst, two, 40, 20);
        m.apply(&inst.flush().diff).unwrap();
        let source = |app: &str| {
            m.children(one)
                .iter()
                .copied()
                .find(|n| {
                    inst.drag_value(*n)
                        .and_then(|v| {
                            v.as_record()
                                .and_then(|r| r.fields[0].as_text().map(str::to_string))
                        })
                        .as_deref()
                        == Some(app)
                })
                .unwrap()
        };
        let at = |app: &str, hidden| {
            let args =
                drop_args(&inst, &host, &DropPayload::Node(source(app)), 43, hidden).unwrap();
            args[1].clone()
        };
        let hidden = Some(HiddenRows {
            list: two,
            before: 40,
        });
        assert_eq!(at("p5", hidden), Value::int(42), "p5 is at row 5");
        assert_eq!(at("p5", None), Value::int(43), "nothing hidden");
        assert_eq!(
            at(
                "p5",
                Some(HiddenRows {
                    list: two,
                    before: 5
                })
            ),
            Value::int(43),
            "rows 0..5 hold no p5"
        );
        assert_eq!(at("p20", hidden), Value::int(42));
        // A list that is gone: as render said.
        let gone = Some(HiddenRows {
            list: NodeId::new(9999, 0),
            before: 40,
        });
        assert_eq!(at("p5", gone), Value::int(43));
    }

    /// (M6) A row render could not see is a copy of the dragged item by
    /// render's own test: its `drag:` value has the dragged value's
    /// typed identity. Logic reads what each row's `drag:` value is of
    /// its item off the mounted rows. Three windowed lists of the same
    /// 60 pins moved to rows 40..60, whose rows drag their pin a level
    /// down (`box { box { drag: p } }`), nothing, or their pin's app
    /// (`drag: p.app`, the key); dropped from lists of pins, apps and of
    /// `Tile`s keyed by the same texts. A pin finds its copy among the
    /// pins, and an app among the apps; a `Tile` finds none in either
    /// (another type: render finds none among mounted rows either), nor
    /// does an app's text among installed `App`s (keyed by that text),
    /// and nothing does among rows with no `drag:` (render counts none).
    #[test]
    fn a_hidden_copy_is_one_by_renders_test() {
        let mut src = String::from(
            "type Pin { app: text }\ntype Tile { id: text }\nexport state pins: [Pin] key app = [",
        );
        for i in 0..60 {
            src.push_str(&format!("Pin(app: \"p{i}\"), "));
        }
        src.push_str("]\nexport state tiles: [Tile] key id = [");
        for i in 0..60 {
            src.push_str(&format!("Tile(id: \"p{i}\"), "));
        }
        src.push_str(
            "]\npanel P {\n  col {\n    list { for t in tiles { box { drag: t } } }\n    list { for p in pins { box { drag: p.app } } }\n    list { for p in pins { box { drag: p } } }\n    list { for p in pins { box { box { drag: p } } } }\n    list { for p in pins { box { } } }\n    list { for p in pins { box { drag: p.app } } }\n    list { for a in apps.all { box { drag: a } } }\n  }\n}\n",
        );
        let mut map = SourceMap::new();
        map.add("dock.strand", src);
        let b = Build::compile(None, map).unwrap();
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
        let all: Vec<Value> = (0..60)
            .map(|i| {
                let id = format!("p{i}");
                host.record(
                    "App",
                    &[
                        ("id", Value::text(id.as_str())),
                        ("name", Value::text(id.as_str())),
                    ],
                )
            })
            .collect();
        host.set(&rt, "apps.all", Value::list(all)).unwrap();
        let inst = Instance::from_build(&rt, &b, host.clone(), Storage::none());
        let mut m = SceneMirror::new();
        m.apply(&inst.flush().diff).unwrap();
        let lists = m.of_kind(NodeKind::List);
        let [tiles, apps, from, pins, bare, keyed, installed] = lists[..] else {
            panic!("{lists:?}");
        };
        for l in [pins, bare, keyed, installed] {
            set_window(&inst, l, 40, 20);
        }
        m.apply(&inst.flush().diff).unwrap();
        let name = |v: Value| match v.as_record() {
            Some(r) => r.fields[0].as_text().map(str::to_string),
            None => v.as_text().map(str::to_string),
        };
        let source = |list, app: &str| {
            m.children(list)
                .iter()
                .copied()
                .find(|n| inst.drag_value(*n).and_then(name).as_deref() == Some(app))
                .unwrap()
        };
        let at = |from, to| {
            let hidden = Some(HiddenRows {
                list: to,
                before: 40,
            });
            let args = drop_args(&inst, &host, &DropPayload::Node(from), 43, hidden).unwrap();
            args[1].clone()
        };
        assert_eq!(at(source(from, "p5"), pins), Value::int(42), "a pin");
        // An installed `App` is keyed by its id: the same text as the
        // dragged app's, but not the same type.
        assert_eq!(at(source(apps, "p5"), installed), Value::int(43), "an App");
        assert_eq!(at(source(tiles, "p5"), pins), Value::int(43), "a Tile");
        assert_eq!(at(source(apps, "p5"), keyed), Value::int(42), "an app");
        assert_eq!(at(source(tiles, "p5"), keyed), Value::int(43), "a Tile");
        assert_eq!(at(source(from, "p5"), bare), Value::int(43), "no row drags");
    }
}
