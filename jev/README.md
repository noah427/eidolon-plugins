# jev

Automation graphs for eidolon, run as workflows. A graph is a small state chart
(JSON): states, the events that move between them, the tool calls to make on the
way, and at each decision a *chooser* that scores a menu of options in one
forward pass. The interpreter is a Rune library in this plugin; each graph has a
workflow that runs it; a person is asked only when the chooser is not sure.

| graph | does | needs |
|---|---|---|
| `find-related` | traces a term through docs, Rust, Rune, extensions and the design record with `grep`, then reads the head of the first file | nothing |
| `ctf-juice-recon` | headers and `robots.txt` of a local OWASP Juice Shop at 127.0.0.1:3001 | `curl` |
| `triage-linux` | what a Linux box is, what it exposes, what runs on it, what looks out of place: nine fixed read-only commands, a chooser picking the next | the jev service |
| `triage-wsl` | `triage-linux` inside a WSL distribution (Ubuntu by default) | `wsl.exe`, the jev service |
| `triage-botforge` | `triage-linux` for unattended use over ssh (no NLI, no confidence floor) | `ssh`, the jev service (`jev_choose` only) |
| `wiki-hop` | follows links from one Wikipedia article to another | the real [`browser`](../browser/) plugin, installed with its service running; the jev service |

**The graphs here are the canonical copy.** `jev/graphs/*.json` in this plugin is
the one source of truth for the six graphs. The copies in the Minerva repository
(`Minerva/extensions/jev/graphs`) are the old format, kept only until a
real-service run of these graphs passes, and will be retired after it. Edit a graph
here, re-pin it (see the end of this file), and nowhere else.

```
eidolon workflow run <jev dir> <graph> --args '{...}'
        └─ workflows/<graph>.rn      pinned to graphs/<graph>.json by sha256
             └─ lib/interp.rn        the interpreter, running inside the workflow
                  ├─ tool_call ─▶ gate ─▶ jev_<command>, browser_*, grep, read   the graph's own actions
                  ├─ tool_call ─▶ jev_choose, jev_entail ─▶ jev service (Python)  the decisions
                  ├─ tool_call ─▶ jev_mark                                         where the run is
                  └─ park ─▶ a person                                               when the chooser is unsure
```

Every effect is a journaled step, so a run is a session you can read, and a
resumed run replays what it already did instead of doing it twice.

## Tools

| tool | does |
|---|---|
| `jev_graph` | a graph as a drawing (JSON: states, edges, menus), as its exact text (`raw`), or in pages (`from`) |
| `jev_mark` | records where a run is; a no-op whose *input* is the record (see "Drawing a run") |
| `jev_choose` | the service's chooser: options scored against a context, `{probs, best}` |
| `jev_entail` | the service's NLI scorer: premise against hypotheses |
| `jev_os_release` `jev_uname` `jev_hostname_uptime` `jev_who` `jev_listening` `jev_established` `jev_addresses` `jev_firewall` `jev_processes` | the nine triage commands, one fixed command each |
| `jev_juice_headers` `jev_juice_robots` | the two Juice Shop probes, one fixed `curl` each |

Each command tool runs exactly one command, which is in its file and not in its
input, so the gate sees a verb that can do that and nothing else. Optionally it
takes `distro` (run it inside that WSL distribution, `wsl.exe -d <distro> -e bash
-lc ...`) or `ssh` and `ssh_port` (run it on that host, key-based,
`BatchMode=yes`); both are checked to be plain names before they reach a command
line. A command that does not succeed (non-zero exit, timeout, no such program)
is an error, which a graph routes to its `ERROR` event.

## Prerequisites

- Linux or WSL, and eidolon with the plugin runtime **and workflows** (upstream
  `master`; `eidolon workflow run` must exist).
