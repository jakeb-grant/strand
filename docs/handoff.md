# Handoff: M5 wave 0 to wave 1 (updated 2026-10-10)

A pausing point: M5's wave 0 is merged and hardened, and wave 1 has
not started. Read this first when picking the repo up cold, then
`docs/m5-plan.md` (streams, waves, ledger, closing checks),
`docs/decisions.md` (`## m5-owner`, `## m5-integration`, `## m5-core`,
`## m5-size`, `## m5-infra`, `## m5-w0-harden`) for the reasons, and
`docs/features.md` for M5's boxes. `docs/m4-report.md` has M4's
measured numbers.

## State

- **M0–M4 are complete** and on `main` (`70cdc56`). features.md: M0
  20/20, M1 58/59, M2 30/30, M3 14/14, M4 18/18, M5 1/12 (ten boxes and
  two exit criteria; the ticked one is the importers half of the
  importer box, split by the owner's Q17 and ticked from M2's tests).
  The open M1 box is the tree-sitter grammar, which M5 owns (S-lsp).
- **M5's plan and the owner's answers** (2026-10-10): `docs/m5-plan.md`,
  and decisions.md `## m5-owner` (Q1–Q20). M5 is not the 1.0 release:
  it closes as a normal milestone with no version bump, tag, release
  notes or release artifact (Q20). 0a (architecture.md, "M5 interfaces
  (wave 0a)") and 0-freeze (design.md's four open questions answered
  and ticked, the ladder's `Async` at level 4, the importer box split)
  landed on `laptop/m5-plan`.
- **Wave 0 is merged on `laptop/m5-w0`** (decisions.md m5-integration),
  in the plan's order:
  - 0b, `laptop/m5-core`: `strand_core::ipc` (types, JSON-line codec,
    `socket_path`, `EventSink`), `crates/strand/src/ipc.rs` split into
    `ipc/{mod,server,cli,describe}.rs`, `Shell::request` dispatching to
    `run/request/*.rs` (one empty module per new command), the
    `strand` lib/bin split, `ActionCtx`/`InputCtx` (now in
    `strand_core::action`) threaded through every host, the
    `IpcRequests` channel, the inert render stubs, `Suggestion::title`,
    and `crates/strand/tests/m5.rs` with its empty `m5/` modules.
  - 0-size, `laptop/m5-size`: `scripts/size-report.sh` and reclaims 1
    (strand-theme decodes WebP with `image-webp`, no `image`) and 3
    (blake3 tried at "z" and not taken), −37,056 B CPU-only; reclaim 2
    (resvg at "z") withdrawn for its bindable-SVG cost.
  - 0-infra, `laptop/m5-infra`: three spikes (headless sway renders on
    renderD128 with GLES2 or Vulkan and offers linux-dmabuf, so Q19 is
    (A); the tree-sitter CLI 0.27.0 generates from `grammar.js` without
    node; sway 1.9 issues and honours an xdg-activation token from a
    layer surface), the pinned tree-sitter CLI in the Dockerfile and
    ci.yml's lint job, `scripts/check-tree-sitter.sh`, the budgets
    ledger line, `gpu-session.sh` on GLES2 or Vulkan,
    `scripts/gpu-measure.sh` (owner-run only; no agent runs it), and
    handoff's deferred CI items.
  - The CPU-only `.text` gate went 15 MiB → 15,925,248 B (M5's
    provisional re-base) → **15,859,712 B** after the reclaims.
- **Wave 0 was hardened in three audit rounds** (decisions.md
  m5-w0-harden): 17, 12 and 13 confirmed findings, all fixed, in
  `cc9085f`, `94fe372` and `b0aee20`. The ones that change what wave 1
  builds on:
  - the inspector's messages are fixed as types: `ToMain::Highlight(Option<NodeId>)`,
    `DamageFlash`, `PickMode`, `SnapshotKind::Layout { node }`,
    `ToLogic::Picked`; main resolves a node's surfaces with
    `Renderer::surfaces_of` (logic never learns a `SurfaceId`);
  - `EventSink` feeds D-Bus `watch` through `Shell::broadcast`, socket
    or not; the request channel closes as the loop ends;
  - the handler input context's route and the ordering rule (every
    invocation numbered from 1 and kept across `await`, one queue for
    all services) are architecture.md text for S-platform;
  - an overlaid token's origin is `strand_scene::tokens::OVERLAY_ORIGIN`;
  - `strand::process_setup!()` gives a plugin binary the release
    binary's mimalloc and THP-off constructor;
  - CI's reload-latency gate and the fuzzer's dedicated run use
    `--lib` again (they ran 0 tests after the lib/bin split), through
    `scripts/container/tests-ran.sh`, which fails a step that runs no
    test;
  - `reset{node}` on the request channel, the budgets job's trial
    binaries (`strand-release-binaries`), `gpu-measure.sh`'s header and
    `check-tree-sitter.sh`'s clean regeneration.
