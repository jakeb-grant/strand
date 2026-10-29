#!/usr/bin/env bash
# (report) Tests scripts/report-view.sh and scripts/eval/replay-mounts.sh
# (docs/eval.md, "Replaying a glitch report"; features.md "Glitch
# reports"). Runs inside the container: strand-eval through
# scripts/eval/eval.sh.
#
#   (default)  fixture bundles (eval/tests/replay.rs writes them): the
#              newest replayed by default, <bundle>/replay/ written, a
#              stub opener on PATH that must not run with --no-open and
#              runs without it; what run.sh mounts for `eval replay`;
#              a directory that is no bundle refused; (replay-clip) a
#              clip of the replay (text, command, .zip) and an unknown
#              node refused
#   --e2e      also end to end: `strand run` on a headless sway, a
#              scripted session (a hover, a click on a button, a service
#              update), `strand report`, the replay, fidelity.json
#              with no divergence from the live shell and the verdict
#              VERIFIED; the bundle with one drawn box moved 3 px
#              DRIFTED, naming that node; (replayfix) a session that
#              opens a hover popup over its own trigger and moves on it:
#              the replay shows the popup when the live shell did and
#              takes every pointer step on it; (replayfix) a busy
#              session (four workspaces of windows, a clock, a player,
#              notifications, a title changing) whose every press lands
#              on the chip it did live, with no value lost; (replay-clip)
#              the session's clip, each box the replay's trace's;
#              (fidelity) a session on four headless outputs (two scales
#              besides 1, a bar per screen with its state kept per
#              screen, input on three): every output replayed as a
#              screen of its own, no divergence, VERIFIED on every
#              surface, and its clip naming each surface's output;
#              (services) a session writing service fields from its
#              handlers on a private PipeWire (a mute with the volume
#              beside it, an item's volume, do not disturb twice), from
#              after a recorder off: no divergence (each service write
#              held to the live one), no config write played as a
#              service's change, VERIFIED
#
# Run: scripts/container/run.sh bash scripts/eval/test-report-view.sh [--e2e]

set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target}
e2e=0
[ "${1:-}" = --e2e ] && e2e=1
DIR=$(mktemp -d)
# What a replay leaves in the checkout: its local scenario (and icon
# folder) and its run under target/report-view/.
made=()
cleanup() {
  local n
  for n in "${made[@]}"; do
    rm -rf "eval/scenarios/local/report-$n.scn" "eval/scenarios/local/report-$n.icons" "target/report-view/$n"
  done
  rm -rf "$DIR"
}
trap cleanup EXIT
scenario_name() { basename "$1" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9._-]/-/g; s/^\.*//'; }
bad=0

ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; bad=1; }
need() { # NAME FILE
  [ -f "$2" ] && ok "$1" || no "$1: no $2"
}

export REPORT_VIEW_EVAL="bash $ROOT/scripts/eval/eval.sh"
mkdir -p "$DIR/bin"
cat >"$DIR/bin/xdg-open" <<STUB
#!/usr/bin/env bash
echo "\$@" >>"$DIR/opened"
STUB
chmod +x "$DIR/bin/xdg-open"
export PATH="$DIR/bin:$PATH"

# The fixture bundles: an older and a newer one.
if ! REPORT_VIEW_FIXTURE=$DIR/state cargo test -q --locked --manifest-path eval/Cargo.toml \
  --test replay the_report_view_fixture_bundles_are_written -- --exact >"$DIR/fixture.log" 2>&1; then
  cat "$DIR/fixture.log"
  no "the fixture bundles are written"
  exit 1
fi
newest=$DIR/state/reports/2026-10-20_14-03-07
older=$DIR/state/reports/2026-10-20_10-00-00
made+=("$(scenario_name "$newest")" "$(scenario_name "$older")")

# The newest by default, --no-open.
if STRAND_REPORTS=$DIR/state/reports bash scripts/report-view.sh --no-open >"$DIR/out" 2>&1; then
  ok "report-view runs on the newest bundle"
else
  cat "$DIR/out"
  no "report-view failed on the newest bundle"
