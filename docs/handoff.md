# Handoff: M5 wave 0 to wave 1 (updated 2026-10-11)

A pausing point: M5's wave 0 is merged and hardened, the speed review
is merged on top, and wave 1 has not started. Read this first when picking the repo up cold, then
`docs/m5-plan.md` (streams, waves, ledger, closing checks),
`docs/decisions.md` (`## m5-owner`, `## m5-integration`, `## m5-core`,
`## m5-size`, `## m5-infra`, `## m5-w0-harden`, the `## m5-speed-*`
sections, `## m5-speed` and `## m5-open-budgets`) for the reasons, and
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
    provisional re-base) → **15,859,712 B** after the reclaims (18,350,080 B since the speed review).
- **Wave 0 was hardened in three audit rounds** (decisions.md
  m5-w0-harden): 17, 12 and 13 confirmed findings, all fixed, in
  `cc9085f`, `94fe372` and `b0aee20`. No round came back clean; the
  hardening stopped after the third, so wave 1's reviews should expect
  more. The ones that change what wave 1 builds on:
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
- **The speed review is merged** (decisions.md m5-speed and the
  `m5-speed-*` sections), under the owner's policy of 2026-10-10:
  speed wins where the trade is sensible, gates catch accidental bloat
  and agents set them by judgement. Two streams, each reviewed and fixed
  once (8 and 9 findings):
  - `laptop/m5-speed-profile`: the release opt-levels re-chosen for
    speed. `strand` at opt 3 (fat LTO runs the whole binary at the
    linking crate's level), the image decoders, usvg/roxmltree,
    velato/serde_json, material-colors and wayland-client/-backend at
    3; strand-services, fontique, toml, calloop and xkeysym stay at
    size after measuring. budgets.rs's guard fails on a size level
    reaching a speed crate (per crate, wildcard or profile-wide).
    `crates/strand/src/speed.rs` (feature `speed-bench`) runs the
    benches in the shipped LTO session.
  - `laptop/m5-speed-code`: caches sized for speed (paint cache 32 MB
    with entries to 16 MB, those over 2 MB built only on a second whole
    repaint; offscreen, image and player caches 32 MB; a stopped frame
    loop keeps unused entries 30 s), the 4K crossfade snapshot in its
    own huge-page mapping (`hugepage::HugeBuf`, THP otherwise off), the
    presented GPU leg's after-drop bound at 12 MiB (since a 16 MiB
    backstop beside a three-cycle no-growth tripwire, m5-open-budgets).
  - Gates re-based: `.text` 18,350,080 B CPU-only and 22,282,240 B GPU;
    the bar's PSS 39 MB target / 44 MB ceiling, the full shell's 67 / 74
    MB, since raised with headroom for waves 1-2 to 42 / 48 MB and
    76 / 86 MB (m5-open-budgets; "Budgets now").
- **Wave 0's writing column was not done.** S-docs' spec.md skeleton,
  guide outline, ladder placements and `strand new` template, and
  S-tools' export mapping table, move to the start of wave 1's writing
  column, the spec.md skeleton first (decisions.md m5-w0-harden, second
  round); accepted as scheduling (open item 5).
- `main` is still at M4's close (`70cdc56`). `laptop/m5-w0` is ahead of
  it by wave 0, the hardening and the speed review; the owner fast-forwards `main` to its
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
   allocation: CPU-only Δ ≤ +2,518,656; GPU Δ ≤ +3,074,048, which
   includes the speed re-base; m5-plan "Reading Δ as spend"), and records each merge under `## m5-integration`.
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

