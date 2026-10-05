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
- [ ] 10k-node reactive graph benchmark (propagation latency, memory per node)
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
- [ ] `if`/`else`, `match`, `for x in xs [key e]`; plain data without a key is an error (syntax done: `crates/strand-compiler/tests/grammar_rules.rs`, `grammar_examples.strand`; missing-key error is the checker's)
- [ ] `enter {}` / `exit {}` poses; exit mirrors enter (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`toasts.strand`); mirroring is runtime)
- [x] Events (syntax only; `on change` semantics are under Checking and runtime): `on click`, `on secondary`, `on scroll(dy)`, `on show`, `on activate`, `on drop(p: T, at: int)`, `on change a, b [after T]`, `on notifications.received(n)` — `crates/strand-compiler/tests/fixtures.rs` (`osd.strand`, `snippets.strand`), `crates/strand-compiler/tests/diagnostics.rs::did_you_mean_keywords`
- [ ] Timers: `after T while cond { }`, `every T while cond { }` (syntax done: `crates/strand-compiler/tests/fixtures.rs` (`toasts.strand`, `snippets.strand`); pausing timers are runtime)
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
- [ ] Handlers as cancellable coroutines; errors as values; cancelled at next `await` on unmount
- [ ] Reactive graph: push-pull, glitch-free, equality cut-off, generational ids, stale read is an error value
- [ ] Batching: one `SceneDiff` per tick
- [ ] State vs events: latest-value coalescing vs lossless queues
- [ ] Write generation tags (ignore service echoes); >30 writes/s per cell warns and throttles
- [ ] Keyed collections: `push/insert/remove_key/move/update`, `VecDiff`, incremental `filter/map/take/sort_by`
- [ ] `Async<T>` with `.pending`, `.error`, keeps last value
- [ ] `persist` storage with default hash

Live reload:
- [ ] Directory inotify (depth 3), `CLOSE_WRITE` + `MOVED_TO` only, scratch-name filter, overflow rescan
- [ ] Symlinks: canonicalise, watch link and target directories; NFS polling fallback
- [ ] 15 ms coalesce; BLAKE3 no-op skip incl. own writes
- [ ] Off-thread compile of changed modules + dependents; atomic commit of the largest consistent set
- [ ] Last good tree kept; compiled cache keyed by source hash + compiler version + schema hash
- [ ] Identity: source span → key/id → position; ambiguity resets with a warning
- [ ] Edit table: token swap, prop patch animates, node add/remove poses, state default adoption rules, name/type change resets one cell, handler restart, timer rescale, surface recreate, service restart, lock deferral
- [ ] Merkle hashes over handler reachability
- [ ] Error overlay after 250 ms quiet; did-you-mean; click to `$EDITOR`; runtime fault freezes one component outlined red
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
