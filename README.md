# Strand

A Rust toolkit for Wayland shells. You write bars, launchers, OSDs and
notification stacks in one small typed declarative language. Everything
animates by default, nothing runs when nothing changes, and saving a file is a
state change, not a restart.

```
bar Top {
  edge: top; height: 32
  split {
    start  { text windows.focused?.title ?? "" }
    center { text clock.format("%H:%M") }
    end    { text pct(battery.percent) }
  }
}
```

That file is already on every monitor, reactive, themed and animated. It wakes
once a minute.

**Status:** M0 spike done; M1's exit gates are met (see
[`docs/m1-report.md`](docs/m1-report.md)). `strand run [dir]` compiles your
`.strand` files (type checker, bytecode VM, reactive core), puts the
surfaces on every monitor and reloads live on save: a token or markup
edit is presented by headless sway about 18 ms after the save (p95; about
34 ms with a 60 Hz monitor's vblank wait added), with state kept as the
design's edit table says, a broken save held back behind an error
overlay, and 10,000 random edits through five editor save styles on two
screens without a panic, an intermediate or blank frame (checked on the
scene and in offline-rendered pixels) or a leaked surface. `strand check` reports did-you-mean diagnostics, and
`strand watch` / `strand reload` talk to a running shell. Still open in
M1: the formatter, the tree-sitter grammar and the LSP (`strand-dev`),
and the parts of `fn`/`keyframes`/`shader`/`canvas` that are render work;
layout, services and theming come in M2 and M3. Progress is tracked in
[`docs/features.md`](docs/features.md); [`docs/design.md`](docs/design.md)
has the full design. (M0: `strand run --demo`, about 21 MB PSS on two
monitors; [`docs/m0-report.md`](docs/m0-report.md).)

## Layout

Each crate is one box in the runtime architecture:

![Runtime architecture](docs/images/architecture.png)

| Crate | Role | Milestone |
| --- | --- | --- |
| `strand-watch` | inotify directory watches, portal, outputs, compositor events, IPC | M1 |
| `strand-compiler` | Parser, type checker, bytecode, reconciler; shared by runtime, `strand check` and LSP | M1 |
| `strand-core` | Reactive graph on the logic thread: signals, state, handlers, timers | M0 bench, M1 |
| `strand-services` | Lazy, refcounted services over zbus, PipeWire and compositor IPC | M3 |
| `strand-text` | parley shaping and per-scale glyph atlases on a worker thread | M0 |
| `strand-render` | Springs, tokens, layout, damage, vello_cpu or GPU | M0, M2 |
| `strand-surface` | Layer-shell, poses, blur, input, frame timing | M0 |
| `strand` | The runtime binary and CLI | all |
| `strand-dev` | LSP and inspector, kept out of the runtime binary | M1, M5 |

## Roadmap

![Roadmap](docs/images/roadmap.png)

Every milestone ends on measurable exit criteria. Usable v0.1 comes at week 24
and 1.0 at week 52.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same three on every push, with headless sway, grim and DejaVu
fonts installed so the Wayland integration tests and the M0 demo run (set
`STRAND_REQUIRE_SWAY=1` to make a missing sway fail instead of skip), plus
`cargo test --release -p strand --test demo` for the 34 MB PSS budget and
`cargo test --release -p strand --bin strand reload_latency --
--test-threads=1` for the save-to-pixels budget. Offline render tests and a
short run of the reload fuzzer (60 edits through the five save styles, on
tmpfs) are part of plain `cargo test`; a nightly job runs the fuzzer for
10,000 edits (`STRAND_FUZZ_EDITS`, `STRAND_FUZZ_SEED`) and the latency bench
for 200 edits per kind. The mocked D-Bus services tier comes with M3.
