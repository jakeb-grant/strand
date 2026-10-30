# Install

The install script installs Strand for your user, and updates it when
you run it again. It needs no root, and writes only to
`~/.local/share/strand` and `~/.local/bin`.

```sh
curl -fsSL https://strand.jacobgrant.dev/install.sh | sh
```

It downloads the newest release from GitHub and checks it against its
checksum. Then it starts the new `strand` once before switching to it,
so a build that cannot run on your system changes nothing. When it is
done:

```sh
strand --version
```

prints the version and the commit it was built from, such as
`strand 0.1.0 (a764f45c0d1e)`.

To read the script before it runs, download it first:

```sh
curl -fsSLO https://strand.jacobgrant.dev/install.sh
less install.sh
sh install.sh
```

Every release on GitHub carries the same script. `sh install.sh --help`
lists its options.

## What you need

- **Linux on x86\_64 with glibc 2.39 or newer**, such as Arch, Fedora 40
  or newer, Ubuntu 24.04 or newer, or Debian 13. The releases are built
  on Ubuntu 24.04. On an older system, or another architecture, the
  script says so and stops; [build from source](#build-from-source)
  instead.
- **PipeWire's library, xkbcommon, Wayland's client library and
  fontconfig.** A Wayland desktop has them already. When one is missing
  the script names it, stops, and prints the command that installs them:

| Distribution | Command |
| --- | --- |
| Arch | `sudo pacman -S libpipewire libxkbcommon wayland fontconfig` |
| Debian, Ubuntu | `sudo apt install libpipewire-0.3-0t64 libxkbcommon0 libwayland-client0 libfontconfig1` |
| Fedora | `sudo dnf install pipewire-libs libxkbcommon libwayland-client fontconfig` |

- **curl** (or wget), **tar**, **gzip** and **sha256sum**.

## What it installs

```text
~/.local/share/strand/
  versions/0.1.0/      strand, strand-auth, strand-dev, strand-eval
                       and their licenses
  current              -> versions/0.1.0
~/.local/bin/
  strand               -> ~/.local/share/strand/current/strand
  strand-dev           -> ~/.local/share/strand/current/strand-dev
```

- `strand` is the shell and its command line.
- `strand-auth` is the small helper the lock screen checks passwords
  with. It stays beside `strand`, off your `PATH`
  ([the lock screen's helper](#the-lock-screens-helper)).
- `strand-dev` is the language server and the inspector
  ([Editor setup](../tools/editor.md)).
- `strand-eval` is the sandbox `strand replay` runs
  ([Reporting a glitch](../tools/reporting.md)).

`$XDG_DATA_HOME/strand` takes the place of `~/.local/share/strand` when
`XDG_DATA_HOME` is set, and `--bin-dir DIR` puts the links in another
directory.

When `~/.local/bin` is not on your `PATH`, the script says so. Add it
to your `PATH` in your shell's profile, or start Strand from your
compositor by its full path, `~/.local/bin/strand run`
([Running a shell](running.md#start-it-and-bind-keys)).

The script will not run as root: it installs Strand for one user. It
also will not replace a `strand` in `~/.local/bin` that it did not put
there, such as a copy of one you built. Move that away, or pass
`--force`.

## Update

Run the same command again:

```sh
curl -fsSL https://strand.jacobgrant.dev/install.sh | sh
```

When the newest build is the one installed, it says so and changes
nothing. Otherwise it puts the new version beside the old one and
switches `current` to it in one step. A shell that is running keeps the
version it started with: restart it to run the new one. Commands such
as `strand toggle` run the new version straight away, and still work
with the older shell.

The script keeps the version it replaced, and any version a program is
still running from, such as your shell or an editor's language server.
It removes older versions. A running shell's lock screen needs the
`strand-auth` beside it, so its version stays until the shell stops.

Strand never updates itself and never checks for updates.

## Releases and edge

There are two channels:

- **Releases** are tagged `v0.1.0` and so on, on GitHub. The script
  installs the newest one unless you ask for another.
- **Edge** is the newest build of `main`. Every push to `main` that
  passes CI replaces it.

```sh
curl -fsSL https://strand.jacobgrant.dev/install.sh | sh -s -- --edge
```

The script remembers the channel. After `--edge`, running it again
updates to the newest edge build, until `--release` switches back.
`--version 0.1.0` installs that release and switches back too. An edge
build names its channel: `strand 0.1.0 (edge, a764f45c0d1e)`.

A build on either channel passed the whole of CI first, including the
compositor matrix, the lock screen in its VM, and this script
installing, updating and removing it on fresh Ubuntu and Arch systems.

## Uninstall

```sh
curl -fsSL https://strand.jacobgrant.dev/install.sh | sh -s -- --uninstall
```

This removes `~/.local/share/strand` and the two links. It keeps your
config in `~/.config/strand`, and Strand's state and cache. While a
`strand` from there is running it refuses, since that shell's lock
screen could no longer check passwords: stop the shell first.

## The lock screen's helper

Strand looks for `strand-auth` beside its own executable, then in
`/usr/libexec/strand`, `/usr/lib/strand` and `/usr/local/libexec/strand`,
and nowhere else: never on your `PATH`, since it is the program your
password is handed to. The install script puts it beside `strand`.
When a config has a `lock` and there is no helper, `strand run` warns at
start, or at the reload that adds the `lock`, that the lock would have
no way to unlock, and `strand check` reports an error for the config.
Both say where to put the helper. A shell with no lock screen runs
without the helper and says nothing about it.

The helper checks passwords with the PAM service `strand`, from
`/etc/pam.d/strand`. Without that file it uses your system's `login`
service, which needs nothing installed, and says so once in the log. To
give the lock its own PAM setup, write `/etc/pam.d/strand` (this needs
root) with your system's usual stack: `auth include system-auth` and
`account include system-auth` on Arch and Fedora, or
`@include common-auth` and `@include common-account` on Debian and
Ubuntu.

## Build from source

### What you need to build it

- **Rust 1.97 or newer.** The workspace sets `rust-version = "1.97"`.
- **The development files of fontconfig, xkbcommon, Wayland, PipeWire
  (with SPA) and PAM**, found through `pkg-config`, plus clang and
  libclang. On Ubuntu 24.04:

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev \
  libpipewire-0.3-dev libspa-0.2-dev clang libclang-dev libpam0g-dev
```

Other distributions name the packages differently.

### Build it

```sh
git clone https://github.com/jakeb-grant/strand
cd strand
cargo build --release -p strand -p strand-auth
```

This writes two programs into `target/release/`:

- `strand`, the shell and its command line;
- `strand-auth`, the lock screen's helper. It is the only program in
  the workspace that links libpam.

Put both in the same directory on your `PATH`, for example:

```sh
mkdir -p ~/bin
cp target/release/strand target/release/strand-auth ~/bin/
strand --version
```

When the helper is missing, `strand run` and `strand check` say where
to put it: beside the `strand` that is running, with `cargo install
--locked --path crates/strand-auth --root <prefix>` when that `strand`
is in `<prefix>/bin`, else by copying `target/release/strand-auth` into
its directory.

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
  workspace of its own, so it lands in `eval/target/release/`. Put it
  beside `strand`. See [Reporting a glitch](../tools/reporting.md).

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

### How a release is made

CI's `package` job builds the archive the script installs on every push
(`scripts/release/package.sh`) and tests the script against it in fresh
containers (`scripts/release/smoke.sh`). The `release` job publishes it
once every other job has passed: a push to `main` as the `edge`
pre-release, a tag `vX` as the release `vX`
([docs/development.md](../../docs/development.md#releasing)).
<!-- /repo-only -->

Next: [Running a shell](running.md).
