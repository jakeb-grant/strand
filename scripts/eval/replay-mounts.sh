# (report) Sourced by scripts/container/run.sh for `run.sh eval replay …`
# alone, after scripts/eval/local-mounts.sh (which mounts the bundle
# itself, an absolute path among the arguments, read-only, and the host
# fonts for `fonts = "local"`). Adds to run.sh's `mounts`, read-only,
# each directory of the bundle's files/ tree that holds a file, at the
# path it was copied from, so the replay reads a wallpaper or a settings
# file where the live shell read it (docs/eval.md, "Replaying a glitch
# report"). Paths inside the checkout are run.sh's already.
# Expects ROOT and mounts (run.sh's) and the command's arguments.

replay_bundle=""
for arg in "$@"; do
  case "$arg" in
    -*) ;;
    *)
      if [ -f "$arg/bundle.json" ]; then
        replay_bundle=$(cd "$arg" && pwd -P)
        break
      fi
      ;;
  esac
done
replay_seen=()
if [ -n "$replay_bundle" ] && [ -d "$replay_bundle/files" ]; then
  while IFS= read -r -d '' d; do
    at=${d#"$replay_bundle/files"}
    [ -n "$at" ] || continue
    case "$at" in "$ROOT" | "$ROOT"/*) continue ;; esac
    if [ -n "$(find "$d" -maxdepth 1 -type f -print -quit)" ]; then
      mounts+=(-v "$d:$at:ro")
      replay_seen+=("$at")
    fi
  done < <(find "$replay_bundle/files" -type d -print0 | sort -z)
fi
if [ "${#replay_seen[@]}" -gt 0 ]; then
  echo "eval replay: the bundle's files read-only at: ${replay_seen[*]}" >&2
fi
