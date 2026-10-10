#!/usr/bin/env bash
# run.sh: check the librewolf tools' answer when the bridge is not running.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/librewolf/run.sh
#
# Nothing is started and no browser is involved: the test only runs when nothing
# answers on 127.0.0.1:8091, so the tools cannot reach a real bridge even if the
# operator has one up in their own session.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

: "${XDG_STATE_HOME:?set XDG_STATE_HOME to a temp dir}"
: "${XDG_DATA_HOME:?set XDG_DATA_HOME to a temp dir}"

eidolon=$(command -v eidolon || true)
[ -n "$eidolon" ] || { echo "eidolon is not on PATH" >&2; exit 2; }

# A live bridge would take these calls. This test is about one that is not there.
if timeout 2 bash -c 'exec 3<>/dev/tcp/127.0.0.1/8091' 2>/dev/null; then
    echo "refusing: something is answering on 127.0.0.1:8091. Close the LibreWolf that has the bridge extension loaded; this test must not reach a real session" >&2
    exit 2
fi

plug=$XDG_CONFIG_HOME/eidolon/plugins
mkdir -p "$plug"
rm -rf "$plug/librewolf"
cp -r "$repo/librewolf" "$plug/librewolf"
mkdir -p "$plug/librewolf/workflows"
cp "$here/not-connected.rn" "$plug/librewolf/workflows/not-connected.rn"

# The token each verb resolves, and the grant rows the README's install writes, so
# the call gets as far as the dial instead of stopping at a missing credential.
token=$HOME/.config/eidolon/librewolf.token
mkdir -p "$(dirname "$token")"
(umask 077 && head -c 24 /dev/urandom | base64 > "$token")
run_eidolon() { XDG_CONFIG_HOME=$XDG_CONFIG_HOME "$eidolon" "$@"; }
run_eidolon plugins trust librewolf >/dev/null
for v in status tabs read structure click type scroll navigate back detach; do
    run_eidolon plugins grant "librewolf_$v" "file:~/.config/eidolon/librewolf.token" >/dev/null
done

report=$XDG_STATE_HOME/not-connected.txt
mkdir -p "$XDG_STATE_HOME"
run_eidolon workflow run "$plug/librewolf" not-connected --args '{}' --provider mock >"$report" 2>&1 || true

if grep -q "checks hold" "$report"; then
    cat "$report"
    echo "ok   every verb names what starts the bridge"
    exit 0
fi
echo "FAIL not-connected" >&2
sed -n 1,40p "$report" >&2
exit 1
