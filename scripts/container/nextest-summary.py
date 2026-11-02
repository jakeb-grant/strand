"""What a nextest run's junit file says, for the log and GitHub's job summary:
the counts, each failure, and each flaky test, a test that failed and then
passed on its retry. A retry must never hide a failure, so a flaky result is
named here on every run. `nextest-summary.sh` runs this; see it for the exit
status."""

import os
import sys
import xml.etree.ElementTree as ElementTree


def main(path, summary):
    root = ElementTree.parse(path).getroot()
    total = 0
    failed = []
    flaky = []
    for suite in root.findall("testsuite"):
        for case in suite.findall("testcase"):
            total += 1
            name = f"{suite.get('name')} {case.get('name')}"
            if case.find("failure") is not None or case.find("error") is not None:
                failed.append(name)
            elif case.find("flakyFailure") is not None or case.find("flakyError") is not None:
                flaky.append(name)
    line = f"nextest: {total} tests, {len(failed)} failed, {len(flaky)} flaky"
    print(line)
    for name in flaky:
        print(f"FLAKY (passed on its retry): {name}")
    if summary:
        with open(summary, "a", encoding="utf-8") as out:
            out.write(f"### Workspace tests\n\n{line}\n\n")
            for title, names in (("Failed", failed), ("Flaky: failed, then passed on the retry", flaky)):
                if names:
                    out.write(f"**{title}**\n\n")
                    out.writelines(f"- `{name}`\n" for name in names)
                    out.write("\n")
    return total


if __name__ == "__main__":
    sys.exit(0 if main(sys.argv[1], os.environ.get("GITHUB_STEP_SUMMARY")) else 1)
