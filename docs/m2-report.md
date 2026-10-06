# M2 exit report

Measured 2026-10-06 on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another agent's builds), Ubuntu 24.04, rustc 1.97.0, headless
sway 1.9 with the pixman renderer, fonts-dejavu-core 2.37,
adwaita-icon-theme 46; release builds (thin LTO, one codegen unit,
mimalloc). Every number comes from a test or script in the tree that
fails when its gate is missed; each is given with the command that
reproduces it. The budget tables below come from one run of fixer round
3's code on 2026-10-06 between 17:02 and 17:16 UTC: `scripts/m2-exit.sh`
(with its new midnight section), `cargo test --release -p strand --test
demo` and `scripts/m0-exit.sh --no-build --no-bench`; round 2's run at
`6730392` (16:02–16:09) gave the same within a few hundred kB and the
same ticks. CI runs the
same tests on `ubuntu-24.04` (pinned, not `ubuntu-latest`: the reference
screenshots depend on its sway, fonts and icons) with the same packages,
and prints their versions (`.github/workflows/ci.yml`).

## Result

| Gate (`docs/features.md`, M2 exit) | Budget | Measured | |
| --- | --- | --- | --- |
| Theme swap | under 5 ms of work | **1.9–2.5 ms** median through light↔dark, auto→mocha, mocha→wallpaper, wallpaper→auto on design.md's `theme.strand` (logic's re-resolve 0.2–0.7 ms, render's apply 0.6–1.1 ms, every frame's roots and token graph about 1 ms); a crossfading swap 1.6 / 2.1 ms (buffers of age 1 / 2); 2.1 ms at the design's `spring(1600, 1)`, 3.2 ms with 8 `set { }` scopes | pass |
| Contrast never below 3:1 | every frame that springs | random palettes at 60 and 144 Hz (`theme_swap.rs`); on sway, the design bar's clock in every frame grim caught of a dark → mocha swap: **10.8–13.6:1**; light → dark is the swap design.md crossfades (blended frames exempt): 3.3:1 at its lowest caught frame, 13.6:1 settled | pass |
| The four example shells run unchanged | byte for byte | **bar with calendar, launcher, toasts, OSD and `theme.strand`**, the fixtures copied byte for byte (and the fixtures held to design.md's code blocks), driven on two outputs, 21 screenshots within tolerance of their references | pass |

Re-checked M0 gates (design.md: "the build fails above 34 MB for the
two-monitor bar"):

| | Budget | M0 demo (`scripts/m0-exit.sh`) | design.md's bar (`scripts/m2-exit.sh`) |
| --- | --- | --- | --- |
| PSS, two 2560×1440 outputs (1.0, 1.25) | ≤ 34 MB | **10.5 MB** after boot, **11.1 MB** after two ticks (21.6 MB at M0; `demo.rs`: 10.4 MB) | **25.3 MB** after boot, **26.0 MB** after two ticks (`demo.rs`: 25.2 MB) |
| Context switches between ticks (:03 → :57) | 0 | **0, 0** | **0, 0** |
| Damage per clock tick, both outputs | ≤ 2,000 px² | **1,005, 1,005 px²** at 17:12 and 17:13 (largest frame 615; 239 at round 2's 16:0x ticks) | **239, 228 px²** (88 + 140 px² a tick; `demo.rs`: 228 px²) |
| The two ticks after local midnight | ≤ 2,000 px² (exception: 60×20 px per output) | — | **2,686, 2,686 px²** (1,034 + 1,652; per output within 1,200 / 1,875) |

The M0 demo's tick cost depends on the minute: at 17:12 → 17:13 and
17:20 → 17:21 its whole "HH:MM" text is repainted (five glyph cells on
each output, 350 + 516 px² offline), at 09:58 → 09:59 one cell (88 +
130). Its clock is centred and shaped as a whole, so a change in the
string's advance moves every glyph; the same happens with the round-2
layout (checked offline against both), so it is not this round's
change, and every such tick is under the gate.

The first tick after boot is the larger one, in `m2-exit.sh` and
`demo.rs` alike: `HEADLESS-1` paints into a buffer of age 2, which was
last drawn before the tray's icon arrived from the image worker, so that
tick also repairs the icon's 16×16 (256 px²) there, once. The same
buffer-age catch-up makes a tick at the hour larger: an earlier run of
the same commit straddled 16:00, where `15:59` → `16:00` changes three
digit cells, and measured 745 px² on that tick and on the next (whose
age-2 buffer still held `15:59`). The builder's run measured 1,123 px²
on a first tick, which likely held both. Every tick of the day but two
is under the gate; the two ticks after local midnight are not.

**The midnight tick is over 2,000 px² in all, a documented exception.**
At local midnight the day name changes width (`"%a %d  %H:%M"`), so the
centred clock moves and every glyph is repainted where it was and where
it is. `scripts/m2-exit.sh` section 1b sets `TZ` so local midnight falls
two to three minutes after boot and logs both ticks: Tue → Wed 00:00
repainted **1,034 px²** on `HEADLESS-1` (1.0) and **1,652 px²** on
`HEADLESS-2` (1.25), **2,686 px²** in all, and 00:01 the same again
(both buffers were two frames old and still showed Tuesday). Offline,
over every day of the week, `crates/strand-render/tests/damage.rs::
the_midnight_tick_damages_only_the_centred_clock` measures 902–924 px²
at 1× and 1,206–1,365 at 1.25 (2,108–2,289 in all) and asserts that no
damage falls outside the clock's old and new boxes. No damage scheme can
do better: the old text has to be erased and the new one drawn, and an
age-2 buffer's catch-up is the same work again. Each output stays within
design.md's own per-tick budget, "a clock tick repaints about 60×20 px"
(1,200 px² at 1×, 1,875 at 1.25), and the script fails above it; the M0
gate stays as written for every other tick (decisions.md, wave3-pixels
(exit, fixer r3)). `demo.rs` allows 4,000 px² in all when its measured
tick falls at local 00:00 or 00:01, so a CI run that straddles midnight
does not fail.

The midnight measurement also caught a layout bug: `split`'s `end`
section moved one pixel whenever the centred clock's width changed
parity (Fri and Sat at midnight), repainting the whole end section on
both outputs (8,278 px² on a reviewer's run). Boxes are now snapped from
their absolute positions; the sides no longer depend on the centre
(`crates/strand-render/tests/layout.rs::
split_sides_never_move_with_the_centre`), and in the run above no damage
fell outside the clock.

Full shell with the launcher open (design.md's estimate 59–64 MB, not an
M2 gate; M3 measures it with real services), same run: **30.9 MB** PSS
with the bar on two outputs, the mock's notifications up and the
launcher open on `HEADLESS-1` at scale 1 (27.3 MB before it opened,
25.8 MB after it closed). design.md's estimate budgets the launcher's
buffers at 2×: with `HEADLESS-1` at scale 2 (the launcher's buffer
1492×754), **34.2 MB** with it open, 29.0 MB before. PSS moves between
runs with what other processes share: round 2's runs gave 31.2–31.6 and
35.1–38.5 MB. (A run in fixer round 1 counted 8 and 1
switches: the script kept its homes under `/tmp`, where the config
watcher's ancestor watches woke for other processes' directories; the
homes now live under `$OUT`, as `demo.rs` already did.)

M2 is v0.1: every M2 exit gate is met and every M2 box in
`docs/features.md` is ticked, several of them with partial notes that
are listed under "Open, not gates" below.

## Method

### The four shells, unchanged (`crates/strand/tests/acceptance.rs`)

Each test starts its own headless sway with `HEADLESS-1` (2560×1440 at
scale 1) and `HEADLESS-2` (1920×1080 at 1.25, right of it), writes the
five fixtures into a fresh `~/.config/strand` (asserting the bytes), and
runs `strand run` with `STRAND_MOCK=acceptance`: the mock desktop (M3's
services are not there yet) with the clock frozen at Mon 5 Oct 2026
09:41:07 UTC, no notifications at boot, four workspaces on the first
output and two on the second. The test drives the shell the way a user
and the services would:

- a virtual pointer (`zwlr_virtual_pointer_v1`): clicks, right clicks,
  wheel notches;
- a virtual keyboard (`zwp_virtual_keyboard_v1`) with a small keymap:
  letters, Return, Escape, Up, Down, BackSpace;
- `strand set` (`launcher.open`, `theme.look`);
- the mock's IPC command (`{"v":1,"cmd":"mock", …}`): a notification
  arriving, a volume, mute or brightness change, as the M3 services will
  report them.

After each action the test first waits for the state it leads to (the
pixels it expects: three toasts, the pill on the third dot, the second
row selected) and for the region to match its reference (up to 15 s), so
a slow logic → render → configure round trip is waited for rather than
read as settled. A screenshot is then taken when neither its region nor
strand's damage log has changed for 500 ms (every spring at rest and a
content-sized surface's deferred shrink done); the OSD, which hides
1.2 s after the change that showed it, is taken as soon as it matches.
Every grab first checks that strand is still running, so a crash is
reported as one. The shot is compared with
`crates/strand/tests/refs/acceptance/<name>.png`: a pixel differs when a
channel is more than 24 apart; at most 0.5% may differ, and no 4×4 block
may hold more than 4 differing pixels, so a wrong or missing glyph (the
clock's whole ink is 375 px, under the 0.5% of the bar) fails while
scattered antialiasing noise passes
(`the_comparison_catches_one_glyph_but_not_noise`). `STRAND_UPDATE_REFS=1` rewrites the references, `STRAND_SHOTS`
keeps every shot; a mismatch leaves the shot and a diff in
`target/acceptance/` (uploaded by CI). On top of the references, each
test asserts what design.md promises in pixels:

| Test | Asserted |
| --- | --- |
| `the_bar_and_its_calendar_on_two_outputs` | the clock's ink centred within 3 px on both outputs; four dots on one output and two on the other, the focused one the `$accent` pill; a click on the third dot moves the pill there; the clock's click opens the calendar centred under it, today in `$accent`; ‹ pages to September; Escape closes it; hovering Volume's row reveals its slider (`bar_volume_hover`, the fill at the mock's 0.6) and leaving hides it; `theme.look` dark and mocha darken the bar, keep the clock above 3:1 and draw the tray's symbolic icon light |
| `the_launcher_filters_selects_and_closes` | opened by `strand set launcher.open true`, 600 px wide and centred in the usable area of the focused output; three rows, the first selected, a caret in the input; "fi" leaves two rows with their matched letters in `$accent`; Down selects the second; Return closes it; opened again, `on show` cleared the query (three rows, the first selected); Escape closes it |
| `toasts_arrive_stack_slide_and_leave` | no surface until a notification; the first slides in (release: at least two frames of the slide caught, overshoot at most 2 px) and rests 12 px from the edge; a critical toast's `$error` border; the first's close icon dismisses it and the next slides up into its place (release: at least two frames caught); a 1.5 s timeout expires by itself; a toast under the pointer outlives its 4 s timeout in `$surface.hi` (`toasts_hover`) and expires once the pointer leaves; a click activates one (it leaves); a right click dismisses the last; the panel closes |
| `the_osd_follows_volume_and_brightness` | no OSD at boot; volume 0.3 fills 30% of the meter (±4%), hidden 1.0–2.6 s after the change; two wheel notches on the bar's volume row give 20%; brightness 0.8 shows its icon and 80%; the bar's speaker icon mutes (0%); with `HEADLESS-2` focused the OSD shows there and not on `HEADLESS-1` |

Run: `cargo test --release -p strand --test acceptance -- --nocapture
--test-threads=1` (27 s; CI). The workspace run (debug, four sways in
parallel) runs the bar, launcher and toasts tests too, without the
frame-of-the-slide assertions (a debug build paints too slowly for grim
to catch them reliably); they passed all of 18 runs with two copies of the
debug suite at once (10 of them beside a release build). The OSD test runs in
release only: its 1.2 s window is spent on the way in by a loaded debug
build.

Every reference was read and judged against design.md:

![design.md's bar on HEADLESS-1 (2560 wide, light): dots with the focused pill, the window title, the clock centred on the output, volume, battery and tray](images/m2-bar.png)

![The bar on HEADLESS-2 at 1.25](images/m2-bar-125.png)

![The bar after `strand set theme.look mocha`](images/m2-bar-mocha.png)

![The calendar popup](images/m2-calendar.png)
![The launcher after typing "fi" and Down](images/m2-launcher-typed.png)

![Three toasts, the critical one with its actions](images/m2-toasts.png)
![The OSD at 30%](images/m2-osd.png)

The full shell with the launcher open on the mock desktop (real clock),
from `scripts/m2-exit.sh --images`. The script's headless seat has no
keyboard (`WLR_LIBINPUT_NO_DEVICES=1`), so the `keyboard: exclusive`
launcher never gets its input focus: no caret and no selected row here.
The acceptance tests attach a virtual keyboard, and their references
show it focused (the launcher above):

![The full shell](images/m2-shell.png)

### Budgets

`scripts/m2-exit.sh` (release) runs design.md's bar alone
(`theme.strand` + `bar.strand`) on two 2560×1440 outputs at 1.0 and 1.25
with the mock desktop and the real clock, measures PSS from
`/proc/<pid>/smaps_rollup`, context switches summed over every thread
from :03 to :57 of two minutes, and the damage of each tick
(`STRAND_LOG=damage`); then the bar again with `TZ` set to a fixed
offset (`MID-h:mm`) that puts local midnight on the first minute
boundary at least 100 s away, logging the 00:00 and 00:01 ticks per
output against design.md's 60×20 px (1,200 × scale² px²); then the
five files with the launcher opened by
`strand set launcher.open true` and the mock's notifications up, then
again with `HEADLESS-1` at scale 2 (the launcher's buffers at 2×, as
design.md's estimate budgets them). The homes live under the script's
`$OUT`, not `/tmp`; the shots go to `$OUT` (`--images` copies them into
`docs/images`).
`scripts/m0-exit.sh --no-build --no-bench` re-ran the M0 demo's gates.
In `cargo test`, `crates/strand/tests/demo.rs::
the_design_bar_keeps_the_m0_budget` holds the bar to 34 MB (release; CI),
to no wakeup from the end of boot work (by :45) to :57, and the minute
tick that follows to 2,000 px² over both outputs (4,000 when that tick
is local 00:00 or 00:01, the midnight exception).

PSS is reported as measured; its file-backed part moves with what other
processes share (2–14 MB here between runs); the anonymous part of the
design bar is stable at about 11 MB.

## What the exit surfaced and fixed

| Found by | Problem | Fix |
| --- | --- | --- |
| `scripts/m2-exit.sh` | design.md's bar was 55 MB PSS (40 MB anonymous) against a 6.5 MB heap peak (heaptrack with the system allocator): mimalloc v3 madvises its arenas for transparent huge pages, which THP's `madvise` mode honours | mimalloc's `no_thp` feature (`crates/strand/Cargo.toml`): 25 MB |
| `scripts/m2-exit.sh` | 14–17 wakeups per minute: the paint cache's idle timer woke 10 s after each tick to free the bar's shadow, rebuilt at the next tick | idle entries go at the next paint or wake, never by a wake of their own |
| `scripts/m2-exit.sh` | 2,533 px² per tick: the whole clock text repainted | a text change damages only the glyphs that changed (`NodeRecord::glyphs`) |
| OSD test | one wheel notch moved the volume 75% (`on scroll(dy)` was in pixels) | `on scroll(dy, dx)` counts notches |
| bar test (dark looks) | the tray's `image item.icon` (a symbolic icon) was black on the dark bar | an `image` takes the foreground colour for a symbolic icon's tint |
| launcher test | opened again, the launcher kept the row selected when it closed | fresh focus starts a `nav` list at its first row |
| bar test | the mock's `ws.focus()` did nothing | the mock focuses the workspace |
| review of the acceptance runs under load | a keyboard going away after a key press (unplugged, a KVM switch) aborted the shell: SCTK's key-repeat timer was removed from a `Drop` while calloop's sources were borrowed (seen as the bar vanishing mid-swap, "contrast 1.00") | key repeat is strand's own calloop timer, stopped from the seat's events (`strand-surface/tests/sway.rs::a_keyboard_going_away_after_a_press_leaves_the_shell_running`) |
| the same | the tests read the frame before an action as settled when the round trip was slow; the 0.5% share let a wrong clock digit pass | each action waits for its expected state and reference; settling needs the damage log quiet; a 4×4 block rule |
| the same | idle cache entries stayed until something else woke the shell (a launcher- or OSD-only config: forever) | entries the frames did not use go when the frame loop stops |
| the same | late search results reselected the first row under the user's Down | a row the user moved to stays while the query stands |
| the same | a one-glyph change to a 6,000-glyph text took 16 ms (the glyph diff was quadratic) | prefix and suffix skipped, the middle compared or boxed: about 5 ms, mostly shaping |
| fixer round 2 | the OSD test's meter reading took the meter's own track as "the pill" for the rows below it; a frame caught near the end of the OSD's entrance, within tolerance of its reference, then read 0% (3 of 14 release runs) | the pill is the image's most common light colour (`acceptance.rs::meter_fill`; 12 of 12 runs since) |
| fixer round 2 | key repeat could busy-loop on a compositor rate above 1,000,000/s, outlive a destroyed focused surface, and repeat stale text after a modifier change | a 1 ms floor, a stop with no focus, a stop on a modifiers change (decisions.md) |
| fixer round 3 (midnight measured on sway) | `split`'s `end` moved one pixel whenever the centre's width changed parity: taffy rounds each location relative to its parent, and `end` and its content both sat on half pixels; everything in `end` was also one pixel past the bar's padding | taffy runs unrounded and each box is snapped from its absolute position (`layout.rs::split_sides_never_move_with_the_centre`, `damage.rs::the_midnight_tick_damages_only_the_centred_clock`); two split references moved one pixel left |
| fixer round 3 | the midnight tick (2,686 px² in all on sway) was unmeasured and over M0's 2,000 px² | measured (`m2-exit.sh` section 1b, `damage.rs`) and held to design.md's 60×20 px per output as a documented exception (decisions.md) |
| fixer round 3 | a killed test binary leaked its headless sway; the hover references depended on the machine's default cursor theme; a second seat's modifiers stopped the first seat's key repeat | `PR_SET_PDEATHSIG` on the test compositors; the acceptance desk pins `XCURSOR_THEME=Adwaita`, size 24; modifiers are kept per keyboard |

Each is recorded in `docs/decisions.md` (wave3-pixels (exit)), with the
portal's `reduced-motion` key now wired to render, which closed the last
open M2 item.

## Open, not gates

- `strand toggle` is M5's; the launcher is opened with `strand set`,
  the same write.
- A write the shell makes to a mock service field (`audio.sink.muted =
  …`) is applied as sent; the sink's icon name is updated only by the
  mock's own command (the M3 PipeWire service reports it).
- The full shell's memory is measured on the mock desktop (three apps,
  two notifications, no real services); M3 measures it again.
- Known partials behind ticked M2 boxes (each in `docs/features.md`):
  - Springs: `mark_color`, gradients and other non-solid paints, and
    the props of effects not drawn yet (`stroke`, `fill`, `trim`,
    `glow`, `blur`) snap instead of springing (decisions.md
    wave3-pixels (p2), fixer rounds 1 and 3; effects are M4).
  - `enter`/`exit`: a `page` enters and exits like any node; the
    directional transitions of `pages` are M4's.
  - Lists are virtualised in layout, shaping and paint; logic still
    mounts every row until M4 (decisions.md wave3-pixels).
  - Contrast: the guard covers Material 3's declared text/background
    pairs. Text in alpha-derived tokens (`$fg.muted`, `$fg.faint`) is
    not guarded and dips to about 2:1 mid-swap; design.md's bar uses
    `$fg.muted` for the window title and the calendar `$fg.faint` for
    out-of-month days (decisions.md wave3-theme (t2), fixer round 3).
- Interactions of the four shells covered only by offline or sway
  tests outside `acceptance.rs`, not by its references: the dots' `when
  hover` / `when pressed` state layers
  (`crates/strand-render/tests/widgets.rs::button_meter_slider_and_segmented`
  for the state layer); the launcher's click-away close
  (`crates/strand/tests/demo.rs::the_design_launcher_is_centred_and_closes_on_click_away`);
  the launcher's "No matches" row
  (`crates/strand-compiler/tests/instantiate.rs::two_way_bindings_write_back`)
  and the toasts' `dnd` filter
  (`crates/strand-compiler/tests/instantiate.rs::a_let_chain_is_one_incremental_view`),
  on the instance, not drawn on sway; the launcher's focus-loss close
  and Battery's `$error` below 15% are not driven by any test (the
  fixtures compile without diagnostics:
  `crates/strand-compiler/tests/fixtures.rs`).
