# Architecture

How the design in `design.md` maps onto crates, threads and interfaces. This
file fixes boundaries; each crate is free inside its own boundary.

## Threads

| Thread | Crates | Owns | Never does |
| --- | --- | --- | --- |
| Main: render + surface | `strand-render`, `strand-surface` | Wayland connection (calloop), springs, token evaluation per frame, layout, damage, paint, presentation | Wait on the logic thread, run handlers, evaluate bytecode |
| Logic (`strand-logic`) | `strand-core`, `strand-compiler` (VM, reconciler) | Reactive graph, state, handlers, timers, the live program; the `strand` binary's IPC Unix socket (`strand reload`, `strand watch`, `set`, `reset`, and M5's commands as 0a lists them: `get`, `toggle`, `call`, `clear`, `inspect.node`, `inspect.dump`, `reset{node}`, `export` and `report`, and since M5 wave 3 `inspect.pick`, `inspect.highlight` and `inspect.flash`, on the socket only) is a source on this loop, as is the request channel from the D-Bus front end; it also renders the `file` outputs after each tick's diff is sent ("The theming hub") | Touch Wayland or pixels |
| Compiler worker (`strand-compile`) | `strand-compiler` | Parse, check, lower changed modules off-thread | Mutate live state (it hands a compiled `Program` to logic) |
| Text worker (`strand-text`) | `strand-text` | parley shaping, swash rasterisation, per-scale glyph atlases | Block render: a painted surface keeps drawing its last layout (or a realigned stand-in from another scale or width) until the new one arrives |
| Image worker (`strand-image`, one per renderer; one that cannot start stops the shell) | `strand-render` (`image.rs`, `media/`) | Image and icon decodes (an animated GIF, APNG or WebP's player kept between its frames), the `svg` and `lottie` file jobs (`ImageStore::load_file`: read through a bounded read and parsed here, a Lottie's assets with it), and `set_idle_hook`'s allocator trim; each result pings the render loop | Block render: a frame draws nothing, or a stand-in of the same image at another size, until the decode arrives; read a file without a size bound |
| Render timer (`strand-tooltip`) | `strand-render` (`renderer/wake.rs`) | The render loop's own due times: one wake at the latest time it was sent (a tooltip's delay, the exit of a surface whose output stopped sending frames), through the loop's waker | Any work but the wake: `Renderer::update` runs on the main thread |
| Watcher | `strand-watch` | inotify directory watches and polling (one `strand-watch` thread); not the IPC socket (`docs/decisions.md`, wave2-watch) | Parse files (it sends paths and hashes) |
| Persist IO (`strand-persist`, one per `PersistStore`) | `strand-core` | Atomic writes of persisted cells, settings-file edits, settings overlays and last-good snapshots; reports each file it is about to change to `PersistStore::on_written` | Run on the logic tick or block logic (failures come back as diagnostics in a later tick) |
| Services | `strand-services` | One tokio current-thread runtime thread (`strand-services`), started with the first service that runs on it: every async service body (the `system` service follows the portal Settings with `strand_watch::follow` here; the `workspaces`, `windows` and `wm` stores and their one compositor hub with its IPC adapter run here, the sway adapter on swayipc-types (swayipc-async 3.0's types) over its own tokio framing, so no async-io reactor thread) and the portal icon-theme follower (`strand_services::icon_theme`, a task on the same session connection); the `audio` store runs the PipeWire loop on its own service thread (`strand-audio`, `Start::Thread`), and the `video` store the same loop following only video on its own (`strand-video`, services-round), and the hub's Wayland toplevel/workspace protocol client its own `strand-toplevel` thread (the hub's, not a `Start::Thread` body: three stores share it; the hub tells it to stop on its last stop without waiting on the shared runtime, and joins it at the next start or stop and on `Services::shutdown`); the `clipboard` store runs on its own `strand-clipboard` thread (`Start::Thread`, asleep in `poll(2)` like `strand-outputs`), and each value it copies is held by the process-wide `strand-clipboard-keep` keeper thread, started on the first copy and ending once another program takes the selection (m6-service-gaps); (services-round) the `idle` store runs ext-idle-notify on its own `strand-idle` thread (`Start::Thread`, asleep in `poll(2)` like `strand-outputs`) (idle: zero wakeups, `crates/strand-services/tests/idle.rs`, `tests/audio_idle.rs`, `tests/wm_services.rs`, `tests/audio_service.rs`) | Block logic: they send patches and events over channels, applied by `Services::pump` on the logic thread |
| Lock auth (M4; `strand-lock-auth`, started at the fallback lock's first submit, ends with the lock session) | `strand-auth` (`Client`), the binary's `run/lock.rs` | The fallback lock's `strand_auth::Client` and its helper process; each check is one PAM conversation that blocks here, bounded by the client's deadlines (decisions.md m6-lock), and its verdict goes back over the main loop's channel | Run while no fallback is shown, or make the main thread wait on PAM |
| GPU (M4; `strand-gpu`, started with the shell when a device can exist (gpu-first), ends with the device) | `strand-gpu` | The wgpu instance, adapter and device, vello_gpu's renderer, shader pipelines, promoted surfaces' swapchains, offscreen passes and readbacks (see "`strand-gpu`") | Run while nothing needs it, touch the scene tree, or make the main thread wait while the shell runs: every reply is a message and a ping (the one wait is at exit: `run` joins the thread before the surface manager drops, for at most `GPU_SHUTDOWN`, 3 s; see "`strand-gpu`") |
| Theme quantiser (`strand-quantise`, one per `Quantiser`, started at the first wallpaper palette asked for) | `strand-theme` (`image.rs`), driven from `strand-compiler`'s `vm/theme.rs` | Wallpaper decodes and palette quantisation off the logic thread, with the quantise cache; a short-lived `strand-quantise-grace` sleeper per torn wallpaper wakes the owner when its grace ends | Run on the logic tick: logic polls for results and keeps the last palette meanwhile |
| Token overlay writer (`strand-tokens`, M5; started on the first write, ends when idle) | `strand-compiler` (`instantiate/overlay.rs`) | Atomic writes of `$XDG_STATE_HOME/strand/tokens.toml` (temp file, `fsync`, rename, directory `fsync`), the newest queued text winning; a bounded wait for the last write at exit | Run on the logic tick or block logic (a failure comes back as settings' `WriteFailed` notice); it is not the persist IO thread |
| File output writer (`strand-files`, S-tools; started with the first mounted `file` output, ends with the shell) | the binary's output host (`run/outputs.rs`) | Writes the bytes logic rendered for each `file` output (atomic temp file and rename, following symlinks, keeping the mode; latest bytes per path win), keeps the manifest under `$XDG_STATE_HOME/strand/files`, tries a refused or failed write again (1 s, doubling to 10 s), runs `run:` with its 200 ms trailing debounce, one run per file plus one queued and the 10 s timeout that kills its process group, each run waited for on a short-lived `strand-files-run` thread; each outcome goes back to logic through a shared outcome list logic drains when the writer pings it (one job queue with a condvar, no channel), so a failure is a notice and a `strand watch` event through `Shell::broadcast` | Render a template, touch the scene or Wayland, or block logic; it has no channel to the main thread |
| State writer (`strand-state-write`, one per `FileWriter`: the logic thread's for the portal's last values, the theme VM's for the last palette, each started on first use and idle until a write) | `strand-theme` (`writer.rs`) | Atomic writes of the last palette and the portal's last settings, batched per path | Block logic (a write is queued and returns at once; a failure is a WARN) |
| Introspection asks (`strand-introspect`, one short-lived thread per uncached ask; in `strand lsp` only) | `strand-introspect` (`Cache::properties_or_ask`), used by `strand-dev`'s workspace (`strand check` and the loader ask on their own thread and wait) | One D-Bus introspection call, its answer remembered for `TTL`, then `done` | Block the LSP's analysis: it gets the remembered answer, or none, at once |
| Short-lived waiters | the binary's `overlay.rs` (`strand-editor`), `strand-services`'s `custom.rs` (`strand-kill`) and (services-round) `wm/screen.rs` (`strand-screenshot`) | `strand-editor`: waits on the `$EDITOR` the overlay's click opened, so it is reaped; `strand-kill`: SIGKILLs a dropped `permit exec` process group after its SIGTERM grace; `strand-screenshot`: one per `thumbnail screen` shot, ending with its frame or `SHOT_WAIT` | Anything else: each ends with its one wait |
| Report writer (`strand-report`, decisions.md report; one per `strand report` with words, ends when its bundle is written) | the binary's `report/bundle.rs` | Writes one glitch report's bundle: the two ring dumps, the JSON parts, the frames encoded as PNG, the config and asset copies (bounded reads), then prunes the reports directory to the newest 20; its outcome goes back to logic through a shared slot and the logic loop's ping (see "Glitch reports") | Touch Wayland, the scene or the rings (it gets copies), or block logic or main |

Two library handles have threads the shell does not start:
`strand_watch::PortalSettings` (`strand-portal`; the shell follows the
portal with `follow` on the services runtime instead) and
`strand_services::audio::Audio::spawn` (`strand-pipewire`; the shell's
`audio` store runs its loop on `strand-audio`). Tests and benches spawn
their own unnamed threads.

(flex) The cursor is pulled the same way as `drop_accepted`:
`SurfaceHost::cursor(surface) -> strand_scene::CursorShape` (default
the arrow), which the shell's host answers with `Renderer::cursor` (the
`cursor:` of the innermost node of the hovered chain the host loop gave
`Renderer::pointer_input`, latched to the pressed chain while held; an
`input` is `text`). The surface manager asks after each pointer frame
on a surface of ours and after a paint or `poll` while a seat's pointer
is on it, and calls `ThemedPointer::set_cursor` (`wp_cursor_shape_v1`
or the theme) only when the answer changed since the pointer entered
(`State::cursor_shape` reads it back for tests).

Channels are the only coupling between threads. Logic → render is one
`SceneDiff` per tick. Render → logic is `InputEvent`s (`strand-scene`) and layout facts
(`self.width` for container queries and (M6) `id.box`:
`Renderer::take_layout_facts`, the laid-out boxes that changed of nodes
carrying `Prop::Watch`, each `(NodeId, LogicalRect)` from the top-left
of its surface's box (a popup's own, its place in its parent surface;
render's `renderer/facts.rs`), sent as `run::ToLogic::Layout { seq,
sizes }` with `Renderer::layout_seq`; a `box` watch also reports a move,
a size watch only a resize, and a node's first box holds its frame as a
query's change does) and (flex) pointer facts (`id.pointer`:
`Renderer::take_pointer_facts`, each `(NodeId, Option<LogicalPoint>)`
for a node whose `Prop::Watch` keyword has a `pointer` part (parts join
with `_`: `box_pointer`), in its laid-out box, `None` once the pointer
is not over it; render's `renderer/pointer.rs`), sent as
`run::ToLogic::Pointer` and applied with `Instance::set_pointer`. The
host gives render the pointer after routing each input event,
`Renderer::pointer_input(surface, at, hovered chain, left held)`
(`host_loop::input` does, and hands the facts over before the event's
intents, so a press's flags reach logic after where it went down); a
surface with a watched node then wants a frame, whose paint reports
what changed (once a frame however many events), and a press is
reported at once. (flex review) A frame that moved a watched node
under a pointer resting on its surface reports it too (`report_pointer`
runs whenever the pointer is over the surface, sending only what
changed). Every `LoopHost` hands facts over through `facts()` at those
points, the eval harness too (it sets them on the instance and its next
flush echoes the batch), so replay orders them as the shell does.
While the left button is held the frame is the one
the press began in, the surface's place included, so a drag that moves
the node or its surface does not feed back. The
next diff logic sends echoes the last batch it took in as
`SceneDiff::layout_seen` (sent even with no ops): render holds a frame
whose layout changed a `watch: query` node's size until then, or for
`strand_render::QUERY_WAIT` (`Renderer::set_query_wait`; zero offline),
so container queries settle inside the frame. `SceneDiff::reduced_motion`
carries a change of the desktop's reduced-motion preference
(`system.reduced_motion`) to render (sent even with no ops).
`SceneDiff::press: Option<u32>` (M6, m6-w2-surface) is the compositor
serial of the press that caused the diff (`run::PressCause`): the last
press among the events and two-way writes logic handled since the last
step (`ToLogic::Event`'s and `ToLogic::Write`'s `input`; a hover change
or an input-less write handled after it does not take it away), else,
on the step after the services reported a change, the press of the
last action a handler ran with one (`Composite::action_press`, from
`ActionCtx::input`), up to `GRAB_WINDOW` after it: the tray's `opened`
answering `item.menu.open()`. The host marks each popup a diff creates
or opens with it (`demo::popup_causes`, `State::set_popup_cause`)
before applying it, so the surface grabs only for the press that
opened it. (window) `SceneDiff::window_asks: Vec<(NodeId, WindowAsk)>`
carries what handlers asked of a `window` (`w.maximize()`,
`w.resize(edge)`; `strand_scene::WindowAsk`), gathered by the instance
through `VmHooks::window(rt, node, ask)` for a node that is a window's
root and taken once; the binary applies the diff, then hands each to
`State::window_ask(node, ask, press)` with the diff's `press`. The
other way, `SurfaceHost::window_state(surface, &ToplevelState)` reports
each configure that changes what the compositor says of a window
(`strand_surface::ToplevelState`: the axes it sized, `activated`,
`maximized`, `fullscreen`, `tiled`, `decorated`, `configure_bounds`,
`wm_capabilities`; also `State::toplevel_state(node)` and
`SurfaceInfo::toplevel`) before its `configured`: the host passes the
sized axes to `Renderer::set_window_sized(surface, (bool, bool))`
(laid out at the configured size there, no growth spring, no size
asked; an axis the compositor lets go of takes the content's size at
once, with no spring from the size it was given), the bounds to `set_surface_bounds`, and the states to logic as
`set_flag` with `NodeFlag::Focused`, `Maximized`, `Fullscreen`,
`Tiled` and `Decorated` on the window's root, each only when it
changed (`window_flags`; the sized axes and bounds likewise, so a
configure that only resizes, each step of an interactive resize, wakes
no logic). The host keeps each window's last state: bounds a configure
gave outlast `surface_entered`, which gives the whole monitor's only to
a surface without them (`Renderer::surface_bounds(surface)` reads a
surface's back). `SurfaceHost::window_closed(surface)` tells the
compositor's close (`xdg_toplevel.close`) just before its `ClickAway`,
so the recorder marks that click away `flight::click_away::COMPOSITOR`
and a replay tells it from `w.close()`'s, which its own handler asks
again. No locks are shared across threads on a hot path.

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
new variant of the same message, and M6 its `hidden` field (the rows of
a windowed list render could not see; see "Drag and drop"). Input is routed on the main thread by
`strand_render::input::Router` (`handle(&InputEvent, &mut dyn
InputScene) -> Vec<Intent>`; the `Renderer` is the `InputScene`: hit
chains, scrolling, the tree): hover and pressed chains, keyboard focus
(the focused node of the focused surface gets `key(k)`), list selection,
`nav`, `input` edits, and `open: false` on a surface whose `open` is
two-way (`Prop::TwoWay`) on Escape, focus loss and click-away
(`InputEvent::ClickAway`, or a left press on another Strand surface while
an open `keyboard: exclusive` one has a two-way `open`:
`InputScene::exclusive_open`; or, integration-r8, while an open popup with
no `trigger: hover` has one, `InputScene::popup_open`, unless the press
is in that popup or one nested in it, or on its anchor). (window) A
`window` is closed only by `InputEvent::ClickAway`, its compositor's
close or `WindowAsk::Close` (in pick mode too); Escape and focus loss
leave it open (`is_window`), and the router never writes its root's
`Flag::Focused` (`Router::flush` drops it: a window root's `focused`
is the compositor's `activated`, which the host sends logic from
`SurfaceHost::window_state`). The host passes each logic diff to
`Router::observe` before applying it, so logic's own `input` text wins
over edits in flight, and calls `Router::settle(&mut dyn InputScene)`
after applying it (a focused `input`'s `nav` list with rows and no
selection selects its first row), sending its intents like `handle`'s.
A `KeyboardEnter` with no leave before it (the keyboard back from a
grabbing popup) keeps the surface's focused node. An `Intent` is `Flag { node, flag, on }`,
`Event { node, event }`, `Write { node, prop, value }` or (m6-service-gaps)
`Clipboard(ClipboardAsk)`: a focused `input`'s copy, cut or paste key
(`widgets::edit` returns `Edit::Copy`, `Cut` or `Paste`; a `password`
input copies and cuts nothing) asks the host to copy text or to paste,
the Router remembering which node asked (per surface; cleared on
detach, focus loss and prune); the paste comes back as
`InputEvent::Paste`, built on the edits in flight like a key
(`widgets::pasted` makes it one line) and dropped if focus moved; the binary's
host (`demo/host.rs`, `Forward`) only maps them to `ToLogic::Flag`,
`Event` and `Write`, keeping the clipboard asks for strand-surface's
`SurfaceHost::take_clipboard_asks` (both with the press they came from, `input`; a
`Write` logic applies with `Instance::write`, a
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
opt-levels build the code users wait on for speed and event-rate
glue for size (decisions.md m5-speed-profile), and neither `cargo install`
from crates.io nor a packager's own profile carries them, so packages
build from the workspace (`budgets.rs` holds the release binary's
`.text` to 28,311,552 B, and a build without the GPU backend to
24,379,392 B, both re-based in M5 for the speed profile, again to
wave 2's merged tree, in M6 for remote media art's HTTPS stack, on
M6 wave 1's `main` with a 512 KiB reserve, and on M6 wave 2's and
m6-service-gaps' merged trees with the same reserve, and raised by the
same rule at the owner's word on `main` at 091c7f64 and for
services-round's services (decisions.md
m5-speed, m5-integration-w2, m6-integration-w1, m6-owner,
m6-service-gaps, growth); see
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
binding read: `size`, or `query` from a `when`; (M6) `box` or
`box_query` when one read its `box`, and on every element a
conditional id may name what is read through the name, `Program::alias_watch`); a surface's configured
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
reloads, IPC requests, `strand watch` events), `lock.rs` (the session
lock in the binary: `Guard::wire`, which enables the session lock only
with `auth`'s tokens wired to `State::unlock`, and tells `auth` whether
a lock is shown, and when one is woken by input, through
`auth::LockSignal`; the main loop's fault
checks, the first-frame timeout and the heartbeat watchdog; the
built-in fallback (`LockScreen`) and its own `strand_auth::Client`
(the `Checker`); lock sessions tagging verdicts; the restart marker;
the password redaction (`Secrets`); and the deferred load committed
after the unlock), `feeds.rs` (media feeds: one producer per visible
`spectrum` or `thumbnail`), `lists.rs` (virtualised lists' windows and
drops' arguments), `gpu.rs` (`GpuHost`: the binary's side of the GPU
thread), `trim.rs` (`trim`, `Trimmer`, `structural`) and `tests.rs`
(`run::tests`).

**IPC** (`crates/strand/src/ipc.rs`): a Unix socket at `$STRAND_SOCKET`
or `$XDG_RUNTIME_DIR/strand-<WAYLAND_DISPLAY>.sock`, newline-delimited
JSON. Requests are `{"v": 1, "cmd": …}`; each is answered with one line
`{"ok": true, …}` or `{"ok": false, "error": …}`, and an unknown `cmd`
or a newer `v` is refused without closing the connection, so M5's `get`,
`toggle` and `call` are new `cmd`s on the same socket (`set` is one
already; M5 moves the protocol's types into `strand_core::ipc`, see "M5
interfaces (wave 0a)"). Version 1: `set` (`"path"`, `"value"` as text: `strand set
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

**M4 additions (planned in docs/m4-plan.md, waves 0a and 0c; as built).** The GPU
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
  built-in fallback lock. The main thread then owns the unlock gate, and
  the fallback's password checks run on a thread of their own,
  `strand-lock-auth` (`run/lock.rs`'s `Checker`, started at the
  fallback's first submit), which holds the fallback's
  `strand_auth::Client` (looking the helper up again at each check until
  one is found), blocks in PAM within the client's deadlines, and
  answers on the main loop's channel (`LockMsg::Checked`), so the main
  thread never waits on PAM. Every `LockMsg` carries the lock session its
  check began in (`LockScreen::session`, bumped when a lock ends; `auth`
  reads it through `AuthConfig::session` when a check begins), and a
  verdict for a session that has ended is dropped (m4-audit). SIGINT and SIGTERM end `strand run` only
  while no lock is shown.
  As built (m4-lock wave 2, `run/lock.rs`; decisions.md m4-lock-w2):
  `lock::Guard::wire` connects `auth`'s tokens (`AuthConfig::sink`) and
  failures (`AuthConfig::failed`) to the main loop and only then calls
  `State::enable_session_lock`. The host's `lock::LockScreen` forwards
  `SurfaceHost::lock_changed` as `ToLogic::LockState` and, once a fault
  shows the fallback, paints every content surface of the lock with
  `strand_render::lock_fallback` and takes its keys. The watchdog is
  `ToLogic::Beat(seq)`, sent only while locked, 1 s after the last
  answer, with 3 s to answer. The faults also include a runtime fault
  inside the lock, the text worker stopping, `auth` failing to check,
  `finished` after `locked` and a lock with no `lock` open. A marker,
  `$XDG_RUNTIME_DIR/strand-<display>.locked`, is written once a lock is
  asked for (before the compositor says locked, so a strand that dies in
  between comes back to the lock; decisions.md m4-audit) and removed
  once no lock is asked for or held
  (an unlock, or a lock the compositor refused); a strand started while
  it exists locks at once with the fallback. While the config's lock
  shows, the other outputs' solids take the `lock` node's `bg`; while
  the fallback shows, its background. While any `type: password` input
  is mounted, logic replaces the error of every runtime fault that can
  carry values with `<redacted>` (keeping what failed and where) in the
  messages it logs and streams to `strand watch`; otherwise it replaces
  the known values, and redacts a message whole when any 4-byte run of
  one is still in it, compared lower-cased (Unicode) and with the
  message's Debug and JSON escapes undone. The known values are the
  inputs' current ones and the newest four replaced by a value they do
  not start (submitted, edited, their input removed), zeroized when
  dropped (`lock::Secrets`, M4 interaction-finish). Values under 4
  bytes are never matched or remembered (any message would match
  them): a password that short is covered only while its input is
  mounted. Residual risk: the remembered values outlive the lock, so
  until four newer values replace them the process's memory holds up
  to four recent passwords after an unlock (freed copies of the field's
  text may linger too, since only `Password` zeroizes); a core dump of
  strand (systemd-coredump after an abort under the unit below) writes
  them to disk. A session that wants no password in a core sets
  `LimitCORE=0` in the unit (or `Storage=none` in coredump.conf).
  The `faults` feature
  (`STRAND_FAULT`) injects each fault for `tests/lock.rs`, which runs
  only in the lock VM.
  Residual risk: rendering runs on the main thread, and the main thread
  is what outlives every other fault. If the process itself dies while
  locked (a panic on the main thread, an allocation failure, which
  aborts, or SIGKILL), the compositor keeps the session locked, as the
  protocol requires, and shows its own abandoned-lock screen with no
  password field. The marker brings the field back only when strand is
  started again, and strand ships no supervisor: a session that uses
  the lock must run strand under one that restarts it when it exits
  abnormally (a systemd user unit with `Restart=on-failure`, or a
  restart loop with a short sleep in the compositor's autostart). Without one, a main
  thread fault while locked needs another way into the session (a VT,
  ssh) to start strand again. With one, the session stays locked
  throughout and the field is back as soon as the new strand runs:
  `tests/lock.rs::a_supervised_strand_dying_while_locked_keeps_the_session_locked`
  (lock VM) runs strand under a restart loop with the policy of the unit
  below, read from this file, kills it twice while locked (SIGKILL, then
  SIGABRT once it is back) and then six more times in a crash loop, each
  as soon as the new strand runs, and finds the desktop hidden in every
  shot until the restarted strand's field shows, with no test step
  starting strand. A user unit for it:

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

  `ExecStart` names the binary by an absolute path: systemd looks a bare
  name up on its own fixed path (`/usr/local/bin`, `/usr/bin`, ...), never
  the user's `$PATH`, so a bare `strand` would not find the
  `~/.cargo/bin` install README describes and the unit would never start
  it. `%h` is the user's home; a package that installs to `/usr/bin`
  writes `ExecStart=/usr/bin/strand run`.

  `StartLimitIntervalSec=0` is what keeps a crash loop from ending the
  lock's way out. Without it systemd's default start limit (5 starts in
  10 s) marks the unit failed after the fifth quick death and starts
  strand no more, and the compositor holds the abandoned lock with no
  password field: exactly the state the supervisor is there to end
  (`tests/lock.rs::the_supervisor_gives_up_as_systemd_does_unless_the_unit_lifts_the_limit`
  plays both). With no limit, the restart delay is the brake instead:
  100 ms at first, growing over five restarts to 2 s (`RestartSteps`,
  systemd 254; older systemd ignores the two keys and waits 100 ms each
  time), so a strand that cannot start costs a restart every 2 s, not
  a busy loop, and strand is started again at most 2 s after any
  death. The compositor keeps the session locked meanwhile.
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
  run only for them. Built for spectra (m4-effects-media): `feed(node,
  bands)` takes `Levels::bins` (empty: rest);
  `take_feed_demand() -> Option<Vec<FeedDemand>>` answers after a paint
  when the set of fed nodes a painted surface drew with pixels changed
  (`FeedDemand { node, kind: FeedKind::Spectrum { device } }`, the
  device's id as logic sends it; none under `reduced_motion`). The host
  asks after each paint and `run/feeds.rs` keeps one
  `audio::tap_levels(LevelTarget::Device(id), …)` per visible spectrum,
  its readings crossing to the main thread on a calloop channel; a node
  that loses its tap is fed silence.

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
          └── strand-introspect (D-Bus introspection; zbus and log only)
strand (binary) wires everything; its ServiceHost adapters join
strand-services' stores to strand-compiler's VM. M5: strand-core gains
`ipc` (the protocol types, serde_json), which strand, strand-services
(the D-Bus front end) and strand-dev share; strand gains a [lib] target
beside its [[bin]] for plugin binaries; strand-dev gains the
tree-sitter-strand package as a dev-dependency (see "M5 interfaces
(wave 0a)").

strand-auth  (M4) lib: wire protocol, Client, UnlockToken (libc, zeroize)
             bin: the PAM helper, the only code that links libpam
  ^-- strand-services (the `auth` service), strand-surface (the unlock
      gate), strand (the fallback lock's `strand-lock-auth` thread);
      both `Client` owners pass `child::restore_in_child` in as the
      spawn's `pre_exec`

strand-fake-wayland  (M4, tests only; publish = false) a fake compositor
                     on wayland-server: toplevels, workspaces, toplevel
                     capture, layer surfaces, xdg popups, a seat keyboard,
                     the M4 surface globals and optionally
                     wp_presentation, with frame callbacks and feedback
                     it can hold, and data control; no Strand crate
  ^-- dev-dependency of strand-services (the wm protocol client's and the
      thumbnail capture tests) and strand-surface (the manager's tests,
      tests/fake.rs); a dependency of eval/ (the surface leg), which is
      outside the root workspace

eval/ (strand-eval)  the animation evaluation sandbox: its own Cargo
                     workspace beside the root one, not a member; path
                     dependencies on strand-render (with `eval-hooks`),
                     strand-compiler, strand-core, strand-scene,
                     strand-text, strand-gpu, strand-services-schema,
                     and for its surface leg strand-surface (default
                     features, `gpu` among them) and strand-fake-wayland
  ^-- nothing: no crate under crates/ depends on it
```

`strand-fake-wayland` (M4) is a test fixture, not part of the runtime: no
root crate depends on it outside `[dev-dependencies]`
(`scripts/eval/check-isolation.sh` (d) checks it), and it depends on no
Strand crate, so it adds no runtime edge to the graph above. It serves
the toplevel and workspace protocols strand-services' wm client speaks
(`ext-foreign-toplevel-list-v1`, `ext-workspace-v1`, optionally
`zwlr_foreign_toplevel_management_v1`) and, with `FakeBuilder::capture`,
the toplevel capture strand-services' thumbnails use
(`ext_foreign_toplevel_image_capture_source_manager_v1`,
`ext_image_copy_capture_manager_v1`, and services-round
`ext_output_image_capture_source_manager_v1` for screen shots, a
monitor's ident being `output:<name>`; `src/capture.rs`,
`strand-services/tests/capture.rs`), with `FakeBuilder::data_control`
the clipboard's `ext-data-control-v1` and `wlr-data-control`
(`src/data_control.rs`: one selection, a client's source or one the fake
sets itself with `Cmd::SetSelection`, reported by `Fake::selection`;
`strand-services/tests/clipboard.rs`); and `wl_compositor`, `wl_shm`,
layer shell, `xdg_wm_base` for popups and (oi-window) toplevels
(`src/xdg.rs`: a configure with a size, states and bounds sent with
`Cmd::ConfigureWindow`, kept for its app id, the compositor's close with
`Cmd::CloseWindow`, the window's title, app id, sizes, geometry, acks
and asks recorded in `SurfaceRecord::toplevel`), a `wl_seat` with a
keyboard whose focus the tests move, the viewporter, single-pixel
buffers, the alpha modifier and `ext-background-effect-v1` for
strand-surface, recording what each surface committed for the tests to
assert on (decisions.md, m4-surface-w1; the crate's own doc comment,
`src/lib.rs`, lists the globals). For eval's surface leg
(decisions.md eval-surface-leg) it also logs every commit with the
state it leaves (`Fake::commits`, `CommitRecord`: buffer, scale,
viewport, damage, opaque, input and blur regions, alpha, (eval-rules-6)
a layer surface's `margin` after it (top, right, bottom, left), frame
callbacks, feedback), and with `SurfaceGlobals::vblank` shows commits
only at a refresh the test sends (`Cmd::Vblank`, at a time on the
presentation clock it names), when it completes frame callbacks and
presents or discards feedback; with
`SurfaceGlobals::window_configure_on_cmd` a window's first configure
waits for `Cmd::ConfigureWindow`. `FakeBuilder::try_start` and
`Fake::try_connect` return an error where `start` and `connect` panic.

`eval/` (eval-sandbox) is the animation evaluation sandbox, the
`strand-eval` package: scenario scripts run on a virtual clock against
`Renderer` and the logic VM in one process, with per-node checks on
every frame (docs/eval.md; docs/decisions.md eval-sandbox). Its driver
takes logic's diffs, wakes, input and its surfaces' events through
`strand_render::host_loop`'s reactions, as the shell's main thread does
(eval-host-loop). It is a
Cargo workspace of its own (`eval/Cargo.toml` has `[workspace]`, and
the root's `members = ["crates/*", …]` does not reach it), so root
`--workspace` builds, tests and feature unification never see it. It
names the Strand crates it uses by path (the root's
`[workspace.dependencies]` cannot be inherited across workspaces; the
path dependencies' own `.workspace = true` fields resolve in the root
workspace, which they are members of), keeps its own `Cargo.lock`
seeded from the root's and held to the root's versions
(`scripts/eval/check-lock.sh`), and copies the root's profiles and
lints (`eval/tests/drift.rs` fails when they drift). It is the only
place strand-render's `eval-hooks` feature is turned on (see
"`strand-render` module map"); `scripts/eval/check-isolation.sh`
checks that no `crates/*/Cargo.toml` names `strand-eval`, a path into
`eval/` or `eval-hooks` (strand-render's own `[features]` line
aside), that the root resolution never enables `eval-hooks`, and that
every `cfg(feature = "eval-hooks")` under `crates/` is in
strand-render, and that strand-fake-wayland is only ever a root
crate's dev-dependency. Its surface leg (docs/eval.md, "Surface leg")
drives the real `strand_surface::SurfaceManager` through its public
interface only: `apply_surface_change`, `repaint`, `raw_handles`,
`hand_off`, `sync_presented` and `take_back`, as crates/strand's GPU host
calls them, with a `SurfaceHost` of its own; strand-surface has no
eval-only code. Its commands run from the repository root with
`--manifest-path eval/Cargo.toml`, so the container's
`CARGO_TARGET_DIR=target/container` and the shared compile cache serve
both workspaces (`scripts/eval/eval.sh`, `scripts/container/run.sh
eval`).

`strand-auth` (M4) is the lock's whole security boundary, small enough to
review alone. Its lib depends on `libc` and `zeroize` and no Strand
crate; its binary adds a hand-written PAM FFI and nothing else. The lib
holds the framed conversation protocol over a socketpair, a blocking
`Client` (`Client::new(helper: PathBuf, pre_exec: fn())`, which spawns
the helper and runs `pre_exec` in the child between fork and exec;
`converse(Inbox, &mut dyn FnMut(Said)) -> Conversed`, one PAM
conversation whose texts and prompts arrive as PAM says them and whose
prompts take the passwords sent through its `Answers`;
`submit(password) -> Verdict`, a conversation whose every prompt gets
that password, and (M6, m6-integration-w1) `submit_saying(password,
&mut dyn FnMut(Said)) -> Verdict`, the same with each text and prompt
told as PAM says it, which the fallback lock's checker uses to show
PAM's text while its check runs; deadlines, respawn on a dead helper, with the same hook),
password buffers that zeroize on drop, and `UnlockToken`:
a value only `Client` mints, from a success reply, which is `Send` and
neither `Clone` nor constructible elsewhere. `strand-surface` releases
a session lock only for an `UnlockToken`, so no other code path can
unlock. The PAM service is `strand`, or `login` with a one-time warning
when libpam would not read a `strand` file (decisions.md, m4-owner; the
rule as built, decisions.md m4-lock-w1 and m4-audit, "the `strand`
service is chosen only when libpam will read its file"): `strand` when
`/etc/pam.d/strand` opens for reading, or when `/usr/lib/pam.d/strand`
does and `login` is only in `/usr/lib/pam.d` (libpam built with a
vendor directory); otherwise `login`, so a missing, unreadable or
ignored `strand` file all fall back (`strand-auth`'s `choose_service`).
Every other PAM error fails closed. A `faults` cargo feature (off in default
and release builds) adds `STRAND_FAULT` injection points here and in
`strand`.
As built (decisions.md, m4-lock-w1; conversations, m6-lock): `protocol`
(frames of a `u32` length, a kind and at most 1 KiB, version 2: `HELLO`
with the version and the service; `BEGIN`, which starts a conversation;
`INFO`, a `PAM_TEXT_INFO` or `PAM_ERROR_MSG` text, and `PROMPT`, a
`PAM_PROMPT_ECHO_OFF` prompt, each sent as PAM says it; `SUBMIT`, the
answer to a prompt; `VERDICT` with success, denied or error and PAM's
error text, which ends the conversation), `Password`, `Client` (`new`,
`with_timeout`, `converse`, `submit`, `helper_pid`, `spawns`,
`service`; `faults` adds `with_test_env`), `answers() -> (Answers,
Inbox)` (`Answers::send(Password) -> bool`, one waiting at a time, from
any thread; `Answers::take_back() -> Option<Password>`, the one no
prompt took, which once the conversation has ended is one sent too late
for it (added in m6-lock's review fixes, lock-r1-5: `send` cannot tell a
conversation that has just ended from one still running); dropping it
ends a conversation with no password in it, `AuthError::Cancelled`), `Said { Message(&str), Prompt(&str) }`,
`Conversed { verdict, answered }`, `refusal(&Password)` (a NUL, or
longer than PAM takes), `Verdict { Unlocked(UnlockToken), Denied {
message }, Failed(AuthError) }`, `default_helper()` (beside the
executable, then the libexec paths) and `take_service_warning()` (the
`login` fallback's warning, once per process). No deadline runs while
no password waits for PAM or is with it (PAM may wait on a reader);
once PAM has a password it must send a frame within the timeout (30 s)
of the last one or of the answer, and a password must be answered (the
next prompt or the verdict) within `LIMIT_FACTOR` (3) timeouts of being
sent. A missed deadline or a bad frame kills the helper and everything
it started; the next conversation starts another. Under `faults` the helper
also takes a private PAM confdir (`STRAND_AUTH_PAM_CONFDIR`,
`pam_start_confdir`), which the tier-A tests use; those tests
(`pam`, `no_faults`) name the feature (`required-features`) and run with
`cargo test -p strand-auth --features faults`, so no plain test build
leaves a helper with the hooks in `target/`.

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
libwayland-client: that build is the CPU core the CPU-only `.text` gate
measures (24,379,392 B since services-round's re-base, 22,806,528 B
from the owner's raise of 2026-10-22 on `main` at 091c7f64; decisions.md
growth, services-round).

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
through it. (M6, the owner's ruling of 2026-10-14)
`set_config_icons(Option<PathBuf>)` (and `config_icons()`) names the
config's flat `icons/` folder, which `resolve` (so `exists`) tries
before the theme for each pair of candidates, a name and its
`-symbolic` variant (`<name>.svg`, else `<name>.png`; listed once per
`generation`); the crate has no dependencies, so it does not find the
config itself: `strand run` sets it first thing (`live::config_icons_dir`)
and hands the watcher `live::config_icons_source(dir)`, a depth-0
`CacheSource::Tree` of `CacheKind::Icons`, so a file added, edited or
removed there goes the `index.theme` way (`invalidate`, the renderer's
`icons_changed`, the apps service). Render recolours a folder file only
when its own name ends in `-symbolic`, and `Renderer::icons_changed`
paints the surfaces drawing icons whole (a new decode may reuse the old
pixmap's address, which the frame records hash). `strand_services::icon_theme::spawn(&Services, switched)`
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
again when it breaks or hangs, which it logs at `warn`: `dropped its
connection :1.N to the bus (why)`, or `gave up connecting to the bus`
when the connect ran past the bound; the reloads test reads these to
tell a reconnect from a connection made for nothing, decisions.md
m6-flakes), `parse(xml)`,
`default_path(name)`, and `Cache`: answers remembered for `TTL` (10 s),
`properties` blocking, `properties_or_ask` answering from what it
remembers and asking on a thread of its own): what `from dbus` services
are checked against (`strand check`, the loader, the LSP) and what the
running service reads signatures from. The compiler's
`check::dbus::Introspect::properties` answers `None` while a question is
out (the LSP's, which never waits on a bus: an unanswered service is
unchecked until the answer wakes the server to publish again).
(M6, decisions.md m6-w2-language) It reads an object's methods from the
same XML: `Method { interface, name, inputs }` (the in-argument
signatures, an argument without a direction counting as in),
`parse_methods(xml)`, `methods(&Bus, name, path)` blocking, and
`members_on(conn, name, path) -> Result<(Vec<Property>, Vec<Method>),
String>`, which the running custom service binds its actions' methods
with. `Cache<T>` is generic over the answer (`Cache<Vec<Property>>` the
default, `Cache::<Vec<Method>>::methods(ttl)`); `answer`/`answer_or_ask`
are the generic forms of `properties`/`properties_or_ask`. The
compiler's `Introspect::methods(system, name, path) ->
Option<Result<Vec<BusMethod>, String>>` defaults to `None` (unchecked),
so an implementor that predates it compiles unchanged.

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
  sizing, size holds; `resize_share`, public with `layout::edge_shift`
  so `crates/strand/tests/resize_parity.rs` holds where render keeps a
  resized box to strand-surface's placement, which render cannot
  depend on), `surfaces.rs` (attach, configure, detach, hit),
  `pose.rs` (exit poses and closing surfaces), `lists.rs` (scrolling),
  `tooltip.rs`, `swap.rs` (theme swaps), `tests.rs`, `feed.rs` (media
  sources for the nodes a diff creates, feeds and feed demand; built by
  m4-effects-media, with `media/` beside `renderer/`: `graph.rs`,
  `spectrum.rs`, `animated.rs`, `svg.rs`, `lottie.rs` (velato with
  `default-features = false`, drawn through a vello_cpu `RenderSink`;
  its image assets decoded with the file); the `svg` and `lottie` files
  are read and parsed on the image decode worker as file jobs
  (`ImageStore::load_file`, reached from `RasterProps::files`), whose
  arrival marks every surface dirty; `thumbnail.rs`, `thumbnail w`'s
  CPU raster node fed by the windows service's captures),
  `backend.rs` (M4: the GPU backend as render drives it, promotion and
  the device's lifecycle, lowering a display list to `strand-gpu`'s
  frames, `shader` passes and the replies), with `promote.rs` (the
  promotion state machines) and `canvas.rs` (a `canvas`'s draw list as
  display items) beside `renderer/`.
- `flatten/`: `mod.rs` (display list types, `flatten`, `Flattener`),
  `node.rs` (one node: box, paint, shadows, text, clips, children),
  `text.rs`, `paint.rs`, `hash.rs`, `widget.rs`, `image.rs`, `gpu.rs`
  (M4, `gpu` builds: the pass wants a node adds while drawn and the
  GPU's pixels drawn in place of the CPU's), `tests.rs`.
- `raster/`: `mod.rs` (`Raster`, its threads' `Lane`s and `share`),
  `atlas.rs` (`AtlasMirror`), `paint.rs` (scene paints as vello
  paints), `draw.rs` (drawing a display list, disjoint damage),
  `tests.rs`.
- `anim/`: `mod.rs` (`Animator` and its per-frame `paint`), `motion.rs`
  (channel encoding, `PropMotion`), `pose.rs` (enter/exit poses),
  `sizes.rs` (size springs), `keyframes.rs` (M4, S-effects: `play`
  playback composed over the springs), `stagger.rs` (M4, S-effects:
  children entering one `stagger:` step apart), `morph.rs` (M4,
  S-effects: shared-element `morph:` from the box its name was last
  drawn at, on the same surface or, moved by the surfaces' origins
  (`Renderer::set_surface_origin(surface, Option<(Option<String>,
  LogicalPoint)>)`, which the binary's host calls from
  `SurfaceHost::surface_placed` with the surface's monitor; a popup
  takes its parent's output, looked up again whenever a surface is
  placed, moves or goes), on another surface of the same output),
  `pages.rs` (page slides), `tests.rs`.
- `effects/` (S-effects): `mod.rs` (group effects from props),
  `filter.rs` (colour matrices), `glow.rs` (CPU glows), `light.rs`
  (`glow:`, `inner_shadow:`, `rim:` as display items), `raster.rs`
  (raster sources built from props: `grain:`, particles, the built-in
  effects), `builtin.rs` (lightning, sparks, shimmer, ripple, aurora's CPU
  fallback; the canvas and sprites), `particles.rs` (CPU sprite blits),
  `letters.rs` (a text's `letters`, one letter at a time), `roll.rs`
  (`roll: true` texts rolling their changed letters), `lean.rs`
  (`parallax:` and the CPU's 2D `tilt:` following the pointer),
  `jelly.rs` (`jelly:` squash and stretch of a dragged node, from its
  drawn offset),
  `transition.rs` (transition masks: clip paths, pixelate's mosaic),
  `goo.rs` (`merge d`'s goo field contoured by marching squares, a CPU
  raster under the children), `gpu.rs` (M4, `gpu` builds: the bundled
  GPU effects as pass wants, decisions.md m4-gpu-effects).
- `backdrop.rs` (S-effects): `backdrop: blur()` and `glass()`'s CPU
  fallback, an offscreen group of what is drawn behind the node;
  `hash_behind`, what the node's damage and its pass's want key hash of
  what is behind it: the items over its region and (perf-gpu-retain
  review) the clip, opacity, transform and layer markers around them
  and every layer over it, the groups open at the node (its ancestors')
  left out.
- Top-level files beside the directories, among them: `cache.rs`
  (dithered gradients and blurred shadows drawn once into pixmaps, a
  large shadow as size-independent slice tiles),
  `layers.rs` (M4: effect layers as `PushLayer`/`PopLayer`),
  `offscreen.rs` (M4: cached offscreen groups and CPU raster nodes;
  m6-owner-fixes: `offscreen/fade.rs`, fade layers),
  `clock.rs` (M4: per-node clocks with frame caps), `time.rs` (M4: `t`,
  `wave`, `noise` evaluated per node), `lock_fallback.rs` (the built-in
  fallback lock, drawn with no text worker; (M6, m6-integration-w1)
  with PAM's text below its field in a 5×7 bitmap font of its own,
  `lock_fallback/font.rs`: `LockFallback::set_message(&str) -> bool`,
  and `lock_fallback::paint(target, dots, state, message)` takes the
  text as its new last argument).
- `shapes/` (S-effects): `mod.rs` (the shape library as outlines and
  paths, `Polygon` coverage for `mask: shape()`), `morph.rs` (`shape:`
  morphs, held by the `Animator`), `stroke.rs` (stroke styles: trim,
  wave, dash, caps; arc centre lines).
- `host_loop.rs` (eval-host-loop, public as `strand_render::host_loop`):
  the host loop's reactions and the `LoopHost` and `LoopSync` traits a
  host gives its side through (see "Render loop"); the shell's main
  thread and `eval/`'s driver both run them.
- `layout/`: `mod.rs` (the pass, `Boxes`, `RootSize`, prop helpers),
  `style.rs` (a node's taffy style), `text.rs` (`TextSizes`, leaf
  measuring), `list.rs` (`ScrollState`, list virtualisation).
- (eval-sandbox) The `eval-hooks` feature, off by default and never
  enabled by a crate of the root workspace (only `eval/` turns it on;
  `scripts/eval/check-isolation.sh`), adds the evaluation sandbox's
  probe and changes nothing a frame draws (but for the test-only damage
  mutant below):
  - `probe.rs`, public as `strand_render::probe`: plain data,
    `FrameTrace` (one per `Painter::paint`, CPU or GPU, empty damage
    included, never `update`'s preview: the frame's time, backend,
    whether it was flattened afresh, its own damage, rasterised pixels,
    items, records, layout passes and nodes, pending and held texts, the
    advisory wall times, and its nodes), `NodeTrace` (per drawn or
    hidden node: laid, drawn and visual boxes, own opacity, scale and
    rotation, the offset parts, `Motions` and `authored` `Axes` bits,
    `Hidden`, `TextDrawn`; a text's `fit` and `ink`, and (eval-r3)
    `overhang`, how far the ink of the layout it drew reaches past that
    layout's own box on the left and right), `OffsetParts`, `HeldText`;
    (stagger) `enter_of(&SceneTree, NodeId) -> Option<PropValue>`, a
    node's `enter` read through its token scope as `anim::pose::has_enter`
    reads it, so the sandbox numbers a stagger's children as render does;
    (eval-coverage) `FrameTrace::fade`, a theme swap's crossfade weight
    while one plays, and `NodeTrace::paint`, a `NodePaint` read from the
    display items the node added itself (`fills` and `borders` as
    `Paint`s, `shadows` as `ShadowDrawn` (its colour and the `extent` it
    may cover, before transforms; (eval-rules-8) `asked`, the offset,
    blur and spread its props asked this frame, springs applied, in
    logical px before the frame's scale, and `frame`, the box it is cast
    by as drawn), `glyphs` as `GlyphsDrawn` (its paint,
    span colours, the `text` its layout was shaped from and the `subset`
    of glyphs drawn when not all were)); (eval-rules-4,
    eval-coverage-3) `NodeTrace::images`, an `ImageDrawn` per decode an
    `image` or `icon` drew this frame, an image swap's old one first (its
    box, where the whole decoded pixmap is drawn in it, the pixmap's size
    in device pixels, the source's size as the decode placed it, the
    `fit:` the node asks for and the fit the pixmap was decoded by, whether
    a decode for another box size stands in, a symbolic icon's tint, and
    (integration-r7) `file`, the `source` the decode was made from, the
    node's own or the one an image swap comes from;
    `Fit::name` gives a fit's keyword), and `NodeTrace::cut`, a
    `CutDrawn` for a text cut to its line box when it drew the layout
    shaped for that box at this scale (the text, `ellipsis:` and
    `max_lines` as asked, the drawn layout's cluster stops with their
    lines, and the unbounded layout's stops when it is one line);
    (eval-rules)
    `Motions` also says which of a node's own colours springs
    (`FILL_SPRING`: its `bg`, a meter's `value` or `track`;
    `INK_SPRING`: its text colour or solid `fill`; `LINE_SPRING`: its
    border); (eval-rules-2) `FrameTrace::period` (the surface's frame
    period its changes start one of before their first frame) and
    `FrameTrace::springs`, a `SpringTrace` per sample the frame took of a
    prop's, a laid-out size's, a glide's or (eval-rules-3) a theme
    swap's palette root's `Motion` (`SpringOf::{Prop, Size, Glide,
    Palette}`: the node (for a palette root, the root of the surface it
    painted, with the root's token path in `token`), the sample's time,
    the value, the target and `Curve` it moves along, `eps` (one
    tolerance per channel, border-eps), `rest` when it was at rest before,
    for a palette root `asked` (the curve the swap's table gives
    `Prop::Color`, worked out by the plan's own expression: a drift guard),
    `start` (`SpringStart`: its sample before and a FLIP's jump) when a
    change started at it, and on the frame its node enters, `entering`:
    whether its enter pose sets that prop), from `Motion::view` taken
    before the sample; `NodeTrace::clip` (the clip its ancestors set,
    logical, when narrower than its surface), `NodeTrace::roll`
    (`RollDrawn`: a rolling text's two texts and progress, and on the
    frame a change interrupted a roll, that roll) and `NodeTrace::drag`
    (a slider's drag value while a drag holds it); (eval-rules-6)
    `NodeTrace::ring`, a `RingDrawn` for a node's `border:` as the
    flattener drew it (its drawn box in device px on the physical pixels
    its edges fall on, the even-odd outline it filled as a `kurbo::BezPath`
    in device px before the node's transforms, the device pixels per
    logical pixel, the drawn box's logical size, whether it follows a
    `shape:` outline or squircle corners, and the `border:` width and
    `radius:` as this frame's props resolve them), and `NodeTrace::gap`, a container's `gap:` as its props give
    it this frame (logical px, `None` when unset or not a number);
  - `Renderer::set_frame_trace(bool)`, `take_frame_traces() ->
    Vec<FrameTrace>`, `set_text_hold(bool)`,
    `set_text_hold_with(Option<HoldPick>)` (`HoldPick = Box<dyn
    FnMut(NodeId, &str) -> bool>`: hold only the texts it picks),
    `held_text() -> Vec<HeldText>` and `release_text(&mut dyn
    FnMut(&HeldText) -> bool) -> usize` (`renderer/probe.rs`): the
    trace's switch, and a hold on the layouts inline shaping makes, which
    stay pending, as a late text worker's do, until released through the
    usual delivery;
  - `Renderer::set_now(Option<Instant>)`: the time the renderer's own
    timers read in place of the wall clock (a tooltip's delay, a capped
    clock's ticks, so `next_wake` answers on it), through the private
    `Renderer::now`, which is `Instant::now()` without the feature or
    unset; and `take_inline_shaped() -> u32`, the layouts inline shaping
    delivered at once since the last call (each one a text worker would
    have woken the host's loop for);
  - (eval-refs, the `redraw` check) `Renderer::set_keep_frames(bool)`:
    keep each surface's last painted frame as drawn (its display list,
    passes, size, scale and crossfade weight, in the probe);
    `repaint_fresh(&self, SurfaceId, &mut PaintTarget) -> bool`: that
    frame drawn again in full by a raster with nothing cached, blended
    over its crossfade snapshot; `fresh_gpu_frame(&self, SurfaceId, id,
    first_upload) -> Option<strand_gpu::Frame>` (with `gpu`): it lowered
    again with a raster and an upload table of their own, every pixmap
    in its uploads, with the presented frame's `FrameSnapshot` (a
    snapshot the GPU thread holds is the fresh device's own last frame,
    the one before drawn fresh); (eval-rules-4) `repaint_unblended(&self, SurfaceId,
    &mut PaintTarget) -> Option<(&[u8], f32)>`: when that frame was
    blended over a crossfade snapshot standing alone, the frame drawn
    again in full without the blend, with the snapshot's pixels and the
    weight it was blended at, for a check that blends them by its own
    arithmetic. All three read the renderer only. And
    `set_damage_inset(u32)`, a mutant only eval/tests turns on: each
    frame's own damage shrunk by that many pixels on every side;
  - `Router::set_now(Option<Instant>)`: the time the router reads for
    hover delays, grace and edits in place of the wall clock;
  - the trace is built in `flatten/probe.rs` from the locals
    `Flattener::node` already has (`Animator::motions` reads motion
    state without sampling), into `Flattened::trace`.
  Their only consumer is `eval/`.

## M5 interfaces (wave 0a)

The cross-crate interfaces M5's streams build against, fixed before any
stream branches (`docs/m5-plan.md`, "0a: interface text", with the
owner's answers of 2026-10-10, decisions.md m5-owner). Wave 0b lands
them as code with no behaviour change; after that each stream edits only
its own crate's text here. Names marked "new" do not exist at `70cdc56`.

**IPC protocol in `strand-core` (`strand_core::ipc`, new).**
- The request, answer and event types and their JSON-line codec move
  from `crates/strand/src/ipc.rs` into `strand_core::ipc`: types and
  codec only, no I/O and no threads. `strand-core` gains `serde_json`
  (already in the graph through strand-services and the binary; the
  codec reads `serde_json::Value`, so `serde`'s derives are not needed).
  Answers and events are JSON objects (`ipc::Answer`, with `ok()`,
  `refused(why)`, `unknown(cmd)`, `refusal(answer)` and `line(v)`). The binary (socket server and CLI), strand-services (the
  D-Bus front end) and strand-dev (`strand-dev inspect`) all name these
  types. strand-dev takes a direct `strand-core` dependency for them
  (S-inspect adds it to strand-dev's `Cargo.toml` with its client, and
  the integrator merges `Cargo.lock`): a crate reached only through
  strand-compiler cannot be named, and strand-compiler does not
  re-export it. (0a said strand-dev "reaches strand-core through
  strand-compiler"; decisions.md m5-w0-harden.)
- The socket's path is `strand_core::ipc::socket_path()`
  (`$STRAND_SOCKET`, else `$XDG_RUNTIME_DIR/strand-<display>.sock`),
  so the shell, its CLI and `strand-dev inspect` find it by one rule.
  (launch-env) Its state directory is likewise
  `strand_core::persist::state_dir()` (`$STRAND_STATE_DIR`, else
  `$XDG_STATE_HOME/strand`, else `~/.local/state/strand`): every store
  (`PersistStore::from_env`, and through it settings, tokens, palettes,
  `file` manifests, `apps` and `notifications`; the clipboard history;
  `strand export`) resolves it there. Both variables address one
  shell, so `strand_services::child::child_env` removes them
  (`child::INSTANCE_VARS`) from every program the shell starts.
- `ipc.rs` in the binary becomes `ipc/{mod, server, cli, describe}.rs`
  over them.
- Protocol version 1 gains commands only, and every rule above holds (an
  unknown `cmd` is refused without closing the connection):
  - `get{path}`, `toggle{path}`, `call{target, args}`, `clear{path}`,
    `report{}`;
  - the inspector's `inspect.pick{on}`, `inspect.node{id}`,
    `inspect.dump{surface?}`, `inspect.highlight{id?}`,
    `inspect.flash{on}`;
  - the existing `reset` gains a `{node}` form, "reset subtree"
    (`Instance::reset_subtree`), `Request::ResetNode`.
  - the theming hub's `export{kind, template?, name?}` (S-tools, M5
    wave 2: `Request::Export`; `kind` is `json`, `base24` or
    `template`; `template` carries the `--template` file's text and
    `name` its path, so an error is located in the file the user named;
    `--palette` never travels: the CLI renders it offline, and it also
    mounts offline when no shell answers, the shell predates `export`,
    or the template is too large for one request line), a socket request only: the D-Bus front end has
    no `Export` method (decisions.md m5-tools, "the CLI"). Its
    `parse_m5` arm, `Shell::request` arm and handler
    (`run/request/export.rs`) are S-tools' (m5-plan, Streams).
  - On the wire a node id (`inspect.node{id}`, `inspect.highlight{id}`,
    `reset{node}`) and `inspect.dump`'s `surface` are opaque strings
    the inspector's own answers spell (S-inspect picks the spelling);
    `call`'s `args` are a list of strings, each read by the action
    parameter's type as `set`'s value is.
  - Every command above is served on the socket since M5 wave 3.
    Where one is not served (the D-Bus front end has no `Export`
    method, and answers `inspect.pick`, `inspect.highlight` and
    `inspect.flash`, which are socket-only, through `not_served`) it is
    answered exactly as an unknown one (`unknown command `x``), so 0b
    changes no answer;
    `reset{node}` is answered "`reset` needs a `path`", M4's answer,
    from the socket and from the request channel alike.
    `Request` has every command's variant and `encode` writes each, but
    `ipc::parse` reads only the commands the shell serves: M5's fields
    are read by `ipc::parse_m5(cmd, &v)`, which a stream calls from a
    `parse` arm when it serves its command (S-inspect's arm sends a
    `reset` with a `node` to it). Until then `parse` answers as M4 did,
    field for field: a `reset` naming a `node` beside its `path` resets
    the path, and one with only a `node` hears "`reset` needs a `path`".
    (0a had `parse` read every command; 0b measured that at about 5 KB
    of `.text` for commands nothing serves, and it changed what M4
    clients heard for malformed requests: decisions.md, m5-core.)
    Served since M5 wave 2 (S-ipc, over S-inspect's library): `parse`
    sends `inspect.node`, `inspect.dump` and a `reset` with a `node`
    and no `path` to `parse_m5` (a `reset` with neither still hears
    "`reset` needs a `path`", and one with both still resets the path);
    their answers are `{"ok": true, …}` with the library's fields
    (`node`/`files`, `nodes`/`files`, `reset`: the cells changed), and
    a node ref that does not read or names no node is refused.
  - `get` reads what `set` writes: exported values, settings fields and
    token overlays (decisions.md wave3-theme, `strand set`; service field
    reads are out of scope). `call` reaches service-level actions only
    (m5-owner Q13). `toggle` is bool only. Paths that name a token use
    the token spelling, `$accent` (m5-owner Q10).
- No paging: `MAX_LINE` (64 KiB) bounds only incoming requests, and an
  answer is bounded by the connection's 1 MiB queue.
- **The D-Bus name** (m5-owner Q12): `pub const DBUS_NAME: &str =
  "io.github.jakeb_grant.Strand";` in `strand_core::ipc`, the bus name
  and the interface name, at object path `/`. It is the only spelling of
  the name: if zbus's `#[interface(name = …)]` needs a literal, that
  literal is the one other occurrence, and a strand-services unit test
  fails unless it equals `DBUS_NAME`. The owner may rename it before 1.0
  (spec.md says so).

**Request channel from the services thread to logic (`IpcRequests`,
new).**
- `strand_core::ipc::IpcRequests` sends `IpcCall { request: Request,
  reply: Reply }` to the logic loop (`ipc::channel(wake) ->
  (IpcRequests, IpcReceiver)`; `IpcRequests::call(request) ->
  Result<ReplyFuture, IpcClosed>`). It is a shared queue (a
  `Mutex<Vec<IpcCall>>` the receiver drains whole) plus a wake function
  the binary supplies (the logic loop's sleeper ping), so strand-core
  depends on neither calloop nor tokio. (0a said a `std::sync::mpsc`
  sender; 0b measured its generic channel code at about 2 KB of
  `.text` against 0b's 1 KB bound, and the queue does the same job:
  decisions.md, m5-core.) Dropping the receiver refuses what is queued
  and every later call (`IpcClosed`). `Reply` is
  a one-shot that completes once and wakes a `std::task::Waker`, so an
  async caller awaits it (`ReplyFuture`) without a runtime in
  strand-core; a `Reply` dropped unanswered completes with a refusal,
  so a caller never hangs.
- The logic loop drains it as a non-blocking source beside the socket
  and answers each call through the same `Shell::request` dispatch the
  socket uses (`Caller::Socket(id)` or `Caller::Bus(reply)`); logic
  never waits on the services thread. The loop makes the channel (its
  wake is the sleeper's ping) and hands a clone of the sender to
  `Live::requests_to` when one is given (tests; S-ipc passes its own
  to `strand_services::ipc_bus`). A `reload` from the channel waits
  for its load as a socket client's does: `Job::Reload`'s client stays a
  client id, and a bus reload gets one of its own with the top bit set
  (`BUS_RELOAD`; the socket numbers its clients up from 1), kept with
  its `Reply` until that load's event answers it (decisions.md m5-ipc).
  `watch` from the channel is answered as unknown: on D-Bus, watching
  is the `Changed` signal.
- **Events the other way (`strand_core::ipc::EventSink`, new in the
  wave-0 hardening).** The logic loop hands every event the socket's
  watchers get (`reload`, `fault`, `notices`) to an `EventSink`, a
  cloneable `Fn(&Answer) + Send + Sync` given through
  `Live::events_to`, whether or not the socket bound or anyone
  watches; `Shell::broadcast` is the one fan-out (socket and sink).
  The sink runs on the logic thread and must not block: the D-Bus
  front end hands each event to the services thread, which emits
  `Changed`. Run test:
  `requests_from_another_thread_are_answered_by_the_logic_loop`.
- The D-Bus front end (`strand_services::ipc_bus`, new, S-ipc) runs on
  the services thread's tokio runtime, which owns the zbus connection.
  Each method call becomes a `Request`, awaited through `Reply`.
  `watch` is a `Changed` signal fed by the `EventSink` above. The name is owned when `strand run` starts; a second instance
  that cannot own it runs without D-Bus IPC, as with the socket
  (decisions.md wave2-runtime, the IPC socket). It lives in strand-services
  because that thread owns the async runtime and the bus; the binary has
  no executor of its own. As built (decisions.md m5-ipc):
  - `ipc_bus::spawn(&Services, IpcRequests) -> (FrontEnd, EventSink)`:
    a task on the shared services runtime (`Services::spawn_task`), on
    a session-bus connection of its own (so the name goes with it),
    serving until the `FrontEnd` is dropped; the logic loop calls it
    once the real services exist (not under `STRAND_MOCK`) and hands
    the sink, beside `Live::events_to`'s, to `Shell::broadcast`. The
    object is served before the name is asked for (`DoNotQueue`).
  - Methods, each answering the socket's answer object as JSON text, a
    refusal being `org.freedesktop.DBus.Error.Failed` with the socket's
    `error`: `Get(s) → s`, `Set(s, s) → s`, `Toggle(s) → s`, `Clear(s)
    → s`, `Reset(s) → s`, `Call(s, as) → s`, `Reload(b) → s`. The signal
    is `Changed(s event, s data)`: the event's kind and the whole event
    as JSON. The inspector's commands, `report` and `export` are the
    socket's only.

**Snapshot hop and the inspector's messages between logic and main
(new).** Logic sends main `ToMain` messages over a logic→main channel
(today logic sends main only `SceneDiff`s):
- `ToMain::Snapshot { kind: SnapshotKind, reply }`, answered with
  `ToLogic::Snapshot(Snapshot)`, where `reply` is a `SnapshotId` of
  logic's numbering and `Snapshot { reply, data }` carries the snapshot
  as the JSON its IPC answer holds. `SnapshotKind::Report` is `report`'s
  (the compositor's globals with versions, outputs and scales, frame
  summaries, GPU status, reason and lost-device count, promotion
  reasons); `SnapshotKind::Layout { node }` is a node's layout on each
  surface showing it (main resolves them with
  `Renderer::surfaces_of(node)` and answers
  `Renderer::node_layout(surface, node)` for each). Main answers from its
  own state between frames and never waits on logic.
- `ToMain::Highlight(Option<NodeId>)` and `ToMain::DamageFlash(bool)`:
  main calls `Renderer::set_highlight`, which outlines the node on every
  surface showing it, and `set_damage_flash` (`inspect.highlight`,
  `inspect.flash`).
- Logic names nodes, never surfaces: a `SurfaceId` is main's (assigned
  at `attach_surface`), only a pick and an input context carry one to
  logic, and only for the surface clicked,
  and one root shows on several surfaces (a bar per output). So no
  logic→main message carries a `SurfaceId`; a node chosen from a dump or
  typed into the terminal client is resolved on main.
- `ToMain::PickMode { on, seq }`: the router's pick-mode hook on or off
  (`inspect.pick`, and off when the client that asked disconnects);
  `seq` numbers logic's `PickMode` messages.
  A pick happens later, on a click, so it is not a snapshot: main sends
  `ToLogic::Picked(PickEvent::Picked { surface, node, seq })` (`node`
  from `Renderer::pick`, none when nothing pickable is under the click;
  `seq` the `PickMode` main was in), and
  `ToLogic::Picked(PickEvent::Ended { seq })` when pick mode ends on
  main's side (a surface unmapped, a lock started), `seq` being the last
  `PickMode`'s main acted on. The socket and this channel are separate
  sources on logic's loop, so an `Ended` can arrive after a newer
  `inspect.pick` has turned pick mode on again; logic ignores an `Ended`
  whose `seq` is not its last (decisions.md m5-inspect, inspect-r1-2),
  and a `Picked` likewise, so a click from an earlier pick mode never
  reaches a client that took pick mode since (inspect-r2-2).

0b landed the snapshot types, and the wave-0 hardening the rest
(decisions.md m5-w0-harden; 0b had a `SnapshotKind::Pick` request, and
the first hardening round's `SurfaceId` in `Layout` and `Highlight` went
in the second, since logic cannot name one), all in `strand/src/run/mod.rs`.
The channel itself is wired by the first stream that sends on it,
since an unused calloop channel would cost `.text` for nothing: S-tools'
`report` wired it (`Live::to_main`, a calloop channel whose source on
the main loop answers `SnapshotKind::Report` with
`crate::report::main_part`; logic keeps each waiting caller under its
`SnapshotId`, adds its own half, the PSS from
`/proc/self/smaps_rollup`, the version and the compositor's name from
the environment, and answers; with no main thread, `report` is
refused), and S-inspect reuses it.

**`strand-compiler` `Instance` inspection (new; read-only, never read by
evaluation).**
- `identity(node) -> Option<Identity { instance_path, key, id, also,
  sibling_index, scope }>` (`None` for a node the instance does not
  hold); `id` is the element's own `id:`, else its component call's,
  and `also` every other `id:` naming it through a chain of calls
  (eval-finish);
- `cells_of(node) -> Vec<CellInfo { path, kept, persisted, layer }>`,
  where `layer` is the settings layer or the token overlay a value came
  from (the three kept badges: kept over a changed default, persisted,
  from an overlay);
- `reset_subtree(node) -> Result<usize, Error>` (the number of cells
  reset): `strand reload --hard`'s scope limited to the subtree;
  settings are untouched;
- `source_hash(file) -> Option<String>`: the hash of the text the
  running program was compiled from, which write-back compares against;
- `inspect_node(node) -> Option<Json>` and `dump(root: Option<NodeId>)
  -> Option<Json>`: the inspector's answers. `dump` takes a node (a
  surface root) or `None` for every surface, and answers `{"nodes":
  [...], "files": {name: blake3}}` in tree order, each node with `id`,
  `kind`, `parent`, `origin` (file, 1-based line and column, byte span),
  `identity`, `props` (as text), `cells` and `tokens` (each read token's
  provenance chain); `inspect_node` answers the same shape for one node
  (decisions.md m5-inspect, "node ids on the wire");
- the free functions `inspect::node_ref(NodeId) -> String` and
  `inspect::parse_node_ref(&str) -> Option<NodeId>`: a node on the wire
  is `index.generation` (`12.0`), which `inspect.node{id}`,
  `inspect.highlight{id}` and `reset{node}` read back; a bare index
  would name a reused slot's new node. `inspect.dump{surface}` carries
  the surface root's node ref, and the wave-2 arm passes that node to
  `dump`.
- A dump redacts password inputs (the lock's `Secrets` rule above:
  current and the newest four replaced values, learned from each tick's
  sent props and from every widget write, any 4-byte run, lower-cased).

**The token overlay layer (`strand-compiler`, new, S-ipc).**
`Instance::set_token_overlay(path, value)` and
`clear_token_overlay(path)` mirror `strand_core::settings`'
`set_overlay`/`clear_overlay`. A token overlay is global, persisted in
`$XDG_STATE_HOME/strand/tokens.toml` beside settings' overlay (written
off the logic thread by the overlay's own writer thread, `strand-tokens`:
temp file, `fsync`, rename, directory `fsync`, the newest text queued
winning; a failed write is settings' `WriteFailed` notice; not the
persist IO thread, which has no raw-file write; decisions.md m5-ipc),
and takes the token swap path (design.md, "Reload"). Clearing a
name that is neither a token of the current build nor held by an
overlay or a `tokens.toml` entry is refused with the closest name, as
setting one is; an overlay or a `tokens.toml` entry for a name the
build lacks (a token since removed, or an entry that is not a value) is
cleared (`cleared: true`); a known token without an overlay answers
`cleared: false` (decisions.md m5-ipc, `clear` and review round 2). A `tokens.toml` that
cannot be read is never written over: `set` and `clear` on a token are
refused until it can be. One edited by hand while the shell runs is
read again by the next `set` or `clear` before it is written, so the
hand edit stays. `TokenTable`'s recorded origins stay strings (a tier and its
name: `base`, `tokens compact`, `palette:wallpaper`); the overlay origin
is the reserved string `overlay`, `strand_scene::tokens::OVERLAY_ORIGIN`
(landed in the wave-0 hardening, so no stream edits `tokens.rs` for it).
S-ipc's overlay layer records it with `set_origin` for each overlaid
path, in place of the file's origin while the overlay is set (the next
build without the overlay records the file's again), and S-inspect's
provenance walk reads it as the overlay step. There is no `TokenOrigin`
type (decisions.md m5-w0-harden). A save the overlay shadows raises a notice like
settings' `SettingsIssue::Shadowed`: `accent: file changed but runtime
overlay wins [clear]`; `[clear]`, `strand clear '$accent'` and IPC
`clear{path}` remove it.

**`strand-scene`: `TokenTable::provenance(path, scope) ->
Vec<ProvenanceStep>` (new).** It walks `TokenExpr::Ref` through the
`TokenScope` chain: `self` is the global table (component `tokens { }`
blocks are part of it, as `Toast.*` entries with origin `component
Toast`) and `scope` the subtree `set { }` override levels, outermost
first (decisions.md m5-inspect). It reads each step's `origin`, and adds
an overlay step and a contrast-guard step where they apply.

**`strand-render` (new).**
- `Renderer::pick(surface, point) -> Option<NodeId>`;
- `Renderer::node_layout(surface, node) -> LayoutInfo`;
- `Renderer::surfaces_of(node) -> Vec<SurfaceId>`: the surfaces
  attached at the nearest surface root at or above the node (every
  output's for a bar, a popup's own for a node in it; landed, not inert,
  in the wave-0 hardening);
- `Renderer::set_highlight(Option<NodeId>)`, drawn on every surface
  `surfaces_of` names;
- `Renderer::set_damage_flash(bool)`: each frame's damage tinted for
  300 ms, the flash's own repaint excluded; off means no clock and no
  allocation;
- `Renderer::frame_summary(surface) -> Option<FrameSummary>`:
  p50/p95/p99 over the frames drawn among the last 600 paints, and
  `drawn` and `unchanged`, the counts of those paints that drew a frame
  and that changed nothing (`renderer/summary.rs`, S-tools; built: a
  paint is one `paint`, its time the render thread's work in it, drawn
  when it damaged something and unchanged when it damaged nothing,
  which no percentile counts (m5-fixups; "discarded" stays
  `wp_presentation`'s word); a frame a `paint`
  sends to the GPU (a promoted surface's) runs from that `paint`'s start
  to the GPU thread's `Presented` or `Pixels`, and counts as drawn
  only then; `Presented { surface, frame, at, waited, ran }`'s `at` is
  taken when the GPU thread has submitted the frame, before `present`,
  and its `waited` (the request's time in the GPU thread's queue behind
  other work, and the thread's block on the swapchain for an image,
  FIFO pacing) is subtracted; the GPU thread never waits for a
  presented frame's GPU work, so the GPU's run of it comes from
  timestamp queries in `ran` of a later `Presented` of that surface,
  and render counts the frame then, as its time before the GPU plus
  its run (a frame whose run never comes is skipped). A readback
  frame's `Pixels` comes after the GPU ran it and carries its queue
  time as `waited`, subtracted too. Both backends time the frame
  through the GPU's render, and neither times vsync or other surfaces'
  work (m5-tools review rounds 2 and 3); nearest-rank percentiles over the drawn frames, zero before the first; a detached
  surface's frames go). Beside it,
  `Renderer::gpu_lost_devices() -> u32` (0 without the `gpu` feature)
  for the report's lost-device count, and strand-surface's
  `State::globals() -> Vec<(String, u32)>` (each global's interface
  and version, sorted, no global names); the outputs are the existing
  `State::monitors()`, so no `outputs()` was added.

0b landed these inert; S-inspect's wave 3 built them in `src/inspect/`
(decisions.md m5-inspect, wave 3): `pick` is the topmost node of the
surface's last painted frame under the point (none on a lock surface);
`LayoutInfo { rect: Option<LogicalRect> }` is the node's box as that
frame drew it, in logical pixels, after paint offsets, scrolling and
any transform (their bounding box), without `hit: grow(n)`; the outline
is a 2 px ring inside the node's rounded box, under its transform and
its ancestors' clip; the flash tints each frame's own repaint, fading
over 300 ms, and only frames with a presentation time (never a lock
surface's). Both are display
items appended to the frame's list after flattening, so the CPU raster
and the GPU's lowering draw them unchanged. Pick mode's two calls,
beside them:
- `Router::set_picking(on, scene) -> Vec<Intent>` and
  `Router::picking()`: while picking, pointer events reach no handler
  (the pointer's position is still tracked), and neither does another
  program's drag (no drop target, so its offer is declined, and no
  `on drop`); turning it on ends hovers, presses and drags as a leave
  would, forgets an offer, and returns their intents;
- `Renderer::pick_input(&InputEvent) -> Option<(SurfaceId,
  Option<NodeId>)>`, fed the pointer events the router held back: the
  node under the pointer is outlined on whichever surface it is over
  (in place of the highlight while picking), and a left press answers
  the surface and the node picked; `Renderer::pick_ended()` drops that
  outline.

Highlight and flash draw on CPU and `GpuPresent` surfaces alike; they
are the only inspector UI the shell draws (m5-owner Q9: the panel is
`strand-dev inspect`'s, in a terminal). The router gains a pick-mode
hook that captures pointer events while picking. Pick mode needs no
keyboard and no surface changes its keyboard interactivity: the terminal
client ends it (Enter, or Escape then Enter, there sends
`inspect.pick{on:false}`; Ctrl-C ends the client, and its disconnect
ends pick mode), a click picks, and pick mode also ends on the IPC
client's disconnect, on the unmap of the surface picked on and when a
lock starts. Lock surfaces are never pickable, and `inspect.pick` is
refused while locked, as is `inspect.highlight` of a node. An outline
asked before the lock is hidden while it lasts (main clears it) and
drawn again after the unlock, still its client's.

As served (S-inspect, wave 3): `inspect.pick`, `inspect.highlight` and
`inspect.flash` are socket requests only (the D-Bus front end has no
methods for them) and each belongs to the client that asked: its
disconnect ends pick mode, the outline and the flash. One client picks
at a time; another taking pick mode ends the first's, which hears
`{"event": "pick_ended"}`, as it does when main ends it. A pick reaches
the client in pick mode as `{"event": "picked", "surface": n, "node":
"12.0" | null}` on its connection, between answers; pick mode stays on
until it is ended. An answer to `inspect.pick` means logic has told
main, not that main has turned it on. `inspect.node`'s answer gains
`layout`: main's `node_layout` on each surface showing the node, `[{
"surface", "x", "y", "width", "height" }]` (empty when none draws it),
through the snapshot hop; its `files` and `inspect.dump`'s hash every
file the program was compiled from, not only the nodes' own, so
write-back can check a token's defining file. (From the wave-3
sweep, decisions.md m5-integration-w3; accepted by the owner, m6-owner.) For a
`shader` node that draws nothing it also gains `why`, a string from the
GPU status main last sent logic (`ToLogic::GpuStatus`; no main hop):
`Unavailable { reason }` and `Starting` each say so, `Up` and `Unused`
add nothing; `strand-dev inspect`'s pane prints it under the title
line.

**Handler input context (`strand-compiler`, cross-crate; built by
S-platform in M5 wave 3, decisions.md m5-platform).**
- The VM records the input event that started a handler, `InputCtx {
  surface, serial }`, available to its actions until the handler's
  first `await`. Its route:
  - strand-surface tracks every button and key press's serial with its
    seat (`manager/seat.rs`, `State::note_press`) and tells the host
    before the press itself through a new defaulted hook,
    `SurfaceHost::press_serial(serial)`. The binary's host
    (`demo/host.rs`'s `Forward`) keeps the last one and joins it to the
    node events a press causes: a button's press or release, a key
    press (or repeat), a click away; none for scrolls, drags, key
    releases, settled intents and the demo. strand-render's router is
    unchanged (`Intent::Event` keeps its shape; the join is in the
    host, which already turns intents into `ToLogic`).
    `ToLogic::Event` gains `input: Option<InputCtx>`;
  - `Instance::event_with(node, event, args, input: Option<InputCtx>)`
    is added beside `Instance::event(node, event, args)`, which stays
    and passes `None`. The VM keeps `input` in the handler invocation's
    frame until the first `await` and numbers every handler invocation,
    with input or without, from one process-wide counter starting at 1
    (`ActionCtx::invocation`; process-wide, not per `Instance`, so
    numbers never repeat across a reload's new instance while an old
    invocation's actions still wait). The number stays in the frame for
    the invocation's whole run, across every `await`; only `input` is
    cleared at the first one. Invocation 0 is `ActionCtx::default()`,
    an action outside any handler (an IPC `call`, a test): it is
    unordered, queued behind nothing and nothing behind it. The host
    reads the running stretch's context through
    `strand_compiler::vm::action_ctx()`;
  - strand-services' `DynService::action` takes the `ActionCtx` as its
    last parameter (`strand_core::ActionCtx`). A service names the
    actions that want a token with `Service::ACTIVATION` (the
    `#[service(activation = &["invoke", "activate"])]` macro option)
    and reads a delivered action's token with
    `Cx::activation_token()`. The ordering queue
    (`strand_services::activation`, one for every service, keyed by
    invocation, since one invocation's actions may span services)
    waits on the services' shared runtime. It asks the main thread for
    a token through the `TokenSource` the binary hands
    `Services::set_token_source`: an `Arc<dyn Fn(u32, u32) ->
    TokenFuture>` (the surface's number, as in `InputCtx::surface`, and
    the serial), whose one-shot (`token_reply()`) the binary queues for
    main as a `run::TokenAsk` and wakes main with a `calloop::ping` (a
    queue rather than a calloop channel, for `.text`; decisions.md
    m5-platform); main calls
    `State::activation_token` and completes the one-shot (the services
    thread cannot call `SurfaceHost`, the manager's callback trait on
    main).
- `ServiceHost::action` takes an `ActionCtx` argument (the `InputCtx`,
  if any, and the handler invocation's id), as its last parameter:
  `ActionCtx { input: Option<InputCtx>, invocation: u64 }`, `InputCtx {
  surface: u32, serial: u32 }` (`surface` is the `strand_scene::SurfaceId`'s
  number), defined in `strand_core::action` and re-exported from
  `strand_compiler::vm::host`, so the VM and strand-services name one
  type without strand-services depending on strand-compiler or
  strand-scene (decisions.md m5-w0-harden). The binary's services
  adapter passes it to `DynService::action`.
- `State::activation_token(surface, serial, done: impl FnOnce(Option<String>))`
  on the main thread (`strand-surface/src/activation.rs`): a callback,
  not an `async fn`, since main has no executor. It sets the press's
  serial with the seat it came from, and the surface while it is still
  mapped. Without `xdg_activation_v1`, `done(None)` runs before it
  returns; a token never answered is dropped (`done(None)`, its object
  destroyed) after 5 s by a calloop timer armed when it is asked.
- **Launches.** A launched program gets the press's token in
  `XDG_ACTIVATION_TOKEN` and `DESKTOP_STARTUP_ID`
  (`strand_services::apps::spawn_launched`), and never the token strand
  inherited: every program strand starts (launches, `exec` sources, the
  editor, a `file` element's `run:` command, a declared action's
  command) goes through `strand_services::child::child_env`, which
  removes both on the `Command` before `fork`, and with them the
  variables that address this shell (`STRAND_SOCKET`,
  `STRAND_STATE_DIR`; launch-env).
- **Ordering rule.** An action that needs a token (notification
  `invoke` and `activate`, an app launch) waits up to 200 ms from when
  the handler sent it (a deadline, so several in one invocation wait
  together), then goes ahead without one (m5-owner Q14). Every later action from the same handler
  invocation queues behind it, whichever service it goes to and whether
  or not an `await` came between, so `n.invoke("default"); n.dismiss()`
  and `n.invoke("default"); await x; n.dismiss()` both send
  `ActivationToken`, `ActionInvoked`, then the close. The queue lives on
  the services side, one for all services; logic never blocks. A compositor
  without `xdg_activation_v1`, or one that never answers, means no
  token: never a wait beyond 200 ms, never a dropped or reordered
  action. An action whose service's run ended or was stopped while it
  waited goes back to the logic thread, which sends it to the run that
  is up then (started for it), as an action sent at once would be. The
  queue it goes back to is one for every service, in the order actions
  were handed back, and while it holds anything every action joins it
  at the back, any service's, whether it waited or was sent at once; so
  the actions after a handed-back one follow it there, even one that
  was handed the newer run or goes to another service, and none
  overtakes it. `Services::pump` sends the queue on after the services'
  envelopes, and `act_with` before its own action. An action sent at
  once (no wait) to a run whose body has gone, with nothing handed
  back, is the caller's error, as before tokens.

**`strand-compiler` `Suggestion`** gains `title: Option<String>` (the
code action's title; `None` keeps today's "Change to `…`", the title
`strand-dev`'s quick fixes have used since M1), and a
zero-width span (an insertion) is allowed.

**`strand-dev` (M5).**
- `strand-dev inspect`: a terminal client over the socket, using the
  `strand_core::ipc` types. It runs pick mode and prints the node pane
  (identity, layout, the provenance chain, badges). It offers reset
  subtree (`reset{node}`), open in `$EDITOR`, and write-back: only a
  literal prop value at the node's own `origin()` span or a token's
  literal value at its definition, written only if the file's bytes
  still hash to `source_hash`, atomically, following symlinks; a
  read-only target is refused with the reason and an offer to open it
  in `$EDITOR` (m5-owner Q11).
  Built (wave 3) as `strand_dev::inspect` (`client`, `pane`, `write`):
  a line-mode terminal session (`p` pick, `r` reset, `e` edit, `set
  <prop> <value>`, `token <path> <value>`, `f` flash, `q`), and
  `write::{prop_edit, token_edit, write}`: a prop is found on the
  node's own element (the innermost element at its `origin` span), a
  token in the `tokens <name>` block or component `tokens { }` its
  provenance step's origin names (a palette's, the built-in theme's or
  an overlay's is refused); the value replaced and the value written
  must each be one literal (a number with its unit, a string, a colour,
  a boolean); the write is a temporary file beside the symlink's
  target, swapped with it by `renameat2(RENAME_EXCHANGE)` (M6,
  m6-edges): the file the exchange took out of place must be the one
  whose bytes were hashed (same inode, same hash), else the two are
  swapped back and the edit refused, so an editor's save after the
  check (renamed into place or written in place) is kept and nothing
  checked before the swap is trusted after it; saved again before the
  swap back, the latest save is put in place and the one before it kept
  at the write's own temporary name, which the refusal names; each
  exchange of the swap back is checked by what it brings out (inode and
  text), never by what was found before it (m6-edges review round 2).
  A file missing at the exchange (`ENOENT`: an editor moving it to a
  backup before writing it anew) is refused as `Changed`. Where the
  filesystem has no exchange (`EINVAL`, `ENOSYS`) the target is checked
  (inode, hash) just before a plain rename, as before M6.
  `write::write_with(edit, Swap, hook)` is the test hook
  (`Swap::{Auto, Exchange, Rename}`; `hook(Moment, &Path)` at
  `Moment::{Before, Window, Back, Again, Missing}`: before the swap,
  after the last look at the file before it is replaced, before a swap
  back, before each further exchange that puts a later save in place,
  and after an exchange that found no file, before its refusal;
  `Again` added in review round 2 so a test can save in that gap,
  `Missing` in round 3 so a test can write the file anew there:
  `ENOENT` alone decides that refusal, never a later look).
- `strand-dev reference [dir]`: generates `docs/reference/` from the
  builtin and service schemas (`Schema::builtin_with` of every builtin
  service's text, never the plugin schema files, so the pages are the
  same on every machine) and their `///` docs: `README.md`,
  `elements.md`, `services.md`, `types.md`, `functions.md` and
  `tokens.md` (`strand_dev::reference::{schema, pages, write}`).
  `write` replaces or removes only pages that open with the generated
  note and refuses a directory holding any other `.md` file; with no
  directory named, the CLI needs `./docs/reference` to exist
  (decisions.md m5-docs3). The pages are committed; `reference::tests::the_reference_is_current`
  fails on any difference, so CI fails on a schema edit without its
  pages (built in M5 wave 3, m5-docs).
- **tree-sitter layout.** `crates/strand-dev/tree-sitter-strand/` is a
  separate package (`publish = false`): `grammar.js`, the committed
  generated `src/` (`src/grammar.json`, `src/parser.c`,
  `src/node-types.json` and `src/tree_sitter/`, whose `parser.h`
  `parser.c` includes; `tree-sitter generate` writes all four and
  `scripts/check-tree-sitter.sh` compares all four), an external scanner
  (`src/scanner.c`) that decides where SEP falls: which line breaks are
  SEP, and the zero-width SEP after an expression's closing `}` (rule 2;
  SAME_LINE follows from it; TOUCH is `token.immediate`, and the contextual
  keywords are tree-sitter's keyword extraction, aliased to `identifier`
  where a keyword may be a name: decisions.md m5-lsp),
  `queries/{highlights, locals, indents, injections}.scm` and
  `tree-sitter.json`; its own `build.rs` (`bindings/rust/`) compiles
  `parser.c` and `scanner.c` with `cc`, and it exposes the language as
  `LANGUAGE` (a `tree_sitter_language::LanguageFn`, which
  `tree_sitter::Language` takes) and the queries as strings. strand-dev uses it only as a
  dev-dependency (the conformance tests), so neither the runtime binary
  nor `strand-dev lsp` links it. The root `Cargo.toml` lists it in
  `members` explicitly, since `crates/*` does not reach it. A pinned
  prebuilt tree-sitter CLI (0.27.0, with its sha256) runs only in CI's
  lint job and the container image: `scripts/check-tree-sitter.sh`
  regenerates `src/` from `grammar.js` with `tree-sitter generate
  --js-runtime native` (the CLI's built-in QuickJS, so no node) and
  fails on any difference in `parser.c`, `grammar.json`,
  `node-types.json` or `tree_sitter/`. (Changed in m5-infra from
  "`parser.c` against `grammar.json`": the spike showed the prebuilt CLI
  evaluates `grammar.js` itself, and checking from `grammar.js` also
  catches a stale `grammar.json`; decisions.md m5-infra.)

**Plugins: `strand` gains a `[lib]` target (new).** `main.rs`'s module
tree moves under `lib.rs`, and `main.rs` kept `main` and the COMMANDS
table until the wave 1 → 2 boundary, when both moved into `src/entry.rs`
(accepted 2026-10-11, moved in m5-integration-w2; "Plugin builder" below) (0b; planned within 1 KB of `.text`, measured at +12,544 B
CPU-only and +8,512 B GPU with the request channel, and accepted and
charged to the ledger: decisions.md m5-core, m5-integration). The library
exposes `strand::Shell::builder().service::<S>().run()`: a plugin is a
user binary that links `strand` and registers its own `#[service]`
types beside the builtins (`Builtin::register` is unchanged, and the
composite host routes the extra names). It is git-only and marked
unstable. A plugin binary writes its services' schemas as `.schema`
files into `$XDG_DATA_HOME/strand/schemas/` (`--write-schemas`), outside
the watched config directory, where `strand check`, `strand run` and
the LSP read them (`strand run` added, accepted 2026-10-11: one schema
for all three, "Plugin builder" below); no dylib is loaded (m5-owner Q18). A plugin binary must copy the
workspace's `[profile.release]` and its package overrides, and invoke
`strand::process_setup!()` once at its crate root (mimalloc as the
global allocator, and THP off from an `.init_array` constructor before
the first allocation), or the size and PSS figures do not hold: a
library's `#[used]` constructor is not guaranteed to reach the binary
that links it, so the library cannot do it alone (decisions.md
m5-w0-harden). The example is `[[example]] plugin_hello`
in the strand crate, never in the release binary's `.text`. The binary
reaches the library through a hidden `strand::cli` module (its entry
points: `check`, `fmt`, the IPC commands, `compositor_rules`, `demo`,
`run`, `trim`, `LogConfig`, `default_dir`, `export_cli`, `new_cli` and
`report_cli`; `entry.rs`'s
command line is reached only through `Builder::run`, which `main.rs`
calls as `Shell::builder().run()`); the module tree stays
private. `main.rs` invokes `strand::process_setup!()`, and so do
`lib.rs` under `cfg(test)` (so the unit tests run as the binary does)
and `tests/m5/process_setup.rs` (a linking binary, as a plugin is).

**Plugin builder (S-plugins; wave 1 notes, accepted by the integrator
2026-10-11, built in wave 2).** The reasons are in decisions.md
`## m5-plugins`. Two parts are interface changes the integrator
accepted: `main.rs`'s dispatch moving into the library, and the stock
`strand` reading the plugin schema files.
- **Public surface, all of it.** `strand::Shell` (a unit struct),
  `Shell::builder() -> Builder`, `Builder::service::<S:
  strand_services::Service>(self) -> Self` and `Builder::run(self) ->
  std::process::ExitCode`, beside the existing `process_setup!` and the
  hidden `cli` and `__process`. Errors are not public types: `run`
  prints them and fails. A plugin depends on `strand` and
  `strand-services` from the same git URL and rev (the `#[service]`
  and `Store` derives emit `::strand_services` paths); `strand` does not
  re-export it.
- **One CLI (accepted).** `Builder::run` is the whole `strand` command
  line: `main.rs`'s `COMMANDS`, `dispatch` and `main` body (idle hooks
  included) move into a library module, `src/entry.rs`, and `main.rs`
  becomes `strand::process_setup!(); fn main() -> ExitCode {
  strand::Shell::builder().run() }` with its allocator and THP tests.
  (Moved in m5-integration-w2; S-plugins then replaced `main.rs`'s
  interim `strand::cli::main()` with the builder's `run`, which calls
  `entry::main`, and `cli` no longer re-exports it. `strand::Builder` is
  exported beside `strand::Shell`. `--write-schemas` is handled by one
  call in `entry::main` before the other commands and is not a COMMANDS
  row.) A
  plugin binary therefore answers `run`, `check`, `fmt`, `call` and the
  rest exactly as `strand` does, against its own schema. This replaces
  0a's "`main.rs` keeps `main` and the COMMANDS table". The move is one
  pure-move commit at the wave 1 → 2 boundary, after S-ipc's CLI arms
  merge and before S-tools wires `new`, `export` and `report`; S-plugins
  writes it and the integrator merges it. Ownership after the move:
  `entry.rs`'s COMMANDS table and dispatch arms are a shared dispatch
  point, like `Shell::request`'s arms; S-ipc keeps the COMMANDS table
  and its own rows (the D-Bus front end is `strand-services`'
  `ipc_bus.rs`, not this table), and S-tools changes the three rows
  that already name its commands (`new`, `export`, `report`, which
  answered "not implemented yet (M5)" until S-tools served them; once
  every row was served, S-tools' wave-3 review removed that fallback and
  the table's milestone column, so an unknown command is the only
  error) and adds their dispatch arms,
  each as one-line entries the integrator merges. S-plugins owns the builder in
  `src/plugin.rs` (registration, `--write-schemas`, the setup check)
  and the single call from `entry.rs` into it, not the table.
- **Registration.** `service::<S>()` appends a private `Registration {
  name: S::NAME, schema: S::schema, register: fn(&Services, &Runtime) ->
  Rc<dyn DynService> }` (function pointers, monomorphised only in a
  plugin binary). `run` checks the list (below), then stores it once in
  a private process-wide `OnceLock` (`plugin::extras()`, empty in
  `strand`), before any subcommand, through `plugin::install(slot,
  list)`, which takes the `OnceLock` as a parameter and refuses one
  already set. `services::schema()` and `Real::start` read it, each
  through a pure twin that takes the list instead:
  `services::schema_with(extras, files) -> Served` builds the served
  schema and says which services the files declare that no linked
  store serves: `Served { schema: Schema, unserved: Vec<(String,
  PathBuf)>, warnings: Vec<String> }`, each unserved name with the path
  of the file that declared it, and the warnings (files skipped by
  `load` and `apply`, a linked service's stale file) that `served()`
  logs at warn and `strand check` prints as `warning: plugin schema: …`
  (decisions.md m5-plugins, "built"). `services::served()` is the process-wide `OnceLock`
  over it with `plugin::extras()` and the files `plugins::load` found,
  and `schema()` is `&served().schema` (its signature and callers are
  unchanged). `Real::start_with(rt, types, buses, fallback, wake,
  extras, served: &'static Served)` is `Real::start` with both passed
  in (`Real::start` passes `plugin::extras()` and `served()`; its
  signature and callers are unchanged), and `Real` keeps `served`:
  `start_with` adds the `UnservedHost`s from `served.unserved` (below),
  and `Real::set_text` resolves the service, its fields and their
  types in `served.schema`, through `services::set_text_with(schema,
  host, rt, types, path, text)`, the pure twin of the free `set_text`
  (which is `set_text_with(schema(), …)`, unchanged for its callers).
  Unit tests use the twins, a local `OnceLock`, and a `Served` built
  by `schema_with` from a test-defined extra and files in a temporary
  directory (leaked to `'static`, as a test may), so none of them sets
  or reads the process-wide lists that the shared lib test binary's
  other tests already initialised. `Real::start_with` registers the extras after
  `Builtin::register` (unchanged) and adds each to the `Composite` as a
  `StoreHost`, as it does the builtins, so reads, writes, events and the
  D-Bus front end reach them through the existing name routing, and
  `shutdown` disposes them with the other stores. Two paths look a
  service up by name to start it and wait for its first read, and both
  go through one new lookup, `Real::served(name) -> Option<Rc<dyn
  DynService>>` (the builtins, then the extras): `Real::set_text`
  (`strand set`) and S-ipc's `Shell::run_action` (`strand call`,
  `run/request/call.rs`), which today search `builtin.all()` alone and
  would skip the acquire and the `wait_ready_of(SET_WAIT)` for a plugin
  service. For `set` the miss is a wrong value: a relative write
  (`+1`) on a service nobody reads would be computed from the schema
  default before the first read, and the first read would then
  overwrite it. For `call` it is not observable from outside: an action
  is fire-and-forget (`Client::act` answers once `Msg::Action` is
  queued, and `with_run` starts a stopped service itself, so the body
  takes the action after its start either way), and the lookup only
  makes `call` return after the first read, as `set` does. So the
  regression tests are `Real::served`'s unit test and `set`'s, not a
  `strand call`. The `run_action` change is one line in S-ipc's file, landed
  with S-plugins' wave-2 PR as a one-line entry the integrator merges
  (S-ipc reviews). A plugin service runs on the shared services
  runtime, or its own thread with `thread`, as a builtin does.
- **The served schema (accepted: the stock `strand` reads the files)**
  (`services::schema()`, `strand-dev`'s `schema()`): the builtin texts,
  then each linked plugin text in `service` order, then the plugin
  schema files in file-name order, skipping a file whose stem names a
  linked service (code wins over its file). Each text goes through
  `Schema::extend`, which is atomic: a file that does not apply is
  skipped with a warning naming it, never fatal. The fingerprint chains
  every applied text, so the compile cache follows. `strand` reads the
  files too, so `strand check`, `strand run` and the LSP check against
  one schema as before. A service a file declares that no linked store
  serves answers from the `SchemaHost` fallback at its defaults, behind
  a thin strand-local `UnservedHost` that `Real::start_with` adds to the
  `Composite` under that name, one for each entry of the `Served`
  it is given (`served.unserved`, so the notice's path comes from
  `schema_with`, never from a second read of the directory). Reads go
  to the fallback; everything
  that would reach the service itself (`write`, `write_item`, `call`,
  `action`) is refused with the notice's text below, so `strand set`
  and `strand call` on such a name answer with an error naming the
  file rather than an `ok` that did nothing (`SchemaHost::action`
  returns `Ok` without acting, and its `write` changes only defaults).
  Its first `acquire` (the config reads the service) raises one
  `ServiceDiagnostic { notice: true, resolved: false }` through
  `Services::report`, which `Shell::service_diagnostics` takes with the
  services' own, and the last `release` (a reload stops reading it)
  reports the same diagnostic with `resolved: true`, which takes the
  overlay rows away; so a leftover file that no config reads raises
  nothing (only a log line at start). The notice reads: "`hello` is
  declared by `<path>` but this binary does not serve it; run the
  plugin binary that wrote it, or remove the file if that plugin is
  gone". No strand-services change. Like every service notice, it is
  broadcast to the watchers connected when it is taken: one raised by
  the boot's mount is in the log and on the overlay, and `watch_request`
  does not replay it (it replays warnings and host notices only, and
  that is S-ipc's file), so its test raises it with a reload after
  `strand watch` connects. A linked
  service whose file differs from its text logs "stale; run `<binary>
  --write-schemas`".
- **Schema files** (`strand_compiler::schema::plugins`, a new file
  `strand-compiler/src/schema/plugins.rs`, std only, owned by S-plugins
  in m5-plan's strand-compiler list; strand-dev links it through
  strand-compiler): `dir()` is `$XDG_DATA_HOME/strand/schemas/`
  (absolute only, else `$HOME/.local/share/strand/schemas/`; neither: no
  plugin schemas); `load(dir) -> (Vec<(PathBuf, String)>, Vec<Warning>)`
  reads `*.schema` but dotfiles, regular files only (symlinks followed;
  FIFOs and devices refused), at most 1 MiB each, UTF-8; a missing
  directory is no plugins; `apply(&mut Schema, files, linked: &[&str])
  -> Vec<Warning>` is the skip-and-warn loop both consumers call;
  `writer(text) -> Option<&str>` reads a file's header; and (flex
  review) `beyond_its_service(schema, next, own) -> Vec<String>` names
  what a text adds beyond its own service (another service, an
  element, group, function, value, method or token), which `apply`
  skips a file for and the plugin builder's `layered` refuses a
  registration for (`PluginError::Schema`). Files are read
  once per process; a running shell or LSP sees new files after a
  restart.
- **`--write-schemas`** (a top-level flag, like `--version`): for each
  registered service, checks that its text applies over the builtin and
  earlier plugin texts and that the resulting entries are documented,
  then writes `<name>.schema` (a header line `// strand plugin schema,
  written by <exe> --write-schemas; do not edit`, then `S::schema()`
  verbatim) to a temporary `.<name>.schema.tmp` and renames it into
  `dir()` (created as needed). `<exe>` is the writing binary's
  identity: `std::env::current_exe()` canonicalised (symlinks resolved),
  printed in full; never `argv[0]` or a package name, so two projects
  that both build a `my-shell` do not share one. It then removes the
  `.schema` files whose header names this exact path and whose service
  it no longer registers, and lists, without touching, the files another
  path wrote that declare a name it does not register ("left alone:
  `<file>` (written by `<exe>`)"), so a dropped service's file from a
  `target/` build is shown to the user rather than guessed at. If
  `current_exe()` fails, it writes the header with `unknown` and removes
  nothing. It prints each path written or removed, and exits. It takes
  no directory argument (tests set `XDG_DATA_HOME`). `strand
  --write-schemas` writes nothing and says the binary registers no
  plugin services.
- **Errors** (`plugin::PluginError`, private, printed as `<binary>:
  <error>` with a failing exit): `DuplicateService(name)` (a builtin's
  name, or registered twice), `BadName(name)` (not `[a-z_][a-z0-9_]*`),
  `Schema { service, errors }` (its text does not apply),
  `Undocumented { service, entries }` (`Schema::undocumented()` of its
  entries; `--write-schemas` only), `NoDataDir`, `Write { path, error }`.
  The first three fail every subcommand before it runs: `run` checks
  the names, then that each linked text applies over the builtin and
  the earlier linked texts (files are not involved, so a bad file
  never stops a plugin), so a plugin never boots on a text that
  `services::schema()` would log and fall back from.
- **Process setup check.** `run` checks, once, that a heap block is
  mimalloc's and that THP is off for the process; if not it prints a
  warning naming `strand::process_setup!()` and goes on. The decision
  is a pure `plugin::setup_warning(mimalloc: bool, thp_off: bool) ->
  Option<&str>` over the two probes, so its polarity is unit-tested on
  all four inputs; the binary tests spawn their children with THP
  turned back on (`PR_SET_THP_DISABLE` is inherited across fork and
  exec, and the m5 test binary turns it off itself), so a child that
  reports THP off turned it off itself.
- **Examples.** `crates/strand/examples/plugin_hello.rs` (`[[example]]
  plugin_hello`): `strand::process_setup!()` at its root, a `hello`
  service (`greeting: text`, `count: int`, writable, which the first
  read sets to 41 as a stand-in for a value a real service reads from
  its source; `action greet(name: text)` sets `greeting` to `hello,
  <name>` and bumps `count`; each documented). `fn main()
  { strand::Shell::builder().service::<Hello>().run() }`.
  `crates/strand/examples/plugin_bare.rs` is the same `main` without
  `process_setup!()` and without services, the case the setup check
  exists for. Both are built by `cargo test` and never linked into the
  `strand` binary.

**Ownership.** Integrator: this section, the root `Cargo.toml` and
`Cargo.lock`, CI files, budgets.rs's gate constants and
`crates/strand/tests/m5.rs`'s module list.
- **Landed as inert code in 0b (S-core) and the wave-0 hardening:**
  `strand_core::ipc` (types, codec, `IpcRequests`, `EventSink`,
  `socket_path`, `DBUS_NAME`), the `ipc/` split, `Shell::request`'s
  dispatch with a module per command, the lib/bin split and
  `strand::cli`, `ActionCtx`/`InputCtx` threaded as
  `ActionCtx::default()`, the `Renderer` inspect and `frame_summary`
  stubs, the `ToMain`/`ToLogic` snapshot and inspector messages,
  `OVERLAY_ORIGIN`, `Suggestion::title` and `tests/m5.rs`.
- **Landed by their streams, not in 0b:** `Instance::identity`,
  `cells_of`, `reset_subtree`, `source_hash`, `inspect_node`, `dump`,
  `inspect::node_ref`/`parse_node_ref` and `TokenTable::provenance`
  (S-inspect, wave 1), the token overlay layer and the `get`,
  `toggle`, `call` and `clear` commands (S-ipc, wave 1);
  `activation_token` and the input context's route (S-platform, wave
  2), `Shell::builder` (S-plugins, wave 2), and the theming hub's
  pieces (S-palette and S-tools, wave 2; "The theming hub" names each
  owner). Each then belongs to the stream
  `docs/m5-plan.md`'s "Streams" table names.
- **Shared dispatch points, merged by the integrator:** each stream adds
  only its own one-line entries to `strand_core::ipc::parse`'s and
  `parse_m5`'s matches, `Shell::request`'s match
  (`run/request/mod.rs`), `ToMain`/`ToLogic` and `Shell::handle`
  (`run/mod.rs`, `run/shell.rs`), `Live`'s fields, `lib.rs`'s `mod`
  list and `strand::cli` re-exports, `ipc/mod.rs`'s re-exports, and,
  `src/entry.rs`'s COMMANDS rows and dispatch arms (the CLI table,
  moved there from `main.rs` at the wave 1 → 2 boundary); the
  integrator merges them.

## Glitch reports (decisions.md report)

design.md "Reporting a glitch" as interfaces, built; features.md
"Glitch reports" lists the boxes and the tests that prove them. No
crate gains a dependency on another: the record
format lives in `strand-core`, which the binary and `eval/` already
depend on, and every hook below is a callback the binary installs.

**Two rings, one per thread.** The main thread records what reaches it
(input, surfaces, outputs, frames that came late); the logic thread
records what reaches it (service changes, the config's state writes,
calls, actions, reloads, surface roots). Each thread owns its ring
outright, so nothing is shared or locked and nothing crosses a channel
until a report asks; the two are merged by time when a report is
converted. Both stamp records from one process-wide epoch
(`flight::epoch()`, an `Instant` taken once), so their times compare.

| Ring | Records | Names | Arena | Bytes |
| --- | --- | --- | --- | --- |
| Input (main) | 6,144 × 24 B | 64 × 64 B | 4,096 B | 155,648 |
| Changes (logic) | 1,024 × 24 B | 256 × 64 B | 65,536 B | 106,496 |
| **Both** | | | | **262,144** (256 KiB) |

Each is allocated once, zeroed, when `strand run` starts, and
never grows or allocates again while it records; a record is
written on a path that already runs (an input event's routing, a
service's pump, a cell write, a surface callback), so no timer, wakeup
or flush exists. At the input ring's rate the window is about 100 s of
pointer motion at 60 Hz and 42 s at 144 Hz, longer when the pointer
rests (a real session of the owner's wrote about 690 records a
minute: nine minutes). (replayfix) The changes ring's arena is what a
busy shell fills first: 64 KiB hold about 57 s of `report.rs`'s busy
session (four workspaces of windows, a press on a chip every 1.7 s, a
title changing every 400 ms, a clock, a player, notifications) at its
69 KB a minute, and 24 KiB of input records paid for it (decisions.md
replayfix, 2026-10-21).

**`strand_core::flight` (new): types and codec only, no I/O, no
threads** (as `strand_core::ipc`).
- `epoch() -> Instant`, `now_ns() -> u64` (ns since the epoch);
  `epoch_unix_ns() -> i64` (the wall time the epoch was taken at).
- `RingSpec { records, names, arena }` with `INPUT` and `CHANGES` the
  table's sizes; `Ring::new(RingSpec)`; `push(Rec)`; `merge_motion(slot,
  t, x, y, period_ns)` and `merge_text(slot, t, password)` (the
  coalescing rules below, falling back to a push); `name(&str) -> u16`
  (interned once; `NAME_IN_ARENA` when longer than 63 bytes or the
  table is full), `find_name`, `name_at`; `value(|w: &mut ArenaWriter|
  …) -> ARef` (a value written straight into the arena by a bounded
  writer, so nothing is allocated; past `VALUE_CAP`, 8,192 bytes
  (replayfix; 2,048 before), or the arena's size if smaller, it is
  dropped and the record is marked truncated); (fidelity)
  `named_value(in_arena: Option<&str>, write) -> ARef` (`value` with
  the name at its head, ended by a 0 byte; a value past the cap keeps
  the name: the name alone is written, the `ARef` marked truncated with
  the name's length, so the reader still names the change);
  `push_named(t, kind, name, inline, flags, value)` (a
  change record: the name interned, or written at the head of the
  value ended by a 0 byte when it cannot be, through `named_value`); `pause(t, gap_kind,
  why)` and `resume(t, gap_kind, why)` (a gap's two records, nothing
  appended between); `newest()`; `clear()`; `dump(&self, now_ns) ->
  Vec<u8>` (the only allocation, made when a report asks); `read(&[u8])
  -> Result<Dump, FlightError>` (a newer version is refused, naming
  it); `Dump::entries()` oldest first with each name and arena value
  resolved or marked lost. `json_text`, `json_held`, `json_number` and
  `json_color` write the value encoding below into any `fmt::Write`.
- **Records** are 24 bytes, little-endian: `t` u64 (ns since the
  epoch), `kind` u8, `flags` u8 (1 truncated, 2 redacted, 4 lost, set
  by the reader, 8 a service's boot value, (services) 16 a change the
  config's own write made, `LOCAL`), `slot` u16 (a surface or
  output slot in the input ring, a name index in the changes ring),
  then 12 bytes of payload by kind. An arena reference (`ARef`) is an
  absolute offset u32 and a length u16 at payload bytes 6..12 (a value
  is at most `VALUE_CAP`, so 16 bits hold it, and the first six bytes
  stay free for a kind's inline fields); the kinds with one are field,
  keyed, event, cell, fetch, write, reload and request, and a key or action
  record whose name is `NAME_IN_ARENA`. The arena is a byte ring, so a
  value is readable while its offset is within the arena's size of the
  newest byte written, and lost after. (replayfix) A ring keeps no
  change record whose value is lost: writing a value evicts, oldest
  first, every record up to the newest one whose value it lapped
  (counted in the dump's records evicted), so a dump of version 2 holds
  a suffix of what was recorded with every value in it. The input ring's
  arena holds only long key names, whose records stay, marked lost.
- **Input kinds.** `0x01` enter (x f32, y f32, surface-local logical
  px), `0x02` leave, `0x03` motion (x, y), `0x04` button (x, y, button
  u16 as evdev minus `BTN_MOUSE`, pressed u8), `0x05` hit (the innermost
  node the press resolved to, `NodeId` as u64), `0x06` axis (dx f32, dy
  f32 in px, source u8, stop u8, discrete i16 in 120ths of a notch),
  `0x07` keyboard enter, `0x08` keyboard leave, `0x09` key (name index
  u16, modifiers u16: shift, ctrl, alt, super; state u8:
  release, press, repeat), `0x0A` text (count u32, last u32 in ms after
  `t`, password u8), `0x0B` paste (count u32, password u8), `0x0C`
  click-away, `0x0D` drag (phase u8: enter, motion, leave, drop; x, y;
  the kinds offered as a u16 mask; never the payload), `0x20` surface
  mapped (slot = a new surface slot; root `NodeId` u64, output slot u8,
  255 when it has none yet, kind u8: bar, panel, osd, popup, other,
  and (window) 5 for a `window`, `flight::MAPPED_WINDOW`; its
  namespace is in `bundle.json`'s `surfaces` by slot, since a name per
  surface would crowd the key names out of the input ring's 64), `0x21`
  configured (the buffer's width u16 and height u16 in physical px,
  scale u16 in 120ths), `0x22`
  placed (x i32, y i32 on its output), `0x23` unmapped, `0x24` backend
  (gpu u8), `0x25` late (frames missed u16: a presented frame that came
  two or more refresh periods after the one before while the surface
  was animating), `0x26` refresh (period ns u32, when first known or
  changed), (window) `0x27` toplevel (a `window`'s configure, written
  by `InputRecorder::window` from `SurfaceHost::window_state`: width
  u16 and height u16 in logical px, the state bits u8
  `flight::toplevel::{SIZED_W, SIZED_H, MAXIMIZED, FULLSCREEN, TILED,
  ACTIVATED, DECORATED}`, the capabilities u8 `{CAN_MAXIMIZE,
  CAN_FULLSCREEN, CAN_MINIMIZE, WINDOW_MENU}`, the configure bounds'
  width u16 and height u16, 0 for none; written again for every open
  window at a gap's end and a ring's restart, as a surface's map is;
  a window's `0x0C` click away has payload byte 0
  `flight::click_away::COMPOSITOR` when the compositor closed it,
  none for `w.close()`),
  `0x30` output (slot = an output slot; connector name u16,
  width u16, height u16, scale u16 in 120ths, x i16, y i16), `0x31`
  output gone, `0x3E` gap begins (why u8: lock, off, inspector pick,
  and (services-round) `flight::gap::SECRET`, a password dialog or
  login screen),
  `0x3F` gap ends. `0x10`–`0x12` are kept for touch, which
  strand-surface does not deliver yet.
- **Change kinds** (slot = the name's index). `0x40` field (name
  `service.field`; arena `[old, new]`, or (replayfix) `{"e": [[path,
  old, new], …]}`, what changed in it, when that is shorter: a path
  names a record's field and a list's item by index, `windows.1.title`,
  and `""` never appears; a list of another length, and a value of
  another type or record type, is replaced whole at its path), `0x41`
  keyed (arena `{"d": diffs, "r": the items those diffs removed or
  replaced}`; (replayfix) an update whose edits are shorter than its
  item is `{"patch": [index, key, edits]}` and puts nothing in `r`, an
  update to an equal item is left out, and a change of only those is
  not recorded), `0x42`
  event (`service.event`; arena: its arguments), `0x43` cell (name: the
  cell's path as `Instance::cells_of` spells it; arena `[old, new]`),
  `0x44` fetch (`service.method`; arena `{"ok": answer}` or `{"err":
  text}`; a call id u32; never the arguments), `0x45` action (the
  action's name; exit i32: 0 it succeeded, 1 it failed or never
  started, 0 on its start record; argument count u8, started u8),
  `0x46` write (an `rw` service field the config wrote; arena: the
  value; (services) the change it made to the field, or to an item of a
  keyed list, is a field or keyed record flagged `LOCAL` at the same
  time, its old value what the write replaced: a local write is applied
  at once and its echo is never a patch, so without it a service's
  answer after a write was recorded as edits from a value the field
  never held), `0x47` reload (name `reload`; the module files, the cells
  kept and the cells reset, u16 each; arena `{file: blake3}`),
  `0x48` surface root (name: its origin `file.strand:LINE:COL`, the
  file under the config directory as `nodes.json` names it; the
  root `NodeId` u64, kind u8; (replayfix) a root with no place in the
  config, Strand's own error panel, or one whose origin the name table
  cannot hold, has slot `NAME_IN_ARENA` and no arena value, so no name:
  never a placeholder), `0x49` root gone (`NodeId`), (report
  review) `0x4A` request (a cell written by `strand set`, `toggle` or
  `reset`: name the path as asked; verb u8, `flight::request::{SET,
  TOGGLE, RESET}`; arena: the cell's value after it, held back as a
  cell's), `0x4E` gap begins, `0x4F` gap ends.
- **Values** in the arena are compact JSON typed by the schema or the
  program (records as objects, lists as arrays, enums by variant name,
  durations in ms, colours as `#rrggbbaa`). A text the redaction rules
  below hold back is written `{"~text": N}`, N its length in
  characters.
- **Coalescing.** A motion merges into the newest record when that is
  a motion of the same surface written less than one refresh period
  ago (the surface's last `0x26`, else 1/60 s), keeping its time and
  taking the new position: at most one motion per frame interval per
  surface, with no frame hook. A text key merges into the newest record
  when that is text on the same surface: the count grows and `last`
  moves.
- **The dump** (`input.flight`, `changes.flight`): a 64-byte header
  (magic `STRNDFLT`, version u16 = 4 (replayfix, version 2: values
  may be edits and patches, and no record's value is lost; (services)
  version 3: a config's write to a service field is recorded as the
  change it made, `LOCAL`; (window) version 4: a window's mapped kind
  and its `0x27` toplevel records; versions 1 to 3 still read, and
  `Dump::version` says which was read),
  ring u8, flags u8 (1 wrapped, 2
  paused), record size u16, record count u32, name count u32, arena
  length u32, the arena's first absolute offset u64, the dump's
  `now_ns` u64, `epoch_unix_ns` i64, records evicted u64), then the
  names (64 bytes each: a length byte and UTF-8), the arena bytes, and
  the records oldest first.

**Main side** (the binary's `run/record.rs`, `InputRecorder`, a field
of `demo::host::Host`).
- Input is recorded in `Host::intents` for `Cause::Input(event)`,
  after routing, so the recorder sees exactly what the router saw.
  Pointer events record their surface-local position; a press is
  followed by its `hit` (the innermost node of `Renderer::hit`'s
  chain). (report review) The hit also goes to logic as
  `ToLogic::Hit(NodeId)` (new), sent before the press's events, and
  `ChangeRecorder` keeps the identity the node has then (its source
  place and `Instance::identity`) for the newest 256 presses, so
  `nodes.json` names a node the press removed (a menu row that closes
  its menu). They are dropped with the ring.
- **Typed text is never recorded.** A key whose `KeyInput::text` is
  not empty and that carries no ctrl, alt or super is a text key: it is
  counted (`0x0A`), never named. While the focused node of the key's
  surface is a password input (`Router::focused_input(surface, tree)
  == Some(InputKind::Password)`, new; the router holds no tree, so the
  renderer's is passed) every key is counted as one
  password span with no count. A paste (`InputEvent::Paste`) records
  its character count (none into a password), never its text; a copy
  or cut records nothing. A drop records its kinds, never its payload.
  A named key's release is recorded only when its press was (the
  recorder holds the presses it named), so no release names a text
  key either; (report review) the presses held for a surface are
  forgotten when its keyboard focus leaves and when it unmaps (Wayland
  sends no release after a leave), and a text key's press forgets any
  held press of the same key.
- **The lock.** On `Host::lock_started` (a lock asked for) the ring
  writes a gap and appends nothing until `lock_changed` reports the
  session unlocked; an event on a lock surface (`lock::LockScreen`
  knows them) is never recorded, whatever the state. While the
  inspector picks, input is held from handlers and is not recorded
  either (a gap). (services-round) While logic's
  `Real::secret_shown` holds (a running `polkit` with a `request`, a
  running `greetd` that is `available`), logic sends main
  `ToMain::Secret(true)` (new) and both rings are in a
  `flight::gap::SECRET` gap until `ToMain::Secret(false)`; a lock
  inside it ends it and begins its own, and the glitch report is
  refused meanwhile, as under a lock.
- Surfaces and outputs are recorded from the `SurfaceHost` callbacks
  (`surface_attached`, `surface_configured`, `surface_placed`,
  `surface_detached`, `monitor_added`/`changed`/`removed`), refresh,
  lateness and backend from `Host::painted`'s presentation facts
  (`State::refresh(surface) -> Option<Duration>`, new, from the frame
  clock).

**Logic side** (`run/record.rs`, `ChangeRecorder`, owned by the logic
thread's `Shell` in an `Rc<RefCell<…>>` its taps share).
- **Services.** `strand_services::Services::tap(Tap)` (new) installs one
  callback the registry calls on the logic thread for every patch
  `Services::pump` applies: `TapEvent { service: &str, change:
  Change<'_> }`, `Change::Field { field, old: &Data, new: &Data,
  initial, local }` (the old value read from the cell before the patch, only
  while a tap is installed), `Change::Keyed { field, diffs:
  &[VecDiff<Data, Data>], removed: &[Data], initial, local }`, `Change::Event {
  event, args: &[Data] }`. The patches a refusal or an unavailable
  service resets fields with are tapped too; a patch that leaves a
  field as it was is not. An echo of a local write is not a patch, so
  it is never tapped. (services) A local write (`DynService::write`, a
  field or a leaf of one; `write_item`, an item of a keyed list) is
  told as it is applied, with `local: true`: the field's value before
  and after (a write leaving it as it was is not told), or the list's
  `Update` of the item with the item it replaced in `removed`; the
  recorder writes it as a field or keyed record flagged `LOCAL`. `Services::snapshot(rt)` reads every registered
  service's fields as `ServiceSnapshot { name, running, fields }`. `Data::write_json(&self, w, &mut dyn
  FnMut(&str) -> bool)` (new) writes a value, the closure deciding per
  text whether it is kept. Config services (`service … from`) run on
  the contract and are tapped like builtins; each is one keyed list of
  its values by field index (`strand_services::custom`), so the
  recorder writes each value a patch changes as a field record of its
  declared name (`mood.level`, its old value the last one recorded),
  and the snapshot names their fields the same way. (replayfix) A
  field the program never reads is not recorded: `Program::reads`'s
  chunks name each `(service, Some(field))` read and `(service, None)`
  for a service read whole (a method's call, `workspaces.slots(screen)`,
  or an action), and the recorder keeps a service's field changes only
  for those (`ServiceReads`, rebuilt on each reload). Events, a config
  service's values and `system` (the theme's mode, reduced motion and
  the contrast are read past the chunks) are always recorded; the
  snapshot keeps every field.
- **The config's writes.** `strand_compiler::Instance::set_cell_tap(
  Box<dyn FnMut(CellWrite<'_>)>)` (new): every write to a `state`
  cell, a settings field or a token overlay, by a handler, a two-way
  binding, IPC, a reset or a reload, as `CellWrite { path, old: &Value,
  new: &Value }`, told once per tick (the value before the tick and
  after; a cell written several times in a tick is told once, one left
  as it was not at all), read from the runtime's list of the cells the
  tick wrote, so a cell's write path pays nothing; a token overlay is
  told as `$path` with its text. `Program::is_text_literal(&str) ->
  bool` (new) holds every text constant of the program, built on its
  first use rather than at lowering, so a shell with no report pays
  nothing. (report review) Only the cells a tick wrote are read:
  each cell's core ids are indexed to its registry key as it mounts
  (`Ctx::cell_ids`), so a tick pays for its writes, not for every
  live cell. Left out entirely, of the tap and `cells_snapshot` alike
  (`Ctx::secret_cells`): every cell mounted under a `lock` surface (its
  own `state` and its components'), and every cell a password input's
  `text` or `value` is bound to two-way, marked when the input becomes
  a password one, when it writes, and as a snapshot or a tap is taken.
  `vm::json::{to_json, from_json}` (new) convert a `Value` by
  its type, `to_json` taking the same per-text closure; `from_json`
  reads a held-back text as `x` repeated to its length.
- **Calls and actions** are recorded where the binary already handles
  them: a `fetch` (an `Async` call) when its answer arrives
  (`services/adapter.rs`, `custom.rs`), a declared action when it
  starts and exits (`run/exec.rs`), an `rw` write (`composite.rs`).
  Synchronous calls the mock computes from state (`clock.format`,
  `workspaces.on`) are not recorded.
- **Redaction** (the changes ring and the snapshot alike). `auth` is
  never recorded or snapshotted, nor (services-round) `polkit` and
  `greetd`: `strand_services::secret(name)`, which `strand call`'s
  refusal and `history.json` use too. Every text of `clipboard` is held back
  (`{"~text": N}`), an image in its history as its size only. A text of
  the config's own (a cell's value, an `rw` write) is kept only when
  `Program::is_text_literal` holds; a token overlay (`$accent`), set
  from the command line rather than typed into the shell, is kept. Call and action arguments are
  never recorded (a count at most), and the answer of a call given a
  text that is not a literal has every text in it held back. Service
  values are kept as reported.
- Reloads are recorded in `Shell`'s commit (`run/shell.rs`), surface
  roots from the diff logic sends (a create of a `bar`, `panel`, `osd`,
  `popup`, `window` or `lock` root, its origin from
  `Instance::identity`; a `lock` root is never recorded).
  `ToLogic::LockState` and the lock
  asked for write a gap and pause the ring until the unlock, as on
  main.

**On and off.** `system.replay: bool rw` (strand-services-schema's
`system.schema`, new; default true; kept in the state directory as
`notifications.remember` is). The logic thread already holds a reader
of `system`; when `replay` changes it clears and pauses (or resumes)
its ring and sends main `ToMain::Recording(bool)` (new), and main does
the same with its own. Off drops both rings (their memory is freed,
so nothing recorded stays in the process) and on allocates them anew,
empty: the only allocations a ring makes after `new`. Both threads
allocate theirs as `strand run` starts; a shell kept off frees them as
soon as the `system` service's boot value says so. The logic thread's
taps are installed while recording is on and removed while it is off,
so an off shell reads no old value for them. Under `STRAND_MOCK`, the
mock's `system.replay` starts true. (report review) A ring made anew
by on starts with a gap's end (`gap::OFF`), then the outputs, mapped
surfaces and refresh periods (main) and the surface roots (logic)
shown then, written again, so a replay's window starts after the
recorder off and still knows them. Logic records each successful
`strand set` of a state or settings field or a token overlay, `toggle`
and `reset` as a request (`ChangeRecorder::request`; a service's write
is its patch).

**Saving a report.** IPC version 1's `report` gains an optional field:
`{"v": 1, "cmd": "report", "save": {"words": "…"}}`
(`Request::Report { save: Option<String> }` in `strand_core::ipc`).
Without `save` the answer is as before. With it:
1. Logic refuses while a lock is asked for or shown ("the screen is
   locked"). Otherwise it sends `ToMain::Snapshot { kind:
   SnapshotKind::Bundle, reply }`; main answers between frames, as for
   `Report`, with `Snapshot { reply, data, bundle:
   Some(Box<MainBundle>) }`: `data` is `report::main_part`;
   `MainBundle { ring: Option<Vec<u8>> (the dump, `None` while off),
   frames: Vec<Frame { slot, name, pixels: FramePixels, repainted }>,
   images: Vec<PathBuf>, surfaces: Vec<Json> }`, `surfaces` being
   `bundle.json`'s: each mapped surface's slot, namespace, kind, root
   `NodeId`, output, backend, logical and buffer size, scale, its
   frame's file and whether it was repainted, and (window) a window's
   `window`, what the compositor last said of it
   (`report::window_json`: `size`, `sized` per axis, `maximized`,
   `fullscreen`, `tiled`, `activated`, `decorated`, `bounds` and
   `can`), null for any other surface; bare `strand report` prints the
   same as a window's `window:` line. A frame is the newest
   committed shm buffer, copied (`strand_surface::State::frame_pixels(
   surface) -> Option<FramePixels>`, new), or, for a surface presented
   by the GPU, its current scene repainted by the CPU into a fresh
   buffer (`strand_render::Renderer::repaint_offscreen(surface) ->
   Option<FramePixels>`, new, the same full repaint eval's `redraw`
   compares against). `FramePixels { width, height, scale, stride,
   premultiplied RGBA8 }` is `strand-scene`'s (new). `images` are the
   files the renderer holds decodes of for mounted nodes
   (`Renderer::image_sources() -> strand_render::image::DrawnSources`,
   new: the image keys and the icon theme only, cheap on main;
   (report review) `DrawnSources::files()` resolves them, icon lookups
   and all, on the `strand-report` thread). No lock surface is in it.
   The CPU repaint of a GPU surface stays on main, where its scene is.
2. Logic adds its own part: `report::logic_part` (the diagnostics as
   today), its ring's dump, `Services::snapshot()` (new: each
   registered service's name, whether it runs, and every field's
   `Data`, under the redaction rules), `Instance::cells_snapshot() ->
   Vec<(String, Value)>` (new: every live cell by the inspector's path,
   settings fields and overlays included), the module set's files, the
   files the program reads (the ones the shell watches: its settings
   files and their overlays, the theme's wallpapers and imports, the
   local files its `image`s, `svg`s and `lottie`s draw; then the images
   the renderer holds, and the config's `icons/` folder), the strand
   version and commit (`env!("STRAND_COMMIT")`,
   set by the binary's `build.rs` from `git rev-parse HEAD`, `unknown`
   outside a checkout, `-dirty` when it was), the wall time and
   `now_ns`, and the words.
3. It hands that to a `strand-report` thread (see Threads) and keeps
   the caller under the snapshot's id. The thread writes the bundle,
   prunes, and stores its outcome in a shared slot and pings the logic
   loop, which answers `{"ok": true, "bundle": "<dir>", "paste":
   "<line>", "report": {…}}`, or the error.

**The bundle**: `strand_core::persist::state_dir()/reports/<YYYY-MM-DD_HH-MM-SS>/`
(local time; `-2`, `-3` appended when taken), mode 0700, files 0600:

| Path | What |
| --- | --- |
| `bundle.json` | `{"format": 1, "words", "wall": RFC 3339 with offset, "now_ns", "epoch_unix_ns", "strand": {"version", "commit"}, "config_dir", "outputs", "surfaces", "replay": bool, "files": {path: blake3}, "missing": [{path, bytes, why}]}`; `surfaces` as `MainBundle`'s, `outputs` the diagnostics' outputs (name, make, model, scale, size, position, refresh), each (replayfix) with its `screens.all` `id` and `description` and (fidelity) its `order`, its index in `screens.all`, `config_dir` the directory `config/` mirrors |
| `words.txt` | the words |
| `diagnostics.json`, `diagnostics.txt` | what `strand report --json` and `strand report` print (no serials or ids, as `report.rs` holds) |
| `input.flight`, `changes.flight` | the two dumps (absent with `system.replay` off) |
| `services.json` | `{service: {"running": bool, "fields": {field: value}}}` |
| `cells.json` | `{cell path: value}` |
| `history.json` | (history) `{"boot_ns", "series": [{"key", "span_ms", "points": [[at, value, ramp], …]}]}`: each series a `graph` keeps history of (`Instance::histories`; one reading `auth` left out; (graph-follow) a list graph's items each a series, `key[i]`, so a list of N items holds up to N × 8 KiB of points), its points oldest first with `at` on the boot clock in seconds and a `null` value for a gap's start, and the boot clock at the report (`boot_ns`, read with `now_ns`); (graph-follow) written with every report, `series` empty when nothing is kept (`history_json_holds_the_boot_clock_when_nothing_is_kept`). The replay seeds what each held before its window and runs on the live boot clock (its scenario's `boot`, docs/eval.md) |
| `drawn.json` | (replay-owner) `{"format": 2, "surfaces": [{"slot", "root": node_ref, "drawn_ns", "period_ns", "nodes": [{"ref": node_ref, "origin", "id", "key", "box": [x, y, w, h], "opacity", "travel", "fade"}]}]}`: each mapped surface's nodes as its last frame drew or hid them, in paint order, named as `nodes.json` names them, the box in surface logical px through every transform (axis aligned, to 1/64) and the node's own opacity, with how far it moves over the next frame period (`travel`: the largest edge move, px; `fade`: of the opacity; to 1/64, 0 at rest); `drawn_ns` is when that frame was drawn on the flight rings' clock (absent when not known) and `period_ns` the surface's frame period; main takes them with the frames (`Renderer::drawn_nodes`), logic names them; what the replay's verdict holds its own frame to |
| `nodes.json` | `{node_ref: {"origin": "file.strand:LINE:COL", "id", "key"}}`: each node the input ring names (a press's hit, a mapped surface's root) and each mapped surface's root, still mounted, the file under the config directory and the `for` row key (none for a per-screen surface's own screen), what a replay holds its own press's hit to |
| `frames/<n>-<namespace or popup>.png` | each mapped surface's last frame, at its buffer size |
| `config/` | the module set's `.strand` files at their paths under the config directory, its `icons/` folder |
| `files/<absolute path>` | every other file the program or the renderer read, at its own path (settings files, templates, shaders, imported palettes, wallpapers, images, the theme's icons drawn), each at most 2 MiB and at most 16 MiB in all, newest-referenced first; the rest are listed in `missing` |
| `replay/` | written later by `strand replay` (`strand-eval replay`) or `scripts/report-view.sh` |

(report review) The bundle is written into
`reports/.<stamp>-<n>.partial` and renamed to its name once
`bundle.json` is in it; on any error the partial directory is removed,
and pruning removes a `.partial` one older than 10 minutes (a crash's).
A file is copied under `files/` at its path's lexical normal form, so a
`..` never leads out of it; the config's `icons/` is read as
strand-icons reads it (the top level, links followed, `png` and
`svg`). Pruning keeps the 20 newest directories holding a
`bundle.json` and removes only those. The answer's `paste` line names the directory and
says what to run: `Strand glitch report "<words>" at <dir>: run
scripts/report-view.sh --no-open <dir> in the strand checkout and read
<dir>/replay/replay.txt. To watch it: strand replay <dir>` (replayfix:
the last sentence for the owner, who runs `strand replay` to watch it).

(replay-owner) `strand_render::Renderer::drawn_nodes(SurfaceId) ->
Option<DrawnFrame>` (`DrawnFrame { drawn_at: Option<Instant>, period:
Duration, nodes: Vec<DrawnNode> }`, `DrawnNode { node, rect:
LogicalRect, opacity, travel, fade }`), the interface the binary uses
for `drawn.json`: the surface's scene flattened again at the time of
its last paint, as a repaint that commits nothing does
(`repaint_offscreen`'s flatten), with each node drawn or hidden kept
with its box through its transforms and its own opacity, and flattened
once more a frame period later for each node's `travel` and `fade`
(motion is peeked, never advanced, so the next frame draws as it
would have); `drawn_at` is the wall time that paint stands for (the
surface's clock), `period` its frame period; `None` before a surface's
first layout. It runs only
when a report asks, on main between frames, so no frame pays for it
and no eval hook is in the shipped binary.

**The replay** is eval's (`eval/src/replay/`, `strand-eval replay
<bundle>`; docs/eval.md "Replaying a glitch report"). It reads the
dumps with `strand_core::flight::read` and values with
`strand_compiler::vm::json::from_json`, both crates it already depends
on, and writes a local scenario; `scripts/report-view.sh` runs it in
the container (`run.sh eval replay` mounts the bundle and its `files/`
tree read-only: `scripts/eval/replay-mounts.sh`) and copies the result
into `<bundle>/replay/`. The mock host it runs on
(`SchemaHost::mock_at`) takes the window start's wall time and offset
for `clock` and local time, and keeps a log of the fetches it made
(`SchemaHost::take_fetches`: whether a queued `answer` gave each),
which the replay lists as calls the mock answered. (fidelity) A
report from several outputs replays each as a screen of the sandbox
through the interfaces any eval run uses: the scenario's `screens` set
as `screens.all` (each output its own `Monitor` with its identity,
scale and place), a surface pinned by `Screens::Named` mapped on its
own output, and eval's own trace marking each surface's output
(`SurfaceInfo::output`) and scale (`Frame::scales`); no crate's
interface changes for it, beyond `Ring::named_value` above and the
bundle's outputs' `order`. The interfaces
eval uses for it: `Instance::seed_cell(path, Value)` (a cell set as a
baseline, no `on change`, refused when its type does not hold the
value), `Instance::cell_type(path) -> Option<Ty>` (a cell's declared
type by the inspector's path), (replayfix) `Instance::cell_mounted(path)
-> bool` (a cell is mounted there now: a declared one inside a popup
not yet open is not, and its seed waits for its mount),
`SchemaHost::answer(method,
Result<Value, String>)` (the next call of `method` answered with it)
and `SchemaHost::answers_left() -> Vec<(String, usize)>` (answers
queued and never taken. (replay-owner) `strand replay [report-dir]
…` (the binary's `report/replay.rs`) hands off git-style, as `git`
runs `git-<name>`: it finds an executable `strand-eval` beside the
running binary (`current_exe`), else the first on `PATH`, and `exec`s
`strand-eval replay [<newest report>] <args>` with the huge-page
setting restored (`strand_services::child::restore_in_child`); without
one it prints that the replay needs `strand-eval` installed next to
`strand` and exits 1. The shipped binary links nothing of eval and
reads nothing of its output; `strand-eval replay` itself runs as a
plain desktop process on its in-process stand-in for the compositor
and the CPU, writes
`<bundle>/replay/` and opens it with `xdg-open` unless `--no-open`.
(replay-clip) `strand replay clip [report-dir] <args>` hands off the
same way, as `strand-eval replay clip [<newest report>] <args>` (the
report put after `clip`, the clip's options told from it by
`CLIP_VALUED`, the same list as eval's `replay::clip::CLIP_VALUED`):
it reads a replay already written and writes only its `--save` file
(docs/eval.md, "A clip").
(report review) A step the sandbox cannot
take is skipped and kept as a divergence, the window is capped at
120 s before the report, a request record is replayed as a `set` or
`toggle`, and a horizontal scroll is named as not replayed. Nothing
under `crates/` depends on eval, and the shipped binary has none of it.

**Budgets.** The 256 KiB are counted against the PSS budgets, the
rings being allocated in every measured run (the default is on); the
build measures them (features.md "Glitch reports") rather than
assuming the zeroed pages stay untouched: both rings filled, the
anonymous PSS (`Pss_Anon`, where the heap is) on minus off is at most
288 KiB (264 to 268 kB measured; benchmarks.md). No thread, timer or wakeup
exists while no report is being saved, so the idle-wakeup windows in
`crates/strand/tests/budgets.rs` hold unchanged. The release binary's
`.text` gates in `budgets.rs` are measured with the code in; a
re-base, if it needs one, is a dated decisions.md entry.

## Contracts

### `strand-scene`

- **Geometry**: `Rect { x, y, w, h }` in physical pixels as `i32`/`u32`,
  `Size`, `Point`, and a logical-pixel `f32` variant; `Scale` is the
  fractional scale as numerator/120 (`wp_fractional_scale_v1`).
- **Props** (M6, m6-w2-surface): `Prop::affects_layout` says which props
  layout reads (render relays out for them; `strand_render::affects_layout`
  forwards it; `options` among them since the review of 66c2f03) and so
  which bindings on `id.box` lay boxes out again (the compiler's
  box-dependency graph, `check::box_layout_loop`).
- **Local sources** (M6, m6-owner-fixes, bug 8): `source_is_file(&str)
  -> bool` says whether an `image` node's `Prop::Source` is a file
  (starts with `/`, `~/`, `./` or `file://`, or ends in an image
  extension) or an icon name. Render's `image::is_icon` and the
  compiler's `check::assets` both use it, so the renderer draws by the
  rule the compiler resolves and checks by. A `Prop::Source` of an
  `image`, `svg` or `lottie` reaches render with a relative path
  already made absolute under the config directory (the compiler's
  `check::assets::local_source`, applied as instantiate binds the
  prop); render reads the path it is handed.
- **Colour**: `Color` stored as straight-alpha sRGB `f32`, with exact
  conversions to OKLab/OKLCH; interpolation for springs happens in OKLab.
- **Motion** (`strand_scene::motion`, shared by the renderer's prop
  springs and the theme's palette springs): `Spring { stiffness, damping }`
  is a unit-mass damped oscillator (`spring(3000, 0.75)`: stiffness and
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
  `rest(value, eps)` (or `rest_each(value, [eps; N])`, a tolerance per
  channel: a border's width and its colour, decisions.md border-eps),
  `retarget(target, curve)` and `shift(delta, curve)`
  (a FLIP jump) take effect at the next `sample(at)`, which starts them
  at `at` less one frame (at most `START_LEAD`, never before the previous
  sample: `sampled_at(prev)` seeds it; at a time sampled already, a frame
  laid out again before it is drawn, from the sample before that one, so
  one frame's changes start together, decisions.md launcher-flip-pass)
  from wherever the value is there;
  `sample_frame(at, period)` is the renderer's form, the frame being one
  refresh `period` of the surface drawing it (its estimated frame
  period, so a 144 Hz surface's first frame shows one 144 Hz frame of
  progress, whatever its idle frames painted),
  *keeping its velocity* (a spring starts with it; a timed curve adds
  it as `v0·t·(1 − t/d)²`, which fades out by the end of the duration);
  `retarget_at` starts at a given time;
  `peek(at)`, `velocity(at)`, `is_settled(at)` read without starting
  anything (a spring is settled once each channel's amplitude,
  `√(x² + (v/ω₀)²)` from its target, is within its tolerance: a damped
  spring's energy never grows, so it stays settled, and the moment it
  settles is the curve's, whatever the frame rate; decisions.md
  roll-rate), and `probe(at)` reads all three at once (the curve evaluated
  once: the theme swap's contrast play-through). (eval-rules-2)
  `view() -> MotionView<N>` is its state as plain data, read without
  sampling: the segment in flight (`start`, `from`, `vel`, `target`,
  `curve`), the change waiting for its first sample (`pending`: target,
  curve and FLIP jump), its last sample's time and `eps` (per channel); strand-render's
  `eval-hooks` probe records it beside each sample. Everything is a pure
  function of the timestamps sampled, so frames are testable as images. `color_channels`/`channels_color` map a
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
  `Modifiers` and repeat (m6-service-gaps, review r1: and `latin`, the
  key's keysym name in the keymap's first Latin layout when the active
  one gives it a non-ASCII character, which shortcuts match through
  `KeyInput::shortcut()`); `ClickAway { surface }`, a press on the
  click-away catcher under an open `keyboard: exclusive` surface whose
  `open` is two-way, `SurfaceSpec::open_two_way` from `Prop::TwoWay`;
  m6-service-gaps: `Paste { surface, text }`, the clipboard's text a
  paste asked for, answering `ClipboardAsk::Paste { surface }` beside
  `ClipboardAsk::Copy { surface, text }`) in
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
  time, every frame, so only palette roots need to spring. (M6,
  decisions.md m6-w2-language) Each palette extra the program reads
  outside templates (`lower::Program::palette_reads`) is also a plain
  colour at `palette.<name>`, so it springs as a root does; logic reads
  the palette for `$palette.<name>` through `VmHooks::palette(rt, name)`
  (`Op::Palette`), over the instance's palette memo, which the token
  table's memo is made over. `strand export json` leaves those paths out
  of `tokens` (they are under `extras`). Derived
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
  side reading it) reads them, except a token that reads time
  (`TokenTable::time_paths`, through other tokens too), which each
  lookup evaluates at the asking scope's time. Render freezes the tree's table when a
  `SetTokens` lands and in each frame a swap moves the roots; a clone is
  not frozen, equality ignores it, `insert`s thaw it, and a direct write
  to its pub fields needs a `freeze()` again (or `thaw()`). `enter`/`exit` are props whose
  value is a `PropValue::Pose` (prop/value pairs) or a preset keyword.

- **Surfaces**: `SurfaceSpec` (in `strand-scene`) is what a surface-kind
  node asks of Wayland: kind, name (namespace `strand-<Name>`), edge,
  anchor, layer, keyboard, margin (`Insets`, zero while unset) and
  `margin_set` (m6-owner-fixes: whether `margin` is set at all, `0`
  included, since a popup's gap is 6 px only while it is not), requested
  logical width and height, `screens` (`All`, `Focused`, or `Named` monitor identities, which
  logic uses to pin each per-monitor `bar` instance), `open` and `attach`,
  `overhang` (how far shadows reach past the box, filled in by render
  from layout), (flex) `exclusive: Exclusive` (`Auto`, `None`,
  `Length(f32)`, from `Prop::Exclusive`), (flex) `at_pointer` (a popup
  with `anchor: pointer`: render gives its `anchor_rect` as 1 × 1 at the
  press that opened it, recorded by `Renderer::pressed_at(surface,
  point)`, which the host loop calls for each button press before
  routing it (with `released_at(surface)` for a release and
  `key_step()` for a key, each a new input step): (flex review) the
  press is taken only when the popup opens in that press's step and the
  press's hit chain holds its element, else the element's box; an open
  one re-takes a newer press on its element once logic's first diff
  after its release is applied (`Pointing::diff_applied`), so a press
  that closes it never moves it first; `popup_config` makes it `PopupSide::Below` with
  `aligned`, a positioner anchored and gravitating bottom-right),
  (window) `title`, `app_id` (`Prop::Title`, `Prop::AppId`;
  `window_title()` and `window_app_id()` give the name and namespace
  while unset), `min_size` and `max_size` (`(Option<f32>,
  Option<f32>)`, from `min_*`/`max_*`) and `decorations: Decorations`
  (`Server`, `None`; `Prop::Decorations`), read only for a `window`,
  plus `thickness()` (a bar's
  across its edge), `exclusive_edge()`, `exclusive_zone()` (what
  `exclusive` keeps along that edge) and `needs_recreate()` (kind,
  namespace or layer changed). (flex) Logic pins each per-monitor
  instance of a `panel` or `osd` whose `screens:` picks monitors as it
  does a bar's; a `lock` is one instance with `screen` bound to the
  focused (else first) monitor and no pin; a `window` is one instance
  with no `screen` and no pin. A surface without a size of its own (a
  panel, OSD, popup or window without `width`/`height`, a bar without a
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
  row)` reveals a row (the reveal stands in the list's `ScrollState`
  until the pointer scrolls it, and each layout pass that changes the
  list's view places it again); a `list` lays out only the rows in
  view. Text is
  measured from delivered layouts; until the first arrives it takes no
  room of its own (a list's row's is estimated; decisions.md
  late-text-dip).
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
  `SetTokens`; text changes never glide. A paint offset in flight (an
  `x`/`y` spring, a glide) is drawn whole logical pixels from its rest,
  as layout places boxes, so content moves as one and a spring's tail
  draws its rest; a lifted node's held offset (a drag) is drawn as it
  is. A target changed by tokens or
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
  (p2) fixer round 3). (growth) A content-sized panel, OSD or popup
  whose content's size steps (a text grew, a row mounted or left with no
  pose that springs its size) springs its root's box over the step per
  axis: `Animator`'s per-root step motion holds how far the box is from
  the content's size and springs it to zero along the root's `width`'s,
  `height`'s or `size`'s transition (`width: auto ~ …`; `$motion.spatial`
  by default; `~ instant` and `reduced_motion` take the step at once).
  `Renderer::lay_out_stepped` lays the root out at the size in flight:
  the boxes that fill it on that axis at rest and in the last frame drawn
  follow it (`StepFills`) and clip what they hold, which keeps its rest
  size (`Forced::keep`) and, on the axis a far-anchored surface keeps
  (a bottom OSD's height), its rest place by that edge. The box is
  placed where the compositor keeps it as the buffer, configured at the
  larger of the two sizes, is resized, so the anchored edge stays put;
  an overshoot past the buffer is laid out where it is and cut by it.
  The pass that starts a step glides what the step moves from where it
  was drawn, compared with the box laid out where the last frame drew
  it. The chain of boxes that follow goes into a `pages` (its page fills
  it) and across a `scroll` or `list`, never down one, where everything
  keeps its rest size. A far-anchored box is placed by the room between
  the frame and the box, each rounded (`RootSize::Placed`'s `exact`), so
  its anchored edge is where the frame's snaps at rest at any scale. A
  growth's buffer shift counts in a FLIP only on an axis where the box is
  placed short of its buffer: one filling the larger buffer at once
  (`~ instant`) moved with its edge, and what it holds snaps with it.
  While the root's box lies short of its buffer, `Painter::input_region`
  is that box as laid out and as drawn (`Renderer::placed_root`), so the
  margin passes the pointer (not on an OSD or a tooltip, which take none). A surface painted before the configure at
  the larger size it asked for (`SurfaceState::awaits`, once per size)
  keeps its box at the buffer's size with what it holds kept, and the
  step is measured from there when the configure comes. Once a surface
  rests, a popup anchored in it whose anchor's box moved is re-resolved
  (`place_waiting_popups`), so it never keeps an anchor read from a
  preview pass in the old buffer. A content-sized surface never shrinks while something on it
  moves or its box springs down, and asks for its own size once nothing
  does. The paint that
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
  planning (in full for a new or invalid buffer), for a surface of any
  size (no memory cap: design.md's budgets are test targets). A crossfade
  landing mid-crossfade takes the blend on screen as its snapshot; a
  surface that paints nothing for the exit stall loses its snapshot; a
  table that changes no colour leaves fades running, a snapping one
  ends them. The blend works on the CPU `PaintTarget`; on a surface
  presented by the GPU (M4) the frame draws the snapshot weighted `1 -
  w` and adds the new frame weighted `w` (a `Plus` layer) along the same
  curve; (perf-gpu-retain) its snapshot is the frame the GPU thread
  last drew for it (`FrameSnapshot::Take` at the fade's first frame,
  then `Keep`, drawn by `Op::Snapshot`), so nothing is repainted on the
  CPU or uploaded, the blend on screen included when a crossfade lands
  mid-crossfade; render takes it so only when it lowered the surface's
  last frame at this size since it was attached, and a surface that
  goes back to the CPU before or during its fade takes its snapshot on
  the CPU from the old display list. A surface in readback mode blends
  on the CPU as today.
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
  The steps below that touch the renderer and the input router are
  `strand_render::host_loop`'s reactions (eval-host-loop), which the
  shell's main thread (`crates/strand/src/demo`: `apply`, `text_ready`,
  `Host`'s `SurfaceHost` callbacks and `painted`) and the evaluation
  sandbox's driver (`eval/src/harness.rs`) both run, each with its own
  side behind `host_loop::LoopHost` (`parts() -> (&mut Renderer,
  Option<&mut Router>)`, `shown`, `applied`, `intents(Cause, Vec<Intent>)`
  with `Cause::{Settle, Tick, Input(&InputEvent)}`, `facts`, `wake`, and
  default no-op hooks `attaching`, `configuring`, `detaching`,
  `detached`, `after_paint` at fixed points) and `host_loop::LoopSync:
  LoopHost` (`sync`: the surface changes to the surfaces). The
  reactions, each generic and doing only what the host did by hand, in
  the same order: `apply(h, diff)` (`Router::observe`,
  `Renderer::apply`, `applied`, `Router::settle` and its intents, the
  input wake, `sync`); `woke(h)` (`Router::tick` and its intents, the
  input wake, `Renderer::update`, `sync`); `input(h, event, idle)` (the
  shown surfaces not wanting a frame, `Router::handle` and its intents,
  `set_pointer`, the input wake, `facts`, `wake` if surface changes
  wait or an idle surface now wants a frame); `attached(h, surface,
  node, bounds)` (`attach_surface`, `set_surface_bounds`, `attaching`,
  `Router::attached`, `wake` if changes wait); `configured(h, surface,
  size, scale)` (`configure_surface`, `configuring`, `facts`, `wake` if
  changes wait); `detached(h, surface)` (`detach_surface`, `detaching`,
  `Router::detached`, `detached`, the tick); `painted(h, surface)`
  (`facts`, `after_paint`, `wake` if changes wait). The input wake is
  `Renderer::set_input_wake(Router::next_wake())`; a host with no
  router (the demo bar) skips routing's steps. No host takes these
  steps by hand outside tests (`demo::tests::
  the_shell_takes_the_host_loops_steps_only_through_its_reactions`,
  `eval/tests/drift.rs`).
  0. After each `apply`, drain `take_surface_changes()` and hand them to
     the surface manager: create a layer surface (or xdg_popup, lock
     surface) per matching output for `Created`, reconfigure or recreate
     it for `Updated`, destroy it and `detach_surface` for `Removed`.
  1. Spawn the text worker with `TextWorker::spawn_with_waker(config,
     Some(waker))`, where the waker pings the main calloop loop, and
     build `Renderer::try_new(TextBackend::Worker(worker))`, which
     also starts the image worker and fails, as the text worker's spawn
     does, when it cannot (`Renderer::new` decodes inline instead: tests
     and offline tools). `Renderer::text()`
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
  physical size into a 32 MB LRU (`strand_render::image`, freedesktop icon
  theme, PNG/JPEG/SVG; JPEG IDCT-scaled and PNG reduced row by row so a
  decode holds about the drawn size), on a worker with a text worker,
  inline offline; while a size springs a decode of the source at
  another size or fit stands in, the one that holds what the box's fit
  shows (`ImageStore::stand_in`, `Decoded::holds`), placed by the
  node's own fit (`Decoded::placed_for`, `Item::Image::dest`); when none
  does (a `cover` box changing shape), one decode of the whole source is
  asked for to stand in for the rest of the spring (stand-in-fit). Gradients draw
  from dithered pixmaps and shadows from cached ones (a 32 MB paint
  cache of entries up to 16 MB, one over 2 MB built only when a second
  frame repaints all of it, so a first frame and partial frames draw it
  directly; entries unused for `IDLE_FREE` (30 s)
  freed at the next paint or the next `Renderer::update`, whatever woke
  the loop: nothing wakes just to free them, so an idle shell does zero
  work between ticks; a frame loop that stops keeps them, so a surface
  shown again within `IDLE_FREE` draws from them). A shadow on whole
  pixels (no `scale`/`rotate` above it) whose box is at least 8σ and
  twice its corners' reach across, with corners vello does not clamp to
  the box, is drawn from slice
  tiles instead of a pixmap of its size (shadow-slice,
  `PaintCache::shadow_slices`, both backends): per quadrant a base tile
  (a square corner's field padded past its edges) and a corner tile of
  the quadrant's radius, keyed by blur, colour, radius and the edges'
  sub-pixel phase, never by the size, so a box whose size springs builds
  nothing after its first frame; tiles are at most 2 MB (a larger one
  leaves the shadow whole); a corner tile is held uncached (up to 4 MB
  of them, counted in the cache's bytes) and cached when any later
  frame asks again, every surface's paint and GPU lowering being a frame
  of the shared cache, so a springing radius builds two small tiles a
  frame and evicts nothing, and a still one is built once however many
  surfaces paint; a
  text node's record keeps its glyph cells (`NodeRecord::glyphs`), so a
  change that only swaps glyphs damages those glyphs (a clock tick
  repaints its last digit), and a node's border along its box keeps
  its ring's strips (`NodeRecord::ring`), so a change of the border
  alone (a conic turning with `t`) damages only the ring; a box drawing
  only solid fills, borders and rims, untransformed and without effects
  or an inner shadow, and every node under a clipping box keep their
  geometry
  (`NodeRecord::geom`: the rounded boxes of the node and of the clips it
  is drawn through, the rest hashed apart; a mask's frame is hashed into
  its children's, as the mask is drawn across it), so a box or clip that only
  moves, resizes or rounds its corners damages along its edges and
  corners (`RoundBox::changed`), not its whole area and its content's
  (a card swelling under its content), and each frame keeps its nodes'
  paint order (`Flattened::order`), so where nodes that overlap swap
  places (a lifted child raised, a keyed reorder) what they share is
  damaged (`order_damage`, dock-drag); a gradient is cached
  only when a second frame draws the same paint at the same size, so one
  whose paint or size changes every frame is dithered cell by cell,
  uncached. `Renderer::last_paint_times` (`PaintTimes`) says where the
  last CPU paint spent its time (laying out and flattening, of that
  waiting for text, drawing); `strand run`'s damage log prints it.
  (perf-bench) It also splits flattening's layout out (`layout`), times
  the damage diff (`diff`), lowering to the GPU's frame (`lower`) and
  the crossfade's snapshot and blend (`blend`), and carries the
  raster's own split (`raster_parts: RasterTimes`: preparing the paint
  cache, offscreen groups, encoding cells, vello_cpu's fine pass and
  copying out, with the cells drawn), which `tests/anim_bench.rs`
  reports per frame; every timer is a few `Instant::now` calls a frame
  on paths that already run, so idle work is unchanged.
  (perf-cpu-raster) A paint of at least `PARALLEL_CELLS` (8) cells
  draws them on up to `MAX_THREADS` (4) threads, the render thread one
  of them (`raster::share`): each takes the next cell not yet drawn,
  encodes and rasterises it in a vello_cpu context of its own (each
  context single-threaded, as before), and copies it into its row of
  cells' band of the buffer under that band's lock; the threads are
  started for the paint and joined before it returns, so nothing runs
  or wakes between paints; at most `MAX_THREADS` − 1 of them run in the
  process at once however many renderers paint together (a paint that
  finds no place draws on fewer); and every cell is drawn alone as
  before, so which thread draws it changes no pixel.
  `RasterTimes::threads` says how many drew; with more than one,
  `encode`, `fine` and `copy` share the pass's wall-clock time as the
  threads spent theirs. Each thread
  keeps its contexts and scratch (`Lane`) between paints; the other
  threads' lanes are freed once no paint has drawn on them for
  `LANE_IDLE` (1 s), at the next paint's or wake's trim
  (`Raster::trim_idle`, never a wake of its own), so an animation keeps
  them and an idle shell does not. A
  gradient over 64 k pixels is dithered on the same threads, by rows,
  and a shadow's missing slice tiles are built on a thread each
  (`PaintCache::build_tiles`: a radius spring's four corner tiles). A
  gradient's colour table (1,024 OKLab samples) is kept by its stops'
  bits (256 tables, about 4 MB, cleared when full, and freed with the
  paint cache's idle entries once none has been asked for for
  `IDLE_FREE`; `cache::lut_cached`),
  and the ramp cuts vello's gradients are given (`raster::paint::cuts`)
  are one table shared by every thread. A gradient drawn smoothly
  (under `scale`/`rotate`) still draws each cell's part (integration-r8),
  copied from its cached whole pixmap when the cache holds it
  (`cache::crop_part`, the same pixels) instead of dithered again. A
  mask (`layers::cell_mask`) is computed only within the reach of its
  bounded masks (a shape's outline or a radial's disc, grown past its
  antialiased edge); a shape's polygon bins its edges by band of rows
  (`shapes::Polygon`), so a pixel's coverage reads the few edges near
  it, the same bits as all 120. Goo (`effects::goo`) works out each
  grid corner's box distances into one buffer (on the stack up to
  eight boxes), not a vector a corner. A
  frame waits for new text (`ESTIMATE_WAIT`) once, however many layout
  passes it takes, its `update` and `paint` sharing the deadline
  (`SurfaceState::text_deadline`, set at its first wait, cleared once
  painted). Painting a surface moves every other surface's
  first-frame and size hold still running by the time it took
  (`Renderer::pause_holds`): they wait for the text worker and the
  compositor, not for the loop's own painting, and are moved on by
  at most `MAX_HOLD_PAUSE` (1 s) in all, so a configure that never
  matches costs a bounded wait, not a stall. An empty `text` lays out as 0 × 0. `marks:` arrives as a list of `[start, end]` pairs: the
  compiler's scene conversion turns a `Range` record into that pair. A `popup`'s spec gets `parent` and `anchor_rect` (its
  element's laid-out box in the parent surface; `None` until that box is
  laid out, and the layout pass that lays it out reports the spec again,
  decisions.md popup-boot-wake); `tooltip: expr` makes a
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
    `noise(x)` only when `x` reads `t`; (flex review) a maths builtin of
    a time leaf is `TokenExpr::Math { f: MathFn, args }`, which the VM
    builds instead of computing, `MathFn::apply` evaluating it in
    degrees and giving no value for a result that is not finite): its node repaints each frame of
    its clock while visible, and only it. So is a prop or override that
    references a global token reading time (`$pulse: 8 * wave(2s)` in a
    token set): render asks `reads_time_with(&|p|
    global.time_reads(p))`.
  - **Per-node clocks with frame caps.** A node that reads time or plays
    frames (an animated image at its own rate, `grain` at 12 fps,
    `shimmer` at 30, others at refresh) has a clock; a clock that is not
    due by the next frame leaves `wants_frame` false and puts its due
    instant into `Renderer::next_wake()`, so the surface needs no new
    call. Hidden nodes' clocks stop, and the frame loop stops when every
    clock is idle. Built (m4-runtime F2): `strand-render`'s `clock.rs`
    holds the rate table (`clock::rate`; S-effects adds `grain`, GIF and
    raster-node rates there) and each surface's clocks from its last
    fresh frame. A capped clock reads `t` in whole ticks, a tick reached
    half a frame early, so it repaints only on ticks; between them the
    surface wants the next frame only if that frame is the one nearest a
    tick, and otherwise `next_wake` is half a frame before the tick
    (mapped through the surface's last frame), after which `update` makes
    `wants_frame` true. The frame period is the shortest of the
    surface's last 8 frame gaps (gaps over 1/60 s and a tenth, skipped
    frames or idle time, left out), at most 1/60 s. A node is hidden, so it has no
    clock, when it is not laid out, when its opacity or scale is 0, when
    it is under a hidden ancestor, or when all it draws is outside the
    clip. The first two count only while the hiding prop does not
    follow time. The clip counts only while nothing that places the node
    (`x`, `y`, `scale`, `rotate`, `shadow`) follows time.
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
    canvas work. M4 added `FillThemed(PropValue)` and `StrokeThemed {
    paint, width }` for paints naming tokens (`c.fill($accent)`), which
    render resolves in the node's token scope. The instance runs the
    lambda in the prop's memo with a `Canvas` of the node's laid-out
    size, the VM recording each `Canvas` method call; render turns the
    list into clipped fills (`strand-render/src/canvas.rs`; `fill` with
    no path fills the box; `text` is drawn in the node's font, see
    features.md). (flex) Paths drawn point by point added `MoveTo { x,
    y }`, `LineTo { x, y }`, `Arc { x, y, r, from, to }` (degrees, 0
    pointing right, growing clockwise; at most one whole turn) and
    `Close` for `c.move`, `c.to`, `c.arc` and `c.close`, and
    `FillEvenOdd(Paint)` and `FillThemedEvenOdd(PropValue)` for
    `c.fill(paint, evenodd: true)`. `to` and `arc` continue the open
    subpath (an arc joined by a line to its start) or start one at their
    first point; `close`, a paint, a `rect` and a `circle` leave none
    open. Render's display list carries an even-odd fill as
    `FillShape::EvenOdd(BezPath)` beside `Rect` and `Path`, drawn by the
    CPU raster and lowered to the GPU's `Op::Fill { even_odd: true }`
    (decisions.md flex).
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
    are buffer values, so divided by `scale`), nothing for the rest. Render's display list gains a layer group,
    `Item::PushLayer(Arc<layers::Layer { effects, frame, scale, xform,
    gpu }>)` (`gpu: Option<offscreen::Drawn>`, m4-gpu-effects: its
    bundled passes' pixels read back, drawn as its offscreen group in
    place of the CPU's version) …
    `Item::PopLayer` (markers like its clip, opacity and transform
    groups, the push carrying the group's bounds; built: m4-runtime F3),
    whose bounds and whose nodes' damage grow by its effects' reach,
    lowered by each backend its own way (vello_cpu: one cell's
    `push_layer` with the opacities multiplied, the last blend and a
    cell-sized mask of `fade`/`radial`; masks always on the CPU;
    `strand-render`'s `layers.rs`). Render builds a node's `Effect`s
    from its resolved props each frame (`strand-render`'s `effects/`:
    `filter:` colour functions composed into one matrix per run, `blur`,
    the bundled filters as shader passes, then `mask:`, then `blend:`);
    on the CPU a `bloom` pass is an offscreen group drawn as a glow of
    its own pixels and the other shader passes draw unfiltered; with a
    GPU the bundled passes run there (`Layer::gpu`), after the CPU's
    blur and colour matrix whatever the written order. The
    display list also gains `Item::Raster`, a CPU
    raster node (particles, grain, graphs, spectrum, animated image
    frames) drawn into a cached pixmap at its clock's rate (its fields
    below). Cached offscreen groups (glows,
    filtered subtrees, glass sources) redraw only when their children
    change, in a second 32 MB budget freed when idle. Built (m4-runtime
    F4, `strand-render`'s `offscreen.rs`):
    - A layer with `Blur` or `ColorMatrix` (`Layer::cell_local` false) is
      drawn whole into a pixmap of its bounds, filtered there, and drawn
      into each cell as an image under the layer's cell-local part. The
      pixmap is keyed by the hash of what the group draws
      (`OFFSCREEN_BYTES`, LRU; a group over the budget is drawn uncached;
      freed with the paint cache's idle rules).
    - The raster node is `Item::Raster { node, key, pixmap, rect }`
      (`key` hashes the source's config, its `state`, size, scale and
      `TimeContext`; `rect` is its box in physical pixels). A
      `RasterSource` (`draw(pixels, w, h, scale, TimeContext)`, `rate()`)
      is built from the node's props each frame (`crate::effects::raster`:
      `grain:`, particles, the built-in effects) and drawn through
      `RasterNodes::pixmap_from`; a source with state of its own (graphs,
      spectrum, animated frames, the media nodes) is set with
      `RasterNodes::set`. Its pixmap is redrawn only when its config,
      state, tick or size changes. (M4, m4-effects-media) Two provided
      methods follow: `clock() -> Option<Rate>` (default `Some(rate())`;
      `None` for a source that changes only when fed or when its props
      change: a spectrum, an `svg`, a `lottie` with no file), and
      `state(&RasterProps) -> u64` (default 0), called on every flatten
      that draws the node with its resolved props (`RasterProps { get,
      color, parts, files, boot, slack, still, commit }`, exported from
      `lib.rs`; (graph-follow) `boot: Duration` is the frame on the
      boot clock, zero in a frame with no clock, `slack` how early a
      tick counts as reached (half a frame), `still` that time stands
      still in it, and `commit` that the frame will be painted, not a
      preview flattened at the last painted frame's time): the pixmap
      is drawn again when that hash changes. (graph-follow)
      `clock::Rate` is `Refresh`, `Every(period)` (a cap, ticking from
      the node's first frame) or `Aligned(period)`, which ticks at the
      period's whole multiples on the boot clock
      (`NodeTimes::aligned_context`), so two renderers drawing at
      different times tick together: a `graph`'s columns. Media nodes
      (`graph`, `spectrum`, `svg`, `lottie`, `thumbnail`) get their
      sources from render itself when logic creates them. The props keep
    arriving as `PropValue::Call`; render builds the `Effect`s.
    - (m6-owner-fixes, owner-testing.md bug 17) **Fade layers**
      (`offscreen/fade.rs`): an opacity group whose content only moves
      or fades (an `enter`/`exit` pose, a `when` fade) is drawn once into
      a cached offscreen group and, on the frames after, as that image at
      the group's opacity. The flattener tags the group's marker with its
      node: `Item::PushOpacity(f32, Option<Fade>)`, `Fade { node: NodeId,
      moving: bool }` (`moving`: its opacity or paint offset is in
      flight). The key hashes the group's items from the top-left pixel
      of what they draw (`flatten::Shift`, quantised to 1/4096 px), so
      content moved by whole pixels reuses its layer. Only groups outside
      every transform and offscreen group, with no offscreen group inside
      and with a shadow, gradient or paint-filled text in them, are
      layered. Damage is unchanged. The layers share `OFFSCREEN_BYTES`.
      `Renderer::set_fade_layers(bool)` (on by default) and
      `Renderer::fade_layers() -> (built, drawn)` are for tests and
      benches. Pixels match a re-raster within the PNG tolerance (2 per
      channel; up to 8 on a clip's antialiased edge crossing the group),
      not bit for bit.
  - **SVG parts.** An `svg "icon.svg" { #needle { rotate: … } }` selector
    block is a child node of kind `NodeKind::SvgPart` (`svg_part`, the
    one kind with no schema element) carrying the id it
    selects as `Prop::Name` (`Text`, without the `#`) and ordinary props,
    which render applies to that layer (decisions.md, 2026-10-05 render).
    Built (m4-effects-media): the compiler lowers `hir::Selector` (which
    carries its own `NodeIdx`) to a `svg_part` element whose positional
    is `name`; render's `media/svg.rs` draws the `svg` as a raster node,
    its parts' props resolved in flattening and handed over in
    `RasterProps::parts`.
  - **Keyframes.** `Prop::Play` holds `PropValue::Keyframes(Arc<Keyframes>)`
    in place of `[name, seq]`: `Keyframes { name, seq: u32, stops:
    Vec<(f32, Vec<(Prop, PropValue)>)>, duration, delay, repeat:
    Option<u32> (None forever), alternate, easing }`,
    the compiled `keyframes` block inline (stops as fractions, settings as
    `check::keyframe_settings` allows), so render keeps no keyframe table.
    Keyframes are offsets composed with the node's springs; a new `seq`
    restarts them.
  - **Backlog** (decisions.md history). `Prop::Backlog` (compiler-set,
    snapped) holds `PropValue::Backlog(Arc<Backlog>)` on a `graph`
    whose value logic keeps history of: `strand_scene::Backlog { points:
    Vec<BacklogPoint { at: f64, value: f32, ramp: f32 }> }` (16 bytes a
    point), oldest first, `at` the seconds on the boot clock
    (`strand_scene::backlog::boot_now()`, `CLOCK_BOOTTIME`, which counts
    time suspended; `clocks() -> Clocks { boot, awake }` reads it with
    `CLOCK_MONOTONIC`) when the value was taken, `ramp` the seconds over
    which the value moved to that point from the one before (0 steps:
    its time is exact; a sample a hidden service polled ramps over that
    service's polling period), a value that is not finite the start of a
    gap (the machine slept); `Backlog::value_at(at)`, `oldest()`. Logic
    sends it when the graph mounts and again when the kept history gains
    a point; render reads the boot clock at each draw and works out each
    column's time from it, filling from the backlog only the columns it
    did not sample (its first draw, a gap in its ticks) and drawing a
    gap as no column. (graph-follow) `Renderer::set_history_clock(
    clock::BootClock)` (`fn(Duration) -> Duration`: the boot clock at a
    time of the presentation clock; the real one is `clock::boot_at`,
    `strand_scene::backlog::boot_at`: the presentation time moved by
    what `CLOCK_BOOTTIME` is ahead of the presentation clock, which is
    `CLOCK_MONOTONIC` unless the compositor named another, in which case
    strand-surface's `PresentationClock::set_clock_id` calls
    `backlog::set_presentation_clock(clk_id)`; a `CLOCK_BOOTTIME` one is
    taken as it is) replaces that clock (tests, the eval sandbox's
    virtual one); the animator reads it at each frame's `begin`, and a
    graph's columns are its whole periods (`history / slots`), tick `j`
    the value at `j × column` seconds of it: the backlog's from its
    oldest point on, else the value the last painted frame at or before
    that time saw (a preview's sighting is stamped with the next painted
    frame's time). A backlog is not part of
    the graph's state hash: it repaints nothing by itself. A graph of a
    list value (`float | [float]`: `Prop::Value` a `List` of numbers)
    gets a `List` of backlogs, one per item (each item's store bounded
    as one series', so a list of N items keeps up to N × 8 KiB). A list
    that changes length keeps the series of the items still there and
    starts the added ones from their backlogs. `strand_scene::backlog::
    DEFAULT_HISTORY` (60 s) is a graph's `history` when unset, on both
    sides.
  - **Ranges** (decisions.md graph-follow). `Prop::Range` (snapped) on a
    `slider`, `meter`, `arc` or `graph` is a `List` of its two ends,
    each a `Number` or `Keyword("auto")`, or `Keyword("auto")` for
    both; unset, 0 to 1. Render reads it with `range::Spec::of` (`None`
    for an end that is not a finite number: a node keeps its last
    usable range, `range::Auto::usable`, and a slider's drag writes
    nothing), maps a value with `Range::frac` (clamped) and a slider's
    drag with `Range::at`, which `Intent::Write` carries in range units;
    an auto range is `range::Auto` (a graph's own, over its columns; a
    meter's or an arc's in `Animator::ranges`, over the values logic set
    it to in the last minute, not the spring's). A ranged widget's
    `value` settles within its tolerance times its range's span.
    `Prop::LineWidth` (snapped) is a graph's line or bar width,
    `Prop::Style` its `line`, `steps` or `bars`, and its `Prop::Color`
    and `Prop::Fill` may be `List`s, one per series in turn. A graph's
    `Prop::Value` does not spring (`anim::springs`). (flex)
    `Prop::Step` (snapped, range units) makes render round a slider's
    drag to `lo + n·step` before `Intent::Write`, computed in `f64` and
    rounded to the decimals `lo` and `step` are written with
    (`Range::snap`). A `slider` or `meter` is upright when
    `widgets::upright(node, w, h)` says so: its written `Width` and
    `Height` (or `Size`) where both are lengths, else its laid-out box
    taller than wide; the Router latches it for a drag
    (`Router::drag_upright`), and an upright meter draws no wave.
  - **Layout and input props** (flex). `Prop::Wrap` on a `row` or
    `col` is taffy `flex_wrap` with `align_content: flex-start`.
    `Prop::Axis` (`Keyword`: `vertical`, `horizontal`, `both`) and
    `Prop::Snap` (`Bool`) on a `scroll` (`Snap` on a `list` too) are
    read by `renderer/lists.rs`: a wheel event moves the axis it moves
    most along, a dy-only wheel goes to the innermost x-only scroll only
    when no scroll under the pointer can move along y
    (`sideways_wheel_at`), and a touchpad gesture keeps the node and
    axis its first event took until its lift. `Prop::Cursor` is a
    `CursorShape`, which the Router resolves for the pointer's chain
    (the press's while the left button is held) and the binary hands to
    strand-surface through `SurfaceHost::cursor`. `Prop::OnKey`
    (compiler-set, `Bool`) marks an element with an `on key` handler:
    Tab and Shift+Tab move focus in render only when no element from the
    focused node out to its surface has it (`input.rs` `hears_keys`);
    the move skips nodes at `Opacity` 0 and their subtrees.
  - **Virtualised lists.** On a `list` whose direct child is a `for`,
    logic sets `Prop::RowCount` (`Number`: all rows) and `Prop::RowFirst`
    (`Number`: the global index of the first mounted row). `SceneOp::Create`
    and `Remove` gain `window: bool`, true for a row the list window mounts
    or unmounts: render plays no `enter`, `exit` or FLIP for it (nor
    for what the same diff creates under it), and an unmounted row
    leaves no ghost. `SceneDiff::create` and `SceneDiff::remove` send
    `false`. Render lays rows out at their global indexes (unmounted
    rows take the measured rows' mean height), scrolls by a paint offset
    with no relayout while the view stays within the rows laid out,
    never shows past the mounted rows, and reports the rows it wants
    (view plus overscan) with `Renderer::take_list_windows() ->
    Vec<(NodeId, Range<u32>)>`, which the binary sends as
    `ToLogic::ListWindow`. Pointer scrolling enters as
    `Renderer::scroll_input(surface, point, ScrollInput { dx, dy, kind:
    ScrollKind::{Wheel, Touch, Lift}, time })` (the Router maps
    `AxisSource` and `stop` to it; (flex) `dx` moves a `scroll` whose
    `axis:` is `horizontal` or `both`, whose offset along x is
    `ScrollState::across` and whose boxes' x offset is
    `ListBox::applied_x`, and a `snap: true` scroll or list keeps its
    children's starts in `ScrollState`'s snaps from each layout, which a
    wheel step and a lift settle on); `Renderer::list_frames() ->
    ListFrames { frames, gaps, stalls, top_row }` counts painted frames
    (cached ones too) with a list in view, those that showed an
    unmounted gap, and those whose view was held at the mounted rows'
    edge short of the scroll's offset (logic's rows late), and gives the
    global index of the first row the last one showed (the M4 exit's
    per-frame checks; the binary's `STRAND_LOG=damage` lines carry them
    as `gaps=`, `stalls=` and `top=`).
  - **Directional pages** (design.md, "Pages"). Logic sets
    `Prop::RowFirst` on a `pages` node in the diff that swaps the page:
    one more than before when the new page comes after the old one in
    mount order (source order, a `for`'s item order), one less when
    before (only the direction is meaningful). Render keeps the old page as a ghost,
    and at the end of the diff slides the new page in from the right and
    the old one out to the left when `row_first` grew (mirrored when it
    shrank), each by its own width (the `slide(edge)` preset); a page
    with its own `enter`/`exit` plays that, a `pages` with `transition:`
    leaves the swap to its mask, `reduced_motion` snaps. `pages` clips
    its content. `Renderer::page_swap(pages) -> Option<PageSwap {
    entering, leaving, forward }>` pairs the two pages while the old one
    plays out, for S-effects' transition masks.
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
    (`at` a global row index, with the dragged source's row left out of
    its own list and another list's first copy of the dragged item out
    of that list) for `on drop(p, at)`. As built
    (m4-lists wave 2): the Router turns a press on a `drag:` node into a
    drag past `DRAG_THRESHOLD` (6 px), draws the source at the pointer
    through `InputScene::lift(node, Option<LogicalPoint>)`
    (`Renderer::lift`: a held paint offset, painted above its siblings;
    `None` springs it back as a FLIP glide), finds the target with
    `InputScene::hit_under(surface, at, skip)` (the hit past the dragged
    source) and the node's `Accepts`, and emits `Drop` on release or on
    `InputEvent::DragDrop`; Escape cancels. The binary forwards it as
    `run::NodeEvent::Drop { payload, at }`, and `run/lists.rs` makes the
    handler's arguments on the logic thread.
    As built (m4-interaction-finish): a source whose value has a form
    outside Strand sends `Prop::Drag` as `List [Keyword(type),
    Keyword(kind), Text…]` instead of the bare `Keyword` (kind `text`,
    `files` or `app`, then the text, the paths or the app id);
    `strand_scene::drag_type` and `strand_scene::drag_export` read either
    form, and nothing else may look inside the prop. As built
    (m5-fixups): the instance always sends the `List`, its last item
    `List [Text(identity)]`, a hash of the value's type and identity (a
    keyed record's key, else the value, encoded losslessly) that is
    equal exactly when the values are (`==`);
    `strand_scene::drag_identity` reads it, `drag_export` ignores it, and
    `drop_index` leaves out, in another list than the source's, its
    first row holding a `drag:` source of the dragged one's identity
    (one pin's icon on another monitor's dock; the first, as `move`
    takes the first equal item out), as it leaves out the source's row
    in its own list, where equal items beside it are counted.
    (M6, m6-edges; an interface change, accepted by the owner,
    m6-owner) `NodeEvent::Drop` gains `hidden: Option<HiddenRows>`
    (`strand_render::HiddenRows { list: NodeId, before: u32 }`, the
    target list and its `row_first` as render held it), and so does
    `run::NodeEvent::Drop`, which the binary forwards unchanged: render
    has only a windowed list's mounted rows, so a copy of the dragged
    item scrolled out above them is counted in `at`. `drop_index` sends
    `hidden` when the drop is a `drag:` source of another list (its own
    row is mounted, and left out) on a windowed list with `row_first >
    0`, and no mounted copy was left out before the pointer (by the
    copy's middle when it is laid out);
    `run/lists.rs::drop_args` then asks `Instance::drop_at(value, at,
    list, before) -> u32`, which leaves out (`at - 1`) an item among the
    list's first `before` that is a copy by render's test: its row's
    `drag:` values include one with the dragged value's typed identity
    (`strand_scene::drag_identity`). A row not mounted has none, so
    logic works them out from its item (m6-integration-w1, replacing
    S-edges' guess from the mounted rows): `ListWindow` keeps the
    `for`'s body, binding and scope (`mount::probe::RowProbe`), and
    `Ctx::probe_row` walks the row's tree as mounting would (`if`,
    `match`, components and their slots, `let`s, a `state` read from
    the cell the row keeps while scrolled out or else its initial
    value), evaluating each `drag:` source (its last holding `when`,
    a laid-out size read as before layout) in a scope disposed after,
    with render's boundaries (a nested `list`'s or `for`'s rows are not
    the row's); nothing reaches the scene, the registry or the
    services. Logic, not render, because logic holds every item
    (`ListWindow::all`; `Ctx::list_windows` now keeps the window beside
    its setter); render alone would need each row's identity for the
    whole list sent with every change.
    `Prop::DropRows`
    (`Bool`, compiler-set) marks an element whose direct children
    come from a `for` (m5-fixups round 2: every such element, not only
    an `on drop` target), so render places drops among those items even
    when they are not `drag:` sources, and a `drag:` source under one
    (or under a `list`) is that list's item, not an item of a list
    further out: a row of an outer list holding the source's list (a
    kanban column holding the dragged card) is counted. `Prop::NotRow`
    (`Bool`, compiler-set, m5-fixups round 3) marks a child of a
    `DropRows` element that is not one of its `for`'s rows (a column's
    header, a dock item's icon beside its `for` of window dots): a
    `drag:` source in it is not that element's item, so the element is
    not a nested list for it and an outer list leaves it out as the
    source's row (or as another list's copy). `Renderer::drag_image(surface, node) ->
    Option<DragImage>` draws a node alone at rest (lift and glide held,
    no animation stepped) for the drag icon; `strand_scene::DragImage {
    size, scale, origin, pixels }` is premultiplied ARGB at the surface's
    scale with `origin` its corner in surface coordinates.
  - **Surface poses** (design.md, "Compositor-animated poses").
    `SurfacePose { opacity: f32, scale: f32, offset: LogicalPoint }`. When
    the compositor allows it (`Renderer::set_compositor_poses`) and a
    root's motion is only its own opacity, scale (on an axis anchored on
    one side) and x/y, render paints the content at rest and reports each
    frame's pose through `Painter::surface_pose`; the surface manager
    applies opacity with `wp_alpha_modifier_v1`, scale with the
    viewporter's destination size and x/y with layer-shell margins. A
    frame whose pose changed while `paint` returned no damage is a
    pose-only commit: no buffer is attached and `age` does not advance.
    The pose reported is the one the frame just painted took, even when
    that frame's end (a closing pose finishing) makes the next frame
    flatten anew, so a closing surface's last commit keeps its exit's
    last pose (m6-owner-fixes, owner-testing.md bug 7). A popup's x/y
    always repaints. Without the protocols, render repaints.
  - **Compositor capabilities.** `CompositorCaps { alpha_modifier,
    viewporter, single_pixel_buffer, background_effect, session_lock,
    data_device, hyprland }` (all `bool`; `delegates_poses()` needs the
    first two; `hyprland` (M4 interaction-finish): a `hyprland_*` global
    is in the registry), reported once the globals are bound
    (`SurfaceHost::compositor_caps`); the host hands render what it uses
    (`set_compositor_blur`, `set_compositor_poses`,
    `set_compositor_pose_scale(!hyprland)`; the blur fallback's reason
    names `strand compositor-rules` when `hyprland`).
  - **Backends** (`strand_scene::backend`): `Backend` (`Cpu`,
    `GpuPresent`, `GpuReadback`), `BackendChange` (`Start`, the warm
    start; `Promote(surface)`, `Demote(surface)`; gpu-first replaced
    M4's `Drop`), `GpuStatus` (`Unused`, `Starting`,
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
    placement puts the box at its margin on that side from the attached
    edge, 0 while unset (m6-owner-fixes, owner-testing.md gap 26: the
    margin is kept, so a negative one overlaps). Fillets take the paint
    of the outermost box touching that edge.
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
    position, read at flatten time by `parallax` and `tilt` (set by
    enter, motion, buttons and axis frames; `None` after a leave). The
    host hands it to render after each routed event with
    `Renderer::set_pointer(surface, Option<LogicalPoint>)`
    (m4-effects-paint-w2), which keeps it in `flatten::Extras::pointers`
    by surface root and repaints the surface only if a node on it leans.
  - `Router::drag() -> Option<DragView>`: the drag in flight, read by the
    drag ghost, `jelly` and list reordering. `DragView { source: NodeId,
    surface: SurfaceId, pointer: LogicalPoint, velocity: LogicalPoint
    (px/s), target: Option<NodeId>, index: Option<u32> (a global row
    index) }`. `None` but while a drag is past its threshold.
  - `Router::drop_target(surface) -> Option<NodeId>`: the node a drop on
    `surface` would land on now (our drag or another program's offer);
    the surface manager accepts the compositor's offer only while it is
    `Some` (`SurfaceHost::drop_accepted`).
  - Virtualised `nav`: `Router::selected_index(list) -> Option<u32>` is
    the selection's global index, mounted or not. Keys move it by index
    (Up, Down, Page_Up/Prior and Page_Down/Next by
    `InputScene::rows_in_view`, Home and End, which in an `input` are
    Ctrl+Home and Ctrl+End: plain ones move its caret); a row not mounted is
    scrolled to with `InputScene::reveal_index(list, index)` and the
    selection lands in `Router::settle` once logic mounts it (Return
    pressed meanwhile activates it then). `Router::observe` keeps a
    selected row that a diff unmounts with `window: true` selected by
    index until it mounts again. Both `InputScene` methods have no-op
    defaults; the `Renderer` maps them to `Renderer::reveal_index` and
    `Renderer::rows_in_view`.
  - Submenus: Right on a row that opens a nested `popup` opens it, Left
    or Escape closes the innermost; S-lists adds these keys for
    S-surface's tray menus, emitting the existing intents (a two-way
    `open` write). Built: Right (focus not on an `input`) writes `open:
    true` to the first closed `popup` with a two-way `open` under the
    focused list's selected row, else among the direct children of the
    hovered chain; Left on a surface whose root is a `popup` nested in
    another `popup` closes it as Escape does (`open: false` and
    `dismiss`); Left in a top-level popup does nothing.
  - The lock: keys on a lock surface while render's built-in fallback is
    shown never reach the `Router`; the binary (`run/lock.rs`) hands them
    to `strand_render::lock_fallback`, which needs no text worker. An
    `input` with `type: password` is edited by the `Router` as any
    `input`; its value is redacted by the binary in `strand watch`, logs
    and (M5) the inspector: runtime faults, write errors, and the values
    and notices of cells kept over a changed default (the overlay's rows
    too).
  - `trigger: hover` popups (M6, m6-w2-surface; spec.md 6.3, the
    owner's ruling). The router opens and closes each popup whose
    `Prop::Trigger` is `hover` by an `Intent::Write { prop: Open }`
    (logic sets `NodeState::open`, which drives the popup's `open`
    binding; `Instance::write` accepts it for such a popup only). It
    opens one while the pointer is on the element it is anchored to
    (its tree parent; the innermost such element, after
    `Prop::Delay`), closing at once the one open from the same surface
    (the nearest surface node above it). It keeps one open while the
    pointer, on any surface, is on that element or on a node whose tree
    ancestors include it (the popup, popups opened from it), or while a
    left press that went down there is held, and closes it
    `Prop::Grace` (default `strand_render::HOVER_GRACE`, 200 ms) after,
    the popups opened from it first. Pick mode opens nothing and ends
    the graces; leaving it hit-tests the pointer again, so the hover
    (and the popup under it) comes back at once. The router's open
    popups follow logic: `Router::observe` matches each `SetProp { Open
    }` on a popup against the values it wrote and not yet saw, in
    order, and takes any other value as logic's own (a reload
    remounting one open or closed); only live `trigger: hover` popups
    stay its own (a reload that drops `trigger` hands one back to the
    program). (m6-owner-fixes) The program's `open:` on such a popup is
    lowered to `Prop::Hold` (never written in source), which
    `Router::observe` follows: while it is true the router opens the
    popup at once (no delay, closing another open from the same
    surface) and runs no grace for it; once false, the pointer rules
    above apply. A `ClickAway` on such a popup's surface (`popup_done`)
    closes it in logic too, and it opens again only once the pointer
    has left its anchor. A `delay` or `grace` too long for the clock
    never comes due. `Router::next_wake() -> Option<Instant>` is the earliest
    delay or grace to end and `Router::tick(&mut dyn InputScene) ->
    Vec<Intent>` runs them. The host hands `next_wake` to
    `Renderer::set_input_wake(Option<Instant>)` after each routed event
    and each settle (which forgets removed popups), so the renderer's
    timer wakes the loop then, and calls `tick` on every wake before
    `Renderer::update` (`demo::text_ready`, `host_loop::woke`) and
    after a surface detaches (`host_loop::detached`), sending its
    intents like `handle`'s.
    `SurfaceSpec::hover_trigger` marks such a popup for the surface
    manager, which never grabs for it (it takes input).

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

- `Role` (49 Material 3 system roles, the fixed accents included, then
  the 13 hue roles of "The theming hub" below: 62),
  `name()` / `m3()`), `Palette` (every role a colour, `is_dark()`,
  `source()` / `with_source()` (provenance, not compared),
  `insert_into(&mut TokenTable)` writes the roots, their origins and
  the contrast pairs, `to_text()` / `from_text()` the persisted form),
  `Partial` (what an importer found; `fill()` gamut-maps and makes
  opaque what it was given, derives the rest by one table, then
  guards).
- `material::from_seed(Color, Options { variant, dark, contrast })`:
  `material-colors` 0.5, spec 2021 pinned (`material::SPEC`). Each
  thread keeps its last eight palettes (keyed by 8-bit seed, variant,
  mode and clamped contrast); `material::forget_kept_palettes()`
  (hidden, for the timing benches) drops this thread's, so a gate
  times the solve, not a kept palette.
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

#### The theming hub (m5-tools, owner-approved 2026-10-11)

> **Built in M5 wave 2 and merged on `laptop/m5-w2`** (decisions.md
> m5-integration-w2). The owner approved the design on 2026-10-11, and
> the integrator took this text into force at wave 1's integration
> (decisions.md m5-integration-w1). The palette half (S-palette:
> `Role::COUNT` 62, extras, identity, the reserved names, the `json`
> and `base24` writers; decisions.md m5-palette) and S-tools' half
> (templates, `export` offline and over the socket, the `file` element
> and its writer; decisions.md m5-tools, wave 2) are described below as
> built. `strand new` and `strand report` were built in a later slot
> (decisions.md m5-tools, "`strand new` and `strand report`"). Two streams
> build it (m5-plan, Streams): **S-palette** owns the strand-theme and
> strand-scene parts, **S-tools** the strand-compiler, strand-core and
> binary parts. The design and its reasons are in decisions.md m5-tools
> ("the owner-approved theming design" and the paragraphs after it).

- **strand-theme.**
  - `Role` gains 13 hue roles after the 49 Material 3 roles: `Red`,
    `Orange`, `Yellow`, `Green`, `Cyan`, `Blue`, `Magenta`,
    `BrightRed`, `BrightYellow`, `BrightGreen`, `BrightCyan`,
    `BrightBlue`, `BrightMagenta` (`Role::COUNT` 49 → 62; `Role::HUES`,
    `is_hue()`; `m3()` becomes `Option<&'static str>`, `None` for a hue
    role).
  - `Partial::fill` derives a missing hue by `blend::harmonize`
    toward the accent (≤ 15°) at tone 80 dark / 40 light; a missing
    bright is its base ± 0.08 OKLCH L. Given hues are never changed,
    and hue roles are not contrast pairs.
  - `Palette` gains `extras() -> &[(String, Color)]` (source order,
    snake_case dotted names; identity names get a trailing `_`) and
    `name()`, beside `source()` and `is_dark()`.
  - `to_text`/`from_text` write and read a `name <text>` line and
    `extra <name> #rrggbbaa` lines; older files derive the hues and
    read the name as the source string.
  - Each importer fills extras (Catppuccin's 26, base16/base24's
    `baseXX`, matugen's non-role keys, W3C colour tokens,
    `source_color` for material).
  - `export::{json(&Palette, &TokenTable) -> String,
    base24(&Palette, &TokenTable) -> String}` are pure writers; the
    `Palette` gives identity and source, the resolved `TokenTable`
    (overlays included) every value, so `strand set` reaches both;
    `base24` follows the slot table in decisions.md m5-tools ("the
    CLI"), and `json` the value rules in "template expressions and
    formats".
  - `image::Quantiser::read_only(dir)` (S-tools): reads the
    seed index in `dir`, never writes it; the offline mount's.
  - Owner: S-palette (hue roles, `Role::COUNT` 62, extras, identity,
    the persisted file's `name` and `extra` lines, the harmonised
    derivation, every importer's extras, and the `json` and `base24`
    writers).
- **strand-scene** (S-palette). `TokenTable` gains the hue roots and reserves
  `palette` as a first segment for extras and identity (read only by
  templates for now). The roots are inserted like every role
  (`HUE_ROOTS` names them); the identity and extras are held apart from
  the tokens, in `TokenTable::palette: Vec<(String, PropValue)>`
  (`name`, `source`, `dark`, then the extras in source order, written by
  `Palette::insert_into`), read with `palette_value(name)`, so they
  never spring, are never frozen and no token expression reads them;
  `PALETTE_SEGMENT` and `is_palette_path` name the reservation, which
  the checker reports as `check::reserved_token` (decisions.md
  m5-palette). No new `TokenMethod`: the template-only methods
  (`.red()`, `.green()`, `.blue()`, `.opacity()`, `.light()`,
  `.dark()`) never reach the render thread.
- **strand-compiler** (S-tools; the parser's `top_file` arm is a small
  change S-lsp reviews, since the tree-sitter grammar follows it). As
  built in M5 wave 2 (changed from the first text with the code,
  decisions.md m5-tools, "the interfaces as built"):
  - `Instance::palette(&self) -> Result<Palette, Error>` evaluates
    `use palette` with no fallback (built-in only when the program
    has none; the offline mount's pinned palette when it has one); a
    pending wallpaper gives its last value if it has one, else
    `instantiate::theme::PALETTE_PENDING` (`is_pending(&Error)` tells
    it), the one sentinel the outputs, the IPC request and the offline
    mount share. `token_table` still swallows a failing palette for the
    screen. `Instance::tokens_now() -> Result<TokenTable, Error>` is
    the current table, overlays included, as the last tick resolved it.
  - Mounting with a pinned palette root (the persisted palette, for
    the offline mount), and with a read-only `ThemeHost`
    (`Storage::read_only(state_dir, config_dir)`, which also opens a
    read-only `PersistStore` and token overlay): it reads
    `palettes/palette` and the `Quantiser`'s seed index from the
    `palettes` directory but writes neither (`remember_palette` keeps
    the palette in memory only; `strand_theme::image::Quantiser::
    read_only(dir)` reads the index and stores no seed), since
    `Storage::palette_dir` would otherwise give it the real directory
    and an offline export would write `palettes/palette`.
  - The other mode: the theme's `use palette` and `use tokens`
    evaluated with every read of `system.dark` answered by its
    negation, with no fallback. It evaluates in an other-mode child env
    with fresh slots for every top-level `let` that the theme reaches
    and that reaches `system.dark` (the static dependency graph,
    through `fn` bodies), since a `let` is one shared slot already
    computed with the real `system.dark`; states, settings and
    `persist` cells stay shared; the env flags the service read (and
    `default_palette`) to negate `system.dark`. It never calls
    `ThemeHost::remember_palette`, never reads or sets `last_palette`,
    and stores no `Quantiser` seed. The runtime token overlays apply to
    its table as to the current one. It is one counted memo per
    instance (in place of the first text's `Instance::other_mode(rt)`):
    `Instance::hold_other_mode() -> OtherModeHold` takes a hold
    (`OtherModeHold::memo() -> Memo<TokenTable>`), and
    `Instance::release_other_mode(OtherModeHold)` lets go of it on the
    program it was taken on, so a hold from before a reload releases
    nothing the reloaded program holds; `other_mode_holders()` counts
    them, and `other_table() -> Result<Option<TokenTable>, Error>`
    pulls it now under a hold of its own (`Ok(None)` when the theme
    gives the same mode either way, `instantiate::theme::
    other_differs`). Each mounted output whose template reads `.light()`
    or `.dark()` holds it, and so does an IPC `export` request whose
    template does, until its reply; made with the first holder and
    dropped with the last, it writes no cell, tracks what only the
    other mode reads (imports, settings fields, a pending image's
    `Async`) as any memo does, and is pulled on the logic thread after
    the tick's diff, once per change for all outputs.
  - `vm::theme::persisted_palette(dir) -> Option<Palette>` is the
    `pub` path for the persisted palette.
  - `template::Template::compile(file, src) -> Result<Template,
    Vec<Diagnostic>>` and `Template::compile_expr(file, &ast::Expr)` (a
    `text:` prop) produce a pure IR (text runs and compiled holes;
    token-only scope; the template-only methods `.red()`, `.green()`,
    `.blue()`, `.opacity()`, `.light()` and `.dark()`;
    `reads_other_mode()`; `token_paths()` for the check).
    `Template::render(&self, current: &TokenTable, other:
    Option<&TokenTable>) -> Result<Vec<u8>, Vec<Diagnostic>>` is pure
    and `Send`, over the tick's frozen token tables (decisions.md
    m5-integration-w1).
  - The `file` element in the schema, and at the top level in the
    parser (grammar.md `top_file`; every other top-level element keeps
    `syntax::misplaced`), checked by `check/files.rs`: one of
    `template:`/`text:`, `run:` against `permit exec`, `when` blocks
    that change its props (the source keeping its kind), a `text:`'s
    tokens. The HIR is an element of kind `Builtin("file")` (its `when`s
    its children; a top-level one in `FileHir.outputs`), with
    `Program.file_texts` (each `text:` compiled, by its value's span)
    and `Program.file_templates` (the literal `template:` paths);
    lowering makes it `lower::ElementKind::File(Arc<FileOutput>)`, its
    path and its props in source order, each under its `when`'s
    condition. `check::files::templates(&Program, Option<&Path>) ->
    Vec<Diagnostic>` checks literal template paths against their files
    (read, compiled, tokens known); `strand check` and the LSP run it,
    `strand run`'s loader does not (a template is watched, and its
    errors are its output's notice).
  - (M6, m6-owner-fixes, bug 8) `check::assets`: `local_source(kind,
    source, config_dir) -> Option<String>` is an `image`'s, `svg`'s or
    `lottie`'s relative file made absolute under the config directory
    (`Storage::config_dir`), which instantiate applies to every value
    of those nodes' `source` prop, literal or computed; and
    `check(&Program, Option<&Path>) -> Vec<Diagnostic>` warns
    (`check::missing_file`) on a literal source that names no file (an
    `image`'s only when `strand_scene::source_is_file`, so never an
    icon name). It stats files, so `strand check` and the LSP run it
    beside `check::files::templates`.
  - Outputs are pulled, not pushed in `Update` (in place of the first
    text's `Update.outputs: Vec<OutputChange>`): each mounted `file` is
    a content memo with equality cut-off (path, template, `run:`, the
    current table, the held other mode, and one shared no-fallback
    palette check), and `Instance::take_outputs() -> Vec<OutputChange>`,
    called after the tick's diff is sent, renders the changed ones on
    the logic thread and hands them out: `OutputChange { path:
    Option<PathBuf>, file, span, content }`, the content
    `OutputContent::Write { bytes, run: Option<Vec<String>> }` or
    `OutputContent::Failed { diags, template: Option<(name, text)> }`
    (the diagnostics point into the template when it is given). A
    pending palette hands out nothing; one path mounted from two places
    is `file::twice` on both. `Instance::output_count()` counts them,
    and `Instance::output_targets(each: impl FnMut(&Path))` names the
    paths they write now (each output whose last handed-out change was
    a write; a removed `file`, a changed path and a failing render have
    none), so the writer drops what it still tries elsewhere.
- **strand-core** (S-tools). A read-only `PersistStore` (reads cells, never
  writes) for the offline mount.
- **strand (binary)** (S-tools).
  - `strand export json | base24 | --template <file> [--palette
    <expr>] [-o <file>]` (`src/export.rs`).
  - The `export` IPC request: `Request::Export { kind, template:
    Option<String>, name: Option<String> }` in `strand_core::ipc`
    (`export{kind, template?, name?}` on the wire: the template's text
    and its name for diagnostics travel, `--palette` never does) with
    its `parse_m5` arm, its `Shell::request` arm and its handler,
    `run/request/export.rs` (0a's v1 command list names it). It is a
    socket request only (no D-Bus method). It renders against the live
    table, overlays included, on the logic thread, through the same
    `export::produce` as the offline mount; when its template reads the
    other mode it holds it (`Instance::hold_other_mode`) until its reply
    is built, and one whose other palette is still loading waits (tried
    again after each tick's diff, at most 30 s, the deadline among the
    logic loop's wake-ups so an idle shell answers). The CLI falls back to
    the offline mount when no shell answers, when the shell predates
    `export`, or when the template is too large for one request line.
  - The output host (`run/outputs.rs`) lives on the logic side. After
    the tick's diff is sent, logic calls `Instance::take_outputs`, which
    renders each changed output over the tick's token tables, current
    and, when the template reads it, the other mode; a render error is
    that output's notice and writes nothing. The rendered bytes go to
    the file output writer thread (`strand-files`, Threads), so logic
    never waits on a disk or a child process. After the changes, logic
    sends the writer the outputs' targets when they changed
    (`Instance::output_targets`), and the writer drops the retries of
    every other path (spec.md 9.7, "Removing a `file` stops its
    updates"); a `run:` due for bytes already written there still runs,
    so its consumer hears of the file it holds. No main-side channel is added: main never sees
    an output.
  - The writer thread writes atomically (temp file and rename,
    following symlinks and keeping the file's mode; the latest bytes per
    path win), and Strand's own writes are registered with the watcher
    in advance (`Live::own_writes`, an `OwnWrites` hook the binary takes
    from `Worker::own_write_hook`; the link and the file it names both).
    It keeps a manifest of written files in
    `$XDG_STATE_HOME/strand/files` (a `#` comment line, then one
    `<hash> <path>` line per file, the hash FNV-1a 64 of the bytes as 16
    hex digits; the path is the file written, links followed, and the
    hash only spares rewriting the manifest), and refuses unowned and
    `.strand` targets, a read-only file and a link to a missing file,
    each judged on the file a link names, not the link. The thread ends
    when the shell's `Files` is dropped (a `Stop` job: its runs'
    threads hold senders of its queue). A refused or failed write is
    tried again (after 1 s, doubling to 10 s) until it lands or newer
    content replaces it; an unowned target's notice offers `[replace]`
    (`overlay::Click::Replace`), which hands the file to Strand.
  - It runs `run:` with a 200 ms trailing debounce, at most one run
    per file plus one queued, and a 10 s timeout that kills the
    process group. Its stderr is read without blocking while it runs
    and for 100 ms after, never to its end of file, so a process it
    backgrounds never holds the writer. Each outcome goes back to logic
    through the shared outcome list the logic loop drains when pinged: a failure (a refused target, a
    failed write, a failing `run` with the last 4 KiB of its stderr)
    becomes an overlay notice and a `strand watch` event through
    `Shell::broadcast`, and the last good file stays.
  - The CLI's `export` row (in `src/entry.rs` after the wave 1 → 2 move)
    reads "write the palette: json | base24 | --template <file>
    [--palette <expr>] [-o <file>]" and dispatches to
    `cli::export_cli`; the `new` row ("write a starter shell into [dir]
    …") dispatches to `cli::new_cli` and the `report` row to
    `cli::report_cli`. `entry.rs` serves every one-shot command (the
    socket's, `new`, `compositor-rules`) from one table
    (`entry::one_shot`), so no listed command answers "not implemented
    yet".

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
  `rt.after/every(_dyn)`, `on change` is `rt.on_change_each(_after)`
  with each target paired with the identity of the keyed object it reads
  (none for a key, a field holding it, or a target with no such object):
  it fires when some target changes while its own identity stays (no
  firing on a sink switch; `rt.on_change_keyed` is the one-target form);
  `on change` handlers run after the
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
  (`persist` in `strand_core::persist::state_dir()`, by default
  `$XDG_STATE_HOME/strand/persist`, one file per cell path; its writes go
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
  the diagnostics of the whole attempt; a commit's sources are stored
  under `Cache` (`$XDG_CACHE_HOME/strand/last-good/<config hash>`) keyed
  by their hashes, `COMPILER_VERSION` and `Schema::fingerprint()`, and a
  config broken at boot starts from them. The write is `store_cache()`'s
  (a no-op without a commit since the last call; `cache_error()` reports
  its failure), which the compiler worker calls after sending the build
  on, so the disk write is never on the path from a save to its pixels
  (decisions.md m5-open-reload).
- **Diagnostics** (`strand_compiler::diagnostic`): `Diagnostic { severity,
  code: &'static str, message, labels: Vec<Label { file: FileId, span,
  message, primary }>, help: Option<String>, suggestions: Vec<Suggestion {
  file, span, replacement }> }`, built with
  `Diagnostic::error/warning/note(code, msg).with_label(span, msg)`
  (M6: `Severity::Note`, which is not a problem and never a load notice:
  only `check::names::notes` makes one, the LSP sends it as
  Information and `strand check` counts it apart from errors and
  warnings),
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
  `compile_with(&map, &schema)`. A `service … from
  dbus|dir|file|listen|poll` declaration's source is a constant the
  checker evaluates (`hir::ServiceDecl::spec`, a `hir::SourceSpec`;
  services-round: `SourceSpec::Dir { path }`; fields carry their key
  path, and a field's type may hold the builtin `File` record and the
  `Date` it carries, `check::tree`'s `DOCUMENT_RECORDS`, besides
  declared `type`s); `check::dbus::check(&hir::Program, &dyn Introspect)` compares
  its `dbus` fields with an object's introspection (`Introspect::
  properties(system, name, path) -> Result<Vec<BusProperty>, String>`;
  the caller brings the bus: `strand check`, the loader, the LSP), and
  (M6) its actions (`hir::ServiceDecl::actions`, each a
  `hir::ServiceAction { name, params, method }`) with the object's
  methods (`Introspect::methods`, `BusMethod { interface, name, inputs
  }`). A top-level `action` is `hir::Item::Action(hir::ActionDecl {
  def, params, argv: Vec<ArgvPart> })`, `ArgvPart::Text` or
  `ArgvPart::Param(index)`, with `ActionDecl::shown()` the line `strand
  check` lists (`argv` empty when the command was refused, `permit
  exec` included); a call of it is `Callee::Action(def)`.
  (M6, owner testing Gaps 7) A component that reads `screen` (or calls
  one that does) has `hir::Component::screen: Option<LocalId>`, its
  hidden parameter, and `hir::Program::call_screens: BTreeMap<NodeIdx,
  LocalId>` gives each call of such a component the local whose
  `screen` it passes (lowered into the call's arguments, so the
  instance needs nothing new). (M6, owner testing Gaps 8) A call's
  handlers and `id:` are its root element's: `lower::Component::root:
  Option<NodeIdx>` is the root element of a one-element component, the
  call's `lower::Element::call_handlers: Arc<Vec<Node>>` are its block's
  handlers, run in the caller's scope and attached to the root's scene
  node once it mounts, and `vm::Env::adopt_node(idx, state)` gives the
  root the call's element state, so `c.hover` is the root's.
  (M6, decisions.md m6-w2-language "the same name in two files")
  `check::names::notes(&hir::Program) -> Vec<Diagnostic>` gives the
  `check::same_name` note at each declaration of a component, type, enum
  or `fn` whose name more than one file declares; `strand check` and the
  LSP call it as they call the paths check, and a load never does.
  `ast::Element { qual: Option<Ident>, kind, … }` holds the file of a
  `menu.Row` call. `hir::Program::same_named(DefId)`,
  `::qualified_name(DefId)` and `::file_stem(FileId)` answer which other
  files declare a name and how a use would write it. A shared type or
  enum keeps its name in the `TypeTable` and carries `ty::Home { file,
  shared }`: `TypeTable::show` writes it `file.Name`, `TypeTable::stable`
  writes every config type `file.Name` (the key cells' types are kept
  by across reloads), and `RecordDef::same_record`/`EnumDef::same_enum`
  match a value's type by home file and name when a reload translates
  it. A shared component's `lower::Component::name` is `file.Name` for
  display (state paths through `lower::Program::owner_name`, the
  inspector's scope, a token's origin `component file.Name`), and
  `lower::Program::owner_key` (`module.Name`, always) keys its cells and
  reload identity, so a name becoming shared re-keys nothing (a unique
  one moved to another file is found by name: `Identity` counts each
  component name's files, `Ctx::moved_from` gives the old file's key,
  `ty::CellTy` compares a cell's type in its stable and shown
  forms, and two unshared types of one name are one); a
  persisted path that changes form moves (`instantiate::mount`'s
  `moved_path`/`migrate_persisted` on a cold boot, `Carry::dropped`
  removed after a reload's old instance goes). (Review of 2026-10-14)
  The checker resolves a global through `Checker::global(name, span,
  check::names::Want)`, rules 1 to 3 over the declarations the position
  takes, and a component token path through
  `Checker::component_token_path`. `check::names::{is_identifier,
  nameable, listed, naming}` are public for the language server's hover
  and completion, which also reads `ctx::Context::call` (an element's
  qualifier and kind span) to take the call's component from
  `program.refs`. The tree-sitter scanner gains the external token
  `_qualified_dot`. (Second review of 2026-10-14) An element's or a
  type's `q.Name` goes through `Checker::file_for_name` (only a local or
  file-level `q` blocks it), an expression's also through
  `Checker::member_of_other`; `check::names::naming(stems, form,
  hidden)` takes the stems a use cannot write, and
  `hir::Program::unqualifiable`/`can_qualify` carry them to the note,
  hover and completion. `Ctx::moved_from` takes a call's cells from the
  one old file that no longer declares the component's name,
  `Ctx::take_parked_moved` from a parked bar's, and
  `Identity::take_over_moved_bodies` gives a pasted-then-cut
  component's body the cut one's identity (only when the survivor had
  no calls and the cut one had some, by `Identity::calls`). `vm::SettingsSlot::types`
  holds each field's `ty::CellTy` and the type string its handle holds.
  A persisted value is migrated between path forms only on a fresh
  mount (`Ctx::reloading` false, no cell carried).

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
  their own declarations shadow, services included: (M6, the owner's
  ruling, spec.md 4.2) a config `service` named like a builtin or
  contributed service shadows it with the `check::shadows_builtin`
  warning, the runtime's own reads still reaching the builtin (the
  `*_builtin` host methods below), so a crate that adds a service name a
  config already declares (M6 wave 2's `session`) leaves that config
  loading (decisions.md m6-w2-language, m6-integration-w2). An element's positional names the prop it fills,
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
  Member(type, member), Action(type, member), Function(name), Value(name),
  Method(type, method), Element(kind), Prop(kind, name) (also `on event`,
  `stroke.dash` and scope names; group docs reach their elements),
  Token(path)}`; (window) a record's method or action is its `Member`'s
  unless a field shares its name (`fullscreen` and `fullscreen()`), when
  it keeps its own as `Action`: `Schema::method_doc(type, name)` reads
  either (the reference's method rows, completion), and the LSP hover of
  such a field shows both; `RecordDef::doc` mirrors `DocKey::Type`;
  `Schema::token_doc(path)` falls
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
- **Time-bound values** (m4-runtime F1). `t`, `wave(…)`,
  `noise(…)` and a `letters` letter's `index` and `count` (bound at
  mount by `builtins::scope_value`) travel like tokens: `Value::Time`
  holds a `strand_scene::TokenExpr` with time leaves (`Time`, `Wave`,
  `Noise`, `Index`, `Count`: `strand-scene`, "M4 vocabulary"), so
  `t * 20deg` or `index * 0.1` stays an expression (`Value::symbolic`
  picks `Time` or `Token` by `TokenExpr::reads_time`, and
  `Value::symbolic_from` keeps `Time` when an operand was); arithmetic builds the tree as
  `builtins::binary` does for tokens (`%` is `BinOp::Rem`), colour
  methods on one stay symbolic, and `convert` maps it to
  `PropValue::Token`, or to a `Template` slot inside a composite value
  (numbers in `PropValue::numbers_mut` order). Render evaluates it per
  node per frame (`strand-render`'s `time.rs`). `noise(x)` is a time
  value only when `x` is; of a constant it is `strand_scene::tokens::
  noise`. A time-bound value reaching logic is an error value
  (`vm::value::time_in_logic`): a comparison, `!`, `&&`/`||`, a
  condition, `match`, an index, a computing builtin (`join`, `pct`,
  `min`, …), a service method or action argument, or a handler storing
  it (`tests/vm.rs::time_values_in_handlers_are_error_values`). The old
  `lower::time_signal` warning is gone.
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
  - (history) `keep(rt, service, Option<Duration>)`: a `graph` of the
    loaded config keeps history of a value `service` gives, so keep it
    running while no reader is visible, sampling at that period then
    (`None`: let it go; with no reader it then stops `STOP_GRACE`
    later). The instance calls it on every boot and reload for each
    service it keeps, and with `None` once for one it no longer does.
    `sampled(rt, service) -> Option<Duration>` (untracked): how its
    values arrive now, `None` when each is exact (a visible reader holds
    it, or it reports its changes as they happen; the default),
    `Some(period)` when it polls every `period` hidden and kept, so a
    value of it may have changed any time in the period before.
    `SchemaHost` records them (`SchemaHost::kept()`,
    `SchemaHost::set_polls(service, Option<Duration>)`), `StoreHost` and
    `CustomHost` pass them to the client (`DynService::keep`,
    `DynService::sampled`; `CustomHost` says a `from poll` service polls
    every `every`), and `Composite` routes them as reads are,
    remembering where each service was kept so `None`, or a reload
    routing the name elsewhere, lets that host go.
  - (M6) A `from dbus` declaration's actions are
    `CustomService::actions` (`lower::CustomAction { name, method,
    arity }`, in declaration order) and methods on its record, so a
    call reaches the host as `action(rt, ActionTarget::Service(name),
    action, args, ctx)`; the strand host sends the custom service
    `strand_services::custom::CustomAction { index, args: Vec<Data> }`,
    and the service calls the method (`custom::Spec::actions`, each an
    `ActionSpec { name, method }`). A top-level `action`'s command is not
    a host call: the VM queues an `ExecRequest { action, argv }`
    (`lower::Program::execs` maps its def to its `ExecAction { name,
    argv }`), and after each tick's diff logic takes them
    (`Instance::take_execs() -> Vec<ExecRequest>`) and starts each on a
    thread of its own (`run/exec.rs`), one run per action at a time with
    one re-run queued. (flex) `ExecRequest` gained `reply:
    Option<vm::ExecReply>` and `ExecAction` gained `ret: Option<Ty>`
    for `action name(params) -> T [command]`: a call of one is an
    `Async<T>` whose request carries an `ExecReply` (the program `Arc`
    and `T`; `ty()`, `types()`, `answer(Result<Value, String>)`,
    `abandoned()`, `peek()`, `wait() -> ReplyWait`, the future `await`
    and loads poll; dropped unanswered, it abandons the reply). In a
    binding it is an `Op::AsyncSite` whose chunk ends with `Op::Exec`
    (the load's input evaluates the arguments, `Machine::call_args`
    returning no receiver, and its fetch asks for the command), as an
    async service method's ends with `CallMethod`; in a handler `Op::Exec`
    pushes the pending `Async`. Such a request is not a host call
    either: the strand crate's `Execs` runs it at once on a thread of its
    own (never queued), stdout piped and bounded by
    `strand_services::custom::MAX_DOCUMENT`, reads the output on the
    logic thread with `services::custom::read_output(types, ty, out)`
    (`from poll`'s `Document` and `coerce`) and answers the reply; each
    tick it ends the process group of every abandoned one (decisions.md
    flex). (flex review) The capture thread owns the command's end: a
    superseded call (its cancel flag), a timeout (`POLL_TIMEOUT`, 10 s,
    the read polled with a deadline) or too much output make it send
    SIGTERM to the group, wait `KILL_GRACE` with `waitid(WNOWAIT)` and
    then SIGKILL, before it reaps the child, so no signal reaches a
    recycled group; the answer's `Ended`/`Answered` carry the real exit
    code. Each answer is recorded (`ChangeRecorder::exec_answered`) as a
    `FETCH` named `action <name>`, its texts held as a call's are when an
    argument is not a text of the program, and the eval replay plan
    turns it into an `answer action <name> …` step, which the harness
    queues for that action's next call; a host that never takes the
    requests (a test) leaves the calls pending, and the eval harness
    answers one with nothing queued with an error.
    (lockmon, decisions.md lockmon) An action's run: `rec.running` and
    `rec.started` check as fields of the schema record `ActionRun`
    (`running: bool`, `started: Date?`, `action stop()`), its base the
    action's `ExprKind::Def` typed as that record (`Checker::action_run`,
    for a field read or a method call only); they lower to
    `Op::ActionRun(def)` (the record, read from the instance's
    `vm::runs::ActionRuns`: two signals per action, made by name when a
    program declares it, owned by no scope and carried across reloads
    with `Vm::set_runs`) and `rec.stop()` to `Op::StopAction(def)`,
    which queues an `ExecRequest` with `stop: true` (new field; empty
    `argv`, no reply) in order with the calls. The host says how each
    action stands with `Instance::set_action_run(action, running: bool,
    started: Option<chrono::NaiveDateTime>)` (local time; kept to the
    minute) and can read it back with `Instance::action_running`. The
    strand crate's `Execs` gives each command a `Halt` (an atomic ask,
    plus an eventfd for a detached command's thread, which sleeps on the
    child's pidfd and that wake rather than in `wait`): a stop makes the
    owning thread send its group SIGINT, SIGTERM after `STOP_GRACE` (2 s)
    while the leader lives (seen with `waitid(WNOWAIT)`), SIGKILL after
    another, then reap; `Execs::runs()` reports the actions whose run
    changed (a detached command, or any result call; the oldest start),
    which `run_execs` passes to `set_action_run` and wakes the loop for.
    A stop is not recorded (the replayed handler stops again); the
    commands' ends are `ACTION` records as before, and the eval replay
    plan turns each end into an `end action <name>` step. The eval
    harness counts a detached command as running from its call to that
    step or a stop (`stop <name>` in the trace's commands).
  - (M6, decisions.md m6-w2-language, a config `service` shadowing a
    builtin) `read_builtin(rt, service, field)`, `sources_builtin(rt,
    service, field)`, `acquire_builtin(rt, service)` and
    `release_builtin(rt, service)` are the runtime's own reads by name
    (the default palette's `system` fields, the `screens` that mount
    bars, `system.reduced_motion`): they reach the builtin service even
    when the program declares one of that name. Each defaults to the
    plain method, so a host without shadowing is unchanged; `Composite`
    routes them past its `CustomHost`, and `SchemaHost` reads the
    builtin's cells it moved aside. `lower::Program::declares_service(name)`
    says whether the program shadows `name`. A hold taken through
    `acquire`/`acquire_field` is released on the host that took it
    (`Composite` remembers it), so a reload that adds or removes a
    shadow lets go of the right one. `SchemaHost::declare` of a builtin's
    name moves the builtin's cells aside, and `stop` puts them back.
  - `read(rt, service, field)` and `call(rt, service, method, args)`
    (`fn` methods: `clock.format`, `calendar.days`, `workspaces.on`,
    (replayfix) `workspaces.slots`)
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
    One exception (m6-service-gaps, spec.md 8.1):
    `lower::Program::services_anywhere` is what any scope reads (files,
    components, surfaces, bodies, through `let`s and `fn`s), and boot
    holds each field of `instantiate::HELD_ANYWHERE`
    (`("clipboard", "history")`) read anywhere, with its service, at the
    top level for as long as the program runs, as bars hold `screens`,
    unless the config declares its own `service` of that name.
    (services-round) `lower::Program::members` holds the `(record,
    member)` pairs the program names anywhere
    (`Program::names_member`), for a host that changes what it offers
    other programs by whether a member is named at all (the
    notification server's inline replies).
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
  program; (history) each boot and reload first works out the series
  its `graph`s keep (`instantiate::history`: a graph's value chunk
  reading only services, file-level `let`s, `state`s and settings and
  constants, keyed by its code with names spelled out, spanning the
  longest constant `history`), keeps a ring per series in the instance
  (across reloads, freed when no graph shows it), recorded by an effect
  at file scope, binds `Prop::Backlog` on each such graph, and calls
  `ServiceHost::keep`; `Instance::histories() -> Vec<HistoryInfo { key,
  span, services: Vec<(String, Option<Duration>)>, points, oldest,
  ring: Vec<BacklogPoint> }>` says what is kept ((graph-follow) a
  series of a list value as one entry per item, its key the series'
  with `[i]` after it, which `seed_history` takes too);
  `Instance::set_history_clock(ClockFn)` times it on other clocks
  (tests, the eval sandbox) and `Instance::seed_history(key, &[
  BacklogPoint]) -> bool` puts points before a series' own (a glitch
  report's replay); `Instance::tick` reads the clocks first, and time
  the boot clock ran ahead of the awake one by 2 s or more is a gap in
  every ring; `Instance::tick(now)` (or `flush()`) runs the core tick and
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
    `set_size(node, w, h)` (layout facts for `self.width`), (M6)
    `set_box(node, LogicalRect)` (a fact with its place, for `id.box`;
    it sets the size too) and
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
    type, with its text, files or app id when it has them, and its
    identity last) and
    `Prop::Accepts` (from each `on drop` parameter's type), plus
    `Prop::DropRows` on every element with a direct `for` child,
    `on drop` or not, and `Prop::NotRow` on each of that element's
    children that is not one of the `for`'s rows; a
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
    second `lock` an error (`check::lock_twice`) and `layer`, `anchor`
    and `keyboard` on a lock warnings (`check::lock_prop`).
    (lockmon) A lock mounts like a bar: one instance per monitor its
    `screens:` picks (every monitor by default), keyed by `Screen` id
    through `mount_keyed`, each with its own `screen` and `let`s; its
    `state`s live once in a lock-level `Env` all instances share, so
    there is one password and one attempt. A lock `state` whose start
    reads `screen` or a lock `let` is the error `check::lock_state_screen`
    (it would differ per monitor). (lockmon review) The lock's timers
    and `on change` and service handlers, in its body and its elements
    (not in `if`/`match` branches, components or `for` bodies, which
    are each instance's with their `state`s), are taken out of the
    instances' tree (`instantiate::lock::lock_logic`) and mounted once,
    in the lock-level `Env`, when the first instance shows; they are
    suspended while none is shown and resumed after
    (`Ctx::lock_logic_shown`, `lock::LockLogic`), so a counter steps
    once for every monitor and `auth.submit` from a handler runs once.
    Element events (`on key`, `on activate`) stay per instance. Reading
    `screen`, a lock `let` or a lock element (or `play` on one) in that
    logic is `check::lock_state_screen` too
    (`crates/strand-compiler/tests/surfaces.rs::the_locks_timers_and_handlers_run_once_for_every_monitor`,
    `::the_locks_timers_and_handlers_cannot_read_its_screen`).
    `screens: focused`, or a pick no
    plugged monitor matches, mounts one instance on the focused monitor
    (its `screen` is that monitor). The lock-level `Env` is a fault
    bound (`Env::set_fault_bound`), so a runtime fault below it freezes
    only the instance it happened in, not its siblings or the shared
    `Env`; a fault in the shared logic or the `screens:` pick freezes
    the lock's own scope. `Instance::fault_nodes(&RuntimeError)` names
    the nodes a fault froze (its scope's nodes, else its node), which
    the binary matches to each monitor's instance, so a fault with no
    node (an `if` effect, the pick) is seen
    (`::a_lock_fault_with_no_node_is_seen_by_what_it_freezes`) (`crates/strand-compiler/tests/surfaces.rs::a_fault_in_one_monitors_lock_freezes_that_instance_only`).
    An `input` of a lock instance whose two-way `text` is a lock-level
    `state` (`Ctx::shares_text`; lockmon review: not a component's or a
    `for` row's own) carries `Prop::Twin` naming the same field in the
    other instances, so render's `Router` shares the
    in-flight text and caret among them
    (`crates/strand-render/tests/input.rs::twin_inputs_share_writes_in_flight_and_the_caret`);
    the caret shows only on the surface with keyboard focus.
    A lock from which nothing calls `auth.submit` (its handlers and
    timers, in any branch, and the components it shows) is the error
    `check::lock_no_auth`, judged only on a program with no other error:
    such a lock draws and never faults, so no fallback would ever show.
    A `popup` or `tooltip` never opens on a lock surface, so a call
    inside one does not count, and each is the warning
    `check::lock_popup`. One lock code is not the compiler's:
    `strand check` (the binary's `check.rs`, not the compiler, the LSP
    or reload) makes a `lock` the error `check::lock_no_helper` when
    `strand_auth::default_helper`, the lookup `strand run` uses, finds
    no helper; it is one error on the first `lock`, reported whichever
    file of the config is checked.
  - `compositor-rules`: a query over a `Build` listing the surfaces whose
    tree has `blur`, with their namespaces (`strand-<Name>`), for
    `strand compositor-rules` (S-surface owns it). It lives in the binary
    (`strand/src/rules.rs`, `blur_rules(&Build) -> Vec<Rule>`) and walks
    the lowered program's public tree: a layer surface's own `blur` gives
    `blur`, a nested popup's gives `blur_popups` on its layer.
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
atlas is capped at `AtlasConfig::max_bytes` of alpha (8 MiB by default,
a bound on hostile text, not the memory budget; glyphs that do not fit
are skipped), fonts at `MAX_FONT_PX` (512) and
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
per output and (window) `xdg_toplevel` windows, a 2–3 buffer shm pool
per surface with buffer age,
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
  Option<Instant>`, `frame_dropped(surface)`, `input(&InputEvent)`
  (main thread, for hit testing) and (flex) `cursor(surface) ->
  CursorShape` (the arrow by default; see Threads). The binary implements it on a wrapper
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
  arriving meanwhile coalesce into the next paint. Two exceptions: a new
  size or scale paints at once (the old frame's feedback comes back
  discarded and no longer matches), and a frame whose callback or
  feedback has not come in `THROTTLE_GIVE_UP` (1 s) no longer blocks:
  the next change paints anyway, counted in `Stats::throttle_given_up`,
  and a change refused while throttled arms a timer that marks the
  surface again at the give-up. The cost: a surface the compositor does
  not show (occluded, DPMS off) paints new content at most once a second
  instead of not at all (decisions.md, m4-audit round 7).
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
  (graph-follow) `PresentationClock::set_clock_id` also calls
  `strand_scene::backlog::set_presentation_clock`, so render puts frame
  times on the boot clock from the clock they are on.
- `MonitorId` is `"make | model | description"`, minus a trailing
  `" (<connector>)"` wlroots appends (a duplicate gets ` #2`, in plug
  order); `Screens::Named` matches it or the connector name. `SurfaceId`s
  are stable per (node, monitor), or (node, focused), while the monitor is
  remembered.
- A spec's `overhang` grows the layer size and moves the margins out
  (`placement::layer_config`), the exclusive zone grows by the overhang
  on its edge so margin + zone is unchanged, and the input region is the
  box inside it (`SurfaceInfo::input_region`; an OSD's is empty). (M6,
  m6-w2-surface) A painter may replace it (`Painter::input_region`,
  surface-local logical rects, applied by `sync_painted_input` at each
  commit and kept in `SurfaceInfo::painted_input`): render asks it for a
  `trigger: hover` popup that draws nothing itself, the boxes of its
  top-level elements that drew (a blur or backdrop counts), so the
  pointer passes through the rest (a travelling island); (growth) a
  content-sized panel or popup whose box is drawn short of its buffer
  (springing over a step, or held), that box; `None`, for every other
  surface, keeps the manager's region. A popup keeps its
  configure position (`Role::Popup::at`) and is placed again whenever
  its parent's origin changes, so `SurfaceHost::surface_placed` tells
  the host its new place after its parent's; render reports a popup's
  own box from both when facts are next taken. Render
  makes the overhang even on the axes the anchor leaves centred, so the
  compositor centres the box, and (M6, m6-w2-surface) no larger than the
  margin on a side the surface is anchored to, so its buffer never
  passes its output's edge (Hyprland constrains a popup to the monitor
  at the parent's origin). `LayerConfig::fit` clamps a layer surface's
  box to its output's logical size less its margins; the host passes the
  output's size to render (`Renderer::set_surface_bounds`), and render
  holds a content-sized surface's frame for the configure at a new size
  (`Renderer::set_resize_wait`, `strand_render::RESIZE_WAIT`; zero
  offline). A surface the GPU thread presents is held too (gpu-first
  review 2): the manager sets its new size without a commit (the GPU
  thread is its only committer), so when render hands the surface
  changes over (`Renderer::take_surface_changes`) it asks the thread to
  present that surface's last frame again (`GpuRequest::Commit`, once
  per size held for), whose commit carries the new size; the configure
  then comes as a CPU surface's does and the new layout is drawn at its
  new size. (47a3cd04 drew it at once into the old size instead: a
  frame or two shifted by half the growth and clipped on the far side.)
  A presented surface's held frame wakes the loop at the hold's end
  through `GpuHost::wait`, since the manager arms that deadline only
  for the surfaces it paints; a hold that runs out draws at the size the
  surface has. Spec changes render makes while the manager calls in (a
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
- (window) Windows (`NodeKind::Window` specs, `manager/window.rs`): an
  `xdg_toplevel` through sctk's `XdgShell` (which binds `xdg_wm_base`
  versions 1–6 and the `xdg-decoration` manager), `Role::Window`, made
  from `placement::window_config` (a `WindowConfig`: title, app id from
  `SurfaceSpec::window_app_id`, asked size, min/max sizes, overhang,
  decorations). Title, app id, limits and the decoration mode change in
  place (`reconfigure_window`); its window geometry is its box inside
  the overhang, as a popup's. Its configure takes the compositor's size
  on an axis when a state constrains it (maximized, fullscreen, tiled,
  resizing) or it differs from the size asked (`kept_size`, by
  `placement::window_kept_size(given, asked, constrained)`, the rule
  eval's stand-in compositor shares) and from every size of its own it
  committed since the last configure was acked (`Toplevel::echoes`,
  kept by `committing` as `send_geometry` sets the window geometry: a
  configure sent before the compositor saw the latest commit echoes
  one), keeps its
  own otherwise, and reports `SurfaceHost::window_state` on a change;
  `close` is the surface's `InputEvent::ClickAway` (after
  `SurfaceHost::window_closed`), as is `WindowAsk::Close`.
  `State::window_ask` sends move and resize with the
  diff's press serial (else the seat's last), and minimize, maximize
  and fullscreen (toggles) only where `wm_capabilities` lists them; a
  maximize, fullscreen or minimize asked of a window its spec opens but
  not made yet waits in `State::window_asks` and is asked at its
  creation, before the first commit. `placement::window_config` raises a
  `max_*` under its `min_*` to it (`xdg_toplevel.invalid_size` would end
  the connection). An output unplugged leaves its windows and their
  popups alone (`output_removed`: the compositor moves them). A window
  is never placed (no `surface_placed`: a compositor never says where it
  is), so neither is a popup in it.
- Popups (`NodeKind::Popup` specs): an `xdg_popup` (`xdg_wm_base` bound
  through sctk's `XdgShell`, versions 1–6) nested in a mapped surface of
  `SurfaceSpec::parent` (the one last pressed, when several), through
  `zwlr_layer_surface.get_popup` or the parent popup's or window's
  `xdg_surface` (a window's box being its window geometry),
  positioned by `placement::popup_config` (a `PopupConfig`: box size,
  overhang, anchor rect in the parent's window geometry, side and gap)
  with slide and flip; the gap is the popup's margin on the side facing
  its anchor, 6 px only while `SurfaceSpec::margin_set` is false, so
  `0` touches the anchor and a negative margin overlaps it (spec.md
  5.1; with `attach:` 0 while unset, a set one kept; a tooltip 4); its
  window geometry is its box, its buffer the
  box plus the overhang. Size or anchor changes reposition it
  (`xdg_popup.reposition`; made again below `xdg_wm_base` version 3).
  (m6-owner-fixes, owner-testing.md bug 6) The manager moves the box
  inside its parent's output along its anchor's edge itself
  (`placement::popup_shift`, through the positioner's offset), from
  where it placed the parent (`origin.rs`) and the output's logical
  place, so the compositor need not slide it; slide stays asked for.
  A configure that puts the box elsewhere than the positioner asked
  (`placement::popup_request`) counts in `Stats::popups_moved`.
  (M6, m6-w2-surface) It grabs by its cause:
  only when the press that opened it (`State::set_popup_cause(node,
  Option<u32>)`, from `SceneDiff::press`) is the seat's last button or
  key press and came within `GRAB_WINDOW` (500 ms), so a popup a hover,
  timer or IPC opened never grabs and a click's menu grabs however far
  the pointer moved before its surface was made; never under a parent
  popup that has no grab (xdg-shell allows a grab only under a
  toplevel, a layer surface or a grabbing popup), never for a
  `trigger: hover` popup (`SurfaceSpec::hover_trigger`), and never for a
  tooltip (`SurfaceSpec::tooltip`: no grab, empty input region);
  `Stats::grabs` counts grabs. "Grabbing" is what was sent, not what
  the spec asked for: only a popup made with `xdg_popup.grab` dismisses
  a grabbing chain outside its own first, makes its layer surface
  `exclusive` while open (`State::holds_keyboard_for_popup`) and takes
  the keys on it (the topmost grabbing popup gets a `KeyboardEnter` of
  its own; when the keys move on, the old target gets `KeyboardLeave`
  unless the new one is nested in it, and when the grab ends the surface
  with focus gets `KeyboardEnter` again). A popup opened with no grab
  takes no keyboard and leaves
  other popups open: one no press opened, or opened more than
  `GRAB_WINDOW` after it.
  `popup_done` is sent as `InputEvent::ClickAway { surface }` (before the
  surface goes), then the popup and the popups nested in it are destroyed
  (innermost first, as any surface's are), and it is not shown again until
  its spec closes. `Painter::blur_region` is read for the blur ladder
  (M4, below).
- **M4 additions** (docs/m4-plan.md). 0b landed the host hooks as no-op
  defaults (`compositor_caps`, `gpu_release`, `lock_changed(LockState)`,
  `LockState` beside `SurfaceHost`); the manager calls them as the
  streams below build their parts.
  - GPU hand-off (`gpu_handoff.rs`, feature `gpu`): `State::raw_handles(
    surface) -> Option<RawHandles>`, `State::hand_off(surface)`,
    `State::take_back(surface)` and the hook
    `SurfaceHost::gpu_release(surface)`; the rules are in
    "`strand-gpu`", "Surface hand-off".
  - Capabilities (`caps.rs`): the manager binds `wp_alpha_modifier_v1`,
    `wp_single_pixel_buffer_v1` and `ext_background_effect_manager_v1`
    when offered, looks up `ext_session_lock_manager_v1`,
    `wl_data_device_manager` and any `hyprland_*` global in the
    registry, and calls the new hook
    `SurfaceHost::compositor_caps(&CompositorCaps)` at the end of its
    first wakeup (after the binds' replies, before any surface is
    configured), and again whenever they change (the background
    effect's `capabilities` event can come later or change).
    `background_effect` is true only once that event names blur.
    `State::compositor_caps()` reads them. The binary's host hands
    `set_compositor_blur` to render and forwards `Painter::blur_region`.
    (M4 interaction-finish) While the compositor does not blur, the
    host sends logic `ToLogic::BlurFallback { surface, nodes, why }`
    (each `blur` node of a surface, its kind and whether it draws the
    tint; `caps::blur_missing`) when a surface's blur-region count
    changes, and logic makes a `strand watch` notice per box with its
    `file:line:col` (`Instance::origin`).
  - Poses (`manager/pose.rs`, wave 2): each frame, right after
    `paint`, it reads `Painter::surface_pose` and sets what changed as
    pending state (a `wp_alpha_modifier_surface_v1` multiplier, made with
    the first opacity; the viewport's destination, the logical size times
    the scale, which every surface now has whenever the viewporter is
    bound; the layer surface's margins moved by the offset,
    `placement::posed_margin`); it rides that frame's buffer commit, or a
    bare commit with no buffer when the paint drew nothing. A reconfigure
    keeps a pose's margins. `SurfacePose` is in the surface's own terms:
    the top-left corner stays and the offset moves it (compositors draw a
    smaller layer surface from its arranged box's top-left corner);
    render's `strand_render::pose` decides what a placement can take
    (opacity everywhere but a lock; an offset on an axis a panel or OSD
    is anchored to on one side; scale only with both) and folds the
    centre-keeping move into the offset. `State::clear_pose` drops a
    pose as pending state, for the GPU hand-off. Render holds
    `Removed`/`open: false` until an exit settles, so a delegated exit
    plays to its end.
  - Origins and submenus (`manager/origin.rs`, wave 2): the no-op-default
    hook `SurfaceHost::surface_placed(surface, (x, y))` says where a
    surface's buffer lies in the global layout, logical pixels (its
    output's position added), when that changes: a layer surface as
    `placement::arranged_area` arranges it (M4 interaction-finish: the
    wlroots order, our own exclusive surfaces on that output taken out
    layer by layer from `overlay` down; other programs' zones are not
    known), a popup from its configure's position under its parent's
    window geometry. `SurfaceInfo::origin` reads it; adding, resizing or
    destroying an exclusive layer surface, or one whose zone drops to
    none or that moves to another output, places the others on each
    output it took from or takes from again (`Surface::zone_on`). The binary's host turns a press into the tray's click point
    with it (`strand_services::tray::set_click_point`: the pressed node's
    bottom-left corner in the layout). A tray action takes that point
    only when called by a handler of an input event
    (`tray::TrayCall`, the service's action type, reads it in
    `from_call`; `tray::set_input_probe(strand_compiler::vm::in_input_handler)`
    at startup, `strand_compiler::vm::INPUT_EVENTS` naming the events a
    press just set the point for: `click`, `secondary`, `middle`,
    `activate` and `key`, a key press setting (0, 0);
    `ActionCall::input` records it); a timer's, an IPC write's, and a
    `scroll`, `dismiss` or `drop` handler's send (0, 0), never the last
    press's point. A popup nested in a popup opens
    beside its anchor (`PopupConfig::aligned`: level with the row's top,
    flipped in x), right unless its `anchor:` names left, top or bottom.
  - Solid surfaces (`solid.rs`): a single-pixel buffer scaled by the
    viewporter, for scrims and lock backgrounds; shm when the protocol is
    missing (1×1 with the viewporter, else surface-sized). A spec's
    `scrim` is a layer surface over the output's usable area (exclusive
    zone 0, all four anchors) on the panel's output, on the layer below
    the panel's: two layer surfaces on one layer stack in an order the
    protocol leaves open (sway 1.9 puts the older on top, the others the
    newer), so placement raises a `top` panel with a scrim to `overlay`
    and its scrim goes on `top`. It is the click-away catcher too when
    the panel has one (`strand-<Name>-click-away`, with its hole), else
    `strand-<Name>-scrim` with an empty input region. A popup's `scrim`
    is the same surface on its root layer surface's layer and output
    (popups stack above layer surfaces), taking no clicks (the grab
    closes it).
  - (services-round) Idle inhibition (`manager/idle.rs`):
    `SurfaceSpec::inhibit_idle` (`Prop::InhibitIdle`, `inhibit_idle:`
    on any surface) gives each of the node's Wayland surfaces a
    `zwp_idle_inhibitor_v1` from `zwp_idle_inhibit_manager_v1` (bound
    when offered): made once a layer surface, popup or (services-round
    review) lock surface is mapped, its first buffer committed (lockmon:
    sway 1.12 ignores an inhibitor made before; `session_lock.rs` syncs
    it as each lock surface's content goes in, so a lock's content holds
    it from its first frame) and when `apply_surface_change`
    turns it on in place, destroyed when it turns off and before the
    surface goes. `SurfaceInfo::inhibits_idle` says
    whether one is held.
  - Fillets: placement puts an `attach`ed box at its margin on that
    side from that edge, 0 while unset and negative over it (a panel
    anchored to it, its margins as set; a popup opening away from its
    attached side, its gap 0 while its margin is unset; m6-owner-fixes,
    owner-testing.md gap 26); the overhang render adds for the fillets
    grows the buffer only. Render's `fillet.rs` finds the target, draws
    it, grows the overhang along the edge and drops it past the edge,
    through one call each in `flatten` (the target's fill) and the two
    places a surface's overhang is set (`specs.rs`, `layout_pass.rs`).
    A clip group open above the target takes in its fillets and the
    square corners they meet at the corners its box reaches, bounded by
    that box (`fillet::corners`), and the group's other
    items are wrapped in clips of the box again when it closes
    (`Flattener::let_fillets_through`, `hold_to_box`; fillet-clip).
  - Blur ladder (`blur.rs`, `manager/effect.rs`): after each paint
    `Painter::blur_region` becomes a logical `wl_region`
    (`blur::region_rects`: inside every rounded shape, rounded inward,
    the middle one rectangle and about 1 px bands down each rounded
    corner), cached per surface and sent with
    `ext_background_effect_surface_v1.set_blur_region` (made with the
    first region) on that frame's buffer commit only when it changed,
    null once empty; nothing while the compositor does not blur, and
    sent again with a bare commit when it starts. `SurfaceInfo::
    blur_region` and `Stats::blur_updates` show it. Rung 3 is the
    binary's: the first frame that asks for blur on a compositor that
    cannot blur logs one warning with the reason
    (`caps::blur_fallback_reason`, naming `strand compositor-rules` on
    Hyprland) and sends it to `strand watch` as a notice
    (`ToLogic::Notice`), which logic keeps for the run: it is usually
    said at boot, before anyone watches, so each watcher that subscribes
    later gets it in the `notices` event that follows its `watch`
    answer.
  - Popups: a `popup` nested in a popup may open to the side
    (`anchor:`, right by default, flipping left), for tray submenus.
  - Session lock (`session_lock.rs`): `State::lock()` asks
    `ext_session_lock_v1` for a lock; the host hears
    `SurfaceHost::lock_changed(LockState)` (`Locked`, `Finished`, and
    `Unlocked` after an unlock). A `lock` spec gets one lock surface per
    output, including outputs plugged in while locked; config content
    goes on the focused output (lockmon: on each output an instance is
    pinned to, below), a single-pixel background in the lock's colour on
    the rest. (lockmon) Each lock instance is its own spec,
    pinned to its output (`wants`): every output with a live instance
    gets that instance's content surface, the rest the solid; a focused
    (unpinned) instance follows the focused output.
    `State::lock_contents()` lists the content surfaces,
    `State::lock_content_on(output)` names one, and `State::lock_content()`
    is the one on the focused output (else the first).
    `State::lock_instance_failed(node)` puts that instance's output back
    on the solid for the rest of the lock session (keys go to another
    content); the binary calls it for a faulted, unmounted or never-drawn
    instance (lockmon review: one whose fault nodes fall in it, or that
    has not drawn `INSTANCE_FIRST_FRAME` = 3 s after its own first
    configure while another has; a fault matching no content yet is
    kept until one appears;
    `crates/strand/src/run/lock.rs::tests::one_monitors_instance_fails_by_its_faults_and_its_first_frame`).
    A spec removed while locked leaves its content in place
    until a new spec wants that output; when no spec wants any output
    the one remaining content follows the focused output. Keyboard
    focus is the compositor's: sway 1.9 keeps it on the first lock
    surface mapped and moves it only when that surface goes away, so a
    click on another output's content reaches it as pointer input
    without moving the keys. `SurfaceHost::before_lock()` runs just
    before the lock request, the last moment the outputs show the
    desktop: the binary takes its pre-lock screen stills there
    (`crates/strand-surface/tests/session_lock.rs::a_lock_instance_on_every_output`). `State::unlock(strand_auth::UnlockToken)` is the
    only way to unlock; `finished` without `locked` is a diagnostic and
    the lock counts as not shown.
    As built (m4-lock wave 1): the module is
    `src/manager/session_lock.rs` (the manager's other concerns live
    there too). The content surface is a `Role::Lock` surface the host
    paints like any other (`SurfaceHost::surface_attached` with the
    spec's node, or `LOCK_FALLBACK_NODE` when `State::lock()` was called
    with no `lock` spec: the seam for render's built-in fallback);
    `State::lock_content()` names it. The other outputs get a
    manager-painted solid in `set_lock_color`'s colour: a
    `wp_single_pixel_buffer_v1` buffer, else a 1×1 shm buffer, scaled by
    `wp_viewporter` (a full-size shm one without it), from the scrims'
    `solid` helper (wave 2), and a keyboard focus on one of them is
    delivered as the focused output's content's (else the first's). A spec closing (`open: false`) or going
    away never unlocks: the surfaces stay and only `unlock` releases the
    lock; a closed spec re-arms it, so an open spec locks again only
    after it closed. `Finished` after `Locked` is answered with
    `unlock_and_destroy` (the compositor no longer uses the object) and
    asks for a new lock at once, once per lock session, so a session the
    compositor keeps locked gets its password field back; the host
    hears `Finished`, then `Locked` or `Finished` for the new lock. The tests take a session lock
    only inside the lock VM (`scripts/lockvm/scenarios/`).
    Nothing locks until `State::enable_session_lock()`: before it an
    open `lock` spec is a warning and `State::lock()` returns
    `LockError::NotEnabled`. The binary calls it only once `auth`'s
    tokens reach `State::unlock` (wave 2, `run/lock.rs`), so no build
    can take a lock it cannot release. A token given while the lock is
    pending is kept and spent when `locked` arrives (`destroy` would be
    a protocol error if `locked` is already on the wire). On a compositor
    without `ext_session_lock_manager_v1` (`State::session_lock_supported()`
    false) an open spec gets `LockError::Unsupported` and the host hears
    `Finished`, so logic stops counting the lock as shown; the binary
    says so at start, in its log and to `strand watch` (m4-audit).
  - Drag and drop (`src/dnd.rs`, included as `manager::dnd`): a
    `wl_data_device` per seat produces `InputEvent::Drag*` (external
    files, apps and text as `DropPayload::External`, read through calloop
    without blocking, at most `MAX_DROP_BYTES`). Two `SurfaceHost` hooks
    with defaults pull from the host: `drop_accepted(surface) -> bool`
    (the offer is accepted, copy only, exactly while it holds) and
    `drag_source(surface) -> Option<NodeId>`: when the held pointer
    leaves the surface a drag is in flight on, the manager starts a
    `wl_data_device` drag with the press serial and a MIME type private to
    this process, which enters our surfaces with no kinds and drops as
    `DropPayload::Node`; dropped elsewhere or cancelled, the origin gets a
    left release far outside it. `State::carrying_drag()` says one is in
    flight. As built (m4-interaction-finish), two more hooks with `None`
    defaults: `drag_data(node) -> Option<DropPayload>` (what the source
    gives other programs: the drag offers `text/uri-list` for files or an
    app's `.desktop` file, and `text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`,
    `TEXT` for text, paths or an app id, written through calloop in
    4 KiB steps; `dnd::exports_of` builds them) and `drag_image(surface,
    node) -> Option<DragImage>` (the drag icon, committed on a
    `wl_surface` of its own after `start_drag`, held where the source was
    grabbed). The private MIME type starts with `dnd::STRAND_MIME`, so
    another Strand process's drag is recognised and read through its
    exports as a `DropPayload::External`.
  - Copy and paste (`src/selection.rs`, included as
    `manager::selection`; m6-service-gaps): right after a key press
    reaches the host, the seat drains `SurfaceHost::take_clipboard_asks()
    -> Vec<ClipboardAsk>` (default: none). A copy is a
    `wl_data_source` offering the five text types and a private type
    (`STRAND_SELECTION_MIME` with the pid and the manager) set as the
    selection with that press's serial, its text written to readers
    through calloop without blocking; `cancelled` drops it. A paste of
    our own selection (its offer carries the private type) is answered
    from memory; another program's text is read through calloop
    (`MAX_PASTE_BYTES`, 1 MiB; `PASTE_READ_TIMEOUT`, 5 s) and sent as
    `InputEvent::Paste`. A keyboard's seat gets a data device too. The
    pipe reader and writer dnd and the selection share live in
    `manager/pipes.rs`. Neither runs for a session-lock surface, nor for
    any surface while a lock is asked for or held
    (`State::clipboard_shut`; review r1).
  - (m6-service-gaps, review r1) Each keyboard's keymap is compiled
    again with `xkbcommon` 0.8.0 (the version SCTK links) in
    `update_keymap`, once (`seat::compile_keymap`, without default
    includes or environment names) for both this and gap 31's layout
    names, into `seat::LatinKeys`: for a keymap with several
    layouts, each key's level-1 keysym in the first layout giving it a
    printable ASCII one. A key whose active keysym is a non-ASCII
    character carries that Latin keysym's name as `KeyInput::latin`
    (strand-scene; `None` otherwise), and `KeyInput::shortcut()` (`latin`
    else `name`) is what Render's `widgets::edit` matches, so Ctrl+V
    under `ru` is `v`; `name` (what `on key(k)` sees) is unchanged.
- **M6, gap 31** (decisions.md m6-service-gaps): a no-op-default hook
  `SurfaceHost::keyboard_state(Option<&KeyboardState>)` and `pub struct
  KeyboardState { caps_lock, num_lock, layout: Option<String> }`
  (re-exported from `lib.rs`). `seat.rs` keeps the keyboard that entered
  the focused surface, its keymap's layout names (`update_keymap`,
  compiled with xkbcommon 0.8, the release SCTK links, without default
  includes or environment names, the same compile `LatinKeys` reads)
  and its locked or held `Lock` and `Mod2`; it reports them when they change while one of ours has the
  keys (`update_modifiers`, `update_keymap`; an `enter` drops the
  keyboard's lock keys, stale from its last focus, and reports with the
  `modifiers` the compositor sends after it), and `None` on `leave`
  (a held one when it resolves as real), when that surface goes
  (`layer.rs`'s destroy, the lock's refocus) or the seat loses its
  keyboard. The binary's host forwards it to
  `strand_services::keyboard::focused`. (services-round) `KeyboardState`
  also carries `layouts: Vec<String>` (the keymap's names by index,
  empty for one it does not name) and `modifiers: strand_scene::Modifiers`
  (that keyboard's held, latched or locked Ctrl, Alt, Shift and Logo,
  kept per keyboard beside its lock keys), which the host maps to
  `Focused { layouts, alt, ctrl, shift, logo }`.
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
presentation global). M4 added `session_lock.rs` (`ext_session_lock_v1`:
a lock surface per output, hotplug included, the content surface, the
unlock), `scrim.rs` (solid buffers for catchers and scrims, with the shm
fallback), `pose.rs` (compositor-animated poses: alpha modifier,
viewport scale, margins), `effect.rs` (the blur region on
`ext_background_effect_surface_v1`) and `origin.rs` (each surface's
place in the compositor's logical layout, for `surface_placed`), and two
modules kept beside the manager in `src/` but part of it: `dnd.rs`
(drag and drop over `wl_data_device`), `selection.rs` (copy and paste
over it; m6-service-gaps, with `manager/pipes.rs`) and `gpu_handoff.rs` (a surface's
hand-off between `wl_shm` and the GPU thread's WSI).

### `strand-gpu`

(M4; docs/m4-plan.md wave 0c, from the wave-0 spike: decisions.md,
m4-gpu-spike; the backend landed in m4-gpu-w2, the bundled effects in
m4-gpu-effects.) The GPU backend is in every build and falls back to the
CPU (decisions.md, m4-owner). vello_cpu into `wl_shm` draws every still
surface; the GPU draws a surface while it animates a large area, and
shaders (decisions.md gpu-first). The binary starts the thread as the
shell starts, so the device and its pipelines are ready before the
first animation, and keeps it for the process; nothing in this crate
runs, and no Vulkan library is mapped, before `Gpu::spawn`.

**Boundary.** `strand-gpu` depends on `strand-scene`, wgpu 30 (`vulkan`,
`wgsl`, `std`; no default features), vello_gpu 0.3 (`wgpu`, `std`; no
`text`, since glyphs arrive as atlas images and paths), vello_common 0.3,
raw-window-handle 0.6 and pollster; on no other Strand crate and on no
Wayland crate. Its interface:

- `Gpu::spawn(waker: Box<dyn Fn() + Send>, opts: GpuOptions) -> Gpu`
  starts the thread `strand-gpu`, which creates the instance, adapter and
  device off the main thread (45–274 ms on hardware, about 100 ms on
  lavapipe), then draws a small frame offscreen (fills, a gradient, a
  layer, a clip, an image; (perf-gpu-retain) a radial gradient, a
  shadow in its even-odd ring, a backdrop blur and glass in an additive
  layer, and a second frame taking and drawing a snapshot) and records
  the present blit for both swapchain formats, waiting for the GPU,
  before it answers `Ready`: the pipelines and resources a first frame
  would build are built (gpu-first). Nothing it draws is kept but the
  pipelines. A failure there answers `Unavailable` as a failed start. `Gpu::send(GpuRequest)` never blocks; `Gpu::try_recv() ->
  Option<GpuReply>` is drained when the waker's ping fires on the main
  loop. Dropping the `Gpu` (or `GpuRequest::Shutdown`) drops every
  surface, pipeline, texture, the device and the instance, and the thread
  ends; while the shell runs the binary joins it after its `Exited`
  reply, never blocking on a device drop. At exit it is the one
  exception (decisions.md m4-audit, "the run ends the GPU thread before
  the surface manager drops"): `run` calls `GpuHost::shutdown(GPU_SHUTDOWN)`
  before the `SurfaceManager` drops, which blocks the main thread up to
  3 s joining every GPU thread, since a presenting thread holds the
  display and the `wl_surface`s it was lent (`RawHandles`) and drops its
  swapchains on them as it ends. A thread still running then (a hung
  driver) keeps the connection: the manager is leaked
  (`std::mem::forget`), not dropped under it, and the process exits.
  This is how the `RawHandles` contract (no swapchain outlives its
  `wl_surface` or display) holds at exit.
- `GpuRequest`: `Attach { surface: SurfaceId, handles: Option<RawHandles>,
  size, scale, opaque: bool }` (no handles: readback only), `Resize {
  surface, size, scale }`, `Release(SurfaceId)`, `Commit(SurfaceId)`
  (present the surface's last frame again so the compositor applies its
  pending state; unanswered, skipped when no frame matches the
  swapchain's size), `Frame(Frame)`,
  `Pass(PassFrame)` (a `shader` node's pass on a surface the GPU does
  not draw, or a bundled effect's: drawn offscreen at its size and read
  back), `Shutdown`. `PassFrame { key, id, size, pass: ShaderPass,
  then: Vec<ShaderPass>, globals: PassGlobals, input: Option<Arc<Pixmap>>
  }`: `input` is the pixels a `Content` or `Backdrop` pass reads (render
  rasterises them on the CPU when it asks: a filter layer's group,
  CPU-filtered, or what is drawn behind a backdrop), uploaded as
  `strand_input`; each of `then` runs on the one before's output
  (m4-gpu-effects). A surface the GPU draws asks for no backdrop's pass
  this way but one an offscreen group the CPU draws holds or reads
  (perf-gpu-retain: its frames draw the rest, `Op::Backdrop`).
  `GpuReply`: `Ready(AdapterInfo)`, `Unavailable(GpuError)`,
  `Attached { surface, mode: GpuMode }`, `Released(SurfaceId)`,
  `Presented { surface, frame: u64, at: Instant, waited: Duration,
  ran: Vec<(u64, Duration)> }` (`at` when the frame was submitted,
  before `present`; `waited` its time in the thread's queue and the
  block on the swapchain for its image; `ran` the GPU's run of earlier
  frames of the surface by frame number, from timestamp queries, two
  frames timed at once per surface, or this frame at zero on a device
  without them; the thread never waits for a presented frame's GPU
  work; m5-tools review rounds 2 and 3), `Pixels { surface, frame: u64,
  pixels: Readback, waited: Duration }` (`waited` its queue time), `PassPixels { key, frame, pixels }`, `Failed {
  surface, key, error }` (a frame or pass that could not be drawn, such
  as a shader that fails to compile on the device; the device stays
  up), `Lost(GpuError)`, `Exited`. A readback waits at most
  `HUNG_AFTER` (10 s) for its submission (m4 audit): past it the pass or
  frame answers `Failed`, then `Lost` ("stopped answering") and
  `Exited`, and the thread leaks the hung device instead of dropping it
  (a drop would wait for the queue), so a validated shader that never
  ends cannot leave render waiting on a reply. A hardware driver may
  reset the hung context first (ANV after about 6 s), and wgpu then
  panics in `poll`: the thread catches it, answers the frame or pass
  `Failed`, then `Lost` ("was lost") and `Exited` (m4-integration-w3).
  Every frame and pass is answered, a panic included. Render never runs
  a shader file whose pass or frame was answered `Failed` with kind
  `Lost` again in that process, whatever its uniforms, size or node
  (`ShaderResults::lost`; an edit is new code), so a looping file leaks
  at most one device (m4-audit round 5). The thread keeps at most
  64 compiled pipelines and 16 pass readback buffers (by size), least
  recently used first out.
- `Frame { surface, id: u64, size, scale, ops: Vec<Op>, uploads:
  Vec<Upload>, retire: Vec<u64>, clear: AlphaColor<Srgb>, snapshot:
  FrameSnapshot }` (`clear`: the
  background the frame is cleared to; no `readback` field, since an
  attached surface's mode decides and a pass on a CPU surface is its
  own `Pass` request: decisions.md, m4-gpu-w2) is what render lowers its
  display list into (`renderer/backend.rs`): fills and strokes of
  kurbo paths with peniko paints and transforms, images by texture id,
  clips (`PushClipEvenOdd` for a shadow's ring), blends and opacity as
  layers, and `Op::Pass(ShaderPass, bounds)`. `Upload`s carry atlas
  pages and cached pixmaps (images, gradients, masks: masks are
  always rasterised on the CPU) keyed by id; they live on the GPU until
  a frame lists them in `retire` (render retires an upload whose pixmap
  died or that no frame drew for 600 lowerings) or the device drops. The GPU thread builds the vello_gpu scene from the ops, so
  that work is off the main thread. (perf-gpu-retain) Three ops draw
  on the GPU what render drew on the CPU and uploaded:
  `Op::BlurredRect { rect, radius, std_dev, color }` (a box shadow:
  vello's blurred rounded rect, the primitive the CPU draws shadows with,
  one per quadrant when its corners differ, so no shadow pixmap or
  corner tile is built for a GPU frame); `Op::Backdrop { region, ops,
  pass, then, globals, key, reduce }` (a backdrop: `ops`, what is drawn
  behind it, balanced, from the identity transform, rendered into a
  texture of `region` whose origin is its corner, read as
  `strand_input` by `pass` and each of `then`, the result drawn over
  `region` at the identity transform under the clips and layers in
  force; each backdrop's render is submitted on its own, before the
  frame's; `reduce` (1, 2 or 4; review) draws it at that fraction of
  the region's size, rounded up, its passes run there with the
  uniforms given, and scales its pixels back up by exactly `reduce`,
  bilinearly from pixel centres with the edge pixels repeated, as the
  CPU draws a backdrop blur under about 0.2 Mpx; `key`, render's hash
  of its want's key (what is behind it, every group around what is
  behind it included, its passes, globals and `reduce`), its region and
  the pixels of each CPU-filtered group among its ops, or 0 when what
  is behind it runs passes: a backdrop with the key of one the
  surface's last frame drew is drawn from that one's pixels, nothing
  rendered or run). (review) The textures a surface's backdrops render
  and run their passes into come from a pool of its own, by size, each
  handed back once read (`pass::Pool`), so a backdrop drawn anew every
  frame allocates nothing once the pool has its two, what is behind it
  and its pixels, which swap roles each frame; the pool keeps what a
  frame handed back and frees the rest, and the last frame's pixels no
  backdrop of the frame keeps are the frame's to draw into. A surface
  holds two textures per backdrop drawn anew in its last frame and one
  per backdrop drawn from its last pixels, none once a frame draws no
  backdrop; and `Op::Snapshot { rect }` (the surface's
  crossfade snapshot 1:1 from the origin, nothing when none is held).
  `FrameSnapshot` is `None` (none held: one held is freed), `Take`
  (before the frame draws, the surface's last frame at the frame's
  size is copied into its snapshot; none if it has no such frame) or
  `Keep`. Passes inside a backdrop's ops are numbered after it
  (`Op::passes_in`: each before the ones inside it).
- `Readback` is `Bgra8Unorm` premultiplied rows (the `wl_shm` ARGB8888
  byte order), padded to wgpu's 256-byte row alignment, with its stride.
- (perf-bench) `record_frame_phases(on: bool)` and
  `take_frame_phases() -> Vec<FramePhases>` (`FramePhases { surface,
  frame, total, phases: Vec<(&'static str, Duration)>, uploads: (count,
  bytes) }`): while on, the thread keeps each frame's phase spans
  (upload, passes, build, texture, render, then submit and read for a
  readback or the present's acquire, submit and present; at most 4,096 frames,
  the oldest dropped), the same marks its debug log prints, for the
  animation bench (`strand-render/tests/anim_bench.rs`). Off by default;
  nothing in the shell turns it on.

**Thread.** One thread, started as the shell starts (the warm start,
`BackendChange::Start`) or on the first demand after a failure, and
ending with the device; one device per process, shared by every
surface. The adapter is
requested without a surface, so readback works whatever the WSI can do.
A software adapter (`DeviceType::Cpu`, lavapipe) counts as no device:
it would draw on the CPU and keep about 80 MiB mapped after the drop.
`STRAND_GPU_SOFTWARE=1` accepts it, and a user never needs it. The
lavapipe tier sets it beside `STRAND_REQUIRE_GPU=1` (CI's env, `run.sh`
and `ci.sh`). The adapter is requested with `PowerPreference::LowPower`
(gpu-first review; it was `HighPerformance`): a hardware adapter still
wins over a software one, and on a hybrid laptop the integrated GPU,
the one compositing, is chosen, so the device held for the session does
not keep the discrete GPU out of runtime suspend and no present crosses
GPUs. (The compositor's dmabuf-feedback main device would be the exact
choice; the adapter is requested before any surface exists, so it is
not used.) The thread waits on its
channel and on presents, never on a timer: it does not decide when to
stop.

**Promotion** (`strand-render/src/promote.rs`, a pure state machine per
surface; `Renderer` drives it and reports changes with
`Renderer::take_backend_changes() -> Vec<BackendChange>`, which the
binary carries out, and queues what it wants of the GPU thread itself
with `Renderer::take_gpu_requests() -> Vec<GpuRequest>` (passes and
readback frames): the binary starts the `Gpu` when requests come and
none runs, and hands every reply to `Renderer::deliver_gpu(GpuReply)`.
The binary calls both after every dispatch of its loop; taking the
changes also re-arms render's timer, since a reply or a detached surface
can bring the device's drop forward; decisions.md m4-gpu-w2):

- (gpu-first; decisions.md gpu-first, replacing M4's 500 ms rule) When
  a device can exist, whatever animates a large area draws on the GPU. A
  surface is promoted at the first large frame (at least 0.2 Mpx of the
  damage a CPU frame would have; render still diffs display lists on a
  promoted surface) of an animation: a frame with a spring in flight (a
  morph, a panel's entrance, a scroll, a theme crossfade), or the third
  large frame in a row, each within 250 ms of the one before, with none
  (a clock: shader time, particles). That frame follows a still one,
  so the backends' antialiasing difference is lost in the motion. A lone
  large frame (new content, a first frame with nothing moving) is not an
  animation, nor is the frame on which a surface's last spring comes to
  rest (m4-audit). A paint that damages nothing is not a frame (it
  commits nothing). A shader pass also needs the device, without
  promoting its surface (below).
- Demotion: once a promoted surface has been settled for 3 s
  (`promote::SETTLE_AFTER`), at a still frame; one that paints nothing
  goes back at a wake of its own, its settle window starting when its
  last hot frame was shown (`Presented`, or the attach). One wake ends
  an animation; an idle shell takes none. Settled is about damage
  (gpu-first review): a hot frame is a large one, or a small one with a
  spring in flight whose CPU frames were slow (the median of the last
  `promote::SPRING_SAMPLES` (5) small spring frames the CPU drew, once
  `SPRING_SAMPLES_MIN` (3) came, is `promote::CHEAP_FRAME`, 4 ms, or
  more: `Promotion::frame(now, damage, springs, cpu)` takes the CPU's
  raster time of a frame it drew over at most twice its own damage,
  `None` for a GPU frame, a repaint in full or one widened further by
  the buffer's age; gpu-first review 2: one slow sample, a preempted
  raster, no longer holds a surface); a small spring the CPU draws
  cheaply (a turning 80 px cover, a spinner) never holds its surface. A surface promoted again within `promote::AGAIN_WITHIN`
  (30 s) of going back settles `AGAIN_FACTOR` (5) times longer, 15 s,
  so repeated animations a few seconds apart keep the GPU without a
  CPU spell (and a full CPU repaint) between them. Clocks (time
  signals, shader time, particles, animated images) do not count as
  springs.
- The device is kept up for the process (gpu-first; M4 dropped it 30 s
  after its last use). `strand run` calls `Renderer::warm_gpu()` as it
  builds the renderer: render asks for the device and reports
  `BackendChange::Start`, and the binary starts the `Gpu` with nothing
  to send it (once any thread still ending has ended). A start that
  fails before anything shown needs the GPU is logged at `info` ("…;
  everything draws on the CPU"); a machine without a device draws as it
  always did. Render asks for no wake for the device, but one (gpu-first
  review 2): a kept-warm device that failed for a reason that may pass,
  or was lost, is started again once at its retry deadline
  (`promote::Device::retry_at`), so the next animation does not wait for
  it; a device that fails again is asked for on demand only, so an idle
  shell whose device never starts is not woken every 30 s.
- (gpu-first review) The device is asked for only where one can exist:
  `strand_gpu::device_possible(&GpuOptions) -> bool` is true with a
  software adapter accepted or a `/dev/dri/renderD*` node. Without one
  `strand run` calls `Renderer::gpu_absent(reason)` instead of
  `warm_gpu`: the status is `Unavailable { reason }`, logged once at
  `info`, and the device is `Absent` (`promote::Device::absent`), which
  never asks again. A start answered `Unavailable` of kind `NoAdapter`
  or `Software` makes it `Absent` too (that refusal holds for the
  process); other failures keep the 30 s retry. So a machine that can
  never have a device builds no Vulkan instance after the first refusal,
  or none at all.
- (gpu-first review) The Vulkan loader filter. Before any thread starts
  (a second `process_setup!` constructor, `.init_array.00101`, after the
  THP one) the binary sets `VK_LOADER_DRIVERS_DISABLE` to
  `strand_gpu::loader_disable(&GpuOptions, render_vendors, inherited,
  chosen) -> Option<OsString>`: the software drivers' manifests
  (`*lvp_icd*`, `*swiftshader*`) unless a software adapter is accepted,
  and, where every render node's PCI vendor was read
  (`strand_gpu::render_vendors() -> Option<Vec<u16>>`, from
  `/sys/class/drm/renderD*/device/vendor`, `None` when one is unreadable
  or a `/dev/dri/renderD*` node has no sysfs entry, a sandbox hiding
  sysfs: no hardware driver is then kept out), the drivers of the vendors
  with no render node (Intel's, AMD's, which link LLVM, NVIDIA's,
  virtio-gpu's) and of the platform GPUs (Asahi, Freedreno, Panfrost,
  PowerVR, Broadcom). It appends to an inherited list, and sets nothing
  when the environment chose drivers itself (`VK_DRIVER_FILES`,
  `VK_ICD_FILENAMES`, `VK_LOADER_DRIVERS_SELECT`). A loaded driver stays
  mapped for the process, so this is what keeps lavapipe's LLVM out of
  a shell whose only real GPU is Intel's. The variable is given back to
  the programs strand spawns as they found it
  (`strand_services::child::set_own_var`, read by `child_env`).
- A failed start or a lost device is recorded as `GpuStatus::Unavailable
  { reason }`. Every promoted surface goes back to the CPU at once (it
  cannot wait for a settled frame) with a full repaint. Render asks
  again (a promotion or a pass, not the warm start) at most once per
  30 s while demand lasts, until the device has
  been lost 3 times in the process (`promote::LOST_CAP`; a
  `GpuReply::Lost`: a hang, a reset or a panic; a failed start does not
  count). From then on the GPU is off until strand restarts: render
  never asks for a device again, promotes nothing, and holds
  `GpuStatus::Unavailable { reason: promote::GPU_OFF }`. The renderer
  lives for the whole process and survives reloads, so the count does
  too (m4-close-gpucap).
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
for readback-only, and for a lock surface); the reply's `GpuMode`
becomes `Backend::GpuPresent` or `GpuReadback`, and render draws the
surface's next frame on the GPU (for `GpuPresent`, after
`State::hand_off`). Until the reply comes the CPU keeps drawing:
promotion never stalls a frame. A surface the manager lends its
handles presents whenever the WSI can (Hyprland, sway on a GPU
renderer, any compositor offering linux-dmabuf to the device's WSI);
readback is for a lock surface, a surface an ending thread still
holds, and a compositor the WSI cannot present to (a pixman sway with
ANV).

**Frames.** In readback mode, `Painter::paint` copies in the pixels of
the frame it sent last (full damage) and lowers and sends the current
one when its records changed or it animates, so frames run one behind
and the main thread never waits on the GPU; until the first pixels come
the CPU draws the frame in full. The `Pixels` reply calls
`Renderer::deliver_gpu`, which marks the surface for the next `paint`
(decisions.md, m4-gpu-w2). In present mode the surface manager does not
call `paint` for that surface: the GPU thread's `Presented` reply is its
frame callback, the binary asks render for the next frame
(`Renderer::paint_gpu(surface, at) -> Option<Frame>`, `at` the last
present plus the output's refresh period, which `PaintTarget::time`
would have been) and sends it. One frame is in flight per surface.

**Surface hand-off** (`strand-surface/src/gpu_handoff.rs`). One
`wl_surface` moves between shm and the WSI; it is never recreated.
- `State::raw_handles(&mut self, surface) ->
  Option<strand_surface::RawHandles>` returns the connection's
  `wl_display` and the surface's `wl_surface` as `raw-window-handle`
  handles (the binary moves them into `strand_gpu::RawHandles`; none
  for a lock surface, which is never handed off). The handles are lent:
  from then until `State::take_back` a destroy of the surface goes
  through `gpu_release` as for a handed-off one, since the GPU thread
  may be building a swapchain on it before its `Attached` reply comes
  back (the device takes 100 ms or more to start). From then the
  manager attaches no buffer to it (bare commits acking a configure
  still go): a WSI with explicit sync (`wp_linux_drm_syncobj_v1`, Mesa's
  Vulkan on Hyprland) owns its buffers once it builds a swapchain, and
  an shm buffer with no acquire point is a protocol error; its frames
  resume at `hand_off` (the GPU's) or `take_back` (decisions.md
  syncobj-lend). It needs
  `wayland-backend`'s `client_system` feature, which strand-surface's
  `gpu` feature turns on (on by default). That feature switches the
  backend for every crate in the build to libwayland-client (57 kB of
  `.text`, and a library every Wayland desktop has); the CPU-only build
  keeps the Rust backend.
- `State::hand_off(surface)`: from then the GPU thread is the only
  thread that commits that `wl_surface`. The manager stops attaching shm
  buffers, requesting frame callbacks and calling `paint` for it. It
  still handles configures (the ack is sent, `surface_configured` tells
  the host, which sends `Resize`, the buffer scale or viewport
  destination and a popup's window geometry are set, and the next
  present commits the new size) and sets pending state (input and
  opaque regions, margins from placement, a popup grab's keyboard
  interactivity) without committing; the next present applies it.
  `State::sync_presented(surface)`, which the host calls after sending
  the GPU thread each frame, sets the regions that frame asks for
  (opaque, a hover popup's painted input, blur) the same way
  (decisions.md gpu-regions).
  Compositor poses are not delegated while presented: render paints the
  pose into the GPU frames, which are full frames anyway.
- `State::take_back(surface)` after the GPU thread's `Released` reply
  (also after an `Attached { Readback }` for lent handles, and when the
  thread has ended): the manager paints and commits a full shm frame
  (`age` 0) and resumes frame callbacks and pose delegation; a lent
  surface is no longer lent. Between `Release` and the next
  shm commit nothing commits that surface.
- When the manager has to destroy or recreate a handed-off surface
  (`Removed`, an unplugged output, a layer surface `closed`), it calls
  `SurfaceHost::gpu_release(surface)`, keeps the `wl_surface` alive, and
  destroys it at `take_back`. The swapchain is therefore always dropped
  before its `wl_surface`.
- In readback mode the surface is never handed off: the main thread
  commits, and poses are delegated as on any CPU surface.
- The binary's side (`strand/src/run/gpu.rs`, `GpuHost::pump` after
  every dispatch): `Attached { Present }` hands the surface off;
  `Presented` lets the next `paint_gpu` frame go (one in flight); a new
  buffer size or scale from the manager sends `Resize`; `Released`,
  `Exited`, or the end of a thread dropped while it had a surface's
  handles (handed off, or its `Attach` not answered yet) takes it back.
  A `Gpu` is started only once every dropped thread has ended (one
  device per process); requests meanwhile wait in the host. Once a
  thread has ended the binary calls libc's `malloc_trim(0)` and
  mimalloc's collect (the driver's freed arena pages).
  (gpu-first) There is no idle drop, so M4's `STRAND_GPU_IDLE_MS` test
  variable is gone; `Renderer::set_gpu_settle` shortens the 3 s settle
  window in render's tests.

**Shaders and effects.** `strand_scene::effect::Effect::Shader(ShaderPass)`
with `ShaderPass { code: ShaderRef, uniforms: Arc<[f32]>, input:
ShaderInput }`. `ShaderRef` is `Bundled(Bundled)` (design.md's eight
bundled GPU effects; their WGSL lives in `strand-gpu/src/bundled.rs`,
each a `ShaderCode` with four `vec4` uniforms `u0`–`u3` run by the file
pipeline, particles an instanced sprite pipeline of its own; the knobs
and uniform layouts are in decisions.md, m4-gpu-effects) or
`File(Arc<ShaderCode>)`. `ShaderInput` is `None` (a
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
angles in radians, durations in seconds, colours premultiplied sRGB
`vec4`: the space Strand composites in, which is also the space the
fragment's output is read in, so a file that returns `u_tint` paints
exactly the colour `bg: $accent` would (decisions.md, m4-gpu-w2
review).

On a surface that is not promoted (the usual case: a small aurora
behind a bar's clock), a pass is drawn offscreen by the GPU at its
bounds and read back into the CPU frame as a raster item; the surface
does not switch. The frame holds for the result like a readback frame,
up to `GPU_WAIT` (8 ms), and otherwise draws the previous result. On a
promoted surface a node's own pass (a `shader` node, aurora, particles)
runs in the GPU frame; a layer's bundled passes (`filter:`, glass, a
large backdrop blur) are `Pass` requests read back, whose pixels
render attaches to the layer (`strand_render::layers::Layer::gpu`) as
its offscreen group, but a backdrop a GPU frame draws itself
(perf-gpu-retain: `Op::Backdrop`; a blur under about 0.2 Mpx there at
the CPU's reduction, and the CPU's wherever no GPU frame draws it). Render asks for a bundled pass
only while its node shows (inside its ancestors' clip and the
surface), which is what starts the device and keeps it up.

**CPU fallback** (no device yet, none at all, a lost device, or a build
without `gpu`). Rendering stays on the CPU. Bundled effects use their
CPU versions: bloom becomes glow, glass becomes blur and tint, particles
cap at 1,000, tilt stays 2D, aurora is static, large backdrop blur is
the quarter-scale CPU blur; CRT, chromatic aberration and wobble draw
the node unfiltered. Aurora's and the particles' notices are said only
when no GPU can draw them (`GpuStatus::Unavailable`, or no `gpu`
feature); `tilt:` turns in 3-D only once the GPU has answered for it. A `shader` node keeps its box and draws nothing.
The same applies while the device starts, so a first frame never
waits for it. `Renderer::gpu_status() -> GpuStatus` (`Unused`,
`Starting`, `Up(AdapterInfo)`, `Unavailable { reason }`, with
`"built without the GPU backend"` as a reason) is sent to logic as
`ToLogic::GpuStatus` when it changes while a shader or bundled effect
is shown (`Renderer::gpu_in_demand() -> bool`), or once
`Renderer::gpu_off() -> bool` is true (the GPU turned off after
`LOST_CAP` lost devices; always false without the `gpu` feature),
whatever the frame shows; logic logs it once per reason, reports it as
a `strand watch` notice, and keeps it for `strand report` and the
inspector (M5) to say why.

**Budgets and tests.**
- `.text` (`strand/tests/budgets.rs`): the default build at most
  28,311,552 B and `strand --no-default-features` at most 24,379,392 B
  (decisions.md services-round, 2026-10-24: services-round's tree,
  27,620,503 and 23,581,207 B in the container suite, plus a 512 KiB
  reserve over CI's runner, rounded up to 256 KiB; before it 26,738,688
  and 22,806,528 B, decisions.md growth, 2026-10-22: raised at the owner's word on `main`
  at 091c7f64, 26,207,698 and 22,218,514 B in GitHub CI's budgets job,
  plus a 512 KiB reserve,
  rounded up to 256 KiB; before it 26,214,400 and 22,282,240 B,
  decisions.md m6-service-gaps, integration: its merged tree, 25,465,431
  and 21,512,215 B, plus the same reserve; before that 25,165,824 and 21,233,664 B, decisions.md m6-owner, 2026-10-14: M6
  wave 2's merged tree, 24,451,607 and 20,533,655 B, plus the same
  reserve; before that 24,379,392 and 20,447,232 B, decisions.md m6-owner, 2026-10-12: M6
  wave 1 on `main`, 23,620,631 and 19,702,999 B, plus a 512 KiB reserve,
  rounded up to 256 KiB; before that 23,855,104 and 19,922,944 B, decisions.md m6-integration-w1: M6 wave
  1's tree with remote media art, 23,556,951 and 19,639,511 B, plus a
  64 KiB reserve, rounded up to 256 KiB; earlier 22,806,528 and 18,874,368 B, from m5-integration-w2: M5 wave 2's merged tree, 22,473,111 and
  18,555,351 B, plus M5's unspent lines and a 64 KiB reserve, rounded up
  to 256 KiB; 22,282,240 and 18,350,080 B after the speed review,
  decisions.md m5-speed). Before the speed profile they were 18.5 MiB
  (19,398,656 B: the spike's 17,683,543 B with GPU code linked, plus
  9.7%) and 15 MiB (the spike's CPU build had 1.14 MiB to spare),
  re-based in M5 to 15,925,248 B, then 15,859,712 B after wave 0's
  reclaims (decisions.md m5-owner Q6, m5-integration). The spike measured the GPU build's bar
  PSS at a mean of 33.7 MB, about 0.3 MB under the 34 MB target, all of
  it cold-code mapping and relocations. With the backend landed
  (m4-gpu-w2): `.text` 18,456,983 B (default) and 15,118,167 B
  (`--no-default-features`, its own CI step), and the design bar's PSS
  33,731–34,016 kB over three runs, under the 34 MiB (34,816 kB) target,
  with no `libvulkan` mapped (asserted there). That is the spike's
  prediction for the GPU build (mean 33.7 MB, about 1.3 MB over the CPU
  build measured alternately with it), not a regression of the landed
  backend: features.md's 31.2–32.3 MB are the CPU build's. Since
  gpu-first the device is warm wherever one can start, so `budgets.rs`
  measures two legs (decisions.md gpu-first). Warm, under
  `STRAND_REQUIRE_GPU=1` with lavapipe accepted, the figures include
  lavapipe's and LLVM's pages: the bar warns above 242 MiB and fails
  above 252 MiB (re-based from 232 / 242 MiB, decisions.md perf), the full shell 296 / 324 MiB, debug 288 MiB, each the
  measurement plus the cold gates' absolute headroom. Cold
  (`the_design_bar_with_no_gpu_keeps_the_cold_budget`,
  `the_full_shell_with_no_gpu_keeps_the_cold_budget`; `demo.rs` and
  `services.rs`' bars), with no render node and `STRAND_GPU_SOFTWARE`
  and `STRAND_REQUIRE_GPU` removed, no device is asked for: the old
  42 / 48 MB and 76 / 104 MB gates (96 / 92 MiB debug), no `libvulkan`
  or `libLLVM` mapped, "GPU: no GPU render node" logged. The cold legs
  skip where a render node exists (`gpu.sh`).
- Cold (`strand/tests/gpu_cold.rs`, per feature set with `cargo
  metadata`): wgpu, vello_gpu and naga reach `strand` only through
  `strand-gpu`, and naga also through `strand-compiler`'s `shaders`;
  none of them in `--no-default-features`. `budgets.rs`: under
  `STRAND_REQUIRE_GPU=1` the warm device is up (`libvulkan` mapped, "GPU:
  the device is up" logged) and is never dropped.
- Idle on lavapipe
  (`strand/tests/gpu_idle.rs::a_settled_surface_goes_back_and_the_device_stays_warm`,
  enforced in CI; M4's `gpu_is_released_when_idle`): at boot the warm
  device is up (the `strand-gpu` thread runs, Vulkan is mapped) and the
  shell is quiet; a large animated shader panel is promoted and
  presented (lavapipe must attach in `Present` mode), stays presented
  while it animates, and its screenshot matches a small panel's
  CPU-drawn strip whose still shader reads back; hidden, it is demoted
  once settled and taken back; after each cycle the `strand-gpu` thread
  still runs, the process takes no wakeups, and the memory strand holds
  (PSS of its anonymous, shared-memory and swapped pages, not its share
  of mapped files, which moves with whatever else maps them) after
  every later cycle is within 4 MiB of that after the first (no growth;
  a glibc arena of Mesa's moves by up to 2.7 MiB between cycles,
  decisions.md m5-open-budgets, pss-flake).
- Hardware leg (`gpu.sh`, advisory): the same tests on
  `/dev/dri/renderD128` over three cycles, whose no-growth check is the
  leak tripwire (a leaked device or swapchain adds itself each cycle;
  what the first cycle leaves is paid once), plus backstops over the
  warm baseline after every cycle's demotion (until gpu-first, after the
  device's drop over the pre-GPU baseline): 12 MiB in readback mode
  (the spike's ANV left 5.2–6.3 MiB, all libraries unmapped; 4.6–7.3
  MiB measured in M5) and 16 MiB presenting (`STRAND_GPU_PRESENT=1`:
  4.6–8.8 MiB measured; decisions.md m5-speed-gpu-bound,
  m5-open-budgets), and the promoted cost checked against design.md's +20–40 MB
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

Code actions (M5, S-lsp): every `Diagnostic::suggestions` entry is a
quick fix titled by its `title` (else "Change to `…`"), applied in the
suggestion's own file (a missing key on a keyed method is added at the
`state` declaration, which may be in another file); one suggestion is
preferred, several are not, except `check::missing_key`, whose first (the
compiler's guess) is. `refactor::extract` is "extract component"
(`refactor.extract`, offered for the element whose name the selection
starts on): the element moves into `NewComponent` (`NewComponent2`, …)
after the enclosing component or surface, formatted with `fmt::format`;
what it reads from around it (parameters, `for` bindings, nested `let`
and `state` it only reads, `screen`) becomes a parameter the call passes
by name. Writing the parent's state (`=`, `<->`, a list mutation), an
`id:` element outside it, the `slot`, or a result that would add an error
(the config is checked again with the edit) refuses it, as a disabled
action with the reason for clients with `codeAction.disabledSupport` and
no action otherwise. For a client that resolves an action's `edit`
(`codeAction/resolve`), the action is offered without one
(`refactor::offer`, no compile), and the edit is made and checked when
it is chosen; a result that would not check fails the resolve with the
reason. `context.only` filters both kinds.

The tree-sitter package (`crates/strand-dev/tree-sitter-strand/`) joined
it in M5 wave 2 (it was not built in M1, `docs/decisions.md`,
wave2-lsp; m5-lsp). The reference generator (`strand-dev reference`,
`src/reference.rs`) joined it in M5 wave 3 (m5-docs). The inspector
client (`strand-dev inspect`, `src/inspect/{client,pane,write}.rs`)
joined it in M5 wave 3 (m5-inspect; "M5 interfaces (wave 0a)" gives its
protocol and write-back).

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
  on }, Keep(Option<Duration>)}` and `None` once stopped; `visible()` (a
  reader is visible: a service polling as a whole, cpu or memory, runs
  only then); (history) `kept()` (the period a `graph`'s history asks
  for while no reader is visible; `Msg::Keep` says when it changes) and
  `period(visible) -> Option<Duration>` (how often a polled service
  samples now: `visible` while shown, the kept period while hidden and
  kept, never faster than `visible`, `None` when it should not sample);
  `Ticker::new(&cx, visible)` follows it (`follow(&cx)` after each
  message, `reset()`, `tick().await`; cpu, memory and disk use it, and a
  `from poll` service follows `period` itself); asking `period` tells
  the client the body samples on a clock (`Client::sampled`);
  `Msg` is `#[non_exhaustive]`;
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
  (history) `keep(rt, Option<Duration>)` / `kept()` (a kept service
  starts, or stays running, with no reader, told `Keep`; the stop timer
  is armed only once it is neither read nor kept; `DynService::keep`
  and `kept` likewise), `polls(Option<Duration>)` (it samples on a
  clock every so often while shown: the builtins say it of cpu, memory,
  disk and (services-round) network, from
  `strand_services_schema::POLLED`, a host of its `from poll`
  services) and `sampled() -> Option<Duration>` (`Some` while it
  is kept, unread and polls, by `polls` or its body's `Cx::period`:
  the coarser of that and the kept period; `DynService::sampled`),
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
  writes it never answered (`Cells::forget_echoes`). (M6, gap 20) An
  action or write that reaches no run (the body failed and backs off)
  is not `DynService`'s error, so the VM never faults on it: the write
  is dropped and logged, and the action is logged and applied as
  `Service::refused(&action, error) -> Option<Event>`
  (`#[service(refused = path)]`; `None` by default), the event the
  service reports the action failing with (`session.failed`,
  `network.failed`, `bluetooth.failed`). A body idling without its bus
  (`dbus::idle_without_bus`) emits the same event for each action, and
  a body that ends without reading its queue (failing to connect
  before its first `recv`) leaves the actions still queued as its `Cx`
  drops, refused the same way as the run ends.
  `Client::act` still returns the refusal to Rust callers. A caller
  outside the program wraps its call in
  `strand_services::catch_refusals(f) -> (R, Vec<String>)`, which
  returns each action `DynService::action` could not hand to a run
  inside `f` (`` `name`: why ``): `strand call` (and its D-Bus front
  end) answers those as a refusal, not `{"ok": true}`, the program
  still hearing the failure event (review of the integration).
- **Builtin services here** (wave 4): `system` (the portal's appearance
  settings through `strand_watch::follow` on the shared runtime, plus
  `hostname`; M6, gap 34: `kernel` from `/proc/sys/kernel/osrelease`,
  read once, and `uptime`, a stream field from `/proc/uptime` floored
  to minutes, read again at each whole minute of uptime only while a
  visible reader shows it, `system::uptime_ticks()` counting the wakes
  for tests), `cpu` and `memory` (procfs, sampled once a second only
  while a reader is visible; services-round: `memory`'s `available`,
  `free`, `cached` (`Cached` plus `Buffers`), `swap_used` and
  `swap_total` from the same `/proc/meminfo` reading,
  `procfs::Meminfo`), `disk` (M6, gap 33: the mounted
  filesystems of `/proc/self/mountinfo` selected by `disk::select`,
  none at or under an `autofs` point, measured with rustix's `statvfs` on the blocking pool one sample at a
  time, every 10 s only while a reader is visible;
  `disk::configure(Option<disk::Config { mountinfo, period }>)` points
  it at a fixture and a short period in tests, `disk::samples()` counts
  its samples), `keyboard` (M6, gap 31: `caps_lock`, `num_lock` and
  `layout`, stream fields; first what `keyboard::focused(Option<Focused>)`
  holds, which the binary's host sets from strand-surface's
  `SurfaceHost::keyboard_state`; else the lock keys from the kernel's
  LEDs, `<sys>/class/leds/*::capslock` and `*::numlock`, read every
  `keyboard::LED_PERIOD` (250 ms) only while a visible reader shows one
  and no focused report holds, and the layout from the shared hub's
  `WmChange::KeyboardLayout`, subscribed only while `layout` is watched;
  `keyboard::configure(Option<keyboard::Config { sys, led_period }>)`
  points it at fake LEDs in tests, `keyboard::led_reads()` counts the
  reads; services-round: `layouts` from `WmChange::KeyboardLayouts`
  (subscribed while `layout` or `layouts` is watched; a focused keymap
  naming any wins), or on Hyprland, which names only the active
  layout, from `WmChange::KeyboardRules` (the keyboard's xkb rule
  names, `wm::XkbNames`), compiled into names with libxkbcommon by
  `keyboard` only while `layouts` is watched and only when the rules
  change, `alt`/`ctrl`/`shift`/`logo` from `Focused` only, and the
  actions `next_layout()`/`set_layout(index)` as
  `WmAction::SwitchLayout(LayoutSwitch::Next | Index(u32))`, each on a
  subscription of its own that asks again after each batch while the
  adapter answers `NotConnected`, for 3 s at most), and the D-Bus
  services (wave 4, a2):
  `battery` (UPower's display device and devices; services-round:
  `#[service(call = BatteryCall)]`, `BatteryCall::History { span,
  points: Option<i64> }` answered in a task of the body's own by
  `GetHistory("charge", span, 2 × points)` on the first battery
  (`Type` 2, `PowerSupply`, `HasHistory`) and `battery::resample(records,
  now, span, points)`, the charges at even times oldest first, those
  before the first record left out; `HISTORY_POINTS` 60 unset,
  `MAX_HISTORY_POINTS` 2000; with no bus, `dbus::idle_without_bus` now
  answers every async method with why), `brightness`
  (`/sys/class/backlight` watched with inotify, writes through logind's
  `SetBrightness`; `brightness::set_backlight_root` /
  `STRAND_BACKLIGHT_DIR` point it at fake backlights in tests), `network`
  (NetworkManager; `access_points` is a stream field: read, followed and
  scanned only while watched; a join is a task of the body's own, M6,
  one per network at a time and stopped when NetworkManager is
  replaced, an enterprise (802.1X) network joins only through a profile
  saved in NetworkManager, and `AccessPoint.secure` is true exactly
  where `connect_with(password)` joins: WPA personal or WEP; M6 wave 2,
  m6-w2-services: where NetworkManager does not run and iwd does, the
  body is `network::iwd::run`, iwd's object manager with a passphrase
  agent and a signal level agent served at `iwd::AGENT_PATH` on the
  shared system connection, and either daemon appearing or going chooses
  again; `dbus::Daemon::watch` follows a name without D-Bus activation,
  and every call to iwd goes through `iwd::call` with `NO_AUTO_START`,
  so iwd is never started for us; both agents answer only iwd's current
  unique name; m6-service-gaps, gap 28: `disconnect()` (NetworkManager's
  `Device.Disconnect` on the Wi-Fi device, iwd's `Station.Disconnect`,
  each a task; joins it stops are neither followed nor reported while it
  is in flight, then dropped once it has left (over NetworkManager an
  activation answered meanwhile is deactivated then) or followed again
  if it fails),
  `connecting`/`disconnecting` from the daemon (NetworkManager's
  ACTIVATING/DEACTIVATING active connections, which no longer count as
  joined; iwd's Station `State`) merged with the shell's own requests
  by `network::Pending` and `overlay`, which both backends share, as
  are `address` and `scanning`'s request; `frequency` and `security`
  (`WifiSecurity`) from the joined access point's flags or iwd's
  `StationDiagnostic.GetDiagnostics`, read on each join or roam;
  `scan()`/`scanning`: NetworkManager's accepted `RequestScan` until its
  `LastScan` changes or `SCAN_LIMIT` (15 s), iwd's `Scanning`; `address`
  (a `#[store(stream)]` field) from `network::address::Follower`, an
  rtnetlink socket on rustix's `net` (RTM_GETADDR dump, then the IPv4
  and IPv6 address groups; no polling) opened only while `address` is
  watched and the backend names an interface (NetworkManager's
  default-route device, iwd's Wi-Fi interface); services-round: `rx`
  and `tx` (stream fields) from `network::rates::Rates`, owned by the
  body across both backends and the no-bus body (`without_bus`, which
  refuses actions as `dbus::idle_without_bus` does): `/proc/net/dev`'s
  byte counters summed over the interfaces with a
  `/sys/class/net/<if>/device` link, each interface's difference since
  the last reading (one that came, went or went back counts nothing),
  on a `Ticker` started while `rx` or `tx` is watched or the service is
  kept (`Cx::kept`): every `rates::PERIOD` (1 s) while `rx` or `tx` is
  watched, else at the kept period (`rates::every`; services-round
  review: a visible reader of other fields no longer samples a hidden
  graph's rates every second; `Ticker::at`/`follow_every`), a fresh
  baseline at each start, laid over the backend's state by `overlay` from
  `Pending::rates`; `rates::configure_root(Option<PathBuf>)` points it
  at fake counters and `rates::samples()` counts its reads (tests);
  services-round: `kind: Option<ConnectionKind>` from `Nm::kind` (the
  activated active connection with `Default`, else `Default6`, a VPN or
  WireGuard one first, else `PrimaryConnection`'s `Type`, else
  `PrimaryConnectionType`; `wifi` over iwd while connected) and
  `connections: Vec<Connection>` (`#[store(keyed, stream)]`, keyed by
  UUID) from `read_profiles` (`Settings.ListConnections`, each
  `GetSettings`' `connection` id, uuid and type, loopback left out) only
  while watched, the Settings namespace subscribed (`"settings"`) then:
  (services-round review) the profiles are kept across `Nm::read` (an
  active connection's change reads no settings) and read again only on
  a Settings signal (`Nm::settings_signal`: `NewConnection`,
  `ConnectionRemoved`, a connection's `Updated` or `Removed`), one
  `GetSettings` per profile (`read_profile`); `NetworkAction::Up`/`Down { item:
  Connection }` run in a `JoinSet<Switched>`: `activate` (the settings
  path as read, else `GetConnectionByUuid`, then `ActivateConnection(path,
  "/", "/")`) whose active connection joins `joining`, so its failure is
  reported as a join's, and `deactivate` of `Nm::active_of(uuid)`, each
  task's connection name kept by its `tokio::task::Id` so one cut short
  by NetworkManager going is `failed(name, "NetworkManager stopped")`;
  over iwd both fail with `network::IWD_NO_CONNECTIONS`), `bluetooth` (BlueZ's
  object manager; m6-service-gaps, gap 29: `nearby` is a stream field,
  discovery (`SetDiscoveryFilter`, `StartDiscovery`) running only while
  it is watched, `StopDiscovery` only for the body's own session (BlueZ
  keeps one per client) and when the body ends; signals that change
  only advertisement data are dropped; `pair()` (pair, trust, connect)
  and `forget()` (`Adapter1.RemoveDevice`) are tasks, and a device's
  `connecting`/`disconnecting` are the body's tasks in flight by
  address (`bluetooth::InFlight`: the call asked last shows, a call
  repeating one in flight on its device is dropped), a pairing past its
  limit is cancelled (`CancelPairing`); no pairing agent is registered),
  `notifications` (our own `org.freedesktop.Notifications` server;
  `Notification.time` is a `Date` with the hour and minute since M6;
  M6, gap 35: the history is kept in the persist store's
  `services:notifications.all` (JSON, `{"version":1,"notifications":[…]}`,
  the newest `KEPT`, `transient` ones and our runtime-dir pictures left
  out) and `services:notifications.remember` (`0` or `1`), saved 1 s
  after the last change (`SAVE_AFTER`) but at most `SAVE_WITHIN` (5 s)
  after the first unsaved one, on the blocking pool (latest generation wins,
  a `SaveOnStop` finalizer making the last save) and restored, renumbered
  1..=n without actions, before the name is asked for;
  `notifications::set_config(Option<notifications::Config { state }>)`
  points it at a test's directory, or `state: None` for none, as
  `apps::set_config` does; services-round: `progress`, `category` and
  `desktop_entry` from the `value`, `category` and `desktop-entry`
  hints, kept in the history, and `can_reply` from an `inline-reply`
  action, never kept; the server advertises `inline-reply` only while
  `notifications::offer_replies(true)` (the binary's
  `services::follow_program`: the program names `Notification.can_reply`
  or `reply`, `lower::Program::names_member`), an `inline-reply` action
  being an ordinary button otherwise, and
  `NotificationsAction::Reply { item, text }` emits
  `NotificationReplied`; every signal about a notification
  (`NotificationClosed`, `ActionInvoked`, `ActivationToken`,
  `NotificationReplied`) is unicast to the bus name that sent it,
  recorded at `Notify`), `apps` (services-round: `App.actions:
  Vec<apps::AppAction>` from the entry's `[Desktop Action]` groups,
  `Entry::action_execs` their `Exec` lines, `AppsAction::LaunchAction {
  item: AppAction }` the action's `launch`, starting
  `Entry::for_action(id)`), `idle` (services-round: `ext_idle_notifier_v1` on a
  thread of its own, `strand-idle`, as `outputs` runs; `idle::want(t)`
  counts a reader of a timeout process-wide (`idle::unwant` at the
  reading scope's cleanup) and pokes the running services' eventfds,
  each keeping one `ext_idle_notification_v1` per timeout read, and
  destroying one `idle::LET_GO` (1 s) after its last reader went;
  `Idle.timeouts: Vec<IdleTimeout { timeout, idle }>` keyed by timeout
  is what the `fn after(timeout)` reads, tracked; `idle::configure`
  points it at a test's socket), (services-round) `polkit` (its own
  polkit authentication agent, `polkit/mod.rs` and `polkit/helper.rs`:
  while it runs, a zbus `org.freedesktop.PolicyKit1.AuthenticationAgent`
  object at `polkit::AGENT_PATH` on the system connection, registered
  with the Authority for this process's session, `unix-session` from
  logind's `GetSession("auto")`, else the `session-N.scope` of
  `/proc/self/cgroup`, else `$XDG_SESSION_ID`; refused with polkitd's
  "already exists" it sets `error` to `polkit::HELD` once and registers
  again at the Authority's `Changed` signal; it is never registered
  without a helper, and a stop (the body dropped, as every store's) drops
  the `Exported` guard, whose `Drop` unregisters and removes the object
  on a task the next run awaits (`TEARDOWN`). `BeginAuthentication`
  calls from any sender but the Authority's owner fail. Each request is
  queued (`Polkit.request` the first, `waiting` the rest), its
  identities read as user names (`polkit::users_of`: `unix-user`, and a
  `unix-group`'s members), and answered by a conversation with
  `polkit-agent-helper-1` (`helper::converse` over polkit 127's socket,
  `helper::SOCKET`, when it listens, else the setuid program from
  `helper::HELPER_DIRS` with a cleared environment and
  `child::restore_in_child`): its `PAM_*` lines become `prompt`, `echo`
  and `info`, each `respond` one answer line in a `Zeroizing` buffer
  sized once (one with a newline, a NUL or over 511 bytes refused
  whole), and the helper answers polkit itself; `FAILURE` after an
  answer sets `failed` and starts the next conversation. `cancel()`
  returns `org.freedesktop.PolicyKit1.Error.Cancelled`,
  `CancelAuthentication` dismisses its request, `choose(user)` restarts
  the conversation for another listed user. (services-round review) A
  `respond` is taken only once the shown request (for its user) has
  asked (`Requests::seen`), so an answer typed for a request that went
  is dropped; one given before a retry's question is held in the
  conversation (`Conv::held`) and written only to a secret question.
  `helper::Helper::find` takes a socket only when it is a socket and a
  program only when it is setuid, both owned by `PolkitConfig::owner`
  (root), else the store does not register; (lockmon review) the path
  is read with `symlink_metadata`, and `Helper::open` takes a connected
  socket only when its peer's uid (`SO_PEERCRED`) is `Helper::owner`,
  failing with `PermissionDenied`, never falling back to the program
  (`helper.rs::tests::the_socket_first_and_only_its_owners`). The request's `command` is
  the details' `command_line`, else `program`.
  `polkit::configure(Some(PolkitConfig { helper, socket, owner }))`
  points stores started later at a test's helper), and `greetd` (`greetd.rs`:
  `$GREETD_SOCK`'s length-prefixed JSON, `greetd::proto`, one request in
  flight; `login` is `create_session` after a `cancel_session` when a
  login is under way, a PAM text is acknowledged with a null response
  at once, a refused answer (`auth_error`) is cancelled and the user
  asked again with `failed` kept, `start` is held until the login
  succeeds and each word quoted for the shell line greetd makes of it
  (`proto::shell_word`), and greetd's success calls the `started` hook
  of `greetd::configure(Some(GreetdConfig { socket, server_uid,
  started }))`, which `strand run` sets to raise SIGTERM on itself;
  without `$GREETD_SOCK`, or when the socket's peer (`SO_PEERCRED`) is
  not `server_uid` (by default root or Strand's own uid, who can read
  its memory anyway), `available` is false and every action sets
  `error`; an answer held from before greetd asked goes only to a
  `secret` question; `child::INSTANCE_VARS` keeps `GREETD_SOCK` from
  every program Strand starts; answers are `strand_auth::Password`s
  from the action on (`FromData for Password`, as `auth.submit`'s),
  framed in a `Zeroizing` buffer sized once).
  Both run on the shared runtime (the helper is a `tokio::process`
  child, killed when its conversation drops) and are
  `strand_services::secret` services, with `auth`
  (`strand_services_schema::SECRET`),
  `media` (MPRIS; `elapsed`/`position` carried forward, ticking only
  while watched; M6, gap 32: `can_next`, `can_previous` and `can_seek`
  from the active player's `CanGoNext`, `CanGoPrevious` and `CanSeek`,
  false while `CanControl` is, `can_seek` the very test a `position`
  write makes; remote `https`/`http` art fetched by `art`, below;
  services-round: `players: Vec<MediaPlayer>` (`#[data(name =
  "Player", key = id)]`, the bus name), a `#[store(keyed, stream)]`
  field built by `Players::items` only while watched, each item's art
  through the same cache, the active player's looked up last; the
  ticks run while `players` is watched and any player plays
  (`next_tick(players, now, listing)`); `MediaAction::{PlayPauseOf,
  NextOf, PreviousOf, Select} { item }` before the service's own unit
  actions, so a shared name is matched by its item first;
  `Players::selected` (the bus name and the clock when chosen) wins
  `active()` while the player is known and none other was touched
  playing since; a write of an item's `position` is `Write { field:
  "players", key: Text(bus name), path: [position] }`, sought by
  `Players::seek_player`) and
  `tray` (StatusNotifierItem host, its own watcher
  when the session has none, DBusMenu model), and (wave 4, wm) the
  compositor's `workspaces`, `windows` and `wm` and PipeWire's `audio`
  (below). The D-Bus services are all on our own zbus calls
  (decisions.md, wave4-a2), with `logind-zbus` for `SetBrightness`.
  (M6, m6-services) `art`: `art::fetch(url) -> Result<art::Art,
  String>` (`Art { file: pixmap::Pinned, bytes: u64 }`; blocking; the media body runs it on the runtime's blocking
  pool) fetches with `ureq` over rustls/ring and the system's
  certificates (`rustls-native-certs`), at most `art::MAX_BYTES` (8 MiB)
  within `art::TIMEOUT` (10 s), with every signal but a fault blocked on
  its thread while it runs (replayfix: a caught signal, tokio's SIGCHLD,
  would fail the socket read with EINTR and end the fetch), checks the
  bytes are a PNG, JPEG, GIF or WebP and writes them with `pixmap::write_file(bytes, ext)` (the
  content-addressed, pinned files `write_png` already made); the
  renderer decodes them as any local picture. `art::Cache` (16 URLs
  kept, LRU, at most `art::KEPT_BYTES` (16 MiB) of files together, since
  they are tmpfs memory, the one shown last never dropped; six fetches at once, the shown URL's and skipped tracks'
  still running; a failed URL not asked again for 5 min) lives in the media body, which
  leaves it, the shown file only, to the next body on its services thread
  (`media::Kept`, a per-thread slot, so another runtime's body never
  takes it), so the store's `art` path never names a removed file; the
  fetches still running are left with it, counted, and their ends come
  to the next body over a channel (a file that comes in while no body
  runs is not kept: `art::Cache::abandon`), so six at once holds across
  bodies (m6-services, services-r2-3/-4, services-r3-1/-2). `art::agent(roots)` and
  `art::fetch_with` take other roots (tests).
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
  waited for from its nearest existing ancestor), (services-round) a
  `dir` (`custom::Source::Dir`: the same watch on the directory itself
  for entries created, deleted, moved, closed after writing or changed
  in attributes; `custom::list_dir(dir) -> io::Result<Option<Vec<Data>>>`
  lists its regular files, links followed, hidden ones left out, as
  `File` records `{ path, name, modified: Date (local, to the minute),
  size }` sorted by name, the first `custom::MAX_DIR_FILES` 10 000 by
  name (the names read and sorted first, only those kept `stat`ed),
  `None` while it does not exist; the document is `{ files: [...] }`,
  listed again `custom::DIR_SETTLE` 50 ms after a change and at most
  every `custom::DIR_RELIST` 500 ms, one listing at a time on a blocking
  task, warned about once past `READ_WAIT`), a `listen` command's
  lines (merged and sent at most once per `custom::LISTEN_FLUSH`, a
  frame; services-round: while `custom::hears_lines(spec)` (a count
  `hear_lines` raises and `unhear_lines` lowers), each line also emits the store's `line` event, `Data::List` of that
  line's field values by index, before the merge) or a `poll` command or file (only while visible) as
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
  BlueZ connects, pairs and forgets) run as tasks in a `JoinSet` owned by the body, so they
  hold up nothing and are cancelled when it stops; what they find (a
  player read, a tray item read, a failed connect for
  `bluetooth.failed`) comes back to the loop as the task's result. Pixels from D-Bus
  (`image-data`, `IconPixmap`, a menu entry's `icon-data`) are checked
  against the bytes sent before allocating, sampled down to 512 px a
  side and become content-addressed PNG files under
  `$XDG_RUNTIME_DIR/strand/pixmaps/<pid>` (`strand_services::pixmap`;
  without `XDG_RUNTIME_DIR`, a `strand-<uid>` directory in the
  temporary directory, used only when it is this user's and mode 0700)
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
  `Builtin`. (M6 wave 2, m6-w2-services) `session`
  (`strand_services::session`, `strand_services_schema::SESSION`,
  `Builtin.session`): logind's `Can*` answers as `can_*`, its power and
  session calls as actions (each a task of the body's, `failed(action,
  error)` on a refusal) and the own session's `Lock` signal as
  `lock_requested`; a provisional stub in builtin.schema carries its
  members. (M6, m6-owner-fixes) `outputs`, the monitors' configuration
  over wlr-output-management (below).
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
  (services-round) `ServiceHost::event(rt, svc, "line")` on a `from
  listen` declaration calls `custom::hear_lines`, acquires the client
  for the current owner scope (released, with `unhear_lines`, by its
  cleanup: a line handler is a reader) and returns the declaration's
  `EventQueue`, fed by the one observer each client gets when it is
  registered (`LineSink`: a restart re-points it at the new mount's
  queue, never adds another), which converts each line to the
  declaration's record (`Typing::record`); the checker gives every `from listen` service
  record that one event (`RecordDef::events`, `line(doc: <record>)`).
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
  fallback answers them at their defaults; (M6) a declared one first, so
  a config's `service battery` shadows the builtin, spec.md 4.2, except
  for the `*_builtin` reads, which skip it), and everything else (the
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
  thumbnails capture by it through `ProtoCmd::Capture` on the
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
  (services-round) `win.move_to(ws)` is `WindowAction::MoveTo { item,
  ws: WorkspaceItem }`, sent as `WmAction::MoveWindow { window,
  workspace }`: Hyprland's classic `movetoworkspacesilent N,address:…`
  or Lua `hl.dsp.window.move({ workspace, follow = false, window })`, sway's `[con_id=N]
  move container to workspace "<name>"`, niri's
  `MoveWindowToWorkspace { window_id, reference: { Id }, focus: false }`;
  `Unsupported` on the protocols. `model::Window::rect:
  Option<WinRect { x, y, width, height: i64 }>` (served as
  `WindowItem::rect: Option<WindowRect>`, the builtin `Rect` record) is
  the window's place on its screen from the adapters (Hyprland's `at`
  and `size` less its monitor's `x`/`y`, re-read also on
  `changefloatingmode`; sway's container `rect` less its output's;
  niri's floating `layout`, patched by `WindowLayoutsChanged`), null on
  the protocols. `WmState::mode: Option<String>`, published as
  `WmChange::Mode` (and in the replay), is `wm.mode`: Hyprland's
  `submap>>`, sway's `mode` events after `get_binding_state`, none on
  niri. `WmState::keyboard_layouts: Vec<String>`, published as
  `WmChange::KeyboardLayouts`, is sway's first keyboard's
  `xkb_layout_names` and niri's `KeyboardLayoutsChanged` names; on
  Hyprland, whose IPC names only the active layout, it stays empty and
  `WmState::keyboard_rules: Option<wm::XkbNames>`, published as
  `WmChange::KeyboardRules`, carries the main keyboard's `j/devices`
  rule names, which the `keyboard` store compiles into names with
  libxkbcommon (`XkbNames::layout_names`, the `xkbcommon = 0.8.0`
  dependency without default features) only while `keyboard.layouts`
  is watched (services-round review: not on every connection).
  `workspaces.on(screen)` is a `fn` method over the cells (the
  workspaces whose `screen` is the `Screen` record's `name`).
  (M6, gap 31) `WmState::keyboard_layout: Option<String>` is the
  compositor's name for the active keyboard layout, published as
  `WmChange::KeyboardLayout` (and in the hub's replay) for the
  `keyboard` service: Hyprland's `j/devices` (the `main` keyboard's
  `active_keymap`, else the first's) read at connection and after a
  reload, then `activelayout>>` events (the name after the first
  comma); sway's `GET_INPUTS` at connection (the first keyboard's
  `xkb_active_layout_name`) and `input` events (`xkb_layout`,
  `xkb_keymap`, `added` while none is known); niri's
  `KeyboardLayoutsChanged` and `KeyboardLayoutSwitched` from its event
  stream. A failed read leaves it `None` and degrades nothing.
  (M6 wave 2, m6-w2-services) `WmState::configured` holds the workspaces
  the compositor's config places on a screen that do not exist yet
  (Hyprland's numbered `j/workspacerules` with a monitor that is on,
  read at connection and on `configreloaded`; sway's `workspace <name>
  output <outputs…>` lines of `get_config`, read at connection and on
  `reload`, placed on the first of their outputs `get_outputs` says is
  active, the outputs read again on every `output` event; niri none, its named workspaces always exist), published as
  `WmChange::Configured` (keyed by id, a `Reset` on the first publish and
  in the hub's replay) and served as `workspaces.configured`; a slot's id
  is its number on Hyprland and `-(n + 1)` for the n-th assignment on
  sway, and `ws.focus()` on it makes it. `workspaces.slots(screen)` is a
  `fn` over `all` and `configured` (`wm::service::slots`: by number,
  the id on Hyprland and the number a name starts with elsewhere, then
  the rest in order). `workspaces.focus(target:
  int | text)` is a service-level action (`WorkspaceAction::FocusTarget`,
  a `WorkspaceTarget::{Id, Name}`), sent as
  `WmAction::FocusWorkspaceNumber(n)` or `WmAction::FocusWorkspaceNamed(name)`;
  `FocusWorkspaceNumber` is the same number everywhere: Hyprland's id
  (`dispatch workspace N`), sway's `workspace number N` (a negative n
  being a configured slot's id), niri's `{"Index": n}` on the focused
  output and a protocol workspace named n, else `UnknownWorkspace`;
  `FocusWorkspace` of an id no workspace has is a number Hyprland makes
  or a sway slot's name, else `UnknownWorkspace`; `FocusWorkspaceNamed` is
  Hyprland's `name:` (a number by number), sway's `workspace "<name>"`,
  niri's `{"Name": …}` reference (a number not named: `{"Index": n}`) and
  `ext-workspace-v1`'s activation of a listed workspace, else
  `WmError::UnknownWorkspaceName`.
  (M6 wave 2, m6-w2-services) A window carries `app: Option<apps::App>`
  (`Window.app: App?`), and its `icon` is its desktop entry's: `wm::run`
  reads `apps::AppIndex::load(&apps::Config::current())` on the blocking
  pool at its start and again when `apps::changed()` is called (the
  watcher's `CacheKind::Apps` and `CacheKind::Icons`; one read at a
  time), publishes the compositor's state without waiting for it, and
  patches each window's `icon` and `app` from `AppIndex::window(app_id)`
  (`AppIndex::find`, then `apps::window_icon`), looked up on the blocking
  pool one batch at a time and kept per app id until the next read;
  `WmState::link_apps` leaves a window not yet looked up as it is. `apps::Entry`
  gains `wm_class` (`StartupWMClass`); `App` is `Eq`.
  `wm::live_runs()` counts live `run`s (tests). `screens.focused` is the
  `screens` service's, which no crate serves yet; when one does it takes
  `FocusedScreen` from the same hub, with a store's lifecycle (it drops
  its subscription only 5 s after its last reader). The schema texts
  are `strand_services_schema::{WINDOWS, WORKSPACES, WM}` (one service
  per text; `wm::{WINDOWS_SCHEMA, WORKSPACES_SCHEMA, WM_SCHEMA}`),
  replacing the provisional stubs and adding `Workspace.active`,
  `Window.urgent`, `Window.maximized` and `event
  config_reloaded(failed: bool?)`. `requests` takes
  `WmRequest { action: WmAction::{FocusWorkspace, FocusWorkspaceNumber,
  FocusWorkspaceNamed,
  FocusWindow, CloseWindow, MinimizeWindow, MaximizeWindow,
  FullscreenWindow},
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

- **Monitor configuration (`strand_services::outputs`, the `outputs`
  service; M6, m6-owner-fixes, owner-testing gap 18).** `outputs::Outputs`
  (`#[service(name = "outputs", action = OutputsAction, refused = …,
  thread)]`, `Builtin.outputs`, schema `strand_services_schema::OUTPUTS`
  with a provisional stub in builtin.schema, and the enum
  `OutputTransform` there, which both use) runs on a thread of its own
  (`strand-outputs`) with its own Wayland connection, asleep in
  `poll(2)` on the socket and an eventfd `Cx::set_notify` pokes, and binds
  `zwlr_output_manager_v1` (version 4 at most; `wayland-protocols-wlr`'s
  `output_management`). Records (`#[derive(Data)]`): `Output` (keyed by
  `name`: description, make, model, serial, `enabled`, `mode` and
  `preferred: OutputMode?`, `modes` deduplicated, `x`, `y`, `scale`,
  `transform`, `adaptive_sync`, physical size), `OutputMode { width,
  height, refresh }` (Hz, from the protocol's mHz) and `OutputChange`
  (`name` and every setting as an `Option`). `all` and `available` are
  published at each manager `done`; ready after the first `done`, or
  after the registry's first sync shows no manager (`available` false).
  `OutputsAction::{Apply, Test} { changes: Vec<OutputChange> }` queue in
  order, one `zwlr_output_configuration_v1` in flight at a time against
  the last `done`'s serial; every head goes in it (the protocol's rule),
  each property a change leaves `None` sent as the head has it (an
  enabled head's current mode, else its preferred, else its first; a
  disabled head disabled), adaptive sync only on version 4. A change is
  checked before sending (an unknown or repeated name, a scale outside
  1/256 to `wl_fixed`'s largest, a mode size not above 0, a refresh or
  place out of range), since the protocol counts those as errors that end
  the connection; a mode is the listed one of its size nearest its rate
  within 0.5 Hz (`outputs::pick`), else `set_custom_mode`. `succeeded` is
  `OutputsEvent::Succeeded(action)`, `failed` and every refusal
  `Failed((action, error))`, `cancelled` sent again from the new state
  up to `outputs::MAX_TRIES` (3) times. A lost connection empties `all`,
  clears `available`, emits `failed` for each configuration in flight,
  waiting or unread, and ends the run with an error (the contract's
  backoff); while it backs off, `Service::refused` makes an action
  `failed` too. A stop waits up to 500 ms for the answer in flight.
  `outputs::configure(Option<WaylandTarget>)` points runs started later
  at a socket (tests; `None`: `$WAYLAND_DISPLAY`, resolved by
  `wm::protocol::display_socket`, now `pub(crate)`). Tests:
  `crates/strand-services/tests/outputs.rs` on headless sway 1.9.

- **Clipboard (`strand_services::clipboard`, the `clipboard` service;
  M6, m6-service-gaps, owner-testing gap 30).** `clipboard::Clipboard`
  (`#[service(name = "clipboard", action = ClipboardAction, refused = …,
  thread)]`, `Builtin.clipboard`, schema
  `strand_services_schema::CLIPBOARD` with a provisional stub in
  builtin.schema) runs on its own `strand-clipboard` thread with its own
  Wayland connection, asleep in `poll(2)` on the socket, an eventfd and
  the one read in flight. `clipboard/protocol.rs` is one client over
  `ext_data_control_manager_v1` (bound where offered) and
  `zwlr_data_control_manager_v1` (version 1, no primary selection),
  with the first `wl_seat`; a device that finishes is made again while
  the seat lives. Fields: `available`, `text`, `holds_image` and
  `#[store(keyed, stream)] history: Vec<ClipboardEntry>` (`ClipboardEntry
  { id, text, image: Option<String>, size, time: Date }`, keyed by `id`);
  event `failed((action, error))`; actions `Copy { value }`, `Clear` and
  the item actions `CopyEntry { item }` (`#[call(name = "copy")]`) and
  `Remove { item }`. `clipboard/offer.rs` says what a selection is (a
  text type wins; PNG, then JPEG, kept; another image only flagged) and
  whether `x-kde-passwordManagerHint` marks it `secret` (read first, 64
  bytes; unreadable is secret): a secret is read no further, recorded
  nowhere, logged nowhere. A device's first selection, told before the
  `wl_display.sync` sent after `get_data_device` is answered
  (`Ev::Selection { at_bind }`), is the selection found, not a copy: it
  sets `text` and `holds_image` and is never recorded; the run is ready,
  and runs queued actions, only once that sync is answered (review
  r1). A text is read 1 MiB and 5 s at most;
  while `history` is watched each text and kept image is pushed to
  `clipboard/history.rs`'s `History` (newest first, deduplicated by
  content keeping the id, `Caps::DEFAULT` 100 entries, 64 MiB, 1 MiB a
  text, 16 MiB an image, oldest evicted with its file) and saved in
  `<state>/strand/clipboard/` (`history.json` and `images/<id>-<time>.png
  |.jpg`, directory 0700, files 0600, each write a synced temporary file
  renamed over, the directory synced; a corrupt file is an empty history
  and a `Cx::warn`; `images/` holds only files `history.json` names, the
  rest and any temporary file a cut-short save left deleted at a load or
  save, temporary files at `clear` too: review r2), loaded at the first
  watch or action that needs it.
  `copy` and `e.copy()` go to `clipboard/keeper.rs`: the process-wide
  `strand-clipboard-keep` thread with its own connection holds the
  source, writes each paste without blocking (5 s without progress ends
  a write) and ends when another program takes the selection, so a copy
  outlives the run's 5 s stop; the run reads its own copy back like any
  selection. Without data control `available` is false and the actions
  fail as `failed`; a lost connection clears `available`, fails what
  was queued and ends the run (the contract's backoff).
  `clipboard::configure(Option<Config { target, state_dir }>)` points
  runs started later at a display and state directory (tests). Tests:
  `crates/strand-services/tests/clipboard.rs` (headless sway 1.9 and
  strand-fake-wayland), `compositor_matrix.rs`'s
  `the_clipboard_works_on_every_compositor`.

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
  `request(AudioAction::{SetVolume(DeviceRef, f64), SetMuted(DeviceRef,
  bool), MakeDefault(DeviceRef)}) ->
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
  tagged cell makes quick steps compound). There is no relative action:
  `StepVolume`, which no language path sent, was removed in M6
  (decisions.md, m6-services). Volume and mute go to the
  card's active `Route` (`save: true`) when the node has one, else to
  the node's `Props`.
  `set_levels(targets)` replaces the set of peak meters
  (`LevelTarget::{DefaultSink, DefaultSource, Device(id)}`); the store
  passes what visible readers want, and an empty set stops them all.
  Their consumers are the `spectrum` element (M4,
  `spectrum(AudioDevice -> source)`, through `tap_levels`) and
  (services-round) `audio.levels`, below; the meters' own capture
  streams carry `stream.monitor = true`, as pipewire-pulse marks
  pavucontrol's, and `Meter::node()` (the stream's node id) leaves this
  shell's own out of `audio.streams` whatever the registry says of them.
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
  (services-round) Streams: `streams: Vec<AudioStream>` (`#[store(keyed,
  stream)]`, `AudioStream { id, app, icon, direction: StreamDirection,
  volume, muted }`, keyed by the node's `object.serial`, which PipeWire
  never reuses, as node ids are; `Node::reused_ids` says which ids a
  list diff may see reused). The loop keeps every
  `Stream/Output/Audio` and `Stream/Input/Audio` global
  (`Session::stream_globals`) but binds them (`bind_node` with a
  `StreamDirection`, as a device's node is bound) only while
  `Cmd::Streams(true)`, which the store sends while `Cx::watched
  ("streams")`, and drops them on `false`; `app` is `application.name`,
  else `application.process.binary`, else `node.name`, `icon`
  `application.icon-name`, else the binary, else
  `applications-multimedia` (`NodeProps::read`, from the global and
  again from each info event whose mask has `PROPS`; a later info
  without it carries no properties). Monitors (`stream.monitor`) are
  left out; an input stream recording a sink's monitor
  (`stream.capture.sink`) is `StreamDirection::Monitor`; streams never count as defaults (`default_id`), and
  `MakeDefault` of one is `UnknownDevice`. `AudioChange::Streams` is a
  keyed diff like `Sinks`, `Serials` covers stream ids too, and a write
  of an item of `audio.streams` (`Shown::Stream`) runs and is answered
  as a device item's, matched by `app` where a device item is by
  `node.name`, its node found by serial (`Store::stream_ref`), so a
  write from an item held past its stream's end is refused. Video: `strand_services::video` (`Video`,
  `#[service(name = "video", thread)]`, `streams: Vec<VideoStream>`
  keyed, `VideoStream { id, app, source: VideoSource }`) runs the same
  loop (`thread::run(audio::service_config(), Follow::Video, host, rx)`
  on its own `strand-video` service thread) following only video:
  `Video/Source`, `Video/Source/Virtual` and `Stream/Output/Video` nodes
  as sources, `Stream/Input/Video` nodes as takers, and the `Link`
  globals between them (`link.output.node`, `link.input.node`); a taker
  with a link from a source is an item, `camera` when one of its sources
  is (`media.role` `Camera`, a `device.id`, `device.api` `v4l2` or
  `libcamera`), else `screencast`. `AudioChange::Video` carries the
  whole list (`VideoHost` turns it into keyed patches); the `audio`
  host ignores it, and an audio loop never binds a video node. Levels
  for the language: `levels(device, bars = 16) -> [float]`, a service
  fn (`fns = audio_fns`, run on the logic thread over the cells): each
  evaluation asks for the device's meter (`ask_levels(id)`, a
  process-wide map counting readers that pokes the running stores when
  it gains a device; `release_levels(id)` at the evaluation scope's
  cleanup) and reads `meters: Vec<AudioMeter>` (`#[store(keyed)]`,
  `AudioMeter { device, bands }` keyed by device, the meter's latest
  64-band spectrum, empty while quiet) through `audio::bars(bands, n)`
  (each bar the loudest band it covers, a band repeated over more bars
  than bands, `n` zeros quiet, `n` at most `audio::MAX_BARS` 512). The
  store meters the tapped targets plus the asked devices while a reader
  is visible; a device no evaluation reads any more is let go
  `audio::LEVELS_LEASE` (1 s) after its last reader went, and one still
  read but not evaluated again within the lease while shown lapses
  (its item leaves `meters`) whether or not its meter sent anything (a
  quiet device sends nothing); while no reader is visible no lease
  runs, so a bar shown again restarts its meters. A device id of 0 (the schema's
  default record) or one not there reads `n` zeros.
  `audio::configure(Some(AudioConfig))` points stores started later at
  a socket (tests) and (services-round) the `video` store too; `strand
  run` uses PipeWire's own default. Levels:
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
    (`busy`, `failed`, `message`, `prompt`, `submit(password)`; m6-lock
    added `message` and `prompt`), with an `AUTH` constant and
    a `schemas()` entry, and it is registered in `Builtin`. Like every
    other builtin service it keeps its `provisional service auth` stub
    in builtin.schema, which the extension replaces, so the compiler's
    own tests and the grammar's lock example still check without the
    service crates (decisions.md, m4-lock-w1). Its store runs a
    `strand_auth::Client` built with `child::restore_in_child` as the
    helper's `pre_exec`, started with the store and killed when it
    stops; each conversation runs on a blocking task, its texts and
    prompts reach the store over a channel as `message` and `prompt`,
    and a submit answers the prompt PAM waits on or the next, `busy`
    until PAM answers it; one arriving during a check is dropped. While
    the binary's `auth::LockSignal` (`AuthConfig::lock`) says a lock is
    shown, the store keeps a conversation open: one begins when the
    lock shows and again after one a password was given to that did not
    unlock; one with no password in it ends when the lock goes, and
    after one that ended with no password only a submit begins the
    next. `LockSignal::wake`, which the binary calls for each key and
    pointer event on a shown lock, ends a conversation waiting on a
    prompt with no password given that PAM asked only after waiting
    `auth::READER_WAIT` (2 s) or longer since its first text (the reader
    ahead of `pam_unix` waited and gave up) and begins the next, so the
    reader listens again; a prompt that followed its texts at once (a
    banner, a lockout's texts) is left alone. `LockSignal::wakes` counts
    the wakes (decisions.md m6-lock, lock-r2-1, lock-r3-1; `READER_WAIT`
    is an interface addition). The password is a
    `strand_auth::Password` from the moment it arrives, wiped once
    sent. A success hands the `strand_auth::UnlockToken` to the
    `auth::UnlockSink` the binary sets with `auth::configure`
    (`AuthConfig { helper, timeout, sink, failed, session, client_hook,
    lock }`),
    which passes it to the surface manager's unlock (a conversation
    with no password in it, a finger's, mints one too); a refusal of a
    password, or a check with one in it that could not be made, sets
    `failed`, and one with no password in it (a refusal such as
    `pam_faillock`'s lockout, a cancelled or failed conversation)
    leaves `failed` as it was. A check with a password in it that
    could not be made also goes to the `auth::FailureSink` and is a
    warning diagnostic (one with none is only logged), as is the `login`
    fallback (once per process). The blocking task hands the verdict to
    the sinks itself, with the `auth::SessionTag` value read when the
    conversation began (`Fn(UnlockToken, u64)`, `Fn(&str, u64)`), so a check
    outlives a store that stops mid-check (a lock that reads no `auth`
    field holds no reader; m4-audit).
  - Audio: `Levels` carries FFT bins for a `spectrum` tap. The FFT
    (`audio::spectrum::Fft`, an in-place radix-2 transform; realfft
    until decisions.md m4-effects-media-w2) runs on the audio thread only while a reader is visible,
    and stops while the source is silent. Built (m4-effects-media):
    each meter's data thread keeps its last 4,096 samples mixed to mono
    in a ring of atomics (`audio::spectrum::Ring`); a reading with sound
    (at most one a frame) carries `Levels::bins`, a Hann-windowed
    2,048-point FFT folded into `spectrum::BANDS` (64) bands evenly
    spaced in pitch from 40 Hz to 16 kHz, each the loudest bin in it in
    dB from −72 dBFS (0) to 0 dBFS (1); a quiet reading carries none, and
    a meter sends nothing on silence, so no FFT runs then
    (`crates/strand-services/tests/audio.rs::a_test_tone_lights_its_spectrum_band`).
  - wm: `ProtoCmd::Capture(Vec<capture::Want>)` on the `strand-toplevel`
    thread captures windows by their `Window::toplevel` identifiers
    through ext-image-copy-capture, for `thumbnail`. The entry point is
    process-wide like the audio taps: `wm::capture::capture_window(window
    id, max: (u32, u32), on_frame: Fn(Option<&CaptureFrame>) + Send +
    Sync) -> CaptureTap`, the session living while any tap of that window
    does (dropping the tap ends it); `on_frame(None)` comes once when the
    window the tap was capturing is no longer listed (it closed), so the
    thumbnail stops showing its last frame. The running `wm` service
    maps window ids to identifiers and sends the wants whenever the taps
    or windows change. One session per window, one frame in flight, frames only when
    the compositor reports the window changed and at most
    `capture::MAX_FPS` (15) a second, into a memfd `wl_shm` buffer
    following the session's buffer constraints; a failed session retries
    after a second, a stopped one ends and is replaced after that second
    while its window is still wanted and listed (a closed window leaves
    the list). A
    `CaptureFrame { width, height, pixels: Arc<[u8]> }` is premultiplied
    BGRA, scaled down (box filter) to cover `max` (0 for no limit). The
    callback runs on the protocol thread. Frames reach render through the
    binary (`Renderer::feed_frame(node, Option<ThumbnailFrame>)`, the same
    fields as `CaptureFrame`), only while the thumbnail is visible:
    `take_feed_demand` lists it as `FeedKind::Thumbnail { window, max }`
    (its source's window id, its drawn physical size rounded up to
    `THUMBNAIL_STEP` = 64), and `run/feeds.rs` holds one `CaptureTap` per
    such node, dropping what a tap sent once it is no longer its node's.
    A thumbnail whose source names another window drops its frame at
    once, rather than showing the old window until the new one's first
    frame (`crates/strand-services/tests/capture.rs`, against
    strand-fake-wayland's ext-image-copy-capture;
    `crates/strand-render/tests/thumbnail.rs`).
    (services-round) A screen: `thumbnail(Window | Screen -> source)`,
    a `Screen`'s source being its id. `wm::screen::capture_screen(
    connector, max, f: FnOnce(Option<CaptureFrame>) + Send) ->
    ScreenShot` takes one frame on a `strand-screenshot` thread with a
    connection of its own to the display `wm::configure` names
    (`wm::service::wayland_target`, else `$WAYLAND_DISPLAY`): through
    `ext_output_image_capture_source_manager_v1` and
    ext-image-copy-capture when offered, else `zwlr_screencopy_manager_v1`
    (`capture_output` without the cursor, `y_invert` flipped), into a
    memfd buffer in ARGB8888 or XRGB8888, scaled by `downscale` and
    turned by `upright`; `f(None)` when the connector is not there, no
    protocol is offered, the copy fails, the thread cannot start
    (lockmon review) or `screen::SHOT_WAIT` (3 s) passes; a dropped
    `ScreenShot` never calls `f`. `run/feeds.rs`
    learns each plugged monitor's connector by its `Screen` id
    (`Feeds::screen(id, Option<connector>)`, from the host's monitor
    callbacks) and turns a `FeedKind::Thumbnail` whose text is one into
    `Want::Screen(connector, max)`, a `Tap::Shot` taken each time the
    node becomes visible and kept while it is
    (`crates/strand-services/tests/screen.rs`, against the fake's output
    source and headless sway's screencopy;
    `crates/strand/src/run/feeds.rs::tests::a_screens_thumbnail_takes_a_frame_of_its_monitor`).
    (lockmon) Stills: in `before_lock` the host calls
    `Feeds::take_lock_stills(connectors, LOCK_STILL_WAIT)` (500 ms at
    most, blocking) for the monitors the lock's `thumbnail screen`s
    name; a lock thumbnail is `Want::LockStill` and shows only that
    still (nothing when there is none: never a live frame of the lock),
    and the stills go at the lock's end (`Feeds::lock_ended(renderer)`,
    from the host's `lock_changed` on `Unlocked` or `Finished` and the
    guard's end, which also feeds `None` to every lock thumbnail fed one,
    so no still shows at a later lock; a missing still feeds `None`).
    (lockmon review) Every still is taken at most `STILL_MAX` =
    1280×720; `Feeds::prune(now)`, called each loop turn with
    `Feeds::wait(now)` in the loop's wait, lets a pre-map still go
    `STILL_KEEP` after it was taken and gives up a shot that has not
    answered in `SHOT_GIVE_UP` (`SHOT_WAIT` + 2 s)
    (`feeds.rs::tests::stills_and_shots_are_let_go_in_time`). Before a
    non-lock surface maps (`surface_attached`) the host calls
    `Feeds::before_map(surface, screens_under(tree, node))`, which
    starts a shot of each monitor its `thumbnail screen`s name and holds
    the surface's first frame (`Feeds::holds`, `HOLD` = 300 ms, read by
    `wants_frame` and `frame_deadline`); a thumbnail of that monitor
    shows the still (`Tap::Still`, kept `STILL_KEEP` = 2 s) or waits for
    it (`Tap::Waiting`), so it never captures the surface showing it
    (`feeds.rs::tests::a_lock_thumbnail_shows_the_still_from_before_the_lock`,
    `a_still_from_before_a_surface_mapped_is_what_its_thumbnail_shows`;
    `crates/strand/tests/acceptance.rs::a_panels_screen_thumbnail_is_a_still_from_before_it_opened`).
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
  Role::{Shader, Settings, Wallpaper, Other, Asset})` the program references;
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
  (M6) `Role::Asset` is a local file a mounted `image`, `svg` or
  `lottie` draws: `Instance::asset_files()` (each bound `source`'s
  current value made a path, `check::assets::file_of`; kept per binding
  and dropped by its cleanup; `take_assets_changed()` says when it
  moved, checked right after each step) joins the logic thread's
  `Job::Referenced` list (`Shell::watch_settings`; its `at` is when the
  set was taken, and an asset file newly registered and modified since
  then, less `ASSET_CATCH_UP`, is reported at once). The worker hands
  the asset files a batch changed to `live::Job::Assets(AssetSink)`'s sink, which `strand run`
  forwards to the main thread's `Renderer::assets_changed(&paths)`: no
  reload. There `ImageStore::invalidate_files` keeps a drawn decode of
  such a path as outdated (drawn until its re-decode replaces it,
  undrawn ones dropped, in-flight decodes and `svg:`/`lottie:` file jobs
  stale, an animated source's timeline and worker players
  (`image::Job::Forget`) gone), each `SvgSource`/`LottieSource` whose
  source is one reads its file again (the old text or composition drawn
  meanwhile; a generation in its `state` draws the new one), and the
  surfaces showing them paint whole.
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
