# subagent

Child eidolon sessions for one bounded task each, on a model you pick per kind of
task. One tool starts `eidolon run` in the background with a brief; others pick
the model, plan a formation, follow the children and their spend, steer and stop
them. The child reports back to the session that started it with `send`.

| tool | file | does |
|---|---|---|
| `subagent_spawn` | `tools/spawn.rn` | start a child `eidolon run` with a brief on the model picked for its `kind`; returns at once with its id, pid and log. With no pick, returns `needs_choice` and starts nothing |
| `subagent_pick` | `tools/pick.rn` | choose, change, clear or list the model for a kind; sticky, global or per project |
| `subagent_plan` | `tools/plan.rn` | resolve a formation (roles, counts, budgets) into a plan to show the operator; spawns nothing |
| `subagent_list` | `tools/list.rn` | every child this machine spawned: alive or not, how it ended, budget, spend, journal, peer id |
| `subagent_trace` | `tools/trace.rn` | the tail of one child's log, with alive/ended, budget and spend |
| `subagent_steer` | `tools/steer.rn` | one message into a running child, read at its next step |
| `subagent_cancel` | `tools/cancel.rn` | SIGTERM to the child's whole process group, SIGKILL if it will not go |

| workflow | file | does |
|---|---|---|
| `fanout` | `workflows/fanout.rn` | run a plan: spawn each role's children, watch finished children's spend against the thresholds (step down, ask, stop), report each result |
| `door` | `workflows/door.rn` | one command for an orchestrator outside eidolon (Claude Code, Codex, aoide): `{tool, input}` in, the tool's answer out |

| other | file | does |
|---|---|---|
| `lib/util.rn` | `lib/util.rn` | helpers the two workflows share; upstream compiles `lib/` in front of a *workflow* only, so the tools stay self-contained and carry their own copies |
| `subagent-door` | `bin/subagent-door` | POSIX wrapper: `subagent-door '<door args JSON>'` finds the plugin directory and eidolon, and runs the door |

A tool file's **stem is the bare verb** and its declared `name:` is the
**namespaced** one: `tools/spawn.rn` is adopted as `subagent_spawn`, because
the plugin's directory name is the namespace. That is why these files are not
called `subagent_spawn.rn`.

```
eidolon ──plugin subagent/tools/spawn.rn──shell_background──▶ bash ──▶ eidolon run (the child)
     │                        pid, log under ~/.cache/eidolon/background/
     │
     ├── picks:   ${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/subagents/picks.json     (global)
     │            <cwd>/.eidolon/subagent-picks.json                                      (project, wins)
     └── a brief, a meta file and (with a call budget) a config.toml per child, under
         ${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/subagents/<id>/
         read back by subagent_list, subagent_trace, subagent_steer, subagent_cancel
```

No service and no shared registry: the child is an ordinary session, its state
directory is what the parent wrote down, and its own registration among its
peers is what a steer is addressed to. The child's journal is what
`eidolon resume <journal>` picks up again; the path is in the child's log, and
`subagent_list` prints it. [`brief.md`](brief.md) is the brief the child is
handed, and each tool file carries what it needs whole.

## Picks

No model or tier is hardcoded anywhere in this plugin. A **kind** is a free
string the orchestrator (the agent calling these tools) names for a sort of work:
`research`, `review`, `grind`. Each kind has at most one pick, `kind -> model`,
sticky until changed:

- global: `${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/subagents/picks.json`
- per project: `<cwd>/.eidolon/subagent-picks.json`, which wins over global for
  the same kind. (It is a file in your repo: add it to `.gitignore` or commit it,
  as you like.)

`subagent_pick` is the only writer:

```
subagent_pick {kind: "research", model: "ollama:deepseek-v4.1-flash"}      save (global)
subagent_pick {kind: "review", model: "zai:glm-5.3", scope: "project"}     save for this project
subagent_pick {kind: "research", clear: true}                              remove
subagent_pick {kind: "*"}                                                  list both files
subagent_pick {kind: "research"}                                           ask: a dialog over `eidolon models`
```

Models are named as `eidolon models` prints them (`provider:model`). With no
`model`, `subagent_pick` asks the operator through `choices_user`, listing the
catalog cheapest first (unpriced last, never as free), 12 at most, with price and
context in each description. It is the only tool here that opens a dialog.

**`subagent_spawn` resolves its model**, first hit wins: the `model` argument
(one-off, never saved), the project pick for the kind, the global pick for the
kind. With none of those it **starts nothing** and returns, as a successful
output rather than a refusal:

