#!/bin/sh
# Strand's installer (site/start/install.md). Installs Strand for this
# user, or updates it, from the releases on GitHub: strand, strand-auth,
# strand-dev and strand-eval go into ~/.local/share/strand/<version>/,
# and ~/.local/bin gets `strand` and `strand-dev` linked to them. It
# needs no root and writes nowhere else.
#
#   curl -fsSL https://strand.jacobgrant.dev/install.sh | sh
#   curl -fsSL https://strand.jacobgrant.dev/install.sh | sh -s -- --edge
#
# `--help` lists the options. Run it again to update: it keeps the
# channel chosen last (--edge or --release), the version it replaces and
# any version a running program still uses, and removes older ones.
set -eu

REPO=jakeb-grant/strand
# Tests point this at a file:// directory laid out as GitHub's API.
API=${STRAND_INSTALL_API:-https://api.github.com/repos/$REPO}
ARCH=x86_64
# The newest glibc symbol version the release binaries need
# (scripts/release/package.sh checks the binaries against it).
MIN_GLIBC=2.39
PROGRAMS="strand strand-auth strand-dev strand-eval"
LINKED="strand strand-dev"

say() { printf '%s\n' "$*"; }
die() {
  printf 'strand-install: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF_USAGE'
usage: install.sh [--edge | --release | --version X | --from FILE]
                  [--bin-dir DIR] [--force] [--uninstall]

Installs Strand for this user, or updates it: strand, strand-auth,
strand-dev and strand-eval into ~/.local/share/strand/<version>/, with
`strand` and `strand-dev` linked from ~/.local/bin. No root needed.

  --edge          the newest build of main that passed CI
  --release       the newest tagged release (the default)
  --version X     the release tagged vX, such as 0.1.0
  --from FILE     a release archive already downloaded, with FILE.sha256
  --bin-dir DIR   where the links go (default ~/.local/bin)
  --force         replace a strand or strand-dev in the bin directory that
                  this script did not put there; uninstall while strand runs
  --uninstall     remove what this script installed (never your config)

Run it again to update, on the channel chosen last.
EOF_USAGE
}

channel=
pin=
from=
bin_dir=
force=0
action=install
while [ $# -gt 0 ]; do
  case $1 in
    --edge) channel=edge ;;
    --release) channel=release ;;
    --version)
      [ $# -ge 2 ] || die "--version needs a version, such as --version 0.1.0"
      pin=${2#v}
      shift
      ;;
    --from)
      [ $# -ge 2 ] || die "--from needs a file"
      from=$2
      shift
      ;;
    --bin-dir)
      [ $# -ge 2 ] || die "--bin-dir needs a directory"
      bin_dir=$2
      shift
      ;;
    --force) force=1 ;;
    --uninstall) action=uninstall ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown option $1 (--help lists them)" ;;
  esac
  shift
done

[ -n "${HOME-}" ] || die "HOME is not set"
[ "$(id -u)" != 0 ] || [ $force = 1 ] ||
  die "run this as your own user, not root: it installs Strand for one user and needs no root (--force goes ahead)"
