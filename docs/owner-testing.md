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

8. **A relative `svg` or `image` path draws nothing.** (fixed
   2026-10-14, decisions.md m6-owner-fixes) Found with the GitHub
   build of `0cba252` on headless sway: `svg "dot.svg"` and
   `image "dot.svg"`, next to `target/try/svgtest/strand/bar.strand`,
   drew nothing; the same file by absolute path drew, and `strand
   check` reported nothing. Cause: render read a relative source
   against the process's directory (`crates/strand-render/src/image.rs`,
   `resolve` and `read_local`), where every other path in the language
   is under the config directory. Fix: instantiate makes an `image`'s,
   `svg`'s or `lottie`'s relative file absolute under the config
   directory, a subdirectory's (`icons/wifi.svg`) too
   (`check::assets::local_source`), and `strand check` warns
   (`check::missing_file`) on a literal path that names no file, never
   on an icon name (the rule render draws by, now
   `strand_scene::source_is_file`). spec.md 6.1 says so
   (`crates/strand-compiler/tests/render.rs::relative_media_paths_are_under_the_config_directory`,
   `crates/strand-compiler/tests/checker.rs::a_missing_literal_file_is_a_warning`).
   Since the owner's approval the same day, such a file is watched and
   saving it redraws it with no reload
   (`crates/strand/tests/demo.rs::strand_run_redraws_an_edited_svg_without_a_reload`,
   `crates/strand-render/tests/local_assets.rs`); decisions.md
   m6-owner-fixes.
   The island test bar (`target/try/island`, built the same day) avoided
   `svg` entirely because of this bug.

Bugs 9 to 12 and gaps 20 to 27 come from the island test bar
(`target/try/island`, "Liquid island": one small island at the top
edge that swells into a dashboard and into live activities), built by
a workflow on 2026-10-14 against the GitHub build of `0cba252` and
checked only on headless sway. The test bars are functional-parity
probes: these are findings, recorded so a later wave can reproduce
them minimally; they do not by themselves shape Strand, and nothing
here is a proposal.

9. **A drop shadow makes a shrinking size snap.** (fixed, 2026-10-14;
   found by the island test bar (`target/try/island`), headless sway;
   not yet reproduced minimally) A `col` whose `width` and `height`
   spring (`width: w ~ s_w; height: h ~ s_h; x: … ~ s_w`, with `when
   !grown { width: rest_w ~ s_fold; height: rest_h ~ s_fold; x: … ~
   s_fold }`) and that has a constant `shadow: 0 10px 28px …`: when
   `grown` goes false, `width` and `height` snap to their targets while
   `x` still springs. Growing animates fine, and with the `shadow`
   removed the shrink springs. The node is the card in a `trigger:
   hover` popup that draws nothing itself (`MorphBody` in
   `target/try/island/strand/notch.strand`, which now carries no shadow
   and a comment saying why). A/B screenshots from the builder's run:
   `target/try/shots/island/r7slowfold` and
   `target/try/shots/island/r7slowfold-noshadow`. Not known whether it
   needs the popup. Likely direction: look first at how layout takes a
   shadowed node's overhang while its size moves (the path bug 6 changed
   for a moving island in a hover popup).
   **Fixed; pending the owner's check on Hyprland.** Strand's (B-a): the
   shadow reaches past the popup's box once the card is grown, so the
   popup's overhang followed the springing card and changed every frame,
   and the popup was resized every frame (13 times in one grow).
   Hyprland repositions it each time; sway 1.9 (`xdg_wm_base` 2) makes
   it again, and a popup made again snapped the size springs (bug 11's
   cause), as headless sway showed both ways. Now while a size springs
   on a surface, its overhang is the larger of the reach where the
   spring started and where it lands, kept until it settles: one resize
   at most each way, and none mid spring
   (`crates/strand-render/tests/morph.rs::a_shadowed_card_springs_its_size_without_resizing_its_popup`,
   which failed before: 13 resizes on a reposition, the width at 400 on
   every frame when made again). decisions.md m6-owner-fixes. The config
   is right.
10. **A conditional transition does not switch on the change that
    switches it.** (fixed, 2026-10-14; found by the island test bar
    (`target/try/island`), headless sway; not yet reproduced minimally)
    `opacity: grown ? 1 : 0 ~ (grown ? ease(ease_out, 160ms) :
    ease(ease_in, 1000ms))` faded out in about 150 ms when `grown` went
    false: the transition the change picked was not the one applied to
    that change. The documented form, `opacity: 1 ~ ease(ease_out,
    160ms)` with `when !grown { opacity: 0 ~ ease(…) }`, works (what the
    config now uses). The same pattern on `width` and `height` seemed to
    work, which is inconsistent and is the first thing a minimal repro
    should settle. Likely direction: check whether a `~`'s easing is
    read before or after the write that retargets the value.
    **Fixed; pending the owner's check on Hyprland.** Strand's (B-b): a
    `~` expression was evaluated once, when its node mounted, so every
    change used the transition of the state it mounted in (the card's
    box mounts with `grown` true: 160 ms both ways). Width and height
    read the same frozen transition, so they only seemed to work where
    the mount state's transition was the one wanted. spec.md 6.6 now
    says a change takes the transition in effect after it; the `~` is
    evaluated with the value
    (`crates/strand-compiler/tests/instantiate.rs::a_conditional_transition_applies_to_the_change_that_switches_it`,
    which failed before with 160 ms on the fade out); decisions.md
    m6-owner-fixes. The config is right.
11. **A size in a popup's `enter` pose does not play.** (fixed,
    2026-10-14; found by the island test bar (`target/try/island`),
    headless sway; not yet reproduced minimally) A `width` or `height`
    in the `enter` pose of a popup's card does not play on the popup's
    first frame: the card mounts at its final size. The config works
    around it by mounting the card at the island's size and flipping a
    flag 30 ms later (`on change big.open after 30ms { grown = big.open
    }`). The card is the root of the popup's `if big.open` branch, where
    spec.md 6.6 says poses apply. Bug 2 recorded a card's `enter` in the
    `qs` config never playing, its pose belonging on the popup; whether
    this is the same is for the repro to settle. Likely direction: check
    what a popup's first layout and first configure are sized from when
    its content's enter pose changes a size.
    **Fixed; pending the owner's check on Hyprland.** Strand's (B-c): a
    surface just made is laid out once before its first frame
    (`configure_surface`), with no clock yet, and that layout snapped
    every size spring under it to its target: it took the `enter` sizes
    waiting to start and dropped them, and (bug 9) a spring in flight on
    a popup made again. Now a layout with no clock leaves size springs
    alone and the first frame painted starts them
    (`crates/strand-render/tests/morph.rs::a_size_in_an_enter_pose_plays_as_its_popup_opens`,
    which failed before: 400 × 200 on every frame), as spec.md 6.6 says
    (a node starts at its `enter` values and springs to its own).
    decisions.md m6-owner-fixes. The config is right.
