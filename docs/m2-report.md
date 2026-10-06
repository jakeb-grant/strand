# M2 exit report

Measured 2026-10-06 on the dev container (Intel Xeon @ 2.10 GHz, 4 vCPUs
shared with another agent's builds), Ubuntu 24.04, rustc 1.97.0, headless
sway 1.9 with the pixman renderer, fonts-dejavu-core 2.37,
adwaita-icon-theme 46; release builds (thin LTO, one codegen unit,
mimalloc). Every number comes from a test or script in the tree that
fails when its gate is missed; each is given with the command that
reproduces it. CI runs the same tests on Ubuntu 24.04 with the same
packages (`.github/workflows/ci.yml`).

## Result

| Gate (`docs/features.md`, M2 exit) | Budget | Measured | |
| --- | --- | --- | --- |
| Theme swap | under 5 ms of work | **1.9–2.5 ms** median through light↔dark, auto→mocha, mocha→wallpaper, wallpaper→auto on design.md's `theme.strand` (logic's re-resolve 0.2–0.7 ms, render's apply 0.6–1.1 ms, every frame's roots and token graph about 1 ms); a crossfading swap 1.6 / 2.1 ms (buffers of age 1 / 2); 2.1 ms at the design's `spring(1600, 1)`, 3.2 ms with 8 `set { }` scopes | pass |
| Contrast never below 3:1 | every frame that springs | random palettes at 60 and 144 Hz (`theme_swap.rs`); on sway, the design bar's clock in every frame grim caught of a dark → mocha swap: **10.8–13.6:1**; light → dark is the swap design.md crossfades (blended frames exempt): 3.3:1 at its lowest caught frame, 13.6:1 settled | pass |
| The four example shells run unchanged | byte for byte | **bar with calendar, launcher, toasts, OSD and `theme.strand`**, the fixtures copied byte for byte (and the fixtures held to design.md's code blocks), driven on two outputs, 19 settled screenshots within tolerance of their references | pass |

Re-checked M0 gates (design.md: "the build fails above 34 MB for the
two-monitor bar"):

| | Budget | M0 demo (`scripts/m0-exit.sh`) | design.md's bar (`scripts/m2-exit.sh`) |
| --- | --- | --- | --- |
| PSS, two 2560×1440 outputs (1.0, 1.25) | ≤ 34 MB | **11.3 MB** (21.6 MB at M0) | **25.8 MB** after boot, **27.3 MB** after two ticks |
| Context switches between ticks (:03 → :57) | 0 | **0, 0** | **0, 0** |
| Damage per clock tick, both outputs | ≤ 2,000 px² | **239 px²** (largest frame 140) | **239 px²** (88–99 + 140 px²); the first tick 628 (the tray icon's late decode at 1.25, once) |

Full shell with the launcher open (design.md's estimate 59–64 MB, not an
M2 gate; M3 measures it with real services): **31.4 MB** PSS with the
bar on two outputs, the mock's notifications up and the launcher open on
`HEADLESS-1` at scale 1 (28.2 MB before it opened, 27.8 MB after it
closed). design.md's estimate budgets the launcher's buffers at 2×: with
`HEADLESS-1` at scale 2 (the launcher's buffer 1492×754), **37.4 MB**
with it open, 32.1 MB before. Re-run after this round's fixes
(`scripts/m2-exit.sh --no-build`): the design bar 26.2 MB after boot,
27.0 MB after two ticks, 0 and 0 context switches from :03 to :57, ticks
of 228 and 239 px². (A first re-run counted 8 and 1 switches: the script
kept its homes under `/tmp`, where the config watcher's ancestor watches
woke for other processes' directories; the homes now live under `$OUT`,
as `demo.rs` already did.)

M2 is v0.1: every M2 box and exit gate in `docs/features.md` is ticked.

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
| `the_bar_and_its_calendar_on_two_outputs` | the clock's ink centred within 3 px on both outputs; four dots on one output and two on the other, the focused one the `$accent` pill; a click on the third dot moves the pill there; the clock's click opens the calendar centred under it, today in `$accent`; ‹ pages to September; Escape closes it; `theme.look` dark and mocha darken the bar, keep the clock above 3:1 and draw the tray's symbolic icon light |
| `the_launcher_filters_selects_and_closes` | opened by `strand set launcher.open true`, 600 px wide and centred in the usable area of the focused output; three rows, the first selected, a caret in the input; "fi" leaves two rows with their matched letters in `$accent`; Down selects the second; Return closes it; opened again, `on show` cleared the query (three rows, the first selected); Escape closes it |
| `toasts_arrive_stack_slide_and_leave` | no surface until a notification; the first slides in (release: at least two frames of the slide caught, overshoot at most 2 px) and rests 12 px from the edge; a critical toast's `$error` border; the first's close icon dismisses it and the next slides up into its place (release: at least two frames caught); a 1.5 s timeout expires by itself; a click activates one (it leaves); a right click dismisses the last; the panel closes |
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
(`STRAND_LOG=damage`); then the five files with the launcher opened by
`strand set launcher.open true` and the mock's notifications up, then
again with `HEADLESS-1` at scale 2 (the launcher's buffers at 2×, as
design.md's estimate budgets them). The homes live under the script's
`$OUT`, not `/tmp`; the shots go to `$OUT` (`--images` copies them into
`docs/images`).
`scripts/m0-exit.sh --no-build --no-bench` re-ran the M0 demo's gates.
In `cargo test`, `crates/strand/tests/demo.rs::
the_design_bar_keeps_the_m0_budget` holds the bar to 34 MB (release; CI),
to no wakeup from the end of boot work (by :45) to :57, and the minute
tick that follows to 2,000 px² over both outputs.

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
