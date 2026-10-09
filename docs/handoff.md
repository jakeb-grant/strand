# Handoff: state after M3 (updated 2026-10-09)

Where the project stands, what the owner still has to decide, and what was
deliberately left for later. Read this first when picking the repo up again,
then `docs/m3-report.md` for the measured numbers and `docs/decisions.md`
for the reasoning behind each item below.

## State

- `main` is at `f56aade` ("Merge laptop/labwc into laptop/integration2"),
  fast-forwarded from `laptop/integration2`: M0, M1, M2 and M3 are
  complete. GitHub CI run 37921942812 passed all six jobs on it (`lint`,
  `test`, `budgets`, `acceptance`, `timing`, `compositors`; the
  compositors on sway 1.12, niri 26.04, Hyprland 0.56.2 and labwc
  0.20.2). The previous reference was run 37887410210 on `b87865a`.
- `docs/features.md`: M0 21/21, M1 56/59, M2 30/30, M3 14/14 (exit line
  included), M4 0/17, M5 0/9. The three open M1 boxes are owned by later
  milestones (below).
- Every build and test runs on the owner's laptop through the container
  suite (`scripts/container/`, CLAUDE.md; decisions.md laptop-container).
  Its wall-clock timing steps are advisory there; GitHub's `timing` job
  is the latency reference (decisions.md laptop-open).
- Remote branches: only `origin/main`. The `wave4/*` and `laptop/*`
  branches were all deleted on 2026-10-09.
- Merged since `b87865a` (decisions.md laptop-open, laptop-resilience,
  laptop-media, laptop-labwc):
  - `win.maximize()` and `win.fullscreen()`, toggles beside
    `win.maximized` and `win.fullscreen`, on every adapter and the wlr
    fallback, checked live on all four compositors of the matrix.
  - The compositor adapters degrade to the standard protocols when
    connected to a compositor they cannot understand (a reply of another
    shape, a stream of no events, focus and close refused in every
    dialect). They raise one `ServiceDiagnostic` naming the compositor,
    its version and what was not understood (log and `strand watch`, not
    the overlay), and recover on their own. A later action an older
    compositor cannot parse is only rejected. Hyprland dispatches go out
    in Lua first and fall back to classic on `Invalid dispatcher`.
  - The `compositors` job runs nightly against `archlinux:latest` too;
    its summary tables each compositor's version and result.
  - `media`: a new track's time starts at 0 in the same update as its
    title (the `media_follows_the_active_player_without_polling` flake).
  - The wlr protocol thread waits (500 ms at most) for the compositor to
    read the requests it sent before it stops (the labwc fullscreen
    flake in CI run 37913227852's `compositors` job).

## Owner decisions (answered 2026-10-08)

All four are answered; each is recorded in `docs/decisions.md` under
"laptop-decisions". One decision is open: how M4's GPU work is tested
("Before starting M4").

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

- The matrix test helper `compositor_matrix.rs` still drives Hyprland
  through `hyprctl` with the classic syntax (outside `strand-services`);
  that is its own check of the compositor, not the adapter's.
- `theme_swap_bench`'s tightest gated case, 8 scopes on
  `spring(1600, 1)` (whole swap), measured 4.46 ms on GitHub against
  5 ms (about 11% headroom, no retry; decisions.md laptop-ci). If
  `timing` fails there, look at that case first. Consolidating test
  binaries was measured (about 39 s of linking in all) and not done.
- On the laptop the timing gates warn: `scripts/container/run.sh ci`
  prints WARN for `reload_latency` and `theme_swap_bench` gate misses
  (token p95 about 21 ms headless against a break near 19.9 ms;
  decisions.md laptop-open). The likely cause, the laptop's power state,
  is unconfirmed; `CI_JOB=timing STRAND_STRICT_TIMING=1
  scripts/container/run.sh ci` under a performance power profile would
  confirm it.
- CI flakes seen once and not reproduced (decisions.md laptop-flakes;
  m3-report Open). The fixed ones (the audio `default` read again, the
  damage text race, the cold-boot save gap, the pose tests, the labwc
  fullscreen request, the media track time) are recorded there and in
  decisions.md laptop-labwc and laptop-media.
  - `strand/tests/budgets.rs::the_full_shell_on_the_real_services_is_measured`
    (launcher drew no marked icon). A failure now says whether the icons
    are late or never drawn, and where the magenta is.
  - `strand/tests/demo.rs::demo_bar_on_two_outputs_then_idle`. Its idle
    window now starts after boot has settled; the woken thread is named
    on failure.
  - `strand-watch/src/core.rs::tests::a_file_rewritten_without_pause_is_read_within_max_delay`
    (read before `max_delay`) and
    `strand/src/run.rs::tests::a_save_fixed_at_once_never_opens_the_overlay`
    (the overlay flashed on the formatted save), once each in the `test`
    job of run 37874718084 (attempts 1 and 2; attempt 3 green). 0 of 30
    and 0 of 15 failed in the container.
- `crates/strand/tests/reloads.rs` still allows a bus connection that
  only introspects, though `strand-introspect` now keeps one connection
  per bus; tightening it is left to that file's owner (decisions.md
  laptop-services).

