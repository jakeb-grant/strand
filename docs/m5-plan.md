# M5 plan

How M5 (features.md, "M5 Developer experience", weeks 45–52) is built
in parallel streams. design.md, architecture.md and features.md still
win. This file says only who builds what, in which order, and how each
part is proved. The readings design.md left open were put to the owner
as Q1–Q20, and the owner answered them on 2026-10-10 ("Owner
decisions" at the end; decisions.md `## m5-owner` has one paragraph
each). Readings that the docs already settle are agent decisions
(listed under "Agent decisions"). Each one gets a dated paragraph in
decisions.md, but they are not put to the owner.

Baseline: `main` at 70cdc56 (M0–M4 merged). Revised from the draft after
two critiques (the critique log stayed with the planning session), then
amended with the owner's answers: **M5 is not the 1.0 release** (Q20).
It closes as a normal milestone, with no version bump, tag, release
notes or release artifact; the owner wants to use Strand for a while
first.

Exit: newcomer builds a multi-monitor bar in 15 minutes · language spec
frozen.

## Goal

Make Strand usable by someone who did not build it, and freeze what the
language promises:

- **See and change a running shell**: the inspector; `strand get | set |
  toggle | watch | call` over the socket and D-Bus; runtime overlays for
  tokens and settings with the "overlay wins" notice; `strand report`.
- **Start and spread a shell**: `strand new`, `strand export
  json|base24|--template` and the `file` element, the learning-ladder
  guide and the language spec,
  editor tooling (tree-sitter, the last two LSP quick-fixes).
- **Fit the desktop**: xdg-activation for notification actions and app
  launches.
- **Freeze the language**: answer design.md's four open questions, write
  a self-contained `docs/spec.md` with a compatibility policy next to
  `docs/grammar.md`, and stop changing them.
- **Close what M4 handed over**: the promoted GPU cost on hardware,
  plugins (the importer box was ticked in 0-freeze), and handoff.md's
  deferred CI items.

No release: M5 ends without a version bump, a tag, release notes or a
release artifact (Q20).

All of it lands inside budgets that are nearly full. The CPU-only release
`.text` is 15,657,671 B of its 15,728,640 B gate (70,969 B left). The GPU
build is 19,041,095 B of 19,398,656 B (357,561 B left). The size strategy
(below) sets a provisional gate on day one, so no stream waits on
measurements.

## Exit criteria and how they are proved

### Newcomer builds a multi-monitor bar in 15 minutes

**The target (Q15, decided).** design.md's bar (a) (design.md:133–220).
It is the bar on every monitor with workspace dots, the focused title,
the clock with its calendar popup, the volume slider, battery and tray.
The learning ladder (design.md:121–131) defines levels 1–3 as exactly
what builds it: level 2 gives "Workspace dots, tray" and level 3 gives
"Clock with calendar, volume slider". The constructs it uses that the
ladder does not place (`on scroll`, `on secondary`, `hit: grow`, `-=`,
`split`) are placed by the agent decisions under S-docs before the guide
is final.

