//! The lock's runtime semantics (`instantiate/lock.rs`): while the
//! compositor reports the session locked, a config write of `open:
//! false` is ignored with a notice and the lock's content stays live;
//! after `Unlocked` (an `auth` success, reported by the surface manager)
//! the runtime writes `open` false; `lock_shown` follows the reports, so
//! the reload exemption (`EditClass::LockDeferred`) holds while the
//! session is locked whatever the config wrote, and ends when the
//! compositor refused or released the lock.

use std::rc::Rc;

use strand_compiler::SourceMap;
use strand_compiler::instantiate::{
    IGNORED_CLOSE, Instance, SceneMirror, SessionLock, Storage, Update,
};
use strand_compiler::reconcile::{Build, EditClass};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_scene::{NodeId, NodeKind, Prop, PropValue};

struct Shell {
    inst: Instance,
    scene: SceneMirror,
    build: Build,
    _host: Rc<SchemaHost>,
}

fn compile(prev: Option<&Build>, src: &str) -> Build {
    let mut map = SourceMap::new();
    map.add("shell.strand", src.to_string());
    Build::compile(prev, map).unwrap_or_else(|d| panic!("{d:#?}"))
}

fn boot(src: &str) -> Shell {
    let build = compile(None, src);
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &build.program.types));
    let screen = host.record(
        "Screen",
        &[
            ("id", Value::text("Mock | DP-1 | Display")),
            ("name", Value::text("DP-1")),
            ("make", Value::text("Mock")),
            ("model", Value::text("DP-1")),
            ("description", Value::text("Display")),
        ],
    );
    host.set(&rt, "screens.all", Value::list(vec![screen]))
        .unwrap();
    let inst = Instance::from_build(&rt, &build, host.clone(), Storage::none());
    let mut shell = Shell {
        inst,
        scene: SceneMirror::new(),
        build,
        _host: host,
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

impl Shell {
    fn flush(&mut self) -> Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }

    fn lock(&self) -> NodeId {
        self.scene.of_kind(NodeKind::Lock)[0]
    }

    fn open(&self) -> Option<&PropValue> {
        self.scene.prop(self.lock(), Prop::Open)
    }

    fn click(&mut self, node: NodeId) -> Update {
        self.inst.event(node, "click", Vec::new());
        self.flush()
    }

    fn locked(&self) -> Value {
        self.inst.get("shell.locked").unwrap()
    }

    fn report(&mut self, state: SessionLock) -> Update {
        self.inst.set_session_lock(state);
        let u = self.flush();
        assert!(u.errors.is_empty(), "{:?}", u.errors);
        u
    }

    /// Commits `src`; whether the lock made it wait.
    fn reload(&mut self, src: &str) -> bool {
        let build = compile(Some(&self.build), src);
        let report = self.inst.reload(&build);
        let deferred = report.classes.contains(&EditClass::LockDeferred);
        if !deferred {
            self.build = build;
        }
        self.flush();
        deferred
    }
}

/// `open: <-> locked`; a click on the lock writes `locked = false` (a
/// config write) and counts; a click on the bar locks.
fn shell(lock_text: &str) -> String {
    format!(
        "export state locked = true\n\
         lock {{\n  open: <-> locked\n  state n = 0\n  on click {{ locked = false; n += 1 }}\n  \
         on key(k) {{ auth.submit(k.name) }}\n  \
         text join(\"\", \"{lock_text} \", n)\n}}\n\
         bar Top {{\n  on click {{ locked = true }}\n  text \"bar\"\n}}\n"
    )
}