case ${XDG_DATA_HOME-} in
  /*) data_home=$XDG_DATA_HOME ;;
  *) data_home=$HOME/.local/share ;;
esac
DATA=$data_home/strand
BIN=${bin_dir:-$HOME/.local/bin}
case $BIN in
  /*) ;;
  *) BIN=$(pwd)/$BIN ;;
esac

# The lock: one installer at a time. A lock left by an installer that
# is gone is taken over.
lock() {
  mkdir -p "$DATA"
  if ! mkdir "$DATA/.lock" 2>/dev/null; then
    other=$(cat "$DATA/.lock/pid" 2>/dev/null || true)
    if [ -n "$other" ] && kill -0 "$other" 2>/dev/null; then
      die "another install is running (pid $other)"
    fi
    rm -rf "$DATA/.lock"
    mkdir "$DATA/.lock" || die "cannot lock $DATA"
  fi
  echo $$ >"$DATA/.lock/pid"
  trap 'rm -rf "$DATA/.lock" "$DATA/.tmp-$$"' EXIT
  trap 'exit 130' INT TERM HUP
}

# Programs running from a directory under $1 (its real path): "pid
# path" lines. Another user's processes cannot be read and do not
# matter.
running_from() {
  real=$(cd "$1" 2>/dev/null && pwd -P) || return 0
  for p in /proc/[0-9]*; do
    exe=$(readlink "$p/exe" 2>/dev/null) || continue
    case $exe in
      "$real"/*) printf '%s %s\n' "${p#/proc/}" "$exe" ;;
    esac
  done
}

# Whether $1 is a link this script made: one into $DATA.
ours() {
  [ -L "$1" ] || return 1
  case $(readlink "$1") in
    "$DATA"/*) return 0 ;;
  esac
  return 1
}

uninstall() {
  [ -d "$DATA" ] || {
    for n in $LINKED; do
      if ours "$BIN/$n"; then rm -f "$BIN/$n"; fi
    done
    say "Strand is not installed in $DATA: nothing to remove."
    return
  }
  lock
  busy=$(running_from "$DATA/versions")
  if [ -n "$busy" ] && [ $force = 0 ]; then
    say "$busy" | sed 's/^/  running: pid /' >&2
    die "strand is running from $DATA; stop it first (its lock screen needs strand-auth from there), or pass --force"
  fi
  for n in $LINKED; do
    if ours "$BIN/$n"; then rm -f "$BIN/$n"; fi
  done
  trap - EXIT
  rm -rf "$DATA"
  say "Removed $DATA and the links in $BIN."
  say "Your config (~/.config/strand), state and cache are kept."
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "this needs $1, which is not installed"
}

# Whether version $1 is at least $2 (both major.minor).
at_least() {
  a1=${1%%.*} a2=${1#*.} b1=${2%%.*} b2=${2#*.}
  a2=${a2%%.*} b2=${b2%%.*}
  [ "$a1" -gt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -ge "$b2" ]; }
}

check_system() {
  [ "$(uname -s)" = Linux ] || die "Strand runs on Linux only"
  [ "$(uname -m)" = $ARCH ] ||
    die "there are release builds for $ARCH only, and this machine is $(uname -m): build Strand from source (site/start/install.md)"
  glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
  case $glibc in
    "glibc "*) glibc=${glibc#glibc } ;;
    *) die "the release builds need glibc $MIN_GLIBC or newer, and this system has no glibc (musl?): build Strand from source" ;;
  esac
  at_least "$glibc" $MIN_GLIBC ||
    die "the release builds need glibc $MIN_GLIBC or newer, and this system has $glibc: build Strand from source, or use a newer distribution"
  for t in tar gzip sha256sum readlink mv ln; do need $t; done
  if command -v curl >/dev/null 2>&1; then
    fetcher=curl
  elif command -v wget >/dev/null 2>&1; then
    fetcher=wget
  else
    die "this needs curl or wget to download"
  fi
}

# fetch URL FILE: downloads URL into FILE; fails quietly.
fetch() {
  case $1 in
    https://*) ;;
    file://*) [ -n "${STRAND_INSTALL_API-}" ] || return 1 ;;
    *) return 1 ;;
  esac
  if [ $fetcher = curl ]; then
    curl --proto '=https,file' --tlsv1.2 -fsSL --retry 2 -o "$2" "$1" 2>/dev/null
  else
    case $1 in file://*) return 1 ;; esac
    wget -q --https-only -O "$2" "$1"
  fi
}

# The release archive's URL, from the release's JSON in $1: the one
# asset named strand-<id>-x86_64-linux.tar.gz (edge holds the newest and
# the one before while it is replaced; either is a whole build).
archive_url() {
  # shellcheck disable=SC2020 # each of the three characters ends a line
  tr ',{}' '\n\n\n' <"$1" |
    sed -n 's/^ *"browser_download_url": *"\([^"]*\)" *$/\1/p' |
    grep -- "/strand-[0-9A-Za-z._-]*-$ARCH-linux\.tar\.gz\$" | tail -n 1
}

# id_of NAME: the version a release archive's name holds.
id_of() {
  n=${1##*/}
  n=${n#strand-}
  n=${n%-"$ARCH"-linux.tar.gz}
  case $n in
    "" | *[!0-9A-Za-z._-]* | .*) return 1 ;;
  esac
  printf '%s\n' "$n"
}

