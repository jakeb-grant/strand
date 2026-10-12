# Handoff: M5 after wave 2 (updated 2026-10-11)

A pausing point: M5's wave 2 (its first launch) and S-tools' wave-3
`strand new` and `strand report` are merged on the integration branch,
`laptop/m5-w2`, the `.text` gates are re-based to that tree under the
owner's size ruling, and the rest of wave 3 has not started. Read this
first when picking the repo up cold, then `docs/m5-plan.md` (streams,
waves, ledger, closing checks), `docs/decisions.md` (`## m5-owner`,
`## m5-integration`, `## m5-w0-harden`, the `## m5-speed*` and
`## m5-open-*` sections, `## m5-w1-owner`, the streams' `## m5-docs`,
`## m5-plugins`, `## m5-tools`, `## m5-lsp`, `## m5-inspect`,
`## m5-ipc`, `## m5-palette`, and the integrator's
`## m5-integration-w1` and `## m5-integration-w2`) for the reasons, and
`docs/features.md` for M5's boxes. `docs/m4-report.md` has M4's
measured numbers.

## State

- **M0–M4 are complete**, and **M5's wave 1 is on `main`** (`6cee276`,
  fast-forwarded by the owner). features.md: M0 20/20, M1 59/59 (the
  tree-sitter box, carried to M5, ticked in wave 2), M2 30/30, M3 14/14,
  M4 18/18, **M5 6/12 on `laptop/m5-w2`** (ten boxes and two exit
  criteria; ticked: CLI + IPC, `new`/`export`/`report`, the importers,
  plugins, tree-sitter and the LSP; open: the inspector, xdg-activation,
  the promoted GPU cost on hardware, the docs, and both exit criteria).