### Verification gaps

- A second monitor is checked live on sway and Hyprland (CI, vkms) by the
  compositor matrix; niri runs on one output (nested winit cannot add
  one), and Hyprland cannot run locally without a KMS card (decisions.md
  laptop-verify). Hyprland's matrix leg is CI's alone.
- The Hyprland 0.56.2 and niri 26.04 IPC fixtures are checked against
  real captures (`*-captured` fixtures; `scripts/capture-hyprland.sh`,
  `scripts/container/capture-niri.sh`). The Hyprland capture lacks a
  second monitor, close, move, fullscreen, reload and special workspaces.
  It was put together from three separate captures with unrecorded gaps
  between them (its SOURCE.txt). A capture from
  `scripts/capture-hyprland.sh` during a busy session would close that
  gap: it reads its replies while the stream is quiet and records in
  `marks.txt` the stream line where each set falls.

### Known limits, recorded and not M3 blockers

- No IPC adapter for labwc, wayfire or river.
  `zwlr_foreign_toplevel_management_v1` is the fallback for windows:
  `windows.focused`, `minimized`, `maximized`, `fullscreen` and
  `win.focus()`/`close()`/`minimize()`/`maximize()`/`fullscreen()` work
  on labwc (checked live in the compositor matrix) and should on wayfire
  and river (not run here). Still limited: no window has a `workspace`
  without an adapter (no standard protocol relates the two); the wlr
  protocol has no identifier, so a window's `ext-foreign-toplevel-list`
  handle (M4 thumbnails) is joined by app id and title and is missing
  while twins disagree; `wm.config_reloaded` never fires without IPC;
  and the design bar's pixel test does not run on labwc (only the
  stores and window-state tests do).
- niri: its IPC has no window state, so `maximized` and `fullscreen`
  come from the wlr toplevels, matched by app id (twins by app id and
  title in order). Twins whose counts disagree get neither state rather
  than a guess (decisions.md laptop-open, winstate).
- sway: `win.maximize()` answers `Unsupported` on the adapter (sway has
  no maximize). Through the wlr fallback (adapter off) the reply is
  `Ok` though sway ignores `set_maximized`; the wlr protocol cannot tell.
- Notifications: ActivationToken (spec 1.2) needs an xdg-activation token
  from the clicked surface (M4), so the server reports spec 1.1;
  `Notification.time` has no time of day yet.
- Tray: Activate and ContextMenu get position (0, 0) until M4 popup
  placement passes real coordinates.
- Media: remote (https) art is not fetched.
- Network: `connect()` is awaited inline (bounded: 5 s per settings call,
  25 s for activation); enterprise (802.1X) and WEP networks are refused.
- Audio: `StepVolume` has no caller on the language path; the future
  `spectrum` element needs PCM samples as well as peaks. Without an
  inotify instance the audio thread reconnects on its 10 s timer (no other
  unprivileged signal exists) and makes its socket watch again as soon as
  inotify gives one.
- Audio: an item kept past its device leaving is told apart from the
  device that reused its id by `node.name` (`Write::held`); a device
  replugged under its freed id with the same node name counts as the same
  device.