- **Wave 0's writing column was not done.** S-docs' spec.md skeleton,
  guide outline, ladder placements and `strand new` template, and
  S-tools' export mapping table, move to the start of wave 1's writing
  column, the spec.md skeleton first (decisions.md m5-w0-harden, second
  round). This waits on the owner's confirmation (open items below).
- `main` is still at M4's close (`70cdc56`). `laptop/m5-w0` is ahead of
  it by wave 0 and the hardening; the owner fast-forwards `main` to its
  head once the closing checks are green on that exact commit (the
  owner runs the ff+push; agents never push `main`).

## Next: M5 wave 1

Wave 1 has not started. From m5-plan.md's Waves table and stream
sections:

| Stream | Branch and worktree | Owns (wave 1 part) | First steps |
|---|---|---|---|
| S-ipc (building) | `laptop/m5-ipc`, `../strand-wt/ipc` | `strand/src/ipc/{query,call,overlay}.rs`, `run/request/{get,set,toggle,call,clear}.rs`, the COMMANDS table, strand-compiler `instantiate/overlay.rs` | the `get`/`toggle`/`call`/`clear` parse and encoding in strand-core's ipc tests; then `get` and `toggle` (bool only); `call` (service actions, Q13); the token overlay layer in `$XDG_STATE_HOME/strand/tokens.toml` with the "overlay wins" notice and `OVERLAY_ORIGIN`; `tests/m5/cli.rs`. D-Bus (`ipc_bus.rs`) is wave 2. Budget 35 KB (+37,400 B D-Bus later) |
| S-inspect (building) | `laptop/m5-inspect`, `../strand-wt/inspect` | strand-compiler `instantiate/inspect.rs`, strand-scene `provenance.rs` | `identity`, `cells_of`, `reset_subtree`, `source_hash`; `TokenTable::provenance` (with `set { }`, component `tokens { }`, overlay and contrast steps); the three kept badges; `inspect.dump` JSON with passwords redacted; `crates/strand-compiler/tests/inspect.rs` and provenance unit tests. One CPU-only `.text` measurement after wave 1 (budget 55 KB; cut scope before wave 2 if over) |
| S-lsp (building) | `laptop/m5-lsp`, `../strand-wt/lsp` | `strand-dev/src/{diag.rs,refactor/**}`, `server.rs`'s capability block, `check/tree.rs`/`check/expr.rs` key suggestions | the missing-key quick fix (`check::missing_key`, one `Suggestion` per candidate, preferred first), then extract component (`REFACTOR_EXTRACT`); their lsp.rs tests and `reloads.rs::an_extracted_component_resets_only_its_subtree`. The tree-sitter grammar is wave 2 and starts only once S-docs' spec.md skeleton is on `laptop/m5-w0`. Budget 3 KB |
| S-docs (writing, no slot) | `laptop/m5-docs`, `../strand-wt/docs` | `docs/guide/**` (template included), `docs/spec.md` | the `docs/spec.md` skeleton first (merged before any other S-docs writing), then the guide outline, the ladder placements (one decisions.md paragraph each) and `docs/guide/template/bar.strand`; then levels 1–3 and spec.md's text |
| S-tools (writing, no slot) | `laptop/m5-tools`, `../strand-wt/tools` | `strand-theme/src/export.rs` (wave 2 code) | the role→key export mapping table in decisions.md `## m5-tools`, and confirm the palette source (persisted palette, else the offline solve, else the error) |
| S-plugins (writing, no slot) | `laptop/m5-plugins`, `../strand-wt/plugins` | the builder API (wave 2 code) | design notes for `Shell::builder().service::<T>().run()`, `--write-schemas` and `plugin_hello` (which must invoke `strand::process_setup!()`) |

