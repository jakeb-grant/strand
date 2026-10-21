#!/usr/bin/env bash
# (report) Replays a glitch report in the evaluation sandbox (design.md,
# "Reporting a glitch"; docs/eval.md, "Replaying a glitch report"): the
# agents' container path to what the owner runs as `strand replay`. Runs
# `strand-eval replay <bundle>` in the container (scripts/container/run.sh
# mounts the bundle read-only and each directory of its files/ tree at
# the path it was copied from: scripts/eval/replay-mounts.sh), copies the
# run into <bundle>/replay/ (index.html, replay.txt, fidelity.json,
# summary.txt, and <id>/ with trace.json, checks.json, scenario.scn and
# the frames) and opens the viewer unless --no-open. Agents and tests
# always pass --no-open.
#
# Usage: scripts/report-view.sh [--no-open] [bundle-dir] [--output NAME]
#                               [--traced SECONDS|all] [--frames all|failing|none]
#        (default bundle: the newest under $STRAND_REPORTS, else
#        ${XDG_STATE_HOME:-~/.local/state}/strand/reports)
# Env:   REPORT_VIEW_EVAL  the command that runs strand-eval's commands
#        (default: scripts/container/run.sh eval; inside the container,
#        scripts/eval/eval.sh)
#
# Exits non-zero only when the replay could not run: what it found is the
# point, not a failure.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
open=1
bundle=""
extra=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-open) open=0 ;;
    -h | --help) sed -n '2,22p' "$0"; exit 0 ;;
    --output | --traced | --frames)
      [ "$#" -ge 2 ] || { echo "report-view: $1 needs a value" >&2; exit 2; }
      extra+=("$1" "$2")
      shift
      ;;
    -*) echo "report-view: unknown option $1" >&2; exit 2 ;;
    *)
      [ -z "$bundle" ] || { echo "report-view: one bundle at a time ($bundle, $1)" >&2; exit 2; }
      bundle=$1
      ;;
  esac
  shift
done

if [ -z "$bundle" ]; then
  reports=${STRAND_REPORTS:-${XDG_STATE_HOME:-$HOME/.local/state}/strand/reports}
  # Bundle names start with their time, so the last by name is the newest.
  for d in "$reports"/*/; do
    [ -f "$d/bundle.json" ] && bundle=${d%/}
  done
  [ -n "$bundle" ] || { echo "report-view: no glitch report under $reports (save one with \`strand report <words>\`)" >&2; exit 2; }
fi
[ -f "$bundle/bundle.json" ] || { echo "report-view: $bundle is not a glitch report (no bundle.json)" >&2; exit 2; }
bundle=$(cd "$bundle" && pwd -P)
name=$(basename "$bundle")

read -r -a eval_cmd <<<"${REPORT_VIEW_EVAL:-$ROOT/scripts/container/run.sh eval}"
out=$ROOT/target/report-view/$name
rm -rf "$out"
mkdir -p "$out"
echo "report-view: replaying $bundle" >&2
# The same subcommand `strand replay` hands off to, into the checkout
# (the bundle is mounted read-only), on both legs as CI's image can, the
# viewer opened here rather than in the container.
if ! "${eval_cmd[@]}" replay "$bundle" --out "$out" --no-open --gpu "${extra[@]}"; then
  echo "report-view: the replay of $bundle could not run" >&2
  exit 1
fi

rm -rf "$bundle/replay"
mkdir -p "$bundle/replay"
cp -R "$out"/. "$bundle/replay/"
chmod -R go-rwx "$bundle/replay"
echo "replay: $bundle/replay/replay.txt"
echo "viewer: $bundle/replay/index.html"
if [ "$open" = 1 ]; then
  if command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$bundle/replay/index.html" >/dev/null 2>&1 &
  else
    echo "report-view: no xdg-open; open $bundle/replay/index.html" >&2
  fi
fi
