# Handoff: M5 wave 1 to wave 2 (updated 2026-10-11)

A pausing point: M5's wave 1 is merged on its integration branch,
`laptop/m5-w1`, and wave 2 has not started. Read this first when picking
the repo up cold, then `docs/m5-plan.md` (streams, waves, ledger,
closing checks), `docs/decisions.md` (`## m5-owner`, `## m5-integration`,
`## m5-w0-harden`, the `## m5-speed*` and `## m5-open-*` sections,
`## m5-w1-owner`, the wave-1 streams' `## m5-docs`, `## m5-plugins`,
`## m5-tools`, `## m5-lsp`, `## m5-inspect`, `## m5-ipc`, and
`## m5-integration-w1`) for the reasons, and `docs/features.md` for M5's
boxes. `docs/m4-report.md` has M4's measured numbers.

## State

- **M0–M4 are complete**; `main` is at `88d4d99` (wave 0, its hardening,
  the speed review and the `m5-open-*` fixes, fast-forwarded by the
  owner). features.md: M0 20/20, M1 58/59, M2 30/30, M3 14/14, M4
  18/18, M5 2/12 (ten boxes and two exit criteria; ticked: the importers
  half of the importer box, from M2's tests, and the LSP box, from
  S-lsp's wave-1 tests). The open M1 box is the tree-sitter grammar,
  which M5 owns (S-lsp, wave 2).
- **Wave 1 is merged on `laptop/m5-w1`** (branched from `main` at
  `88d4d99`, decisions.md m5-w1-owner), all six streams, each reviewed
  and fixed in up to three rounds before its merge; local `run.sh ci`
  passed on the merged tree (all 32 steps):
  - **S-ipc** (`laptop/m5-ipc`, building): `strand get | toggle | call
    | clear` over the socket; the token overlay layer in
    `$XDG_STATE_HOME/strand/tokens.toml` with its own writer thread
    (`strand-tokens`), the "overlay wins" notice and `[clear]`; a hand
    edit read again rather than written over. Tests:
    `crates/strand/tests/m5/cli.rs` (`get_toggle_and_a_token_overlay_over_the_socket`,
    `call_runs_a_service_action`), `crates/strand-compiler/tests/token_overlay.rs`
    (11), `crates/strand/src/ipc/call.rs`'s and `crates/strand-core/src/ipc.rs`'s
    tests. D-Bus is wave 2.
  - **S-inspect** (`laptop/m5-inspect`, building): the compiler and
    scene half: `Instance::identity`, `cells_of` (three kept badges),
    `reset_subtree`, `source_hash`, `inspect_node`, `dump` (node refs
    `index.generation`, passwords redacted by the lock's `Secrets`
    rule) and `TokenTable::provenance`. Tests:
    `crates/strand-compiler/tests/inspect.rs` (12, from
    `identity_of_per_monitor_keyed_rows` to
    `a_dump_redacts_a_password_written_and_submitted_before_a_tick`) and
    `crates/strand-scene/src/provenance.rs`'s 6.
  - **S-lsp** (`laptop/m5-lsp`, building): the missing-key quick fix
    (one titled fix per candidate, preferred first) and extract
    component (`refactor.extract`, resolved lazily, refusing what would
    not check). Tests: `crates/strand-dev/tests/lsp.rs` and
    `crates/strand-dev/tests/reloads.rs::an_extracted_component_resets_only_its_subtree`;
    the LSP box is ticked.
  - **S-docs** (`laptop/m5-docs`, writing): `docs/spec.md` (skeleton,
    then every section's text, three review rounds), the guide's
    README, levels 1–3 (design.md's bar (a), byte for byte at the end)
    and outlines of levels 4–5, the ladder placements (design.md's
    "Also taught there" column) and `docs/guide/template/bar.strand`.
  - **S-tools** (`laptop/m5-tools`, writing): the theming design,
    reviewed three times and **approved by the owner** (decisions.md
    m5-tools, "the owner-approved theming design"): no per-app mappings
    or templates; 13 hue roles; extras and identity as `$palette.*`;
    templates with `.light()`/`.dark()` and channel methods; `strand
    export json | base24 | --template`; the `file` element (a top-level
    form, grammar.md `top_file`). Not built yet.
  - **S-plugins** (`laptop/m5-plugins`, writing): the builder's design
    notes, reviewed three times; the integrator accepted moving
    `main.rs`'s dispatch into `src/entry.rs` and the stock `strand run`
    reading plugin schema files. Not built yet.
- **Sizes on the merged tree** (budgets.rs's ledger line in the
  container): CPU-only `.text` **18,077,143 B** (Δ +115,328 over
  `f61acec`'s 17,961,815 B) of the **18,350,080 B** gate; GPU
  **21,974,679 B** of **22,282,240 B**. S-ipc (~87 KB against 35 KB) and
  S-inspect's library (~74 KB against 55 KB) overran their lines; they
  overlap when merged. The integrator accepted the overruns under the
  owner's speed-first policy (cold IPC and inspector code, no frame or
  swap path), kept the gates, and re-based the ledger ("Budgets now").
- **Owner and integrator decisions of 2026-10-11**:
  - the hardening's four interface choices kept (m5-w1-owner: node-only
    inspector messages, the ordering rule, `OVERLAY_ORIGIN`, the
    input-context route as text);
  - the theming hub approved as m5-tools records it; it is interface
    text in force in architecture.md ("The theming hub") and
    owner-approved language in spec.md, and the docs mark it "M5 wave 2"
    wherever they would otherwise present it as built;
  - the plugin notes' CLI move and schema-file reading accepted;
  - the wave-1 overruns accepted, gates unchanged, wave-2 lines set
    against the remaining headroom; the hub split into **S-palette** and
    **S-tools**; the `file` output host renders on the logic thread
    after the diff and writes on its own thread (`strand-files`); the
    `export` request is `Request::Export`, S-tools'
    (decisions.md m5-integration-w1).
- The CLI help no longer advertises `export gtk | kitty | hyprland`; it
  reads "write the palette: json | base24 | --template <file> (M5 wave
  2)" until S-tools wires it.
- `main` is at `88d4d99`; `laptop/m5-w1` is ahead by wave 1. The owner
  fast-forwards `main` to its head once the closing checks are green on
  that exact commit (the owner runs the ff+push; agents never push
  `main`).

## Next: M5 wave 2

From m5-plan.md's Waves table and stream sections. Wave 2 integrates on
`laptop/m5-w2`, created from `laptop/m5-w1`'s head (or from `main` once
the owner has fast-forwarded it there). The first commit is S-plugins'
pure move of `main.rs`'s dispatch and COMMANDS into `src/entry.rs`; every
other stream branches after it is merged. **Done:** the move is
`laptop/m5-w2`'s first commit (made by the integrator from `main`;
decisions.md m5-integration-w2); streams branch from `laptop/m5-w2`.

