# Handoff: M6 wave 1 (updated 2026-10-13)

M6 wave 1 (hardening, approved by the owner 2026-10-12) is on `main`
(`5484c4f`): its four streams merged, the integrator's sweep closed what
they left, three intermittent CI failures found while verifying the
head were fixed at their causes, and the laptop's full set and GitHub CI
(the nightly job included) passed on that commit before the
fast-forward. Read this first when picking the repo up cold, then
`docs/features.md` ("M6 Hardening"), `docs/decisions.md` (the
`## m6-*` sections; `## m6-integration-w1` holds the merges and the
sweep), and for M5's numbers `docs/m5-report.md`.

## State after M6 wave 1

- **M0–M5 are complete** (features.md: M0 20/20, M1 59/59, M2 30/30,
  M3 14/14, M4 18/18, M5 12/12, the newcomer exit ticked 2026-10-13 by
  Q15's fallback (ii)). **M6 Hardening** has
  ten boxes, all ticked.
- **Branches.** `main` is at `5484c4f`, fast-forwarded from M5's close
  (`361296e`) to `laptop/m6-w1`, which held wave 1: S-services
  (`laptop/m6-services`, fast-forwarded), S-edges (`laptop/m6-edges`),
  S-lock (`laptop/m6-lock`) and S-flakes (`laptop/m6-flakes`), merged in
  that order, then the integrator's sweep and the three verification
  fixes (`laptop/m6-matrixflake`, `m6-scaleflake`, `m6-fuzzflake`).
  Every one of those branches and worktrees is deleted, locally and on
  the remote; the remote holds only `main`.
- **What wave 1 did** (decisions.md m6-services, m6-edges, m6-lock,
  m6-flakes, m6-integration-w1): remote media art fetched and cached;
  enterprise and WEP joins; `Notification.time` with its time of day;
  `StepVolume` removed; a drop's `at` exact in a windowed `list` (the
  sweep made it exact for every row shape: logic works out a hidden
  row's `drag:` values from its item); inspector write-back by
  `renameat2(RENAME_EXCHANGE)`; PAM's texts and prompts in the lock
  (`auth.message`, `auth.prompt`), and in the sweep in the built-in
  fallback too; the CI flakes fixed at their causes; the compiler's
  provisional service stubs brought level with the service schemas.
- **Budgets re-based** in wave 1: remote media art links an HTTPS stack
  (ureq, rustls), so both `.text` gates moved to 19,922,944 B (CPU-only)
  and 23,855,104 B (GPU), measured plus 64 KiB rounded up to 256 KiB
  (decisions.md m6-integration-w1, the S-services merge); after the
  fast-forward the owner asked for more room, and they are now
  20,447,232 B and 24,379,392 B, measured on `main` plus 512 KiB rounded
  up to 256 KiB (decisions.md m6-owner). On M6 wave 2's merged tree
  (`laptop/m6-w2`) the surface stream's box graph took about 181 KB of
  each, past both, and they were re-based by the same rule to
  21,233,664 B and 25,165,824 B (decisions.md m6-owner, 2026-10-14).
- **Checks.** Each merge and the sweep ran fmt, workspace clippy and
  the touched crates' tests in the container, and the lock VM's
  `scenarios/all.sh` where lock code changed (LOCK VM SCENARIOS PASSED
  on the sweep's lock change). The close measured `.text` at `067711a`
  with `scripts/size-report.sh` (inside both gates, below) and ran the
  release budgets test there (11 passed, 1 ignored); the lock VM and
  the strict hardware GPU leg passed at `692dcc1`. On the final head
  `5484c4f`: `run.sh ci` passed all 32 steps (only `reload_latency`'s
  advisory laptop WARN), `matrix.sh` passed sway, niri and labwc, and
  GitHub CI passed every job (push run 38327195975; dispatched run
  38329258170 with the nightly job).
- **Verification fixes** (decisions.md m6-integration-w1, the
  2026-10-12 verification paragraphs), found as intermittent GitHub
  failures on the head and fixed before the fast-forward:
  the matrix's "the wallpaper never showed" (the launch test's helper
  app outlived its test and its window covered the read pixel; the
  test now ends it), `strand report`'s "not two painted bars" (a real
  bug: on sway 1.9 a surface made just before its output's scale
  changed never heard the new scale and stayed blurry;
  `strand-surface` now re-derives an unshown surface's scale from its
  output), and the nightly fuzzer's overlay check (the check credited a
  held-back save with ending the hold; Strand was right, and the check
  now measures the hold on the logic thread against 250 ms).
