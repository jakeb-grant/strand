# Plugins: a service written in Rust

Most services need no code: `service … from dbus`, `from file`, `from
listen` and `from poll` (level 5) cover a D-Bus object, a file and a
command. When a service needs more than that, write it in Rust and
build your own `strand` binary with it inside. That binary is a
**plugin**: it answers every `strand` command (`run`, `check`,
`call`, …) and serves your service beside the builtin ones.

**Unstable.** The plugin builder is git-only and may change in any
commit. Pin the revision you build against.

## The crate

A plugin is a binary crate that depends on `strand` and
`strand-services`, both from the same git URL and revision (the
`#[service]` derives name `strand_services`; two revisions of it do not
compile together):

```toml
[dependencies]
strand = { git = "https://github.com/jakeb-grant/strand", rev = "<rev>" }
strand-services = { git = "https://github.com/jakeb-grant/strand", rev = "<rev>" }
```

Its `main.rs` declares the service and hands it to the builder. This is
`crates/strand/examples/plugin_hello.rs` in the repository, a `hello`
service with a greeting, a count and one action:

```rust
use strand_services::{Call, Cx, Msg, ServiceError, Store, service};

// mimalloc and no transparent huge pages, as `strand` runs.
strand::process_setup!();

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

#[derive(Call, Debug)]
enum HelloAction {
    Greet(String),
}

#[service(name = "hello", schema = SCHEMA, action = HelloAction)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
struct Hello {
    greeting: String,
    #[store(rw)]
    count: i64,
}

impl Hello {
    async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
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
```

- The schema is what `strand check`, hovers and completion see. Every
  entry needs a `///` doc: `--write-schemas` (below) refuses a schema
  with a hover-less name.
- The service name is lower case: letters, digits and `_`. A builtin's
  name (`battery`, `clock`, …) is refused.
- `run` starts when a config first reads `hello` and stops 5 s after
  the last reader goes, as a builtin service does. `cx.ready()` says the
  first read is in. Add `thread` to `#[service(…)]` for a body that
  blocks; it then gets a thread of its own.
- A registration the builder cannot serve (a bad or taken name, a
  schema that does not apply or that declares a service besides its
  own) stops every command with a message, before anything runs.

## The two things every plugin must copy

`strand`'s size and memory figures hold for your binary only if it is
built and set up the way `strand` is:

1. **`strand::process_setup!();` at the crate root**, once. It makes
   mimalloc the allocator and turns transparent huge pages off before
   the first allocation. A library cannot do this for the binary that
   links it. Without it the shell runs, but uses about twice the memory,
   and the binary warns on every start:
   ``warning: this binary does not invoke `strand::process_setup!()` …``.
2. **The workspace's release profile**, every line of it, in your
   crate's `Cargo.toml`:

```toml
[profile.release]
lto = "fat"
codegen-units = 1

[profile.release.package.strand-services]
opt-level = "z"

[profile.release.package.zbus]
opt-level = "z"

[profile.release.package.zvariant]
opt-level = "z"

[profile.release.package.zbus_names]
opt-level = "z"

[profile.release.package.tokio]
opt-level = "z"

[profile.release.package.swayipc-types]
opt-level = "z"

[profile.release.package.logind-zbus]
opt-level = "z"

[profile.release.package.strand-icons]
opt-level = "s"

[profile.release.package.strand-introspect]
opt-level = "z"

[profile.release.package.toml_edit]
opt-level = "s"

[profile.release.package.toml_parser]
opt-level = "s"

[profile.release.package.fontique]
opt-level = "s"

[profile.release.package.quick-xml]
opt-level = "s"

[profile.release.package.calloop]
opt-level = "s"

[profile.release.package.xkeysym]
opt-level = "s"

[profile.release.package.miette]
opt-level = "z"

[profile.release.package.chrono]
opt-level = "s"

[profile.release.package.freedesktop-desktop-entry]
opt-level = "z"

[profile.release.package.pipewire]
opt-level = "z"

[profile.release.package.libspa]
opt-level = "z"

[profile.release.package.ureq]
opt-level = "z"

[profile.release.package.ureq-proto]
opt-level = "z"

[profile.release.package.rustls]
opt-level = "z"

[profile.release.package.rustls-native-certs]
opt-level = "z"
```

Leave your own crate at the release default, opt-level 3: do not add
`opt-level = "s"` or `"z"` under `[profile.release]`. Fat LTO
optimises the whole program at the level of the crate that links it,
so a plugin built for size slows the render path with it.

## Schemas for `strand check` and your editor

`strand check` and the language server (`strand-dev lsp`) never load
plugin code. Your binary writes its schema to a file they read:

```sh
cargo build --release
target/release/my-shell --write-schemas
```

It writes `hello.schema` into `$XDG_DATA_HOME/strand/schemas/`
(`~/.local/share/strand/schemas/` by default), prints each path, and
removes the files it wrote earlier for services it no longer has. Files
another binary wrote are listed and left alone. Run it again whenever
the schema changes; a running shell and the language server read the
directory when they start.

The stock `strand` reads these files too, so `strand check` passes a
config that reads `hello.greeting`. If you run the stock `strand` on
such a config, the service has no code behind it: reads give its
defaults, writes and calls are refused, and a notice names the file.
Run your plugin binary instead, or delete the file if you no longer use
that plugin.

## Running it

Run your binary wherever you ran `strand` (your compositor's autostart
line):

```sh
my-shell run
```

The other commands work from either binary, since they talk to the
running shell over its socket: `strand call hello.greet world`, `strand
set hello.count 0`, `strand get` of a value your config exports.

A config uses the service like a builtin one:

```
export let greeting = hello.greeting
bar Top {
  text greeting
  on click { hello.greet("you") }
}
```
