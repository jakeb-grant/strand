//! The logic side of virtualised lists: render asks for the rows a
//! `list` shows (its view plus overscan, `ToLogic::ListWindow`), and the
//! instance mounts them by key (`Instance::set_list_window`). Drag and
//! drop lands here too: a drop's arguments (`on drop(p, at)`).

use strand_compiler::instantiate::{Instance, MAX_LIST_WINDOW};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
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
/// insertion index render worked out (a global row index in a `list`).
pub(super) fn drop_args(
    inst: &Instance,
    host: &SchemaHost,
    payload: &DropPayload,
    at: u32,
) -> Option<Vec<Value>> {
    let value = match payload {
        DropPayload::Node(source) => {
            let v = inst.drag_value(*source);
            if v.is_none() {
                log::debug!("drop of {source:?}: the source holds no value now");
            }
            v?
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
        let args = drop_args(&inst, &host, &DropPayload::Node(boxes[2]), 0).unwrap();
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
        let args = drop_args(&inst, &host, &files, 2).unwrap();
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
        let args = drop_args(&inst, &host, &app("firefox.desktop"), 1).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "app"), Value::text("Firefox"));
        let args = drop_args(&inst, &host, &app("nope.desktop"), 1).unwrap();
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
        let args = drop_args(&inst, &host, &text, 3).unwrap();
        assert!(inst.event(row, "drop", args));
        inst.flush();
        assert_eq!(texts(&inst, "got"), Value::text("hello"));
        // A source that is no node of ours.
        let gone = NodeId::new(9999, 0);
        assert_eq!(drop_args(&inst, &host, &DropPayload::Node(gone), 0), None);
    }
}
