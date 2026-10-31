#!/usr/bin/env python3
"""The handler copies of `coordinator/lib/lifecycle.rn`, generated and verified.

A Rune **tool is compiled as one source file** (`crates/rune/src/script.rs:88`,
`compile_source` over one `src`), and only a **workflow** has the plugin's `lib/*.rn`
compiled in front of it (`crates/rune/src/workflow.rs:241-253`). So there is no
module/import/prelude path a tool could use to reach the library directly, and the
handlers must carry a copy.

This file is the single source of those copies: `--write` emits the region into each
handler, and `--check` (which `lifecycle-drift.sh` runs) compares the region byte for
byte against what `--write` would produce, plus the stamp of the library the copies
came from. The comparison is **by region, not by name**: renaming a copied function
does not exempt it, because the region must equal the generated text exactly.

    python3 tests/coordinator/lifecycle-copies.py --check     # exit 1 on divergence
    python3 tests/coordinator/lifecycle-copies.py --write     # after editing the library

Edit `coordinator/lib/lifecycle.rn`, then `--write`, and commit both.
"""
import hashlib
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parent.parent
LIB = REPO / "coordinator" / "lib" / "lifecycle.rn"
TOOLS = REPO / "coordinator" / "tools"

MARKER = "// --- generated: the policy copy, from coordinator/lib/lifecycle.rn ---"