```json
{"status":"needs_choice","kind":"research",
 "question":"No model picked for kind 'research'. Choose one (or ask the operator), then call subagent_pick {kind, model} or retry with model.",
 "options":[{"model":"zai:glm-5.3-flash","price":"$0.075/0.25 per M","ctx":"1048k ctx"}, ...],
 "total_models":26,
 "retry":{"tool":"subagent_spawn","input":{"task":"...","kind":"research"}}}
```

The calling agent reads it, decides or asks its operator in its own way, then
calls `subagent_pick` (or retries with `model`). `subagent_plan` does the same
for any role whose kind has no pick, and `subagent_pick` itself falls back to
this payload when its dialog cannot be answered (headless). No tool but
`subagent_pick` ever calls `choices_user`. `kind` omitted means kind `default`.

**Asks go up to the orchestrator.** A child cannot ask its operator anything,
and it must not guess. A child that needs a decision ends with the question in
its final text; the orchestrator sees it with `subagent_trace`, decides (or asks
its own operator), and answers with `subagent_steer`.

## Formations

A formation is a JSON file saying which roles to start, how many, and under what
budgets:

```json
{"name": "fanout",
 "roles": [{"role": "scout", "count": 3, "kind": "research",
            "tools": ["read", "grep", "bash"],
            "budget": {"calls": 40, "usd_max": 0.5, "usd_min": 0.1},
            "floor": null}],
 "thresholds": [{"at": 0.7, "action": "step_down"},
                {"at": 0.9, "action": "ask"},
                {"at": 1.0, "action": "stop"}]}
```

Two ship in [`formations/`](formations/): `solo` (one worker) and `fanout`
(three scouts). Your own go in
`${XDG_STATE_HOME:-$HOME/.local/state}/eidolon/subagents/formations/<name>.json`
and win over the shipped ones of the same name. Optional per role: `yolo`,
`deadline_min`. `tools` is **advisory**: it is written into the child's brief and
nothing enforces it, because upstream has no `eidolon run --tools` yet.
`floor` is a model key the role may not be stepped down below.

`subagent_plan {formation, task, kinds?, budget?}` reads a formation, gives each
role the model picked for its kind, and returns the plan:

```json
{"status":"plan","formation":"fanout","source":"...","task":"...",
 "total_usd":1.5,"total_from":"sum of count x usd_max over the roles",
 "roles":[{"role":"scout","count":3,"kind":"research","model":"zai:glm-5.3-flash",
           "model_from":"global pick","tools":["read","grep","bash"],
           "budget":{"calls":40,"usd_max":0.5,"usd_min":0.1},"floor":""}],
 "thresholds":[...],"ladder":[{"model":"zai:glm-5.3-flash","usd":0.325}, ...],
 "warnings":[],"confirm":"Show this plan to the operator, and run workflow subagent/fanout ..."}
```

It spawns nothing and asks nothing: show it to the operator, and run the
`fanout` workflow with it only after they agree. `kinds` overrides a role's kind
(`{"scout": "cheap-research"}`); `budget: {usd}` sets the total the thresholds
are measured against (otherwise the sum of `count x usd_max`, and with neither
the thresholds are inactive). A role whose kind has no pick turns the answer into
`needs_choice` for that kind. Where the formation file is looked for: the state
directory first, then `~/.config/eidolon/plugins/subagent/formations/`, then
copies of the two shipped ones embedded in `tools/plan.rn` (a tool script cannot
ask where its own directory is, so the second is a guess at where
`plugins install` put the plugin; keep the embedded copies in step).

## Budgets

`subagent_spawn {budget: {calls, usd_max, usd_min}}`:

| field | what it does | enforced |
|---|---|---|
| `calls` | the most model calls the child may make. Its own `config.toml` is the operator's with `max_iterations = calls`; the child runs with `--config` on it, so the original is never touched. At the limit the turn ends with an iteration-limit settle | live |
| `usd_max` | a dollar ceiling. Recorded in the child's meta; `subagent_list` / `subagent_trace` flag `OVER usd_max` (even the low end of the spend range is past it) or `may be over` (only the high end is) | after the fact |
| `usd_min` | a reserve: dollars the role may spend before the fan-out workflow will step it down | recorded; the workflow reads it |
| `floor` (formation role) | a model the workflow's step-down may not go below, by price order | by the workflow |

