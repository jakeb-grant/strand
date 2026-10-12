# Handoff: M5 closing (updated 2026-10-12)

M5's work is done on the integration branch `laptop/m5-w3`: every
stream of waves 0–3 is merged, the integrator's sweep closed what the
waves left, all ten of M5's feature boxes are ticked, the language is
frozen, and GitHub CI is green on every job at the head. What is left
is the owner's: the newcomer trials (or Q15's fallback), the close gate,
one proposed interface field, and the fast-forward of `main`. Read this
first when picking the repo up cold, then `docs/m5-report.md` (what M5
delivered, sizes, memory, timing, rulings, what remains),
`docs/m5-plan.md` (streams, ledger, closing checks), `docs/decisions.md`
(the `## m5-*` sections; the integrator's `## m5-integration`,
`-w1`, `-w2` and `-w3` hold the merges and the sweep) and
`docs/features.md`. `docs/m4-report.md` has M4's numbers.

## State after M5

- **M0–M4 are complete; M5 is complete but for its newcomer exit.**
  features.md: M0 20/20, M1 59/59, M2 30/30, M3 14/14, M4 18/18, **M5
  11/12** (ten boxes and the freeze exit ticked; the newcomer exit
  waits on the owner).
- **Branches.** `main` is at `aa3053c` (waves 0–2). `laptop/m5-w3`
  (from `aa3053c`) holds wave 3, the sweep, the freeze and the closing
  paperwork; it is what the owner fast-forwards `main` to.
- **Wave 3** (decisions.md m5-integration-w3): S-fixups (wave 2's
  stragglers, `strand report`'s grim reference PNGs), S-inspect's render
  and binary half (pick, outline, flash, `strand-dev inspect` with
  write-back), S-platform (xdg-activation), S-docs (ladder test,
  generated reference, newcomer test, agent-proxy harness, spec review),
  each reviewed in up to three rounds before its merge; then the
  integrator's sweep (Hyprland's activation policy through `hyprctl
  eval`, the `run:` THP restore, the equality rule, canvas paint order,
  the shader "why" line, the GPU box on hardware, a strand-auth flake)
  and the freeze commit (`672b114`). m5-report.md lists every wave.
- **No stragglers.** Every item handoff.md deferred from wave 2 (the
  inspector's render half, S-platform, S-docs' wave-3 work, and the
  five proposed fixes for other owners: `drop_index`, `rw_root`,
  `Document::parse`, the tray `open()` doc, the plugins' leftover-file
  log level, kept at `info` with its reason) is built or ruled on
  (m5-fixups, m5-integration-w3). Nothing wave 3 left is unowned:
  S-inspect's deferred shader "why" was built in the sweep and S-docs'
  proposed equality change landed.
- **Checks on the head.** GitHub CI run 38220728825 at `672b114`: lint,
  test, budgets, acceptance, timing, compositors (Hyprland 0.56.2
  included) and lock-vm all green. Locally in the sweep: the lock VM's
  `scenarios/all.sh`, the hardware `gpu.sh` leg, the
  `--no-default-features` clippy and build; at close,
  `scripts/size-report.sh` and the release budgets test (11 passed).
  **Not yet run on the head**: the laptop's `run.sh ci` and `matrix.sh`
  (the closing tasks were held to crate-scoped tests); the standing
  fast-forward approval needs both.

## Next

1. **The laptop's full set on the closing head**, one at a time:
   `scripts/container/run.sh ci`, then `scripts/container/matrix.sh`.
   With GitHub CI already green, that meets the standing approval for
   the owner's fast-forward of `main` to `laptop/m5-w3`.
2. **The owner's open items** below; the human trials' results go in
   m5-report.md's "Human trials" table.
3. **After the fast-forward**: delete the merged wave-3 branches
   (`laptop/m5-w3`, `m5-fixups`, `m5-inspect`, `m5-platform`,
   `m5-docs3`) from the remote, as earlier waves' were.
4. **M6 or later** is not planned in features.md. The candidates are
   m5-report.md's "What remains" and the known limits below; the 1.0
   release is the owner's call once Strand has been used for a while
   (Q20). From here a change to grammar.md or spec.md needs a
   decisions.md paragraph citing spec.md 14's compatibility policy.

## Open items for the owner

Each has its paragraph in decisions.md.