- Audio: the metadata is read again `audio::REREAD` (250 ms) after client
  churn or a cleared default, because PipeWire drops `default` updates
  to existing bindings during another client's bind. Reviewed by the
  owner and kept, with no upstream report (decisions.md laptop-open).

Out of scope, not a limit (owner's decision, 2026-10-08; decisions.md
laptop-open): COSMIC, a full desktop with its own shell; people who build
a custom shell run bare compositors. No adapter or test for it is
planned. Strand still runs there; with only `ext-foreign-toplevel-list`,
`windows.focused` stays null and window actions answer `Unsupported`.

### Open M1 boxes owned by later milestones

- `keyframes` playback, `shader` and `canvas` drawing (render work, M4).
- tree-sitter grammar for `.strand` (M5).
- The watcher's `.wgsl`, wallpaper and link-target watching beyond what the
  binary already does (needed once M4 shaders exist).

## Before starting M4

- GPU promotion and the 8 bundled GPU effects need a GPU to test against.
  GitHub's runners have none. The proposal is a software Vulkan driver
  (lavapipe) in the container image and CI, plus real-hardware checks
  through `/dev/dri/renderD128` on the owner's laptop (the only node a
  container may get, CLAUDE.md). **Still the owner's open decision.**
- The lock screen must be tested in a local QEMU VM with injected faults,
  never on a real session (design.md). The laptop can do it: `/dev/kvm`
  there is `crw-rw-rw-` (0666, checked 2026-10-09), so a container given
  `--device /dev/kvm` runs a KVM-accelerated VM as the owner's user with
  no host config change. That needs QEMU in an image of the container
  suite; GitHub's runners would need their own KVM setup to run it in CI.
- The rest of M4 (blur protocols, drag and drop, tray menus, page
  transitions, the effects catalogue, 2,000-row scrolling) can run
  headless as M2 did.

## Handoff checklist

- [x] CI green on `main`'s head (`gh run list --repo jakeb-grant/strand -L 3`):
  run 37921942812 on `f56aade`, all six jobs.
- [x] `scripts/container/matrix.sh` passes locally: sway, niri and labwc
  on 2026-10-09 at `f56aade` (Hyprland skipped: it needs a KMS card;
  CI's vkms leg covers it).
- [ ] `scripts/container/run.sh ci` exits 0 locally (timing steps may
  warn; `STRAND_STRICT_TIMING=1` to enforce them). On 2026-10-09 at
  `f56aade` only its `test` job was run, and it passed; the whole run
  last passed on 2026-10-08 at `b87865a` (laptop/gates:
  `reload_latency` warned). CI's run 37921942812 covers every job.
- [x] Open decisions 1–4 answered and recorded in `docs/decisions.md`
  (laptop-decisions); the audio read again and COSMIC too (laptop-open).
- [x] Remote branches `wave4/*` and `laptop/*` deleted (2026-10-09; only
  `origin/main` remains).
- [x] README status, `docs/features.md` and `docs/m3-report.md` agree
  with the code (audited 2026-10-09 on branch `laptop/cleanup`). Every
  `file.rs::name` citation in the four docs (382) and every backticked
  test or fixture name was checked against the tree; three renamed tests
  (four citations in features.md) are fixed. Stale claims fixed:
  README (Hyprland/sway second output "not yet checked", the
  format-on-save check listed as open, sign-offs owed), m3-report (the
  opt-level overrides as unconfirmed, the latency margin and
  `strand-introspect`'s connection per refresh as open, the CI jobs),
  features.md (the services tier's `check` job). COSMIC appears only as
  out of scope (design.md still names it as an example of a compositor
  with only `ext-foreign-toplevel-list`, and in its blur risk row; neither
  promises support).
- [x] `scripts/m3-shots.sh` not re-run: the shell's look has not changed
  since `docs/images/m3-*.png` (`dbe2104`, 2026-10-08). Since then the
  only render source change is `a595513` (a percentage-sized
  `image`/`icon` may shrink; no fixture sizes one in percent), plus a test
  font (`d43cb46`); the fixtures and the shots' extra component are
  unchanged.