How to launch it:

1. **Branch point.** If the owner has fast-forwarded `main` to
   `laptop/m5-w0`'s head (`git merge-base --is-ancestor laptop/m5-w0
   main`), each stream runs `git worktree add ../strand-wt/<name> -b
   laptop/m5-<name> main`; otherwise it branches from `laptop/m5-w0`.
   It is the same commit either way (m5-plan, Ground rules; decisions.md
   m5-integration, handoff).
2. **Slots.** At most three worktrees build at once
   (`CARGO_BUILD_JOBS=6` each), the integrator's included whenever it
   builds or runs `run.sh ci`, `matrix.sh`, `gpu.sh` or `lockvm.sh`.
   Writing streams hold no slot. Streams run crate-scoped tests only;
   only the integrator runs `run.sh ci` or a workspace test run, one at
   a time.
3. **Each stream** works only in the files it owns (m5-plan's Streams
   table and per-file ownership; the shared dispatch points take
   one-line entries the integrator merges), appends decisions under
   `## m5-<name>`, runs `run.sh cargo fmt --all` and `run.sh cargo
   clippy --workspace --all-targets -- -D warnings` (exit 0) before each
   commit, pushes only its branch, and deletes its `target/` at the end.
4. **Review and fix.** Each finished item gets a review round and its
   fixes on the stream's branch before it is handed to the integrator.
5. **Integrator.** Merges finished items into `laptop/m5-w0`, runs the
   budgets job on the merged tree and checks the ledger line (within
   allocation: CPU-only Δ ≤ +192,944; GPU Δ ≤ +232,624; m5-plan "Reading
   Δ as spend"), and records each merge under `## m5-integration`.
6. **Full checks on the exact head**, one at a time:
   `scripts/container/run.sh ci`; `scripts/container/matrix.sh`; the
   lock VM (`scripts/container/run.sh bash
   scripts/lockvm/scenarios/build.sh`, then `scripts/container/lockvm.sh
   bash scripts/lockvm/scenarios/all.sh`, which must print LOCK VM
   SCENARIOS PASSED); `scripts/container/gpu.sh` (advisory); then GitHub
   CI green on that sha (`gh run list --branch <branch> -L 1`, `gh run
   watch <id> --exit-status`). Anything committed after a local check
   ran means the local set runs again on the final head.
7. **`main`** is moved by the owner, who runs the ff+push once the
   checks above are green on the exact commit.

## Open items for the owner

None blocks wave 1's building streams. Each has its paragraph in
decisions.md.

1. **The presented GPU leg's after-drop bound** (m5-infra; features.md's
   GPU-cost box): settled (owner, 2026-10-10: memory is not sacred).
   Presenting on renderD128 left 4.6–8.1 MiB after the drop in six
   runs; that leg is now held to 12 MiB, asserted again, a leak
   tripwire with headroom over that spread
   (decisions.md m5-speed-gpu-bound). Whether `scripts/gpu-measure.sh`'s
   KMS-card run is still wanted stays the owner's call.
2. **The duplicated image decoders** (m5-size, last paragraph).
   `jpeg_decoder` and `image_webp` are each built twice (about 44 KB +
   45 KB, ~89 KB), once for strand-theme and once for strand-render.
   Removing the copy needs one crate to own the decode, a new dependency
   between them, or strand-theme reading wallpapers into memory: an
   architecture change offered, not taken.
3. **How 0b's +12.5 KB overrun was charged** (m5-integration). 0b's
   gate was "unchanged within 1 KB"; it measured +12,544 B CPU-only
   (+8,512 B GPU). The integrator accepted it and charged 2,600 B to
   S-ipc's D-Bus line (40,000 → 37,400 B) and 9,944 B to the reserve
   (42,000 → 32,056 B). The owner may overrule; the lib/bin split or the
   request-channel drain is what would move. The hardening's +2,816 and
   +64 B also came from the reserve, now 29,176 B.
