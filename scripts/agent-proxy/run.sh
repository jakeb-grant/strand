#!/usr/bin/env bash
# run.sh OUT: starts the proxy session in OUT (made by prepare.sh) with
# PROMPT.md as its only instruction: AGENT_PROXY_CMD, a command that
# takes the prompt on stdin and runs in OUT (for example `claude -p`
# with its tools limited to OUT). Its transcript goes to OUT/session.log.
# Never run by CI or by an agent on its own: a rehearsal is started by
# a person (docs/m5-plan.md).
set -euo pipefail
OUT=${1:?usage: run.sh OUT}
CMD=${AGENT_PROXY_CMD:?set AGENT_PROXY_CMD to the session command}
[ -f "$OUT/PROMPT.md" ] || { echo "run.sh: $OUT has no PROMPT.md (prepare.sh)" >&2; exit 2; }
cd "$OUT"
date -u +%s > started
# shellcheck disable=SC2086
$CMD < PROMPT.md 2>&1 | tee session.log