# download: sets $archive and $id, the archive downloaded into $tmp and
# checked against its .sha256. Asked again when a file has gone (edge
# replaced between the two downloads).
download() {
  case $channel in
    edge) what="the edge build" ref=releases/tags/edge ;;
    *)
      if [ -n "$pin" ]; then
        what="release $pin" ref=releases/tags/v$pin
      else
        what="the newest release" ref=releases/latest
      fi
      ;;
  esac
  tries=0
  while :; do
    tries=$((tries + 1))
    if ! fetch "$API/$ref" "$tmp/release.json"; then
      if [ -z "$pin" ] && [ "$channel" != edge ]; then
        die "found no release on GitHub (there may be none yet: --edge installs the newest build of main), or GitHub could not be reached or is limiting requests (try again in an hour)"
      fi
      die "could not find $what on GitHub, or GitHub could not be reached or is limiting requests (try again in an hour)"
    fi
    url=$(archive_url "$tmp/release.json") || true
    [ -n "$url" ] || die "$what has no $ARCH Linux archive"
    id=$(id_of "$url") || die "$what's archive has a name this script does not know: ${url##*/}"
    archive=$tmp/${url##*/}
    if [ "$(installed_id)" = "$id" ] && whole "$DATA/versions/$id"; then
      return 0
    fi
    say "Downloading strand $id"
    if fetch "$url.sha256" "$archive.sha256" && fetch "$url" "$archive"; then
      break
    fi
    [ $tries -lt 3 ] || die "could not download $url"
    sleep 5
  done
  verify
}

verify() {
  [ -f "$archive.sha256" ] || die "no checksum file $archive.sha256"
  want=$(cut -d' ' -f1 <"$archive.sha256")
  got=$(sha256sum "$archive" | cut -d' ' -f1)
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    die "${archive##*/} does not match its checksum: the download is damaged (try again)"
  fi
}

whole() {
  for p in $PROGRAMS; do
    [ -f "$1/$p" ] && [ -x "$1/$p" ] || return 1
  done
}

installed_id() {
  t=$(readlink "$DATA/current" 2>/dev/null) || return 0
  printf '%s\n' "${t#versions/}"
}

# The libraries the programs in $1 cannot find, one per line.
missing_libs() {
  command -v ldd >/dev/null 2>&1 || return 0
  for p in $PROGRAMS; do
    ldd "$1/$p" 2>/dev/null | sed -n 's/^[[:space:]]*\([^ ]*\) => not found$/\1/p'
  done | sort -u
}

# Unpacks the archive and starts the new strand once, before anything
# installed changes.
unpack() {
  mkdir "$tmp/x"
  tar -xzf "$archive" -C "$tmp/x" --no-same-owner --no-same-permissions ||
    die "${archive##*/} could not be unpacked"
  new=$tmp/x/strand-$id
  whole "$new" || die "${archive##*/} does not hold $PROGRAMS"
  libs=$(missing_libs "$new")
  if [ -n "$libs" ]; then
    say "$libs" | sed 's/^/  missing: /' >&2
    die "Strand needs these libraries; install them with your package manager:
  Arch:          sudo pacman -S libpipewire libxkbcommon wayland fontconfig
  Debian/Ubuntu: sudo apt install libpipewire-0.3-0t64 libxkbcommon0 libwayland-client0 libfontconfig1
  Fedora:        sudo dnf install pipewire-libs libxkbcommon libwayland-client fontconfig
then run this again. Nothing was changed."
  fi
  if ! out=$("$new/strand" --version 2>&1); then
    say "$out" >&2
    die "the new strand does not start on this system; nothing was changed"
  fi
  version_line=$out
}

