# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, per-frame token evaluation, layout, damage, paint, presentation | Wait on logic, run handlers, evaluate bytecode |
| Logic (`strand-logic`) | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program; the IPC socket and the D-Bus request channel as loop sources; `file` outputs rendered after each tick's diff ("The theming hub") | Touch Wayland or pixels |
| Compiler worker (`strand-compile`) | `strand-compiler` | Parse, check, lower changed modules | Mutate live state (it hands logic a `Program`) |
| Text worker (`strand-text`) | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: a surface draws its last layout (or a stand-in) until the new one arrives |
| Image worker (`strand-image`, one per renderer; failing to start stops the shell) | `strand-render` (`image.rs`, `media/`) | Image and icon decodes, `svg`/`lottie` file jobs (`ImageStore::load_file`), the idle trim hook; each result pings render | Block render; read a file without a size bound |
| Render timer (`strand-tooltip`) | `strand-render` (`renderer/wake.rs`) | One wake at the latest due time sent (tooltip delay, a stalled surface's exit) | Any work: `Renderer::update` runs on main |
| Watcher (`strand-watch`) | `strand-watch` | inotify watches and polling (decisions.md wave2-watch) | Parse files (it sends paths and hashes) |
| Persist IO (`strand-persist`, one per `PersistStore`) | `strand-core` | Atomic writes of persisted cells, settings edits, overlays, last-good snapshots; announces each through `PersistStore::on_written` | Block logic (failures return as later diagnostics) |
| Services | `strand-services` | The `strand-services` tokio current-thread runtime: async service bodies, the compositor hub and its IPC adapters, the portal and icon-theme followers. `Start::Thread` services on their own threads asleep in `poll(2)` or a PipeWire loop (`strand-audio`, `strand-video`, `strand-clipboard`, `strand-idle`, `strand-outputs`); the process-wide `strand-clipboard-keep` keeper holding a copied value; the hub's `strand-toplevel` Wayland client (joined at the next start or stop and on `Services::shutdown`) | Block logic: patches and events go over channels to `Services::pump` |
| Lock auth (`strand-lock-auth`, first fallback submit to end of lock session) | `strand-auth`, the binary's `run/lock.rs` | The fallback lock's `strand_auth::Client`; each check one blocking PAM conversation within the client's deadlines | Run while no fallback shows; make main wait on PAM |
| GPU (`strand-gpu`, while a device can exist) | `strand-gpu` | wgpu device, vello_gpu, pipelines, promoted swapchains, offscreen passes, readbacks ("`strand-gpu`") | Touch the scene; make main wait while the shell runs (one wait at exit, at most `GPU_SHUTDOWN`, 3 s) |
| Theme quantiser (`strand-quantise`, per `Quantiser`) | `strand-theme` (`image.rs`), driven by `vm/theme.rs` | Wallpaper decodes, palette quantisation and cache; a `strand-quantise-grace` sleeper per torn wallpaper | Run on the logic tick (logic polls, keeps the last palette) |
| Token overlay writer (`strand-tokens`, until idle) | `strand-compiler` (`instantiate/overlay.rs`) | Atomic writes of `tokens.toml`, newest text wins; bounded wait at exit | Block logic (a failure is settings' `WriteFailed` notice) |
| File output writer (`strand-files`) | the binary's `run/outputs.rs` | Each `file` output's bytes (atomic, following symlinks, keeping the mode), its manifest, retries (1 s doubling to 10 s), `run:` (200 ms debounce, one queued, 10 s timeout killing the group, waited on `strand-files-run`); outcomes to logic via a shared list and a ping | Render templates, touch the scene, block logic; it has no channel to main |
| State writer (`strand-state-write`, per `FileWriter`) | `strand-theme` (`writer.rs`) | Batched atomic writes of the last palette and portal settings | Block logic (a failure is a WARN) |
| Introspection asks (`strand-introspect`, per uncached ask; `strand lsp` only) | `strand-introspect` (`Cache::properties_or_ask`) | One D-Bus introspection call, remembered for `TTL` | Block the LSP (it answers from memory or not at all) |
| Action commands (`strand-exec`, one per running command) | the binary's `run/exec.rs` | A declared action's command, or a command output's capture, with its timeout and kill grace | Block logic (results are taken after a tick) |
| Raster helpers (`strand-raster`, scoped to one CPU frame) | `strand-render` (`raster/`) | A large frame's cells, drawn in parallel and joined before the frame ends | Outlive the frame |
| Short-lived waiters | `overlay.rs` (`strand-editor`), services' `custom.rs` (`strand-kill`), `wm/screen.rs` (`strand-screenshot`) | Reap `$EDITOR`; SIGKILL a dropped `permit exec` group after its grace; one screen shot until its frame or `SHOT_WAIT` | Anything but that wait |
| Report writer (`strand-report`, per saved report) | the binary's `report/bundle.rs` | One glitch report's bundle and pruning ("Glitch reports"); outcome via a shared slot and a ping | Touch Wayland, the scene or the rings; block logic or main |

Two library handles have threads the shell does not start:
`strand_watch::PortalSettings` (`strand-portal`; the shell uses
`follow` on the services runtime) and `strand_services::audio::Audio::spawn`
(`strand-pipewire`). Tests and benches spawn unnamed threads.

**Channels are the only coupling between threads**; no lock is shared on
a hot path.

- **Logic → render**: one `SceneDiff` per tick, carrying besides its ops
  `layout_seen` (the last layout batch logic took in), `reduced_motion`,
  `press: Option<u32>` (`run::PressCause`: the serial of the press that
  caused it, or of the last action a handler ran with one, up to
  `GRAB_WINDOW` after; the host marks each popup the diff opens with it,
  `State::set_popup_cause`, so a surface grabs only for its own press)
  and `window_asks: Vec<(NodeId, WindowAsk)>` (applied with
  `State::window_ask(node, ask, press)` after the diff). A diff whose
  `layout_seen` or `reduced_motion` changed is sent even with no ops.
- **Render → logic**: routed input, layout facts
  (`Renderer::take_layout_facts`, changed boxes of `Prop::Watch` nodes,
  sent as `ToLogic::Layout { seq, sizes }`) and pointer facts
  (`Renderer::take_pointer_facts`, sent as `ToLogic::Pointer`, applied
  with `Instance::set_pointer`). The host feeds render the pointer after
  routing (`Renderer::pointer_input`) and hands facts over before the
  event's intents; every `LoopHost`, eval's too, does so through
  `facts()`, so a replay orders them as the shell does. While the left
  button is held the frame of reference is the press's, so a drag does
  not feed back. Render holds a frame whose layout resized a
  `watch: query` node until logic echoes the batch in `layout_seen`, or
  for `QUERY_WAIT` (`Renderer::set_query_wait`; zero offline), so
  container queries settle inside the frame.
- **Windows**: `SurfaceHost::window_state(surface, &ToplevelState)`
  reports each configure that changes the window's state; the host
  passes the sized axes to `Renderer::set_window_sized`, the bounds to
  `set_surface_bounds`, and changed states to logic as `NodeFlag`s
  (`Focused`, `Maximized`, `Fullscreen`, `Tiled`, `Decorated`) on the
  root, so an interactive resize wakes no logic.
  `SurfaceHost::window_closed` precedes the compositor-close
  `ClickAway`.
- **Cursor**: `SurfaceHost::cursor(surface) -> CursorShape` is pulled
  (answered by `Renderer::cursor`) after pointer frames and paints, and
  set through `ThemedPointer::set_cursor` only when it changes.

`strand run [dir]` (`crates/strand/src/run/`) is this wiring. Input is
routed on main by `strand_render::input::Router` (`handle(&InputEvent,
&mut dyn InputScene) -> Vec<Intent>`; the `Renderer` is the
`InputScene`): hover and press chains, focus, selection, `nav`, `input`
edits, and closing a two-way `open` on Escape, focus loss and
click-away. A `window` closes only on click-away, its compositor's close
or `WindowAsk::Close`, and the router never writes its root's
`Focused`. An `Intent` is `Flag`, `Event`, `Write` or
`Clipboard(ClipboardAsk)`; the binary's host (`demo/host.rs`, `Forward`)
maps them to `ToLogic` and keeps clipboard asks for
`SurfaceHost::take_clipboard_asks`. The host passes each diff to
`Router::observe` before applying it and calls `Router::settle` after.
`NodeEvent` has a variant per input kind (`Click`, `Secondary`,
`Middle`, `Scroll`, `Activate`, `Key`, `Dismiss`, `Drop`); logic
bubbles it to the nearest handler.

**Loading.** `live::Worker::spawn` starts the watcher, boots the
`Loader` (a build, a cached last good one, or none) and starts
`strand-compile`, which sends `live::FromWorker::{Loaded, Settings,
Theme}`; logic sends it `Job::{Reload, Referenced}` (the settings,
wallpapers and imports the program reads). A load that only clears
problems (`Outcome::cleared`) is sent; a repeat on the same files
(`Outcome::repeated`) is not. `on_written` registers Strand's own
writes. While a lock is shown the newest load waits for the unlock.

**The logic thread** owns the runtime, `SchemaHost::real` and the
`Instance`; keeps the error overlay (`overlay.rs`, 250 ms quiet);
freezes faulting components; serves the IPC socket (`ipc/`); loops on
`Instance::step`; and sleeps in its own calloop loop until a message,
the runtime's wake ping, `Wake::deadline`, or `Wake::wall` on a
`CLOCK_REALTIME` timerfd (`TFD_TIMER_CANCEL_ON_SET`). Without
`STRAND_MOCK` it runs the real services (`Real::start` registers every
builtin behind a composite host whose fallback is the `SchemaHost`),
calls `Services::pump` after every sleep, before the step, holds a
reader of `system` (render needs `system.reduced_motion`), seeds
`system` with the values it last kept in the state directory (written
back on a `strand-state-write` thread), and the first frame waits up to
100 ms (`Services::wait_ready`).
`STRAND_MOCK=desktop|acceptance` runs a mock desktop (`mock.rs`; the
latter with a frozen clock) and accepts the IPC `mock` command.

**Memory.** After a structural diff, logic and main each force one
allocator collect 500 ms after their last wake, at most 5 s after it
(`run::trim::Trimmer`, `mi_collect(true)`); otherwise each trims inline
at most once per 5 s, never while `Renderer::in_motion()` (decisions.md
wave4-exitMemory). The text and image workers run the same hook as their
queues drain (`set_idle_hook`, ruled by `strand_text::HookGate`, so an
idle worker never wakes). The services runtime ends an idle
blocking-pool thread 500 ms after its last task
(`client::BLOCKING_KEEP_ALIVE`). The memory budget rests on the root
`Cargo.toml`'s `[profile.release.package]` opt-levels (decisions.md
m5-speed-profile), which packagers' profiles do not carry, so packages
build from the workspace. `budgets.rs` holds `.text` to 28,311,552 B
(24,379,392 B without the GPU backend; re-bases are dated decisions.md
entries). Every program strand starts gets back the inherited THP
setting (`strand_services::child`).

**Shutdown.** SIGINT, SIGTERM (a `signalfd`; the signals are blocked in
every thread) and the compositor going away send `ToLogic::Shutdown`;
main joins logic, which unmounts, runs `Runtime::shutdown` and drops its
stores, so debounced writes reach the disk.

`crates/strand/src/run/`: `mod.rs` (messages, the main thread, `run`),
`logic.rs` (`Live`, logic's loop), `sleep.rs`, `shell.rs` (`Shell`:
commits, IPC requests, watch events), `request/` (one module per IPC
command), `lock.rs`, `feeds.rs`, `lists.rs`, `gpu.rs` (`GpuHost`),
`outputs.rs`, `exec.rs`, `record.rs`, `trim.rs`, `tests.rs`.

**IPC** (`crates/strand/src/ipc/`, types in `strand_core::ipc`): a Unix
socket (`socket_path()`), newline-delimited JSON. A request `{"v": 1,
"cmd": …}` gets one line, `{"ok": true, …}` or `{"ok": false, "error":
…}`; an unknown `cmd` or newer `v` is refused without closing the
connection. Commands: `set`, `reload` (answered once committed or held;
`"deferred": true` while locked), `reset`, `watch` (then one `reload`,
`notices` or `fault` event per line) and those in "M5 interfaces". A
new watcher first gets a `notices` event with the running config's
check warnings, when it has any. A reload event's `total_ms` runs from
the watcher's last event behind the save to the send of the diff
holding the reload. A client that cannot take its output gets a write
source on the logic loop; one more than 1 MiB behind is dropped.

**M4 additions.** The GPU thread, promotion and hand-off are in
"`strand-gpu`".
- `ToLogic::ListWindow { list, first, count }` (from
  `Renderer::take_list_windows`, applied with
  `Instance::set_list_window`); `ToLogic::LockState(LockState)` feeds
  `lock_shown`.
- **The lock in the binary.** While a lock is shown, main outlives logic:
  logic ending or hanging (`ToLogic::Beat(seq)` 1 s after the last
  answer, 3 s to answer), SIGTERM, a runtime fault inside the lock, the
  text worker stopping, `auth` failing, `finished` after `locked`, no
  `lock` open, or no first frame within 1 s leave the session locked and
  show `strand_render::lock_fallback`. Its checks run on
  `strand-lock-auth` (`Checker`) and answer as `LockMsg::Checked`; each
  carries its lock session (`LockScreen::session`,
  `AuthConfig::session`), and a verdict for an ended session is dropped.
  `lock::Guard::wire` connects `AuthConfig::sink` and `failed` before
  calling `State::enable_session_lock`, and tells `auth` through
  `auth::LockSignal` whether a lock is shown and when input wakes it;
  `lock::LockScreen` forwards `SurfaceHost::lock_changed` as
  `ToLogic::LockState`. SIGINT and SIGTERM end `strand
  run` only while no lock is shown. Faults are injectable under the
  `faults` feature (`STRAND_FAULT`) for `tests/lock.rs` (lock VM only).
- A marker, `$XDG_RUNTIME_DIR/strand-<display>.locked`, exists from a
  lock being asked for until none is asked for or held; a strand started
  while it exists locks at once with the fallback.
- **Redaction.** While a `type: password` input is mounted, every
  runtime fault that can carry values is logged and streamed as
  `<redacted>`; otherwise known values are replaced, and a message is
  redacted whole when any 4-byte run of one remains (lower-cased,
  escapes undone). Known values are the inputs' current ones and the
  newest four replaced (`lock::Secrets`, zeroized); values under 4 bytes
  are never matched. Residual risk: up to four recent passwords outlive
  the lock in memory and in a core dump (`LimitCORE=0` avoids the
  latter).
- **Supervisor.** If the process dies while locked, the compositor keeps
  the session locked with no password field until strand starts again;
  strand ships no supervisor, so a session using the lock must run one.
  `tests/lock.rs::a_supervised_strand_dying_while_locked_keeps_the_session_locked`
  runs the unit below, read from this file. A user unit for it:

  ```ini
  [Unit]
  Description=Strand shell
  PartOf=graphical-session.target
  After=graphical-session.target
  StartLimitIntervalSec=0

  [Service]
  ExecStart=%h/.cargo/bin/strand run
  Restart=on-failure
  RestartSec=100ms
  RestartSteps=5
  RestartMaxDelaySec=2s

  [Install]
  WantedBy=graphical-session.target
  ```

  `ExecStart` needs an absolute path (systemd does not search `$PATH`).
  `StartLimitIntervalSec=0` keeps a crash loop from ending the lock's
  way out (`tests/lock.rs::the_supervisor_gives_up_as_systemd_does_unless_the_unit_lifts_the_limit`);
  the delay growing from 100 ms to 2 s is the brake instead.
- The PAM helper is a process: the `strand-auth` binary, fork+exec'd
  over a socketpair by `strand_auth::Client`, one per lock session,
  respawned when it dies, with `child::restore_in_child` as `pre_exec`.
  Only it links libpam.
- **Media feeds.** Spectrum bins and thumbnail frames are produced off
  main and reach render through the binary (`Renderer::feed`,
  `feed_frame`); render says which fed nodes a painted surface drew
  (`take_feed_demand() -> Option<Vec<FeedDemand>>`, none under
  `reduced_motion`) and producers run only for those (`run/feeds.rs`,
  one `audio::tap_levels` per visible spectrum).

## Crate graph

```
strand-scene      shared vocabulary: ids, geometry, colour, scene protocol, Painter
  ^   ^   ^
  |   |   strand-surface   (layer-shell, xdg toplevels, shm, damage submit, input, frame timing)
  |   strand-render ── strand-text
  |   |   └── strand-icons (the icon theme lookup; no dependencies)
  |   strand-theme     (palette schema, material(), importers; colour maths in strand-scene)
  |     ^
  strand-core ── strand-compiler ── strand-dev (LSP, inspector; links
     ^                                strand-services-schema and strand-introspect,
     |                                not the runtime)
     strand-services ──> strand-watch (EventSink, CompositorEvent; portal follow;
          |                            strand-watch depends on no Strand crate)
          ├── strand-services-macros (#[service], #[derive(Store, Data, Call)])
          ├── strand-services-schema (the builtin services' schema texts)
          ├── strand-icons (apps' icons, the renderer's lookup)
          └── strand-introspect (D-Bus introspection; zbus, tokio and log only)
strand (binary) wires everything; its ServiceHost adapters join
strand-services' stores to strand-compiler's VM. strand-core's `ipc`
(serde_json) is shared by strand, strand-services and strand-dev;
strand has a [lib] target for plugin binaries.

strand-auth  lib: wire protocol, Client, UnlockToken (libc, zeroize)
             bin: the PAM helper, the only code that links libpam
  ^-- strand-services (`auth`), strand-surface (the unlock gate),
      strand (`strand-lock-auth`); `Client` owners pass
      `child::restore_in_child` as `pre_exec`

strand-fake-wayland  (tests only; publish = false) a fake compositor;
                     no Strand crate
  ^-- dev-dependency of strand-services and strand-surface; eval/

eval/ (strand-eval)  the animation evaluation sandbox, its own workspace
  ^-- nothing under crates/

strand-site  the docs site generator; a developer tool, never shipped
  ^-- nothing

strand-web   the render path in a browser (cdylib + rlib), never on
             strand-surface; no `gpu`, `shaders` or `eval-hooks`
  ^-- nothing
```

`strand-scene` has no heavy dependencies: render and surface build and
test without the language, and the language without pixels.
`strand_scene::placement` is the pure half of the compositor side
(`LayerConfig`, `WindowConfig`, `PopupConfig`, `popup_shift`; no
Wayland), re-exported as `strand_surface::placement`.

**wasm** (docs/live-examples.md). The render path's monotonic clock is
`strand_scene::time::Instant` (web-time's on wasm, std's natively), so
every signature taking an `Instant` follows; the wall clock is
`strand_scene::time::system_now()`. `scripts/wasm/lint.sh` (CI's `wasm`
job) fails any call to std's `Instant::now()` or `SystemTime::now()` in
a crate strand-web builds. A host without a file system supplies icon
files through `strand_icons::set_files` (also read by render's
`image::read_capped`).

**`strand-site`** (`site/` to `target/site`) depends on strand-dev,
strand-compiler, strand-scene, strand-theme and tree-sitter-strand. It
renders `site/*.md`, `docs/guide/*.md` and the generated reference
(`strand_dev::reference::pages`) to relative-link HTML plus a search
index and llms.txt markdown copies (`llms.rs`), and fails, writing
nothing, on a dead link, an example that does not check clean or is not
formatted, a text colour under 4.5:1, a `site/claims.toml` claim its
page or source does not bear out, a stale CLI capture, or example media
not tied to their scenario, dark twin and approved recording. Figures
are eval scenarios (`eval/scenarios/docs/`) rendered by
`scripts/site/media.sh`; the generator never runs the renderer. CI's
`deploy` job uploads the site to Railway (deploy/railway/README.md).
The site also serves the installer, `scripts/install.sh`, at
`/install.sh` (`strand_site::INSTALLER`; docs/development.md,
"Releasing").

**`strand-fake-wayland`** is a test fixture: no root crate depends on it
outside `[dev-dependencies]`. It serves what strand-services' wm,
capture and clipboard clients and strand-surface speak, records each
commit (`Fake::commits`) and can present only at refreshes the test
sends (`SurfaceGlobals::vblank`); `src/lib.rs` lists the globals.

**`eval/`** (`strand-eval`; docs/eval.md) runs scenarios on a virtual
clock against `Renderer` and the logic VM, through
`strand_render::host_loop` as the shell's main thread does. As its own
Cargo workspace, root `--workspace` builds and feature unification
never see it; it pins the root's lock versions
(`scripts/eval/check-lock.sh`), profiles and lints (`eval/tests/drift.rs`).
`scripts/eval/check-isolation.sh` checks that no `crates/*/Cargo.toml`
names `strand-eval`, `eval/` or `eval-hooks` (strand-render's
`[features]` aside), that root resolution never enables `eval-hooks`,
that every `cfg(feature = "eval-hooks")` is in strand-render, and that
strand-fake-wayland is only a dev-dependency. Its surface leg drives
`strand_surface::SurfaceManager` through its public interface only;
strand-surface has no eval-only code.

**`strand-auth`** is the lock's whole security boundary: its lib depends
on `libc` and `zeroize` only, its binary adds a hand-written PAM FFI.
The lib holds the framed protocol (`protocol`, version 2), a blocking
`Client` (`new(helper, pre_exec: fn())`, `converse`, `submit`,
`submit_saying`), `answers() -> (Answers, Inbox)`, zeroizing
`Password`, `Verdict { Unlocked(UnlockToken), Denied, Failed }`.
`UnlockToken` is minted only by `Client` from a success reply, `Send`,
neither `Clone` nor constructible elsewhere; strand-surface releases a
session lock only for one. No deadline runs while PAM holds no
password (it may wait on a reader); once it has one it must send a frame
within the timeout (30 s), and a password must be answered within
`LIMIT_FACTOR` (3) timeouts; a missed deadline or bad frame kills the
helper and its children. The PAM service is `strand` when libpam will
read its file, else `login` with a one-time warning (`choose_service`);
every other PAM error fails closed. The `faults` feature (never in
default or release builds) adds `STRAND_FAULT` and a private PAM
confdir for tests.

**`strand-gpu`** holds every GPU crate (wgpu, vello_gpu, naga through
wgpu); its only Strand dependency is `strand-scene`. `strand-render`
depends on it under `gpu` and lowers its display lists into
`strand-gpu`'s frame type, so the GPU crate never sees render's types.
`strand-surface` hands out raw Wayland handles under `gpu` and never
depends on it; the binary owns the thread and joins the three
(`run/gpu.rs`). `strand-compiler`'s `shaders` feature uses naga at
wgpu's version (a second version in `Cargo.lock` is a merge blocker).
The binary's default `gpu` feature enables render's and surface's `gpu`
and the compiler's `shaders` (which `strand-dev` also turns on, so the
LSP checks shaders); with `--no-default-features` it links
none of wgpu, vello_gpu, naga or libwayland-client, which the CPU-only
`.text` gate measures (24,379,392 B).

