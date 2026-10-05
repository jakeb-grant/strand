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

## Checks (run before every commit)

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Headless Wayland for tests

sway, grim and DejaVu fonts are installed in the dev container and in CI
(`.github/workflows/ci.yml`). Start a headless compositor:

```sh
export XDG_RUNTIME_DIR=$(mktemp -d); chmod 700 $XDG_RUNTIME_DIR
printf 'xwayland disable\noutput HEADLESS-1 resolution 1920x1080\n' > $XDG_RUNTIME_DIR/sway.cfg
WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
  sway -c $XDG_RUNTIME_DIR/sway.cfg &
# WAYLAND_DISPLAY=wayland-1; add a monitor: swaymsg create_output
# scale: swaymsg output HEADLESS-2 scale 1.5 ; screenshot: grim out.png
```

Always kill the sway you started. The machine has 4 CPUs; avoid running
more than one heavy `cargo` build at a time per task.
