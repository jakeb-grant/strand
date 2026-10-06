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

**Status: v0.1.** M2's exit gates are met (see
[`docs/m2-report.md`](docs/m2-report.md)): the four example shells of
the design (a bar with a calendar popup, a fuzzy launcher, a
notification stack and a volume/brightness OSD) and its `theme.strand`
run unchanged in `strand run`, laid out (taffy: flex, `split` with a
truly centred middle, grids, lists laid out and painted only where
visible (logic still mounts every row until M4), container queries),
themed (Material 3 palettes from a seed, a wallpaper or Catppuccin and
base16 imports, derived tokens, `set { }` overrides, the portal's dark
mode, accent, contrast and reduced motion) and animated (springs on
colour, layout and transform props, while gradients and `mark_color`
snap; `enter`/`exit` poses, FLIP; theme swaps springing in OKLab in
about 2 ms of work with declared text/background pairs kept above 3:1,
while muted and faint text is not guarded mid-swap). They are tested
on a headless sway with two outputs, driven by clicks, the wheel and
keys against mock services, with screenshots compared to references.
design.md's bar on two 2560×1440 monitors uses about 26 MB, does no work
between minute ticks and repaints about 230–750 px² per tick (about
2,700 px² on the two ticks after midnight, when the centred clock moves:
a documented exception, within design.md's 60×20 px per output); the full
shell with the launcher open about 31 MB. Left for later milestones: the
rich `tooltip { … }` element (the checker warns), clipboard in `input`,
the directional `pages` transitions and mounting only visible list rows
(M4), real background blur (a tint until the compositor blurs), and
`strand toggle` (M5; `strand set launcher.open true` is the same write).

`strand run [dir]` compiles your `.strand` files (type checker, bytecode
VM, reactive core), puts the surfaces on every monitor and reloads live
on save (M1, [`docs/m1-report.md`](docs/m1-report.md): a token or markup
edit presented about 18 ms after the save on headless sway, 10,000
random edits through five editor save styles without a panic or a blank
frame). `strand check` reports did-you-mean diagnostics, `strand fmt`
formats, `strand set` writes an exported state or a settings field,
`strand watch` / `strand reload` talk to a running shell, and
`strand-dev lsp` serves diagnostics, completion, hover,
go-to-definition, rename and quick fixes. Real system services (audio,
battery, notifications, apps, tray, workspaces) come in M3; until then
`STRAND_MOCK=desktop` fills them with a mock desktop. Still open from
M1: the tree-sitter grammar, the render side of `keyframes`, `shader`
and `canvas`, a dedicated format-on-save overlay check, the loader's
`.wgsl` and wallpaper module paths and the portal clause of the latency
benchmark. Progress is
tracked in [`docs/features.md`](docs/features.md);
[`docs/design.md`](docs/design.md) has the full design.

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

CI runs these on every push, with headless sway, grim and DejaVu fonts
installed so the Wayland integration tests and the M0 demo run (set
`STRAND_REQUIRE_SWAY=1` to make a missing sway fail instead of skip):

- fmt and clippy as above;
- `cargo test --workspace -- --skip random_edits_through_five_save_styles`
  (the workspace tests, offline render tests included, without the
  reload fuzzer; the acceptance tests run here in debug too, but for the
  OSD's, whose 1.2 s window is release-only);
- `cargo test --release -p strand --test demo` for the 34 MB PSS budget
  (the M0 demo and design.md's bar);
- `cargo test --release -p strand --test acceptance -- --test-threads=1`:
  the four design shells, unchanged, on sway with two outputs against
  the mock services, screenshots compared with
  `crates/strand/tests/refs/acceptance` (`STRAND_UPDATE_REFS=1` rewrites
  them; a mismatch is uploaded from `target/acceptance`);
- `cargo test --release -p strand-render --test theme_swap_bench` for
  the 5 ms theme swap;
- `cargo test --release -p strand --bin strand reload_latency --
  --test-threads=1` with `STRAND_LATENCY_ROUNDS=50` for the save-to-pixels
  budget;
- the reload fuzzer's short run in a step of its own: `cargo test -p
  strand --bin strand random_edits_through_five_save_styles --
  --test-threads=1` (60 edits through the five save styles and the sway
  pipeline, on tmpfs) with `STRAND_FUZZ_MAX_GAP_MS=20`, a delete-to-create
  gap of at most 20 ms.

A nightly job runs the fuzzer for 10,000 edits (`STRAND_FUZZ_EDITS`,
`STRAND_FUZZ_SEED`), the instance-level fuzzers long, and the latency
benches for 200 edits per kind. The mocked D-Bus services tier comes with M3.

`scripts/m0-exit.sh` and `scripts/m2-exit.sh` measure the memory, idle
and damage gates over whole minutes on a release build (the M0 demo and
design.md's bar, and the full shell with the launcher open).