**Spend** is read from the child's journal: `subagent_list` and `subagent_trace`
run `eidolon log --json <journal>` and add up the usage records (turns that
settled or were cancelled, tool-less model calls) and count the model calls. The
journal path is the `session:` line in the child's log; a child that never
printed one reads as spend *unknown*, never zero. Dollars are the catalog's input
and output prices from `eidolon models`, which lists no cache rates, so spend is
a range: cache reads free (low) to cache reads at the input rate (high). A model
the catalog does not price is `unpriced`. Usage is journaled when a turn ends, so
a child in the middle of its one long turn shows nothing until it settles.
`subagent_list {format: "json", ids: [...], wait_s: N}` gives the same facts as
JSON for a program, optionally after sleeping up to 60 s.

## Fan-out workflow

`workflows/fanout.rn` runs a plan: it spawns each role's `count` children (at most
`parallel` at once, default 3), follows them with `subagent_list`, and when
finished children's known spend crosses a threshold (ratio = spend / `total_usd`,
high end of each range):

- `step_down`: each crossed step_down threshold moves the role one rung down the
  plan's `ladder` (the next strictly cheaper priced model), never below the role's
  `floor`, and not for a role that has not yet spent its `usd_min`.
- `ask`: parks the run with the question. Resume with `--answer stop` to stop
  spawning, anything else to go on. Asked once per threshold.
- `stop`: stops spawning; running children are left running and named.

```bash
eidolon workflow run ~/.config/eidolon/plugins/subagent fanout --args '<the plan JSON>'
eidolon workflow resume --plugin <plugin dir> --answer stop <session> <run>   # for a parked run
```

(`workflow run` takes the plugin **directory**, not its name; see the door section.) Add
`parallel`, `poll_s` and `max_polls` to the args to tune the waiting. The report has one block per child (state, spend, the
tail of its log, which is where its report or its question is) and a note for
every threshold that acted.

## From Claude Code, Codex or aoide

An orchestrator that is not an eidolon session has no tools of its own to call;
`workflows/door.rn` is its door. One command, one JSON line back, no dialog:

```bash
eidolon workflow run ~/.config/eidolon/plugins/subagent door --args '{"tool":"spawn","input":{"task":"count the files","kind":"research"}}'
```

`eidolon workflow run` takes the plugin **directory**, not its name, and upstream
has no lookup by name: the directory is
`${XDG_CONFIG_HOME:-$HOME/.config}/eidolon/plugins/subagent`. So the plugin
ships a wrapper that resolves it, and finds the eidolon binary (`EIDOLON_BIN`,
then `PATH`, then `~/.local/bin`; see Prerequisites), and runs exactly the
command above:

```bash
~/.config/eidolon/plugins/subagent/bin/subagent-door '{"tool":"schema"}'
# from a Windows host, with no login shell and no `~` expansion under -e:
wsl.exe -d Ubuntu -e /home/<user>/.config/eidolon/plugins/subagent/bin/subagent-door '{"tool":"schema"}'
```

The argument is the door's args JSON, and anything after it goes to `workflow run`
(`--yolo`). The wrapper's exit code is the door's. `plugins install` copies `bin/`
with its executable bit (it copies files with `fs::copy`, which keeps the mode);
if a copy ever loses it, run `eidolon workflow run
"${XDG_CONFIG_HOME:-$HOME/.config}/eidolon/plugins/subagent" door --args '<json>'`
yourself, or `sh .../bin/subagent-door '<json>'`.

`tool` is `spawn`, `pick`, `plan`, `list`, `trace`, `steer`, `cancel` or `schema`;
`input` is that tool's input, and the tool's answer comes back unchanged as the
`report` of the JSON line. `schema` returns what each tool takes (a summary kept in
`door.rn`: the full manifests are `eidolon tools | grep -A3 subagent_`, always
current). Anything else is refused.

Exit codes of the line: `0` completed, `1` parked, `2` failed (the `reason` says
why: the tool refused or errored), `3` refused (nothing ran, nothing journaled).

The needs_choice loop, from the outside:

1. call `spawn` (or `plan`). If the report's `status` is `needs_choice`, nothing
   started: read `options`, decide or ask your operator.
2. call `pick {kind, model}` (or repeat the call with `model` in `input`).
3. call `spawn` again; poll with `list` (`format: "json"`) and `trace`; answer a
   child's question with `steer`.

The plugin must be **trusted first** (`eidolon plugins trust subagent`): until
it is, the gate asks about every verb the door calls, and nobody is there to
answer.

## Prerequisites

- Any Unix eidolon runs on — Linux, WSL, macOS. Every command in the tool files
  is POSIX sh (and awk, sort, cut): no `timeout` (GNU coreutils, absent on
  macOS), no `setsid` binary, no `/proc`, no `sed -i`, and no `readlink -f`.