1. **The newcomer exit criterion (Q15).** Two or three timed human
   trials on their own machines and compositors, with the budgets job's
   `strand-release-binaries` artifact (30 days) or a source build; or
   invoke Q15's fallback (ii), where the green automated newcomer test
   and the agent proxy carry it (no proxy session has been run: starting
   one is a person's call, `scripts/agent-proxy/run.sh` with
   `AGENT_PROXY_CMD`).
2. **The close gate** (m5-plan Closing step 5): proposed **keep both**
   `.text` gates (18,874,368 / 22,806,528 B; the speed rule reproduces
   them); the alternative is m5-plan's original "next 64 KiB above"
   (18,743,296 / 22,675,456 B). The gate test keeps today's values until
   the owner says.
3. **Wave 3's `.text` overruns, accepted by the integrator** under the
   standing policy (S-platform +94,272 B against 20 KB, S-inspect
   +35,008 B against 20 KB; decisions.md m5-integration-w3, sweep-3).
   The owner may re-rule.
4. **Interface changes**: wave 3's accepted ones (S-fixups' drag
   identity and `drawn`/`unchanged`, S-inspect's inspect protocol,
   S-platform's handler input context and token types) and one
   **proposed** in architecture.md: the `why` field of `inspect.node`'s
   answer for a `shader` node.
5. **The fast-forward of `main`** once item 1 of "Next" is green.

Answered and standing: the owner deferred `strand report`'s grim
reference PNGs to the integrator (2026-10-11), who made them,
deterministic on the frozen-clock acceptance mock (m5-fixups); wave 2's
overruns (accepted 2026-10-11), S-ipc's `Request(s json)` alternative
(not taken), the presented GPU leg's after-drop bound (a leak
tripwire), the `gpu-measure.sh` KMS-card run (not needed, Q19 (A)).
Every M4 owner item was answered on 2026-10-10.

## Budgets now

At `672b114` (m5-report.md has the tables). Size and memory gates are
regression tripwires, never caps on a user's shell: growth past them is
read by its cause and re-based or cut with a recorded reason.

- **`.text`** (`scripts/size-report.sh` in the container; Δ against
  M4's merged 15,657,671 / 19,041,095 B): CPU-only **18,711,127 B** (Δ
  +3,053,456) of the **18,874,368 B** gate, 163,241 B of headroom; GPU
  **22,630,295 B** (Δ +3,589,200) of **22,806,528 B**, 176,233 B of
  headroom. GitHub's runner links about 2 KB smaller (18,709,202 /
  22,628,370 B); read the ledger against the same machine's baseline.
- **Memory** (CI tripwires set by judgement, decisions.md
  m5-open-budgets): the design bar warns above 42 MB (43,008 kB) and
  fails above 48 MB (49,152 kB): 37,328 kB in the container, 38,912 kB
  on GitHub. The full shell warns above 76 MB (77,824 kB) and fails
  above 86 MB (88,064 kB): 51,944 kB (4 entries) and 62,204 kB (161)
  in the container, 66,422 kB (15) and 68,070 kB (172) on GitHub.
- **The promoted GPU surface on hardware** (reported, never gated):
  Intel PTL presenting to a GLES2 sway on renderD128: frame p50
  1.09–1.10 ms, p95 3.87–4.18 ms, p99 4.53–5.08 ms; PSS +21.0–26.1 MB
  while promoted, +6.35 MB after the drop (bound 12 MiB).
