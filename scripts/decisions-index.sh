#!/bin/sh
# Prints docs/decisions.md's tracks, one a line, tab-separated: the heading's
# line, the track, its first and last entry dates, and its first line. The file
# is too long to read through, so this finds a track:
#
#   scripts/decisions-index.sh | grep -i keyboard
#
# An entry starts a line with its date (`**2026-10-05 · …`, `(2026-10-31) …`,
# `2026-10-31. …`); a track with none shows `-`. An argument names another file.
exec awk '
function flush() {
  if (track != "")
    printf "%d\t%s\t%s..%s\t%s\n", start, track, (first == "" ? "-" : first), (last == "" ? "-" : last), line
}
/^```/ { fenced = !fenced }
!fenced && /^## / {
  flush()
  track = substr($0, 4); start = NR; first = ""; last = ""; line = ""
  next
}
track == "" { next }
line == "" && NF {
  line = $0
  gsub(/\*\*/, "", line)
  if (length(line) > 100) line = substr(line, 1, 100) "..."
}
match($0, /^[*(]*[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]/) {
  date = substr($0, RLENGTH - 9, 10)
  if (first == "" || date < first) first = date
  if (date > last) last = date
}
END { flush() }
' "${1:-$(dirname "$0")/../docs/decisions.md}"