- **Wave 2 is merged on `laptop/m5-w2`** (branched from `main` at
  `6cee276`; its first commit `3e833c9` is the integrator's pure move of
  `main.rs`'s CLI into `src/entry.rs`). Six streams, each reviewed and
  fixed in up to three rounds before its merge, merged in this order
  (decisions.md m5-integration-w2 records each merge and the crate tests
  run in the container for it):
  - **S-lsp** (`0a1a558`): the tree-sitter grammar package
    `crates/strand-dev/tree-sitter-strand/` (`grammar.js`, the line-break
    scanner, highlights/locals/indents/injections queries, the generated
    `src/` held to `grammar.js` by `scripts/check-tree-sitter.sh`), and
    the guide's strand-check test `crates/strand-dev/tests/guide.rs`.
    Tests: `crates/strand-dev/tests/tree_sitter.rs`. grammar.md now has
    a closing section on how the tree-sitter grammar writes it.
    Size: 0 B in `strand` (nothing links it).
  - **S-ipc** (`8f8350a`): the D-Bus front end
    (`strand_services::ipc_bus`: `io.github.jakeb_grant.Strand` at `/`,
    typed methods `Get`, `Set`, `Toggle`, `Clear`, `Reset`, `Call`,
    `Reload`, the `Changed` signal, a second instance running without
    the name; spec.md 13 now names them), a bus `reload` that waits for
    its load, and the inspector's `inspect.node`, `inspect.dump` and
    `reset{node}` arms over S-inspect's library. Tests:
    `crates/strand/tests/m5/dbus_ipc.rs`, `ipc_bus.rs`'s units,
    `run/tests.rs` (`saves_reload_live_with_state_kept`,
    `a_cell_kept_at_boot_reaches_the_event_sink_as_a_late_watcher_hears_it`,
    `a_boot_kept_cell_reset_or_set_over_ipc_is_not_reported_kept`); the
    CLI + IPC box is ticked. Size as reported: CPU-only **+112,960 B**
    over `3e833c9` (D-Bus +52,736 B against 37,400 B; the inspect arms
    +60,416 B against 3,000 B, almost all S-inspect's wave-1 library,
    dead code until these arms called it).
  - **S-palette** (`8cc3982`): the 13 hue roles (`Role::COUNT` 62,
    derived from the sRGB primaries and secondaries harmonised toward
    the accent), every importer's extras in source order and the
    palette's identity (`$palette.*`, beside the tokens in
    `TokenTable::palette`, not springing), the persisted file's `name`
    and `extra` lines, the reserved names (`check::reserved_token`), and
    the `json` and `base24` writers. Tests: `crates/strand-theme/tests/
    {palette,export}.rs`, the negative fixture `reserved_token.strand`.
    Size as reported: **+56,128 B** CPU-only and GPU against its 10 KB
    line.
  - **S-plugins** (`dbc1ffb`): `strand::Shell::builder().service::<S>()
    .run()` (`main.rs` is now `Shell::builder().run()`),
    `--write-schemas`, `strand_compiler::schema::plugins` read by `strand
    check`, `strand run` and the LSP, `UnservedHost` for a file's
    service no binary serves, the `plugin_hello` and `plugin_bare`
    examples and `docs/guide/plugins.md`. Tests:
    `crates/strand/tests/m5/plugin.rs`, `crates/strand-dev/tests/lsp.rs`'s
    plugin tests and the units; the plugins box is ticked. Size as
    reported: **+25,408 B** CPU-only and GPU against its 8 KB line.
  - **S-tools** (`33c4490`): templates (`strand_compiler::template`, the
    other mode with `.light()`/`.dark()`, channel methods, `hex`/`argb`),
    `strand export json | base24 | --template [--palette] [-o]` offline
    and over the socket (`Request::Export`, agreeing with offline), the
    `file` element (top level and in trees) with its output host and the
    `strand-files` writer (atomic, owned, manifest, retries, `run:`
    debounce and timeout, `[replace]`). Tests: `crates/strand-compiler/
    tests/{template,hub,outputs}.rs`, `crates/strand/tests/m5/export.rs`,
    `run::outputs` and `run::request::export` units, and
    theme_swap_bench's `file_outputs_after_a_swap_are_under_five_milliseconds`
    (0.55 ms median). Size as reported: **+219,776 B CPU-only and
    +219,264 B GPU** against `6a33477` after 77 KB reclaimed with no
    feature dropped, against its 28 KB line; on its branch that crossed
    both gates (CPU-only by 98,199 B, GPU by 63,127 B).
  - **S-tools, wave 3** (`6b8b8e6`, merged by `8e84059`): `strand new
    [dir]` (the guide's hello bar into a new or empty directory,
    refusing a non-empty one) and `strand report [--json]` (globals,
    outputs, per-surface frame p50/p95/p99 over the last 600 frames,
    GPU status and lost devices, the PSS; through the snapshot hop),
    with three review rounds on a promoted frame's GPU time (timestamp
    queries in `strand-gpu`'s `timer.rs`, `Presented { surface, frame,
    at, waited, ran }`). Tests: `crates/strand/tests/m5/{new,report}.rs`,
    `renderer::summary` and `report`/`new` units,
    `crates/strand-render/tests/gpu.rs`'s frame-time tests. The
    `new`/`export`/`report` box is ticked; m5-plan's grim reference PNGs
    for `report` are missing (the two-output test in `report.rs` stands
    in), left to the owner to confirm. Size on the merged tree: **+70,208
    B CPU-only, +90,688 B GPU** over `db85876`.
  - **S-docs** (`732976f`): guide levels 4 and 5 (design.md's builds
    byte for byte, then one construct per step; level 5's `file`
    example as its own `colors.strand` with a neutral CSS template), the
    README and level 3 edits, and the guide tests in `tests/guide.rs`
    (every step checked, templates included). The docs box stays open
    (ladder test, generated reference, newcomer test and the freeze are
    wave 3).
- **Docs synced after the merges** (this commit): spec.md follows the
  hub as built (9.5's canonical hues, collisions and role extras; 9.7's
  retries, links, `when` and run rules; 9.8's `join`), names the D-Bus
  methods and `Changed` (13), the plugin schema files (5.6) and
  `export --palette` as offline only; design.md, grammar.md and
  architecture.md drop the "M5 wave 2" marks of what is built (the
  hub, D-Bus, plugins, tree-sitter) and say what is not (`strand new`,
  `strand report`, the inspector's pick/highlight/flash and its client,
  the reference generator); architecture.md takes the plugin stream's
  proposed as-built text (`Served.warnings`, `cli` without `main`,
  `strand::Builder`); the guide notes that `strand new` and `strand
  report` are not built yet; features.md adds S-tools' and S-docs'
  evidence under their open boxes.
- **Sizes on the merged tree, and the gates re-based** (decisions.md
  m5-size, wave 2, and m5-integration-w2; m5-plan's "Re-based after
  wave 2"). The streams measured against different bases, so their
  figures do not simply add. `scripts/size-report.sh` on `laptop/m5-w2`
  at `8e84059` gives CPU-only **18,555,351 B** (+478,208 over wave 1's
  merged tree) and GPU **22,473,111 B** (+498,432). The size pass found
  no gross verbosity and cut nothing; the owner accepted the overruns
  (2026-10-11), and the gates are now **18,874,368 B** CPU-only and
  **22,806,528 B** GPU (measured + the unbuilt lines + 64 KiB, rounded
  up to 256 KiB). The budgets test passes on the tree in the container.
- **Full checks have not run on the merged head**: only crate-scoped
  tests, fmt and workspace clippy ran per merge. `run.sh ci`,
  `matrix.sh`, the lock VM scenarios, `gpu.sh` and GitHub CI are next.

## Deferred from wave 2

- **S-inspect's render and binary half** (pick across surfaces, the
  outline and repaint flash on main, `inspect.pick`,
  `inspect.highlight`, `inspect.flash`, `strand-dev inspect`; 20 KB
  line): never launched; those commands still answer `unknown command`.
- **S-platform** (xdg-activation: `ActivationToken` from the
  notification server, `XDG_ACTIVATION_TOKEN` for launches; 20 KB
  line): never launched.
- **S-docs' wave-3 work**: the ladder test, `docs/reference/` and its
  generator, `tests/m5/newcomer.rs`, the agent-proxy harness, the spec
  freeze.
- **Proposed fixes for other owners, recorded and not made**:
  strand-render's `drop_index` (a dock drop on another monitor's dock
  lands one place on; m5-docs round 3); the checker's `rw_root` refusing
  writes to a config service's `rw` field (m5-docs round 1);
  strand-services' `Document::parse` reading a `#` comment line whole
  and keeping single quotes (m5-docs round 2); the tray schema's
  `open()` doc (m5-docs round 3 of wave 1); S-plugins' leftover-file
  log level.