fi
grep -q "replay: $newest/replay/replay.txt" "$DIR/out" && ok "it names the replay it wrote" ||
  no "it did not name $newest/replay/replay.txt: $(cat "$DIR/out")"
[ ! -e "$older/replay" ] && ok "the older bundle is left alone" || no "the older bundle was replayed"
for f in index.html replay.txt fidelity.json summary.txt; do
  need "<bundle>/replay/$f is written" "$newest/replay/$f"
done
id=report-2026-10-20_14-03-07
for f in trace.json checks.json scenario.scn; do
  need "<bundle>/replay/<id>/$f is written" "$newest/replay/$id/$f"
done
# (replayfix) A replay's report is packed: its trace frame by frame
# against the one before, its frames patches of the ones before.
grep -q '"packed":{"format":1' "$newest/replay/$id/trace.json" && ok "the replay's trace.json is packed" ||
  no "the replay's trace.json is not packed"
grep -q '"faithful": true' "$newest/replay/fidelity.json" && ok "the fixture replays faithfully" ||
  no "the fixture's replay diverges: $(cat "$newest/replay/fidelity.json")"
grep -q "input press left at 40.0,20.0" "$newest/replay/replay.txt" && ok "replay.txt lists the press" ||
  no "replay.txt lacks the press: $(head -40 "$newest/replay/replay.txt")"
[ ! -e "$DIR/opened" ] && ok "--no-open opens nothing" || no "--no-open ran the opener: $(cat "$DIR/opened")"

# (replay-clip) A clip of the replay: its text, its JSON and its .zip;
# a node no replay holds refused.
if bash scripts/eval/eval.sh replay clip "$newest" --from -22.5s --to -21s --node '#lamp' \
  --note 'the lamp jumps' --save "$DIR/clip.zip" >"$DIR/clip.txt" 2>"$DIR/clip.err"; then
  ok "replay clip runs on the replayed bundle"
else
  cat "$DIR/clip.err"
  no "replay clip failed"
fi
grep -q '^Strand replay clip: "the bar dipped"' "$DIR/clip.txt" && grep -q '^\[1\] box#lamp · shell.strand:' "$DIR/clip.txt" &&
  ok "the clip names the report and the lamp" || no "the clip reads: $(head -20 "$DIR/clip.txt")"
grep -q "^regenerate: strand replay clip '$newest' --from -22.500s --to -21.000s --node 'lamp|" "$DIR/clip.txt" &&
  ok "the clip names the command that makes it again" || no "no regenerate line: $(tail -2 "$DIR/clip.txt")"
python3 -I - "$DIR/clip.zip" <<'PY' && ok "the .zip holds the text, the JSON and the frames" || no "the .zip is not the clip's"
import json, sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
names = z.namelist()
assert z.testzip() is None, "a CRC"
assert names[:2] == ["clip.txt", "clip.json"], names
assert any(n.startswith("frames/k") and n.endswith(".png") for n in names), names
j = json.loads(z.read("clip.json"))
assert j["clip"] == 1 and j["spec"]["note"] == "the lamp jumps", j["spec"]
PY
if bash scripts/eval/eval.sh replay clip "$newest" --from -2s --node '#nothing' >/dev/null 2>"$DIR/clip.err"; then
  no "a clip of a node the replay does not hold was made"
else
  grep -q 'no node "#nothing"' "$DIR/clip.err" && ok "a node the replay does not hold is refused" ||
    no "the refusal does not say why: $(cat "$DIR/clip.err")"
fi

# A bundle named, opened.
if bash scripts/report-view.sh "$older" --traced all >"$DIR/out2" 2>&1; then
  ok "report-view runs on a named bundle"
else
  cat "$DIR/out2"
  no "report-view failed on $older"
fi
need "the named bundle's replay is written" "$older/replay/replay.txt"
grep -q "traced: the whole window" "$older/replay/replay.txt" && ok "flags reach strand-eval replay" ||
  no "--traced all did not reach the replay: $(head -5 "$older/replay/replay.txt")"
