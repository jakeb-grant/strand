# Feature checklist

Every feature in `design.md`, grouped by the milestone that delivers it. Tick
a box only when a test proves it; put the test's path after the item.
Milestone exit criteria are the gates; a milestone is done when all of its
boxes and its exit criteria are ticked.

## M0 Spike (weeks 1–6)

Exit: [x] ≤34 MB PSS on 2 monitors · [x] no wakeups between minute ticks ·
[x] ≤2,000 px² damage per tick — measured by `scripts/m0-exit.sh` over six
boots (21.3–21.6 MB, 0 context switches from :03 to :57, 444–456 / 690–705 px²
per frame at 1.0 / 1.25, at most 1,161 px² per tick over both outputs, no
correction frame after boot); see `docs/m0-report.md`; in `cargo test`:
`crates/strand/tests/demo.rs` (`demo_bar_on_two_outputs_then_idle`: PSS ≤ 34 MB,
idle, alignment on 2560@1.0, 2560@1.25 and a hotplugged 1920@1.0; PSS held to
34 MB in a release run, to a 40 MB debug ceiling in a debug run; SKIPPED
without sway, so not yet in CI, see the CI line below), `crates/strand/src/demo/mod.rs`
(`a_minute_tick_repaints_at_most_2000_px2`)

- [x] `strand-scene` vocabulary: geometry, colour (sRGB ↔ OKLab), `Damage` (≤8 rects, merge), `Painter`, scene protocol types — `crates/strand-scene/src/{geometry,color,damage,id,paint,protocol,surface,tokens}.rs` (unit + proptest), `crates/strand-render/tests/damage.rs` (`surface_specs_resolve_tokens_and_report_changes`)
- [x] SCTK layer-shell bar on every output, anchored to an edge, exclusive zone — `crates/strand-surface/tests/sway.rs` (`bar_on_every_output_with_hotplug`, `bars_on_two_outputs_at_startup`, `floating_bar_margins`: margin 8, 8, 0 on sway), `crates/strand-surface/src/placement.rs`
- [x] Output hotplug: bar appears on a new output and is destroyed when one goes; layer-surface `closed` handled — `crates/strand-surface/tests/sway.rs` (`bar_on_every_output_with_hotplug`; `replugged_monitor_keeps_its_surface_ids`: disable/enable within 30 s gives back the same `SurfaceId` with `monitor_added(.., reconnected: true)`), `crates/strand-surface/src/monitor.rs` (30 s retention)
- [x] wl_shm pool, 2–3 buffers per surface, buffer age tracked per buffer — `crates/strand-surface/src/shm.rs` (`a_fresh_buffer_copied_forward_has_age_one`: a new buffer starts as a copy of the newest frame, age 1), `crates/strand-surface/tests/sway.rs` (`renders_pixels_with_exact_damage` checks every buffer holds the frame its age claims)
- [x] `damage_buffer` with exact rects; `set_opaque_region` when opaque — `crates/strand-surface/tests/sway.rs` (`renders_pixels_with_exact_damage` compares the sent rects with the painter's; `fractional_scale_buffers_and_viewport` checks the logical opaque region at 1.5), `crates/strand-surface/tests/render.rs` (the real renderer through the surface manager)
- [x] Fractional scale (`wp_fractional_scale_v1`) + viewporter; crisp at 1.0, 1.25, 1.5, 2.0 — `crates/strand-surface/tests/sway.rs` (`fractional_buffers_are_crisp`: a 1-px checkerboard at 33 × 1.25 = 41.25, 33 × 1.5 = 49.5, 33 × 2 and 33 × 1 shown 1:1; `fractional_scale_buffers_and_viewport`, `integer_scale_fallback`, `renders_pixels_with_exact_damage`)
- [x] Frame callbacks requested only while something is dirty or unsettled — `crates/strand-surface/tests/sway.rs` (`idle_requests_no_frames_and_commits_nothing`; `commits_lock_to_the_refresh_rate`: 100 changes in 200 ms are coalesced, every frame presented on a later refresh than the one before (the compositor's presentation timestamps); `empty_first_paint_does_not_stall`), `crates/strand-surface/tests/render.rs` (`first_frame_waits_for_its_text`: with the text worker the first commit already has its text and the hold's timer is cancelled)
- [x] `wp_presentation` feedback as the frame clock; injectable fake clock for tests — `crates/strand-surface/src/clock.rs` (60 Hz, 144 Hz, jitter), `crates/strand-surface/tests/sway.rs` (`presentation_feedback_feeds_the_frame_clock`)
- [x] vello_cpu (single-threaded) paints the scene IR into shm with rect clips to damage — `crates/strand-render/tests/damage.rs` (`clock_tick_damage_is_small_and_exact`, `random_edits_match_full_repaint`, `clock_tick_on_4k_rasterises_only_the_damage`)
- [x] Retained scene → display list → damage diff (only changed nodes' bounds) — `crates/strand-render/tests/damage.rs`
- [x] Text: parley shaping on the text worker; swash rasterisation; LRU glyph atlas per scale — `crates/strand-text/tests/text.rs` (`huge_distinct_glyphs_stay_within_the_byte_budget`), `crates/strand-render/tests/damage.rs` (`text_survives_output_hotplug`, `worker_rescale_keeps_text_on_the_first_frame`, `first_frame_of_a_new_surface_has_its_text`, `atlas_mirror_stays_bounded`, `new_surface_of_another_width_waits_for_its_own_layout`), `crates/strand-render/src/renderer.rs` (`one_text_on_two_widths_at_one_scale_aligns_on_each`: a layout per scale and line box width)
- [x] Offline render tests: scenes → PNG compared to references within tolerance — `crates/strand-render/tests/scenes.rs`
- [x] Clock tick aligned to the minute boundary; process sleeps between ticks — `crates/strand/src/demo/clock.rs` (`boundary_is_the_next_whole_minute`, `timer_sleeps_until_its_absolute_time`: a `CLOCK_REALTIME` timerfd armed at the absolute next minute), `crates/strand/src/demo/logic.rs` (`the_thread_ticks_at_each_boundary_and_ends_when_hung_up`: the real loop on a 200 ms period sends one diff per boundary, never early, and ends once hung up; `a_minute_tick_is_one_diff_with_one_text_prop`, `first_tick_emits_the_bar_with_the_clock`: idle with no deadline), `crates/strand/tests/demo.rs` (no context switches while idle), `scripts/m0-exit.sh` (a whole minute)
- [x] 10k-node reactive graph benchmark (propagation latency, memory per node) — `crates/strand-core/benches/graph.rs`, shape checked by `crates/strand-core/tests/bench_graph.rs`, results in `docs/benchmarks.md`
- [x] mimalloc allocator in the runtime binary — `crates/strand/src/main.rs` (`mimalloc_is_the_global_allocator`)
- [x] Measurement script: PSS, wakeups, damage per tick, on headless sway with 2 outputs — `scripts/m0-exit.sh` (also a hotplugged third output of another width at the same scale; results in `docs/m0-report.md`)
- [ ] CI runs the Wayland and budget tiers on every push (design.md, "Testing"): install sway, grim and fonts-dejavu-core in `.github/workflows/ci.yml` (`sudo apt-get install -y sway grim fonts-dejavu-core`) so `crates/strand/tests/demo.rs` and `crates/strand-surface/tests/sway.rs` run instead of printing SKIPPED, and add `cargo test --release -p strand --test demo` so the 34 MB gate is checked on a release build. Until then the budgets are checked by hand with `scripts/m0-exit.sh`. Owner: the lead (`ci.yml` belongs to no track); needed before M1 links in the compiler, VM and watcher.

## M1 Language and live reload (weeks 7–16)

Exit: [ ] 10k random edits with no panic or blank frame · [ ] under 50 ms from save to pixels

Language (`docs/grammar.md`):
- [ ] Lexer: snake_case idents, `$token.path`, numbers with units (`px`, `%`, `deg`, `ch`, `s`, `ms`), hex colours, strings, comments
- [ ] One call shape `kind [positional] { props; children }`; props end at `;` or newline; comma shorthands
- [ ] Surfaces: `bar`, `panel`, `osd`, `popup`, `lock`; `screens:`; `bar` on every monitor with `screen` in scope
- [ ] `component Name(params with defaults)` + `slot`; component `tokens { }` block
- [ ] `state` / `state … persist` / `state x from "file.toml" { typed fields }` / `let` / `export`
- [ ] `enum`, `type` (records), keyed collections `state xs: [T] key f = []`
- [ ] `when cond { props }`; `hover`, `pressed`, `focused`, `selected`; `id:` and `other.hover`; later `when` wins
- [ ] `if`/`else`, `match`, `for x in xs [key e]`; plain data without a key is an error (core side done: a `for` over a plain list expression is `rt.keyed_memo(key_fn, f)` / `Memo::keyed` / `AsyncMemo::keyed`, published as keyed diffs, duplicate keys an error value — `crates/strand-core/tests/keyed_props.rs::keyed_memo_matches_naive_and_publishes_keyed_diffs`, `keyed_memo_reports_duplicate_keys_as_a_value_and_recovers`, `memo_and_async_lists_feed_keyed_loops`, `a_fast_changing_keyed_memo_is_not_rate_throttled`)
- [ ] `enter {}` / `exit {}` poses; exit mirrors enter
- [ ] Events: `on click`, `on secondary`, `on scroll(dy)`, `on show`, `on activate`, `on drop(p: T, at: int)`, `on change a, b [after T]`, `on notifications.received(n)` (runtime side done in strand-core: `on_change`, `on_change_after`, `on_change_keyed` for "never on a sink switch", `EventQueue` with a runtime cycle guard, `input_events` for `on click`/`on scroll` (not rate-counted) — `crates/strand-core/tests/handlers.rs`, `tests/feedback.rs::smooth_input_scrolling_is_neither_warned_nor_delayed`, `tests/graph.rs::on_change_skips_first_value`, `tests/handler_semantics.rs::on_change_keyed_ignores_a_sink_switch`, `listeners_reemitting_in_a_loop_are_a_cycle`, `on_change_fires_once_per_outside_write_after_writers_settle`; service events kept for a frozen listener — `tests/disposal.rs::service_events_wait_for_a_frozen_listener_without_delaying_others`)
- [ ] Timers: `after T while cond { }`, `every T while cond { }` (runtime side done in strand-core: `Runtime::after/every` — `crates/strand-core/tests/handlers.rs::after_while_pauses_and_resumes`, `every_while_repeats_only_while_true`, `tests/handler_semantics.rs::a_timer_sees_a_pause_written_in_the_same_tick`, `every_keeps_its_phase_despite_late_ticks`, `zero_period_every_pauses_and_reports`, `timers_due_together_fire_in_creation_order`)
- [ ] Two-way binding `prop: <-> target`
- [ ] Expressions: `?.`, `??`, ternary, lambdas `x => e`, method calls, named args `f(months: -1)`, `match` expressions
- [ ] Spring override `prop: value ~ $motion.bouncy | ~ 200ms | ~ instant | ~ ease(..) | ~ bezier(..)`
- [ ] Token declarations: `tokens base { … }`, `extends`, `override`, `use tokens … , palette …`, `set { $x: … }`
- [ ] `service x from dbus system "…" { field: type rw = Prop }`, `from file|listen|poll`, `permit exec`
- [ ] `fn` (pure), `keyframes`, `shader "x.wgsl" { uniforms }`, `canvas { draw: … }`
- [ ] Parser recovery: never panics; every error has file, line, caret, label

Checking and runtime:
- [ ] Name resolution across files, no imports; `file.name` export paths
- [ ] Type checker: records, enums, `Async<T>` vs `T`, nullable `?`, durations, colours, lengths
- [ ] Errors: unknown name with did-you-mean; redeclaration across files; assignment to `let` or bound prop; static cycles name the path
- [ ] Bytecode lowering + VM evaluating bindings against `strand-core` signals
- [ ] `strand run` wiring beyond the M0 demo: the `SurfaceHost` hooks `monitor_added`/`monitor_changed`/`monitor_removed`/`monitor_forgotten` forwarded to logic as the `screens` service; one `bar` instance per monitor, pinned with `Screens::Named`, with `screen` in scope and its state kept across an unplug (`rt.reparent`); render → logic channel for `InputEvent`s (`take_input()`) and layout facts (`self.width`). The demo's one shared bar node on every output is an M0 shortcut, see `docs/decisions.md` (m0)
- [x] Handlers as cancellable coroutines; errors as values; cancelled at next `await` on unmount — `crates/strand-core/tests/handlers.rs` (`unmount_cancels_a_handler_at_its_next_await`, `handler_errors_are_values`), `tests/handler_semantics.rs` (`a_load_started_by_a_handler_outlives_the_handler`, `handlers_do_not_accumulate_nodes`, `a_self_waking_task_does_not_spin_the_flush`, `disposing_a_handler_cancels_its_in_flight_tasks`, `reevaluating_a_handler_keeps_its_tasks`, `woken_tasks_run_in_wake_order_not_slot_order`); VM handlers plug in through `Runtime::spawn` / `spawn_for(site, fut)` / `spawn_input(site, fut)` with `handler_site()`
- [x] Reactive graph: push-pull, glitch-free, equality cut-off, generational ids, stale read is an error value — `crates/strand-core/tests/graph_props.rs` (memos and effects check themselves against naive recomputation when they run; effects that write, scope disposal and stale reads included), `tests/graph.rs`, `tests/disposal.rs` (incl. effects, memos and keyed derivations that own and read a child: `an_effect_that_creates_and_reads_a_child_runs_once_per_tick`)
- [ ] Batching: one `SceneDiff` per tick (core side done: writes coalesce, one `Tick` per flush listing changed watched props — `crates/strand-core/tests/graph.rs::writes_coalesce_and_effects_run_once_per_tick`, `watch_reports_changed_props_once`; the emitter is strand-compiler. Effects run in creation order, not a computed topological order: an effect re-triggered by a later effect's write re-runs in the same flush; `on change` handlers run in a late phase so they fire once per outside write — see `docs/decisions.md`, core, "Ticks and batching")
- [x] State vs events: latest-value coalescing vs lossless queues — `crates/strand-core/tests/handlers.rs::events_are_lossless_while_state_coalesces`
- [x] Write generation tags (ignore service echoes); >30 writes/s per cell warns and throttles — `crates/strand-core/tests/feedback.rs` (graph-triggered handlers only, smooth 30 Hz leaky bucket once tripped: `more_than_thirty_writes_per_second_warns_and_throttles`, `a_graph_triggered_writer_is_still_throttled`, `smooth_input_scrolling_is_neither_warned_nor_delayed`, `throttling_covers_the_service_write_path`, `a_handler_spawning_one_task_per_event_is_one_writer`, `a_held_write_of_a_disposed_handler_never_lands`, `an_unchanged_newer_write_supersedes_a_held_one`, `a_runaway_loop_started_by_a_click_is_throttled`, `input_tasks_writing_before_their_first_await_stay_exempt`, `timer_body_and_its_task_writes_count_separately`)
- [x] Keyed collections: `push/insert/remove_key/move/update`, `VecDiff`, incremental `filter/map/take/sort_by` — `crates/strand-core/tests/keyed_props.rs` (incl. `keyed_memo_matches_naive_and_publishes_keyed_diffs` for collections derived from plain lists)
- [x] `Async<T>` with `.pending`, `.error`, keeps last value — `crates/strand-core/tests/handlers.rs::async_keeps_previous_value`, `async_load_runs_as_a_cancellable_handler`, `tests/handler_semantics.rs::a_cancelled_load_clears_pending`, `async_memo_follows_its_input_and_supersedes_quietly` (read-only `AsyncMemo`)
- [ ] `persist` storage with default hash

Live reload:
- [ ] Directory inotify (depth 3), `CLOSE_WRITE` + `MOVED_TO` only, scratch-name filter, overflow rescan
- [ ] Symlinks: canonicalise, watch link and target directories; NFS polling fallback
- [ ] 15 ms coalesce; BLAKE3 no-op skip incl. own writes
- [ ] Off-thread compile of changed modules + dependents; atomic commit of the largest consistent set
- [ ] Last good tree kept; compiled cache keyed by source hash + compiler version + schema hash
- [ ] Identity: source span → key/id → position; ambiguity resets with a warning (core side done: `Runtime::reparent` moves live state, keyed cells keep their diff log, handlers create nodes in their current component — `crates/strand-core/tests/disposal.rs::a_moved_scope_survives_its_old_parent`, `an_on_change_handler_moved_to_a_new_owner_creates_nodes_there`)
- [ ] Edit table: token swap, prop patch animates, node add/remove poses, state default adoption rules, name/type change resets one cell, handler restart, timer rescale, surface recreate, service restart, lock deferral (core side of handler restart and timer rescale done: disposing a handler cancels and reports its in-flight `await` — `crates/strand-core/tests/handler_semantics.rs::disposing_a_handler_cancels_its_in_flight_tasks`; `Timer::rescale_from` / `Debounced::rescale_from` carry the countdown's lifecycle — `tests/handlers.rs::reactive_duration_and_rescale`, `rescaling_from_an_after_that_fired_never_fires_again`, `a_debounce_restarted_mid_countdown_fires_once_at_the_rescaled_time`)
- [ ] Merkle hashes over handler reachability
- [ ] Error overlay after 250 ms quiet; did-you-mean; click to `$EDITOR`; runtime fault freezes one component outlined red (core side done: `Runtime::suspend/resume` — `crates/strand-core/tests/disposal.rs::a_suspended_component_freezes_and_resumes`, `resume_wakes_the_host_for_held_work_and_overdue_timers`, `an_effect_moved_out_of_a_suspended_scope_is_not_left_deaf`, `a_nested_scope_moved_out_of_a_suspended_parent_runs_again`, `a_frozen_task_woken_repeatedly_is_held_once`)
- [ ] `strand check`, `strand watch [--json]`, `strand reload [--hard]`, `@reset`
- [ ] Basic LSP in `strand-dev`: diagnostics, completion after `$` `.` `<->`, hover, rename
- [ ] Reload fuzzer: five save styles, cold-boot equivalence

## M2 Layout, animation, tokens: usable v0.1 (weeks 17–24)

Exit: [ ] theme swap under 5 ms · [ ] contrast never below 3:1 · [ ] the four example shells run unchanged

- [ ] taffy layout: `row`, `col`, `stack`, `grid`, `scroll`, `list` (virtualised), `split`, `spacer`; flex + min/max; `place: absolute`
- [ ] Container queries `when self.width < N` with 4 px hysteresis, ≤1 extra pass
- [ ] Hit testing on rounded shape; `hit: grow(n)`; shadows don't enlarge input region
- [ ] Springs on every visual prop; `$motion.spatial` vs `$motion.effects`; retarget keeps velocity
- [ ] Paint-only props never relayout; size springs relayout only under nearest size-stable ancestor
- [ ] FLIP reordering; `enter`/`exit` on surfaces, `if` branches, list items, pages
- [ ] Inherited props (`font`, `color`)
- [ ] Token graph: palette → base → component tiers; derived tokens stay derived; cycles error; gamut mapping
- [ ] Token methods `alpha`, `mix`, `lighten`, `darken`, `oklch(from …)`
- [ ] Theme swap springs palette roots in OKLab; per-frame token re-evaluation on render thread
- [ ] Contrast guard ≥3:1; crossfade fallback for light↔dark
- [ ] Snap rules: fonts, padded shadow lists, layout lengths; `reduced_motion`
- [ ] `material(seed:)`, `material(image:)` (128 px downscale off-thread, content-hash cache), importers (base16/24, Catppuccin, matugen, W3C) filling the full palette
- [ ] Portal `system.dark`, `system.accent`, `system.contrast`
- [ ] Last palette persisted; no default-colour flash at boot
- [ ] Settings files: per-field validation, `toml_edit` write-back, symlink-following, read-only overlay
- [ ] Widgets: `text`, `icon`, `image`, `box`, `button`, `slider`, `input`, `meter`, `segmented`, `popup`, `tooltip`
- [ ] Shapes and paint: per-corner radius, squircle, borders, shadows (`$elevation`), gradients with dither

## M3 Services (weeks 25–34)

Exit: [ ] runs on Hyprland, niri and sway · [ ] 100 reloads with no reconnects · [ ] memory verified

- [ ] Service contract: `#[service]`, `#[derive(Store)]`, lazy start, refcount, stop 5 s after last reader, visibility gating
- [ ] audio (PipeWire), brightness (logind), battery (UPower), network (nmrs), bluetooth, tray (SNI + DBusMenu), notifications server, workspaces (`ext-workspace-v1` + IPC adapters), windows (`ext-foreign-toplevel-list`), apps (desktop entries, icons, nucleo fuzzy + frecency), portal settings, clock, calendar, media (MPRIS), cpu/memory
- [ ] Hyprland, niri, sway IPC adapters (own implementations)
- [ ] No-code services `from dbus` checked against introspection; `from file|listen|poll`; `permit exec`
- [ ] Service schemas drive type checking and LSP hover
- [ ] Notification name conflict with dunst/mako fails clearly
- [ ] python-dbusmock CI tier

## M4 Power features (weeks 35–44)

Exit: [ ] smooth 2,000-row scrolling · [ ] GPU released when idle · [ ] lock fails closed under faults

- [ ] GPU promotion (vello_gpu/wgpu) for large long animations; switch only when settled; device dropped after 30 s idle
- [ ] Compositor-animated poses: alpha modifier, viewporter scale, layer-shell margins
- [ ] Popups as nested xdg_popups; tray menus; tooltips
- [ ] Virtualised long lists; keyboard `nav:`
- [ ] Drag and drop: `drag:`, typed `Drop`, springs by key
- [ ] `pages current:` with directional transitions; hidden pages unmount
- [ ] Shaders (naga-checked, hot-reloaded); canvas
- [ ] Blur ladder: `ext-background-effect-v1`, Hyprland rules (`strand compositor-rules`), tint fallback; `backdrop: blur()` at quarter scale
- [ ] Lock screen on `ext-session-lock`, forked PAM helper, fail-closed, exempt from reload
- [ ] Effects catalogue: shapes + morphing, strokes, arcs, goo merge, glow, inner shadow, rim, grain, text effects, scrim, filters, blend modes, masks, named curves, pose presets, time signals (`t`, `wave`, `noise`), keyframes, stagger, shared-element `morph`, rolling numbers, jelly, parallax/tilt, built-in effects, particles, transition masks, spectrum, graphs, wavy meter, GIF/APNG/WebP, Lottie, bindable SVG, thumbnails
- [ ] Bundled GPU effects (8) start only while visible
- [ ] Runtime: effect layers in scene IR, cached offscreen groups, CPU raster nodes, per-node clocks with frame caps

## M5 Developer experience and 1.0 (weeks 45–52)

Exit: [ ] newcomer builds a multi-monitor bar in 15 minutes · [ ] language spec frozen

- [ ] Inspector: pick across surfaces, identity, layout, token provenance, kept-state badges, repaint flashing, safe write-back
- [ ] CLI + IPC: `get | set | toggle | watch | call` over socket and D-Bus; runtime overlays and "overlay wins" notice
- [ ] `strand new`, `strand export gtk|kitty|hyprland`, `strand report`
- [ ] Theme importers complete; plugins (`#[service]` crates)
- [ ] LSP: quick-fixes (extract component, missing key), schema hovers
- [ ] Docs: learning ladder levels 1–5, language spec