- **Latency** (GitHub's timing job enforces; advisory on the laptop),
  CI at `672b114`: reload token p95 16.2 ms headless against a break
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
- CI flakes seen once and not reproduced (decisions.md laptop-flakes;
  m3-report Open). The fixed ones are recorded there and in
  decisions.md laptop-labwc and laptop-media.
  - `strand/tests/budgets.rs::the_full_shell_on_the_real_services_is_measured`
    (launcher drew no marked icon). A failure now says whether the icons
    are late or never drawn, and where the magenta is.
  - `strand/tests/demo.rs::demo_bar_on_two_outputs_then_idle`. Its idle
    window now starts after boot has settled; the woken thread is named
    on failure.
  - `strand-watch/src/core.rs::tests::a_file_rewritten_without_pause_is_read_within_max_delay`
    (read before `max_delay`), once in run 37874718084. 0 of 30 failed
    in the container.
- Explained and fixed (decisions.md m5-open-spectrum): `strand/tests/wm_audio_shells.rs::a_test_tone_lifts_a_spectrum_bar`
  (the tone's bar never lifted; GitHub runs 38065740030, 38076927579
  and 38116737469). When the bar's first frame came before the `audio`
  service's first state, the spectrum was tapped for device 0. The
  real id's diff changed no pixels, so no frame followed and the tap
  was never retargeted. Feeds now sync after every applied diff
  (`Host::apply_diff`). Reproduced in the container by delaying the
  audio loop 3 s: it fails before the fix and passes after.
- `crates/strand/tests/reloads.rs` still allows a bus connection that
  only introspects, though `strand-introspect` now keeps one connection
  per bus; tightening it is left to that file's owner (decisions.md
  laptop-services).
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
  `scripts/container/capture-niri.sh`). The Hyprland capture lacks a
  second monitor, close, move, fullscreen, reload and special workspaces.
  It was put together from three separate captures with unrecorded gaps
  between them (its SOURCE.txt). A capture from
  `scripts/capture-hyprland.sh` during a busy session would close that
  gap: it reads its replies while the stream is quiet and records in
  `marks.txt` the stream line where each set falls.

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
  after the check is kept. Two edges remain: on a filesystem without the
  exchange (`EINVAL`/`ENOSYS`) it falls back to the check just before a
  plain rename, with that small window; an editor saving without pause
  through eight exchanges is refused with a later save kept, not
  necessarily with the latest in place; and an editor already holding
  the file open that writes into it after the check of the old file
  writes into the file being removed (no lock editors take could stop
  it). An answer to `inspect.pick` means logic told main, not that main
  has turned pick mode on (decisions.md m5-inspect; architecture.md).
- Drag and drop: a copy of the dragged item above a windowed `list`'s
  mounted rows is left out of `at` by logic (M6, m6-edges), which reads
  each row's own `drag:` values off the mounted rows (render's walk: a
  `list` or `for` inside the row, such as a dock item's window dots, is
  not the row's, wherever it sits among the row's children) as its item,
  its key or one of its fields; rows none of whose own `drag:` values is
  one of those (or not the same one for every mounted row) are never
  copies there, so `at` stays one too high for them, as before M6.
- Notifications: `Notification.time` has no time of day yet (spec 1.2's
  `ActivationToken` is sent since S-platform, M5 wave 3).
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
- Audio: `StepVolume` has no caller on the language path. Without an
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
- Remote branches: `origin/main` and M5's wave-3 `origin/laptop/m5-*`
  branches: the integration branch `m5-w3` and wave 3's streams
  (`m5-fixups`, `m5-inspect`, `m5-platform`, `m5-docs3`), all merged
  into `m5-w3`. Wave 2's integration branch and streams are gone from
  the remote (wave 2 is on `main`); earlier waves' branches were merged
  into `main` and deleted.

## Handoff checklist (M5 close)

Wave 3's checklist is in git history (`672b114`), wave 2's at
`aa3053c`, wave 1's at `6cee276`, wave 0's at `88d4d99`, M4's at
`70cdc56`.

- [x] Every M5 stream merged on its integration branch, reviewed and
  fixed before its merge; wave 2's and wave 3's stragglers built or
  ruled on (m5-fixups, m5-integration-w3).
- [x] All ten M5 boxes ticked with their tests; the freeze commit
  (`672b114`) ticks the freeze exit.
- [x] `.text` measured at close with `scripts/size-report.sh`, inside
  both gates; m5-plan's ledger "Confirmed at close"; the close gate
  proposed (keep both).
- [x] The release budgets test passes in the container at the head (11
  passed); GitHub CI green on every job at `672b114`.
- [x] m5-report.md written; features.md's M5 count; this file as the
  M5 handoff.
- [x] Wave-3 stream worktrees' `target/` directories deleted
  (`strand-wt/{fixups,inspect,platform,docs3}`).
- [ ] The laptop's `run.sh ci` and `matrix.sh` on the closing head, one
  at a time.
- [ ] The newcomer trials or Q15 (ii) (owner), recorded in
  m5-report.md.
- [ ] The owner fast-forwards `main` to the closing head; the merged
  `laptop/m5-*` branches are deleted from the remote after it.
- [ ] `strand-wt/w3/target` deleted when the integrator's work on the
  branch ends.