for _ in $(seq 50); do [ -s "$DIR/opened" ] && break; sleep 0.1; done
grep -qx "$older/replay/index.html" "$DIR/opened" 2>/dev/null && ok "without --no-open the viewer is opened" ||
  no "the opener was not run on $older/replay/index.html: $(cat "$DIR/opened" 2>/dev/null)"

# (replayfix) --out: the run goes there, the bundle is left as it is.
rm -rf "$newest/replay"
if bash scripts/report-view.sh --no-open "$newest" --out "$DIR/elsewhere" >"$DIR/out4" 2>&1; then
  ok "report-view runs with --out"
else
  cat "$DIR/out4"
  no "report-view failed with --out"
fi
need "--out holds the replay" "$DIR/elsewhere/replay.txt"
[ ! -e "$newest/replay" ] && ok "--out leaves the bundle as it is" || no "--out wrote into the bundle"

# Not a bundle.
mkdir -p "$DIR/empty"
if bash scripts/report-view.sh --no-open "$DIR/empty" >"$DIR/out3" 2>&1; then
  no "a directory with no bundle.json was replayed"
else
  grep -q "not a glitch report" "$DIR/out3" && ok "a directory that is no bundle is refused" ||
    no "the refusal does not say why: $(cat "$DIR/out3")"
fi

# What run.sh mounts for `eval replay`: each directory of files/ that
# holds a file, at its own path, read-only; nothing for the checkout.
B=$newest
mkdir -p "$B/files/opt/wall" "$B/files/opt/empty/deeper" "$B/files$ROOT/inside"
touch "$B/files/opt/wall/sky.png" "$B/files/opt/empty/deeper/x.json" "$B/files$ROOT/inside/y"
mounts=()
# shellcheck source=replay-mounts.sh
source "$HERE/replay-mounts.sh" "$B" --out "$DIR/out-dir" 2>"$DIR/err"
has() {
  local w
  for w in "${mounts[@]}"; do [ "$w" = "$2" ] && { ok "$1"; return; }; done
  no "$1: no \`$2\` in: ${mounts[*]}"
}
has "a copied file's directory is mounted at its own path" "$B/files/opt/wall:/opt/wall:ro"
has "a nested one too" "$B/files/opt/empty/deeper:/opt/empty/deeper:ro"
case " ${mounts[*]} " in *":/opt/empty:ro"*) no "a directory with no file of its own was mounted" ;; *) ok "a directory with no file of its own is not mounted" ;; esac
case " ${mounts[*]} " in *"$ROOT/inside"*) no "a path in the checkout was mounted" ;; *) ok "the checkout is run.sh's already" ;; esac
grep -q "read-only at" "$DIR/err" && ok "it says what it mounted" || no "it printed nothing: $(cat "$DIR/err")"