- **What remains of wave 1's scope:** nothing. The owner-run Hyprland
  capture, the one hardening item an agent could not do here, was taken
  on 2026-10-12 (open item 3).

## Next

1. **No owner items are open** (below, all done).
2. **Later M6 waves** are the owner's to plan; candidates are the known
   limits below and m5-report.md's "What remains". The language is
   frozen: a change to grammar.md or spec.md needs a decisions.md
   paragraph citing spec.md 14's compatibility policy.

## Open items for the owner

Each has its paragraph in decisions.md (m6-owner holds the 2026-10-12
rulings).

1. ~~**The newcomer exit criterion (Q15)**~~: done 2026-10-13. The
   owner asked the integrator to do the newcomer run, so Q15's fallback
   (ii) carries it: the automated newcomer test and one agent-proxy
   session, which built bar (a) from the guide alone and scored 9/9 on
   two outputs (m5-report.md, "Human trials"). The guide now says where
   the `strand` binary comes from (decisions.md m6-owner).
2. ~~**The `.text` gates**~~: done 2026-10-12. More room to run, as the
   owner asked: measured on `main` plus a 512 KiB reserve, rounded up to
   256 KiB, so 20,447,232 B (CPU-only) and 24,379,392 B (GPU), about
   745 and 760 KB of headroom (it was about 220 KB). This also settles
   M5's close-gate question, whose values are superseded (decisions.md
   m6-owner). Re-based again 2026-10-14 by the same rule on M6 wave 2's
   merged tree: 21,233,664 B and 25,165,824 B.
3. ~~**The Hyprland IPC capture**~~: done 2026-10-12. The owner ran
   `scripts/capture-hyprland.sh 300` on three monitors; scrubbed, it is
   `tests/fixtures/hyprland-0.56.2-captured-2`, replayed by two adapter
   tests (decisions.md m6-owner).

