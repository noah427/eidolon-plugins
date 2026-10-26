# claude — the driver host

The half of a driver that runs *beside* a session and owns the external CLI.
eidolon dials it on a private unix socket; the host runs the turn and reports
what it observed; eidolon keeps the record, the gate and the execution.

**Status: both modes, with both doors.** The transport, the handshake, the
refusal rules, a readiness probe, a **fake backend** that scripts a turn, and a
**real backend** that spawns a CLI, translates its stream-json and serves both
doors: the `PreToolUse` hook for own-tools mode and an MCP server for registry
mode. In registry mode the host executes nothing — `tools/list` answers from the
list the session pushed with the turn, and `tools/call` asks the session, which
runs it under its own gate. What is *not* verified is a real CLI: the adapter has
only ever run against the fake one in this repo's tests, and **that sentence is about those
tests, not about the plugin in use.** A host started with `--real` spawns a real vendor CLI,
which authenticates as you and reaches the network as you — that is what it is for. This
plugin reads no credential *itself* and holds none; what it spawns is a different claim, and
the one to keep in view.

**Why each rule is what it is** is written where the rule is — beside it in
`service/src/*.rs`, in the section the rule belongs to — rather than in a document
somewhere else on this machine. A report under `~/.local/share/eidolon/reports/` is
one machine's notes and not a thing a plugin can hand a reader; the file you are
reading and the code beside it are what travels.

## What this plugin is, and why it has no verbs

Every other plugin here ships `tools/*.rn`. This one ships none: a driver has
nothing for a person to type. It has a **service**, and a session dials it.

That has one consequence worth knowing before you follow the usual four
sections, because it is the opposite of every other plugin in this repo:

> **`eidolon plugins trust claude` refuses this plugin**, by design. Trust
> vouches *verbs*, and there are none — the command stops with `ships no tools
> to vouch for` (`crates/cli/src/plugins.rs:738`). Skip that step. The thing the
> operator approves here is the **service declaration**, with `eidolon plugins
> service approve claude`; the hash it is held against is the declaration's text,
> so an edit to `plugin.rn`'s `service:` block asks again.

## The wire, briefly

Frames are a `u32` little-endian length and then that many bytes of one JSON
object — no checksum, because a socket cannot reorder or drop a byte and a short
read already ends the connection. The 64 MiB cap is checked on the length word,
before the body. Every frame carries a `"method"`, and the host speaks first.

```
host → session   hello      {protocol, host, version, backend, modes}
session → host   hello      the confirmation, naming the one mode it wants
session → host   run_turn   {req_id, turn_id, mode, model, guidance, tools,
                             prompt, images, resume, cwd, scratch}
session → host   cancel     {turn_id, req_id, deadline_ms}
host → session   event      {turn_id, seq, kind, payload}    seq from 0, contiguous
host → session   session    {turn_id, opaque}                the vendor's resume id
host → session   usage      {turn_id, priced, vendor}
host → session   settle     {turn_id, outcome, stop_reason}  terminal
host → session   cancel_ack {turn_id, req_id — the session's, echoed, killed}
host → session   call       {req_id, turn_id, call_id, tool, input}    registry
host → session   adjudicate {req_id, turn_id, call_id, tool, input}    own tools
```

The session answers `call` with `tool_result {req_id, call_id, content, is_error,
images}` and `adjudicate` with `verdict {req_id, call_id, allow, reason}`.

**In registry mode the host's `tool_result` event is a marker** — the call id and
no text — because the session executed the call and journals *its own* held
result at that position. A host that could put its own words in that record
could overwrite evidence, so it is not allowed to. In own-tools mode the CLI ran
the tool and this host is the only witness, so the report carries the text and
the session marks it as reported rather than as something it watched happen.

No session id and no `origin` are ever on the wire: the connection is *about* one
session because the dialling side chose the socket.

### The socket, and what it does not promise

The turn protocol is a private unix socket at
`$XDG_RUNTIME_DIR/eidolon-claude.sock`, mode `0600`, bound the way
`eidolon_core::ipc` binds the hook, MCP and doorbell sockets. That is the whole
access story and it is **not isolation**: any same-uid process can reach it,
exactly as it can reach those three, and as it can read the session log
directly. There is no token and no grant, so there is nothing to leak.

Two rules it keeps about the path itself:

* **A socket that still answers is never replaced.** A second host starting
  finds the name taken and refuses, rather than unlinking it out from under a
  running service and leaving every session dialling nothing.
* **A file at that path which is not a socket — including a symlink — is
  refused, not removed and not followed.** A socket nobody answers on is stale
  and is replaced; anything else is left alone.

