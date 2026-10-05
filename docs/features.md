# Feature checklist

Every feature in `design.md`, grouped by the milestone that delivers it. Tick
a box only when a test proves it; put the test's path after the item.
Milestone exit criteria are the gates; a milestone is done when all of its
boxes and its exit criteria are ticked.

## M0 Spike (weeks 1–6)

Exit: [ ] ≤34 MB PSS on 2 monitors · [ ] no wakeups between minute ticks ·
[ ] ≤2,000 px² damage per tick

- [ ] `strand-scene` vocabulary: geometry, colour (sRGB ↔ OKLab), `Damage` (≤8 rects, merge), `Painter`, scene protocol types
- [ ] SCTK layer-shell bar on every output, anchored to an edge, exclusive zone
- [ ] Output hotplug: bar appears on a new output and is destroyed when one goes; layer-surface `closed` handled
- [ ] Monitor identity is make + model + description; a monitor's state survives a 30-second unplug and is restored when it returns
- [ ] wl_shm pool, 2–3 buffers per surface, buffer age tracked per buffer
- [ ] `damage_buffer` with exact rects; `set_opaque_region` when opaque
- [ ] Fractional scale (`wp_fractional_scale_v1`) + viewporter; crisp at 1.0, 1.25, 1.5, 2.0
- [ ] Frame callbacks requested only while something is dirty or unsettled
- [ ] `wp_presentation` feedback as the frame clock; injectable fake clock for tests
- [ ] vello_cpu (single-threaded) paints the scene IR into shm with rect clips to damage
- [ ] Retained scene → display list → damage diff (only changed nodes' bounds)
- [ ] Text: parley shaping on the text worker; swash rasterisation; LRU glyph atlas per scale
- [ ] Offline render tests: scenes → PNG compared to references within tolerance
- [ ] Clock tick aligned to the minute boundary; process sleeps between ticks
- [x] 10k-node reactive graph benchmark (propagation latency, memory per node) — `crates/strand-core/benches/graph.rs`, shape checked by `crates/strand-core/tests/bench_graph.rs`, results in `docs/benchmarks.md`
- [ ] mimalloc allocator in the runtime binary
- [ ] Measurement script: PSS, wakeups, damage per tick, on headless sway with 2 outputs

## M1 Language and live reload (weeks 7–16)

Exit: [ ] 10k random edits with no panic or blank frame · [ ] under 50 ms from save to pixels

Language (`docs/grammar.md`):
- [x] Lexer: snake_case idents, `$token.path`, numbers with units (`px`, `%`, `deg`, `ch`, `s`, `ms`), hex colours, strings, comments — `crates/strand-compiler/src/syntax/lexer.rs` (tests), `crates/strand-compiler/tests/fixtures.rs::fixtures_lex_losslessly`, `crates/strand-compiler/tests/grammar_rules.rs::literals_and_units`
- [x] One call shape `kind [positional] { props; children }`; props end at `;` or newline; comma shorthands — `crates/strand-compiler/tests/grammar_rules.rs` (`positional_then_block`, `items_end_at_semicolons_and_line_breaks`, `commas_spaces_and_transitions_in_values`)
- [ ] Surfaces: `bar`, `panel`, `osd`, `popup`, `lock`; `screens:`; `bar` on every monitor with `screen` in scope (syntax done: `crates/strand-compiler/tests/fixtures.rs`; per-monitor runtime pending)
- [ ] `component Name(params with defaults)` + `slot`; component `tokens { }` block (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`snippets.strand`); slot rendering and `$Name.token` resolution pending)
- [ ] `state` / `state … persist` / `state x from "file.toml" { typed fields }` / `let` / `export` (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`theme.strand`, `toasts.strand`); storage and export paths pending)
- [x] `enum`, `type` (records), keyed collections `state xs: [T] key f = []` — `crates/strand-compiler/tests/grammar_rules.rs::types`, `crates/strand-compiler/tests/fixtures.rs` (`snippets.strand`, `osd.strand`)
- [ ] `when cond { props }`; `hover`, `pressed`, `focused`, `selected`; `id:` and `other.hover`; later `when` wins (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`bar.strand`); evaluation order pending)
- [ ] `if`/`else`, `match`, `for x in xs [key e]`; plain data without a key is an error (syntax done: `crates/strand-compiler/tests/grammar_rules.rs`, `grammar_examples.strand`; missing-key error is the checker's) (core side done: a `for` over a plain list expression is `rt.keyed_memo(key_fn, f)` / `Memo::keyed` / `AsyncMemo::keyed`, published as keyed diffs, duplicate keys an error value — `crates/strand-core/tests/keyed_props.rs::keyed_memo_matches_naive_and_publishes_keyed_diffs`, `keyed_memo_reports_duplicate_keys_as_a_value_and_recovers`, `memo_and_async_lists_feed_keyed_loops`, `a_fast_changing_keyed_memo_is_not_rate_throttled`)
- [ ] `enter {}` / `exit {}` poses; exit mirrors enter (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`toasts.strand`); mirroring is runtime)
- [x] Events (syntax only; `on change` semantics are under Checking and runtime): `on click`, `on secondary`, `on scroll(dy)`, `on show`, `on activate`, `on drop(p: T, at: int)`, `on change a, b [after T]`, `on notifications.received(n)` — `crates/strand-compiler/tests/fixtures.rs` (`osd.strand`, `snippets.strand`), `crates/strand-compiler/tests/diagnostics.rs::did_you_mean_keywords` (runtime side in strand-core in strand-core: `on_change`, `on_change_after`, `on_change_keyed` for "never on a sink switch", `EventQueue` with a runtime cycle guard, `input_events` for `on click`/`on scroll` (not rate-counted) — `crates/strand-core/tests/handlers.rs`, `tests/feedback.rs::smooth_input_scrolling_is_neither_warned_nor_delayed`, `tests/graph.rs::on_change_skips_first_value`, `tests/handler_semantics.rs::on_change_keyed_ignores_a_sink_switch`, `listeners_reemitting_in_a_loop_are_a_cycle`, `on_change_fires_once_per_outside_write_after_writers_settle`; service events kept for a frozen listener — `tests/disposal.rs::service_events_wait_for_a_frozen_listener_without_delaying_others`)
- [ ] Timers: `after T while cond { }`, `every T while cond { }` (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`toasts.strand`, `snippets.strand`); pausing timers are runtime) (runtime side done in strand-core: `Runtime::after/every` — `crates/strand-core/tests/handlers.rs::after_while_pauses_and_resumes`, `every_while_repeats_only_while_true`, `tests/handler_semantics.rs::a_timer_sees_a_pause_written_in_the_same_tick`, `every_keeps_its_phase_despite_late_ticks`, `zero_period_every_pauses_and_reports`, `timers_due_together_fire_in_creation_order`)
- [ ] Two-way binding `prop: <-> target` (syntax done: `crates/strand-compiler/tests/grammar_rules.rs::commas_spaces_and_transitions_in_values`; writable-target check pending)
- [x] Expressions (syntax only; `??`/`?.` with `Async` are under Checking and runtime): `?.`, `??`, ternary, lambdas `x => e`, method calls, named args `f(months: -1)`, `match` expressions — `crates/strand-compiler/tests/grammar_rules.rs` (`precedence`, `lambdas`, `named_and_from_arguments`, `match_arms_split_on_commas_or_lines`)
- [x] Spring override (syntax only; per-prop transitions are under M2) `prop: value ~ $motion.bouncy | ~ 200ms | ~ instant | ~ ease(..) | ~ bezier(..)` — `crates/strand-compiler/tests/fixtures.rs` (`snippets.strand`), `crates/strand-compiler/tests/grammar_rules.rs::commas_spaces_and_transitions_in_values`
- [x] Token declarations (syntax only; override rules are under Checking, `set { }` under M2 tokens): `tokens base { … }`, `extends`, `override`, `use tokens … , palette …`, `set { $x: … }` — `crates/strand-compiler/tests/fixtures.rs` (`theme.strand`, `snippets.strand`), `crates/strand-compiler/tests/grammar_rules.rs::tokens_keys`
- [x] `service x from dbus system "…" { field: type rw = Prop }`, `from file|listen|poll`, `permit exec` — `crates/strand-compiler/tests/fixtures.rs` (`grammar_examples.strand`, `snippets.strand`), `crates/strand-compiler/tests/diagnostics.rs::did_you_mean_keywords`
- [ ] `fn` (pure), `keyframes`, `shader "x.wgsl" { uniforms }`, `canvas { draw: … }` (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`grammar_examples.strand`, `snippets.strand`); purity check pending)
- [x] Parser recovery: never panics (nor does rendering: lines past 1,000 characters fall back to one-line reports); every error has file, line, caret, label; a lone `\r` is a line break — `crates/strand-compiler/tests/robustness.rs` (10,000 random edits with span nesting checked, pathological nesting rendered, `errors_far_along_a_long_line_render`), `crates/strand-compiler/tests/grammar_rules.rs::a_lone_carriage_return_is_a_line_break`, `crates/strand-compiler/tests/diagnostics.rs` (`unclosed_enum_and_match_stop_at_the_next_declaration`, `missing_brace_hint_stays_inside_the_unclosed_block`, `every_error_has_a_located_label` (multi-line), `errors_do_not_cascade` (Allman `{`)), `crates/strand-compiler/tests/grammar_rules.rs::a_dot_or_colon_ending_a_line_does_not_take_the_next_line` (dangling `<->`, `~`, `,`, `??`, integer token keys, a closing bracket on the next line)
- [x] Did-you-mean at every keyword position, the misspelt word then read as the keyword: declarations (`compnent`, `tokns`), clause keywords (`key`, `persist`, `from`, `extends`, `after`, `rw`, `else`, `override`, `export stat`, a component's `tokens`, `permit exec`, a poll's `every`), and handler keywords known by their block of statements (`aftr 6s { n.expire() }`); never for short names (`n.expire()` is not `on`); a statement in a tree says statements go in a handler; snake_case fixes for `max-width:` / `$fg-muted` — `crates/strand-compiler/tests/diagnostics.rs` (`did_you_mean_keywords`, `a_misspelt_clause_keyword_does_not_ask_for_a_value`, `a_misspelt_keyword_parses_as_the_keyword`, `statements_in_a_tree_are_not_misspelt_keywords`, `misplaced_elements_are_not_misspelt_keywords`)
- [x] Diagnostics carry file identity (`FileId` + `SourceMap`); one diagnostic can label several files; rendering capped per file — `crates/strand-compiler/src/diagnostic.rs` (tests `labels_in_other_files_render_there`, `rendering_is_capped_per_file`), `crates/strand-compiler/tests/diagnostics.rs::diagnostics_carry_their_file`
- [ ] Formatter (`strand fmt` / LSP formatting) and format-on-save that never flashes the error overlay
- [ ] tree-sitter grammar for `.strand` in `strand-dev` (editor highlighting), kept in step with `docs/grammar.md`