**`strand new` writes the level-1 hello bar** (design.md:25–34, "a working
shell") and a comment that points to the guide. The newcomer builds
everything else by following the guide, so the 15 minutes measure real
building, not reading a finished template.

Three layers of evidence:

1. **Automated newcomer path (gate, CI).** Owner: S-docs.
   `crates/strand/tests/m5/newcomer.rs::the_guide_builds_bar_a_on_two_outputs`.
   - It starts with a fresh `$HOME` in the container and runs `strand
     new`.
   - It applies each step of the guide's levels 1–3, in order, as a live
     save to a running `strand run` on two headless outputs (2560x1440@1
     and 1920x1080@1.25, as in acceptance.rs), with STRAND_MOCK services.
   - After each step:
     - `strand check` reports zero diagnostics;
     - the frame matches its grim reference on both outputs;
     - `strand watch --json` shows the kept cells and the reset cells
       that design.md:662–664's identity rules predict for that step.
       A step that wraps nodes in a new component resets them, and the
       guide says so.
   - The guide's code blocks are the test's inputs byte for byte, the way
     `fixtures.rs` ties design.md to its fixtures.
   - Time budget: the whole test runs in 120 s or less in the container.
     Each step takes one grim pair per output, not a frame series.
2. **Docs-only agent proxy (rehearsal, advisory, cuttable).** Owner:
   S-docs.
   - A fresh model session gets the published guide and nothing else. It
     works in the container with two headless outputs and a wall clock.
   - Its result is scored on inspector dumps (`inspect.dump`) against a
     written checklist in `docs/guide/target-checklist.md`: surfaces per
     output, the nodes present, the bindings, and the popup opening. It
     is not compared pixel for pixel.
   - Findings are fixed in the guide before the human trials. This tests
     design.md:769's "a grammar small enough to fit in one prompt". It
     gates nothing and is the first thing cut under time pressure.
3. **Timed human trials (tick the box; Q15 (i)).**
   - Two or three people who have never used Strand. The owner recruits
     them.
   - They use their own machines and their own layer-shell compositor
     (sway, Hyprland, niri, labwc, river or similar; not GNOME). Never
     the laptop: a compositor on the host is forbidden.
   - They get a release-profile binary from a CI build artifact (the
     budgets job's binaries, artifact `strand-release-binaries`; not a
     release) or
     build it from source, plus the guide. The clock starts once the
     binary is installed and `strand new` has run. It stops when bar
     (a) runs on every monitor.
   - If recruiting proves impossible, Q15's fallback (ii) applies: the
     automated test and the agent proxy carry the box, the trials are
     reported but not gating, and decisions.md records it.
   - The owner records times and stumbles in `docs/m5-report.md`. The
     owner's own run does not count.

### Language spec frozen

- design.md's open questions (design.md:780–783) are answered (Q1–Q4)
  and ticked, with one decisions.md paragraph each (done in 0-freeze,
  `laptop/m5-plan`).
- **`docs/spec.md` is self-contained normative text (Q5)**, written by
  S-docs. It is copied from design.md and adapted, never linked to it:
  "The rules", "Reactivity" (with "Who wins"), "Styling, tokens and
  theming"'s semantics, "Identity and state", the reload table, settings
  files, and architecture.md's reload rules where design.md is silent.
  It closes the gaps those sections leave. Then design.md points to
  spec.md for the semantics, so a later edit to design.md's rationale
  cannot quietly change frozen behaviour.
- **Compatibility section in spec.md.** In 1.x no syntax is removed or
  changes meaning, and builtin schema entries (props, elements, service
  fields, actions) are only added. Anything else waits for 2.0.
- **Every planned design.md edit lands before the freeze commit**: the
  ladder placements, the Async level (Q17), the open-question ticks, the
  GPU box wording (Q19), and the theming hub's: the `file` line joining
  design.md's theme code block and `theme.strand` with S-tools' parser
  and schema change, and the "(M5 wave 2)" marks dropped once the hub's
  code is merged. The hue role names and the reserved `palette`
  segment (S-palette) must land before the freeze, since a user token
  named `red` becomes an error. The Async level, the ticks, the export
  default (Q16), the inspector placement (Q9) and the box wording
  landed with 0-freeze; the ladder placements are S-docs'.
- What holds the freeze is a process plus the existing tests, not a hash
  pin. After the freeze, any change to grammar.md or spec.md needs a
  decisions.md paragraph citing the compatibility section. Typo fixes
  are exempt. The machine checks are the fixture tests, the tree-sitter
  conformance test, and the guide's ladder test.
- The freeze is the last commit of wave 3, after every stream that could
  add a schema entry. No stream adds syntax except the owner-approved
  top-level `file` element (grammar.md `top_file`, decisions.md
  m5-tools), and the owner's answers to Q1–Q4 add none.

## Size strategy

### Ledger and allocations

Code that must stay in `strand` (the gated binary). Estimates come from
the surveys. "Allocation" is what a stream may spend.

**Re-based after wave 1 (2026-10-11, decisions.md
m5-integration-w1).** Wave 1 overran its lines (S-ipc's commands and
overlay measured +87,104 B against 35 KB on its branch; S-inspect's
library 73,718 B against 55 KB), and the integrator accepted the merged
figure under the owner's speed-first policy: both are cold IPC and
inspector code on no frame or swap path. The gates did not change. The
230 KB total is replaced by what the CPU-only gate leaves: wave 2's
lines are set from realistic estimates against that headroom, less the
gate's 64 KiB reserve, and the review-fix reserve is what remains; it no
longer pays for the theming hub, plugins or the inspector. The rows
above the line are charged as measured; the rows below it are
allocations.

| M5 item | Estimate | Allocation | Charged (measured) |
|---|---|---|---|
| Wave 0: 0b's shared code (lib/bin split, request channel) and the wave-0 hardening | — | — | 15,360 B (decisions.md m5-integration, m5-w0-harden) |
| Wave 1, merged on `laptop/m5-w1`: S-ipc's `get`, `toggle`, `call`, `clear` and the token overlay layer; S-inspect's compiler and scene half (identity, cells, reset subtree, provenance, dump); S-lsp's `Suggestion` use and key suggestions | 46–123 KB (the surveys' 15–40, 30–80 and 1–3) | 93 KB (35 + 55 + 3) | **115,328 B** (merged tree against `f61acec`: 18,077,143 − 17,961,815 B; per branch S-ipc +87,104 B and S-inspect's library 73,718 B, which overlap when merged) |
| D-Bus front end (one `#[zbus::interface]`; zbus is already linked) | 15–40 KB | 37,400 B (40 KB less 0b's request channel, 2,600 B) | |
| Wave-2 IPC arms: `inspect.node`, `inspect.dump`, `reset{node}` in `Shell::request` (S-ipc in wave 2, over S-inspect's library) | 2,186 B, measured with the arms wired temporarily (decisions.md m5-inspect) | 3 KB | |
| Inspector wave 2: pick and the Router hook, the highlight outline and the repaint flash on CPU surfaces | 10–20 KB (the rest of the 30–80 KB survey after wave 1's 73.7 KB) | 20 KB | |
| `strand report` (snapshot hop, smaps_rollup, frame summary ring) | 10–25 KB | 20 KB | |
| xdg-activation (Dispatch impls, handler input context, ordered action queue, launch env) | 8–20 KB | 20 KB | |
| Theming hub, S-palette: 13 hue roles and extras and identity (4–6), the `json` and `base24` writers (3–4) | 7–10 KB | 10 KB | |
| Theming hub, S-tools: template IR and formats with the other-mode evaluation (8–12), the `file` element and its output host (10–14), `strand new`'s code (1–2; its template is `.rodata`, which is not gated), the `export` CLI and IPC request | 19–28 KB | 28 KB | **219,776 B** measured on `laptop/m5-tools` against `6a33477` after review round 2 (GPU 219,264 B; first measured 297,088 B and 296,768 B, then 77 KB reclaimed with no feature dropped): over its line and over both gates on the branch (CPU-only 18,448,279 B by 98,199 B, GPU 22,345,367 B by 63,127 B), so CI's `budgets` job fails on the branch; the cause is the estimate, not one bloated item (the code costs what the crates' code costs per line), and the cuts that would close it drop owner-approved features, listed for the owner (decisions.md m5-tools, wave 2 review round 2, item 7); wave 3's `strand new` and `strand report` measured **+70,208 B** CPU-only and **+73,280 B** GPU against `db85876` (18,555,351 B and 22,455,703 B; decisions.md m5-tools, "the `.text` of `new` and `report`"), the gate's re-base left to the integrator under the owner's 2026-10-11 ruling |
| Plugins: the schema-file reader and applier, `--write-schemas`, the builder's validation and the process-setup check (decisions.md m5-plugins, "size and speed") | 4–8 KB | 8 KB | |
| tree-sitter grammar (S-lsp; linked only into strand-dev's tests) | 0 B in `strand` | 0 | |
| Reserve for review-fix rounds (M4's cost 2–9 KB each), held by the integrator | — | 61,001 B | |
| **Total M5 (CPU-only)** | | **338,217 B**: the gate (18,350,080 B) less the 64 KiB reserve, less the speed re-base and M4's baseline | 130,688 B charged so far (130,816 B by the ledger line's formula, which also counts the 128 B `f61acec` reads over the speed review's merged tree) |
| GPU-only: outline and flash on `GpuPresent` surfaces, `report`'s GPU section | 15–40 KB | 40 KB (GPU build only) | |

How the lines add up (CPU-only): the merged wave-1 tree is 18,077,143 B
against the 18,350,080 B gate, 272,937 B of headroom; less the gate's
64 KiB reserve (65,536 B, not spendable) that leaves 207,401 B; the
wave-2 and wave-3 lines above take 146,400 B, and the review-fix reserve
is the 61,001 B left. The GPU build (21,974,679 B of 22,282,240 B,
307,561 B of headroom, 242,025 B after its 64 KiB reserve) carries the
same lines plus the GPU-only 40 KB, so its review-fix margin is 55,625 B:
the GPU gate binds first, and the integrator reads both ledger lines at
every merge. The superseded allocations (35 KB IPC, 55 KB inspector,
15 KB new/export plus 11–23 KB from the 29,176 B reserve, plugins from
that reserve) are in git history at `3493b04`.

Optional items, paid from the reserve only if the reserve allows:
check-time `import()` validation (2–4 KB) and the "says why" lines in the
node pane (2–5 KB).

### The CPU-only gate (Q6)

Decided (owner, 2026-10-10): **(A) a provisional re-base on day one,
lowered as reclaims land, tightened at close.**

- On day one the integrator sets `TEXT_GATE_CPU` (done on
  `laptop/m5-plan`) to the baseline plus
  the 230 KB allocation, rounded up to 64 KiB: 15,657,671 + 230,000 =
  15,887,671, which rounds to **15,925,248 B** (+196,608 B over 15 MiB).
  The integrator records it in decisions.md `m5-integration`.
- The 64 KiB rounding (37,577 B) is not spendable. The ledger holds
  streams to their allocations, and only the gate test sees the rounded
  figure.
- Each reclaim that lands lowers the gate by the bytes it measured,
  rounded to 64 KiB. The gate is never raised again in M5 without the
  owner. If the reclaims reach about 159 KB, the gate returns to
  15 MiB.
- At M5 close the integrator proposes to the owner the next 64 KiB above
  the final measured size as the gate from then on.
- **Now (after wave 0): 15,859,712 B.** Reclaims 1 and 3 measured
  −37,056 B, and the gate is recomputed from the lowered baseline:
  15,620,615 + 230,000 = 15,850,615, rounded up to 64 KiB (decisions.md
  m5-integration; 9,097 B of rounding, not spendable).
- **Now (after the speed review): 18,350,080 B** (decisions.md
  m5-speed). The owner's policy of 2026-10-10 puts speed first where the
  trade is sensible and has agents set gates by judgement. The speed
  profile (`strand` at opt 3 runs the whole binary's fat-LTO pass at 3)
  and the speed-code reverts measured 17,961,687 B on the merged tree,
  +2,325,712 B over the pre-speed 15,635,975 B, a re-base rather than M5
  spend. The gate is that size plus M5's unspent allocation (214,640 B)
  plus a 64 KiB reserve, rounded up to 256 KiB (108,217 B of rounding,
  not spendable). "Lowered as reclaims land" and "never raised without
  the owner" are superseded by that policy: a deliberate speed choice
  re-bases the gate with its measured reason (decisions.md
  m5-speed-policy).

Not taken: **(B) keep 15 MiB, as m4-integration-w2 did (decisions.md:9573:
"the code moved, not the gate").** That would have left 71 KB plus
whatever the reclaims recover (realistically 20–80 KB), and meant
cutting runtime scope: the D-Bus front end (a dropped feature, leaving
the CLI + IPC box open), the inspector's outline and flash, and
`report`'s frame percentiles.

### GPU gate (Q6, second part)

Kept at 18.5 MiB until the speed review. The GPU build pays the 230 KB
plus its own 40 KB. **Now: 22,282,240 B** (decisions.md m5-speed): the
merged tree's 21,857,111 B (+2,841,424 B over the pre-speed 19,015,687
B) plus M5's unspent GPU allocation (258,032 B of 270,000) plus a 64 KiB
reserve, rounded up to 256 KiB (101,561 B of rounding).

### Reclaims (S-size, wave 0, behaviour-neutral, measured)

Each reclaim is one commit with before and after numbers. It is kept
only if it saves bytes and every timing step stays green on GitHub's
`timing` job.
- **Drop `image` from strand-theme's runtime `[dependencies]`**
  (Cargo.toml:17). Decode WebP wallpapers with `image-webp` 0.2.4
  directly, as strand-render does. `image-webp` is already linked and
  already built at opt "z", so only `image`'s `DynamicImage` and
  conversion glue goes away. Expect tens of KB. The dev-dependency stays.
  Proof: strand-theme's image tests and `image.rs`'s WebP cap test pass
  unchanged.
- **resvg's raster path at opt "z"**: tried and **withdrawn** (m5-size
  reclaim 2). Icons rasterise once per size, but bindable SVG redraws on
  the render path every animation frame, about 3.7 times slower at "z"
  (210 µs against 56 µs at opt 3 for a 300 × 40 meter); Cargo.toml says
  why in its place. Do not retry it without a faster bindable-SVG path.
- **blake3 at opt "z"**: tried and **not taken** (m5-size reclaim 3): it
  grew both binaries (+2,048 B CPU-only, +2,368 B GPU), since the
  hashing is its SIMD assembly.
- **Candidates found by `scripts/size-report.sh`**, the tool written
  below. It finds the top symbols per crate under fat LTO. Each change
  is taken only if it is behaviour-neutral, the owning crate agrees, and
  the timing steps stay green.
- **Not taken**:
  - nucleo at opt "z": no gate measures per-keystroke search time, and
    list_scroll_bench (strand-render) never calls nucleo. Taking it
    would need a new search bench first.
  - lld ICF: it needs RUSTFLAGS, which packagers would not carry.
  - panic=abort: catch_unwind is load-bearing.
  - Dropping features.
  - Nightly build-std: Rust is pinned at 1.97.0.
- **`strand fmt` stays in `strand` (Q8).** `size-report.sh` reports
  `strand_compiler::fmt`'s cost under fat LTO. Fat LTO drops it only
  because `strand/src/fmt.rs` is its only caller in the binary, which
  holds today. Moving it would un-tick features.md:67 (M1) and edit
  architecture.md:2489, so it is offered to the owner only with the
  measured figure.

Expected: 20–80 KB. Recorded as measured, not promised.

### How size is measured without burning build slots

- `scripts/size-report.sh` (S-infra, wave 0): run once for the baseline
  and on demand. It runs two fat-LTO builds (CPU-only and GPU) and
  reports `.text`, a per-crate breakdown (`nm -C --size-sort -S`,
  attributed by crate prefix) and the top 200 symbols. The baseline goes
  into decisions.md `m5-size`.
- **The ledger line comes from builds CI already makes.** The budgets job
  already builds both release binaries. S-infra adds a ledger line to
  its output, CPU-only and GPU `.text` against the gates and against the
  baseline:

      .text CPU-only <size> (Δ vs 15,657,671; gate <g>) · GPU <size> (Δ vs 19,041,095; gate <g>)

  **Reading Δ as spend.** Δ is against M4's baseline, so it already nets
  out wave 0: the reclaims (−37,056 B CPU-only, −37,376 B GPU) and 0b
  (+12,544 / +8,512, charged to the D-Bus line and the reserve). What
  M5's allocations have spent is **Δ + 37,056** CPU-only (**Δ + 37,376**
  GPU), 0b included. "Within allocation" means Δ + 37,056 ≤ 230,000,
  that is **Δ ≤ +192,944** CPU-only (GPU: Δ + 37,376 ≤ 270,000, Δ ≤
  +232,624). The gate (15,859,712 B) is hit only at Δ = +202,041, and
  the 9,097 B between is the gate's rounding, which no stream may spend
  (decisions.md m5-w0-harden). The wave-0 merged tree read Δ −24,576,
  so 12,480 B spent (0b, to within 64 B); after the wave-0 hardening it
  read Δ −21,760 after its first round and Δ −21,696 after its second
  (15,635,975 B), so 15,360 B spent and 214,640 B left.
  **With the speed re-base** (decisions.md m5-speed), Δ also carries
  the speed profile and the speed-code reverts, +2,325,712 B CPU-only and
  +2,841,424 B GPU on the merged tree, which no allocation pays. Spend
  is then **Δ − 2,288,656** CPU-only (Δ + 37,056 − 2,325,712) and **Δ −
  2,804,048** GPU. "Within allocation" is Δ ≤ +2,518,656 CPU-only (GPU Δ
  ≤ +3,074,048); the gates (18,350,080 / 22,282,240 B) are hit at Δ =
  +2,692,409 and +3,241,145, the 64 KiB reserve and the rounding
  between. The merged tree reads Δ +2,304,016 CPU-only (17,961,687 B;
  15,360 B spent, 214,640 B left) and GPU Δ +2,816,016 (21,857,111 B;
  11,968 B spent, 258,032 B left). A later deliberate speed revert adds
  its measured cost to the re-base and the gate, recorded.
  **After wave 1** (decisions.md m5-integration-w1) the merged tree on
  `laptop/m5-w1` reads CPU-only 18,077,143 B (Δ +2,419,472; 130,816 B
  spent) and GPU 21,974,679 B (Δ +2,933,584; 129,536 B spent). With the
  ledger re-based to the gate's headroom, "within allocation" is the
  gate less its 64 KiB reserve: **Δ ≤ +2,626,873** CPU-only (GPU **Δ ≤
  +3,175,609**), and each stream is held to its own line above.

- Streams do not build release binaries routinely. The two least
  predictable items (D-Bus, inspector) each take one CPU-only
  measurement in their first week, inside their own slot. Every merge
  carries the ledger line from the integrator's budgets run on the
  merged tree, not the branch. M4's wave 2 passed per branch and failed
  merged.
- A stream that would overrun its allocation stops and asks the
  integrator. It never cuts an opt-level on a hot path to make room. A
  deliberate speed revert (a cache sized back up, a size-kept algorithm
  replaced, a hot path's opt-level raised) is not spend: it lands with
  its measured gain and `.text` cost, and the integrator raises the gate
  by that recorded amount (decisions.md m5-speed-policy).

### Memory (reported, never a runtime cap)

Each byte of `.text` is roughly a byte of bar PSS (decisions.md
wave4-exitMemory). +230 KB, less the reclaims, moves the design bar by
about 0.15–0.25 MB. The bar is about 36.5–37.8 MB after the speed
profile against the 42 MB target (WARN) and the 48 MB ceiling (fail),
raised with headroom for waves 1-2 (decisions.md m5-open-budgets; it
was 34.1–34.9 MB against 34.8 / 38.9 MB when this plan was written).
- budgets.rs's design-bar run also reports PSS with the D-Bus name owned
  (S-ipc).
- The existing per-mapping PSS print (budgets.rs:1187) is copied into
  m5-report.md as the resident-code split. It is not offered as a
  replacement gate: the binary's resident file share depends on the page
  cache (decisions.md wave4-exitMemory: 8.5 MB on one machine, 16 MB on
  another, for the same build).
- The owner's rule applies: memory budgets are benchmark targets for CI,
  never runtime caps on a user's shell.

## Ground rules

These are M4's rules (m4-plan.md "Ground rules"), with the changes noted.

- **Branches.** Each wave integrates on a branch of its own,
  `laptop/m5-w<N>`: wave 0 on `laptop/m5-w0` (merged into `main` and
  deleted), wave 1 on **`laptop/m5-w1`** (all six wave-1 streams
  merged), wave 2 on `laptop/m5-w2`, created from `laptop/m5-w1`'s head.
  Each stream works in `../strand-wt/<name>` on `laptop/m5-<name>`,
  created from the current integration branch's head, or from `main`
  once the owner has fast-forwarded `main` to that head (then the two
  are the same commit), never from an older `main`. The integrator
  merges finished streams into the wave's branch, and `main` moves by
  fast-forward when local `ci`, `matrix.sh` and GitHub CI are green on
  the exact commit (the owner runs the ff+push; agents never push
  `main`). Push only your own branch; never force-push. (Changed in
  m5-w0-harden from "created from `main` after 0b" and
  "`laptop/integration-m5`", at the wave-0 handoff for the branch point
  after the fast-forward, and at m5-w1-owner for one branch per wave;
  decisions.md m5-w0-harden, m5-integration, m5-w1-owner,
  m5-integration-w1.)
- **Builds (changed).** At most three worktrees hold a build slot at once
  (`CARGO_BUILD_JOBS=6` each). **The integrator's worktree counts as one
  slot whenever it builds or runs `run.sh ci`, `matrix.sh`, `gpu.sh` or
  `lockvm.sh`.** Streams that are writing hold no slot. Streams run
  crate-scoped tests only. The integrator alone runs `run.sh ci` or
  `cargo test --workspace`, one at a time. Delete your worktree's
  `target/` when the stream ends.
- **Decisions.** Each stream appends only under its own heading
  `## m5-<name>`, one dated paragraph per decision. Owner answers go
  under `## m5-owner`.
- **Interfaces.** All cross-crate interface text lands in 0a (docs), and
  the shared code in 0b, before any stream branches. After that a stream
  edits only its own crate's architecture text.
- **Invented CLI surface is recorded.** Each flag, command name, file
  path or IPC command not named in design.md gets a decisions.md
  paragraph from the stream that adds it. That covers `--json` on `get`,
  `call` and `report`; `strand clear`; `export -o`; plugin
  `--write-schemas`; the schemas directory; and the `inspect.*` command
  names.
- **Shared boxes.** Two features.md boxes span streams: CLI + IPC; and
  "`strand new`, `export`, `report`" (S-tools and S-palette). Streams
  append evidence inside the parenthetical. Only the integrator ticks. (The importer/plugins box
  was split per Q17: its importer half was ticked in 0-freeze from M2's
  tests and is closed; the plugins box is S-plugins' alone.)
- **Test binaries (new).** New integration tests in `crates/strand/tests`
  go into one binary, `m5.rs`, with a module file per stream under
  `tests/m5/`, or into an existing binary (`reloads.rs`,
  `acceptance.rs`). Each test binary links the whole graph, and the
  existing set already costs about 39 s of linking. Lock tests extend
  `tests/lock.rs`, and the scenario list in
  `scripts/lockvm/scenarios/strand_lock.sh` is updated in the same
  commit.
- **Laptop rules** (CLAUDE.md, restated for M5's new risks):
  - No test, inspector or `strand call` ever targets `wayland-1` or the
    live session bus. Every test starts its own headless sway and
    `dbus-run-session`.
  - `strand new` and `strand export` tests write only into temporary
    `XDG_CONFIG_HOME` directories.
  - Pick mode is tested on headless sway only. Anything that touches lock
    surfaces runs in `lockvm.sh`.
  - The GPU measurement on a KMS card, if the owner keeps that wording
    (Q19), is owner-run only, never by an agent.
  - Human trials happen off the laptop.
- **Integrator-owned files**: root `Cargo.toml`, `Cargo.lock`,
  `.github/workflows/ci.yml` and `scripts/container/ci.sh` once wave 0
  ends; budgets.rs's gate constants; `crates/strand/tests/m5.rs`'s
  module list; `docs/m5-report.md`; handoff.md.

## Streams

| Stream | Waves | Owns (exclusive) | Closes |
|---|---|---|---|
| Integrator | all | 0a interface text; the files above; merges; the ledger; the gate changes; m5-report; handoff; the closing checks | shared box ticks; exit evidence |
| S-core | 0b | the shared-code PR (below); then dissolves | interfaces as code |
| S-size | 0 | the reclaim edits, `[profile.release.package]` entries (merged by the integrator), budgets.rs's opt-level test | size baseline; lowered gate |
| S-infra | 0, then on call | `scripts/container/*`, `scripts/size-report.sh`, `scripts/gpu-measure.sh` (owner-run only), `.github/*` until wave 0 ends, the budgets ledger line, the three spikes, deferred CI tests in crates no stream owns | wave-0 tooling; spikes; deferred CI items |
| S-ipc | 1, 2 | `strand/src/ipc/{query,call,overlay}.rs`, `run/request/{get,set,toggle,call,clear}.rs`, the request channel's arms in `run/request/{reload,watch}.rs` (a bus `reload` and `watch`), the `EventSink` it hands `Live::events_to`, the COMMANDS table and its own rows (in `strand/src/entry.rs` after the wave 1 → 2 move, a shared dispatch point; S-tools changes the existing `new`, `export` and `report` rows there as one-line entries), the token overlay layer (strand-compiler `instantiate/overlay.rs`, beside settings' `set_overlay`), `strand-services/src/ipc_bus.rs`, `tests/m5/{cli,dbus_ipc}.rs`; in wave 2 also the `inspect.node`, `inspect.dump` and `reset{node}` arms over S-inspect's library (their handlers in `run/request/inspect.rs`, which S-inspect reviews) | CLI + IPC box |
| S-inspect | 1, 2, 3 | `strand-compiler/src/instantiate/inspect.rs`, `strand-scene/src/provenance.rs`, `strand-render/src/inspect/**` (CPU and GPU paths), `run/request/inspect.rs` (the pick, highlight and flash arms; S-ipc writes the node, dump and reset arms there in wave 2), `ipc/inspect.rs`, `strand-dev/src/inspect/**`, `tests/m5/inspect.rs`, its `tests/lock.rs` case | Inspector box |
| S-lsp | 1, 2 | `strand-dev/src/{diag.rs, refactor/**}`, `server.rs`'s capability block, `check/tree.rs` and `check/expr.rs` key suggestions, `diagnostic.rs`'s `Suggestion` use, `crates/strand-dev/tree-sitter-strand/**` | LSP box; both tree-sitter boxes (M1 line 69, M5) |
| S-platform | 0 (spike), 2 | `strand-surface/src/activation.rs` and the key serials in `manager/seat.rs`; the activation parts of strand-services' notifications, apps and child; the activation path in `strand/src/services/adapter.rs`; strand-fake-wayland's `xdg_activation_v1`; the handler input context's route (architecture.md): the serial beside `NodeEvent` in strand-render's router (`input.rs`), `ToLogic::Event`'s `input` field and its arm in `run/shell.rs` (and `demo/host.rs`'s `Intent::Event`), `Instance::event`'s `input` parameter and the VM's per-invocation frame in strand-compiler, `DynService::action`'s `ActionCtx`, and the services→main token channel in `strand/src/run/` | xdg-activation box |
| S-palette | 2 | the theming hub's palette half: strand-theme's `role.rs` (13 hue roles, `Role::COUNT` 62), `palette.rs` (extras, identity, the persisted file's `name` and `extra` lines, the harmonised derivation in `Partial::fill`), every importer's extras in `import.rs`, `export.rs` (the `json` and `base24` writers); strand-scene's hue roots and the reserved `palette` first segment in `tokens.rs`, with the checker's reservation error (a line in strand-compiler `check/tokens.rs`); strand-theme's tests for them | `new`/`export`/`report` box (shared with S-tools) |
| S-tools | 1 (writing), 2, 3 | the theming hub's tools half: strand-compiler's `template/**` (the template module), the `file` element's schema entry, parser arm (`top_file`, reviewed by S-lsp) and checks, `Instance::take_outputs` (the first text's `Update.outputs`), `Instance::palette`/`hold_other_mode` (the first text's `other_mode`), the pinned-palette offline mount and `vm/theme.rs`'s public persisted-palette path; strand-core's read-only `PersistStore`; in `strand`: `src/{new.rs, export.rs, report.rs}`, the output host `run/outputs.rs` and its `strand-files` writer thread, `run/request/{export,report}.rs` and `Request::Export`'s `parse_m5` arm (one-line entries), the existing COMMANDS rows for `new`, `export`, `report`; theme_swap_bench's both-modes case; strand-render `frame_summary`, strand-surface `globals()`/`outputs()`, `tests/m5/{new,export,report}.rs`; optional `check/imports.rs` | `new`/`export`/`report` box (shared with S-palette) |
| S-docs | 1–3 (wave 0's writing moved to wave 1) | `docs/guide/**` (the `strand new` template included), `docs/spec.md`, `docs/reference/**` (generated), `strand-dev/src/reference.rs`, the ladder test, `tests/m5/newcomer.rs`, the agent-proxy harness, the design.md edits for the ladder and Async | Docs box; the newcomer exit's automated layers |
| S-plugins | 1 (writing), 2 | the builder API (`strand/src/plugin.rs`, `lib.rs`'s `Shell::builder`), the pure-move commit of `main.rs`'s dispatch into `strand/src/entry.rs` (not the COMMANDS table after it; the move landed as `laptop/m5-w2`'s first commit, made by the integrator, m5-integration-w2), the `[[example]]`s `plugin_hello` and `plugin_bare`, plugin schema loading in `strand/src/services/mod.rs` and `strand-dev/src/server.rs` (a small PR S-lsp reviews), `strand-compiler/src/schema/plugins.rs`, `tests/m5/plugin.rs`, the guide's plugin page | plugins half |

Per-file ownership in shared crates:
- strand-theme: S-palette owns `role.rs`, `palette.rs`, `import.rs` and
  `export.rs` for the theming hub; the rest is unchanged in M5.
- strand-scene: S-inspect owns `provenance.rs`; S-palette owns the hue
  roots and the `palette` reservation in `tokens.rs`.
- strand-core: S-tools owns the read-only `PersistStore`; strand-core's
  `ipc.rs` match arms are shared dispatch points (below).
- strand-services: `ipc_bus.rs` belongs to S-ipc; the activation parts of
  notifications, apps and child belong to S-platform.
- strand-render: `inspect/` belongs to S-inspect; `frame_summary`
  belongs to S-tools. The shared `Renderer` fields both use land in 0b.
- strand-surface: `activation.rs` belongs to S-platform;
  `globals`/`outputs` belong to S-tools.
- strand-dev: `server.rs`'s capability block belongs to S-lsp. S-inspect,
  S-docs and S-plugins add modules through small PRs that S-lsp reviews.
- strand-compiler: S-inspect owns `instantiate/inspect.rs`; S-ipc owns
  `instantiate/overlay.rs` and the `toggle`/`call` entry points; S-lsp
  owns `check/` key suggestions; S-plugins owns `schema/plugins.rs` (the
  plugin schema files' reader, a new module beside `schema/mod.rs`);
  S-tools owns `template/**`, the `file` element's schema entry, parser
  arm and checks (the parser arm a small PR S-lsp reviews, since the
  tree-sitter grammar follows it), `Instance::take_outputs`
  (the first text's `Update.outputs`), `Instance::palette`/`hold_other_mode`, the offline mount and `vm/theme.rs`'s
  public path; S-palette adds one line in `check/tokens.rs` for the
  reserved names. The handler input context's types
  (`ActionCtx`, `InputCtx`, `ServiceHost::action`'s parameter) landed in
  0b, defined in `strand_core::action` since m5-w0-harden so
  strand-services can name them; its route from the seat to the VM and the token channel are
  S-platform's (wave 2).
- strand (the binary's crate): the shared dispatch points — the
  `parse` arms in strand-core's `ipc.rs`, `Shell::request`'s arms,
  `ToMain`/`ToLogic` and `Shell::handle`'s arms, `Live`'s fields,
  `lib.rs`'s `mod` list and `strand::cli` re-exports, `ipc/mod.rs`'s
  re-exports, and, from the wave 1 → 2 boundary, `entry.rs`'s COMMANDS
  rows and dispatch arms (S-ipc's table; S-tools wires its three
  existing rows) and `Real::served`'s one use in
  S-ipc's `run/request/call.rs` — take one-line entries from each stream that needs them
  and are merged by the integrator (architecture.md, "Ownership").

## Wave 0

The owner answered every question on 2026-10-10, so no wave waits on the
owner. 0a and 0-freeze land together on `laptop/m5-plan`.

### 0a: interface text (docs only, integrator, no build slot)

- **IPC protocol in strand-core.** `Request`, the answer and the event
  encoding move from `crates/strand/src/ipc.rs` into
  `strand-core::ipc` (types and JSON-line codec only, no I/O).
  - `strand` (the socket server and CLI), strand-services (the D-Bus
    front end) and strand-dev (`strand-dev inspect`) all name the same
    types. strand-dev needs a direct strand-core dependency to name them
    (linking it through strand-compiler is not enough; S-inspect adds
    it, the integrator merges `Cargo.lock`). The socket's path is
    `strand_core::ipc::socket_path()`, shared by every client.
  - Protocol v1 gains commands only:
    - `get{path}`, `toggle{path}`, `call{target,args}`, `clear{path}`,
      `report{}`;
    - the inspector's `inspect.pick{on}`, `inspect.node{id}`,
      `inspect.dump{surface?}`, `inspect.highlight{id?}`,
      `inspect.flash{on}`;
    - the existing `reset` gains a `{node}` form for "reset subtree".
  - No paging. `MAX_LINE` (64 KiB) limits only incoming requests, and
    answers are bounded by the 1 MiB queue.
  - Unknown commands are refused without closing the connection, as
    today.
- **The D-Bus name (Q12).** Bus name and interface
  `io.github.jakeb_grant.Strand`, object path `/`, owned when `strand
  run` starts; a second instance runs without D-Bus IPC. The name lives
  in one constant, `strand_core::ipc::DBUS_NAME`, so a rename is a
  one-line change: if zbus's `#[interface(name = …)]` needs a literal,
  that literal is the only other spelling, and a test fails unless it
  equals the constant. The owner may rename it before 1.0, and spec.md
  says so.
- **Request channel from the services thread to logic.** strand-core
  gains `IpcRequests`, a sender of `(Request, reply: oneshot)` that the
  logic loop polls as a non-blocking source, beside the socket.
  - The D-Bus interface (on the services thread's tokio runtime, which
    owns the zbus connection) translates each call into a `Request`. It
    awaits the reply without blocking logic.
  - `watch` becomes a `Changed` signal fed by the same events the
    socket's watchers get, through `strand_core::ipc::EventSink`
    (`Live::events_to`; landed in the wave-0 hardening).
  - It lives in strand-services because that thread owns the async
    runtime and the bus. The binary has no executor of its own.
- **Snapshot hop between main and logic.** A logic→main request
  `Snapshot(kind, reply)` and its main→logic reply (`ToLogic::Snapshot`).
  It is used by `report` (compositor globals, outputs, frame summaries,
  GPU status) and by the inspector (layouts, `Layout { node }`, answered for each surface showing the
  node: logic names nodes only, never a `SurfaceId`).
  It is one interface for both. The inspector's other messages share the
  channel: `ToMain::{Highlight, DamageFlash, PickMode}`, and a click's
  pick (or pick mode ending on main's side) comes back unasked as
  `ToLogic::Picked(PickEvent)` (architecture.md; landed in the wave-0
  hardening).
- **strand-compiler `Instance` inspection** (read-only, never read by
  evaluation):
  - `identity(node) -> Identity{instance_path, key, id, sibling_index,
    scope}`;
  - `cells_of(node) -> Vec<CellInfo{path, kept, persisted, layer}>`;
  - `reset_subtree(node)`;
  - `source_hash(file)`.
- **The token overlay layer** (S-ipc): `Instance::set_token_overlay(path,
  value)` and `clear_token_overlay(path)`, mirroring settings'
  `set_overlay`/`clear_overlay`. `TokenTable`'s origin of an overlaid
  token is the reserved string `strand_scene::tokens::OVERLAY_ORIGIN`
  (`overlay`; there is no `TokenOrigin` type).
  A save that the overlay shadows raises the existing `Shadowed`-style
  notice: `accent: file changed but runtime overlay wins [clear]`.
  Addressed with the token spelling, `strand set '$accent' '#ff8800'`,
  global scope, persisted beside settings' overlay (Q10).
- **strand-scene**: `TokenTable::provenance(path, scope) ->
  Vec<ProvenanceStep>`. It walks `TokenExpr::Ref` and the `TokenScope`
  chain, then reads `origin()`, with an Overlay step and a
  contrast-guard step.
- **strand-render**:
  - `Renderer::pick(surface, point) -> Option<NodeId>`;
  - `node_layout(surface, node) -> LayoutInfo`;
  - `surfaces_of(node) -> Vec<SurfaceId>` (landed in the wave-0
    hardening; main resolves the inspector's node-only requests with it);
  - `set_highlight(Option<NodeId>)`, on every surface showing the node;
  - `set_damage_flash(bool)`;
  - `frame_summary(surface)`.

  Highlight and flash draw on CPU and `GpuPresent` surfaces alike. The
  Router gains a pick-mode hook. Pick mode needs no keyboard: the
  terminal client ends it (Escape or Ctrl-C there sends
  `inspect.pick{on:false}`), and a click picks. Pick mode also ends on
  IPC disconnect, on surface unmap, and when a lock starts. No surface
  changes its keyboard interactivity.
- **Handler input context (strand-compiler, cross-crate).**
  - The VM records the input event that started a handler, `InputCtx{
    surface, serial }`. It is available to actions until the handler's
    first `await`.
  - `ServiceHost::action` gains an `ActionCtx` argument that carries the
    context and the handler invocation's id.
  - `strand-surface::activation_token(surface, serial) -> async
    Option<String>`, called on the main thread; the services thread
    reaches it through the token channel (a services→main calloop
    channel and a one-shot), not through `SurfaceHost`, which it cannot
    call (architecture.md, "Handler input context", is the route).
    Key-press serials are tracked beside `button_serial`.
  - **Ordering rule.** An action that needs a token (notification
    `invoke`, app launch) waits up to 200 ms for it. Every later action
    from the same handler invocation queues behind it, across services
    and across `await`s (the invocation's number lives for its whole
    run; only the input is cleared at the first `await`; invocation 0,
    outside any handler, is unordered), so `n.invoke("default");
    n.dismiss()` still sends `ActivationToken`, `ActionInvoked`, then
    the close. Logic never blocks: the queue lives on the services side,
    one for all services (architecture.md, "Handler input context").
- **strand-compiler `Suggestion`** gains `title: Option<String>`.
  Zero-width (insertion) spans are allowed.
- **strand-dev**: the inspector client, the reference generator, and the
  tree-sitter layout. The layout is `crates/strand-dev/tree-sitter-strand/`,
  a separate package (its own `build.rs` compiling `parser.c` with `cc`,
  plus the `tree-sitter` crate). strand-dev uses it only as a
  dev-dependency. It is added to the root `members` explicitly, because
  `crates/*` does not reach it.
- **Plugins**: `strand` gets a `[lib]` target (`strand::Shell::builder()
  .service::<S>().run()`) beside its `[[bin]]`, git-only and marked
  unstable. Plugin schema files are `.schema` files in
  `$XDG_DATA_HOME/strand/schemas/` (Q18).

### 0b: shared code (S-core, one PR; integrator merges; every stream rebases)

The interfaces of 0a, landed as code with no behaviour change:
- the `strand-core::ipc` types, and `crates/strand/src/ipc.rs` split into
  `ipc/{mod, server, cli, describe}.rs` over them;
- `Shell::request` (run/shell.rs:510–600) dispatching to
  `run/request/{set, reset, …}.rs`, with empty modules for each new
  command, each owned by one stream;
- **the lib/bin split**: `main.rs`'s module tree moves under `lib.rs`,
  and `main.rs` keeps `main` and the COMMANDS table (both moved into
  `entry.rs` at the wave 1 → 2 boundary, m5-integration-w2). Done here, before
  the streams branch, so no later stream rebases across it. budgets.rs
  confirms `.text` is unchanged (±1 KB) (measured +12,544 B CPU-only
  with the request channel; accepted and charged to the ledger:
  decisions.md m5-core, m5-integration);
- `ActionCtx` threaded through `ServiceHost::action` and every host, with
  an empty context;
- the `Renderer` inspect and `frame_summary` fields as inert stubs;
- `Suggestion::title`;
- the `IpcRequests` channel as a source that is polled but never fed;
- `crates/strand/tests/m5.rs` with empty `m5/` modules, and the root
  `Cargo.toml` member for the tree-sitter package (integrator).

Gate: `run.sh ci` green; `.text` unchanged within 1 KB. (Missed: 0b
measured +12,544 B; m5-integration accepted it and charged the D-Bus
line and the reserve.)

### 0-size: S-size

`size-report.sh` baseline, then the reclaims above, one commit each.
After each one the integrator lowers the gate. Tests: strand-theme's
image tests unchanged, budgets.rs's opt-level test updated in the same
commit, and GitHub's `timing` job green for each opt-level change.

### 0-infra: S-infra

- **Three spikes**, each recorded in decisions.md `m5-infra`. Scripts go
  in `spikes/`; nothing is shipped.
  1. **The GPU compositor.** `gpu-session.sh` saves `sway.log`. Try
     `WLR_RENDERER=gles2`, then `vulkan`, on renderD128, and record why
     each falls back to pixman (m4-report:173). This decides whether
     Q19's option (A) is reachable.
  2. **tree-sitter CLI without node.** Check whether the pinned
     tree-sitter-cli release can run `generate` from `grammar.js`
     without node (a built-in JS runtime, or a cargo feature). If it
     cannot, `generate` runs from the committed `src/grammar.json`, which
     needs no JS. A developer regenerates `grammar.json` from
     `grammar.js` with node through uvx or a container, and CI checks
     only `parser.c` against `grammar.json`.
  3. **xdg-activation from a layer surface on sway 1.9** (takes S-infra's
     slot, written by S-platform). Does sway issue a token for a
     layer-shell surface and serial, and does a launched xdg-toplevel
     with that token get focus? If sway refuses, the container tests use
     strand-fake-wayland, and focus is proved only in `matrix.sh` on the
     compositors that honour it.
- **tree-sitter CLI**: a pinned prebuilt release binary with its sha256,
  in the Dockerfile and in ci.yml's **lint job only** (no
  `cargo install`, no cost to the other jobs). This is one commit; the
  image is retagged by its hash.
- **The budgets ledger line** (above).
- **`scripts/gpu-measure.sh`**, headed "owner-run only; never run by an
  agent". It runs `strand run` with a promoted panel and records
  `strand report` and PSS. It is for the owner's own machine on a KMS
  card, Q19's branch (B) if the GPU spike fails.
- **A CI build artifact for the human trials** (Q15, Q20): the budgets
  job uploads the release-profile binaries it already builds (CPU-only
  and GPU) as a workflow artifact. It is not a release: no version, tag
  or notes.
- **Deferred CI** (handoff.md's deferred CI list at `70cdc56`):
  - `tokens.rs::huge_fan_out_fails_fast` and lsp.rs's hung-bus case
    assert on work done or a step count, or move the time bound to a
    `GATE_MISS` check;
  - compositor_matrix.rs moves to Hyprland's current hyprctl syntax;
  - one GitHub `timing` run for theme_swap_bench's 8-scope spring;
  - `CI_JOB=timing STRAND_STRICT_TIMING=1` once under a performance
    power profile;
  - flakes stay tracked in handoff.md.

### 0-freeze: owner docs (no build)

Q1–Q5 are answered. The integrator ticks design.md:780–783 on
`laptop/m5-plan`, each tick with its decisions.md paragraph, and S-docs
creates the spec.md skeleton. This must land before S-lsp's tree-sitter grammar and before
the guide's code blocks are final. (The ticks landed in wave 0; the
spec.md skeleton did not and moved to wave 1, opened by S-docs before
any other S-docs writing. The move was accepted as scheduling, and the
skeleton and spec.md's text are merged on `laptop/m5-w1`, so S-lsp's
grammar starts from them in wave 2: decisions.md m5-w0-harden,
m5-integration-w1.)

## Waves

The integrator's worktree counts as a slot whenever it builds.

| Wave | Building (three slots, integrator included) | Writing (no slot) | Gate |
|---|---|---|---|
| 0 | S-core (0b) → then S-size; S-infra (image, spikes, ledger line); integrator for 0b's `run.sh ci` | 0a text; 0-freeze docs (S-docs' and S-tools' wave-0 writing did not happen and moved to wave 1, next row) | 0b merged, `run.sh ci` green, `.text` unchanged; provisional gate set; spikes recorded |
| 1 | S-ipc (commands, token overlay), S-inspect (compiler and scene half), S-lsp (missing key, extract component); the integrator rotates in for merges | carried from wave 0 (decisions.md m5-w0-harden): S-docs' spec.md skeleton first, then the guide outline, ladder placements and the template; S-tools' export mapping table; then S-docs levels 1–3 and spec.md; S-plugins design notes | merge per finished item; ledger within allocation on the merged tree (done: all six streams merged on `laptop/m5-w1`; the overruns accepted and the ledger re-based, decisions.md m5-integration-w1) |
| 2 | S-plugins' pure-move commit first (`main.rs`'s dispatch into `entry.rs`; landed, m5-integration-w2); then the first launch (handoff.md): S-ipc (D-Bus, the inspect IPC arms), S-palette, S-tools, S-lsp (tree-sitter, from spec.md's construct index) and S-plugins (builder), three building at once, the rest writing or waiting on review; S-inspect (render and binary half) and S-platform take slots as they free; the integrator rotates in | S-docs levels 4–5 (with the guide's strand-check test), reference text | `matrix.sh` for activation and IPC; PNG tests (CPU and lavapipe) green |
| 3 | S-inspect (strand-dev client, write-back), S-docs (ladder, newcomer, reference generator, agent proxy), integrator | freeze text, m5-report, handoff | see "Closing" |

Critical paths:
- 0a → 0b → every stream.
- S-inspect compiler half → render half → strand-dev client and
  write-back.
- 0-freeze → tree-sitter and the guide → ladder and newcomer tests →
  agent proxy → human trials.
- S-infra GPU spike → Q19's branch, (A) or (B) → closing GPU
  measurement.

Timeline in waves: wave 0 takes about one week (0a, then 0b; S-size and
the spikes alongside). Wave 1 about two weeks. Wave 2 two to three
weeks. Wave 3 about two weeks, including closing. That fits the eight
weeks features.md gives M5 (weeks 45–52). Human trials run after wave
3's newcomer test and agent proxy are green, during closing.

### S-ipc (waves 1–2)

- **`get`** reads what `set` writes: exported values, settings fields and
  token overlays (the Q10 addressing). Output is text, with `--json` as
  in `watch`. `--json` shows the layer for settings and tokens
  (`Settings::layer`, `TokenTable::origin`, `OVERLAY_ORIGIN`).
- **`toggle`**: one logic-thread step, bool only. Any other type is an
  error naming the type. The refusals it replaces live in strand-core since 0b's split:
  `crates/strand-core/src/ipc.rs`'s
  `requests_round_trip_and_unknown_ones_are_refused` (parse) and
  `answers_read_back` (the answer).
- **`call`**: service-level actions only (Q13). Arguments are text,
  parsed by the schema's parameter types as `parse_text` does. The
  return value is printed, or JSON with `--json`.
- **Token overlays** (Q10): writing a token path through `set` writes the
  token overlay layer. It persists in `$XDG_STATE_HOME/strand/tokens.toml`,
  as settings' overlay does in `settings/`, takes the token swap path
  (design.md:405), and raises the "overlay wins" notice when a save is
  shadowed. **`clear`** (CLI `strand clear <path>`, IPC `clear{path}`)
  removes a settings or token overlay field. It is the CLI twin of the
  overlay's `[clear]`.
- **D-Bus front end** (`strand-services/src/ipc_bus.rs`): one interface
  with the same commands, plus a `Changed` signal for watch. The bus
  name is `io.github.jakeb_grant.Strand` (Q12, the one constant). It is
  owned when `strand run` starts. A second
  instance runs without D-Bus IPC, matching the socket rule
  (decisions.md:3257).
- Tests:
  - `strand-core::ipc` unit tests: parsing and encoding of each new
    command;
  - `tests/m5/cli.rs` against a headless `strand run` with STRAND_MOCK:
    - get, toggle, call;
    - toggle on a non-bool is refused;
    - call with a wrong argument type is refused with a did-you-mean;
    - a token overlay springs the colour, a shadowed save shows "overlay
      wins", and `strand clear` drops the overlay;
  - `tests/m5/dbus_ipc.rs` under `dbus-run-session`:
    - get, set, toggle and call over D-Bus;
    - a second instance runs without the name;
    - `Changed` arrives;
  - budgets.rs's design-bar idle window and PSS, with the name owned
    (the strand-services idle tests never start `strand run`).
- Budget: 35 KB (commands and overlay; wave 1 measured +87,104 B on its
  branch, accepted inside the merged wave-1 charge of 115,328 B,
  decisions.md m5-integration-w1) + 37,400 B (D-Bus, wave 2: 40 KB less
  0b's request channel, decisions.md m5-integration) + 3 KB (wave 2's
  `inspect.node`, `inspect.dump` and `reset{node}` arms, 2,186 B
  measured: `inspect.dump{surface}` passes the surface root's node ref
  to `Instance::dump`, decisions.md m5-inspect).
- Ticks: the CLI + IPC box (shared).

### S-inspect (waves 1–3)

**Wave 1, compiler and scene half (no Wayland):**
- `identity`, `cells_of`, `reset_subtree`, `source_hash`;
  `TokenTable::provenance`, with subtree `set { }` and component
  `tokens { }` scopes, overlay steps, and a contrast-guard step.
- Kept badges, in three kinds: kept over a changed default, persisted,
  and from an overlay layer (settings or token).
- `inspect.dump` JSON. Password inputs are redacted
  (architecture.md:1381–1384).
- Tests:
  - `crates/strand-compiler/tests/inspect.rs`:
    - identity of a per-monitor row, keyed `for` rows, sibling index;
    - each badge kind;
    - `reset_subtree` resets only the subtree, and a reload keeps the
      rest;
  - provenance unit tests: a four-step chain, a `set` step, an overlay
    step, a contrast step;
  - a password input's value never appears in a dump.

**Wave 2, render and binary half:**
- Pick mode:
  - the Router captures pointer events while picking;
  - `Renderer::pick` and the highlight outline, on CPU and `GpuPresent`
    surfaces;
  - the pick follows the pointer across surfaces and popups;
  - the terminal client ends it.
- While locked: `inspect.pick` is refused, and pick mode and the
  highlight end when a lock starts. Lock surfaces are never pickable.
- Repaint flash: `set_damage_flash(true)` tints each frame's damage for
  300 ms. The flash's own repaint is excluded. When it is off there is
  no clock and no allocation.
- "Says why" (optional, from the reserve): the blur fallback, the shader
  fallback, the contrast tint, all read from existing notices.
- Tests:
  - `crates/strand-render/tests/inspect.rs`, offline PNGs at 1× and 2×:
    - `the_picked_node_is_outlined`;
    - `repaint_flash_tints_only_the_damage` (no feedback);
    - `flash_off_wakes_nothing`;
    - `pick_follows_the_pointer_across_surfaces`;
  - a lavapipe test that a `GpuPresent` surface shows the outline and
    the flash (beside the CPU PNGs);
  - `tests/m5/inspect.rs` (headless sway, two outputs):
    - a virtual-pointer pick on output 2, then `inspect.node` returns
      identity, layout and provenance;
    - ending pick mode returns input to the handlers;
  - `tests/lock.rs::the_inspector_cannot_pick_while_locked`, added to
    `strand_lock.sh`'s list and run in the lock VM;
  - design-bar PSS unchanged when the inspector is never opened.

**Wave 3, strand-dev client and write-back:**
- `strand-dev inspect`: a terminal client over the socket. It runs pick
  mode and prints the node pane (identity, layout, chain, badges, and
  why). It offers reset subtree (`reset{node}`) and open in `$EDITOR`.
- Write-back (Q11: refuse a read-only target):
  - only a literal prop value at the node's own `origin()` span, or a
    token's literal value at its definition;
  - written only if the file's current bytes hash to the `source_hash`
    the edit was computed from;
  - atomic, following symlinks, as a span edit;
  - a read-only target is refused with the reason and an offer to open
    it in `$EDITOR`.
- Tests (`crates/strand-dev/tests/inspect.rs`):
  - one span is edited;
  - a file changed underneath is refused;
  - a symlink is followed;
  - a read-only target is refused, offering `$EDITOR`;
  - a rename-into-place between hash and write is refused.

  Plus `tests/m5/inspect.rs::a_written_back_edit_reloads_and_keeps_state`.
- Budget: wave 1's compiler and scene half measured 73,718 B of library
  code against 55 KB; the integrator accepted it inside the merged
  wave-1 charge (decisions.md m5-integration-w1), so "cut scope before
  wave 2 if it is over" is waived for wave 1's code under the owner's
  speed-first policy. Wave 2: 20 KB for pick, outline and flash, plus
  its part of the GPU-only 40 KB; the IPC arms' 3 KB (2,186 B measured)
  is S-ipc's line.
- Ticks: the Inspector box, citing each of its seven parts.

### S-lsp (waves 1–2)

- **Missing key**: `check::missing_key` (tree.rs:707) attaches a
  zero-width `Suggestion` per candidate field, titled "Add `key
  <b.field>`". The compiler's guess comes first and is marked preferred.
  The state-declaration key errors get the same where a field can be
  inferred.
- **Extract component** (`REFACTOR_EXTRACT`), over a selected element
  subtree:
  - free variables become params, and parent state that is only read
    becomes a prop;
  - parent state that is written or bound is refused with a reason,
    unless spec.md allows `<->` to a component param (checked and
    recorded, not assumed);
  - the result goes after the enclosing item as `NewComponent`
    (`NewComponent2`…), formatted with `fmt::format`;
  - the subtree's state resets on reload (design.md:662–664), and the
    action's description says so.
- **tree-sitter** (after 0-freeze): `grammar.js`, an external scanner
  for SAME_LINE, SEP and TOUCH plus contextual keywords, and
  `queries/{highlights, locals, indents, injections}.scm`. The generated
  `src/` is committed whole: `parser.c`, `grammar.json`,
  `node-types.json` and `tree_sitter/` (`parser.c` includes its
  `parser.h`), the four files `scripts/check-tree-sitter.sh` compares. It is linked only into strand-dev's
  tests.
- Tests:
  - lsp.rs:
    - `quick_fix_for_a_missing_key`;
    - `missing_key_offers_each_candidate_preferred_first`;
    - `extract_component_moves_a_subtree_into_a_new_component`;
    - `extract_component_refuses_parent_state_writes`;
  - `reloads.rs::an_extracted_component_resets_only_its_subtree`;
  - `crates/strand-dev/tests/tree_sitter.rs`:
    - every accepted fixture, repo `.strand` file and guide block parses
      without errors;
    - the negative syntax fixtures produce errors;
    - the highlights cover every keyword in grammar.md;
  - the lint job's regeneration diff (`scripts/check-tree-sitter.sh`:
    `parser.c`, `grammar.json`, `node-types.json` and `tree_sitter/`).
- Budget: 3 KB in `strand`.
- Ticks: the LSP box and both tree-sitter boxes.

### S-platform (wave 0 spike, wave 2)

- `xdg_activation_v1` is bound in strand-surface (wayland-protocols
  staging is already linked). Key serials are tracked.
- The handler path follows 0a's `InputCtx`/`ActionCtx` and the ordering
  rule. A compositor without the protocol, or one that never answers,
  means no token: never a delay beyond 200 ms, never a dropped or
  reordered action.
- notifications.rs: it emits `ActivationToken(id, token)` before
  `ActionInvoked`, `SPEC_VERSION` becomes "1.2", and
  tests/notifications.rs:150 is updated.
- apps.rs: launches set `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID`.
  `restore_in_child` strips the inherited ones.
- Tests:
  - fake.rs:
    - `an_activation_token_comes_from_the_clicked_surface` (pointer and
      key serial);
    - `no_activation_protocol_means_no_token_and_no_wait`;
  - notifications.rs:
    - `activation_token_precedes_action_invoked`;
    - `invoke_then_dismiss_keeps_order_while_waiting_for_a_token`;
  - apps.rs: `a_launch_carries_the_token_and_not_an_inherited_one`;
  - compositor_matrix.rs: `a_launched_window_gets_focus_with_the_token`,
    on the compositors where the spike showed the token is honoured. It
    checks focus, not just that a token was issued. Hyprland is skipped
    without a KMS card, as today.
- Budget: 20 KB.
- Ticks: the xdg-activation box.

### S-tools (wave 1 writing, waves 2–3)

- **`strand new [dir]`**: the default dir is `$XDG_CONFIG_HOME/strand`,
  and a non-empty dir is refused. It writes the level-1 hello bar from
  `docs/guide/template/bar.strand` (owned by S-docs; embedded with
  `include_str!`).
- **The theming hub** (owner-approved 2026-10-11; decisions.md m5-tools
  is the design, superseding the gtk, kitty and hyprland writers first
  planned here). Strand ships no per-app mappings or templates.
  - Palette (strand-theme and strand-scene; S-palette's, below): 13 hue roles (62 in all), filled from the source or
    harmonised toward the accent; extras in source order as
    `$palette.<name>` (templates and `file` text only); identity `name`,
    `source`, `dark`.
  - `strand export json | base24 | --template <file> [--palette <expr>]`,
    to stdout or `-o <file>` (Q16, atomic). The source is the running
    shell over IPC, else the offline solve with the persisted palette
    pinned, else "run strand once, or fix the theme".
  - Templates: `{{ expr }}` holes over the theme, `hex`/`argb`, the channel
    methods `.red()`/`.green()`/`.blue()`/`.opacity()`,
    `.light()`/`.dark()` for the other mode, errors with
    location, nothing written on error.
  - The `file` element (`template:` or `text:`, `run:` under
    `permit exec`): re-rendered on theme changes, written only when
    changed, boot catch-up, 200 ms `run` debounce and 10 s timeout,
    never overwriting a file Strand did not write.
  - A top-level `file` is a grammar change (grammar.md `top_file`),
    so it lands before the freeze.
  - Cross-crate interfaces: architecture.md, "The theming hub (m5-tools,
    owner-approved 2026-10-11; built in M5 wave 2)", which names each
    bullet's owner.
- **`strand report`**:
  - text, plus `--json`;
  - compositor and globals with versions; outputs and scales; PSS from
    smaps_rollup;
  - per-surface frame p50/p95/p99 over the last 600 frames, presented
    and discarded;
  - GPU status, reason and lost-device count; promotion reasons;
  - no serials or IDs.

  It goes through the snapshot hop.
- **Optional (from the reserve)**: check-time validation of literal
  `import()` sources. strand-theme already errors at load
  (import.rs:55); this only moves the error to check time.
- Tests:
  - `tests/m5/new.rs`:
    - the new shell checks and runs on two outputs (grim refs);
    - a non-empty dir is refused;
  - S-tools' share of the theming tests in decisions.md m5-tools ("the
    rest"): template diagnostics and format goldens, `file` behaviour in
    the container, and IPC and offline agreeing; no fixture imitates an
    application (the palette's tests are S-palette's, below);
  - `tests/m5/export.rs`: writes only into a temp `XDG_CONFIG_HOME`, or
    stdout;
  - `tests/m5/report.rs`: every section present; the JSON round-trips;
  - a `frame_summary` percentile unit test.
- Budget: 28 KB (the template module, the `file` element and output
  host, `new`'s code, the `export` CLI and request) + 20 KB (report);
  S-palette's 10 KB is its own line.
- Ticks: the "`strand new`, `export`, `report`" box. (The importer box
  was ticked in 0-freeze from M2's tests and is closed; the optional
  check test, if written, adds evidence to that tick only.)

### S-palette (wave 2)

The theming hub's palette half (decisions.md m5-tools, "13 hue roles"
and "extras and identity"; architecture.md, "The theming hub",
strand-theme and strand-scene bullets).

- 13 hue roles after the 49 Material 3 roles (`Role::COUNT` 62), filled
  from the source as given, else harmonised toward the accent; never
  contrast-guarded.
- Extras in source order as `$palette.<name>` and the identity `name`,
  `source`, `dark`; `to_text`/`from_text` write and read `name` and
  `extra` lines, and older files derive the hues.
- The reserved names: the hue roots in strand-scene's `TokenTable`, the
  `palette` first segment, and the checker's error for a token set that
  declares either. They land before the freeze.
- The `json` and `base24` writers (`strand-theme/src/export.rs`), pure
  over the palette and the resolved token table; S-tools' CLI and IPC
  request call them.
- Tests (decisions.md m5-tools, "the rest"): importers keep every source
  colour in order and map their hues; harmonised hues within 15° and
  all 62 roles in gamut in light and dark (re-proving the importer
  box's "whole palette schema" for the grown schema); `to_text`
  round-trips extras and `name`; the `export json` golden from
  `catppuccin:mocha`; `base24`'s round trip; a user token `red` or
  `palette.x` is refused.
- Budget: 10 KB.
- Ticks: adds its evidence to the shared `new`/`export`/`report` box,
  which the integrator ticks; the 62-roles test is also cited beside the
  closed importer box's tick, since the schema it fills grew.

### S-docs (waves 1–3)

- **`docs/guide/`**: levels 1–5 in plain Markdown, with code blocks
  tagged by level (` ```strand level=2 `), and the `strand new`
  template. Levels 1–3 end at bar (a), or at the Q15 target.
- **Ladder placements** (agent decisions, one decisions.md paragraph
  each, by "does it remove a concept, or add one?"): let, match, fn,
  slot, key, else, every, `id:`, `set {}`, exit, `on scroll`,
  `on secondary`, `on show`, `hit:`, `split`, compound assignment,
  keyframes/play, canvas, enum/type, `@reset`. design.md's ladder table
  gets the placements before the freeze. Async is at level 4 (Q17,
  landed with 0-freeze).
- **`docs/spec.md`**: self-contained semantics plus the compatibility
  policy (Q5). It names the D-Bus interface and says the name
  (`io.github.jakeb_grant.Strand`) may change before 1.0 (Q12).
- **`docs/reference/`**: generated by `strand-dev reference` from the
  builtin and service schemas. CI fails on a diff.
- **The agent-proxy harness** and `target-checklist.md` (advisory).
- Tests:
  - `crates/strand-dev/tests/ladder.rs`:
    - `no_guide_block_uses_a_construct_above_its_level` (a HIR walk per
      block);
  - `crates/strand-dev/tests/guide.rs::every_guide_block_checks` (written
    by S-lsp in wave 2, decisions.md m5-lsp; the block reader is
    `tests/support/guide.rs`, for the ladder test too);
  - `reference.rs::the_reference_is_current`;
  - `tests/m5/newcomer.rs`.
- Budget: 0 (the template is .rodata).
- Ticks: the Docs box. The newcomer exit's evidence goes to the
  integrator.

### S-plugins (wave 2)

- `strand::Shell::builder().service::<MyService>().run()`.
  `Builtin::register` is unchanged, and the composite host routes the
  extra names.
- Plugin schemas reach `strand check`, `strand run` and the LSP as
  `.schema` files
  written by the plugin binary (`--write-schemas`) into
  `$XDG_DATA_HOME/strand/schemas/`, outside the watched config dir (Q18).
  No dylib loading.
- The example is `[[example]] plugin_hello` in the `strand` crate: `cargo
  test` builds it, and it never enters the release binary's `.text`.
  There is no nested cargo run and no out-of-workspace package.
- The guide's plugin page and D-plugins state that a plugin binary must
  copy the workspace's `[profile.release]` and its package overrides,
  and invoke `strand::process_setup!()` at its crate root (mimalloc and
  THP off before the first allocation; `plugin_hello` does). Otherwise
  the size and PSS figures do not hold (handoff.md, "Owner decisions already answered", item 1; decisions.md
  m5-w0-harden).
- Tests:
  - `tests/m5/plugin.rs::an_extra_service_is_checked_and_runs` runs the
    built example binary;
  - `tests/m5/plugin.rs::the_stock_strand_reads_plugin_schemas` and
    `::the_setup_warning_shows_without_the_macro`;
  - lsp.rs: `plugin_schemas_hover` and
    `the_lsp_binary_reads_plugin_schemas`;
  - the full list is decisions.md m5-plugins, "`plugin_hello` and the
    wave-2 tests".
- Budget: 8 KB in `strand`, the ledger's plugins line: the wave-1 notes
  estimate 4–8 KB for the schema-file reader, `--write-schemas` and the
  checks the stock binary now holds, confirmed by the ledger in wave 2
  (decisions.md m5-plugins, "size and speed"). A plugin binary's own
  size is not gated.
- Design notes (wave 1): decisions.md `## m5-plugins`, and the
  interface in architecture.md ("Plugin builder"), accepted by the
  integrator on 2026-10-11, including moving `main.rs`'s dispatch into
  the library at the wave 1 → 2 boundary and the stock `strand run`
  reading the plugin schema files.
- Ticks: the plugins half (Q17's split).

## Closing (wave 3, integrator)

1. **The freeze commit.** spec.md and grammar.md are final, every
   planned design.md edit has landed, and decisions.md records the
   freeze.
2. **Full checks on the exact commit**:
   - `run.sh ci`;
   - `matrix.sh`;
   - `gpu.sh`;
   - **`scripts/lockvm/scenarios/all.sh`**, the whole lock VM suite, as
     M5's closing gate (design.md:795: "before touching the lock code");
   - GitHub CI green.
3. **The GPU box** (Q19): run the measurement path the owner chose, and
   record PSS over the pre-GPU baseline and frame time in `GpuPresent`
   mode. `gpu_idle.rs` gets a presented-mode variant whose bounds are
   reported, not gated.
4. **Human trials** (Q15), on the CI build artifact or a source build,
   recorded in m5-report.md.
5. **Ticks and paperwork**: tick each box with its test; write
   m5-report.md (ledger, PSS, the resident split, trial times); update
   handoff.md; propose the tightened `.text` gate; delete the worktree
   `target/` directories; push only `laptop/*` branches. No version
   bump, tag, release notes or release artifact (Q20).

## Test realism

- **sway 1.9 and `xdg_activation_v1`**: whether sway honours tokens from
  a layer surface is a wave-0 spike, not an assumption. Focus is checked
  only in `matrix.sh`.
- **D-Bus front end**: a private `dbus-run-session` only, and two
  instances on one private bus for the name collision.
- **Pick mode**: the virtual pointer from demo.rs on headless sway. The
  terminal client's cancel is driven over the socket.
- **Repaint flash**: offline vello_cpu PNGs at 1× and 2× with the fake
  clock, and one lavapipe case for `GpuPresent`.
- **Newcomer test**: pixel-strict at both scales, STRAND_MOCK, and a
  120 s budget.
- **tree-sitter conformance**: runs in the test job. Only the regeneration
  diff of the four generated files (`scripts/check-tree-sitter.sh`)
  needs the CLI, in lint.

## Risks

1. **The ledger is overrun anyway.** D-Bus and the inspector are the
   least predictable items. Mitigations:
   - measure each in its first week;
   - the integrator keeps a reserve (42 KB planned, 29,176 B after 0b
     and the wave-0 hardening; re-based to 61,001 B after wave 1, with
     the theming hub, plugins and the inspector given lines of their
     own so nothing else is promised to it: decisions.md
     m5-integration, m5-w0-harden, m5-integration-w1). Wave 1 overran
     anyway (S-ipc and S-inspect, accepted as cold code under the
     speed-first policy), so wave 2's streams measure in their first
     week and stop at their line;
   - cut scope before asking the owner: D-Bus `watch` as a signal only,
     a dump with fewer fields, "says why" dropped.
2. **Merges collide on the budget** (decisions.md:9573). Every merge's
   ledger line is measured on the merged tree.
3. **Reclaims touch timing.** Only GitHub's timing job decides, and
   anything that moves a gate is reverted.
4. **The terminal inspector disappoints.** The fallback is Q9 (b).
5. **Write-back races.** The guard compares against the compiled text
   and re-reads under the rename, and the tests cover each case.
6. **Provenance mid-animation.** The pane shows render's resolved value
   next to the chain.
7. **Pick mode keeps input.** It holds no keyboard, ends on lock, unmap
   and disconnect, and the lock case runs in the VM.
8. **D-Bus costs idle PSS.** Owning the name at boot starts the services
   thread for every shell. This is measured. If it is large, the
   fallback is to own the name when the first service starts.
9. **xdg-activation reorders or hangs actions.** The 200 ms bound and
   the per-handler queue are tested, and the matrix checks focus.
10. **tree-sitter drifts from the parser.** The fixture conformance test
    covers it.
11. **Spec freeze slips.** Q1–Q5 are answered, and the answers add no
    syntax.
12. **Human trials slip or fail.** They need recruits and a CI build
    artifact. The agent proxy finds guide problems first. If recruiting
    is not possible, Q15's fallback (ii) applies: the automated layer
    plus the proxy carry the box, with that recorded.
13. **Flakes block the FF rule.** The diagnostics stay armed, and the
    integrator reruns once and records it.
14. **Plugin API churn.** The API is git-only and marked unstable.

## Out of scope for M5

- Item actions and exported `fn`s in `strand call`; service field reads
  in `strand get` (not exported values, decisions.md:3944).
- Tray `ProvideXdgActivationToken` and `DBusActivatable` `Activate`.
- QuickJS, a WASM tier and AccessKit (Q1–Q3).
- A 1.0 release: version bump, tag, release notes, release artifact
  (Q20).
- A graphical inspector panel drawn by the shell.
- Dynamic plugin loading, and a semver-stable plugin API.
- Paging IPC answers.
- Replacing the `.text` gate with resident-code PSS.
- nucleo at opt "z" (no search gate exists).
- The handoff.md known limits that need design.md changes: PAM interim
  text streaming, remote https art, Notification time of day, 802.1X,
  StepVolume's caller.
- niri multi-output and the busy-session Hyprland capture.
- Raising the GPU `.text` gate.

## Agent decisions (recorded in decisions.md, not put to the owner)

- **Protocol**: v1 plus additive commands (ipc.rs's header,
  decisions.md:3249), inspector commands under `inspect.`, reset subtree
  as `reset{node}`.
- **`get`/`toggle`/`clear`**: `get` mirrors `set` (decisions.md:3944:
  exports and settings, plus the Q10 tokens); text output with `--json`;
  `toggle` bool only; `strand clear` is the CLI twin of `[clear]`.
- **Kept badges**: three kinds (`Report::kept_over_default`, persist,
  `Settings::layer`/`TokenTable::origin`'s `OVERLAY_ORIGIN`).
- **Reset subtree**: the scope of `strand reload --hard`, limited to the
  subtree. Settings are untouched.
- **Inspector while locked**: refused, and ended on lock. Lock surfaces
  are not pickable, and password values are redacted. This follows the
  lock rules and architecture.md:1381–1384; design.md:659 defers anything
  inside `lock` on reload.
- **tree-sitter home**: the package `crates/strand-dev/tree-sitter-strand/`
  (features.md puts it in strand-dev), with the generated `src/`
  committed (`parser.c`, `grammar.json`, `node-types.json` and
  `tree_sitter/`), a pinned prebuilt CLI in lint only, and no runtime
  linking.
- **LSP quick-fixes**: all candidates, preferred first; extract to
  `NewComponent` after the enclosing item; refuse parent-state writes
  unless spec.md allows `<->` to a component param.
- **Ladder placements** for the unplaced constructs (S-docs).
- **Resident code**: report the existing per-mapping PSS split; no new
  gate.

## Owner decisions (2026-10-10)

The owner accepted every recommendation of the draft's "Questions for
the owner" except Q12 (the owner named the D-Bus name) and Q20 (no 1.0
release in M5). decisions.md `## m5-owner` has one dated paragraph per
answer; this list is the short form.

**Language freeze**

1. **QuickJS escape hatch (design.md:778): no.** Helpers stay
   in-language. A JS runtime adds a concept, and the `.text` cannot hold
   it.
2. **WASM plugin tier (design.md:779): no.** `from dbus|file|listen|poll`
   services and Rust plugin crates cover the need.
3. **AccessKit depth in v1 (design.md:780): none in v1, recorded.**
   Inferred AT-SPI roles and names, and an accessible-name prop, can
   arrive in 1.x as schema additions under the compatibility policy.
4. **`.strand` or KDL (design.md:781): keep `.strand`.** The parser,
   checker, LSP, formatter and fuzzer are built on it, and KDL has no
   expression grammar.
5. **What "frozen" means: (a).** grammar.md plus a self-contained
   spec.md with a compatibility policy (1.x: no removals or changes of
   meaning; schema entries are only added), held by process and the
   conformance tests. No hash pin.

**Size**

6. **The CPU-only `.text` gate: (A).** A provisional re-base to
   15,925,248 B (+196,608) on day one, lowered as reclaims land, back to
   15 MiB if they reach about 159 KB, tightened to the next 64 KiB above
   the final size at close. The GPU gate stays 18.5 MiB. (Lowered to
   15,859,712 B after wave 0's reclaims: decisions.md m5-integration.
   Superseded by the owner's speed policy of 2026-10-10: 18,350,080 B
   CPU-only and 22,282,240 B GPU after the speed review, decisions.md
   m5-speed.)
7. **Reclaim policy: behaviour-neutral reclaims only**, each kept only
   if GitHub's timing job stays green. No opt-level cuts on gated hot
   paths.
8. **`strand fmt`: (a) keep it in `strand`.**

**Inspector and overlays**

9. **Inspector UI: (c).** The shell draws only the outline and the
   flash; `strand-dev inspect` is a terminal client.
10. **Token runtime overlay addressing: (a).** `strand set '$accent'
    '#ff8800'`, global scope, persisted in `$XDG_STATE_HOME` beside
    settings' overlay, removed by `strand clear '$accent'` or the
    notice's `[clear]`.
11. **Write-back to a read-only target: (a) refuse** with the reason and
    offer "open in `$EDITOR`"; design.md:760's overlay redirect is read
    as the settings write-back.

**IPC, CLI and platform**

12. **The D-Bus name: `io.github.jakeb_grant.Strand`** (bus name and
    interface), path `/`, owned at `strand run` start; a second instance
    runs without D-Bus IPC. The owner may rename it later: it lives in
    one constant (`strand_core::ipc::DBUS_NAME`), and spec.md says the
    name may change before 1.0.
13. **`strand call` scope: service-level actions only.**
14. **xdg-activation: the 200 ms bound.** Later actions from the same
    handler queue behind the wait, `DESKTOP_STARTUP_ID` is set too,
    inherited tokens are stripped, and tray and DBusActivatable stay out.

**Exit, docs and the release question**

15. **The newcomer exit: target (a), bar (a) as written; evidence (i)**,
    the automated test, the agent proxy and two or three human trials
    off the laptop, the clock starting after install, which tick the
    box. If recruiting is not possible, (ii), recorded as a deliberate
    reading. The trials use a CI build artifact or a source build, not a
    release (Q20).
16. **`strand export` default output: stdout, with `-o <file>`.**
17. **Ladder and box wording:** Async moves to level 4, and
    features.md's "Theme importers complete; plugins" box is split in
    two, the importers half ticked from M2's tests.
18. **The plugin model: a user binary built through a `strand` library
    builder**, git-only and marked unstable, schemas as `.schema` files
    in `$XDG_DATA_HOME/strand/schemas/`, with the profile-copy
    requirement documented.
19. **The GPU-on-hardware box: (A) if wave 0's spike passes, else (B)**,
    reworded now. (A): a real GPU presenting through a dmabuf-capable
    compositor (`gpu.sh` on renderD128). (B): the owner runs
    `scripts/gpu-measure.sh` on a KMS card; never an agent.
20. **Is M5 the 1.0 release? No.** The owner wants to use Strand for a
    while first. M5 closes as a normal milestone: no version bump (the
    workspace stays 0.0.0), no tag, no release notes, no release
    artifact. The language freeze and the compatibility policy still
    land; they describe what 1.x will promise when the owner releases
    it.
