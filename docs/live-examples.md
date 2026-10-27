# Live examples: Strand's renderer in the docs site

Every example figure on the site can run live. Strand's compiler, logic
VM, layout, text and CPU renderer, built to WebAssembly, run the
example's own scenario in the page and draw into a canvas where the
frame player was. The reader can press the scenario's steps, replay
them, point and click, and edit the config, which runs again and shows
`strand check`'s diagnostics when it does not compile. The playground
page (`site/playground.md`, linked from Tools) runs every docs example
the same way.

It began as a spike (branch `laptop/wasm-spike`, 2026-10-26; its shims
and build measurements are kept below) and shipped on the same branch.
The decisions are in docs/decisions.md under `live-examples`.

## What a reader gets

- The frame player stays each figure's default and its fallback: it
  needs no script beyond the site's, works from `file://`, and does not
  play by itself when the reader asks for reduced motion.
- A figure offers **Try it live** only where it can run: the site has
  the WebAssembly build (`wasm/manifest.js` names it), the page is
  served over http(s) (a browser loads no WebAssembly from a file), and
  the browser has WebAssembly. Nothing is fetched until the reader asks;
  the browser check holds every page to that.
- Live, the canvas takes the frames' place and one bar the player's:
  **Play** boots the example again and plays its steps (as entering live
  mode does), and reads **Playing** while they run; **Next step** runs
  the scenario's next step that acts, round again after the last, and
  names it (`set on true` is `on = true`, a list its length; the
  command as written is its tooltip); **Back to recording** goes back
  to the player. A line over the config says it runs in the browser
  and runs again when edited. A figure written ```` ```example NAME
  recording ```` (the home page's) shows the frames alone, with no
  offer to run it live. A stage with nothing on screen and nothing
  coming says so, and that Next step runs the next step. The config
  is in an editor (a textarea over its own text, highlighted with the
  site's `hl-` classes). An edit runs again after 350 ms; a broken one shows
  its diagnostic as `strand check` prints it and leaves the last good
  run on the canvas; **Reset** puts the example's config back.
- The run follows the page's theme (`system.dark`, set as the site's
  dark twin of each scenario sets it) and the reader's reduced-motion
  setting (`system.reduced_motion`, so motion snaps as on their
  desktop, and the note says so).

## How it is built

- `crates/strand-web`, the host:
  - `shell.rs` boots a config against `SchemaHost::mock` with
    `Storage::none()` and runs it on a virtual clock, through
    `strand_render::host_loop`, as eval's driver and the shell do.
  - `steps.rs` plays the part of eval's step language the docs examples
    use (`rest [max N]`, `frames N`, `wait MS`, `set`, `mark`) one tick
    at a time, as eval's driver plays it; any other step is refused by
    name.
  - `surfaces.rs` is the compositor's side, placed by
    `strand_scene::placement`, composited for a canvas.
  - `live.rs` is an example as its figure carries it, and the icon files
    a host without a disk supplies (`strand-web-icons` finds them).
  - `web.rs` is the wasm-bindgen API: `Live.boot`, `advance`, `act`,
    `tick`, `set`, `get`, the pointer, `view`, `compose`, `frame_ptr`,
    `record`, `paints`, `notes`, `warnings`.
- `crates/strand-site`: each example figure carries its live data as
  JSON (`example::live`: files, `[services]`, screen, rate and steps; a
  header key the web host would not honour fails the build), and the
  ```` ```playground ```` block writes the playground's. Every page loads
  `wasm/manifest.js`, which the build writes as `null`, so the site
  builds with no wasm tooling and works as before from a file or a host.
- `scripts/wasm/build.sh`, in `scripts/wasm/run.sh`'s image (CI's image
  plus the wasm32 target, wasm-bindgen-cli 0.2.129 and binaryen 124):
  the lint below, the module, its glue, the two fonts, the icon files
  the examples draw (found by running them natively in that image, whose
  Adwaita is the one eval draws with) with their licences, and
  `sizes.json`. It fails over the size budget.
- `scripts/wasm/site.sh` adds that build to a built site: `wasm/` with
  the module, glue and icons named by their content's hash, and the
  manifest written again to name them.
- `site/assets/live.js` is the page's side, loaded only when the reader
  asks.
