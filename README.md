# eidolon-plugins

Plugins for [eidolon](https://github.com/noah427/eidolon). Each folder is
one plugin, installed with `eidolon plugins install dxcently/eidolon-plugins
<name>` or by an agent reading its README. This repo is the curated
collection, not the only source: any git repo with a `plugin.rn` folder
installs the same way.

## The plugins

| plugin | gives eidolon | runs alongside | install |
|---|---|---|---|
| [`browser/`](browser/) | `browser_open`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_read`, `browser_back`, `browser_state`; workflows `page_walk`, `page_watch` | `eidolon-browser`, a Rust service driving one headless Chromium | `eidolon plugins install dxcently/eidolon-plugins browser` — see its README for the service |
| [`subagent/`](subagent/) | `subagent_spawn`, `subagent_pick`, `subagent_plan`, `subagent_list`, `subagent_trace`, `subagent_steer`, `subagent_cancel` (models picked per kind, formations, budgets, spend); workflows `fanout`, `door` | `eidolon run`, child sessions in the background | `eidolon plugins install dxcently/eidolon-plugins subagent`, then `eidolon plugins trust subagent` |
| [`coordinator/`](coordinator/) | `coordinator_adopt`, `coordinator_release`, `coordinator_adoptions`, `coordinator_notices` | `eidolon-coordinator`, a monitor that watches the workers a coordinator adopted and sends one notice per observed halt | `eidolon plugins install dxcently/eidolon-plugins coordinator`, then `eidolon plugins trust coordinator` — see its README |
| [`jev/`](jev/) | automation graphs as workflows: `find-related`, `ctf-juice-recon`, `triage-linux`, `triage-wsl`, `triage-botforge`, `wiki-hop`, `selftest`; tools `jev_graph`, `jev_mark`, `jev_choose`, `jev_entail` and eleven one-command recon tools (`jev_os_release`, ... `jev_juice_robots`) | for the chooser graphs, the jev service (Python and torch, not shipped here) | `eidolon plugins install dxcently/eidolon-plugins jev`, then `eidolon plugins trust jev` |

All need eidolon with the plugin runtime (upstream `master`); `jev` also needs workflows.

**Web search** isn't here on purpose: eidolon ships it as the built-in
`search` tool, tied to the session's own key. To read a result, use `fetch`,
or `browser_open` + `browser_read` for pages that need a real browser.

## Installing one

Tell an agent:

> Install the `<name>` plugin from this repo: read `<name>/README.md` and
> follow it.

Or do it yourself. Every plugin README has the same four sections, run in
order: **Prerequisites**, **Install**, **Verify**, **Uninstall**. See
[AGENTS.md](AGENTS.md) for the rules an agent follows while doing it.

## Layout

```
<name>/                        → ~/.config/eidolon/plugins/<name>/
├── plugin.rn      the manifest: name, version, description, claims
├── README.md      what it is, and Prerequisites / Install / Verify / Uninstall
├── tools/*.rn     one file per tool: the stem is the bare verb, registration is <name>_<verb>
├── lib/*.rn       Rune compiled in front of the workflows (optional)
├── workflows/*.rn programs that drive the plugin's tools (optional)
├── service/       a Rust crate, when the tools need something running beside eidolon
└── package.nix    how the flake builds that service
```

A service the plugin owns is that Rust crate. The exception is a model server
the operator runs separately (jev's chooser): it is not shipped, and the plugin's
README says how to start it, its port and its token file; the plugin's tools
fail or park with a reason when it is down.

The folder *is* the plugin directory: install it by putting the folder where
eidolon looks, and every `tools/*.rn` is adopted as `<name>_<verb>`, attributed
to the plugin in the gate's question and the log's audit lines. A tool file
carries everything it needs, endpoint included (upstream's rule: a tool is one
file you can hand to somebody). `plugin.rn` declares only what the plugin says
about itself — the layout is what names the verbs. A plugin cannot shadow a
built-in.

Upstream eidolon without the plugin runtime wants the flat install instead:
`tools/*.rn` copied into `~/.config/eidolon/tools/` under their own names
(`browser_open.rn`, …). That layout is in this repo's history before the plugin
port; the two do not share tool filenames.

## Building

```bash
nix build .#browser          # the service, with Chromium from nixpkgs
nix develop                  # cargo, clippy, chromium; `cargo test` runs the real-browser tests too
cargo test                   # without Nix: unit tests, plus browser tests if EIDOLON_BROWSER_CHROME is set
```

Linux only (WSL counts), same as upstream eidolon.

## License

MIT
