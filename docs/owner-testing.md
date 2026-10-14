# Owner testing: issues and capability gaps

**Wave 2 (2026-10-14).** The states below reflect `laptop/m6-w2`: M6
wave 2's services stream (through `6103464`), language stream
(through `f65ec82`, the same name across files) and surface stream
(through `15a0de7`: `id.box`, conditional ids and the travelling
island, with the reviews' fixes). Each entry's
**Wave 2** line gives its state with the test that proves a fix, or
the decisions.md section that holds a proposal.

What the owner's hands-on testing of Strand finds: bugs, missing
capabilities and rough edges, kept for review before later M6 waves
plan their work. Each entry says what happened, what is known about
why, and its state. Started 2026-10-13, testing `main` at `8b88110`
(the `strand-release-binaries` artifact, GPU build) on the owner's
laptop: Hyprland 0.56.2, four monitors (eDP-1 1920×1200 at 1, DP-6
1920×1080 at 1, DP-8 and DP-10 3840×2160 at 1.5).

The test configs are throwaway (`target/try/`): `config/` is a glass
pill bar written to show the effects catalogue in few lines, and `qs/`
is a loose copy of the owner's Quickshell top bar (floating bar,
workspace pill, status pill, hover islands).

States: **open** (not looked at further), **diagnosed** (cause known,
not fixed), **config** (the test config was wrong, Strand is right;
kept when the trap is worth fixing in docs), **fixed**, **proposed**
(designed and waiting for the owner's ruling, not built), **pending
verification** (fixed where CI can test it, waiting for the owner's
run on Hyprland), **in progress** (being built on a stream's branch).

## Bugs

1. **Hover popups misbehave across monitors.** (open, cause suspected)
   With four monitors, a hover island sometimes opens on a different
   monitor, or away from the bar item on the monitor next to it, and
   the reactivity feels wrong. Suspected cause: every popup but a
   tooltip is opened with an xdg_popup grab
   (`crates/strand-surface/src/placement.rs`, `popup_config`'s
   `grab: !spec.tooltip`). A grab suits click-opened menus; a popup
   opened from hover has no click serial to grab with, and while a
   grab holds, pointer enter and leave go to the grabbing client, so
   `hover` on the bar items can latch or flicker. Not reproduced yet;
   a headless test with two outputs at scales 1 and 1.5 and a virtual
   pointer moving between a bar item, its popup and the other output
   would show it. See also gap 4. The tray menus are hover islands
   too (as in the owner's Quickshell), so they share this.
   **Wave 2: pending the owner's verification on Hyprland** (monitor
   placement). The cause was Hyprland's: it constrains a popup to the
   monitor at its parent layer surface's origin, which the bar's
   shadow overhang pushed onto the left neighbour. Render now keeps a
   layer surface's buffer inside its anchored edges, and a popup grabs
   only for the press that opened it (a hover-opened one never does).
   Proven on headless sway at scales 1 and 1.5
   (`crates/strand/tests/hover_popups.rs::hover_islands_open_under_their_items_on_mixed_scale_outputs`,
   `crates/strand-render/tests/layout.rs::a_layer_surface_buffer_stays_inside_its_anchored_edges`,
   `crates/strand-surface/tests/sway.rs::a_popup_grabs_only_for_the_press_that_opened_it`);
   decisions.md m6-w2-surface.
2. **Animation feels less snappy, lower frame rate, than Quickshell.**
   (open) The owner's impression on the hover islands, the workspace
   indicator and the slots. Not measured yet: `strand report` on the
   owner's session gives frame stats. Candidates: CPU rasterising wide
   surfaces on the 1.5-scale 4K monitors (a 2560-logical bar is 3840
   physical pixels wide), the GPU build not promoting these surfaces,
   and springs (`$motion.spatial`) settling more slowly than
   Quickshell's 200 ms OutBack curves.
   **Wave 2: fixed** by the owner's ruling (2026-10-14, option (b) of
   decisions.md m6-w2-surface, "a snappier default
   `$motion.spatial`"; recorded in decisions.md m6-owner). Measured at
   1.5, there is no raster cost: the 3840×125 bar paints in 4.6 ms,
   the pill's move at 0.16 ms a frame, an island's enter at 1.25 ms a
   frame
   (`crates/strand-render/tests/owner_motion.rs::the_owners_bar_and_island_at_one_and_a_half`).
   What reads as slow is the test config's `enter` on each island's
   card, which never plays (the pose belongs on the popup), and
   spring(700, 0.9) reaching 90% at 129 ms where OutBack does at 60 ms.
   The default `$motion.spatial` is now spring(3000, 0.75): half way
   at 27 ms, 90% at 52 ms, 2.8% overshoot, settled within 0.2% at
   142 ms
   (`crates/strand-scene/src/motion.rs::tests::the_default_spatial_spring_is_snappy`,
   `crates/strand-theme/src/defaults.rs::tests::the_built_in_theme_resolves_every_base_token`).
   Whether it feels right is the owner's to judge on their session.
3. **A window's icon is its raw app id.** (diagnosed) `Window.icon` is
   the app id until "the apps service resolves desktop entries"
   (`crates/strand-services/src/wm/model.rs`, `derive`), and nothing
   resolves it, so Spotify (`Spotify`, XWayland) and Chromium
   (`Chromium-browser`) draw no icon: the icon theme has `spotify`
   and `chromium`. A window should resolve to its desktop entry (by
   id, lowercased id, `StartupWMClass`, reverse-DNS tail), giving its
   icon and categories (gap 9).
   **Wave 2: fixed**
   (`crates/strand-services/tests/apps.rs::windows_find_their_desktop_entries`,
   `crates/strand-services/tests/wm_services.rs::windows_are_linked_to_their_apps`);
   decisions.md m6-w2-services.
4. **`corners: squircle` with `radius: 20` on a 40 px bar draws nearly
   square corners.** (open) Expected a pill-ended squircle; without
   `corners: squircle` the same radius draws the pill.
   **Wave 2: fixed**: a capped squircle corner eases towards the
   circle, so a corner of half the side is the pill
   (`crates/strand-render/tests/paint.rs::a_squircle_bar_of_half_its_height_is_a_pill`);
   decisions.md m6-w2-surface.

5. **Text looks low in its box next to rounded corners.** (open) In
   the islands, `pad: 12` left the first line's letters about 18 px
   below the top edge and 13 px from the left at `radius: 20`; the
   font's ascent (JetBrains Mono) sits above the caps. The test config
   uses `pad: 10, 16, 14, 16` to balance it. Worth comparing Strand's
   line box (ascent, descent, half-leading) with Qt's for the same
   font, which the owner's Quickshell islands draw with the same 12 px
   padding without looking low.
   **Wave 2: config.** Strand's line box is CSS's `line-height:
   normal`, and for JetBrains Mono (line gap 0) it puts the baseline
   where Qt does; the room above the capitals is the font's ascent.
   Uneven padding is the idiom, and nothing changes
   (`crates/strand-render/tests/layout.rs::a_text_line_box_is_css_normal`);
   decisions.md m6-w2-surface. The guide's padding examples could say
   so (docs stream).

## Capability gaps

1. **A handler cannot run a command.** No power menu (suspend,
   reboot, power off, log out), no compositor dispatch, no "open
   settings". Only a declared service may run a program (`permit
   exec`), and D-Bus services expose properties, not method calls, so
   even logind's `Suspend` is out of reach. The owner's Quickshell bar
   has a power popout and a system popout that need it.
   **Owner's ruling (2026-10-13):** shells must be flexible (open a
   custom app, compositor dispatch, a CLI-only tool such as listing
   out-of-date packages and updating them in a terminal). First-party
   services cover the common needs, and two general fallbacks are added:
   declared actions (`action name(params) [argv]` under `permit exec`;
   each parameter is exactly one argv element, never split or passed
   through a shell, and every command stays listed by `strand check`),
   and D-Bus method calls on `service … from dbus` (`action name(params)
   = Method`, checked against introspection). Plugins remain the escape
   hatch for real integrations, not for a button that runs a program.
   Reading command output into the UI already works (`from poll` and
   `from listen`, with JSON, `key=value` or plain text and `split`).
   **Wave 2: fixed**: declared actions and D-Bus method calls
   (`crates/strand-compiler/tests/instantiate.rs::a_declared_action_asks_for_its_command_with_whole_arguments`,
   `crates/strand/src/run/tests.rs::a_failing_actions_command_is_a_watch_notice`,
   `crates/strand/src/services/mod.rs::tests::custom_services::the_design_ppd_service_on_a_mock_power_profiles_daemon`;
   decisions.md m6-w2-language), and the power menu through the
   builtin `session` service (gap 17).
2. **No way to switch to a workspace that does not exist yet.**
   `ws.focus()` exists only on a `Workspace`; Hyprland creates a
   workspace when it is switched to, so an empty slot of a persistent
   workspace cannot be clicked. Needs something like
   `workspaces.focus(id)`.
   **Wave 2: fixed**: `workspaces.focus(target: int | text)`
   (`crates/strand-services/tests/hyprland.rs::configured_and_named_workspaces_are_switched_to_and_made`,
   `crates/strand-services/tests/sway.rs::sway_config_assignments_are_configured_workspaces`);
   decisions.md m6-w2-services.
3. **Persistent workspaces are not exposed.** Hyprland's workspace
   rules (`hyprctl workspacerules`) say which workspaces belong on
   which monitor before they exist; the owner's bar shows them all.
   The test config hardcodes the owner's monitors and ids.
   **Wave 2: fixed**: `workspaces.configured` and
   `workspaces.slots(screen)`
   (`crates/strand-services/src/wm/hyprland.rs::tests::numbered_rules_are_configured_workspaces`,
   `crates/strand-services/src/wm/service.rs::tests::slots_follow_the_compositors_numbers`);
   decisions.md m6-w2-services.
4. **No hover popup.** A popup is click-and-grab only (bug 1); a
   hover-opened island that stays open while the pointer is on its
   item or on itself, without a grab, has no element. A grace delay
   had to be written by hand (`on change over after 200ms`).
   **Owner's ruling (2026-10-13):** `popup { trigger: hover }`. Strand
   owns the open state: the popup opens while the pointer is on the
   anchor, stays open while it is on the anchor or the popup (a 200 ms
   grace across the gap), never grabs, and is one at a time per
   surface (moving to the next item closes the current one at once).
   It replaces an opt-in `hover: keep` prop, which still needed the
   wiring written by hand and let two islands overlap. It is also the
   base for gap 5's travelling island.
   **Wave 2: fixed**: `trigger: hover`, `delay`, `grace` and `p.open`
   (`crates/strand-render/tests/input.rs::a_hover_popup_opens_on_its_anchor_and_closes_after_the_grace`,
   `::going_straight_from_a_to_b_swaps_the_popups_with_no_overlap`,
   `crates/strand/tests/hover_popups.rs::hover_trigger_islands_need_no_wiring`);
   decisions.md m6-w2-surface.
5. **One island cannot travel between items.** Quickshell's popout is
   one surface that slides and resizes from item to item. A Strand
   popup anchors only to its parent element, and no expression can
   read where an element sits, so each item gets its own popup and
   they swap rather than slide.
   **Owner's ruling (2026-10-13):** built in wave 2 on top of
   `trigger: hover`. Hover popups with the same `group:` on one surface
   are one island: moving between their items springs the island's
   position and size to the next anchor and swaps its content; `enter`
   and `exit` play only when the group opens from nothing or closes;
   the travel direction is readable for effects. Reading an element's
   laid-out box by id stays proposed as the general capability.
   **Owner's ruling (2026-10-14):** the general capability instead of
   `group:` (the proposal's option (a)): `id.box`.
   **Wave 2: fixed**: `id.box` is an element's laid-out box (`Rect`,
   logical, from its surface's top-left), the first one snapping and
   later ones springing; a binding on it may set only what cannot lay
   that box out again (`check::box_layout_loop`); `when c { id: cur }`
   names whichever element's condition holds, so the workspace
   indicator is `x: cur.box.x - ws.box.x`. The travelling island is
   one `trigger: hover` popup sized from the pill's box, its island an
   absolute element following the hovered item's box, letting input
   through outside the island and keeping its popup still while it
   travels
   (`crates/strand-render/tests/boxes.rs::the_island_travels_between_items`,
   `::input_passes_through_outside_the_island`,
   `::a_shadowed_island_keeps_its_popup_still`,
   `::the_workspace_indicator_follows_the_current_box`,
   `::boxes_are_logical_on_mixed_scale_outputs`,
   `crates/strand-compiler/tests/checker.rs::the_specs_travelling_island_checks`,
   `::a_box_loop_through_several_bindings_is_refused`,
   `crates/strand-compiler/tests/instantiate.rs::box_reads_the_reported_box_from_the_popups_tree`,
   `::a_conditional_id_names_the_copy_whose_condition_holds`,
   `crates/strand-surface/tests/sway.rs::a_popups_painted_input_region_lets_the_rest_pass_through`);
   decisions.md m6-w2-surface.
6. **An `id:` inside a component is invisible outside it.** A
   component cannot expose its `hover` (or any state) to its caller,
   so a reusable island component could not report that the pointer
   is on it; each popup had to be written inline with its own `id`.
   Nor can a component change its caller's state: a tray menu's rows
   could not switch the menu to a submenu from a row component, so the
   rows were inlined into the tray icon's component.
   **Wave 2: fixed for the owner's case; the rest declined for now**
   (the owner's ruling, 2026-10-14, decisions.md m6-owner). `id:` and
   handlers on a call are its root element's, so a component's root
   hover (and a root popup's `open`) reach the caller and a caller's
   handler on a row switches its menu
   (`crates/strand-compiler/tests/instantiate.rs::a_calls_handlers_and_id_are_its_root_elements`,
   `::a_calls_id_and_screen_reach_its_hover_popup`). State that two
   places need lives in the caller. Exposing a non-root element's
   state and a component writing its caller's state (`name: <-> T`
   parameters, decisions.md m6-w2-language, "the rest of owner testing
   Gaps 6") are not taken; they are revisited only if real testing
   hits a component that must write its caller's state from inside
   itself, and then land in a later wave with that case as the test.
7. **`screen` is not in scope in components a bar uses.** It has to
   be passed as a parameter (`Bar { scr: screen }`).
   **Wave 2: fixed**
   (`crates/strand-compiler/tests/instantiate.rs::a_component_reads_its_bars_screen`,
   `crates/strand-compiler/tests/checker.rs::a_components_screen_comes_from_its_bar`);
   decisions.md m6-w2-language.
8. **Handlers cannot go on a component call.** `Sym "x" { on click
   { … } }` is a load error; the call has to be wrapped in a `box`.
   **Wave 2: fixed**
   (`crates/strand-compiler/tests/checker.rs::handlers_on_a_call_are_its_root_elements`,
   `crates/strand-compiler/tests/instantiate.rs::a_calls_handlers_and_id_are_its_root_elements`);
   decisions.md m6-w2-language.
9. **A window has no link to its app.** Mapping windows to desktop
   entry categories (Quickshell's glyph per category) needs a
   heuristic match over `apps.all` in the config (bug 3).
   **Wave 2: fixed**: `win.app?.categories`
   (`crates/strand-services/tests/wm_services.rs::windows_are_linked_to_their_apps`);
   decisions.md m6-w2-services.
10. **Palette extras are template-only.** `$palette.overlay1`,
    `$palette.mantle` and the rest of Catppuccin's names are unknown
    in props; a config ported from a Catppuccin shell has to translate
    each to the role it maps to.
    **Wave 2: fixed**
    (`crates/strand-compiler/tests/theme.rs::palette_extras_are_read_in_props`);
    decisions.md m6-w2-language.
11. **List and text helpers are thin.** No `distinct`/`unique` on
    lists (deduplicating glyphs took a `find` per item), no `+` on
    text (it needs `join`), no `sum` (the indicator's offset had to be
    built from `count`s).
    **Wave 2: fixed**: `+` on texts, `distinct`, `sum`
    (`crates/strand-compiler/tests/vm.rs::text_plus_distinct_and_sum`);
    decisions.md m6-w2-language.
12. **Integer list literals are `[float]`.** `[1, 2, 3]` checks as
    `[float]`, so a `fn` returning workspace ids must say `[float]`;
    comparing them with `int` ids works at runtime.
    **Wave 2: fixed**
    (`crates/strand-compiler/tests/checker.rs::whole_number_values_widen_where_fractions_are_written`,
    `::a_looped_whole_number_list_focuses_workspaces`); decisions.md
    m6-w2-language.

13. **Media cannot be seeked.** `media.position` is read-only and
    `media` has no seek action, so a scrubber can only show progress;
    the owner's Quickshell seek bar is dragged to seek
    (`Players.setPosition`).
    **Wave 2: fixed**: `media.position` is writable and writing it
    seeks (`crates/strand-services/tests/media.rs::writing_the_position_seeks_the_player`);
    decisions.md m6-w2-services.
14. **No clock-style duration.** `dur` gives `45 s`, `12 min`,
    `2 h 5 min`, right for a battery's time left and wrong for a
    track's `1:23 / 3:45`; there is no `m:ss` form or way to format a
    duration's parts, so the media card shows no times.
    **Wave 2: fixed**: `dur(d, clock: true)`
    (`crates/strand-compiler/tests/vm.rs::dur_has_a_clock_form`);
    decisions.md m6-w2-language.

15. **The network service reads NetworkManager only.** On iwd (the
    owner's laptop: `iwd` active, NetworkManager not installed),
    `network.connected` is false and `ssid` null while Wi-Fi is up.
    iwd has its own D-Bus API (`net.connman.iwd`, `Station.State`,
    `ConnectedNetwork`); systemd-networkd and ConnMan are not read
    either. The test config polls `iwctl station wlan0 show` every 3 s
    through a `from poll` service with `permit exec "sh"`, as the
    owner's Quickshell does. (Bluetooth, through BlueZ, lists the
    owner's paired devices correctly.)
    **Wave 2: fixed** for iwd
    (`crates/strand-services/tests/network_iwd.rs::network_follows_iwd_where_networkmanager_is_absent`);
    decisions.md m6-w2-services. systemd-networkd and ConnMan are
    still not read.

16. **Dates do not order.** `d.date < clock.today` is a type error
    (`<` needs numbers); trimming a calendar's trailing week took
    `d.date.add(months: 1).month_start() == clock.today.month_start()`.
    **Wave 2: fixed** (`crates/strand-compiler/tests/vm.rs::dates_order`);
    decisions.md m6-w2-language.
17. **No session or power service.** Suspend, hibernate, reboot,
    power off, lock and log out are logind calls; a first-party
    `session` service with `can_*` fields is in wave 2's services stream.
    **Wave 2: fixed**: the builtin `session` service over logind
    (`crates/strand-services/tests/session.rs::session_reads_logind_and_acts_through_it`,
    `crates/strand-compiler/tests/checker.rs::session_is_a_power_menu`);
    decisions.md m6-w2-services.
18. **No monitor-configuration service.** A settings UI for modes,
    scale, position and enabling monitors needs a service over
    wlr-output-management (Hyprland, sway, niri and labwc implement
    it), writable with apply and test. Queued for the next wave.
    **Wave 2: open**: the monitor-config service is the next wave's.
19. **Component names clobber across files.** Every component, type,
    enum and `fn` is global, so two files cannot each have their own
    `Row`. **Owner's ruling (2026-10-14):** names stay global, but a
    file's own declaration wins inside it; a name declared in several
    other files must be qualified (`tray.Row`), an error at the use
    site that names the files and the fix. `strand check` and the
    language server note every name declared in more than one file, and
    the spec and the guide document it. Surfaces and services stay
    unique. In wave 2's language stream.
    **Wave 2: fixed**: a file uses its own declaration, else the single
    one elsewhere, else a bare use is `check::ambiguous_name` offering
    `file.Name`; `tray.Row`, `x: tray.Item`, `tray.Kind.dark` and
    `tray.fmt(x)` name one file's; a `check::same_name` note at each
    declaration; state survives a name becoming shared
    (`crates/strand-compiler/tests/checker.rs::a_file_uses_its_own_declaration_of_a_shared_name`,
    `::a_bare_use_of_a_name_two_other_files_declare_is_ambiguous`,
    `::qualified_names_reach_one_files_declaration`,
    `::a_shared_name_is_a_note_at_each_declaration`,
    `::surfaces_and_services_stay_unique`,
    `crates/strand-compiler/tests/reload.rs::same_named_components_in_two_files_stay_apart`,
    `::a_name_becoming_shared_keeps_the_unedited_files_state`,
    `crates/strand-dev/tests/lsp.rs::same_named_declarations_in_two_files`,
    `::an_ambiguous_name_offers_each_file`,
    `crates/strand-dev/tests/guide.rs::the_guide_quotes_the_same_name_error`,
    `crates/strand-compiler/tests/instantiate.rs::a_qualified_call_takes_its_hover_popups_id`;
    the full list is features.md's); spec.md 4.2, the guide's level 2,
    decisions.md m6-w2-language.

## Docs and traps

1. **`after T while c` fires once per mount.** (config) As spec.md 8.4
   says, but it reads like a re-arming grace timer; written as a
   popup's close delay it closed the first popup and never again. The
   idiom is `on change c after T { if !c { … } }`. The spec and the
   guide could show the hover-grace idiom and say `after` is one-shot
   in its first sentence.
   **Wave 2: fixed**: spec.md says one-shot in its first sentence and
   shows the grace idiom
   (`crates/strand-compiler/tests/instantiate.rs::after_while_is_one_shot_and_the_grace_idiom_runs_each_time`);
   decisions.md m6-w2-language. A hover island now needs no grace at
   all (`trigger: hover`, gap 4).
2. **The guide did not say where `strand` comes from.** (fixed
   2026-10-13, decisions.md m6-owner) The owner's first try was
   `strand: command not found`; README.md's "What you need" now says.