- eidolon **with the plugin system** — `eidolon plugins --help` is the proof;
  the `eidolon` on an old `PATH` has none, and the install below fails there.
  The tools shell out to eidolon (`eidolon models`, `eidolon log`, and
  `eidolon run` for the child), so it must be the same plugin-capable one. They
  find it as: **`$EIDOLON_BIN`** if set (and then only that: a wrong value is an
  error, not a fall-through), else `eidolon` on `PATH`, else
  `$HOME/.local/bin/eidolon` -- each only if it is an executable file that
  answers `--version`. None works: the tool fails and says so, naming
  `EIDOLON_BIN`. This matters when the session was not started from a login
  shell (a Windows `wsl.exe -e`, a service, cron): `PATH` there may hold no
  eidolon at all, or a Windows-side stub that is executable and runs nothing. The
  child `subagent_spawn` starts runs the same binary and gets `EIDOLON_BIN` set
  to it.
  Plus `shell_background` and the swarm's `send` (any upstream with the swarm
  built-ins).
- The swarm **not disabled**: `[swarm] enabled = true` in
  `~/.config/eidolon/config.toml`, which is the default. `subagent_spawn`
  refuses when `peers` names no id, and `subagent_steer` cannot deliver: a
  session that is not registered among its peers has nobody to report to and
  nothing to send from.
