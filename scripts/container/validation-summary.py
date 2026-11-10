"""What the validation job found (scripts/container/validation.sh), for its log and
the job summary. Reads DIR's junit files and exits 1 unless:

- the proofs hold: the bad call trapped in `proof-core`, was read in `proof-sync`,
  and strand's GPU thread had the layer loaded in both;
- every test of `core-*` and `sync-*` passed, a crash at exit apart (below);
- no message the layer printed in `sync-*` is outside ALLOWED.

A SIGSEGV or SIGABRT after libtest said `test result: ok. 1 passed`, or a hang
there until nextest's timeout, is counted and not failed: the layer's own statics
are torn down at exit while strand's detached GPU thread is still destroying its
device (`Gpu`'s Drop doesn't wait, on purpose), and that race ends in a crash or a
deadlock. A trap (a validation error), or a crash or hang before that line, fails. Self-test: scripts/container/test-validation-summary.sh
"""

import collections
import glob
import os
import re
import sys
import xml.etree.ElementTree as ElementTree

# What the layer may report, exactly, as (ID, submitted access, prior access); each
# with its reason, and a line in decisions.md robustness.
ALLOWED = {
    # vello_gpu makes a mapped vertex buffer for each clear (`create_buffer_init`,
    # label "Clear Rects"). Lavapipe has one memory type, so the allocator can put
    # it where a freed `Layer Intermediate Texture` was, and the layer holds the
    # texture's last transition against it. wgpu and vello_gpu make these calls;
    # strand makes no Vulkan call.
    (
        "SYNC-HAZARD-READ-AFTER-WRITE",
        "SYNC_VERTEX_ATTRIBUTE_INPUT_VERTEX_ATTRIBUTE_READ",
        "SYNC_IMAGE_LAYOUT_TRANSITION",
    ),
}

MESSAGE = re.compile(r"^\s*((?:VUID|SYNC|UNASSIGNED)-[\w.-]+)\(ERROR / ", re.M)
PASSED = "test result: ok. 1 passed"
BAD_CALL = "VUID-VkBufferCreateInfo-size-00912"


def at_exit(kind, out):
    """How a test that passed ended badly at exit: "crash", "hang" or None."""
    if kind is None or PASSED not in out:
        return None
    if "SIGSEGV" in kind or "SIGABRT" in kind:
        return "crash"
    if "timeout" in kind:
        return "hang"
    return None


def signature(out, start):
    """The (ID, submitted, prior) of the message that starts at `start`."""
    match = MESSAGE.match(out, start)
    end = out.find("\n", start)
    line = out[start : end if end != -1 else len(out)]
    used = re.search(r"\((?:submitted_usage|usage): (\w+)", line)
    prior = re.search(r"prior_usage: (\w+)", line)
    return (match.group(1), used.group(1) if used else "", prior.group(1) if prior else "")


def cases(path):
    """Each test of a junit file, as (name, failure type or None, its output)."""
    for suite in ElementTree.parse(path).getroot().findall("testsuite"):
        for case in suite.findall("testcase"):
            bad = case.find("failure")
            if bad is None:
                bad = case.find("error")
            out = "\n".join((case.findtext("system-out") or "", case.findtext("system-err") or ""))
            kind = None if bad is None else f"{bad.get('type') or ''} {bad.get('message') or ''}"
            yield f"{suite.get('name')} {case.get('name')}", kind, out


def proofs(directory, problems):
    for mode, traps in (("proof-core", True), ("proof-sync", False)):
        path = os.path.join(directory, f"{mode}.xml")
        if not os.path.exists(path):
            problems.append(f"{mode}: no results")
            continue
        found = {name.split()[-1]: (kind, out) for name, kind, out in cases(path)}
        bad = found.get("a_bad_vulkan_call_traps")
        layer = found.get("strands_gpu_thread_runs_under_the_layer")
        if bad is None or layer is None:
            problems.append(f"{mode}: its tests did not run")
            continue
        if BAD_CALL not in bad[1]:
            problems.append(f"{mode}: the bad call went unreported: this run can't fail")
        if traps and "SIGTRAP" not in (bad[0] or ""):
            problems.append(f"{mode}: the bad call didn't trap: this run can't fail ({bad[0]})")
        # Passed, then the crash at exit every other test here can have (above).
        if layer[0] is not None and at_exit(*layer) is None:
            problems.append(f"{mode}: strand's GPU thread didn't have the layer ({layer[0]})")


def main(directory):
    problems = []
    proofs(directory, problems)
    counts = collections.Counter()
    seen = collections.Counter()
    exits = {"core": [], "sync": []}
    hangs = {"core": [], "sync": []}
    for mode in ("core", "sync"):
        paths = sorted(glob.glob(os.path.join(directory, f"{mode}-*.xml")))
        if len(paths) != 2:
            problems.append(f"{mode}: {len(paths)} of 2 runs have results")
        for path in paths:
            for name, kind, out in cases(path):
                counts[mode] += 1
                for match in MESSAGE.finditer(out):
                    sign = signature(out, match.start())
                    seen[sign] += 1
                    if sign not in ALLOWED:
                        problems.append(f"{mode}: `{name}`: {' '.join(s for s in sign if s)}")
                if kind is None:
                    continue
                end = at_exit(kind, out)
                if end == "crash":
                    exits[mode].append(name)
                elif end == "hang":
                    hangs[mode].append(name)
                else:
                    problems.append(f"{mode}: `{name}` failed: {kind.strip()[:200]}")
        if not counts[mode]:
            problems.append(f"{mode}: no test ran")

    lines = ["### Vulkan validation on lavapipe\n"]
    version = os.path.join(directory, "layer.version")
    if os.path.exists(version):
        with open(version, encoding="utf-8") as f:
            lines.append(f"Layer {f.read().strip()}.\n")
    lines += ["| pass | tests | crashes at exit | hangs at exit |", "|---|---|---|---|"]
    lines += [
        f"| {m} | {counts[m]} | {len(exits[m])} | {len(hangs[m])} |" for m in ("core", "sync")
    ]
    lines.append("")
    if seen:
        lines += ["| reported | submitted | prior | times | |", "|---|---|---|---|---|"]
        for sign, times in seen.most_common():
            verdict = "allowed (decisions.md robustness)" if sign in ALLOWED else "**failed**"
            lines.append(f"| {sign[0]} | {sign[1]} | {sign[2]} | {times} | {verdict} |")
        lines.append("")
    else:
        lines.append("The layer reported nothing.\n")
    if problems:
        lines += ["**Failed**\n"] + [f"- {p}" for p in problems] + [""]
    else:
        lines.append("Proved first: the bad call trapped in core and was read in sync.\n")
    text = "\n".join(lines) + "\n"
    print(text)
    with open(os.path.join(directory, "summary.md"), "w", encoding="utf-8") as f:
        f.write(text)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(text)
    if os.environ.get("GITHUB_ACTIONS"):
        for problem in problems:
            print(f"::error title=validation::{problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
