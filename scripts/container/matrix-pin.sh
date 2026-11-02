#!/usr/bin/env bash
# The date Dockerfile.matrix is pinned to, which must not go stale unseen: pushes
# test a compositor set that no longer exists anywhere once it has aged, and an old
# keyring eventually can't verify anything newer.
#
# Usage: matrix-pin.sh check            says the pin's age, in the job summary too;
#                                       fails past MAX_DAYS. ci.yml's compositors
#                                       job runs it nightly, so past that the night
#                                       goes red and files the nightly-red issue;
#                                       no push waits on it. MATRIX_PIN_NOTE names
#                                       a file for the same words, which the
#                                       issue quotes.
#        matrix-pin.sh refresh [DATE]   rewrites the date (default yesterday, UTC,
#                                       the newest the archive surely has) and the
#                                       digest of today's archlinux:latest. Then run
#                                       scripts/container/matrix.sh and push: a
#                                       Dockerfile.matrix change publishes the image
#                                       and the compositors job proves it. A night
#                                       green on today's packages says it should pass.
# Self-test: scripts/container/test-matrix-pin.sh

set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
FILE=${MATRIX_PIN_FILE:-$HERE/Dockerfile.matrix}
MAX_DAYS=30

pinned() { sed -n 's/^ARG ARCH_DATE=//p' "$FILE"; }

case "${1:-}" in
  check)
    date=$(pinned)
    [ -n "$date" ] || { echo "no ARG ARCH_DATE in $FILE" >&2; exit 2; }
    days=$((($(date -u +%s) - $(date -u -d "$date" +%s)) / 86400))
    line="Pushes test the Arch packages of $date, $days days ago (at most $MAX_DAYS)."
    echo "$line"
    [ -z "${GITHUB_STEP_SUMMARY:-}" ] || { echo; echo "$line"; } >>"$GITHUB_STEP_SUMMARY"
    [ -z "${MATRIX_PIN_NOTE:-}" ] || { mkdir -p "$(dirname "$MATRIX_PIN_NOTE")"; echo "$line" >"$MATRIX_PIN_NOTE"; }
    if [ "$days" -gt "$MAX_DAYS" ]; then
      echo "::error title=compositor pin::the Arch pin is $days days old: run scripts/container/matrix-pin.sh refresh"
      exit 1
    fi
    ;;
  refresh)
    date=${2:-$(date -u -d yesterday +%F)}
    date -u -d "$date" +%F >/dev/null
    curl --proto '=https' --tlsv1.2 -sSfI --max-time 60 \
      "https://archive.archlinux.org/repos/$(tr - / <<<"$date")/core/os/x86_64/core.db" >/dev/null ||
      { echo "the archive has no snapshot of $date" >&2; exit 1; }
    docker pull -q archlinux:latest >/dev/null
    digest=$(docker image inspect --format '{{index .RepoDigests 0}}' archlinux:latest)
    digest=${digest#*@}
    case "$digest" in sha256:*) ;; *) echo "no digest for archlinux:latest: $digest" >&2; exit 1 ;; esac
    sed -i -e "s/^FROM archlinux@.*/FROM archlinux@$digest/" -e "s/^ARG ARCH_DATE=.*/ARG ARCH_DATE=$date/" "$FILE"
    echo "pinned to $date, base $digest"
    echo "next: scripts/container/matrix.sh, then commit and push"
    ;;
  *)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
