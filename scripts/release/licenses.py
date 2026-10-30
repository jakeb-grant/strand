"""THIRD-PARTY-LICENSES.txt for the release archive (scripts/release/package.sh).

usage: licenses.py OUT JSON...

Each JSON is cargo-about's `--format json` for one program the archive
holds. Each license text is written once, with every crate under it
from any of them.
"""

import json
import sys


def main(out, paths):
    texts = {}
    for path in paths:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
        for lic in data["licenses"]:
            key = (lic["id"], lic["text"].strip())
            entry = texts.setdefault(key, {"name": lic["name"], "crates": set()})
            for used in lic["used_by"]:
                c = used["crate"]
                entry["crates"].add((c["name"], c["version"], c.get("repository") or ""))
    lines = [
        "Third-party licenses",
        "",
        "Strand's programs (strand, strand-auth, strand-dev, strand-eval) are",
        "built with the crates below, under these licenses. Strand itself is",
        "under MIT or Apache-2.0, at your option (LICENSE-MIT, LICENSE-APACHE).",
        "",
    ]
    for (lid, text), entry in sorted(texts.items(), key=lambda kv: (kv[0][0], sorted(kv[1]["crates"]))):
        lines.append("=" * 78)
        lines.append(f"{entry['name']} ({lid})")
        lines.append("")
        lines.append("Used by:")
        for name, version, repo in sorted(entry["crates"]):
            lines.append(f"  {name} {version}" + (f" ({repo})" if repo else ""))
        lines.append("")
        lines.append(text)
        lines.append("")
    with open(out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2:])
