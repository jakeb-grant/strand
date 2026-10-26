# The wasm spike: Strand's renderer in a browser

The owner asked whether the renderer can be compiled to WebAssembly and
run on the docs site, so a reader can edit an example and see it redraw.
This page says what was tried, what works, what it costs, and what it
would take to ship. Branch `laptop/wasm-spike`, 2026-10-26.

## The short answer

It works. The render path (compiler, logic VM, mock services, layout,
text, the CPU renderer) builds for `wasm32-unknown-unknown` with ten
small shims, all behind `cfg(target_family = "wasm")` or a type alias
that is std's natively. In headless Chromium the two docs examples
draw every frame byte for byte as eval draws them: the hello bar's
frame and all 75 ticks of the springs example. A tick takes about
0.6 to 1.5 ms, the module is 2.3 MB over the wire (brotli), and the
page shows its first frame about 0.3 s after it starts loading.

The recommendation is to ship it as the site's live examples, next to
the sprite players the site already has, once the three things in
"To make it production" are done.

## What runs

`crates/strand-web` is a new crate (cdylib for the browser, rlib for
native tests). It has two parts:

- `shell.rs`: a config compiled with the builtin and service schemas,
  booted against `SchemaHost::mock` (the mock eval uses), and run on a
  virtual clock. Each tick runs logic to its time, wakes the loop, paints
  every surface that wants a frame and hands layout facts back to logic.
  All of it goes through `strand_render::host_loop`, the code the shell
  and eval's driver already share, so the frame order is theirs. It is
  eval's `Harness` cut down: no trace, no checks, no GPU or surface leg.
- `surfaces.rs`: the compositor's side. Bars, panels and OSDs are sized
  and placed, and popups positioned, by strand-surface's own
  `placement.rs`, included by path (`#[path]`), because strand-surface
  itself needs Wayland. All surfaces are composited into one RGBA image
  of the screen for a canvas.
- `web.rs` (wasm only): the wasm-bindgen API. `add_font(bytes)`;
  `WebShell.load(files_json, services_json, width, height, scale, hz)`,
  which returns the shell or the compiler's errors rendered as
  `strand check` renders them; `tick()`, `rest(max)`, `at_rest()`;
  `set(path, text)` and `get(path)` (state, a service field or a
  `$token`); `pointer_move(x, y)`, `pointer_button(pressed)`,
  `pointer_leave()`; `frame()`, `frame_top(rows)`, `content_rows()`,
  `painted()`; `notes()`, `warnings()`; `memory_bytes()`.

The page is `site/playground.md` with `site/assets/playground.js` and
`playground.css`. It shows the hello bar and the springs example, each
with its config in a text box, its service values in a second one, Run
(or Ctrl+Enter), and a button per `set` in the scenario (`on: false`).
Pointer moves and clicks on the canvas go to the shell. The examples'
text comes from `eval/scenarios/docs/*.scn` at build time
(`scripts/wasm/examples.py` writes `examples.json`), so the page, eval
and the site's sprite players run the same config. The theme follows
the site's: `system.dark` is set from the page's light or dark mode.

The tests that prove it:

- `crates/strand-web/tests/frames.rs` (native): the hello bar at rest
  and each of the springs example's 75 ticks are compared with the
  frames eval rendered for the site (`site/media/*-light.png` and their
  `.json`): no pixel differs. Also: an error gives its diagnostic at
  `shell.strand:3:`, a click reaches an `on click` handler, a click
  opens a popup below its bar.
