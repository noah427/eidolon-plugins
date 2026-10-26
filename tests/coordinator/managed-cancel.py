#!/usr/bin/env python3
"""Real dispatcher/Rune/service cancellation with an owned, malformed-Held fixture child.
Includes an exec-preserved real harness child on the mock backend. All config/tokens/state are isolated.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

EIDOLON, MONITOR = [str(Path(p).resolve()) for p in sys.argv[1:3]]
SOURCE = Path(__file__).resolve().parents[2] / "coordinator"

with tempfile.TemporaryDirectory(prefix="eidolon-managed-cancel-") as directory:
    root = Path(directory)
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root)}
    for key, sub in [("XDG_CONFIG_HOME", ".config"), ("XDG_DATA_HOME", "data"),
                     ("XDG_STATE_HOME", "state"), ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")]:
        (root / sub).mkdir()
        env[key] = str(root / sub)
    config = root / ".config/eidolon"
    config.mkdir()
    (config / "config.toml").write_text("[swarm]\nenabled = true\n")
    work = root / "work"
    work.mkdir()
    plugin = config / "plugins/coordinator"
    shutil.copytree(SOURCE, plugin)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    for tool in (plugin / "tools").glob("*.rn"):
        tool.write_text(tool.read_text().replace("127.0.0.1:8095", f"127.0.0.1:{port}"))
    (plugin / "workflows").mkdir(exist_ok=True)
    (plugin / "workflows/cancel-only.rn").write_text('''
pub fn workflow() { #{ name: "cancel-only", description: "gate probe", approval: "mutating", params: #{ "type": "object" } } }
pub async fn run(input) { eidolon::tool_call("coordinator_cancel", input).await }
''')
    (plugin / "workflows/cancel.rn").write_text('''
pub fn workflow() { #{ name: "cancel", description: "isolated cancellation", approval: "mutating", params: #{ "type": "object" } } }
pub async fn run(input) {
    let start = match eidolon::tool_call("coordinator_start", input.clone()).await { Ok(v) => v, Err(e) => `${e}` };
    let args = #{ "operation": input.operation, "reason": "fixture held launch cleanup" };
    let cancelled = eidolon::tool_call("coordinator_cancel", args.clone()).await?;
    let again = eidolon::tool_call("coordinator_cancel", args).await?;
    let blocked = match eidolon::tool_call("coordinator_start", input).await { Ok(v) => v, Err(e) => `${e}` };
    Ok(#{ start: start, cancelled: cancelled, again: again, blocked: blocked })
}
''')
    child = root / "held-fixture"
    child.write_text(f"#!{sys.executable}\n" + '''import json, os, socket, sys
from pathlib import Path
fd = int(sys.argv[sys.argv.index("--managed-start-fd") + 1])
channel = socket.socket(fileno=fd)
# This forces a partial prepare with a live OWNED child but no verified Held identity.
channel.sendall(b'{"state":"invalid-held-fixture"}\\n')
if Path.cwd().name == "real-work":
    os.set_inheritable(fd, True)
    os.execv(REAL_HARNESS, [REAL_HARNESS] + sys.argv[1:])
while channel.recv(4096):
    pass
sys.stdin.buffer.read()
'''.replace('REAL_HARNESS', repr(EIDOLON)))
    child.chmod(0o700)
    env.update(EIDOLON_COORDINATOR_PORT=str(port), EIDOLON_COORDINATOR_BIN=str(child), EIDOLON_COORDINATOR_INTERVAL_S="60")
    monitor_root = root / "monitor"
    log_file = (root / "monitor.log").open("wb")
    monitor = None

    def run(*args, check=True):
        result = subprocess.run([EIDOLON, *args], env=env, cwd=work, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=45)
        if check and result.returncode:
            raise AssertionError(f"{args}: {result.returncode}\n{result.stdout}")
        return result.stdout

    def start_monitor():
        process = subprocess.Popen([MONITOR, "--root", str(monitor_root)], env=env, cwd=work,
                                   stdin=subprocess.DEVNULL, stdout=log_file, stderr=log_file)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                if urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1).status == 200:
                    return process
            except OSError:
                pass
            time.sleep(0.05)
        process.terminate()
        process.wait(timeout=5)
        raise AssertionError("fixture monitor never became ready")

    def call(method, args):
        token = (config / "coordinator.token").read_text().strip()
        request = urllib.request.Request(f"http://127.0.0.1:{port}/call",
            data=json.dumps({"method": method, "args": args}).encode(),
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.load(response)

    try:
        monitor = start_monitor()
        probe = json.dumps({"operation": "unknown", "reason": "gate probe"})
        out = run("workflow", "run", str(plugin), "cancel-only", "--args", probe, "--provider", "mock", check=False)
        assert "not vouched" in out, out
        run("plugins", "trust", "coordinator")
        out = run("workflow", "run", str(plugin), "cancel-only", "--args", probe, "--provider", "mock", check=False)
        assert not json.loads((monitor_root / "state.json").read_text())["starts"], out
        print("ok: cancellation registry gate refuses unvouched/ungranted calls without managed state")
        for tool in ["coordinator_start", "coordinator_cancel"]:
            run("plugins", "grant", tool, "file:~/.config/eidolon/coordinator.token")
        request = {"operation": "cancel-fixture", "workspace": str(work), "model": "mock", "brief": "no model child"}
        out = run("workflow", "run", str(plugin), "cancel", "--args", json.dumps(request), "--provider", "mock")
        state = json.loads((monitor_root / "state.json").read_text())
        assert len(state["starts"]) == 1, (out, state)
        record = state["starts"][0]
        evidence = record["cancellation"]["outcome"]["Exited"]
        assert evidence["code"] == 0 and evidence["signal"] is None, (out, evidence)
        assert record["phase"] == "Intent" and record["session"] is None, record
        assert not state["records"], state
        assert "cancellation intent exists" in out, out
        assert "adoption_closed" in out, out
        parent_log = root / "data/eidolon/sessions" / (record["request"]["caller"] + ".eid")
        assert "coordinator_cancel" in run("log", str(parent_log))
        print("ok: dispatched cancellation closes owned fixture pipes, reaps exit, persists receipt; duplicate reads receipt, start stays blocked")
        # A second partial start execs the real binary in the same owned process.
        # Wait for its own Held journal before cancelling the unknown-session branch.
        real_work = root / "real-work"
        real_work.mkdir()
        logs = root / "data/eidolon/sessions"
        before = set(logs.glob("*.eid"))
        real_request = {"caller": "isolated-fixture", "operation": "real-held", "workspace": str(real_work),
                        "model": "mock", "brief": "No turn may begin"}
        partial = call("start", real_request)
        assert partial["ok"] is False and "Held receipt" in partial["error"], partial
        deadline = time.monotonic() + 10
        real_log = None
        while time.monotonic() < deadline:
            new_logs = set(logs.glob("*.eid")) - before
            for candidate in new_logs:
                records = [json.loads(line) for line in run("log", str(candidate), "--json").splitlines()]
                markers = [json.loads(r["kind"]["Note"]["text"].split("managed-start/v1 ", 1)[1])
                           for r in records if "Note" in r["kind"] and "managed-start/v1 " in r["kind"]["Note"]["text"]]
                if any(m["state"] == "held" for m in markers):
                    real_log = candidate
                    break
            if real_log:
                break
            time.sleep(0.05)
        assert real_log, "real harness never journaled Held"
        state = json.loads((monitor_root / "state.json").read_text())
        real_record = next(r for r in state["starts"] if r["request"]["operation"] == "real-held")
        assert real_record["phase"] == "Intent" and real_record["session"] is None, real_record
        cancelled = call("cancel", {"caller": "isolated-fixture", "operation": "real-held", "reason": "real held proof"})
        assert cancelled["ok"], cancelled
        receipt = json.loads(cancelled["result"])
        assert receipt["status"] == "exit_observed" and receipt["adoption_closed"] is False, receipt
        assert receipt["exit"]["roster_before_close"]["queried"] is False, receipt
        records = [json.loads(line) for line in run("log", str(real_log), "--json").splitlines()]
        assert all(set(r["kind"]) <= {"SessionStart", "Note"} for r in records), records
        assert records[0]["kind"]["SessionStart"]["model"] == "mock", records[0]
        markers = [json.loads(r["kind"]["Note"]["text"].split("managed-start/v1 ", 1)[1])
                   for r in records if "Note" in r["kind"] and "managed-start/v1 " in r["kind"]["Note"]["text"]]
        assert [m["state"] for m in markers] == ["held", "aborted"], markers
        assert markers[-1].get("reason"), markers
        print("ok: actual mock harness journals Held then abort, no released marker or turn records; owned unknown-session cancellation reaps exit")
        state = json.loads((monitor_root / "state.json").read_text())
        monitor.terminate()
        monitor.wait(timeout=5)
        monitor = start_monitor()
        reply = call("cancel", {"caller": record["request"]["caller"], "operation": "cancel-fixture", "reason": "fixture held launch cleanup"})
        assert reply["ok"] is False and "host restarted" in reply["error"], reply
        assert json.loads((monitor_root / "state.json").read_text()) == state
        print("ok: restarted service refuses cancellation without owned receipt, even for a historical Exited record")
    except Exception:
        log_file.flush()
        print((root / "monitor.log").read_text(errors="replace"), file=sys.stderr)
        raise
    finally:
        if monitor and monitor.poll() is None:
            monitor.terminate()
            monitor.wait(timeout=5)
        log_file.close()
