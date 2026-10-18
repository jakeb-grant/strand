# Sourced by scripts/container/run.sh for `run.sh eval local …` alone
# (docs/eval.md, "Local configs and fonts"; docs/decisions.md eval-local).
# Adds to run.sh's `mounts` and `envs`, read-only:
#
# - each config the gitignored eval/scenarios/local/*.scn name by an
#   absolute path (`config = "/…"`, `config_dir = "/…"`, `${VAR}` taken
#   from the host's environment), and each absolute path among the
#   command's own arguments (`local new <id> --config /…`), outside the
#   checkout: its directory, at its own path;
# - the host's font directories (/usr/share/fonts, /usr/local/share/fonts,
#   ~/.local/share/fonts, ~/.fonts, those that exist), at
#   /opt/host-fonts/<n>, with a fontconfig file
#   (target/eval/local/fonts.conf) that adds them to the image's, so a
#   `fonts = "local"` scenario shapes with the fonts the owner's shell
#   finds.
#
# Nothing is copied: the configs and fonts are read where they are.
# Expects ROOT, mounts and envs (run.sh's) and the command's arguments.

local_seen=()
local_mount() { # DIR: mounted read-only at its own path, once
  local d=$1 s
  case "$d" in "$ROOT" | "$ROOT"/*) return ;; esac
  for s in "${local_seen[@]}"; do [ "$s" = "$d" ] && return; done
  local_seen+=("$d")
  mounts+=(-v "$d:$d:ro")
}
local_path() { # PATH (maybe with ${VAR}): its directory mounted
  local p=$1 name
  while [[ "$p" =~ \$\{([A-Za-z_][A-Za-z0-9_]*)\} ]]; do
    name=${BASH_REMATCH[1]}
    p=${p//"\${$name}"/${!name:-}}
  done
  case "$p" in /*) ;; *) return ;; esac
  if [ -d "$p" ]; then
    local_mount "$(cd "$p" && pwd -P)"
  elif [ -f "$p" ]; then
    local_mount "$(cd "$(dirname "$p")" && pwd -P)"
  fi
}

local_dir=$ROOT/eval/scenarios/local
if [ -d "$local_dir" ]; then
  while IFS= read -r -d '' scn; do
    # The header only: up to the first `=== ` line.
    while IFS= read -r line; do
      case "$line" in "=== "*) break ;; esac
      if [[ "$line" =~ ^[[:space:]]*(config|config_dir)[[:space:]]*=[[:space:]]*\"([^\"]*)\" ]]; then
        local_path "${BASH_REMATCH[2]}"
      fi
    done <"$scn"
  done < <(find "$local_dir" -name '*.scn' -print0)
fi
for arg in "$@"; do
  local_path "$arg"
done

local_fonts=()
for d in /usr/share/fonts /usr/local/share/fonts "$HOME/.local/share/fonts" "$HOME/.fonts"; do
  [ -d "$d" ] || continue
  n=${#local_fonts[@]}
  mounts+=(-v "$d:/opt/host-fonts/$n:ro")
  local_fonts+=("/opt/host-fonts/$n")
done
mkdir -p "$ROOT/target/eval/local"
{
  echo '<?xml version="1.0"?>'
  echo '<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">'
  echo '<!-- Written by scripts/eval/local-mounts.sh: the image fontconfig and the host font directories, read-only. -->'
  echo '<fontconfig>'
  echo '  <include ignore_missing="yes">/etc/fonts/fonts.conf</include>'
  for d in "${local_fonts[@]}"; do
    echo "  <dir>$d</dir>"
  done
  echo '</fontconfig>'
} >"$ROOT/target/eval/local/fonts.conf"
envs+=(-e "FONTCONFIG_FILE=$ROOT/target/eval/local/fonts.conf")
if [ "${#local_seen[@]}" -gt 0 ]; then
  echo "eval local: read-only: ${local_seen[*]}; fonts: ${local_fonts[*]:-none}" >&2
fi
