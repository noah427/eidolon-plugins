#!/usr/bin/env bash
# lifecycle-shape.sh: the lifecycle consumer's pure policy, checked without a
# store, a roster or a peer.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/coordinator/lifecycle-shape.sh
#
# `eidolon workflow run` compiles `<plugin>/lib/*.rn` in front of
# `<plugin>/workflows/<name>.rn`. That is the whole runner: this script copies the
# shipped `coordinator/lib/lifecycle.rn` and the fixture workflow beside it into a
# throwaway plugin, and the workflow calls the functions directly. No tool is
# registered and no session is watched, so a pass says nothing about the harness —
# it says the id derivation, the halt decision and the store's shape are what the
# file says they are.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

eidolon=${EIDOLON_BIN:-}
if [ -z "$eidolon" ]; then
    eidolon=$(command -v eidolon || true)
fi
if [ -z "$eidolon" ] || [ ! -x "$eidolon" ]; then
    echo "SKIP lifecycle-shape.sh: no eidolon binary (set EIDOLON_BIN)" >&2
    exit 3
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
plug=$work/plugin
mkdir -p "$plug/lib" "$plug/workflows"

cp "$repo/coordinator/lib/lifecycle.rn" "$plug/lib/lifecycle.rn"
cp "$here/lifecycle.rn" "$plug/workflows/lifecycle.rn"
cat >"$plug/plugin.rn" <<'EOF'
pub fn manifest() {
    #{
        name: "lifecycle-shape",
        version: "0.0.0",
        description: "the lifecycle consumer's pure policy, checked",
        claims: #{ tools: [] },
    }
}
EOF

status=0
out=$("$eidolon" workflow run "$plug" lifecycle --provider mock --yolo --args '{}' --cwd "$work" 2>&1) || status=$?
printf '%s\n' "$out"

if [ "$status" -ne 0 ]; then
    echo "FAIL lifecycle-shape.sh: workflow exited $status" >&2
    exit 1
fi
if ! printf '%s' "$out" | grep -q '"status":"completed"'; then
    echo "FAIL lifecycle-shape.sh: the workflow did not complete" >&2
    exit 1
fi
if ! printf '%s' "$out" | grep -q 'lifecycle: [0-9][0-9]* checks hold'; then
    echo "FAIL lifecycle-shape.sh: no check summary in the report" >&2
    exit 1
fi
