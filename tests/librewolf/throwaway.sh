#!/usr/bin/env bash
# throwaway.sh: the browser proof, against a disposable LibreWolf profile and
# local pages — never the operator's session.
#
#   t=$(mktemp -d)
#   HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state XDG_DATA_HOME=$t/data \
#     bash tests/librewolf/throwaway.sh
#
# What it drives, end to end: the browser spawns the host as its native messaging
# host, the extension connects with the right add-on id, the host's loopback
# endpoint answers reads and structure calls out of real pages, a new document ends
# the attachment, a second and third attach work, `detach` ends it, and killing the
# browser takes the host with it.
#
# The pages carry the extraction semantics the unit tests can only mock: a password
# field, a hidden CSRF input, a link and an input inside `display:none`, a
# `hidden`-attribute link, a `visibility:hidden` block, a textarea with content, an
# ordinary input with a value, and a page whose visible text is empty. What a person
# can see is in the answer; what they cannot see is asserted not to be.
#
# Limits: the second origin is another local port, not a foreign host. The
# cross-origin document/return chain is tested, but server-side redirect hops
# without a foreign document commit are not. See tests/librewolf/README.md.
#
# A headless browser has no toolbar, so there is no click
# to make. The harness *substitutes* the click: it appends a test-only script to a
# copy of the extension which attaches the active tab on request, and gives that copy
# a host permission for the local test origin so the read path can run without a
# person. The shipped extension keeps `activeTab` and the toolbar click alone, and
# nothing here is evidence that the click and the `activeTab` grant it creates work.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
. "$here/guard.sh"
require_temp_config || exit 2

browser=${EIDOLON_LIBREWOLF_BROWSER:-librewolf}
command -v "$browser" >/dev/null || { echo "no '$browser' on PATH (set EIDOLON_LIBREWOLF_BROWSER)" >&2; exit 2; }

# The binary under test, and its provenance. A stale binary would make this whole
# run say nothing about the sources beside it, so the default path is rebuilt here
# every time (cargo is incremental) and what was used is printed with its digest.
if [ -n "${EIDOLON_LIBREWOLF_BIN:-}" ]; then
    bin=$EIDOLON_LIBREWOLF_BIN
    [ -x "$bin" ] || { echo "EIDOLON_LIBREWOLF_BIN=$bin is not executable" >&2; exit 2; }
    echo "note: EIDOLON_LIBREWOLF_BIN was set; not rebuilding, and testing whatever is there"
else
    bin=$repo/target/debug/eidolon-librewolf
    echo "building the host from the sources beside this test ..."
    (cd "$repo" && cargo build -p eidolon-librewolf) >&2 || exit 2
fi
echo "ok   host under test: $bin"
echo "     $(sha256sum "$bin" | cut -c1-32)…  mtime $(date -r "$bin" '+%Y-%m-%d %H:%M:%S')"
newest=$(find "$repo/librewolf/service/src" "$repo/librewolf/service/Cargo.toml" -type f -newer "$bin" | head -1)
if [ -n "$newest" ] && [ -z "${EIDOLON_LIBREWOLF_BIN:-}" ]; then
    echo "FAIL the host binary is older than $newest (harness problem)" >&2
    exit 1
fi

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }
port=${EIDOLON_LIBREWOLF_PORT:-$(free_port)}
webport=$(free_port)
export EIDOLON_LIBREWOLF_PORT=$port
# The host treats the extension's silence as the browser's death (end of stream is
# not reliable — a content process can hold the pipe). The shipped default is 30s;
# the proof shortens it so the last check does not have to wait out the default.
export EIDOLON_LIBREWOLF_SILENCE_S=${EIDOLON_LIBREWOLF_SILENCE_S:-6}

lwpid=""
pgid=""
webpid=""
webpid2=""
cleanup() {
    # Only ever our own browser's process group, named by the pid this script
    # launched. Nothing here is picked out of a process listing.
    if [ -n "$pgid" ]; then kill -TERM -"$pgid" 2>/dev/null || true; fi
    if [ -n "$webpid" ]; then kill "$webpid" 2>/dev/null || true; fi
    if [ -n "$webpid2" ]; then kill "$webpid2" 2>/dev/null || true; fi
    return 0
}
trap cleanup EXIT

fail() { echo "FAIL $*" >&2; exit 1; }

# ---- the pages -------------------------------------------------------------

pages=$HOME/pages
mkdir -p "$pages"

