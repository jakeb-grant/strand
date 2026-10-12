# Target checklist: bar (a)

What the agent proxy's result is scored against (m5-plan, "Newcomer
builds a multi-monitor bar in 15 minutes", layer 2). The target is
design.md's bar (a), the bar levels 1 to 3 of the guide build. A proxy
session gets the guide and nothing else, builds the bar in its own
config directory, and `scripts/agent-proxy/score.sh` runs that config
on a headless sway with two outputs (`HEADLESS-1` 2560x1440 at scale 1,
`HEADLESS-2` 1920x1080 at 1.25) and the deterministic mock services
(`STRAND_MOCK=acceptance`), reads the inspector's dump
(`inspect.dump`), then clicks along `HEADLESS-1`'s bar (the top centre
first, then outwards) until a dump shows the calendar, wherever the
clock is. Every item but the calendar is scored on the dump taken
before any click. The score is never a pixel comparison: it reads what
the shell shows, not how the files are written or laid out, so any file
layout, any component names, any arrangement along the bar and any way
of drawing a dot pass.

The mock's desktop: four workspaces on `HEADLESS-1` (the second
focused), two on `HEADLESS-2`; the focused window "strand —
docs/design.md — Helix"; the clock frozen at Mon 5 Oct 2026 09:41 UTC;
the battery at 87%, 4 h 20 min left; the speaker at 60% with
`audio-volume-medium-symbolic`; one tray item, `network-wireless`.

Each item's id is what the scorer prints (`PASS id` or `FAIL id`). The
newcomer test (`crates/strand/tests/m5/newcomer.rs`) scores the guide's
own bar against this list and requires every item, so the scorer is
checked on a known good result.

- **`bar-per-output`**: one surface along the top edge on each output,
  and no more (a `bar` is on every monitor).
- **`dots`**: a dot per workspace on its own monitor's bar, keyed by the
  workspace (four on `HEADLESS-1`, two on `HEADLESS-2`), drawn any way.
- **`title`**: the focused window's title on both bars.
- **`clock`**: the day and time on both bars ("Mon 05  09:41").
- **`calendar`**: clicking the clock opens a popup with the month's
  title ("October 2026") and a cell for each day.
- **`volume`**: the speaker's icon on both bars (the slider shows on
  hover, which the score does not need).
- **`battery`**: the charge on both bars ("87%").
- **`tray`**: the tray item's icon on both bars.
- **`themed`**: each bar's background reads a theme token (`$surface`
  or another), so the bar follows the theme.

The proxy's run is advisory: it gates nothing, its findings are fixed in
the guide before the human trials, and it is the first thing cut under
time pressure (m5-plan).
