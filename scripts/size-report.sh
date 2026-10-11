#!/usr/bin/env bash
# The release binary's `.text`, broken down by crate and by symbol, for
# the two gated builds: CPU-only (--no-default-features) and GPU (the
# default features). Each binary is built exactly as budgets.rs's gate
# test builds it (`cargo test --release -p strand --test budgets`, fat
# LTO, the workspace's per-package opt-levels), so the `.text` figure is
# the gate's figure (docs/m5-plan.md, "How size is measured").
#
# Usage: scripts/container/run.sh scripts/size-report.sh   (re-execs
#        itself through run.sh when started outside the container)
#        scripts/size-report.sh --attribute   reads demangled symbol
#        names on stdin, prints "<crate>\t<name>" (the attribution rule,
#        tested by scripts/test-size-report.sh)
# Env:   SIZE_REPORT_BUILDS="cpu gpu" (default both; one of them to save
#        a build)
#        SIZE_REPORT_TOP=200 symbols listed per build
#        SIZE_REPORT_CRATES=40 crates listed per build
#        SIZE_REPORT_BASELINE=<dir> a previous run's output directory:
#        each crate row and `.text` also show the delta against it
#        SIZE_REPORT_NOBUILD=1 reports on the binaries a previous run left
#        in SIZE_REPORT_OUT instead of building
#        SIZE_REPORT_OUT=<dir> where the binaries, per-crate TSVs and
#        symbol lists go (default target/container/size-report)
#
# Attribution: symbols in the `.text` section (readelf, by section
# index) are demangled with c++filt and charged to the crate of their
# path's first segment; `<T as Trait>::f` goes to T's crate unless T is
# a std or primitive type (then the trait's crate), and
# `core::ptr::drop_in_place::<T>` (drop glue) and other generic std
# functions instantiated on one workspace or third-party type go to that
# type's crate. Symbols without a Rust path (C, compiler builtins) are
# "[c]"; `.text` bytes no symbol covers (padding, stripped local code)
# are "[unattributed]". Under fat LTO, inlined code is charged to the
# function it was inlined into, so this is a guide to where bytes sit,
# not an exact cost of each crate.

set -euo pipefail