12. **`.hover` disagrees with a hover popup that maps under a still
    pointer.** (fixed, 2026-10-14; found by the island test bar
    (`target/try/island`), headless sway; not yet reproduced minimally)
    When a `trigger: hover` popup maps under a pointer that does not
    move, `card.hover` (an element in the popup) and the anchor's
    `.hover` read false until the pointer moves, while the popup itself
    stays open: Strand's popup hover tracking and node `.hover`
    disagree. The config latches `touched` on the first `card.hover` so
    the card does not fold back meanwhile. This popup covers its anchor
    (gap 24's negative margin), so the anchor's `.hover` reading false
    is what spec.md 6.3 says (the pointer on a popup does not hover its
    anchor); `card.hover` is the disagreement. It may be sway 1.9 not
    sending `wl_pointer.enter` to a surface mapped under the pointer.
    Likely direction: check on Hyprland first; if it is sway's, decide
    whether node `.hover` should follow the popup tracker's view.
    **Fixed; pending the owner's check on Hyprland.** Both compositors'
    (B-d): sway 1.9 sends no `wl_pointer.enter` to a popup that maps, or
    whose input region grows, under a still pointer, and Hyprland 0.56.2
    re-picks on map only against the first commit's region. spec.md 6.3
    defines `hover` by where the pointer is, so after each frame a popup
    paints, a pointer still on a surface the popup opens from, where the
    popup now takes input, is taken onto the popup (a leave and an enter
    through the router, `Host::rebase_pointer`), and the compositor's
    events for the old surface go to the popup until it moves the
    pointer itself
    (`crates/strand/tests/hover_popups.rs::a_hover_popup_mapped_under_a_still_pointer_is_hovered`,
    which failed before). decisions.md m6-owner-fixes. The config's
    `touched` latch is no longer needed. Only a pointer on a surface
    the popup opens from is taken (spec.md 6.3): `big` opening over
    `peek`, its sibling, under a still pointer is hovered from the
    pointer's next motion.

Bugs 13 to 16 are the owner's own, on Hyprland with the island test bar
(`target/try/island`, 2026-10-14).

13. **Cut text flickers while the island morphs.** (fixed, 2026-10-14,
    Hyprland at scales 1 and 1.5) "When it expands certain text gets
    flickery … all text that gets cut off with '...' is the stuff that
    flickers as it gets triggered to cutoff": the window title in the
    crown's pill, squeezed while the card's width springs.
    **Fixed; pending the owner's check on Hyprland.** Strand's: a text
    narrower than its unbounded layout draws a layout shaped for its
    box's width, and a springing width asks the text worker for a new
    one every frame, which arrives after that frame. Meanwhile the
    unbounded layout stood in, so every frame whose width had changed
    drew the whole title spilling past its box, and only frames where
    the rounded width held still (the spring's slow tail) drew it cut:
    cut and whole by turns. The inline shaper of the offline tests never
    showed it. Now a layout of another width stands in: the widest
    shaped for no more than the box (last frame's, while it grows), else
    the nearest wider one clipped to the box; the unbounded one only
    where nothing else was ever shaped, also clipped
    (`crates/strand-render/tests/morph.rs::cut_text_stays_cut_while_its_box_springs`,
    on the text worker at scales 1 and 1.5, which failed before with the
    whole title drawn 28 px past its box on the first frame). Text whose
    box keeps its width draws as before. decisions.md m6-owner-fixes.
    The config is right.
14. **The music bars move ungracefully while the island morphs.**
    (fixed, 2026-10-14, Hyprland) "Certain elements … move around
    ungracefully when the island morphs between open / compact": the
    green music level bars (`Bars`, three boxes at the start of the
    crown's status row, `crown.strand:184`).
    **Fixed (Strand's part); pending the owner's check on Hyprland.**
    Growing, the crown mounts the readings after the bars: glyphs in
    Material Symbols, whose names (`battery_5_bar`) are one ligature
    glyph about 16 px wide. Text the worker has not shaped yet was laid
    out by an estimate of its width (0.55 em a character, about 110 px
    for that name), and the shaped size arrived a frame later, so the
    row was laid out twice: the bars jumped left by the estimate, then
    back and on with the readings' glide (on headless sway too: they
    were drawn 39 px left of where they slid from, then back). Now
    laying out waits up to 5 ms for the worker to shape a text that has
    no layout yet, so the frame is laid out with its real size (not
    text in a `list`'s rows, which mount as it scrolls)
    (`crates/strand-render/tests/morph.rs::a_mounted_text_moves_its_siblings_once`,
    which failed before: on the worker the bars took a frame at the
    estimate first). What is left is as specified: the bars sit at the
    end of the split, whose width springs with the card (overshoot
    included), and the readings entering beside them (`enter { width: 0
    }` on their text) push them along as they grow; the gap changes from
    6 to 10 at once (layout lengths snap). At scale 1.5 each 3 px bar is
    4 or 5 device pixels by turns as it moves (edges snap to pixels one
    by one, so fills stay crisp); an even logical width (2 or 4) keeps
    every bar's width. decisions.md m6-owner-fixes.
15. **The island closes or stops responding while a slider is
    dragged.** (fixed, 2026-10-14, Hyprland, the island config
    `target/try/island/`) Dragging the brightness slider (and others)
    on the dashboard folds the card or closes it, or the slider stops
    following. **Fixed where Strand can; pending the owner's check on
    Hyprland.** Not the config: its fold reads `card.hover ||
    notch.hover`, and a held press keeps `hover` on its whole chain, the
    card included (spec.md 6.3), so on headless sway a drag carried
    below the popup keeps the card grown and the slider following, and
    the release outside folds it (target/try/shots/input/drag15). The
    cause is Hyprland's: it keeps the pointer on the pressed surface
    only while some surface has keyboard focus
    (`src/managers/input/InputManager.cpp:412-413`, v0.56.2). Without
    one (an empty workspace) the pointer goes to whatever is under it as
    soon as it leaves the popup's input region, the drawn card: the
    popup gets `wl_pointer.leave`, which ends Strand's drag and drops
    `hover` (it never hears the release), so the card folds after the
    config's 350 ms and the popup closes after its grace, and coming
    back with the button down drags nothing. Strand now gives a hover
    popup its whole box as input region while a press on it is held, so
    the drag survives leaving the card inside the popup's box (50 px
    each side, 28 below, for the island)
    (`crates/strand-render/tests/boxes.rs::a_held_press_takes_the_hover_popups_whole_box`,
    which failed before). Past the popup's box Hyprland still takes the
    pointer away and no client can keep it. decisions.md
    m6-owner-fixes.
16. **Sliders and scrubbers register away from the pointer.** (fixed,
    2026-10-14, Hyprland at scales 1 and 1.5, the island config
    `target/try/island/`) Where the pointer presses a slider and where
    its value lands do not match. **Fixed; pending the owner's check on
    Hyprland.** Strand's, at any scale and on any compositor: a slider
    (and a `segmented`) read the pointer against its laid-out box, while
    it is drawn, and hit, moved by the paint offsets and transforms of
    it and its ancestors. The island's card springs by `x: slack + ear`
    (50 px), so its sliders took the pointer 50 px ahead of it (headless
    sway at 1.5: a press 60% along the brightness slider wrote 100%).
    Now the router moves the pointer into the widget's laid-out box
    first (`Renderer::to_laid`, from the drawn hit shape)
    (`crates/strand-render/tests/widgets.rs::widgets_moved_by_paint_offsets_read_the_pointer_where_drawn`,
    at scales 1 and 1.5, which failed before). The window-geometry
    overhang, the negative margin and bug 6's placement shift are not
    it: pointer coordinates are surface-local and hit testing used them
    right (the press found the slider). decisions.md m6-owner-fixes. The
    config is right.
17. **The island card's open morph misses 60 Hz.** (2026-10-14,
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
      its text waited the estimate wait (bug 14) again for the rest, up
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
    panels cost less meanwhile.
    (2026-10-15) That lever is built (fade layers, decisions.md
    m6-owner-fixes) and measured. It does not help these panels.
    Rastering costs about the same per pixel whatever the panels draw,
    and for flat fills and text a layer's image costs as much as the
    content it replaces. So only fades of shadows, gradients or
    paint-filled text are layered; offline those draw in 5.2 ms instead
    of 6.9 ms
    (`crates/strand-render/tests/morph.rs::a_fading_dashboard_draws_its_panels_from_layers`,
    `crates/strand-render/tests/fade_layers.rs`). The owner's config
    draws as before. On headless sway the grow ran at 50 to 53 fps
    before and after, with 5.8 to 6.8 ms of drawing a frame on average
    and 8 to 10 ms on its 0.5 Mpx frames. The levers left are
    per-pixel: vello_cpu's faster 8-bit pipeline (about three times
    faster here, but incremental frames then stop matching full
    repaints along clip edges) or several drawing threads (design.md
    keeps vello_cpu single-threaded); both are the owner's call. The 9
    to 11 ms mounting frames are layout: three full layout passes (at
    rest, with the sizes in flight, again when the text arrives) of
    about 2 ms each, and 2 to 3 ms on every frame of the grow.
    Most gaps in the log are frames with
    nothing to draw, which are not committed or logged; `strand run`'s
    damage log now prints each paint's `flatten=`, `wait=` and `raster=`
    milliseconds.
    The repaints after a close are the config's: the exits of
    `MorphBody` and `MorphEar` (`opacity: 0`, eased over 140 ms) fade
    the folded card out over the bar's island, nine frames whose pixels
    differ in alpha only. Without `opacity: 0` there the popup goes in
    one frame. decisions.md m6-owner-fixes.
18. **A test of the tray menu handing over to the island is flaky
    under load.** (fixed, 2026-10-15; found in a cold `run.sh ci`, not
    by hand: debug builds now optimise dependencies, see the end of this
    entry.) `crates/strand/tests/hover_popups.rs::a_tray_menu_left_for_the_island_goes_once`
    sometimes sees the tray menu go but no island within its 900 ms
    window: 2 of 53 runs of `85f4058` failed, `main` at `ab83c73`
    passed 36 of 36, too few to say the m6-service-gaps merge caused
    it. Likely direction: whether the hover that opens the island is
    lost while the menu's exit holds the pointer (`Host::rebase_pointer`,
    `Router::held`), or the test's window is short for a loaded run.
    Cause: neither. `main` fails it too, and the damage log shows the
    debug build rasterising each animated frame of the menu and the
    island (a box with a 24 px shadow, scaled by `enter`) in 100 to
    380 ms, so the hand-over overran the 900 ms window on a slower run.
    Dependencies are now optimised in debug builds (opt-level 2, the
    workspace's crates at 0): the same frames take 1 to 11 ms.
    decisions.md m6-service-gaps.

19. **The island's card opens a sliver below the screen's edge on
    Hyprland.** (fixed, 2026-10-15; found 2026-10-15 by the owner on
    Hyprland 0.56.2 with the island config and no other bar.) The bar
    sits flush with the top edge; the card it grows into (a popup lifted
    over the bar with `margin: -rest_h`) shows wallpaper above it. Under
    another program's top bar the island sits lower and the gap was
    there but hidden. Cause: every popup asked the compositor to slide
    it on both axes, and Hyprland slides a popup into its output less
    4 px on every side (`CXDGPositionerRules::getPosition`), so a card
    asking for y 0 was moved to y 4. sway does not pad, which is why the
    headless screenshots were flush. A popup now asks for a slide only
    on an axis where its box does not fit its output (Strand already
    moves it inside, bug 6), or while the output is not known
    (`crates/strand-surface/src/placement.rs::tests::a_popup_that_fits_its_output_asks_for_no_slide`).
    decisions.md popup-slide. Confirmed by the owner on Hyprland
    (2026-10-15): the card meets the top edge.

20. **A seam flickers where the island's card meets its fillet while
    it morphs.** (fixed, 2026-10-15; found 2026-10-15 by the owner on
    Hyprland with the island config.) The card is three pieces side by
    side (`MorphEar`, `MorphBody`, `MorphEar`); while they spring, a
    column a pixel wide of wallpaper showed now and then where the
    body's square top corner meets the right fillet. Cause: the right
    ear's `x` is the body's `x` plus its width, but layout rounded the
    body's springing width, flattening its springing `x`, and the ear's
    `x` each on its own, and the rounded sum missed the rounded whole by
    a pixel on about ten frames of the swell and three of the fold. The
    left seam shares one rounded offset, so it never opened. Each drawn
    edge is now rounded from where it falls exactly (the box's edge from
    layout before snapping, moved by every offset in flight)
    (`crates/strand-render/tests/morph.rs::pieces_that_meet_stay_joined_while_they_spring`,
    which shows the hole on `main`). decisions.md seam-snap. Confirmed
    by the owner on Hyprland (2026-10-15).

21. **Dragging the brightness slider on another monitor shuts the
    island.** (fixed, 2026-10-15; found 2026-10-15 by the owner on
    Hyprland with the island config, the laptop's screen and two more.)
    On the laptop's screen the slider works; on the other monitors the
    card closed as the drag began and would not open again for a while.
    Cause: everything one logic step does carries the press that started
    it (`SceneDiff::press`), `on change` handlers included, in every
    monitor's instance of the bar. The slider's two-way write changed
    `brightness.level`, each other monitor's island (its card closed)
    raised its brightness activity, and those popups, marked with the
    slider's press, took an `xdg_popup` grab: Hyprland then sent the
    pointer only to them, so the card being dragged in lost it and
    folded, and could not be hovered open until the activities ended. A
    press now grabs only for popups on the surface it landed on or on
    another surface on its monitor
    (`crates/strand-surface/tests/sway.rs::a_press_grabs_only_for_popups_on_its_monitor`,
    which fails on `main`). Why the laptop's screen escaped is not
    known; the activities on the other monitors would have grabbed
    there too. decisions.md grab-scope. Confirmed by the owner on
    Hyprland (2026-10-16): the slider works on every monitor.
22. **Hyprland drops the connection: `wp_linux_drm_syncobj_surface_v1`
    error 4, "Missing acquire timeline".** (fixed, 2026-10-16; found
    2026-10-16 by the owner on Hyprland with the launcher test config
    (`target/try/launcher`) on the gpu-first build.) It worked for a
    moment, then on a later launch Strand died with a broken pipe.
    Cause: a surface's handles are lent to the GPU thread when it is
    promoted, and until the thread's `Attached` reply the manager still
    painted and committed shm frames to it. Mesa's Vulkan WSI on
    Hyprland turns explicit sync on for a surface it builds a swapchain
    on, after which every buffer committed to that surface needs an
    acquire point; an shm frame landing in that window was a protocol
    error. Before gpu-first a surface was promoted only once its springs
    had settled, so no frame fell in the window; promoted at the first
    frame of an animation, one often did. A lent surface now takes no
    buffer until it is handed off or taken back
    (`crates/strand-surface/tests/gpu_handoff.rs::a_lent_surface_takes_no_buffer_until_it_is_taken_back`,
    which fails on `main`). Neither CI's sway 1.9 nor its Hyprland (no
    hardware GPU) speaks explicit sync with a Vulkan client, so no test
    reached it. decisions.md syncobj-lend. Check on Hyprland: open the
    launcher and Tab through its modes, several launches.
23. **The island closes as soon as the pointer leaves its closed size.**
    (fixed, 2026-10-16; found 2026-10-16 by the owner on Hyprland with
    the island test config (`target/try/island`) on the syncobj-lend
    build.) Open, the island collapsed the moment the pointer left the
    area of the closed island, wherever its open content reached.
    Cause: the island is a hover popup that draws nothing itself, so it
    takes the pointer only where its content drew in the last frame
    (`Painter::input_region`, decisions.md m6-owner-fixes). The surface
    manager sets that region after each shm frame; a frame drawn on the
    GPU never passes through it, so the region of the last shm frame,
    the closed island, stood while it grew, and the pointer leaving it
    left the popup. gpu-first moved the island to the GPU as it starts
    to open, which exposed it. The host now sets each GPU frame's
    opaque, input and blur regions as pending state for its present
    (`crates/strand-surface/tests/gpu_handoff.rs::a_handed_off_surface_takes_input_where_its_gpu_frame_drew`).
    decisions.md gpu-regions. Check on Hyprland: open the island and
    move the pointer over its open content, away from where the closed
    island sat.
24. **A status glyph mounting in the island throws its neighbours up.**
    (fixed, 2026-10-17; found 2026-10-16 by the owner on Hyprland with
    the island test config.) Turning Focus on put its glyph in the
    crown's status row; the music bars beside it jumped out of the
    island and fell back into place while sliding left, which the owner
    called jarring. Cause: a frame lays a surface out more than once
    when new text arrives while it waits (decisions.md m6-owner-fixes,
    bug 14). The first pass placed the glyph by an estimate
    (`do_not_disturb_on`, seventeen letters, wrapped into ten lines in
    its 14 px), which made the row about 170 px tall, and started the
    bars' glide (FLIP) from where they had been to where that pass put
    them. The second pass, with the shaped glyph, put them back on
    their level, but their glide kept the first pass's offset, so they
    were drawn far above it and fell. A pass that a text's arrival
    causes now glides what it moves from where it was drawn, while the
    change that mounted the text still waits on its shaping, and a box
    never drawn is not glided from where an earlier pass put it
    (`crates/strand-render/tests/morph.rs::a_mounted_text_moves_its_siblings_from_where_they_were_drawn`,
    which fails on `main`). decisions.md flip-open. Check on Hyprland:
    with music playing, turn Focus on and off; the bars slide sideways
    only.
25. **Turning Focus off drops the status row into the island.** (fixed,
    2026-10-17; found 2026-10-17 by the owner on Hyprland with the island
    open and a notification waiting.) As Focus went off, its glyph was
    replaced by the unread count at once, and the music bars, Wi-Fi,
    battery and volume dropped into view though nothing about them had
    changed. Cause: the count comes back as a reading that enters faded
    out (`enter { opacity: 0; x: -6 }`), and a node drawn at `opacity` 0
    (or `scale` 0) was not visited when flattening, so the texts under
    it (the `notifications` ligature and the count) were neither asked
    for nor waited on, unlike visible text since bug 14. Layout still
    placed them, by estimates (the ligature's name wrapped into eight
    lines in its 16 px), and that frame's glide sent every sibling far
    from its place; the shaped size came a frame later, after the glide
    had been aimed. A hidden node now asks for the unbounded layouts of
    the texts under it, so they are waited on and laid out as visible
    text is (`crates/strand-render/tests/morph.rs::text_entering_faded_out_moves_its_siblings_once`,
    which fails on `main`). decisions.md hidden-text. Check on Hyprland:
    open the island with a notification waiting and turn Focus on and
    off; nothing but the glyph and the count changes place.

26. **The island's sliders stuck at 0 and ignored the mouse.** (fixed,
    2026-10-26; found 2026-10-26 by the owner on Hyprland, report
    2026-10-26_00-45-36.) Volume, microphone and brightness showed only
    a knob with no track or fill, and dragging across did nothing.
    Cause: flex's upright sliders (3eb9435b) read the written `width:
    0` beside `height: 14` as a slider taller than wide, so each was
    drawn and dragged as an upright 14 px slider. A slider growing from
    a written 0 now runs along the axis it grows on
    (`crates/strand-compiler/tests/controls.rs::a_slider_growing_from_zero_runs_along_its_growth`,
    which fails on `main`). decisions.md slider-fix. Check on Hyprland:
    open the island's levels; each slider shows its fill and follows a
    drag across.

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
    **Fixed; pending the owner's check on Hyprland** (m6-owner-fixes,
    decisions.md): the builtin `outputs` service over
    `zwlr_output_manager_v1`. `outputs.all` lists every monitor, on or
    off (`name`, `description`, `make`, `model`, `serial`, `enabled`,
    `mode`, `preferred`, `modes`, `x`, `y`, `scale`, `transform`,
    `adaptive_sync`, physical size); `outputs.apply(changes)` and
    `outputs.test(changes)` take `[OutputChange]` (a null field keeps
    the monitor's value, monitors not named keep theirs) and answer
    `outputs.succeeded(action)` or `outputs.failed(action, error)`;
    `outputs.available` says whether the compositor offers it
    (`crates/strand-services/tests/outputs.rs::outputs_lists_configures_and_tests_sways_heads`,
    `::refused_configurations_arrive_as_failed`,
    `::without_a_compositor_outputs_is_unavailable_and_fails_as_events`,
    `crates/strand-compiler/tests/checker.rs::outputs_is_a_display_settings_page`).
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

Gaps 20 to 27 come from the island test bar (see the note before bug 9):
findings only.

20. **No way to tell whether a builtin service is running.** (fixed,
    2026-10-14; found by the island test bar (`target/try/island`),
    headless sway; not yet reproduced minimally) `network`, `bluetooth`
    and `session` have no field saying their daemon answers
    (`brightness.available` is the only one). A write or an action on a
    service that is not there (`network.wifi = true` without
    NetworkManager or iwd) is a runtime fault, freezing the enclosing
    component in its red frame until a reload; spec.md 12 says service
    failures reach the program as values, so part of this may be a bug.
    `session.lock()` with no logind faults and does not fire
    `session.failed`, which the reference says covers "no login
    session". The config latches a sign of life per service (`net_seen`,
    `bt_seen`, `fn logind()` from the `can_*` fields). Likely direction:
    a `running`/`available` field per service, or actions and writes
    failing as values.
    **Fixed; pending the owner's check on Hyprland** (m6-owner-fixes,
    decisions.md): both. The fault was a bug against spec.md 12: an
    action a service cannot take is now its `failed` event
    (`session.failed("lock", …)` included) and a write is dropped,
    neither a fault; `network.available`, `bluetooth.available` and
    `session.available` say whether the daemon answers
    (`crates/strand-services/tests/session.rs::without_logind_session_is_unavailable_and_its_actions_fail_as_events`,
    strand's
    `services::tests::a_service_that_cannot_run_fails_as_values_not_faults`).
21. **A `trigger: hover` popup cannot be opened from code.** (fixed,
    2026-10-14; found by the island test bar (`target/try/island`),
    headless sway; not yet reproduced minimally) It takes no `open:`
    (spec.md 6.3, as designed), so a live-activity peek that covers the
    dashboard's anchor cannot hand over to the dashboard: the hover
    popup opens only when the pointer re-enters the anchor. The config
    drops the peek after 800 ms on its middle and relies on the pointer.
    Likely direction: a way to open (not hold) a hover popup from a
    handler.
    **Fixed; pending the owner's check on Hyprland** (G-b): `open:` on a
    `trigger: hover` popup opens it from code and holds it while true
    (at once, without its `delay`); once false the pointer holds it as
    any hover popup. One way only (`open: <-> x` stays a load error).
    The handover is `open: handing`, cleared `on show`, with bug 12's
    fix taking the pointer onto the popup
    (`crates/strand-render/tests/input.rs::open_on_a_hover_popup_opens_and_holds_it`,
    `crates/strand-compiler/tests/instantiate.rs::open_on_a_hover_popup_is_its_hold`,
    `crates/strand/tests/hover_popups.rs::open_opens_a_hover_popup_from_code_and_holds_it`);
    spec.md 6.3, decisions.md m6-owner-fixes.
22. **A hover popup's input covers its elements' empty gaps.**
    (fixed, 2026-10-14; found by the island test bar
    (`target/try/island`), headless sway; not yet reproduced minimally)
    A `trigger: hover` popup that draws nothing takes input in each
    top-level element's whole box (spec.md 6.7), gaps that draw nothing
    included; there is no `hit: none` per node and no input that follows
    drawn pixels. A card with decorative side pieces (the island's two
    concave fillets) had to be three top-level siblings, each placed by
    `x`, so the strips beside the fillets let the pointer through.
    Likely direction: a per-node way out of the input region.
    **Fixed; pending the owner's check on Hyprland** (G-c): `hit: none`
    gives a node no hit shape of its own (a point in it off its children
    hits what is below), its children keeping theirs; on a `trigger:
    hover` popup that draws nothing, such a top-level element hands the
    input region to its children's boxes
    (`crates/strand-render/tests/boxes.rs::hit_none_takes_no_input_in_its_own_box`);
    spec.md 6.7, decisions.md m6-owner-fixes.
23. **No index-of on lists.** (fixed, 2026-10-14; found by the island
    test bar (`target/try/island`), headless sway; not yet reproduced
    minimally) Lists have `find`, `filter`, `first` and `last` but
    nothing gives an item's position, so stepping workspaces by scroll
    (`fn ws_step` in `target/try/island/strand/parts.strand`) compares
    `id`s and calls `workspaces.focus(int)`. That is right on Hyprland,
    where the id is the number, and may step wrongly on sway and niri,
    where `id` is not the number `focus(int)` takes. Not tried there.
    Likely direction: an index-of (or index-carrying `find`) so the next
    slot's record can be focused with `ws.focus()`.
    **Fixed; pending the owner's check on Hyprland** (m6-owner-fixes,
    decisions.md): `xs.index_of(test)`, an `int?`, is `find`'s position,
    so the next slot is
    `slots[(slots.index_of(w => w.focused) ?? -1) + 1]?.focus()`
    (`crates/strand-compiler/tests/vm.rs::index_of_finds_the_first_position`).
24. **A popup's gap side: a negative `margin` and a `margin` of 0.**
    (fixed, 2026-10-14; found by the island test bar
    (`target/try/island`), headless sway; not yet reproduced minimally)
    `margin: -h, 0, 0` on a bar's popup (the island's popups covering
    the bar to grow out of the screen's edge) works on sway but is
    undocumented and untested on Hyprland; and a `margin` of 0 on the
    gap side becomes the default 6 px (`popup_config` in
    `crates/strand-surface/src/placement.rs`), so a popup cannot sit
    flush against its anchor without `attach:`. Likely direction:
    document the gap side's meaning and give "no gap" a spelling.
    **Fixed; pending the owner's check on Hyprland** (G-e): spec.md 5.1
    now says a popup's `margin` on the side facing its anchor is the
    gap, 6 px only while unset, `0` touching and a negative one
    overlapping the anchor (the island's covering popups); and
    `SurfaceSpec::margin_set` tells an unset margin from `0`, so
    placement gives the default 6 only while `margin` is unset
    (`crates/strand-surface/tests/sway.rs::a_popups_margin_on_its_anchors_side_is_its_gap`,
    which failed before with the `margin: 0` popup 6 px below the bar).
    decisions.md m6-owner-fixes.
25. **`graph` keeps its history only while mounted.** (open, 2026-10-14;
    found by the island test bar (`target/try/island`), headless sway;
    not yet reproduced minimally) A `graph` in a popup (the dashboard's
    system load) starts empty each time the popup opens: the history is
    the node's own (reference, `graph`), and a popup mounts its children
    only while shown. Likely direction: a history that can outlive the
    node (kept by the service or the surface).
    **Open; decided for `graph`** (m6-owner-fixes, decisions.md, G-e):
    history stays per mount, as built; a history kept by the data source
    is a service-side series, a new concept for a later wave on the
    owner's ruling, not a `graph` feature.
26. **`attach:` fillets cannot serve a popup over its bar.** (open,
    2026-10-14; found by the island test bar (`target/try/island`),
    headless sway; not yet reproduced minimally) A popup that overlaps
    its bar to grow out of the screen's edge (gap 24's negative margin)
    cannot use `attach: top`: `attach:` makes the gap 0 whatever the
    margin, so the popup opens below the bar. The island draws its
    concave fillets itself (`Fillet` in `notch.strand`). Likely
    direction: let `attach:` name the screen edge for a popup that
    covers its anchor.
    **Fixed; pending the owner's check on Hyprland** (m6-owner-fixes):
    `attach:` no longer moves the box; the margin on its side is kept
    like any margin, 0 while unset (spec.md 5.1), so `attach: top;
    margin: -32, 0, 0` takes a popup of the 32 px bar, or a panel under
    it, over the bar, its fillets meeting the screen's edge; the
    island's card could use `attach: top` in place of its `MorphEar`s,
    its body's top radius set to the ears' (a target's corners on the
    attached side give the fillets their radius). A panel with `attach:` and a set margin on
    that side now keeps it, where it was flush
    (`crates/strand-surface/tests/render.rs::attached_surfaces_with_a_negative_margin_grow_out_of_the_screens_edge`,
    grim on headless sway, which failed before with both at y 36;
    `crates/strand-surface/tests/sway.rs::an_attached_surface_keeps_its_margin_on_its_attached_side`;
    `crates/strand-render/tests/fillet.rs::a_popup_attached_over_its_bar_draws_its_fillets_at_the_edge`).
    decisions.md m6-owner-fixes.
27. **A popup's root has no per-child alignment.** (documented,
    2026-10-14; found by the island test bar (`target/try/island`),
    headless sway; not yet reproduced minimally) A popup's root lays its
    children out centred on both axes, and there is no per-child
    `align_self`, so the island's three side-by-side pieces are each
    placed by `x` from the popup's size. Neither the spec nor the
    reference says how a surface lays out its top-level children. Likely
    direction: document the root's layout, then decide whether
    `align`/`justify` on the popup suffice.
    **Documented** (spec.md 6.7, G-e): a popup, like a `stack`, a `box`
    and every surface, lays its children out over each other, its
    `align` placing all of them on both axes; a child placed apart goes
    in a `row` or `col` that fills the popup and places it, or is
    `place: absolute`; no per-child `align_self` is added
    (`crates/strand-render/tests/layout.rs::a_stacks_align_places_every_child_on_both_axes`).
    decisions.md m6-owner-fixes.
28. **The network service cannot disconnect, rescan on demand, or tell
    the joined network's details.** (open, 2026-10-15; found by surveying
    the owner's quickshell config for capabilities) A Wi-Fi menu cannot
    leave the joined network, cannot show that a network is being joined
    or left (and which), cannot show the joined network's IP address,
    frequency or band, or security, and cannot rescan on demand with a
    flag while the scan runs; the scan made when a list opens is all
    there is. The owner's laptop runs iwd with systemd-networkd, where
    iwd does not know the address.
    **Fixed; pending the owner's check on Hyprland** (m6-service-gaps,
    decisions.md): `network.disconnect()` (NetworkManager's
    `Device.Disconnect`, iwd's `Station.Disconnect`; it stops a join in
    progress too), `network.connecting` and `network.disconnecting`
    (the network's name), `network.frequency` (MHz; the reference gives
    the bands), `network.security` (the new `WifiSecurity` enum),
    `network.address` (from the kernel over rtnetlink, read only while
    shown, so it works under systemd-networkd), `network.scan()` and
    `network.scanning`; failures of the service's own actions are
    `network.failed("", error)`. Over NetworkManager an activating
    network no longer shows as joined, in `access_points` (`active`)
    as in `ssid`. A scan the device cannot make (the hardware switch)
    fails rather than doing nothing, and a disconnect that fails lets
    the join it stopped go on and be reported. Over iwd, a stopped join
    names its own network as `disconnecting`, never an older one
    (review connectivity-r1). Cancelling a join before the daemon has
    started it (an unsaved network still asking, a new password still
    forgetting the old one) stops it with no failure instead of letting
    it go through, and a double click on Disconnect asks the daemon once
    (review connectivity-r2)
    (`crates/strand-services/tests/network.rs::network_tells_the_joined_networks_details`,
    `::network_says_what_it_is_joining_and_leaving`,
    `::disconnect_leaves_the_joined_network`,
    `::a_join_answered_during_a_disconnect_is_deactivated_once_left`,
    `::a_join_a_refused_disconnect_stopped_is_still_followed`,
    `::a_disconnect_with_nothing_active_stops_the_join_itself`,
    `::a_disconnect_while_one_is_in_flight_is_dropped`,
    `::an_explicit_scan_raises_scanning_until_it_ends`;
    `crates/strand-services/tests/network_iwd.rs::iwd_tells_the_joined_networks_details`,
    `::iwd_says_what_it_is_joining_and_leaving_and_disconnects`,
    `::a_join_a_refused_disconnect_stopped_is_still_reported`,
    `::a_disconnect_before_iwd_joins_stops_the_join_itself`,
    `::a_disconnect_while_one_is_in_flight_is_dropped_on_iwd`,
    `::an_explicit_scan_on_iwd_raises_scanning`;
    `crates/strand-services/src/network.rs::tests::only_an_activated_connection_is_joined`,
    `::a_scan_refusal_is_no_failure`;
    `crates/strand-services/src/network/iwd.rs::tests::the_station_says_what_is_in_progress`;
    `crates/strand-services/src/network/address.rs::tests::the_kernel_gives_lo_its_address`;
    `crates/strand-compiler/tests/checker.rs::network_is_a_wifi_page`).
29. **Bluetooth cannot find or pair new devices, forget one, or show a
    device connecting.** (open, 2026-10-15; found by surveying the
    owner's quickshell config for capabilities) `bluetooth` lists only
    paired devices: a menu cannot search for devices in range, pair one
    (pair, trust, connect), forget a paired one, or show that a device
    is connecting or disconnecting.
    **Fixed; pending the owner's check on Hyprland** (m6-service-gaps,
    decisions.md): `bluetooth.nearby` (unpaired devices in range with a
    name, found by a discovery that runs only while the list is shown),
    `bluetooth.scanning`, `d.pair()` (pair, trust, connect),
    `d.forget()`, and each device's `connecting` and `disconnecting`;
    failures stay `bluetooth.failed(address, error)`. A double click
    on Pair or Connect asks BlueZ once (no false failure), a device
    shows the call asked last, and a pairing that times out is
    cancelled (review connectivity-r1). No pairing agent
    is registered: the system's agent answers a PIN if one runs (the
    owner may overrule this, decisions.md)
    (`crates/strand-services/tests/bluetooth.rs::bluetooth_finds_nearby_devices_only_while_read`,
    `::pairing_a_nearby_device_pairs_trusts_and_connects_it`,
    `::a_repeated_pair_or_connect_is_asked_once`,
    `::disconnecting_and_forgetting_a_paired_device`,
    `::without_bluez_pair_and_forget_fail_as_events`;
    `crates/strand-services/src/bluetooth.rs::tests::a_device_shows_its_last_call_and_repeats_are_dropped`,
    `::a_pairing_that_times_out_is_cancelled`;
    `crates/strand-compiler/tests/checker.rs::bluetooth_is_a_pairing_page`).
30. **No clipboard and no clipboard history.** (found 2026-10-15 by
    surveying the owner's quickshell config) A shell could not copy a
    value from a handler (an IP address, a colour), an `input` could not
    paste or copy, and nothing showed what was copied before, which the
    owner's quickshell config does with a clipboard history popup.
    **Fixed; pending the owner's check on Hyprland** (m6-service-gaps,
    decisions.md): a focused `input` copies, cuts and pastes with
    Ctrl+C/X/V (and Ctrl+Insert, Shift+Delete, Shift+Insert) through the
    compositor's clipboard; a builtin `clipboard` service (over
    `ext-data-control-v1`, else `wlr-data-control`) gives the selection's
    `text` and `holds_image`, `clipboard.copy(value)` and `clear()`, and
    `history` (texts and images, `e.copy()`, `e.remove()`), kept across
    restarts and recorded while the config reads it anywhere, a password
    manager's secret never read; after review: the selection found when
    Strand starts is shown but never recorded, so a cleared or removed
    entry stays gone; the lock neither pastes nor copies; Ctrl+C/X/V
    work with a non-Latin layout active (`us,ru`); the selection-is-an-
    image flag is `clipboard.holds_image`; after review r2: a clear
    also deletes a temporary copy of the history a save cut short left,
    and image files the history no longer names are deleted at the next
    load or save, so they never sit outside its caps
    (`crates/strand-services/src/clipboard/history.rs::tests::a_temporary_file_a_cut_short_save_left_goes_at_clear_and_at_load`,
    `::image_files_no_entry_names_are_deleted`,
    `crates/strand/tests/clipboard.rs::an_input_copies_and_pastes_through_the_compositor`,
    `crates/strand-services/tests/clipboard.rs::a_cleared_or_removed_selection_does_not_come_back_at_a_restart`,
    `crates/strand-surface/tests/session_lock.rs::the_lock_neither_pastes_nor_copies` (lock VM),
    `crates/strand-surface/tests/clipboard.rs::ctrl_v_pastes_with_a_non_latin_layout_active`,
    `crates/strand-services/tests/clipboard.rs`,
    `crates/strand/src/services/mod.rs::tests::a_closed_popup_reading_clipboard_history_holds_it_from_the_top_level`,
    `crates/strand/tests/compositor_matrix.rs::the_clipboard_works_on_every_compositor`).
31. **No Caps Lock warning on the lock screen, and no layout indicator.**
    (fixed, 2026-10-15; found 2026-10-15 by surveying the owner's
    quickshell config) The owner's lock screen warns while Caps Lock is
    on and the bar shows the keyboard layout; Strand had no way to know
    either. **Fixed; pending the owner's check on Hyprland**: a builtin
    `keyboard` with `caps_lock`, `num_lock` and `layout` (`English
    (US)`, null when unknown). While one of Strand's surfaces has the
    keyboard (the lock screen, a launcher) they are that keyboard's own,
    exact as keys are pressed; otherwise the lock keys come from the
    kernel's keyboard lights, read every 250 ms only while a visible
    reader shows them, and the layout from Hyprland, sway or niri.
    Owner question, answered with the recommended reading (overrule it
    if you disagree): polling the lights is acceptable, as there is no
    event for them, and a focused surface needs no polling
    (`crates/strand/tests/lock.rs::the_lock_warns_while_caps_lock_is_on`,
    lock VM only,
    `crates/strand/tests/demo.rs::a_focused_surface_shows_caps_lock`,
    `crates/strand-surface/tests/sway.rs::a_focused_surface_reports_its_lock_keys_and_layout`,
    `crates/strand-surface/tests/fake.rs::an_enter_reports_the_lock_keys_of_now_not_of_the_last_focus`,
    `::a_grabbing_popup_loses_the_keyboard_after_an_earlier_release`,
    `crates/strand-services/tests/keyboard.rs::the_lock_keys_follow_the_keyboard_lights_while_read`,
    `::a_focused_surfaces_keyboard_state_wins`,
    `::the_layout_comes_from_the_compositor`,
    `crates/strand-services/tests/sway.rs::sways_layout_is_the_active_keyboards`,
    `::sways_keyboard_layout_follows_its_input_events`,
    `crates/strand-services/tests/hyprland.rs::hyprland_adapter_follows_replayed_traffic`,
    `crates/strand-services/tests/niri.rs::niri_adapter_follows_replayed_traffic`,
    `crates/strand-compiler/tests/checker.rs::keyboard_is_a_lock_screens_caps_lock_warning`).
    decisions.md m6-service-gaps.
32. **A player's buttons cannot tell what the player can do.** (fixed,
    2026-10-15; found 2026-10-15 by surveying the owner's quickshell
    config) The owner's media controls dim "previous", "next" and the
    scrubber when the player cannot take them; `media` had no way to
    say so. **Fixed; pending the owner's check on Hyprland**:
    `media.can_next`, `media.can_previous` and `media.can_seek` follow
    the active player's MPRIS `CanGoNext`, `CanGoPrevious` and
    `CanSeek` (all false while `CanControl` is false or no player
    runs; `can_seek` also needs a known length, the very test a
    `position` write makes), from the `PropertiesChanged` the service
    already follows, with no polling
    (`crates/strand-services/tests/media.rs::media_says_what_the_active_player_can_do`,
    `crates/strand-services/src/media.rs::tests::the_state_says_what_the_active_player_can_do`,
    `::an_invalidated_capability_is_read_again`,
    `crates/strand-compiler/tests/checker.rs::media_buttons_dim_by_what_the_player_can_do`).
    decisions.md m6-service-gaps.
33. **No disk usage.** (fixed, 2026-10-15; found 2026-10-15 by
    surveying the owner's quickshell config) The owner's dashboard shows
    how full the disks are; Strand had `cpu` and `memory` but nothing
    for storage, short of a polled `df`. **Fixed; pending the owner's
    check on Hyprland**: a builtin `disk` gives the root filesystem's
    `usage`, `used`, `total` and `free` (as `df` counts them) and `all`,
    every mounted filesystem on a local disk keyed by `path`, `/` first
    (no snaps, network or FUSE filesystems, binds, container file
    mounts or automounted filesystems, which sampling would keep from
    expiring), measured with `statvfs` off the shared runtime every 10 s
    and only while a reader is visible
    (`crates/strand-services/tests/builtin.rs::disk_reads_the_root_filesystem_only_while_visible`,
    `crates/strand-services/src/disk.rs::tests::local_disks_are_listed_root_first`,
    `::automounted_filesystems_are_left_untouched`,
    `::mountinfo_is_read_with_its_escapes`, `::usage_is_counted_as_df_does`,
    `crates/strand-compiler/tests/checker.rs::disk_shows_root_and_every_filesystem`).
    GPU statistics stay a config's own command (`service gpu from poll`),
    as planned. decisions.md m6-service-gaps.
34. **No uptime or kernel release.** (fixed, 2026-10-15; found
    2026-10-15 by surveying the owner's quickshell config) The owner's
    dashboard shows how long the machine has been up and the running
    kernel; Strand had neither, short of a polled command.
    **Fixed; pending the owner's check on Hyprland**: `system.kernel`
    is the running kernel's release (`/proc/sys/kernel/osrelease`, read
    once), and `system.uptime` the time since boot to the minute
    (`/proc/uptime`, suspend included), a stream field read only while
    a visible reader shows it and then once a minute, so nothing wakes
    every second and nothing at all while nobody shows it
    (`crates/strand-services/tests/builtin.rs::system_reads_the_kernel_and_counts_uptime_by_the_minute`,
    `crates/strand-services/src/procfs.rs::tests::uptime_is_read_and_floored_to_minutes`,
    `crates/strand-services/src/system.rs::tests::the_uptime_timer_waits_for_the_next_whole_minute`,
    `::uptime_advances_as_each_minute_turns`,
    `crates/strand-compiler/tests/checker.rs::system_shows_uptime_and_kernel`).
    decisions.md m6-service-gaps.
35. **The notification history is lost on every restart.** (fixed,
    2026-10-15; found 2026-10-15 by surveying the owner's quickshell
    config) The owner's notification centre keeps its history across
    restarts; `notifications.all` lived in memory only, so a reload of
    strand or a log-in emptied it. **Fixed; pending the owner's check on
    Hyprland**: while `notifications.remember` is on (the default; one
    write of false erases the history at once and stays off) `all` is
    kept under `$XDG_STATE_HOME/strand/persist` (readable only by you;
    the newest 1,000; a sender's `transient` notifications and this run's
    pictures never written) and comes back after a restart, renumbered
    below every new id, without buttons (their senders cannot hear an
    answer), never as popups and without `received`; closing one tells
    no sender, and `clear()` and `dismiss()` forget them on disk too;
    a change is saved 1 s after the last one and at most 5 s after the
    first unsaved one, so a notification updated without pause is not
    left unsaved; a sender's picture given as a percent-encoded
    `file://` URL comes back with it, and a history file that does not
    read is replaced or erased, never kept aside
    (`crates/strand-services/tests/notifications.rs::remembered_notifications_come_back_after_a_restart`,
    `::forgetting_erases_the_history_and_stays_off`,
    `::clear_forgets_every_remembered_notification`,
    `crates/strand-services/src/notifications.rs::tests::the_history_round_trips_without_actions`,
    `::the_history_keeps_the_newest_and_never_a_transient_one`,
    `::only_a_senders_existing_picture_is_remembered`,
    `::a_percent_encoded_file_url_picture_is_found_at_restore`,
    `::a_history_that_does_not_read_is_refused`,
    `::a_broken_history_is_erased_by_forgetting_not_kept_aside`,
    `::the_transient_hint_is_read`,
    `::a_history_changing_without_pause_is_saved_within_its_bound`,
    `crates/strand-compiler/tests/checker.rs::notification_history_can_be_forgotten`).
    decisions.md m6-service-gaps.

Dropped from the island bar's notes, the spec already answering them: a
component whose tree is not one element cannot take `id:` (spec.md 5.2:
a load error whose fix is to wrap the call in a `box`); and a component
cannot read a bar's `let`s or `state` except through `export` (spec.md
4.2 and 5.3: a top-level value belongs to its file and other files read
it only when exported, a surface's state is per instance and reaches a
component as a parameter; gap 6's ruling keeps state two places need in
the caller).

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
3. **`on change` over a field next to a list query's key never runs on
   a new item.** (proposed, 2026-10-16; found with change-optional-chain
   by the launcher test config (`target/try/launcher`), headless sway)
   `on change clipboard.history.first?.text { … }` does not run on a new
   copy: the newest entry is another record, and spec.md 8.4 takes a
   watched service path switching to another object as the new
   baseline, which is right for `audio.sink.volume` on a sink switch
   but hides every new item behind a list query (`first`, `last`,
   `[i]`). Watching the key itself (`… first?.id`) or a field holding it
   (`hits.first?.app` under `key app.id`) runs, decisions.md
   change-optional-chain; a `let`, binding or `if` over the field
   follows it either way. Waiting for the owner's ruling: should a list
   query's item switch re-baseline the way an object reference's does,
   or count as a change of the field watched? Pinned as built by
   `crates/strand-compiler/tests/instantiate.rs::on_change_over_an_identity_fires_on_a_switch`
   (`texts`).
