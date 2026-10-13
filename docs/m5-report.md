# M5 exit report

Measured 2026-10-12 on the owner's laptop (Intel Core Ultra X7 358H, 16
threads) through the container suite (`scripts/container/run.sh`:
Ubuntu 24.04 with CI's packages, rustc 1.97.0, headless sway 1.9,
lavapipe as the only Vulkan driver), on `laptop/m5-w3` at `672b114`
(every M5 stream merged, the integrator's wave-3 sweep and the language
freeze; the commits after it change docs only). GitHub's figures are
from CI run 38220728825 at `672b114`, green on every job (lint, test,
budgets, acceptance, timing, compositors with Hyprland 0.56.2, lock-vm),
read from its logs. Every number below comes from a test or script in
the tree; the tests that gate fail when their gate is missed, and the
ones that only report are marked so.

M5 closes as a normal milestone, not the 1.0 release: no version bump,
tag, release notes or release artifact (owner, 2026-10-10; decisions.md
m5-owner Q20). The plan is `docs/m5-plan.md`; the reasons are in
decisions.md's `m5-*` sections.

## Result

| Exit (`docs/features.md`, M5) | Evidence | State |
| --- | --- | --- |
| Language spec frozen | spec.md and grammar.md say they are frozen, and spec.md 14's compatibility policy is in force: in 1.x no syntax is removed or changes meaning and builtin schema entries are only added; a later change to either file needs a decisions.md paragraph citing it (decisions.md m5-integration-w3, "the language freeze"). Held by the conformance tests: `crates/strand-dev/tests/spec.rs`, `crates/strand-compiler/tests/fixtures.rs`, `crates/strand-dev/tests/tree_sitter.rs`, `crates/strand-dev/tests/ladder.rs` | **ticked** (`672b114`) |
| Newcomer builds a multi-monitor bar in 15 minutes | the automated layer is green: `crates/strand/tests/m5/newcomer.rs::the_guide_builds_bar_a_on_two_outputs` builds design.md's bar (a) from `strand new` through guide levels 1–3 as live saves on two headless outputs, every step clean, its kept cells as predicted and its frames matching 27 reference PNGs; one agent-proxy session (2026-10-13, below) built bar (a) from the guide alone and scored 9/9 | **ticked** 2026-10-13 by Q15's fallback (ii) (decisions.md m6-owner) |

All ten of M5's feature boxes are ticked, each with the tests that
prove it (features.md, M5). With the freeze, features.md's M5 line
stands at **11 of 12** (ten boxes and two exit criteria); M0–M4 are
complete (20/20, 59/59, 30/30, 14/14, 18/18).

### Human trials

No human trial was run: the owner recruited none and, on 2026-10-13,
asked the integrator to do the newcomer run, so Q15's fallback (ii)
carries the exit (decisions.md m6-owner). The run that was done is the
agent proxy's, below; human trials stay welcome and are recorded in the
table at the end of this section, but they gate nothing.

