# Developing Strand

How the repository is laid out, how to build it, and how it is tested.
The user documentation is the site (`site/`, built by `crates/strand-site`,
published at https://strand.jacobgrant.dev). The project's own records
live beside this file:

- `design.md`: behaviour, syntax and budgets, the source of truth;
- `spec.md` and `grammar.md`: the language, precisely;
- `architecture.md`: the crates, their boundaries and interfaces;
- `features.md`: the feature checklist, each box with its test;
- `decisions.md`: one dated paragraph per decision;
- `eval.md`: the animation evaluation sandbox;
- `live-examples.md`: the site's in-browser examples;
- `benchmarks.md`: measured numbers, updated by hand, so they go
  stale (the gates themselves are tests);

## Layout

Each crate is one box in the runtime architecture:

![Runtime architecture](images/architecture.png)

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
| `strand-surface` | Layer-shell and xdg toplevel windows, poses, blur, input, frame timing, session lock | M0, M4 |
| `strand-auth` | The lock screen's PAM helper and the client that spawns it: the lock's security boundary | M4 |
| `strand-fake-wayland` | A fake Wayland compositor for tests (toplevels, workspaces, layer surfaces) | M4 |
| `strand` | The runtime binary and CLI | all |
| `strand-dev` | LSP and inspector, kept out of the runtime binary | M1, M5 |

Beside the workspace, `eval/` is the animation evaluation sandbox: a Cargo
workspace of its own, never part of the shipped binary, that runs a config
on a virtual clock and checks every frame's nodes (docs/eval.md).

## Roadmap

![Roadmap](images/roadmap.png)

Every milestone ends on measurable exit criteria. Usable v0.1 comes at week 24.
M5 closes at week 52 with the language frozen, but it is not the 1.0
release: the owner releases 1.0 after using Strand for a while.

## Building and installing

Most people install a release with the install script
(`scripts/install.sh`, served at `https://strand.jacobgrant.dev/install.sh`;
site/start/install.md says what it does), which needs no Rust. From a
checkout:

```sh
cargo build --release                    # target/release/strand and strand-auth
cargo install --locked --path crates/strand
cargo install --locked --path crates/strand-auth   # the lock's PAM helper
```

A config with a `lock` needs `strand-auth`: run both install commands.
The lock screen checks passwords in that separate helper, the binary of
its own package, so `cargo install --path crates/strand` alone does not
install it (cargo takes one `--path` per install). Installing from the
repository takes both in one command:
`cargo install --locked --git https://github.com/jakeb-grant/strand strand strand-auth`.
Without the helper no password can unlock a `lock`:
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
m4-audit). A fingerprint module ahead of `pam_unix` works: while the
lock is shown PAM's conversation runs, its texts reach the lock as
`auth.message` ("Place your finger on the fingerprint reader") and its
password prompt as `auth.prompt`, so show them in your `lock`
(`if auth.message != "" { text auth.message }`). A finger unlocks with
no password. Once `pam_fprintd` stops waiting (30 s by default;
`timeout=10` on its line shortens that, as Ubuntu's
`pam-auth-update` does) PAM asks for the password, and the reader
listens again only when the lock is woken: press a key or move the
pointer, then touch the reader. A password typed while `pam_fprintd`
waits is tried once it stops waiting, the first key on a lock whose
reader gave up starts that wait again, and after a wrong password PAM
starts over with the reader. Keep `timeout=` under 90 s: a password
typed while the reader waits must be answered within 90 s (three times
the helper's 30 s), so a longer wait leaves only the finger, and the
fallback field, which goes through the same stack, times out too. If
you want the password checked at once, write the `strand` file with
`pam_unix` alone (`auth required pam_unix.so` and
`account required pam_unix.so`).

If strand dies while the session is locked, the compositor keeps it
locked and a restarted strand shows the password field again, so run
it under a supervisor that restarts it. docs/architecture.md
("Threads", the lock's entry) has the systemd user unit. Its
`ExecStart` is `%h/.cargo/bin/strand run`, for the install above;
change it to where strand is if it is installed elsewhere
(`%h/.local/bin/strand run` for the install script, whose link follows
each update). Its
`StartLimitIntervalSec=0` matters: systemd's default start limit
would otherwise stop restarting a strand that keeps dying and leave
the lock with no password field.

Release builds use the workspace's `[profile.release]` in the root
`Cargo.toml`: fat LTO, one codegen unit, `strand` and the code people
wait on (rendering, image and SVG decoding, Wayland dispatch) at
opt-level 3, and 20 per-package `opt-level` overrides that build the
event-rate glue (D-Bus, services, settings parsing) for size
(`docs/decisions.md`, wave4-exitMemory, m4-integration-w2, which moved
to fat LTO, and m5-speed-profile, which re-chose the levels for
speed). Copy the
tables from `Cargo.toml` itself, not from this paragraph. The memory figures in the docs
(the bar's 42 MB target, the full shell's 76 MB) are measured with
exactly these settings and hold for our builds only. Both commands above,
and `cargo install --git` of this repository, read them. A build that
does not read the root manifest's profile tables does not get them: a
crate packaged for a registry (packaging drops workspace profiles),
Strand built as a dependency of another workspace, or a distribution
package that sets its own `CARGO_PROFILE_RELEASE_*`, `opt-level` or LTO
flags. Such a build works, but its code size and memory differ; copy the
profile tables to reproduce ours.

## Releasing

The install script installs one of two kinds of build from GitHub's
releases, both made by CI from the same archive
(`scripts/release/package.sh`: strand, strand-auth, strand-dev and
strand-eval built with the release profile, the licenses and every
dependency's license in `THIRD-PARTY-LICENSES.txt`):

- **edge**: every push to `main` whose every CI job passed replaces the
  build on the `edge` pre-release (`scripts/release/publish.sh`). Its
  archive is `strand-edge-<12 hex digits of the commit>-x86_64-linux.tar.gz`.
  The `edge` tag stays at the commit the pre-release was first made
  from, since moving a tag breaks `git fetch` for anyone who has it.
- **a tagged release**: to release version X,
  1. set `version = "X"` under `[workspace.package]` in `Cargo.toml` and
     in `eval/Cargo.toml`, update both lock files
     (`cargo update -w` and `cargo update -w --manifest-path eval/Cargo.toml`),
     and land that on `main` as usual;
  2. once `main`'s CI has passed on that commit, tag it and push the tag:
     `git tag -a vX -m "Strand X" <commit>` and `git push origin vX`.

  CI runs every job again on the tag; the `package` job fails when the
  tag is not the workspace's version, and the `release` job publishes
  the release `vX`. Its notes say how to install it; edit them on
  GitHub to say what changed.

The archive's layout (one `strand-<version>/` directory holding the four
programs) is what every installer reads, the one on the site and those
attached to older releases alike: keep it, or teach `scripts/install.sh`
both layouts. The programs must not need a newer glibc than
`MIN_GLIBC` in `scripts/install.sh` (package.sh checks); raising it
drops the systems site/start/install.md lists, so change both together.

`scripts/release/ci.sh` runs the `package` job on this machine: the
archive built in the dev container, then the installer against it in
fresh Ubuntu 24.04, Ubuntu 22.04 and Arch containers on the host's
Docker (`scripts/release/smoke.sh`).

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
- `cargo test --release -p strand --test demo` for the 42 MB PSS budget
  (the M0 demo and design.md's bar);
- `cargo test --release -p strand --test acceptance -- --test-threads=1`:
  the four design shells, unchanged, on sway with two outputs against
  the mock services, screenshots compared with
  `crates/strand/tests/refs/acceptance` (`STRAND_UPDATE_REFS=1` rewrites
  them; a mismatch is uploaded from `target/acceptance`);
- `cargo test --profile timing -p strand-render --test theme_swap_bench`
  for the 5 ms theme swap;
- `cargo test --profile timing -p strand --lib reload_latency --
  --test-threads=1` with `STRAND_LATENCY_ROUNDS=50` for the save-to-pixels
  budget;
- the reload fuzzer's short run in a step of its own: `cargo test -p
  strand --lib random_edits_through_five_save_styles --
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
gets a KMS card), and `scripts/container/hyprland-nested.sh <command>` runs
a command against a real Hyprland nested in a headless labwc, with only the
render node, for presenting that only shows there (its monitors are headless,
not DRM). The lock screen's fault matrix runs only in a QEMU
guest with real PAM: `scripts/container/lockvm.sh bash
scripts/lockvm/scenarios/all.sh` after `scripts/container/run.sh bash
scripts/lockvm/scenarios/build.sh` (CI's `lock-vm` job where KVM is
available).

Animation changes are checked by the evaluation sandbox (docs/eval.md):
`scripts/container/run.sh eval check eval/scenarios/curated` runs the
curated scenarios, and `scripts/container/run.sh eval selftest` runs its
whole gate, which CI's `eval` job runs on every push (`CI_JOB=eval` for
`run.sh ci`'s steps 35 to 39). The effect matrix at 60 and 144 Hz and the
mutants run nightly (`.github/workflows/eval-matrix.yml`); a report's
`index.html` scrubs every frame.

`scripts/m0-exit.sh` and `scripts/m2-exit.sh` measure the memory, idle
and damage gates over whole minutes on a release build (the M0 demo and
design.md's bar, and the full shell with the launcher open).
`scripts/m3-shots.sh` takes screenshots of the full shell on the real
services.