cat > "$pages/page-one.html" <<'EOF'
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="12;url=page-two.html">
<title>Bridge page one</title>
<h1>Bridge page one</h1>
<p>UNIQUE-TEXT-ONE</p>
<a href="page-two.html">a link to page two</a>
<!-- An experiment, not the plugin: can a value be kept in the content script's own
     world, out of the page's reach, across two injections in one document? The
     extension writes #sandbox-diag; this page's own script writes #nonce-diag, looking
     for the same value through the window it can see. -->
<pre id="sandbox-diag">sandbox-diag: pending</pre>
<pre id="nonce-diag">nonce-diag: pending</pre>
<script>
  setInterval(function () {
    var v = window.__eidolon_nonce_probe;
    document.getElementById("nonce-diag").textContent =
      "nonce-diag: page-sees " + (typeof v === "string" ? v : "nothing");
  }, 400);
</script>
EOF

# Everything a page hides, and one thing it shows. The values are distinctive so
# that their absence is a check and not an impression.
cat > "$pages/page-two.html" <<'EOF'
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="16;url=page-three.html">
<title>Bridge page two</title>
<h1>Bridge page two</h1>
<p>UNIQUE-TEXT-TWO</p>
<form>
  <label for="pw">Password</label>
  <input id="pw" type="password" name="password" value="PASSWORD-SECRET-alpha">
  <input type="hidden" name="csrf_token" value="CSRF-SECRET-beta">
  <input type="text" name="email" placeholder="Your email" value="EMAIL-SECRET-zeta">
  <textarea name="notes">TEXTAREA-SECRET-epsilon</textarea>
  <button type="submit">Send</button>
</form>
<div style="display:none">
  <a href="/hidden">HIDDEN-LINK-SECRET-gamma</a>
  <input type="text" name="note" value="HIDDEN-VALUE-delta">
</div>
<div hidden><a href="/hidden2">HIDDEN-ATTR-LINK-SECRET-eta</a></div>
<div style="visibility:hidden"><a href="/hidden3">VISIBILITY-HIDDEN-LINK-SECRET-theta</a></div>
EOF

# A document with nothing a person can read, and a script holding a secret: a read
# of it must come back empty, not full.
cat > "$pages/page-three.html" <<'EOF'
<!doctype html>
<meta charset="utf-8">
<title>Bridge page three</title>
<script>const SCRIPT_ONLY_SECRET = "SCRIPT-ONLY-SECRET-zzz";</script>
<p id="empty"></p>
EOF

# A second origin. A different port is a different origin to the bridge (and to a
# browser), even though the site permission Firefox asks for cannot name a port —
# which is exactly the boundary this pair of servers is here to exercise.
port2=$(free_port)
pages2=$HOME/pages2
mkdir -p "$pages2"

cat > "$pages/away-goto.html" <<EOF
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0;url=http://127.0.0.1:$port2/away.html">
<title>Leaving this origin</title>
<p>This document hands the tab to another origin.</p>
EOF

: > "$pages/want-eventlog"

cat > "$pages/hop.html" <<EOF
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0;url=http://127.0.0.1:$port2/out.html">
<title>Hopping out</title>
<p>This document leaves the origin, and the next one comes back.</p>
EOF

cat > "$pages/home-again.html" <<'EOF'
<!doctype html>
<meta charset="utf-8">
<title>Home again</title>
<h1>Home again</h1>
<p>HOME-AGAIN-TEXT</p>
EOF

cat > "$pages2/away.html" <<'EOF'
<!doctype html>
<meta charset="utf-8">
<title>Another origin</title>
<h1>Another origin</h1>
<p>AWAY-TEXT</p>
EOF

cat > "$pages2/out.html" <<EOF
<!doctype html>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0;url=http://127.0.0.1:$webport/home-again.html">
<title>Turning back</title>
<p>This document sends the tab back where it came from.</p>
EOF

(cd "$pages" && exec python3 -m http.server "$webport" --bind 127.0.0.1) >/dev/null 2>&1 &
webpid=$!
(cd "$pages2" && exec python3 -m http.server "$port2" --bind 127.0.0.1) >/dev/null 2>&1 &
webpid2=$!

# ---- the profile, the host manifest, and a copy of the extension ----------

profile=$HOME/profile
extdir=$profile/extensions/librewolf-bridge@eidolon.local
mkdir -p "$extdir"
cp -r "$repo/librewolf/extension/." "$extdir/"