Answered and standing: the owner deferred `strand report`'s grim
reference PNGs to the integrator (2026-10-11), who made them,
deterministic on the frozen-clock acceptance mock (m5-fixups); wave 2's
overruns (accepted 2026-10-11), S-ipc's `Request(s json)` alternative
(not taken), the presented GPU leg's after-drop bound (a leak
tripwire), the `gpu-measure.sh` KMS-card run (not needed, Q19 (A)).
On 2026-10-12 (decisions.md m6-owner): remote media art's HTTPS stack
stays in-process (the system-`curl`, feature-flag and `http://`-only
alternatives are not taken); M5 wave 3's `.text` overruns (S-platform
+94,272 B, S-inspect +35,008 B) are accepted; M5 wave 3's interface
changes (S-fixups' drag identity and `drawn`/`unchanged`, S-inspect's
inspect protocol with `inspect.node`'s `layout`, `files` and `why`,
S-platform's handler input context and token types) and M6 wave 1's
(`NodeEvent::Drop`'s `hidden`, `strand-dev`'s write-back hooks, helper
protocol version 2 with `auth.message`/`auth.prompt`,
`strand-introspect`'s drop log, `Client::submit_saying`,
`lock_fallback::paint`'s `message`) are accepted as architecture.md
gives them; the fast-forward of `main` is done (`5484c4f`).
Every M4 owner item was answered on 2026-10-10.

## Budgets now

`.text` on `laptop/m6-w2` at `209eafe` (M6 wave 2's merged tree, its
final pass); memory at M6 wave 1's close at `067711a` (in the container; M5's figures at
`672b114` are in m5-report.md). Size and memory gates are regression
tripwires, never caps on a user's shell: growth past them is read by
its cause and re-based or cut with a recorded reason.

- **`.text`** (`scripts/size-report.sh`): CPU-only **20,533,655 B** of
  the **21,233,664 B** gate (700,009 B of headroom; +830,656 B over
  wave 1 on `main`, 19,702,999 B, about 181 KB of it the surface
  stream's box graph over `4eac7ac`); GPU **24,451,607 B** of
  **25,165,824 B** (714,217 B of headroom; +830,976 B over `main`'s
  23,620,631 B). M5's close was 18,711,127 and 22,630,295 B. The gates
  were re-based here by the owner's rule, measured plus a 512 KiB
  reserve rounded up to 256 KiB (decisions.md m6-owner, 2026-10-14).
- **Memory** (CI tripwires set by judgement, decisions.md
  m5-open-budgets): the design bar warns above 42 MB (43,008 kB) and
  fails above 48 MB (49,152 kB): 38,201 kB in the container at
  `067711a` (M5: 37,328 kB; 38,912 kB on GitHub). The full shell warns
  above 76 MB (77,824 kB) and fails above 86 MB (88,064 kB): 52,984 kB
  (4 entries) and 68,585 kB (161) in the container at `067711a` (M5:
  51,944 and 62,204 kB; GitHub at M5: 66,422 kB (15) and 68,070 kB
  (172)). The 161-entry figure is mostly launcher icon and buffer
  memory, under the warn line; read it against GitHub's at the next
  CI run.
- **The promoted GPU surface on hardware** (reported, never gated):
  Intel PTL presenting to a GLES2 sway on renderD128: frame p50
  1.09–1.10 ms, p95 3.87–4.18 ms, p99 4.53–5.08 ms; PSS +21.0–26.1 MB
  while promoted, +6.35 MB after the drop (bound 12 MiB).
- **Latency** (GitHub's timing job enforces; advisory on the laptop),
  CI at `672b114` (not re-measured in wave 1; GitHub's timing job on the
  pushed head is the check): reload token p95 16.2 ms headless against a break
  of 19.3 ms (design.md's 35 / 50 ms at its vblank model: 31.9 / 32.1
  ms); theme_swap_bench's 8-scope `spring(1600, 1)` whole swap 2.43 ms
  against 5 ms; list scroll p95 1.07 ms (60 Hz) and 0.99 ms (144 Hz)
  against 3.5 ms, 0 stalled frames.

## CI notes

- GitHub's jobs: lint, test, budgets, acceptance, timing, compositors,
  lock-vm (nightly is scheduled). `scripts/container/run.sh ci` runs the
  check jobs' steps in order; `CI_JOB=<job>` runs one.
- Tests in the `strand` crate's library (`run`, `bench`, `fuzz`) need
  `--lib` since 0b's lib/bin split; `-p strand --bin strand <filter>`
  runs 0 tests and passes. `scripts/container/tests-ran.sh` wraps the
  gated commands and fails a step that runs none;
  `scripts/container/test-tests-ran.sh` also fails if README.md or
  features.md cite the `--bin` form.
- The reload-latency gate is enforced on GitHub again from `94fe372`
  (it ran 0 tests between 0b and then); the first run passed.
- On the laptop the wall-clock gates (`reload_latency`,
  `theme_swap_bench`, `list_scroll_bench`) print WARN on a miss
  (`gate-misses.sh`); `STRAND_STRICT_TIMING=1` makes them fail. The
  laptop's token p95 is about 20–21 ms headless against a break near
  19.9 ms when the machine is otherwise idle. Cause confirmed
  (decisions.md m5-open-reload): the CPU idles down in the 250 ms
  between edits, so every stage of work runs about twice as slow
  (about 4.5 ms against 2.1 ms after the 15 ms coalesce). Pinning to P-,
  E- or low-power cores changes nothing; one busy loop on a core the
  test does not use (`taskset -c 15 sh -c 'while :; do :; done' &`,
  the test on 0–14) makes it pass, 17.9–18.1 ms. The gate is right and
  stays; the last-good cache write (0.5 ms on btrfs) left the reload
  path in the same section.
- The lint job fetches the pinned tree-sitter CLI 0.27.0 (sha256 in
  ci.yml and the Dockerfile; edit both together) and runs
  `check-tree-sitter.sh` (since wave 2 it regenerates S-lsp's `src/`
  from `grammar.js` and fails on a diff) and
  the script tests (`test-size-report.sh`, `test-ledger.sh`,
  `test-gpu-measure.sh`, `test-check-tree-sitter.sh`,
  `test-tests-ran.sh`).
- The budgets job prints the size ledger line in its summary and
  uploads the release binaries as artifact `strand-release-binaries`
  (30 days; for the human trials, not a release).
- `matrix.sh` skips Hyprland locally (no KMS card); Hyprland's leg is
  CI's (vkms), where it runs 0.56.2 with a Lua config. Hyprland settings
  a test changes go through `hyprctl eval 'hl.config({ … })'`, never
  `keyword`, which a Lua config refuses with exit status 0, and are
  checked with `hyprctl -j getoption` (`compositor_matrix.rs`'s
  `Policy::hyprland`; decisions.md m5-integration-w3).
- `gpu.sh` is advisory (WARN unless `STRAND_STRICT_GPU=1`);
  `STRAND_GPU_PRESENT=1` presents on renderD128. Never run
  `scripts/gpu-measure.sh` as an agent.

## M4 owner items (all answered 2026-10-10)

1. **The noise field**: a `.wgsl` shader on the bundled path; its box
   is ticked (m4-owner-2).
2. **A hung frame on a `GpuPresent` surface: accepted**, bounded by the
   WSI's acquire timeout; it never reaches the lock (a lock surface is
   never handed to the GPU thread, each lock frame waits at most
   `GPU_WAIT`, a readback that never ends loses the device after
   `HUNG_AFTER`) (m4-owner-3).
3. **theme_swap_bench's 8-scope gate**: kept at 5 ms with a cheaper
   swap (m4-owner-swap).
4. **The shape list**: all 13 stay (m4-owner-2).
5. **xdg-activation**: scheduled for M5 (features.md's M5 box).
6. **The promoted GPU cost on hardware**: moved to M5 (features.md's M5
   box); presenting works on renderD128 (Q19 (A)), its after-drop bound
   a leak tripwire (m5-speed-gpu-bound); measured and ticked in M5.
7. **Lost devices**: the GPU is off after 3 (`promote::LOST_CAP`)
   (m4-owner-3).
8. **A 4K crossfade**: resolved by a cheaper blend (m4-owner-3).
9. **A lock with no `strand-auth` helper**: warn and lock, fail closed;
   `strand check` reports `check::lock_no_helper` (m4-owner-3).

## Owner decisions already answered

The M4 decisions (GPU tests on lavapipe with an advisory hardware leg,
the GPU backend in every build with its own `.text` gate, the bundled
effects' syntax, the `login` PAM fallback, `/dev/kvm` for the lock VM,
`letters` without a positional) are under "m4-owner" (2026-10-09). The
four below, from M3's close, are under "laptop-decisions" (2026-10-08).

1. **Release build settings: signed off.** The profile has since moved
   to fat LTO, and M5 re-chose its 20 `[profile.release.package]`
   overrides for speed (m4-integration-w2, m5-speed-profile;
   `Cargo.toml` is the reference). The memory
   figures hold for builds that read the workspace's release profile;
   README's "Building and installing" says so.
2. **Reload latency gate: kept** at design.md's 35 ms (token) and 50 ms
   (markup) p95, with 50 rounds.
3. **`shrink: 0` for absolutely sized `image`/`icon`: acknowledged** in
   design.md's layout section ("Shrinking").
4. **`strand-watch` ancestor watches stop below `$HOME`**
   (`Options::home`, `paths::watched_ancestors`).

The memory gates the owner confirmed in M3 (34 / 38 MB for the bar,
64 / 70 MB for the full shell; decisions.md wave4-core, commit 607bd10)
were re-based under the owner's later policy of 2026-10-10, which lets
agents set binary and memory gates by judgement with recorded reasons:
39 / 44 MB and 67 / 74 MB (decisions.md m5-speed), then 42 / 48 MB and
76 / 86 MB with headroom for M5's waves 1-2 (decisions.md
m5-open-budgets). Change them again only with a measurement and a
recorded reason.

## Deferred work

### CI

- `theme_swap_bench`'s tightest gated case, 8 scopes on
  `spring(1600, 1)` (whole swap), measured 2.53–5.06 ms over fifteen
  GitHub timing jobs before m4-owner-swap (one over the gate, run
  38050248241); after it, 4.31 ms median on `laptop/m5-infra` (run
  38106819097) and 2.56 ms at `94fe372` (run 38115583761). Consolidating
  test binaries was measured (about 39 s of linking in all) and not
  done.
- The laptop's timing WARNs: `CI_JOB=timing STRAND_STRICT_TIMING=1
  scripts/container/run.sh ci` under a performance power profile would
  confirm the power-state cause (decisions.md laptop-open).
- The CI flakes seen once (decisions.md laptop-flakes; m3-report Open)
  are resolved in M6 wave 1 (decisions.md m6-flakes):
  - `strand/tests/budgets.rs::the_full_shell_on_the_real_services_is_measured`
    (launcher drew no marked icon): a real race in the image worker. The
    launcher's frames want its icons, then none, then the icons again
    within about 30 ms; a request dropped as unwanted in that gap, and
    wanted again before the render loop polled the drop, was never asked
    for again. The worker now wakes the loop for a drop and the poll
    sends a wanted key again, a stale icon from a theme change while it
    was in flight too (`strand-render`'s
    `image::tests::a_dropped_request_wanted_again_is_decoded` and
    `a_dropped_stale_icon_wanted_again_is_decoded`).
  - `strand/tests/demo.rs::demo_bar_on_two_outputs_then_idle`: its idle
    window now reads where it began (just before its snapshot) and
    ended, and is taken again if it began within 2 s of a minute boundary
    or a minute tick fell in it (a loaded runner holding the test
    thread). The cause of
    the one CI failure is not proven; the fixed test passed 30 of 30
    runs (20 loaded), the reloads test 15 of 15 and the image worker's
    tests 200 of 200 in the sweep's stress (decisions.md
    m6-integration-w1, sweep-6).
  - `strand-watch/src/core.rs::tests::a_file_rewritten_without_pause_is_read_within_max_delay`
    (read before `max_delay`): the test's writes are stamped ahead of
    the clock, so a stall between write and read no longer makes them
    look old; it stalls once on purpose.
- Explained and fixed (decisions.md m5-open-spectrum): `strand/tests/wm_audio_shells.rs::a_test_tone_lifts_a_spectrum_bar`
  (the tone's bar never lifted; GitHub runs 38065740030, 38076927579
  and 38116737469). When the bar's first frame came before the `audio`
  service's first state, the spectrum was tapped for device 0. The
  real id's diff changed no pixels, so no frame followed and the tap
  was never retargeted. Feeds now sync after every applied diff
  (`Host::apply_diff`). Reproduced in the container by delaying the
  audio loop 3 s: it fails before the fix and passes after.
- `crates/strand/tests/reloads.rs` allows no bus connection during the
  reloads now that `strand-introspect` keeps one connection per bus,
  but its reconnect after a drop strand's log names (`strand-introspect`
  logs each connection it drops and each connect it gives up on; a
  starved runner's mock), one reconnect per drop and nothing timed, and
  counts the `from dbus` refreshes on the kept connection (decisions.md
  m6-flakes).
- Done in M5's wave 0 (decisions.md m5-infra): the two functional tests
  that bounded wall-clock time now check work done and order, and
  `compositor_matrix.rs` sends Hyprland's Lua dispatch form first (its
  classic fallback has never run: no leg uses a hyprlang config); the
  sweep moved its option settings to `eval` too (m5-integration-w3).

### Verification gaps

- A second monitor is checked live on sway and Hyprland (CI, vkms) by the
  compositor matrix; niri runs on one output (nested winit cannot add
  one), and Hyprland cannot run locally without a KMS card (decisions.md
  laptop-verify). Hyprland's matrix leg is CI's alone.
- The Hyprland 0.56.2 and niri 26.04 IPC fixtures are checked against
  real captures (`*-captured` fixtures; `scripts/capture-hyprland.sh`,
  `scripts/container/capture-niri.sh`). The first Hyprland capture was
  put together from three captures with unrecorded gaps and lacked a
  second monitor, close, move, fullscreen, reload and special
  workspaces; the owner's single `scripts/capture-hyprland.sh` run of
  2026-10-12 (`hyprland-0.56.2-captured-2`: three monitors, one
  unplugged and plugged back, and each of those events) closes that gap
  (decisions.md m6-owner).

### Known limits, recorded and not blockers

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
- Inspector: write-back swaps the file in with `renameat2(RENAME_EXCHANGE)`
  and checks the file it took out (M6, m6-edges), so an editor's save
  after the check is kept. Three edges remain: on a filesystem without
  the exchange (`EINVAL`/`ENOSYS`) it falls back to the check just
  before a plain rename, with that small window (the fallback is
  tested by failing the exchange in a seccomp filter, m6-integration-w1); an editor saving
  without pause through eight exchanges is refused with a later save
  kept, not necessarily with the latest in place; and an editor already
  holding the file open that writes into it after the check of the old
  file writes into the file being removed (no lock editors take could
  stop it). An answer to `inspect.pick` means logic told main, not that main
  has turned pick mode on (decisions.md m5-inspect; architecture.md).
- Drag and drop: (resolved in M6: a copy of the dragged item above a
  windowed `list`'s mounted rows is left out of `at` by logic, which
  works out a hidden row's `drag:` values from its item by walking the
  row as mounting would; decisions.md m6-edges, m6-integration-w1.)
  What the walk cannot know of a row never laid out: a `when` on its
  laid-out size (`self.width`) reads it as 0, as before a mounted row's
  first layout, and a `let` awaiting a service method is not fetched.
- Notifications: (resolved in M6 by S-services: `Notification.time` has
  its hour and minute; decisions.md m6-services.) Spec 1.2's
  `ActivationToken` is sent since S-platform, M5 wave 3.
- Tray: Activate and ContextMenu get the press's output-logical point
  (`demo/host.rs::a_press_sets_the_tray_click_point`); (0, 0) only before
  the first press and for actions no press caused (`dismiss`, `scroll`,
  `drop`).
- Media: (resolved in M6 by S-services: remote `https`/`http` art is
  fetched and cached; decisions.md m6-services.) An enterprise proxy that
  needs a login, or a private CA outside the system's certificates, leaves
  remote art null.
- Lock: PAM's texts and prompts reach the lock as PAM says them
  (`auth.message`, `auth.prompt`; M6 S-lock, decisions.md m6-lock), so
  a `pam_fprintd` stack shows "Place your finger" and unlocks on a
  finger. What PAM's one conversation still imposes: a password typed
  while `pam_fprintd` waits is tried only once it stops waiting (30 s
  by default), after a wrong password PAM starts over with the reader,
  once the reader gives up it listens again only when the lock is
  woken (a key or pointer event; the first key of a password typed
  then waits through the reader's wait again; m6-lock lock-r2-1; texts
  with no wait before the prompt, a banner or a lockout's, are not
  restarted, lock-r3-1), a reader wait of 90 s or more (`timeout=`
  past three times the client's 30 s) leaves a typed password, the
  fallback's too, to time out, so only the finger unlocks (lock-r3-2;
  README says to keep it under 90 s). The fallback field shows PAM's
  text (its own bitmap font; m6-integration-w1) but not its prompts:
  it answers every prompt with the password typed, as before.
- Network: (resolved in M6 by S-services: a join is a task; an
  enterprise (802.1X) network with a profile saved in NetworkManager
  joins with `connect()`; WEP joins with its key like a WPA personal
  network, and `secure` is true exactly for those two; decisions.md
  m6-services.) An unsaved enterprise network is refused: design.md has
  no prompt for an EAP login, so it is set up in the system's network
  settings.
- Audio: (`StepVolume` was removed in M6, decisions.md m6-services.)
  Without an inotify instance the audio thread reconnects on its 10 s timer (no other
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

None: the tree-sitter grammar, M1's last open box, was built by S-lsp in
M5 wave 2 and both its boxes are ticked.

## Infrastructure

- Container suite: every build and test runs through
  `scripts/container/` (CLAUDE.md; decisions.md laptop-container).
- GPU: lavapipe in the container image and CI, advisory hardware checks
  through `/dev/dri/renderD128` (`scripts/container/gpu.sh`); the GPU
  backend is in every build with its own `.text` gate (decisions.md
  m4-owner).
- Lock: `scripts/container/lockvm.sh [CMD]` runs CMD as root in a KVM
  guest with real PAM, sway and user `tester` (no CMD: the smoke); the
  owner allowed `/dev/kvm` for that container only (m4-owner). Build
  with `scripts/container/run.sh bash scripts/lockvm/scenarios/build.sh`,
  run `scripts/container/lockvm.sh bash scripts/lockvm/scenarios/all.sh`.
  CI's `lock-vm` job runs the same harness under the runner's KVM.
- Size: `scripts/size-report.sh` (two fat-LTO builds, per-crate
  breakdown, top symbols; on demand, it takes a build slot).
- Plans and reports: `docs/m5-plan.md` and `docs/m5-report.md` (M5),
  `docs/m4-plan.md` and `docs/m4-report.md` (M4); interface text in
  `docs/architecture.md`.
- Remote branches: `origin/main` only. M5's and M6 wave 1's branches
  are merged into `main` and deleted.

## Handoff checklist (M6 wave 1)

M5's close checklist is in git history (`361296e`); its open lines
(the newcomer trials or Q15 (ii), `run.sh ci` and `matrix.sh` on a
closing head) carry over below or to the owner's items.

- [x] All four wave-1 streams merged into `laptop/m6-w1`, each reviewed
  and fixed before its merge (decisions.md m6-*).
- [x] The integrator's sweep: the drop position exact for every row
  shape, the fallback lock's PAM text, the stubs, the write-back
  fallback's test, the flake fixes stressed (m6-integration-w1).
- [x] M6 Hardening's ten boxes ticked with their tests.
- [x] `.text` measured at the head (`067711a`): inside both gates, no
  re-base at close.
- [x] The release budgets test passes in the container at the head (11
  passed, 1 ignored).
- [x] The lock VM's `scenarios/all.sh` printed LOCK VM SCENARIOS PASSED
  on the last lock change (the sweep).
- [x] Stream worktrees' `target/` directories gone
  (`strand-wt/m6-{lock,services,edges,flakes}`).
- [x] The three intermittent failures found on the head fixed at their
  causes, each with a test (m6-integration-w1, verification).
- [x] The laptop's `run.sh ci` and `matrix.sh` on the head `5484c4f`,
  one at a time, and GitHub CI green on the same commit, the nightly
  job included.
- [x] `main` fast-forwarded to `5484c4f` under the standing approval;
  the merged `laptop/m6-*` branches deleted locally and from the
  remote.
- [x] Every `strand-wt/m6*` worktree and its `target/` removed.
- [x] The Hyprland capture (owner-run, open item 3): `hyprland-0.56.2-captured-2`.
