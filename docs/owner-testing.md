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
   **Verified by the owner on Hyprland, 2026-10-14** (the CI build of
   `0e45b19`): hover popups open on the right monitor.
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

6. **The travelling island runs off the right edge.** (2026-10-14,
   Hyprland, the CI build of `0e45b19`) "Some of the popouts go off
   the right side of the screen. E.g., power, battery, bt": the
   rightmost items of the status group, whose island the config's
   `clamp` holds at the popup's right end.
   **Fixed.** The island's underdamped spring carried it a pixel or
   two past the popup's box at the edge, which grew the popup's shadow overhang and
   so repositioned the popup (`xdg_popup.reposition`); Hyprland 0.56.2
   slides a repositioned popup against its monitor moved by the
   popup's own window-geometry offset (its 37 px overhang), so it
   landed 33 px past the right edge, the clamped island with it
   (`src/protocols/XDGShell.cpp:93-96`). Sway slides correctly and was
   right throughout. Now an overshoot no longer changes the popup, and
   Strand moves a popup inside its output itself, so the compositor
   need not slide it
   (`crates/strand-render/tests/boxes.rs::an_island_overshooting_the_popups_edge_keeps_it_still`,
   `crates/strand-surface/tests/sway.rs::a_popup_at_an_outputs_edge_is_placed_inside_it`,
   `crates/strand/tests/hover_popups.rs::the_travelling_island_stays_on_its_output_at_the_right_edge`
   on 1920- and 2560-wide outputs); decisions.md m6-owner-fixes. The
   config is right.
   **Verified by the owner on Hyprland, 2026-10-14** (the GitHub CI
   build of `0cba252`, with bug 7): "They behave well and I'm not
   seeing flicker."
