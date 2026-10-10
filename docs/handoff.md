# Handoff: M4 to M5 (updated 2026-10-10)

Where the project stands at M4's close, what the owner still has to
decide, and what was deliberately left for later. Read this first when
picking the repo up again, then `docs/m4-report.md` (and
`docs/m3-report.md`) for the measured numbers, `docs/features.md` for
M5's boxes, and `docs/decisions.md` for the reasoning behind each item
below.

## State

- M0–M4 are complete, but for one M4 box that waits on the owner (the
  bundled noise field). M4's three exits are ticked in features.md:
  smooth 2,000-row scrolling, GPU released when idle, and lock fails
  closed under faults (docs/m4-report.md, "Result").
- `main` holds all of M4: the owner merged `laptop/integration-m4-w3`
  (M4 waves 0–3, the m4 audit's three rounds of fixes and the closing
  integrator's fix) at `3dc3f71` and deleted the merged branches. Its
  measured head is `e765276` (docs/m4-report.md); the commits after it
  change docs, two tests' harnesses and the GPU thread's answer to a
  request that panics (decisions.md m4-integration-w3, closing). The
  fourth to seventh m4 audit rounds' fixes are on `laptop/m4-audit`
  (decisions.md m4-audit); they change runtime code as well as docs
  and tests (docs/m4-report.md's header lists what).
- `docs/features.md`: M0 20/20, M1 58/59, M2 30/30, M3 14/14, M4 17/18,
  M5 0/9 (boxes and exit criteria, counted 2026-10-10). The open M1
  box is the tree-sitter grammar, which M5 owns.
- Budgets at `e765276` (laptop; docs/m4-report.md has CI's): `.text`
  19,006,471 B of 19,398,656 with the GPU backend and 15,634,439 B of
  15,728,640 CPU-only (about 94 KB left: the tightest budget in the
  tree); design.md's bar 34,187 kB against its 34,816 kB target (CI
  measures it over the target, 35,557 kB at `70af44f` and 35,617 kB at
  `e765276`, which warns; the ceiling is 38,912 kB);
  the full shell 42–50 MB against 64 MB.
  After the sixth audit round (`laptop/m4-audit`, decisions.md
  m4-audit round 6) the laptop measures 15,648,775 B CPU-only (about
  80 KB left) and 19,030,727 B with the GPU backend; the design bar
  34,484 kB. After the seventh: 15,653,063 B CPU-only (about 75 KB
  left) and 19,036,167 B with the GPU backend (the bar not measured
  again; round 7 adds no code a mocked bar runs at rest).
- Every build and test runs on the owner's laptop through the container
  suite (`scripts/container/`, CLAUDE.md; decisions.md laptop-container).
  Its wall-clock timing steps are advisory there; GitHub's `timing` job
  is the latency reference (decisions.md laptop-open). The GPU tier runs
  on lavapipe (`run.sh`, CI) and advisorily on the laptop's GPU
  (`scripts/container/gpu.sh`); the lock tier runs in a KVM guest
  (`scripts/container/lockvm.sh`, CI's `lock-vm` job).
- Remote branches: `origin/main` and `origin/laptop/m4-audit` (audit
  rounds 4 to 7, until it is merged). The integration branch and
  the wave-3 branches it merged are deleted.

## Open items for the owner

From M4 (docs/m4-report.md, "Open"; decisions.md m4-gpu-effects and
m4-audit):

1. **The bundled noise field.** design.md counts "aurora and noise
   fields" among the eight bundled GPU effects but names no spelling
   for a noise field. m4-gpu-effects reads noise fields as `.wgsl`
   shaders on the same path; the box stays open until the owner names
   a spelling or accepts that reading.
2. **A hung frame on a presented (`GpuPresent`) surface** is bounded
   only by the WSI's acquire timeout. This does not reach the lock: a
   lock surface is never lent or handed to the GPU thread
   (`strand-surface/tests/session_lock.rs::a_lock_surface_is_never_handed_to_the_gpu`),
   each of its frames holds for the GPU at most `GPU_WAIT` (8 ms)
   before the CPU draws it
   (`strand-render/tests/gpu.rs::a_readback_frame_the_gpu_never_answers_holds_at_most_gpu_wait`),
   and a readback that never ends loses the device after `HUNG_AFTER`
   (10 s).
3. **`theme_swap_bench`'s 8-scope `spring(1600, 1)` gate** has little or
   no headroom on GitHub: 2.53–5.06 ms over fifteen timing jobs against
   5 ms, one failure (run 38050248241). Accept the occasional failure,
   use a larger runner, or make the swap cheaper.
4. **The shape list**: keep the 13 shapes (a reading of design.md,
   recorded with its reasons) or trim them to design.md's five plus
   polygons.
5. **xdg-activation**: the notification server's ActivationToken (spec
   1.2) and the `XDG_ACTIVATION_TOKEN` of app launches moved past M4
   and need a milestone.
6. **The promoted GPU cost on hardware** is unmeasured: ANV cannot
   present on the advisory leg's pixman sway, and presenting needs a
   compositor on a KMS card, which the laptop rules keep out of
   containers. The leg's readback mode passed at `e765276` (PSS 4,723
   kB over the pre-GPU baseline after the drop, bound 6 MiB).
7. **A process-wide cap on hung devices.** A shader file whose pass or
   frame loses the device is never run again in that process (m4-audit
   round 5), but every *edit* of a file that still loops forever is new
   code, and each hangs and leaks one more lavapipe device (a spinning
   CPU thread) after the 30 s retry. Bounding that needs a rule the
   design does not have: for example, no GPU for the rest of the
   process after N lost devices, the CPU drawing everything and
   `shader` nodes nothing. Accept the per-edit cost (a developer's
   loop, one device per save at most every 30 s) or name a cap.

## Owner decisions already answered

The M4 decisions (GPU tests on lavapipe with an advisory hardware leg,
the GPU backend in every build with its own `.text` gate, the bundled
effects' syntax, the `login` PAM fallback, `/dev/kvm` for the lock VM,
`letters` without a positional) were answered on 2026-10-09 and are
under "m4-owner" (summarised in docs/m4-report.md). The four below,
from M3's close, were answered on 2026-10-08 and are under
"laptop-decisions".

1. **Release build settings: signed off.** The 27
   `[profile.release.package]` `opt-level` overrides stay as they are.
   (Since this sign-off the profile changed: m4-integration-w2 moved
   `[profile.release]` to fat LTO, since thin LTO no longer kept `.text`
   within its gates, and built the image and SVG decoders for size; the
   profile now has 40 overrides. `Cargo.toml` is the reference; decisions.md m4-integration-w2.)
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

- Two functional tests still bound wall-clock time, with wide margins:
  `strand-scene/src/tokens.rs::huge_fan_out_fails_fast` (< 500 ms) and
  `strand-dev/tests/lsp.rs`'s hung-bus case (< 1500 ms). Left to their
  crates' owners (decisions.md m4-audit).
- The matrix test helper `compositor_matrix.rs` still drives Hyprland
  through `hyprctl` with the classic syntax (outside `strand-services`);
  that is its own check of the compositor, not the adapter's.
- `theme_swap_bench`'s tightest gated case, 8 scopes on
  `spring(1600, 1)` (whole swap), measured 4.46 ms on GitHub against
  5 ms (about 11% headroom, no retry; decisions.md laptop-ci). Since
  then fifteen timing jobs measured 2.53–5.06 ms, one over the gate
  (run 38050248241; decisions.md m4-audit, round 3): a `timing` failure
  there is most likely runner noise, and the gate is the owner's call. Consolidating test
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
    `strand/src/run/tests.rs::a_save_fixed_at_once_never_opens_the_overlay`
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

### Known limits, recorded and not M4 blockers

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
  from the clicked surface, so the server reports spec 1.1;
  `Notification.time` has no time of day yet. M4 did not take up
  xdg-activation (neither features.md's M4 boxes nor m4-plan.md list
  it), nor the `XDG_ACTIVATION_TOKEN` for app launches (decisions.md
  wave4-a3 said "left for M4"): both move to a later milestone, to
  be scheduled by the owner (decisions.md m4-audit).
- Tray: Activate and ContextMenu get the press's output-logical point
  (`demo/host.rs::a_press_sets_the_tray_click_point`); (0, 0) only before
  the first press and for actions no press caused (`dismiss`, `scroll`,
  `drop`).
- Media: remote (https) art is not fetched.
- Lock: PAM's info and error texts reach the lock only with the verdict,
  inside one 30 s client timeout. A stack with `pam_fprintd` before
  `pam_unix` (30 s default wait) never shows "Place your finger" and
  times out before the password is tried, then shows the fallback,
  which meets the same wall. Streaming interim text needs a protocol
  message and a lock prompt design.md does not have; moved past M4
  (decisions.md m4-audit, round 3). README asks for a password-only
  stack meanwhile.
- Network: `connect()` is awaited inline (bounded: 5 s per settings call,
  25 s for activation); enterprise (802.1X) and WEP networks are refused.
- Audio: `StepVolume` has no caller on the language path. (`spectrum`
  landed in M4 with an FFT on the audio thread.) Without an
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

- tree-sitter grammar for `.strand` (M5). M4 closed the other two:
  `keyframes` playback, `shader` and `canvas` drawing, and the watcher's
  `.wgsl` and wallpaper paths.

## M4 infrastructure

- GPU: lavapipe in the container image and CI, advisory hardware checks
  through `/dev/dri/renderD128` (`scripts/container/gpu.sh`); the GPU
  backend is in every build with the `.text` gate at the measured size
  (decisions.md m4-owner).
- Lock: `scripts/container/lockvm.sh [CMD]` runs CMD as root in a KVM
  guest with real PAM, sway and user `tester` (no CMD: the smoke); the
  owner allowed `/dev/kvm` for that container only (m4-owner). Build
  with `scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh`,
  run `scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh`.
  CI's `lock-vm` job runs the same harness under the runner's KVM.
- The execution plan (streams, waves, owners, tests) is
  `docs/m4-plan.md`; its interface text is in `docs/architecture.md`.

## Handoff checklist

M4's, for the merge of `laptop/integration-m4-w3` (m4-plan.md's
"Closing M4"). M3's checklist is in git history (`f4899c0`).