- A model the child can run, **picked for its kind** (see [Picks](#picks)): a
  tool-calling model with a credential in your config. There is no default and no
  fallback to the config's `default_model` (on a stock install that is `mock`,
  which is not tool-calling). The first spawn of a kind returns `needs_choice`;
  that is the plugin working, not failing.

## Install

```bash
eidolon plugins install noah427/eidolon-plugins subagent
eidolon plugins trust subagent
```

`plugins install` fetches the repo, validates `subagent/plugin.rn`, copies the
folder into `~/.config/eidolon/plugins/subagent/` and records what it fetched.
**Installing does not trust**: the seven verbs land on the gate like any other
caller's, so `plugins trust subagent` is the second, separate act — it vouches
every verb the plugin ships (`spawn`, `pick`, `plan`, `list`, `trace`, `steer`,
`cancel`). Add `--ref <branch|tag|commit>` to pin the install; the default is the
repo's HEAD.

A local checkout works the same way, and is the way to install a branch before
it is pushed:

```bash
eidolon plugins install file:///path/to/eidolon-plugins subagent --ref main
```

## Verify

The listing reads the plugin directory and compiles `plugin.rn`; the tools are
adopted at session build, so `eidolon tools` prints each manifest. A file that
does not compile is a note instead (`[tool] … failed to compile`, or
`not adopted — …`), not a silent absence.

```bash
eidolon plugins --dir ~/.config/eidolon/plugins
# subagent 0.2.1
#     child eidolon sessions for a bounded task: spawn on a model picked per kind, plan formations, budgets, list, trace, steer, cancel, and a fan-out workflow
#     payloads lib, tools, workflows
#     claims   tools: spawn, list, trace, steer, cancel, pick, plan
#     claims   workflows: fanout, door
#     subagent_cancel              vouched by …
#     subagent_list                vouched by …
#     subagent_pick                vouched by …
#     subagent_plan                vouched by …
#     subagent_spawn               vouched by …
#     subagent_steer               vouched by …
#     subagent_trace               vouched by …

eidolon tools 2>&1 | grep -i '"name": "subagent'
# "name": "subagent_spawn",  ... one line per tool
```

A subagent cannot spawn a subagent — the guard line as `tools/spawn.rn`
writes it, run with the variable set:

```bash
EIDOLON_SUBAGENT=1 sh -c '[ -n "$EIDOLON_SUBAGENT" ] && exit 3'
echo $?
# 3
```

The door, and a spawn that has no pick (nothing is started):

```bash
eidolon workflow run ~/.config/eidolon/plugins/subagent door --args '{"tool":"schema"}'
eidolon workflow run ~/.config/eidolon/plugins/subagent door --args '{"tool":"spawn","input":{"task":"x","kind":"zzz"}}'
# {"report":"{\"status\":\"needs_choice\",\"kind\":\"zzz\", ...","run":1,"session":"...","status":"completed"}

# the same, by plugin name, and with no eidolon on PATH:
~/.config/eidolon/plugins/subagent/bin/subagent-door '{"tool":"schema"}'
EIDOLON_BIN=/nonexistent ~/.config/eidolon/plugins/subagent/bin/subagent-door '{"tool":"schema"}'; echo $?
# subagent-door: EIDOLON_BIN is set to /nonexistent, which is not an executable that runs. ...
# 127
```

A spawn, end to end — this needs a model that calls tools and takes a while:

```bash
eidolon run --yolo --model ollama:deepseek-v4.1-flash \
  "Call subagent_pick with kind demo and model ollama:deepseek-v4.1-flash. Then call subagent_spawn with task 'Count slowly: run the bash command sleep 20 three times, then reply done.', kind demo, budget {calls: 30}, deadline_min 3, yolo true. Then call subagent_list. Then run bash 'sleep 15'. Then call subagent_steer with that id and text 'Stop counting and reply steered.'. Then bash 'sleep 20'. Then call subagent_trace with that id. Then call subagent_cancel with that id. Then subagent_list again. Print each tool result verbatim."
# subagent sa-<seconds>-<4 hex>
# pid: <pid>
# log: ~/.cache/eidolon/background/<n>.log
# kind: demo
# model: ollama:deepseek-v4.1-flash (global pick), deadline 3 min
# budget: calls 30 (enforced: max_iterations in the child's own config)
# ...
# sa-<seconds>-<4 hex>  alive  nothing recorded yet (still running, or nobody reaped it)
#   kind demo, model ollama:deepseek-v4.1-flash, started ..., deadline 3 min, pid ...
#   spend: unknown (no journal readable yet: ...)  -- or calls and dollars once a turn has ended
```

## Uninstall

```bash
eidolon plugins uninstall subagent
```

That removes the directory and the plugin's row in the record. It is the one
act that shrinks the record, and the vouch rows are **left alone** — they
answer nothing while no verb of that name is loaded, and a reinstall inherits
the earlier yes. A service still running refuses; this plugin declares none.

The children already spawned are untouched, and so are their state directories
and journals: `kill` them, or `eidolon resume` them, by hand. Your picks
(`picks.json`, `.eidolon/subagent-picks.json`) and formations stay too.

## Limits

- **Depth is one.** `subagent_spawn` refuses before it starts anything when
  `EIDOLON_SUBAGENT` is set in its own environment, and the child's command line
  carries the same guard, so a child that calls the tool gets a refusal rather
  than a grandchild. Only the orchestrator spawns.
- **No `session_model()`.** A tool script cannot ask which model the calling
  session runs on, so the plugin cannot default a child to the parent's model:
  the first spawn of a kind returns `needs_choice`, once.
- **Per-role `tools` are advisory** until upstream has `eidolon run --tools`:
  they go into the child's brief and nothing enforces them.
- **Child spend is not counted by `workflow run --budget-spend`.** That counts
  the workflow's own session; the children are other sessions. The fan-out
  workflow does its own accounting from the children's journals, and that has
  limits: dollars are known only after a child's turn ends (a running child
  counts as zero), they are a range because the catalog lists no cache rates, an
  unpriced model has no dollars at all, and `usd_max` is checked after the fact.
- **A steer lands at the child's next step, not mid-token.** The message wakes
  the child and it reads it before it settles, so it changes the plan; it does
  not interrupt the call already in flight. A child inside a 20-minute `bash`
  reads the steer when that call returns.
- **A child without `yolo` stalls on its first flagged tool call** (`bash`,
  `write`): nobody is at its end to answer the gate. Pass `yolo: true` for any
  child meant to act.
- **Cancel is a signal, not a checkpoint.** SIGTERM to the process group
  (`shell_background` starts the child under `setsid`, so the group is the
  child's whole tree), SIGKILL after 5 s. The state directory and the journal
  stay: the record and the thing `eidolon resume` reads.
- **One report, once, and only if the child starts.** A model that does not
  resolve, or a provider with no credential, never reports; its log says why,
  and `subagent_trace` shows it.
- **The peer id is matched on the journal, not on the title.** `eidolon run`
  registers before it is asked anything, so a headless child's title in `peers`
  is empty; its journal path is exact. The brief's first line still says which
  child it is for anyone reading a roster by eye.
- **The child inherits the parent's directory, config and secrets**, and the
  parent sees its pid, its log and its journal — nothing of its turns. With a
  call budget its config is a copy of yours with one key changed.
- **The door's `schema` is a summary** kept in `workflows/door.rn`; a workflow
  cannot read a tool's manifest (the one built-in that returns them spills past
  8000 characters), so `eidolon tools` is the exact source.