```
strand-scene <── strand-gpu (wgpu, vello_gpu, vello_common, raw-window-handle)
                   ^
                   strand-render [gpu]      strand-surface [gpu]: raw handles,
                   strand (binary) [gpu]       wayland-backend client_system
strand-compiler [shaders] ──> naga (check-time; same version as wgpu's)
```

**`strand-icons`** is the Icon Theme Specification lookup (`lookup`,
`resolve`, `exists`, `invalidate`, `set_desktop_theme`), used by the
renderer and the `apps` service. It has no dependencies, so `strand
run` names the config's `icons/` folder (`set_config_icons`, tried
before the theme) and watches it (`live::config_icons_source`);
`strand_services::icon_theme::spawn` follows the portal's icon theme.

**`strand-introspect`** reads properties and methods from D-Bus
introspection (`properties_on`, `members_on`; blocking `properties`
and `methods` with a 2 s bound over one kept connection per bus;
`Cache<T>` remembering answers for `TTL`, 10 s). `from dbus` services
are checked against it (`strand check`, the loader, the LSP, whose
`check::dbus::Introspect` answers `None` while a question is out) and
the running service binds its actions' methods with it.

### `strand-render` module map

Large files are directories of modules so parallel work owns disjoint
files; each `mod.rs` re-exports what the crate uses.

- `renderer/`: the `Renderer` (`mod.rs`), frames and damage
  (`frame.rs`), the timer (`wake.rs`), text slots (`text.rs`), layout,
  size springs and flattening (`layout_pass.rs`), facts (`facts.rs`,
  `pointer.rs`), diffs (`apply.rs`), surface specs (`specs.rs`;
  `resize_share` is public for `crates/strand/tests/resize_parity.rs`),
  `surfaces.rs`, `pose.rs`, `lists.rs`, `tooltip.rs`, `swap.rs`,
  `summary.rs`, `feed.rs`, `assets.rs` (local files a config draws,
  read again when saved), `backend.rs` (the GPU backend: promotion,
  lowering, `shader` passes), `probe.rs`.
- `media/` (graph, spectrum, animated images, `svg`, `lottie`,
  thumbnails), `promote.rs`, `canvas.rs`, `flatten/` (display lists),
  `raster/`, `anim/` (springs, poses, keyframes, stagger, `morph:`
  across surfaces via `Renderer::set_surface_origin`, which the
  binary's host calls from `SurfaceHost::surface_placed`), `effects/`,
  `backdrop.rs`, `shapes/`, `layout/`, `inspect/`, and top-level files,
  among them `cache.rs`, `layers.rs`, `offscreen.rs`, `clock.rs`,
  `time.rs`.
- `lock_fallback.rs`: the built-in fallback lock, drawn with no text
  worker (PAM's text in its own bitmap font; `LockFallback::set_message`).
- `host_loop.rs` (public as `strand_render::host_loop`): the loop's
  reactions and the `LoopHost` and `LoopSync` traits, run by the shell's
  main thread and `eval/`'s driver ("Render loop").
- The `eval-hooks` feature, enabled only by `eval/`, adds the probe and
  changes nothing a frame draws (but for a test-only damage mutant):
  `strand_render::probe` (`FrameTrace` per `Painter::paint`,
  `NodeTrace` per drawn or hidden node; fields in `probe.rs`, built in
  `flatten/probe.rs` without sampling motion), the trace and text-hold
  switches (`set_frame_trace`, `take_frame_traces`,
  `set_text_hold_with`, `held_text`, `release_text`), the `redraw`
  check's read-only repaints (`set_keep_frames`, `repaint_fresh`,
  `fresh_gpu_frame`, `repaint_unblended`) and `set_damage_inset`. Only
  `eval/` consumes them.
- Virtual clocks, on every build, for hosts on their own clock (eval,
  strand-web; never the shell): `Renderer::set_now(Option<Instant>)`,
  `Renderer::take_inline_shaped()` and `Router::set_now`. They change a
  frame only through the time they set (decisions.md live-examples).

## M5 interfaces (wave 0a)

The interfaces of IPC, the inspector, the handler input context and
plugins (decisions.md m5-owner, m5-core, m5-ipc, m5-inspect,
m5-platform, m5-plugins).

**IPC protocol in `strand-core`** (`strand_core::ipc`): request, answer
and event types and their JSON-line codec; no I/O, no threads. The
binary (server and CLI), strand-services (D-Bus) and strand-dev
(`strand-dev inspect`, through a direct `strand-core` dependency) all
name them.
- `socket_path()` (`$STRAND_SOCKET`, else
  `$XDG_RUNTIME_DIR/strand-<display>.sock`) and
  `strand_core::persist::state_dir()` (`$STRAND_STATE_DIR`, else
  `$XDG_STATE_HOME/strand`, else `~/.local/state/strand`) are the one
  rule for finding a shell and its state. `child::child_env` removes
  them, and greetd's `GREETD_SOCK` (`child::INSTANCE_VARS`), from every
  program the shell starts.
- Version 1 also has `get{path}`, `toggle{path}`, `call{target, args}`,
  `clear{path}`, `report{save?}` ("Glitch reports"), `export{kind,
  template?, name?}` ("The theming hub"), `inspect.pick{on}`,
  `inspect.node{id}`, `inspect.dump{surface?}`,
  `inspect.highlight{id?}`, `inspect.flash{on}` and `reset{node}`
  (`Instance::reset_subtree`). Node ids are node refs; `call`'s `args`
  are strings read by each parameter's type.
- `ipc::parse` reads `set`, `reload`, `reset`, `watch` and `mock` and
  hands the rest to `ipc::parse_m5`; a `reset` with only a `node`
  resets the subtree, one with both resets the path, one with neither
  hears "`reset` needs a `path`". A command a transport does not
  serve is answered as unknown (`not_served`).
- `get` reads what `set` writes (exported values, settings fields, token
  overlays). `call` reaches service-level actions only. `toggle` is bool
  only. Token paths are spelled `$accent`.
- `MAX_LINE` (64 KiB) bounds requests; an answer is bounded by the
  connection's 1 MiB queue.
- **The D-Bus name**: `DBUS_NAME` (`io.github.jakeb_grant.Strand`), bus
  and interface name, object path `/`; its only spelling (a
  strand-services test holds any zbus literal equal to it). It may be
  renamed before 1.0.

**Request channel from the services thread to logic** (`IpcRequests`).
- `ipc::channel(wake) -> (IpcRequests, IpcReceiver)`:
  `IpcRequests::call(request) -> Result<ReplyFuture, IpcClosed>` queues
  an `IpcCall` and calls the binary's wake (logic's sleeper ping), so
  strand-core needs neither calloop nor tokio. Dropping the receiver
  refuses queued and later calls; a `Reply` dropped unanswered completes
  with a refusal, so a caller never hangs.
- Logic answers each through `Shell::request`, as the socket's
  (`Caller::Bus(reply)`), never waiting on the services thread. A bus
  `reload` waits for its load under a client id with the top bit set
  (`BUS_RELOAD`); `watch` is answered as unknown.
