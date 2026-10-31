#!/usr/bin/env bash
# live-rune-guards.sh — the guard fixture: every refusal `coordinator_adopt` and
# `coordinator_release` now answer with, driven through the plugin's own shipped tools.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     EIDOLON_BIN=/path/to/eidolon bash tests/coordinator/live-rune-guards.sh
#
# WHY THIS EXISTS. Core used to witness these guards itself: `adopt_refuses_an_empty_worker_address`,
# `a_session_cannot_adopt_itself`, `a_second_live_adoption_is_refused_naming_the_epoch`,
# `re_adoption_after_a_release_takes_the_next_epoch`, `releasing_a_worker_this_session_does_not_hold_says_so`
# and a release that could not be written down. Slice 2 removed the verbs, so those tests left with
# them and the guards moved here — written in `coordinator/tools/adopt.rn` and `release.rn`, and
# until this fixture existed, unwitnessed at the layer that owns them. Each check below names the
# sentence the tool answers, not merely that it answered.
#
# No monitor, no port, no service: adoption and release are this ledger's acts. One real `eidolon`,
# one throwaway config, one workflow run, nothing of the operator's read or written.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

EID=${EIDOLON_BIN:-}
if [ -z "$EID" ] || [ ! -x "$EID" ]; then
    echo "SKIP live-rune-guards.sh: set EIDOLON_BIN to a built \`eidolon\` binary" >&2
    exit 3
fi

fail=0
checks=0
say() { printf '%s\n' "$*"; }
check() { # check <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "ok   $1"
    else
        say "FAIL $1: wanted to find [$3] in the tool's answer"; fail=$((fail + 1))
    fi
}
check_absent() { # check_absent <name> <haystack> <needle>
    checks=$((checks + 1))
    if printf '%s' "$2" | grep -qF -- "$3"; then
        say "FAIL $1: must not contain [$3]"; fail=$((fail + 1))
    else
        say "ok   $1"
    fi
}

plug=$XDG_CONFIG_HOME/eidolon/plugins/coordinator
mkdir -p "$plug/workflows"
cp -r "$repo/coordinator/." "$plug/"
cp "$here/rune-guards.rn" "$plug/workflows/rune-guards.rn"
"$EID" plugins trust coordinator >/dev/null

say "=== driving the guards through the plugin's own tools ==="
run=$("$EID" workflow run "$plug" rune-guards --provider mock --yolo 2>&1 || true)
printf '%s\n' "$run" | sed 's/^/     /'

worker=1793000004242

# G1 — nothing named. The tool's JSON schema refuses this before the body runs, which is a stronger
# guard than the sentence below it: the check is that the refusal names the missing field.
check "G1 an adoption with no session names the missing field" "$run" \
    "missing required field \`session\` (string)"
# G2 — self-adoption. A run whose roster names no row skips; that is reported, not passed.
if printf '%s' "$run" | grep -q '^G2 SKIP'; then
    say "ok   G2 skipped: this run's roster named no row for it (the guard itself is not exercised here)"
else
    check "G2 a session cannot adopt itself" "$run" "a session cannot adopt itself"
fi
# G3 — the first adoption.
check "G3 the first adoption lands in epoch 1" "$run" "adopted $worker in epoch 1"
# G4 — the second live adoption is refused, naming the epoch and pointing at release.
check "G4 a second live adoption is refused, naming its epoch" "$run" \
    "$worker is already adopted by this session, in epoch 1 and still monitored"
check "G4 and the refusal points at the release that ends it" "$run" "\`coordinator_release\` ends it first"
# G5 — release ends it, and says the record stays.
check "G5 a release ends monitoring and keeps the record" "$run" "released $worker (epoch 1); monitoring ends now"
# G6 — a re-adoption is the next epoch, never a reused one.
check "G6 a re-adoption after a release takes epoch 2" "$run" "adopted $worker in epoch 2"
# G7 — a release of something never held says so.
check "G7 a release of an unheld worker says there is nothing to release" "$run" \
    "1793000009494 is not adopted by this session (or was already released); there is nothing to release"
# G9 — an empty address. The schema cannot refuse it (`""` is a string), so the tool must. This is
# the guard core's `adopt_refuses_an_empty_worker_address` kept, and until this fixture was written
# the plugin adopted the empty address into a live row: guard 1 of its own list ("worker non-empty")
# was prose with no code behind it.
check "G9 an empty address is refused, naming what is missing" "$run" \
    "coordinator_adopt needs a durable address: \`session\` is empty"
check_absent "G9b and no live row naming nobody was written" "$run" "G9 OK adopted  in epoch"

# G8 — a release with nothing named; refused by the schema, naming the field.
check "G8 a release with no session names the missing field" "$run" \
    "coordinator_release\` was called with invalid arguments: missing required field \`session\` (string)"
# The refusals must be refusals: none of them may be answered as a success line.
check_absent "no refusal was answered as an OK" "$run" "G4 OK"
check_absent "and the second adoption did not land a second live row" "$run" "adopted $worker in epoch 2. A live session"

say ""
if [ "$fail" -eq 0 ]; then
    say "live-rune-guards complete: $checks checks, 0 FAIL"
else
    say "live-rune-guards FAILED: $fail of $checks checks"
    exit 1
fi