| Stream | Branch and worktree | Owns (wave 2 part) | First steps |
|---|---|---|---|
| S-plugins (building) | `laptop/m5-plugins`, `../strand-wt/plugins` | `src/plugin.rs`, `entry.rs`'s call into it, `strand-compiler/src/schema/plugins.rs`, the `plugin_hello`/`plugin_bare` examples, `tests/m5/plugin.rs`, the guide's plugin page | the pure-move commit (dispatch, COMMANDS and `main`'s body into `src/entry.rs`; `main.rs` keeps `process_setup!`, `main` and its allocator and THP tests) is done on `laptop/m5-w2`; branch from it, replace `main.rs`'s `strand::cli::main()` with `Shell::builder().run()`, then the builder, `--write-schemas`, the schema-file reader and the setup check, with decisions.md m5-plugins' test list. Budget 8 KB |
| S-ipc (building) | `laptop/m5-ipc`, `../strand-wt/ipc` | `strand-services/src/ipc_bus.rs`, `tests/m5/dbus_ipc.rs`, the `inspect.node`, `inspect.dump` and `reset{node}` arms over S-inspect's library | the D-Bus front end (`io.github.jakeb_grant.Strand`, the `Changed` signal, a second instance without the name, PSS with the name owned); then the inspect arms (`inspect.dump{surface}` passes the surface root's node ref to `Instance::dump`). Budget 37,400 B + 3 KB |
| S-palette (building) | `laptop/m5-palette`, `../strand-wt/palette` | strand-theme's `role.rs`, `palette.rs`, `import.rs`, `export.rs`; strand-scene's hue roots and `palette` reservation; the reservation error's line in `check/tokens.rs` | the 13 hue roles (`Role::COUNT` 62) and their derivation, then extras, identity and the persisted file's lines, then the reserved names (before the freeze), then the `json` and `base24` writers; m5-plan's S-palette tests. Budget 10 KB |
| S-tools (building) | `laptop/m5-tools`, `../strand-wt/tools` | strand-compiler's `template/**`, the `file` element (schema, `top_file` parser arm reviewed by S-lsp, checks), `Instance::take_outputs` (the first text's `Update.outputs`), `Instance::palette`/`hold_other_mode` (the first text's `other_mode`), the offline mount; strand-core's read-only `PersistStore`; `src/{new,export,report}.rs`, `run/outputs.rs` (`strand-files`), `run/request/{export,report}.rs`, `Request::Export`, the `new`/`export`/`report` rows | the template module and `export --template` offline, then `Request::Export` and the IPC path, then the `file` element with its output host (the parser change, design.md's theme block and `theme.strand` in one commit), then `strand new` and `strand report`; the swap bench's both-modes case. Budget 28 KB + 20 KB (report) |
| S-lsp (building) | `laptop/m5-lsp`, `../strand-wt/lsp` | `crates/strand-dev/tree-sitter-strand/**` | the tree-sitter grammar from spec.md's construct index (section 3, `top_file` included) and grammar.md, the external scanner, the queries, the committed `src/`; `crates/strand-dev/tests/tree_sitter.rs`; review S-tools' parser arm. 0 B in `strand` |
| S-docs (writing, no slot) | `laptop/m5-docs`, `../strand-wt/docs` | `docs/guide/**`, `docs/spec.md` | levels 4–5 (level 5 with a neutral CSS `file` example, no per-app template), and the guide's strand-check test (`every_guide_block_checks`, written with S-lsp's review in strand-dev); spec.md follows the hub as it lands |

S-inspect's render and binary half (pick, outline, flash; 20 KB) and
S-platform (xdg-activation; 20 KB) are wave-2 work in m5-plan that this
first launch leaves out; they take building slots as the streams above
free them.

How to launch it (as wave 1, run through workflows):

1. **Branch point.** The integrator creates `laptop/m5-w2` from
   `laptop/m5-w1`'s head (or from `main` if `git merge-base
   --is-ancestor laptop/m5-w1 main`). S-plugins' pure-move commit is
   merged into it first. A continuing stream brings its branch up to
   that head by a merge (never a force-push); a new one runs `git
   worktree add ../strand-wt/<name> -b laptop/m5-<name> laptop/m5-w2`.
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
   commit, measures `.text` once in its first week and stops at its
   ledger line, pushes only its branch, and deletes its `target/` at
   the end.
4. **Workflow per stream: build → review → verify → fix**, looped at
   most three rounds. The build agent finishes an item; a review pass
   reports findings; a verify pass confirms or drops each; a fix agent
   fixes the confirmed ones on the stream's branch. A clean round, or
   the third, ends the loop, and what is left goes to the integrator
   with the stream's hand-back.
5. **Integrator.** Merges finished items into `laptop/m5-w2`, runs the
   budgets job on the merged tree and checks the ledger line (within
   allocation: CPU-only Δ ≤ +2,626,873, GPU Δ ≤ +3,175,609, and each
   stream within its own line; m5-plan "Reading Δ as spend"), and
   records each merge under `## m5-integration`.
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

No item blocks wave 2. Each has its paragraph in decisions.md.

1. **Wave 1's `.text` overruns: accepted by the integrator, open to the
   owner's veto.** S-ipc (~87 KB on its branch against 35 KB) and
   S-inspect's library (~74 KB against 55 KB) were accepted under the
   speed-first policy, the gates unchanged and the ledger re-based to
   the gate's headroom (m5-integration-w1). The owner may still ask for
   cuts; the streams list what is left and what each would cost users
   (m5-ipc integrator decision a, m5-inspect "size remeasured").
2. **The theming hub's ledger lines** (S-palette 10 KB, S-tools 28 KB)
   are at the top of the design's 26–38 KB estimate and no longer draw
   on the review-fix reserve (61,001 B); the GPU build's margin is
   smaller (55,625 B), so the GPU gate binds first.
3. **The wave-2 launch leaves S-inspect's render half and S-platform
   for later slots** (m5-integration-w1); both stay in M5.
4. **The presented GPU leg's after-drop bound: answered.** A leak
   tripwire: growth between cycles (each later one within 4 MiB of the
   first) with 16 MiB presenting and 12 MiB reading back as backstops
   (m5-infra, m5-speed-gpu-bound, m5-open-budgets).
5. **`scripts/gpu-measure.sh`'s KMS-card run: not needed** (Q19 (A));
   owner-optional, never run by an agent.
6. **The duplicated image decoders: dropped** (m5-size, last
   paragraph).

Every M4 owner item was answered on 2026-10-10 (decisions.md m4-owner-2
and m4-owner-3; summarised under "M4 owner items" below).

## Budgets now

- **`.text`**, from budgets.rs's gate test (the ledger line; Δ is
  against M4's merged figures 15,657,671 / 19,041,095 B), the merged
  wave-1 tree on `laptop/m5-w1` in the container: CPU-only
  **18,077,143 B** (Δ +2,419,472; +115,328 over `f61acec`) of the
  **18,350,080 B** gate, 272,937 B of headroom; GPU **21,974,679 B** (Δ
  +2,933,584) of **22,282,240 B**, 307,561 B of headroom. The speed
  re-base (+2,325,712 CPU-only, +2,841,424 GPU) is no allocation's
  spend: spend is Δ − 2,288,656 CPU-only and Δ − 2,804,048 GPU, so M5
  has spent 130,816 B CPU-only and 129,536 B GPU. The gates are
  unchanged since the speed review (decisions.md m5-speed,
  m5-integration-w1). A later deliberate speed revert raises its gate by
  its recorded cost (m5-speed-policy); anything else that overruns is
  accidental growth.
  - GitHub's runner links about 2 KB smaller than the container; read
    the ledger against the same machine's baseline.
  - The ledger is re-based to the gate's headroom less its 64 KiB
    reserve (207,401 B CPU-only). Wave-2 and wave-3 lines (m5-plan's
    ledger): D-Bus 37,400; wave-2 IPC arms 3,000 (2,186 measured);
    inspector wave 2 20,000; report 20,000; xdg-activation 20,000;
    theming hub S-palette 10,000 and S-tools 28,000; plugins 8,000;
    tree-sitter 0; review-fix reserve 61,001 B (55,625 B on the GPU
    build after its GPU-only 40,000 B line). Within allocation: CPU-only
    Δ ≤ +2,626,873, GPU Δ ≤ +3,175,609.
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
   is open item 4 above.
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
  branches: the integration branch `m5-w1`, wave 1's streams (`m5-ipc`,
  `m5-inspect`, `m5-lsp`, `m5-docs`, `m5-tools`, `m5-plugins`). Wave 0's
  branches (`m5-plan`, `m5-core`, `m5-size`, `m5-infra`, `m5-w0`) and the
  speed review's were merged into `main` and deleted.

## Handoff checklist (wave 1)

Wave 0's checklist is in git history (`88d4d99`), M4's at `70cdc56`.

- [x] Wave 1's six streams merged on `laptop/m5-w1`, each reviewed and
  fixed before its merge; local `run.sh ci` passed on the merged tree.
- [x] The overruns decided and the ledger re-based (decisions.md
  m5-integration-w1); m5-plan's ledger, Streams table and this file
  agree.
- [x] The docs agree with each other after the merge: 25 confirmed
  coherence findings fixed (decisions.md m5-integration-w1).
- [ ] The full local set on the branch's final head, one at a time
  (`run.sh ci`, `matrix.sh`, the lock VM scenarios, `gpu.sh`), then
  GitHub CI green on that head. The integrator runs these after the last
  docs commit, so their results are in its hand-back, not here.
- [ ] The owner fast-forwards `main` to that head.
- [ ] Delete worktree `target/` directories (each agent at its task's
  end; the cargo registry stays in its volumes).
