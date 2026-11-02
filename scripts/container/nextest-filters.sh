#!/usr/bin/env bash
# Holds .config/nextest.toml's filters to the tests there are, which nextest
# itself doesn't: a filter matching nothing is silently no filter, so a renamed
# binary would lose its group or its retries with nothing said. Fails when
#   - any override's filter matches no test, or a retry names a binary or a test
#     whose source isn't there;
#   - a test with a retry is in a binary that may never have one (a budget, a
#     pixel reference, logic), or holds a timing gate or a wake, idle or unread
#     count in a binary that may, read from the binary's source and the modules
#     it includes, by its name, its comments and messages and a `quiet_for`
#     asserted zero (decisions.md robustness);
#   - a retry's filter isn't of the one form this can read: anchored
#     `binary_id(/^crate::name$/)` or `binary_id(/^crate::(a|b)$/)` terms joined
#     by `|`, then at most `& not (test(=name) | ...)`;
#   - a profile but `ci` has retries of its own, the stress job's above all,
#     whose every failure counts towards a rate;
#   - the `nightly` job names nextest.
# Runs after the build in CI's `test` job and ci.sh. Needs cargo-nextest, so the
# test binaries; static checks only with --static.
# Self-test: scripts/container/test-nextest-filters.sh

set -uo pipefail
cd "$(dirname "$0")/../.."

CONFIG=${NEXTEST_FILTERS_CONFIG:-.config/nextest.toml}
export NEXTEST_FILTERS_CRATES=${NEXTEST_FILTERS_CRATES:-crates}
WORKFLOW=${NEXTEST_FILTERS_WORKFLOW:-.github/workflows/ci.yml}
bad=0
no() { echo "nextest-filters: $*"; bad=1; }

