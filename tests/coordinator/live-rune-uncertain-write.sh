#!/usr/bin/env bash
# live-rune-uncertain-write.sh — a store that cannot be written, and the two doors that must not
# claim an act they did not record. Witness map rows 10 and 11, which had no witness of any kind.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-uncertain-write.sh
#
# WHY. `adopt.rn` and `release.rn` each have a path for "the write came back uncertain" and "the
# write was refused", and until this fixture nothing drove either. The failure is the host's own
# bound on one state row — a body over 1 MiB is refused before a temp file exists
# (`crates/rune/src/plugin_state.rs:199-205`) — reached through the plugin's ordinary path by an
# adoption carrying an oversized `note`, or a release carrying an oversized `reason`. No production
# seam is added for the test, no permission is changed, and the store stays readable, so the
# ordinary adoption at the end is a control: the ledger is healthy and the refusal was about that
# one write.
#
# A first cut of this fixture tried `chmod 0500` on the session's plugin-state directory and did not
# work: `plugin_state::write` calls `private_dir` on every write, which puts 0700 back
# (`plugin_state.rs:139-140`). Recorded because it is the sort of thing that looks like a passing
# test until the arm is run.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2
: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-uncertain-write.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi
# Its own roster, for the same reason the guard fixture has one: nothing here may register beside
# the operator's real sessions.
runtime=$(mktemp -d)
export XDG_RUNTIME_DIR=$runtime
trap 'rm -rf "$runtime" 2>/dev/null || true; :' EXIT

fail=0
checks=0
skipped=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then say "ok   $1"; else
        say "FAIL $1: wanted to find [$3] in the tool's answer"; fail=$((fail + 1)); fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"; fail=$((fail + 1)); else say "ok   $1"; fi
}

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/lifecycle-fixture/tools/seed_json.rn" "$here/lifecycle-fixture/tools/dump_rows.rn" "$plug/tools/"
sed -i 's/"rank", "unrank", "ranks", "hook_ready", "hook_peer"\]/"rank", "unrank", "ranks", "hook_ready", "hook_peer", "seed_json", "dump_rows"]/' "$plug/plugin.rn"
grep -q 'seed_json' "$plug/plugin.rn" || { say "FAIL the seed fixture is not registered"; exit 1; }
cp "$here/rune-uncertain.rn" "$plug/workflows/rune-uncertain.rn"
"$EID" plugins trust coordinator >/dev/null

say "=== driving two doors at a store that cannot be written ==="
run=$("$EID" workflow run "$plug" rune-uncertain --provider mock --yolo 2>&1 || true)
printf '%s\n' "$run" | sed 's/^/     /'

# As in the guard fixture: a run that failed outright would satisfy every absence check below.
check "the workflow run completed" "$run" '"status":"completed"'

seeded=1793000011111
refused=1793000022222
after=1793000044444
check "U0 the fixture wrote a live row while the store was ordinary" "$run" "U0 OK"
# Row 10 — an adoption whose write is refused says it did not happen. It used to answer an EMPTY
# success instead, because its refusal ended in a trailing `;` and the block's value was the unit;
# `b51d6e4` deleted the character and `crates/rune` now reports a unit result as an error rather than
# an empty success. There is no skip left here: the sentence is required, and `m13` (the `;` put back)
# is the arm that keeps it.
check "U1 an adoption whose write is refused says it was NOT written down" "$run" \
    "U1 ERR the adoption of $refused was **not written down**, so it did not happen"
check "U1 and the refusal it carries is the one the host gave" "$run" \
    "over the 1048576-byte bound on one state row; nothing was written"
check_absent "U1b and it was not answered as an adoption" "$run" "U1 OK"
check_absent "U1c and it was not answered as a unit result either" "$run" "evaluated to \`()\`"
# U5 — `release.rn`'s `left_word`, the second site of the same one-character class: a release that
# leaves a pending notice alone must still RELEASE and say how many it left, which is a sentence the
# broken tail could not build at all.
check "U5 a release with an unmatched pending notice still releases" "$run" \
    "U5 OK released 1793000055555 (epoch 1); monitoring ends now"
check "U5 and it says the notice was left alone, and why" "$run" \
    "1 pending notice(s) were left alone because their epoch could not be matched to this adoption"

# Row 11: the same, for a release.
check "U2 a release that cannot be written down did not happen" "$run" \
    "U2 ERR the release of $seeded was **not written down**, so it did not happen"
check_absent "U2b and it was not answered as a release" "$run" "U2 OK"
# The answer and the store must agree.
check_absent "U3 the refused adoption is not in the ledger's rows" "$run" "(called \`not written\`)"
check "U3 the row that could not be released is still there and not released" "$run" \
    "epoch 1 — $seeded (called \`uncertain-write fixture\`)"
check_absent "U3b and it does not carry the refusal's reason" "$run" "released because: 0123456789"
# The control: the same ledger accepts the next ordinary adoption, in epoch 1.
check "U4 the store still works after both refusals" "$run" \
    "U4 OK adopted $after in epoch 1"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-uncertain-write complete: $checks checks ($skipped skipped), 0 FAIL"
else
    say "live-rune-uncertain-write FAILED: $fail of $checks checks"
    exit 1
fi
