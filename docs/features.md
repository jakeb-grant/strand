# Feature checklist

Every feature in `design.md`, grouped by the milestone that delivers it. Tick
a box only when a test proves it; put the test's path after the item.
Milestone exit criteria are the gates; a milestone is done when all of its
boxes and its exit criteria are ticked.

## M0 Spike (weeks 1–6)

Exit: [ ] ≤34 MB PSS on 2 monitors · [ ] no wakeups between minute ticks ·
[ ] ≤2,000 px² damage per tick

- [x] `strand-scene` vocabulary: geometry, colour (sRGB ↔ OKLab), `Damage` (≤8 rects, merge), `Painter`, scene protocol types — `crates/strand-scene/src/{geometry,color,damage,id,paint,protocol,surface,tokens}.rs` (unit + proptest), `crates/strand-render/tests/damage.rs` (`surface_specs_resolve_tokens_and_report_changes`)
- [ ] SCTK layer-shell bar on every output, anchored to an edge, exclusive zone
- [ ] Output hotplug: bar appears on a new output and is destroyed when one goes; layer-surface `closed` handled
- [ ] wl_shm pool, 2–3 buffers per surface, buffer age tracked per buffer
- [ ] `damage_buffer` with exact rects; `set_opaque_region` when opaque
- [ ] Fractional scale (`wp_fractional_scale_v1`) + viewporter; crisp at 1.0, 1.25, 1.5, 2.0
- [ ] Frame callbacks requested only while something is dirty or unsettled
- [ ] `wp_presentation` feedback as the frame clock; injectable fake clock for tests
- [x] vello_cpu (single-threaded) paints the scene IR into shm with rect clips to damage — `crates/strand-render/tests/damage.rs` (`clock_tick_damage_is_small_and_exact`, `random_edits_match_full_repaint`, `clock_tick_on_4k_rasterises_only_the_damage`)
- [x] Retained scene → display list → damage diff (only changed nodes' bounds) — `crates/strand-render/tests/damage.rs`
- [x] Text: parley shaping on the text worker; swash rasterisation; LRU glyph atlas per scale — `crates/strand-text/tests/text.rs` (`huge_distinct_glyphs_stay_within_the_byte_budget`), `crates/strand-render/tests/damage.rs` (`text_survives_output_hotplug`, `worker_rescale_keeps_text_on_the_first_frame`, `first_frame_of_a_new_surface_has_its_text`, `atlas_mirror_stays_bounded`)
- [x] Offline render tests: scenes → PNG compared to references within tolerance — `crates/strand-render/tests/scenes.rs`
- [ ] Clock tick aligned to the minute boundary; process sleeps between ticks
- [ ] 10k-node reactive graph benchmark (propagation latency, memory per node)
- [ ] mimalloc allocator in the runtime binary
- [ ] Measurement script: PSS, wakeups, damage per tick, on headless sway with 2 outputs

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
- [ ] `if`/`else`, `match`, `for x in xs [key e]`; plain data without a key is an error
- [ ] `enter {}` / `exit {}` poses; exit mirrors enter
- [ ] Events: `on click`, `on secondary`, `on scroll(dy)`, `on show`, `on activate`, `on drop(p: T, at: int)`, `on change a, b [after T]`, `on notifications.received(n)`
- [ ] Timers: `after T while cond { }`, `every T while cond { }`
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
