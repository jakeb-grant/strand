//! Headless instantiation: programs mounted on a mock service host, their
//! scene diffs applied to a mirror and snapshotted.

use std::rc::Rc;
use std::sync::Arc;

use strand_compiler::instantiate::{Instance, SceneMirror};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;

struct Shell {
    rt: Runtime,
    host: Rc<SchemaHost>,
    inst: Instance,
    scene: SceneMirror,
}

impl Shell {
    fn flush(&mut self) -> strand_compiler::instantiate::Update {
        let u = self.inst.flush();
        self.scene.apply(&u.diff).unwrap();
        u
    }
}

fn boot(files: &[(&str, &str)], setup: impl FnOnce(&Runtime, &SchemaHost)) -> Shell {
    let mut map = SourceMap::new();
    for (name, text) in files {
        map.add(*name, text.to_string());
    }
    let compiled = strand_compiler::compile(&map);
    assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::mock(&rt, &program.types));
    setup(&rt, &host);
    let inst = Instance::new(&rt, program, host.clone(), None);
    let mut shell = Shell {
        rt,
        host,
        inst,
        scene: SceneMirror::new(),
    };
    let u = shell.flush();
    assert!(u.errors.is_empty(), "{:?}", u.errors);
    shell
}

fn screens(rt: &Runtime, host: &SchemaHost, names: &[&str]) {
    let list = names
        .iter()
        .map(|n| host.record("Screen", &[("name", Value::text(*n))]))
        .collect();
    host.set(rt, "screens.all", Value::list(list)).unwrap();
}

#[test]
fn hello_bar_on_two_monitors() {
    let src = include_str!("fixtures/hello_bar.strand");
    let shell = boot(&[("hello_bar.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1", "HDMI-A-1"]);
        host.set(rt, "battery.percent", Value::float(0.42)).unwrap();
    });
    assert_eq!(shell.scene.roots().len(), 2);
    assert_eq!(
        shell.scene.texts(),
        ["", "09:41", "42%", "", "09:41", "42%"]
    );
}

#[test]
fn the_hello_bar_clock_ticks_once_a_minute() {
    use std::time::{Duration, UNIX_EPOCH};
    let src = include_str!("fixtures/hello_bar.strand");
    let mut shell = boot(&[("hello_bar.strand", src)], |rt, host| {
        screens(rt, host, &["DP-1"]);
    });
    let t0 = strand_compiler::vm::schema_host::MOCK_TIME;
    // The host wakes at the next minute boundary, never before.
    assert_eq!(
        shell.inst.next_wake(),
        Some(UNIX_EPOCH + Duration::from_secs(t0 - 7 + 60))
    );
    assert_eq!(shell.inst.next_deadline(), None);
    shell
        .host
        .set_time(&shell.rt, UNIX_EPOCH + Duration::from_secs(t0 + 20));
    assert!(
        shell.flush().diff.is_empty(),
        "nothing changes within the minute"
    );
    shell
        .inst
        .wake(UNIX_EPOCH + Duration::from_secs(t0 - 7 + 60));
    let u = shell.flush();
    assert_eq!(u.diff.ops.len(), 1, "{:?}", u.diff);
    assert_eq!(shell.scene.texts(), ["", "09:42", "0%"]);
}