- `scripts/wasm/ci.sh` runs CI's `wasm` job on the laptop.

## Pixel parity

Every docs example, live, draws every frame eval drew for the site's
media, at the same time from its first frame and with the same pixel
hash, in light and dark mode: hello 1 of 1, springs 75 of 75, swap 33
of 33, toasts 44 of 44, OSD 34 of 34. Two checks hold it:

- `crates/strand-web/tests/live.rs` natively
  (`every_docs_example_draws_every_frame_eval_drew`, the icons only from
  the supplied files);
- `scripts/wasm/check-browser.sh` in headless Chromium, on the module
  as the site ships it.

The media are held to the approved reference recordings by eval's gate,
so the live frames are too.

Three things had to match the native run:

- **The clock.** The renderer's and router's own timers (a tooltip's
  delay, a hover popup's grace) read the virtual clock
  (`Renderer::set_now`, `Router::set_now`). The loop wakes only when the
  renderer's next wake is due or inline shaping delivered
  (`take_inline_shaped`), as eval's driver wakes it. The spike woke
  every tick and let those timers read the page's clock.
- **The icons.** The OSD's speaker and the toasts' app icons come from
  the icon theme. The page supplies the six files the examples draw
  (`strand_icons::set_files`, 8.5 KB brotli). Without them the OSD's
  frames differ (`the_osd_without_its_icon_is_not_what_eval_drew`).
- **The multiply-add.** vello_cpu's SIMD layer (fearless_simd) fuses a
  multiply and an add into one rounding natively (AVX2, NEON). Its wasm
  backend multiplied, then added, rounding twice. That moved a scaled
  or springing shape's edge by a step of alpha: the OSD's entrance
  (`scale: 0.9`) differed in 2 of its 34 frames.

  WebAssembly's only fused multiply-add is relaxed SIMD's, which a
  browser may fuse or not. Built with it, the OSD's entrance matched,
  but the toasts' and OSD's shadows did not, frame by frame. So it is
  not used.

  Instead the module is built with fearless_simd 0.7.0 patched
  (`scripts/wasm/exact-fma.py`, applied to the registry's source and
  checked by hash). An f32x4 multiply-add is done exactly in f64 lanes
  and rounded once to f32; the rare halfway case goes to the software
  `fmaf`, and an f64x2 multiply-add to `fma`. Those are the same bits as
  the processor's instruction. The patch is passed to cargo on the
  command line, never to the workspace, so native builds use the
  registry's crate; build.sh puts back the Cargo.lock entry cargo writes
  for it.

The parity is with this laptop's processor and CI's, both AVX2 with
FMA. A native build on a processor without FMA would draw what the
unpatched wasm draws; eval's references already assume FMA.

## Size

| File | brotli -q 11 (bytes) | raw (bytes) |
|---|---:|---:|
| the module | 2,282,940 | 7,978,819 |
| its glue | 4,040 | 19,174 |
| the two Liberation Sans faces | 339,322 | 825,388 |
| the icons (`icons.json`) | 8,516 | 84,226 |
| **what a reader fetches, once** | **2,634,818** | |

The budget is 2,500,000 bytes brotli for the module
(`WASM_BROTLI_BUDGET`). It is a tripwire: it fails CI's `wasm` job when
a change adds a large dependency to the render path. Raise it with a
reason in docs/decisions.md.

The build is opt-level s, wasm-opt -Oz and the simd128 target feature.
The spike measured it the smallest build as fast as opt-level 3 (see
"The spike: build variants"). The patch adds 3.6 KB brotli.

## Speed

`scripts/wasm/check-browser.sh` prints these. They come from headless
Chromium in Docker on the laptop (16 cores, loaded by other builds) on
2026-10-26, in milliseconds.

| Example | Ticks | Run headless | A tick live | A draw | First live frame | Memory |
|---|---:|---:|---:|---:|---:|---:|
| hello | 1 | 6 | 3.6 | 3.9 | 357 | 14.4 MB |
| springs | 75 | 456 | 1.7 | 2.8 | 398 | 14.4 MB |
| swap | 33 | 150 | 3.7 | 1.1 | 425 | 14.4 MB |
| toasts | 184 | 874 | 2.0 | 5.9 | 278 | 23.2 MB |
| OSD | 100 | 853 | 7.9 | 2.7 | 305 | 16.2 MB |

- **Run headless**: the steps run to their end with no canvas (the
  check's own run, which records each frame's hash).
- **A tick live**: logic, layout and paint at the screen's rate.
- **A draw**: composing the shown surfaces into the frame buffer, plus
  `putImageData`. The canvas reads the buffer in place in the module's
  memory, with no copy into JavaScript.
- **First live frame**: from the click, the script's load included.

The module is fetched, compiled and started in 42 to 57 ms, and its
fonts and icons are added in 19 to 25 ms. An edit boots again in about
46 ms.

The exact multiply-add costs time where vello_cpu works hardest. The
OSD's 100 ticks (a blurred backdrop, a shadow, a scaled entrance) took
604 ms unpatched, 1,160 ms with every lane through the software `fma`,
and 853 ms with the f64 lanes. The toasts' took 695, 1,357 and 874 ms.
Springs and swap barely moved. At 7.9 ms a tick the OSD keeps 60 Hz
here; a slow phone may drop frames while it springs, but the frames it
draws are still eval's.

## What is not there

- **Services**: the mock's values (`SchemaHost::mock`). Actions are
  recorded, not run, and the clock's time is the mock's.
- **Surfaces**: bars, panels, OSDs and popups only. There are no
  windows, lock surfaces, second screen or compositor poses. The canvas
  shows the part of the screen the shown surfaces hold, as the media do.
- **Input**: pointer moves and the left button. There is no keyboard,
  wheel or touchpad gesture yet.
- **The GPU**: the CPU renderer only (strand-render without `gpu`, the
  compiler without `shaders`).
- **Threads**: one. The raster's helpers, the text worker, the image
  decoder's thread and the renderer's timer thread do not start. The
  shell shapes text and decodes images inline, as eval does.
- **Files**: `Storage::none()`, so no `persist`, settings or palette
  files. Images other than the supplied icons are not found.
- **History clocks**: services' history samples (a graph's) read the
  wall clock. No docs example draws one.