# attribute: stdin demangled names → "<crate>\t<name>".
attribute() {
  awk '
    function strip(s) { gsub(/\[[0-9a-f]+\]/, "", s); return s }
    # The first path segment of s (no leading "<", "&", "*", "dyn ").
    function head(s,   i, c) {
      sub(/^[<&*( []+/, "", s)
      sub(/^(mut |const |dyn |unsafe |extern "[^"]*" )+/, "", s)
      sub(/^[<&*( []+/, "", s)
      i = index(s, "::")
      if (i == 0) return ""
      c = substr(s, 1, i - 1)
      if (c !~ /^[A-Za-z_][A-Za-z0-9_]*$/) return ""
      return c
    }
    function isstd(c) { return c == "" || c == "core" || c == "alloc" || c == "std" }
    # The first non-std crate named in the generic arguments of s.
    function generic(s,   rest, c, i) {
      i = index(s, "<")
      if (i == 0) return ""
      rest = substr(s, i + 1)
      while (match(rest, /[A-Za-z_][A-Za-z0-9_]*::/)) {
        c = substr(rest, RSTART, RLENGTH - 2)
        if (!isstd(c)) return c
        rest = substr(rest, RSTART + RLENGTH)
        # The rest of this path (alloc::string::String) names no crate.
        while (match(rest, /^[A-Za-z_][A-Za-z0-9_]*::/)) rest = substr(rest, RLENGTH + 1)
      }
      return ""
    }
    {
      name = $0
      s = strip($0)
      sub(/::h[0-9a-f]{16}$/, "", s)
      crate = ""
      if (substr(s, 1, 1) == "<") {
        # <T as Trait>::f or <T>::f
        depth = 0; split_at = 0; end = 0
        for (i = 2; i <= length(s); i++) {
          ch = substr(s, i, 1)
          if (ch == "<") depth++
          else if (ch == ">") { if (depth == 0) { end = i; break } depth-- }
          else if (depth == 0 && substr(s, i, 4) == " as " && split_at == 0) split_at = i
        }
        if (split_at > 0) {
          self = substr(s, 2, split_at - 2)
          trait = substr(s, split_at + 4, end - split_at - 4)
        } else {
          self = substr(s, 2, end - 2)
          trait = ""
        }
        crate = head(self)
        if (isstd(crate)) crate = generic(self)
        if (isstd(crate)) crate = head(trait)
        if (isstd(crate)) crate = generic(trait)
        if (crate == "") crate = head(self)
        if (crate == "") crate = head(trait)
      } else {
        crate = head(s)
        if (isstd(crate) && crate != "") {
          g = generic(s)
          if (g != "") crate = g
        }
      }
      if (crate == "") crate = "[c]"
      printf "%s\t%s\n", crate, name
    }'
}

if [ "${1:-}" = --attribute ]; then
  attribute
  exit 0
fi

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
if [ ! -e /.dockerenv ]; then
  # run.sh passes only STRAND_* in: carry SIZE_REPORT_* on the command.
  exec "$ROOT/scripts/container/run.sh" env $(env | grep -E '^SIZE_REPORT_[A-Z]+=' || true) \
    "$ROOT/scripts/size-report.sh" "$@"
fi
cd "$ROOT"

BUILDS=${SIZE_REPORT_BUILDS:-cpu gpu}
TOP=${SIZE_REPORT_TOP:-200}
CRATES=${SIZE_REPORT_CRATES:-40}
OUT=${SIZE_REPORT_OUT:-${CARGO_TARGET_DIR:-target}/size-report}
BASE=${SIZE_REPORT_BASELINE:-}
TARGET=${CARGO_TARGET_DIR:-target}
mkdir -p "$OUT"

# num: awk function turning readelf's decimal or 0x-prefixed (or bare
# hex, with hex=1) numbers into decimal (mawk has no strtonum).
NUM='function num(s, hex,   i, n) { if (s ~ /^0x/) { s = substr(s, 3); hex = 1 } if (!hex) return s + 0; s = tolower(s); n = 0; for (i = 1; i <= length(s); i++) n = n * 16 + index("0123456789abcdef", substr(s, i, 1)) - 1; return n }'

text_size() {
  readelf -SW "$1" | sed 's/^ *\[ *[0-9]*\]//' | awk "$NUM"' $1 == ".text" { printf "%d\n", num($5, 1); exit }'
}

# commas 1234567 → 1,234,567 (negative too).
commas() {
  echo "$1" | sed -E ':a; s/^(-?[0-9]+)([0-9]{3})/\1,\2/; ta'
}

report() {
  local build=$1 bin=$2 dir=$OUT/$1
  mkdir -p "$dir"
  local text
  text=$(text_size "$bin")
  echo "$text" >"$dir/text"
  local idx
  idx=$(readelf -SW "$bin" | sed -n 's/^ *\[ *\([0-9]*\)\] \.text .*/\1/p')
  # size<TAB>mangled name, .text symbols only, one per address (aliases
  # share bytes).
  readelf -sW "$bin" |
    awk -v idx="$idx" "$NUM"' { $3 = num($3) } $7 == idx && $3 > 0 && ($4 == "FUNC" || $4 == "NOTYPE") && !seen[$2]++ { print $3 "\t" $8 }' \
      >"$dir/raw.tsv"
  cut -f2 "$dir/raw.tsv" | c++filt | "$ROOT/scripts/size-report.sh" --attribute >"$dir/attr.tsv"
  # size<TAB>crate<TAB>demangled name
  paste <(cut -f1 "$dir/raw.tsv") "$dir/attr.tsv" | sort -t$'\t' -k1,1nr >"$dir/symbols.tsv"
  awk -F'\t' -v text="$text" '
    { by[$2] += $1; n[$2]++; total += $1 }
    END {
      for (c in by) printf "%d\t%s\t%d\n", by[c], c, n[c]
      if (text > total) printf "%d\t[unattributed]\t0\n", text - total
    }' "$dir/symbols.tsv" | sort -t$'\t' -k1,1nr >"$dir/crates.tsv"

  local label
  case "$build" in
    cpu) label="CPU-only (--no-default-features)" ;;
    gpu) label="GPU (default features)" ;;
  esac
  echo
  echo "## $label"
  echo
  local line=".text: $(commas "$text") B"
  if [ -n "$BASE" ] && [ -f "$BASE/$build/text" ]; then
    local before
    before=$(cat "$BASE/$build/text")
    line="$line (baseline $(commas "$before") B, Δ $(commas $((text - before))) B)"
  fi
  echo "$line"
  echo
  echo "### Crates (top $CRATES)"
  echo
  if [ -n "$BASE" ] && [ -f "$BASE/$build/crates.tsv" ]; then
    echo "| bytes | Δ | % | symbols | crate |"
    echo "|---:|---:|---:|---:|---|"
    awk -F'\t' -v text="$text" -v top="$CRATES" '
      NR == FNR { base[$2] = $1; next }
      FNR <= top { printf "| %d | %+d | %.2f | %d | %s |\n", $1, $1 - base[$2], 100 * $1 / text, $3, $2 }
    ' "$BASE/$build/crates.tsv" "$dir/crates.tsv"
  else
    echo "| bytes | % | symbols | crate |"
    echo "|---:|---:|---:|---|"
    awk -F'\t' -v text="$text" -v top="$CRATES" '
      NR <= top { printf "| %d | %.2f | %d | %s |\n", $1, 100 * $1 / text, $3, $2 }
    ' "$dir/crates.tsv"
  fi
  echo
  echo "### Symbols (top $TOP)"
  echo
  echo "| bytes | crate | symbol |"
  echo "|---:|---|---|"
  awk -F'\t' -v top="$TOP" 'NR <= top {
      name = $3; sub(/::h[0-9a-f]{16}$/, "", name); gsub(/\[[0-9a-f]+\]/, "", name); gsub(/\|/, "\\|", name)
      if (length(name) > 160) name = substr(name, 1, 157) "..."
      printf "| %d | %s | `%s` |\n", $1, $2, name }' "$dir/symbols.tsv"
}

echo "# strand .text size report"
echo
echo "$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown) · $(rustc --version)"
echo "Built as budgets.rs's gate builds it: cargo test --release -p strand --test budgets --no-run."
for build in $BUILDS; do
  case "$build" in
    cpu) features=(--no-default-features) ;;
    gpu) features=() ;;
    *) echo "SIZE_REPORT_BUILDS: $build is not cpu or gpu" >&2; exit 2 ;;
  esac
  if [ "${SIZE_REPORT_NOBUILD:-0}" != 1 ]; then
    echo "building $build" >&2
    cargo test --release -p strand "${features[@]}" --test budgets --no-run >&2
    cp "$TARGET/release/strand" "$OUT/strand-$build"
  fi
  report "$build" "$OUT/strand-$build"
done