#[test]
fn a_config_write_cannot_close_a_locked_session() {
    let mut s = boot(&shell("lock"));
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));
    assert!(s.inst.lock_shown(), "requested, not answered yet: shown");
    let u = s.report(SessionLock::Locked);
    assert!(u.notices.is_empty(), "{:?}", u.notices);
    assert!(s.inst.lock_shown());

    // The config writes `false`: the cell takes it, the lock does not.
    let lock = s.lock();
    let u = s.click(lock);
    assert_eq!(s.locked(), Value::Bool(false));
    assert_eq!(s.open(), Some(&PropValue::Bool(true)), "held open");
    assert_eq!(u.notices, [IGNORED_CLOSE], "one notice");
    assert!(s.inst.lock_shown());
    // The content stays live (not frozen as a hidden surface's is).
    assert!(s.scene.texts().contains(&"lock 1".to_string()));
    let u = s.click(lock);
    assert!(s.scene.texts().contains(&"lock 2".to_string()));
    assert!(u.notices.is_empty(), "once per lock session");
    // `strand set` is a config write too.
    s.inst.set("shell.locked", Value::Bool(false)).unwrap();
    s.flush();
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));

    // While locked, a lock edit waits, whatever `open` says.
    assert!(s.reload(&shell("edited")), "deferred while locked");
    assert!(s.scene.texts().contains(&"lock 2".to_string()));

    // The unlock: the runtime writes `open` false.
    s.inst.set("shell.locked", Value::Bool(true)).unwrap();
    s.flush();
    s.report(SessionLock::Unlocked);
    assert_eq!(s.locked(), Value::Bool(false), "the runtime wrote false");
    assert_eq!(s.open(), Some(&PropValue::Bool(false)));
    assert!(!s.inst.lock_shown());
    assert!(!s.reload(&shell("edited")), "lands once unlocked");

    // Locked again: requested, then reported; a new session warns again.
    let bar = s.scene.of_kind(NodeKind::Bar)[0];
    s.click(bar);
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));
    assert!(
        s.scene.texts().contains(&"edited 2".to_string()),
        "the edit landed"
    );
    assert!(s.inst.lock_shown());
    s.report(SessionLock::Locked);
    let lock = s.lock();
    let u = s.click(lock);
    assert_eq!(u.notices, [IGNORED_CLOSE]);
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));
}

#[test]
fn a_refused_lock_is_not_shown() {
    let mut s = boot(&shell("lock"));
    s.report(SessionLock::Finished);
    assert!(!s.inst.lock_shown(), "the compositor refused it");
    // A refused lock holds nothing: a config write closes it at once.
    let lock = s.lock();
    let u = s.click(lock);
    assert!(u.notices.is_empty(), "{:?}", u.notices);
    assert_eq!(s.open(), Some(&PropValue::Bool(false)));
    assert!(!s.reload(&shell("edited")));
    // Opened again, it is a new request.
    let bar = s.scene.of_kind(NodeKind::Bar)[0];
    s.click(bar);
    assert!(s.inst.lock_shown());
}

#[test]
fn the_held_lock_survives_a_reload_of_the_rest() {
    let mut s = boot(&shell("lock"));
    s.report(SessionLock::Locked);
    let other = shell("lock").replace("text \"bar\"", "text \"bar 2\"");
    assert!(!s.reload(&other), "a bar edit lands while locked");
    assert!(s.scene.texts().contains(&"bar 2".to_string()));
    assert!(s.inst.lock_shown());
    let lock = s.lock();
    let u = s.click(lock);
    assert_eq!(u.notices, [IGNORED_CLOSE]);
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));
}

/// A one-way `open` cannot be written: the unlock still ends the lock
/// (the surface manager released it; `lock_shown` follows the report)
/// and nothing fails.
#[test]
fn a_one_way_open_is_left_alone_by_the_unlock() {
    let src = "export state locked = true\nlock {\n  open: locked\n  on key(k) { auth.submit(k.name) }\n  text \"locked\"\n}\n";
    let mut s = boot(src);
    s.report(SessionLock::Locked);
    s.report(SessionLock::Unlocked);
    assert!(!s.inst.lock_shown());
    assert_eq!(s.locked(), Value::Bool(true));
}