- **Events the other way**: logic hands every watcher event to each
  `strand_core::ipc::EventSink` (`Live::events_to`, the front end's),
  whether or not anyone watches; `Shell::broadcast` is the one fan-out.
  A sink runs on logic and must not block.
- **The D-Bus front end**: `strand_services::ipc_bus::spawn(&Services,
  IpcRequests) -> (FrontEnd, EventSink)`, a task on the services runtime
  with its own session connection, serving until the `FrontEnd` drops;
  logic starts it with the real services. It serves the object before
  asking for the name (`DoNotQueue`); an instance that cannot own the
  name runs without it. Methods return the socket's answer as JSON text
  (refusals as `org.freedesktop.DBus.Error.Failed`): `Get(s)`, `Set(s,
  s)`, `Toggle(s)`, `Clear(s)`, `Reset(s)`, `Call(s, as)`, `Reload(b)`,
  each `→ s`; the signal is `Changed(s event, s data)`. Inspector
  commands, `report` and `export` are socket-only.

**Snapshot hop and the inspector's messages between logic and main**
(`run/mod.rs`, over `Live::to_main`):
- `ToMain::Snapshot { kind, reply: SnapshotId }` → `ToLogic::Snapshot(
  Snapshot { reply, data, bundle })`, for `SnapshotKind::Report`
  (`report::main_part`), `Layout { node }` (`Renderer::node_layout` on
  each of `surfaces_of(node)`) and `Bundle` ("Glitch reports"). Main
  answers between frames and never waits on logic.
- `ToMain::Highlight(Option<NodeId>)`, `ToMain::DamageFlash(bool)`.
- `ToMain::PickMode { on, seq }`; main sends
  `ToLogic::Picked(PickEvent::Picked { surface, node, seq })` on a click
  and `PickEvent::Ended { seq }` when it ends pick mode (an unmap, a
  lock). Logic ignores either when `seq` is not its last.
- (screens-focus) `ToMain::FocusedScreen(Option<String>)`: the
  `Screen.id` of the monitor the compositor names as focused, sent when
  it changes; main calls `State::set_focused_monitor` with that monitor
  (plugged in or remembered), `None` when none is named. Only surfaces
  with `SurfaceSpec::follows_focus` (and the lock's focused content) go
  there.
- Logic names nodes, never surfaces: a `SurfaceId` is main's and a root
  may show on several, so no logic→main message carries one.

**`strand-compiler` `Instance` inspection** (read-only, never read by
evaluation; `instantiate/inspect.rs`): `identity(node)`,
`cells_of(node)` (with the kept badges), `reset_subtree(node) ->
Result<usize, Error>` (settings untouched), `source_hash(file)`,
`inspect_node(node)` and `dump(root)` (`{"nodes", "files": {name:
blake3}}` over every compiled file). `inspect::node_ref` and
`parse_node_ref` spell a node as `index.generation` (`12.0`). A dump
redacts password inputs by the `Secrets` rule.

**The token overlay layer** (`strand-compiler`).
`Instance::set_token_overlay(path, value)` and `clear_token_overlay`
mirror settings' `set_overlay`/`clear_overlay`: global, persisted in
`tokens.toml` (by `strand-tokens`), taking the token swap path
(design.md, "Reload"). Clearing an unknown name is refused with the
closest name; a stale entry is cleared (`cleared: true`). An unreadable
`tokens.toml` is never written over; a hand edit is read again before
the next write. An overlaid path's origin is
`strand_scene::tokens::OVERLAY_ORIGIN` (`overlay`). A save the overlay
shadows raises `accent: file changed but runtime overlay wins [clear]`.

**`strand-scene`: `TokenTable::provenance(path, scope) ->
Vec<ProvenanceStep>`** walks `TokenExpr::Ref` through the `TokenScope`
chain (the global table, then `set { }` levels outermost first), adding
overlay and contrast-guard steps.

**`strand-render`** (`src/inspect/`, `renderer/summary.rs`):
`Renderer::pick(surface, point)` (topmost node of the last frame; never
on a lock surface), `node_layout(surface, node) -> LayoutInfo`,
`surfaces_of(node)`, `set_highlight` (a 2 px ring on every surface
showing the node) and `set_damage_flash` (each frame's damage, fading
over 300 ms; off costs nothing), drawn as display items after
flattening so both backends draw them; they are the only inspector UI
the shell draws. `Router::set_picking(on, scene)` withholds pointer
events and drags from handlers; `Renderer::pick_input` outlines and
picks, `pick_ended()` drops the outline. Pick mode ends with its
client, the unmap of its surface and a lock; `inspect.pick` and a
node's `inspect.highlight` are refused while locked.
`frame_summary(surface)`: nearest-rank p50/p95/p99 over frames drawn
among the last 600 paints, with `drawn` and `unchanged` counts; a GPU
frame runs to its `Presented` (minus `waited`) plus its GPU run from a
later `Presented`'s `ran`, or to `Pixels`; vsync is not timed. Beside
it, `Renderer::gpu_lost_devices()` and `State::globals()`.

As served: `inspect.pick`, `.highlight` and `.flash` belong to the
client that asked and end with it; one client picks at a time (the
displaced one hears `{"event": "pick_ended"}`), and a pick arrives as
`{"event": "picked", "surface": n, "node": "12.0" | null}`. An answer
to `inspect.pick` means logic has told main, not that main has turned
it on. An outline asked before a lock is hidden while it lasts and
drawn again after the unlock. `inspect.node` adds `layout` (through
the snapshot hop) and, for a
`shader` drawing nothing, `why` (from `ToLogic::GpuStatus`).

**Handler input context** (`strand-compiler`, cross-crate; decisions.md
m5-platform).
- The VM records `InputCtx { surface, serial }` of the event that
  started a handler, for its actions until its first `await`.
  strand-surface tracks press serials (`State::note_press`) and reports
  each through `SurfaceHost::press_serial` before the press; the host
  joins it to the node events a press causes (button press or release,
  key press or repeat, click-away) as `ToLogic::Event`'s `input`.
- `Instance::event_with(node, event, args, input)` sits beside `event`.
  Invocations are numbered from one process-wide counter from 1
  (`ActionCtx::invocation`), kept for the whole run; invocation 0
  (`ActionCtx::default()`) is unordered. The host reads the running
  context with `strand_compiler::vm::action_ctx()`.
- `ActionCtx { input, invocation }` and `InputCtx { surface: u32, serial:
  u32 }` live in `strand_core::action` (re-exported by
  `strand_compiler::vm::host`), so strand-services needs neither the
  compiler nor strand-scene. `ServiceHost::action` and
  `DynService::action` take it last. `Service::ACTIVATION`
  (`#[service(activation = …)]`) names actions that want a token;
  `Cx::activation_token()` reads it.
- The ordering queue (`strand_services::activation`, one for all
  services) asks main for a token through the `TokenSource` given to
  `Services::set_token_source`; the binary queues a `run::TokenAsk` with
  a ping, and main calls `State::activation_token(surface, serial,
  done)` (a callback; `done(None)` at once without
  `xdg_activation_v1`, after 5 s if unanswered).
- **Launches.** A launched program gets the press's token in
  `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID`
  (`apps::spawn_launched`), never the one strand inherited:
  `child::child_env` removes both before every `fork`.
- **Ordering rule.** An action needing a token waits up to 200 ms
  (`TOKEN_WAIT`, a deadline) then goes without one; every later action
  of the same invocation, any service, across `await`s, queues behind
  it. Logic never blocks, and no action is dropped or reordered. An
  action whose service run ended while it waited goes back to logic for
  the run up then, and while that hand-back queue holds anything every
  action joins it at the back.

**`strand-compiler` `Suggestion`** has `title: Option<String>` (`None`:
"Change to `…`"); a zero-width span is an insertion.

**`strand-dev`.**
- `strand-dev inspect` (`strand_dev::inspect::{client, pane, write}`): a
  line-mode terminal client over the socket (`p` pick, `r` reset
  subtree, `e` `$EDITOR`, `set`, `token`, `f` flash, `q`). Write-back
  changes one literal (a prop at the node's own element, or a token in
  the block its provenance names; palettes, the built-in theme and
  overlays refused) only if the file still hashes to `source_hash`. It
  swaps a temporary file in with `renameat2(RENAME_EXCHANGE)` and undoes
  the swap unless the file swapped out is the one hashed, keeping an
  editor's save made meanwhile; `ENOENT` refuses as `Changed`; without
  exchange support it checks just before a plain rename.
  `write::write_with(edit, Swap, hook)` is the test hook.
- `strand-dev reference [dir]` writes `docs/reference/` from the builtin
  and service schemas (never plugin files), touching only pages with
  the generated note; `reference::tests::the_reference_is_current` fails
  on any difference.
- **tree-sitter layout.** `crates/strand-dev/tree-sitter-strand/` is a
  separate package (`publish = false`, listed in `members`):
  `grammar.js`, the committed generated `src/` (`grammar.json`,
  `parser.c`, `node-types.json`, `tree_sitter/`), the external scanner
  `src/scanner.c` that places SEP (decisions.md m5-lsp),
  `queries/*.scm`, `tree-sitter.json`, and Rust bindings exposing
  `LANGUAGE` and the queries. It is strand-dev's dev-dependency only, so
  no runtime binary links it (`strand-site` does, for highlighting).
  `scripts/check-tree-sitter.sh` (CI's lint job and the container, with
  a pinned tree-sitter CLI 0.27.0) regenerates `src/` from `grammar.js`
  (`--js-runtime native`) and fails on any difference.

**Plugins: `strand` has a `[lib]` target.** The module tree lives under
`lib.rs`; `main.rs` is `strand::process_setup!(); fn main() -> ExitCode
{ strand::Shell::builder().run() }`, and the command line is
`src/entry.rs`, reached only through `Builder::run`. A plugin is a
user binary linking `strand` that registers its own `#[service]` types
(git-only, unstable; no dylib is loaded) and writes their schemas to
`$XDG_DATA_HOME/strand/schemas/` (`--write-schemas`), where `strand
check`, `strand run` and the LSP read them. It must copy the
workspace's release profile and invoke `strand::process_setup!()` once
at its crate root (mimalloc, THP off before the first allocation), or
the size and PSS figures do not hold; a library cannot do it alone
(decisions.md m5-w0-harden). The binary reaches the library through the
hidden `strand::cli` module.

**Plugin builder** (decisions.md m5-plugins).
- **Public surface, all of it.** `strand::Shell`, `Shell::builder() ->
  Builder`, `Builder::service::<S: strand_services::Service>(self) ->
  Self`, `Builder::run(self) -> ExitCode`, `process_setup!`, and the
  hidden `cli` and `__process`. Errors are printed, not public types. A
  plugin takes `strand` and `strand-services` from the same git rev.
- **One CLI.** `Builder::run` is the whole command line, so a plugin
  answers every `strand` command against its own schema.
  `--write-schemas` is one call in `entry::main`, not a COMMANDS row;
  the builder is `src/plugin.rs`.
- **Registration.** `service::<S>()` appends a private `Registration`;
  `run` checks the list and stores it in a process-wide `OnceLock`
  (`plugin::extras()`, empty in `strand`) via `plugin::install`.
  `services::schema_with(extras, files) -> Served { schema, unserved,
  warnings }` builds the served schema (`services::served()` is its
  process-wide `OnceLock`, `schema()` is `&served().schema`);
  `Real::start_with(…, extras, served)` adds the extras as `StoreHost`s
  after the builtins; `set_text_with` is `set_text`'s twin. Tests use
  the twins, never the process-wide lists. `Real::served(name)` is the
  one lookup `strand set` and `strand call` use to start a service and
  wait for its first read.
- **The served schema**: builtin texts, linked plugin texts in
  `service` order, then plugin files by name, skipping a file whose stem
  names a linked service. `Schema::extend` is atomic; a file that does
  not apply is skipped with a warning. The fingerprint chains every
  applied text. A service declared only by a file is served by an
  `UnservedHost` over the `SchemaHost` defaults that refuses writes,
  calls and actions with the notice; its first `acquire` raises a
  `ServiceDiagnostic` notice ("`hello` is declared by `<path>` but this
  binary does not serve it; …") which the last `release` resolves; like
  every service notice it reaches only the watchers connected when it
  is taken (`watch` does not replay it). A
  linked service whose file differs logs "stale; run `<binary>
  --write-schemas`".
- **Schema files** (`strand_compiler::schema::plugins`, std only):
  `dir()` (`$XDG_DATA_HOME/strand/schemas/`, else
  `$HOME/.local/share/strand/schemas/`), `load(dir)` (non-dot
  `*.schema`, regular files, at most 1 MiB, UTF-8), `apply`, `writer`,
  `beyond_its_service` (a text reaching beyond its own service is
  skipped or refused, `PluginError::Schema`). Files are read once per
  process, so a running shell or LSP sees a new file after a restart.
- **`--write-schemas`**: checks each registered service's text applies
  and is documented, then writes `<name>.schema` atomically with the
  header `// strand plugin schema, written by <exe> --write-schemas; do
  not edit` (`<exe>` the canonical `current_exe()`, else `unknown`). It
  removes files this exact path wrote for services it no longer
  registers and lists others untouched. `strand --write-schemas` writes
  nothing.
- **Errors** (`plugin::PluginError`, printed `<binary>: <error>`):
  `DuplicateService`, `BadName` (not `[a-z_][a-z0-9_]*`), `Schema`,
  `Undocumented`, `NoDataDir`, `Write`; the first three fail every
  subcommand before it runs, files never.
- **Process setup check.** `run` warns, naming `process_setup!()`, when
  the heap is not mimalloc's or THP is on (`plugin::setup_warning`).
- **Examples.** `crates/strand/examples/plugin_hello.rs` (a `hello`
  service) and `plugin_bare.rs` (no setup, no services); built by
  `cargo test`, never linked into `strand`.

**Shared dispatch points**: `ipc::parse`/`parse_m5`, `Shell::request`
(`run/request/mod.rs`), `ToMain`/`ToLogic`, `Shell::handle`, `Live`'s
fields, `lib.rs`'s modules and `strand::cli`, and `entry.rs`'s COMMANDS
rows take one-line entries per feature.

## Glitch reports (decisions.md report)

design.md "Reporting a glitch" as interfaces; features.md "Glitch
reports" names the proving tests. No crate gains a dependency: the
record format is in `strand-core`, and every hook is a callback the
binary installs.

**Two rings, one per thread.** Main records input, surfaces, outputs and
late frames; logic records service changes, the config's writes, calls,
actions, reloads and surface roots. Each owns its ring outright, so
nothing is shared or sent until a report asks; both stamp from one
epoch (`flight::epoch()`) and a report merges them by time.

| Ring | Records | Names | Arena | Bytes |
| --- | --- | --- | --- | --- |
| Input (main) | 6,144 × 24 B | 64 × 64 B | 4,096 B | 155,648 |
| Changes (logic) | 1,024 × 24 B | 256 × 64 B | 65,536 B | 106,496 |
| **Both** | | | | **262,144** (256 KiB) |

Each is allocated once, zeroed, when `strand run` starts and never
allocates while recording; records are written on paths that already
run, so no timer, wakeup or flush exists.

**`strand_core::flight`: types and codec only, no I/O, no threads.**
`epoch()`, `now_ns()`, `epoch_unix_ns()`; `RingSpec` (`INPUT`,
`CHANGES`); `Ring::new(spec, which)` with `push`, `merge_motion`,
`merge_text`, `name` (interned, else `NAME_IN_ARENA`), `value` (written
into the arena by a bounded writer; past `VALUE_CAP`, 8,192 B, dropped
and marked truncated), `named_value`, `push_named`, `pause`/`resume`
(a gap), `clear`, and `dump(now_ns)`, the only allocation; `read(&[u8])
-> Result<Dump, FlightError>` (newer versions refused) and
`Dump::entries()`.
- **Records** are 24 bytes, little-endian: `t` u64 (ns since the epoch),
  `kind` u8, `flags` u8 (truncated, redacted, lost, boot value,
  `LOCAL`), `slot` u16, 12 payload bytes with any arena reference
  (`ARef`) at 6..12. Kinds and payloads are the constants in
  `flight::kind` and their docs: input `0x01`–`0x0D`, surfaces and
  outputs `0x20`–`0x31` (a `window`'s configure is `0x27`), changes
  `0x40`–`0x4A`, gaps `0x3E`/`0x3F` and `0x4E`/`0x4F` (why in
  `flight::gap`). A ring keeps no change record whose value the arena
  lapped, so a dump holds a suffix with every value in it.
- **Values** are compact JSON typed by the schema or program; a field
  change is `[old, new]` or shorter `{"e": …}` edits, a keyed one
  `{"d", "r"}` or `{"patch": …}`; a held-back text is `{"~text": N}`.
- **Coalescing.** A motion merges into a newest same-surface motion
  under one refresh period old; a text key into newest same-surface
  text.
- **The dump** (`input.flight`, `changes.flight`): a 64-byte header
  (magic `STRNDFLT`, version 4; 1 to 3 still read), names, arena,
  records oldest first.

**Main side** (`run/record.rs`, `InputRecorder` in `demo::host::Host`).
- Input is recorded after routing (`Host::intents`). A press is followed
  by its `hit`, also sent as `ToLogic::Hit(NodeId)` so logic keeps the
  identity of the newest 256 hit nodes for `nodes.json`.
- **Typed text is never recorded.** A text key is counted, never named;
  in a password input (`Router::focused_input`) keys are one uncounted
  span. A paste records its length (none into a password), a drop its
  kinds. A key release is recorded only when its press was.
- **The lock.** From `Host::lock_started` to the unlock the ring is in a
  gap; lock-surface events are never recorded; picking is a gap. While
  `Real::secret_shown` holds (polkit, greetd), `ToMain::Secret(bool)`
  puts both rings in a `gap::SECRET` gap and reports are refused.
- Surfaces, outputs and windows come from the `SurfaceHost` callbacks,
  refresh and lateness from `Host::painted` (`State::refresh`).

**Logic side** (`run/record.rs`, `ChangeRecorder`, in the logic `Shell`).
- **Services.** `Services::tap(Some(Tap))` installs one callback for every
  patch `Services::pump` applies (`TapEvent { service, change }`,
  `Change::{Field, Keyed, Event}`, with `initial` and `local`); a local
  write is told as applied (`local: true`), and its echo is never a
  patch. `Services::snapshot(rt)` reads every service's fields;
  `Data::write_json` takes a per-text keep closure. Fields the program
  never reads (`Program::reads`, `ServiceReads`) are not recorded;
  events, config services and `system` always are.
- **The config's writes.** `Instance::set_cell_tap(Some(CellTap))`
  (`CellTap` is `Box<dyn FnMut(CellWrite<'_>)>`) tells each state cell,
  settings field or token overlay written in a tick once (`CellWrite { path,
  old, new }`), from the tick's own write list. Cells under a `lock` surface
  and cells bound to a password input are left out of the tap and
  `cells_snapshot` (`Ctx::secret_cells`). `Program::is_text_literal` and
  `vm::json::{to_json, from_json}` serve redaction and replay.
- **Calls and actions** are recorded where the binary handles them
  (`services/adapter.rs`, `custom.rs`, `run/exec.rs`, `composite.rs`).
- **Redaction.** `auth`, `polkit` and `greetd` are never recorded or
  snapshotted (`strand_services::secret(name)`); `clipboard` texts are
  held back; the config's own texts are kept only when
  `Program::is_text_literal` holds; call and action arguments are never
  recorded, and a call given a non-literal text has its answer's texts
  held back.
- Reloads, surface roots (never a `lock` root), lock gaps and successful
  `set`, `toggle` and `reset` requests (`ChangeRecorder::request`) are
  recorded too.

**On and off.** `system.replay: bool rw` (default true, kept in the
state directory). A change clears and pauses or resumes logic's ring
and sends main `ToMain::Recording(bool)`. Off frees both rings and
removes the taps; on allocates them anew, starting with a gap's end
(`gap::OFF`) and the outputs, surfaces and roots shown then.

**Saving a report.** `{"v": 1, "cmd": "report", "save": {"words":
"…"}}` (`Request::Report { save }`):
1. Logic refuses while a lock is asked for or shown. Otherwise it sends
   `ToMain::Snapshot { kind: SnapshotKind::Bundle, reply }`; main answers
   between frames with `bundle: Some(Box<MainBundle>)` (its ring dump,
   frames, image sources, surfaces). A frame is the newest committed shm
   buffer (`State::frame_pixels(surface) -> Option<FramePixels>`) or a
   GPU surface's scene repainted on the CPU
   (`Renderer::repaint_offscreen`); `FramePixels` is `strand-scene`'s;
   images come from `Renderer::image_sources() -> DrawnSources`,
   resolved on `strand-report`. No lock surface is included.
2. Logic adds `report::logic_part`, its dump, `Services::snapshot`,
   `Instance::cells_snapshot()`, the module and read files, version and
   commit (`env!("STRAND_COMMIT")`), wall time, `now_ns` and the words.
3. A `strand-report` thread writes the bundle and pings logic, which
   answers `{"ok": true, "bundle", "paste", "report"}` or the error.

**The bundle**: `<state_dir>/reports/<YYYY-MM-DD_HH-MM-SS>/` (0700,
files 0600):

| Path | What |
| --- | --- |
| `bundle.json` | format, words, wall and ring times, version and commit, `config_dir`, outputs, surfaces, `replay`, file hashes, `missing` |
| `words.txt` | the words |
| `diagnostics.json`, `diagnostics.txt` | `strand report --json` and `strand report` |
| `input.flight`, `changes.flight` | the dumps (absent with replay off) |
| `services.json`, `cells.json` | the snapshots |
| `history.json` | each `graph` history (`Instance::histories`) on the boot clock; always written |
| `drawn.json` | each surface's nodes as last drawn (`Renderer::drawn_nodes`) |
| `nodes.json` | origin, id and key of each node the input ring names and each surface root |
| `frames/<n>-<namespace or popup>.png` | each surface's last frame |
| `config/`, `files/<absolute path>` | the config and every other file read (2 MiB each, 16 MiB in all; the rest in `missing`) |
| `replay/` | written later by `strand replay` or `scripts/report-view.sh` |

The bundle is written as `reports/.<stamp>-<n>.partial` and renamed once
`bundle.json` is in; on an error the partial directory is removed, and
a `.partial` older than 10 minutes (a crash's) is pruned. Paths
under `files/` are lexically normalised, so `..` never escapes. Pruning
keeps the 20 newest directories with a `bundle.json`. The `paste` line
says to run `scripts/report-view.sh --no-open <dir>` or `strand replay
<dir>`.

`Renderer::drawn_nodes(SurfaceId) -> Option<DrawnFrame>` flattens the
surface's scene at its last paint, and a frame period later for each
node's `travel` and `fade`, peeking at motion without advancing it; it
runs only when a report asks, on main between frames.

**The replay** is eval's (`strand-eval replay <bundle>`; docs/eval.md
"Replaying a glitch report"): it reads dumps with `flight::read` and
values with `vm::json::from_json`, writes a local scenario, and drives
it through `SchemaHost::mock_at`, `take_fetches`, `answer`,
`answers_left`, `Instance::seed_cell`, `cell_type` and `cell_mounted`.
`scripts/report-view.sh` runs it in the container with the bundle
read-only. `strand replay [report-dir] …` and `strand replay clip …`
(`report/replay.rs`) `exec` a `strand-eval` beside the binary, else on
`PATH`, with the huge-page setting restored, and exit 1 without one.
Nothing under `crates/` depends on eval.

**Budgets.** The 256 KiB count against the PSS budgets; `budgets.rs`
fills both rings and holds anonymous PSS on minus off to at most
288 KiB (`RINGS_KB`). No thread, timer or wakeup exists while no report
is being saved, so the idle-wakeup windows hold.

## Contracts

### `strand-scene`

- **Geometry**: `Rect { x, y, w, h }` in physical pixels (`i32`/`u32`),
  `Size`, `Point`, and logical-pixel `f32` variants; `Scale` is the
  fractional scale as numerator/120 (`wp_fractional_scale_v1`).
- **Props**: `Prop::affects_layout` says which props layout reads; render
  relays out for them (`strand_render::affects_layout`), and the
  compiler's box-dependency graph (`check::box_layout_loop`) uses it for
  bindings on `id.box`.
- **Local sources**: `source_is_file(&str)` says whether an `image`'s
  `Prop::Source` is a file or an icon name; render's `image::is_icon`
  and the compiler's `check::assets` both use it. A relative `Source` of
  an `image`, `svg` or `lottie` reaches render already absolute under the
  config directory (`check::assets::local_source`); render reads the
  path it is handed.
- **Colour**: `Color` is straight-alpha sRGB `f32` with exact OKLab/OKLCH
  conversions; springs interpolate in OKLab.
- **Motion** (`strand_scene::motion`, shared by render's prop springs and
  the theme swap's palette springs): `Spring { stiffness, damping }` is a
  unit-mass damped oscillator (damping is a *ratio*), `Spring::step` its
  closed form; `SPATIAL`, `EFFECTS`, `BOUNCY` are the design's
  `$motion.*` springs. `Curve` (`Instant`, `Spring`, `Timed { duration,
  easing }`) is a resolved transition; `ease(Easing, p)` evaluates it.
  `Easing::named` (in `protocol.rs`, beside `Easing`) knows exactly
  `Easing::NAMES`; the checker's `enum
  Curve` in `builtin.schema` must stay a subset of it, so a name it
  accepts never runs as `STANDARD`. `Motion<N>` is an `N`-channel value
  in flight: `retarget`/`shift` (a FLIP jump) take effect at the next
  `sample(at)`, starting one frame before `at` (at most `START_LEAD`,
  never before the previous sample), so one frame's changes start
  together (decisions.md launcher-flip-pass); a retarget *keeps its
  velocity*; `peek`, `velocity`, `is_settled` and `probe` start nothing.
  A spring is settled once each channel's amplitude is within its
  tolerance, and stays settled, independent of frame rate (decisions.md
  roll-rate). Everything is a pure function of the timestamps sampled,
  so frames are testable as images. `TokenScope::transition` falls back
  to `SPATIAL`/`EFFECTS`/`BOUNCY` when the table lacks the token.
- **Damage**: `Damage` is at most 8 `Rect`s; a ninth merges the pair
  whose union grows area least. `Damage::area()` is what the ≤2,000 px²
  per clock tick budget is measured on.
- **Painter** (render implements, surface calls): `PaintTarget` (`wl_shm`
  ARGB8888 premultiplied, little-endian) and the `Painter` trait are in
  `crates/strand-scene/src/paint.rs`, documented there.

  Buffer-age rule: a non-empty `paint` result is a new frame, and the
  caller must commit that buffer with exactly that damage; an empty
  result means nothing was drawn, so the caller does not commit (or does
  not count it). `age` counts commits of that surface. If a painted
  buffer cannot be committed, call `Renderer::invalidate`.
  `PaintTarget::new` sets `time` to zero; the surface manager sets the
  predicted presentation time (`.at(t)`), tests pass fixed values.
  `opaque_region` is in buffer pixels; convert with
  `Scale::inner_logical_region`, which rounds inward. The input region
  defaults to the box inside the shadow overhang: render puts the shadow
  reach into `SurfaceSpec::overhang` and the surface manager sets the
  region from it, unless `Painter::input_region` returns the rects the
  last frame drew (a popup that draws nothing itself takes input only
  where its content is; an empty list takes none).

- **Input**: `InputEvent` (pointer, keyboard, `Key { key: KeyInput }`
  whose `latin` keysym shortcuts match through `KeyInput::shortcut()`,
  `ClickAway`, `Paste`, drag-and-drop) is in surface-local logical
  pixels per `SurfaceId`. `strand-surface` produces it; render hit-tests
  it on the main thread and forwards node events to logic. Wayland
  serials stay in `strand-surface`.

- **Scene protocol** (logic → render, one batch per tick): `SceneDiff`
  holds ordered `SceneOp`s over a retained tree: `Create`, `Remove`
  (render plays `exit` before unmounting), `Move`, `SetProp { id, prop,
  value, transition }`, `SetTokens { table, transition }` (`Instant` for
  the boot table, `Default` after). Node ids are generational; a removed
  id is dead at once and its slot may be reused in the same diff.
  `Move`'s `index` counts the new parent's children after the node is
  detached. `parent: None` marks a surface root; `PropValue::Unset`
  reverts a prop; comma shorthands may arrive as a `List` of 1–4 values
  expanded like CSS; call-shaped values are `PropValue::Call`; a prop
  naming another node (`nav: results`) is `PropValue::Node(id)`; a
  surface's declared name is `Prop::Name`, set by the compiler; `rotate`
  is a `PropValue::Angle` in degrees; `marks:` arrives as a list of
  `[start, end]` pairs (the compiler's scene conversion of a `Range`).
  `transition` matches `~`; `TokenScope::transition` resolves `Default`
  and `Token(path)` through `$motion.*` at render time (class `Snap`
  props always snap). `Renderer::attach_surface` maps a surface root to
  Wayland surfaces. A surface-kind node under a parent (a `popup` in a
  `bar`) is its own root: it inherits tokens, colour and font but paints
  only on its own surface.

  Tokens travel unresolved as `PropValue::Token(TokenExpr)` (also nested
  in `List`, `Pose`, `Call`, and `Template` for composite values), and
  `TokenScope::resolve` resolves them. The `TokenTable` holds plain
  values (palette roots, scales, fonts, springs) and derived tokens as
  expressions that render evaluates at flatten time every frame, so only
  palette roots need to spring. Palette extras read outside templates
  (`lower::Program::palette_reads`) are plain colours at
  `palette.<name>`; logic reads them through `VmHooks::palette`. Derived
  colours are gamut-mapped. `TokenTable::contrast` holds the contrast
  guard's pairs: a text token's lightness is solved to 3:1 over its
  backgrounds in the node's scope (`Color::with_contrast`, memoised on
  the render thread), so a palette mid-spring and subtree overrides stay
  readable. `TokenTable::origins` is provenance only. Text with no
  `color`/`font` draws in `$fg`/`$font.ui`; a `bar` with no `bg` paints
  `$surface`. Logic resolves which theme applies.

  Subtree overrides (`set { }`, a component's `tokens { }`) are the
  `tokens` prop (`PropValue::Tokens`), resolved through the `TokenScope`
  chain (global table, then each ancestor's override, nearest first). An
  override's right-hand side sees its parent scope (`set { $surface:
  $surface.alpha(0.5) }` is not a cycle), and global derived tokens are
  evaluated in the asking node's scope. `TokenTable::freeze()` caches
  every token's value except those reading time (`time_paths`); render
  freezes when `SetTokens` lands and each swap frame; a clone is not
  frozen, `insert` thaws, a direct field write needs `freeze()` again.
  `enter`/`exit` hold a `PropValue::Pose` or a preset keyword.

- **Surfaces**: `SurfaceSpec` (`crates/strand-scene/src/surface.rs`) is
  what a surface-kind node asks of Wayland, with `thickness()`,
  `exclusive_edge()`, `exclusive_zone()` and `needs_recreate()`. Logic
  pins each per-monitor instance of a `bar`, `panel` or `osd` whose
  `screens:` picks monitors (`Screens::Named`); a `lock` has one
  instance per monitor its `screens:` picks, and with `screens: focused`
  (or nothing it picks plugged in) one instance with `screen` bound to
  the focused (else first) monitor and no pin (decisions.md lockmon); a
  `window` is one instance, unpinned. A surface without a size of its
  own gets it from a content layout pass before the spec is reported.
  Render resolves specs through the node's token scope after every
  `apply`: `Renderer::surface_spec(node)` reads one, and
  `take_surface_changes()` returns `(NodeId, SurfaceChange)`s
  (`Created`, `Updated { spec, recreate }`, `Removed`) in order. A popup
  with `anchor: pointer` is anchored at the press that opened it: the
  host calls `Renderer::pressed_at` for each press before routing it
  (`released_at`, `key_step` for other input steps).
- **Layout**: taffy 0.14 on the render thread: one pass per surface whose
  layout inputs changed (paint-only props never relayout), boxes in
  surface logical pixels (`Renderer::boxes`), `x`/`y` applied at flatten
  time as paint offsets. `Renderer::scroll` and `scroll_into_view(list,
  row)`; a `list` lays out only the rows in view. Text takes no room
  until its first layout arrives (decisions.md late-text-dip).
- **Animation** (`anim/`): the render thread owns every spring. A prop in
  `ANIMATED` (`x`, `y`, `opacity`, `scale`, `rotate`, `bg`, `color`,
  `border`, `shadow`, `radius`, `value`, `track`, `stroke`, `trim`,
  `wave`, `glow`, a text's solid `fill`, and a `shader`'s uniforms) set on a
  surface shown with a clock springs along `TokenScope::transition`;
  `width`/`height`/`size` spring the laid-out size. Other layout lengths
  snap and the boxes they move glide (FLIP), as do siblings of created,
  removed and moved nodes and every box after a `SetTokens`; text changes
  never glide. A target changed by tokens or inheritance snaps at rest and
  steers a motion in flight.

  `enter` plays for a node created on a shown surface or in the diff that
  opens it. `Remove` of a node with a pose makes its subtree a ghost
  (`SceneTree::ghost`: dead to logic, slot free at once, drawn but never
  hit; the `Router` drops its focus) that unmounts when the pose
  settles; a surface whose `open` goes false stays open in its spec
  until its exit or its ghosts settle. Exits are bounded:
  `MAX_GHOSTS_PER_PARENT` (8), and an exit on a surface that painted
  nothing for `EXIT_STALL` (1 s) or older than `MAX_MOTION` + 1 s ends
  at the next `apply`/`update`. `Renderer::next_wake()` is the earliest
  such instant; the renderer arms its own timer and wakes the loop
  through the text worker's waker, so a surface whose output stopped
  sending frame callbacks still closes.

  A content-sized panel, OSD or popup whose content's size steps springs
  its root's box over the step (`Renderer::lay_out_stepped`); the buffer
  is configured at the larger size so the anchored edge stays put, and
  meanwhile `Painter::input_region` is the box as drawn. A content-sized
  surface never shrinks while something on it moves; a panel or OSD
  anchored to one edge is as wide as its output along that edge and
  never resizes there (`Renderer::centred_span`). The paint that
  finishes an exit or lets a held surface shrink refreshes the specs, so
  the host must check `has_surface_changes()` after every `paint`. A
  surface just attached previews at time zero, so `enter` plays from its
  first painted frame. Not animated: gradients, `mark_color` (span
  colours are part of the shaping request) and `blur` (the
  compositor's).

  `PaintTarget::time` zero and `reduced_motion` snap everything. `strand
  run` sends each change of `system.reduced_motion` (the portal's
  `SystemSetting::ReducedMotion`) with the next diff
  (`SceneDiff::reduced_motion`, applied as
  `Renderer::set_reduced_motion`); a theme can also set `motion.reduced`.
  `Painter::wants_frame` is true while anything moves.
- **Theme swaps** (`renderer/swap.rs`): a `SetTokens` with a non-`Instant`
  transition, on a surface with a clock and motion not reduced, springs
  the palette roots that differ in premultiplied OKLab along
  `TokenScope::transition(t, Prop::Color)`; every other plain token
  snaps. Each frame writes the roots into `SceneTree::tokens` before
  flattening, so derived tokens and the contrast guard follow exactly;
  a newer table retargets with velocity. Span colours are slot
  stand-ins, so a springing `$accent` never reshapes text. The planned
  roots are first played through in the global scope and each `set { }`
  scope that reaches a declared background (at most 32, bounded work);
  where a readable pair would dip below 3:1, the global case snaps and
  crossfades every shown surface, a scope case crossfades only the
  surfaces drawing it. A crossfade blends from a snapshot of the old
  frame (on the GPU, the GPU's last frame). `Renderer::swapping()` and
  `swap_crossfades()` report it.
- **Hit testing**: `Renderer::hit(surface, LogicalPoint) -> Vec<NodeId>`
  is the topmost node under a point in the last frame and its ancestors
  up to the surface root. A node is hit inside its laid-out rounded box,
  grown by `hit: grow(n)` and cut by ancestors' clips; shadows never
  count; transforms are undone (`HitBox::inverse`).

- **Render loop** (the binary wires this; surface calls `Painter`): the
  steps that touch the renderer and the input router are
  `strand_render::host_loop`'s reactions (`apply`, `woke`, `input`,
  `attached`, `configured`, `detached`, `painted`), which the shell's
  main thread (`crates/strand/src/demo`) and the eval driver
  (`eval/src/harness.rs`) both run behind `host_loop::LoopHost`; no host
  takes them by hand outside tests (`eval/tests/drift.rs`). The router
  side: `Router::tick` before `update` on every wake, and
  `Renderer::set_input_wake(Router::next_wake())` after routing.
  0. After each `apply`, drain `take_surface_changes()` into the surface
     manager: create per matching output for `Created`, reconfigure or
     recreate for `Updated`, destroy and `detach_surface` for `Removed`.
  1. Spawn the text worker with `TextWorker::spawn_with_waker(config,
     Some(waker))` (the waker pings the main loop) and build
     `Renderer::try_new(TextBackend::Worker(worker))`, which also starts
     the image worker (`Renderer::new` decodes inline: tests and offline
     tools).
  2. When the ping fires, call `Renderer::update()`, which collects
     delivered layouts and marks surfaces dirty.
  3. Call `apply(diff)` once per logic tick.
  4. On output and configure events call `attach_surface`,
     `configure_surface(size, scale)` (before the first paint) and
     `detach_surface`.
  5. Request a frame callback while `wants_frame(surface)` and `paint`
     in it; afterwards, if `has_surface_changes()`, run step 2. Commit
     only a non-empty result with exactly that damage and the converted
     `opaque_region`; on a failed commit call `invalidate`. A surface
     that has never painted holds its first frame for its text up to
     50 ms (`set_first_frame_wait`): arm a timer for
     `frame_deadline(surface)`. An idle surface holds a frame for new
     text up to `NEW_TEXT_WAIT` (16 ms); a surface in motion paints at
     once and the glyphs follow (decisions.md wave2-exit).

- **Widgets and paint** (decisions.md wave3-pixels): widget state the
  router owns but widgets draw (hover, press, focus, caret, a dragged
  slider's value) lives in render (`strand_render::widgets::Widgets`),
  written through `InputScene`, so widgets answer on the frame the input
  arrives; logic still gets flags and two-way writes. Images decode at
  the box's physical size into a 32 MB LRU on a worker; the paint cache
  (32 MB) frees entries unused for `IDLE_FREE` (30 s) at the next paint
  or `update`, never by a wake of its own, so an idle shell does zero
  work between ticks. A paint of at least `PARALLEL_CELLS` cells draws
  them on up to `MAX_THREADS` threads, joined before `paint` returns;
  each cell is drawn alone, so the thread changes no pixel.
  `Renderer::last_paint_times` says where a paint spent its time.
  Painting a surface moves other surfaces' holds on by the time it took
  (`Renderer::pause_holds`, at most `MAX_HOLD_PAUSE` in all). A
  `popup`'s spec gets `parent` and `anchor_rect` (`None` until that box
  is laid out; decisions.md popup-boot-wake); `tooltip: expr` makes a
  render-owned popup after `TOOLTIP_DELAY`.
- Text is shaped once without a width bound per (node, scale) and
  aligned at flatten time; only a box narrower than it asks for a layout
  of its width (decisions.md wave3-pixels).

- **M4 vocabulary**. The scene types for time, effects, media, lists,
  drag and drop and surface poses; `strand-compiler/tests/
  scene_catalogue.rs` checks them against builtin.schema. `Prop` stays a
  `Copy` enum. The GPU backend, shader ABI and `Effect::Shader` are in
  "`strand-gpu`".
  - **Time-bound values** (design.md, "Motion and time"). `TokenExpr`
    has the leaves `Time` (`t`, seconds since the node appeared), `Wave`,
    `Noise`, `Index` and `Count`, and `Math` for a maths builtin of one;
    `Template::numbers` holds numeric slots, so `glow: 10 * wave(2s),
    $accent` travels as one value. A scope reads a per-node
    `TimeContext` through `TokenScope::with_time`; under
    `reduced_motion` it is `TimeContext::frozen`. A prop that reads time
    (`PropValue::reads_time`, also through a global token) repaints its
    node each frame while visible, and only it.
  - **Per-node clocks with frame caps.** A node that reads time or plays
    frames has a clock (`strand-render`'s `clock.rs`); a clock not due
    by the next frame leaves `wants_frame` false and puts its due instant
    in `next_wake()`. Hidden nodes have no clock, and the frame loop
    stops when every clock is idle.
  - **Shader uniforms.** `u_*` props travel as one `Prop::Uniforms`,
    each entry springing as its type would; the checked WGSL is
    `Prop::Shader` (`PropValue::Shader(Arc<ShaderCode>)`), set by the
    compiler.
  - **Canvas.** `Prop::Draw` holds `PropValue::DrawList(Arc<[DrawOp]>)`,
    what the VM recorded running `draw: (c) => …` over a `Canvas` of the
    node's laid-out size. `DrawOp` (`strand_scene::canvas`,
    `#[non_exhaustive]`) has one variant per `Canvas` method
    (`DrawOp::METHODS`, checked by the catalogue test), themed variants
    whose paints name tokens, and path ops (decisions.md flex).
  - **Effect layers** (design.md, "Runtime changes these need").
    `strand_scene::effect::Effect` is a group effect (`ColorMatrix`,
    `Blur`, `Blend`, `Mask`, `Opacity`, `Shader(ShaderPass)`);
    `Effect::reach(scale)` is how far it spreads damage. Render brackets
    a group with `Item::PushLayer`/`PopLayer` and lowers it per backend
    (masks always on the CPU). `Item::Raster` is a CPU raster node
    (particles, grain, graphs, spectrum, animated frames) cached at its
    clock's rate through a `RasterSource`. Offscreen groups
    (`offscreen.rs`, `OFFSCREEN_BYTES`) redraw only when their content
    changes; fade layers (`offscreen/fade.rs`) draw a group that only
    moves or fades once and reuse it, matching a re-raster within the
    PNG tolerance. Media nodes (`graph`, `spectrum`, `svg`, `lottie`,
    `thumbnail`) get their sources from render.
  - **SVG parts.** An `svg "icon.svg" { #needle { … } }` selector block is
    a child of kind `NodeKind::SvgPart` carrying the id as `Prop::Name`;
    render applies its props to that layer.
  - **Keyframes.** `Prop::Play` holds `PropValue::Keyframes(Arc<
    Keyframes>)`, the compiled block inline, so render keeps no keyframe
    table. Keyframes are offsets composed with the node's springs; a new
    `seq` restarts them.
  - **Backlog** (decisions.md history). `Prop::Backlog` holds a `graph`'s
    history (`strand_scene::Backlog`), points stamped on the boot clock
    (`CLOCK_BOOTTIME`), a non-finite value marking a gap. Logic sends it
    on mount and when it gains a point; render fills only the columns it
    did not sample, and it repaints nothing by itself.
    `Renderer::set_history_clock` replaces the boot clock (tests, eval).
    `DEFAULT_HISTORY` (60 s) is the unset `history` on both sides.
  - **Ranges** (decisions.md graph-follow). `Prop::Range` on a `slider`,
    `meter`, `arc` or `graph` is two ends (numbers or `auto`); unset, 0
    to 1. A slider's drag writes in range units (`Range::at`, rounded
    by `Prop::Step`). A graph's `Value` does not spring.
  - **Layout and input props** (flex). `Prop::Wrap`, `Prop::Axis`,
    `Prop::Snap`; `Prop::Cursor` is resolved by the Router and handed to
    strand-surface (`SurfaceHost::cursor`); `Prop::OnKey` marks an
    element with `on key`, and Tab moves focus in render only when no
    element from the focused node out to its surface has it.
  - **Virtualised lists.** On a `list` whose direct child is a `for`,
    logic sets `Prop::RowCount` and `Prop::RowFirst`, and `Create`/
    `Remove` carry `window: true` for rows the window mounts or unmounts
    (no `enter`, `exit`, FLIP or ghost). Render lays rows out at their
    global indexes, never shows past the mounted rows, and reports the
    rows it wants with `Renderer::take_list_windows()`, sent as
    `ToLogic::ListWindow`. `Renderer::list_frames()` counts frames, gaps
    and stalls for the list checks.
  - **Directional pages** (design.md, "Pages"). Logic sets
    `Prop::RowFirst` on a `pages` node one more or less when it swaps the
    page (only the direction counts); render slides the pages
    accordingly. `Renderer::page_swap(pages)` pairs the two pages.
  - **Drag and drop** (design.md, "Drag and drop"). `Prop::Drag` is a
    `List` of the dragged value's type, kind, exported texts and
    identity; only `strand_scene::drag_type`, `drag_export` and
    `drag_identity` may look inside it. `Prop::Accepts` lists the types
    an `on drop` takes. `DropPayload` lives here because
    `strand-surface` produces the external one: `Node(NodeId)` (a
    `drag:` source in Strand, mapped back to its value by logic) or
    `External { kind: DropKind, files, text, app_id }`; logic resolves
    `app_id` to an `App` through the `apps` service when it builds the
    `Drop` value (null when no app has that id). `NodeEvent::Drop {
    payload, at, hidden }` carries `at` a global row index, with the
    dragged source's row left out of its own list and another list's
    first copy of the dragged item out of that list; `hidden` covers a
    windowed list's unmounted rows, resolved by `Instance::drop_at`.
    The binary forwards it unchanged as `run::NodeEvent::Drop`, and
    `run/lists.rs` makes the handler's arguments on the logic thread.
    `Prop::DropRows` and `Prop::NotRow` mark which children are rows.
    The Router starts a drag past `DRAG_THRESHOLD` (6 px);
    `Renderer::drag_image` draws the drag icon.
  - **Surface poses** (design.md, "Compositor-animated poses"). When the
    compositor allows it and a root's motion is only its opacity, scale
    and x/y, render paints at rest and reports a `SurfacePose` through
    `Painter::surface_pose`, applied with `wp_alpha_modifier_v1`, the
    viewporter and layer-shell margins. A pose change with no damage is
    a pose-only commit (no buffer, `age` unchanged).
  - **Compositor capabilities.** `CompositorCaps`, reported through
    `SurfaceHost::compositor_caps`; the host hands render
    `set_compositor_blur`, `set_compositor_poses` and
    `set_compositor_pose_scale(!hyprland)`.
  - **Backends** (`strand_scene::backend`): `Backend`, `BackendChange`,
    `GpuStatus` and `AdapterInfo` live here so render, logic and the
    binary name them in a CPU-only build (decisions.md m4-scene).
  - **Scrims and fillets.** `SurfaceSpec::scrim: Option<Color>` (`popup`
    and `panel`; a single-pixel buffer, so `color`, not `paint`). `attach`
    fillets are drawn outside the box: render adds their radius to
    `overhang`, so the input region stays the box.
  - **Input.** `InputEvent`'s `DragEnter`, `DragMotion`, `DragLeave` and
    `DragDrop` come from `wl_data_device` (external drops and drags
    between Strand surfaces).

- **Router hooks** (`strand_render::input`). The `Router` is the one
  place input is routed. New `InputScene` methods always have no-op
  defaults.
  - `Router::pointer(surface)`: the last pointer position, handed to
    render with `Renderer::set_pointer` for `parallax` and `tilt`.
  - `Router::drag() -> Option<DragView>`: the drag in flight.
  - `Router::drop_target(surface)`: where a drop would land; the surface
    manager accepts an offer only while it is `Some`.
  - Virtualised `nav`: `Router::selected_index(list)` is the selection's
    global index, mounted or not; a row not mounted is revealed with
    `InputScene::reveal_index` and the selection lands in
    `Router::settle` once logic mounts it.
  - Submenus: Right opens the first closed two-way `popup` under the
    selected row (else the hovered chain); Left in a nested popup closes
    it as Escape does (decisions.md m4-lists-w1).
  - The lock: keys on a lock surface while render's built-in fallback is
    shown never reach the `Router`; the binary (`run/lock.rs`) hands
    them to `strand_render::lock_fallback`. A `type: password` value is
    redacted by the binary in `strand watch`, logs and the inspector.
  - `trigger: hover` popups (spec.md 6.3): the router opens and closes
    them by `Intent::Write { prop: Open }` (which `Instance::write`
    accepts for such a popup only) after `Prop::Delay` and
    `Prop::Grace` (default `HOVER_GRACE`, 200 ms); `Router::observe`
    tells its own writes from logic's; the program's `open:` is lowered
    to `Prop::Hold`, which opens it at once. `Router::next_wake()` and
    `Router::tick` drive the timers. `SurfaceSpec::hover_trigger` tells
    the surface manager never to grab for it.

### `strand-theme`

Palettes, used by the compiler's VM (`material()`, `import()`) and by
the instance's built-in theme; render reaches it only through the token
table.

- `Role` (49 Material 3 system roles, then the 13 hue roles of "The
  theming hub" below: 62), `Palette` (every role a colour;
  `insert_into(&mut TokenTable)` writes the roots, origins and contrast
  pairs; `to_text`/`from_text` the persisted form), `Partial` (what an
  importer found; `fill()` derives the rest, then guards).
- `material::from_seed(Color, Options)`: `material-colors` 0.5, spec
  2021 pinned (`material::SPEC`); each thread keeps its last eight
  palettes (`forget_kept_palettes()` for the timing gates).
- `image::Quantiser`: `lookup(path)` (`Ready`, `Pending { last }`,
  `Failed { error, last }`) costs a `stat` on the calling thread; a
  worker hashes the file and decodes only unseen content into a seed,
  kept in a versioned index under `$XDG_STATE_HOME/strand/palettes`.
  `set_waker`, `poll()`, `invalidate(path)`.
- `writer::FileWriter`: atomic writes on a worker (latest per path
  wins), `flush(timeout)`; drop waits up to 1 s.
- `import(source, base_dir)`: `catppuccin:<flavour>[:<accent>]`,
  `base16:`, `base24:`, `matugen:`, `w3c:` + a file of at most 1 MiB
  (decisions.md wave3-theme).
- `contrast::{PAIRS, guard, ratio, solve}`, `defaults::base_tokens()`
  (design.md's `tokens base`), `gamut::map`.

#### The theming hub

The design and its reasons are in decisions.md m5-tools ("the
owner-approved theming design" and after) and m5-palette.

- **strand-theme.** 13 hue roles after the Material 3 ones
  (`Role::HUES`, `is_hue()`; `m3()` is `None` for them); `Partial::fill`
  derives missing hues by harmonising toward the accent and never
  changes given ones; hue roles are not contrast pairs.
  `Palette::extras()` and `name()` are the palette's extras and identity.
  `export::{json, base24}(&Palette, &TokenTable)` are pure writers over
  the resolved table (overlays included). `Quantiser::read_only(dir)`
  never writes the index.
- **strand-scene.** `TokenTable` has the hue roots (`HUE_ROOTS`) and
  reserves `palette` as a first segment (`is_palette_path`;
  `check::reserved_token`). Identity and extras are held apart in
  `TokenTable::palette` (`palette_value(name)`): they never spring, are
  never frozen and no token expression reads them. The template-only
  methods never reach the render thread.
- **strand-compiler.**
  - `Instance::palette()` evaluates `use palette` with no fallback; a
    pending wallpaper gives its last value or `PALETTE_PENDING`
    (`is_pending`), the one sentinel the outputs, the IPC request and
    the offline mount share. `Instance::tokens_now()` is the current
    table, overlays included.
  - The offline mount uses the persisted palette and a read-only
    `ThemeHost` (`Storage::read_only`), which writes nothing.
  - The other mode: the theme evaluated with `system.dark` negated, in a
    child env with fresh slots for the `let`s that reach it, never
    remembering a palette. It is one counted memo per instance
    (`hold_other_mode()`, `release_other_mode`, `other_table()`), held by
    each output or IPC `export` whose template reads `.light()`/`.dark()`
    and pulled on the logic thread after the tick's diff.
  - `template::Template::compile(file, src)` and `compile_expr` make a
    pure IR over a token-only scope plus the template-only methods
    `.red()`, `.green()`, `.blue()`, `.opacity()`, `.light()`, `.dark()`;
    `Template::render(current, other)` is pure and `Send`.
  - The `file` element (grammar.md `top_file`), checked by
    `check/files.rs`, lowers to `lower::ElementKind::File`.
    `check::files::templates` and `check::assets::check` touch the disk,
    so `strand check` and the LSP run them, not the loader.
  - Outputs are pulled: each mounted `file` is a content memo, and
    `Instance::take_outputs()`, called after the tick's diff, renders
    the changed ones on the logic thread (`OutputContent::Write` or
    `Failed`). `Instance::output_targets` names the paths written now,
    and the writer drops its retries of every other path.
- **strand-core.** A read-only `PersistStore` for the offline mount.
- **strand (binary).**
  - `strand export json | base24 | --template <file> [--palette
    <expr>] [-o <file>]` (`src/export.rs`); the socket request
    `Request::Export` renders against the live table through the same
    `export::produce`, and the CLI falls back to the offline mount when
    no shell answers.
  - The output host (`run/outputs.rs`) is on the logic side; rendered
    bytes go to the file writer thread (`strand-files`, Threads), so
    logic never waits on a disk or a child, and main never sees an
    output. The writer writes atomically, pre-registers its writes with
    the watcher (`Live::own_writes`), keeps a manifest in
    `$XDG_STATE_HOME/strand/files`, refuses unowned and `.strand`
    targets (an unowned one's notice offers `[replace]`), and retries
    failures. `run:` is debounced 200 ms, one run per file plus one
    queued, killed after 10 s; failures become notices and `strand
    watch` events, and the last good file stays.
  - `entry.rs` serves every one-shot command from one table
    (`entry::one_shot`).

### `strand-core`

A fine-grained push-pull graph, generic over value types
(`T: Clone + PartialEq + 'static`): `Signal` (state), `Memo` (lazy
derived), `Effect` (observer at the edge, e.g. the scene emitter),
batching into ticks, generational handles where a stale read returns an
error. Keyed collections publish `VecDiff`s and incremental
`filter/map/take/sort_by` keep keys. `Async<T>` keeps its last value and
exposes `pending`/`error`. The dynamic `Value` used by the VM is defined
by `strand-compiler`, not here. See `crates/strand-core/src/lib.rs`.

- One `Runtime` per logic thread, passed as `&Runtime`; it is `Rc`-based
  and not `Send`. Reads and memo closures return `Result<T, Error>`.
- The host loop calls `rt.tick(now)` and sleeps until
  `rt.next_deadline()` or the wake hook (`rt.set_wake_hook`);
  `rt.is_idle()` with no deadline means idle. Each `Tick` carries the
  `errors` and `diagnostics` raised since the last one.
- The scene emitter calls `rt.watch(id)` per bound prop and turns
  `Tick::changed` (creation order) into `SetProp`s; `for` loops turn a
  collection's diffs into keyed `Create`/`Remove`/`Move`. A `for` over a
  plain list expression uses `rt.keyed_memo(key_fn, f)`.
- Mounting runs inside `rt.scope(..)`; `scope.dispose(rt)` drops its
  nodes, timers and handlers.
- Reload identity: `rt.reparent(id, new_owner)` moves a node before the
  old owner is disposed. A runtime fault freezes one component with
  `rt.suspend(scope)` (state kept, service events kept up to
  `MAX_FROZEN_EVENTS` per listener, input dropped); the fixing reload
  calls `rt.resume(scope)` or reparents and disposes. Reloaded timers
  take over the old countdown with `rescale_from`.
- Handlers: each listener, timer and `on change` handler has a site
  (`rt.handler_site()`, `rt.site_of`) owning its tasks; disposing it
  cancels them at their `await`. `rt.spawn_for(site, fut)` runs
  graph-triggered events, rate-counted per site (the 30 writes/s
  guard); `rt.spawn_input(Some(site), fut)` runs external input, not
  counted up to its first suspending `await` and counted per run after.
  `<->` writes are not counted. Timers are `rt.after/every(_dyn)`; `on
  change` is `rt.on_change_each` (`rt.on_change_after_each` for the
  late phase), each target paired with the
  identity of the keyed object it reads, so it never fires on a sink
  switch; `on change` handlers run after the tick's other effects
  settle.
- Ordering: sinks run in a computed topological order over read edges,
  ownership and handler write edges. The compiler declares
  `rt.reads_from(node, &sources)` (the syntactic read set over every
  branch, even empty) and `rt.writes_to(handler, target)` before the
  first flush; then every sink runs exactly once per flush with final
  values. Undeclared edges are learned when first seen (a sink may
  re-run once). `writes_to` returns `WriteEdge::Ranked | Feedback`: a
  write edge that closes a loop is a feedback edge bounded by the
  runtime cycle guard, not an error. Listeners and woken tasks are
  ranked like sinks (at one rank, tasks before listeners before sinks);
  tasks woken from other threads are polled at the start of the next
  flush, never between sinks.
- Reload writes: `signal.set_reloaded(rt, v)` (and
  `xs.replace_all_reloaded`) change a cell while every downstream `on
  change` takes the value as its baseline instead of firing ("`on
  change` fires on changes, never at boot or reload").
- Keyed writes from throttled handlers land as one keyed diff
  re-applied by key (`Diagnostic::KeyedConflict` counts what no longer
  applies). Read collections without copying through `xs.with`,
  `with_untracked`, `get_key`.
- Service writes: `write_tagged(value, send)` with reports through
  `receive`; keyed list items through `KeyedSignal::write_item_tagged`
  and `receive_items(diffs, echo_of)`, which drops a write's echo
  (latest write wins per item); `forget_echoes()` when a service run
  ended unanswered. An async service call is `rt.async_memo(input,
  fetch)`.
- Service lifecycle (start on first reader, stop 5 s after the last
  leaves or goes invisible) is driven by the VM through `acquire` on
  mount or show and `release` in `rt.on_cleanup` or on hide.
- Introspection for the inspector, `strand watch` and the LSP:
  `rt.sources/observers/owned(id)`, `rt.site_of(handler)`.
- `state x = d persist` is `rt.persisted(&store, path, d, encode,
  decode)`: one `PersistStore::from_env()` per process
  (`$XDG_STATE_HOME/strand/persist`, one file per cell path), writing
  through one persist IO thread, so `fsync` never stalls a tick. `path`
  names one live cell, instance-qualified (`list[<key>].x`). A second
  live cell on a path in use is `Diagnostic::PersistPathInUse` and waits
  for the first to go. The `Persisted` handle offers `redeclare` (adopt a
  new default only when the value still holds the old one),
  `reset_reloaded` (`@reset`, a reload write) and `reset` (the overlay's
  `[reset]`). A write is never lost to the debounce: unmount,
  `rt.shutdown()` and dropping the last handle queue the live value.
  Instance-qualified files unclaimed for 90 days
  (`PERSIST_RETENTION`) are swept; plain paths never expire.
  `rt.persisted_keyed` persists a keyed list.
- Strand's own writes: `persist_store.on_written(..)` runs on the persist
  IO thread for every file about to be replaced or removed, before the
  rename; the binary hashes the content and pre-registers it with
  `strand-watch`.
- `state prefs from "prefs.toml" { … }` is `rt.settings_file(&store,
  path, fields)` (`strand_core::settings`), one `FieldSpec` per field
  over `toml_edit::Item` (re-exported). The `Settings<V>` handle gives
  each field a `Signal` whose writes are saved after 250 ms of quiet,
  keeping comments and order; `reload(rt)` when the file changes (a
  syntax error keeps the last good values; unsaved writes never undone);
  `set_overlay`/`clear_overlay` (overlay over file over default; a
  read-only target's writes go to the overlay); a last-good snapshot
  per file; `redeclare(rt, fields)` for a changed declaration. Reports
  are `Diagnostic::Settings(SettingsNotice)`. Watcher hand-off: see
  `strand-watch`.

### `strand-compiler`

`syntax` (lossless lexer and parser with spans and recovery), `schema`
(builtin elements, services, functions and tokens as data), `ty`
(types), `check` (names, types, did-you-mean) producing `hir` (the typed
program), `lower` (bytecode), `vm` (evaluates bytecode against
`strand-core` signals and services; codecs for persistence and settings
files), `instantiate` (mounts a program and emits scene diffs),
`reconcile` (old program + new program → identity map → `SceneDiff` and
state migration). One crate serves runtime, `strand check` and the LSP.
The grammar is in `docs/grammar.md`.

Public interfaces:

- **Files** (`strand_compiler::source`): `FileId(u32)` indexes a
  `SourceMap`; a `Span { start, end }` is a byte range in one file.
- **Parsing** (`strand_compiler::syntax`): `parse(file, src) -> Parse`.
  Never panics. `Parse::tokens` is the lossless token stream. Trees are
  at most 256 levels deep (grammar.md, "Error recovery"), so passes may
  recurse on a 2 MiB stack.
- **Lowering into `strand-core`** (the VM's obligations): declare every
  read set with `rt.reads_from` and every assignment and `emit` with
  `rt.writes_to` as nodes are created; read keyed collections with
  `with`/`with_untracked`/`get_key`; tests (and debug builds) run with
  `rt.set_strict_edges(true)`, which reports each learned edge as
  `Diagnostic::UndeclaredWrite`/`UndeclaredRead`; `WriteEdge::Feedback`
  is not an error (only a static cycle among `let`s is); an async
  `let`'s input reads go on `memo.effect_id()`, a debounce's on
  `d.effect`/`d.timer`; keep `Persisted` handles for reload and reset;
  adopt other changed defaults with `set_reloaded`; node closures use
  their `rt` parameter or a `WeakRuntime`, never a captured `Runtime`
  clone (an `Rc` cycle that loses debounced writes); the binary calls
  `rt.shutdown()` on SIGTERM, SIGINT and normal exit before dropping
  the stores. The compiler meets this with `lower::reads`
  (`Program::reads(chunk)`, `Program::writes(chunk)`), resolved to core
  nodes by the instantiator (`ServiceHost::sources` for service fields).
- **Identity and change detection.** AST `PartialEq` compares spans, so
  reload identity and "did this handler change" use a span-insensitive
  structural hash over the significant tokens in a node's span;
  `reconcile` owns it.
- **Reconcile** (`strand_compiler::reconcile`): `Build` is a compiled
  config, plain data (`Send`): `Build::compile(prev, SourceMap)`
  inherits identities from `prev`. `Identity` gives every element,
  surface, call, `for`, `if`, `match`, handler and timer a `Sid` (source
  text through a token diff, then `id:`, then position). `Hashes` are
  Merkle hashes per handler and timer over everything they reach.
  `Report` is what a reload did (`EditClass` names design.md's edit
  table rows). `reconcile::loader::Loader` is the compiler worker's
  state (`.with_check(ExtraCheck)`: `strand run`'s D-Bus
  introspection); `boot()`, `changed(..)`, `rescan()`, `recheck()`
  commit the largest consistent set of changed files and hold the rest.
  A commit is cached under `$XDG_CACHE_HOME/strand/last-good/`, keyed
  by source hashes, `COMPILER_VERSION` and `Schema::fingerprint()`, and
  a config broken at boot starts from it; `store_cache()` runs after the
  build is sent on, so the disk write is never on the save-to-pixels
  path (decisions.md m5-open-reload).
- **Diagnostics** (`strand_compiler::diagnostic`): `Diagnostic {
  severity, code, message, labels, help, suggestions }`.
  `Severity::Note` is never a load notice. Editors and the overlay read
  `suggestions` (the proposed replacement as data), never the help
  text. `render` draws miette reports; `render_short` gives one
  `file:line:col: severity[code]: message; help` line each.
  `suggest`/`closest` are the shared, deterministic near-miss logic.
- **Schema** (`strand_compiler::schema`): everything the language knows
  before reading a config, as data, written in `schema/builtin.schema`
  and parsed once by `Schema::builtin()`. Service crates clone it and
  call `Schema::extend(text)`, which only adds (a duplicate is "declared
  twice"; atomic on error), then check with `compile_with`.
  `provisional` stubs are replaced in place by the first extension that
  declares the name. `handle record`s compare but `persist` and settings
  refuse them. Configs see contributed names as a prelude their
  declarations shadow; a config `service` named like a builtin shadows
  it with `check::shadows_builtin`, the runtime's own reads still
  reaching the builtin (spec.md 4.2). Members after `.` come from
  `schema::members_of`, the one table the checker and the LSP use; `///`
  comments document entries (`Schema::doc(&DocKey)`), and every builtin
  entry is documented (a test enforces it). `Schema::fingerprint()` is
  part of the compiled-output cache key. The schema's element kinds and
  props are the scene's, checked by `tests/scene_catalogue.rs`.
  `check::dbus::check(&hir::Program, &dyn Introspect)` and
  `check::paths::check` touch the outside world, so they are not part
  of `compile`; `strand check`, the loader and the LSP run them, each
  bringing the bus as its own `Introspect` (`properties`, `methods`).
  A top-level `action` is `hir::Item::Action`.
  `check::names::{is_identifier, nameable, listed, naming}` are public
  for the language server's hover and completion.
  `lower::Program::members` (`names_member(record, member)`) are the
  members the program names anywhere, for a host that changes what it
  offers other programs by whether a member is named.
  A shared name in two files: `check::names::notes` gives the
  `check::same_name` note; `lower::Program::owner_key` (`module.Name`)
  keys a component's cells and reload identity, so a name becoming
  shared re-keys nothing (decisions.md m6-w2-language).
- **Types** (`strand_compiler::ty`): `Ty` is `Error` (already reported,
  accepted everywhere), `Any`, `Null`, `Unit`, `Prim`, `Opaque`,
  `Enum`, `EnumType`, `Record`, `List(T, keyed)`, `Optional`, `Async`,
  `Fn`, `Tuple` and `Union` (schema props only), with records and enums
  in a `TypeTable`.
- **Checking**: `strand_compiler::compile(&SourceMap) -> Compiled`
  checks every file as one program. It never panics and always returns
  a program: an unresolved name or ill-typed expression becomes
  `Ty::Error`, so one mistake gives one diagnostic. Scoping: components,
  surfaces, enums, `type`s, `fn`s, token sets, keyframes and custom
  services are global; top-level `state`/`let` belong to their file and
  are reachable elsewhere (and from the CLI) only as `file.name` when
  exported; tree `state`/`let` belong to their component, surface or
  list item. Checking is safe on small worker stacks
  (`stacker::maybe_grow`).
- **HIR** (`strand_compiler::hir`): the typed, resolved program the VM
  lowering, the reconciler and the LSP consume. Declarations are
  `DefId`s, local names `LocalId`s, every element has a program-unique
  `NodeIdx`; every `Expr` is typed. `Program::reference_at(file, offset)`
  serves go-to-definition, hover and rename; `Program::exports()` lists
  `file.name` paths for the CLI.
- **Lowering** (`strand_compiler::lower`): `lower(&hir::Program,
  &Schema) -> lower::Program`, plain data (`Send`), built by the
  compiler worker and handed to logic. Expressions are `Chunk`s of
  stack-machine bytecode (`Op`); the tree around them is a small
  structure the instantiator mounts (`lower::Node`).
- **VM** (`strand_compiler::vm`): `Value` is the dynamic value;
  token-bound values stay symbolic as `strand_scene::TokenExpr`, so
  `$surface.alpha(0.72)` reaches render unresolved. `Vm::eval` runs a
  chunk synchronously, tracking only what the run read; `Vm::handler` is
  a handler body as a future for `rt.spawn*`, cancelled by dropping.
  Errors are values: a failing binding keeps the prop's last good
  value, a failing handler's task returns an `Err`; calls nest at most
  `MAX_CALL_DEPTH` (200). A loop's frame is constant size.
- **Time-bound values**: `t`, `wave(…)`, `noise(…)` and a letter's
  `index`/`count` travel like tokens (`Value::Time`, "M4 vocabulary"
  above), and render evaluates them per node per frame. A time-bound
  value reaching logic (a comparison, condition, index, computing
  builtin, service argument, handler store) is an error value
  (`vm::value::time_in_logic`).
- **Services** (`strand_compiler::vm::ServiceHost`): the VM's only way
  to services; every method has a default.
  - `declare(rt, &lower::CustomService, types)` mounts a no-code service
    once at instantiation; `restart`/`stop` follow a reload that changed
    or removed it (only that one); `retype` hands a surviving service the
    new type table without restarting.
  - `keep(rt, service, Option<Duration>)`: a `graph` keeps history of
    the service's value, so it keeps running unseen, sampling at that
    period; `sampled` says whether its values are exact or polled.
  - `read(rt, service, field)` and `call(..)` (`fn` methods) must read
    through the graph (a `Signal<Value>` per field) so bindings depend on
    exactly that field. `read_keyed` publishes a list field as a keyed
    collection, so a `for` over it follows its `VecDiff`s.
  - `sources(rt, service, field)` and `action_writes(rt, service)`: the
    core nodes a read depends on and an action can write, declared
    before the first flush (a superset is fine).
  - `read_builtin`, `sources_builtin`, `acquire_builtin`,
    `release_builtin`: the runtime's own reads reach the builtin service
    even when the program declares one of that name.
  - `write(rt, service, path, value)` writes one `rw` leaf, so a
    concurrent sibling change is not overwritten; `write_item` writes a
    leaf below a keyed list item found by its schema `key`. Hosts tag
    writes and ignore their echo. The checker accepts an `rw` field only
    from a `state`/settings, a service or such an item.
  - `fetch(rt, service, method, args)`: an async method, run through a
    per-scope `rt.async_memo` created on first read; an argument change
    starts one fetch and cancels the superseded one; the value keeps its
    last result while pending. `call` is never asked for an async
    method. `fetch_reads` reads what a result depends on besides its
    arguments.
  - `action(rt, ActionTarget::{Service, Item(&record)}, name, args,
    ctx)` and
    `event(rt, service, event) -> EventQueue<Vec<Value>>`, the lossless
    queue an `on svc.event` listens to. A `from dbus` service's actions
    reach the custom service as `strand_services::custom::CustomAction`.
  - A top-level `action`'s command is not a host call: the VM queues an
    `ExecRequest`, logic takes them after each tick's diff
    (`Instance::take_execs()`), and the strand crate's `Execs`
    (`run/exec.rs`) runs each on its own thread, one run per action plus
    one queued; a call of `action name(params) -> T` is an `Async<T>`
    answered from the command's bounded stdout. The capture thread owns
    the command's end (SIGTERM to the group, `KILL_GRACE`, SIGKILL, then
    reap), so no signal reaches a recycled group. `rec.running`,
    `rec.started` and `rec.stop()` read and stop an action's run
    (`ActionRun`; `Instance::set_action_run`; decisions.md lockmon).
  - `acquire`/`release(rt, service)`: a reader count. Mounted components
    and the top level hold what their body reads; a surface holds its
    body's services only while shown; a parked bar lets go of
    everything; a `let` or `fn` holds nothing itself. The service starts
    on its first reader and stops 5 s after the last leaves; a host arms
    the stop with core's timers on `release` (no synchronous disposal in
    scope cleanup). `acquire_field`/`release_field` hold each field read
    directly, which is what a `#[store(stream)]` field's stream runs
    for. Boot holds `instantiate::HELD_ANYWHERE` (`clipboard.history`)
    for the program's life when read anywhere (spec.md 8.1).
  - Several service crates, one host: `Instance::new` takes one
    `Rc<dyn ServiceHost>`, and `ServiceHost` (with `Value` and the VM's
    dynamic types) stays in `strand-compiler`: `strand-services`
    depends only on `strand-core` and never sees `Value`. A service runs
    on its own thread and never holds logic-thread cells:
    `#[derive(Store)]` generates a `Send` patch type the service thread
    sends over a channel, and a logic-side store applies the patches to
    its cells. Writes, actions and methods go the other way as messages;
    a method's result comes back as a patch completing its `Async`. The
    binary adapts each store to `ServiceHost` (a `Memo<Value>` per typed
    field), and a composite host routes each call by service name to the
    one member serving it (`SchemaHost::real` answers the rest at
    defaults), unions `next_wake` and fans out `wake`. No edge from
    `strand-services` to `strand-compiler`.
  - `next_wake(rt) -> Option<SystemTime>` and `wake(rt, now)` let
    wall-clock services wake the host loop only at minute boundaries
    (seconds only while a binding shows them).
  - `vm::schema_host::SchemaHost` serves every schema service at its
    defaults: `SchemaHost::mock` for tests (clock fixed at 2026-10-05
    09:41:07 UTC) and `SchemaHost::real`.
- **Storage** (`strand_compiler::instantiate::Storage`): `from_env`
  (one IO thread for persist and settings), `in_dirs` for tests,
  `none()`. Persisted cells use the VM's codec (`vm::persist`) and a
  path qualified by instance (`Clock.open`, `TopBar[<monitor
  id>].expanded`, `Row[<item key>].open`); `Instance::reset(path)` is
  `@reset` and the overlay's `[reset]`. Settings fields are each their
  own signal (`prefs.compact`). The watcher gets the files from
  `Instance::settings_files()`/`settings_sources()`, reads a changed file
  on its own thread, and logic calls `Instance::reload_settings_with`.
- **Instantiation** (`strand_compiler::instantiate`): `Instance::new(rt,
  Arc<lower::Program>, Rc<dyn ServiceHost>, Storage)` mounts the
  program. `Instance::tick(now)` (or `flush()`) returns `Update { diff,
  errors, diagnostics, notices, kept }`: one diff per tick, the boot one
  starting with `SetTokens { transition: Instant }`, later tables
  `Default`. The table is the built-in base tokens under the chosen
  token set, with the `use palette` palette (else `material(seed:
  system.accent ?? #7aa2f7, dark: system.dark, contrast:
  system.contrast)`).
  - History: each boot and reload works out the series the `graph`s
    keep, keeps a ring per series across reloads, binds `Prop::Backlog`
    and calls `ServiceHost::keep`; `histories()`, `set_history_clock`
    and `seed_history` (a glitch report's replay) expose it.
  - Theming (`vm::theme::ThemeHost`, kept across reloads):
    `material(image:)` returns an `Async<Palette>` holding the last
    image palette while one is quantised; `Instance::theme_files()` and
    `theme_files_changed` serve the watcher. A `use palette` that fails
    or has no value takes the last palette (`palettes/palette`), else
    the built-in.
  - `Instance::set_text(path, text)` is `strand set`; `get`/`set(path)`
    read and write exported `file.name` values, type-checked.
  - Each bound prop is one watched memo folding the base binding and its
    `when` blocks in source order (later wins); `if`/`match` are effects
    swapping branch fragments.
  - `for` keeps one item per key with its own value cell, following a
    keyed source's diffs (else `rt.keyed_memo`); a `.filter`/`.map`/
    `.take`/`.sort_by` chain on a keyed source, or a `let` holding one,
    is one incremental view, so one new notification is one diff from
    the service to the scene. Reads like `xs.len` are `Op::Keyed`,
    never building the list.
  - `await` on a pending value with nothing to wait on is an error
    value, never a silent null.
  - A `bar` is a keyed instance per `screens.all` item its `screens:`
    picks, keyed by `Screen.id`, with `screen` in scope. A bar whose
    monitor leaves is parked (nodes removed, scope frozen, services
    released) and returns as it was, or is dropped by
    `Instance::forget_screen(id)`. Every surface gets `Prop::Name`.
  - A surface's `on show`/`on hide`/`on dismiss` are always live; the
    rest of its body mounts when first shown and is frozen while hidden.
    The instance, not render, sends `show`/`hide`.
  - Runtime errors are located values (`RuntimeError`), the span the
    innermost failing operation's. `Instance::origin(node)` maps a scene
    node to its element. `Instance::freeze(&err)` suspends the faulting
    component and outlines it in red; `thaw` and reloads clear it. A
    top-level fault is outlined and nothing is frozen.
  - The host loop: `Instance::step(now, wall) -> (Update, Wake)`. The
    `strand run` logic thread feeds `screens` from the surface layer's
    monitor hooks, maps render's input to `event(node, name, args)`
    (delivered to the innermost element with a handler), `set_flag`,
    `set_size`, `set_box` and `write(node, prop, value)` (`<->` writes,
    type-checked), calls `step`, sends the diff, and sleeps until the
    logic deadline, the wall deadline (a realtime timer), input, a
    monitor hook or the wake hook. These take scene `NodeId`s from
    `Renderer::hit`. `SceneMirror` renders diffs as text for snapshots.
  - Live reload: `Instance::from_build` mounts a build;
    `Instance::reload(&Build) -> Report` mounts the new program beside
    the old, which hands over by reload key (the scope's path in the
    instance tree plus the node's `Sid`): scene nodes keep their ids,
    state cells are reparented (a changed default adopted only where the
    value still held the old one), handlers with an unchanged hash keep
    their tasks, timers rescale. While a `lock` is shown a build that
    changes a lock is not committed (`LockDeferred`), and every later
    build waits too until `lock_shown()` turns false (decisions.md
    wave2-runtime). `reload_hard` mounts afresh, keeping the old tree's
    service holds until the end of the next tick so no service stops.
  - Nodes outside the program (the error overlay): `external_create`,
    `external_set`, `external_remove`, `is_external`; they survive
    reloads, and input on them is the caller's.

- **M4 additions**.
  - Shaders (`check/shaders.rs`, feature `shaders`): a `shader "x.wgsl"`
    file is registered with the watcher (`Role::Shader`) and checked
    with naga as `strand_scene::shader::PRELUDE` followed by the file
    (diagnostics point into the file, the prelude's lines subtracted):
    one `@fragment`, no `@vertex`, each `@group(1)` uniform a `u_*`
    `f32` or `vec2`–`vec4<f32>` matching a prop (a prop the file lacks
    is an error; a uniform no prop sets is a warning and zero-filled).
    `Prop::Shader` carries
    the checked text and its slots, so render never re-reads the file;
    a failing save keeps the last good build. Without `shaders` the node
    draws nothing.
  - List windows: a `for` that is a `list`'s direct child mounts only the
    rows of the list's window, row state kept by key;
    `Instance::set_list_window(list, first, count)` (from
    `ToLogic::ListWindow`) mounts and unmounts rows and marks those ops
    `window` (decisions.md m4-lists-w1).
  - Drag and drop: the instance sets `Prop::Drag`, `Prop::Accepts`,
    `Prop::DropRows` and `Prop::NotRow`; a `Drop` from a Strand source
    is delivered with its `drag:` value, an external one as a `Drop`
    record.
  - The lock: only an `auth` success unlocks; the runtime then writes the
    lock's `open` false, and a config write of `false` while locked is
    ignored with a warning. `lock_shown()` follows `ToLogic::LockState`,
    the compositor's state, not the `open` prop:
    `Instance::set_session_lock(SessionLock::{Locked, Finished,
    Unlocked})`, which the binary maps from strand-surface's
    `LockState` (the compiler does not depend on strand-surface;
    decisions.md m4-lock-w1). Before any report since the lock last
    opened, `lock_shown()` is true while a `lock` on the scene is not
    `open: false`. A lock mounts one instance per monitor its
    `screens:` picks (`focused`, or a pick no monitor matches, mounts
    one on the focused monitor), sharing one
    lock-level `Env` of `state`s (one password, one attempt) and one copy
    of its timers and handlers (`instantiate::lock::lock_logic`); that
    `Env` is a fault bound, so a fault freezes only its instance
    (`Instance::fault_nodes`). An `input` bound to a shared `state`
    carries `Prop::Twin`, so the `Router` shares its text and caret
    across monitors. Checks: `check::lock_twice`, `check::lock_prop`
    (`layer`, `anchor`, `keyboard` on a lock), `check::lock_state_screen`,
    `check::lock_no_auth`, `check::lock_popup`, `check::secret_flow`
    (where a password's text may leak), and `check::lock_no_helper`,
    made by the binary's `strand check` (`check.rs`) when
    `strand_auth::default_helper` finds no helper, not by the compiler,
    the LSP or reload (decisions.md lockmon).
  - `compositor-rules`: `blur_rules(&Build)` (`strand/src/rules.rs`)
    lists the surfaces that blur, by namespace: a layer surface's own
    `blur` gives `blur`, a nested popup's `blur_popups` on its layer.
  - Checks live in per-concern submodules (`check/surfaces.rs`,
    `effects.rs`, `lock.rs`, `shaders.rs` and the rest); `attach` and
    `scrim` are allowed only on `popup` and `panel`.

- **Formatting** (`strand_compiler::fmt`): `format(src) -> Result<String,
  FormatError>`, the one formatter behind `strand fmt` and LSP
  formatting. It never changes a file's meaning: syntax errors give
  `FormatError::Syntax`, and a result whose tree differs from the input's
  up to spans is `FormatError::Unstable`. The output is idempotent,
  keeps comments and line breaks, and ends with one `\n`.

### Config files

`strand_compiler::source::find_files(dir) -> io::Result<Discovery>`
defines a config's **`.strand` module set**, and only that: `.strand`
files at most `MAX_DEPTH` (3) directories below the config directory,
dot-names skipped, symlinks followed, each file and directory claimed
once at its shallowest path; unreadable directories and dangling
`*.strand` links go in `errors`, too-deep directories holding `.strand`
files in `too_deep` (`strand check` warns), and `dirs` lists every
directory scanned by canonical path, link targets included. The loader
and `strand check` both call it, never a reimplementation, so they never
disagree on the module set. `strand-watch` does not depend on
`strand-compiler`: the binary calls `find_files` and hands the watcher
plain paths through its constructor and a `rescan` callback. `strand
check <file>` checks the file with the rest of its config (the default
config directory if the file is in it, else the file's own directory)
and reports only diagnostics with a label in that file.

It is not the watch set. Per design.md ("Change sources") the watcher
also watches `.wgsl` shaders, settings TOML, the wallpaper and other
referenced paths (collected by the compiler, not this scan), and the
canonical targets of linked files and directories.

### `strand-text`

Request/response over a channel on the text worker: `TextRequest { key,
text, style, max_width, scale }` → `TextLayout` (size, glyph runs,
caret stops). `TextStyle` (font, line height, alignment, ellipsis,
`max_lines`, `spans`) is documented in `strand-text/src/lib.rs`; weight
picks a face by CSS font matching and synthesises bold only for 600+ on
a face lighter than 600. Render draws the last delivered layout.

Contracts render relies on:

- Each `TextLayout` carries the `AtlasUpload`s for glyphs rasterised
  while producing it (render applies them to its atlas mirror in
  arrival order), leases on the pages it uses (the worker never recycles
  a page a live layout draws from), and `atlas_pages` (the scale's live
  pages, so the mirror drops trimmed ones). Page generations are unique
  per process.
- The request channel also carries, in order, `cancel(key)` (skip a
  still-queued request) and `drop_scale(scale)` (free a scale's atlas;
  render drops its mirror pages at the same time). The worker drains
  its queue and folds in cancels before shaping.
- A panicking request does not kill the worker: it starts a fresh engine
  and answers with a layout whose `is_reset()` is true, on which render
  drops its mirror and every layout and re-requests. `reload_fonts()`
  (watcher's `CacheKind::Fonts`, via `Renderer::fonts_changed`) does the
  same with key 0. `Renderer::icons_changed` is the icon counterpart
  (`strand_icons::invalidate`, decodes dropped, icon surfaces repainted).
- Hostile-input bounds: `AtlasConfig::max_bytes` per scale (8 MiB;
  glyphs that do not fit are skipped and the layout `is_incomplete()`,
  which render re-asks a bounded number of times), `MAX_FONT_PX` (512),
  `MAX_TEXT_BYTES` (64 KiB) per request.
- `TextLayout::carets` lists every cluster boundary per line (byte
  offset, logical x), from which render draws an `input`'s caret and
  selection and hit-tests clicks.
- `TextWorker::waker()` hands out the render loop's waker for render's
  other workers (image decoder, tooltip timer), so the binary wires one
  waker. Dropping the worker discards its queue.

### `strand-surface`

Owns the Wayland connection (smithay-client-toolkit): layer-shell
surfaces per output, `xdg_toplevel` windows and `xdg_popup`s, a 2–3
buffer shm pool per surface with buffer age, damage, opaque regions,
fractional scale + viewporter, frame callbacks only while
`Painter::wants_frame`, `wp_presentation` timing, output hotplug and
input as `InputEvent`s. It creates and updates surfaces from the
`SurfaceSpec`s and `SurfaceChange`s render reports (namespace
`strand-<Name>`, anchor, margin, size, exclusive zone, keyboard
interactivity, outputs from `screens`, mapping by `open`), and applies
compositor-animated poses.

Interface (main thread; `SurfaceManager<H>` owns the calloop
`EventLoop` and the connection):

- `trait SurfaceHost: Painter` (`src/manager/mod.rs`) is what the
  manager calls: `Painter`'s `paint`/`wants_frame`/`opaque_region`/
  `blur_region`/`surface_pose`/`input_region`, plus hooks with no-op
  defaults for surface lifecycle (`surface_attached`,
  `surface_entered`, `surface_configured` once per wakeup right before
  the first paint at that size, `surface_detached`, `surface_placed`),
  monitors (`monitor_added/changed/removed/forgotten`, the last 30 s
  after an unplug), frames (`frame_deadline`, `frame_dropped`), input
  (`input`, `cursor`, `press_serial`, `keyboard_state`,
  `take_clipboard_asks`), drag and drop, windows (`window_state`,
  `window_closed`), `compositor_caps`, `gpu_release`, `before_lock` and
  `lock_changed`. The binary implements it on a wrapper around
  `Renderer`.
- `SurfaceManager::connect(host, Config)` / `with_connection`;
  `dispatch(timeout)` blocks while idle. Other sources go on
  `loop_handle()`; their callbacks get `&mut State<H>` and call
  `apply_surface_change(node, change)` for each `take_surface_changes()`
  entry, then `poll()` or `repaint(surface)`. `repaint_handle()` gives a
  `Send` `RepaintHandle` (`Request::{Repaint, RepaintAll, Poll}`);
  `take_input()` creates the `InputEvent` receiver (nothing is queued
  before it).
- **Frame pacing.** After a buffer commit a surface paints again only
  after that frame's callback or its presentation feedback; changes
  meanwhile coalesce. Exceptions: a new size or scale paints at once,
  and a frame unanswered for `THROTTLE_GIVE_UP` (1 s) no longer blocks
  (`Stats::throttle_given_up`), so a surface the compositor does not
  show paints at most once a second (decisions.md, m4-audit round 7).
- **Holds.** While `wants_frame` is false and `frame_deadline` is
  `Some`, no buffer is committed; a timer at the deadline asks again.
- `FrameClock` (`now`, `presented`, `discarded`, `predict`) is fed by
  `wp_presentation`; `PresentationClock` is the real one, `FakeClock`
  the injectable one; `predict` becomes `PaintTarget::time`.
  `PresentationClock::set_clock_id` also sets
  `strand_scene::backlog::set_presentation_clock`.
- **Monitors.** `MonitorId` is `"make | model | description"` without
  wlroots' trailing ` (<connector>)` (a duplicate gets ` #2`, in plug
  order); `Screens::Named` matches it or the connector. `SurfaceId`s are
  stable per (node, monitor) while the monitor is remembered. The binary
  forwards the `monitor_*` hooks to logic as the `screens` service.
  `screens: focused` is one layer surface created without an output.
  (screens-focus) `SurfaceSpec::follows_focus` (`Prop::Screens` is the
  compiler's `FOLLOW_FOCUS` keyword, given to an instance of a `screens:`
  that evaluates to `focused` and to the lock's single instance) marks one
  whose `screen` is the focused monitor: `State::set_focused_monitor`
  pins it (and moves an open one), which the binary calls from
  `ToMain::FocusedScreen`. Any other `screens: focused` surface (a
  `panel` or `osd` with no `screens:`, or `focused` written) is the
  compositor's to place, focus set or not. An `osd` has an empty input
  region.
- **Placement** (`strand_scene::placement`, clamped to ±`MAX_LOGICAL`).
  A spec's `overhang` grows the buffer and moves the margins out,
  keeping margin + exclusive zone; the input region is the box inside
  (`SurfaceInfo::input_region`). A painter may replace it
  (`Painter::input_region`, applied at each commit,
  `SurfaceInfo::painted_input`): render does so for a `trigger: hover`
  popup that draws nothing itself and for a content-sized surface drawn
  short of its buffer. Render keeps the overhang even on centred axes
  and no larger than the margin on an anchored side, so a buffer never
  passes its output's edge. `LayerConfig::fit` clamps a layer box to
  its output; the host passes the output's size
  (`Renderer::set_surface_bounds`) and render holds a content-sized
  surface's frame for the configure at its new size
  (`Renderer::set_resize_wait`, `RESIZE_WAIT`). A GPU-presented surface
  is held too: the manager sets its new size without a commit and render
  asks the GPU thread to present the last frame again
  (`GpuRequest::Commit`), the held frame waking the loop through
  `GpuHost::wait`. Spec changes render makes while the manager calls in
  are reported by `Renderer::has_surface_changes`; the host pings its
  loop so they apply at once.
- **Click-away catchers** (`SurfaceInfo::click_away`): a transparent
  layer surface on the same layer and output with the surface's box
  holed (`LayerConfig::box_in`), and a whole-output one on every other
  output showing no surface of that node.
- **Keyboard.** One `wl_keyboard` per seat with xkbcommon keymaps and
  repeat, delivered as `InputEvent::Key` on the focused surface. Each
  keymap is also compiled with `xkbcommon` 0.8 (the release SCTK links;
  `seat::compile_keymap`) into `seat::LatinKeys`, so a non-ASCII key
  carries `KeyInput::latin` and `KeyInput::shortcut()` matches Ctrl+V
  under `ru` as `v` (`name`, what `on key(k)` sees, is unchanged). The
  focused keyboard's lock keys, layout names and modifiers are reported
  as `KeyboardState` through `SurfaceHost::keyboard_state` (`None` when
  none of ours has the keys); the host forwards them to
  `strand_services::keyboard::focused`.
- **Windows** (`NodeKind::Window`, `manager/window.rs`): an
  `xdg_toplevel` through sctk's `XdgShell` (with `xdg-decoration`),
  built from `placement::window_config` (a `max_*` below its `min_*`
  is raised, since `invalid_size` ends the connection); window geometry
  is the box inside the overhang. Which configure sizes are taken is
  `placement::window_kept_size` (with `Toplevel::echoes`); state goes to
  `SurfaceHost::window_state`, and `close` is `InputEvent::ClickAway`
  after `window_closed`. `State::window_ask` sends move/resize with the
  press serial, then hands the host an `InputEvent::PointerLeave` of the
  window (the press is the compositor's grab now), and the rest only
  where `wm_capabilities` lists them; asks before the window exists wait
  in `State::window_asks`. An unplugged output leaves its windows alone
  (the compositor moves them). Windows (and popups in them) are never
  placed (`surface_placed`).
- **Popups** (`NodeKind::Popup`): an `xdg_popup` nested in a mapped
  surface of `SurfaceSpec::parent` (the one last pressed), positioned by
  `placement::popup_config` with slide and flip; the gap is the popup's
  margin on its anchor side, 6 px only while `margin_set` is false (a
  tooltip 4; spec.md 5.1). Size or anchor changes reposition it. The
  manager shifts the box inside its parent's output itself
  (`placement::popup_shift`); a configure elsewhere than asked counts in
  `Stats::popups_moved`. A popup in a popup opens beside its anchor
  (`PopupConfig::aligned`, right unless `anchor:` says otherwise).
  Grabs: a popup grabs only when the press that opened it
  (`State::set_popup_cause`, from `SceneDiff::press`) is the seat's last
  press and within `GRAB_WINDOW` (500 ms), never under a non-grabbing
  parent popup, never for `trigger: hover` or a tooltip
  (`Stats::grabs`). Only a grabbing popup dismisses other grabbing
  chains, makes its layer surface `exclusive`
  (`State::holds_keyboard_for_popup`) and takes the keys (with
  `KeyboardEnter`/`KeyboardLeave` as focus moves). `popup_done` is sent
  as `InputEvent::ClickAway` before the popup and its children are
  destroyed innermost first; it is not shown again until its spec
  closes. A popup is placed again whenever its parent's origin changes.

**Compositor protocols** (`caps.rs` and `manager/`):

- *Capabilities* (`caps.rs`): the manager binds `wp_alpha_modifier_v1`,
  `wp_single_pixel_buffer_v1` and `ext_background_effect_manager_v1`
  when offered, notes `ext_session_lock_manager_v1`,
  `wl_data_device_manager` and `hyprland_*` globals, and calls
  `SurfaceHost::compositor_caps(&CompositorCaps)` at the end of its
  first wakeup (before any configure) and on every change
  (`background_effect` only once its `capabilities` event names blur).
  While the compositor does not blur, the host sends logic
  `ToLogic::BlurFallback` (`caps::blur_missing`) per surface whose blur
  regions change, which logic turns into `strand watch` notices.
- *Poses* (`manager/pose.rs`): right after `paint` the manager reads
  `Painter::surface_pose` and sets what changed as pending state (alpha
  modifier, viewport destination, margins via `placement::posed_margin`)
  on that frame's commit, or a bare commit. `SurfacePose` keeps the
  top-left corner; render's `strand_render::pose` decides what a
  placement can take. `State::clear_pose` drops a pose for the GPU
  hand-off. Render holds `Removed`/`open: false` until an exit settles.
- *Origins* (`manager/origin.rs`): `SurfaceHost::surface_placed(surface,
  (x, y))` gives a surface's buffer origin in the global logical layout
  when it changes (layer surfaces by `placement::arranged_area`, the
  wlroots order with our own exclusive zones only; popups from their
  configure). The host uses it for the tray's click point
  (`strand_services::tray::set_click_point`); a tray action takes that
  point only from a handler of an input event
  (`tray::set_input_probe(strand_compiler::vm::in_input_handler)`,
  `vm::INPUT_EVENTS`), else (0, 0).
- *Solid surfaces and scrims* (`solid.rs`, `manager/scrim.rs`): a
  single-pixel buffer scaled by the viewporter (shm fallback). A panel's
  `scrim` is a layer surface over the output's usable area one layer
  below the panel (a `top` panel with a scrim is raised to `overlay`);
  it doubles as the click-away catcher when the panel has one. A
  popup's scrim sits on its root surface's layer and takes no clicks.
- *Idle inhibition* (`manager/idle.rs`): `SurfaceSpec::inhibit_idle`
  gives each of the node's Wayland surfaces (lock surfaces included) a
  `zwp_idle_inhibitor_v1` once it is shown (the compositor has answered
  a frame: a frame callback or presentation feedback; Hyprland 0.57
  counts a layer's only once its alpha is above zero, decisions.md
  hypr-057), or when turned on in place; destroyed when turned off and
  before the surface goes (`SurfaceInfo::inhibits_idle`).
- *Fillets*: placement puts an `attach`ed box at its margin from that
  edge (0 while unset); render's `fillet.rs` grows the overhang along
  the edge and lets a clip group's fillets through
  (`Flattener::let_fillets_through`, `hold_to_box`).
- *Blur ladder* (`blur.rs`, `manager/effect.rs`): after each paint
  `Painter::blur_region` becomes a `wl_region` (`blur::region_rects`)
  sent with `ext_background_effect_surface_v1.set_blur_region` on that
  commit only when it changed; nothing while the compositor cannot
  blur, and resent when it starts (`SurfaceInfo::blur_region`,
  `Stats::blur_updates`). The binary logs one warning on the first
  frame that wants blur on a compositor that cannot
  (`caps::blur_fallback_reason`) and sends it as a `ToLogic::Notice`,
  which logic keeps for later `strand watch` subscribers.
- **Session lock** (`manager/session_lock.rs`). `State::lock()` asks
  `ext_session_lock_v1`; the host hears `lock_changed(LockState)`
  (`Locked`, `Finished`, `Unlocked`). Each lock instance is its own spec
  pinned to its output; every output with a live instance gets that
  instance's `Role::Lock` content surface (painted by the host like any
  other; `LOCK_FALLBACK_NODE` when there is no `lock` spec), the rest a
  manager-painted solid in `set_lock_color`'s colour, outputs plugged in
  while locked included. `State::lock_contents()`,
  `lock_content_on(output)` and `lock_content()` (the focused output's,
  else the first) name them. `State::lock_instance_failed(node)` puts an
  instance's output back on the solid for the session; the binary calls
  it for a faulted, unmounted or never-drawn instance (not drawn
  `INSTANCE_FIRST_FRAME`, 3 s, after its configure while another has).
  A spec removed while locked leaves its content in place until a new
  spec wants that output. Keyboard focus is the compositor's; a focus on
  a solid is delivered as the focused output's content's (else the
  first's). `before_lock()` runs just before the request (the binary
  takes pre-lock stills there).
  Invariants: `State::unlock(strand_auth::UnlockToken)` is the only way
  to unlock; a spec closing or going away never unlocks (a closed spec
  re-arms, so an open spec locks again only after it closed);
  `finished` without `locked` is a diagnostic and the lock counts as
  not shown; `Finished`
  after `Locked` is answered with `unlock_and_destroy` and a new lock is
  asked for once per session; nothing locks until
  `State::enable_session_lock()` (else `LockError::NotEnabled`), which
  the binary calls only once `auth`'s tokens reach `unlock`; a token
  given while pending is spent when `locked` arrives; without the
  protocol (`session_lock_supported()` false) an open spec gets
  `LockError::Unsupported` and the host hears `Finished`. Session-lock
  tests run only in the lock VM (`scripts/lockvm/scenarios/`).
- *Drag and drop* (`src/dnd.rs`, as `manager::dnd`): a
  `wl_data_device` per seat yields `InputEvent::Drag*` (external
  payloads read through calloop without blocking, at most
  `MAX_DROP_BYTES`). Host hooks `drop_accepted`, `drag_source`,
  `drag_data` and `drag_image` drive acceptance and outgoing drags,
  which offer `dnd::exports_of` types plus a private
  `dnd::STRAND_MIME` type (another Strand process's drag is read
  through its exports). `State::carrying_drag()`.
- *Copy and paste* (`src/selection.rs`, `manager/pipes.rs`): after a
  key press reaches the host the seat drains
  `SurfaceHost::take_clipboard_asks()`. A copy is a `wl_data_source`
  with the text types and a private `STRAND_SELECTION_MIME`; a paste of
  our own selection is answered from memory, another program's read
  through calloop (`MAX_PASTE_BYTES` 1 MiB, `PASTE_READ_TIMEOUT` 5 s) as
  `InputEvent::Paste`. Neither runs on a lock surface nor while a lock
  is asked for or held (`State::clipboard_shut`).

Module map (`src/manager/`; the public API is re-exported from
`lib.rs`): `mod.rs` (`SurfaceHost`, `Config`, `SurfaceError`,
`Request`, `RepaintHandle`, `Stats`, `SurfaceInfo`, `State`,
`SurfaceManager`, node reconciliation), `layer.rs`, `window.rs`,
`popup.rs`, `catcher.rs`, `outputs.rs` (hotplug, identity, expiry),
`commit.rs` (geometry, paint, commit, callbacks, deadlines, feedback),
`seat.rs`, `protocols.rs`, `session_lock.rs`, `scrim.rs`, `pose.rs`,
`effect.rs`, `origin.rs`, `idle.rs` and `pipes.rs`; beside it in
`src/` but part of it: `dnd.rs`, `selection.rs` and `gpu_handoff.rs`.

### `strand-gpu`

The GPU backend is in every build and falls back to the CPU
(decisions.md m4-owner, gpu-first). vello_cpu into `wl_shm` draws every
still surface; the GPU draws a surface while it animates a large area,
and shaders. The binary starts the thread as the shell starts and keeps
it for the process; nothing in this crate runs, and no Vulkan library is
mapped, before `Gpu::spawn`.

**Boundary.** `strand-gpu` depends on `strand-scene`, wgpu 30
(`vulkan`, `wgsl`, `std`; no default features), vello_gpu 0.3 (`wgpu`,
`std`; no `text`: glyphs arrive as atlas images), vello_common 0.3,
raw-window-handle 0.6 and pollster; on no other Strand crate and no
Wayland crate. Its interface (`strand-gpu/src/lib.rs`):

- `Gpu::spawn(waker, GpuOptions) -> Gpu` starts the `strand-gpu`
  thread, which creates the instance, adapter and device, then draws a
  warm-up frame offscreen exercising every pipeline a first frame would
  build and records the present blit for both swapchain formats, before
  answering `Ready` (failure: `Unavailable`). `Gpu::send(GpuRequest)`
  never blocks; `Gpu::try_recv()` is drained when the waker fires on
  the main loop.
- Shutdown: dropping the `Gpu` (or `GpuRequest::Shutdown`) drops every
  GPU resource and the thread ends; while the shell runs the binary
  joins it after `Exited`, never blocking on a device drop. At exit,
  `run` calls `GpuHost::shutdown(GPU_SHUTDOWN)` (3 s) before the
  `SurfaceManager` drops, since a presenting thread holds the display
  and lent `wl_surface`s; a thread still running then leaks the manager
  (`std::mem::forget`). This is how "no swapchain outlives its
  `wl_surface` or display" holds at exit (decisions.md m4-audit).
- `GpuRequest`: `Attach { surface, handles: Option<RawHandles>, size,
  scale, opaque }` (no handles: readback only), `Resize`, `Release`,
  `Commit` (present the last frame again so pending state applies),
  `Frame(Frame)`, `Pass(PassFrame)` (a shader pass for a surface the
  GPU does not draw, drawn offscreen and read back; `input` is the CPU
  pixels a `Content` or `Backdrop` pass reads, `then` chains passes),
  `Shutdown`.
- `GpuReply`: `Ready(AdapterInfo)`, `Unavailable(GpuError)`,
  `Attached { surface, mode: GpuMode }`, `Released`, `Presented { at,
  waited, ran, .. }` (`ran` from timestamp queries; the thread never
  waits for a presented frame's GPU work), `Pixels`, `PassPixels`,
  `Failed { surface, key, error }` (the device stays up), `Lost`,
  `Exited`. Every frame and pass is answered, a panic included.
- Hangs: a readback waits at most `HUNG_AFTER` (10 s); past it, or on
  a wgpu panic after a driver reset, the request answers `Failed`, then
  `Lost` and `Exited`, and a hung device is leaked rather than dropped.
  Render never reruns a shader file answered `Failed` with kind `Lost`
  in that process (`ShaderResults::lost`), so a looping file leaks at
  most one device. The thread keeps at most 64 pipelines and 16 pass
  readback buffers, LRU.
- `Frame` (`surface`, `id`, `size`, `scale`, `ops`, `uploads`,
  `retire`, `clear`, `snapshot`) is what render lowers its display list
  into (`strand-render/src/renderer/backend.rs`); the GPU thread builds
  the vello_gpu scene from the ops. `Upload`s (atlas pages, cached
  pixmaps; masks are always CPU-rasterised) live on the GPU until a
  frame lists them in `retire` or the device drops. `Op::BlurredRect`,
  `Op::Backdrop` and `Op::Snapshot` draw on the GPU what the CPU would
  have uploaded; `Op::Backdrop`'s `key` lets an unchanged backdrop
  reuse the last frame's pixels, and its textures come from a
  per-surface `pass::Pool` so steady animation allocates nothing.
  `FrameSnapshot` is `None`, `Take` or `Keep` (the crossfade
  snapshot). The op and field contracts are documented on `Op` and
  `Frame`.
- `Readback` is `Bgra8Unorm` premultiplied rows (`wl_shm` ARGB8888
  byte order), padded to 256-byte rows, with its stride.
- `record_frame_phases`/`take_frame_phases` serve the animation bench
  (`strand-render/tests/anim_bench.rs`); off by default.

**Thread.** One thread and one device per process, shared by every
surface, started as the shell starts (`BackendChange::Start`) or on
first demand after a failure. The adapter is requested without a
surface (readback works whatever the WSI can do) and with
`PowerPreference::LowPower` (on a hybrid laptop, the integrated GPU
that composites). A software adapter (`DeviceType::Cpu`, lavapipe)
counts as no device unless `STRAND_GPU_SOFTWARE=1`, which CI's lavapipe
tier sets beside `STRAND_REQUIRE_GPU=1`. The thread waits only on its
channel and on presents, never on a timer.

**Promotion** (`strand-render/src/promote.rs`, a pure per-surface state
machine; decisions.md gpu-first). `Renderer` reports
`take_backend_changes() -> Vec<BackendChange>` and
`take_gpu_requests() -> Vec<GpuRequest>`, which the binary carries out
after every dispatch (starting the `Gpu` when requests come and none
runs), and hands every reply to `Renderer::deliver_gpu(GpuReply)`.

- Promote: when a device can exist, a surface goes to the GPU at the
  first large frame (≥ 0.2 Mpx of CPU damage) of an animation: a frame
  with a spring in flight, or the third large frame in a row 250 ms
  apart with no spring (a clock). A lone large frame is not an
  animation, nor is the frame a last spring rests on. Shader passes
  need the device without promoting.
- A promoted surface stays promoted for the rest of its life
  (decisions.md gpu-stay): `Promotion::frame` answers only whether this
  frame promotes, and nothing idle or small takes it back. Only a real
  failure does (`Promotion::force_cpu`, `BackendChange::Demote`): a frame
  that cannot be drawn (`GpuReply::Failed`), a lost device, an attach a
  failure overtook. A promoted surface that paints nothing asks for no
  wake, no frame and no request. The `Release`/`Released`/`take_back`
  machinery serves those failures and a destroyed surface; no route
  releases a live, healthy surface's swapchain.
- The device is kept for the process. `strand run` calls
  `Renderer::warm_gpu()`, or `Renderer::gpu_absent(reason)` when
  `strand_gpu::device_possible(&GpuOptions)` is false (no render node,
  no software adapter accepted): the device is then `Absent` and never
  asked for, as after an `Unavailable` of kind `NoAdapter`/`Software`.
  Other failures retry once at `promote::Device::retry_at`, then on
  demand at most every 30 s.
- A failed start or lost device is `GpuStatus::Unavailable`; promoted
  surfaces go back to the CPU at once with a full repaint. After
  `LOST_CAP` (3) `GpuReply::Lost`s the GPU is off until restart
  (`promote::GPU_OFF`); the count survives reloads.
- The Vulkan loader filter: before any thread starts (a
  `.init_array.00101` constructor) the binary sets
  `VK_LOADER_DRIVERS_DISABLE` from `strand_gpu::loader_disable(..)`:
  software drivers unless accepted, and the drivers of vendors with no
  render node (`strand_gpu::render_vendors()` from sysfs; nothing kept
  out when unreadable). It appends to an inherited list and sets
  nothing when `VK_DRIVER_FILES`, `VK_ICD_FILENAMES` or
  `VK_LOADER_DRIVERS_SELECT` is set. Spawned programs get the variable
  as inherited (`strand_services::child::set_own_var`).
- Under `reduced_motion` shader time stays 0 (`TimeContext::frozen`).

**Backends** (`strand_scene::Backend`, set by
`Renderer::set_backend(surface, Backend)` once the GPU thread answers):

- `Cpu`: vello_cpu into the surface's shm buffer.
- `GpuPresent`: the GPU thread presents through wgpu's WSI, chosen when
  the surface's capabilities offer `PreMultiplied` alpha (or it is
  opaque) and a non-sRGB `Bgra8Unorm`/`Rgba8Unorm` format; `Fifo`, full
  damage.
- `GpuReadback`: rendered offscreen and copied into the shm buffer by
  the main thread, committed as a CPU frame; for lock surfaces, a
  surface an ending thread holds, and compositors the WSI cannot present
  to (a pixman sway with ANV).

Negotiation: render reports `Promote(surface)`; the binary sends
`Attach` with `State::raw_handles` (none for readback-only and lock
surfaces); the reply's `GpuMode` becomes the backend. The CPU keeps
drawing until the reply: promotion never stalls a frame.

**Frames.** Readback: `Painter::paint` copies in the last frame's
pixels and sends the current one, so frames run one behind and the main
thread never waits on the GPU; `Pixels` marks the surface via
`Renderer::deliver_gpu`. Present: the manager does not call `paint`;
`Presented` is the frame callback and the binary sends
`Renderer::paint_gpu(surface, at) -> Option<Frame>`. One frame in
flight per surface.

**Surface hand-off** (`strand-surface/src/gpu_handoff.rs`). One
`wl_surface` moves between shm and the WSI; it is never recreated.

- `State::raw_handles(surface) -> Option<RawHandles>` lends the
  display and `wl_surface` (none for a lock surface, which is never
  handed off). From then until `take_back`, a destroy goes through
  `gpu_release` and the manager attaches no buffer (bare configure acks
  still go), since a WSI with explicit sync owns its buffers
  (decisions.md syncobj-lend). Needs `wayland-backend`'s
  `client_system` (strand-surface's `gpu` feature, on by default), which
  switches every crate to libwayland-client; the CPU-only build keeps
  the Rust backend.
- `State::hand_off(surface)`: the GPU thread becomes the only committer.
  The manager stops attaching, frame callbacks and `paint`, but still
  handles configures (`surface_configured` → host sends `Resize`) and
  sets pending state without committing; `State::sync_presented`
  (after each frame sent) sets that frame's opaque, input and blur
  regions (decisions.md gpu-regions). Poses are painted into the GPU
  frames, not delegated.
- `State::take_back(surface)` after `Released` (or an `Attached {
  Readback }` for lent handles, or the thread's end) paints a full shm
  frame and resumes callbacks and pose delegation. Between `Release` and
  that commit nothing commits the surface.
- A handed-off surface the manager must destroy goes through
  `SurfaceHost::gpu_release`; the `wl_surface` lives until `take_back`,
  so the swapchain is always dropped first.
- The binary's side is `strand/src/run/gpu.rs` (`GpuHost::pump` after
  every dispatch). A new `Gpu` starts only once every dropped thread has
  ended; then the binary calls `malloc_trim(0)` and mimalloc's collect.

**Shaders and effects.**
`strand_scene::effect::Effect::Shader(ShaderPass { code: ShaderRef,
uniforms, input: ShaderInput })`. `ShaderRef` is `Bundled(Bundled)`
(nine variants for design.md's eight effects; WGSL in
`strand-gpu/src/bundled.rs`; knobs in decisions.md m4-gpu-effects) or
`File(Arc<ShaderCode>)`. `ShaderInput` is `None` (draws in its box),
`Content` (a `filter:` pass reads its subtree) or `Backdrop`. Render
packs `uniforms` each frame in slot order and buffer units;
`ShaderCode::packed` lays slots out densely, and the GPU thread copies
each slot to its own binding. A bundled pass's reach is
`Bundled::reach(uniforms, scale)`; a file's pass draws inside its box.

The ABI (`strand_scene::shader::PRELUDE`, prepended by the checker and
the GPU thread alike via `ShaderCode::module()`): Strand supplies the
vertex stage over the node's box; the file supplies one `@fragment`
taking `StrandVertex` (`uv` 0..1, `pos` in buffer pixels) and returning
premultiplied `vec4<f32>`. `@group(0)` is Strand's (`strand: Strand`
with `time`, `size`, `scale`, `pointer` or -1; `strand_input`, 1×1
transparent for `None`; `strand_sampler`). `@group(1)` holds the file's
`u_*` uniforms, each its own `var<uniform>` of `f32`/`vec2`–`vec4<f32>`.
Values are in buffer units: lengths px × scale, angles radians,
durations seconds, colours premultiplied sRGB, the space Strand
composites and reads output in (decisions.md, m4-gpu-w2 review).

On an unpromoted surface a pass is drawn offscreen and read back into
the CPU frame, holding up to `GPU_WAIT` (8 ms), else the previous
result. On a promoted surface a node's own pass runs in the GPU frame;
a layer's bundled passes are `Pass` requests attached to the layer
(`strand_render::layers::Layer::gpu`), except backdrops a GPU frame
draws itself (`Op::Backdrop`). Render asks for a bundled pass only
while its node shows.

**CPU fallback** (no device yet, none, lost, or a build without `gpu`):
bundled effects use CPU versions (bloom → glow, glass → blur and tint,
particles capped at 1,000, tilt 2D, aurora static, backdrop blur at
quarter scale; CRT, chromatic and wobble draw unfiltered); a `shader`
node keeps its box and draws nothing. A first frame never waits for
the device. `Renderer::gpu_status() -> GpuStatus` (`Unused`,
`Starting`, `Up`, `Unavailable { reason }`) goes to logic as
`ToLogic::GpuStatus` when it changes while `gpu_in_demand()` or once
`gpu_off()`; logic logs it once per reason, reports it to `strand
watch`, and keeps it for `strand report` and the inspector.

**Budgets and tests.**

- `.text` (`strand/tests/budgets.rs`, `TEXT_GATE_GPU`/`TEXT_GATE_CPU`):
  the default build at most 28,311,552 B, `--no-default-features` at
  most 24,379,392 B (decisions.md services-round, 2026-10-24, which
  records each earlier re-base).
- Memory (`budgets.rs`), two legs since the device is warm wherever one
  can start (decisions.md gpu-first). Warm, under `STRAND_REQUIRE_GPU=1`
  with lavapipe: the bar warns above 242 MiB and fails above 252 MiB,
  the full shell 296 / 324 MiB, debug 288 MiB (decisions.md perf). Cold
  (`the_design_bar_with_no_gpu_keeps_the_cold_budget`,
  `the_full_shell_with_no_gpu_keeps_the_cold_budget`; no render node):
  no device is asked for, 42 / 48 MB and 76 MB / 128 MiB (96 / 92 MiB
  debug), no `libvulkan` or `libLLVM` mapped; skipped where a render
  node exists.
- `strand/tests/gpu_cold.rs`: wgpu, vello_gpu and naga reach `strand`
  only through `strand-gpu` (naga also through `strand-compiler`'s
  `shaders`), none in `--no-default-features`.
- `strand/tests/gpu_idle.rs::a_promoted_surface_stays_on_the_gpu_and_the_device_stays_warm`
  (CI, lavapipe): a large animated shader panel is promoted and
  presented once, matches the CPU strip, and hidden stays on the GPU with
  its swapchain (no `Released`, no second attach), the thread stays up,
  the process takes no wakeups for 4 s, and strand's own PSS after every
  later cycle is within 4 MiB of the first (decisions.md m5-open-budgets,
  pss-flake, gpu-stay).
- Hardware leg (`gpu.sh`, advisory): the same over three cycles on
  `/dev/dri/renderD128`, with backstops over the warm baseline of
  12 MiB readback and 16 MiB presenting (`STRAND_GPU_PRESENT=1`), and
  the promoted cost checked against design.md's +20–40 MB.
- GPU frames are compared with the CPU frame under their own documented
  tolerance, not `assert_matches_ref`'s.

### `strand-dev`

The developer tooling binary; the runtime never links it. It depends on
`strand-compiler` (with `shaders`), `strand-services-schema` (the
services' schema texts, not their runtime), `strand-introspect` and
`strand-core` (the inspector's socket protocol).

The language server, `strand-dev lsp` (stdio, `lsp-server` and
`lsp-types`): `strand_dev::serve(&Connection)` runs the protocol against
the builtin schema; `serve_with(&Connection, Arc<Schema>)` takes the
schema chosen by the caller; `run_stdio()` serves stdin/stdout;
`capabilities()` is what it advertises. It uses only `strand-compiler`'s
public interfaces: `compile_with` per config, `hir::Program` for hover,
definition, rename and completion, `Schema::doc`,
`Diagnostic::suggestions` for quick fixes, `schema::members_of`,
`fmt::format`, and `source::find_files` for which files a document is
checked with (the default config directory if the file is in it, else
a workspace folder that is a config, else the file's directory, else
the file alone; decisions.md wave2-lsp). Open documents replace their
files' text; disk files are re-read on size or mtime change. Positions
are UTF-16, sync is full-document, diagnostics are published per config
after a 200 ms debounce (`initializationOptions.debounceMs`) and at once
on open and save; `initializationOptions.configDir` overrides the
config directory.

Code actions: each `Diagnostic::suggestions` entry is a quick fix in
its own file (one preferred, several not, except
`check::missing_key`'s first). `refactor::extract` is "extract
component" (`refactor.extract`): the element moves into
`NewComponent` with what it reads becoming named parameters; writes to
the parent's state, an outside `id:`, the `slot`, or a result that
would add an error refuse it (a disabled action where supported). With
`codeAction/resolve` the action is offered without an edit
(`refactor::offer`) and checked when chosen. `context.only` filters
both kinds.

Also here: the tree-sitter package (`crates/strand-dev/tree-sitter-strand/`,
linked into no binary), the reference generator (`strand-dev
reference`, `src/reference.rs`) and the inspector client (`strand-dev
inspect`, `src/inspect/`; protocol and write-back in "M5 interfaces
(wave 0a)").

### `strand-services`

The service contract. `strand-services` depends on `strand-core`,
`strand-watch`, `strand-auth`, `strand-icons`, `strand-introspect`, its
proc-macro crate `strand-services-macros` (re-exported) and
`strand-services-schema` (the builtin services' schema texts as
constants and `schemas()`, with no dependencies: what `strand-dev`
links instead of the runtime; each service module's `SCHEMA` is its
constant there). It never depends on `strand-compiler` and never sees
`Value`. Service state is typed Rust; where something is handled by
name it is `strand_services::Data` (`src/data.rs`), with `ToData` /
`FromData` / `SchemaType` for primitives, `Option`, `Vec` and derived
types.

`strand_services::child` is the spawn contract: every program Strand
starts, in any crate, must run `child::restore_in_child()` as its
`CommandExt::pre_exec`, since the kernel keeps `MMF_DISABLE_THP` (set
once by `child::thp_off()` from `strand`'s ELF constructor) across
`fork` and `execve`. `child::INSTANCE_VARS` are kept from every child
(`GREETD_SOCK`); `child::set_own_var` restores variables Strand set for
itself.

- **A service** is a state struct with `#[service(name = "…")]`
  (options: `schema`, `action`, `call`, `fns`, `refused`, `thread`)
  and `#[derive(Store)]`; field attributes `#[store(rw)]`,
  `#[store(keyed)]`, `#[store(stream)]` (produced only while a visible
  reader reads it); events are `Event<T>` fields. The derive generates
  the `…Patch` (`Send`), `…Event` and `…Cells` (logic thread: `Signal`,
  `KeyedSignal`, `EventQueue`) types and the `Store`/`Cells` impls;
  field docs are held equal to the schema text's by test.
  `#[derive(Data)]` covers records (`#[data(name, key)]`, `key`
  implements `Keyed`) and unit enums; `#[derive(Call)]` covers action
  and async-method enums (a field named `item` takes the item called
  on; `#[call(name = "…")]` lets variants share a name across item
  records), with `FromCall::signatures()` held to the schema by test.
  `#[service]` implements `Service` (`NAME`, `schema()`, `Action`,
  `Call`, `call` for `fn` methods on the logic thread, `start`).
- **Service side** (`Cx<S>`, `src/cx.rs`): `state()`, `update(|s| …)`
  (changed fields as one envelope, one tick), `send(patches)`,
  `emit(event)`, `report(&write, |s| …)` (answers a write, tagged with
  field index and generation so the logic thread ignores the echo and
  drops a refused optimistic value), `ready()` (ends the boot phase:
  earlier updates are boot values `on change` takes as baseline; also
  ends the first frame's wait), `recv()`/`blocking_recv()`/`try_recv()`
  of `Msg::{Write, Action, Call, Visible, Watch, Keep}`
  (`#[non_exhaustive]`; `None` once stopped), `visible()`, `kept()`,
  `period(visible)` (how often a polled service samples now; `Ticker`
  follows it), `watched(field)`, `session()`/`system()` (one zbus
  connection per bus per runtime thread, pinged before reuse; an error
  without a tokio runtime), `notice`/`warn`/`resolve`, and
  `set_notify(f)` for a thread service with its own event loop. A body
  returns when stopped; one that errs while read restarts after a
  backoff (1 s doubling to 30 s, reset only after 30 s up).
- **Threads.** `Start::Shared`: the body runs on the one tokio
  current-thread runtime thread (`strand-services`, a `LocalSet`, so
  bodies need not be `Send`), started lazily and joined by
  `Services::shutdown`; stopping drops the future. `Start::Thread`
  (`#[service(thread)]`, a blocking `fn run(cx)`): a thread per run
  (`strand-<name>`), stopped by closing its messages; the next run
  joins the previous, and `shutdown` joins them (2 s at most).
- **Logic side.** `Services::new(rt, Buses, wake)` (`wake` called from
  any thread after every envelope); `register::<S>(rt) -> Client<S>` /
  `register_as`; `pump(rt)` applies waiting envelopes outside handlers;
  `wait_ready(rt, limit)` / `wait_ready_of`; `report` and
  `take_diagnostics`; `shutdown()`. `Client<S>` (`src/client.rs`):
  `acquire`/`release` (reader count: the first acquire starts it; the
  last release sends `Visible(false)` and arms a `STOP_GRACE` (5 s)
  core timer that stops it; `release` disposes nothing, so it is safe in
  scope cleanup), `keep`/`kept` (a kept service runs with no reader),
  `polls`/`sampled` (from `strand_services_schema::POLLED`),
  `acquire_field`/`release_field` (stream fields get `Watch`), `seed`,
  `act`, `request` (both start a stopped service for that operation),
  counters (`readers`, `running`, `starts`, `stops`, `reports`), and
  `dynamic() -> Rc<dyn DynService>`, the by-index view the language
  side drives (`read`, `write`, `write_item`, `action`, `call`,
  `fetch`, `observe(f)` of `Applied::{Keyed, Event}`). When a run ends
  the cells forget its unanswered writes (`Cells::forget_echoes`). An
  action or write that reaches no run is not an error to the VM: the
  write is dropped and logged, the action logged and applied as
  `Service::refused(&action, error) -> Option<Event>` (e.g.
  `network.failed`); `dbus::idle_without_bus` does the same.
  A body that ends without reading its queue leaves the queued actions
  refused the same way as its `Cx` drops.
  `Client::act` still returns the refusal to Rust callers, and
  `strand_services::catch_refusals(f)` lets `strand call` answer
  refusals.
- **Failures are diagnostics.** A run that errs or panics is a
  `ServiceDiagnostic { service, message, notice: false, .. }`;
  `Cx::notice` raises one the user must act on (`notice: true`),
  `Cx::warn` one the run retries itself. `take_diagnostics()` yields
  each distinct message once (until a run stays up `RETRY_MAX` or ends
  cleanly); a notice that no longer holds comes back once with
  `resolved: true`. `strand run` logs them, sends them to `strand
  watch`, and shows notices as overlay rows under `strand: services`.
- **Calls of one name on several records.** Variants sharing a
  `#[call(name)]` and taking items of different records
  (`TrayItem.activate()`, `TrayMenuItem.activate()`) are told apart by
  the item's record type.
- **Builtin services.** `Builtin::register(&services, rt)` registers
  them all; `strand_services::schemas()` lists their schema texts
  (`strand_services_schema::schemas()`, less audio and video without
  the `pipewire` feature). A real service keeps every field, `rw` mark
  and event of its provisional stub in builtin.schema. A new service
  adds its text to `strand-services-schema` and its store to `Builtin`.
  Each module's doc comment gives its sources, fields and limits:
  `system`, `cpu`, `memory`, `disk`, `keyboard`, `battery`,
  `brightness`, `network` (NetworkManager, else iwd, never
  D-Bus-activated), `bluetooth`, `notifications` (our own server;
  history in the persist store), `apps`, `idle`, `polkit` (our own
  authentication agent), `greetd`, `auth`, `media` (MPRIS; remote art
  through `art`), `tray`, `session` (logind), `outputs`, `clipboard`,
  `workspaces`/`windows`/`wm`, `audio` and `video`. `auth`, `polkit` and
  `greetd` are `strand_services::secret` services
  (`strand_services_schema::SECRET`): never recorded, snapshotted,
  reported, or run from `strand call`; their answers are
  `strand_auth::Password`s in `Zeroizing` buffers. No-code services
  (`from dbus`, `file`, `dir`, `listen`, `poll`) run on the same
  contract as `custom::Custom`, one store per declaration;
  `Client::restart`, `stop_now` and `unregister` serve their reloads,
  and commands run in their own process group, ended whole.
  Hooks the binary sets: `keyboard::focused` (from
  `SurfaceHost::keyboard_state`), `notifications::offer_replies` (from
  `services::follow_program`: whether the program reads
  `Notification.can_reply` or `reply`), `apps::changed()` (the
  watcher's `Apps`/`Icons` caches), `greetd::configure` (its `started`
  hook, which `strand run` sets to raise SIGTERM on itself),
  `auth::configure` (below) and `tray::set_click_point`/
  `set_input_probe` (see "`strand-surface`", origins).
- **Shared D-Bus plumbing** (`strand_services::dbus`, `bus`): `Daemon`
  follows a bus name through `NameOwnerChanged` and checks signals
  against the current owner; property reads are bounded by
  `READ_TIMEOUT` (5 s), calls to apps by `CALL_TIMEOUT` (2 s) or a
  caller's bound (`timed_for`). The shared thread's connections are
  counted per running body (`bus::with_user`) and close with the last;
  a service owning a bus name uses its own (`bus::own_session`). What a
  dropped body still has to say is a finalizer (`client::finalize`),
  awaited up to `FINALIZE_LIMIT` (500 ms) as the thread ends. Calls
  that may wait on an app run as tasks in a body-owned `JoinSet`.
  Pixels from D-Bus become bounded, content-addressed PNGs under
  `$XDG_RUNTIME_DIR/strand/pixmaps/<pid>` (`strand_services::pixmap`),
  each living while a `Pinned` handle does.
- **Language side** (`crates/strand/src/services`, the binary).
  `services::schema()` is `Schema::builtin_with(&strand_services::
  schemas())`, used by `strand check`, the live loader and `strand
  run`; `strand-dev lsp` serves the same through `strand_dev::schema()`.
  `StoreHost` is one store as a `ServiceHost`: a `Memo<Value>` per
  plain field (converted by `services::convert`), a `KeyedSignal` per
  keyed field fed by `Applied::Keyed`, an `EventQueue` per event;
  `write` refuses a non-`rw` field written whole. An async call made
  while nothing reads the service waits for a reader (decisions.md
  wave4-a3). `CustomHost` serves no-code services (`custom::coerce`
  to the declared types; a `from listen` service's `line` event).
  No reload waits on a bus (`custom::dbus_check`).
  `services::set_text` is `strand set`
  (a signed number steps from the current value). `Composite` routes by
  service name, item actions by record, declared services first (a
  config's `service battery` shadows the builtin, spec.md 4.2, except
  `*_builtin` reads), and the rest (clock, calendar, `screens`) to the
  `SchemaHost` fallback. A service added to `Builtin` is served with no
  change here.
- **Tests** (`strand_services::testing`): `PrivateBus::start()` (a
  private `dbus-daemon` with no service directories;
  `start_activating` adds python-dbusmock activation) and
  `DbusMock::start(..)` (python-dbusmock). Both skip without their
  tool unless `STRAND_REQUIRE_DBUS` is set (CI), where they fail.

- **Compositor (`strand_services::wm`, the `workspaces`, `windows` and
  `wm` services).** Typed records in a `WmState`, and `wm::run(WmConfig
  { backend, wayland, events, desktop }, sink, requests)`, a future on
  the shared runtime stopped by dropping it. `sink` gets one non-empty
  `Vec<WmChange>` batch per change (keyed `VecDiff`s with a `Reset`
  first; focus, name, mode, keyboard layouts and rules, configured
  slots, `ConfigReloaded`, and `Sources` always in the first batch);
  state goes out once the adapter's first state arrives, or once the
  protocols have spoken when there is no usable adapter.
  `wm.config_reloaded` has one owner: the `wm` store emits it from
  `WmChange::ConfigReloaded`; the copy sent to `events` as
  `ChangeEvent::Compositor(ConfigReloaded)` is only the live-reload
  change source and must not become a second event.
  The stores share one run through `wm::WmHub` (one per `Services`
  runtime, `wm::hub()`): `subscribe() -> WmSubscription` starts it on
  the first subscriber, replays the current state to a later one (never
  a past reload) and stops it when the last drops. Runs are numbered so
  a stopped run's batch never reaches the next run. A subscriber's
  queue is bounded (`MAX_QUEUED` = 64): a store that stops draining gets
  one rebuilding batch instead. Each store subscribes from its body, so
  its 5 s grace keeps the adapter and the `strand-toplevel` thread
  across a read/unread/read cycle
  (`tests/wm_services.rs::the_compositor_stores_follow_sway_through_one_hub`).
  The hub owns the protocol thread (`ProtocolClient::spawn`); a stop
  only sends `request_stop`, and dropping the hub joins it (2 s at
  most). Each start takes `wm::configure`'s config, else
  `WmConfig::from_env(None)` afresh.
  Requests: `WmRequest { action: WmAction, reply }` /
  `WmSubscription::request(action) -> WmReply`, answered `Ok` or a
  `WmError`; a request dropped unanswered reads as `NotConnected`.
  Window actions (`focus`, `close`, `minimize`, `maximize` and
  `fullscreen` as toggles, `move_to`) and workspace focus by id, number
  or name are translated per adapter (Hyprland in both dialects, sway,
  niri, the protocols); unsupported combinations answer `Unsupported`.
  `wm::detect()` picks the `Backend` (Hyprland, niri behind the
  default-on `niri` feature, sway through swayipc-types). The protocol
  client runs `ext-foreign-toplevel-list-v1`,
  `zwlr_foreign_toplevel_management_v1` and `ext-workspace-v1` on its
  own `strand-toplevel` thread and connection; `wm::merge` joins it with
  the adapter (decisions.md wave4-wm, laptop-toplevel, laptop-open).
  Source order: the adapter, then the wlr protocol (ids `wlr-<key>`),
  then `ext-foreign-toplevel-list-v1` read-only. An adapter that is
  connected but does not understand its compositor (`wm::understood`)
  sends `AdapterMsg::Degraded(text)`: the run serves the protocols as if
  disconnected, sets `Sources::degraded`, and retries on its backoff
  without flapping ids (`understood::Degradation`); the oldest
  subscription raises each new text with `Cx::warn`. Unknown events are
  ignored; a known one that cannot be read triggers a re-read
  (decisions.md laptop-resilience). Windows carry `app` and `icon` from
  `apps::AppIndex`, read on the blocking pool and refreshed by
  `apps::changed()`. `Window::toplevel` (not a schema field) is the
  identifier thumbnails capture by. `wm::follow_focused_screen(&Services,
  Fn(Option<String>)) -> FocusFollower` holds one hub subscription on the
  services runtime until dropped and calls back with each new
  `WmChange::FocusedScreen` (a connector name, `None` when none is
  named). The binary's `screens` (`run/focus.rs`) holds a follower while
  the program reads the focus (the `screens.focused` cell has an
  observer, or the program names `Screen.focused`), let go
  `STOP_GRACE` after the last reader: `screens.focused` and
  `Screen.focused` are the monitor whose `name` the follower gives, else
  the first, and main hears it as `ToMain::FocusedScreen`. Schema texts:
  `strand_services_schema::{WINDOWS, WORKSPACES, WM}`.

- **Monitor configuration (`strand_services::outputs`, the `outputs`
  service).** `outputs::Outputs` runs on its own `strand-outputs`
  thread and Wayland connection and binds `zwlr_output_manager_v1`
  (≤ 4). `all` and `available` are published at each manager `done`.
  `OutputsAction::{Apply, Test}` queue in order, one configuration in
  flight against the last `done`'s serial, every head included (the
  protocol's rule); changes are validated before sending, since the
  protocol ends the connection on those errors, and a mode is the
  listed one nearest within 0.5 Hz (`outputs::pick`), else custom.
  `cancelled` is retried from the new state up to `MAX_TRIES` (3). A
  lost connection empties `all`, fails what is in flight and ends the
  run. `outputs::configure` points later runs at a socket (tests:
  `crates/strand-services/tests/outputs.rs`).

- **Clipboard (`strand_services::clipboard`, the `clipboard`
  service).** Runs on its own `strand-clipboard` thread and connection
  over `ext_data_control_manager_v1` or `zwlr_data_control_manager_v1`
  (v1, no primary selection). Fields `available`, `text`,
  `holds_image`, and `history` (keyed, stream). A selection marked by
  `x-kde-passwordManagerHint` (or whose hint is unreadable) is secret:
  read no further, recorded and logged nowhere. The selection found at
  bind is not a copy and is never recorded; the run is ready only once
  that bind's sync is answered. History (`clipboard/history.rs`) is
  kept only while watched, bounded (`Caps::DEFAULT`) and saved under
  `<state>/strand/clipboard/` (0700/0600, atomic writes; a corrupt file
  is an empty history and a `Cx::warn`). A copy is held by the
  process-wide `strand-clipboard-keep` thread (`clipboard/keeper.rs`),
  so it outlives the run's stop. Without data control `available` is
  false and actions fail as `failed`. Tests:
  `crates/strand-services/tests/clipboard.rs`, `compositor_matrix.rs`.

- **Audio (`strand_services::audio`, the `audio` service; cargo feature
  `pipewire`, on by default).** `Audio::spawn(AudioConfig, sink)` runs
  pipewire 0.10.1 (`v1_0_0`) on its own `strand-pipewire` thread (the
  `audio` store runs the same loop, `thread::run`, on its service
  thread). Dropping the handle asks it to stop and returns at once
  (safe on the shared runtime); `stop()` also joins, which blocks
  briefly and belongs off the logic thread. `sink` gets one non-empty
  `Vec<AudioChange>`
  batch per burst on that thread and must never block: `Connected`
  (first in the first batch), keyed `Sinks`/`Sources` diffs by
  PipeWire id (an id reused for another device is a `Remove` and an
  `Insert`) and `Streams` by `object.serial`, the defaults,
  `Serials`, and `Levels` (at most one per meter per `audio::FRAME`).
  `AudioDevice` is exactly the schema's record; `audio::Mirror`
  applies the stream.
  `request(AudioAction::{SetVolume, SetMuted, MakeDefault}) ->
  AudioReply` (`.await`, or `wait()` outside a runtime) answers `Ok`
  once PipeWire was asked, or an `AudioError`; dropped unanswered reads
  as `NotConnected`. Actions before a connection settles wait in order;
  with no connection they answer `NotConnected` after `audio::GRACE`
  (2 s). A connection settles once its syncs and the `default`
  metadata are back, or after `audio::SETTLE` (3 s); until then the
  last state stays, so a daemon restart never flashes an empty sink. A
  daemon that never answers its first sync is dropped after
  `audio::UNANSWERED` (6 s). The `default` metadata is re-bound
  `audio::REREAD` (250 ms) after a client comes or goes. `DeviceRef::
  Id { id, serial }` names a device only while that id holds that
  `object.serial`, else `UnknownDevice`.
  Every language-side write arrives as `SetVolume`/`SetMuted`; relative
  `strand set` steps are resolved generically by `services::set_text`
  (there is no relative action; decisions.md m6-services). Volume and
  mute go to the card's active `Route` when there is one, else the
  node's `Props`. The thread reports a volume it wrote exactly as
  written (within the last `audio::ECHOES` writes).
  The store (`audio::AudioStore`) maps writes of `audio.sink`/`source`
  to the default device and item writes to `DeviceRef::Id`, refusing a
  write from an item held past its device (`Write::held`). Each write is
  answered tagged by the first batch that shows its value, or with the
  device as it is when refused or unseen within `audio::ANSWER_WAIT`
  (1 s); answers go in write order per cell. Streams (`audio.streams`,
  keyed by `object.serial`) are bound only while watched; this shell's
  own meters are left out. `strand_services::video` runs the same loop
  following only video nodes and links (`camera` or `screencast`).
  `levels(device, bars)` is a service fn over `meters` (64-band
  spectra), leased per evaluation (`ask_levels`/`release_levels`,
  `audio::LEVELS_LEASE` 1 s); `audio::bars` maps bands to at most
  `MAX_BARS` (512). `set_levels(targets)` replaces the meter set; the
  store passes only what visible readers want. `audio::tap_levels(
  target, f) -> LevelTap` is the `spectrum` element's hook (held by
  `run/feeds.rs`): it asks the running store to meter `target`, passed
  to the loop only while a reader is visible (`Cx::visible`).
  `audio::configure` points later stores (`video`'s too) at a socket
  (tests). The taps and the `audio::configure`/`wm::configure` targets
  are process-wide (decisions.md wave4-wm fixes r1). Without
  `pipewire` the module is
  absent and `audio.*` answers at the schema's defaults.

- **Auth, spectrum and capture feeds.**
  - `auth` (`services/auth.rs`, schema
    `strand-services-schema/src/auth.schema`, `AUTH`): its store runs a
    `strand_auth::Client` (helper started with `restore_in_child`,
    killed when the store stops); each conversation is a blocking task
    whose texts and prompts become `message` and `prompt`. While
    `auth::LockSignal` says a lock is shown the store keeps a
    conversation open; `LockSignal::wake` (each key and pointer event on
    a shown lock) restarts one stuck on a prompt PAM asked after
    `READER_WAIT` (2 s) (decisions.md m6-lock). The password is a
    `strand_auth::Password` from arrival, wiped once sent. A success
    hands the `UnlockToken` to the `auth::UnlockSink` set by
    `auth::configure(AuthConfig { .. })`, which passes it to the
    surface manager's unlock (a conversation with no password in it, a
    finger's, mints one too). A submit arriving during a check is
    dropped. A refused password sets `failed`; one with no password in
    it leaves `failed` as it was; a check with a password that could not
    be made goes to the `auth::FailureSink`. The blocking
    task calls the sinks itself with the `auth::SessionTag` read at its
    start, so a check outlives a store that stops mid-check.
  - Spectrum: each meter's data thread keeps 4,096 mono samples in a
    ring (`audio::spectrum::Ring`); a reading with sound carries
    `Levels::bins`, a Hann-windowed 2,048-point FFT
    (`audio::spectrum::Fft`) folded into `spectrum::BANDS` (64) bands
    from 40 Hz to 16 kHz, −72 dBFS..0 as 0..1. Silence sends nothing, so
    no FFT runs (`tests/audio.rs::a_test_tone_lights_its_spectrum_band`).
  - Window thumbnails: `wm::capture::capture_window(window, max,
    on_frame) -> CaptureTap` captures through ext-image-copy-capture on
    the `strand-toplevel` thread, one session per window while any tap
    lives, frames only on damage and at most `capture::MAX_FPS` (15);
    `on_frame(None)` once when the window closes. `CaptureFrame` is
    premultiplied BGRA scaled to cover `max`; the callback runs on the
    protocol thread. Frames reach render through the binary
    (`Renderer::feed_frame`) only while the thumbnail is visible
    (`take_feed_demand`, `FeedKind::Thumbnail`, sizes rounded to
    `THUMBNAIL_STEP`); `run/feeds.rs` holds one tap per node.
  - Screen thumbnails: `wm::screen::capture_screen(connector, max, f)
    -> ScreenShot` takes one frame on a `strand-screenshot` thread
    (ext output capture source, else `zwlr_screencopy_manager_v1`);
    `f(None)` on any failure or after `SHOT_WAIT` (3 s); a dropped
    `ScreenShot` never calls `f`. `run/feeds.rs` maps `Screen` ids to
    connectors and takes a shot each time the node becomes visible.
    Before a non-lock surface maps, `Feeds::before_map` takes stills of
    the monitors its thumbnails name and holds its first frame (`HOLD`
    300 ms), so a thumbnail never captures the surface showing it.
  - Lock stills: in `before_lock` the host calls
    `Feeds::take_lock_stills` (`LOCK_STILL_WAIT`, 500 ms, blocking); a
    lock thumbnail shows only that still, never a live frame, and the
    stills go at the lock's end (`Feeds::lock_ended`). A monitor
    unplugged or changing connector during the lock loses its still. A
    thumbnail fed nothing is `blank` (`NodeFlag::Blank`, `t.blank`).
    Bounds (`STILL_MAX`, `STILL_KEEP`, `SHOT_GIVE_UP`) are in
    `run/feeds.rs`.

### `strand-watch`

Produces typed events, never parsed content; logic turns them into
writes. It depends on neither `strand-compiler` nor `strand-core`.

- **One channel.** `strand_watch::channel() -> (EventSink,
  Receiver<ChangeEvent>)`; `EventSink` is `Clone + Send`, and
  `.with_waker(Fn())` calls a waker after each send. `ChangeEvent` is
  `Files(FileBatch)`, `System(SystemBatch)` or
  `Compositor(CompositorEvent)`.
- **Files.** `Watcher::spawn(Option<ConfigWatch>, Options, EventSink)`
  runs the `strand-watch` thread (raw inotify plus a control eventfd
  under `poll(2)`; everything polled without inotify). Dropping the
  `Watcher` stops it; `Watcher::join` also reports a panic.
  `ConfigWatch { root, modules: ModuleSet, rescan }` is `find_files`'s
  `Discovery` plus a callback the binary implements with `find_files`,
  called when names, directories or links change in a config directory
  or on a rescan. When `spawn` returns every watch is in place and every
  module's baseline hash was read after its watch: start the watcher,
  then load. The set is listed once more at the first quiet period, so
  a module created in between is reported `Created`.
  - `set_referenced(items)` replaces the loader's set of `(path, role[,
    hash])` for every `Role::{Shader, Settings, Wallpaper, Other,
    Asset}` the program references; a file no longer holding the
    `hash` the loader read is reported. `watch_file(path, role)` /
    `unwatch_file` add counted ad-hoc registrations that
    `set_referenced` never replaces (the wallpaper owner);
    `watch_file_loaded(path, role, hash)` is read-then-register. Neither
    a referenced file nor its directory need exist yet. Module-set
    membership is separate from roles.
  - Caches: `watch_tree(dir, depth, CacheKind::{Apps, Icons, Fonts})`
    and `watch_file(path, Role::Cache(kind))`. `strand run`'s compiler
    worker takes them as `live::Job::Caches` and calls `changed(kind)`
    once per kind a batch touched (all three after an overflow); the
    binary then tells the `apps` service, `strand_icons::invalidate` and
    the renderer (`run::cache_changed` treats `Apps` and `Icons` alike).
  - Assets: `Role::Asset` is a local file a mounted `image`, `svg` or
    `lottie` draws (`Instance::asset_files()`, with
    `take_assets_changed()` checked after each step), joining the
    logic thread's `Job::Referenced` list. The worker hands changed
    asset files to `live::Job::Assets`' sink, which reaches
    `Renderer::assets_changed(&paths)` on the main thread: no reload;
    the old decode or text is drawn until the new one replaces it.
  - **Blocking:** `watch_file`, `set_referenced` and `watch_tree` wait
    for the watcher thread; call them from the loader, compile worker,
    boot or a service thread, never from logic. `watch_file_loaded`,
    `unwatch_file`, `register_own_write` and `rescan` return at once
    and are safe on the logic thread.
  - `register_own_write(path, hash_bytes(&bytes))` before Strand writes
    a file makes the matching write silent, under every registered path
    resolving to that file. Own writes should be atomic (temporary file
    renamed over).
  - Torn reads: a file with a write in progress (a `MODIFY` in a config
    directory, or a created file not yet closed) is never read; after
    `Options::stalled_write` (5 s) with no change it is read anyway with
    `Notice::StalledWrite`. A file a write may have reached during its
    read is left out and read at a later quiet period, but within
    500 ms of the event that made it due. In config directories no
    batch carries a torn read; elsewhere (watched without `MODIFY`) an
    in-place writer pausing over 15 ms can be read torn, and its
    `CLOSE_WRITE` reports the whole file next batch.
  - Ancestors of each watched directory get a light watch (moves and
    deletes) up to `Options::home` (default `$HOME`), or outside it up
    to its mount's root (design.md "Watch directories, not files";
    decisions.md laptop-decisions); moving `$HOME` itself is not seen.
  - `rescan()` is `strand reload`.
- **`FileBatch { changes, rescan, notices, first_event, last_event }`.**
  One batch per quiet period: 15 ms after the last completed write
  (50 ms when the latest event removed a watched file; at most 500 ms
  after the first event). `changes` is sorted by path then role, one
  `FileChange { path, canonical, kind, hash, role, error }` per role;
  unchanged hashes and own writes are dropped, so every change is real.
  A `Modified` with an unchanged hash means a link now resolves
  elsewhere. `path` is as registered (module files as `find_files`
  returned them); only regular files are hashed (others get `error:
  Some(InvalidInput)`), cache-tree entries never. `rescan` is
  `Some(Overflow | Requested)`; `notices` covers polled directories,
  rescan failures, backend errors, stalled writes and
  `Notice::ModuleSet { errors, too_deep }` (complete lists, when they
  change). Latency measurements use `sent − last_event` as the
  watcher's share of save-to-pixels.
- **System settings.** `strand_watch::follow(&zbus::Connection,
  EventSink)` is the async portal client, which `strand-services` runs
  on its shared runtime; `PortalSettings::spawn(Bus::Session, sink)`
  runs it on its own `strand-portal` thread. It sends one `SystemBatch {
  settings, at_boot: true, received }` within `BOOT_READ_TIMEOUT`
  (500 ms; empty without a portal; `on change` must not fire for it),
  then one per `SettingChanged`, plus `at_boot: false` batches for late
  boot reads and full re-reads when the portal (re)starts.
  `SystemSetting::{Dark, Accent, Contrast}`, each with `.path()`
  (`system.dark`, `system.accent`, `system.contrast`).
- **Compositor.** `CompositorEvent::ConfigReloaded { failed:
  Option<bool> }` is the compositor-reload change source (`None` from
  Hyprland and sway). `wm.config_reloaded` comes from the `wm` store,
  not this copy. The adapters live in `strand-services` and send it
  through a clone of the same `EventSink`, so `strand-services` depends
  on `strand-watch`, never the reverse.

Settings files (the Threads table holds: the watcher reads and hashes,
it never parses). On a change event the watcher hashes the file
(BLAKE3); an unchanged hash stops there, including Strand's own writes:
the binary registers `persist_store.on_written(..)` and passes each
`OwnWrite`'s content hash to the watcher for its `target` (and `path`
when a link) before the rename. A changed hash goes to the logic thread
as (path, hash), which calls `settings.reload(rt)` on every `Settings`
handle on that path; `reload` takes the write mark before reading, so a
concurrent own write is never undone. `Settings::sources()`,
`SettingsSources::read_from` and `Settings::reload_with` remain for a
thread allowed to parse. Strand's temp files beside a settings file are
`.<name>.tmp.<pid>.<n>`, renamed over it (the watcher sees `MOVED_TO`
for the file). The temp file's own events name no watched file, and its
leading dot keeps it out of a config directory's module scan
(`paths::is_hidden`), so it never reads as an edit.
