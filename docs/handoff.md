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
- Work is paused at the owner's request. Do not start M4 until the open
  decisions below are settled.

## Open decisions for the owner

1. **Release build settings.** `Cargo.toml` has 27 `[profile.release.package]`
   `opt-level` overrides: event-rate crates are built for size and the
   per-frame and reload paths stay at 3. This is what holds the bar under
   its memory target. The owner has not signed them off.
   (decisions.md wave4-exitMemory and wave4-exitReport review r2)
2. **Reload latency gate headroom.** The token-edit clause of the M1
   latency bench has under 0.2 ms of p95 headroom on the dev container and
   fails about two runs in three there; CI passes it. The size opt-levels
   are not the cause. Either profile the ~19 ms headless token reload or
   decide how the gate should treat runner noise. (m3-report Open)
3. **Layout default to acknowledge.** An `image` or `icon` sized in absolute
   lengths now defaults to `shrink: 0`, which fixed the launcher's icon
   alignment. It is a language-visible default the strand-render owner
   should confirm. (decisions.md wave3-pixels `shrink` note, wave4-exitReport)
4. **`strand-watch` ancestor watches.** The watcher wakes its own thread for
   every name created or removed in any ancestor of a watched directory, up
   to `/` (on a desktop, every atomic save in `~` or `~/.config`). No logic
   wake or frame follows. Decide whether watches above the config root's
   parent are needed. (m3-report Open)

Settled, do not reopen: the memory gates are owner-confirmed. The
two-monitor bar warns above a 34 MB target and fails above a 38 MB ceiling,
and the full shell warns above 64 MB and fails above 70 MB
(decisions.md wave4-core, commit 607bd10). Agents must not change them.

## Deferred work

### CI

- Split the single `check` job; give the timing gates a lighter release
  profile; consolidate test binaries to cut link time; make
  `theme_swap_bench` robust to runner noise.
- The CI flakes, looped in the container on branch `laptop/flakes`
  (decisions.md laptop-flakes; m3-report Open):
  - Fixed at the cause: `strand-services` audio lost `default`
    metadata updates (PipeWire drops them for existing bindings while
    another client's bind is in its handshake); the audio thread now
    reads the metadata again `audio::REREAD` after client churn or a
    cleared default. Reproduced as `a_daemon_restart_reconnects` and
    `peak_meters_run_only_while_asked_for` timeouts (12 of 70 loaded
    runs), 0 of 40 after; CI's
    `devices_volume_mute_and_the_default_arrive` has the same shape but
    did not reproduce itself (0 of 300).
  - Fixed at the cause: `strand-render/tests/damage.rs::first_frame_of_a_new_surface_has_its_text`
    (raced the text worker; the test holds the worker now).
  - Fixed: `strand/src/run.rs::tests::five_save_styles_land_on_a_cold_boot`
    (a save missing past the 50 ms removal grace is a real removal; such
    a round is excused, `STRAND_SAVE_GAP_MS` reproduces it).
  - Cause found, test fixed: the `strand-surface/tests/render.rs`
    lone-toast and toggling-panel pose tests. A repeated first value is
    a new surface's first two frames sampled for `now` (no feedback
    yet) a sliver of a refresh apart; loaded stalls also failed the
    toggling panel, which (contrary to this list before) did not have
    the lone toast's tolerance. Both now judge the fade by sample time.
  - Not reproduced, diagnostic added:
    `strand/tests/budgets.rs::the_full_shell_on_the_real_services_is_measured`
    (launcher drew no marked icon; a failure now says whether the icons
    are late or never drawn, and where the magenta is).
  - Not reproduced, hardened: `strand/tests/demo.rs::demo_bar_on_two_outputs_then_idle`
    (its idle window now starts after boot has settled; the woken
    thread is named on failure).

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
- [ ] Open decisions 1–4 answered and recorded in `docs/decisions.md`.
- [ ] Remote branches `wave4/core`, `wave4/exit-ci` and `wave4/wm` deleted.
  Each is fully contained in `main` (0 commits missing as of 2026-10-08).
- [ ] README status, `docs/features.md` and `docs/m3-report.md` still
  agree with the code.
- [ ] `scripts/m3-shots.sh` re-run if the shell's look changed; the images
  in `docs/images/m3-*.png` match `main` as of 2026-10-08.