The mode is the protection against *connecting*; the containing directory's
permissions are the protection against *replacing the name*. Creation is **not
atomic**: `bind` creates the socket with the process umask's mode and the
`chmod` that follows is a second syscall, so those two are not one act and a peer
able to reach the path could connect in between. What closes that interval in
the shipped binary is that `main` sets `umask(0o077)` before it starts a runtime,
so `bind` itself creates the socket `0700` — owner-only, exactly as connectable
by its owner as `0600` and no more by anyone else — and the `chmod` to `0600`
that follows is the tightening rather than the whole of the protection. (`0o077`
and not `0o177`, which would also strip the owner's search bit: a umask falls on
the directories this process creates as well as the socket, and a directory
without its search bit is one nothing can be put inside, including by its owner.)
A caller that sets no umask is bounded by its directory instead.

A related defect was real and measured: binding a *tokio* listener outside the
runtime panicked after creating the file and left the socket at `0755`, the
`chmod` never reached at all. The test that pins this drives the real binary,
because library tests run inside a runtime and could not see it.

## The service is a demo; a real turn is this

**Read this before installing.** `plugin.rn` declares the service as

```rune
command: "eidolon-claude",
```

which is the **fake backend**. It answers the readiness probe and scripts a turn: it reads
no prompt, spawns nothing, and runs nothing. `service start` therefore proves the transport,
the handshake and the readiness — and **not** a working driver. Nothing has been changed to
make that read as more than it is; the default is deliberately the backend that cannot do
harm.

A real turn is the same binary with `--real`, run by hand, in its own terminal:

```sh
# 1. the demo service holds the socket; stop it.
eidolon plugins service stop claude

# 2. the host on the real CLI, in the foreground of a dedicated terminal, so you can
#    see what it says and stop it with ^C. `--cli` defaults to `claude`; name a path to
#    use another. --health-port 0 skips the readiness port, which nothing needs here.
eidolon-claude --real --socket "${XDG_RUNTIME_DIR:-/tmp}/eidolon-claude.sock" --health-port 0

# 3. and the session side, in ${XDG_CONFIG_HOME:-$HOME/.config}/eidolon/config.toml.
#    The socket must be the one above, and the backend string must be the one the host
#    declares (its `hello` says it; `claude-cli` is the default).
[[driver]]
backend = "claude-cli"
socket  = "/run/user/1000/eidolon-claude.sock"   # ← your $XDG_RUNTIME_DIR
mode    = "registry"          # or "own_tools" — see the mode section below
models  = ["claude-*"]

# 4. a turn. `--provider` takes the backend's bare name, and is what selects the driver.
eidolon run --provider claude-cli "hello"
```

Two things to know about the pairing. `mode` must be one the host offers — `registry` needs
no rows in your `policy.rn`, `own_tools` does (see the section below). And the socket in the
config has to be the socket the host bound; a mismatch is reported as
`connecting to the driver host at … / No such file or directory`, which is the same answer a
host that is not running gives.

**What was verified with this recipe:** every step, with `--cli` pointing at the repository's
**fake** CLI fixture instead of `claude`. No real vendor CLI was invoked, no credential was
read and nothing was spent, and the default `claude` path in step 2 is the one part no test
here can stand behind.

## Prerequisites

* eidolon with the plugin runtime, upstream `master`.
* `eidolon-claude` on `PATH` — from `nix build .#claude`, or a profile install.
  The service declaration names it by bare name.
* For `--real`, the Claude CLI on `PATH` (or named by `--cli`) and logged in.
  Not needed for the fake backend, which spawns nothing.

## Install

```sh
eidolon plugins install noah427/eidolon-plugins claude
eidolon plugins service approve claude     # the step that matters; there are no verbs to trust
eidolon plugins service start claude
```

The record the install writes names the source and the ref —
`noah427/eidolon-plugins#claude@main` — and the listing reads `record managed`.

*(This repository is a fork of `dxcently/eidolon-plugins`; the ref above names the fork, because the fork is where this plugin ships from.)*

From a checkout instead — the path the rest of this file was tested against:

```sh
cp -r <checkout-of-this-repo>/claude "${XDG_CONFIG_HOME:-$HOME/.config}/eidolon/plugins/claude"
eidolon plugins service approve claude     # the step that matters; there are no verbs to trust
eidolon plugins service start claude
```

A copied one is listed as `record unmanaged — installed by hand`, which is exact rather
than apologetic: there is no source to fetch from and no ref to compare against, and it is
why `update` has nothing to say about it.

`service start` runs `eidolon-claude` by bare name, so it must be on `PATH` —
from `nix build .#claude`, or a profile install. See Prerequisites.

