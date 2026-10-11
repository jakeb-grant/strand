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

**Status: v0.1, M4 (power features) exits met; M5 (developer experience
and 1.0) next.** Where the project stands and what is still open are in
[`docs/handoff.md`](docs/handoff.md); M4's measured exit is in
[`docs/m4-report.md`](docs/m4-report.md), and its boxes and the tests
behind them in [`docs/features.md`](docs/features.md). M4 landed GPU
promotion (vello_gpu on wgpu, in every build, its device dropped after
30 s idle), shaders, canvas and the bundled GPU effects, the effects
catalogue, lists that mount only their window (2,000 rows scroll
without a gap), drag and drop, directional `pages` transitions,
compositor-animated poses, tray menus, and the lock screen on
`ext-session-lock` with a forked PAM helper (`strand-auth`) that fails
closed, tested in a local QEMU VM with injected faults. Its three exits
are met and every box is ticked (the bundled noise field on the
owner's 2026-10-10 reading: a `.wgsl` shader); the owner items still
open are in handoff.md. M4 is merged to `main`.
M3's exit gates are met (see
[`docs/m3-report.md`](docs/m3-report.md)): every builtin service is
real (`auth` since M4). The portal, cpu, memory, battery
(UPower), brightness (logind), network (NetworkManager), Bluetooth
(BlueZ), the tray (SNI + DBusMenu), media (MPRIS) and the shell's own
notification server run on D-Bus and procfs, audio on PipeWire,
`clock`, `calendar` and `screens` in the host, workspaces, windows and
`wm` on the compositor (our own Hyprland, niri and sway IPC adapters,
`ext-workspace-v1`, `ext-foreign-toplevel-list`, and
`zwlr_foreign_toplevel_management_v1` where no adapter runs), apps on the desktop
entries and icon themes, and `from dbus | file | listen | poll` declare
services without Rust. A service starts on its first reader and stops
5 s after the last one leaves or goes invisible; streams such as a
Wi-Fi scan or audio levels run only while visible. Of the builtins
only `cpu` and `memory` sample on a timer, and only while read on a
visible surface (a `from poll` service polls at its declared
interval); everything else waits on events, so nothing wakes while
nothing changes.
design.md's bar runs on Hyprland, niri and sway (CI's `compositors`
job, which also checks the stores and window actions on labwc, a
compositor with no IPC), takes 100 live reloads without a service
reconnecting, and on
two 2560×1440 monitors with the real services uses about 34 MB in
today's default build, which links the GPU backend (33,731–34,016 kB
when it landed, 34.1–34.5 MB in the M4 integration's laptop budget
runs and 34,187 kB at M4's close; 35,557 and 35,617 kB in CI's runs at `70af44f` and `e765276`,
over the target, which warned; target 34,816 kB, ceiling 38 MiB; docs/m4-report.md, "Budgets";
docs/architecture.md, "`strand-gpu`").
M3's CPU-only build used 31–32 MiB, and with the launcher, two toasts
and the OSD up 44–61 MiB with 12 to 172 desktop entries across the runs
measured then (design.md: 59–64; those figures are M3's, in
docs/m3-report.md). `STRAND_MOCK=desktop` still fills everything with
a mock desktop for tests. What M3 leaves open is listed in
docs/m3-report.md's Open section and docs/handoff.md: a second output is
checked live on sway and Hyprland but not on niri (nested niri has one
output), and Hyprland runs only in CI. On compositors without an IPC
adapter, `zwlr_foreign_toplevel_management_v1` serves `windows.focused`
and the window actions (labwc, wayfire, river). COSMIC, a full desktop
with its own shell, is out of scope; Strand runs there with
`windows.focused` null and no window actions.

M2's exit gates are met too (see
[`docs/m2-report.md`](docs/m2-report.md)): the four example shells of
the design (a bar with a calendar popup, a fuzzy launcher, a
notification stack and a volume/brightness OSD) and its `theme.strand`
run unchanged in `strand run`, laid out (taffy: flex, `split` with a
truly centred middle, grids, lists laid out and painted only where
visible (since M4 logic mounts only a window of rows too), container
queries),
themed (Material 3 palettes from a seed, a wallpaper or Catppuccin and
base16 imports, derived tokens, `set { }` overrides, the portal's dark
mode, accent, contrast and reduced motion) and animated (springs on
colour, layout and transform props, while gradients and `mark_color`
snap; `enter`/`exit` poses, FLIP; theme swaps springing in OKLab in
about 2 ms of work with declared text/background pairs kept above 3:1,
while muted and faint text is not guarded mid-swap). They are tested
on a headless sway with two outputs, driven by clicks, the wheel and
keys against mock services, with screenshots compared to references.
Left for later milestones then: the
rich `tooltip { … }` element (the checker warns), clipboard in `input`,
real background blur (a tint until the compositor blurs), and `strand
toggle` (M5; `strand set launcher.open true` is the same write); the
directional `pages` transitions, mounting only visible list rows and the
`auth` service landed in M4.

