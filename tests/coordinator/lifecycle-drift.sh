#!/usr/bin/env bash
# lifecycle-drift.sh: the handlers carry a copy of the library, and the copy is what runs.
#
#   bash tests/coordinator/lifecycle-drift.sh
#
# `eidolon` compiles each `tools/*.rn` on its own and `lib/` only in front of
# workflows, so every handler that needs the policy repeats it. That makes two texts
# where there is one intent: `coordinator/lib/lifecycle.rn` is what
# `lifecycle-shape.sh` tests, and the handler's copy is what the dispatcher actually
# executes. A green test over a library the handler does not carry is exactly the
# failure this file exists to refuse.
#
# The library is authoritative: every `fn lifecycle_*` that appears in a handler must
# be byte-identical to the library's, and a handler may not carry one the library does
# not have. Exit 0 when every copy matches, 1 on the first divergence (named), 3 if
# there is nothing to compare (which is itself a failure of this guard, not a pass).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
lib=$repo/coordinator/lib/lifecycle.rn

exec python3 - "$lib" "$repo/coordinator/tools" <<'PY'
import pathlib, re, sys

lib = pathlib.Path(sys.argv[1])
tools = pathlib.Path(sys.argv[2])
if not lib.is_file():
    print(f"DRIFT SKIP: no library at {lib}", file=sys.stderr)
    sys.exit(3)


def functions(text):
    """Every top-level `fn name(...) { ... }`, by brace matching, in file order."""
    out = {}
    for m in re.finditer(r'^fn ([A-Za-z0-9_]+)\(', text, re.M):
        name = m.group(1)
        start = m.start()
        i = text.index('{', m.end())
        depth = 0
        while i < len(text):
            if text[i] == '{':
                depth += 1
            elif text[i] == '}':
                depth -= 1
                if depth == 0:
                    out[name] = text[start:i + 1]
                    break
            i += 1
        else:
            print(f"DRIFT: unbalanced braces in `fn {name}` of {lib}", file=sys.stderr)
            sys.exit(1)
    return out


canonical = functions(lib.read_text())
if not canonical:
    print(f"DRIFT SKIP: {lib} defines no function at all", file=sys.stderr)
    sys.exit(3)

carriers = 0
checked = 0
problems = []
for tool in sorted(tools.glob("*.rn")):
    copies = {n: body for n, body in functions(tool.read_text()).items()
              if n.startswith("lifecycle_")}
    if not copies:
        continue
    carriers += 1
    for name in sorted(copies):
        want = canonical.get(name)
        if want is None:
            problems.append(f"{tool.name}: carries `fn {name}`, which {lib.name} does not define")
            continue
        checked += 1
        if copies[name] != want:
            first = next((i for i, (a, b) in enumerate(zip(copies[name], want)) if a != b),
                         min(len(copies[name]), len(want)))
            problems.append(
                f"{tool.name}: `fn {name}` differs from the library at byte {first}\n"
                f"    handler: {copies[name][first:first + 90]!r}\n"
                f"    library: {want[first:first + 90]!r}")

if checked == 0 or carriers == 0:
    print(f"DRIFT SKIP: no handler carries a copy; nothing was compared "
          f"({carriers} file(s) with `lifecycle_*` functions, {checked} function(s))",
          file=sys.stderr)
    sys.exit(3)

if problems:
    for p in problems:
        print(f"FAIL {p}")
    print(f"lifecycle-drift.sh: {len(problems)} divergence(s) between the library and the "
          f"copies that actually run")
    sys.exit(1)

print(f"lifecycle-drift.sh: {checked} copied function(s) across {carriers} handler(s) are "
      f"byte-identical to {lib.name}")
PY
