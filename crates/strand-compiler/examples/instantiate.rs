//! Runs `.strand` files headless on the real clock and prints the scene
//! after every tick that changed it: the hello bar's clock, live.
//!
//! `cargo run -p strand-compiler --example instantiate -- hello_bar.strand`
//! (`--once` prints the boot scene and exits).

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::instantiate::{Instance, SceneMirror};
use strand_compiler::vm::Value;
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::{SourceMap, lower};
use strand_core::Runtime;

fn main() {
    let mut map = SourceMap::new();
    let mut once = false;
    for arg in std::env::args().skip(1) {
        if arg == "--once" {
            once = true;
            continue;
        }
        match std::fs::read_to_string(&arg) {
            Ok(s) => {
                map.add(arg, s);
            }
            Err(e) => eprintln!("{arg}: {e}"),
        }
    }
    let compiled = strand_compiler::compile(&map);
    if compiled.errors() > 0 {
        eprint!("{}", render(&compiled.diagnostics, &map, Style::Plain));
        std::process::exit(1);
    }
    let program = Arc::new(lower::lower(
        &compiled.program,
        strand_compiler::schema::Schema::builtin(),
    ));
    let rt = Runtime::new();
    let host = Rc::new(SchemaHost::real(&rt, &program.types));
    // One monitor until the binary feeds the real `screens` service.
    let screen = host.record("Screen", &[("name", Value::text("HEADLESS-1"))]);
    let _ = host.set(&rt, "screens.all", Value::list(vec![screen]));
    let inst = Instance::new(&rt, program, host, None);
    let start = Instant::now();
    let mut scene = SceneMirror::new();
    loop {
        let u = inst.tick(start.elapsed());
        for e in &u.errors {
            eprintln!("error: {e}");
        }
        if !u.diff.is_empty() {
            if let Err(e) = scene.apply(&u.diff) {
                eprintln!("inconsistent diff: {e}");
            }
            print!("{}", scene.render());
            println!("--");
        }
        if once {
            return;
        }
        // Sleep until the runtime or a service needs us: the next minute
        // for a clock, never for a static bar.
        let logic = inst
            .next_deadline()
            .map(|d| d.saturating_sub(start.elapsed()));
        let wall = inst
            .next_wake()
            .map(|t| t.duration_since(SystemTime::now()).unwrap_or_default());
        let Some(wait) = [logic, wall].into_iter().flatten().min() else {
            return;
        };
        std::thread::sleep(wait);
        inst.wake(SystemTime::now());
    }
}
