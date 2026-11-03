"""What a stress run found (scripts/container/stress.sh), for its log and the job
summary: each test that failed with its rate and where, what each block ran, and
the wall-clock gates missed under the load, apart, as they say nothing there.

Reads DIR/blocks.tsv and DIR/block-*.xml; writes DIR/summary.md, adds it to
$GITHUB_STEP_SUMMARY and names each failure in an annotation under GITHUB_ACTIONS.
Exits 1 when a test failed twice or more, a test failed other than by its own
panic (a crash, a timeout), a block didn't run or no test ran, so a run that never
happened can't pass as a clean one. A test that panicked once in the night is a
WARN with its rate: at this load a long night always has one somewhere, and a
night red for that every time is a night nobody reads (decisions.md robustness). Self-test: scripts/container/test-stress-summary.sh
"""

import csv
import glob
import os
import re
import sys
import xml.etree.ElementTree as ElementTree

GATE_MARK = "timing gate missed"


def is_gate_miss(text):
    """Every panic in the failure's output carries the gate marker, and there is
    one: as gate-misses.sh judges a log, so a crash or a timeout is no miss."""
    lines = text.splitlines()
    panics = [i for i, line in enumerate(lines) if re.match(r"thread .* panicked at ", line)]
    return bool(panics) and all(
        i + 1 < len(lines) and lines[i + 1].startswith(GATE_MARK) for i in panics
    )


def is_own_panic(text):
    """The test failed by its own panic, and nothing killed it or ran it out of
    nextest's time (a test's own "timed out waiting" is its own panic)."""
    return bool(re.search(r"^thread .* panicked at ", text, re.M)) and not re.search(
        r"aborted with signal|\btest timed out\b|\bTIMEOUT\b", text
    )


def read(directory):
    """Returns ({(binary, test): record}, blocks, runs in total)."""
    with open(os.path.join(directory, "blocks.tsv"), encoding="utf-8") as f:
        blocks = {row["block"]: row for row in csv.DictReader(f, delimiter="\t")}
    tests = {}
    runs = 0
    for path in sorted(glob.glob(os.path.join(directory, "block-*.xml"))):
        block = re.search(r"block-(\d+)-", os.path.basename(path)).group(1)
        load = blocks.get(block, {})
        where = f"-j {load.get('jobs', '?')} beside {load.get('spinners', '?')}"
        for suite in ElementTree.parse(path).getroot().findall("testsuite"):
            binary = re.sub(r"@stress-\d+$", "", suite.get("name"))
            for case in suite.findall("testcase"):
                runs += 1
                record = tests.setdefault(
                    (binary, case.get("name")),
                    {"ran": 0, "failed": 0, "panics": 0, "gates": 0, "where": {}, "first": ""},
                )
                record["ran"] += 1
                bad = case.find("failure")
                if bad is None:
                    bad = case.find("error")
                if bad is None:
                    continue
                text = "\n".join((bad.get("message") or "", bad.text or ""))
                if is_gate_miss(bad.text or ""):
                    record["gates"] += 1
                    continue
                record["failed"] += 1
                record["panics"] += is_own_panic(text)
                record["where"][where] = record["where"].get(where, 0) + 1
                if not record["first"]:
                    record["first"] = " ".join(text.split())[:300]
    return tests, blocks, runs


def rate(part, whole):
    return f"{part}/{whole} ({100 * part / whole:.1f}%)"


def report(tests, blocks, runs):
    failed = sorted(
        ((k, r) for k, r in tests.items() if r["failed"]),
        key=lambda item: -item[1]["failed"] / item[1]["ran"],
    )
    red = [(k, r) for k, r in failed if r["failed"] >= 2 or r["panics"] < r["failed"]]
    gates = sorted((k, r) for k, r in tests.items() if r["gates"])
    broken = [row for row in blocks.values() if row["status"] != "0"]
    seconds = sum(int(row["seconds"]) for row in blocks.values())
    out = [
        "### Stress run\n",
        f"{runs} test runs of {len(tests)} tests in {len(blocks)} blocks, "
        f"{seconds / 60:.0f} min: {len(failed)} tests failed ({len(red)} twice or more "
        f"or by a crash or timeout, the rest once), {len(gates)} missed a wall-clock gate.\n",
    ]
    if failed:
        out += [
            "**Failed** (once by a panic: WARN; twice or more, or a crash or timeout: fails)\n",
            "| test | failed | under | first failure |",
            "|---|---|---|---|",
        ]
        for (binary, name), r in failed:
            under = ", ".join(f"{w}: {n}" for w, n in sorted(r["where"].items()))
            first = r["first"].replace("|", "\\|")
            out.append(f"| `{binary} {name}` | {rate(r['failed'], r['ran'])} | {under} | {first} |")
        out.append("")
    if gates:
        out += ["**Gates missed under the load (not failures)**\n", "| test | missed |", "|---|---|"]
        out += [f"| `{b} {n}` | {rate(r['gates'], r['ran'])} |" for (b, n), r in gates]
        out.append("")
    if broken:
        out += ["**Blocks that didn't run**\n"]
        out += [f"- block {row['block']}: nextest exited {row['status']}" for row in broken]
        out.append("")
    out += [
        "**Blocks**\n",
        "| block | tier | -j | spinners | partitions | seconds |",
        "|---|---|---|---|---|---|",
    ]
    out += [
        f"| {r['block']} | {r['tier']} | {r['jobs']} | {r['spinners']} | {r['partitions']} | {r['seconds']} |"
        for r in blocks.values()
    ]
    return "\n".join(out) + "\n", failed, red, broken


def main(directory):
    tests, blocks, runs = read(directory)
    text, failed, red, broken = report(tests, blocks, runs)
    print(text)
    with open(os.path.join(directory, "summary.md"), "w", encoding="utf-8") as f:
        f.write(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    if os.environ.get("GITHUB_ACTIONS"):
        for (binary, name), r in failed:
            kind = "error" if ((binary, name), r) in red else "warning"
            print(f"::{kind} title=stress: {name}::{binary} {name} failed {rate(r['failed'], r['ran'])}")
    if not runs:
        print("stress: no test ran", file=sys.stderr)
    return 1 if red or broken or not runs else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
