# Handoff: state after M3 (2026-10-08)

Where the project stands, what the owner still has to decide, and what was
deliberately left for later. Read this first when picking the repo up again,
then `docs/m3-report.md` for the measured numbers and `docs/decisions.md`
for the reasoning behind each item below.

## State

- `main` is at the wave 4 merge (`503967f`): M0, M1, M2 and M3 are complete
  and CI is green on it (run 37821722003, `check` and `compositors`).
- `docs/features.md`: M0 21/21, M1 56/59, M2 30/30, M3 14/14 (exit line
  included), M4 0/17, M5 0/9. The three open M1 boxes are owned by later
  milestones (below).
- Work is paused at the owner's request. The open decisions that blocked
  M4 are settled (below, 2026-10-08).

## Owner decisions (answered 2026-10-08)

All four are answered; each is recorded in `docs/decisions.md` under
"laptop-decisions".

1. **Release build settings: signed off.** The 27
   `[profile.release.package]` `opt-level` overrides stay as they are.
   Caveat: the memory figures hold for builds that read the workspace's
   release profile (`cargo build --release`, `cargo install --path` or
   `--git` of this repository). A registry package, a build as a
   dependency of another workspace, or a distribution's own profile flags
   do not get them. README's "Building and installing" says so.
2. **Reload latency gate: kept.** Still design.md's 35 ms (token) and
   50 ms (markup) at p95, with 50 rounds; the cloud dev container is no
   longer the reference machine. On the laptop, three runs passed with
   1.0–1.6 ms of token headroom. A per-stage profile shows about 2.6 ms
   of work and thread wakes outside the 15 ms coalesce, with no dominant
   stage. A slow run is every stage a little slower plus wake-up tails
   (the laptop was on battery, `powersave`). No code changed.
3. **`shrink: 0` for absolutely sized `image`/`icon`: acknowledged** and
   written into design.md's layout section ("Shrinking"), with the
   percentage carve-out and the CSS replaced-element precedent.
4. **`strand-watch` ancestor watches: stop below `$HOME`.** Under
   `$HOME` only ancestors strictly below it are watched (`~/.config`
   yes, `~` no); outside it, up to the root of the mount holding the
   directory. Done in `strand-watch` (`Options::home`,
   `paths::watched_ancestors`) with tests; design.md's "Watch
   directories, not files" gives the reason.

Settled, do not reopen: the memory gates are owner-confirmed. The
two-monitor bar warns above a 34 MB target and fails above a 38 MB ceiling,
and the full shell warns above 64 MB and fails above 70 MB
(decisions.md wave4-core, commit 607bd10). Agents must not change them.

## Deferred work

### CI

- Split the single `check` job; give the timing gates a lighter release
  profile; consolidate test binaries to cut link time; make
  `theme_swap_bench` robust to runner noise.
- Flakes seen once each, with diagnostics added so a recurrence names its
  cause (none reproduced locally; all in m3-report Open):
  - `strand-services/tests/audio.rs::devices_volume_mute_and_the_default_arrive`
    (default sink not seen within 5 s; now prints `pw-metadata`).
  - `strand/tests/budgets.rs::the_full_shell_on_the_real_services_is_measured`
    (launcher drew no marked icon in the settle second).
  - `strand/src/run.rs::tests::five_save_styles_land_on_a_cold_boot`
    (delete-then-write gap stretched past the 50 ms grace; label now
    carries the gap).
  - `strand-surface/tests/render.rs` lone-toast and toggling-panel pose
    tests (a repeated first frame; tolerated, cause unknown).
  - `strand-render/tests/damage.rs::first_frame_of_a_new_surface_has_its_text`
    (the assertion races the text worker).
  - `strand/tests/demo.rs::demo_bar_on_two_outputs_then_idle` (an unnamed
    thread woke; it now names the thread).

### Verification gaps

- A second monitor on Hyprland, niri and sway is not checked live; the
  compositor matrix covers one output.
- The Hyprland 0.56.2 and niri 26.04 IPC fixtures were rebuilt from source,
  not captured from real sessions. Diff them against `socat` captures.

### Known limits, recorded and not M3 blockers

- No IPC adapter for labwc, COSMIC, wayfire or river: `windows.focused` is
  null and window actions answer `Unsupported` until
  `zwlr_foreign_toplevel_management_v1` is bound as a fallback.
- Notifications: ActivationToken (spec 1.2) needs an xdg-activation token
  from the clicked surface (M4), so the server reports spec 1.1;
  `Notification.time` has no time of day yet.
- Tray: Activate and ContextMenu get position (0, 0) until M4 popup
  placement passes real coordinates.
- Media: remote (https) art is not fetched.
- Network: `connect()` is awaited inline (bounded: 5 s per settings call,
  25 s for activation); enterprise (802.1X) and WEP networks are refused.
- Audio: `DeviceRef::Id` does not carry `object.serial`; when `inotify_init`
  fails the audio thread stays on its 10 s retry timer; `StepVolume` has no
  caller on the language path; the future `spectrum` element needs PCM
  samples as well as peaks.
- Item writes: a stale held write from one handler can land after a newer
  one from another handler in the same throttle window.
- `strand-introspect` opens a new D-Bus connection for each 10 s refresh of
  a `from dbus` check.

### Open M1 boxes owned by later milestones

- `keyframes` playback, `shader` and `canvas` drawing (render work, M4).
- tree-sitter grammar for `.strand` (M5).
- The watcher's `.wgsl`, wallpaper and link-target watching beyond what the
  binary already does (needed once M4 shaders exist).

## Before starting M4

- GPU promotion and the 8 bundled GPU effects need a GPU to test against.
  Neither the dev container nor GitHub's runners have one; decide on a
  software Vulkan driver (lavapipe/llvmpipe) for CI and real-hardware
  checks on the owner's machine.
- The lock screen must be tested in a local QEMU VM with injected faults,
  never on a real session (design.md). The dev container has no KVM, so
  that exit criterion needs a VM-capable runner or the owner's machine.
- The rest of M4 (blur protocols, drag and drop, tray menus, page
  transitions, the effects catalogue, 2,000-row scrolling) can run
  headless as M2 did.

## Handoff checklist

- [ ] CI green on `main`'s head (`gh run list --repo jakeb-grant/strand -L 3`).
- [ ] Local checks from `CLAUDE.md` pass: `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` with `STRAND_REQUIRE_SWAY=1`,
  `STRAND_REQUIRE_DBUS=1` and `STRAND_REQUIRE_PIPEWIRE=1`
  (needs sway, grim, dbus-daemon, python3-dbusmock, pipewire, wireplumber).
- [x] Open decisions 1–4 answered and recorded in `docs/decisions.md`
  (laptop-decisions).
- [ ] Remote branches `wave4/core`, `wave4/exit-ci` and `wave4/wm` deleted.
  Each is fully contained in `main` (0 commits missing as of 2026-10-08).
- [ ] README status, `docs/features.md` and `docs/m3-report.md` still
  agree with the code.
- [ ] `scripts/m3-shots.sh` re-run if the shell's look changed; the images
  in `docs/images/m3-*.png` match `main` as of 2026-10-08.