Checking and runtime:
- [ ] Name resolution across files, no imports; `file.name` export paths
- [ ] Type checker: records, enums, `Async<T>` vs `T`, nullable `?`, durations, colours, lengths; space-separated (`Spaced`) values only in shadow-list and font props, elsewhere an error suggesting commas (`margin: 8 8 0` → `8, 8, 0`)
- [ ] `??` covers a pending or failed `Async` (and null); `?.` short-circuits on null
- [ ] `on change a, b` never fires at boot, on reload or on a sink switch; `after T` debounces (the OSD example)
- [ ] Errors: unknown name with did-you-mean; redeclaration across files; assignment to `let` or bound prop; static cycles name the path
- [ ] Unknown-element did-you-mean includes tree and top-level keywords (`whn hover { … }` → `when`, `enterr { … }` → `enter`, and `exit`, `slot`, `set`, `play`, `else`; the parser already catches `els { }` right after an `if` body)
- [ ] Loud token overrides: redefining a token needs `override`; a misspelt `override` is an unknown-name error, not a new token
- [ ] Lint: a raw hex colour in a prop warns (use a token); defaults in settings files are exempt
- [ ] Bytecode lowering + VM evaluating bindings against `strand-core` signals
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
- [x] Config `.strand` module-set discovery (`source::find_files`), used by `strand check` (the binary hands its result to the loader and watcher): depth 3 (deeper directories holding `.strand` files are warned about), hidden names skipped, symlinks followed breadth-first, canonical dedup, unreadable sub-directories and dangling `*.strand` links reported — `crates/strand/src/check.rs` (`finds_strand_files_to_depth_three`, `a_deeper_link_does_not_hide_the_real_directory`, `a_dangling_strand_link_is_reported`, `unreadable_subdirectories_do_not_stop_the_check`, `a_single_file_can_be_checked`)
- [ ] Loader and `strand-watch` use `source::find_files` for the module set (the binary calls it and passes the paths in; `strand-watch` does not depend on the compiler); the watcher additionally watches `.wgsl`, settings files, wallpapers and `Discovery::dirs` (link targets)
- [ ] `strand check`, `strand watch [--json]`, `strand reload [--hard]`, `@reset` (`strand check` parses and reports: `crates/strand/src/check.rs` tests; `@reset` parses: `snippets.strand`; checker, watch, reload pending)
- [ ] Basic LSP in `strand-dev`: diagnostics, completion after `$` `.` `<->`, hover, rename
- [ ] Reload fuzzer: five save styles, cold-boot equivalence