# The overrides' filters, one line each, as `profile<TAB>filter`, and what the
# retries break as `!<TAB>complaint`.
overrides=$(python3 -I - "$CONFIG" <<'PY'
import os, re, sys, tomllib

# Binaries a retry may never reach. `strand-render` and the compiler's are logic
# and pixels; `demo`, `budgets`, `acceptance` and `services` hold memory or idle
# gates; `m5` holds the newcomer's references and its gate; `*_idle` and `idle`
# are wake counts; `reloads` is its own gate.
FORBIDDEN = re.compile(
    r"^strand-(render|compiler|scene|core|text|theme|gpu|site|web|dev|fuzzy|icons)::"
    r"|::(budgets|demo|acceptance|services|m5|reloads|gpu_idle|presented_quiet"
    r"|dbus_idle|audio_idle|idle|.*bench.*)$"
)
# What a test may not hold under a retry: a marked gate, or a count of wakes,
# of work done idle or unread, or of commits, by the words of its message or a
# comment in it, or a stream's quiet window asserted to be zero. Words and
# shapes, which is all this can read: it catches what looks like those it was
# made from, and the list itself came from reading them (decisions.md robustness).
HOLDS = re.compile(
    r'Gates::default|GATE_MISS'
    r'|"[^"]*\b(?:while idle|while unread|while nobody|woke|wakes|commits for|frames within)\b'
    r'|"[^"]*\b(?:[Nn]obody reads|for nobody|nothing asked before|not read)\b'
    r'|\.quiet_for\(.*\)\.await,\s*(?:0\b.*)?$'
)
# The same in a test's own comment, doc comments aside: they say what a test is
# about, and belong to the next function.
UNREAD = re.compile(r'^\s*//(?!/).*(?:\b(?:[Nn]obody|[Nn]othing) reads\b|\bIdle: )')
# What a test says of itself in its name.
NAMED = re.compile(r'only_while|only_when|while_read|while_watched|while_visible|while_unread|without_polling')
TERM = r"binary_id\(/\^([\w-]+)::(?:\(([\w|-]+)\)|([\w-]+))\$/\)"
HEAD = re.compile(rf"{TERM}(?: \| {TERM})*")
TAIL = re.compile(r"\(test\(=\w+\)(?: \| test\(=\w+\))*\)")
FN = re.compile(r"^(?:pub(?:\(\w+\))? )?(?:async )?fn (\w+)")


CRATES = os.environ["NEXTEST_FILTERS_CRATES"]
MOD = re.compile(r"^\s*(?:pub(?:\([\w ]+\))? )?mod (\w+);")
PATH = re.compile(r'^\s*#\[path = "([^"]+)"\]')


def source(binary):
    crate, name = binary.split("::")
    for path in (f"{CRATES}/{crate}/tests/{name}.rs", f"{CRATES}/{crate}/tests/{name}/main.rs"):
        if os.path.exists(path):
            return path
    return None


def included(path, top=True):
    """Every file `path` includes by `mod` or `#[path]`, as rustc finds them,
    and the `mod`s it can't find."""
    base = os.path.dirname(path)
    stem = os.path.basename(path)[: -len(".rs")]
    # A file that isn't a root or a mod.rs holds its modules in its own directory.
    below = base if top or stem == "mod" else os.path.join(base, stem)
    found, lost, named = [], [], None
    for line in open(path):
        if line.lstrip().startswith("//"):
            continue
        m = PATH.match(line)
        if m:
            named = m.group(1)
            continue
        m = MOD.match(line)
        if m:
            name = m.group(1)
            if named:
                # A `#[path]` reads from the file's own directory, and its
                # file takes modules from beside it, as a root does.
                tries = [os.path.join(base, named)]
            else:
                tries = [os.path.join(below, f"{name}.rs"), os.path.join(below, name, "mod.rs")]
            hit = next((t for t in tries if os.path.exists(t)), None)
            if hit:
                found.append((os.path.normpath(hit), bool(named)))
            else:
                lost.append(f"{path}: mod {name}")
        if line.strip() and not line.lstrip().startswith("#["):
            named = None
    return found, lost


def sources(path):
    """`path` and everything it includes, and the `mod`s nowhere to be found."""
    seen, lost, queue = [], [], [(path, True)]
    while queue:
        at, top = queue.pop()
        if at in seen:
            continue
        seen.append(at)
        found, gone = included(at, top)
        lost += gone
        queue += found
    return seen, lost


def holders(path):
    """Yields (what holds it, is it a test) for each line holding a gate."""
    lines = open(path).read().split("\n")
    current = attrs = None
    pending = ""
    for line in lines:
        if UNREAD.search(line):
            yield current, bool(attrs and "test" in attrs)
        code = line.split("//")[0] if line.lstrip().startswith("//") else line
        if code.startswith("#["):
            pending += code
        m = FN.match(code)
        if m:
            current, attrs, pending = m.group(1), pending, ""
            if "test" in attrs and NAMED.search(current):
                yield current, True
        elif not code.startswith("#[") and code and not code[0].isspace() and code[0] != "}":
            pending = ""
        if HOLDS.search(code):
            yield current, bool(attrs and "test" in attrs)


def retried(profile, filt):
    if " & not " in filt:
        head, tail = filt.split(" & not ", 1)
        if not TAIL.fullmatch(tail):
            return [f"can't read what the retry leaves out: {tail}"]
        kept_out = set(re.findall(r"test\(=(\w+)\)", tail))
    else:
        head, kept_out = filt, set()
    if head.startswith("(binary_id"):
        head = head[1:-1]
    if not HEAD.fullmatch(head):
        return [f"can't read the retry's filter, which so holds nothing: {filt}"]
    out, heard = [], set()
    for term in re.finditer(TERM, head):
        crate, several, one = term.groups()
        for name in several.split("|") if several else [one]:
            binary = f"{crate}::{name}"
            if FORBIDDEN.search(binary):
                out.append(f"the retry filter names {binary}, which may not be retried")
                continue
            path = source(binary)
            if path is None:
                out.append(f"the retry filter names {binary}, which has no source")
                continue
            files, lost = sources(path)
            for gone in lost:
                out.append(f"{binary}: can't find {gone}, so can't read it for what a retry may not reach")
            for file in files:
                text = open(file).read()
                heard |= kept_out & set(re.findall(r"fn (\w+)", text))
                for fn, is_test in holders(file):
                    if not is_test:
                        out.append(f"{binary}: {fn or 'a line'} in {file}, outside a test, holds a gate or a wake count, under a retry")
                    elif fn not in kept_out:
                        out.append(f"{binary}::{fn} holds a gate or a wake count, and has a retry")
    for name in sorted(kept_out - heard):
        out.append(f"the retry leaves out {name}, in none of its binaries")
    return out


with open(sys.argv[1], "rb") as f:
    config = tomllib.load(f)
for profile, body in config["profile"].items():
    if profile != "ci" and body.get("retries", 0) != 0:
        print(f"!\tprofile {profile} has retries; only ci may")
    if body.get("retries", 0) > 1:
        print(f"!\tretries = {body['retries']}; at most one")
    for o in body.get("overrides", []):
        filt = " ".join(o["filter"].split())
        count = o.get("retries", 0)
        print(f"{profile}\t{filt}")
        if count == 0:
            continue
        if profile != "ci":
            print(f"!\tprofile {profile} has retries; only ci may")
        if count != 1:
            print(f"!\tretries = {count}; at most one")
        for complaint in retried(profile, filt):
            print(f"!\t{complaint}")
PY
) || { echo "nextest-filters: can't read $CONFIG"; exit 1; }
[ -n "$overrides" ] || no "$CONFIG has no overrides"

while IFS=$'\t' read -r profile filter; do
  [ -n "$profile" ] || continue
  if [ "$profile" = '!' ]; then no "$filter"; continue; fi
  [ "${1:-}" = --static ] && continue
  count=$(cargo nextest list --workspace --profile "$profile" -E "$filter" --message-format json --cargo-quiet 2>/dev/null |
    jq -r '[."rust-suites"[].testcases[] | select(."filter-match".status == "matches")] | length')
  if [ "${count:-0}" = 0 ]; then
    no "profile $profile: no test matches: $filter"
  else
    echo "ok   $profile: $count tests match: ${filter:0:70}"
  fi
done <<<"$overrides"

# The `nightly` job, whose failures are what it's for, has no retries: it doesn't
# use nextest. (`stress` does, under the `stress` profile checked above.)
if awk '/^  nightly:$/ { on = 1; next } /^  [a-z_-]+:$/ { on = 0 } on' "$WORKFLOW" | grep -q nextest; then
  no "the nightly job names nextest: it gets no retries"
fi

[ "$bad" = 0 ] && echo "ok   the nextest filters hold"
exit "$bad"
