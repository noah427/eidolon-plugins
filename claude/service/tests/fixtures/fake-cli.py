#!/usr/bin/env python3
"""A fake vendor CLI, for testing the adapter without a credential.

It emits the stream-json shapes `crates/claude/src/stream.rs` parses and really
does what the real CLI does around a tool call: in own-tools mode it runs the
`PreToolUse` command named in the settings file it was given; in registry mode it
spawns the MCP server named in its `--mcp-config` and speaks MCP to it. So both
doors are exercised end to end, not stubbed.

`FAKE_CLI_MODE` changes the ending for the failure paths:
  `normal` (default)  a `result` line, so the turn settles
  `noresult`          exit without one, so the host has to say the turn failed
  `sleep`             sit there, so a cancel has something to signal
Nothing here touches the network, a credential, or a real CLI.
"""
import json
import os
import subprocess
import sys

args = sys.argv[1:]
# Prompt markers, not a process-global environment: tests run in parallel.
#   SLEEP     sit there, so a cancel has something to signal
#   NORESULT  end without a `result`, so the host has to say the turn failed


def opt(name):
    return args[args.index(name) + 1] if name in args else None


raw = sys.stdin.read()  # the prompt, as one JSON user message
try:
    message = json.loads(raw)["message"]
    prompt = "".join(b.get("text", "") for b in message["content"] if b.get("type") == "text")
except Exception:
    prompt = raw
mode = (
    "sleepterm" if "SLEEPTERM" in prompt
    else "sleep" if "SLEEP" in prompt
    else "noresult" if "NORESULT" in prompt
    else "normal"
)
print(json.dumps({"type": "system", "subtype": "init", "session_id": "fake-session-1"}), flush=True)

# What this CLI was handed, one JSON line per run, when someone asks. The variable
# is set on the *host's* environment, so it reaches this spawn without the host
# knowing it exists. `resume` is the argv flag the host passed; `blocks`,
# `images` and `prompt` are the user message the host built. This is how a chain
# run asserts what the model would have been *shown* rather than only that a turn
# ended — a caught-up transcript, a resumed session, a picture.
_dump = os.environ.get("FAKE_CLI_DUMP")
if _dump:
    try:
        blocks = json.loads(raw)["message"]["content"]
    except Exception:
        blocks = []
    with open(_dump, "a") as f:
        f.write(json.dumps({
            "argv": args,
            "resume": opt("--resume"),
            "model": opt("--model"),
            "prompt": prompt,
            "blocks": [b.get("type") for b in blocks],
            "images": [b.get("source", {}).get("media_type")
                       for b in blocks if b.get("type") == "image"],
        }) + "\n")

if mode == "noresult":
    # Ends before it ever calls a tool: no `result` line, so the host has to say
    # the turn failed rather than going quiet.
    sys.exit(0)

if mode in ("sleep", "sleepterm"):
    # "SLEEP <path>": a CLI that ignores SIGTERM and leaves a descendant that
    # ignores it too, writing both pids, so a test can check afterwards whether
    # the *group* went or only the leader.
    #
    # "SLEEPTERM <path>": the leader lets SIGTERM stop it and only the descendant
    # ignores it. That is the case where "the leader exited" and "the group is
    # gone" come apart — a host that killed only the leader would pass the first
    # claim and fail the second.
    import signal
    import time
    if mode == "sleep":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    descendant = subprocess.Popen([
        sys.executable, "-c",
        "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(120)",
    ])
    where = prompt.split()
    if len(where) > 1:
        with open(where[1], "w") as f:
            f.write("%d %d" % (os.getpid(), descendant.pid))
    time.sleep(120)


def registry_args(name):
    """Real arguments, where the real harness has a tool this name.

    A validation error proves the dispatcher was *reached*; a tool that runs
    proves it *ran*. When the fixture is pointed at a real session the first
    offered tool is `read`, and this file is the one path it can be sure of, so
    the call succeeds and the marker has a real result behind it. Any other
    registry (the host's own tests offer a single `echo`) keeps the old shape.
    """
    if name == "read":
        return {"path": os.path.abspath(__file__), "limit": 2}
    return {"say": "hi"}


def assistant(blocks, last=False):
    message = {"id": "msg_1", "role": "assistant", "content": blocks}
    if last:
        message["usage"] = {"input_tokens": 11, "output_tokens": 3}
    print(json.dumps({"type": "assistant", "message": message}), flush=True)


assistant([{"type": "text", "text": "I will run the check."}])

tool_use_id = "toolu_1"
result_text = "faked tool output"
is_error = False

# The model's call. Both modes print it the same way; only the door it goes
# through afterwards differs.
assistant([{"type": "tool_use", "id": tool_use_id, "name": "Bash", "input": {"command": "echo hi"}}], last=True)

config = opt("--mcp-config")
if config is not None:
    # registry mode: the CLI's own MCP client reaches the harness's registry.
    server = json.loads(config)["mcpServers"]["eidolon"]
    env = dict(os.environ)
    env.update(server.get("env", {}))
    proc = subprocess.Popen(
        [server["command"], *server.get("args", [])],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env,
    )

    def mcp(payload):
        proc.stdin.write(json.dumps(payload) + "\n")
        proc.stdin.flush()
        return json.loads(proc.stdout.readline())

    mcp({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}})
    listed = mcp({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    names = [t["name"] for t in listed["result"]["tools"]]
    if not names:
        sys.exit("the door offered no tools")
    called = mcp({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": names[0], "arguments": registry_args(names[0]),
            "_meta": {"claudecode/toolUseId": tool_use_id},
        },
    })
    if "error" in called:
        result_text, is_error = called["error"]["message"], True
    else:
        result_text = "".join(c.get("text", "") for c in called["result"]["content"])
        is_error = called["result"].get("isError", False)
else:
    # own-tools mode: the CLI runs the PreToolUse hook it was configured with.
    settings = opt("--settings")
    if settings is None:
        sys.exit("the fake CLI was not given --settings")
    command = json.load(open(settings))["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
    run = subprocess.run(
        command, shell=True, capture_output=True, text=True,
        input=json.dumps({
            "tool_name": "Bash", "tool_input": {"command": "echo hi"}, "tool_use_id": tool_use_id,
        }),
    )
    if not run.stdout.strip():
        sys.exit("the hook printed nothing")
    decision = json.loads(run.stdout)["hookSpecificOutput"]
    allowed = decision["permissionDecision"] == "allow"
    result_text = "faked tool output" if allowed else "the gate said no"
    is_error = not allowed

# The tool result, as the CLI reports it: a `user` message with a tool_result block.
print(json.dumps({
    "type": "user",
    "message": {"role": "user", "content": [{
        "type": "tool_result", "tool_use_id": tool_use_id,
        "content": result_text, "is_error": is_error,
    }]},
}), flush=True)

if mode == "noresult":
    sys.exit(0)

print(json.dumps({
    "type": "result", "stop_reason": "end_turn",
    "usage": {"input_tokens": 11, "output_tokens": 5},
    "total_cost_usd": 0.0, "is_error": False, "result": "done",
}), flush=True)
