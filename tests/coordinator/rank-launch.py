#!/usr/bin/env python3
"""A ranked managed start, end to end through the real monitor.

The child is a stand-in `eidolon` that records its own argv and answers the held
receipt on the channel it was handed. That is deliberately *not* the real harness:
`chat --system-append` is the core/CLI lane's flag (the installed binary has it on no
subcommand, the core lane's head on `run` only), so the stand-in *claims* the flag in
its `chat --help`, which is the contract this plugin is written against. What this
fixture measures is this plugin's half — the HTTP
door reads the rank block, binds the digest of its exact bytes into the operation's
immutable request, and the argv the host builds carries
`--system-append <path to that block>` — plus the two refusals that follow from the
binding. Nothing of the operator's is read or written: throwaway HOME, config,
state and token.

    python3 tests/coordinator/rank-launch.py <monitor-binary>
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

MONITOR = str(Path(sys.argv[1]).resolve())
SOURCE = Path(__file__).resolve().parents[2] / "coordinator"
checks = 0


def check(name):
    global checks
    checks += 1
    print(f"ok: {name}")


def wait_for(predicate, seconds=20):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("bounded fixture wait expired")


with tempfile.TemporaryDirectory(prefix="eidolon-rank-launch-") as directory:
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

    # The operator's text, written where the plugin ships its blocks.
    block = plugin / "ranks/miles.md"
    block.write_text("Hold the flank; report the count before you sleep.\n")
    digest = hashlib.sha256(block.read_bytes()).hexdigest()
    argv_log = root / "argv.txt"

    # The stand-in child: record argv, answer `held` on the inherited channel.
    fake = root / "eidolon"
    fake.write_text(f'''#!{sys.executable}
import json, os, sys
args = sys.argv[1:]
if args[:2] == ["chat", "--help"]:
    print("Usage: eidolon chat [OPTIONS]")
    print("      --system-append <PATH>  Append this file text to the system prompt")
    sys.exit(0)
if args[:2] == ["peers", "--json"]:
    print(json.dumps({{"me": None, "sessions": []}}))
    sys.exit(0)
if args[:1] == ["send"]:
    print(json.dumps({{"outcome": "failed", "words": "fixture sends nothing"}}))
    sys.exit(0)
open({str(argv_log)!r}, "w").write("\\n".join(args) + "\\n")
def after(flag):
    return args[args.index(flag) + 1] if flag in args else ""
fd = int(after("--managed-start-fd"))
line = json.dumps({{"state": "held", "operation": after("--managed-operation"),
                   "instance": after("--managed-instance"),
                   "session": "fixture-held-child", "pid": os.getpid()}}) + "\\n"
os.write(fd, line.encode())
''')
    fake.chmod(0o755)

    # The monitor binds 8095 and every tool's `service()` dials it; neither can be told
    # otherwise, so this run owns that port instead of choosing one — and the tools under
    # test are the SHIPPED bytes, never a copy rewritten to point somewhere else.
    port = 8095

    monitor_root = root / "monitor"
    log_file = (root / "monitor.log").open("wb")
    env.update(EIDOLON_COORDINATOR_BIN=str(fake),
               EIDOLON_COORDINATOR_INTERVAL_S="600", EIDOLON_COORDINATOR_RANKS=str(plugin / "ranks"))

    def health():
        try:
            return urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1).status == 200
        except OSError:
            return False

    def call(method, args):
        token = (config / "coordinator.token").read_text().strip()
        request = urllib.request.Request(f"http://127.0.0.1:{port}/call",
            data=json.dumps({"method": method, "args": args}).encode(),
            headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=35) as response:
            return json.load(response)

    monitor = subprocess.Popen([MONITOR, "--root", str(monitor_root)], env=env, cwd=work,
                               stdin=subprocess.DEVNULL, stdout=log_file, stderr=log_file)
    try:
        wait_for(health)
        coordinator = "1792959651919"
        start = {"caller": coordinator, "operation": "ranked-one", "workspace": str(work),
                 "model": "mock", "brief": "bounded task", "rank": "miles"}

        answer = call("start", start)
        starts = json.loads((monitor_root / "state.json").read_text())["starts"]
        assert len(starts) == 1, (answer, starts)
        recorded = starts[0]["request"]
        assert recorded["rank"] == "miles", recorded
        assert recorded["rank_digest"] == digest, (recorded, digest)
        check("the monitor read the block and bound the digest of its exact bytes into the immutable request")

        argv = argv_log.read_text().splitlines()
        at = argv.index("--system-append")
        assert at == len(argv) - 2, argv
        handed = Path(argv[at + 1])
        assert handed != block.resolve(), (handed, block.resolve())
        assert handed.read_bytes() == block.read_bytes(), handed
        assert hashlib.sha256(handed.read_bytes()).hexdigest() == digest, handed
        check("the argv ends with --system-append <file of the exact hashed bytes>, not the block's path")

        assert answer["ok"] is False, answer
        assert "partial/unconfirmed" in answer["error"], answer
        assert "managed" not in answer.get("result", "") or "status" not in answer, answer
        check("the answer is a partial/unconfirmed start, not a managed one: a fixture child is not a session")

        # The caller may name a rank. It may not name the digest.
        named = dict(start, operation="ranked-two", rank_digest=digest)
        refused = call("start", named)
        assert refused["ok"] is False and "not a caller's input" in refused["error"], refused
        assert len(json.loads((monitor_root / "state.json").read_text())["starts"]) == 1
        check("a caller-supplied digest is refused and nothing is recorded")

        # The file the child was handed is a copy of what was hashed, so editing the
        # operator's block does not change the text this launch composes.
        block.write_text("Something else entirely.\n")
        assert hashlib.sha256(handed.read_bytes()).hexdigest() == digest, handed
        check("the launch's own copy still hashes to the bound digest after the block was edited")

        again = call("start", start)
        assert again["ok"] is False, again
        assert "`miles` rank block changed since this operation was recorded" in again["error"], again
        assert digest in again["error"], again
        assert len(json.loads((monitor_root / "state.json").read_text())["starts"]) == 1
        check("a retry after the block was edited is refused by name, and the record is unchanged")
    finally:
        monitor.terminate()
        try:
            monitor.wait(timeout=5)
        except subprocess.TimeoutExpired:
            monitor.kill()
            monitor.wait(timeout=5)
        log_file.close()

print(f"\nranked launch fixture complete: {checks} checks")
