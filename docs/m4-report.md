# M4 exit report

Measured 2026-10-10 on the owner's laptop (Intel Core Ultra X7 358H, 16
threads) through the container suite (`scripts/container/run.sh`:
Ubuntu 24.04 with CI's packages, rustc 1.97.0, headless sway 1.9 with
the pixman renderer, lavapipe from Mesa 25.2.8 as the only Vulkan
driver), at `e765276` on `laptop/integration-m4-w3`: M4 waves 0–3
merged, the m4 audit's three rounds of fixes, and the closing
integrator's one code fix (Lock). The commits after `e765276` change
docs, two tests' harnesses, and the GPU thread's answer to a frame
or pass that panics (decisions.md m4-integration-w3, closing). Release figures use the workspace's release profile (fat
LTO, one codegen unit, mimalloc, the 40 per-package `opt-level`s of
`Cargo.toml`; thin LTO until m4-integration-w2, which switched because
thin LTO broke the `.text` gates). The latency benches use the
`timing` profile (release's opt-levels without LTO). CI's figures are
from GitHub run 38055358381 at `e765276`, read from its logs. Every
number below comes from a test in the tree; the tests that gate fail
when their gate is missed, and the ones that only report are marked
so.

M4's three exits are ticked in `docs/features.md`, with 14 of its 15
boxes. The open box is the bundled noise field, which waits on the
owner (Open).

## What M4 delivered

- **GPU, in every build** (decisions.md m4-owner). `strand-gpu` owns one
  wgpu device on its own thread. A large animation that lasts 500 ms
  is promoted to vello_gpu and presented through wgpu's WSI, switching
  only between settled frames; a `shader` on a CPU surface is drawn
  offscreen and read back. The device drops 30 s after the last GPU
  frame. With no device, everything stays on the CPU and the inspector
  and `strand report` say why.
- **Shaders and canvas**: `.wgsl` files checked by naga against their
  uniforms at check time, hot-reloaded, and refused when their private
  memory passes 16 KiB per pixel. `canvas` draws on the CPU.
- **The bundled GPU effects**: bloom, liquid glass, fresnel rim,
  particles above 1,000, 3-D tilt, raster wobble, CRT and chromatic
  aberration, aurora and large backdrop blur. Each starts only while
  visible and has a CPU version.
- **The effects catalogue** in the scene IR and on the CPU raster:
  shapes and morphing, strokes, arcs, goo, glow, inner shadow, rim,
  grain, text effects, filters, blend modes, masks, named curves, pose
  presets, time signals, keyframes, stagger, shared-element `morph`,
  rolling numbers, jelly, parallax, particles, transition masks,
  `spectrum`, graphs, GIF/APNG/WebP and Lottie, with effect layers,
  cached offscreen groups and per-node clocks.
- **Lists that mount only their window**: 2,000 rows scroll with logic
  mounting only the rows in view, wheel steps that spring, flings that
  decay, `nav` past the window, drag and drop with typed `Drop`, and
  directional `pages` transitions.
- **Compositor-animated poses** (alpha modifier, viewporter, layer-shell
  margins), tray menus as nested xdg_popups, single-pixel scrims,
  `attach: top` fillets, and `open: <-> x` written false by Escape,
  click-away and focus loss.
- **The blur ladder**: `ext-background-effect-v1`, Hyprland rules
  (`strand compositor-rules`) and the tint fallback.
- **The lock screen** on `ext-session-lock` with a forked PAM helper
  (`strand-auth`) that fails closed, exempt from reload, with a restart
  marker that locks again after a crash, tested only in a KVM guest
  with real PAM.

## Result

| Exit (`docs/features.md`, M4) | Gate | Measured | |
| --- | --- | --- | --- |
| Smooth 2,000-row scrolling | render frame p95 within 3.5 ms at 60 and 144 Hz on an optimised build, no frame stalled with logic answering 50 ms late (`crates/strand-render/tests/list_scroll_bench.rs`); on headless sway over 2,000 mock apps, no gap in any frame and each settled screenshot the one before moved by exactly the scroll (`crates/strand/tests/demo.rs::the_design_launcher_scrolls_2000_apps`) | 60 Hz: 408 frames, median 0.84 ms, **p95 1.27 ms**, worst 1.72 ms; 144 Hz: 929 frames, median 0.77 ms, **p95 0.96 ms**, worst 1.01 ms; **0 stalled frames** (216 and 520 windows answered 50 ms late); CI: p95 1.48 and 1.40 ms, 0 stalled; the e2e test green in the release budgets run | pass |
| GPU released when idle | the GPU thread gone and nothing waking after the idle drop; PSS after a second promote/drop cycle within 3 MiB of the first (`crates/strand/tests/gpu_idle.rs::gpu_is_released_when_idle`) | lavapipe (debug): thread gone and no context switch in 2 s after each of two cycles; PSS 65,283 kB before the GPU, 155,321 then 151,201 kB after the drops; the presented strip matches the CPU's (0 pixels past 6, worst 3) | pass |
| Lock fails closed under faults | every fault of m4-plan's matrix under real PAM with no desktop pixel in any shot, the fallback shown, only the right password unlocking (`scripts/lockvm/scenarios/all.sh`) | all four scenarios at `6a27238` (`session_lock` 8 of 8, `strand_lock` 27 of 27, `faillock`, `pam` with both services); `strand_lock` 29 of 29 and `pam` in the audit's third round; CI's `lock-vm` job green at `e765276` | pass |

The tests behind each exit are listed in m4-plan.md's "Exit criteria
and their tests" and cited on features.md's exit line.

## Budgets

| Budget | Gate | Laptop (`e765276`) | CI (`e765276`) |
| --- | --- | --- | --- |
| `.text`, default build (GPU backend) | 19,398,656 B (18.5 MiB) | **19,006,471 B** (392,185 B to spare) | 19,004,482 B |
| `.text`, CPU-only (`--no-default-features`) | 15,728,640 B (15 MiB) | **15,634,439 B** (94,201 B to spare) | 15,632,450 B |
| design.md's bar, 2×2560×1440, real services | 34,816 kB target (warns), 38,912 kB ceiling (fails) | **34,187 kB** (anon 12,556, file 19,295, shmem 2,336) | 35,617 kB (over the target: warned) |
| full shell: launcher open, two toasts, OSD | 64 MB target, 70 MB ceiling | **42,465 kB** (4 desktop entries), **50,457 kB** (161) | 49,120 kB (15), 51,931 kB (172) |
| full shell, launcher closed, toasts up (gated since the audit's third round) | the same | **36,493 kB** (4), **40,985 kB** (161) | 39,610 kB (15), 41,919 kB (172) |
| frame time, launcher enter and toast exit | every frame within 16.7 ms, median within 8.3 ms (`crates/strand-render/tests/motion.rs::animated_frames_fit_the_refresh_budget`, timing job) | launcher median 2.28 ms, worst 6.35 ms; toast median 0.03 ms, worst 0.76 ms | launcher median 1.85 ms, worst 2.28 ms; toast median 0.06 ms, worst 0.98 ms |

Across the audit `.text` grew 13,504 B in the default build and 8,704 B
in the CPU-only build (CI at `70af44f`, 18,990,978 and 15,623,746 B,
against CI at `e765276`; the laptop's builds come out about 2 kB
larger than CI's). The CPU-only margin is about 94 KB, down from the 216 KB the
wave-3 brief quoted: most of that went to the wave-3 merges before the
audit. No budget, gate or target was raised.

The bar sits at its target on the laptop (34.1–34.5 MB across the
integration's runs) and just over it on GitHub's runners: 35,557 kB at
`70af44f` and 35,617 kB at `e765276`, which warns; the ceiling is
3.3 MB above that. The runners measure the bar 1.4 MB higher than the
laptop's container, and the full shell's bar alone 2.9 MB higher
(36,972 against 34,089 kB); why was not broken down. M3's CPU-only
build measured 31.7–32.6 MB (docs/m3-report.md); the difference is the
GPU backend's code linked into every build, cold but mapped
(docs/architecture.md, "`strand-gpu`"; the spike predicted it within
0.1 MB).

The launcher-closed figure was 136–147 MB before the audit's third
round, on the laptop and in CI: the closing launcher was promoted,
because frames seconds apart counted as one 500 ms animation, and
lavapipe's libLLVM stayed mapped. A pause past `RUN_GAP` (250 ms) now
ends the run (`crates/strand-render/src/promote.rs`), and the
full-shell budgets fail if strand's log has any `GPU: ` line
(decisions.md m4-audit, "large frames seconds apart are not one
animation").

## Idle wakeups

design.md: nothing wakes while nothing changes, and the GPU device is
gone after its idle drop.

| Test | What it holds | Result |
| --- | --- | --- |
| `crates/strand/tests/budgets.rs::the_design_bar_on_the_real_services_keeps_the_budget` (release) | the design bar on every real backend: 10 s with no context switch in any thread, no thread started or ended, no frame | **0** switches (31 of 31 watched directories mirrored) |
| `crates/strand/tests/services.rs::the_real_services_sleep_when_nothing_changes`, `::the_m3_services_sleep_when_nothing_changes` (release) | M3's services idle clause, unchanged | **0** wakeups |
| `crates/strand/tests/gpu_idle.rs::gpu_is_released_when_idle` (lavapipe; CI's test job and the container suite) | after each idle drop: no `strand-gpu` thread, no context switch in any thread for 2 s | **0**, both cycles |
| `crates/strand-render/src/promote.rs::tests::drops_after_30s_with_one_wake` | the idle drop costs one wake, at its deadline | one wake |
| `crates/strand-render/tests/gpu_effects.rs::bundled_effects_start_the_gpu_only_while_visible` | a hidden bundled effect starts nothing and holds nothing | pass |

## GPU

On lavapipe, in CI and in the container suite (`STRAND_REQUIRE_GPU=1`,
`STRAND_GPU_SOFTWARE=1`), the readback and presented frames match the
CPU's within the GPU tolerance, and the idle drop leaves no thread and
no wake. Lavapipe's own cost stays after the drop: PSS 65,283 kB before
the GPU and 151–155 MB after, nearly all of it the libLLVM and
`libvulkan_lvp` mappings a software driver keeps once loaded. The test
asserts no growth between cycles, which lavapipe meets (4,120 kB less
after the second drop), not a return to the baseline, which it cannot
(decisions.md m4-scene).

The hardware leg (`STRAND_STRICT_GPU=1 scripts/container/gpu.sh cargo
test --release -p strand --test gpu_idle`, advisory) passed on the
laptop's Intel GPU (8086:b080, ANV) at `e765276`: PSS 25,732 kB before
the GPU and 30,455 kB after the first drop, **4,723 kB over the
baseline**, inside the leg's 6,144 kB bound; the read-back strip
matched the CPU's (0 pixels past 6, worst 3). ANV's WSI cannot present
on the leg's pixman sway (no linux-dmabuf), so the panel ran in
`Readback` mode, and **the promoted cost against design.md's +20–40 MB
is still unmeasured on hardware**: presenting through ANV needs a
compositor on a KMS card, which the laptop rules keep out of
containers.

The closing run of the leg found one real difference from lavapipe. On
ANV the kernel resets a context running a shader that never ends after
about 6 s, before `HUNG_AFTER`, and wgpu then panics in `poll` ("Parent
device is lost"). The thread caught the panic and reported the device
lost, but it never answered the pass, against its contract that every
frame and pass is answered. It now answers `Failed` first
(`crates/strand-gpu/src/thread.rs::tests::a_panicking_pass_or_frame_is_answered_and_loses_the_device`),
and `crates/strand-gpu/tests/hang.rs` accepts either way of losing
the device; it passes on lavapipe (10 s) and on ANV (6 s).

From the audit: a readback that never ends loses the device after
`HUNG_AFTER` (10 s, `crates/strand-gpu/tests/hang.rs`); a readback
frame or pass the GPU never answers holds its surface at most
`GPU_WAIT` (8 ms), then the CPU paints it
(`crates/strand-render/tests/gpu.rs`); a lock surface is never handed
to the GPU thread
(`crates/strand-surface/tests/session_lock.rs::a_lock_surface_is_never_handed_to_the_gpu`);
the pipeline and readback caches are bounded; the run joins the GPU
thread before its Wayland connection goes; and a shader with more than
16 KiB of private memory per pixel is refused at check time.

## Lock

The lock runs on `ext-session-lock` with a forked PAM helper
(`strand-auth`). Every fault in m4-plan's matrix was injected in a KVM
guest with real `pam_unix` (`scripts/container/lockvm.sh`, CI's
`lock-vm` job), never on a real session: logic panic and hang, the text
worker's death, the helper crashing, hanging, answering garbage,
missing, SIGKILLed mid-check and deleted then killed mid-session, a lock
runtime fault, no first frame, no lock compiled, SIGTERM, SIGKILL and
SIGABRT with a restart (also while the lock is still pending), a
supervisor's restart loop, `finished` after `locked`, a refused lock
and hotplug. A healthy lock's first frames took 570–612 ms in the
guest, against the 1,000 ms deadline.

The audit found and fixed three holes in "fails closed": a fallback
that looked for a deleted helper only once; a config with no path to
`auth.submit`, now the check error `check::lock_no_auth`; and the
`strand` PAM service chosen where libpam would not read its file, which
denied every password. It also stopped a `STRAND_MOCK` run from taking
a lock that nothing could release, and now writes the restart marker as
soon as a lock is asked for.

The closing integrator's fix: that mock rule logged a WARN on every
mocked run, and the design bar's layout test fails on any WARN line, so
CI's `test` and `budgets` jobs failed at `ffa8438` (run 38054473409).
The line is now at info (decisions.md m4-audit, round 3, "no session
lock under `STRAND_MOCK`").

## Decisions the owner made in M4

All on 2026-10-09 (decisions.md m4-owner):

- GPU tests run on lavapipe in the container and CI, enforced, with an
  advisory leg on the laptop's GPU through `/dev/dri/renderD128` only.
- The GPU backend is in every build, not an opt-in feature. The default
  build's `.text` gate is the spike's size plus about 10% (18.5 MiB);
  the CPU-only build keeps 15 MiB.
- The four bundled effects design.md left unnamed are new values of
  existing props: `filter: bloom(r)`, `crt()`, `chromatic(px)`,
  `wobble(amp)`, and `backdrop: glass()`.
- A missing `/etc/pam.d/strand` falls back to PAM's `login` service with
  a one-time warning, not to `other`.
- The lock VM container gets `/dev/kvm`.
- `letters` keeps no positional; it animates its enclosing `text`.

M3's release-profile sign-off (laptop-decisions, owner decision 1)
predates the switch to fat LTO and 40 overrides (m4-integration-w2);
handoff.md notes it.

## Open

Waiting on the owner (decisions.md m4-gpu-effects, m4-audit):

- **The bundled noise field** (features.md's open M4 box): design.md
  counts "aurora and noise fields" among the eight bundled effects but
  names no spelling for one, and inventing one would add syntax.
- **A hung frame on a presented surface that is not a lock** is bounded
  only by the WSI's acquire timeout. A lock is not affected (GPU).
- **`theme_swap_bench`'s 8-scope `spring(1600, 1)` gate** has little or
  no headroom on GitHub: 2.53–5.06 ms over fifteen timing jobs against
  5 ms, one failure. The choices are to accept the occasional failure,
  use a larger runner, or make the swap cheaper. The laptop measured
  1.64 ms.
- **The shape list**: whether to trim the 13 shapes to design.md's five
  plus polygons (decisions.md m4-audit, "the shape list is a reading").
- **xdg-activation** (the notification ActivationToken and the launch
  token) moves past M4, to be scheduled.

Not measured, and why:

- **The promoted GPU cost on hardware**: ANV cannot present on the
  advisory leg's sway (GPU).
- **Reload latency on the laptop** misses its gate in the container
  (token p95 22.2 ms headless against a break at 20.1 ms), as it has
  since laptop-open; GitHub's timing job, which enforces it, passes.

Left to their crates' owners:

- Two functional assertions still bound wall-clock time, with wide
  margins: `strand-scene/src/tokens.rs::huge_fan_out_fails_fast`
  (< 500 ms) and `strand-dev/tests/lsp.rs`'s hung-bus case (< 1500 ms).
- PAM's interim messages (a fingerprint prompt) reach the lock only
  with the verdict; streaming them needs a protocol message and a lock
  prompt design.md does not have. README asks for a password-only
  `strand` PAM file meanwhile.

Closing steps: merging `laptop/integration-m4-w3` to `main` and deleting
the merged wave branches. (Done: the owner merged it at `3dc3f71`, and
the merged branches are deleted.)

## CI tiers

M4 added these to M3's:

- `lock-vm`: the KVM guest, run on lock paths, nightly and on tags.
- The timing job's list-scroll bench and frame-time bench.
- The lavapipe GPU tests inside the test job (`STRAND_REQUIRE_GPU=1`,
  `STRAND_GPU_SOFTWARE=1`).
- The CPU-only `.text` gate in the budgets job.
- `cargo test -p strand-auth --features faults`, the helper's PAM tier.

The compositors job runs the matrix, thumbnails included, on sway,
labwc, niri and Hyprland (nightly on `archlinux:latest`). A missing
capture global fails everywhere but niri.
