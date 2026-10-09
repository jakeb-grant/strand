# M4 plan

How M4 (features.md, "M4 Power features") is built in parallel streams. It
merges the execution plan with its review and the owner's answers
(decisions.md, m4-owner, 2026-10-09). design.md, architecture.md and
features.md still win; this file only says who builds what, in which order,
and how it is proved.

Exit: smooth 2,000-row scrolling · GPU released when idle · lock fails
closed under faults.

## Settled before work starts

- **GPU tests** run on lavapipe in the container image and in GitHub CI,
  where they are enforced. An advisory leg runs on the laptop's GPU through
  `/dev/dri/renderD128` only, never `/dev/dri/card*`, with its own
  compositor inside the container.
- **The GPU backend is in every build** (design.md, Paint). With no device,
  rendering stays on the CPU, bundled effects use their CPU versions, a
  `shader` node draws nothing, and the inspector and `strand report` say
  why. GPU crates stay cold until something needs them, so the PSS budget
  is unchanged. The release binary's `.text` gate becomes the spike's
  measured size plus about 10%, recorded in decisions.md with the number; a
  CPU-only build (`--no-default-features`) keeps the 15 MiB gate. A cargo
  feature to build without the GPU may exist, on by default.
- **Bundled effect syntax**: `filter: bloom(r) | crt() | chromatic(px) |
  wobble(amp)` and `backdrop: glass()` (design.md, "Bundled GPU
  effects"). 0b lands each signature in builtin.schema; after 0b the
  stream that builds an effect edits only that effect's declaration
  there (its knobs) and records them in decisions.md.
- **A missing `/etc/pam.d/strand`** falls back to `login` with a one-time
  warning; every later PAM error fails closed.
- **`/dev/kvm`** is allowed for the lock VM container only
  (`scripts/container/lockvm.sh`). The owner updates CLAUDE.md's device
  rule; streams do not edit CLAUDE.md.
- **`letters` keeps no positional**: it animates the text of its enclosing
  `text` node, with `index` and `count` in scope.
- **Clipboard stays out of M4.** design.md does not ask for it; it can
  follow drag and drop's `wl_data_device` later.

## Ground rules

- **Branches.** Each stream works in `../strand-wt/<name>` on
  `laptop/m4-<name>`, created from `main`. The integrator merges into
  `laptop/integration-m4`, then `main`. Push only your own branch; never
  force-push.
- **Builds.** At most three streams hold a build slot at once
  (`CARGO_BUILD_JOBS=6` each); the others write code or docs until a slot
  frees. Streams run only crate-scoped tests (`run.sh cargo test -p
  <crate>`). Only the integrator runs `run.sh ci` or `cargo test
  --workspace`, one at a time. Delete your worktree's `target/` when the
  stream ends.
- **Decisions.** Each stream appends only under its own heading
  `## m4-<name>`, one dated paragraph per decision.
- **Interfaces.** All cross-crate interface text lands first: 0a (everything
  but the GPU) and 0c (the GPU, after the spike). After that a stream edits
  only its own crate's architecture text.
- **Shared boxes.** Three features.md boxes span several streams:
  scrims and lock backgrounds, the blur ladder, and the effects
  catalogue. Streams append evidence inside the parenthetical; only the
  integrator ticks them.
- **`strand-scene`** belongs to S-runtime, which lands every M4 scene type
  in 0b. A later scene change is a small PR reviewed by S-runtime and
  merged by the integrator before the stream that needs it.
- **Integrator-owned files**: root `Cargo.toml` (members), `Cargo.lock`
  (regenerated on merge), and `.github/workflows/ci.yml` plus
  `scripts/container/ci.sh` once wave 0 ends. Streams ask for new CI steps
  (gpu legs, the `faults` leg, `lock-vm`, `list_scroll_bench` in the timing
  gate list) in their merge request.
- **Monoliths are split in wave 0**, with no behaviour change, into modules
  that each have one owner:
  - F0 (S-runtime): `flatten.rs`, `raster.rs`, `renderer.rs` into
    `renderer/{frame, wake, feed, backend, lists, pose}.rs`, plus `anim.rs`
    (`anim/{keyframes, morph, stagger, pages}.rs`) and `layout.rs`.
  - S0 (S-surface): `manager.rs` into `manager/{protocols, catcher, popup,
    layer, commit}.rs`.
  - Wave 0 also splits `strand/src/run.rs` into `run/{lock, feeds, lists,
    gpu}.rs`.
  - Compiler checks go into per-stream submodules: `check/{shaders,
    surfaces, effects, lists, lock}.rs`. 0b lands every M4 entry in
    `builtin.schema` and narrows `scrim: paint` to `scrim: color`.

## Streams

| Stream | Waves | Owns (exclusive) | Closes |
|---|---|---|---|
| S-infra | 0, first | `scripts/container/*`, `.github/*` until wave 0 ends | none alone; the GPU and lock harnesses |
| Integrator | 0a, 0c, every merge | architecture.md interface text, `Cargo.lock`, root `Cargo.toml`, ci.yml and ci.sh after wave 0, shared boxes | ticks of the shared boxes; m4-report, handoff |
| S-gpu spike | 0, after the infra image | a scratch crate, nothing merged | the 0c inputs |
| S-runtime | 0 (F0, 0b), 1 | `strand-scene/**`; `strand-render/src/{flatten/, raster/, clock.rs, time.rs, layers.rs, offscreen.rs, renderer/{frame,wake}.rs}`; `instantiate/convert.rs`, `lower/` (time); `builtin.schema` in 0b | Runtime box; time signals in the catalogue |
| S-surface | 0 (S0), 1, 2 | `strand-surface/src/{manager/*, placement.rs, caps.rs, solid.rs, blur.rs}`; `strand-render/src/{pose.rs, fillet.rs, renderer/pose.rs}`; `strand/src/demo/host.rs`; `services/tray.rs`; `tray.schema`; `check/surfaces.rs`; `strand compositor-rules` in `main.rs` | poses; popups and tray menus; `open: <-> x`; the blur ladder's protocol rungs; scrims and fillets |
| S-lock | 1, 2 | `crates/strand-auth/**`; `services/auth.rs`; `strand-services-schema/src/auth.schema` and its `AUTH` entry in that crate's `lib.rs`; the removal of builtin.schema's `provisional service auth` block (after 0b); `strand-surface/src/session_lock.rs`, `tests/session_lock.rs`; `strand-render/src/lock_fallback.rs`; `strand/src/run/lock.rs`; `strand/tests/lock.rs`; `check/lock.rs`; `scripts/lockvm/scenarios/*` | lock screen; lock backgrounds |
| S-lists | 1, 2 | `strand-render/src/{list.rs, scroll.rs, input.rs, renderer/lists.rs, anim/pages.rs}`; `instantiate/mount.rs`; `strand-surface/src/dnd.rs`, `tests/dnd.rs`; `strand/src/run/lists.rs`; `strand/src/mock.rs` | virtualised lists; drag and drop; pages |
| S-effects | 1, 2, 3 | `strand-render/src/{effects/**, shapes/, media/, backdrop.rs, widgets.rs (arc, graph, meter), image.rs (animated frames), anim/{keyframes,morph,stagger}.rs, renderer/feed.rs}`; `services/audio/*`; `services/wm` Capture; `check/effects.rs`; `strand/src/run/feeds.rs` | effects catalogue items; `backdrop: blur()`; M1 keyframes playback |
| S-gpu | 2, 3 | `crates/strand-gpu/**`; `strand-render/src/{promote.rs, canvas.rs, renderer/backend.rs}`; `strand-surface/src/gpu_handoff.rs`; `check/shaders.rs`; `strand/src/live.rs` (shader arms); `strand/src/run/gpu.rs`; `strand/tests/{gpu_cold,gpu_idle}.rs` | GPU promotion; shaders and canvas; bundled GPU effects; M1 shader and canvas drawing; the `.wgsl` part of M1's loader box |

Per-file ownership in shared crates: tray to S-surface, auth to S-lock,
audio and wm to S-effects. `strand/src/main.rs` belongs to S-surface; the
GPU and auth-spawn hooks there are small PRs it reviews.

Wave 0 as landed (integration-m4): the splits created only modules that
have code today, so the table's `renderer/{feed,backend}.rs`,
`anim/{keyframes,morph,stagger,pages}.rs`, `run/{feeds,lists,gpu}.rs`,
`pose.rs`, `fillet.rs`, `list.rs` and `scroll.rs` are created by the
streams that own them. The modules the table does not name are owned so:
S-runtime takes `renderer/{mod,text,layout_pass,apply,specs,tooltip}.rs`,
`anim/{mod,motion,sizes}.rs` and `layout/{mod,style,text}.rs`; S-surface
takes `renderer/surfaces.rs` and `anim/pose.rs` (with `renderer/pose.rs`);
S-lists takes `layout/list.rs` (today's scroll and virtualisation code)
with `renderer/lists.rs`; `run/{mod,logic,shell,sleep,trim,tests}.rs` are
shared, changed by small PRs that S-runtime reviews. The module maps are
in architecture.md.

### S-infra

- Lavapipe (`mesa-vulkan-drivers libvulkan1 vulkan-tools`) and
  `libpam0g-dev` in `scripts/container/Dockerfile` and
  `.github/actions/setup/action.yml` in one commit (the image is tagged by
  its hash); the compositors job gets `libpam0g-dev` too.
- Env in ci.yml and `run.sh`: `STRAND_REQUIRE_GPU=1`; `LP_NUM_THREADS=1`
  and `LP_NATIVE_VECTOR_WIDTH=256` for stable pixels; `VK_DRIVER_FILES`
  (and the deprecated `VK_ICD_FILENAMES`) pointing at lvp. No device is
  passed: lavapipe is pure CPU.
- `scripts/container/gpu.sh`: the advisory hardware leg in the Arch image
  (`vulkan-intel vulkan-icd-loader vulkan-swrast`; the laptop's Intel
  `xe` GPU needs Arch's Mesa). It passes only `--device
  /dev/dri/renderD128`, never the host Wayland socket, and starts its own
  sway. Functional assertions are strict; pixel and timing differences
  print WARN in the `gate-misses.sh` style.
- `scripts/container/Dockerfile.lockvm` (`FROM` the check image, plus
  `qemu-system-x86 virtme-ng linux-image-virtual`) and `lockvm.sh`
  (`--device /dev/kvm` and nothing else; refuses without KVM unless
  `STRAND_LOCK_VM_TCG=1`). Its first spike items:
  - `chmod` the kernel image, which Ubuntu ships 0600 root.
  - Share with 9p, or virtiofsd `--sandbox none`; virtiofsd usually
    cannot sandbox in an unprivileged container.
  - Bake the test user, `/etc/shadow` and `/etc/pam.d/strand` into the
    image (or overlay `/etc` on tmpfs): the guest cannot write a
    read-only, uid-mapped rootfs.
  - Prove setuid `unix_chkpwd` works through the share, which `pam_unix`
    needs for a non-root user.
- Tests: a `vulkaninfo --summary` step that requires lavapipe; images
  build; `ls -l /dev/kvm` fails loudly when KVM is missing.
- GitHub `lock-vm` job: a udev rule exposes `/dev/kvm` on ubuntu-24.04
  runners (public repos, since 2024-04). Its first step `ls -l /dev/kvm`
  fails rather than skips. Triggers: a paths filter (strand-auth, the lock
  modules, `*lockvm*`), nightly, `workflow_dispatch`, release tags. The
  udev and `sudo` step runs on the runner only, never in `run.sh`. If KVM
  is not there, the laptop run stays the gate the design asks for.

### S-runtime

- **F0**: the render splits above.
- **0b**: every M4 `strand-scene` type (architecture.md, `strand-scene`,
  "M4 vocabulary"), every `SurfaceHost` and `InputEvent` addition, and
  every M4 schema entry, with stubs where behaviour is pending;
  `scene_catalogue.rs` stays green.
- **F1** time-bound values end to end: the VM's symbolic value,
  `convert` to `TokenExpr` time leaves, a time value reaching logic is an
  error value, the `lower::time_signal` warning goes, render evaluates
  time per node per frame, `reduced_motion` samples every time leaf at 0.
- **F2** per-node clocks with frame caps, through `Renderer::next_wake`
  (the surface needs no change).
- **F3** the effect-layer display-list item, lowered to vello_cpu
  `push_layer`, damage grown by each effect's reach.
- **F4** cached offscreen groups (about 4 MB, freed when idle) and the CPU
  raster node.
- Tests: `strand-render/tests/damage.rs::a_time_signal_node_damages_only_itself`,
  `::hidden_time_nodes_request_no_frames`, `::capped_clocks_paint_at_their_rate`
  (fake 60/144 Hz clock), `::all_idle_clocks_stop_the_frame_loop`;
  `reduced_motion.rs::time_signals_freeze`; offscreen cache bound, reuse
  and idle-free tests; `strand-compiler/tests/vm.rs::time_values_in_handlers_are_error_values`
  (replaces `time_signals_are_warned_about_once`),
  `instantiate.rs::time_values_convert_to_token_time_leaves`.

### S-surface

- **S0**: the `manager.rs` split.
- Bind `wp_alpha_modifier_v1`, `wp_single_pixel_buffer_v1` and
  `ext_background_effect_manager_v1`; report them through
  `SurfaceHost::compositor_caps`.
- Test fake: factor `strand-services/tests/common/fake_wlr.rs`
  (`wayland-server` 0.31.14) into a shared fake compositor instead of
  writing a second one.
- Blur ladder: rung 1 sends the region as about 1 px bands per rounded
  corner, cached, re-sent only on a shape change, null when empty; rung 2
  `strand compositor-rules [dir]` prints Hyprland 0.56 rules (`--classic`
  optional); rung 3 a fallback reason per node and one diagnostic. The
  inspector half of "the inspector says why" is M5: the reason goes to the
  diagnostic and `strand watch` now (record the decision).
- Compositor-animated poses on a root's opacity, scale, x and y, delegated
  only where anchoring makes it correct.
- Single-pixel solid surfaces; the scrim is the same surface as the
  click-away catcher.
- `attach:` fillets: placement gap 0; `check/surfaces.rs` allows `attach`
  and `scrim` only on `popup` and `panel`.
- Tray menu popups: nested side placement, a recursive menu fixture, real
  click coordinates (the route is S-surface's decision).
- With S-gpu in 0c: which thread commits pose state while a surface is
  GPU-owned.

### S-lock

- New crate `strand-auth`: lib (wire protocol, `Client`, `UnlockToken`;
  libc and zeroize only) and bin (the PAM helper with its own FFI, the only
  thing that links libpam).
- The `auth` service in `strand-services`; `session_lock.rs` in
  `strand-surface` (a lock surface per output, hotplug included,
  `finished`, the unlock gate, single-pixel backgrounds once S-surface's
  helper lands, shm until then).
- A built-in fallback lock in render that needs no text worker.
- Binary wiring (`run/lock.rs`): the main loop outlives logic ending,
  panicking and SIGTERM while locked; a watchdog and a 1 s first-frame
  deadline; password inputs redacted in `strand watch`, logs and later the
  inspector.
- Compiler lock semantics: only `auth` unlocks; `lock_shown` follows the
  real lock state.
- A `faults` feature with `STRAND_FAULT` points, and a test that default
  and release binaries contain no fault code (`strings` on the binary).
- Reload exemption already exists (`LockDeferred`,
  `lock_edits_wait_for_the_unlock_and_then_land`); cite it.

### S-lists

- Windowed `mount_keyed` for a `for` that is a `list`'s direct child: row
  state kept by key, `Instance::set_list_window`, `ToLogic::ListWindow`.
- Render places rows at global indexes and asks for windows with
  overscan. Scrolling applies a paint offset with no relayout; wheel steps
  spring; touchpad flings decay (`AxisSource`, `stop`, `time`). Window
  mounts play no enter, exit or FLIP.
- Index-based `nav`; a selection lands when its row mounts.
- Directional `pages` (slide from source order; `pages` clips) and the
  ghost pairing S-effects' transition masks reuse.
- Drag and drop: `Prop::Accepts`, the Router gesture (6 px threshold,
  source follows the pointer, insertion index, spring back, Escape
  cancels), `NodeEvent::Drop`, `wl_data_device` in `dnd.rs` for external
  drops and drags between Strand surfaces.
- Owns the Router and its hook API (architecture.md, "Router hooks").
- A `STRAND_MOCK=desktop` knob for 2,000 apps in `mock.rs`.

### S-effects

- Wave 1, needing neither F3 nor F4: shapes and morphing, strokes, arcs,
  wavy meter, rolling numbers, graphs; GIF/APNG/WebP; the FFT tap in the
  audio thread; compiler lowering of keyframes, `SvgPart` and `letters`.
- Wave 2, after F3/F4: filters, blend modes, masks, glow, `inner_shadow`,
  `rim`, `grain`, cached groups, `backdrop: blur()` at quarter scale,
  `filter: blur(30)` (the blurred album art row), goo merge, particles up
  to 1,000, built-in effects (aurora as a CPU fallback), text effects,
  shared-element `morph`, transition masks, parallax and 2D tilt, Lottie,
  bindable SVG, thumbnails.
- Wave 3: `jelly` (after S-lists' drag), the `rice_now` acceptance.
- Also: pose presets and named curves exist from M2 (`PoseName`, `Curve`,
  `Easing`, `motion.rs::token_swaps_glide_and_popin_scales`): verify and
  cite them; `Easing` stays in `strand-scene` (S-runtime). Newly drawn
  props (`stroke`, `fill`, `trim`, `glow`, `blur`, gradients) join
  `ANIMATED` and spring. `reduced_motion` tests for keyframe loops,
  built-in effects, particles, grain and spectrum.
- velato 0.12 with `default-features = false`: its default pulls in vello
  0.10 and wgpu. Its kurbo 0.13 and peniko 0.6 match vello_common 0.3, so
  no spike is needed.

### S-gpu

- **Spike (wave 0, after the infra image)**: wgpu 30 on lavapipe
  presenting to a layer surface on a sway 1.9 started inside the container
  (never the host session). Sway runs `WLR_RENDERER=pixman`, so there is no
  linux-dmabuf and lavapipe must present over Mesa's wl_shm WSI path;
  fallback is render offscreen and read back into shm. Measure: wayland-backend
  `client_system` (needed for raw handles; it switches the backend for
  every crate, so keep it under strand-surface's GPU feature), the `.text`
  of naga, wgpu and vello_gpu, and whether lavapipe unmaps after the device
  drops. Results go to decisions.md and feed 0c.
- `strand-gpu`: a GPU thread, device lifecycle, display lists lowered to
  vello_gpu (masks lowered on the CPU), shader passes composited as
  external textures, full-damage presentation, a GPU crossfade.
- `gpu_handoff.rs`: one `wl_surface` handed between shm and the WSI.
- Promotion as a pure state machine: ≥0.2 Mpx damage for >500 ms, switch
  only when settled, device dropped 30 s after the last GPU frame with one
  timer wake. 0c decides whether a shader appearing on a CPU surface is
  drawn offscreen and read back (no switch) or waits for the springs.
- `check::shaders` (naga parse, validate, `u_*` reflection by name and
  type), replacing the interim `uniform_ty`; `u_*` props emitted as one
  `Prop::Uniforms`; `live.rs` handles `Role::Shader`.
- Canvas: the VM records a `DrawList`; render draws it with vello_cpu.
- Wave 3: the 8 bundled effects with S-effects' CPU fallbacks.

## Waves

| Wave | Building (three slots) | Content | Gate |
|---|---|---|---|
| 0 | S-infra image first; then F0 and S0; the spike takes infra's slot once the image lands | 0a docs (no build); lavapipe, PAM headers, lockvm image; render, surface and `run.rs` splits; GPU spike | `run.sh ci` green after the splits; spike recorded |
| 0b | S-runtime | all M4 scene types, `SurfaceHost`/`InputEvent` additions, schema entries | integrator merges; every stream rebases |
| 0c | none (docs) | GPU interface text from the spike: `Painter` backend, `raw_handles` and hand-off rules, GPU thread, pose state while GPU-owned, shader-on-CPU-surface rule, where naga lives, the `.text` gate number | integrator merges |
| 1 | S-runtime, S-lock, S-lists build; S-surface and S-effects write, then rotate in | F1–F4; strand-auth, PAM confdir tests, session lock, compiler lock semantics; windowing, scroll, nav, pages; caps, fake compositor, blur rungs, scrim, fillets; S-effects' wave-1 items | merge per finished item |
| 2 | S-effects, S-gpu, S-lock hold slots; S-lists and S-surface take one when a holder is writing or running the VM | effects needing F3/F4; GPU core, hand-off, naga check, canvas, promotion; lock fallback, wiring, faults, VM; drag and drop; poses, tray menus | lock VM run on the laptop (KVM approved); lavapipe tiers green in CI |
| 3 | S-gpu, S-effects, integrator | bundled GPU effects, jelly, `rice_now` acceptance, budget legs, hardware leg, ticks, m4-report, handoff | full `run.sh ci`, `matrix.sh`, `lockvm.sh`, `gpu.sh` (advisory) |

Critical paths: 0b → F3/F4 → S-effects CPU fallbacks → bundled GPU
effects; and spike → hand-off → promotion → the idle-drop exit test.

## Test realism

- **Sway 1.9 (wlroots 0.17) lacks most new protocols**:
  `ext_background_effect_v1`, ext-image-copy-capture, and very likely
  `wp_alpha_modifier_v1`. Pose, alpha, blur-rung and thumbnail tests run
  against the fake compositor in the container; real-compositor proof runs
  in `matrix.sh` (Arch sway, niri, Hyprland). `wp_single_pixel_buffer_v1`
  is probably there.
- **Feature legs.** Tests that need a non-default feature declare it
  (`[[test]] required-features = ["faults"]` on `lock.rs`); CI runs them
  and adds an `--all-features` clippy leg. With the GPU on by default,
  `gpu_idle.rs` and `gpu_cold.rs` run in the default build.
- **`gpu_cold.rs`** cannot keep walking `Cargo.lock`, which lists optional
  dependencies whatever is enabled; it uses `cargo metadata` (or `cargo
  tree -e normal`) per feature set.
- **GPU idle**: the strict checks are "GPU thread gone, no wakeups, PSS
  back within tolerance". Lavapipe and LLVM mappings may stay after
  `vkDestroyInstance`; that is a spike measurement, not a gate.
- **GPU pixels** differ from vello_cpu's antialiasing, so GPU-vs-CPU frames
  get their own documented tolerance, not `assert_matches_ref`'s.
- **"No frame shows a gap"** cannot be proved with grim, which samples
  frames. It needs a per-frame render-side check (frame stats or a watch
  hook); grim only checks the settled refs.
- **`list_scroll_bench`** uses the advisory machinery: a `GATE_MISS` gate
  checked at the end, listed in ci.sh's timing gates and `gate-misses.sh`.
  Its 3.5 ms per frame is this plan's number, not the design's; S-lists
  records it in decisions.md.
- Verified to work: PAM confdir tests (`pam_start_confdir`, Linux-PAM
  1.5.3), a virtual keyboard into a sway 1.9 session lock, `swaymsg
  create_output` for hotplug, spectrum against a PipeWire test tone,
  `vello_gpu` 0.3.0 and `wgpu` 30.0.1 in the registry.

## Readings streams record

Proposed readings of what design.md leaves open. The owning stream
records each in decisions.md when it builds it.

- **GPU**: the shader ABI (one `@fragment` entry from the file, the vertex
  stage from Strand; each `u_*` prop matches `var<uniform> u_name` by name
  and type; built-ins `time`, `size`, `scale`, `pointer`; lengths in px ×
  scale, angles in radians, durations in seconds, colours premultiplied
  linear `vec4`; a uniform with no prop is a warning and zero-filled, a
  prop the file lacks an error with a did-you-mean). Canvas follows a
  canvas-2D-like state model recorded on the logic thread, with `c.width`
  and `c.height` from layout facts. Visible means laid out on a mapped,
  frame-receiving surface, inside its clip, opacity > 0, not in a closed
  popup or a page that is not current, and `reduced_motion` off.
- **Lock**: only `auth` success unlocks, and the runtime then writes `open`
  false; a config write of false while locked is ignored with a warning. A
  lock is triggered by `strand set` on bound state; no `strand lock`, no
  logind listener. "Forked helper" is fork+exec of `strand-auth` over a
  socketpair (`restore_in_child` in pre_exec, handed to `Client::new` by
  its owner, closed fds, scrubbed env;
  one helper per lock session, respawned on crash; `pam_authenticate` then
  `pam_acct_mgmt`, no `setcred`). Config content on the focused output,
  single-pixel buffers elsewhere. Faults: logic ended or hung, the lock
  component frozen, no lock compiled, the text worker gone, auth
  unreachable, no first frame within 1 s (that deadline is a `GATE_MISS`
  check; "the fallback is shown" is strict). `finished` without `locked`
  is a diagnostic and the lock counts as not shown. Tokens and themes
  still animate while locked. Passwords are zeroized; the residual risk is
  recorded. The headless-sway tier does not replace the VM tier.
- **Surface**: pose delegation covers a root's `enter`/`exit` opacity,
  scale and x/y; scale only on an axis anchored on one side, verified per
  compositor; popup x/y always repaint. Fillets take the paint of the
  outermost box touching the attached edge. Hyprland: keep the tint unless
  ext-background-effect confirms blur, and print one hint naming `strand
  compositor-rules`. Submenus open to the side through `anchor:`, right by
  default, flipping left. Tray click coordinates are the anchor's
  output-logical position.
- **Lists**: smooth means wheel steps spring, flings decay, per-frame work
  under 3.5 ms at injected 60 and 144 Hz, rows laid out once during a
  scroll, no unmounted gap. Windowing covers only a `for` that is a
  `list`'s direct child. Pages: forward enters from the right, backward
  mirrors; `transition:` or a page's own pose overrides; `reduced_motion`
  snaps. Drag: 6 px threshold, targets from the type in `on drop`, `at` a
  global index, no accepting target springs back, drags out to other
  programs out of scope.
- **Effects**: `t` counts from when the node appeared, survives a reload
  that keeps the node, restarts on a remount; `noise(x)` is a time signal
  only when `x` contains `t`. Keyframes are offsets composed with springs
  and restart on a new `seq`. Morph across surfaces needs surface origins,
  else falls back to the enter pose. `tilt` is 2D on the CPU, 3D on the
  GPU. Before the GPU path, particles cap at 1,000 and aurora is static,
  with a notice. A graph repaints only its new column. Spectrum runs its
  FFT only while visible and stops when silent. Animated images keep at
  most 2 decoded frames per (source, size). The offscreen group cache is a
  second 4 MB budget.
- **Open boxes that stay open**: M1's loader and `find_files` box also needs wallpaper and
  `Discovery::dirs` watching; S-gpu closes only its `.wgsl` part.

## Exit criteria and their tests

### Smooth 2,000-row scrolling (S-lists)

- `strand-compiler/tests/instantiate.rs::a_2000_row_list_mounts_only_its_window`
  (replaces `a_2000_row_list_mounts_eagerly_and_updates_one_row`): only the
  window's rows in the scene; `set_list_window` mounts by key with no
  poses; an in-window change is one op, an out-of-window change none.
- `strand-render/tests/layout.rs::a_2000_row_list_lays_out_only_its_window`
  (ref `layout_list_window.png`).
- `strand-render/tests/motion.rs::wheel_steps_spring_the_offset`,
  `::a_touchpad_fling_decays_and_stops`, `::window_mounts_do_not_play_poses`.
- `strand-render/tests/input.rs::nav_selects_rows_beyond_the_mounted_window`.
- `strand-render/tests/list_scroll_bench.rs` (timing job, advisory locally).
- `strand/tests/demo.rs::the_design_launcher_scrolls_2000_apps` (headless
  sway, virtual-pointer wheel, 2,000 mock apps): the per-frame no-gap
  check, and grim refs once settled.

### GPU released when idle (S-gpu)

- `strand-gpu/tests/lifecycle.rs::the_device_is_created_on_first_visible_effect_and_dropped_after_idle`.
- `strand-render/src/promote.rs` unit tests: `::promotes_after_500ms_of_large_damage`,
  `::switches_only_when_settled`, `::drops_after_30s_with_one_wake`.
- `strand/tests/gpu_idle.rs::gpu_is_released_when_idle` (headless sway,
  lavapipe WSI, `STRAND_REQUIRE_GPU=1`): a shader shown promotes, hidden
  demotes; after the shortened idle window the GPU thread is gone, nothing
  wakes, PSS is back within tolerance, and frames match the CPU frame
  within the GPU tolerance.
- `strand/tests/gpu_cold.rs` (rewritten): GPU crates reached only through
  `strand-gpu`, and their code stays cold until promotion.
- `strand/tests/budgets.rs`: no libvulkan mapped before promotion;
  promoted-then-dropped returns memory (+20–40 MB while promoted); the
  release `.text` gate at the measured size; the CPU-only build at 15 MiB.
- Advisory: `gpu.sh` runs the same tests on renderD128.

### Lock fails closed under faults (S-lock)

- `strand-auth/tests/*` (tier A): private PAM confdir stacks (permit, deny,
  exec-hang with the timeout firing, exec-fail, missing service), framing
  round trips and garbage replies, zeroized buffers.
- `strand-render/tests/lock_fallback.rs`: the fallback field (empty, 4
  dots, failure tint) at 1× and 1.25×, with no text worker.
- `strand/tests/lock.rs` (tier B, headless sway, `faults`):
  `::locks_every_output_including_hotplug`, `::finished_is_reported_and_not_shown`,
  `::only_auth_unlocks`, and `::<fault>_keeps_the_session_locked_and_shows_the_fallback`
  for logic panic and hang, a lock runtime error, no lock compiled, text
  worker panic, helper missing, crashed, hung or answering garbage, and
  SIGTERM while locked. Each checks that no grim pixel shows the desktop,
  that the fallback is there, and that the right password typed through a
  virtual keyboard unlocks and a wrong one does not.
- `lockvm.sh` (tier C, the design's required tier): real `pam_unix` with
  right, wrong and empty passwords; `pam_faillock`; a missing
  `/etc/pam.d/strand` (falls back to `login`); `kill -9 strand` (the
  compositor stays locked and a restarted strand re-locks); the helper
  OOM-killed or deleted; hotplug; every tier B fault under real PAM. It
  runs on the laptop, and in CI's `lock-vm` job if KVM is there; the
  features.md tick cites the run.

Closing M4 (integrator, wave 3): tick each box with its test, write
`docs/m4-report.md`, update `handoff.md`, delete worktree `target/`
directories, push only `laptop/*` branches.