- `scripts/wasm/check-browser.sh` (Chromium in eval's Playwright image,
  `--network none`, the site served on the container's loopback): the
  same comparison inside the page, in the wasm build, 1 of 1 and 75 of
  75 frames equal; no console error, exception or failed request; a
  broken config shows `` `battery` has no field `nope` `` with its
  caret, and a fixed one runs again; no sideways scroll at 390 px.

## The shims

Every change to an existing crate, and what it costs natively.

| # | Where | Native code before | On wasm | Native cost |
|---|---|---|---|---|
| 1 | strand-scene `backlog.rs` | rustix `clock_gettime` for the boot, monotonic and raw clocks | one clock, the page's `performance.now()` (web-time) plus an hour; rustix becomes a non-wasm dependency | none: same calls; `ClockId` is rustix's |
| 2 | strand-scene `time.rs` (new), strand-core `time.rs` (new) | | `Instant` is web-time's; `system_now()` reads `Date.now()` | `Instant` is `std::time::Instant` |
| 3 | 35 files: strand-render (26), strand-compiler (3), strand-core (2), strand-theme (2), strand-text (1), strand-scene (1, a test) | `use std::time::Instant` | `use strand_scene::time::Instant` (strand-core: `crate::time`) | none: the same type |
| 4 | strand-core `persist.rs` `touch`, `flight.rs` epoch; strand-compiler `SchemaHost::real` | `SystemTime::now()` | `strand_scene::time::system_now()` | none: calls `SystemTime::now()` |
| 5 | strand-compiler `Cargo.toml` | `similar` | `similar` with `wasm32_web_time`, so its diff deadline takes web-time's `Instant` | none |
| 6 | strand-theme `Cargo.toml`, `scripts/wasm/run.sh` | | getrandom 0.3 with `wasm_js` (material-colors' ahash seeds from it) and `--cfg getrandom_backend="wasm_js"` | none: wasm-only dependency |
| 7 | strand-theme `image.rs` `Stamp` | `MetadataExt` dev, ino, mtime, ctime | the length only (`std::fs` fails first anyway) | none |
| 8 | strand-core `persist.rs` `create_private_dir` | `DirBuilder` with mode 0700 | no mode (the create fails: no file system) | none: same calls |
| 9 | strand-compiler `check/paths.rs` `is_program` | stat and read the file's head | `false` | none |
| 10 | strand-compiler `check/paths.rs` | `use std::io::Read` | not imported | none |

Interface changes, all on wasm only (docs/architecture.md, "Crate
graph"): every signature with an `Instant` in it (`Renderer::set_now`,
`Router::set_now`, `Renderer::next_wake`, `Renderer::frame_deadline`,
`raster::trim_idle` and the rest) takes web-time's `Instant` on wasm.
`SystemTime` stays std's on both: std's type works on wasm, only its
`now()` panics.

Spots that needed nothing, though they looked native-only:

- `hugepage.rs` (mmap, madvise) is already `target_os = "linux"` only.
- `libc::gettid` in `raster/mod.rs` is in `#[cfg(test)]` code.
- `malloc_trim` is already `target_os = "linux", target_env = "gnu"`.
- The raster's helper threads: `available_parallelism()` fails on wasm,
  so `threads()` is 1 and no helper is asked for.
- The renderer's timer thread (`wake.rs`), the image decoder's thread
  and the text worker: `thread::Builder::spawn` returns an error on
  wasm, which each already handles (no timer, no images; the web shell
  shapes text inline, as eval does).
- `stacker` in the compiler builds; on wasm it cannot grow the stack, so
  its guard does nothing (see "Risks").
- chrono's `clock` feature builds without `wasmbind`; the mock pins its
  zone to UTC, so `Local` is never asked for here.

## Size and speed

Sizes of `strand_web_bg.wasm` after wasm-bindgen, in bytes (gzip -9,
brotli -q 11). The JS glue is 16,112 bytes (3,489 brotli). The two
bundled Liberation Sans faces are 825,388 bytes (339,322 brotli),
fetched once beside the module. `scripts/wasm/measure.sh` builds and
measures every row.

| Build | Raw | gzip | brotli |
|---|---:|---:|---:|
| opt-level 3, no wasm-opt | 11,960,773 | 3,934,958 | 2,697,892 |
| opt-level 3, wasm-opt -O3 | 9,593,136 | 3,680,438 | 2,604,575 |
| opt-level 3, wasm-opt -Oz | 9,451,764 | 3,678,850 | 2,607,670 |
| opt-level s, wasm-opt -Oz | 8,552,322 | 3,264,762 | 2,344,618 |
| opt-level z, wasm-opt -Oz | 7,673,752 | 2,887,760 | 2,116,106 |
| opt-level 3, -O3, simd128 | 9,415,612 | 3,597,036 | 2,571,056 |
| **opt-level s, -Oz, simd128** (the default) | 7,867,165 | 3,131,717 | 2,266,459 |

Times in headless Chromium, medians of 5 rounds with the variants
interleaved (`scripts/wasm/bench.py`), in milliseconds. "First frame"
is a cold `WebShell.load` of the springs example through its first tick
and frame copy, in a fresh page. A tick is logic, layout and paint; the
copy is `frame_top`, the composite into straight RGBA. The laptop was
loaded (load average about 20 on 16 cores, other agents building), so
take these as rough; the order of the rows held across runs.

| Build | Compile | First frame | Springs tick | p95 | Hello first tick | Copy |
|---|---:|---:|---:|---:|---:|---:|
| opt 3, none | 78.8 | 220.1 | 1.57 | 4.0 | 2.2 | 3.74 |
| opt 3, -O3 | 67.2 | 283.2 | 1.48 | 3.4 | 2.2 | 3.66 |
| opt 3, -Oz | 68.9 | 266.5 | 1.54 | 3.5 | 2.2 | 3.88 |
| opt s, -Oz | 65.0 | 240.6 | 2.59 | 6.7 | 8.1 | 3.72 |
| opt z, -Oz | 60.3 | 251.2 | 3.90 | 11.0 | 10.8 | 4.02 |
| opt 3, -O3, simd128 | 64.0 | 272.3 | 1.50 | 3.6 | 2.1 | 3.66 |
| **opt s, -Oz, simd128** | 64.2 | 240.3 | 1.31 | 2.8 | 2.4 | 3.71 |

Without the `simd128` target feature, fearless_simd (under vello_cpu)
runs its scalar fallback; with it, opt-level s is as fast as opt-level 3
and 300 KB smaller over the wire. Opt-level z costs 2 to 4 times the
time a tick takes. So the build defaults to opt-level s, wasm-opt -Oz
and simd128 (`scripts/wasm/build.sh`).

On the playground page itself (`check-browser.sh`, the default build,
a quieter moment): the module is fetched, compiled and started in
68 to 104 ms; both examples have drawn their first frame 276 and
284 ms after navigation starts. Loading a config once the page is warm
(the Run button) takes 16 to 45 ms. While the springs move, a tick takes
0.6 to 1.6 ms and drawing a painted frame (the copy, `putImageData`)
about 4 to 5 ms; the copy is a per-pixel loop in `surfaces.rs` and is
the first thing to make faster. The module's linear memory is 9.6 MB
with one shell loaded and 18.6 MB with the page's two.

What the code is (opt-level s build, before wasm-opt; 8.5 MB of code,
0.9 MB of data): strand-compiler 1.06 MB, strand-render 0.72 MB,
strand-core 0.33 MB; the text stack (read-fonts, harfrust, skrifa,
zeno, parley) 1.08 MB; SVG (tiny-skia, usvg, resvg) about 0.45 MB;
image decoders (webp, png, jpeg, gif) about 0.25 MB; serde and
serde_json 0.26 MB; taffy 0.16 MB; velato 0.08 MB; and 2.6 MB of
generic code instantiated in core, alloc and hashbrown.

## What is missing

- **Services**: the mock's values only (`SchemaHost::mock`): no D-Bus,
  no PipeWire, no compositor. The page lets a reader edit the values;
  actions are recorded, not run. The clock is the mock's fixed time.
- **Images and icons**: no file system, and the decoder's thread does
  not start, so `image` and `icon` draw nothing. Fonts are the two
  bundled faces; no system fonts.
- **GPU effects**: the CPU renderer only (`strand-render` without
  `gpu`, the compiler without `shaders`).
- **Threads**: one thread. The raster's helpers, the text worker, the
  image decoder and the renderer's timer thread do not run. The web
  shell wakes the loop every tick instead of on the timer.
- **Surfaces and input**: no windows, lock surfaces or second screen;
  pointer moves and the left button only, no keyboard, wheel or
  touch-pad.
- **Persistence and files**: `Storage::none()`: no `persist`, settings
  or palette files.
- **The renderer's own timers** (a tooltip's delay, a hover popup's
  grace) read the page's real clock, because the virtual-clock setters
  are behind `eval-hooks`, which no root crate may enable
  (`scripts/eval/check-isolation.sh`). Frames, springs and logic run on
  the virtual clock.

## Risks

- **A new `std::time::Instant::now()` on the render path builds for wasm
  and panics at run time** ("time not implemented on this platform").
  Nothing stops one being added. It needs a guard: the wasm build and
  the browser check in CI, or a clippy `disallowed-methods` rule.
- **Size**: 2.3 MB brotli on a visit to the page (cached after). It
  grows with the compiler and renderer; a budget should catch jumps.
- **`placement.rs` included by `#[path]`**: if it ever uses `crate::`
  items, strand-web breaks. Its tests also run twice (once in each
  crate).
- **The stack**: wasm's stack is fixed (1 MB by default) and `stacker`
  cannot grow it, so a deeply nested config that is fine natively could
  trap. Not seen with the examples.
- **`std::fs` on wasm returns `Unsupported`**. Paths that read files
  (palettes, images, settings) should report that as an error, as the
  rules require; only the examples' paths were exercised.
- **Dependencies' wasm support**: getrandom 0.3 needs a cfg flag set by
  our build; a dependency that adds native-only code would break the
  build, which only a CI build catches.
- **file://**: the site works from `file://` (docs/architecture.md), but
  a browser loads no WebAssembly or module from a file, so the
  playground needs the site served over http. Opened from a file, the
  page says so and shows nothing else (`scripts/site/shots.sh` passes).

## Recommendation

Ship it as the site's live examples, beside the sprite players (which
need no download and keep working from a file). To make it production:

1. **CI**: a job that runs `scripts/wasm/check.sh` (the render path for
   wasm32-unknown-unknown), builds strand-web (`scripts/wasm/build.sh`)
   and runs `scripts/wasm/check-browser.sh`, which fails unless every
   frame equals eval's. The image is `scripts/wasm/Dockerfile`, on the
   CI image, with the target, wasm-bindgen-cli 0.2.129 and binaryen 124
   pinned.
2. **A size budget** as a tripwire: the module under 2.5 MB brotli
   (2.27 MB now), checked by the same job.
3. **Interfaces**: keep `strand_scene::time` (and strand-core's) as the
   one way the render path names its clock, with the guard above. Move
   `placement.rs` out of strand-surface into a crate or module both can
   depend on, instead of the `#[path]` include. Decide whether the
   renderer's and router's `set_now` should leave `eval-hooks`, so the
   web host can run their timers on the virtual clock too.

Then, in order of value: make the frame copy cheap (hand the canvas a
single surface's buffer, or composite a word at a time), add keyboard
input, and load images and icons from bytes the page fetches.

## How to look at it

```sh
scripts/wasm/run.sh bash scripts/wasm/build.sh        # site/assets/wasm/ (gitignored)
scripts/container/run.sh cargo run -q -p strand-site -- build
python3 -m http.server -d target/site 8000           # then open localhost:8000/playground.html
scripts/wasm/check-browser.sh                         # the headless check; shots in target/site-shots/
```