# The copy differs from the shipped extension in exactly two ways, both marked:
# the background script gets a test-only attach trigger, and the manifest grants
# the local test origin so that trigger can work without a click.
python3 - "$extdir" "$webport" <<'PY'
import json, sys
extdir, webport = sys.argv[1], sys.argv[2]
manifest = json.load(open(f"{extdir}/manifest.json"))
# A match pattern carries no port; this grants the loopback host, all ports.
# INSTRUMENTATION, test copy only: webNavigation is registered here so the run can
# record what the browser reports for each navigation. The shipped manifest does not
# carry it — whether it should is a permission decision that this run informs and
# does not take.
manifest["permissions"] = ["nativeMessaging", "activeTab", "tabs", "webNavigation", "http://127.0.0.1/*"]
manifest["background"] = {"scripts": ["background.js", "test-attach.js"], "persistent": True}
json.dump(manifest, open(f"{extdir}/manifest.json", "w"), indent=2)
open(f"{extdir}/test-attach.js", "w").write(f'''// TEST ONLY. Appended by tests/librewolf/throwaway.sh to a *copy* of the
// extension, to stand in for a toolbar click a headless browser cannot produce.
// None of this is in the shipped extension and none of it is evidence that the
// click, the gesture or the site permission it asks for work: it writes the
// enablement record the shipped click would have written, with the shipped
// `probe()`, and nothing else. Failures are reported to the host as a revocation
// reason, which is the one channel the harness can read.
const TEST_ATTACH_URL = "http://127.0.0.1:{webport}/should-attach";
const TEST_LOG_URL = "http://127.0.0.1:{webport}/want-eventlog";

// TEST ONLY instrumentation: what does the browser actually report as a tab moves
// between documents and origins? Every navigation event the platform offers is
// recorded here and reported to the host, whose revocation reason is how the
// harness reads it back. None of this is in the shipped extension.
const EVENT_LOG = [];
function logEvent(line) {{
    EVENT_LOG.push(line);
    if (EVENT_LOG.length > 80) EVENT_LOG.shift();
}}
browser.tabs.onUpdated.addListener((tabId, changeInfo) => {{
    logEvent("onUpdated#tab" + tabId + " " + JSON.stringify(changeInfo));
}});
try {{
    if (browser.webNavigation && browser.webNavigation.onCommitted) {{
        browser.webNavigation.onBeforeNavigate.addListener((d) => {{
            logEvent("onBeforeNavigate#tab" + d.tabId + " frame" + d.frameId + " url=" + JSON.stringify(d.url));
        }});
        browser.webNavigation.onCommitted.addListener(async (d) => {{
            let seen = "tabs.get failed";
            try {{
                const t = await browser.tabs.get(d.tabId);
                seen = JSON.stringify(t && t.url);
            }} catch (e) {{ seen = "tabs.get threw: " + (e && e.message); }}
            logEvent("onCommitted#tab" + d.tabId + " frame" + d.frameId + " url=" + JSON.stringify(d.url) +
                " transition=" + JSON.stringify(d.transitionType) + " tabs.get.url=" + seen);
        }});
        logEvent("webNavigation listeners registered");
    }} else {{
        logEvent("no browser.webNavigation in this copy");
    }}
}} catch (e) {{
    logEvent("webNavigation registration failed: " + (e && e.message));
}}
let logReported = 0;
setInterval(async () => {{
    try {{
        const r = await fetch(TEST_LOG_URL, {{ cache: "no-store" }});
        if (!r.ok) return;
        if (EVENT_LOG.length === logReported) return;
        logReported = EVENT_LOG.length;
        announce({{ tab_id: null, reason: "eventlog: " + EVENT_LOG.join(" || "), url: null }});
    }} catch (e) {{}}
}}, 1500);
function testReport(what) {{
    announce({{ tab_id: null, reason: "test-attach: " + what, url: null }});
}}
setInterval(async () => {{
    if (enabled.size > 0) return;
    try {{
        const r = await fetch(TEST_ATTACH_URL, {{ cache: "no-store" }});
        if (!r.ok) return;
        const tabs = (await browser.tabs.query({{}})) || [];
        const seen = tabs.map((t) => t.id + (t.active ? "*" : "") + ":" + (t.url || "<no url>")).join(" | ");
        const tab =
            (tabs.find((t) => (t.url || "").startsWith("http://127.0.0.1:"))) ||
            (tabs.find((t) => t.active)) ||
            tabs[0];
        if (!tab) {{ testReport("no tab at all; tabs: " + seen); return; }}
        const href = tab.url || "";
        if (!isHttp(href)) {{ testReport("the tab is not an http(s) page: " + JSON.stringify(href)); return; }}
        const identity = await probe(tab.id);
        if (!identity) {{ testReport("probe failed on tab " + tab.id + " (" + (tab.url || "<no url>") + "); tabs: " + seen); return; }}
        // The clock this fork gives a document, for the record: LibreWolf ships
        // privacy.resistFingerprinting = true, which coarsens timers, and the nonce
        // is what carries the identity. Printed, not trusted.
        let clock = "";
        try {{
            const out = await browser.tabs.executeScript(tab.id, {{
                code: "(() => ({{ time_origin: performance.timeOrigin, now: performance.now(), date: Date.now() }}))()",
                runAt: "document_idle",
            }});
            clock = JSON.stringify((out && out[0]) || null);
        }} catch (e) {{ clock = "unavailable"; }}
        // The nonce experiment (test only). Two injections in one document, to see
        // whether a value put on the sandbox's own global survives between them and
        // whether the page can see it. The answer goes into the page, where the
        // harness reads it back through a normal read; the page's own script writes its
        // half into a second element.
        try {{
            const A = '(function () {{ var fresh = typeof globalThis.__eidolon_nonce_probe !== "string";' +
                ' if (fresh) {{ globalThis.__eidolon_nonce_probe = "N" + Math.random().toString(36).slice(2, 10); }}' +
                ' return {{ fresh: fresh, where: (globalThis === window ? "global-is-window" : "global-distinct") }}; }})()';
            const B = '(function () {{ return {{ fresh: typeof globalThis.__eidolon_nonce_probe !== "string" }}; }})()';
            const ra = (await browser.tabs.executeScript(tab.id, {{ code: A, runAt: "document_idle" }}))[0];
            const rb = (await browser.tabs.executeScript(tab.id, {{ code: B, runAt: "document_idle" }}))[0];
            const note = "sandbox-diag: first=" + JSON.stringify(ra) + " second=" + JSON.stringify(rb);
            // The first experiment in a document writes the answer; a later one —
            // these ticks can overlap, and the value is per document — must not
            // overwrite it, or the diagnostic would report the second run's
            // `fresh:false` as if the value had not survived.
            const write = "(function (txt) {{ var d = document.getElementById('sandbox-diag'); if (d && d.textContent.indexOf('first=') === -1) {{ d.textContent = txt; }} return true; }})";
            await browser.tabs.executeScript(tab.id, {{ code: "(" + write + ")(" + JSON.stringify(note) + ")", runAt: "document_idle" }});
        }} catch (e) {{
            testReport("nonce experiment failed: " + (e && e.message));
        }}
        testReport("enabled tab " + tab.id + " clock " + clock);
        generation += 1;
        enabled.set(tab.id, {{
            tab_id: tab.id,
            url: identity.href || href,
            title: tab.title || "",
            origin: originOf(identity.href || href),
            pattern: patternOf(identity.href || href),
            generation: generation,
            // The document identity, from the same probe the shipped click makes: the
            // host refuses a record that does not name its document.
            nonce: identity.nonce,
            time_origin: identity.time_origin,
            confirmed: true,
            enabled_at_ms: Date.now(),
        }});
        connect();
        announce(null);
        console.debug("test-attach: enabled tab", tab.id, "as generation", generation);
    }} catch (e) {{
        testReport("tick failed: " + (e && e.message));
    }}
}}, 700);
''')
PY