7. **A tray menu flickers back after it has gone.** (2026-10-14,
   Hyprland) First "when I move quickly the new tray opens and then the
   old one flickers just a bit"; then precisely: "I swap hover from
   slack to calendar, but while the calendar is up and the slack has
   disappeared, the slack popup just flickers in and out once
   briefly". The Slack menu is a tray icon's own `popup { trigger:
   hover; delay: 100ms; enter { opacity: 0; scale: 0.9 } … }`, the
   calendar the status group's travelling island.
   **Fixed.** Strand's: the closing popup's exit is handed to Hyprland as an alpha
   multiplier (the alpha modifier). The frame that ended the exit
   also dropped the content kept for it, and that cleared the
   renderer's cached frame, whose pose the surface manager reads
   right after the paint (`commit.rs`). It then found no pose and sent
   the rest pose (multiplier 1) with that frame's commit, the last
   the popup got before it was destroyed. Hyprland applied it (a
   commit applies the multiplier) and, unmapping the popup, faded a
   snapshot of it out at full opacity with `fadePopupsOut` (the
   owner's `fade`, about 300 ms; `src/desktop/view/Popup.cpp:270-278`,
   `src/render/Renderer.cpp:3171`): the whole menu back, fading out.
   Now the pose sent is the one the painted frame took, kept apart
   from the cache, so the last commit carries the exit's last pose
   (faded out)
   (`crates/strand-render/tests/surface_poses.rs::a_closing_popups_last_frame_keeps_its_exit_pose`,
   which failed before: its last frame reported no pose). Every
   delegated closing pose whose content unmounts on close had it, so
   any hover or click popup, and a panel, closing on Hyprland. Sway
   1.9 has no alpha modifier: the exit is painted there and it never
   showed. On headless sway, the other suspects are ruled out: the
   menu is not opened again, its surface is made once and goes once,
   and it draws nothing after its exit
   (`crates/strand/tests/hover_popups.rs::a_tray_menu_left_for_the_island_goes_once`,
   `::a_hover_hand_off_draws_nothing_new_on_the_old_island`). The
   first reading, the islands' cross-fade under Hyprland's popup
   fades, was not it; that proposal is withdrawn. decisions.md
   m6-owner-fixes. The config is right.
   **Verified by the owner on Hyprland, 2026-10-14** (the GitHub CI
   build of `0cba252`, with bug 6): "They behave well and I'm not
   seeing flicker."

8. **A conditional transition does not switch.** (2026-10-14,
   Hyprland, the notch config `target/try/island`; B-b) `opacity:
   grown ? 1 : 0 ~ (grown ? ease(ease_out, 160ms) : ease(ease_in,
   1000ms))` faded out in about 150 ms when `grown` went false; the
   `when` form worked.
   **Fixed.** Strand's: a `~` expression was evaluated once, when its
   node mounted, so every change used the transition of the state it
   mounted in (the card's box mounts with `grown` true: 160 ms both
   ways). Width and height read the same frozen transition, so they
   only seemed to work where the mount state's transition was the one
   wanted. spec.md 6.6 now says a change takes the transition in
   effect after it; the `~` is evaluated with the value
   (`crates/strand-compiler/tests/instantiate.rs::a_conditional_transition_applies_to_the_change_that_switches_it`,
   which failed before with 160 ms on the fade out); decisions.md
   m6-owner-fixes. The config is right.

9. **Cut text flickers while the island morphs.** (2026-10-14,
   Hyprland at scales 1 and 1.5, the notch config) "When it expands
   certain text gets flickery … all text that gets cut off with '...'
   is the stuff that flickers as it gets triggered to cutoff": the
   window title in the crown's pill, squeezed while the card's width
   springs.
   **Fixed.** Strand's: a text narrower than its unbounded layout
   draws a layout shaped for its box's width, and a springing width
   asks the text worker for a new one every frame, which arrives after
   that frame. Meanwhile the unbounded layout stood in, so every frame
   whose width had changed drew the whole title spilling past its box,
   and only frames where the rounded width held still (the spring's
   slow tail) drew it cut: cut and whole by turns. The inline shaper
   of the offline tests never showed it. Now a layout of another width
   stands in: the widest shaped for no more than the box (last frame's,
   while it grows), else the nearest wider one clipped to the box; the
   unbounded one only where nothing else was ever shaped, also clipped
   (`crates/strand-render/tests/morph.rs::cut_text_stays_cut_while_its_box_springs`,
   on the text worker at scales 1 and 1.5, which failed before with
   the whole title drawn 28 px past its box on the first frame).
   Text whose box keeps its width draws as before. decisions.md
   m6-owner-fixes. The config is right.

10. **The music bars move ungracefully while the island morphs.**
   (2026-10-14, Hyprland, the notch config) "Certain elements … move
   around ungracefully when the island morphs between open / compact":
   the green music level bars (`Bars`, three boxes at the start of the
   crown's status row, `crown.strand:184`).
   **Fixed** (Strand's part). Growing, the crown mounts the readings
   after the bars: glyphs in Material Symbols, whose names
   (`battery_5_bar`) are one ligature glyph about 16 px wide. Text the
   worker has not shaped yet was laid out by an estimate of its width
   (0.55 em a character, about 110 px for that name), and the shaped
   size arrived a frame later, so the row was laid out twice: the bars
   jumped left by the estimate, then back and on with the readings'
   glide (on headless sway too: they were drawn 39 px left of where
   they slid from, then back). Now laying out waits up to 5 ms for the
   worker to shape a text that has no layout yet, so the frame is laid
   out with its real size (not text in a `list`'s rows, which mount
   as it scrolls)
   (`crates/strand-render/tests/morph.rs::a_mounted_text_moves_its_siblings_once`,
   which failed before: on the worker the bars took a frame at the
   estimate first). What is left is as specified: the bars sit at the
   end of the split, whose width springs with the card (overshoot
   included), and the readings entering beside them (`enter { width:
   0 }` on their text) push them along as they grow; the gap changes
   from 6 to 10 at once (layout lengths snap). At scale 1.5 each 3 px
   bar is 4 or 5 device pixels by turns as it moves (edges snap to
   pixels one by one, so fills stay crisp); an even logical width (2 or
   4) keeps every bar's width. decisions.md m6-owner-fixes.

11. **A drop shadow breaks the card's shrink.** (2026-10-14, the notch
   config; B-a) With a constant `shadow: 0 10px 28px …` on the card,
   its width and height jumped to their targets while its `x` still
   sprang; without the shadow they sprang.
   **Fixed.** Strand's: the shadow reaches past the popup's box once
   the card is grown, so the popup's overhang followed the springing
   card and changed every frame, and the popup was resized every frame
   (13 times in one grow). Hyprland repositions it each time; sway 1.9
   (`xdg_wm_base` 2) makes it again, and a popup made again snapped
   the size springs (bug 12's cause), as headless sway showed both
   ways. Now while a size springs on a surface, its overhang is the
   larger of the reach where the spring started and where it lands,
   kept until it settles: one resize at most each way, and none mid
   spring (`crates/strand-render/tests/morph.rs::a_shadowed_card_springs_its_size_without_resizing_its_popup`,
   which failed before: 13 resizes on a reposition, the width at 400
   on every frame when made again). decisions.md m6-owner-fixes. The
   config is right.

12. **A size in a popup's `enter` pose does not play as it opens.**
   (2026-10-14, the notch config; B-c) The card's `enter { width: …;
   height: … }` showed it at its own size on the popup's first frame;
   the pose's `x` and `opacity` played.
   **Fixed.** Strand's: a surface just made is laid out once before its
   first frame (`configure_surface`), with no clock yet, and that
   layout snapped every size spring under it to its target: it took
   the `enter` sizes waiting to start and dropped them, and (bug 11) a
   spring in flight on a popup made again. Now a layout with no clock
   leaves size springs alone and the first frame painted starts them
   (`crates/strand-render/tests/morph.rs::a_size_in_an_enter_pose_plays_as_its_popup_opens`,
   which failed before: 400 × 200 on every frame), as spec.md 6.6 says
   (a node starts at its `enter` values and springs to its own).
   decisions.md m6-owner-fixes. The config is right.

13. **`margin: 0` on a popup's gap side gives 6.** (2026-10-14, the
   notch config; G-e) Placement reads a popup's `margin` there as the
   gap to its anchor and takes 0 as unset, so a popup cannot touch its
   anchor (a negative margin, which overlaps it, works).
   **Open**, spec'd: spec.md 5.1 now says the gap is 6 px only while
   `margin` is unset, `0` touches and a negative one overlaps. The fix
   is in strand-surface (`placement::popup_config`) and needs
   `SurfaceSpec` to tell an unset margin from 0; it is handed to that
   crate's owner. Meanwhile `margin: 0.1` (rounded to 0 px) touches.
   decisions.md m6-owner-fixes.

14. **The island card's open morph misses 60 Hz.** (2026-10-14,
   Hyprland, the notch config, a popup 1470 × 708 at scale 1.5) The
   grow ran at about 40 to 47 fps with 62 to 93 ms gaps, and about ten
   identical 630 × 48 repaints followed a close.
   **Partly fixed.** Strand's: three causes in the card's morph, and one
   found on the way:
   - Each frame of the grow repainted the whole card and everything in
     it (0.9 Mpx a frame on headless sway at the owner's size, 14 to
     17 ms drawing): the card's clip moved, which changed every record
     under it. A box that only moves, resizes or rounds its corners now
     damages along its edges and corners
     (`crates/strand-render/tests/damage.rs::a_swelling_clip_repaints_only_its_edges`).
   - The card's `x` springs with its width, so its content moved by
     fractions of a pixel every frame, and a spring's tail drew it a
     pixel off and back after the card looked settled (two frames of
     300,000 and 210,000 pixels). A paint offset in flight is now drawn
     whole logical pixels from its rest
     (`crates/strand-render/tests/morph.rs::a_swelling_card_repaints_its_edges_and_settles_still`).
   - As the dashboard mounted, each layout pass that received some of
     its text waited the estimate wait (bug 10) again for the rest, up
     to 31 ms in one frame. A frame now waits once
     (`crates/strand-render/tests/morph.rs::a_frame_waits_for_its_new_text_once`).
   - Under load a surface's first frame could show at the size its
     text's estimate gave it, when painting other surfaces ran out its
     hold for the configure. Painting no longer counts against another
     surface's hold
     (`crates/strand-render/src/renderer/tests.rs::painting_another_surface_does_not_run_out_a_hold`).

   Offline (optimised, the card reduced to its morph and rows at the
   owner's size), a swell frame takes a median of 2.1 to 2.7 ms and at
   worst 3.4 to 5.5 ms (2.7 to 3.1 and 4.5 to 5.9 before), repainting
   a third of the pixels; CI's timing job gates it
   (`the_island_card_swells_within_the_frame_budget`: median under
   8.3 ms, every frame under 16.7 ms).
   **Open:** on headless sway the whole config's grow still runs at
   about 44 fps (44 to 47 before). What its frames repaint now is the
   dashboard's content entering: the three panels of `dash.strand`'s
   top row (312 px tall, `enter { opacity: 0; y: -12 }`) fade in over
   about ten frames, 0.5 Mpx a frame and 0.7 to 0.8 with the buffer's
   age, 14 to 17 ms of drawing on the CPU. A fade has to repaint what
   it fades, so the remaining lever is drawing a fading subtree once
   and blending it (a layer), not damage. Smaller or shorter entering
   panels cost less meanwhile. Most gaps in the log are frames with
   nothing to draw, which are not committed or logged; `strand run`'s
   damage log now prints each paint's `flatten=`, `wait=` and `raster=`
   milliseconds.
   The repaints after a close are the config's: the exits of
   `MorphBody` and `MorphEar` (`opacity: 0`, eased over 140 ms) fade
   the folded card out over the bar's island, nine frames whose pixels
   differ in alpha only. Without `opacity: 0` there the popup goes in
   one frame. decisions.md m6-owner-fixes.

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
20. **A `graph`'s history lasts only while it is mounted.** (2026-10-14,
    the notch config; G-e) A graph in a popup starts empty every time
    the popup opens, since a popup mounts what it holds only while
    shown. **Ruling (m6-owner-fixes, decisions.md):** history stays per
    mount, as built; a history kept by the data source is a service
    feature for a later wave, not a `graph` one.

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
3. **A popup's children are not aligned one by one.** (2026-10-14,
   the notch config; G-e) A popup lays its children out as a stack:
   `align` places all of them on both axes, with no per-child form.
   **Documented** (spec.md 6.7): a child placed apart goes in a `row`
   or `col` that fills the popup and places it, or is `place:
   absolute`
   (`crates/strand-render/tests/layout.rs::a_stacks_align_places_every_child_on_both_axes`).
   decisions.md m6-owner-fixes.