# link_into DIR NAME TARGET: DIR/NAME -> TARGET, replaced in one rename.
link_into() {
  ln -s "$3" "$1/.$2.new-$$"
  mv -fT "$1/.$2.new-$$" "$1/$2"
}

check_links() {
  [ $force = 1 ] && return 0
  for n in $LINKED; do
    if [ -e "$BIN/$n" ] || [ -L "$BIN/$n" ]; then
      ours "$BIN/$n" ||
        die "$BIN/$n is already there and is not this installer's (a copy from an older install?): move it away, or pass --force to replace it"
    fi
  done
}

prune() {
  keep_a=$1 keep_b=$2
  for d in "$DATA"/versions/*; do
    [ -d "$d" ] || continue
    v=${d##*/}
    [ "$v" = "$keep_a" ] || [ "$v" = "$keep_b" ] && continue
    if [ -n "$(running_from "$d")" ]; then
      say "Kept $v: a program still runs from it."
      continue
    fi
    rm -rf "$d"
  done
}

on_path() {
  case :$PATH: in
    *:"$1":*) return 0 ;;
  esac
  return 1
}

do_install() {
  [ -z "$pin" ] || [ "$channel" != edge ] || die "--edge and --version cannot go together"
  [ -z "$from" ] || [ -z "$channel$pin" ] || die "--from installs the archive it names; leave out --edge, --release and --version"
  check_system
  lock
  if [ -z "$channel" ]; then
    channel=$(cat "$DATA/channel" 2>/dev/null || echo release)
    case $channel in edge | release) ;; *) channel=release ;; esac
  fi
  [ -z "$pin" ] || channel=release
  check_links
  tmp=$DATA/.tmp-$$
  rm -rf "$tmp"
  mkdir -p "$tmp" "$DATA/versions" "$BIN"
  if [ -n "$from" ]; then
    [ -f "$from" ] || die "no file $from"
    id=$(id_of "$from") || die "$from is not named as a release archive (strand-<version>-$ARCH-linux.tar.gz)"
    archive=$tmp/${from##*/}
    cp "$from" "$archive"
    cp "$from.sha256" "$archive.sha256" 2>/dev/null || die "no checksum file $from.sha256 beside $from"
    verify
  else
    download
  fi
  before=$(installed_id)
  if [ "$before" = "$id" ] && whole "$DATA/versions/$id"; then
    for n in $LINKED; do link_into "$BIN" "$n" "$DATA/current/$n"; done
    echo "$channel" >"$DATA/channel"
    say "strand $id is installed already and up to date."
    return 0
  fi
  unpack
  if [ -d "$DATA/versions/$id" ]; then
    if [ -n "$(running_from "$DATA/versions/$id")" ]; then
      # The same version, in use: what is there stays.
      whole "$DATA/versions/$id" || die "$DATA/versions/$id is incomplete and in use; stop strand and run this again"
    else
      rm -rf "$DATA/versions/$id"
      mv "$new" "$DATA/versions/$id"
    fi
  else
    mv "$new" "$DATA/versions/$id"
  fi
  link_into "$DATA" current "versions/$id"
  for n in $LINKED; do link_into "$BIN" "$n" "$DATA/current/$n"; done
  echo "$channel" >"$DATA/channel"
  prune "$id" "$before"
  say "Installed $version_line"
  say "  in $DATA/versions/$id; \`strand\` is $BIN/strand"
  if [ -n "$before" ] && [ -n "$(running_from "$DATA/versions/$before")" ]; then
    say "A strand from $before is still running: restart it to run this version."
  fi
  if ! on_path "$BIN"; then
    say "$BIN is not on your PATH: add it, or start Strand from your compositor as $BIN/strand run"
  fi
}

case $action in
  install) do_install ;;
  uninstall) uninstall ;;
esac
