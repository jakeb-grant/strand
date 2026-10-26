"""The playground's examples (site/assets/wasm/examples.json) from the
docs scenarios they are drawn from (eval/scenarios/docs/*.scn): each
one's config, `[services]`, screen and steps, so the page and eval run
the same text. Usage: python3 -I examples.py OUT.json SCN..."""

import json
import sys
import tomllib
from pathlib import Path


def read(path):
    text = Path(path).read_text()
    head, _, rest = text.partition("=== shell.strand\n")
    config, _, steps = rest.partition("=== steps\n")
    meta = tomllib.loads(head)
    services = {k: v for k, v in meta.get("services", {}).items()}
    return {
        "id": meta["id"],
        "screen": meta["screen"],
        "services": services,
        "config": config.rstrip("\n") + "\n",
        "steps": [s.strip() for s in steps.splitlines() if s.strip()],
    }


def main():
    out, *scns = sys.argv[1:]
    examples = {Path(s).stem: read(s) for s in scns}
    Path(out).write_text(json.dumps(examples, indent=1) + "\n")


if __name__ == "__main__":
    main()
