# M4 exit report

Measured 2026-10-10 on the owner's laptop (Intel Core Ultra X7 358H, 16
threads, on battery or mains as it was) through the container suite
(`scripts/container/run.sh`: Ubuntu 24.04 with CI's packages, rustc
1.97.0, headless sway 1.9 with the pixman renderer, lavapipe as the only
Vulkan driver), at `6a27238` on `laptop/integration-m4-w3` (M4 waves
0–3 merged, with the m4 audit's fixes; the commits after it change only
docs). Release figures use the workspace's release profile (thin LTO,
one codegen unit, the per-package `opt-level`s); the latency benches
use the `timing` profile. CI's figures are from run 38048149054 at
`70af44f`, the integration branch before the audit's last fixes, read
from its notices. Every number below comes from a test in the tree;
the tests that gate fail when their gate is missed, and the ones that
only report are marked so.

M4's three exits are ticked in `docs/features.md`, with 14 of its 15
boxes. The open box is the bundled noise field, waiting on the owner
(below).

## Result

| Exit (`docs/features.md`, M4) | Gate | Measured | |
| --- | --- | --- | --- |
| Smooth 2,000-row scrolling | render frame p95 within 3.5 ms at 60 and 144 Hz on an optimised build, no frame stalled with logic answering 50 ms late (`crates/strand-render/tests/list_scroll_bench.rs`) | 60 Hz: 408 frames, median 0.84 ms, **p95 1.22 ms**, worst 2.29 ms; 144 Hz: 929 frames, median 0.77 ms, **p95 0.96 ms**, worst 1.02 ms; **0 stalled frames** (216 and 520 windows answered 50 ms late) | pass |
| GPU released when idle | the GPU thread gone and nothing waking after the idle drop; PSS across two promote/drop cycles within 3 MiB (`crates/strand/tests/gpu_idle.rs`) | thread gone, no context switch in 2 s, after each of two cycles; PSS 151,973 then 149,361 kB (debug build, lavapipe); the bloomed box 78 levels over the plain one in both cycles | pass |
| Lock fails closed under faults | every fault of the matrix with no desktop pixel in any shot, the fallback shown, only the right password unlocking (`scripts/lockvm/scenarios/all.sh`) | `session_lock` 8 of 8, `strand_lock` 27 of 27, `faillock`, `pam` (both services), at `6a27238` | pass |

## Budgets

| Budget | Gate | Laptop (`6a27238`) | CI (`70af44f`) |
| --- | --- | --- | --- |
| `.text`, default build (GPU backend) | 19,398,656 B (18.5 MiB) | **19,004,103 B** (394,553 B to spare) | 18,990,978 B |
| `.text`, CPU-only (`--no-default-features`) | 15,728,640 B (15 MiB) | **15,634,247 B** (94,393 B to spare) | 15,623,746 B |
| design.md's bar, 2×2560×1440, real services | 34,816 kB target (warns), 38,912 kB ceiling (fails) | **34,425 kB** | 35,557 kB (over the target: warned) |
| full shell, launcher, two toasts, OSD | 64 MB target, 70 MB ceiling | **44,147 kB** (4 desktop entries), **50,551 kB** (161) | 52,040 kB (15), 51,877 kB (172) |
| frame time, launcher enter and toast exit | every frame within 16.7 ms, median within 8.3 ms (`motion.rs::animated_frames_fit_the_refresh_budget`, timing job since the audit) | launcher median 2.30 ms, worst 6.50 ms; toast median 0.03 ms, worst 0.75 ms | not run before the audit |