`strand run [dir]` compiles your `.strand` files (type checker, bytecode
VM, reactive core), puts the surfaces on every monitor and reloads live
on save (M1, [`docs/m1-report.md`](docs/m1-report.md): a token or markup
edit presented about 18 ms after the save on headless sway, 10,000
random edits through five editor save styles without a panic or a blank
frame). `strand check` reports did-you-mean diagnostics, `strand fmt`
formats, `strand set` writes an exported state or a settings field,
`strand watch` / `strand reload` talk to a running shell, and
`strand-dev lsp` serves diagnostics, completion, hover,
go-to-definition, rename and quick fixes. Still open from M1: the
tree-sitter grammar (M5); M4 closed the render side of `keyframes`,
`shader` and `canvas` and the loader's `.wgsl` and wallpaper paths.
Progress is
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
| `strand-services-macros` | `#[service]`, `#[derive(Store)]`, `#[derive(Data)]`, `#[derive(Call)]` | M3 |
| `strand-services-schema` | The builtin services' schema texts, for the checker and LSP without the service runtime | M3 |
| `strand-icons` | freedesktop icon theme lookup shared by the renderer and the apps service | M3 |
| `strand-introspect` | D-Bus introspection that checks no-code `from dbus` services | M3 |
| `strand-text` | parley shaping and per-scale glyph atlases on a worker thread | M0 |
| `strand-scene` | Shared vocabulary: ids, geometry, colour, damage, the scene protocol and `Painter` | M0 |
| `strand-theme` | Palettes: Material 3 roles, `material(seed:/image:)`, importers, the contrast guard | M2 |
| `strand-render` | Springs, tokens, layout, damage, vello_cpu; lowering to the GPU, effects, promotion | M0, M2, M4 |
| `strand-gpu` | The GPU thread: one wgpu device, vello_gpu frames, shader passes, readback and WSI presents | M4 |
| `strand-surface` | Layer-shell, poses, blur, input, frame timing, session lock | M0, M4 |
| `strand-auth` | The lock screen's PAM helper and the client that spawns it: the lock's security boundary | M4 |
| `strand-fake-wayland` | A fake Wayland compositor for tests (toplevels, workspaces, layer surfaces) | M4 |
| `strand` | The runtime binary and CLI | all |
| `strand-dev` | LSP and inspector, kept out of the runtime binary | M1, M5 |

## Roadmap

![Roadmap](docs/images/roadmap.png)

Every milestone ends on measurable exit criteria. Usable v0.1 comes at week 24
and 1.0 at week 52.

## Building and installing

```sh
cargo build --release                    # target/release/strand and strand-auth
cargo install --locked --path crates/strand
cargo install --locked --path crates/strand-auth   # the lock's PAM helper
```

A config with a `lock` needs `strand-auth`: run both install commands.
The lock screen checks passwords in that separate helper, the binary of
its own package, so `cargo install --path crates/strand` alone does not
install it (cargo takes one `--path` per install, so there is no single
command for both). Without it no password can unlock a `lock`:
`strand check` reports an error (`check::lock_no_helper`), and
`strand run` warns at start, in its log and to `strand watch`, and still
locks when asked (failing closed), leaving a TTY as the only way out.
strand looks for it beside its own executable (`~/.cargo/bin` after the
commands above, `target/release` in the build tree), then in
`/usr/libexec/strand`, `/usr/lib/strand` and `/usr/local/libexec/strand`.
A package installs it at one of those.