## Next: M5 wave 3

From m5-plan.md's Waves table ("Closing" for the integrator):

1. Done: the owner's size ruling, the ledger and both gates re-based
   to the merged tree, the budgets test passing on it.
2. **The full local set on `laptop/m5-w2`'s head**, one at a time
   (`run.sh ci`, `matrix.sh`, the lock VM scenarios, `gpu.sh`), then
   GitHub CI green on that head, then the owner's fast-forward of
   `main`.
3. **The deferred streams take slots** (at most three building):
   S-inspect's render half and its `strand-dev inspect` client with
   write-back, and S-platform; S-docs' wave-3 writing (ladder, reference, newcomer, agent
   proxy) beside them, holding no slot.
4. **Closing**: the freeze text, m5-report, and this file as the M5
   handoff.

The launch procedure (branch point, slots, per-stream build → review →
verify → fix loop, integrator merges, full checks on the exact head,
the owner's ff of `main`) is wave 2's, in git history at `37f600b`
("How to launch it").

## Open items for the owner

Each has its paragraph in decisions.md.

1. **Wave 2's `.text` overruns: answered** (2026-10-11): "those size
   overruns still don't seem like that big of a deal in the scope of
   the entire binary. We're still pretty small." Accepted as cold code
   (IPC, import, export, writer, report); nothing cut, since the size
   pass found no gross verbosity; both gates re-based to the merged tree
   (decisions.md m5-integration-w2). Per stream as reported: S-ipc
   +112,960 B, S-palette +56,128 B, S-plugins +25,408 B, S-tools
   +219,776 B (wave 2) and +70,208 B (wave 3, on the merged tree).
2. **S-ipc's `Request(s json)` alternative** (about −18 KB): not taken;
   the ruling made the cut unnecessary, and spec.md 13's typed methods
   stand.
3. **The remaining lines** (inspector render half 20 KB, xdg-activation
   20 KB, the GPU-only outline and flash 19,520 B): answered by item 1;
   they are inside the re-based gates.
4. **The presented GPU leg's after-drop bound: answered** (a leak
   tripwire, m5-infra, m5-speed-gpu-bound, m5-open-budgets).
5. **`scripts/gpu-measure.sh`'s KMS-card run: not needed** (Q19 (A));
   owner-optional, never run by an agent.
6. **`strand report`'s grim reference PNGs** (m5-plan): not made; the
   two-output test in `tests/m5/report.rs` stands in for them. Confirm
   or ask for them (decisions.md m5-integration-w2, S-tools' wave-3
   merge).

Wave 1's overruns (S-ipc's socket half, S-inspect's library) were
accepted at wave 1's integration and stand (m5-integration-w1). Every M4
owner item was answered on 2026-10-10 (decisions.md m4-owner-2 and
m4-owner-3; summarised under "M4 owner items" below).

## Budgets now

These are the figures on `laptop/m5-w2` at `8e84059`, wave 2 merged
with S-tools' wave 3 (decisions.md m5-integration-w2).