**Agent proxy, 2026-10-13, on `main` at `f8ea24d`.**
`scripts/agent-proxy/prepare.sh` made the workspace (the guide without
`target-checklist.md`, an empty config directory, a `strand` limited to
`new`, `check`, `fmt` and `help` through the container's debug build).
A fresh model session (Claude Opus 5.5, as a subagent of the
integrator's session) got PROMPT.md and nothing else, with the rules
to read only that workspace, not the web, and to report every stumble.
It read README.md and levels 1–3, ran `strand new` (its `bar.strand`
was level 1's first block), wrote the five files level 3 ends with
(`bar`, `dot`, `clock`, `volume`, `battery`; about 75 lines), and
`strand check` passed on the first try with 0 errors and 0 warnings;
`strand fmt` changed nothing. 45 s from start to "done", 9 tool calls.
`scripts/agent-proxy/score.sh` then ran it on two headless outputs
(acceptance mock): **9/9** (`bar-per-output`, `dots` 4 and 2 by
output, `title`, `clock`, `calendar` opened by a click, `volume`,
`battery`, `tray`, `themed`). The time says the guide's blocks are
complete and correct as written, not how long a person takes: a model
reads and types far faster, and it took each file whole from its last
block rather than building step by step. Stumbles it reported, none
blocking: README.md's links to `docs/spec.md`, `docs/grammar.md`,
`docs/reference/` and `guide.rs` lead outside the guide directory (they
resolve in the repository, where readers have the guide; the proxy's
copy has only the guide, so left as is); level 2's tray note sends a
reader to the `TrayMenu` doc for the menu popup (by design, the
reference holds it); level 1 shows `strand check` without a directory
(it defaults to the config directory, so correct). One gap the owner
hit first, on 2026-10-13, before the proxy ran: README.md's "What you
need" said to have `strand` on your `PATH` but not where it comes from
(`bash: strand: command not found`). It now says to build it with
`cargo build --release -p strand` or take the `strand-release-binaries`
artifact and name `strand-gpu` `strand`.

Earlier text, kept: m5-plan's "Timed human trials" asks for two or three
people new to Strand, on their own machines and layer-shell compositors
(never the laptop), with the budgets job's `strand-release-binaries`
artifact or a source build and the guide; the clock starts once `strand
new` has run and stops when bar (a) runs on every monitor. The owner
records each trial's time and stumbles here, under this heading. If
recruiting proves impossible, the owner invokes Q15 (ii) and
decisions.md records it.

| Person | Machine and compositor | Monitors | Time | Stumbles |
| --- | --- | --- | --- | --- |
| — | — | — | — | — |

## What M5 delivered

By wave. Each stream was reviewed and fixed in up to three rounds
before the integrator merged it; decisions.md has a section per stream
and one per integration (`m5-integration`, `-w1`, `-w2`, `-w3`).

### Wave 0 (on `main` at `88d4d99`)

- **0a and 0-freeze**: architecture.md's M5 interfaces; design.md's
  four open questions answered (no QuickJS, no WASM tier, no AccessKit
  in v1, keep `.strand`), `Async` moved to ladder level 4, the importer
  box split from plugins (Q1–Q4, Q17).
- **S-core (0b)**: `strand_core::ipc` (types, JSON-line codec,
  `EventSink`), the `strand` lib/bin split, `Shell::request` dispatching
  to `run/request/*.rs`, `ActionCtx`/`InputCtx` through every host, the
  inert render stubs.
- **S-size**: `scripts/size-report.sh` and two reclaims (−37,056 B
  CPU-only).
- **S-infra**: the three spikes (sway renders on renderD128 with
  linux-dmabuf, so Q19 is (A); the tree-sitter CLI generates without
  node; sway honours an xdg-activation token from a layer surface), the
  pinned tree-sitter CLI, the budgets ledger line, `gpu-session.sh`.
- **Hardening**: three audit rounds (17, 12 and 13 findings, all fixed;
  m5-w0-harden), then **the speed review** under the owner's policy of
  2026-10-10 (speed wins where the trade is sensible; gates are
  tripwires): release opt-levels re-chosen for speed, caches and the
  THP setting, with both `.text` gates re-based to 18,350,080 B and
  22,282,240 B (m5-speed and the `m5-speed-*` sections).

### Wave 1 (on `main` at `6cee276`)

- **S-ipc**: `strand get | toggle | call | clear` over the socket and the
  token overlay layer (`$XDG_STATE_HOME/strand/tokens.toml`, "overlay
  wins" and its `[clear]`).
- **S-inspect, compiler and scene half**: identity, kept-state badges,
  `reset_subtree`, provenance chains, `dump` with passwords redacted.
- **S-lsp**: the missing-key quick fix and extract component; the LSP
  box ticked.
- **S-docs** (writing): `docs/spec.md` and guide levels 1–3; **S-tools**
  and **S-plugins** (writing): the theming hub's design, approved by the
  owner, and the plugin builder's notes.

### Wave 2 (on `main` at `aa3053c`)

- **S-lsp**: the tree-sitter grammar package and its conformance test
  (M1's last open box and M5's).
- **S-ipc**: the D-Bus front end (`io.github.jakeb_grant.Strand`, typed
  methods and `Changed`), and the inspector's `inspect.node`,
  `inspect.dump` and `reset{node}` arms; the CLI + IPC box ticked.
- **S-palette**: the 13 hue roles (62 roles), importers' extras and the
  palette's identity (`$palette.*`), the `json` and `base24` writers.
- **S-tools**: templates, `strand export json | base24 | --template`
  offline and over the socket, and the `file` element with its
  `strand-files` writer; in its wave-3 slot (merged with wave 2),
  `strand new` and `strand report` with GPU frame timing from timestamp
  queries.
- **S-plugins**: `strand::Shell::builder().service::<S>().run()`,
  `--write-schemas` and plugin schema files read by `check`, `run` and
  the LSP; the plugins box ticked.
- **S-docs**: guide levels 4 and 5 and the guide's check test.

### Wave 3 (on `laptop/m5-w3`)

- **S-fixups**: wave 2's stragglers (drop rows and the drag identity,
  `rw` writes to a config service, `Document::parse`, drawn-only
  percentiles, `export -o` through the `file` writer, the tray `open()`
  doc), and `strand report`'s grim reference PNGs on the frozen-clock
  acceptance mock (the owner deferred them to the integrator, who made
  them).
- **S-inspect, render and binary half**: pick across surfaces, the
  outline and the 300 ms repaint flash on CPU and `GpuPresent` surfaces,
  `inspect.pick | highlight | flash`, and `strand-dev inspect` with safe
  write-back; the Inspector box ticked.
- **S-platform**: xdg-activation for notification actions and app
  launches (ordered, at most 200 ms per token); the box ticked, checked
  on sway, niri and Hyprland.
- **S-docs**: the ladder test, `docs/reference/` generated and checked
  current, the newcomer test, the agent-proxy harness, spec.md reviewed
  for the freeze; the Docs box ticked.
- **The integrator's sweep**: Hyprland's activation policy through
  `hyprctl eval`, the `run:` THP restore, the equality rule landed
  (`==` across units by the ordering rule), canvas paint order pinned by
  a PNG test, the node pane's "why" for a `shader` node, the promoted
  GPU cost measured on hardware (its box ticked), a strand-auth CI flake
  fixed, and the language freeze.

### Tests

M5 added 353 `#[test]` functions and removed 7 (`git diff 70cdc56`;
2,463 in the tree), and 34 reference PNGs: 27 for the newcomer path, 5
in strand-render (the inspector's outline and flash at 1× and 2×,
canvas paint order) and 2 for `strand new` on two outputs. 158 commits
from `70cdc56` (M4's close) to `672b114`.

## Budgets

| Budget | Gate | Laptop (`672b114`) | CI (`672b114`) |
| --- | --- | --- | --- |
| `.text`, CPU-only (`--no-default-features`) | 18,874,368 B | **18,711,127 B** (163,241 B to spare) | 18,709,202 B |
| `.text`, default build (GPU backend) | 22,806,528 B | **22,630,295 B** (176,233 B to spare) | 22,628,370 B |
| design.md's bar, 2×2560×1440, real services | 42 MB target (warns), 48 MB ceiling (49,152 kB, fails) | **37,328 kB** (anon 12,744, file 22,088, shmem 2,496) | 38,912 kB (anon 12,852, file 23,756, shmem 2,304) |
| full shell: launcher open, two toasts, OSD | 76 MB target, 86 MB ceiling (88,064 kB) | **51,944 kB** (4 entries), **62,204 kB** (161) | 66,422 kB (15 entries), 68,070 kB (172) |
| full shell, launcher closed, toasts up | the same | **46,724 kB** (4), **53,268 kB** (161) | 54,106 kB (15), 55,290 kB (172) |

### `.text`

`scripts/size-report.sh` at `672b114` gives the same figures as at
`9e3a91d`, where the sweep measured them (the commits between are docs
only). `cargo test --release -p strand --test budgets -- --nocapture
--test-threads=1` passes in the container at `672b114` (11 passed, 1
ignored: the M3 screenshot writer), and GitHub's budgets job passes
both builds' gate.
Against M4's merged figures (15,657,671 B and 19,041,095 B) M5 added
**+3,053,456 B** CPU-only and **+3,589,200 B** GPU. Most of that is wave 0's
speed review, which re-chose the release opt-levels for speed and took
CPU-only `.text` to 17,961,815 B (`f61acec`) before any M5 feature
landed (the gates went to 18,350,080 B and 22,282,240 B for it); the
features M5 built cost **764,800 B CPU-only and 785,152 B GPU** in the
ledger line's terms (which also count 0b's 15,360 B). By wave (CPU-only, on each merged tree):

| Wave | Merged `.text` | Δ | Notes |
| --- | --- | --- | --- |
| speed review (wave 0) | 17,961,815 B (`f61acec`) | — | the base the M5 features are charged against |
| 1 | 18,077,143 B | +115,328 B | S-ipc and S-inspect's library over their lines; accepted (m5-integration-w1) |
| 2 (with S-tools' `new` and `report`) | 18,555,351 B | +478,208 B | over the gates; the owner ruled the overruns "still don't seem like that big of a deal in the scope of the entire binary" and the gates were re-based to 18,874,368 B / 22,806,528 B (m5-integration-w2) |
| 3 | 18,711,127 B | +155,776 B | S-platform +94,272 B, S-inspect +35,008 B, S-fixups +4,992 B, the rest the merges and the sweep; over wave 3's room by 50,240 B, inside the gates; accepted under the standing policy (m5-integration-w3, sweep-3) |

No gate was crossed by the final tree, and none was raised in wave 3.
The ledger with every stream's line is m5-plan's "Ledger and
allocations". **Proposed close gate: keep both** (the speed review's
rule, measured + unspent lines + 64 KiB rounded up to 256 KiB, gives
today's values, there being no unspent line left); m5-plan's original
"next 64 KiB above the measured size" (18,743,296 B and 22,675,456 B)
is the owner's alternative. The gate test keeps today's values until
the owner says.

### Memory

Gates are tripwires for accidental regressions in CI, never caps on a
user's shell (owner, 2026-10-10); M5 set them by judgement with
recorded headroom (decisions.md m5-speed, m5-open-budgets). The
resident split of the design bar (PSS, laptop / CI): anon
**12,744** / 12,852 kB, file **22,088** / 23,756 kB, shmem
**2,496** / 2,304 kB; at M4's close it was 34,187 kB (anon
12,556, file 19,295, shmem 2,336) on the laptop. The growth is almost
all file-backed: the binary's own mapped code (`.text` +3 MB over M4,
most of it the speed review's), not heap. The full shell's figures
are inside their targets on both machines.

### The promoted GPU surface on hardware (reported, never gated)

Intel PTL (ANV, Mesa 25.2.8) presenting to a GLES2 sway on renderD128,
`STRAND_GPU_PRESENT=1 scripts/container/gpu.sh cargo test --release -p
strand --test gpu_idle`, three cycles, the figures read from `strand
report --json` while `Glow` (640×420, an animated shader with bloom) is
promoted: frame p50 1.09–1.10 ms, p95 3.87–4.18 ms, p99 4.53–5.08 ms;
PSS +21.0–26.1 MB over the 27,140 kB pre-GPU baseline while promoted,
+6.35 MB after the drop (bound 12 MiB), +2.18 MB at most between cycles
(bound 4 MiB). The frame time excludes the swapchain wait and other
surfaces' queue time (decisions.md m5-tools, round 3).

## Timing

GitHub's timing job enforces these; the laptop treats them as advisory
(CLAUDE.md). CI at `672b114`:

| Gate | Test | CI |
| --- | --- | --- |
| reload p95: 35 ms token, 50 ms markup at design.md's model | `strand --lib reload_latency` (timing profile) | token p95 16.2 ms headless (break 19.3), 31.9 ms on the vblank model; markup 16.1 ms headless, 32.1 ms on the model |
| theme swap, 8 scopes on `spring(1600, 1)`, whole swap within 5 ms | `strand-render --test theme_swap_bench` | 2.43 ms (p95 3.08) |
| theme swaps between palettes | the same | medians 1.35–1.83 ms |
| `file` outputs after a flip within 5 ms | the same | 0.19 ms (10 in the current mode), 0.74 ms (10 reading both) |
| 2,000-row list scroll, frame p95 within 3.5 ms | `strand-render --test list_scroll_bench` | 60 Hz p95 1.07 ms, 144 Hz p95 0.99 ms, 0 stalled frames |
| animated frames within the refresh | `strand-render --test motion` | launcher enter median 1.31 ms, worst 1.69 ms; toast exit median 0.04 ms, worst 0.77 ms |

## Owner rulings in M5

- **2026-10-10, Q1–Q20** (decisions.md m5-owner): every plan
  recommendation accepted except Q12 (the owner named the D-Bus name
  `io.github.jakeb_grant.Strand`) and Q20 (M5 is not 1.0).
- **2026-10-10, speed over size**: gates are regression tripwires that
  agents set by judgement with recorded reasons; memory is not sacred
  (m5-speed-policy, m5-speed-gpu-bound).
- **2026-10-11, wave 1**: the hardening's four interface choices kept;
  the theming hub's design approved (no per-app mappings or templates);
  the plugin CLI move accepted (m5-w1-owner, m5-tools,
  m5-integration-w1).
- **2026-10-11, wave 2's `.text` overruns** accepted and the gates
  re-based (m5-integration-w2).
- **2026-10-11, `strand report`'s grim reference PNGs** deferred to the
  integrator, who made them on the frozen-clock mock (m5-fixups).

## What remains

For the owner, before M5 is called closed (handoff.md, "Open items"):

1. ~~The newcomer exit~~: ticked 2026-10-13 by Q15 (ii), the automated
   test and the agent proxy (above, "Human trials").
2. The close gate: keep both `.text` gates (proposed) or tighten to
   the next 64 KiB.
3. The `why` field of `inspect.node`'s answer, written into
   architecture.md as proposed.
4. The fast-forward of `main` once the laptop's `run.sh ci` and
   `matrix.sh` and GitHub CI pass on the closing head.

For M6 or later (recorded, none a blocker; handoff.md, "Known limits"):

- a drop's `at` does not see a copy outside a windowed `list`'s mounted
  rows (spec.md 6.7; a bug fix in 1.x, not a language change);
- write-back's rename window (POSIX has no compare-and-rename);
- no IPC adapter for labwc, wayfire or river (the wlr toplevel fallback
  covers window state);
- streaming PAM prompts (`pam_fprintd`) need a protocol message and a
  lock prompt design.md does not have;
- `Notification.time` has no time of day; remote media art is not
  fetched; 802.1X and WEP networks are refused;
- AccessKit (Q3) and any schema additions arrive under spec.md 14's
  policy;
- the 1.0 release itself, when the owner has used Strand for a while
  (Q20).
