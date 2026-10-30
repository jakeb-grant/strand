#!/usr/bin/env bash
# package.sh ID OUT: builds the release archive the installer installs
# (scripts/install.sh; site/start/install.md) into OUT:
#
#   strand-ID-x86_64-linux.tar.gz   strand-ID/ with strand, strand-auth,
#                                   strand-dev, strand-eval, the licenses
#                                   and THIRD-PARTY-LICENSES.txt
#   strand-ID-x86_64-linux.tar.gz.sha256
#   install.sh
#
# ID is the workspace's version for a tagged release (0.1.0), or
# edge-<12 hex digits of the commit> for the edge build; strand's
# `--version` names the channel. Release builds of the four programs
# (the root workspace's release profile, eval/'s own), with cargo-about
# ($CARGO_ABOUT, else on PATH; scripts/release/about.toml lists the
# licenses accepted) and python3. Fails when a program needs a newer
# glibc than install.sh's MIN_GLIBC, so the installer's check stays true.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
if [ $# -ne 2 ]; then
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi
ID=$1
OUT=$2
ARCH=x86_64
ABOUT=${CARGO_ABOUT:-cargo-about}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
case $ID in
  edge-*)
    [[ $ID =~ ^edge-[0-9a-f]{12}$ ]] || { echo "package: an edge ID is edge-<12 hex digits>, not $ID" >&2; exit 2; }
    export STRAND_CHANNEL=edge
    ;;
  *)
    [ "$ID" = "$version" ] || { echo "package: $ID is not the workspace's version ($version)" >&2; exit 2; }
    export STRAND_CHANNEL=release
    ;;
esac
command -v "$ABOUT" >/dev/null || { echo "package: no cargo-about ($ABOUT): cargo install --locked cargo-about --features cli" >&2; exit 2; }

cargo build --release --locked -p strand -p strand-auth -p strand-dev
cargo build --release --locked --manifest-path eval/Cargo.toml
REL=$CARGO_TARGET_DIR/release
PROGRAMS=(strand strand-auth strand-dev strand-eval)

# The newest glibc symbol version any of them needs, against the one
# the installer checks for.
min=$(sed -n 's/^MIN_GLIBC=//p' scripts/install.sh)
need=$(for p in "${PROGRAMS[@]}"; do readelf -V --wide "$REL/$p"; done |
  grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)
if [ "$(printf '%s\n%s\n' "$min" "$need" | sort -V | tail -1)" != "$min" ]; then
  echo "package: the programs need glibc $need, newer than install.sh's MIN_GLIBC=$min: raise it there and in site/start/install.md" >&2
  exit 1
fi
echo "package: the programs need glibc $need (install.sh checks for $min)"

stage=$CARGO_TARGET_DIR/release-stage
rm -rf "$stage"
dir=$stage/strand-$ID
mkdir -p "$dir" "$OUT"
rm -f "$OUT"/strand-*-$ARCH-linux.tar.gz "$OUT"/strand-*-$ARCH-linux.tar.gz.sha256
for p in "${PROGRAMS[@]}"; do
  install -m 755 "$REL/$p" "$dir/$p"
done
install -m 644 LICENSE-APACHE LICENSE-MIT "$dir/"

# Each program's crates and their licenses, merged.
jsons=()
for m in crates/strand crates/strand-auth crates/strand-dev eval; do
  j=$stage/about-${m##*/}.json
  "$ABOUT" generate -c scripts/release/about.toml --locked --manifest-path "$m/Cargo.toml" \
    --format json -o "$j"
  jsons+=("$j")
done
python3 -I scripts/release/licenses.py "$dir/THIRD-PARTY-LICENSES.txt" "${jsons[@]}"

name=strand-$ID-$ARCH-linux.tar.gz
# The same bytes for the same programs: names sorted, no owners, the
# commit's time, gzip without a timestamp.
when=$(git log -1 --format=%ct 2>/dev/null || echo 0)
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$when" \
  -C "$stage" -cf - "strand-$ID" | gzip -n -9 >"$OUT/$name"
(cd "$OUT" && sha256sum "$name" >"$name.sha256")
install -m 755 scripts/install.sh "$OUT/install.sh"
echo "package: $OUT/$name ($(du -h "$OUT/$name" | cut -f1))"
"$dir/strand" --version
