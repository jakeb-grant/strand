# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, token evaluation per frame, layout, damage, paint, presentation | Wait on the logic thread, run handlers, evaluate bytecode |
| Logic | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program; the `strand` binary's IPC Unix socket (`strand reload`, `strand watch`, M5's `get \| set \| toggle \| watch \| call`) is a source on this loop | Touch Wayland or pixels |
| Compiler worker | `strand-compiler` | Parse, check, lower changed modules off-thread | Mutate live state (it hands a compiled `Program` to logic) |
| Text worker | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: a painted surface keeps drawing its last layout (or a realigned stand-in from another scale or width) until the new one arrives |
| Watcher | `strand-watch` | inotify directory watches and polling (one `strand-watch` thread); not the IPC socket (`docs/decisions.md`, wave2-watch) | Parse files (it sends paths and hashes) |
| Persist IO (one per `PersistStore`) | `strand-core` | Atomic writes of persisted cells, settings-file edits, settings overlays and last-good snapshots; reports each file it is about to change to `PersistStore::on_written` | Run on the logic tick or block logic (failures come back as diagnostics in a later tick) |
| Services | `strand-services` | One tokio current-thread runtime thread (`strand-services`), started with the first service that runs on it: every async service body (the `system` service follows the portal Settings with `strand_watch::follow` here; the `workspaces`, `windows` and `wm` stores and their one compositor hub with its IPC adapter run here, the sway adapter on swayipc-types (swayipc-async 3.0's types) over its own tokio framing, so no async-io reactor thread) and the portal icon-theme follower (`strand_services::icon_theme`, a task on the same session connection); the `audio` store runs the PipeWire loop on its own service thread (`strand-audio`, `Start::Thread`), and the hub's Wayland toplevel/workspace protocol client its own `strand-toplevel` thread (the hub's, not a `Start::Thread` body: three stores share it; the hub tells it to stop on its last stop without waiting on the shared runtime, and joins it at the next start or stop and on `Services::shutdown`) (idle: zero wakeups, `crates/strand-services/tests/idle.rs`, `tests/audio_idle.rs`, `tests/wm_services.rs`, `tests/audio_service.rs`) | Block logic: they send patches and events over channels, applied by `Services::pump` on the logic thread |
| GPU (M4; started on demand, ends with the device) | `strand-gpu` | The wgpu instance, adapter and device, vello_gpu's renderer, shader pipelines, promoted surfaces' swapchains, offscreen passes and readbacks (see "`strand-gpu`") | Run while nothing needs it, touch the scene tree, or make the main thread wait: every reply is a message and a ping |

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s (`strand-scene`) and layout facts
(`self.width` for container queries: `Renderer::take_layout_facts`, the
laid-out sizes that changed of nodes carrying `Prop::Watch`, sent as
`run::ToLogic::Layout { seq, sizes }` with `Renderer::layout_seq`). The
next diff logic sends echoes the last batch it took in as
`SceneDiff::layout_seen` (sent even with no ops): render holds a frame
whose layout changed a `watch: query` node's size until then, or for
`strand_render::QUERY_WAIT` (`Renderer::set_query_wait`; zero offline),
so container queries settle inside the frame. `SceneDiff::reduced_motion`
carries a change of the desktop's reduced-motion preference
(`system.reduced_motion`) to render (sent even with no ops). No locks are shared across
threads on a hot path.

`strand run [dir]` (`crates/strand/src/run/`) is this wiring: the main
thread's surface host forwards the monitor hooks (`screens` as a list of
plain `ScreenInfo`s, `monitor_forgotten` as `Forget(id)`), pointer input
on the node under the pointer (`Renderer::hit`'s chain: `hover` along
it and `pressed` on the chain under a press as `Flag`,
`click`/`secondary` on the innermost node both the press and the release
were over and `scroll` on the innermost node as `Event { node, event:
NodeEvent }`, which logic bubbles to the nearest handler) and surface
sizes to the logic thread over a calloop channel (`run::ToLogic`).
`NodeEvent` is one variant per kind of input, each with its own payload
(`Click`, `Secondary`, `Middle`, `Scroll { dy, dx }`, `Activate`, `Key {
name, text, modifiers }` (a `Key` record made by the service host,
`args_with`), `Dismiss`; `name()` and the args are what `Instance::event`
takes); M4 adds `Drop { payload, at }` for `on drop(p: T, at: int)` as a
new variant of the same message. Input is routed on the main thread by
`strand_render::input::Router` (`handle(&InputEvent, &mut dyn
InputScene) -> Vec<Intent>`; the `Renderer` is the `InputScene`: hit
chains, scrolling, the tree): hover and pressed chains, keyboard focus
(the focused node of the focused surface gets `key(k)`), list selection,
`nav`, `input` edits, and `open: false` on a surface whose `open` is
two-way (`Prop::TwoWay`) on Escape, focus loss and click-away
(`InputEvent::ClickAway`, or a left press on another Strand surface while
an open `keyboard: exclusive` one has a two-way `open`:
`InputScene::exclusive_open`). The host passes each logic diff to
`Router::observe` before applying it, so logic's own `input` text wins
over edits in flight, and calls `Router::settle(&mut dyn InputScene)`
after applying it (a focused `input`'s `nav` list with rows and no
selection selects its first row), sending its intents like `handle`'s.
A `KeyboardEnter` with no leave before it (the keyboard back from a
grabbing popup) keeps the surface's focused node. An `Intent` is `Flag { node, flag, on }`,
`Event { node, event }` or `Write { node, prop, value }`; the binary's
host (`demo/host.rs`, `Forward`) only maps them to `ToLogic::Flag`,
`Event` and `Write` (which logic applies with `Instance::write`, a
two-way write) (decisions.md, wave3-pixels). Before the logic thread
starts, `live::Worker::spawn` starts the `strand-watch` watcher (module
set from `find_files`, rescan callback calling it again), boots the
`Loader` (the boot `Outcome`: a build, or a cached last good one, or
none, with diagnostics) and starts the `strand-compile` thread, which
compiles each watcher batch and `strand reload` off the logic thread and
sends `live::FromWorker::{Loaded, Settings}` on a calloop channel; logic
sends it `Job::{Reload { hard, client }, Referenced { files, settings }}`
(settings files, and the theme's wallpapers and imported files, re-sent
whenever the theme reads a new one, with `Instance::settings_sources()`
so the worker reads a changed settings file itself; their changes come
back as `FromWorker::Settings(Vec<SettingsChange { path, read }>)`,
applied with `Instance::reload_settings_with(path, read)`, and
`FromWorker::Theme(paths)`, and so does each newly
registered file once, right after its registration, so an edit made
before the watcher had it is read)
(the `Loaded` a reload causes carries the IPC clients it answers). A
load that commits nothing but clears the last attempt's problems (a
broken save reverted to the last good text, `Outcome::cleared`) is
sent too, so the overlay closes and `strand watch` hears it; an attempt
on exactly the files the one before it read (`Outcome::repeated`: the
watcher's re-listing after a `strand reload`) repeats its problems
without compiling and is not sent again. The
persist store's `on_written` registers Strand's own writes with the
watcher. The logic thread commits each `Loaded` (`Instance::reload`,
`reload_hard`, held back while a lock is shown: the newest such load
waits, absorbing older ones, and is committed after the step that
closes the lock, with the newest attempt's held files and diagnostics;
a load committed meanwhile drops it), keeps the error overlay
(diagnostics, and reload notices with their `[reset]`, which calls
`Instance::reset`; rows about one cell replace each other)
(`overlay.rs`, external nodes; 250 ms quiet) and freezes faulting
components, and serves the IPC socket (`ipc.rs`) as sources on its loop;
it owns the runtime, `SchemaHost::real`
and the `Instance`, loops on `Instance::step`, sends each non-empty diff
on a calloop channel, and sleeps in a calloop loop of its own until a
message, the runtime's wake hook (a ping, so the hook holds no sender
and the thread ends when the main thread's senders are gone), the logic
clock's `Wake::deadline` (the dispatch timeout) or `Wake::wall` on a
`CLOCK_REALTIME` timerfd (`TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET`:
a resume or a clock step wakes it at once). After a structural burst
(a `SceneDiff` that creates or removes nodes or swaps the tokens: boot,
a reload, a surface, popup, toast or row appearing or going), the
logic thread, which sent it, and `strand run`'s main thread, which
applied it (a dispatch timeout), each force one allocator collect
500 ms after their last wake (`run::trim::Trimmer`, `run::trim::structural`,
`mi_collect(true)`), so what the burst freed is returned to the system
once the shell goes quiet; a tick or a poll only sets props and pays no
trim wakeup, and each loop instead trims inline at the end of a wake it
was given anyway when it has not trimmed for 5 s (`Trimmer::settle`)
(decisions.md, wave4-exitMemory). The services' shared runtime ends
an idle blocking-pool thread 500 ms after its last task
(`client::BLOCKING_KEEP_ALIVE`), inside the burst's settling, not 10 s
into an idle shell. A forced collect frees only the calling thread's
pages (and every arena's pending purges), so the text worker, which
wakes with every tick and no trim reached, takes a hook:
`strand_text::set_idle_hook(fn())`, installed once by `main` with
`run::trim`, runs on each worker's thread when its queue and channel
drain after work, before it blocks (inside the burst, no wakeup of its
own), at most once per 5 s plus that burst's 250 ms tail, as the main
thread's inline trim, which also waits out an animation's frames
(`strand_render::Renderer::in_motion()`, true while any surface has a
spring or crossfade unsettled: a moving wake never trims inline);
`strand_render::image::set_idle_hook(fn())` runs the same hook on the
image decode worker as its queue drains after a decode (not after
requests dropped undecoded). Both workers rule it with
`strand_text::HookGate`: a drain it skips owes the hook, and the worker
runs it once it has been quiet for 500 ms (one wake, only ever after
real work and at most once per 5 s, inside the burst's settling as the
main thread's delayed trim; so an idle worker never wakes, and a skip
past that wake waits for the next drain allowed). The main and logic
threads' delayed trim (`run::trim::Trimmer`, armed by a structural diff) is
pushed back by each wake while armed, but no further than 5 s after it
was armed, so a surface that never settles still trims. The memory budget also rests on the workspace's
release profile: the root `Cargo.toml`'s `[profile.release.package]`
opt-levels build event-rate code for size, and neither `cargo install`
from crates.io nor a packager's own profile carries them, so packages
build from the workspace (`budgets.rs` holds the release binary's
`.text` to 18.5 MiB, and a build without the GPU backend to 15 MiB; see
"`strand-gpu`"). Every program strand starts gets back
the THP setting strand inherited (`strand_services::child`, in each
`pre_exec`). SIGINT, SIGTERM (a
`signalfd` on the main loop, the signals blocked in every thread) and
the compositor going away send `ToLogic::Shutdown`; the main thread
joins the logic thread, which unmounts the instance, runs
`Runtime::shutdown` and drops its stores, so debounced persist and
settings writes reach the disk before the process exits. Without
`STRAND_MOCK` the logic thread runs the real services
(`crates/strand/src/services`: `Real::start` registers every builtin
service of `strand-services` on `Live::buses`, the environment's buses
for `strand run`, behind a composite host whose fallback is the
`SchemaHost`; see `strand-services`, "Language side"). It calls
`Services::pump` after every sleep, before the step (the registry's waker
is a ping on its loop), holds one reader of `system` itself (render needs
`system.reduced_motion`), seeds `system` with the last values it kept
in `$XDG_STATE_HOME/strand/palettes/system` (boot values) and writes
them back off the logic thread whenever the service reports new ones;
after mounting, the first frame waits up to 100 ms
(`Services::wait_ready`) for the first reads of the services the
config started (the portal's boot read among them). Settings-file notices from core are overlay rows (a
shadowed field's `[clear]`) and `strand watch` notices. Layout facts
(`ToLogic::Layout`) address the laid-out nodes logic measures (the
instance sets `Prop::Watch` on an element whose `width`/`height` a
binding read: `size`, or `query` from a `when`); a surface's configured
size still arrives as `Size` on its node. `STRAND_MOCK=desktop` fills the
host with a mock desktop for screenshots before M3 (`mock.rs`);
`STRAND_MOCK=acceptance` is the same desktop with no notifications at
boot and the clock frozen (UTC, `SchemaHost`'s `MOCK_TIME`; the logic
loop arms no wall-clock wake), which the M2 acceptance tests drive. With
either, the IPC command `mock` (`{"v": 1, "cmd": "mock", "notify": {…}
| "volume" | "muted" | "brightness"}`) reports a service change as the
M3 services will; without `STRAND_MOCK` it is refused.

Module map of `crates/strand/src/run/` (split by concern in M4 wave 0
with no behaviour change; the `run::` items other crates' modules use
are re-exported from `mod.rs`): `mod.rs` (the messages, `ScreenInfo`,
`NodeEvent`, `ToLogic`, `set_screens`, `storage`, and the main thread:
signals, cache changes, `run`), `logic.rs` (`Live`, `logic`, the logic
thread's boot and step loop), `sleep.rs` (the logic thread's calloop
sleep: `Inbox`, `Sleeper`), `shell.rs` (`Shell`: messages, commits and
reloads, IPC requests, `strand watch` events), `lock.rs` (the deferred
load committed after the unlock), `trim.rs` (`trim`, `Trimmer`,
`structural`) and `tests.rs` (`run::tests`). M4 streams add `feeds.rs`,
`lists.rs` and `gpu.rs` beside them, and S-lock grows `lock.rs`.

**IPC** (`crates/strand/src/ipc.rs`): a Unix socket at `$STRAND_SOCKET`
or `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock`, newline-delimited
JSON. Requests are `{"v": 1, "cmd": …}`; each is answered with one line
`{"ok": true, …}` or `{"ok": false, "error": …}`, and an unknown `cmd`
or a newer `v` is refused without closing the connection, so M5's `get`,
`toggle` and `call` are new `cmd`s on the same socket (`set` is one
already). Version 1: `set` (`"path"`, `"value"` as text: `strand set
theme.look mocha`, answered `{"ok": true}` or with the error),
`reload` (`"hard"`; answered once the reload is committed or held, with
its event; at once with `"deferred": true` in the event while a lock is
shown), `reset` (`"path"`: a state cell back to its default, as the
overlay's `[reset]`) and `watch` (`{"ok": true}`, then one event per line:
`{"event": "reload", files, committed, held, unreadable, from_cache,
classes, kept, kept_over_default: [{path, shown}], reset: [{cell, why}],
ambiguous, notices, restarted, cancelled, deferred, timing: {watch_ms,
compile_ms, commit_ms, total_ms}, diagnostics: [{severity, code,
message, help, at: {file, line, column}, labels, short}]}`,
`{"event": "notices", kept_over_default, notices}` for cells kept over a
changed default outside a reload (persisted cells at boot, a parked bar
back; with nobody watching they go into the next reload event's
`kept_over_default`) and lowering's notices; right after a `watch` is
answered, a new watcher also gets `{"event": "notices", …,
diagnostics}` with the running config's check warnings when it has any
(`check::dbus_unchecked`, `check::poll_program`; the server hands the
`Watch` request to the logic thread after answering it), which the next
reload event without them resolves; and `{"event": "fault",
message, at, frozen}`). `total_ms` runs from the watcher's last event
behind the save to the moment the diff holding the reload is sent to
render. A client whose socket cannot take its output yet gets a write
source on the logic loop until it is written (no polling); one more
than 1 MiB behind is dropped.

**M4 additions (planned; docs/m4-plan.md, waves 0a and 0c).** The GPU
thread, promotion and the surface hand-off are in "`strand-gpu`";
`run/gpu.rs` is the binary's side of them.
- `ToLogic::ListWindow { list, first, count }`: the rows a virtualised
  `list` wants mounted (its view plus overscan, from
  `Renderer::take_list_windows`), applied by logic with
  `Instance::set_list_window`. `ToLogic::LockState(LockState)`: the
  session lock as the compositor reports it (`Locked`, `Finished`,
  `Unlocked`), which feeds `lock_shown`.
- While a lock is shown, the main thread outlives the logic thread: logic
  ending, panicking or hanging (a watchdog), SIGTERM, and a lock with no
  first frame within 1 s leave the session locked and show render's
  built-in fallback lock. The main thread then owns the unlock gate and a
  `strand_auth::Client` of its own. SIGINT and SIGTERM end `strand run`
  only while no lock is shown.
- The PAM helper is a process, not a thread: the `strand-auth` binary,
  fork+exec'd over a socketpair by `strand_auth::Client`, one per lock
  session, respawned when it dies. The `Client`'s owner hands it
  `strand_services::child::restore_in_child` as its `pre_exec` hook,
  since `strand-auth` depends on no Strand crate. Only the helper links
  libpam.
- Spectrum bins and thumbnail frames are produced off the main thread
  (the audio thread, the `strand-toplevel` thread) and reach render
  through the binary (`Renderer::feed(node, bins)`,
  `Renderer::feed_frame(node, frame)`). Render tells the binary which of
  those nodes are visible (`Renderer::take_feed_demand()`), and producers
  run only for them.

## Crate graph

```
strand-scene      shared vocabulary: ids, geometry, colour, scene protocol, Painter
  ^   ^   ^
  |   |   strand-surface   (layer-shell, shm, damage submit, input, frame timing)
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
          └── strand-introspect (D-Bus introspection; zbus only)
strand (binary) wires everything; its ServiceHost adapters join
strand-services' stores to strand-compiler's VM.

strand-auth  (M4) lib: wire protocol, Client, UnlockToken (libc, zeroize)
             bin: the PAM helper, the only code that links libpam
  ^-- strand-services (the `auth` service), strand-surface (the unlock
      gate), strand (the main thread's fallback client); both `Client`
      owners pass `child::restore_in_child` in as the spawn's `pre_exec`
```

`strand-auth` (M4) is the lock's whole security boundary, small enough to
review alone. Its lib depends on `libc` and `zeroize` and no Strand
crate; its binary adds a hand-written PAM FFI and nothing else. The lib
holds the framed request/reply protocol over a socketpair, a blocking
`Client` (`Client::new(helper: PathBuf, pre_exec: fn())`, which spawns
the helper and runs `pre_exec` in the child between fork and exec;
`submit(password) -> Verdict`, timeout, respawn on a dead helper, with
the same hook), password buffers that zeroize on drop, and `UnlockToken`:
a value only `Client` mints, from a success reply, which is `Send` and
neither `Clone` nor constructible elsewhere. `strand-surface` releases
a session lock only for an `UnlockToken`, so no other code path can
unlock. The PAM service is `strand`, or `login` with a one-time warning
when `/etc/pam.d/strand` is missing (decisions.md, m4-owner); every
other PAM error fails closed. A `faults` cargo feature (off in default
and release builds) adds `STRAND_FAULT` injection points here and in
`strand`.
As built (decisions.md, m4-lock-w1): `protocol` (frames of a `u32`
length, a kind and at most 1 KiB: `HELLO` with the version and the
service, `SUBMIT`, `VERDICT` with success, denied or error and PAM's
text), `Password`, `Client` (`new`, `with_timeout`, `submit`,
`helper_pid`, `spawns`, `service`; `faults` adds `with_test_env`),
`Verdict { Unlocked(UnlockToken), Denied { message }, Failed(AuthError)
}`, `default_helper()` (beside the executable, then the libexec paths)
and `take_service_warning()` (the `login` fallback's warning, once per
process). A timeout or a bad frame kills the helper and everything it
started; the next password starts another. Under `faults` the helper
also takes a private PAM confdir (`STRAND_AUTH_PAM_CONFDIR`,
`pam_start_confdir`), which the tier-A tests use; the crate's own tests
turn `faults` on through a dev-dependency on itself.

`strand-gpu` (M4) holds every GPU crate: wgpu, vello_gpu and, through
wgpu, naga's runtime use. Its only Strand dependency is `strand-scene`.
`strand-render` depends on it under its `gpu` feature and lowers its own
display lists into `strand-gpu`'s frame type, so the GPU crate never
sees render's types; the binary owns the GPU thread's handle and joins
render, surface and the thread (`run/gpu.rs`). `strand-surface` hands
out raw Wayland handles (`raw-window-handle` types) under its `gpu`
feature and never depends on `strand-gpu`. `naga` is also a direct
dependency of `strand-compiler` (feature `shaders`), for check-time
parsing and uniform reflection: the same naga version wgpu uses, so the
binary links one copy (a second version in `Cargo.lock` is a merge
blocker). The `strand` binary's default feature `gpu` turns on render's
`gpu`, surface's `gpu` and the compiler's `shaders`; `strand-dev` turns
on `shaders` so the LSP checks shaders too. Built with
`--no-default-features`, `strand` links none of wgpu, vello_gpu, naga or
libwayland-client: that build is the CPU core the 15 MiB gate measures.

```
strand-scene <── strand-gpu (M4: wgpu, vello_gpu, vello_common, raw-window-handle)
                   ^
                   strand-render [gpu]      strand-surface [gpu]: raw handles,
                   strand (binary) [gpu]       wayland-backend client_system
strand-compiler [shaders] ──> naga (check-time; same version as wgpu's)
```

`strand-icons` (wave 4, a3) is the Icon Theme Specification lookup with a
cache `invalidate()` refreshes (`lookup(name, size, scale, theme)`,
`candidates(name)` and `resolve(name, size, scale, theme)`, the one
candidate chain: the name, its `-symbolic` variant, then the generic
fallbacks; `exists` follows the same chain; `system_theme`, `base_dirs`, `theme_setting_files`,
`generation`, and `set_desktop_theme(Option<String>) -> bool`, the
portal's theme name, preferred to GTK's settings files and invalidating
when it changes); the renderer and the `apps` service both look icons up
through it. `strand_services::icon_theme::spawn(&Services, switched)`
follows the settings portal's `org.gnome.desktop.interface` `icon-theme`
as a task of the shared services runtime, on its shared session
connection (`Services::spawn_task`, crate-private), and feeds
`set_desktop_theme`; `strand run`'s logic thread keeps the returned
`Follower` with the real services (`run::Live::icon_theme_switched` is the
main thread's callback) and has `switched` redraw icons as an
`index.theme` change does. `strand-introspect` reads an object's properties from its
D-Bus introspection (`properties_on(conn, name, path)` async,
`properties(&Bus, name, path)` blocking with a 2 s bound over one
connection per bus kept for the process (on a current-thread runtime
with no thread of its own, so an idle connection costs no wakeup; made
again when it breaks or hangs), `parse(xml)`,
`default_path(name)`, and `Cache`: answers remembered for `TTL` (10 s),
`properties` blocking, `properties_or_ask` answering from what it
remembers and asking on a thread of its own): what `from dbus` services
are checked against (`strand check`, the loader, the LSP) and what the
running service reads signatures from. The compiler's
`check::dbus::Introspect::properties` answers `None` while a question is
out (the LSP's, which never waits on a bus: an unanswered service is
unchecked until the answer wakes the server to publish again).

`strand-scene` has no heavy dependencies; it is what lets render and surface
be built and tested without the language, and the language without pixels.

### `strand-render` module map

The render crate's large files are directories of modules (M4 F0, a
split with no behaviour change), so streams that work in parallel own
disjoint files. Each directory's `mod.rs` re-exports what the rest of
the crate used, so paths such as `crate::flatten::pick` are unchanged.

- `renderer/`: `mod.rs` (the `Renderer` struct, `SurfaceState`, tuning
  constants, widget and asset hooks), `frame.rs` (frame holds and
  deadlines, damage diffing, `Painter`), `wake.rs` (the loop's timer
  thread, `next_wake`, `update`), `text.rs` (`TextBackend`, text slots,
  requests, delivery, pruning), `layout_pass.rs` (the layout step, size
  springs and FLIP, layout facts, flattening), `apply.rs` (scene diffs
  and the motions they start), `specs.rs` (surface specs, content
  sizing, size holds), `surfaces.rs` (attach, configure, detach, hit),
  `pose.rs` (exit poses and closing surfaces), `lists.rs` (scrolling),
  `tooltip.rs`, `swap.rs` (theme swaps), `tests.rs`. The M4 plan's
  `feed.rs` (effects) and `backend.rs` (lowering to `strand-gpu`'s
  frames, readback delivery) have no code yet: their streams create
  them, with `promote.rs` (the promotion state machine) and `canvas.rs`
  beside `renderer/`.
- `flatten/`: `mod.rs` (display list types, `flatten`, `Flattener`),
  `node.rs` (one node: box, paint, shadows, text, clips, children),
  `text.rs`, `paint.rs`, `hash.rs`, `widget.rs`, `image.rs`, `tests.rs`.
- `raster/`: `mod.rs` (`Raster`), `atlas.rs` (`AtlasMirror`), `paint.rs`
  (scene paints as vello paints), `draw.rs` (drawing a display list,
  disjoint damage), `tests.rs`.
- `anim/`: `mod.rs` (`Animator` and its per-frame `paint`), `motion.rs`
  (channel encoding, `PropMotion`), `pose.rs` (enter/exit poses),
  `sizes.rs` (size springs), `tests.rs`. Keyframes, morph, stagger and
  page slides get modules of their own here when they land.
- `layout/`: `mod.rs` (the pass, `Boxes`, `RootSize`, prop helpers),
  `style.rs` (a node's taffy style), `text.rs` (`TextSizes`, leaf
  measuring), `list.rs` (`ScrollState`, list virtualisation).

## Contracts

### `strand-scene`

- **Geometry**: `Rect { x, y, w, h }` in physical pixels as `i32`/`u32`,
  `Size`, `Point`, and a logical-pixel `f32` variant; `Scale` is the
  fractional scale as numerator/120 (`wp_fractional_scale_v1`).
- **Colour**: `Color` stored as straight-alpha sRGB `f32`, with exact
  conversions to OKLab/OKLCH; interpolation for springs happens in OKLab.
- **Motion** (`strand_scene::motion`, shared by the renderer's prop
  springs and the theme's palette springs): `Spring { stiffness, damping }`
  is a unit-mass damped oscillator (`spring(700, 0.9)`: stiffness and
  damping *ratio*), `Spring::step(x0, v0, t) -> (x, v)` its closed form;
  `SPATIAL`, `EFFECTS`, `BOUNCY` are the design's `$motion.*` springs.
  `Curve` (`Instant`, `Spring`, `Timed { duration, easing }`) is a
  resolved transition (`Curve::of(&Transition)`), `ease(Easing, p)`
  evaluates an `Easing`: `Linear`, a CSS cubic `Bezier`, or the closed
  forms `OutElastic` and `OutBounce` (easings.net), which no bézier can
  draw. `Easing::named` knows exactly `Easing::NAMES`: `linear`,
  `standard`, `ease`, `ease_in`/`in`, `ease_out`/`out`,
  `ease_in_out`/`in_out`, `in_back`, `out_back`, `in_out_back`,
  `out_elastic`, `out_bounce`, `emphasized`, `emphasized_decelerate`,
  `emphasized_accelerate`. The checker's `enum Curve`
  (`strand-compiler`'s `builtin.schema`) must stay a subset of that
  list, so a name it accepts never runs as `STANDARD` (compiler
  owner: add `in_out_back`, `emphasized_decelerate`,
  `emphasized_accelerate` and the aliases there, or keep them out).
  `Motion<N>` is an `N`-channel value in flight:
  `rest(value, eps)`, `retarget(target, curve)` and `shift(delta, curve)`
  (a FLIP jump) take effect at the next `sample(at)`, which starts them
  at `at` less one frame (at most `START_LEAD`, never before the previous
  sample: `sampled_at(prev)` seeds it) from wherever the value is there,
  *keeping its velocity* (a spring starts with it; a timed curve adds
  it as `v0·t·(1 − t/d)²`, which fades out by the end of the duration);
  `retarget_at` starts at a given time;
  `peek(at)`, `velocity(at)`, `is_settled(at)` read without starting
  anything. Everything is a pure function of the timestamps sampled, so
  frames are testable as images. `color_channels`/`channels_color` map a
  colour to premultiplied OKLab plus alpha. `TokenScope::transition`
  falls back to `SPATIAL`/`EFFECTS`/`BOUNCY` when the table has no
  `$motion.*` token of that name.
- **Damage**: `Damage` is at most 8 `Rect`s; adding a ninth merges the pair
  whose union grows area least. `Damage::area()` is what the M0 exit
  criterion (≤2,000 px² per clock tick) is measured on.
- **Painter** (render implements, surface calls):

  ```rust
  pub struct PaintTarget<'a> {
      pub pixels: &'a mut [u8],   // ARGB8888 premultiplied, little-endian (wl_shm)
      pub size: Size, pub stride: u32, pub scale: Scale,
      pub age: u8,                // buffer age: 0 = unknown contents, 1 = last frame, ...
      pub time: Duration,         // predicted presentation time (wp_presentation clock)
  }
  pub trait Painter {
      /// Paint everything that changed for `surface` and return the damage,
      /// already widened to cover the buffer's age and clipped to the buffer.
      fn paint(&mut self, surface: SurfaceId, target: &mut PaintTarget<'_>) -> Damage;
      /// True while something is dirty or a spring or time signal on this
      /// surface is unsettled; the surface manager requests frame callbacks
      /// only while true.
      fn wants_frame(&self, surface: SurfaceId) -> bool;
      /// Fully opaque part of the last painted frame, in buffer pixels.
      fn opaque_region(&self, surface: SurfaceId) -> Damage { Damage::new() }
      /// Rounded boxes of nodes with `blur`, in buffer pixels, with their
      /// radius: what the blur ladder's `ext-background-effect-v1` rung
      /// (M4) sends. Render draws the tint fallback (alpha + 0.15) until
      /// `Renderer::set_compositor_blur(true)`.
      fn blur_region(&self, surface: SurfaceId) -> Vec<BlurRegion> { Vec::new() }
      /// (M4) The pose the compositor should apply to the whole surface
      /// this frame, when render delegates its root's pose (see "M4
      /// vocabulary"); `None` means identity.
      fn surface_pose(&self, surface: SurfaceId) -> Option<SurfacePose> { None }
  }
  ```

  Buffer-age rule: a non-empty `paint` result is a new frame and the
  caller must commit that buffer with exactly that damage; an empty result
  means nothing was drawn or recorded, so the caller does not commit (or,
  if it commits anyway, does not count it). `age` counts commits of that
  surface. If a painted buffer cannot be committed, call
  `Renderer::invalidate(surface)`. `PaintTarget::new` sets `time` to zero;
  the surface manager sets it (`.at(t)`) and tests pass fixed values so
  springs sample deterministic timestamps. `opaque_region` is in buffer
  pixels; `wl_surface.set_opaque_region` takes surface-local logical
  coordinates, so convert with `Scale::inner_logical_region`, which rounds
  inward and never claims a translucent pixel. The input region is not a
  painter question: render puts the shadow reach into the surface's spec
  (`SurfaceSpec::overhang`) and the surface manager sets the region to
  the box inside it.

- **Input**: `InputEvent` (`PointerEnter`/`Leave`/`Motion`/`Button`/`Axis`
  with `ButtonState`, `AxisDelta`, `AxisSource`; `KeyboardEnter`/`Leave`
  and `Key { key: KeyInput }` with the xkb keysym name, the typed text,
  `Modifiers` and repeat; `ClickAway { surface }`, a press on the
  click-away catcher under an open `keyboard: exclusive` surface whose
  `open` is two-way, `SurfaceSpec::open_two_way` from `Prop::TwoWay`) in
  surface-local logical pixels, per `SurfaceId`. `strand-surface` produces it; render hit-tests
  it on the main thread and forwards node events to logic. Wayland
  serials stay in `strand-surface`.

- **Scene protocol** (logic → render, one batch per tick): `SceneDiff`
  holding ordered `SceneOp`s over a retained tree: `Create { id, kind,
  parent, index }`, `Remove { id }` (render plays `exit` before unmounting),
  `Move { id, parent, index }`, `SetProp { id, prop, value, transition }`,
  `SetTokens { table, transition }` (logic sends `Instant` for the table
  it boots with and `Default` for later ones, whose palette roots render
  springs: Theme swaps, below). Node ids are generational; a removed id is dead
  at once (logic may reuse the slot with a new generation in the same
  diff). `Move`'s `index` counts the new parent's children after the node
  is detached. Prop values are typed (`Length`, `Color`, `Paint`, `Text`,
  `Shadow`, ...); comma shorthands (`margin: $space.2, $space.2, 0`,
  `radius: 14, 14, 0, 0`) may arrive as a `List` of 1–4 values, expanded
  like CSS, and call-shaped values (`hit: grow(6)`, `filter:
  grayscale(1)`, `backdrop: blur(16)`, `transition: wipe(left)`) are
  `PropValue::Call { name, args }`. A prop naming another node (`nav:
  results`) is `PropValue::Node(id)`. A surface's declared name (`bar Top`)
  is `Prop::Name` (`Text`), set by the compiler. `transition` is `Default` (the token spring for that
  prop class), `Token(path)` (`~ $motion.bouncy`), `Spring { .. }`,
  `Duration { .. }` or `Instant`, matching `~` in the language;
  `TokenScope::transition` resolves the first two through `$motion.*`
  tokens at render time (props of class `Snap` always snap). `Create`/`Move` take `parent: Option<NodeId>`
  (`None` for surface roots) and `PropValue::Unset` reverts a prop to its
  default. Render maps a surface root to Wayland surfaces with
  `Renderer::attach_surface(SurfaceId, NodeId)`. A surface-kind node
  created under a parent (a `popup` in a `bar`) is its own root: it
  inherits tokens, colour and font from its ancestors but paints only on
  its own surface. `radius: full` is `Corners::FULL` (infinite radii) or
  `PropValue::Keyword("full")`; a `%` radius is of the shorter side.
  Token-bound values travel unresolved as `PropValue::Token(TokenExpr)`
  (`$path`, colour methods `alpha`/`mix`/`lighten`/`darken`,
  `oklch(from …)` with channel arithmetic, and `Template` for composite
  values whose colours are tokens, such as `border: 1, $border`), also
  nested inside a `List`, `Pose` or `Call` (`pad: 0, $space.3`), which
  `TokenScope::resolve` resolves in place. The
  `TokenTable` sent by `SetTokens` holds plain values (palette roots,
  scales, fonts, `PropValue::Transition` springs for `$motion.*`) and
  derived tokens as expressions; render evaluates references at flatten
  time, every frame, so only palette roots need to spring. Derived
  colours are gamut-mapped (`Color::gamut_mapped`, CSS Color 4). The
  table also carries the contrast guard's declared pairs
  (`TokenTable::contrast`: a text token and its background tokens, the
  Material 3 `on_X`/`X` pairs and `fg` over the surfaces): wherever a
  text token is evaluated, its lightness is solved to 3:1 over its
  backgrounds in that node's scope (`Color::with_contrast`, memoised
  per text/background colours on the render thread, so a frame solves
  each pair once), so a palette mid-spring and subtree overrides stay
  readable; whether any lightness can is `Color::contrast_reachable`
  (`strand_scene::luminance_reachable` over luminances, conservative
  above `REACH_MAX`). `TokenTable::origins` (path → `palette:<source>`, `base`,
  `tokens <set>`, `component <Name>`) is provenance for the inspector;
  evaluation never reads it. Text that names no `color`/`font` draws in
  `$fg`/`$font.ui` looked up in its own scope, and a `bar` that names no
  `bg` paints `$surface`. Logic still
  resolves which theme applies. Subtree overrides (`set { $x: … }` and a
  component's `tokens { }`, as `Toast.radius`) are the `tokens` prop
  holding a `PropValue::Tokens` table; render resolves through a
  `TokenScope` chain (global table, then each ancestor's override,
  nearest first). An override's right-hand side sees its parent scope
  (`set { $surface: $surface.alpha(0.5) }` is not a cycle) and global
  derived tokens are evaluated in the asking node's scope, so they stay
  derived inside the subtree. `TokenTable::freeze()` evaluates every
  token of a table once in its own scope and keeps the values; a lookup
  in a scope whose global table is frozen (and an override's right-hand
  side reading it) reads them. Render freezes the tree's table when a
  `SetTokens` lands and in each frame a swap moves the roots; a clone is
  not frozen, equality ignores it, `insert`s thaw it, and a direct write
  to its pub fields needs a `freeze()` again (or `thaw()`). `enter`/`exit` are props whose
  value is a `PropValue::Pose` (prop/value pairs) or a preset keyword.

- **Surfaces**: `SurfaceSpec` (in `strand-scene`) is what a surface-kind
  node asks of Wayland: kind, name (namespace `strand-<Name>`), edge,
  anchor, layer, keyboard, margin (`Insets`), requested logical width and
  height, `screens` (`All`, `Focused`, or `Named` monitor identities, which
  logic uses to pin each per-monitor `bar` instance), `open` and `attach`,
  `overhang` (how far shadows reach past the box, filled in by render
  from layout), plus `exclusive_zone()` and `needs_recreate()` (kind,
  namespace or layer changed). A surface without a size of its own (a
  panel, OSD or popup without `width`/`height`, a bar without a
  thickness) gets it from a content layout pass in render before the spec
  is reported. Render resolves specs through the node's token scope after
  every `apply`: `Renderer::surface_spec(node)` reads one, and
  `Renderer::take_surface_changes()` returns `(NodeId, SurfaceChange)`s,
  `Created(spec)`, `Updated { spec, recreate }` or `Removed`, in order;
  token changes that move a resolved value count as updates.
- **Layout**: taffy 0.14 on the render thread (`layout/`): one pass
  per surface whose layout inputs changed (paint-only props never
  relayout; `Renderer::layout_passes` counts them), boxes in surface
  logical pixels (`Renderer::boxes`), `x`/`y` applied at flatten time as
  paint offsets. `Renderer::scroll(surface, point, dy)` scrolls the
  innermost `scroll`/`list` under a point and `scroll_into_view(list,
  row)` reveals a row; a `list` lays out only the rows in view. Text is
  measured from delivered layouts (estimated until the first arrives).
- **Animation** (`anim/`, on the render thread): the render thread owns
  every spring. A prop of `ANIMATED` (`x`, `y`, `opacity`, `scale`,
  `rotate`, `bg`, `color`, `border`, `shadow`, `radius`, `value`,
  `track`) that logic sets
  on a node of a surface shown with a clock springs from its old value
  along `TokenScope::transition(prop's ~, prop)`; `width`/`height`/`size`
  spring the laid-out size (laid out at rest to learn the target when a
  change starts them, then each frame once with the in-flight size
  forced, and while only size springs move, only the subtree under the
  nearest size-stable ancestor, a node of fixed px width and height, is
  laid out: `Renderer::last_layout_nodes`). Other
  layout lengths snap and the boxes they move glide (FLIP), as do the
  siblings of created, removed and moved nodes and every box after a
  `SetTokens`; text changes never glide. A target changed by tokens or
  inheritance snaps at rest and steers a motion in flight. `scale` and
  `rotate` (a `PropValue::Angle` in degrees, as the compiler sends it
  and every rotate sample is; a bare number reads as degrees) draw the
  subtree under `Item::PushTransform`, and its hit shape is the
  untransformed rounded box tested through the inverse transform
  (`HitBox::inverse`). `enter` plays
  for a node created on a shown surface, for a node created by the diff
  that opens its surface (a surface reported closed before, not one
  first seen in that diff: the first toast and `open: shown.len > 0`),
  and for a surface whose `open` becomes true; `Remove` of a laid-out node with an `exit` (or `enter`)
  pose turns its subtree into a ghost (`SceneTree::ghost`: dead to
  logic, its slot free at once, kept in its parent's children and laid
  out and drawn, never hit; the input `Router` drops its focus and
  selection at once) that unmounts when the pose settles, and a
  surface whose `open` goes false stays open in its spec until its exit
  pose settles, or, with no exit pose of its own, until the ghosts under
  it have unmounted (the last toast leaving as the panel closes).
  `Remove` of a node with no pose of its own under a surface closing
  with a pose (its `open: false` in the same diff, or its exit playing)
  makes it a ghost too, drawn at rest and unmounted when the surface
  closes or opens again: logic unmounts a `popup`'s content when it
  closes, and the popup must not play its exit empty.
  Motion state is per node: when one root is shown on several surfaces
  (`screens: all`), a frame ends an exit or drops an enter only for a
  node no other surface of that root drew (in its last frame's records
  or motions). Exits are bounded: a parent keeps at most
  `MAX_GHOSTS_PER_PARENT` (8) ghosts (a new one ends the oldest), and an
  exit on a surface that painted nothing for `EXIT_STALL` (1 s; its
  output asleep) or older than `MAX_MOTION` + 1 s ends at the next
  `apply`/`update`; `Renderer::next_wake() -> Option<Instant>` is the
  earliest such instant (or a tooltip's due time). The renderer arms its
  own timer thread at it after every `apply`, `update` and paint (a
  later instant than the one armed waits for the earlier wake to
  re-arm; nothing left cancels it) and wakes the loop through the text
  worker's waker, whose handler in `strand run` runs `update` and hands
  the surface changes on, so a closing surface whose output stopped
  sending frame callbacks still closes. A host with no waker arms a
  timer at `next_wake` itself. Another surface's last frame counts as having drawn
  a node only while that surface still paints (within `EXIT_STALL`). A node created under a ghost's id replaces the
  ghost. A surface reported closed and then open is opening until
  its first clocked frame: nodes created under it meanwhile enter too.
  A size springing to or from zero folds its padding and the parent's
  gap beside it, so the slot reaches zero (decisions.md, wave3-pixels
  (p2) fixer round 3). A content-sized surface that grows under an
  anchor moving its origin glides its root's children from where they
  were on screen. A content-sized surface never shrinks while something on it
  moves, and asks for its own size once nothing does. The paint that
  finishes an exit (a ghost unmounted, a surface closed) or lets a held
  surface shrink refreshes the specs itself, so `has_surface_changes()`
  is true after it: the host must check it after every `paint` (the
  demo host pings its loop, which runs `update` and hands the changes
  on). A surface just attached previews at time zero (no motion), so
  its first painted frame flattens afresh while any motion on it waits
  to start: `enter` plays from that frame. Lengths resolve against the laid-out boxes before
  they spring (`radius: full` is half the shorter side, a percentage
  `x`/`y` is of the parent's box). Not yet animated: gradients, `mark_color`
  (span colours are part of the text shaping request, so a spring would
  reshape every frame) and the props of effects still to be drawn
  (`stroke`, `fill`, `trim`, `glow`, `blur`); they join `ANIMATED` when
  they render. Presets: `fade`, `slidefade`, `popin(s)`, `slide(edge)`.
  `PaintTarget::time` zero (no clock, offline) and `reduced_motion`
  (`Renderer::set_reduced_motion`, or the global token `motion.reduced:
  true`) snap everything, size springs already in flight included (at
  the next frame). Where it comes from: the host maps the portal's
  `org.freedesktop.appearance` `reduced-motion` key (a
  `SystemSetting`, strand-watch) to `Renderer::set_reduced_motion` and
  to the `system.reduced_motion` value logic reads; a theme writes
  `motion { reduced: system.reduced_motion }` (or a settings field) to
  reach render through the token table
  (`crates/strand-render/tests/reduced_motion.rs`). `strand run` reads
  `system.reduced_motion` (the `system` service's, from the portal's
  `SystemSetting::ReducedMotion`) after every step, and sends render each
  change of it with the next diff (`SceneDiff::reduced_motion`, applied
  as `Renderer::set_reduced_motion`). `Painter::wants_frame` is true while anything
  moves (`Renderer::animating`), so frame callbacks stop once it
  settles.
- **Theme swaps** (`renderer/swap.rs`, on the render thread): a
  `SetTokens` with a non-`Instant` transition, while a surface is shown
  with a clock and motion is not reduced, springs every plain colour of
  the new table that differs from the one on screen (the palette
  roots) in premultiplied OKLab along `TokenScope::transition(t,
  Prop::Color)` (`$motion.effects` for `Default`); every other plain
  token snaps. Each frame writes the roots' values at its presentation
  time into `SceneTree::tokens` before flattening (gamut-mapped; a
  settled root gets logic's exact colour), so derived tokens and the
  contrast guard are evaluated from them exactly; a newer table
  retargets roots in flight with their velocity, and the table is
  frozen again from the frame's roots (the frame's nodes read it). Span
  colours (marks, markup links) are not part of a text's shaping
  request: it carries slot stand-ins and the glyph item the colours, so
  a springing `$accent` never reshapes. Before springing, the planned
  roots are played through (240 Hz while they move fast, up to 100 ms
  steps while they move slowly, four times as finely next to moments
  under 3.3:1, until they settle, at most 10 s and a fixed work budget)
  in the global scope and under each `set { }` scope of the shown nodes
  whose overrides reach a declared background (merged by those
  overrides, at most 32). Where a declared pair readable at both ends
  has a moment with no text lightness at 3:1
  (`Color::contrast_reachable`): in the global scope (or when the check
  cannot finish) the table snaps and every shown surface crossfades;
  under a `set { }` scope (or when the scopes' check cannot finish,
  the global scope having been played through first) only the surfaces
  drawing it crossfade, shown
  the new table at once (held for them, swapped into the tree while they
  lay out and flatten) while the roots spring for the rest. A scope
  that appears while the roots spring (a node given `tokens` or moved by
  a later diff, a surface attached) is played through then, from the
  roots' motions as they are, and held the same way (a surface attached
  mid-swap has no old frame: it just shows the new table). A crossfade
  goes from a snapshot of the surface's old frame to the new frames
  along the colour curve from that surface's first frame, those frames
  painted in full and reporting no opaque region. The snapshot is taken
  at that first frame: the `PaintTarget`'s copy with the damage of the
  frames its age missed drawn again from the old display list kept from
  planning (in full for a new or invalid buffer); at most 1920×1080×4
  bytes per surface and in all (a larger surface snaps). A crossfade
  landing mid-crossfade takes the blend on screen as its snapshot; a
  surface that paints nothing for the exit stall loses its snapshot; a
  table that changes no colour leaves fades running, a snapping one
  ends them. The blend works on the CPU `PaintTarget`; on a surface
  presented by the GPU (M4) the GPU thread keeps the old frame's texture
  and blends in a shader along the same curve (`strand-gpu`'s
  crossfade); a surface in readback mode blends on the CPU as today.
  `Renderer::swapping()` is true while roots spring or a crossfade
  runs on a surface still painting, `swap_crossfades()` counts swaps
  that crossfaded somewhere, `swap_held()` (hidden) lists the surfaces
  shown the held table, and `take_swap_work()`
  and `take_fade_blend_work()` (hidden, for the bench) return the
  render thread's swap work, and the time spent blending, since the
  last call.
- **Hit testing**: `Renderer::hit(surface, LogicalPoint) -> Vec<NodeId>`
  is the node under a surface-local logical point in the last frame (the
  topmost in paint order: later siblings over earlier ones and their
  children) and its ancestors up to the surface's root (the root alone
  where nothing is). A node is hit inside its laid-out rounded box, grown
  by `hit: grow(n)` and cut by its ancestors' clips; shadows never count.

- **Render loop** (the binary wires this; surface calls `Painter`):
  0. After each `apply`, drain `take_surface_changes()` and hand them to
     the surface manager: create a layer surface (or xdg_popup, lock
     surface) per matching output for `Created`, reconfigure or recreate
     it for `Updated`, destroy it and `detach_surface` for `Removed`.
  1. Spawn the text worker with `TextWorker::spawn_with_waker(config,
     Some(waker))`, where the waker pings the main calloop loop, and
     build `Renderer::new(TextBackend::Worker(worker))`. `Renderer::text()`
     returns the backend; `TextWorker::is_running()` is false once the
     worker thread has ended, which with the handle still held means it
     panicked (shaping panics are caught and the engine restarted).
  2. When the ping fires, call `Renderer::update()`: it collects
     delivered layouts and marks the surfaces they change dirty. Without
     this, changed text reaches the screen only with the next unrelated
     event.
  3. Call `apply(diff)` once per logic tick with the `SceneDiff` received.
  4. On output and configure events call `attach_surface`,
     `configure_surface(size, scale)` (before the first paint, so text is
     shaped ahead of it) and `detach_surface`; these also free per-scale
     atlases and text no surface uses.
  5. Request a frame callback while `wants_frame(surface)`, and in it call
     `paint`; afterwards, if `has_surface_changes()`, run step 2 (an
     exit that finished closes a surface or shrinks it with no other
     event). Commit only a non-empty result, with exactly that damage
     (`damage_buffer`) and the converted `opaque_region`; if the commit
     fails, call `invalidate(surface)`. Text still being shaped does not
     keep `wants_frame` true: the delivery does, through step 2. Text
     layouts are per (node, scale, line box width), since `center`/`end`
     alignment happens in the line box. A surface that has never painted
     (or whose text worker restarted) holds its first frame for its own
     layouts, up to the first-frame wait (default 50 ms, set with
     `Renderer::set_first_frame_wait`; the demo uses 500 ms): if
     `frame_deadline(surface)` is `Some(t)`, arm a timer for `t` and check
     `wants_frame` again then. A surface that has painted holds a frame
     the same way while some text on it has no layout at any scale or
     width (a node just added), up to `NEW_TEXT_WAIT` = 16 ms (set with
     `Renderer::set_new_text_wait`), so a new node and its glyphs reach
     the screen in one frame; only while the surface is idle (no frame
     painted within `BUSY_WINDOW` = 34 ms, counted from when its last
     raster finished; `Renderer::set_busy_window` changes it, for tests):
     a surface in motion paints at once and the glyphs follow a frame
     later (decisions.md, wave2-exit). A surface that has painted draws, while its
     layout is re-shaped, a stand-in from another scale or width,
     resampled and shifted so its alignment lands where the right one's
     will; layouts no surface wants are pruned.

- **Widgets and paint** (decisions.md, wave3-pixels (p3)): widget state
  the router owns but widgets draw (hover, press, focus, an `input`'s
  caret and selection, a slider's value while dragged) lives in render
  (`strand_render::widgets::Widgets`, `Renderer::widgets`), written by the
  router through `InputScene` (`set_flag`, `set_caret`, `set_drag`;
  `node_rect` and `caret_at` read layout and the last frame), so widgets
  answer on the frame the input arrives; logic still gets flags and
  two-way writes (`value` of a slider or `segmented`, `text` of an
  `input`). A node may shape several texts (`TextSpec::part`: a
  `segmented`'s labels). `icon`/`image` sources decode at the box's
  physical size into a 6 MB LRU (`strand_render::image`, freedesktop icon
  theme, PNG/JPEG/SVG; JPEG IDCT-scaled and PNG reduced row by row so a
  decode holds about the drawn size), on a worker with a text worker,
  inline offline; while a size springs the latest decode draws placed
  by its fit (`Decoded::placed_in`, `Item::Image::dest`). Gradients draw
  from dithered pixmaps and shadows from cached ones (a 4 MB paint
  cache, entries unused for `IDLE_FREE` (10 s) freed at the next paint
  or the next `Renderer::update`, whatever woke the loop: nothing wakes
  just to free them, so an idle shell does zero work between ticks); a
  text node's record keeps its glyph cells (`NodeRecord::glyphs`), so a
  change that only swaps glyphs damages those glyphs (a clock tick
  repaints its last digit); a gradient is cached
  only when a second frame draws the same paint at the same size, so one
  whose paint or size changes every frame is dithered cell by cell,
  uncached. An empty `text` lays out as 0 × 0. `marks:` arrives as a list of `[start, end]` pairs: the
  compiler's scene conversion turns a `Range` record into that pair. A `popup`'s spec gets `parent` and `anchor_rect` (its
  element's laid-out box in the parent surface); `tooltip: expr` makes a
  render-owned popup (`SceneTree::add_overlay`, ids from `OVERLAY_INDEX`)
  after `TOOLTIP_DELAY` of rest, reported as a spec with `tooltip: true`;
  `Renderer::next_wake` includes its due time.
- Text is shaped once without a width bound per (node, scale) and
  aligned in its box at flatten time; only a box narrower than it asks
  for a layout of its (whole-pixel) width (decisions.md, wave3-pixels).

- **M4 vocabulary** (docs/m4-plan.md). S-runtime landed these types in
  wave 0b, with stubs where behaviour is pending, so the streams build
  against them; `strand-compiler/tests/scene_catalogue.rs` checks them
  against builtin.schema (decisions.md, m4-scene). `Prop`
  stays a `Copy` enum (decisions.md, 2026-10-05 render). The GPU
  backend negotiation, the shader ABI and `Effect::Shader` are in
  "`strand-gpu`" (wave 0c).
  - **Time-bound values** (`t`, `wave(…)`, `noise(…)`; design.md,
    "Motion and time"). `TokenExpr` gains the leaves `Time` (`t`: seconds since
    the node appeared), `Wave { period: Duration, phase: Box<TokenExpr> }`
    (builtin.schema's `wave(period, phase: 0)`), `Noise(Box<TokenExpr>)`
    (`noise(x)`), and `Index` and `Count` (a `letters` letter's `index`
    and the letters' `count`). Arithmetic on them is the existing
    `Binary`. `Template` gains `numbers: Vec<Option<TokenExpr>>`, slots
    filled like its colours in `PropValue::numbers_mut` order (field
    order: a gradient's angle or `from` before its stops' offsets, a
    border's width before its paint), so `glow: 10 * wave(2s),
    $accent.alpha(0.4)` and `conic(from: t * 40deg, …)` travel as one
    value. A scope reads a per-node `TimeContext { t, index, count }`
    through `TokenScope::with_time(Option<TimeContext>)` (`resolve`,
    `eval` and `lookup` keep their signatures; an override's right-hand
    side keeps the node's time). Every leaf is evaluated at the context's
    `t`; without one `t`, `index` and `count` read 0. Under
    `reduced_motion` render passes `TimeContext::frozen(index, count)`:
    the clock stops at `t = 0` and a letter keeps its `index` and
    `count`, so time signals hold their value at `t = 0` and per-letter
    layout (`x: index * 8`) stays. `wave` is `0.5 − 0.5·cos(2π(t/period
    + phase))` (0 to 1, 0 at `t = 0`, phase in periods) and `noise` 1-D
    gradient noise in −1..1, 0 at whole `x`
    (`strand_scene::tokens::noise`). A prop holding a time leaf is
    frame-driven (`PropValue::reads_time`, `TokenExpr::reads_time`;
    `noise(x)` only when `x` reads `t`): its node repaints each frame of
    its clock while visible, and only it.
  - **Per-node clocks with frame caps.** A node that reads time or plays
    frames (an animated image at its own rate, `grain` at 12 fps,
    `shimmer` at 30, others at refresh) has a clock; a clock that is not
    due by the next frame leaves `wants_frame` false and puts its due
    instant into `Renderer::next_wake()`, so the surface needs no new
    call. Hidden nodes' clocks stop, and the frame loop stops when every
    clock is idle.
  - **Shader uniforms.** A node's `u_*` props travel as one
    `Prop::Uniforms` holding `PropValue::Uniforms(Vec<(String,
    PropValue)>)`, sorted by name as written (`u_speed`). Each entry
    springs as a prop of its value's type would. The checked WGSL travels
    as `Prop::Shader` (`PropValue::Shader(Arc<ShaderCode>)`), set by the
    compiler and never written in source; the ABI is in "`strand-gpu`".
  - **Canvas.** `Prop::Draw` holds `PropValue::DrawList(Arc<[DrawOp]>)`,
    what the VM recorded running `draw: (c) => …` (re-run when what it
    read changes; `c.width`/`c.height` come from layout facts). `DrawOp`
    is the renderer's own paint vocabulary in a canvas-2D-like state
    model (`strand_scene::canvas`): 0b landed one variant per method of
    builtin.schema's `Canvas` record (`Line`, `Rect`, `Circle`, `Fill`,
    `Stroke`, `Text`; `DrawOp::METHODS`, checked by the catalogue test).
    It is `#[non_exhaustive]`: transforms and clips are added with the
    canvas work.
  - **Effect layers** (design.md, "Runtime changes these need", items 1–4).
    `strand_scene::effect::Effect` is a tagged group effect:
    `ColorMatrix([f32; 20])` (the `filter:` colour functions compose into
    one, `effect::compose_matrices`), `Blur { radius }` (a standard
    deviation, as CSS's `blur()`), `Blend(BlendMode)` (`screen`, `add`,
    `multiply`, `overlay`, `difference`), `Mask(Mask)` (`Fade { edge, len
    }`, `Radial { at: Anchor, size }`, `Shape(name)`), `Opacity(f32)` and
    `Shader(ShaderPass)` (a bundled GPU effect or a `.wgsl` file's pass;
    see "`strand-gpu`"). `Effect::reach(scale) -> Insets` is how far it
    spreads damage in logical pixels on a surface at `scale`: three
    standard deviations for a blur, a bundled pass's own (its uniforms
    are buffer values, so divided by `scale`), nothing for the rest. Render's display list gains `Item::Layer { effects, bounds,
    items }`, a group whose damage grows by its effects' reach, lowered by
    each backend its own way (vello_cpu `push_layer`; masks always on the
    CPU), and `Item::Raster { node, bounds }`, a CPU raster node
    (particles, grain, graphs, spectrum, animated image frames) drawn into
    a cached pixmap at its clock's rate. Cached offscreen groups (glows,
    filtered subtrees, glass sources) redraw only when their children
    change, in a second 4 MB budget freed when idle. The props keep
    arriving as `PropValue::Call`; render builds the `Effect`s.
  - **SVG parts.** An `svg "icon.svg" { #needle { rotate: … } }` selector
    block is a child node of kind `NodeKind::SvgPart` (`svg_part`, the
    one kind with no schema element) carrying the id it
    selects as `Prop::Name` (`Text`, without the `#`) and ordinary props,
    which render applies to that layer (decisions.md, 2026-10-05 render).
  - **Keyframes.** `Prop::Play` holds `PropValue::Keyframes(Arc<Keyframes>)`
    in place of `[name, seq]`: `Keyframes { name, seq: u32, stops:
    Vec<(f32, Vec<(Prop, PropValue)>)>, duration, delay, repeat:
    Option<u32> (None forever), alternate, easing }`,
    the compiled `keyframes` block inline (stops as fractions, settings as
    `check::keyframe_settings` allows), so render keeps no keyframe table.
    Keyframes are offsets composed with the node's springs; a new `seq`
    restarts them.
  - **Virtualised lists.** On a `list` whose direct child is a `for`,
    logic sets `Prop::RowCount` (`Number`: all rows) and `Prop::RowFirst`
    (`Number`: the global index of the first mounted row). `SceneOp::Create`
    and `Remove` gain `window: bool`, true for a row the list window mounts
    or unmounts: render plays no `enter`, `exit` or FLIP for it (until
    S-lists does that, render mounts such rows like any other).
    `SceneDiff::create` and `SceneDiff::remove` send `false`. Render
    lays rows out at their global indexes (unmounted rows keep their
    extent), scrolls by a paint offset with no relayout, and reports the
    rows it wants (view plus overscan) with `Renderer::take_list_windows()
    -> Vec<(NodeId, Range<u32>)>`, which the binary sends as
    `ToLogic::ListWindow`.
  - **Drag and drop** (design.md, "Drag and drop"). `Prop::Drag` on a
    source reaches render as `PropValue::Keyword` naming the type of the
    dragged value; `Prop::Accepts`, set by the compiler and never written
    in source, is a `List` of `Keyword` type names the node's `on drop`
    takes (`Drop` for other programs' drops, `any` for everything).
    `DropPayload` lives in `strand-scene`, because `strand-surface`
    produces the external one: `Node(NodeId)` (a `drag:` source in Strand;
    logic maps it back to its value) or `External { kind: DropKind, files:
    Vec<PathBuf>, text: String, app_id: Option<String> }`, `DropKind` being
    `Files`, `App` or `Text`. builtin.schema's `record Drop` has `app:
    App?`, not an id: logic resolves `app_id` to the `App` through the
    `apps` service when it builds the `Drop` value (null when no
    installed app has that id).
    `strand_render::input::NodeEvent` gains `Drop { payload, at: u32 }`
    (`at` a global row index) for `on drop(p, at)`. Until S-lists routes
    drags, the Router emits nothing for `InputEvent::Drag*` and the demo
    host does not forward a `Drop` to logic.
  - **Surface poses** (design.md, "Compositor-animated poses").
    `SurfacePose { opacity: f32, scale: f32, offset: LogicalPoint }`. When
    the compositor allows it (`Renderer::set_compositor_poses`) and a
    root's motion is only its own opacity, scale (on an axis anchored on
    one side) and x/y, render paints the content at rest and reports each
    frame's pose through `Painter::surface_pose`; the surface manager
    applies opacity with `wp_alpha_modifier_v1`, scale with the
    viewporter's destination size and x/y with layer-shell margins. A
    frame whose pose changed while `paint` returned no damage is a
    pose-only commit: no buffer is attached and `age` does not advance. A
    popup's x/y always repaints. Without the protocols, render repaints.
  - **Compositor capabilities.** `CompositorCaps { alpha_modifier,
    viewporter, single_pixel_buffer, background_effect, session_lock,
    data_device }` (all `bool`; `delegates_poses()` needs the first
    two), reported once the globals are bound
    (`SurfaceHost::compositor_caps`); the host hands render what it uses
    (`set_compositor_blur`, `set_compositor_poses`).
  - **Backends** (`strand_scene::backend`): `Backend` (`Cpu`,
    `GpuPresent`, `GpuReadback`), `BackendChange` (`Promote(surface)`,
    `Demote(surface)`, `Drop`), `GpuStatus` (`Unused`, `Starting`,
    `Up(AdapterInfo)`, `Unavailable { reason }`, `GpuStatus::NOT_BUILT`)
    and `AdapterInfo { name, driver, software }` live here, so render,
    logic and the binary name them in a CPU-only build; `strand-gpu`
    reports its adapter as this `AdapterInfo`.
  - **Scrims and fillets.** `SurfaceSpec` gains `scrim: Option<Color>`
    (`scrim:` resolved through tokens; `popup` and `panel` only, checked by
    the compiler). A scrim is a single-pixel buffer (design.md), so 0b
    narrows builtin.schema's `scrim: paint` to `scrim: color` and a
    gradient scrim is a type error. `SurfaceSpec::resolve` reads a
    `Color` (or a solid `Paint`) on a `popup` or `panel` and leaves it
    `None` elsewhere. `attach: top` fillets are drawn outside the box beside
    the attached edge: render adds the fillet radius to `overhang` on the
    two sides along that edge, so the input region stays the box, and
    placement puts the box at gap 0 from the attached edge. Fillets take
    the paint of the outermost box touching that edge.
  - **Input.** `InputEvent` gains `DragEnter { surface, at, kinds }`,
    `DragMotion { surface, at }`, `DragLeave { surface }` and `DragDrop {
    surface, at, payload: DropPayload }`, from `wl_data_device` (external
    drops, and drags between Strand surfaces, which the pointer grab of a
    press would otherwise hide).

- **Router hooks** (M4; `strand_render::input`, owned by S-lists). The
  `Router` stays the one place input is routed. Other streams read it
  and do not edit it; a stream that needs more asks S-lists for a
  reviewed addition. New `InputScene` methods always have no-op
  defaults, so existing implementations keep compiling.
  - `Router::pointer(surface) -> Option<LogicalPoint>`: the last pointer
    position, read at flatten time by `parallax` and `tilt`.
  - `Router::drag() -> Option<DragView>`: the drag in flight (source node,
    pointer, velocity in px/s, target and insertion index), read by the
    drag ghost, `jelly` and list reordering.
  - Submenus: Right on a row that opens a nested `popup` opens it, Left
    or Escape closes the innermost; S-lists adds these keys for
    S-surface's tray menus, emitting the existing intents (a two-way
    `open` write).
  - The lock: keys on a lock surface while render's built-in fallback is
    shown never reach the `Router`; the binary (`run/lock.rs`) hands them
    to `strand_render::lock_fallback`, which needs no text worker. An
    `input` with `type: password` is edited by the `Router` as any
    `input`; its value is redacted by the binary in `strand watch`, logs
    and (M5) the inspector.

- Later (render):
  - `flatten_surface` rebuilds the map of every delivered layout and
    prunes text across all surfaces on each call, which is O(surfaces ×
    texts) per surface. Before popups and launchers share the main
    thread, scope the map to the surface's root, or keep it per node
    and update it in `deliver`, and prune once per update.

### `strand-theme`

Palettes, used by the compiler's VM (`material()`, `import()`) and by
the instance's built-in theme; render reaches it only through the token
table.

- `Role` (49 Material 3 system roles, the fixed accents included,
  `name()` / `m3()`), `Palette` (every role a colour, `is_dark()`,
  `source()` / `with_source()` (provenance, not compared),
  `insert_into(&mut TokenTable)` writes the roots, their origins and
  the contrast pairs, `to_text()` / `from_text()` the persisted form),
  `Partial` (what an importer found; `fill()` gamut-maps and makes
  opaque what it was given, derives the rest by one table, then
  guards).
- `material::from_seed(Color, Options { variant, dark, contrast })`:
  `material-colors` 0.5, spec 2021 pinned (`material::SPEC`).
- `image::Quantiser`: `lookup(path) -> Lookup::{Ready(seed), Pending {
  last }, Failed { error, last }}` from a `stat` on the calling thread;
  a worker thread reads the file through one descriptor, BLAKE3-hashes
  it and, for unseen content only, decodes it at reduced size (JPEG by
  DCT scaling, PNG row by row; WebP and progressive JPEG whole, up to a
  peak of `FULL_FRAME_BYTES`, then `malloc_trim`) into a 128 px
  box-filtered grid and quantises it (`seed_from_reader`,
  `seed_from_bytes`); seeds by hash and the path index, each the 64
  most recently used (`MAX_REMEMBERED`), are kept in one versioned
  index file (`CACHE_VERSION`) in a directory
  (`$XDG_STATE_HOME/strand/palettes`, merged under `index.lock` with
  other runs sharing it); pending and failed lookups hold the path's own
  last seed (else the last produced); `set_waker` is called after each
  finished job and when a missing or torn wallpaper's grace
  (`MISSING_GRACE`) runs out, `poll()` takes the results and reports
  those a `lookup` took since the last `poll`; `invalidate(path)` marks
  an entry stale (the watcher saw it change).
- `writer::FileWriter`: `write(path, bytes)` queues an atomic write on
  a worker thread (latest per path wins), `flush(timeout)`; dropping it
  waits up to 1 s. Used for the last palette (`ThemeHost`) and the
  portal's last values (`strand run`).
- `import(source, base_dir)`: `catppuccin:<flavour>[:<accent>]`,
  `base16:`, `base24:`, `matugen:`, `w3c:` + a regular file of at most
  1 MiB (`MAX_IMPORT_BYTES`) (`docs/decisions.md`, wave3-theme).
- `contrast::{PAIRS, guard, ratio, solve}`, `defaults::base_tokens()`
  (design.md's `tokens base`), `gamut::map`.

### `strand-core`

A fine-grained push-pull graph, generic over value types
(`T: Clone + PartialEq + 'static`): `Signal` (state), `Memo` (lazy derived),
`Effect` (observer at the edge, e.g. the scene emitter), batching into ticks,
generational handles where a stale read returns an error. Keyed collections
publish `VecDiff`s and incremental `filter/map/take/sort_by` keep keys.
`Async<T>` keeps its last value and exposes `pending`/`error`. The dynamic
`Value` used by the VM is defined by `strand-compiler`, not here.

How consumers drive it (wave 1, see `crates/strand-core/src/lib.rs`):

- One `Runtime` per logic thread, passed as `&Runtime`; closures receive it.
  Reads return `Result<T, Error>`; memo closures return `Result<T, Error>`.
- The host loop calls `rt.tick(now)` (advance the logic clock, fire timers,
  then `flush`) and sleeps until `rt.next_deadline()` or the wake hook
  (`rt.set_wake_hook`, also called when a flush leaves a woken task for the
  next one and when `rt.resume` re-queues held work or leaves a timer
  overdue); `rt.is_idle()` (nothing queued for a flush) with
  `next_deadline() == None` means idle. Each `Tick` carries `errors` and
  `diagnostics` (write-rate, cancelled handlers, zero periods) raised since
  the previous tick, for the overlay and `strand watch --json`.
- The scene emitter calls `rt.watch(prop_memo.id())` per bound prop and,
  each tick, turns `Tick::changed` (watched ids whose value changed, in
  creation order) into `SetProp`s; `for` loops read a collection
  `Snapshot` and turn `diffs_since(last_version)` into keyed
  `Create`/`Remove`/`Move` ops. A `for` over a plain list expression
  (`calendar.days(month)`, `n.actions`, an `Async` list) goes through
  `rt.keyed_memo(key_fn, |rt| list)` (or `memo.keyed(rt, key_fn)`,
  `async_memo.keyed(..)`), which diffs each new list by key: no writing
  effect, no second copy in the emitter.
- Mounting a component runs inside `rt.scope(..)`; unmounting is
  `scope.dispose(rt)`, which drops its nodes, timers and handlers.
- The reconciler keeps identity across reloads with `rt.reparent(id,
  new_owner)`: a component moved from `start` to `end`, or a surface's
  state kept across a monitor unplug, moves to its new owner before the old
  one is disposed (keyed cells keep their diff log, so items keep identity).
  A runtime fault freezes one component with `rt.suspend(scope)` (effects,
  listeners and tasks stop, timers pause as if their `while` were false,
  state kept; service events are kept for its listeners up to
  `MAX_FROZEN_EVENTS` each, the oldest dropped and counted in
  `Diagnostic::EventsDropped`, also when the listener is disposed instead
  of resumed; input events dropped; `await sleep(..)` inside it pauses too)
  and the fixing reload calls
  `rt.resume(scope)`, or moves the live state out with `reparent` and
  disposes the frozen scope (held work is released either way). Released
  timers and sleeps count again from the host's next `tick` (the logic
  clock stands still while the host sleeps), and the release calls the
  wake hook so the host ticks. Reloaded
  timers take over the old countdown with `new.rescale_from(rt, old)`
  (`Debounced::rescale_from` for `on change … after`).
- Handlers: listeners, timers and `on change` handlers each get a handler
  site (`rt.site_of(handler)`) owning the tasks their bodies `rt.spawn`;
  disposing the handler (reload restarting changed handler code, or
  unmount) cancels them at their `await` and reports `Cancelled`. Nodes a
  handler creates belong to its component. A VM starting one coroutine per
  event creates a site with `rt.handler_site()` (disposed when the handler
  is replaced) and starts each with `rt.spawn_for(site, fut)` for
  graph-triggered events (service events, `on change`: the 30 writes/s guard
  sees one handler) or `rt.spawn_input(Some(site), fut)` for external input
  (`on click`, `on scroll`, `on activate`: not rate-counted up to the
  task's first suspending `await`; after it, writes count against that one
  run of the task, not the site, so one write per event after an `await`
  is never throttled but a loop inside the run is). Input event
  queues are `rt.input_events()`; `<->` writes from widgets are made
  outside any handler and are not counted either. Timers are
  `rt.after/every(_dyn)`, `on change` is `rt.on_change(_after)` or, for
  service paths, `rt.on_change_keyed(key, ..)` with the path's object as
  key (no firing on a sink switch); `on change` handlers run after the
  tick's other effects settle, so they fire once per outside write.
  Sinks run in a computed topological order (wave 2): ranks over read
  edges, ownership and the write edges handlers make. The compiler must
  declare both kinds of edge it lowers, before the first flush:
  `rt.reads_from(node, &sources)` with the syntactic read set (every
  branch: a conservative superset) of every binding (memo, derived
  collection) and handler (effect, timer condition, `on change` tracked
  expression, listener), even an empty one, and `rt.writes_to(handler,
  target)` for every assignment and `emit`. With both, every sink runs
  exactly once per flush with final values, the first flush (boot,
  reload mounts) included. Undeclared edges are learned when first seen,
  which can re-run a sink once in that flush; a sink that has never run
  and declares nothing runs after all ranked sinks on its first run.
  `rt.rank(id)` exposes the rank. `writes_to` returns
  `Ok(WriteEdge::Ranked | WriteEdge::Feedback)` and errs only for a
  disposed id: a write edge that closes a loop (a self-normalising `on
  change x { if x > 10 { x = 10 } }`, two handlers normalising each other)
  is a feedback edge bounded by the runtime cycle guard, not a static-cycle
  error, and the outcome and ranks do not depend on whether reads or writes
  are declared first (`rt.write_edge(w, t)` tells what an edge became).
  Composite nodes: `rt.async_memo` declares its internal effect's write
  edge to the value itself; the VM declares the input's read set on
  `memo.effect_id()` and readers declare `memo.id()`. For `on change …
  after T` (`Debounced`), the tracked expression's reads go on `d.effect`
  and the body's writes on `d.timer`. Listeners and woken tasks are
  ranked like sinks: an emit is handed to every live listener of the
  queue as soon as the flush sees it, and each listener is delivered at
  its own rank (each gets its events in emit order; listeners of one
  queue are not delivered together), a task at its own or its writer's
  rank, so a listener declared to read a cell a handler (an effect, or
  another listener of the same event) writes in the same flush sees the
  final value, and the readers of what a listener writes run after it;
  at one rank, woken tasks run before listeners, listeners before sinks.
  A listener's body and a task's polls are tracked without subscribing:
  a read not declared on the listener (for a task, on its handler) is
  learned (it ranks the handler from then on) and reported in strict
  mode. Tasks woken from other threads (IO and D-Bus replies) are polled
  at the start of the next flush, never between sinks.
  Reload writes: the reconciler adopts a changed `state` default (and
  makes any other reload-driven change to a live cell) with
  `signal.set_reloaded(rt, v)`, not `set`: the value changes as usual, but
  every `on change` / `on_change_after` / `on_change_keyed` handler
  downstream of the cell takes it as its new baseline in the next flush
  instead of firing, and a debounce is not restarted ("`on change` fires
  on changes, never at boot or reload"). `Persisted::redeclare`, the
  persist hand-over to a waiting cell, `Persisted::reset_reloaded`
  (`@reset`) and `Settings::redeclare` use it; the overlay's `[reset]`
  (`Persisted::reset`) and file reloads of a settings file are ordinary
  writes. For a keyed collection the reload write is
  `xs.replace_all_reloaded(rt, values)` (a keyed diff, by key, as a
  reload write). A load an `rt.async_memo` starts because of a reload
  write lands as a reload write too, whenever it resolves; a value a
  handler (timer, listener) copies from a reloaded cell into another
  cell is an ordinary write.
  Service events are `EventQueue`s. Keyed collection writes from
  graph-triggered handlers are rate-guarded too (wave 2): a throttled
  handler writes to a held copy (with the list it started from) whose
  changes land as one keyed diff, re-applied by key onto whatever other
  writes (input handlers, service batches, other handlers) did meanwhile,
  so the emitter only ever sees `VecDiff`s and no push is lost
  (`Diagnostic::KeyedConflict` counts changes that no longer apply). The
  VM reads collections without copying through `xs.with(rt, |v| ..)`
  (tracked), `xs.with_untracked(rt, ..)` and `xs.get_key(rt, &k)`; a
  `KeyedVec` clone (`get_untracked`) held across a write makes that write
  copy the items and the key index.
  Service `rw` writes use `write_tagged(value, send)` (throttled writes are
  held, then sent) and reports come back through `receive`. An item of a
  service's keyed list is written with `KeyedSignal::write_item_tagged(
  key, item, send)` (the item updated at once; a throttled handler's item
  writes are held, the latest per item, each landing with its `send`;
  latest write wins per item: a landing item write drops other
  handlers' held writes of that item made before it and re-bases the
  later ones onto itself, a service's report settling the last landed
  write of the item (`receive_items`, even a correcting one) re-bases
  them too, any other change of the item drops them all, and held
  writes of other items are kept)
  and the service's diffs come back through `receive_items(diffs,
  echo_of)`, which drops an item's echo (by tag, or by value untagged),
  keeps a written item in a `Reset` that echoes it, and drops updates
  that change nothing; `keep_pending_items` keeps written items in a
  boot report, `pending_item_writes(key)` counts them. `forget_echoes()`
  (on `Signal` and `KeyedSignal`) drops the pending writes of a service
  run that ended without answering them; tags keep counting up. `let x =
  svc.call(input)` returning `Async` is `rt.async_memo(input, fetch)`, a
  read-only `AsyncMemo`.
- Service lifecycle (start on first reader, stop 5 s after the last leaves
  or goes invisible) is driven by the VM, not by graph observation: the
  compiler knows which service paths each component reads, so the VM
  acquires them on mount (and when shown) and releases them in an
  `rt.on_cleanup` of the component scope (and when hidden).
- Read-only graph introspection for the inspector, `strand watch` and the
  LSP: `rt.sources/observers/owned(id)`, `rt.site_of(handler)`.
- `state x = d persist` is `rt.persisted(&store, path, d, encode, decode)`
  (wave 2): `store` is one `PersistStore::from_env()` per process
  (`$XDG_STATE_HOME/strand/persist`, one file per cell path; its writes go
  through one persist IO thread, so `fsync` never stalls a tick), and the
  VM supplies a stable byte codec for its `Value`s. `path` names one live
  cell: the cell's `file.name` path plus its instance identity when the
  component has several, `bar[<make model description>].expanded` for a
  `bar` on every monitor (the monitor identity of `strand-surface`), or
  the item key for state on list items (`list[<key>].x`); any bytes are
  allowed, the store escapes them. A second live cell on a path in use is
  reported as `Diagnostic::PersistPathInUse` and waits: it does not write
  while the first owns the file, and takes the path over when the first
  is disposed (the reconciler may mount a replacement before disposing
  the old instance): if it still holds the value it started from, it
  continues from the old owner's last value (flushed first), else its own
  value is written. It returns
  a `Persisted` handle: the cell's `Signal`, the `Restore` decision
  (default, stored, adopted new default, kept over a new default, failed),
  and the calls the reconciler makes: on a reload that changes the
  declared default, `persisted.redeclare(rt, new_default)` (adopt if the
  value still holds the old default, else keep it, report once and
  re-stamp; returns `Redeclared`), for `@reset` at reload
  `persisted.reset_reloaded(rt)` and for the overlay's `[reset]`
  `persisted.reset(rt)` (both cancel a pending or queued write, remove the
  file and set the default; the first is a reload write that `on change`
  handlers take as their baseline, the second the user's write). An
  adopted default and a hand-over to a waiting cell are reload writes too
  (`Signal::set_reloaded`). Warnings arrive as
  `Diagnostic::PersistDefaultChanged` / `Diagnostic::PersistFailed` (write
  failures in a later tick, with a wake-hook call; `rt.is_idle()` is false
  while one waits to be reported). A write is never lost to the debounce:
  unmount, `rt.shutdown()` and dropping the last `Runtime` handle read the
  cell's live value and queue it, even when the owner went in the same
  tick as the write; `rt.shutdown()` waits (bounded) for queued writes.
  A write that fails is written again on the cell's next change or
  capture. Files of instance-qualified paths (a `[` in the path:
  `list[<key>].x`, `bar[<monitor>].x`) that no cell has claimed for 90
  days (`PERSIST_RETENTION`), and quarantined copies that old, are
  removed when the store is dropped at exit and once a day while it runs
  (`PERSIST_SWEEP_INTERVAL`), so per-key files do not pile up; a plain
  declared path (`bar.level`) never expires, however long its component
  stays unmounted; `PersistStore::save`/`remove` are for offline tools (a
  live cell on the path does not see them). `state xs: [T] key f = [...]
  persist` is `rt.persisted_keyed(&store, path, default_keyed_vec, encode,
  decode)`: `encode` writes the list's values, `decode` returns them as a
  `Vec<T>` and the list is rebuilt with the default's key function; the
  `PersistedKeyed` handle (`cell: KeyedSignal`, `restored`) has the same
  `redeclare` (taking the new default `KeyedVec`), `reset_reloaded` and
  `reset`, all applied by key.
- Strand's own writes, for the watcher (wave 2):
  `persist_store.on_written(|w: &OwnWrite| ..)` (also on
  `SettingsStore`: one observer slot per IO thread, so setting it on
  either replaces the other) runs on the persist IO thread (and on the
  caller of `PersistStore::save`/`remove`, which must then not call
  `save`, `remove` or `sync`; a panic removes the observer) for every file it is
  about to replace or remove, with `w.path` (as queued: the declared
  settings path, an overlay, a snapshot or a cell file), `w.target` (symlinks
  followed, canonical directory) and `w.content` (the exact new bytes, or
  `None` for a removal), after the temp file is complete and before the
  rename makes it visible. The binary hashes `content` (BLAKE3) and hands
  the hash to `strand-watch` as pre-registered for `target`, so the
  watcher's no-op check stops there.
- `state prefs from "prefs.toml" { accent: color = #7aa2f7; … }` is
  `rt.settings_file(&settings_store, path, fields)` (wave 2,
  `strand_core::settings`): `settings_store` is
  `persist_store.settings()` (overlays in `$XDG_STATE_HOME/strand/settings`,
  writes on the persist IO thread), `path` the file resolved against the
  config directory, and `fields` one `FieldSpec::new(name, default,
  decode, encode)` per typed field, where `decode(&toml_edit::Item) ->
  Result<V, String>` checks the field's type and `encode(&V) ->
  toml_edit::Item` writes it back (`strand_core::settings::toml_edit` is
  re-exported so the VM uses the same version). It returns a `Settings<V>`
  handle: `signal(name)` is the field's ordinary `Signal` (UI writes, `<->`
  bindings and `strand set prefs.compact true` write it; the write is saved
  through `toml_edit` after 250 ms of quiet, keeping comments, spacing and
  order, following symlinks, temp file plus rename in the target's
  directory); `reload(rt)` is what the watcher calls when the file changes
  (each field checked on its own, a syntax error keeps every last good
  value, a deleted key springs back to its default; edits not yet on disk
  and unsaved UI writes are never undone); `set_overlay(rt, name, v)` /
  `clear_overlay(rt, name)` (the overlay's `[clear]`) manage the runtime
  overlay, which wins over the file, which wins over the default. A
  read-only target (no write permission, `EACCES`, `EROFS`: `/nix/store`)
  gets its writes in the overlay instead. Reports are
  `Diagnostic::Settings(SettingsNotice { file, field, issue })` with
  `SettingsIssue::{Syntax, Unreadable, BadValue, Shadowed, ReadOnly,
  WriteFailed, CorruptOverlay, TypeChanged}`; `Shadowed` displays as
  `accent: file changed but runtime overlay wins [clear]`. Round 3 adds:
  a last-good snapshot per file (`last_good_path()`, under
  `settings/last-good/`) that a broken file falls back to at boot;
  `layer(name) -> SettingsLayer::{Overlay, File, Default}` (inspector
  provenance); `redeclare(rt, fields)` for live reload of the declaration
  (fields matched by name keep their signals; a new default is adopted
  only where nothing set the field; a changed `FieldSpec::with_type` type
  resets that field; added fields are read, removed ones disposed); and
  several handles on one declared file (one per mounted instance) adopt
  each other's writes in the same tick. Watcher hand-off: see
  `strand-watch` below.

### `strand-compiler`

`syntax` (lossless lexer and parser with spans and recovery), `schema`
(builtin elements, services, functions and tokens as data), `ty` (types),
`check` (names, types, did-you-mean) producing `hir` (the typed program),
`lower` (bytecode), `vm` (evaluates bytecode against
`strand-core` signals and services; codecs for core's persistence and
settings files), `instantiate` (mounts a
program and emits scene diffs), `reconcile` (old program + new program →
identity map → `SceneDiff` and state migration; M1 live reload). One crate serves runtime, `strand check`
and the LSP. The grammar is specified in `docs/grammar.md`.

Public interfaces other crates and later stages build on:

- **Files** (`strand_compiler::source`): `FileId(u32)` indexes a
  `SourceMap` (`add(name, text) -> FileId`, `get(id) -> Option<&SourceFile
  { name, text: Arc<str> }>`). A `Span { start, end }` is a byte range
  inside one file; `(FileId, Span)` locates text across the config.
- **Parsing** (`strand_compiler::syntax`): `parse(file: FileId, src: &str)
  -> Parse { file_id, file: ast::File, tokens: Vec<Token>, diagnostics }`.
  Never panics. `tokens` is the lossless token stream (trivia included) for
  semantic highlighting, formatting and keyword spans the tree does not
  store. Spans live on `Item`, `Stmt`, `Expr`, `Block`, `Ident` and the
  other wrapper nodes; payload structs use their wrapper's span. Trees are
  at most 256 levels deep (`docs/grammar.md`, "Error recovery"), so passes
  may recurse over them on a 2 MiB stack.
- **Lowering into `strand-core`** (the VM's obligations): declare every
  read set with `rt.reads_from(node, &sources)` (all branches, also when
  empty) and every assignment and `emit` with `rt.writes_to(handler,
  target)` as nodes are created, so effects run once per flush in
  topological order from the first flush on; read keyed collections with
  `with`/`with_untracked`/`get_key` instead of holding `KeyedVec` clones
  (derived collections, `KeyedMemo`, have the same three reads over an
  Rc-shared slice; they keep no key index, so their `get_key` is an O(n)
  scan, fine once per event, not inside a loop over the rows);
  check the lowering in tests: after lowering real fixtures and running a
  few flushes, `rt.stats().learned_edges` (edges nobody declared) and
  `rt.stats().reruns` (sinks run twice in one flush) are 0 unless the
  program has a feedback edge; or turn on `rt.set_strict_edges(true)` in
  the VM's and compiler's test runtimes (and in debug builds), which
  reports every learned edge once as `Diagnostic::UndeclaredWrite {
  writer, target }` / `Diagnostic::UndeclaredRead { reader, source }`, so
  any fixture fails loudly on a missing declaration;
  `writes_to` answering `WriteEdge::Feedback` is not an error (a
  self-normalising handler is valid; only a static cycle among `let`s is a
  load error); for `let x = svc.call(input)` declare the input's reads on
  `memo.effect_id()`, for `on change … after T` the tracked reads on
  `d.effect` and the body's writes on `d.timer`;
  create persisted cells with an instance-qualified path and keep the
  `Persisted` handle for `redeclare` (reload), `reset_reloaded` (`@reset`)
  and `reset` (the overlay's `[reset]`), and `rt.persisted_keyed` /
  `PersistedKeyed` for a persisted keyed collection; adopt any other
  changed `state` default (and apply `@reset` to a non-persisted one)
  with `signal.set_reloaded(rt, v)`, or `xs.replace_all_reloaded(rt,
  values)` for a keyed collection;
  node closures use their `rt` parameter or a `WeakRuntime`
  (`rt.downgrade()`), never a captured `Runtime` clone: that is an `Rc`
  cycle, so neither dropping the last handle nor a persisted cell's
  writer ever runs, and debounced values are lost silently; the binary
  calls `rt.shutdown()` on exit signals (SIGTERM, SIGINT) and on a normal
  exit, before dropping the stores;
  lower `state x from "file.toml" { typed fields }` to
  `rt.settings_file(&store, resolved_path, fields)` with one `FieldSpec`
  per field from the checked schema (the type's decode and encode over
  `toml_edit::Item`, the declared default), keep the `Settings` handle,
  call `reload` when the watcher reports the file (see `strand-watch`), call `redeclare` when a reload changes the
  declaration, pass each field's type name with `FieldSpec::with_type`,
  and give its path to the watcher. See the `strand-core` section.
  How the compiler meets this: `lower::reads` computes every chunk's
  syntactic read set (`Program::reads(chunk)`: `state`s, `let`s,
  settings fields, scope locals, service fields, element instances;
  the lambdas a chunk makes and the `fn`s it calls included) and
  write set (`Program::writes(chunk)`: assignment and list-mutation
  targets, and the services its action calls can change). The instantiator resolves those names to core nodes in the
  scope a chunk is mounted in (`ServiceHost::sources` names a service
  field's nodes) and declares them as each node is created: binding
  memos, `let`s (after every name of the body is bound), component
  parameters, derived lists and their effects, `if`/`match` effects,
  timers (duration and `while`), `on change` effects (tracked targets),
  listeners (empty), the token table; writes on the handler site (`on
  click`, `on svc.event`), the `on change` effect, the timer, and for
  a debounce `d.effect`/`d.timer`, for an async `let`
  `memo.effect_id()`.
- **Identity and change detection.** AST `PartialEq` compares spans, which
  shift on every edit above a node. Reload identity and "did this handler
  change" use a span-insensitive structural hash over the texts of the
  significant tokens inside a node's span (comments and whitespace
  excluded), computed from `Parse::tokens`; `reconcile` owns it.
- **Reconcile** (`strand_compiler::reconcile`): `Build { program,
  identity, hashes, sources, warnings }` is a compiled config, plain data
  (`Send`): `Build::compile(prev, SourceMap)` (or `compile_with` against a
  schema) checks and lowers it, inheriting identities from `prev`.
  `Identity` gives every element, surface, component call, `for`, `if`,
  `match`, handler and timer a `Sid` (source text through a token diff,
  then `id:` name, then position among same-kind siblings; ambiguity
  resets with `Identity::warnings()`); surfaces share one label, so a
  kind change keeps them. `Hashes` are Merkle hashes per handler and
  timer over everything they reach (`fn`s, `let`s, types, keyframes,
  custom services; `locks()` over every `lock` subtree,
  `changed_services(old)`; a group of declarations naming each other is
  hashed once as a strongly connected component, linear in the def
  graph). `Report { classes: Vec<EditClass>, kept, reset, notices,
  kept_over_default: Vec<KeptCell { path, shown }>, ambiguous,
  restarted, cancelled }` is what a reload did (`notices` is the prose;
  `kept_over_default` and `ambiguous` are the same facts as data, for
  the overlay's `[reset]`, `strand watch` and M5's inspector badges), its
  `EditClass` names the rows of design.md's edit table (`token`, `prop`,
  `node-added`, `node-removed`, `state-default`, `state-reset`,
  `handler`, `timer`, `surface`, `service`, `lock-deferred`, `hard`).
  `reconcile::loader::Loader::new(root, schema, cache_dir)` is the
  compiler worker's state (`.with_check(ExtraCheck)` adds a check run on
  every compile whose diagnostics count as the checker's: `strand run`'s
  D-Bus introspection of `from dbus` services): `boot()`, `changed([(path, exists)])`,
  `rescan()` and `recheck()` (the extra check's answers changed since
  the last compile: the running and held files are checked again, held
  files that now pass are committed, otherwise no build and the late
  diagnostics are reported as a held edit's) each return an `Outcome { build, committed, held,
  diagnostics, sources, unreadable, from_cache, cleared, repeated,
  compile_time }`
  (`cleared`: the last attempt had errors, held or unreadable files and
  this one has none, even when nothing changed against the last good
  build, or the running program's warnings went on a recheck;
  `diagnostics` keeps a successful compile's warnings, and a revert to
  the running text carries the running program's): the
  largest consistent set of changed files committed, the rest held with
  the diagnostics of the whole attempt; a commit stores the sources under
  `Cache` (`$XDG_CACHE_HOME/strand/last-good/<config hash>`) keyed by
  their hashes, `COMPILER_VERSION` and `Schema::fingerprint()`, and a
  config broken at boot starts from them.
- **Diagnostics** (`strand_compiler::diagnostic`): `Diagnostic { severity,
  code: &'static str, message, labels: Vec<Label { file: FileId, span,
  message, primary }>, help: Option<String>, suggestions: Vec<Suggestion {
  file, span, replacement }> }`, built with
  `Diagnostic::error/warning(code, msg).with_label(span, msg)`,
  `.with_secondary(..)`, `.with_label_in(file, ..)`,
  `.with_secondary_in(file, ..)`, `.with_help(..)`, and `.in_file(file)` for
  single-file stages (it moves suggestions too). A suggestion is the
  replacement a diagnostic proposes, as data: `.suggest(span, x)` /
  `.suggest_opt(span, Option<x>)` / `.with_suggestion(span, x)` set the
  help to "did you mean `x`?" and propose `x` for the text at `span` (the
  misspelt word, which need not be the primary span: `on chnage a, b`);
  `.add_suggestion(span, x)` adds one choice of several (the parameters a
  call does not set yet). Editors and the overlay read `suggestions`,
  never the help text. `render(&[Diagnostic], &SourceMap, Style)` draws
  miette reports (labels in other files as related reports, at most 50 per
  file); `render_short(&[Diagnostic], &SourceMap)` gives one
  `file:line:col: severity[code]: message; help` line each, names in
  double quotes (design.md's `unknown prop "expanded"; did you mean
  "open"?`), for the reload overlay's list and editors. `suggest`/`closest` give the shared
  near-miss logic: optimal-string-alignment distance within about one
  edit per three letters, one-letter words matched only by case, no
  one-letter candidate for a longer word, ties to a plausible typo
  (dropped, added or swapped letters), then the closest length, then the
  alphabet (never hash-map order).
- **Schema** (`strand_compiler::schema`): everything the language knows
  before reading a config, as data: element kinds (typed props with
  `two_way`/`inherited` flags and sub-blocks, positional argument type,
  events with payload types, names in scope such as `screen`, `leaf` /
  `surface` / `only_in` flags), records and enums, services (global names
  bound to records whose fields carry `rw`, with `fn` methods, `action`s and
  `event`s), builtin functions with overloads (`lift` passes null through,
  `Async<T>` returns), builtin values (`t`), methods on builtin types,
  palette roles and base-tier tokens. It is written in a small declaration
  language (`schema/builtin.schema`, described in the module docs) and
  parsed once by `Schema::builtin() -> &'static Schema`. Service crates
  contribute their schemas the same way: clone the builtin, call
  `Schema::extend(text) -> Result<(), Vec<SchemaError { line, message }>>`
  (it adds and never replaces: an existing element, group, type, alias,
  value, palette role or token, or a function or method overload with the
  same parameters, is a "declared twice" error; every record's `key` path
  must name a field, re-checked across all records after the extension;
  it is atomic, so on error nothing is added and the fingerprint is
  unchanged), and check with
  `compile_with(&map, &schema)`. A `service … from dbus|file|listen|poll`
  declaration's source is a constant the checker evaluates
  (`hir::ServiceDecl::spec`, a `hir::SourceSpec`; fields carry their key
  path); `check::dbus::check(&hir::Program, &dyn Introspect)` compares
  its `dbus` fields with an object's introspection (`Introspect::
  properties(system, name, path) -> Result<Vec<BusProperty>, String>`;
  the caller brings the bus: `strand check`, the loader, the LSP).
  `check::paths::check(&hir::Program, config_dir: Option<&Path>) ->
  Vec<Diagnostic>` warns (`check::poll_program`) when a `from file` or
  `from poll` path names a program (executable, `#!` or ELF; stat before
  open, so a FIFO never blocks); it touches the disk, so it is not part
  of `compile` and the same three callers run it next to the D-Bus check. The builtin's service stubs, and the
  records only services hand out (`Window`, `Notification`, `Date`, …),
  are declared `provisional service` / `provisional record`: the first
  extension that declares the same name replaces the stub in place (same
  `RecordId`, so `[Window]` fields of other services see the real one; the
  stub's members and docs go) and the name stops being provisional, so a
  second contribution is "declared twice" (`Schema::provisional:
  BTreeSet<String>`). `handle record X` marks a record whose values are
  runtime handles (`Node`, `Canvas`; `RecordDef::handle`): they compare,
  but `persist` and settings files refuse them, as they refuse any record
  that declares an `action`. An element flagged `on_demand` (`popup`,
  `tooltip`, `page`; `ElementFlags::on_demand`) mounts its children only
  on demand, which the component-cycle check reads like an `if`.
  Configs see contributed names as a prelude
  their own declarations shadow, with one exception: a config `service`
  named like a builtin or contributed service is `check::redeclared`
  (services are identified by name at runtime), so a crate that adds a
  service name a config already declares breaks that config; namespace
  new service names. An element's positional names the prop it fills,
  `element meter(float -> value)` (`ElementSchema::arg_prop`), which the
  checker and lowering both read. The LSP reads the same
  table for completion and hover (M3, "service schemas drive type checking
  and LSP hover"). Members after `.` come from one table the checker
  itself types `x.name` and `x.name(…)` by: `schema::members_of(&Ty,
  &Schema, &TypeTable) -> Vec<MemberInfo { name, kind: Field | Method, ty,
  sigs, writes, doc }>` (records and services from the schema with their
  docs, builtin-type methods, and the generic list and `Async` members —
  `len`, `first`, `filter`, `take`, `remove_key`, `pending`, `value`, … —
  built for the item type; `list_members`, `async_members`,
  `ASYNC_TRANSFORMS`). `///` comments in schema text document the entry they
  precede: `Schema::doc(&DocKey) -> Option<&str>` with `DocKey::{Type(name),
  Member(type, member), Function(name), Value(name), Method(type, method),
  Element(kind), Prop(kind, name) (also `on event`, `stroke.dash` and
  scope names; group docs reach their elements), Token(path)}`;
  `RecordDef::doc` mirrors `DocKey::Type`; `Schema::token_doc(path)` falls
  back to the nearest documented group (`space.2` reads `space`). Every
  entry of the builtin schema is documented (a test enforces it). A parameter's default keeps its
  source text (`ParamSig::default: Option<String>`). `Schema::fingerprint()
  -> [u8; 32]` is BLAKE3 chained over every text `extend` was given, in
  order (the builtin first): the schema part of the compiled-output cache
  key (source hash + compiler version + schema hash), so a service crate
  that changes its schema invalidates configs compiled against the old
  one. The schema's element kinds and props are the scene's
  (`strand_scene::protocol::NodeKind`, `Prop`), checked by
  `strand-compiler/tests/scene_catalogue.rs`; `id` is compiler-only.
  Two scene props exist for the language's sake: `Prop::Dash` (`"dash"`,
  class `Effects`), the `dash` sub-prop of `stroke` (`stroke: 3, $accent
  { dash: 6, 4 }`, dash and gap lengths), and `Prop::InputType`
  (`"type"`, `Snap`), an `input`'s `type: text | password` (grammar.md);
  render stores both and draws them when strokes and inputs land.
- **Types** (`strand_compiler::ty`): `Ty` is `Error` (already reported,
  accepted everywhere), `Any`, `Null`, `Unit`, `Prim(Prim)` (`bool int float
  length percent angle duration color paint text path font shadow insets
  corners`), `Opaque(name)` (`Palette`, `Spring`, `Mask`, …), `Enum(EnumId)`,
  `EnumType(EnumId)` (an enum as a value, `options: Look`), `Record(RecordId)`,
  `List(T, keyed)`, `Optional(T)`, `Async(T)`, `Fn(Arc<FnSig>)`, `Tuple`
  (comma shorthands) and `Union` (schema props only). Records and enums live
  in a `TypeTable` (schema first, then the config's `type`, `enum`,
  settings files and custom services), with `assignable(from, to)`,
  `join(a, b)` and `show(ty)` for messages.
- **Checking**: `strand_compiler::compile(&SourceMap) -> Compiled { parses,
  program: hir::Program, diagnostics }` parses every file and checks them as
  one program (`check::check(&[Module { file, name, ast }], &Schema) ->
  Checked { program, diagnostics }` for callers holding parses already); the
  module name is the file stem. It never panics and always returns a
  program: an unresolved name or ill-typed expression becomes `Ty::Error`,
  so one mistake gives one diagnostic. Diagnostics are sorted by file and
  position, syntax and checker together. Scoping: components, surfaces,
  enums, `type`s, `fn`s, token sets, keyframes and custom services are
  global across the config's files; top-level `state`/`let` belong to their
  file and are reachable elsewhere (and from the CLI) only as `file.name`
  when exported; `state`/`let` in a tree belong to their component,
  surface or list item. Every lazily checked declaration runs under
  `stacker::maybe_grow`, so checking is safe on small worker-thread
  stacks (the LSP's, the reload compile's) however long a chain of
  declarations is. An untyped `state`/`let` holding a whole number is an
  `int` in the HIR unless a fraction is written to it (then `float`).
  Fraction writes the source shows plainly are pinned before checking;
  any found while checking re-run the checker, with the pins closed over
  the hand-offs between such states, direct or through untyped `let`s,
  locals and fn values (`Checked::passes` says how many ran; there is no
  cap, see decisions.md). An untyped component parameter has the joined
  type of its call sites' arguments, each checked as an untyped `let`'s
  value (`count + 1` is an `int`); in a cycle of inferring components a
  later call that widens it re-runs the checker with it widened.
- **HIR** (`strand_compiler::hir`), the typed, resolved program the VM
  lowering, the reconciler and the LSP consume. `Program { files:
  Vec<FileHir { file, name, items }>, defs: Vec<Def>, locals: Vec<Local>,
  types: TypeTable, tokens: BTreeMap<path, Ty>, refs: Vec<Reference> }`.
  Declarations are `DefId`s (`Def { name, kind, file, span, ty, exported,
  owner }`, `DefKind::{Component, Surface(kind), State, Settings, Let, Fn,
  Enum, Type, Tokens, Keyframes, Service}`); parameters, `for` bindings,
  event parameters, handler `let`s, element-scope names and `id:` names are
  `LocalId`s; every element has a program-unique `NodeIdx`. Items mirror the
  syntax (`Component { def, params, tokens, body, has_slot }`, `Surface {
  def, element }`, `StateDecl`, `LetDecl`, `FnDecl`, `TokenSet` with
  flattened `TokenDef { path, span, override_, value }`, `Use`,
  `ServiceDecl`, `Keyframes`, `Handler`, `Timer`, `Permit`); tree `Node`s
  are `Element { node, kind: Builtin|Component|Unknown, span, arg, id,
  props, children }`, `When`, `If`, `For { binding, iter, key, body }`
  (`key: None` when items bring their own), `Match`, `Handler { event,
  params, body }`, `Timer`, `Pose`, `Slot`, `Set`, `Selector`, `Play`,
  `State`, `Let`. An element's props are split from its children; `Prop {
  name, span, value, two_way, transition, sub, inherited }`. Every `Expr {
  kind, ty, span }` is typed; names are resolved in the kind (`Local`,
  `Def`, `Service`, `Value`, `Node` for `self`/`hover`/an `id:` name,
  `Variant`, `EnumType`, `Token(path)`), calls name their `Callee`
  (`Fn`, `Builtin { name, overload }`, `Method { receiver, name, overload
  }`, `Record`, `Value`) and arguments their parameter index. Spans are
  file-local (the file is the enclosing `FileHir`'s). `refs` lists every
  resolved name use with its `Target` (def, local, service, builtin,
  variant, token, field, element, file); `Program::reference_at(file,
  offset)` serves go-to-definition, hover and rename, and
  `Program::exports()` lists `file.name` paths for the CLI.
- **Lowering** (`strand_compiler::lower`): `lower(&hir::Program,
  &Schema) -> lower::Program`, plain data (`Send`), so the compiler
  worker builds it and hands it to logic. Every expression a binding,
  `let`, `fn`, handler or timer evaluates is a `Chunk` of bytecode (`Op`:
  a stack machine; `&&`, `||`, `??`, `?.`, ternaries, `match`, `if` and
  `for` statements are jumps, `await` is an op, assignments are `Store`
  to a `Place` and list mutations `Mutate`), with constants that are not
  run-time values. The tree around them stays a small structure the
  instantiator mounts: `Node::{Element, Surface, If, For, Match, Slot,
  State, Let, Handler, Timer, When, Pose, Set, Play}`. An `Element`'s
  props carry their scene `Prop` and schema type (the positional argument
  is the prop it fills: `text`'s `text`, `icon`/`image`'s `source`,
  `meter`'s `value`, from `ElementSchema::arg_prop`); a `<->` prop carries its `TwoWay` place. A `Body`
  (component, surface, `for` item) lists the elements it owns and the
  services it reads.
- **VM** (`strand_compiler::vm`): `Value` is the dynamic value
  (numbers with their unit, text, colours, enums, records by `RecordId`,
  lists, comma and space values, `Async`, closures, node handles, and
  token-bound values kept symbolic as `strand_scene::TokenExpr`, so
  `$surface.alpha(0.72)` reaches render unresolved). `Vm::eval(rt, chunk,
  env)` runs a chunk synchronously; called inside a core memo, every
  `state`, service field and node flag it reads is tracked, and only what
  the run read (a ternary's taken branch). `Vm::handler(rt, chunk, env,
  frame, ctx)` is a handler body as a future for `rt.spawn*`: it
  suspends at `await` and is cancelled by dropping it. Scopes are `Env`s
  (the config's root for file `state`/`let`, then component, surface,
  `for` item and branch scopes) holding `Slot::{Signal, Memo}`s; element
  instances' `hover`/`pressed`/`focused`/`selected`/size are `NodeState`
  signals in the scope that owns the element (so `id:` names work across
  a body). Errors are values: a failing binding is an `Err` in its memo
  (the prop keeps its last good value), a failing handler an `Err` its
  task returns; `fn` and lambda calls nest at most `MAX_CALL_DEPTH` (200).
  Handler frames are scoped: `Op::ScopeEnter`/`ScopeExit` around each
  block that binds locals and around each `for`, whose `IterNext` drops
  the previous iteration's locals, so a loop keeps a frame of constant
  size; a lambda captures only its free locals (`lower::Lambda::free`).
- **Time-bound values (M4 plan, not built yet).** `t`, `wave(…)` and
  `noise(…)` read 0 (`noise` once) on the logic thread today, with one
  `lower::time_signal` warning per name (`Program::warnings`, reported
  as boot-tick notices). They are to travel like tokens: `Value` gains a
  symbolic variant holding a `strand_scene::TokenExpr` with the time
  leaves (`Time`, `Wave`, `Noise`, `Index`, `Count`: `strand-scene`, "M4
  vocabulary"; the warning goes), so `t * 20deg` or `10 * wave(2s)`
  stays an expression; arithmetic on it builds the tree as
  `builtins::binary` already does for `TokenExpr`, and `convert` maps it
  to `PropValue::Token` (a `Template` with numeric slots when it sits
  inside a composite value), which render evaluates per frame (`t` per
  node, from its appearance; a reload that keeps the node keeps its `t`,
  a remount restarts it). A prop holding one is a frame-driven prop
  for render's frame scheduling; nothing else in the emitter changes.
  `noise(x)` is a time value only when `x` is. A time-bound value
  reaching logic (a handler, a comparison, `match`) is an error value,
  as a token in arithmetic without numbers is now.
- **Services** (`strand_compiler::vm::ServiceHost`): the VM's only way
  to services.
  - `declare(rt, &lower::CustomService, types)`: a no-code service the
    program declares (its name, record of `types`, `hir::SourceSpec`
    source and fields with their key paths and `rw`), called once at
    instantiation. `restart(rt, &CustomService, types)` / `stop(rt,
    name)`: a reload changed (or added) / removed a declaration; only
    that service restarts or stops. Built-ins never do. `retype(rt,
    &CustomService, types)`: a reload kept the declaration, but `types`
    (the new program's table) may renumber its record and the enums and
    records its fields name; the host reads its values as those types
    from then on, without restarting it (called for every surviving
    service that does not restart; `SchemaHost` remounts it at the new
    defaults when its record or field types moved). All default to
    nothing.
  - `read(rt, service, field)` and `call(rt, service, method, args)`
    (`fn` methods: `clock.format`, `calendar.days`, `workspaces.on`)
    must read through the graph (a `Signal<Value>` per field) so
    bindings depend on exactly that field.
  - `sources(rt, service, field: Option<&str>) -> Vec<NodeId>`: the
    core nodes a read of `service.field` (or of the service as a whole,
    `None`: a method such as `clock.format`) depends on, which the VM
    declares with `rt.reads_from` before the first flush. A superset is
    fine; the default (none) leaves those edges to be learned on first
    run.
  - `action_writes(rt, service) -> Vec<NodeId>`: the cells an action of
    `service` can write (`n.expire()` changes `notifications.popups`),
    declared with `rt.writes_to` on every handler that calls one, so
    readers are ranked after it from the first flush. Lowering names
    the service from the receiver's type (the service itself, or every
    service whose fields reach the item's record). The default is
    `sources(rt, service, None)`, every field: a superset.
  - `read_keyed(rt, service, field) -> Option<KeyedSignal<ValueKey,
    Value>>`: a list field published as a core keyed collection
    (`notifications.popups`, `workspaces.all`). A `for` directly over
    it follows its `VecDiff`s (one new notification is one diff from
    the service to the scene) instead of comparing whole lists; `None`
    (the default) for plain fields.
  - `write(rt, service, path: &[PathSeg], value)` writes one `rw` leaf:
    `audio.sink.volume = 0.8` is `write("audio", [Field("sink"),
    Field("volume")], 0.8)`, so a service sends only what changed and a
    concurrent `muted` change is not overwritten. Hosts tag writes with
    core's `Signal::write_tagged` (generation per written cell) and
    match the service's reports with `receive`, so the echo of a write
    is ignored (`SchemaHost` writes the field's cell with
    `write_tagged`).
  - `write_item(rt, item: &Value, path, value)` writes one `rw` leaf
    below an item of a service's keyed list: `s.volume = 0.5` for `s`
    in `audio.sinks` (a slider's `<-> s.volume` too) is
    `write_item(s, [Field("volume")], 0.5)`. The host routes by the
    item's record and finds the item by its schema `key` (the sink with
    id 42), applies it at once and ignores its echo, with core's
    `KeyedSignal::write_item_tagged` / `receive_items`. Unless the path
    is fields all the way from a service (`audio.sink.volume` is a
    `write`), lowering roots a place at the base nearest the leaf whose
    type is a keyed schema record (`lower::PlaceRoot::Item`: the item's value comes first among
    the place's index values); the checker accepts an `rw` field only
    where the place starts at a `state`/settings, a service or such an
    item (`check::read_only` otherwise). The default refuses.
  - `fetch(rt, service, method, args) -> Fetch` (a boxed future):
    `let x = svc.m(args)` whose method returns `Async` is `rt.async_memo(
    args, fetch)` per mounted `let`, created on the `let`'s first read
    (a closed launcher never searches). Each change of the argument
    tuple starts one fetch and drops the superseded future (cancelling
    it); the value keeps its last result while pending. The default
    runs `call` once and is ready at once. Every other async service
    call goes through `fetch` too: in a binding (`apps.search(q) ?? []`)
    the compiler lowers it to `Op::AsyncSite(call chunk)`, the scope's
    own load of that call (the same `Vm::async_load` as an async `let`,
    made on first read and kept with the scope); in a handler, `fn` or
    lambda to `Op::FetchMethod`, a pending `Async` whose `await` waits
    for the fetch. `call` is never asked for an async method.
  - `fetch_reads(rt, service, method)`: read, tracked, what an async
    method's result depends on besides its arguments; a load calls it
    where it reads its arguments, so a change fetches again (an open
    launcher searches its query again when the app list or the icon
    theme changes). The default reads nothing; `StoreHost` reads every
    field of the service (a keyed one as its collection).
  - `action(rt, ActionTarget::{Service, Item(&record)}, name, args)` runs
    `notifications.clear()` or `ws.focus()`; `event(rt, service,
    event) -> EventQueue<Vec<Value>>` is the lossless queue `on
    notifications.received(n)` listens to.
  - `acquire`/`release(rt, service)`: a reader count. Every mounted
    component and the config's top level hold the services their body
    reads; a surface holds its body's services only while shown (its
    `open` is true, or it has no `open`; a top-level surface's `open`
    binding itself is held by the file's top level, so what opens it is
    read while it is closed), a hidden surface's content
    (components in it, surfaces nested in it) lets go of everything it
    holds, a surface nested in another (`popup` in a `bar`) holds its
    own children's reads only while it is shown (they do not count for
    the body around it: `lower::Element::services`; a closed popup's
    content is unmounted, its components' `state` cells kept for its
    next opening), and a parked bar
    (monitor unplugged) lets go of everything under it until it
    returns. A scope's reads include those of the `let`s and `fn`s it
    reads, transitively; a `let` or `fn` holds nothing itself (the top
    level holds what its handlers, timers, tokens, `state` initialisers
    and exported `let`s read), so a service read only through a `let`
    that a closed popup shows stays stopped. The
    service starts on its first reader and stops 5 s after its last
    leaves or goes invisible: `rt` lets a host create a service's cells
    lazily on `acquire` and arm the 5 s stop with core's timers on
    `release` (also called from scope cleanup: no synchronous disposal
    there).
  - `acquire_field`/`release_field(rt, service, field)`: the same holds
    for each field a scope reads directly (`lower::ServiceUses` keeps
    `(service, None)` and `(service, Some(field))` per scope; a hold
    acquires the service, then its fields, and releases in reverse).
    What a `#[store(stream)]` field's stream runs for (a Wi-Fi scan,
    audio levels): a bar showing `network.ssid` does not keep a closed
    popup's scan of `network` access points going. Default: nothing.
  - Several service crates, one host (M3 plan): `Instance::new` takes one
    `Rc<dyn ServiceHost>`, and `ServiceHost` (with `Value`, `RecordId`
    and the rest of the VM's dynamic types) stays in `strand-compiler`:
    `strand-services` depends only on `strand-core` (crate graph) and
    never sees `Value`. A service runs on its own thread (tokio, or
    PipeWire's), and core's `Runtime` is `Rc`-based and not `Send`, so a
    service never holds logic-thread cells. The split, per service
    (the `#[service]`/`#[derive(Store)]` contract features.md plans):
    `#[derive(Store)]` generates a `Send` patch type (one variant per
    field, keyed-list diffs, events) that the service thread sends over
    a channel; a logic-side store, built on the logic thread, applies
    those patches to its cells (`Signal<T>` per field, `KeyedSignal` for
    keyed lists, an `EventQueue<T>` per event). Writes, actions and
    `fn`/async methods go the other way as messages to the service
    thread; a method's result comes back as a patch that completes the
    `Async` the call returned. The adapter from those logic-side stores
    to `ServiceHost` lives on the language side: the binary (`strand`,
    which depends on both) gives each service one `ServiceHost`
    implementation that wraps its logic-side store and converts between
    its typed cells and `Value` (a `Memo<Value>` over each typed field,
    so a binding still depends on exactly that field; writes and actions
    converted back and sent to the service thread),
    and a composite host routes every call by service name to the member
    that serves it (`SchemaHost::real` answers the rest at their
    defaults, and the clock and calendar stay there), unions `next_wake`
    (earliest) and fans out `wake`. A service name belongs to exactly one
    member; `declare`d custom services go to the member that implements
    their source kind (`dbus`, `file`, `listen`, `poll`). If the
    conversion turns out to be generic over `#[derive(Store)]` (a store
    describing its fields by name), it moves into `strand-compiler`
    behind a `strand-core` trait instead; no edge from `strand-services`
    to `strand-compiler` is added either way.
  - `declare(rt, &lower::CustomService, types)` adds a custom service; `next_wake(rt) ->
    Option<SystemTime>` and `wake(rt, now)` let wall-clock services (the
    clock) wake the host loop only at minute boundaries (seconds only
    while a binding shows them).
  - `vm::schema_host::SchemaHost` implements it with every schema
    service at its defaults; lists of keyed records are keyed
    collections. `SchemaHost::mock` (clock fixed at 2026-10-05 09:41:07
    UTC; tests `set` fields, `emit` events, read `take_actions()`,
    `take_writes()` and `readers(service)`, `hold`/`release_fetch` a
    method to keep its fetches pending) and `SchemaHost::real` (the wall
    clock and calendar in local time, other services at defaults until
    M3 service crates implement the trait; it records no action or
    write history).
- **Storage** (`strand_compiler::instantiate::Storage { persist:
  Option<strand_core::PersistStore>, settings: Option<SettingsStore>,
  config_dir }`): `Storage::from_env(config_dir)` (one IO thread for
  both), `Storage::in_dirs(state, config)` for tests, `Storage::none()`
  keeps nothing. `state x = d persist` is core's `rt.persisted(store,
  path, d, encode, decode)` with the VM's codec
  (`vm::persist::{encode_bytes, decode_bytes}`: JSON by declared type,
  enums by variant name, records by field name). The path is the
  cell's owner and name qualified by its instance:
  `toasts.dnd` (a file's state), `Clock.open` (a component's or
  surface's), `TopBar[<monitor id>].expanded` (a bar on every monitor),
  `Row[<item key>].open` (state in a `for` item); two live instances on
  one path are core's `PersistPathInUse`. The `Persisted` handles are
  kept: `Instance::reset(path)` is `@reset` (and the overlay's
  `[reset]`; a cell that is not persisted is set to its default). A keyed list `state` that
  is persisted stays a plain signal (core persists `Signal`s).
  `state prefs from "prefs.toml" { typed fields }` is `rt.settings_file(
  store, path, fields)`: the path resolved against `config_dir` (`~/`
  from `$HOME`), one `FieldSpec` per field with the declared default,
  its type name, and a TOML codec by type (`vm::persist::{decode_item,
  encode_item}`: colours as `"#rrggbb"`, durations as `"200ms"`/`"6s"`
  or seconds, enums by variant name, lists and inline tables). Each
  field is its own signal: `prefs.compact` reads (and `prefs.compact =
  true`, `<->`, `strand set theme.prefs.compact true` write) that field
  only; `prefs` alone reads as a record. Without a settings store the
  fields hold their defaults. The watcher gets the files from
  `Instance::settings_files()` and how to read them from
  `Instance::settings_sources()` (one `SettingsSources` per file); it
  reads a changed file on its own thread and the logic thread calls
  `Instance::reload_settings_with(path, Some(read))` (core's
  `Settings::reload_with` on every mounted handle, a clone of the read
  each); `Instance::reload_settings(path)` reads in place.
  `Instance::settings_overlay_paths(path)` names the runtime overlay
  files of those handles, so `strand run` can drop the overlay rows a
  re-read no longer reports.
- **Instantiation** (`strand_compiler::instantiate`): `Instance::new(rt,
  Arc<lower::Program>, Rc<dyn ServiceHost>, Storage)` mounts the
  program; `Instance::tick(now)` (or `flush()`) runs the core tick and
  returns an `Update { diff: SceneDiff, errors: Vec<RuntimeError>,
  diagnostics, notices, kept: Vec<KeptCell> }` (`kept`: persisted cells
  kept over a changed default, also in `notices`): one diff per tick,
  the boot one starting
  with `SetTokens { transition: Instant }`, later token tables with
  `Default`. The table is the built-in theme's base tokens
  (`strand_theme::defaults`) under the chosen token set, with the
  `use palette` palette (or, without one, `material(seed:
  system.accent ?? #7aa2f7, dark: system.dark, contrast:
  system.contrast)`).
  - Theming (`vm::theme::ThemeHost`, one per instance, kept across
    reloads): `material(image:)` asks its `Quantiser` (cache under
    `Storage::palette_dir()`) and returns an `Async<Palette>` holding
    the last image palette while one is quantised; a finished job wakes
    a core task that bumps the host's generation signal, which every
    `material(image:)` and file `import` reads. `Instance::theme_files()
    -> (wallpapers, imports)` (the files the current evaluations read:
    a memo that re-runs or is dropped lets go of its paths) and
    `take_theme_files_changed()` are for the watcher,
    `theme_files_changed(&[PathBuf])` re-reads after a change, `theme()`
    gives the host (tests wait on it). The host keeps the last palette
    the token table was made with (`remember_palette`, persisted as
    `palettes/palette`); a `use palette` that fails or has no value yet
    takes it (error reported), else the built-in palette.
  - `Instance::set_text(path, text)` is `strand set`: `set` with the
    value parsed by the target's type. Paths name an exported value, or
    a settings file's state whether exported or not (`theme.prefs.compact`,
    or `prefs.compact` when one file has a settings `prefs`). `clear_settings_overlay(file,
    field)` is a settings notice's `[clear]`.
  - Each bound prop is one watched memo folding the base binding and
    its `when` blocks in source order (later wins; each source keeps its
    own `~` transition); `if`/`match` are effects swapping branch
    fragments.
  - `for` keeps one item per key with its own value cell (an
    `Update` diff sets that item's cell only, so one changed row re-runs
    one row's bindings). Over a keyed `state` or a host's keyed field it
    follows that collection's diffs; over any other expression it is
    `rt.keyed_memo` over the list keyed by `key e` or the item record's
    key. `Move` becomes scene `Move`s; a `Reset` is reconciled by key,
    moving only the items outside the longest run already in order.
    A chain of `.filter`/`.map`/`.take`/`.sort_by` on a keyed `state`
    or a host's keyed field (`lower::For::chain`) is core's incremental
    views (`KeyedOps::{filter_with, map_with, take_with,
    sort_by_with}`) fed by the source's diffs: each lambda runs per
    item through the VM, and what it reads besides the item (its
    captured values and the values of its read set; a keyed collection
    by its version) is the step's tracked parameters, whose change
    rebuilds that step. A lambda calling a service method falls back
    to `keyed_memo`. A `let` whose value is such a chain (or a chain on
    another such `let`; `lower::Program::let_chains`) is one shared view
    (`Slot::View`: the `KeyedMemo` and a lazy list memo), built once the
    body's keyed `state`s are bound: `let shown = notifications.popups
    .filter(…).take(5)` then `for n in shown` and `shown.len` follow
    that view, so one new notification is one diff from the service to
    the scene. The `for`'s key check looks through the `let`s to the
    collection at the bottom.
  - Reads of a keyed collection (`xs.len`, `.first`, `.last`, `xs[i]`,
    `xs.contains(x)` on a keyed `state`, a view `let` or a host's keyed
    field) are
    `Op::Keyed`: answered with core's `with`/`get_key`, never by
    building the list as a value. The list value (`Slot::Keyed`'s
    memo) is built lazily, only for reads that need the whole list.
  - `await` on an `Async` waits for its operation: a `sleep`, or for an
    async `let` that is pending, the load settling (a settle effect
    wakes the awaiters). The operation is shared by every copy of the
    value, so several handlers awaiting it get the same result.
    `await` on a pending value with nothing to wait on is an error
    value, never a silent null.
  - A `bar` is a keyed instance per `screens.all` item that its own
    `screens:` picks (a connector or monitor id, a list of them,
    `focused`, `all`), keyed by the monitor's identity (`Screen.id`:
    make, model and description), with `screen` in scope and `screens:
    "<monitor id>"` (`Screens::Named`) set by the instance. A bar whose
    monitor leaves `screens.all` is parked: its nodes are `Remove`d (render
    plays their exit), its scope is frozen and its services let go, and
    it comes back as it was (nodes recreated under their ids with their
    last props, `Instant`) when the monitor returns, or is dropped by
    `Instance::forget_screen(id)` (the surface layer's
    `monitor_forgotten`, 30 s after the unplug). Every surface gets
    `Prop::Name`.
  - A surface's own element events (`on show`, `on hide`, `on dismiss`)
    are always live; the rest of its body is mounted when it is first
    shown and frozen (`rt.suspend`) while hidden. The instance sends
    `show` when `open` turns true (at mount for a surface without
    `open`) and `hide` when it turns false; render does not send them.
  - `exit` mirrors `enter` when not given. Component `tokens { }`
    entries (`Toast.radius`) join the global table, so an ancestor's
    `set { $Toast.radius: … }` still overrides them.
  - Runtime errors are values, located: `RuntimeError { what, error,
    file, span, node, component, scope }`, the span being the failing
    operation's (from `Chunk::spans`; the innermost, so a prop failing
    because a `let` it reads failed points into the `let`), else the
    binding's, handler's or timer's. `Instance::origin(node) ->
    (FileId, NodeIdx, Span)` maps a scene node back to its element (the
    overlay's click to `$EDITOR`, inspector provenance), and
    `Instance::freeze(&err)` suspends the faulting component's instance
    scope (`thaw` resumes it after the fixing reload). A fault at the
    config's top level (a file's `let`, handler or timer) has no scope:
    it is outlined, nothing is frozen.
  - The host loop: `Instance::step(now, wall) -> (Update, Wake)` sets
    wall-clock services to `wall` (every step: a wall clock that jumps
    back is followed, and a clock reader mounted by the step sees the
    time now), ticks the logic clock to `now`
    and says when to come back (`Wake { deadline, wall }`,
    `sleep_for(now, wall_now)`). The `strand run` logic thread is: feed
    the `screens` service from the surface layer's monitor hooks
    (`screens.all` with each `Screen { id: MonitorId, name: connector,
    … }`, `screens.focused`; `monitor_forgotten` →
    `forget_screen`); map render's `InputEvent`s to `event(node, name,
    args)` (`click`, `secondary`, `scroll` with `dy, dx`, `activate`,
    …; delivered through core input queues to the innermost element
    with a handler, every handler of that event on it in source order,
    `propagate()` passes it on once), `set_flag(node, NodeFlag, on)`,
    `set_size(node, w, h)` (layout facts for `self.width`) and
    `write(node, prop, PropValue)` (`<->` writes, outside any handler,
    checked against the place's type); call `step` and send the diff to
    render; sleep until `Wake::deadline` (logic clock), `Wake::wall`
    (on a realtime timer, so a suspend or clock step does not delay
    it), input, a monitor hook or the runtime's wake hook, whichever
    comes first. These take scene
    `NodeId`s; render produces them with `Renderer::hit`.
  - `get`/`set(path)` read and write exported `file.name` values and
    fields inside them (`theme.prefs.compact`; the CLI), `set` checked
    against the declared type. Dropping an `Instance` (or `shutdown`)
    disposes everything it mounted. `SceneMirror` applies diffs to a
    retained mirror, checks their consistency and renders it as text
    for snapshots.
  - Live reload: `Instance::from_build(rt, &Build, host, storage)`
    mounts a build with its identities; `Instance::reload(&Build) ->
    Report` commits a new one into the running instance. The new
    program is mounted next to the old one, which hands over by reload
    key (the scope's place in the instance tree, `/s<sid>[<monitor>]/
    c<sid>/f<sid>[<key>]`, plus the node's `Sid`): scene nodes keep
    their ids (the diff is the new tree reduced against what render
    shows: props that changed with their own transitions, moves,
    creates, removes; a kept surface whose layer or namespace changed
    gets a new id with its children moved under it; one of another
    surface kind is a new node with its children and state kept), state
    cells are
    reparented (a changed default adopted only where the value still
    held the old one, as a reload write; renamed or retyped cells and
    `@reset` reset; a surface's cells are keyed by its scope, not its
    name, so a renamed surface keeps them; a surface changed between
    `bar` and a single surface keeps its cells when the bar has one
    instance on that side, and resets them with a warning when it has
    several), handlers with an unchanged hash keep their tasks
    (others are disposed with the old instance, cancelling their
    `await`), timers and debounces rescale from the old countdown, a
    parked bar's cells wait for its monitor. While a `lock` is shown a
    build that changes a lock is not committed (`LockDeferred`); the
    caller retries after `lock_shown()` turns false. Lock hashes are
    compared with the running build's, so once a lock edit waits, every
    later build carries it and waits too (decisions.md, wave2-runtime). `reload_hard(&Build)`
    unmounts everything (persisted cells flushed) and mounts afresh;
    what the old tree held of the services (each service and the fields
    it read) stays held until the end of the next tick, by when the new
    tree has taken its own, so no built-in service stops and no stream
    switches off and on. The diff comes with the next `step`/`tick`/`flush`.
  - `Instance::freeze(&RuntimeError)` also outlines the frozen
    component's top nodes (or the failing node) with a 2 px red
    `border`; `thaw` restores it, and a reload's new tree clears it.
  - Nodes outside the program (the error overlay):
    `external_create(kind, parent, index)`, `external_set(id, prop,
    value)`, `external_remove(id)`, `is_external(id)`. They share the
    instance's id allocator and diff, and survive reloads (hard ones
    too); input on them is the caller's.

- **M4 additions** (planned; docs/m4-plan.md).
  - Shaders (`check/shaders.rs`, feature `shaders`, on by default): a
    `shader "x.wgsl"` file is read by the loader, registered with the
    watcher as `Role::Shader` (`Instance` reports it with the other
    referenced files), and checked with naga as `strand_scene::shader::PRELUDE`
    followed by the file (diagnostics point into the file, the prelude's
    lines subtracted): it parses and validates, has exactly one
    `@fragment` entry and no `@vertex`, and each `var<uniform>` in
    `@group(1)` is named `u_*`, is `f32` or `vec2`–`vec4<f32>`, and
    matches a `u_*` prop by name and type. A prop the file lacks is an
    error with a did-you-mean; a uniform no prop sets is a warning and
    zero-filled. This replaces `check/tree.rs::uniform_ty`'s interim
    limit. The instance sets `Prop::Shader` to the checked text and its
    reflected slots (`ShaderCode`), so render runs what was checked and
    never re-reads the file; a saved file that fails the check keeps the
    last good build, as a module does. Without `shaders` (the CPU-only
    build), the interim limit stays, `ShaderCode` carries no slots, and
    the node draws nothing, as with no device.
  - List windows: a `for` that is a `list`'s direct child mounts through
    `mount_keyed` only the rows of the list's window, row state kept by
    key; `Instance::set_list_window(list, first, count)` (from
    `ToLogic::ListWindow`) mounts and unmounts rows by key, sets
    `Prop::RowCount`/`RowFirst` and marks those ops `window`. A change
    to a mounted row is its ops; a change outside the window sends no row
    op. `nav` selects by index, and a selection lands when its row mounts.
  - Drag and drop: the instance sets `Prop::Drag` (the dragged value's
    type) and `Prop::Accepts` (from each `on drop` parameter's type); a
    `NodeEvent::Drop` with `DropPayload::Node` is delivered with that
    source node's `drag:` value, an `External` one as a `Drop` record.
  - The lock: only an `auth` success unlocks; the runtime then writes the
    lock's `open` false, and a config write of `false` while locked is
    ignored with a warning. `lock_shown()` follows `ToLogic::LockState`,
    the compositor's state, not the `open` prop.
    As built (m4-lock wave 1): `Instance::set_session_lock(SessionLock)`
    takes the report (`Locked`, `Finished`, `Unlocked`; the binary maps
    strand-surface's `LockState` onto it, since the compiler does not
    depend on strand-surface). While `Locked`, the `lock`'s `open`
    binding sends `true` whatever the config wrote, so the content stays
    shown and live, with the notice `IGNORED_CLOSE` once per lock
    session; after `Unlocked` the instance writes `false` through a
    two-way `open` (a one-way one is left alone). `lock_shown()` is
    true while `Locked`, false after `Finished` or `Unlocked`, and,
    before any report since the lock last opened (a host with no session
    lock included), true while a `lock` on the scene is not `open:
    false`. The code is `instantiate/lock.rs`; `check/lock.rs` makes a
    second `lock` an error (`check::lock_twice`) and `screens`, `layer`,
    `anchor` and `keyboard` on a lock warnings (`check::lock_prop`).
  - `compositor-rules`: a query over a `Build` listing the surfaces whose
    tree has `blur`, with their namespaces (`strand-<Name>`), for
    `strand compositor-rules` (S-surface owns it).
  - Checks live in per-stream submodules (`check/{surfaces, effects,
    lists, lock}.rs`); `attach` and `scrim` are allowed only on `popup`
    and `panel`.

- **Formatting** (`strand_compiler::fmt`): `format(src) -> Result<String,
  FormatError>` (and `format_parsed(src, &Parse)`), the one formatter
  behind `strand fmt` and LSP formatting. It never changes a file's
  meaning: a file with syntax errors is `FormatError::Syntax(errors)`, and
  a result whose tree differs from the input's up to spans
  (`fmt::shape(&ast::File) -> String`, the tree's `Debug` with spans
  removed) is `FormatError::Unstable` rather than written. The output is
  idempotent, keeps comments and line breaks, and ends with one `\n`.

### Config files

`strand_compiler::source::find_files(dir) -> io::Result<Discovery { files,
dirs, errors }>` defines the **`.strand` module set** of a config, and only
that: `.strand` files at most `MAX_DEPTH = 3` directories below the config
directory, names starting with `.` skipped (files and directories), symlinks
followed, a breadth-first walk with directories and files deduplicated by
canonical path (so each is claimed at its shallowest path), unreadable
sub-directories and dangling `*.strand` links reported in `errors` and
skipped. `dirs` lists the canonical path of every directory scanned, link
targets included. `strand check` uses it today; the loader must call it (not
reimplement it), so they never disagree on the module set. `strand-watch`
does not depend on `strand-compiler` for this: the binary calls `find_files`
and hands the watcher plain paths (`files`, `dirs`) through its constructor,
and again on every rescan through a `rescan` callback the binary supplies.
Directories past `MAX_DEPTH` that hold `.strand` files are listed in
`Discovery::too_deep` so `strand check` can warn that they are not loaded.
`strand check <file>` checks the file with the rest of its config (the
default config directory if the file is in it, else the file's own
directory, both through `find_files`) and reports only the diagnostics
with a label, primary or secondary, in that file (so both files of a
cross-file redeclaration report it).

It is not the watch set. Per design.md ("Change sources") the watcher also
watches `.wgsl` shader files, settings TOML (`state … from "…"`), wallpaper
and other referenced paths, and the canonical target directories of linked
files and directories (`Discovery::dirs`, plus the parent directory of each
canonical file path). Paths referenced from code come from the compiler
(service and file paths it collects), not from this scan.

### `strand-text`

Request/response over a channel: `TextRequest { key, text, style, max_width,
scale }` → `TextLayout { key, size, glyph runs }`. `TextStyle` holds the
font, line height, alignment, `ellipsis` (start, middle, end), `max_lines`
and `spans` (byte ranges with weight, italic, underline or colour: marks,
markup); a glyph run's `color` is its span's, else the node's, and an
underlined span's run carries its `underline` rect (physical pixels). A
weight picks the family's face by CSS font matching, and a face is
emboldened only for a weight of 600 or more on a face lighter than 600
(CSS `font-synthesis-weight`): a 500 on a family with only 400 and 700
faces draws the 400 face. Glyph atlases are keyed by
scale and LRU-bounded. Render draws the last delivered layout.
Each `TextLayout` also carries the `AtlasUpload`s (alpha pixels) for glyphs
rasterised while producing it, which render applies to its mirror of the
atlas in arrival order, and leases on the atlas pages it uses, so the worker
never recycles a page a live layout draws from. Page generations are unique
in the process, so a mirror never confuses a recreated page with an old
one. Two more messages share the request channel, in order:
`cancel(key)` (skip a still-queued request; render sends it when a request
is superseded) and `drop_scale(scale)` (free that scale's atlas; render
sends it when no surface uses the scale and drops its mirror pages at the
same time, so a returning scale re-uploads its glyphs). The worker drains
its queue before shaping (and folds in newly arrived cancels before each
request) and survives a panicking request: it starts a fresh engine and
answers with an empty layout whose `is_reset()` is true, on which render
drops its mirror and every layout and re-requests its text. Each scale's
atlas is capped at `AtlasConfig::max_bytes` of alpha (1 MiB by default;
glyphs that do not fit are skipped), fonts at `MAX_FONT_PX` (512) and
text at `MAX_TEXT_BYTES` (64 KiB) per request. A layout that had to
skip glyphs for want of atlas room says so (`is_incomplete`; render asks
again a bounded number of times), and each layout lists its scale's live
pages (`atlas_pages`), so the mirror drops pages the worker trimmed.
Dropping the worker discards its queue. `reload_fonts()` (the installed
fonts changed: the watcher's `CacheKind::Fonts`) builds a fresh engine and
answers with a reset layout (key 0), on which render forgets every layout
and shapes again (`Renderer::fonts_changed`; inline,
`TextEngine::reload_fonts`). `Renderer::icons_changed` is the icon side:
`strand_icons::invalidate`, every icon decode dropped (one in flight is
dropped on arrival) and the surfaces drawing icons repainted. Each layout lists its caret stops
(`TextLayout::carets`: every cluster boundary per line, byte offset and x
in logical pixels), from which render draws an `input`'s caret and
selection and places the caret under a click. `TextWorker::waker()`
hands out the render loop's waker (the one the worker was spawned with)
for render's other workers: the image decoder and the tooltip timer
wake the loop through it, so the binary wires one waker only.

### `strand-surface`

Owns the Wayland connection with smithay-client-toolkit: layer-shell surfaces
per output, a 2–3 buffer shm pool per surface with buffer age,
`damage_buffer`, `set_opaque_region`, fractional scale + viewporter, frame
callbacks only while `Painter::wants_frame`, `wp_presentation` timing, output
hotplug (monitor identity = make + model + description), and input forwarded
as `InputEvent`s. It creates and updates surfaces from the `SurfaceSpec`s
and `SurfaceChange`s render reports (render loop step 0): namespace
`strand-<Name>`, anchor from `edge` (stretched along it) or `anchor`,
`margin`, size, `exclusive_zone()`, keyboard interactivity, the outputs
`screens` selects, and mapping by `open`. Compositor-animated poses (alpha modifier, viewporter,
margins) are its job in M4.

Interface (main thread; `SurfaceManager<H>` owns the calloop `EventLoop`
and the connection):

- `trait SurfaceHost: Painter` is what it calls: `paint`/`wants_frame`/
  `opaque_region` plus no-op-default hooks `surface_attached(surface,
  node, Option<&Monitor>)` (`None` for a `screens: focused` surface the
  compositor places), `surface_entered(surface, &Monitor)` (where that
  one was shown), `surface_configured(surface, size, scale)` (once per
  wakeup, right before the first paint at that size), `surface_detached`,
  `monitor_added(&Monitor, reconnected)`, `monitor_changed` (scale,
  logical size or position; same identity), `monitor_removed`,
  `monitor_forgotten` (30 s after an unplug), `frame_deadline(surface) ->
  Option<Instant>`, `frame_dropped(surface)` and `input(&InputEvent)`
  (main thread, for hit testing). The binary implements it on a wrapper
  around `Renderer`, forwarding to `attach_surface`, `configure_surface`,
  `detach_surface`, `frame_deadline` and `invalidate`.
- `SurfaceManager::connect(host, Config)` / `with_connection(conn, ..)`;
  `Config { clock: Box<dyn FrameClock>, fractional_scale, max_buffers }`.
  `dispatch(timeout)` blocks while idle (no timers armed). Other sources
  (logic diffs, the text worker ping) go on `loop_handle()`; their
  callbacks get `&mut State<H>` and call `apply_surface_change(node,
  change)` for each `take_surface_changes()` entry and `poll()` (ask
  `wants_frame` again) or `repaint(surface)` (force a paint).
- `repaint_handle()` gives a `Send` `RepaintHandle` (a calloop channel:
  `Request::{Repaint(id), RepaintAll, Poll}`); `take_input()` creates the
  `mpsc::Receiver<InputEvent>` (events are not queued before; keyboard
  events included). The cursor is set on enter (`wp_cursor_shape_v1`, else the
  cursor theme); `State::last_button_serial()` is for popup grabs.
- Frames lock to the refresh rate: after a buffer commit a surface paints
  again only after that frame's callback (requested while `wants_frame`
  stays true, or always without `wp_presentation`) or its presentation
  feedback (`presented`/`discarded`, requested for every commit). Changes
  arriving meanwhile coalesce into the next paint.
- Monitors reach logic through the binary: it forwards the `monitor_*`
  hooks as the `screens` service (design: Monitors → `screens`), from
  which logic instantiates per-monitor surfaces (`Screens::Named`).
  `Monitor` carries identity, connector, make, model, description,
  `scale`, `logical_size` and `position`.
- A painter's hold is honoured: while `wants_frame` is false and
  `frame_deadline` is `Some`, no buffer is committed (the first frame
  waits for its text, a later one for a new node's); a timer at the
  deadline asks again, and any
  `poll()` before it does too.
- `screens: focused` is one layer surface created without an output
  (wlr-layer-shell puts it on the output the user last interacted with);
  `State::set_focused_monitor(Some(id))` (a compositor IPC service, M3)
  pins it, moving an open one. An `osd` has an empty input region
  (click-through).
- `FrameClock` (`now`, `presented`, `discarded`, `predict(surface)`) is fed
  by `wp_presentation` feedback; `PresentationClock` is the real one,
  `FakeClock` the injectable one. `predict` becomes `PaintTarget::time`.
- `MonitorId` is `"make | model | description"`, minus a trailing
  `" (<connector>)"` wlroots appends (a duplicate gets ` #2`, in plug
  order); `Screens::Named` matches it or the connector name. `SurfaceId`s
  are stable per (node, monitor), or (node, focused), while the monitor is
  remembered.
- A spec's `overhang` grows the layer size and moves the margins out
  (`placement::layer_config`), the exclusive zone grows by the overhang
  on its edge so margin + zone is unchanged, and the input region is the
  box inside it (`SurfaceInfo::input_region`; an OSD's is empty). Render
  makes the overhang even on the axes the anchor leaves centred, so the
  compositor centres the box. `LayerConfig::fit` clamps a layer surface's
  box to its output's logical size less its margins; the host passes the
  output's size to render (`Renderer::set_surface_bounds`), and render
  holds a content-sized surface's frame for the configure at a new size
  (`Renderer::set_resize_wait`, `strand_render::RESIZE_WAIT`; zero
  offline). Spec changes render makes while the manager calls in (a
  configure, a paint) are reported by `Renderer::has_surface_changes`;
  the binary's host then pings its loop (`Host::waking`) so they reach
  the manager at once. A click-away catcher (`SurfaceInfo::click_away`) is a
  transparent layer surface on the same layer and output with an input
  region holed at the surface's box (`LayerConfig::box_in`), since
  layer-shell leaves the order within a layer undefined; every other
  output showing no surface of the same node gets one over the whole
  output (exclusive zone -1, no hole). Placement clamps values to
  ±`placement::MAX_LOGICAL` and saturates.
- The keyboard: one `wl_keyboard` per seat with xkbcommon keymaps and
  key repeat (`get_keyboard_with_repeat`), as `InputEvent::Key` on the
  surface with keyboard focus (`State::keyboard_focus`).
- Popups (`NodeKind::Popup` specs): an `xdg_popup` (`xdg_wm_base` bound
  directly, versions 1–6) nested in a mapped surface of
  `SurfaceSpec::parent` (the one last pressed, when several), through
  `zwlr_layer_surface.get_popup` or the parent popup's `xdg_surface`,
  positioned by `placement::popup_config` (a `PopupConfig`: box size,
  overhang, anchor rect in the parent's window geometry, side and gap)
  with slide and flip; its window geometry is its box, its buffer the
  box plus the overhang. Size or anchor changes reposition it
  (`xdg_popup.reposition`). It grabs with the last button or key press's
  serial when that press came within `GRAB_WINDOW` (500 ms), never for a
  tooltip (`SurfaceSpec::tooltip`: no grab, empty input region);
  `Stats::grabs` counts grabs. "Grabbing" is what was sent, not what
  the spec asked for: only a popup made with `xdg_popup.grab` dismisses
  a grabbing chain outside its own first, makes its layer surface
  `exclusive` while open (`State::holds_keyboard_for_popup`) and takes
  the keys on it (the topmost grabbing popup gets a `KeyboardEnter` of
  its own; when the keys move on, the old target gets `KeyboardLeave`
  unless the new one is nested in it, and when the grab ends the surface
  with focus gets `KeyboardEnter` again). A popup opened with no grab
  (later than `GRAB_WINDOW` after a press) takes no keyboard and leaves
  other popups open.
  `popup_done` is sent as `InputEvent::ClickAway { surface }` (before the
  surface goes), then the popup and the popups nested in it are destroyed
  (innermost first, as any surface's are), and it is not shown again until
  its spec closes. `Painter::blur_region` is read for the blur ladder
  (M4); nothing is sent yet.
- **M4 additions** (docs/m4-plan.md). 0b landed the host hooks as no-op
  defaults (`compositor_caps`, `gpu_release`, `lock_changed(LockState)`,
  `LockState` beside `SurfaceHost`); the manager calls them as the
  streams below build their parts.
  - GPU hand-off (`gpu_handoff.rs`, feature `gpu`): `State::raw_handles(
    surface) -> Option<RawHandles>`, `State::hand_off(surface)`,
    `State::take_back(surface)` and the hook
    `SurfaceHost::gpu_release(surface)`; the rules are in
    "`strand-gpu`", "Surface hand-off".
  - Capabilities: the manager binds `wp_alpha_modifier_v1`,
    `wp_single_pixel_buffer_v1` and `ext_background_effect_manager_v1`
    when offered, and calls the new hook `SurfaceHost::compositor_caps(
    &CompositorCaps)` once its globals are bound.
  - Poses: each frame it reads `Painter::surface_pose` and applies it
    (alpha modifier, viewporter destination size, layer-shell margins);
    a pose change with no damage is a bare commit with no buffer, and
    margin changes ride the next buffer commit when a paint is pending.
    Render already holds `Removed`/`open: false` until an exit settles.
  - Solid surfaces (`solid.rs`): a single-pixel buffer scaled by the
    viewporter, for scrims and lock backgrounds; shm when the protocol is
    missing. A spec's `scrim` is a full-output layer surface under the
    panel or popup, on the same layer and output; it is the click-away
    catcher too when the surface has one (the catcher becomes visible
    instead of a second surface being made).
  - Fillets: placement puts an `attach`ed box at gap 0 from that edge;
    the overhang render adds for the fillets grows the buffer only.
  - Blur ladder (`blur.rs`): `Painter::blur_region` becomes a logical
    `wl_region` inside the manager (rounded corners as about 1 px
    bands), cached and sent with `ext_background_effect_v1` only when
    the shape changes, null when empty.
  - Popups: a `popup` nested in a popup may open to the side
    (`anchor:`, right by default, flipping left), for tray submenus.
  - Session lock (`session_lock.rs`): `State::lock()` asks
    `ext_session_lock_v1` for a lock; the host hears
    `SurfaceHost::lock_changed(LockState)` (`Locked`, `Finished`, and
    `Unlocked` after an unlock). A `lock` spec gets one lock surface per
    output, including outputs plugged in while locked; config content
    goes on the focused output, a single-pixel background in the lock's
    colour on the rest. `State::unlock(strand_auth::UnlockToken)` is the
    only way to unlock; `finished` without `locked` is a diagnostic and
    the lock counts as not shown.
    As built (m4-lock wave 1): the module is
    `src/manager/session_lock.rs` (the manager's other concerns live
    there too). The content surface is a `Role::Lock` surface the host
    paints like any other (`SurfaceHost::surface_attached` with the
    spec's node, or `LOCK_FALLBACK_NODE` when `State::lock()` was called
    with no `lock` spec: the seam for render's built-in fallback);
    `State::lock_content()` names it. The other outputs get a
    manager-painted 1×1 shm buffer scaled by `wp_viewporter` (a
    full-size one without it) in `set_lock_color`'s colour, and a keyboard focus on one of them is
    delivered as the content's. A spec closing (`open: false`) or going
    away never unlocks: the surfaces stay and only `unlock` releases the
    lock; a closed spec re-arms it, so an open spec locks again only
    after it closed. `Finished` after `Locked` sends nothing (the
    protocol leaves the session's state to the compositor) and asks for
    a new lock at once, once per lock session, so a session the
    compositor keeps locked gets its password field back; the host
    hears `Finished`, then `Locked` or `Finished` for the new lock. The tests take a session lock
    only inside the lock VM (`scripts/lockvm/scenarios/`).
    Nothing locks until `State::enable_session_lock()`: before it an
    open `lock` spec is a warning and `State::lock()` returns
    `LockError::NotEnabled`. The binary calls it only once `auth`'s
    tokens reach `State::unlock` (wave 2, `run/lock.rs`), so no build
    can take a lock it cannot release. A token given while the lock is
    pending is kept and spent when `locked` arrives (`destroy` would be
    a protocol error if `locked` is already on the wire).
  - Drag and drop (`dnd.rs`): a `wl_data_device` per seat produces
    `InputEvent::Drag*` (external files, apps and text as
    `DropPayload::External`); `State::start_drag(surface, node)` starts
    a drag between Strand surfaces with a Strand-private MIME type that
    carries the node, delivered as `DropPayload::Node`. Drags out to
    other programs are out of scope.
- Later (planned, so the current shape does not block them):
  - `State::recreate_all()` for `strand reload --hard`.

Module map (`src/manager/`, split by concern in M4 wave 0 with no
behaviour change; the public API is re-exported from `lib.rs` as before):
`mod.rs` (the `SurfaceHost` trait, `Config`, `SurfaceError`, `Request`,
`RepaintHandle`, `Stats`, `SurfaceInfo`, the `Surface`/`Role` records,
`State`, `SurfaceManager`, the public `State` API and node reconciliation),
`layer.rs` (layer surface creation, in-place reconfiguration, configure,
close and destruction), `popup.rs` (xdg popups: nesting, positioners,
grabs and the keyboard they hold, dismissal), `catcher.rs` (click-away
catchers), `outputs.rs` (hotplug, monitor identity and expiry),
`commit.rs` (geometry, paint and commit, frame callbacks, deadlines and
presentation feedback), `seat.rs` (pointer and keyboard input, key repeat)
and `protocols.rs` (registry, shm, viewporter, fractional scale and the
presentation global). New M4 concerns get files of their own beside them.

### `strand-gpu`

(M4, planned; docs/m4-plan.md wave 0c, from the wave-0 spike: decisions.md,
m4-gpu-spike.) The GPU backend is in every build and falls back to the
CPU (decisions.md, m4-owner). vello_cpu into `wl_shm` stays the default
for every surface; the GPU draws a surface only while it animates a
large area, and shaders. Nothing in this crate runs, and no Vulkan
library is mapped, until something needs it.

**Boundary.** `strand-gpu` depends on `strand-scene`, wgpu 30 (`vulkan`,
`wgsl`, `std`; no default features), vello_gpu 0.3 (`wgpu`, `std`; no
`text`, since glyphs arrive as atlas images and paths), vello_common 0.3,
raw-window-handle 0.6 and pollster; on no other Strand crate and on no
Wayland crate. Its interface:

- `Gpu::spawn(waker: Box<dyn Fn() + Send>, opts: GpuOptions) -> Gpu`
  starts the thread `strand-gpu`, which creates the instance, adapter and
  device off the main thread (45–274 ms on hardware, about 100 ms on
  lavapipe). `Gpu::send(GpuRequest)` never blocks; `Gpu::try_recv() ->
  Option<GpuReply>` is drained when the waker's ping fires on the main
  loop. Dropping the `Gpu` (or `GpuRequest::Shutdown`) drops every
  surface, pipeline, texture, the device and the instance, and the thread
  ends; the binary joins it after its `Exited` reply, never blocking on
  a device drop.
- `GpuRequest`: `Attach { surface: SurfaceId, handles: Option<RawHandles>,
  size, scale, opaque: bool }` (no handles: readback only), `Resize {
  surface, size, scale }`, `Release(SurfaceId)`, `Frame(Frame)`,
  `Shutdown`. `GpuReply`: `Ready(AdapterInfo)`, `Unavailable(GpuError)`,
  `Attached { surface, mode: GpuMode }`, `Released(SurfaceId)`,
  `Presented { surface, at: Instant }`, `Pixels { surface, frame: u64,
  pixels: Readback }`, `Lost(GpuError)`, `Exited`.
- `Frame { surface, id: u64, size, scale, ops: Vec<Op>, uploads:
  Vec<Upload>, readback: Option<Rect> }` is what render lowers its
  display list into (`renderer/backend.rs`): fills and strokes of
  kurbo paths with peniko paints and transforms, images by texture id,
  clips, blends and opacity as layers, and `Op::Pass(ShaderPass,
  bounds)`. `Upload`s carry atlas pages and cached pixmaps (images,
  gradients, shadows, masks: masks are always rasterised on the CPU)
  keyed by id and generation; they live on the GPU until the device
  drops. The GPU thread builds the vello_gpu scene from the ops, so
  that work is off the main thread.
- `Readback` is `Bgra8Unorm` premultiplied rows (the `wl_shm` ARGB8888
  byte order), padded to wgpu's 256-byte row alignment, with its stride.

**Thread.** One thread, started on the first demand and ending with the
device; one device per process, shared by every surface. The adapter is
requested without a surface, so readback works whatever the WSI can do.
A software adapter (`DeviceType::Cpu`, lavapipe) counts as no device:
it would draw on the CPU and keep about 80 MiB mapped after the drop.
`STRAND_GPU_SOFTWARE=1` accepts it, and a user never needs it. The
lavapipe tier is to set it beside `STRAND_REQUIRE_GPU=1`; that is
pending: CI's env, `run.sh` and `ci.sh` set only `STRAND_REQUIRE_GPU`
today, and the integrator adds `STRAND_GPU_SOFTWARE=1` with S-gpu's
first test that needs it. The adapter is requested with
`PowerPreference::HighPerformance`, so a hardware adapter wins when both
exist. The thread waits on its
channel and on presents, never on a timer: it does not decide when to
stop.

**Promotion** (`strand-render/src/promote.rs`, a pure state machine per
surface; `Renderer` drives it and reports changes with
`Renderer::take_backend_changes() -> Vec<BackendChange>`, which the
binary carries out):

- A surface is promoted only for heavy animation (design.md, Paint):
  after more than 500 ms in which every frame damaged at least 0.2 Mpx
  (render still diffs display lists on a promoted surface, so it knows
  the damage a CPU frame would have had). A shader pass also needs the
  device, without promoting its surface (below).
- Backends switch only when springs settle: the switch waits for a
  frame with no spring in flight on that surface. Clocks (time signals,
  shader time, particles, animated images) do not count as springs; a
  surface whose springs never settle stays where it is.
- Demotion is the same rule reversed: 500 ms of frames under 0.2 Mpx,
  then the next settled frame.
- The device is dropped 30 s after the last GPU frame (presented, read
  back, or a pass) once no surface is promoted. Render puts that instant
  in `Renderer::next_wake()`, so it costs the main thread one wake;
  render then reports `BackendChange::Drop` and the binary drops the
  `Gpu`.
- A failed start or a lost device is recorded as `GpuStatus::Unavailable
  { reason }`. Every promoted surface goes back to the CPU at once (it
  cannot wait for a settled frame) with a full repaint. Render asks
  again at most once per 30 s while demand lasts.
- Under `reduced_motion` the clock stops at `t = 0`
  (`TimeContext::frozen`), so a shader's `time` reads 0 and its pass
  runs only when its uniforms change.

**Backends** (`strand_scene::Backend`, what `Renderer::set_backend(surface,
Backend)` is told once the GPU thread answers):

- `Cpu`: vello_cpu into the surface's shm buffer, as today.
- `GpuPresent`: the GPU thread presents through wgpu's WSI. Chosen when
  `Surface::get_capabilities(&adapter)` is non-empty and offers
  `PreMultiplied` alpha (or the surface is opaque) and a non-sRGB
  `Bgra8Unorm` or `Rgba8Unorm` format, so blending matches vello_cpu's.
  Present mode `Fifo`. Frames send full damage until wgpu's
  `present_with_damage` lands (design.md).
- `GpuReadback`: the GPU renders the whole frame offscreen and copies it
  into a mapped buffer; the main thread copies that into the shm buffer
  and commits it as a CPU frame with full damage. Chosen when the WSI
  cannot present: the spike found ANV's WSI needs linux-dmabuf, which a
  pixman compositor (CI's sway) does not offer. It worked on every
  compositor and device tried (3.6 ms a frame on lavapipe, 0.76 ms on
  Intel at 256×128).

The negotiation: render reports `Promote(surface)`; the binary starts
the `Gpu` if needed and sends `Attach` with `State::raw_handles` (none
for readback-only); the reply's `GpuMode` becomes `Backend::GpuPresent`
or `GpuReadback`, and render switches at its next settled frame (for
`GpuPresent`, after `State::hand_off`). Until the device is up and the
surface is settled, the CPU keeps drawing: promotion never stalls a
frame.

**Frames.** In readback mode, `Painter::paint` lowers the frame, sends
it, and returns empty damage while it holds the frame for the pixels
(`frame_deadline` reports the hold, as for text); the `Pixels` reply
calls `Renderer::deliver_gpu`, the surface is polled, and the next
`paint` copies them in. In present mode the surface manager does not
call `paint` for that surface: the GPU thread's `Presented` reply is its
frame callback, the binary asks render for the next frame
(`Renderer::paint_gpu(surface, at) -> Option<Frame>`, `at` the last
present plus the output's refresh period, which `PaintTarget::time`
would have been) and sends it. One frame is in flight per surface.

**Surface hand-off** (`strand-surface/src/gpu_handoff.rs`). One
`wl_surface` moves between shm and the WSI; it is never recreated.
- `State::raw_handles(surface)` returns the connection's `wl_display`
  and the surface's `wl_surface` as `raw-window-handle` handles. It needs
  `wayland-backend`'s `client_system` feature, which strand-surface's
  `gpu` feature turns on (on by default). That feature switches the
  backend for every crate in the build to libwayland-client (57 kB of
  `.text`, and a library every Wayland desktop has); the CPU-only build
  keeps the Rust backend.
- `State::hand_off(surface)`: from then the GPU thread is the only
  thread that commits that `wl_surface`. The manager stops attaching shm
  buffers, requesting frame callbacks and calling `paint` for it. It
  still handles configures (the ack is sent, `surface_configured` tells
  the host, which sends `Resize`, and the next present commits the new
  size) and sets pending state (input and opaque regions, margins from
  placement) without committing; the next present applies it.
  Compositor poses are not delegated while presented: render paints the
  pose into the GPU frames, which are full frames anyway.
- `State::take_back(surface)` after the GPU thread's `Released` reply:
  the manager paints and commits a full shm frame (`age` 0) and resumes
  frame callbacks and pose delegation. Between `Release` and the next
  shm commit nothing commits that surface.
- When the manager has to destroy or recreate a handed-off surface
  (`Removed`, an unplugged output, a layer surface `closed`), it calls
  `SurfaceHost::gpu_release(surface)`, keeps the `wl_surface` alive, and
  destroys it at `take_back`. The swapchain is therefore always dropped
  before its `wl_surface`.
- In readback mode the surface is never handed off: the main thread
  commits, and poses are delegated as on any CPU surface.

**Shaders and effects.** `strand_scene::effect::Effect::Shader(ShaderPass)`
with `ShaderPass { code: ShaderRef, uniforms: Arc<[f32]>, input:
ShaderInput }`. `ShaderRef` is `Bundled(Bundled)` (design.md's eight
bundled GPU effects; their WGSL lives in `strand-gpu`, and the stream
that builds each one records its knobs) or `File(Arc<ShaderCode>)`. `ShaderInput` is `None` (a
`shader` node draws in its box), `Content` (a `filter:` pass gets its
subtree's pixels: the F4 cached group) or `Backdrop` (`backdrop:
glass()` gets what is under it in the surface). Render packs `uniforms`
each frame from the springing `Prop::Uniforms` in the code's slot order,
in buffer units (the ABI's, below), so they are what the GPU uploads.
The reach of a bundled pass is its own (bloom's radius, chromatic's
offset, wobble's amplitude: each effect's builder puts it in uniform
slot 0, which `Bundled::reach(uniforms, scale)` reads and divides by
the scale back to logical pixels); a file's pass draws inside its
box. As landed in 0b, `Bundled` has nine variants for the eight
effects, CRT and chromatic aberration being two spellings (`bloom`,
`glass`, `particles`, `tilt`, `wobble`, `crt`, `chromatic`, `aurora`,
`backdrop_blur`), and `ShaderCode { path, wgsl, uniforms:
Vec<UniformSlot { name, ty: UniformType, binding, offset } > }` holds
the file without the prelude (`ShaderCode::module()` prepends it). The
checker fills the slots with `ShaderCode::packed`: in binding order,
`offset` the slot's first `f32` in `ShaderPass::uniforms`, dense (each
slot starts where the previous one ends). There is no packed WGSL
block, so no WGSL layout rule applies: the GPU thread copies each
slot's floats to its own binding's range, aligned to the device's
`min_uniform_buffer_offset_alignment`.

The ABI (the prelude `strand_scene::shader::PRELUDE`, prepended by the
checker and by the GPU thread alike): Strand supplies the vertex stage
over the node's box; the file supplies one `@fragment` entry that takes
`StrandVertex` (`uv` in 0..1 over the box, `pos` in buffer pixels) and
returns a premultiplied `vec4<f32>`. `@group(0)` is Strand's: `strand:
Strand` (`time` in seconds since the node appeared, `size` in buffer
pixels, `scale`, `pointer` in buffer pixels relative to the box or -1
when outside), `strand_input` (`texture_2d<f32>`, 1×1 transparent for
`None`) and `strand_sampler`. `@group(1)` holds the file's `u_*`
uniforms, each its own `var<uniform>` of type `f32` or `vec2`–`vec4<f32>`
at a binding the file picks (one value per binding, not a struct).
Values arrive as `f32` in buffer units: lengths in px × scale,
angles in radians, durations in seconds, colours premultiplied linear
`vec4`.

On a surface that is not promoted (the usual case: a small aurora
behind a bar's clock), a pass is drawn offscreen by the GPU at its
bounds and read back into the CPU frame as a raster item; the surface
does not switch. The frame holds for the result like a readback frame,
up to `GPU_WAIT` (8 ms), and otherwise draws the previous result. On a
promoted surface passes run in the GPU frame.

**CPU fallback** (no device yet, none at all, a lost device, or a build
without `gpu`). Rendering stays on the CPU. Bundled effects use their
CPU versions: bloom becomes glow, glass becomes blur and tint, particles
cap at 1,000, tilt stays 2D, aurora is static, large backdrop blur is
the quarter-scale CPU blur; CRT, chromatic aberration and wobble draw
the node unfiltered. A `shader` node keeps its box and draws nothing.
The same applies while the device starts, so a first frame never
waits for it. `Renderer::gpu_status() -> GpuStatus` (`Unused`,
`Starting`, `Up(AdapterInfo)`, `Unavailable { reason }`, with
`"built without the GPU backend"` as a reason) is sent to logic as
`ToLogic::GpuStatus` when it changes while a shader or bundled effect
is shown; logic logs it once per reason, reports it as a `strand
watch` notice, and keeps it for `strand report` and the inspector (M5)
to say why.

**Budgets and tests.**
- `.text` (`strand/tests/budgets.rs`): the default build at most 18.5
  MiB (19,398,656 B: the spike's 17,683,543 B with GPU code linked, plus
  9.7%); `strand --no-default-features` at most 15 MiB (the spike's CPU
  build had 1.14 MiB to spare). The spike measured the GPU build's bar
  PSS at a mean of 33.7 MB, about 0.3 MB under the 34 MB target, all of
  it cold-code mapping and relocations; S-gpu measures again when the
  backend lands.
- Cold (`strand/tests/gpu_cold.rs`, per feature set with `cargo
  metadata`): wgpu, vello_gpu and naga reach `strand` only through
  `strand-gpu`, and naga also through `strand-compiler`'s `shaders`;
  none of them in `--no-default-features`. `budgets.rs`: no `libvulkan`
  mapped before promotion.
- Idle on lavapipe (`strand/tests/gpu_idle.rs`, enforced in CI): after
  each promote, frames and drop the `strand-gpu` thread is gone and the
  process takes no wakeups, and PSS after the second cycle is within 3
  MiB of PSS after the first (no growth). PSS back to the pre-GPU
  baseline is not asserted there: lavapipe and LLVM stay mapped after
  the device drops (about 80 MiB the spike measured, the same every
  cycle).
- Hardware leg (`gpu.sh`, advisory): the same tests on
  `/dev/dri/renderD128`, plus PSS after the drop within about 6 MiB of
  the pre-GPU baseline (the spike's ANV left 5.2–6.3 MiB, all libraries
  unmapped), and the promoted cost checked against design.md's +20–40 MB
(the spike's ANV: about 15 MiB with the device up, 19–24 MiB
presenting).
- GPU frames are compared with the CPU frame under their own documented
  tolerance, not `assert_matches_ref`'s.

### `strand-dev`

The language server, `strand-dev lsp` (stdio), built on `lsp-server` and
`lsp-types`; the runtime binary never links it. `strand_dev::serve(&
lsp_server::Connection)` runs the protocol on any connection (tests drive
it in process over `Connection::memory()`) against the builtin schema;
`serve_with(&Connection, Arc<Schema>)` takes the schema to check with,
chosen once by the caller (the builtin schema extended with
`Schema::extend` by the service crates it links); `run_stdio()` serves
stdin and stdout, and `capabilities()` is what it advertises. It reads
only `strand-compiler`'s public interfaces: `compile_with` for one config
at a time against that schema, `hir::Program` (`refs`/`reference_at`,
defs, locals, `tokens`, the typed tree) for hover, definition, rename and
completion, `Schema::doc` (of the same schema) for completion and hover
text, `Diagnostic::suggestions` for quick fixes,
`schema::members_of` for what `x.` offers, `fmt::format` for
formatting, and `source::find_files` for which files a document is
checked with (the rule of `strand check <file>`: the default config
directory if the file is in it, else a workspace folder that is itself a
config, else the file's directory, else the file alone; see
`docs/decisions.md`, wave2-lsp). Open documents replace their files' text
on disk; files read from disk are re-read when their size or mtime (or a
scanned directory's) changes, and clients that can are asked to watch
`**/*.strand`. Positions are UTF-16 (the protocol default), sync is
full-document, diagnostics are published per config after a 200 ms
debounce (`initializationOptions.debounceMs`), at once on open and save,
and workspace edits use versioned `documentChanges` when the client
supports them. `initializationOptions.configDir` overrides the default
config directory.

The inspector joins it in M5; tree-sitter highlighting is not built yet
(see `docs/decisions.md`, wave2-lsp).

### `strand-services`

The M3 service contract (wave 4). `strand-services` depends on
`strand-core`, `strand-watch`, its proc-macro crate
`strand-services-macros` (re-exported) and `strand-services-schema` (the
builtin services' schema texts as constants and `schemas()`, with no
dependencies: what `strand-dev` links instead of the runtime; each
service module's `SCHEMA` is its constant there), never on
`strand-compiler`: it
never sees `Value`. Service state is typed Rust; where something must be
handled by name it is `strand_services::Data` (`Null`, `Bool`, `Int`,
`Float`, `Text`, `Duration`, `Color(Rgba)`, `List`, `Record { ty,
fields }` by name, `Enum { ty, variant }`), with `ToData` / `FromData` /
`SchemaType` (the schema spelling: `float`, `text?`, `[Workspace]`) for
the primitives, `Option`, `Vec` and derived types.

`strand_services::child` is the spawn contract: `thp_off()` (called
once, from `strand`'s ELF constructor), `restore_in_child()` and
`thp_enabled(status)`. Every program strand starts, in any crate (the
apps service's launches, `from exec` services, the overlay's editor,
and later the M4 lock helpers and M5's `strand call` and inspector),
must run `child::restore_in_child()` as its `CommandExt::pre_exec`:
the kernel keeps `MMF_DISABLE_THP` across `fork` and `execve`, so a
child spawned without it, and all its descendants, run without
transparent huge pages for life.

- **A service** is a state struct:

  ```rust
  #[service(name = "battery")]   // schema: the `SCHEMA` const in scope (or `schema = …`); + action = A, call = C, fns = f, thread
  #[derive(Store, Clone, Debug, Default, PartialEq)]
  pub struct Battery {
      pub present: bool,
      #[store(rw)] pub level: f64,                // `<->` / assignment
      #[store(keyed)] pub devices: Vec<Device>,  // a keyed collection
      #[store(stream)] pub scan: Vec<Ap>,        // produced only while a visible reader reads it
      pub received: Event<Notification>,          // an event (`()`: none, a tuple: several)
  }
  impl Battery { async fn run(cx: Cx<Self>) -> Result<(), ServiceError> { … } }
  ```

  `#[derive(Store)]` generates `BatteryPatch` (`Send`: one variant per
  field with its new value, a keyed list's `Vec<VecDiff<K, T>>`, an
  event's payload), `BatteryEvent` (one variant per event: what
  `Cx::emit` takes), `BatteryCells` (logic thread: `Signal<T>` per
  field, `KeyedSignal<K, T>` per keyed list, `EventQueue<T>` per event)
  and the `Store` / `Cells` impls (`FIELDS`, `EVENTS` with names, schema
  types, `rw`, `keyed`, `key` (the item's `Keyed::KEY_FIELD`),
  `stream`, `///` docs (held equal to the schema text's by test);
  `diff(old, new)`, `apply`,
  `field_patch`, `item_patch(field, key, sent)` (an item write's
  answer: one `Update`); cells `apply(patch, How::{Initial,
  Report(echo_of)})` (a plain field's boot value is skipped while a
  local write is in flight, a keyed list's boot value keeps items
  written in flight, a keyed report goes through `receive_items`),
  `snapshot`, `read(field) -> Data` tracked, `ids`, `write(field, Data,
  send)` with `write_tagged`, `write_item(field, key, path, value,
  send)` with `write_item_tagged`, `forget_echoes` (a run ended without
  answering its writes), `keyed_items`). `#[derive(Data)]` on a
  record struct (`#[data(name = "Workspace", key = id)]`, `#[data(rename
  = "type")]` on a field; `key` implements `Keyed`) or a unit enum
  (variants in snake_case). `#[derive(Call)]` on an enum of actions or
  async methods: variants in snake_case, fields are the arguments in
  order, a field named `item` takes the item an action was called on
  (`ws.focus()`), so the language side routes a record's item actions
  to the service whose `item_records()` names it; `FromCall::signatures()`
  lists each call's name, arity and item record (`CallSig`), which a
  test holds to the schema's `action`/`fn` declarations. `#[service]`
  implements `Service` (`NAME`, `schema()`: its declarations in the
  schema language, `Action`, `Call` (`NoCall` by default), `call`: `fn`
  methods computed on the logic thread over the cells, `start`).
- **Service side** (`Cx<S>`): `state()` (the logic thread's values when
  it started, then its own updates); `update(|s| …)` sends the fields
  that changed as one `Envelope` (one tick); `send(patches)` for
  hand-made keyed diffs; `emit(BatteryEvent::…)`; `report(&write, |s|
  …)` answers a write (always naming the written field, tagged with its
  field index and generation, so the logic thread ignores the echo and
  drops a refused optimistic value; other fields the answer changes are
  outside changes); `ready()` ends the boot phase (updates before it are boot
  values: `on change` takes them as its baseline; it also ends the first
  frame's wait); `recv().await` / `blocking_recv()` / `try_recv()` give
  `Msg::{Write(Write { field, key, path, value, field_value, held,
  generation }) (an item write names the keyed list in `field`, the
  item's key in `key`, the path below the item, the list's item with the
  write applied in `field_value`, and in `held` the record the writer
  held, which may be an item that left and whose key was reused since;
  `None` for a field write),
  Action(S::Action), Call(S::Call, Reply), Visible(bool), Watch { field,
  on }}` and `None` once stopped; `visible()` (a reader is visible: a
  service polling as a whole, cpu or memory, runs only then);
  `watched(field)` (a visible reader reads that `#[store(stream)]`
  field: a Wi-Fi scan or a level meter runs only then; `Msg::Watch`
  says when it changes); `session()` / `system()` (one zbus connection
  per bus per runtime thread, on the `Buses` the registry was given,
  shared behind one connect, pinged before reuse so a restarted daemon is
  reconnected, dropped with the last body on the thread; an error, not a
  panic, without a tokio runtime: a service on its own thread runs one
  to use them). A body returns when stopped; one that returns an error
  while read is started again after a backoff (1 s doubling to 30 s,
  reset only after a run stayed up 30 s, so a body failing right after
  `ready()` keeps backing off), and services following a daemon may reconnect
  themselves (NameOwnerChanged) to avoid that gap; `set_notify(f)` for a
  service on its own thread with its own event loop (PipeWire's): `f`
  runs whenever a message is queued and when it is stopped.
- **Threads.** `Start::Shared`: the body runs on the one tokio
  current-thread runtime thread (`strand-services`, a `LocalSet`, so
  bodies need not be `Send`), started lazily with the first such service
  and joined by `Services::shutdown`; stopping drops the body's future.
  `Start::Thread` (`#[service(thread)]`, a blocking `fn run(cx)`): a
  thread per run (`strand-<name>`), stopped by closing its messages;
  the next run's thread joins the previous one first, and
  `Services::shutdown` joins them (2 s at most, an overrun logged).
- **Logic side.** `Services::new(rt, Buses, wake)` (where no owner is
  current: its cells and timers live in a scope of their own; `wake` is
  called from any thread after every envelope); `register::<S>(rt) ->
  Client<S>` (`register_as::<S>(rt, name)` under an instance name, used
  in logs, timer names, `wait_ready_of` and `ServiceDiagnostic.service`:
  a no-code service is registered under its declared name;
  `registered()` counts the members); `report(ServiceDiagnostic)` logs a
  diagnostic raised outside a run (a no-code field's value that does not
  convert) and queues it for `take_diagnostics`; `pump(rt)` applies every waiting envelope (outside
  handlers, before a step: reports are not handler writes);
  `wait_ready(rt, limit)` pumps until every running service is ready
  (the first frame's wait); `shutdown()`. `Client<S>`: `cells()`,
  `acquire(rt)` / `release(rt)` (the reader count: the first acquire
  starts it; the last release tells it `Visible(false)` at once and arms
  a core timer, `STOP_GRACE` = 5 s on the logic clock, that stops it;
  an acquire inside the grace cancels the stop, nothing restarts;
  `release` disposes nothing, so it is safe in scope cleanup),
  `acquire_field(i)` / `release_field(i)` (readers of field `i`; a
  stream field's service is told `Watch` on the first and last),
  `seed(rt, |s| …)` (boot values before it first reports: a host's
  remembered values), `act(rt, a)`, `request(rt, call)` (async call →
  future of `Result<Data, String>`; like a write, both start a stopped
  service for that operation, which then stops 5 s later), `readers`,
  `field_readers`, `running` (false once its body ended), `starts`,
  `stops`, `reports` (updates applied), `dynamic() -> Rc<dyn
  DynService>`: the by-index view the language side drives (`fields`,
  `events`, `actions`, `methods`, `action_sigs`, `method_sigs`,
  `item_records` (the records its keyed lists hand out and its calls
  take), `read`, `ids`, `keyed_items`, `write(field, path: &[Step],
  Data)`, `write_item(rt, record, item, path, Data)` (the keyed list
  holding the item's key; the mirror hears of it as an `Update`),
  `action(rt, name, item, args)`, `call`, `fetch(rt, …)`, `acquire`,
  `release`,
  `acquire_field`, `release_field`, `observe(f)`:
  every keyed change and event applied, as `Applied::{Keyed { field,
  diffs: Vec<VecDiff<Data, Data>>, initial }, Event { event, args }}`;
  `initial`: a boot report the store applied as a reload write, which
  a mirror takes with `replace_all_reloaded` so `on change` skips it).
  A committed write reaches the run current then; a write the rate
  guard held that commits with no run (after the 5 s stop, or after the
  body ended) starts one, as a write to a stopped service does. When a
  run ends (its body returned, or the stop), the cells forget the
  writes it never answered (`Cells::forget_echoes`).
- **Builtin services here** (wave 4): `system` (the portal's appearance
  settings through `strand_watch::follow` on the shared runtime, plus
  `hostname`), `cpu` and `memory` (procfs, sampled once a second only
  while a reader is visible), and the D-Bus services (wave 4, a2):
  `battery` (UPower's display device and devices), `brightness`
  (`/sys/class/backlight` watched with inotify, writes through logind's
  `SetBrightness`; `brightness::set_backlight_root` /
  `STRAND_BACKLIGHT_DIR` point it at fake backlights in tests), `network`
  (NetworkManager; `access_points` is a stream field: read, followed and
  scanned only while watched), `bluetooth` (BlueZ's object manager),
  `notifications` (our own `org.freedesktop.Notifications` server),
  `media` (MPRIS; `elapsed`/`position` carried forward, ticking only
  while watched) and `tray` (StatusNotifierItem host, its own watcher
  when the session has none, DBusMenu model), and (wave 4, wm) the
  compositor's `workspaces`, `windows` and `wm` and PipeWire's `audio`
  (below). The D-Bus services are all on our own zbus calls
  (decisions.md, wave4-a2), with `logind-zbus` for `SetBrightness`.
  `apps` (wave 4, a3): desktop entries (`freedesktop-desktop-entry`;
  `NoDisplay`, `Hidden`, `OnlyShowIn`/`NotShowIn`, `TryExec`, the first
  `applications/` directory holding an id deciding it), icons checked
  against `strand-icons`, `search(query) -> Async<[Hit]>` (nucleo's
  matcher behind `apps::Fuzzy`, ranges in characters, frecency from the
  persist store's `services:apps.frecency`), `App.launch()` (`Exec` field
  codes, a terminal for `Terminal=true`, detached with `setsid` and a
  double fork); `apps::changed()` makes a running one read its entries
  again, `apps::set_config` points it at test directories. The no-code
  services (a3) run on the same contract: `custom::Custom`, one store per
  declaration (`values`, a keyed list of untyped `Data` by field index,
  and the id of its `custom::Spec`, which `custom::register` makes known
  to the body), reading `dbus` properties (introspected;
  `PropertiesChanged`; `rw` writes `Set` with the property's signature),
  a `file` (inotify on its directory, its own inode, and the watched
  directory's ancestors for their move or removal; a missing directory
  waited for from its nearest existing ancestor), a `listen` command's
  lines (merged and sent at most once per `custom::LISTEN_FLUSH`, a
  frame) or a `poll` command or file (only while visible) as
  `custom::Document`s (JSON, `key=value` lines, or text). `Client::restart(rt)` (a changed
  declaration: the run stops and starts again if read) and
  `Client::stop_now(rt)` (a removed one) serve their reloads, and
  `Client::unregister(rt)` takes a removed declaration out of `Services`
  (stopped, its cells disposed). Commands run in a process group of
  their own, ended whole (SIGTERM, then SIGKILL after 500 ms) when the
  run stops, restarts or a poll times out; `listen` lines are read
  bounded (`MAX_DOCUMENT`) and lossily decoded. `Data`
  implements `Default` (null) and `SchemaType` (`any`).
  `strand_services::dbus` is what they share: `Daemon` (a bus name
  followed through `NameOwnerChanged`, its signals by match rule, checked
  against the current owner's unique name: a restarted daemon is read
  afresh without restarting the service; `subscribe`/`unsubscribe` add
  and drop match rules as the service needs them), `get_all`/`get`/`set`
  of properties (each bounded by `READ_TIMEOUT`, 5 s: a hung daemon
  must not stall a body, whose full match streams would stop zbus's
  reader for every service on the connection; a read timing out fails
  the run, `is_timeout` telling it from an object gone),
  `properties_changed`, `apply_changed` (its re-reads of
  invalidated properties bounded by `CALL_TIMEOUT`), `owner_process` (pid
  and command of a name's owner), `activate` (`StartServiceByName` without waiting:
  a `Daemon` whose name has no owner asks once per start), `timed` (a
  call to an app given up after `CALL_TIMEOUT`, 2 s) and `timed_for` (a
  bound of the caller's: NetworkManager's activations, 25 s). The
  shared thread's session and system connections (`bus::session`,
  `bus::system`) are counted per running body that asked for them
  (`bus::with_user` wraps each shared body): the last body using one
  closes it, whatever else runs on the thread. A service owning a
  bus name (the notification server, the tray's host and watcher) uses
  a connection of its own (`bus::own_session`), so the name goes with
  the service. What a dropped body still has to say (the notification
  server's `NotificationClosed` for each one open) is a finalizer
  (`client::finalize`): a task on the runtime that the shared thread,
  ending, waits for up to `FINALIZE_LIMIT` (500 ms) before dropping the
  runtime. Calls that may wait on an app (tray items, players,
  BlueZ connects) run as tasks in a `JoinSet` owned by the body, so they
  hold up nothing and are cancelled when it stops; what they find (a
  player read, a tray item read, a failed connect for
  `bluetooth.failed`) comes back to the loop as the task's result. Pixels from D-Bus
  (`image-data`, `IconPixmap`, a menu entry's `icon-data`) are checked
  against the bytes sent before allocating, sampled down to 512 px a
  side and become content-addressed PNG files under
  `$XDG_RUNTIME_DIR/strand/pixmaps/<pid>` (`strand_services::pixmap`)
  that `image` shows by path; each file lives while a `Pinned` handle
  to it does (the notification or tray item showing it).
  `testing::PrivateBus::start_activating` gives a private bus a service
  directory that starts python-dbusmock templates (D-Bus activation).
  `strand_services::schemas()` (the same as
  `strand_services_schema::schemas()`) lists the schema texts of every
  builtin service implemented (the language extends its builtin schema
  with them; a real service may add fields and records to its stub, and
  keeps every stub field with its `rw` mark and every stub event);
  `Builtin::register(&services, rt)` registers them all. A new service
  module adds its text to `strand-services-schema` and its store to
  `Builtin`.
- **Failures are diagnostics.** A run that ends with an error (or
  panics) is a `ServiceDiagnostic { service, message, notice: false }`
  (`service: String`, the registered name);
  a body raises what the user must act on with `Cx::notice(message)`
  (another notification server owning the name, naming its process:
  `notice: true`, raised before `Cx::ready`); a failure the run survives
  and retries itself is `Cx::warn(message)` (`notice: false`, as a failed
  run's: the compositor adapter that does not understand its
  compositor). `Services::take_diagnostics()`
  hands them out after a pump, one per distinct message (a retry failing
  the same way is not repeated until a run stays up `RETRY_MAX` or ends
  cleanly). A notice no longer holding (a later run ready without
  raising it, a clean end, the service stopped: no reader for
  `STOP_GRACE`, or `Services::shutdown`, or the run saying so itself
  with `Cx::resolve()` while it goes on: the notification server once
  the other server let the name go) comes back once with
  `resolved: true`.
  `strand run` logs them (`warn`; a notice at `error`), sends them to
  `strand watch` as `notices`, and shows notices as overlay rows under
  `strand: services`, keyed by service, which a resolved one removes.
  `Services::wait_ready_of(rt, Some(name), limit)` waits for one
  service's first read (`strand set`'s relative step).
- **Calls of one name on several records.** `#[derive(Call)]` takes
  `#[call(name = "…")]` on a variant; variants sharing a name and taking
  items of different records (`TrayItem.activate()`,
  `TrayMenuItem.activate()`) are told apart by the item's record type.
- **Language side** (`crates/strand/src/services`, the binary: it
  depends on both). `services::schema()` is
  `Schema::builtin_with(&strand_services::schemas())`: `strand check`,
  the live loader (whose cache key is the schema's fingerprint) and
  `strand run` use it, and `strand-dev lsp` serves the same
  (`strand_dev::schema()` over `strand_services_schema::schemas()`,
  `serve`; `serve_with` takes any). `StoreHost`
  is one store as a `ServiceHost`: a `Memo<Value>` per plain field over
  `DynService::read` (converted by name, `services::convert`: records by
  type and field name, enums by variant), so a binding depends on
  exactly that field; a keyed field mirrored as a `KeyedSignal<ValueKey,
  Value>` fed by the store's `Applied::Keyed` diffs (keyed by the item
  record's schema `key`; an `initial` one rebaselines it), events as `EventQueue<Vec<Value>>` fed by
  `Applied::Event`; `write` refuses a non-`rw` field written whole and
  passes the leaf path as `Step`s (a leaf below a field is `rw` in its
  record, which the checker saw); `write_item` passes the item's record
  name, the item and the path to `DynService::write_item`; `call` is the store's `fn` methods; `fetch` its async
  methods (every async call in a config reaches it);
  `acquire_field`/`release_field` count readers per field. An async
  call made while nothing reads the service waits for a reader before it
  reaches it (a `let hits = apps.search(query)` only a closed launcher
  shows never searches; decisions.md, wave4-a3).
  `CustomHost` serves the config's no-code services: `declare` registers
  a `custom::Custom` store per declaration (its spec from
  `lower::CustomService`, relative `file` paths under the config
  directory), each field a memo converting the untyped value to the
  declared type (`custom::coerce`); `write` of an `rw` field is an item
  write of its value; `restart`/`stop` restart or stop only that service.
  `custom::BusIntrospector` is the compiler's `Introspect` over the
  environment's buses (answers remembered 10 s); `custom::dbus_check(config_dir, recheck)`
  returns the loader's extra check (the D-Bus check and
  `check::paths::check` under `config_dir`) with it and a `DbusCheck` handle: the
  check waits on the bus only until `DbusCheck::stop_waiting()` (called
  by `live.rs` after `boot()`); later compiles use
  `Cache::properties_or_ask` (a remembered answer even past its ttl, a
  service whose first answer is pending skipped), and an answer that
  differs from the one used calls `recheck`, which queues the worker's
  `Job::Recheck` (`Loader::recheck`). No reload waits on a bus.
  `services::set_text` is `strand set` on a service's `rw` field
  (`brightness.level +5%`: a signed number is a step from the current
  value; `Real::set_text` starts a stopped service and waits up to
  500 ms for its first read first).
  `Composite` routes by service name (one member per name), an item's
  action or write by the record's name to the member whose
  `item_records()` name it, `declare`d custom services to its
  `CustomHost` (`set_custom`; without one, under `STRAND_MOCK`, the
  fallback answers them at their defaults), and everything else (the
  clock and calendar, services no crate serves yet) to the `SchemaHost`
  fallback;
  `next_wake` is the earliest of all, `wake` reaches all. A service
  module added to `strand-services` (`Builtin`, `schemas()`) is served
  by `strand run` with no change here.
- **Tests** (`strand_services::testing`): `PrivateBus::start()` (a
  `dbus-daemon` of the test's own, on a configuration without service
  directories so nothing installed can be activated on it; `buses()`
  for `Services::new`, `env()` for a child process, `wait_for_name`,
  `restart()`: a new daemon at the same address), `DbusMock::start(&bus,
  template, system, parameters, name)` (python-dbusmock: the interpreter
  is `$STRAND_DBUSMOCK_PYTHON`, else the first of `python3`, `python3.12`
  that imports `dbusmock`; its output goes to a file in the bus's
  directory, shown in the failure when the name never appears;
  `DbusMock::log()` names that file, where python-dbusmock writes a line
  for each `Get`, `GetAll` and method call it answers, so a test counts
  a client's calls). Both skip without their tool unless
  `STRAND_REQUIRE_DBUS` is set (CI), where they fail. Every service run
  logs `service `x` started (run N)` and `service `x` stopped` at info
  (`STRAND_LOG=info`): the client's `starts()`/`stops()` counters as a
  whole process shows them (`crates/strand/tests/reloads.rs`).

- **Compositor (`strand_services::wm`, the `workspaces`, `windows` and
  `wm` services).** Typed records (`Workspace`, `Window`: the schema's
  fields plus `Workspace::active` and `Window::urgent`, and
  `Window::toplevel: Option<String>`, the window's
  `ext-foreign-toplevel-list-v1` identifier, which is not a schema field:
  M4's thumbnails will capture by it through a new `ProtoCmd` on the
  `strand-toplevel` thread, the connection that owns the handle) in a `WmState`,
  and `wm::run(WmConfig { backend, wayland, events, desktop }, sink,
  requests) -> impl Future + Send`: the service on the shared runtime,
  stopped by dropping it. `sink: FnMut(Vec<WmChange>)` gets one
  non-empty batch per change: `Workspaces`/`Windows`
  (`Vec<strand_core::keyed::VecDiff<i64 | String, _>>`, a `Reset` on the
  first publish), `FocusedWorkspace`, `FocusedWindow`, `FocusedScreen`
  (for `screens.focused`), `Name` (the adapter's compositor, else
  `desktop`: the first entry of `XDG_CURRENT_DESKTOP`), `ConfigReloaded {
  failed }` and `Sources` (adapter, connected, which protocols exist, why the
  adapter is degraded; always in the first batch). The state goes out
  once the adapter's first state arrives, or once the protocols have
  spoken when there is no adapter or it has failed to connect or is
  degraded.
  `wm.config_reloaded` has one owner: the `wm` store emits it from the
  batch's `WmChange::ConfigReloaded`. The copy sent to `events` as
  `ChangeEvent::Compositor(ConfigReloaded)` is only the live-reload
  change source (design.md's change-source table); the binary must not
  turn it into a second `wm.config_reloaded`.
  The three stores (and the owner of `screens.focused`) share one `run`
  through `wm::WmHub::new(config, runtime_handle)` (or
  `WmHub::fresh(make_config, runtime_handle)`, which makes the config at
  each start): `subscribe() ->
  WmSubscription` (`recv`, `try_recv`, `queued`, `request(WmAction)`, `reports()`)
  starts it on the first subscriber, gives a later one the current state
  as one batch (never a past reload), and stops it when the last
  subscription drops (or the last `WmHub` does; `recv` then ends with
  `None`). Runs are numbered, so a stopped run's in-flight batch never
  reaches the next run's subscribers. A subscriber's queue is bounded
  (`wm::MAX_QUEUED` = 64 batches): a store that stops draining gets one
  batch that rebuilds the current state, plus the reloads it missed,
  instead of an unbounded backlog; a store should still drain promptly.
  Each store subscribes from its body, so a store's 5 s stop grace is its
  own and the hub stops at once once all have stopped (a store still read
  keeps it: `tests/wm_services.rs`), so a binding that toggles does not
  tear down and rebuild the adapter connection and the protocol thread
  each time: `tests/wm_services.rs::
  the_compositor_stores_follow_sway_through_one_hub` checks that a read,
  unread, read cycle within 5 s keeps the store's `starts()`,
  `wm::live_runs()` and the one `strand-toplevel` thread. The hub starts
  the protocol thread itself (`ProtocolClient::spawn`; `wm::run` does
  the same for direct users) and owns it: a stop (the last subscriber
  gone, on the shared runtime under the hub's lock) only sends `Stop`
  (`ProtocolClient::request_stop`) and keeps the client, joined without
  waiting at the next start or stop; dropping the hub (the runtime
  thread's end, which `Services::shutdown` joins) joins it
  (`ProtocolClient::stop`: a done channel, 2 s at most). The stores (`wm::Windows`,
  `wm::Workspaces`, `wm::Wm`; records `wm::WindowItem` and
  `wm::WorkspaceItem`, the schema's `Window` and `Workspace` field for
  field, converted from the model's, which also carries `toplevel`) are
  `#[service]` bodies on the shared runtime: each subscribes to
  `wm::hub()`, the hub of its runtime thread (made on first use, one per
  `Services` runtime; each start takes the config
  `wm::configure(Some(config))` set, else `WmConfig::from_env(None)`
  afresh, so a compositor socket that appeared since is found: what
  `strand run` uses), sends each
  batch as one envelope of its patches (keyed diffs stay keyed diffs),
  is ready once its part of the state has arrived, emits
  `wm.config_reloaded` (the `wm` store only), and runs `ws.focus()`,
  `win.focus()`, `win.close()`, `win.minimize()`, `win.maximize()` and
  `win.fullscreen()` as `WmAction`s (a failure is logged; the change
  arrives in the stream). `maximize()` and `fullscreen()` toggle: per
  adapter, Hyprland's `hl.dsp.window.fullscreen({ mode, window })` (a
  `[[BATCH]]` of `focuswindow` and `fullscreen 1|0` in the classic
  dialect; Lua first, the classic form when Hyprland answers the Lua one
  `Invalid dispatcher`, the dialect understood kept per connection),
  niri's `MaximizeWindowToEdges`/`FullscreenWindow` by id,
  sway's `[con_id=N] fullscreen toggle` (maximize `Unsupported`).
  `workspaces.on(screen)` is a `fn` method over the cells (the
  workspaces whose `screen` is the `Screen` record's `name`).
  `wm::live_runs()` counts live `run`s (tests). `screens.focused` is the
  `screens` service's, which no crate serves yet; when one does it takes
  `FocusedScreen` from the same hub, with a store's lifecycle (it drops
  its subscription only 5 s after its last reader). The schema texts
  are `strand_services_schema::{WINDOWS, WORKSPACES, WM}` (one service
  per text; `wm::{WINDOWS_SCHEMA, WORKSPACES_SCHEMA, WM_SCHEMA}`),
  replacing the provisional stubs and adding `Workspace.active`,
  `Window.urgent`, `Window.maximized` and `event
  config_reloaded(failed: bool?)`. `requests` takes
  `WmRequest { action: WmAction::{FocusWorkspace, FocusWindow,
  CloseWindow, MinimizeWindow, MaximizeWindow, FullscreenWindow},
  reply: Option<oneshot> }`
  (`WmRequest::new(action) -> (WmRequest, WmReply)`;
  `WmSubscription::request(action) -> WmReply`), answered
  `Ok` or a `WmError` (`NotConnected`, `Unsupported`, `Unknown…`,
  `Rejected`, `Io`): `WmReply` is a future of that outcome that reads a
  request dropped unanswered (the run stopped, or the protocol thread
  ended, first) as `NotConnected`, so a caller never sees a closed
  channel. `wm::detect()` picks the `Backend` (Hyprland, niri
  behind the default-on `niri` feature, sway through swayipc-types
  (swayipc-async 3.0's types) over its own lossy i3-ipc framing) from
  the environment; `ProtocolClient::spawn(WaylandTarget, tx)` runs
  `ext-foreign-toplevel-list-v1`, `zwlr_foreign_toplevel_management_v1`
  (with every `wl_seat`; `activate` names the first still offered) and
  `ext-workspace-v1` on its own `strand-toplevel` thread (own
  connection, `poll(2)` on the socket and an eventfd), sending a
  `ProtocolState` per atomic update
  (`toplevels`, `managed: Vec<ManagedToplevel>` with each wlr toplevel's
  `key`, title, app id, `activated`/`minimized`/`maximized`/`fullscreen`
  and output names, `workspaces`, and which of the three globals are
  bound); `wm::merge` joins the two (`docs/decisions.md`, wave4-wm,
  laptop-toplevel and laptop-open). An adapter that is connected but cannot understand its
  compositor (`wm::understood`: a reply to a state request that is not
  the JSON it reads or lacks a field it needs, at once; eight
  event-stream messages in a row that are no event, each re-read; an
  action every supported version has (focus a workspace, focus or close
  a window; `understood::tells_syntax`) whose syntax the compositor
  refuses in every dialect, which holds until the compositor reports
  another version; the same refusal of a later action only rejects it)
  sends
  `AdapterMsg::Degraded(text)` instead of `Connected(false)`: the run
  drops the adapter's state and serves the protocols exactly as when it
  cannot connect (their ids with a `Reset`, their actions), sets
  `Sources::degraded: Option<String>` (the compositor, its version from
  `j/version`, niri's `"Version"` or sway's `get_version`, and what
  was not understood; `connected` false), and the adapter retries on its
  backoff; its next state clears it. Retries do not flip the ids back
  and forth (`understood::Degradation`): after a stream that was not
  understood, a session sends `Connected(true)` and its state only once
  its stream carried an event, and a session that never came up sends
  no second `Degraded`. The oldest subscription
  (`WmSubscription::reports()`) raises each new `degraded` text as a
  `ServiceDiagnostic` with `Cx::warn` (logged and sent to `strand
  watch`; not an overlay notice, as the service retries it itself).
  An event the adapter does not know is ignored; a known event whose
  data it cannot read is followed by a re-read (decisions.md,
  laptop-resilience). An adapter whose IPC reports no
  window state (niri) takes `maximized` and `fullscreen` from the wlr
  protocol, joined by app id (and title for twins). The source order is the adapter, then the wlr
  protocol (window ids `wlr-<key>`, `ManagedToplevel::window_id`; focus,
  state and `win.focus()`/`close()`/`minimize()`/`maximize()`/`fullscreen()`
  as `activate`, `close`, `set_minimized`, `set_`/`unset_maximized` and
  `set_`/`unset_fullscreen` by the state last sent), then `ext-foreign-toplevel-list-v1` read-only (window
  actions `Unsupported`). `Sources::toplevel_management` says whether the
  wlr protocol is bound. `wm::Mirror` applies the stream (tests).

- **Audio (`strand_services::audio`, the `audio` service; cargo feature
  `pipewire`, on by default).**
  `Audio::spawn(AudioConfig { remote }, sink) -> io::Result<Audio>` runs
  pipewire 0.10.1 (`v1_0_0`, built against libpipewire 1.0.5) on its own
  `strand-pipewire` thread (the library handle; the `audio` store runs
  the same loop on its own service thread, below). Dropping the handle asks the thread to stop
  and returns at once (safe on the shared runtime at the 5 s stop);
  `stop()` also joins it, which blocks briefly and belongs off the logic
  thread. `sink: FnMut(Vec<AudioChange>) + Send` gets one non-empty
  batch per burst of PipeWire events, on that thread, and must never
  block (an unbounded channel or a `try_send`): `Connected(bool)`
  (always first in the first batch, which comes once the first connection
  has settled or the first attempt failed), `Sinks`/`Sources`
  (`Vec<VecDiff<u32, AudioDevice>>` keyed by the PipeWire id, a `Reset`
  first; an id PipeWire reused for another device, a new `object.serial`,
  is a `Remove` and an `Insert`), `Sink`/`Source` (`Option<AudioDevice>`: the defaults; `None`
  is shown as the record's schema defaults), `Serials` (every listed
  device's `object.serial` by id, the whole map, in the first batch and
  whenever it changes; `Mirror::device_ref(id)` /
  `AudioState::device_ref(id)` turn it into a `DeviceRef`) and `Levels { target,
  device, peaks }` (at most one per meter per `audio::FRAME`, 1/60 s,
  the cycles read on PipeWire's data thread so the loop wakes about once
  a frame at most; a meter that stops or is retargeted after showing sound sends one with
  no peaks). `AudioDevice` is exactly the schema's record (`id`, `name`,
  `description`, `volume` on the cubic scale wpctl shows, `muted`,
  `icon`, `default`), so the store's `#[derive(Data)]` record can be it.
  `audio::SCHEMA` (`strand_services_schema::AUDIO`) is the text the
  store serves, which is exactly the provisional stub. `audio::Mirror`
  applies the stream.
  `request(AudioAction::{SetVolume(DeviceRef, f64), StepVolume(DeviceRef,
  f64), SetMuted(DeviceRef, bool), MakeDefault(DeviceRef)}) ->
  AudioReply` (a future to `.await` inside a runtime, or `wait()` on a
  plain thread outside any runtime) answers `Ok`
  once PipeWire has been asked (the change comes through the stream) or
  an `AudioError` (`NotConnected`, `UnknownDevice`, `InvalidVolume`,
  `NoDefaultMetadata`, `Failed`). A request dropped unanswered reads as
  `NotConnected`. A connection settles (its first state goes out) once
  its syncs are back, including those after binding each card's routes,
  the session manager's `default` metadata is read, and each default
  shown before a loss names a device again, or after `audio::SETTLE`
  (3 s) once at least its first sync is back; until then the last
  state stays (`connected: false` after a loss), and the defaults shown
  before a restart of the daemon or of the session manager alone stay
  until new ones resolve or `SETTLE` passes, so a restart never flashes
  an empty `audio.sink` or list. A daemon that accepts the connection but
  never answers its first sync (socket activation with a failing
  `pipewire.service`) publishes nothing and is dropped after
  `audio::UNANSWERED` (6 s), then retried like a lost connection.
  The `default` metadata is read again (bound afresh, so the session
  manager replays its keys) `audio::REREAD` (250 ms) after a client
  comes or goes or an effective default is cleared: PipeWire forwards
  no metadata update to existing bindings while another client's bind
  is in its handshake. The keys the new binding replays replace the
  defaults whole once its sync is back (a key a lost update cleared is
  not replayed, so it goes too), and such a read sends only what
  changed. The old binding is dropped only once the new one and its
  sync are made, so a failed read keeps following the keys. A read
  again waits until the first read of a connection is back (nearly
  every connection reads again once: the service's own client, when
  its id is above the metadata's, comes after the bind), so the first
  state never goes out before the replay.
  Actions sent before a connection has settled (a write that lazily
  starts the service, one sent during a restart) wait, in order, and
  run right after its first state; with no connection at all they
  answer `NotConnected` after `audio::GRACE` (2 s). `DeviceRef::
  DefaultSink` resolves on the audio thread when the write runs.
  `DeviceRef::Id { id, serial: Option<u64> }` (`DeviceRef::id(id)`,
  `DeviceRef::device(id, serial)`) names a device by its PipeWire id and,
  with a serial, only while that id holds the device with that
  `object.serial`: PipeWire hands a freed id to the next object it
  creates, so a reference kept past its device leaving answers
  `UnknownDevice` instead of reaching the new device (a node without a
  readable serial matches any).
  Every language-side write arrives as `SetVolume`: VM writes, and
  IPC's relative form (`strand set audio.sink.volume +5%`, design.md
  example (d)), which wave4/core's `services::set_text` resolves
  generically by reading the current value through `ServiceHost` and
  writing an absolute one with `ServiceHost::write` (its optimistic
  tagged cell makes quick steps compound). `StepVolume` adds its delta on
  the audio thread, to the last volume written while its echo is
  pending; it is kept only for callers holding the `Audio` handle
  directly (none on the language path). Volume and mute go to the
  card's active `Route` (`save: true`) when the node has one, else to
  the node's `Props`.
  `set_levels(targets)` replaces the set of peak meters
  (`LevelTarget::{DefaultSink, DefaultSource, Device(id)}`); the store
  passes what visible readers want, and an empty set stops them all.
  No schema field carries levels yet: their consumer is the `spectrum`
  element (M4, `spectrum(AudioDevice -> source)`), which will subscribe
  by its source device and needs the meter to hand out samples for
  realfft, not only folded peaks (docs/decisions.md, wave4-wm (audio)).
  The thread reports a volume it wrote exactly as written: any of a
  device's last `audio::ECHOES` (64, core's `MAX_PENDING_ECHOES`)
  writes, and any volume on the 1/10 000 grid even once forgotten
  (`audio::perceptual` snaps a root within 1e-6 of it); through a card's
  `Route` (hardware mixer steps) an echo within 0.005 of a write reads
  as the closest write.
  The store, `audio::AudioStore` (`#[service(name = "audio", thread)]`,
  records `AudioDevice` with `#[derive(Data)]`, action
  `AudioDeviceAction::MakeDefault`): its body runs the loop
  (`thread::run(config, host, rx)`) on the service's own thread through
  a `Host` (`changes(batch)`, `poll() -> Vec<Cmd>` after each burst of
  work, `deadline()`); `Cx::set_notify` pokes the loop (`Cmd::Poke`),
  which then drains the service's messages there, and a stopped service
  ends the loop. A write of `audio.sink`/`audio.source` (`.volume`,
  `.muted`) becomes `SetVolume`/`SetMuted` on `DeviceRef::DefaultSink`
  (`DefaultSource`); an item write of `audio.sinks`/`audio.sources` on
  `DeviceRef::Id` with the serial the store shows under that id, unless
  the record the write was made on (`Write::held`) names another
  `node.name` than the device under its id now (an item kept past its
  device leaving, its id reused): that write changes nothing and is
  answered with the device as it is, and `make_default()` on such an
  item does nothing. Each write is answered tagged (`Cx::report`) by the
  first batch that changes its device and shows its value (the earlier
  writes of that device's field are overtaken, so the logic thread
  ignores their echoes and settles on the last), or, refused, not a
  writable leaf, or unseen within `audio::ANSWER_WAIT` (1 s) of PipeWire
  taking the action (6 s with no reply), with the device as it is.
  Answers go in write order per cell (`audio.sink`, or one item of a
  list): the logic thread takes an answer tagged `g` as the answer of
  every write of the cell up to `g`, so a write that ended waits for the
  earlier writes of its cell and the run is answered by its last, with
  the state that shows them. A slider's echoes never snap it back, and
  the value path of `strand_core::echo` is not relied on (a report's
  record also carries the `icon`, which the optimistic local value does
  not update). A loop that cannot be created ends the body with an
  error (the contract's diagnostic and backoff). The loop's keyed diffs
  are keyed by the device's `u32` id.
  `audio::configure(Some(AudioConfig))` points stores started later at
  a socket (tests); `strand run` uses PipeWire's own default. Levels:
  `audio::tap_levels(target, f) -> LevelTap` (the M4 `spectrum`
  element's hook) asks the running store to meter `target`; the store
  passes the tapped targets to the loop only while a reader is visible
  (`Cx::visible`), an empty set otherwise. Provisional (decisions.md,
  wave4-wm fixes r1): the taps are process-wide and get per-channel
  peaks; M4's `spectrum` extends `Levels` with samples per tap, and the
  process-wide `audio::configure`/`wm::configure` targets move into
  `Buses` (or a sibling) when one process needs two. Without the
  `pipewire` feature the module is absent and the store answers
  `audio.*` at the schema's defaults (its schema text is left out of
  `strand_services::schemas()`).

- **M4 additions** (planned; docs/m4-plan.md). One owner per file: tray
  to S-surface, auth to S-lock, audio and wm to S-effects.
  - `auth` stops being provisional (`services/auth.rs`, as built in
    M4 wave 1): its schema is `strand-services-schema/src/auth.schema`
    (`busy`, `failed`, `submit(password)`), with an `AUTH` constant and
    a `schemas()` entry, and it is registered in `Builtin`. Like every
    other builtin service it keeps its `provisional service auth` stub
    in builtin.schema, which the extension replaces, so the compiler's
    own tests and the grammar's lock example still check without the
    service crates (decisions.md, m4-lock-w1). Its store runs a
    `strand_auth::Client` built with `child::restore_in_child` as the
    helper's `pre_exec`, started with the store and killed when it
    stops; a submit runs on a blocking task, `busy` meanwhile, and one
    arriving during a check is dropped. The password is a
    `strand_auth::Password` from the moment it arrives, wiped once
    sent. A success hands the `strand_auth::UnlockToken` to the
    `auth::UnlockSink` the binary sets with `auth::configure`
    (`AuthConfig { helper, timeout, sink }`), which passes it to the
    surface manager's unlock; anything else sets `failed`, and a check
    that could not be made is a warning diagnostic, as is the `login`
    fallback (once per process).
  - Audio: `Levels` carries FFT bins for a `spectrum` tap. The FFT
    (realfft) runs on the audio thread only while a reader is visible,
    and stops while the source is silent.
  - wm: `ProtoCmd::Capture` on the `strand-toplevel` thread captures a
    window by its `Window::toplevel` identifier through
    ext-image-copy-capture, for `thumbnail`; frames reach render through
    the binary (`Renderer::feed_frame`), only while the thumbnail is
    visible.
  - Tray: `Activate`, `SecondaryActivate` and `ContextMenu` get the
    anchor's output-logical position for `x`/`y` instead of 0, 0. How it
    reaches the action (an optional argument, or filled in by the host
    from the node that called it) is S-surface's decision, recorded in
    decisions.md; menus open as nested `popup`s.

### `strand-watch`

Produces typed events, never parsed content; logic turns them into writes.
It does not depend on `strand-compiler` or `strand-core`.

- **One channel.** `strand_watch::channel() -> (EventSink, Receiver<
  ChangeEvent>)`; `EventSink` is `Clone + Send` and
  `.with_waker(Fn())` calls a waker after each send (a calloop `Ping` on
  the logic loop). `ChangeEvent` is `Files(FileBatch)`,
  `System(SystemBatch)` or `Compositor(CompositorEvent)`.
- **Files.** `Watcher::spawn(Option<ConfigWatch>, Options, EventSink)`
  runs the `strand-watch` thread (one thread: a raw inotify fd and a
  control eventfd under `poll(2)`; with no inotify instance, everything
  is polled). Dropping the `Watcher` stops it; `Watcher::join(self) ->
  thread::Result<()>` stops it and says whether the thread panicked (the
  binary's `live::Worker::join` joins the compiler worker, then the
  watcher, and `strand run` logs a panic of either; the persist store's
  own-write observer holds the watcher weakly, so the worker's handle is
  the last strong one).
  `ConfigWatch { root, modules: ModuleSet { files, dirs, errors,
  too_deep }, rescan }` is `source::find_files`'s `Discovery` (`files`,
  `dirs`, `errors` with each error as text, `too_deep`) plus a
  `FnMut() -> io::Result<ModuleSet>` the binary implements with
  `find_files`; the watcher calls it when a `.strand` name, a directory
  or a directory link appears or vanishes in a config directory, a
  directory link on the way is swapped, the config directory itself is
  replaced, or on a rescan. When `spawn` returns, every watch is in place
  and every module file's baseline hash was read after its watch:
  start the watcher, then load. The `modules` passed in were listed
  before the watches existed, so the watcher lists the set once more at
  the first quiet period and reports a module created in between as
  `Created` (nothing when the set is unchanged). Referenced paths come
  from the compiler: after each reload the loader calls
  `set_referenced(impl IntoIterator<Item = impl Into<Referenced>>)`,
  each item `(path, role)` or `(path, role, hash)` with `hash` the
  `hash_bytes` of what the loader read, for every `(path,
  Role::{Shader, Settings, Wallpaper, Other})` the program references;
  it replaces the loader's previous set (and only that), and a file that
  no longer holds the bytes its `hash` names (saved between the read
  and the call) is reported. `watch_file(path, role)` /
  `unwatch_file(path, role)` add or drop one ad-hoc registration
  (counted per path and role) for owners whose paths the compiler does
  not collect (the wallpaper owner: `prefs.wallpaper` is a runtime
  value); `set_referenced` never replaces them. `watch_file` is
  register, then read; `watch_file_loaded(path, role, hash)` is read,
  then register. Module-set membership is separate, so a
  module file registered for another role stays a module. Neither a
  referenced file nor its directory need exist yet. Cache sources come
  through `watch_tree(dir, depth, CacheKind::{Apps, Icons, Fonts})` (and
  `watch_file(path, Role::Cache(kind))` for one file: GTK's settings).
  `strand run`'s compiler worker takes them as `live::Job::Caches {
  sources: live::cache_sources(), changed }` and calls `changed(kind)`
  once per kind a batch touched (all three after an overflow); `strand
  run` then tells the `apps` service (`apps::changed`), the icon lookup
  (`strand_icons::invalidate`) and, on the main thread, the renderer
  (`icons_changed`, `fonts_changed`); `Apps` and `Icons` are handled
  alike (`run::cache_changed`: all three of the apps service, the icon
  lookup and the renderer's icons), since an installed app may bring an
  icon the icon watch does not see.
  **Blocking:** `watch_file`, `set_referenced` and `watch_tree` wait for
  the watcher thread (watches synced; a new path given without a hash
  is read before they return, one given with a hash is compared later
  on the watcher thread); call them from the loader, the compile worker,
  boot or a service thread, never from logic. `watch_file_loaded`,
  `unwatch_file`, `register_own_write` and `rescan` return at once and
  are safe on the logic thread.
  `register_own_write(path, hash_bytes(&bytes))` before Strand writes a
  file (settings write-back) makes the matching write silent; the
  registration is in place when it returns, and it silences the write
  under every registered path that resolves to that file. Own writes
  should be atomic (temporary file renamed over the path). A file with a
  write in progress (`MODIFY` seen in a config directory, or a new file
  created and not yet closed, and no `CLOSE_WRITE` yet) is never read;
  after 5 s (`Options::stalled_write`) with no further event and no
  change to its modification time it is read anyway with
  `Notice::StalledWrite(path)`, and read again when it is closed. The
  ancestors of each watched directory get a light watch (names moved or
  deleted) up to `Options::home` (default `$HOME`, canonical), strictly
  below it, or outside it up to the root of the mount holding the
  directory (design.md, "Watch directories, not files"; decisions.md
  laptop-decisions). A file
  a write may have reached while it was being read (its stamp moved, a
  `MODIFY`, creation or removal for it queued by then, outside the config
  directories also a `CLOSE_WRITE`, or modified less than 15 ms before
  the read ended) is left out of the batch with its baseline unchanged
  and read at a later quiet period. A file put off like this is still
  read within 500 ms of the event that made it due, however often it is
  rewritten: from then on a recent modification time alone does not put
  it off. In config directories, where every write makes a `MODIFY`, no
  batch carries a torn read. Outside them writes make no event before
  the close, so an in-place writer that pauses for more than 15 ms
  mid-write can be read torn; its `CLOSE_WRITE` then reports the whole
  file in the next batch. Only config directories are watched with
  `MODIFY`; every other content directory (referenced files, symlink
  hops, cache trees) hears completed writes and names only, so writers
  there cost one wakeup per file closed. The config root's parent and
  the stand-in for a missing directory are watched for names only, and
  each ancestor in the range above holds a light watch (its own move or
  deletion and its children's only), so moving any directory in that
  range reports the files below as `Removed`; moving `$HOME` itself, or
  a directory above it, is not seen.
  `rescan()` is `strand reload`.
- **`FileBatch { changes, rescan, notices, first_event, last_event }`.**
  One batch per quiet period: 15 ms after the last completed write
  (`CLOSE_WRITE`, `MOVED_TO`, a new symlink or hard link; 50 ms when the
  latest event removed a watched file; at most 500 ms after the first
  event). `changes` is sorted by path then role, each `FileChange {
  path, canonical, kind: Created|Modified|Removed, hash:
  Option<blake3::Hash>, role, error }`, one per role the path is watched
  for; unchanged hashes and own writes are dropped before sending, so
  every change is real. A `Modified` whose hash is unchanged means a
  link now resolves elsewhere: skip the recompile, update the path.
  `path` is the path as registered (module files as `find_files`
  returned them, under the config root even when a directory link points
  elsewhere); `canonical` is the resolved target. Only regular files are
  hashed; anything else (a FIFO, a device) has `error:
  Some(InvalidInput)`. Cache-tree entries are not hashed. `rescan` is
  `Some(Overflow | Requested)` for a full rescan; `notices` reports
  polled directories, rescan-callback failures, backend errors, stalled
  writes, and `Notice::ModuleSet { errors, too_deep }` when a rescan's
  diagnostics differ from the previous scan's (complete lists, so the
  overlay replaces what it shows). `first_event` and
  `last_event` (`Instant`) are the earliest and latest events behind the
  batch (for a file put off from an earlier batch, the events that made
  it due, not the flush that put it off); latency measurements use
  `sent − last_event` as the watcher's share of save-to-pixels.
- **System settings.** `strand_watch::follow(&zbus::Connection,
  EventSink)` is the async portal client; `strand-services` runs it on
  the shared tokio current-thread runtime and session connection.
  `PortalSettings::spawn(Bus::Session, sink)` runs the same on its own
  `strand-portal` thread and connection. It sends one `SystemBatch {
  settings, at_boot: true, received }` within `BOOT_READ_TIMEOUT` (500
  ms; empty when there is no portal; `on change` must not fire for it),
  then one batch per `SettingChanged`, plus `at_boot: false` batches for
  boot reads that came late and for a full re-read whenever the portal
  starts or restarts: `SystemSetting::Dark { dark, scheme }`,
  `Accent(Option<[f64; 3]>)`, `Contrast(Normal | High)`, each with
  `.path()` = `system.dark` / `system.accent` / `system.contrast`.
- **Compositor.** `CompositorEvent::ConfigReloaded { failed:
  Option<bool> }` is the compositor-reload change source (`None` from
  Hyprland and sway, which do not say). The language event
  `wm.config_reloaded` comes from the `wm` store's own stream
  (`WmChange::ConfigReloaded`, see `strand-services`), not from this
  copy, so it fires once per reload. The M3 Hyprland and niri adapters live in
  `strand-services` (design.md's services table lists them) and send it
  through a clone of the same `EventSink`, so `strand-services` depends
  on `strand-watch` for `EventSink` and `CompositorEvent`; `strand-watch`
  depends on neither `strand-services` nor `strand-core`.

Settings files (fixed in wave 2 by `strand-core`; the Threads table holds:
the watcher reads and hashes, it never parses). On a change event the
watcher hashes the file (BLAKE3). An unchanged hash stops there, and that
includes Strand's own writes: the binary registers
`persist_store.on_written(..)` and passes the hash of each `OwnWrite`'s
`content` to the watcher as pre-registered for its `target` (and its
`path`, when that is a link), before the rename makes the content
visible, so the change event finds the hash already known. A changed hash
goes to the logic thread as (path, hash), and the logic thread calls
`settings.reload(rt)` on every `Settings` handle declared on that path.
`reload` takes the write mark before it reads, so a write of Strand's
that lands meanwhile is never undone. `Settings::sources()`,
`SettingsSources::read_from` and `Settings::reload_with` stay available
for a thread that is allowed to parse, such as the compiler worker, in
case reading on the logic thread ever shows up in a profile. The watcher
does not call them. Strand's temp files next
to a settings file are named `.<name>.tmp.<pid>.<n>` (renamed over the file:
the watcher sees `MOVED_TO` for the file itself); its scratch-name filter
should ignore that pattern, as it does editors' scratch names.
