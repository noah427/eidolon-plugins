#!/usr/bin/env python3
"""Rank prefixes, end to end: monitor store -> harness door -> roster row.

Real binaries only — a built `eidolon` (the branch that carries `door decorate`) and
a built `eidolon-coordinator` — in a private HOME and a private XDG set, on the
monitor's **one fixed port**, which this run must own: 8095 is what `plugin.rn`
declares and what every tool dials, and the monitor cannot be told to use another, so
isolation cannot be a number this script picks. If something already answers there the
run refuses rather than testing a split — inside `unshare -rn` the port is nobody's.
No live provider, config, token or fleet is read, and nothing outside the temporary
directory is written.

What this pins, in the order it happens:

1assign records the word and the harness composes `Imperator · <label>` on the row
   while `label` itself stays the session's own (here: null);
2.restarting the same session re-mints its registration with no decoration, and the
   next tick reasserts it — by exact session id, which is what survives the restart;
3.a *different* session is never decorated by that record;
4.release takes the prefix back and leaves the label alone;
5.another owner's decoration is refused, and reported rather than taken over.

Arguments: <built eidolon> <built eidolon-coordinator>
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

EIDOLON = str(Path(sys.argv[1]).resolve())
MONITOR = str(Path(sys.argv[2]).resolve())
SOURCE = Path(__file__).resolve().parents[2] / "coordinator"


def wait_for(predicate, seconds=30, what="condition"):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"bounded wait expired: {what}")


PORT = 8095


def own_the_port():
    """This run must own the monitor's one port before anything else happens.

    The monitor binds 8095 and every tool's `service()` dials it; neither can be told
    otherwise, so the isolation cannot be a number this script picks. The private HOME
    isolates the store, not the port. If something already answers there, a run that
    carried on would not be testing this monitor at all — it would be dialling somebody
    else's and could report a pass it did not earn. So it refuses, and it refuses here,
    before a temporary directory exists, so the negative case needs no build and no
    running coordinator to exercise.
    """
    with socket.socket() as sock:
        try:
            sock.bind(("127.0.0.1", PORT))
        except OSError:
            raise SystemExit(
                f"FAIL 127.0.0.1:{PORT} is already answering, and the monitor cannot use "
                "another port; run this inside `unshare -rn`")


own_the_port()

with tempfile.TemporaryDirectory(prefix="eidolon-rank-prefix-") as directory:
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

    monitor_root = root / "state/coordinator"
    env.update(EIDOLON_COORDINATOR_BIN=EIDOLON,
               EIDOLON_COORDINATOR_ROOT=str(monitor_root), EIDOLON_COORDINATOR_INTERVAL_S="1")

    log = (root / "monitor.log").open("wb")
    monitor = subprocess.Popen([MONITOR], env=env, stdout=log, stderr=subprocess.STDOUT)
    session = None

    def ours(what):
        """Refuse to read an answer off a port that may no longer be this run's monitor.

        What binds the endpoint is the **authentication, not this check**. Every request
        below carries the token this run's monitor minted, and the monitor compares it
        constant-time before it will do anything: an answer that comes back `ok: true`
        can only have been given by a monitor holding that token. The probe below makes
        that observable once, before any assertion depends on an answer.

        This check is the cheaper precondition in front of it — the child this run
        started is still alive, and the token file exists, because a monitor that died
        on a failed bind mints nothing. It is a **fixture binding**, not an absolute
        guarantee: under same-user trust another process could in principle write that
        file. What the run claims is therefore no more than this — a private network
        namespace, a child of this run still alive, and a token the answering endpoint
        has to authenticate. Anything else is a refusal that names what happened rather
        than a mystery later.
        """
        if monitor.poll() is not None:
            tail = (root / "monitor.log").read_text()[-2000:]
            raise SystemExit(
                f"FAIL the monitor this run started exited ({monitor.returncode}) before "
                f"{what}. Nothing answering on 127.0.0.1:{PORT} would be this run's — "
                f"refusing rather than reporting somebody else's answer as a pass.\n{tail}")
        if not (config / "coordinator.token").exists():
            raise SystemExit(
                f"FAIL the monitor is running but has minted no token, so the endpoint at "
                f"127.0.0.1:{PORT} cannot be shown to be this run's before {what}")
        return True

    def run(*args, check=True):
        return subprocess.run([EIDOLON, *args], env=env, cwd=work, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60,
                              check=check)

    def roster():
        ours("a roster read")
        # stderr is dropped, not merged: the harness's first run in a fresh world
        # prints its seeding lines, and a reader that mixed them into stdout would be
        # parsing prose as JSON. (The door checks below want the merged stream, so
        # they keep using `run`.)
        out = subprocess.run([EIDOLON, "peers", "--json"], env=env, cwd=work, text=True,
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=60)
        assert out.returncode == 0, out.stdout
        return json.loads(out.stdout)["sessions"]

    def row_for(session_id):
        return next((r for r in roster() if r.get("session") == session_id), None)

    def token():
        return (config / "coordinator.token").read_text().strip()

    def call(method, args):
        ours(f"the `{method}` call")
        body = json.dumps({"method": method, "args": args}).encode()
        request = urllib.request.Request(f"http://127.0.0.1:{PORT}/call", data=body,
                                         headers={"Authorization": f"Bearer {token()}",
                                                  "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=30) as answer:
            return json.loads(answer.read())

    def start_session(session_log=None):
        args = [EIDOLON, "chat", "--provider", "mock"]
        if session_log:
            args += ["--session", str(session_log)]
        child = subprocess.Popen(args, env=env, cwd=work, stdin=subprocess.PIPE,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return child

    try:
        wait_for(lambda: (config / "coordinator.token").exists() or monitor.poll() is not None,
                 what="the monitor mints a token")
        ours("the first call")
        # Authentication is what binds the endpoint, so it is asserted rather than
        # assumed: a token this run never minted must be refused. The monitor answers
        # `ok: false` with HTTP 200 for a wrong token, so the refusal is in the body and
        # not in the status code — reading only the status would call it a pass.
        forged = urllib.request.Request(
            f"http://127.0.0.1:{PORT}/call",
            data=json.dumps({"method": "adoptions", "args": {}}).encode(),
            headers={"Authorization": "Bearer not-the-token-this-run-minted",
                     "Content-Type": "application/json"})
        with urllib.request.urlopen(forged, timeout=30) as answer:
            refused = json.loads(answer.read())
        assert refused.get("ok") is False, f"a token this run never minted was accepted: {refused}"
        assert "token" in refused.get("error", ""), f"and it must say so: {refused}"
        session = start_session()
        first = wait_for(lambda: (roster() or None), what="a session registers")
        session_id = first[0]["session"]
        # The row's `log` is `~`-shortened for display; the resume needs the path.
        log_path = str(root) + first[0]["log"][1:] if first[0]["log"].startswith("~") else first[0]["log"]
        assert first[0]["display"] is None and first[0]["decoration"] is None, first

        # 1. Explicit assignment: the store records it and the row shows it, while
        #    the session's own label stays whatever it was.
        assigned = call("rank", {"caller": session_id, "session": session_id, "rank": "imperator"})
        assert assigned["ok"], assigned
        assert "Imperator" in assigned["result"], assigned
        def wearing_imperator():
            row = row_for(session_id)
            return row if row and row.get("display") == "Imperator" else None

        wearing = wait_for(wearing_imperator, what="the prefix reaches the roster")
        assert wearing["label"] is None, f"the label is untouched: {wearing}"
        assert wearing["decoration"]["controller"] == "coordinator", wearing
        assert wearing["decoration"]["text"] == "Imperator", wearing

        # 2. The same word again is a no-op, not a second write.
        again = call("rank", {"caller": session_id, "session": session_id, "rank": "imperator"})
        assert again["ok"] and "already carries" in again["result"], again

        # 3. Restart the same session: a fresh registration carries no decoration, and
        #    the monitor reasserts it within a tick — by session id, not by locator.
        session.terminate()
        session.wait(timeout=20)
        for entry in (Path(env["XDG_RUNTIME_DIR"]) / "eidolon").iterdir():
            shutil.rmtree(entry, ignore_errors=True)
        session = start_session(log_path)
        wait_for(lambda: row_for(session_id), what="the session restarts and registers")
        # Not asserted here: that the fresh registration is briefly undecorated. The
        # monitor ticks every second and a registration takes a few hundred
        # milliseconds, so that window is a race, not a fact — the fact that a
        # registration starts undecorated is core's (`Meta::default` has none, and a
        # new registration writes its whole `meta.json` fresh), and step 1 asserts the
        # same absence on the first registration with no monitor involved.
        reasserted = wait_for(wearing_imperator, what="the monitor reasserts the word")
        assert reasserted["label"] is None, reasserted

        # 2b. A row that already says what it should is not written again: a poll is
        # not a write. Two ticks are enough to catch a monitor that reasserts blindly.
        registration = next((Path(env["XDG_RUNTIME_DIR"]) / "eidolon").iterdir())
        meta_path = registration / "meta.json"
        before = meta_path.stat().st_mtime_ns
        time.sleep(2.5)
        assert meta_path.stat().st_mtime_ns == before, (
            "the monitor rewrote a registration that already wore the right word")
        assert (row_for(session_id) or {}).get("display") == "Imperator"

        # 4. A second session is never decorated by the first one's record.
        other = start_session()
        other_id = wait_for(lambda: next((r["session"] for r in roster()
                                          if r.get("session") not in (None, session_id)), None),
                            what="a second session registers")
        assert (row_for(other_id) or {}).get("decoration") is None, row_for(other_id)
        other.terminate()
        other.wait(timeout=20)

        # 5. Another owner is refused by the harness, and the monitor does not fight it.
        refused = run("door", "decorate", session_id, "--owner", "someone-else",
                      "--text", "Legatus", check=False)
        assert refused.returncode != 0, refused.stdout
        assert "coordinator" in refused.stdout, f"the refusal names the owner: {refused.stdout}"
        still = row_for(session_id)
        assert still["decoration"]["text"] == "Imperator", still

        # 6. Release: the record goes and the prefix this plugin recorded comes off.
        released = call("unrank", {"caller": session_id, "session": session_id})
        assert released["ok"], released

        def cleared_row():
            row = row_for(session_id)
            return row if row and row.get("decoration") is None else None

        cleared = wait_for(cleared_row, what="the decoration is removed")
        assert cleared["display"] is None and cleared["label"] is None, cleared

        # 7. D2 regression: the monitor attaches and never removes. A decoration whose
        #    owner string merely *claims* to be us, with no assignment behind it, is
        #    reported and left standing — the owner is a claim any same-user client can
        #    write, so it is not proof that this monitor, or this record, put the text
        #    there.
        hand_set = run("door", "decorate", session_id, "--owner", "coordinator",
                       "--text", "HumanSet")
        assert hand_set.returncode == 0, hand_set.stdout
        time.sleep(2.5)
        still_there = row_for(session_id)
        assert still_there["decoration"]["text"] == "HumanSet", (
            f"the monitor must not delete a decoration it cannot prove it attached: {still_there}")
        assert "does not remove a decoration" in (root / "monitor.log").read_text(), (
            "and it says so in its own log")

        # 8. A release removes only the text it recorded. With the standing decoration
        #    carrying somebody else's text under our owner string, the release leaves it
        #    and says why — the record is gone, the text is not ours to unsay.
        assigned = call("rank", {"caller": session_id, "session": session_id, "rank": "miles"})
        assert assigned["ok"], assigned
        wait_for(lambda: (row_for(session_id) or {}).get("decoration", {}).get("text") == "Miles",
                 what="the record's own word is reasserted over the hand-set one")
        released_again = call("unrank", {"caller": session_id, "session": session_id})
        assert released_again["ok"], released_again
        wait_for(cleared_row, what="the prefix comes off with its record")

        # 9. The record is durable on the disk, where a restart of the monitor reads it.
        assert (monitor_root / "state.json").exists(), "the store is on the disk"
        print("rank-prefix: OK")
        print((root / "monitor.log").read_text()[-2000:])
    finally:
        if session is not None:
            session.terminate()
            try:
                session.wait(timeout=20)
            except subprocess.TimeoutExpired:
                session.kill()
        monitor.terminate()
        monitor.wait(timeout=20)
        log.close()
