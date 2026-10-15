# Strand

Rust toolkit for Wayland shells. `docs/design.md` is the source of truth for
behaviour, syntax and budgets; `docs/architecture.md` fixes the crate
boundaries and cross-crate interfaces; `docs/features.md` is the feature
checklist every milestone is measured against. If code and those docs
disagree, the docs win unless you update them in the same commit with a
reason.

## Rules

- Stay true to the design doc. Do not invent syntax or drop features; when
  the doc is ambiguous, pick the reading most consistent with its stated
  principles ("does it remove a concept, or add one?") and record the
  decision in `docs/decisions.md` (one dated paragraph per decision).
- Work only inside the crates your task owns. Cross-crate interface changes
  go through `docs/architecture.md`.
- Add third-party dependencies in the owning crate's `Cargo.toml` with an
  explicit version; check the current version with `cargo search <name>` and
  prefer the version the design names when it exists.
- No `unwrap()`/`expect()` on runtime paths that handle external input
  (files, Wayland, D-Bus). Tests may unwrap.
- No `todo!()` in committed code. Unimplemented paths return a typed error.
- Every feature lands with tests. Rendering lands with offline PNG tests.
- Tick `docs/features.md` boxes for features you finish, with the test that
  proves them.

## Checks

Before every commit, the inner loop:

```sh
scripts/container/run.sh cargo fmt --all
scripts/container/run.sh cargo clippy --workspace --all-targets -- -D warnings
scripts/container/run.sh affected     # the tests of the crates you changed
```

`affected` runs `cargo test` for each workspace crate with a changed file
since the merge base with `main` (committed or not), and for the crates
whose tests read `docs/` when docs changed. Add `--deep` when you change
an interface other crates use (it adds every crate depending on yours),
`--list` to see the choice without running it. While fixing one failure,
rerun that test alone (`run.sh cargo test -p <crate> --test <file>
<name>`), not a suite.

The whole suite, `scripts/container/run.sh ci`, runs once per branch,
before the branch is handed off or merged, and not after each fix. When
it fails at step N: fix it, rerun that test alone, then resume with
`CI_FROM=N scripts/container/run.sh ci` (`affected` and clippy have
checked the fix). Run it from the start again only if the fix touched
what an earlier step checks (formatting, lints, another crate's tests).
The commit that goes to `main` needs one whole run from step 1; that is
the integration's, not each stream's.

Run them, and every build and test, through the container below, never
natively.

## Building and testing (the container suite)

Development happens on the owner's laptop, which runs a live Hyprland
session. Everything builds and tests in Docker images that match CI:

```sh
scripts/container/run.sh cargo test -p strand-watch   # any command
scripts/container/run.sh ci        # every step of CI's check jobs, in order
CI_JOB=timing scripts/container/run.sh ci   # one job: lint, test, budgets,
                                             # acceptance or timing
scripts/container/run.sh shell     # a bash in the image
scripts/container/matrix.sh        # CI's compositors job: sway, niri, Hyprland, labwc
scripts/container/gpu.sh           # advisory real-GPU leg (renderD128 only)
scripts/container/lockvm.sh <cmd>  # a command in the lock VM (KVM); CI's lock-vm job
```

- `run.sh` uses `scripts/container/Dockerfile`: Ubuntu 24.04 with exactly
  CI's apt packages (sway 1.9, grim, DejaVu core only, Adwaita, dbus,
  python3-dbusmock, PipeWire, lavapipe, libpam0g-dev) and Rust 1.97.0, so
  pixel references match CI; lavapipe is the only Vulkan driver in it.
  It runs as your uid with the checkout at its own path (worktrees
  included), CI's env (`STRAND_REQUIRE_SWAY/DBUS/PIPEWIRE/GPU=1`,
  `RUSTFLAGS=-D warnings`, no debug info), the cores shared among the
  strand builds running (`CARGO_BUILD_JOBS` overrides), and builds into
  `target/container`, with sccache's compile cache in the
  `strand-sccache` volume shared by every worktree (`RUN_SCCACHE=0` turns
  it off). Debug builds optimise dependencies and compile the workspace's
  own crates incrementally. Host `STRAND_*` variables are passed in. Tests
  start their own headless sway, buses and PipeWire inside the container.
- `matrix.sh` builds the matrix test there and runs it in an Arch image
  (`Dockerfile.matrix`). Hyprland gets only `/dev/dri/renderD128`; CI's
  vkms card needs modprobe, so Hyprland is skipped when it cannot start
  without a KMS card.
- `gpu.sh` runs a command in the same image with the laptop's real GPU
  (only `/dev/dri/renderD128`) and its own headless sway; failures WARN
  unless `STRAND_STRICT_GPU=1`. `lockvm.sh` boots the lock VM
  (`Dockerfile.lockvm`, only `/dev/kvm` passed in) and runs a command in
  the guest; `LOCKVM_TCG=1` emulates without KVM.
- `ci` treats the wall-clock gates in the timing steps (theme_swap_bench,
  reload_latency, list_scroll_bench) as advisory: when every failure in a step is a gate
  miss (a panic starting "timing gate missed", `gate-misses.sh`), it
  prints WARN with its numbers and the run goes on; any other failure in
  those steps fails. GitHub's `timing` job enforces the gates.
  `STRAND_STRICT_TIMING=1` makes them fail here too. A new wall-clock
  gate starts its message with `GATE_MISS` and is checked once at the
  test's end, after its functional assertions (`Gates` in
  theme_swap_bench, `failed` in bench.rs); a functional assertion never
  does. Memory budgets, pixel tests and the rest stay strict.
- When the Dockerfile changes, the image is rebuilt (tagged by its hash);
  edit it together with `.github/workflows/ci.yml`.

## Laptop rules

- No sudo, no host packages, no host config changes. Docker images,
  containers and named volumes are fine; host Python only via uv/uvx.
- Never touch the live Hyprland session (`WAYLAND_DISPLAY=wayland-1`):
  read-only `hyprctl -j` queries at most, no test clients on it, no
  compositor on the host. Pass only `/dev/dri/renderD128` into containers
  (and `/dev/kvm` into the lock VM container only), never `/dev/dri/card*`.
- Lock-screen code runs only inside the lock VM
  (`scripts/container/lockvm.sh`), never against a host or container
  compositor outside it.
- Run one full-workspace test run at a time, and at most three worktrees
  build at once. Delete your worktree's
  `target/` when your task ends; the cargo registry stays in the
  `strand-cargo-registry` and `strand-cargo-git` volumes, compiled
  dependencies in `strand-sccache`.
- Commit on your own branch and push only that branch after each commit;
  never push `main`, never force-push.
