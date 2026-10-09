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

- Done on `laptop/ci` (decisions.md laptop-ci): the `check` job is split
  into `lint`, `test`, `budgets`, `acceptance` and `timing`; the timing
  gates build on a `timing` profile (release without LTO); `theme_swap_bench`
  gates the median of more swaps, measured once. Its tightest gated
  case, 8 scopes on `spring(1600, 1)` (whole swap), measured 4.46 ms on
  GitHub against 5 ms (about 11% headroom, no retry); if `timing` fails
  there, look at that case first. Consolidating test binaries was
  measured (about 39 s of linking in all) and not done.
- The CI flakes, looped in the container on branch `laptop/flakes`
  (decisions.md laptop-flakes; m3-report Open):
  - Fixed at the cause: `strand-services` audio lost `default`
    metadata updates (PipeWire drops them for existing bindings while
    another client's bind is in its handshake); the audio thread now
    reads the metadata again `audio::REREAD` after client churn or a
    cleared default (the replayed keys replace the ones held, so a lost
    clear comes back too;
    `a_lost_default_update_comes_back_on_a_read_again` loses a set and a
    clear on purpose). Reproduced as `a_daemon_restart_reconnects` and
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
  - Seen once each on `laptop/ci`'s GitHub `test` job, not reproduced:
    `strand-watch/src/core.rs::tests::a_file_rewritten_without_pause_is_read_within_max_delay`
    (`t0.elapsed() >= max_delay` failed: read before `max_delay`) and
    `strand/src/run.rs::tests::a_save_fixed_at_once_never_opens_the_overlay`
    (the overlay flashed on the formatted save), one each in the `test` job
    of run 37874718084 (attempts 1 and 2; attempt 3 green). 0 of 30 and 0
    of 15 failed in the container; `laptop/ci` changed no code either
    test runs (a doc comment in run.rs only).

### Verification gaps

- A second monitor is checked live on sway and Hyprland (CI, vkms) by the
  compositor matrix; niri runs on one output (nested winit cannot add
  one), and Hyprland cannot run locally (decisions.md laptop-verify).
- The Hyprland 0.56.2 and niri 26.04 IPC fixtures are now checked against
  real captures (`*-captured` fixtures; `scripts/capture-hyprland.sh`,
  `scripts/container/capture-niri.sh`). The Hyprland capture lacks a
  second monitor, close, move, fullscreen, reload and special workspaces.
  It was put together from three separate captures with unrecorded gaps
  between them (its SOURCE.txt). A capture from
  `scripts/capture-hyprland.sh` during a busy session would close that
  gap: it reads its replies while the stream is quiet and records in
  `marks.txt` the stream line where each set falls.

### Known limits, recorded and not M3 blockers

- No IPC adapter for labwc, COSMIC, wayfire or river. Since branch
  `laptop/toplevel`, `zwlr_foreign_toplevel_management_v1` is the fallback
  for windows: `windows.focused`, `minimized`, `fullscreen` and
  `win.focus()`/`close()`/`minimize()` work on labwc (checked live in the
  compositor matrix) and should on wayfire and river (not run here).
  Still limited: COSMIC offers only `ext-foreign-toplevel-list`, so
  `windows.focused` is null and window actions answer `Unsupported`
  there; no window has a `workspace` without an adapter (no standard
  protocol relates the two); the wlr protocol has no identifier, so a
  window's `ext-foreign-toplevel-list` handle (M4 thumbnails) is joined
  by app id and title and is missing while twins disagree; maximize and
  fullscreen toggles are not language actions (decisions.md
  laptop-toplevel); `wm.config_reloaded` never fires without IPC; and
  the design bar's pixel test does not run on labwc (only the stores
  test does).
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
