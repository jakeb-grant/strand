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

**Status:** pre-M0. The workspace is laid out, but nothing renders yet. See
[`docs/design.md`](docs/design.md) for the full design.

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

CI runs the same three on every push. The later tiers in the design's Testing
section (offline render tests, headless sway, mocked D-Bus services, the
reload fuzzer and the memory budgets) are added with the milestones that need
them.