/// A lock mounted again by a reload after the unlock, with its one-way
/// `open` still true, is not a new request: the surface manager does not
/// lock again until `open` goes false and true (`session_lock.rs`), so
/// `lock_shown` stays false and later lock edits are not held back for
/// an unlock that never comes. Closing and opening it is a new request.
#[test]
fn a_lock_remounted_open_after_the_unlock_is_not_shown() {
    let src = |t: &str| {
        format!(
            "export state locked = true\nlock {{\n  open: locked\n  on key(k) {{ auth.submit(k.name) }}\n  text \"{t}\"\n}}\n"
        )
    };
    let mut s = boot(&src("locked"));
    s.report(SessionLock::Locked);
    s.report(SessionLock::Unlocked);
    assert!(!s.inst.lock_shown());
    assert!(!s.reload(&src("edited")), "lands: not shown");
    assert!(s.scene.texts().contains(&"edited".to_string()));
    assert_eq!(s.open(), Some(&PropValue::Bool(true)));
    assert!(!s.inst.lock_shown(), "a remount is not a new request");
    assert!(!s.reload(&src("edited again")), "the next edit lands too");
    assert!(s.scene.texts().contains(&"edited again".to_string()));

    s.inst.set("shell.locked", Value::Bool(false)).unwrap();
    s.flush();
    s.inst.set("shell.locked", Value::Bool(true)).unwrap();
    s.flush();
    assert!(s.inst.lock_shown(), "closed and opened: a new request");
    assert!(s.reload(&src("third")), "and a lock edit waits again");
}

/// (report review) A glitch report leaves out a lock's cells and every
/// cell a password input is bound to: the snapshot has neither, even
/// before anything is typed, and the cell tap is told of neither, while
/// a plain input's cell is told and kept.
#[test]
fn a_report_leaves_out_the_locks_cells_and_a_passwords() {
    use std::cell::RefCell;
    let src = "export state locked = true\nexport state pw = \"\"\nexport state q = \"\"\n\
               lock {\n  open: <-> locked\n  state n = 0\n  on click { n += 1 }\n  \
               on key(k) { auth.submit(k.name) }\n  text join(\"\", \"lock \", n)\n}\n\
               bar Top {\n  input { type: password; text: <-> pw }\n  input { text: <-> q }\n}\n";
    let mut s = boot(src);
    let paths = |s: &Shell| -> Vec<String> {
        s.inst
            .cells_snapshot()
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    };
    let before = paths(&s);
    assert!(before.contains(&"shell.q".to_string()), "{before:?}");
    assert!(!before.contains(&"shell.pw".to_string()), "{before:?}");
    assert!(!before.iter().any(|p| p.ends_with(".n")), "{before:?}");
    let told: Rc<RefCell<Vec<String>>> = Rc::default();
    let t = told.clone();
    s.inst.set_cell_tap(Some(Box::new(move |w| {
        t.borrow_mut().push(w.path.to_string())
    })));
    let lock = s.lock();
    s.click(lock);
    assert!(s.scene.texts().contains(&"lock 1".to_string()));
    let inputs = s.scene.of_kind(NodeKind::Input);
    let password = |n: &NodeId| {
        s.scene.prop(*n, Prop::InputType) == Some(&PropValue::Keyword("password".into()))
    };
    let pw = *inputs.iter().find(|n| password(n)).unwrap();
    let plain = *inputs.iter().find(|n| !password(n)).unwrap();
    for text in ["h", "hu", "hun"] {
        s.inst
            .write(pw, Prop::Text, PropValue::Text(text.into()))
            .unwrap();
        s.flush();
    }
    s.inst
        .write(plain, Prop::Text, PropValue::Text("fi".into()))
        .unwrap();
    s.flush();
    assert_eq!(s.inst.get("shell.pw").unwrap(), Value::text("hun"));
    assert_eq!(*told.borrow(), ["shell.q"]);
    let after = paths(&s);
    assert!(after.contains(&"shell.q".to_string()), "{after:?}");
    assert!(!after.contains(&"shell.pw".to_string()), "{after:?}");
    assert!(!after.iter().any(|p| p.ends_with(".n")), "{after:?}");
}