4. **The gate-lowering reading** (m5-integration). The plan's "lowered
   by the bytes it measured, rounded to 64 KiB" is ambiguous for a
   reclaim under one step. The integrator recomputed the gate from the
   lowered baseline (15,620,615 + 230,000, rounded up) to **15,859,712
   B**; m5-size read it as no step, which leaves **15,925,248 B**. The
   lowered gate removes only headroom no ledger line owns (9,097 B of
   rounding remain unspendable). Confirm it, or restore 15,925,248 B.
5. **Wave 0's unwritten writing column** (m5-w0-harden, second round).
   Confirm the move to the start of wave 1 (S-docs' spec.md skeleton
   first, then guide outline, ladder placements and template; S-tools'
   export mapping table), or have them written before wave 1 opens.
   S-lsp's grammar (wave 2) waits on the spec.md skeleton either way.
6. **Interface choices the hardening made** (m5-w0-harden), which the
   owner may overrule: inspector messages carry a node only (main
   resolves surfaces with `Renderer::surfaces_of`); the ordering rule
   (invocations numbered from 1 and kept across `await`, 0 outside any
   handler and unordered, one queue for all services); an overlaid
   token's origin is the reserved string `overlay`, replacing the
   file's origin while the overlay holds; the input-context route is
   architecture.md text and S-platform file ownership, not code.

Every M4 owner item was answered on 2026-10-10 (decisions.md m4-owner-2
and m4-owner-3; summarised under "M4 owner items" below).

## Budgets now

- **The speed profile** (`laptop/m5-speed-profile`, decisions.md
  m5-speed-profile and m5-speed-gates; not yet merged) re-bases both
  builds. `.text` goes to 17,958,935 B CPU-only (+2,322,960) and
  21,854,807 B GPU (+2,839,120), and the design bar's PSS rises by
  about 2 MB (37,817 kB on GitHub). Its proposed gates are 18,219,008 B
  CPU-only, 22,282,240 B GPU, and a 36,864 kB target with a 40,960 kB
  ceiling for the bar. The ledger reads spend as Δ − 2,285,904
  CPU-only and Δ − 2,801,744 GPU (m5-plan). Until the integrator merges
  it and sets those gates, the figures below are wave 0's.
