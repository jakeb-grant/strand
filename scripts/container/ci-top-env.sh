#!/usr/bin/env bash
# The names in a workflow's top-level env: block, one per line.
# Usage: ci-top-env.sh WORKFLOW.yml
awk '/^env:/ { on = 1; next } /^[^ #]/ { on = 0 } on && /^  [A-Z_0-9]+:/ { sub(/^  /, ""); sub(/:.*/, ""); print }' "$1"