The audit's fixes cost 13,125 B of `.text` in the default build and
10,501 B in the CPU-only build (laptop at `6a27238` against CI at
`70af44f`, so the two machines' builds may differ slightly too). The CPU-only build's margin
is now about 94 KB, down from the 216 KB the M4 plan's wave-3 brief
quoted: most of that went to the wave-3 merges before the audit. No
budget, gate or target was raised.

The bar sits at its target: 34.1–34.5 MB across the integration's
laptop runs and 35.6 MB in CI's, where the target warns and the
ceiling (38.9 MB) is 3.3 MB away. M3's CPU-only build measured
31.7–32.6 MB (docs/m3-report.md); the difference is the GPU backend's
code linked into every build, cold but mapped (docs/architecture.md,
"`strand-gpu`": the spike predicted it within 0.1 MB).

## GPU

The backend is in every build (decisions.md m4-owner). A large
animation promotes after 500 ms and presents through wgpu's WSI; a
shader on a CPU surface is drawn offscreen and read back; the device
drops 30 s after the last GPU frame (shortened in the test). On
lavapipe the readback and presented frames match the CPU's within the
GPU tolerance. The hardware leg (`scripts/container/gpu.sh`, advisory)
passed in release on ANV in readback mode, with PSS 4,636 kB over the
pre-GPU baseline after the drop (bound 6 MiB); ANV cannot present on
that leg's sway, so the promoted cost against design.md's +20–40 MB is
still unmeasured on hardware (handoff.md).

A readback that never ends now loses the device after 10 s
(`HUNG_AFTER`, `crates/strand-gpu/tests/hang.rs`), a lock surface is
never handed to the GPU thread (`crates/strand-surface/tests/session_lock.rs::a_lock_surface_is_never_handed_to_the_gpu`),
the run joins the GPU thread before its Wayland connection goes, and a
shader with more than 16 KiB of private memory per pixel is refused at
check time (decisions.md m4-audit).

## Lock

The lock runs on `ext-session-lock` with a forked PAM helper
(`strand-auth`), and every fault in m4-plan's matrix was injected in a
KVM guest with real `pam_unix`, never on a real session. The matrix
passed on 2026-10-10 at `70af44f` and again at `6a27238`, after the
audit's fixes. A healthy lock's first frames took 612, 570 and 575 ms
in the guest, against the 1,000 ms deadline. The audit added three
things. A lock from which nothing calls `auth.submit` is now the check
error `check::lock_no_auth`, because it would lock with no way out. A
plain test build no longer leaves a helper with the fault hooks in
`target/`. And the `strand` PAM service is used only where libpam will
read its file; otherwise the helper falls back to `login`.

## Open

- **The bundled noise field** (features.md's open M4 box): design.md
  counts "aurora and noise fields" among the eight bundled effects but
  names no spelling for one. It waits on the owner (decisions.md
  m4-gpu-effects).
- **The promoted GPU cost on hardware** is unmeasured (above).
- **A hung frame on a presented surface that is not a lock** is
  bounded only by the WSI's acquire timeout. This is for the owner
  (decisions.md m4-audit).
- **PSS after the GPU has run on lavapipe stays high.** It is reported
  here and not gated. In the debug `gpu_idle` run, PSS stood at 64,820
  kB before the GPU and 151,973 kB after the drop. In the release
  budgets, the full shell's "launcher closed" figure is 142,654 and
  146,816 kB on the laptop and 136,024 and 138,746 kB in CI, against
  44–52 MB with the launcher open. The thread is gone and nothing wakes,
  so what stays is most likely the software driver's mappings, which
  this audit did not break down; the hardware leg's 4.6 MB after the
  drop is the figure design.md's budget is about. Neither test gates this
  number. Whether the launcher's exit is promoted at all is worth a
  look, because a closing panel should not need the GPU (handoff.md).
- **Two wall-clock functional assertions** remain, with wide margins:
  `strand-scene/src/tokens.rs::huge_fan_out_fails_fast` and
  `strand-dev/tests/lsp.rs`'s hung-bus case. They are left to their
  crates' owners (decisions.md m4-audit).
- **xdg-activation** moves past M4: the notification server's
  ActivationToken and the launch token (decisions.md m4-audit).
- **Merging** `laptop/integration-m4-w3` to `main` and deleting the
  merged wave branches.

## CI tiers

M4 added these tiers to M3's:
- `lock-vm`: the KVM guest, run on lock paths, nightly and on tags.
- The timing job's list-scroll bench and frame-time bench.
- The lavapipe GPU tests inside the test job (`STRAND_REQUIRE_GPU=1`,
  `STRAND_GPU_SOFTWARE=1`).
- The CPU-only `.text` gate.
- `cargo test -p strand-auth --features faults`, the helper's PAM tier,
  since the audit.

The compositors job runs the matrix, thumbnails included, on sway,
labwc, niri and Hyprland (nightly on `archlinux:latest`). Since the
audit, a missing capture global fails everywhere but niri.