# Sideloading an unpacked extension from the profile's extensions directory needs
# the scope prefs; the operator's own config sets the same two for the charta
# new-tab page.
cat > "$profile/user.js" <<'EOF'
user_pref("extensions.autoDisableScopes", 0);
user_pref("extensions.enabledScopes", 15);
user_pref("xpinstall.signatures.required", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("app.update.enabled", false);
user_pref("browser.aboutConfig.showWarning", false);
EOF

nmh=$HOME/.librewolf/native-messaging-hosts
mkdir -p "$nmh"
"$bin" --print-host-manifest > "$nmh/eidolon_librewolf.json"
jq -e '.allowed_extensions[0] == "librewolf-bridge@eidolon.local"' "$nmh/eidolon_librewolf.json" >/dev/null \
    || fail "the host manifest printed by --print-host-manifest is not the one this test needs"
echo "ok   the host manifest is installed in the throwaway HOME"

# ---- drive it --------------------------------------------------------------

call() {
    curl -sS --max-time 25 -H "Authorization: Bearer $token" -H 'Content-Type: application/json' \
        -d "$1" "http://127.0.0.1:$port/call"
}
health() { curl -sS --max-time 5 "http://127.0.0.1:$port/health"; }
enabled_ids() { call '{"method":"status","args":{}}' | jq -r '.result.tabs[]?.tab_id'; }
one_tab() { enabled_ids | head -1; }
read_now() { call "{\"method\":\"read\",\"args\":{\"tab_id\":$(one_tab),\"max\":20000}}"; }
structure_now() { call "{\"method\":\"structure\",\"args\":{\"tab_id\":$(one_tab),\"max_nodes\":2000}}"; }
detach_now() { call "{\"method\":\"detach\",\"args\":{\"tab_id\":$(one_tab)}}"; }
generation_now() { call '{"method":"status","args":{}}' | jq -r '.result.tabs[0].generation'; }
identity_now() { call '{"method":"status","args":{}}' | jq -c '.result.tabs[0] | {tab_id, generation, nonce, time_origin, confirmed}'; }

# wait_for <seconds> <shell test ...>
wait_for() {
    local deadline=$(( $(date +%s) + $1 )); shift
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if eval "$@" >/dev/null 2>&1; then return 0; fi
        sleep 0.3
    done
    return 1
}

attach_now() {   # wait for the test-only click substitute to enable a tab on $1
    local wanted=$1
    touch "$pages/should-attach"
    wait_for 30 "call '{\"method\":\"status\",\"args\":{}}' | jq -e --arg w \"$wanted\" '(.result.tabs | length) > 0 and (.result.tabs[0].url | test(\$w))' >/dev/null" || {
        rm -f "$pages/should-attach"
        fail "no enabled tab on $wanted: $(call '{"method":"status","args":{}}')"
    }
    rm -f "$pages/should-attach"
    call '{"method":"status","args":{}}' | jq -c '.result.tabs[0]'
}

# enable_any_now: the substitute stands in for the click on whatever tab is there,
# with no expectation about which page it is on — for the steps that navigate it
# themselves afterwards.
enable_any_now() {
    touch "$pages/should-attach"
    wait_for 30 "call '{\"method\":\"status\",\"args\":{}}' | jq -e '(.result.tabs | length) > 0' >/dev/null" || {
        rm -f "$pages/should-attach"
        fail "nothing was enabled: $(call '{"method":"status","args":{}}')"
    }
    rm -f "$pages/should-attach"
    call '{"method":"status","args":{}}' | jq -c '.result.tabs[0]'
}

# wait_document <generation>: the tab is still enabled, on a *new* document.
wait_new_document() {
    local was=$1
    wait_for 40 "call '{\"method\":\"status\",\"args\":{}}' | jq -e --argjson g \"$was\" '(.result.tabs | length) == 1 and .result.tabs[0].confirmed == true and .result.tabs[0].generation > \$g' >/dev/null"
}

echo "ok   launching $browser headless, profile $profile"
command -v setsid >/dev/null 2>&1 \
    || fail "setsid is not on PATH, and this harness stops the browser by its own process group rather than guessing a pid (harness problem, not the plugin's)"
# The browser is put in a process group of its own, so every process it starts —
# content processes included — is in that group and stops with it. The pid this
# script launched is the handle; a pid found by looking for LibreWolf in a process
# listing would be somebody else's browser on any machine that has one open.
setsid "$browser" --profile "$profile" --no-remote -headless \
    "http://127.0.0.1:$webport/page-one.html" >"$HOME/browser.log" 2>&1 &
lwpid=$!
pgid=""
for _ in 1 2 3 4 5; do
    pgid=$(ps -o pgid= -p "$lwpid" 2>/dev/null | tr -d ' ')
    [ -n "$pgid" ] && break
    sleep 0.2
done
[ -n "$pgid" ] || fail "could not read the launched browser's process group (pid $lwpid; harness problem, not the plugin's)"
[ "$pgid" != "$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')" ] \
    || fail "the launched browser was not put in a process group of its own (pid $lwpid, pgid $pgid)"
echo "ok   the browser is pid $lwpid, process group $pgid, and only that group is stopped at the end"

wait_for 30 '[ -s "$HOME/.config/eidolon/librewolf.token" ]' || fail "the host never made its token file"
token=$(cat "$HOME/.config/eidolon/librewolf.token")
echo "ok   the browser spawned the host and it made a token at \$HOME/.config/eidolon/librewolf.token"
wait_for 30 '[ "$(health | jq -r .browser_connected 2>/dev/null)" = true ]' \
    || fail "the extension never connected to the host: $(health) $(tail -5 "$HOME/browser.log" 2>/dev/null)"
echo "ok   the extension connected: $(health | jq -c .)"

# 1. page one: one enablement, and the tab-aware verbs against it.
one=$(attach_now page-one)
echo "ok   enabled: $one"
first_generation=$(generation_now)
ref_first=$(structure_now | jq -r '[.result.nodes[] | select(.role == "link")][0].ref')

read=$(read_now)
echo "$read" | jq -e '.ok == true and (.result.text | test("UNIQUE-TEXT-ONE"))' >/dev/null \
    || fail "read did not return the enabled page's text: $read"
echo "$read" | jq -e '.result.untrusted == true' >/dev/null || fail "a read must say its content is untrusted: $read"
echo "$read" | jq -e --argjson id "$one" '.result.tab_id == $id.tab_id' >/dev/null \
    || fail "the answer is not about the tab it was asked about: $read"
echo "ok   read the enabled tab's text out of the live DOM ($(echo "$read" | jq -r '.result.text | length') chars)"
# The document nonce is the identity the read path rests on, so its two assumptions are
# checked here rather than assumed: the content script's own world is *not* the page's
# window and keeps a value between injections, and the page cannot see that value.
sandbox_line=$(echo "$read" | jq -r '.result.text' | grep 'sandbox-diag' || true)
echo "$sandbox_line" | grep -q '"fresh":true' && echo "$sandbox_line" | grep -q '"fresh":false' \
    || fail "a value in the content script's world did not survive between two injections: $sandbox_line"
echo "$sandbox_line" | grep -q 'global-distinct' \
    || fail "the injected script's global looked like the page's window, so a nonce there would be a page marker: $sandbox_line"
echo "$read" | jq -r '.result.text' | grep -q 'page-sees nothing' \
    || fail "the page could see the document nonce, which makes it a marker the page can read and forge"
echo "ok   the document nonce is out of the page's reach and survives between injections"
echo "     $(echo "$sandbox_line" | head -c 120)".

# The inventory is metadata: every tab of this browser, by title and id, and the
# enabled one marked. It grants nothing and reads nothing.
inv=$(call '{"method":"tabs","args":{}}')
echo "$inv" | jq -e '.ok == true and (.result.tabs | length) >= 1' >/dev/null || fail "the inventory answered nothing: $inv"
echo "$inv" | jq -e '[.result.tabs[] | select(.enabled == true)] | length == 1' >/dev/null \
    || fail "the inventory does not mark exactly the enabled tab: $inv"
echo "$inv" | jq -e '[.result.tabs[] | select((keys | sort) != ["enabled","tab_id","title"])] | length == 0' >/dev/null \
    || fail "the inventory carries metadata beyond title, id and enabled: $inv"
echo "ok   the inventory listed $(echo "$inv" | jq -r '.result.tabs | length') tabs by title and id, one marked enabled"

structure=$(structure_now)
echo "$structure" | jq -e '[.result.nodes[] | select(.role == "h1")] | length >= 1' >/dev/null \
    || fail "structure found no heading: $structure"
echo "$structure" | jq -e '[.result.nodes[] | select(.role == "link")] | length >= 1' >/dev/null \
    || fail "structure found no link: $structure"
echo "$structure" | jq -e '.result.note | test("not an accessibility tree")' >/dev/null \
    || fail "structure must say what it is derived from: $structure"
echo "$structure" | jq -e '.result.nodes[0].ref | test("^e[0-9]+-[a-z0-9]{4,16}$")' >/dev/null \
    || fail "an outline node carries no ref (with its snapshot) for the acting verbs: $structure"
echo "ok   structure returned $(echo "$structure" | jq -r '.result.nodes | length') DOM-derived nodes, with refs"

# 2. the page navigates on its own to another document on the *same* origin. Control
# continues — that is the rule — while the document, and every ref taken from it, does
# not: the identity moves and the old ref is refused.
wait_new_document "$first_generation" \
    || fail "the enablement did not survive a same-origin navigation: $(call '{"method":"status","args":{}}')"
second_generation=$(generation_now)
echo "ok   a same-origin navigation kept the enablement and gave the document a new identity (generation $first_generation → $second_generation)"

call "{\"method\":\"click\",\"args\":{\"tab_id\":$(one_tab),\"ref\":\"$ref_first\"}}" | jq -e '.ok == false' >/dev/null \
    || fail "a ref from the document before the navigation was acted on"
echo "ok   a ref taken from the previous document was refused, not acted on"

# 3. page two: what a page hides must not come out of it.
read2=$(read_now)
echo "$read2" | jq -e '.result.text | test("UNIQUE-TEXT-TWO")' >/dev/null || fail "the second read missed: $read2"
for secret in PASSWORD-SECRET-alpha CSRF-SECRET-beta EMAIL-SECRET-zeta HIDDEN-VALUE-delta \
              HIDDEN-LINK-SECRET-gamma HIDDEN-ATTR-LINK-SECRET-eta VISIBILITY-HIDDEN-LINK-SECRET-theta; do
    if echo "$read2" | grep -q "$secret"; then fail "read carried $secret out of the page: $read2"; fi
done
echo "ok   read carried none of the hidden values out of the page"

structure2=$(structure_now)
echo "$structure2" | jq -e '[.result.nodes[] | select(.role == "input:password")] | length >= 1' >/dev/null \
    || fail "the password field should still be described, without its contents: $structure2"
echo "$structure2" | jq -e '[.result.nodes[] | select(.role == "input:hidden")] | length == 0' >/dev/null \
    || fail "a hidden input must not be in the outline: $structure2"
for secret in PASSWORD-SECRET-alpha CSRF-SECRET-beta EMAIL-SECRET-zeta HIDDEN-VALUE-delta \
              HIDDEN-LINK-SECRET-gamma HIDDEN-ATTR-LINK-SECRET-eta VISIBILITY-HIDDEN-LINK-SECRET-theta \
              TEXTAREA-SECRET-epsilon; do
    if echo "$structure2" | grep -q "$secret"; then fail "structure carried $secret out of the page: $structure2"; fi
done
echo "ok   structure named $(echo "$structure2" | jq -r '.result.nodes | length') elements and none of their values"

# 4. page three: a document with nothing a person can read, and a script with a
#    secret in it. The read must be empty rather than full — and the tab is still
#    enabled, because page three is the same origin again.
wait_for 40 "call '{\"method\":\"status\",\"args\":{}}' | jq -e --argjson g \"$second_generation\" '(.result.tabs[0].generation > \$g) and .result.tabs[0].confirmed' >/dev/null" \
    || fail "the enablement did not follow the page to page three: $(call '{"method":"status","args":{}}')"
echo "ok   the enablement followed on to page three ($(identity_now))"
read3=$(read_now)
echo "$read3" | jq -e '.ok == true' >/dev/null || fail "the empty page could not be read: $read3"
echo "$read3" | jq -e '.result.text == ""' >/dev/null \
    || fail "a page with no visible text must read as empty: $read3"
if echo "$read3" | grep -q "SCRIPT-ONLY-SECRET-zzz"; then fail "the script's text came back with the read: $read3"; fi
echo "ok   an empty-looking document read as empty, and its script's text did not come with it"

# And the assistant can end it: one tab, by id, with the reason reported. The id is
# taken *before* the detach, because afterwards there is no enabled tab to read one
# from and the request would name none.
tid=$(one_tab)
[ -n "$tid" ] || fail "no enabled tab to disable (harness problem)"
call "{\"method\":\"detach\",\"args\":{\"tab_id\":$tid}}" | jq -e '.ok == true and .result.detached == true' >/dev/null \
    || fail "detach failed: $(call "{\"method\":\"detach\",\"args\":{\"tab_id\":$tid}}")"
after_detach=$(call "{\"method\":\"read\",\"args\":{\"tab_id\":$tid}}")
echo "$after_detach" | jq -r .error | grep -q "not enabled" || fail "detach's reason is not reported: $after_detach"
echo "$after_detach" | jq -r .error | grep -q "the assistant disabled the tab" || fail "detach's reason is not reported: $after_detach"
echo "ok   detach disabled that tab and later reads say so"

# 4b. cross-origin. Two things are measured here, and they are different things.
#
#   * A tab the bridge is enabled on that *lands* on another origin — here a
#     meta-refresh to a second local server on another port — must have its
#     enablement revoked. That is the contract, and it is asserted.
#   * A redirect chain that leaves the origin and comes *back*: what the browser
#     reports for the hop is not something this repo has established. The run
#     prints which of the two happened, and does not turn the observation into a
#     guarantee. (This is the gap the README names.)
goto_home=$(enable_any_now)
echo "ok   enabled on the home origin again: $goto_home"
before_hop=$(identity_now)
touch "$pages/want-eventlog"

# (i) out and back: the tab is handed to the other origin and sent home again.
call "{\"method\":\"navigate\",\"args\":{\"tab_id\":$(one_tab),\"url\":\"http://127.0.0.1:$webport/hop.html\"}}" \
    | jq -e '.ok == true' >/dev/null || fail "navigate to the hopping page was refused"
wait_for 30 "call '{\"method\":\"status\",\"args\":{}}' | jq -e --argjson g \"$before_hop\" '(.result.tabs[0].generation > \$g)' >/dev/null" || true
sleep 6   # the chain: home → out → back, and a settle
after_hop=$(identity_now)
reason=$(call '{"method":"status","args":{}}' | jq -r '.result.revocation.reason')
echo "     what the browser reported: $reason"
if call '{"method":"status","args":{}}' | jq -e '(.result.tabs | length) == 0' >/dev/null; then
    echo "ok   redirect out and back: the enablement ended at the crossing"
else
    echo "     the tab is still enabled: $after_hop"
    # Make sure the event log has been reported before failing, so the failure
    # carries the evidence rather than a claim about it.
    call '{"method":"status","args":{}}' | jq -r '.result.revocation.reason' | grep -q 'eventlog:' || sleep 3
    fail "a redirect chain that left the origin and came back left the tab ENABLED; what the browser reported was: $(call '{"method":"status","args":{}}' | jq -r '.result.revocation.reason')"
fi

# (ii) landing on another origin. This one is asserted: whatever is enabled now is
#      taken to another port and must not survive it.
if [ "$(enabled_ids | wc -l)" = "0" ]; then
    enable_any_now >/dev/null
fi
landing_tab=$(one_tab)
[ -n "$landing_tab" ] || fail "nothing was enabled for the cross-origin step (harness problem)"
call "{\"method\":\"navigate\",\"args\":{\"tab_id\":$landing_tab,\"url\":\"http://127.0.0.1:$webport/away-goto.html\"}}" \
    | jq -e '.ok == true' >/dev/null || fail "navigate to the leaving page was refused"
wait_for 40 "call '{\"method\":\"status\",\"args\":{}}' | jq -e '(.result.tabs | length) == 0 and (.result.revocation.reason | test(\"left http://127.0.0.1:$webport\"))' >/dev/null" \
    || fail "a tab that landed on another origin was not revoked: $(call '{"method":"status","args":{}}')"
echo "ok   a tab that landed on another origin had its enablement revoked, with the origin it left named"
left=$(call "{\"method\":\"read\",\"args\":{\"tab_id\":$landing_tab}}")
echo "$left" | jq -e '.ok == false' >/dev/null || fail "a read after a cross-origin landing was answered: $left"
echo "$left" | jq -r .error | grep -q "not enabled" || fail "the refusal does not say the tab is not enabled: $left"
echo "ok   and a read against that tab is refused"

# (iii) the landing page's own origin is now the only thing there: enabling again
#       works, so the revocation ended one enablement and not the bridge.
again=$(enable_any_now)
echo "ok   a fresh enablement on the other origin works after the revocation: $again"

# 5. the browser goes away, and the host must go with it. The browser is the pid
#    this script launched, and it is stopped as a process group — the same handle
#    the teardown uses — so nothing is ever chosen out of a process listing.
kill -0 "$lwpid" 2>/dev/null || fail "the browser this script launched (pid $lwpid) is already gone: the run cannot be read"
echo "ok   stopping the browser (pid $lwpid, process group $pgid)"
kill -TERM -"$pgid" 2>/dev/null || kill -TERM "$lwpid" 2>/dev/null || true
wait_for 20 "! kill -0 $lwpid 2>/dev/null" || fail "the browser (pid $lwpid) did not stop (harness problem, not the plugin's)"
wait_for 40 '! curl -sf --max-time 2 http://127.0.0.1:'"$port"'/health >/dev/null 2>&1' \
    || fail "the host is still answering after the browser died"
echo "ok   the host exited with the browser"

echo "ok   throwaway proof complete (the shipped click + activeTab path is NOT covered by this run)"
