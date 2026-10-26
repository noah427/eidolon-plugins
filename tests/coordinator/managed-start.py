#!/usr/bin/env python3
"""Real dispatcher -> Rune tool -> temporary monitor -> held mock child.
No live provider/config/token/fleet is read. Arguments name built test binaries.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

EIDOLON, MONITOR = map(lambda p: str(Path(p).resolve()), sys.argv[1:3])
SOURCE = Path(__file__).resolve().parents[2] / "coordinator"


def wait_for(predicate, seconds=20):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("bounded fixture wait expired")


def alive(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().split(") ", 1)[1][0] != "Z"
    except FileNotFoundError:
        return False


with tempfile.TemporaryDirectory(prefix="eidolon-managed-start-") as directory:
    root = Path(directory)
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root)}
    for key, sub in [("XDG_CONFIG_HOME", ".config"), ("XDG_DATA_HOME", "data"),
                     ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"),
                     ("XDG_RUNTIME_DIR", "runtime")]:
        (root / sub).mkdir()
        env[key] = str(root / sub)
    config = root / ".config/eidolon"
    config.mkdir()
    (config / "config.toml").write_text("[swarm]\nenabled = true\n")
    work = root / "work"
    work.mkdir()
    plugin = config / "plugins/coordinator"
    shutil.copytree(SOURCE, plugin)
    # The monitor binds 8095 and every tool's `service()` dials it; neither can be told
    # otherwise, so this run owns that port instead of choosing one — and the tools under
    # test are the SHIPPED bytes, never a copy rewritten to point somewhere else.
    port = 8095
    (plugin / "workflows").mkdir(exist_ok=True)
    (plugin / "workflows/managed.rn").write_text('''
pub fn workflow() {
    #{ name: "managed", description: "isolated managed start fixture", approval: "mutating",
       params: #{ "type": "object" } }
}
pub async fn run(input) {
    let first = eidolon::tool_call("coordinator_start", input.clone()).await?;
    let again = eidolon::tool_call("coordinator_start", input).await?;
    Ok(#{ first: first, again: again })
}
''')
    env.update(EIDOLON_COORDINATOR_BIN=EIDOLON,
               EIDOLON_COORDINATOR_INTERVAL_S="60")
    monitor_root = root / "monitor"
    log_file = (root / "monitor.log").open("wb")
    monitor = None
    child_pid = None

    def run(*args, check=True):
        result = subprocess.run([EIDOLON, *args], env=env, cwd=work, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=45)
        if check and result.returncode:
            raise AssertionError(f"{args}: exit {result.returncode}\n{result.stdout}")
        return result.stdout

    def health():
        try:
            return urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1).status == 200
        except OSError:
            return False

    def start_monitor():
        process = subprocess.Popen([MONITOR, "--root", str(monitor_root)], env=env, cwd=work,
                                   stdin=subprocess.DEVNULL, stdout=log_file, stderr=log_file)
        wait_for(health)
        return process

    def call(method, args):
        token = (config / "coordinator.token").read_text().strip()
        request = urllib.request.Request(f"http://127.0.0.1:{port}/call",
            data=json.dumps({"method": method, "args": args}).encode(),
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=35) as response:
            return json.load(response)

    try:
        monitor = start_monitor()
        request = {"operation": "fixture-one", "workspace": str(work), "model": "mock",
                   "brief": "Only answer with a short mock response.\nThis is one multiline assignment."}
        refused = run("workflow", "run", str(plugin), "managed", "--args", json.dumps(request), "--provider", "mock", check=False)
        assert "not vouched" in refused, refused
        assert not json.loads((monitor_root / "state.json").read_text())["starts"]
        run("plugins", "trust", "coordinator")
        refused = run("workflow", "run", str(plugin), "managed", "--args", json.dumps(request), "--provider", "mock", check=False)
        assert not json.loads((monitor_root / "state.json").read_text())["starts"], refused
        print("ok: unvouched tool and missing transport grant leave zero managed operations")
        run("plugins", "grant", "coordinator_start", "file:~/.config/eidolon/coordinator.token")
        output = run("workflow", "run", str(plugin), "managed", "--args", json.dumps(request), "--provider", "mock")
        state = json.loads((monitor_root / "state.json").read_text())
        assert len(state["starts"]) == 1, output
        record = state["starts"][0]
        assert record["phase"] == "Running", (record, output)
        assert "Journaled" in record["brief"], record
        receipt = record["brief"]["Journaled"]["receipt"]
        assert receipt["session"] == record["session"], (receipt, record)
        assert receipt["delivery_key"] == record["brief_delivery_key"], (receipt, record)
        assert receipt["binding_digest"] == record["brief_binding_digest"], (receipt, record)
        assert len(state["records"]) == 1, state
        address = record["session"]
        log = root / "data/eidolon/sessions" / f"{address}.eid"
        parent_log = log.with_name(record["request"]["caller"] + ".eid")
        parent_text = run("log", str(parent_log))
        assert "coordinator_start" in parent_text, parent_text
        text = wait_for(lambda: (t if "assistant" in t.lower() else None) if (t := run("log", str(log))) else None)
        markers = []
        for match in re.finditer("managed-start/v1 ", text):
            value, _ = json.JSONDecoder().raw_decode(text[match.end():])
            markers.append(value)
        assert [m["state"] for m in markers][:2] == ["held", "released"], text
        child_pid = markers[0]["pid"]
        assert alive(child_pid)
        assert text.count("Only answer with a short mock response.") == 1, text
        print("ok: dispatcher-admitted tool starts/adopts once; repeated operation has one brief acceptance, not consumption proof")
        monitor.kill()
        monitor.wait(timeout=5)
        wait_for(lambda: not alive(child_pid))
        text = run("log", str(log))
        assert "chat input closed or unreadable" in text, text
        print("ok: service death closes owned stdin; child exits and journals why")
        before = sorted(p.name for p in (root / "data/eidolon/sessions").glob("*.eid"))
        monitor = start_monitor()
        # **Replay the caller's own arguments, not the stored record.** A retry in the
        # product is the caller repeating what it asked for; the store is reconciled
        # against, never resubmitted. `record["request"]` carries the shapes the
        # service pins for itself — `shape`, and a visible launch's `destination` —
        # and `request_from_args` refuses those as *caller* input, so replaying the
        # record made this line assert a refusal that happened before the recovery
        # path was ever reached, and the two behavioural assertions below passed
        # vacuously. Sending `request` reaches the real question: after a restart,
        # does this operation still answer from its receipt?
        # A caller supplies its **own** address and nothing the service pins for
        # itself. `caller` is harness-derived: the plugin's tool reads it from the
        # roster (`my_address()`), and this RPC-level replay stands in for that
        # caller, so it carries the address the record was made under. `shape` and
        # `destination` are the service's, and `request_from_args` refuses them.
        retry = dict(request, caller=record["request"]["caller"])
        recovered = call("start", retry)
        assert recovered["ok"] is False, recovered
        assert "host restarted or has no receipt" in recovered["error"], recovered
        after = sorted(p.name for p in (root / "data/eidolon/sessions").glob("*.eid"))
        assert after == before, (before, after)
        assert len(json.loads((monitor_root / "state.json").read_text())["records"]) == 1
        print("ok: restarted service reports Unknown, preserves supervision, never respawns or resends")
    except Exception:
        log_file.flush()
        print((root / "monitor.log").read_text(errors="replace"), file=sys.stderr)
        raise
    finally:
        if monitor and monitor.poll() is None:
            monitor.terminate()
            try:
                monitor.wait(timeout=5)
            except subprocess.TimeoutExpired:
                monitor.kill()
                monitor.wait(timeout=5)
        if child_pid and alive(child_pid):
            os.kill(child_pid, 15)
            wait_for(lambda: not alive(child_pid))
        log_file.close()