## M2 Layout, animation, tokens: usable v0.1 (weeks 17–24)

Exit: [ ] theme swap under 5 ms · [ ] contrast never below 3:1 · [ ] the four example shells run unchanged

- [ ] taffy layout: `row`, `col`, `stack`, `grid`, `scroll`, `list` (virtualised), `split`, `spacer`; flex + min/max; `place: absolute`
- [ ] Container queries `when self.width < N` with 4 px hysteresis, ≤1 extra pass
- [ ] Hit testing on rounded shape; `hit: grow(n)`; shadows don't enlarge input region
- [ ] Event routing: events go to the innermost handler; `propagate()` passes one on; `keyboard: none | on_demand | exclusive` sets focus
- [ ] `hover` stays latched on the pressed node while dragging
- [ ] Springs on every visual prop; `$motion.spatial` vs `$motion.effects`; retarget keeps velocity
- [ ] Per-prop `~` transitions (`~ $motion.bouncy`, `~ 200ms`, `~ instant`, `~ ease(..)`, `~ bezier(..)`) reach SceneOp `transition`
- [ ] Paint-only props never relayout; size springs relayout only under nearest size-stable ancestor
- [ ] FLIP reordering; `enter`/`exit` on surfaces, `if` branches, list items, pages
- [ ] Inherited props (`font`, `color`)
- [ ] Token graph: palette → base → component tiers; derived tokens stay derived; cycles error; gamut mapping
- [ ] `set { $x: … }` subtree token overrides; `$x` on the right-hand side is the inherited value, not a cycle
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
- [ ] The compiler collects the service paths a shell uses; only those services start (lazy start input)
- [ ] Notification name conflict with dunst/mako fails clearly
- [ ] python-dbusmock CI tier

## M4 Power features (weeks 35–44)

Exit: [ ] smooth 2,000-row scrolling · [ ] GPU released when idle · [ ] lock fails closed under faults

- [ ] GPU promotion (vello_gpu/wgpu) for large long animations; switch only when settled; device dropped after 30 s idle
- [ ] Compositor-animated poses: alpha modifier, viewporter scale, layer-shell margins
- [ ] Popups as nested xdg_popups; tray menus; tooltips
- [ ] Popup/panel `open: <-> x` is written false by Escape, click-away and focus loss
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
