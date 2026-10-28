#!/usr/bin/env bash
# prepare.sh [OUT]: a workspace for one agent-proxy rehearsal of the
# newcomer exit (features.md, M5: "Newcomer builds a multi-monitor bar
# in 15 minutes", layer 2; advisory, gates nothing).
#
# OUT (default target/agent-proxy/<UTC time>, inside the checkout so the
# container sees it) gets:
#   guide/       the published guide (docs/guide/ without the scorer's
#                checklist), and nothing else of the repository;
#   config/      the empty config directory the session builds in;
#   home/        the session's $HOME, whose .config/strand is config/, so
#                `strand new` and a bare `strand check` or `strand fmt`
#                act on config/;
#   bin/strand   `strand check`, `strand fmt` and `strand new` through the
#                container's debug build, run in the caller's directory
#                with HOME and XDG_CONFIG_HOME under home/ (`strand run`
#                is the scorer's: the session has no compositor, and the
#                two headless outputs are score.sh's);
#   PROMPT.md    the task, given to the session as its only instruction.
# Then start a fresh model session in OUT with PROMPT.md and no other
# context (scripts/agent-proxy/run.sh does, with AGENT_PROXY_CMD), and
# score its config with scripts/agent-proxy/score.sh OUT. The wall clock
# runs from this script's end (OUT/started) to the score.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=${1:-$ROOT/target/agent-proxy/$(date -u +%Y%m%dT%H%M%SZ)}
case "$OUT" in
  "$ROOT"/*) ;;
  *) echo "prepare.sh: $OUT must be inside $ROOT (the container mounts only the checkout)" >&2; exit 2 ;;
esac
[ ! -e "$OUT" ] || { echo "prepare.sh: $OUT exists" >&2; exit 2; }

mkdir -p "$OUT/guide" "$OUT/config" "$OUT/bin" "$OUT/home/.config"
ln -s "$OUT/config" "$OUT/home/.config/strand"
cp -r "$ROOT/docs/guide/." "$OUT/guide/"
rm -f "$OUT/guide/target-checklist.md"

"$ROOT/scripts/container/run.sh" cargo build -q -p strand --bin strand
cat > "$OUT/bin/strand" <<WRAP
#!/usr/bin/env bash
# The container's strand, for the commands that need no compositor. The
# container mounts only the checkout and starts in its root with a
# throwaway HOME, so this runs strand in the caller's directory with
# HOME and XDG_CONFIG_HOME in $OUT/home: strand new writes into
# $OUT/config, and relative paths resolve where the session stands.
case "\${1:-}" in
  check|fmt|new|help|--help|-h) ;;
  *) echo "strand \${1:-}: not available here; this rehearsal runs your shell when it is scored" >&2; exit 2 ;;
esac
case "\$PWD/" in
  "$ROOT"/*) ;;
  *) echo "strand: run it from inside $ROOT (the container mounts only the checkout)" >&2; exit 2 ;;
esac
exec "$ROOT/scripts/container/run.sh" env HOME="$OUT/home" XDG_CONFIG_HOME="$OUT/home/.config" \\
  bash -c 'cd "\$1" && shift && exec "\$@"' bash "\$PWD" "$ROOT/target/container/debug/strand" "\$@"
WRAP
chmod +x "$OUT/bin/strand"

cat > "$OUT/PROMPT.md" <<TASK
You are trying Strand, a toolkit for Wayland desktop shells, for the
first time. Your only documentation is the guide in \`$OUT/guide/\`;
start at \`guide/README.md\`. Do not read anything else about Strand.

Build the bar that levels 1 to 3 of the guide end with (design.md's
bar (a)): on every monitor, workspace dots, the focused window's title,
a clock that opens a calendar, a volume control, the battery and the
tray. Write your config in \`$OUT/config/\` (that is your
\`~/.config/strand\`). \`$OUT/bin/strand check $OUT/config\` checks it,
and \`$OUT/bin/strand fmt\` formats it; there is no compositor here, so
\`strand run\` is not available: your shell is run and scored on two
monitors after you finish.

Say "done" when the bar is finished.
TASK

date -u +%s > "$OUT/started"
echo "$OUT"