The helper authenticates with the PAM service `strand`, read from
`/etc/pam.d/strand`. Without that file, or when it cannot be read, it
uses `login` and says so once
(design.md, "Lock screen"). A minimal file includes the system's usual
stack: on Debian and Ubuntu (the lock VM's file)

```
@include common-auth
@include common-account
```

and on Arch and Fedora `auth include system-auth` and
`account include system-auth`. A file in `/usr/lib/pam.d` counts
only where the system keeps its own `login` there (decisions.md,
m4-audit). Keep the lock's stack to passwords: the lock shows PAM's
messages only with the verdict and gives a check 30 s, so a fingerprint
module ahead of `pam_unix` (`pam_fprintd`, whose default wait is 30 s)
times out before the password is tried. If the system's stack includes
one, write the `strand` file with `pam_unix` alone
(`auth required pam_unix.so` and `account required pam_unix.so`).

If strand dies while the session is locked, the compositor keeps it
locked and a restarted strand shows the password field again, so run
it under a supervisor that restarts it. docs/architecture.md
("Threads", the lock's entry) has the systemd user unit. Its
`ExecStart` is `%h/.cargo/bin/strand run`, for the install above;
change it to where strand is if it is installed elsewhere. Its
`StartLimitIntervalSec=0` matters: systemd's default start limit
would otherwise stop restarting a strand that keeps dying and leave
the lock with no password field.

Release builds use the workspace's `[profile.release]` in the root
`Cargo.toml`: fat LTO, one codegen unit, and 40 per-package `opt-level`
overrides that build the event-rate crates (D-Bus, sockets, parsing,
image decoders) for size and keep the per-frame paths at 3
(`docs/decisions.md`, wave4-exitMemory, laptop-decisions and
m4-integration-w2, which moved to fat LTO because thin LTO no longer kept
`.text` within its gates, and built the image and SVG decoders for
size). Copy the
tables from `Cargo.toml` itself, not from this paragraph. The memory figures in the docs
(the bar's 34 MB target, the full shell's 64 MB) are measured with
exactly these settings and hold for our builds only. Both commands above,
and `cargo install --git` of this repository, read them. A build that
does not read the root manifest's profile tables does not get them: a
crate packaged for a registry (packaging drops workspace profiles),
Strand built as a dependency of another workspace, or a distribution
package that sets its own `CARGO_PROFILE_RELEASE_*`, `opt-level` or LTO
flags. Such a build works, but its code size and memory differ; copy the
profile tables to reproduce ours.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs these on every push, with headless sway, grim and DejaVu fonts
installed so the Wayland integration tests and the M0 demo run (set
`STRAND_REQUIRE_SWAY=1` to make a missing sway fail instead of skip), in
five jobs side by side: `lint` (fmt and clippy), `test` (the debug
workspace tests, the 100 reloads and the short fuzzer run), `budgets`
(demo, services and budgets on the release profile), `acceptance` and
`timing` (the theme swap and latency benches on the `timing` profile:
release's opt-levels without link-time optimisation).
`scripts/container/run.sh ci` runs every step in order in an image that
matches CI (`CI_JOB=NAME` for one job). The steps:

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
- `cargo test --profile timing -p strand-render --test theme_swap_bench`
  for the 5 ms theme swap;
- `cargo test --profile timing -p strand --bin strand reload_latency --
  --test-threads=1` with `STRAND_LATENCY_ROUNDS=50` for the save-to-pixels
  budget;
- the reload fuzzer's short run in a step of its own: `cargo test -p
  strand --bin strand random_edits_through_five_save_styles --
  --test-threads=1` (60 edits through the five save styles and the sway
  pipeline, on tmpfs) with `STRAND_FUZZ_MAX_GAP_MS=20`, a delete-to-create
  gap of at most 20 ms.

A nightly job runs the fuzzer for 10,000 edits (`STRAND_FUZZ_EDITS`,
`STRAND_FUZZ_SEED`), the instance-level fuzzers long, and the latency
benches for 200 edits per kind. The services tier runs every push: python-dbusmock's
UPower, NetworkManager, BlueZ, logind and notification daemon and small zbus mocks on a
private `dbus-daemon`, and a private PipeWire with WirePlumber and a null
sink (`STRAND_REQUIRE_DBUS=1` and `STRAND_REQUIRE_PIPEWIRE=1` make a
missing tool fail instead of skip; locally `STRAND_DBUSMOCK_PYTHON`
names an interpreter that imports `dbusmock`). M3's exit gates have
steps of their own: `cargo test -p strand --test reloads` (100 live
reloads, no reconnects), `cargo test --release -p strand --test
services` (the real services idle) and `cargo test --release -p strand
--test budgets` (memory and idle wakeups of design.md's bar and the full
shell on the real services). The `compositors` job runs
`crates/strand/tests/compositor_matrix.rs` on sway, niri, Hyprland and
labwc in an Arch Linux container (`scripts/compositor-matrix-ci.sh`), on
every push and nightly against `archlinux:latest`;
`scripts/container/matrix.sh` runs it locally (Hyprland only where it
gets a KMS card). The lock screen's fault matrix runs only in a QEMU
guest with real PAM: `scripts/container/lockvm.sh bash
scripts/lockvm/scenarios/all.sh` after `scripts/container/run.sh bash
scripts/lockvm/scenarios/build.sh` (CI's `lock-vm` job where KVM is
available).

`scripts/m0-exit.sh` and `scripts/m2-exit.sh` measure the memory, idle
and damage gates over whole minutes on a release build (the M0 demo and
design.md's bar, and the full shell with the launcher open).
`scripts/m3-shots.sh` takes the M3 report's screenshots of the full
shell on the real services.