- **`.text`**, from `scripts/size-report.sh` and budgets.rs's gate test
  in the container (the ledger line; Δ is against M4's merged figures
  15,657,671 / 19,041,095 B): CPU-only **18,555,351 B** (Δ +2,897,680)
  of the **18,874,368 B** gate, 319,017 B of headroom; GPU **22,473,111
  B** (Δ +3,432,016) of **22,806,528 B**, 333,417 B of headroom. The
  gates were re-based to this tree under the owner's ruling of
  2026-10-11 by the speed review's rule: measured + the unspent lines of
  unbuilt work (40,000 B CPU-only: the inspector's render half and
  xdg-activation; 59,520 B GPU, with the GPU-only outline and flash) + a
  64 KiB reserve, rounded up to 256 KiB (213,481 / 208,361 B of
  rounding, not spendable). The speed re-base (+2,325,712 CPU-only,
  +2,841,424 GPU) is no allocation's spend: spend is Δ − 2,288,656
  CPU-only and Δ − 2,804,048 GPU, so M5 has spent 609,024 B CPU-only
  and 627,968 B GPU. A deliberate speed revert raises its gate by its
  recorded cost (m5-speed-policy); other growth past the lines is read
  by its cause, as wave 2's was, and re-based or cut with a recorded
  reason (size gates are regression tripwires, not caps).
  - GitHub's runner links about 2 KB smaller than the container; read
    the ledger against the same machine's baseline.
  - Wave 3's lines inside the gates: inspector render half 20,000 B,
    xdg-activation 20,000 B, GPU-only outline and flash 19,520 B, and
    the 64 KiB reserve for review rounds. Within them: CPU-only Δ ≤
    +2,937,680, GPU Δ ≤ +3,491,536; the gates are hit at Δ +3,216,697
    and +3,765,433.
- **Memory** (tripwires for accidental regressions in CI, never
  runtime caps on a user's shell; set by judgement under the owner's
  2026-10-10 policy, with headroom for waves 1-2's IPC, inspector and
  LSP code, decisions.md m5-open-budgets): the design bar warns above
  42 MB (43,008 kB) and fails above 48 MB (49,152 kB); it measured
  37,768 kB in the container at `8e84059` (36,465 kB after the speed
  review) and 37,588–37,817 kB
  on GitHub. The full shell warns above 76 MB (77,824 kB) and fails
  above 86 MB (88,064 kB); 64,163 kB (161 entries) and 53,575 kB (4)
  here at `8e84059`, 61,428 kB (161 entries) after the speed review, 63,184–64,480
  kB in speed-code's runs, 62,503 and 68,040 kB (172 entries) on
  GitHub at `31da659` and `2fedfa2`. A target warning is info; raise
  them again with a recorded measurement rather than trimming a
  feature to fit.
- **Latency** (GitHub's timing job enforces; advisory on the laptop):
  reload p95 35 ms token / 50 ms markup at design.md's model, gated by
  the headless break; at `94fe372` token p95 17.1 ms against a break of
  19.6 ms. theme_swap_bench's 8-scope `spring(1600, 1)` whole swap 2.56
  ms against 5 ms at `94fe372` (4.31 ms at `laptop/m5-infra`); 1.50 ms
  in the container at `8e84059`, every case within its gate (swap
  medians 0.96–1.73 ms, `file` outputs after a flip 0.55 ms).

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
- Plans: `docs/m5-plan.md` (M5), `docs/m4-plan.md` (M4); interface text
  in `docs/architecture.md`.
- Remote branches: `origin/main` and M5's `origin/laptop/m5-*`
  branches: the integration branch `m5-w2` and wave 2's streams
  (`m5-ipc`, `m5-lsp`, `m5-palette`, `m5-plugins`, `m5-tools`,
  `m5-docs`), all merged into `m5-w2`. Wave 1's integration branch and
  `m5-inspect` are gone from the remote (wave 1 is on `main`); wave 0's
  branches and the speed review's were merged into `main` and deleted.

## Handoff checklist (wave 2)

Wave 1's checklist is in git history (`6cee276`), wave 0's at
`88d4d99`, M4's at `70cdc56`.

- [x] Wave 2's six streams merged on `laptop/m5-w2`, each reviewed and
  fixed before its merge; fmt, workspace clippy and the affected crates'
  tests passed in the container per merge (decisions.md
  m5-integration-w2).
- [x] The docs follow what landed (spec.md, the guide, design.md,
  grammar.md, architecture.md, features.md and this file; this commit).
- [x] The owner's ruling on wave 2's overruns (accepted), S-tools'
  wave-3 `new` and `report` merged, the budgets test on the merged tree
  and m5-plan's ledger and both `.text` gates re-based to it
  (decisions.md m5-integration-w2).
- [ ] The full local set on the branch's final head, one at a time
  (`run.sh ci`, `matrix.sh`, the lock VM scenarios, `gpu.sh`), then
  GitHub CI green on that head. The integrator runs these after the last
  docs commit, so their results are in its hand-back, not here.
- [ ] The owner fast-forwards `main` to that head.
- [ ] Delete worktree `target/` directories (each agent at its task's
  end; the cargo registry stays in its volumes).