Do **not** run `eidolon plugins trust claude` — see above.

## Verify

```sh
eidolon plugins service status claude
# → running (127.0.0.1:8093 answering)

ls -l "${XDG_RUNTIME_DIR:-/tmp}/eidolon-claude.sock"
# → srw------- … the socket is 0600, and nothing else reaches it

# The host speaks the contract before it is asked anything.
python3 - "${XDG_RUNTIME_DIR:-/tmp}/eidolon-claude.sock" <<'PY'
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
n = struct.unpack("<I", s.recv(4))[0]
print(json.loads(s.recv(n)))
PY
# → {'method': 'hello', 'protocol': 'eidolon-driver/1', …, 'modes': ['registry', 'own_tools']}
```

The readiness port is a convenience, not the transport: if something else holds
8093 the host says so on stderr and **keeps serving on the socket**, but
`plugins service start` will then report the start as failed, because its probe
never answers. Free the port, or start it with `--health-port 0`.

A turn answered by the fake backend is *not* evidence of a working driver — it
reads no prompt and runs nothing. What this recipe verifies is the transport,
the handshake, the refusals and the readiness, which is what exists.

## Uninstall

```sh
eidolon plugins service stop claude
eidolon plugins uninstall claude
```

## Building and testing

```sh
nix build .#claude            # one package, one program
cargo test -p eidolon-claude  # 33 tests (lib 4, protocol 19, real 10)
cargo clippy --all-targets
```

The adapter's tests drive a **fake CLI** (`claude/service/tests/fixtures/fake-cli.py`)
that emits the same stream-json shapes a Claude Code CLI writes — the ones the
in-process driver used to parse before the cut moved it out of the harness — and
really
runs the `PreToolUse` command named in the settings file the adapter wrote — so
the hook door, the gate's answer, the message assembly and the settle are all
exercised over a real socket with a real child process, and none of it touches a
credential or the network. The registry path is exercised the same way: the fake
CLI speaks MCP to the shim, and the test asserts the marker carries the call id
and no text. Cancellation is tested against a real process — the fake CLI writes
its pid, the host signals the group, and the test checks the process is gone.

The tests drive the host over a real socket with a test client, one per
obligation: the handshake, both modes' event order, the marker that carries no
text, framing and its cap, the mode/origin/session refusals, cancel ack and its
absence, the seq-gap knob, two sessions on one host at once, the readiness
probe, the socket's stale/live/file/symlink rules, and the real binary binding
`0600`.

Two environment knobs exist so the session side's negative paths can be
exercised by spawning the binary:

| variable | effect |
|---|---|
| `EIDOLON_FAKE_CANCEL_ACK=0` | a `cancel` goes unanswered, so the session journals cancelled-unacknowledged |
| `EIDOLON_FAKE_SEQ_GAP=1` | a sequence number is skipped, so the session sees a gap it must fail the turn on |

## Which mode to run, and what each needs from your policy

**Registry mode needs nothing from your policy.** The CLI is started with
`--tools ""` and a strict MCP config naming this host alone, so it has no built-ins
of its own and every call it makes arrives under **the harness's** tool names —
`read`, `grep`, `bash` — which the shipped table already knows. Its calls are run
by the session's own dispatcher, validated and gated there, and no tool call is
executed by the CLI. (The process is still a process — see below.) Measured on a
fresh tree with an unmodified `policy.rn`: a
registry turn runs, the tool really executes, and the result is journaled.

**Own-tools mode needs rows you adopt.** The CLI keeps its own tools and its own
vocabulary — `Bash`, `Read`, `Edit` — and reaches the gate through the `PreToolUse`
hook. The harness ships no external CLI's names by ruling, so those names are
unclassified until you say otherwise.

So if you want this backend working with the least ceremony, **registry is the mode
that works out of the box**; choose own-tools when you want the CLI's own tools, and
then adopt the rows below. Adopting them is your decision about your machine, which
is why nothing here writes them.

**What registry mode does not do, stated exactly, because it is easy to overread.**
It narrows what the *model* can reach through tool calls: the CLI is told to offer
none of its built-ins and to reach no MCP server but this host, so every call the
model makes goes through the session's own registry, is validated there, and is
gated there. That is a real narrowing and it is the whole of the claim.

It is **not a sandbox.** The CLI is still a process running with the ambient
authority of the account that started it. Nothing in these two flags confines its
filesystem access, its network access, its own configuration, its own hooks, or
anything else it does outside a tool call — no namespace, no container, no separate
user. A tool call the session denies is not a capability the process lacks; it is a
capability the process was not asked to use. If that distinction matters for what
you are doing, the confinement has to come from outside this plugin: run the whole
session under a user, a container or a sandbox that has only the authority you mean
it to have, and this plugin's gate is then one layer rather than the only one.

