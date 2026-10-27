# Install

Strand has no packaged release yet. There is no distribution package,
no tagged version and no crate on crates.io. You build it from a
checkout of the repository, or take the
[prebuilt binaries](#prebuilt-binaries) GitHub keeps.

## What you need to build it

- **Rust 1.97 or newer.** The workspace sets `rust-version = "1.97"`.
- **The development files of fontconfig, xkbcommon, Wayland, PipeWire
  (with SPA) and PAM**, found through `pkg-config`, plus clang and
  libclang. On Ubuntu 24.04:

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev \
  libpipewire-0.3-dev libspa-0.2-dev clang libclang-dev libpam0g-dev
```

Other distributions name the packages differently.

## Build it

```sh
git clone https://github.com/jakeb-grant/strand
cd strand
cargo build --release -p strand -p strand-auth
```

This writes two programs into `target/release/`:

- `strand`, the shell and its command line;
- `strand-auth`, the small helper the lock screen checks passwords
  with. It is the only program in the workspace that links libpam.

Put both in the same directory on your `PATH`, for example:

```sh
mkdir -p ~/.local/bin
cp target/release/strand target/release/strand-auth ~/.local/bin/
strand --version
```

`strand --version` prints `strand 0.0.0`.

Strand looks for `strand-auth` beside its own executable, then in
`/usr/libexec/strand`, `/usr/lib/strand` and `/usr/local/libexec/strand`,
and nowhere else: never on your `PATH`, since it is the program your
password is handed to. When a config has a `lock` and there is no
helper, `strand run` warns at start, or at the reload that adds the
`lock`, that the lock would have no way to unlock, and `strand check`
reports an error for the config. Both say where to put the helper:
beside the `strand` that is running, with `cargo install --locked
--path crates/strand-auth --root <prefix>` when that `strand` is in
`<prefix>/bin`, else by copying `target/release/strand-auth` into its
directory. A shell with no lock screen runs without the helper and
says nothing about it.

### Without the GPU backend

The GPU backend is a default feature. To build without it:

```sh
cargo build --release -p strand --no-default-features
```

That build links no wgpu, vello\_gpu or naga, and draws everything on
the CPU.

### The other tools

Two more programs live in the same repository, outside the shell:

- `strand-dev`, the language server and the inspector:
  `cargo build --release -p strand-dev`. See
  [Editor setup](../tools/editor.md) and
  [the inspector](../tools/inspector.md).
- `strand-eval`, the sandbox that replays a glitch report:
  `cargo build --release --manifest-path eval/Cargo.toml`. It is a
  workspace of its own, so it lands in `eval/target/release/`. See
  [Reporting a glitch](../tools/reporting.md).

## Prebuilt binaries

Every build of the repository on GitHub keeps its release binaries for
30 days, as a workflow artifact named `strand-release-binaries`:
`strand-gpu`, the default build, and `strand-cpu`, the build without
the GPU backend. Both are built on Ubuntu 24.04. To use one, download
the artifact from a green run's page under the repository's Actions
tab, rename `strand-gpu` to `strand`, make it executable and put it on
your `PATH`.

The artifact holds no `strand-auth`, `strand-dev` or `strand-eval`. A
lock screen still needs the helper built from source.

## What the compositor needs

Strand runs on a Wayland compositor that offers `wl_compositor`,
`wl_shm` and `zwlr_layer_shell_v1` (layer shell). Without one of them,
`strand run` stops at start and names the one that is missing:
"the compositor does not offer zwlr\_layer\_shell\_v1".

Some features need one more protocol:

| Protocol | Needed for | Without it |
| --- | --- | --- |
| `xdg_wm_base` | `window` surfaces and popups | windows and popups are not shown |
| `ext_session_lock_manager_v1` | the `lock` surface | a `lock` cannot lock the session, and `strand run` warns at start |
| `ext_background_effect_manager_v1`, with blur | compositor blur behind `blur` | `blur` draws its tint fallback, and Strand says why once |

Hyprland blurs layer surfaces through its own layer rules instead:
`strand compositor-rules` prints the rules for your config, to paste
into your Hyprland config. Strand never applies them itself. They are
Lua (`hl.layer_rule`) by default, and `--classic` prints hyprlang
`layerrule` lines for an older config.

Strand also uses these when the compositor offers them:
`wp_viewporter`, `wp_fractional_scale_manager_v1`, `wp_presentation`,
`wp_alpha_modifier_v1`, `wp_single_pixel_buffer_manager_v1`,
`zwp_idle_inhibit_manager_v1`, `wl_data_device_manager` and
`xdg_activation_v1`. `strand report` lists every global your
compositor offers, with its version.

The `workspaces`, `windows` and `wm` services read sway's, Hyprland's
and niri's own IPC. On a compositor without one of those adapters, the
windows and workspaces come from the standard protocols:
`ext-foreign-toplevel-list-v1`, `zwlr_foreign_toplevel_management_v1`
and `ext-workspace-v1`.

## Compositors

Strand is tested on **sway**, **Hyprland**, **niri** and **labwc** on
every change.

<!-- repo-only -->
### Compositors CI runs on

CI's `compositors` job runs the compositor matrix test on **sway**,
**niri**, **Hyprland** and **labwc**, installed from `archlinux:latest`,
on every push and every night. It checks what the `workspaces` and
`windows` services report on each of them, and on sway, niri and
Hyprland it runs the design document's bar in `strand run`. The rest
of the test suite runs on a headless sway 1.9 on Ubuntu 24.04.
<!-- /repo-only -->

Next: [Running a shell](running.md).