No item blocks wave 1. The ones below were answered under the owner's
policy of 2026-10-10 ("Speed wins if the tradeoff is sensible"; gates
catch accidental bloat and agents set them by judgement with recorded
reasons; never a runtime cap on a user's shell). Each has its paragraph
in decisions.md.

1. **The presented GPU leg's after-drop bound: answered.** A leak
   tripwire, asserted again (m5-infra, m5-speed-gpu-bound). The plan
   proposed 8 MiB; the speed review measured 4.6–8.1 MiB left after the
   drop over six presented runs on renderD128, so the merged bound is
   12 MiB (`PRESENTED_SLACK_KB`), about 4 MiB over that spread. The
   integrator's `STRAND_GPU_PRESENT=1 gpu.sh` run on the merged tree
   left 7,128 kB over the baseline after the drop, inside it. Since
   m5-open-budgets the tripwire is growth between cycles (the hardware
   leg runs three; each later one within 4 MiB of the first), and the
   after-drop figures are backstops checked after every cycle: 16 MiB
   presenting (4.6–8.8 MiB measured in thirteen runs) and 12 MiB reading
   back (4.6–7.3 MiB in four; 6 MiB had been 21 kB from failing). The
   spread is one glibc arena of Mesa's, whose resident part moves by
   2–2.7 MiB between cycles and runs.
2. **`scripts/gpu-measure.sh`'s KMS-card run: not needed.** `gpu.sh`
   presents on renderD128 (Q19 (A)), which is what the GPU-cost box
   needed. The script stays, owner-optional; agents never run it.
3. **The duplicated image decoders: dropped.** Removing the second copy
   of `jpeg_decoder` and `image_webp` (~89 KB) would only save size and
   needs an architecture change; under the speed-first policy it is not
   worth one (m5-size, last paragraph).
4. **0b's +12.5 KB charge and the gate-lowering reading: superseded.**
   The m5-speed gate reset recomputes both gates from the merged tree's
   measured size plus M5's unspent allocation and a reserve
   (decisions.md m5-speed), so neither the charge's split nor the
   lowering's rounding moves a gate any more. The ledger keeps the
   charged figures (m5-plan, "Ledger and allocations").
5. **Wave 0's writing column moves to wave 1: accepted as scheduling.**
   S-docs' `docs/spec.md` skeleton is the first wave-1 item, merged
   before S-lsp's grammar (wave 2) starts; then the guide outline,
   ladder placements and template, and S-tools' export mapping table
   (m5-w0-harden, second round).
6. **Still open: interface choices the hardening made**
   (m5-w0-harden), which the owner may overrule: inspector messages
   carry a node only (main resolves surfaces with
   `Renderer::surfaces_of`); the ordering rule (invocations numbered
   from 1 and kept across `await`, 0 outside any handler and unordered,
   one queue for all services); an overlaid token's origin is the
   reserved string `overlay`, replacing the file's origin while the
   overlay holds; the input-context route is architecture.md text and
   S-platform file ownership, not code.

Every M4 owner item was answered on 2026-10-10 (decisions.md m4-owner-2
and m4-owner-3; summarised under "M4 owner items" below).

## Budgets now

- **`.text`**, from budgets.rs's gate test (the ledger line; Δ is
  against M4's merged figures 15,657,671 / 19,041,095 B), the merged
  speed review in the container: CPU-only **17,961,687 B** (Δ
  +2,304,016) of the **18,350,080 B** gate; GPU **21,857,111 B** (Δ
  +2,816,016) of **22,282,240 B** (decisions.md m5-speed). The speed
  re-base (+2,325,712 CPU-only, +2,841,424 GPU) is no allocation's
  spend: spend is Δ − 2,288,656 CPU-only and Δ − 2,804,048 GPU, so M5
  has spent 15,360 B (214,640 B left of 230,000) and 11,968 B GPU
  (258,032 B left of 270,000). Each gate is the measured size plus the
  unspent allocation plus a 64 KiB reserve, rounded up to 256 KiB. A
  later deliberate speed revert raises its gate by its recorded cost
  (m5-speed-policy); anything else that overruns is accidental growth.
  - GitHub's runner links about 2 KB smaller than the container; read
    the ledger against the same machine's baseline.
  - Allocations left (m5-plan's ledger): IPC commands and overlay
    35,000; D-Bus 37,400; report 20,000; xdg-activation 20,000;
    inspector 55,000; new/export 15,000; Suggestion 3,000; reserve
    29,176 B; GPU-only 40,000 B (GPU build).
- **Memory** (tripwires for accidental regressions in CI, never
  runtime caps on a user's shell; set by judgement under the owner's
  2026-10-10 policy, with headroom for waves 1-2's IPC, inspector and
  LSP code, decisions.md m5-open-budgets): the design bar warns above
  42 MB (43,008 kB) and fails above 48 MB (49,152 kB); it measured
  36,465 kB in the container on the merged tree and 37,588–37,817 kB
  on GitHub. The full shell warns above 76 MB (77,824 kB) and fails
  above 86 MB (88,064 kB); 61,428 kB (161 entries) here, 63,184–64,480
  kB in speed-code's runs, 62,503 and 68,040 kB (172 entries) on
  GitHub at `31da659` and `2fedfa2`. A target warning is info; raise
  them again with a recorded measurement rather than trimming a
  feature to fit.
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
- Recurring and unexplained: `strand/tests/wm_audio_shells.rs::a_test_tone_lifts_a_spectrum_bar`
  (the tone's bar never lifted) failed three times on GitHub (runs
  38065740030, 38076927579 and 38116737469, the last at `27e2660`); neither commit touched audio or render
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
  branches (`m5-plan`, `m5-core`, `m5-size`, `m5-infra`, `m5-w0`,
  `m5-speed-profile`, `m5-speed-code`).

## Handoff checklist (wave 0)

M4's checklist is in git history (`70cdc56`).

- [x] Wave 0 merged on `laptop/m5-w0` and hardened in three rounds
  (17, 12 and 13 confirmed findings, all fixed; no clean round).
- [x] The speed review merged; `.text` and PSS gates re-based with
  their reasons (decisions.md m5-speed).
- [x] The plan, decisions.md, architecture.md and this file agree on
  branches, the ledger's charged figures and the open owner items.
- [ ] The full local set on the branch's final head, one at a time
  (`run.sh ci`, `matrix.sh`, the lock VM scenarios, `gpu.sh`), then
  GitHub CI green on that head. The integrator runs these after the last
  docs commit, so their results are in its hand-back, not here.
- [ ] The owner fast-forwards `main` to that head.
- [ ] Delete worktree `target/` directories (each agent at its task's
  end; the cargo registry stays in its volumes).
