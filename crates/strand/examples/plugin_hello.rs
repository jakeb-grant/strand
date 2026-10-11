//! A plugin binary: `strand` with one more service, `hello`, written in
//! Rust (docs/guide/plugins.md). Build it, write its schema for `strand
//! check` and the LSP, and run it in place of `strand`:
//!
//! ```sh
//! cargo build --release --example plugin_hello
//! target/release/examples/plugin_hello --write-schemas
//! target/release/examples/plugin_hello run
//! ```
//!
//! A config then reads `hello.greeting` and `hello.count`, and `strand
//! call hello.greet world` greets.

use strand_services::{Call, Cx, Msg, ServiceError, Store, service};

// mimalloc and no transparent huge pages, as `strand` runs.
strand::process_setup!();

/// The service's schema: what `strand check`, hovers and completion see.
/// Every entry has a `///` doc (`--write-schemas` refuses one without).
const SCHEMA: &str = "
/// The plugin example's service.
service hello {
  /// The last greeting, `hello, <name>`.
  greeting: text
  /// How many greetings there have been; 41 after its first read.
  count: int rw
  /// Greets `name`: sets `greeting` and adds one to `count`.
  action greet(name: text)
}
";

/// Its actions, read from a call (`hello.greet(\"world\")`).
#[derive(Call, Debug)]
enum HelloAction {
    Greet(String),
}

/// Its state: what the shell reads.
#[service(name = "hello", schema = SCHEMA, action = HelloAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
struct Hello {
    /// The last greeting.
    greeting: String,
    /// How many greetings; written by the shell too.
    #[store(rw)]
    count: i64,
}

impl Hello {
    /// Runs while a config reads `hello` (and 5 s after).
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
        // The first read: where a real service reads its source.
        cx.update(|s| s.count = 41);
        cx.ready();
        while let Some(m) = cx.recv().await {
            match m {
                Msg::Write(w) => {
                    let n: i64 = w.value().map_err(ServiceError::from)?;
                    cx.report(&w, |s| s.count = n);
                }
                Msg::Action(HelloAction::Greet(name)) => {
                    cx.update(|s| {
                        s.greeting = format!("hello, {name}");
                        s.count += 1;
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn main() -> std::process::ExitCode {
    strand::Shell::builder().service::<Hello>().run()
}