- **`.text`**, from budgets.rs's gate test (the ledger line; Δ is
  against M4's merged figures 15,657,671 / 19,041,095 B):
  - container, after the third hardening round (`b0aee20`): CPU-only
    **15,635,975 B** (Δ −21,696) of the **15,859,712 B** gate, 223,737
    B under it. M5's allocations have spent 15,360 B of 230,000 B (0b
    and the hardening), 214,640 B left. GPU 19,013,831 B (wave 0's
    merged tree, not measured again in the container since) of
    19,398,656 B.
  - GitHub's budgets job at `94fe372` (run 38115583761): CPU-only
    15,633,986 B (Δ −23,685) and GPU 19,013,634 B (Δ −27,461). The
    runner links about 2 KB smaller than the container; read the ledger
    against the same machine's baseline.
  - Allocations left (m5-plan's ledger): IPC commands and overlay
    35,000; D-Bus 37,400; report 20,000; xdg-activation 20,000;
    inspector 55,000; new/export 15,000; Suggestion 3,000; reserve
    29,176 B; GPU-only 40,000 B (GPU build).
- **Memory** (benchmark targets for CI, never runtime caps on a user's
  shell; owner-confirmed, agents do not change them): the design bar
  warns above 34,816 kB and fails above 38,912 kB; GitHub measured
  35,659 kB at `94fe372` (WARN, as since M4). The full shell warns
  above 64 MB and fails above 70 MB; GitHub measured 39.5–53.9 MB.
- **Latency** (GitHub's timing job enforces; advisory on the laptop):
  reload p95 35 ms token / 50 ms markup at design.md's model, gated by
  the headless break; at `94fe372` token p95 17.1 ms against a break of
  19.6 ms. theme_swap_bench's 8-scope `spring(1600, 1)` whole swap 2.56
  ms against 5 ms at `94fe372` (4.31 ms at `laptop/m5-infra`).

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
  19.9 ms; the likely cause, the power state, is unconfirmed.
- The lint job fetches the pinned tree-sitter CLI 0.27.0 (sha256 in
  ci.yml and the Dockerfile; edit both together) and runs
  `check-tree-sitter.sh` (passes until S-lsp writes `grammar.js`) and
  the script tests (`test-size-report.sh`, `test-ledger.sh`,
  `test-gpu-measure.sh`, `test-check-tree-sitter.sh`,
  `test-tests-ran.sh`).
- The budgets job prints the size ledger line in its summary and
  uploads the release binaries as artifact `strand-release-binaries`
  (30 days; for the human trials, not a release).
- `matrix.sh` skips Hyprland locally (no KMS card); Hyprland's leg is
  CI's (vkms), where it runs 0.56.2 with a Lua config.
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
   is open item 1 above.
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
   to fat LTO with 40 `[profile.release.package]` overrides
   (m4-integration-w2; `Cargo.toml` is the reference). The memory
   figures hold for builds that read the workspace's release profile;
   README's "Building and installing" says so.
2. **Reload latency gate: kept** at design.md's 35 ms (token) and 50 ms
   (markup) p95, with 50 rounds.
3. **`shrink: 0` for absolutely sized `image`/`icon`: acknowledged** in
   design.md's layout section ("Shrinking").
4. **`strand-watch` ancestor watches stop below `$HOME`**
   (`Options::home`, `paths::watched_ancestors`).

Settled, do not reopen: the memory gates are owner-confirmed. The
two-monitor bar warns above a 34 MB target and fails above a 38 MB ceiling,
and the full shell warns above 64 MB and fails above 70 MB
(decisions.md wave4-core, commit 607bd10). Agents must not change them.
The owner's policy of 2026-10-10 lets agents set binary and memory gates
by judgement with recorded reasons; decisions.md m5-speed-gates proposes
the bar's re-base under it, for the integrator.

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
- Recurring and unexplained: `strand/tests/wm_audio_shells.rs::a_test_tone_lifts_a_spectrum_bar`
  (the tone's bar never lifted) failed twice on GitHub (runs
  38065740030 and 38076927579); neither commit touched audio or render
  code. The meter's start, failure and retry lines are info now, and
  the failure detail says whether strand's capture stream started,
  failed or was never linked (decisions.md m4-audit rounds 6 and 9). It
  passes in the container.
- `crates/strand/tests/reloads.rs` still allows a bus connection that
  only introspects, though `strand-introspect` now keeps one connection
  per bus; tightening it is left to that file's owner (decisions.md
  laptop-services).
- Done in M5's wave 0 (decisions.md m5-infra): the two functional tests
  that bounded wall-clock time now check work done and order, and
  `compositor_matrix.rs` sends Hyprland's Lua dispatch form first (its
  classic fallback has never run: no leg uses a hyprlang config).

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
- Notifications: ActivationToken (spec 1.2) needs an xdg-activation token
  from the clicked surface, so the server reports spec 1.1 until
  S-platform's wave-2 work (features.md's M5 xdg-activation box, which
  also covers `XDG_ACTIVATION_TOKEN` for app launches);
  `Notification.time` has no time of day yet.
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

- tree-sitter grammar for `.strand` (M5, S-lsp, wave 2).

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
- Plans: `docs/m5-plan.md` (M5), `docs/m4-plan.md` (M4); interface text
  in `docs/architecture.md`.
- Remote branches: `origin/main` and M5's `origin/laptop/m5-*`
  branches (`m5-plan`, `m5-core`, `m5-size`, `m5-infra`, `m5-w0`).

## Handoff checklist (wave 0)

M4's checklist is in git history (`70cdc56`).

- [x] Wave 0 merged on `laptop/m5-w0` and hardened in three rounds; no
  budget or gate raised (the CPU-only gate lowered to 15,859,712 B).
- [x] The plan, decisions.md, architecture.md and this file agree on
  branches, the ledger's charged figures and the open owner items.
- [ ] The full local set on the branch's final head, one at a time
  (`run.sh ci`, `matrix.sh`, the lock VM scenarios, `gpu.sh`), then
  GitHub CI green on that head. The integrator runs these after the last
  docs commit, so their results are in its hand-back, not here.
- [ ] The owner fast-forwards `main` to that head.
- [ ] Delete worktree `target/` directories (each agent at its task's
  end; the cargo registry stays in its volumes).