- `bash`, `awk`, `sed` and `sha256sum` (the plugin's shell tools). `python3`, `git`
  and a checkout of this repository only for Verify's stand-ins: the plugin
  ships none of its test scaffolding, which lives in `tests/jev/` of the repo.
- Per graph, as in the table: `curl`, `ssh`, `wsl.exe` (a Windows host).
- **`wiki-hop` alone needs the real [`browser`](../browser/) plugin from this
  repo**, installed and trusted, with its service running (port 8090, token
  `~/.config/eidolon/browser.token`; see its README). No other graph touches
  it. The two plugins stay apart: jev's service is on 8091 with `jev.token`,
  jev registers no `browser_*` tool of its own (`tests/jev/no-browser-names.sh`
  checks it), and `jev_*` never reads `browser.token`. The repo's `tests/jev/`
  has a *fake* plugin also named `browser` (the name has to match, since
  `wiki-hop` calls `browser_open`); it is a test double, is not a top-level folder
  of the repo so `plugins install --all` never picks it up, and its scripts
  refuse to install it anywhere but a temp config.
- **The jev service**, for `triage-*` and `wiki-hop` (not for `find-related`,
  `ctf-juice-recon` or `selftest`). It is a model server the operator runs
  separately, a documented external service as AGENTS.md allows, so it is not
  shipped and `plugin.rn` has no `service:` block. It is Python and torch
  (`jevlike`, a 169 KB chooser, and `openjev`, a Qwen3.5-4B NLI model of about
  9 GB, which only `entails` guards load), `jev/server.py` in the Minerva
  repository.
  - How to start it: Install step 3 below.
  - Port: `127.0.0.1:8091` (8090 is the browser plugin's); it is written at the
    bottom of `tools/choose.rn` and `tools/entail.rn`, and the service reads it
    from `EIDOLON_SERVICE_PORT`.
  - Token file: `~/.config/eidolon/jev.token`, sent as a bearer token; the two
    scorer tools are granted exactly that file.
  - It answers `POST /call {"method": "choose"|"entail", "args": {...}}`.
  - When it is down: `jev_choose` and `jev_entail` fail with the connection
    error, a graph that needs a choice parks on a person and says why
    (`chooser unavailable: ... Connection refused`), and an `entails` guard does
    not pass. Nothing proceeds on a guess.

## Install

1. The plugin, from this repo (installing does not trust):

   ```bash
   eidolon plugins install noah427/eidolon-plugins jev
   ```

   or, from a clone, `cp -r jev ~/.config/eidolon/plugins/jev`.

2. Vouch for its verbs. There is no warrant in this plugin: the gate decides, per
   verb, and a vouched verb is one you have read.

   ```bash
   eidolon plugins trust jev
   ```

3. For the graphs that need the service: a token, the grants that let the two
   scorer tools read it, and the service.

   ```bash
   mkdir -p ~/.config/eidolon && umask 077
   head -c 24 /dev/urandom | base64 > ~/.config/eidolon/jev.token
   for v in jev_choose jev_entail; do
     eidolon plugins grant $v file:~/.config/eidolon/jev.token
   done
   ```

   Start the service from the Minerva repository's `jev/` directory (its own
   venv, from `requirements.txt`; the two offline variables are the air gap):

   ```bash
   cd <minerva>/jev
   EIDOLON_SERVICE_PORT=8091 EIDOLON_SERVICE_TOKEN="$(cat ~/.config/eidolon/jev.token)" \
     HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 .venv/bin/python -u server.py
   ```

   There is no `service:` block in `plugin.rn` on purpose: a service block makes
   eidolon start a command, and this plugin cannot supply one that runs on a
   machine without that checkout.

## Verify

Each step needs only what the step says. `--provider mock` is there so nothing
needs a model key; no step spends anything.

```bash
P=~/.config/eidolon/plugins/jev
R=<your checkout of noah427/eidolon-plugins>   # the test scaffolding is here, not in $P

# 0. the install carries no test scaffolding: none of these exist under $P
find $P \( -name 'test*' -o -name '*.py' -o -path "$P/*/plugin.rn" \) | wc -l
# 0

# 1. every verb is there and vouched: 15 tools, 7 workflows
eidolon plugins | grep -A30 '^jev ' | grep -c ' vouched by '
# 15
eidolon plugins | grep 'claims   workflows'
# claims   workflows: selftest, find-related, wiki-hop, ctf-juice-recon, triage-linux, triage-wsl, triage-botforge

# 2. the interpreter against itself: 45 fixed checks, no network, no model. It also
#    reads all six graphs back through jev_graph and checks that each loads, and
#    that a `transitions` menu comes out in the graph's own order.
eidolon workflow run $P selftest --provider mock | grep -o 'selftest: [0-9]* checks hold'
# selftest: 45 checks hold

# 3. each graph is what its workflow pins
bash $R/tests/jev/pin.sh --check $P
# ok     ctf-juice-recon sha256:...   (six lines)

# 4. find-related, end to end, on a one-file tree it can search
mkdir -p /tmp/jev-verify/docs && printf 'the jevprobe term\n' > /tmp/jev-verify/docs/note.md
eidolon workflow run $P find-related --args '{"term":"jevprobe"}' --cwd /tmp/jev-verify --provider mock \
  | grep -o 'first_file[^,]*'
# first_file\": \"docs/note.md\"

# 5. jev stays out of the browser plugin's names: no `browser_` verb under jev's
#    entry in the listing, and none in jev/tools/ of the checkout
eidolon plugins | awk '/^jev /{f=1;next} /^[a-z]/{f=0} f' | grep -c 'browser_' || true
# 0
bash $R/tests/jev/no-browser-names.sh
# ok     static: jev/tools declares no browser_* name (15 tools)

# 6. the repo's listing does not offer the fake browser: this prints browser,
#    jev, subagent and nothing called "TEST DOUBLE"
eidolon plugins --dir $R 2>&1 | grep -E '^(browser|jev|subagent) '
eidolon plugins --dir $R 2>&1 | grep -c 'TEST DOUBLE' || true
# browser 0.1.0
# jev 0.2.0
# subagent 0.2.1
# 0

# 7. the fake's own scripts refuse a real config. This one installs nothing: with
#    XDG_CONFIG_HOME unset (or pointing at ~/.config) it stops at once.
env -u XDG_CONFIG_HOME bash $R/tests/jev/wiki-hop.sh; echo "exit $?"
# refusing: XDG_CONFIG_HOME is not set, ...   exit 2
```

With the lab, or a stand-in for it, `ctf-juice-recon` runs two real `curl`s:

```bash
mkdir -p /tmp/lab && printf 'User-agent: *\nDisallow: /ftp\n' > /tmp/lab/robots.txt
(cd /tmp/lab && python3 -m http.server 3001 --bind 127.0.0.1 &)
eidolon workflow run $P ctf-juice-recon --provider mock | grep -o 'final_state[^,]*'
# final_state\": \"report\"
```

With the service up (the stand-in below is enough: it answers the same contract
from stdlib Python, with rules in place of a model, so it proves the plumbing and
nothing about the graphs' judgement), `triage-linux` runs seven real read-only
commands on this machine:

```bash
python3 $R/tests/jev/stub_service.py --port 8091 --token-file ~/.config/eidolon/jev.token &
eidolon workflow run $P triage-linux --provider mock | grep -o 'final_state[^,]*'
# final_state\": \"report\"
kill %1
```

`wiki-hop` has no stand-in step above, because it needs the browser plugin. To
run it without Chromium, `tests/jev/wiki-hop.sh` installs jev and the fake
`browser` into a *temp* config (it refuses any other), starts the stub service
and runs `Cat` to `Ancient Egypt`:

```bash
t=$(mktemp -d); HOME=$t XDG_CONFIG_HOME=$t/cfg XDG_STATE_HOME=$t/state   XDG_DATA_HOME=$t/data bash $R/tests/jev/wiki-hop.sh
# final_state\": \"arrived\"
```

Exit codes of `eidolon workflow run`: 0 completed, 1 parked, 2 failed, 3 refused.

## Running a graph

```bash
eidolon workflow run $P <graph> [--args '{"key": "value"}'] [--cwd DIR]
```

`--args` are the graph's input (`wiki-hop` needs `start` and `goal`; the triage
graphs take `distro`, or `ssh` and `ssh_port`; `triage-wsl` defaults `distro` to
`Ubuntu`; `triage-botforge` defaults `ssh` to `mford@127.0.0.1` and `ssh_port`
to `2222`; `ctf-juice-recon` takes `distro`). A completed run's answer is the
report: `{graph, outcome, final_state, output, path, steps, actions,
chooser_calls, rule_picks, escalations, warnings}` as JSON text.

**Parks.** When the chooser's best option is under the state's floor, or its
margin over the runner-up is too thin, or the service cannot be reached, the run
parks (exit 1) and the line names it: `status: parked`, the `question` (the
options with their scores), the `session` and the `run`.

```bash
eidolon workflow resume <session> <run> --plugin $P --answer "kernel and arch"
```

An answer is an option's number, its exact label, or `stop` / `stop: <reason>`.
Anything else parks again, and each park counts against the graph's
`escalations` budget. `stop` ends the run as a failed run whose report says
`stopped`.

## Drawing a run

`jev_graph {graph}` returns the graph as JSON, for whatever draws it:

```
{ graph, description, initial, input, budget, tools,
  states: [ { path, kind: leaf|compound|final|history, description?, initial?,
              choose?: { source: menu|lines|refs|transitions, options?, floor?, ask? },
              tools?: [ ... ] } ],
  edges:  [ { from, to, kind: initial|always|on|onDone|history, event?, guard?, reenter? } ],
  root_on: [ same shape, from "*" ],
  marks:  { tool: "jev_mark", kinds: [ "enter", "park", "final", "end" ] } }
```

Paths are dotted (`investigate.identify`). `to` is null for a transition with no
target (its actions run and the state stays).

Where a run *is* comes from its journal: the interpreter calls `jev_mark` as a
step whenever it enters a leaf state (`enter`), enters a final state (`final`),
asks a person (`park`) or ends without a final state (`end`, with `outcome`).
The step's input is the record, `{graph, kind, state, step, actions}`, so the
active node is the last `jev_mark` step of the session:

```bash
eidolon log --json <session> | grep '"tool":"jev_mark"'      # every record, in order
eidolon log --json --after <last id seen> <session>          # a poller's cursor
```

No front end ships with the plugin; this is the contract one would read.

## What the interpreter does not do

It covers what the six graphs use, and **refuses at load** a graph that uses
anything else, naming it (never running it with the feature ignored): guards
other than `equals`, `count`, `entails` and `matches` (as a tool's `expect`),
`inc`, deep history, standing orders. Two limits come from the host: a workflow gets **no
clock**, so `wall_s` is not enforced (the steps, actions, escalations and visits
budgets are); and a run is capped at **200 steps**, every tool call, scorer call
and mark counting as one, which bounds `wiki-hop` to roughly thirty hops. A
`transitions` menu keeps the graph's own event order (a parsed object is
sorted, so the order is read off the graph's text when it loads); `selftest`
checks it on `triage-linux`'s `recover`.

A graph is pinned: change `graphs/<id>.json`, then `bash tests/jev/pin.sh` (from the
repo) to write its hash into `workflows/<id>.rn`, and commit both. A graph that does not match its
workflow's pin is refused before it runs. `tests/jev/gen_probes.py`
regenerates the eleven command tools from one table, and
`tests/jev/gen_workflows.py` the thin per-graph workflows; both have `--check`.

## Uninstall

Take the permissions back first (they need the plugin to still be installed),
then remove it:

```bash
eidolon plugins untrust jev
for v in jev_choose jev_entail; do
  eidolon plugins revoke $v file:~/.config/eidolon/jev.token
done
eidolon plugins uninstall jev
rm -f ~/.config/eidolon/jev.token
```

Uninstalling leaves vouches and grants in `~/.config/eidolon/policy.permits.rn`.
They are inert once the directory is gone, but the store is the record and it
should say what is true. The service, if you started it, is yours to stop.