- [x] Every M4 box ticked with its test, but the noise field (owner);
  the three exits cite their tests and runs (features.md).
- [x] `docs/m4-report.md` written with the figures at `e765276`.
- [x] Budgets at `e765276`: `CI_JOB=budgets scripts/container/run.sh ci`
  passed (both `.text` gates, the bar, the full shell with the launcher
  closed, the idle window); `CI_JOB=timing` passed with
  `reload_latency` warning as usual (token p95 22.2 ms headless; the
  list-scroll and frame-time benches inside their gates).
- [x] The advisory hardware leg at `e765276`: `gpu_idle` in release on
  ANV, readback mode, 4,723 kB over the baseline after the drop.
- [ ] The full local set on the branch's final head, one at a time:
  `scripts/container/run.sh ci`, `scripts/container/matrix.sh`, the lock
  VM (`scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh`,
  then `scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh`,
  which prints LOCK VM SCENARIOS PASSED) and `scripts/container/gpu.sh`;
  then GitHub CI green on that head. The closing integrator runs these
  after the last docs commit, so their results are in its hand-back,
  not here.
- [x] Merge `laptop/integration-m4-w3` to `main` (the owner's step), then
  delete the merged wave branches and the integration branch: `main` is
  at its head `3dc3f71`, and `origin` has only `main` (checked
  2026-10-10 with `git ls-remote --heads origin`).
- [ ] Delete worktree `target/` directories (each agent at its task's
  end; the cargo registry stays in its volumes).