## Guards

- **A new `std::time::Instant::now()` or `SystemTime::now()`** on the
  render path builds for wasm and panics at its first call. Such a call
  fails `scripts/wasm/lint.sh`, which runs clippy for
  wasm32-unknown-unknown on every workspace crate strand-web builds,
  with `scripts/wasm/clippy.toml`'s `disallowed-methods`. Only the wasm
  target can catch it: natively the render path's
  `strand_scene::time::Instant` is std's, so the two calls look the
  same. The lint also fails when strand-web comes to build a workspace
  crate the lint does not name.
- **The size budget**, above.
- **The frames**, native and in the browser, above. A renderer change
  that alters a docs example fails eval's gate first; once its media are
  drawn again, the live frames are held to them.
- **The patch**: `exact-fma.py` fails unless fearless_simd's wasm
  backend is the file it was written against. A vello upgrade that
  brings a new fearless_simd fails the build until the patch is looked
  at again, or is no longer needed.

## Serving

The built site, `target/site` with `wasm/` added, is static files, and
any static host serves it.

- **The module's type**: it must go out as `application/wasm`.
  `WebAssembly.instantiateStreaming` refuses another type, and the glue
  then falls back to a slower path.
- **Caching**: the module, its glue and the icons are named by their
  content's hash, so a host may cache `wasm/*.<hash>.*` for good.
  `wasm/manifest.js`, the pages and `assets/` change in place and should
  be revalidated.
- **Compression**: brotli or gzip on the way out matters, since the
  module is 8 MB raw.

## The spike: the shims

Every change to an existing crate the wasm build needed, and what it
costs natively.

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

Every signature with an `Instant` in it takes web-time's `Instant` on
wasm; `SystemTime` stays std's on both, since std's type works on wasm
and only its `now()` panics.

Shipping added more, natively too (docs/architecture.md):

- `strand_scene::placement`, moved from strand-surface, which
  re-exports it;
- the public virtual-clock setters;
- `strand_icons::set_files` and `file`;
- strand-render's image reader taking a supplied file first.

Some spots needed nothing, though they looked native-only:

- `hugepage.rs` is `target_os = "linux"` only.
- `libc::gettid` in `raster/mod.rs` is in test code.
- `malloc_trim` is Linux and glibc only.
- The raster's helpers ask `available_parallelism()`, which fails on
  wasm, so the raster uses one thread.
- The threads `thread::Builder::spawn` cannot start on wasm are already
  handled: no timer, and images and text done inline.
- `stacker` builds but cannot grow the stack, so its guard does nothing
  (see "Risks").
- chrono's `clock` builds without `wasmbind`, and the mock pins its zone
  to UTC.

## The spike: build variants

These are the sizes of `strand_web_bg.wasm` after wasm-bindgen, in
bytes, from the spike build (before the clock, icon and multiply-add
work). The times are from headless Chromium, in ms: medians of 5
interleaved rounds with the laptop loaded, so take them as rough,
though the order held across runs. "First frame" is a cold boot of the
springs example through its first tick and frame copy.

| Build | Raw | brotli | Compile | First frame | Springs tick | Hello first tick |
|---|---:|---:|---:|---:|---:|---:|
| opt-level 3, no wasm-opt | 11,960,773 | 2,697,892 | 78.8 | 220.1 | 1.57 | 2.2 |
| opt-level 3, wasm-opt -O3 | 9,593,136 | 2,604,575 | 67.2 | 283.2 | 1.48 | 2.2 |
| opt-level 3, wasm-opt -Oz | 9,451,764 | 2,607,670 | 68.9 | 266.5 | 1.54 | 2.2 |
| opt-level s, wasm-opt -Oz | 8,552,322 | 2,344,618 | 65.0 | 240.6 | 2.59 | 8.1 |
| opt-level z, wasm-opt -Oz | 7,673,752 | 2,116,106 | 60.3 | 251.2 | 3.90 | 10.8 |
| opt-level 3, -O3, simd128 | 9,415,612 | 2,571,056 | 64.0 | 272.3 | 1.50 | 2.1 |
| **opt-level s, -Oz, simd128** | 7,867,165 | 2,266,459 | 64.2 | 240.3 | 1.31 | 2.4 |

Without simd128, fearless_simd runs its scalar fallback. With it,
opt-level s is as fast as opt-level 3 and 300 KB smaller over the wire.
Opt-level z costs 2 to 4 times as much a tick. To measure a variant
again, build with `WASM_RUST_OPT` or `WASM_OPT` set, add it to a site,
and read the browser check's timings.

What the code is (opt-level s, before wasm-opt: 8.5 MB of code and
0.9 MB of data):

| Part | Size |
|---|---:|
| strand-compiler | 1.06 MB |
| strand-render | 0.72 MB |
| strand-core | 0.33 MB |
| the text stack (read-fonts, harfrust, skrifa, zeno, parley) | 1.08 MB |
| SVG (tiny-skia, usvg, resvg) | about 0.45 MB |
| image decoders | about 0.25 MB |
| serde and serde_json | 0.26 MB |
| taffy | 0.16 MB |
| velato | 0.08 MB |
| generic code instantiated in core, alloc and hashbrown | 2.6 MB |

## Risks

- **The stack**: wasm's stack is fixed (1 MB by default) and `stacker`
  cannot grow it, so a deeply nested config that is fine natively could
  trap. This has not happened with the examples; a trap would show as
  the run's error.
- **`std::fs` on wasm returns `Unsupported`**: paths that read files
  report it as an error, as the rules require. Only the examples' paths
  are exercised.
- **Dependencies' wasm support**: a dependency that adds native-only
  code breaks the wasm build, which CI's `wasm` job catches.
- **The patch** ties the build to fearless_simd 0.7.0 (see "Guards").

## How to look at it

```sh
scripts/wasm/ci.sh                  # lint, build, site, browser check (CI's wasm job)
# or step by step:
scripts/wasm/run.sh bash scripts/wasm/build.sh        # target/wasm/pkg
scripts/container/run.sh cargo run -q -p strand-site -- build
bash scripts/wasm/site.sh                             # target/site/wasm
bash scripts/wasm/check-browser.sh                    # shots in target/site-shots/live/
```

To look by hand, serve `target/site` from inside a container (the
laptop's rules keep browsers and servers off the host session), or
download CI's `site-live` artifact.