# Which library functions each handler carries, in the order it carries them. A
# handler is not required to carry all of them; it may not carry one the library does
# not define. Adding a function here is the deliberate act; `--write` does the rest.
MANIFEST = {
    "release.rn": [
        "lifecycle_refuse_padded_address",
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_text",
        "lifecycle_number",
        "lifecycle_true",
        "lifecycle_released",
        "lifecycle_null",
    ],
    # The one handler whose act is core's and whose *claim* is its own: it reads the log's
    # typed cancellation word, so it carries the decision and the row readers it needs.
    "cancel.rn": [
        "lifecycle_epoch_source",
        "lifecycle_cancel_claim",
        "lifecycle_stated",
        "lifecycle_true",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_live_adoptions",
        "lifecycle_find_adoption",
        "lifecycle_text",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
    "adopt.rn": [
        "lifecycle_refuse_padded_address",
        "lifecycle_true",
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_text",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
    "hook_peer.rn": [
        "lifecycle_blind",
        "lifecycle_notice_id",
        "lifecycle_eat",
        "lifecycle_hex16",
        "lifecycle_ascii_byte",
        "lifecycle_tristate",
        "lifecycle_evaluate",
        "lifecycle_halt_word",
        "lifecycle_gap_row",
        "lifecycle_notice",
        "lifecycle_notice_text",
        "lifecycle_notice_args",
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_live_adoptions",
        "lifecycle_find_any",
        "lifecycle_find_adoption",
        "lifecycle_epoch_live",
        "lifecycle_pending_rows",
        "lifecycle_decide",
        "lifecycle_release_said",
        "lifecycle_record",
        "lifecycle_mark_written",
        "lifecycle_mark_cancelled",
        "lifecycle_apply_marks",
        "lifecycle_watch",
        "lifecycle_payload",
        "lifecycle_text",
        "lifecycle_true",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
    "hook_ready.rn": [
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_live_adoptions",
        "lifecycle_watch",
        "lifecycle_blind",
        "lifecycle_decoration_action",
        "lifecycle_rank_prefix",
        "lifecycle_prefix_of",
        "lifecycle_payload",
        "lifecycle_text",
        "lifecycle_true",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
    "notices.rn": [
        "lifecycle_true",
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_live_adoptions",
        "lifecycle_epoch_live",
        "lifecycle_text",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
    "adoptions.rn": [
        "lifecycle_stated",
        "lifecycle_epoch_word",
        "lifecycle_store_empty",
        "lifecycle_store_list",
        "lifecycle_store_put",
        "lifecycle_blind_notes",
        "lifecycle_live_adoptions",
        "lifecycle_find_adoption",
        "lifecycle_text",
        "lifecycle_true",
        "lifecycle_number",
        "lifecycle_released",
        "lifecycle_null",
    ],
}


def functions(text):
    """Every top-level `fn name(...) { ... }` in file order, by brace matching."""
    out = {}
    order = []
    for m in re.finditer(r'^fn ([A-Za-z0-9_]+)\(', text, re.M):
        name = m.group(1)
        i = text.index('{', m.end())
        depth = 0
        j = i
        while j < len(text):
            if text[j] == '{':
                depth += 1
            elif text[j] == '}':
                depth -= 1
                if depth == 0:
                    out[name] = text[m.start():j + 1]
                    order.append(name)
                    break
            j += 1
        else:
            raise SystemExit(f"unbalanced braces in `fn {name}` of {LIB}")
    return out, order


def stamp(text):
    return hashlib.sha256(text.encode()).hexdigest()


def region(handler, canonical):
    """The text the generated region of `handler` must be, exactly."""
    lib_text = LIB.read_text()
    head = (
        f"{MARKER}\n"
        "//\n"
        "// Do not edit these: they are the library's text. `--write` emits them and\n"
        "// `lifecycle-drift.sh` fails if they differ from it by one byte — the whole region\n"
        "// is compared against what the generator produces, so renaming a copy does not\n"
        "// exempt it. Edit `coordinator/lib/lifecycle.rn`, then run `--write`.\n"
        f"// stamp: sha256:{stamp(lib_text)}\n\n"
    )
    body = []
    for name in MANIFEST[handler]:
        if name not in canonical:
            raise SystemExit(f"manifest for {handler} names `{name}`, which the library does not define")
        body.append(canonical[name])
    return head + "\n".join(body) + "\n"


def split(text):
    """(before, region) — the region runs from MARKER to the end of the file.

    A file with no marker yet is migrated by `--write`: everything from its first
    `fn lifecycle_*` to the end of the file is taken to be the region, so the first
    run replaces exactly the copies a person had made by hand and touches nothing
    above them.
    """
    i = text.find(MARKER)
    if i >= 0:
        return text[:i], text[i:]
    m = re.search(r'^fn lifecycle_', text, re.M)
    if m:
        return text[:m.start()], text[m.start():]
    return text, ""


def main(argv):
    canonical, _ = functions(LIB.read_text())
    write = "--write" in argv
    check = "--check" in argv
    if not (write or check):
        print(__doc__.strip().splitlines()[0])
        print("usage: lifecycle-copies.py --check|--write")
        return 2

    problems = []
    changed = 0
    for handler in MANIFEST:
        path = TOOLS / handler
        text = path.read_text()
        before, have = split(text)
        if before == "" and have == "" and MARKER not in text:
            problems.append(f"{handler}: no generated region and no `fn lifecycle_*` to migrate")
            continue
        want = region(handler, canonical)
        if write:
            if have != want:
                path.write_text(before + want)
                changed += 1
            continue
        if have == want:
            continue
        # Name the first function whose bytes differ (or the first missing/extra one).
        have_fns, have_order = functions(have)
        want_fns, want_order = functions(want)
        named = None
        for name in want_order:
            if name not in have_fns:
                named = f"`fn {name}` is missing from the region"
                break
            if have_fns[name] != want_fns[name]:
                a, b = have_fns[name], want_fns[name]
                at = next((k for k, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
                named = (f"`fn {name}` differs from the library at byte {at}\n"
                         f"    handler: {a[at:at + 80]!r}\n"
                         f"    library: {b[at:at + 80]!r}")
                break
        if named is None:
            for name in have_order:
                if name not in want_fns:
                    named = f"`fn {name}` is in the region and not in the manifest — a renamed copy escapes nothing here, because the region is compared whole"
                    break
        if named is None:
            got = re.search(r'stamp: sha256:([0-9a-f]+)', have)
            named = ("the region's bytes differ outside any function (its header, or the stamp): "
                     f"handler {got.group(1) if got else 'no stamp'}, library {stamp(LIB.read_text())}")
        problems.append(f"{handler}: {named}")

    if problems:
        for p in problems:
            print(f"FAIL {p}")
        print(f"lifecycle-copies.py: {len(problems)} handler(s) out of step with "
              f"{LIB.relative_to(REPO)}; run --write after editing the library")
        return 1
    if write:
        print(f"lifecycle-copies.py: wrote {changed} of {len(MANIFEST)} handler region(s) "
              f"(stamp sha256:{stamp(LIB.read_text())})")
        return 0
    total = sum(len(v) for v in MANIFEST.values())
    print(f"lifecycle-copies.py: {total} copied function(s) across {len(MANIFEST)} handler(s) "
          f"match {LIB.relative_to(REPO)} (stamp sha256:{stamp(LIB.read_text())})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