if [ "$e2e" = 1 ]; then
  keep=$DIR/keep
  if STRAND_REPORT_KEEP=$keep cargo test -q --locked -p strand --test report \
    a_scripted_session_is_kept_for_its_replay -- --exact >"$DIR/e2e.log" 2>&1 && [ -f "$keep/bundle" ]; then
    ok "a session on a headless sway is reported"
    bundle=$(cat "$keep/bundle")
    run_dir=$(cat "$keep/run")
    made+=("$(scenario_name "$bundle")")
    if bash scripts/report-view.sh --no-open "$bundle" >"$DIR/e2e-view" 2>&1; then
      ok "its bundle replays"
      f=$bundle/replay/fidelity.json
      # Its head, for the log: the window, the output, the comparison.
      sed -n '1,/^timeline/p' "$bundle/replay/replay.txt"
      if grep -q '"faithful": true' "$f"; then
        ok "the replay does not diverge from the live shell (hits, surfaces, cell writes, the final frame)"
      else
        no "the replay diverges: $(cat "$f")"
        head -60 "$bundle/replay/replay.txt"
      fi
      grep -q "input press left" "$bundle/replay/replay.txt" && ok "replay.txt lists the live press" ||
        no "replay.txt lacks the press"
      grep -q "service mood.level = 7" "$bundle/replay/replay.txt" && ok "replay.txt lists the service update" ||
        no "replay.txt lacks the service update"
      # The bar's frame was compared, pixel for pixel, not passed over.
      grep -q '"total": [1-9]' "$f" && ok "the final frame was compared with the bundle's PNG" ||
        no "no frame was compared: $(cat "$f")"
      # (replay-owner) The verdict: the live boxes (drawn.json) where
      # the replay drew its nodes at the report.
      head -1 "$bundle/replay/replay.txt" | grep -q '^verdict: VERIFIED: ' &&
        ok "the replay is VERIFIED against the live shell's drawn boxes" ||
        no "the replay is not VERIFIED: $(sed -n '1,/^glitch report/p' "$bundle/replay/replay.txt")"
      # (replay-clip) Its clip: the whole window, each element's boxes,
      # opacity and scale at each frame the replay's trace's; and the
      # command line's text.
      if STRAND_CLIP_REPLAY=$bundle/replay cargo test -q --locked --manifest-path eval/Cargo.toml --test clip \
        a_replayed_session_clips_as_its_trace -- --exact >"$DIR/clip-e2e.log" 2>&1; then
        ok "the session's clip holds the boxes its trace does"
      else
        cat "$DIR/clip-e2e.log"
        no "the session's clip parts from its trace"
      fi
      if bash scripts/eval/eval.sh replay clip "$bundle" --from -1s --save "$DIR/clip-e2e.zip" >"$DIR/clip-e2e.txt" 2>&1 &&
        grep -q '^fidelity:' "$DIR/clip-e2e.txt" && [ -s "$DIR/clip-e2e.zip" ]; then
        ok "replay clip runs on the session's report"
      else
        no "replay clip on the session's report: $(head -20 "$DIR/clip-e2e.txt")"
      fi
      # Its bundle with one box moved 3 px: DRIFTED, naming that node.
      drift=$DIR/drifted/$(basename "$bundle")
      mkdir -p "$DIR/drifted"
      cp -R "$bundle" "$drift"
      rm -rf "$drift/replay"
      made+=("$(scenario_name "$drift")")
      moved=$(python3 -I - "$drift/drawn.json" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
# The first node inside a surface's root (the root's own box is the
# surface's).
for s in d["surfaces"]:
    for n in s["nodes"][1:]:
        n["box"][0] += 3
        json.dump(d, open(p, "w"), indent=1)
        print(n["origin"] + "\t" + ("#" + n["id"] if n.get("id") else ""))
        sys.exit(0)
sys.exit(1)
PY
      ) || no "drawn.json has no node inside a surface to move"
      origin=${moved%%$'\t'*}
      id=${moved#*$'\t'}
      if bash scripts/report-view.sh --no-open "$drift" >"$DIR/drift-view" 2>&1; then
        ok "a replay that drifts still runs"
        head -1 "$drift/replay/replay.txt" | grep -q '^verdict: DRIFTED: ' && ok "the moved box is DRIFTED" ||
          no "the moved box is not DRIFTED: $(head -3 "$drift/replay/replay.txt")"
        grep -F "@$origin" "$drift/replay/replay.txt" | grep -F "$id" | grep -q '3.0 px off' &&
          ok "replay.txt names the moved node ($origin $id) and 3.0 px" ||
          no "no line names $origin $id 3.0 px off: $(sed -n '1,/^glitch report/p' "$drift/replay/replay.txt")"
        grep -q '"verdict": "DRIFTED"' "$drift/replay/fidelity.json" && ok "fidelity.json says DRIFTED" ||
          no "fidelity.json does not say DRIFTED"
      else
        cat "$DIR/drift-view"
        no "the drifted bundle could not be replayed"
      fi
    else
      cat "$DIR/e2e-view"
      no "the session's bundle could not be replayed"
    fi
    rm -rf "$run_dir"
  else
    tail -40 "$DIR/e2e.log"
    no "the scripted session on a headless sway was not reported"
  fi

  # (replayfix) A hover popup opened in the window, over its own
  # trigger, with state kept per screen: the replay shows it when the
  # live shell did and takes the pointer steps on it.
  keep=$DIR/keep-popup
  if STRAND_REPORT_KEEP=$keep cargo test -q --locked -p strand --test report \
    a_popup_session_is_kept_for_its_replay -- --exact >"$DIR/e2e-popup.log" 2>&1 && [ -f "$keep/bundle" ]; then
    ok "a session with a hover popup is reported"
    bundle=$(cat "$keep/bundle")
    run_dir=$(cat "$keep/run")
    made+=("$(scenario_name "$bundle")")
    if bash scripts/report-view.sh --no-open "$bundle" >"$DIR/e2e-popup-view" 2>&1; then
      ok "its bundle replays"
      rep=$bundle/replay
      sed -n '1,/^timeline/p' "$rep/replay.txt"
      popup=$(grep -n 'popup {' "$bundle/config/peek.strand" | head -1 | cut -d: -f1)
      col=$(grep 'popup {' "$bundle/config/peek.strand" | head -1 | sed 's/popup {.*//' | wc -c)
      origin="peek.strand:$popup:$col"
      grep -q "replay surface mapped @$origin" "$rep/replay.txt" &&
        ok "the popup ($origin) shows in the replay" ||
        no "the popup ($origin) never showed in the replay: $(grep -n 'surface' "$rep/replay.txt" | head)"
      steps=$(grep -c "^pointer .* on @$origin\$" "$rep"/report-*/scenario.scn)
      [ "$steps" -ge 2 ] && ok "the replay has $steps pointer steps on the popup" ||
        no "the replay has $steps pointer steps on the popup"
      if python3 -I - "$rep/fidelity.json" <<'PY'
import json, sys
f = json.load(open(sys.argv[1]))
bad = [d for d in f["divergences"] if d["what"] in ("step", "surface")]
for d in bad:
    print("  ", d["t"], d["what"], d["text"])
sys.exit(1 if bad else 0)
PY
      then
        ok "the popup mapped within two frames of the live one, and every step on it was taken"
      else
        no "the popup's replay parts from the live shell (above)"
      fi
      grep -q '"faithful": true' "$rep/fidelity.json" && ok "the popup session replays faithfully" ||
        no "the popup session diverges: $(grep -A3 '"first"' "$rep/fidelity.json")"
      head -1 "$rep/replay.txt" | grep -q '^verdict: VERIFIED: ' &&
        ok "the popup session is VERIFIED against the live shell's drawn boxes" ||
        no "the popup session is not VERIFIED: $(sed -n '1,/^glitch report/p' "$rep/replay.txt")"
    else
      cat "$DIR/e2e-popup-view"
      no "the popup session's bundle could not be replayed"
    fi
    rm -rf "$run_dir"
  else
    tail -40 "$DIR/e2e-popup.log"
    no "the hover popup session on a headless sway was not reported"
  fi

  # (fidelity) A session on four outputs: each a screen of the replay,
  # its input taken there, every surface held to its own. (step-time)
  # A click near a graph column boundary lands in the live column: the
  # replay's `at` step times its change as the live shell kept it.
  keep=$DIR/keep-four
  if STRAND_REPORT_KEEP=$keep cargo test -q --locked -p strand --test report \
    a_four_output_session_is_kept_for_its_replay -- --exact >"$DIR/e2e-four.log" 2>&1 && [ -f "$keep/bundle" ]; then
    ok "a session on four outputs is reported"
    bundle=$(cat "$keep/bundle")
    run_dir=$(cat "$keep/run")
    made+=("$(scenario_name "$bundle")")
    if bash scripts/report-view.sh --no-open "$bundle" >"$DIR/e2e-four-view" 2>&1; then
      ok "its bundle replays"
      rep=$bundle/replay
      sed -n '1,/^timeline/p' "$rep/replay.txt"
      scn=$(ls "$rep"/report-*/scenario.scn | head -1)
      n=$(sed -n '/^screens = \[/,/^\]/p' "$scn" | grep -c '^  { width')
      [ "$n" = 4 ] && ok "the replay has a screen per output" || no "the replay has $n screens: $(head -12 "$scn")"
      grep -q '^screen HEADLESS-' "$scn" && ok "its input is taken on its own output" ||
        no "no \`screen\` step: $(grep -n '^screen\|^press' "$scn" | head)"
      grep -q '"faithful": true' "$rep/fidelity.json" && ok "the four-output session replays faithfully" ||
        no "the four-output session diverges: $(grep -A3 '"first"' "$rep/fidelity.json")"
      if python3 -I - "$rep/fidelity.json" <<'PY'
import json, sys
f = json.load(open(sys.argv[1]))
outs = {o["name"] for o in f["outputs"]}
held = {t.get("output") for t in f["verdict"]["ticks"]}
print("   outputs", sorted(outs), "held on", sorted(o for o in held if o))
sys.exit(0 if len(outs) == 4 and held == outs else 1)
PY
      then
        ok "a surface on every output was held to the live one"
      else
        no "not every output's surfaces were held (above)"
      fi
      head -1 "$rep/replay.txt" | grep -q '^verdict: VERIFIED: ' &&
        ok "the four-output session is VERIFIED against the live shell's drawn boxes" ||
        no "the four-output session is not VERIFIED: $(sed -n '1,/^glitch report/p' "$rep/replay.txt")"
      if bash scripts/eval/eval.sh replay clip "$bundle" --from -1s >"$DIR/clip-four.txt" 2>&1 &&
        grep -q '^  @.* on HEADLESS-[1-4]: ' "$DIR/clip-four.txt"; then
        ok "its clip names each surface's output"
      else
        no "its clip does not name the outputs: $(sed -n '/^fidelity:/,/^elements/p' "$DIR/clip-four.txt" | head -20)"
      fi
      # Each surface is cropped at its own output's scale, within its
      # buffer.
      if bash scripts/eval/eval.sh replay clip "$bundle" --from -1s --json >"$DIR/clip-four.json" 2>"$DIR/clip-four.err" &&
        python3 -I - "$DIR/clip-four.json" "$rep/fidelity.json" <<'PY'
import json, sys
clip = json.load(open(sys.argv[1]))
outs = {o["name"]: o for o in json.load(open(sys.argv[2]))["outputs"]}
bad = []
for c in clip["crop"]:
    o = outs.get(c.get("output"))
    if o is None or abs(c["scale"] - o["scale"]) > 1e-9:
        bad.append((c["surface"], c.get("output"), c["scale"]))
print("   crops", [(c.get("output"), c["scale"], c["rect"]) for c in clip["crop"]])
sys.exit(1 if bad or len(clip["crop"]) < 4 else 0)
PY
      then
        ok "its clip crops each surface at its own output's scale"
      else
        no "its clip crops a surface at another output's scale (above)"
      fi
    else
      cat "$DIR/e2e-four-view"
      no "the four-output session's bundle could not be replayed"
    fi
    rm -rf "$run_dir"
  else
    tail -40 "$DIR/e2e-four.log"
    no "the four-output session on a headless sway was not reported"
  fi

  # (replayfix) A busy shell over its whole window: workspaces with
  # windows that change, a clock, a player and notifications. The
  # changes ring keeps every value, so each press is replayed on the
  # chip it hit live, and no value was lost or dropped.
  keep=$DIR/keep-busy
  if STRAND_REPORT_KEEP=$keep cargo test -q --locked -p strand --test report \
    a_busy_session_is_kept_for_its_whole_window -- --exact >"$DIR/e2e-busy.log" 2>&1 && [ -f "$keep/bundle" ]; then
    ok "a busy session is reported"
    bundle=$(cat "$keep/bundle")
    run_dir=$(cat "$keep/run")
    made+=("$(scenario_name "$bundle")")
    if bash scripts/report-view.sh --no-open "$bundle" >"$DIR/e2e-busy-view" 2>&1; then
      ok "its bundle replays"
      rep=$bundle/replay
      sed -n '1,/^timeline/p' "$rep/replay.txt"
      if python3 -I - "$rep/fidelity.json" <<'PY'
import json, sys
f = json.load(open(sys.argv[1]))
bad = [d for d in f["divergences"] if d["what"] in ("press", "cell", "step")]
for d in bad:
    print("  ", d["t"], d["what"], d["text"])
p, c = f["presses"], f["cell_writes"]
print("   presses", p, "cell writes", c)
sys.exit(1 if bad or p["live"] < 25 or p["live"] != p["replay"] or c["live"] != c["replay"] else 0)
PY
      then
        ok "every press lands on the chip it hit live, and every cell write is the live one"
      else
        no "the busy session's presses part from the live shell (above)"
      fi
      if grep -q "lost from the ring's arena\|over the value cap" "$rep/replay.txt"; then
        no "a value was lost: $(grep "lost from the ring's arena\|over the value cap" "$rep/replay.txt" | head -3)"
      else
        ok "no value was lost from the ring's arena or dropped over the cap"
      fi
      head -1 "$rep/replay.txt" | grep -q '^verdict: VERIFIED: ' &&
        ok "the busy session is VERIFIED against the live shell's drawn boxes" ||
        no "the busy session is not VERIFIED: $(sed -n '1,/^glitch report/p' "$rep/replay.txt")"
    else
      cat "$DIR/e2e-busy-view"
      no "the busy session's bundle could not be replayed"
    fi
    rm -rf "$run_dir"
  else
    tail -40 "$DIR/e2e-busy.log"
    no "the busy session on a headless sway was not reported"
  fi

  # (services) A session whose handlers write service fields (a mute
  # toggled with the volume set beside it, an item's volume, do not
  # disturb toggled twice) and whose sink changes from outside, on a
  # private PipeWire, recorded from after a recorder off: each write is
  # in the changes ring as the change it made, so the replay starts
  # every field where it was and plays each answer on what the write
  # left.
  keep=$DIR/keep-writes
  if STRAND_REPORT_KEEP=$keep cargo test -q --locked -p strand --test report \
    a_service_write_session_is_kept_for_its_replay -- --exact >"$DIR/e2e-writes.log" 2>&1 && [ -f "$keep/bundle" ]; then
    ok "a session writing service fields is reported"
    bundle=$(cat "$keep/bundle")
    run_dir=$(cat "$keep/run")
    made+=("$(scenario_name "$bundle")")
    if bash scripts/report-view.sh --no-open "$bundle" >"$DIR/e2e-writes-view" 2>&1; then
      ok "its bundle replays"
      rep=$bundle/replay
      sed -n '1,/^timeline/p' "$rep/replay.txt"
      if python3 -I - "$rep/fidelity.json" <<'PY'
import json, sys
f = json.load(open(sys.argv[1]))
for d in f["divergences"]:
    print("  ", d["t"], d["what"], d["text"])
p, c, w = f["presses"], f["cell_writes"], f["service_writes"]
print("   presses", p, "cell writes", c, "service writes", w)
sys.exit(1 if f["divergences"] or p["live"] < 6 or p["live"] != p["replay"] or c["live"] != c["replay"]
         or w["live"] < 4 or w["live"] != w["replay"] else 0)
PY
      then
        ok "every write's handler and every answer after it replay as they ran live, each write the live one"
      else
        no "the writes session parts from the live shell (above)"
      fi
      scn=$(ls "$rep"/report-*/scenario.scn | head -1)
      if grep -q '^set notifications\.dnd' "$scn"; then
        no "a config's write is played as a service's change: $(grep -n '^set notifications\.dnd' "$scn" | head -3)"
      else
        ok "a config's write is left to its handler, never played as a service's change"
      fi
      if grep -q 'kept what each write set but not what it replaced' "$rep/replay.txt"; then
        no "a write's place is named unknown: $(grep 'kept what each write set' "$rep/replay.txt" | head -2)"
      else
        ok "no written field's start is guessed"
      fi
      head -1 "$rep/replay.txt" | grep -q '^verdict: VERIFIED: ' &&
        ok "the writes session is VERIFIED against the live shell's drawn boxes" ||
        no "the writes session is not VERIFIED: $(sed -n '1,/^glitch report/p' "$rep/replay.txt")"
    else
      cat "$DIR/e2e-writes-view"
      no "the writes session's bundle could not be replayed"
    fi
    rm -rf "$run_dir"
  else
    tail -40 "$DIR/e2e-writes.log"
    no "the writes session on a headless sway and PipeWire was not reported"
  fi
fi

exit "$bad"