**Measured, not assumed.** With this host's CLI on the other end and nothing added
to your `policy.rn`, an own-tools turn in a fresh tree goes like this:

```
? Bash — unclassified tool (declared mutating). Run it? [yes/no]
>   ↳ Bash ERROR the gate said no
```

The gate is asked, an unattended `run` has nobody to answer, and the call is
denied. So `Bash` — the tool this backend uses for almost everything — does not
work out of the box, and the fix is yours to make, not the harness's.

The harness ships **no** external CLI's tool names, by ruling: wrapping a CLI is a
plugin's shape, and a table full of one vendor's vocabulary in the harness's
default is exactly what the plugin boundary exists to prevent. What a plugin can
do is say which names it forwards, which is why `plugin.rn` carries them under
`claims` — visible in `eidolon plugins --dir <dir>` and nothing more. If you want
this backend running unattended, adopt rows into your own `policy.rn`:

```rune
// Beside the other shell-shaped tools in `shell_arg`. `Bash` holds its command in
// `command`, so saying so routes every `Bash` call through the decomposition
// algebra — which knows an ordinary `echo hi` from a `rm -rf /` — instead of the
// flat table below, which can only see a name.
pub fn shell_arg(tool) {
    match tool {
        "bash" => "command",
        "Bash" => "command",      // ← this plugin's CLI
        _ => "",
    }
}

// And the CLI's own names, in `classify_tool`. Without these, each one is asked
// about, which on an unattended run is a refusal.
"Read" => v(ALLOW, "read", true),
"Glob" => v(ALLOW, "search", true),
"Grep" => v(ALLOW, "search", true),
"LS" => v(ALLOW, "read", true),
"NotebookRead" => v(ALLOW, "read", true),
"Write" => v(ALLOW, "write a file", false),
"Edit" => v(ALLOW, "edit a file", false),
"MultiEdit" => v(ALLOW, "edit files", false),
"NotebookEdit" => v(ALLOW, "edit a notebook", false),
"TodoWrite" => v(ALLOW, "its own scratch list", false),
"WebFetch" => v(ALLOW, "fetches a URL", true),
"WebSearch" => v(ALLOW, "searches the web", true),
"BashOutput" => v(ALLOW, "reads a running command's output", true),
"KillBash" => v(ALLOW, "stops a command it started", false),
"KillShell" => v(ALLOW, "stops a command it started", false),
"Task" => v(ALLOW, "its own subagent", false),
```

The harness's own file is left alone. `eidolon policy-rows` reports which rows are
yours and which the harness added, and this list is the plugin's proposal rather
than anything adopted on your behalf — a `policy.rn` you have edited is never
rewritten. The tiers are the CLI's own: a name this list does not have is
`mutating`, and `real.rs`'s `approval_for` is the table they come from.

## Two things worth knowing before you change this

**The hook waits 240 seconds, then denies.** In own-tools mode the CLI's
`PreToolUse` command blocks on this host, and this host blocks on the session's
gate. Every other way that path can go wrong is already a deny — a payload naming
no tool, a connection that dies, a session that hangs up — but an answered-never
gate is not a *failure*, it is a stall, and a stall takes the turn down with it.
So `real.rs`'s `HOOK_DEADLINE` bounds the wait and the door answers **deny** when
it expires, which is the direction that cannot run a tool nobody approved.

The number is a **coupling, not a guarantee**, and it is written down here because
it cannot be removed from this side: the session has an idle bound of its own
(300 s by default), and this has to be shorter so the operator gets a denied call
rather than a dead turn. It is generous for the same reason it exists — in the TUI
a flag is a question waiting on a person, who may be reading something else. The
clean fix is a deadline on the wire that both sides can read; until then, if you
change the session's idle bound, change this too.

**`model_flag` normalises a catalog key, and says nothing about it.** The session
names a model as its catalog key, `<backend>:<id>`; for this host with no model
named, that is `claude-cli:`, which is a backend's name with an empty id and not a
model any vendor CLI has heard of. `real.rs` strips this host's *own* backend
prefix before the value reaches `--model`, so `claude-cli:` becomes no `--model` at
all and the host picks its own default. It is **defence for a version skew, not a
requirement** — the session sends `""` today — and it is silent by design, which is
the one thing to dislike about it. A guard that reported itself would be better;
the `note` event that could carry that report arrived in the session later than the
protocol version this host advertises, so emitting one against an older session
would fail the turn it was trying to explain. It is here, deliberately, rather than
inferred.
