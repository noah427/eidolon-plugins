# browser

A real browser for eidolon: one headless Chromium, shared by every session on
the machine, driven through six tools, plus `browser_state` — the read a watch's workflow cannot do — and the `page_walk` / `page_watch` workflows.

| tool | does |
|---|---|
| `browser_open` | go to a URL; with `confine`, lock the browser to a list of origins |
| `browser_snapshot` | the page as an accessibility tree, each element with a `[ref=eN]` |
| `browser_click` | click a ref from the latest snapshot |
| `browser_type` | type into a ref, optionally pressing Enter |
| `browser_read` | the page's visible text |
| `browser_back` | back one history entry |

```
eidolon ──tools/*.rn──api_request──▶ eidolon-browser (127.0.0.1:8090) ──CDP──▶ Chromium
                                     token: ~/.config/eidolon/browser.token
```

A ref is only good until the page changes: any click, type, open or back
invalidates them, and a stale ref is refused by name. `confine` routes the
browser through a proxy inside the service that refuses any other origin
before a connection is made, and that covers redirects, popups, iframes and
page scripts.

## Prerequisites

- Linux or WSL.
- eidolon with `api_request` (upstream since `a618ef1`, 2026-09-21) **and the
  plugin runtime** — the tools are installed as a plugin directory, so an older
  eidolon can't compile them and **Verify** shows fewer than 6.
- One of:
  - **Nix** with flakes, or
  - **Rust** 1.85+ (`cargo`) and a **Chromium** or Chrome binary.

## Install

1. The service, either way:

   ```bash
   # Nix: builds it with Chromium included
   nix profile install github:noah427/eidolon-plugins#browser
   ```

   ```bash
   # Cargo: from a clone of this repo; Chromium comes from PATH or EIDOLON_BROWSER_CHROME
   cargo install --path browser/service
   ```

2. The plugin — this directory *is* the plugin directory:

   ```bash
   mkdir -p ~/.config/eidolon/plugins
   cp -r browser ~/.config/eidolon/plugins/browser
   ```

   (A symlink keeps one copy: `ln -s "$PWD/browser" ~/.config/eidolon/plugins/browser`.
   From a Nix-only install without a clone: `git clone https://github.com/noah427/eidolon-plugins` first.)

   Then vouch its verbs — that is what stops the gate asking on every call — and
   grant the token file each verb reads, so the credential is a recorded
   permission instead of a side effect of the copy:

   ```bash
   eidolon plugins trust browser
   for v in open snapshot click type read back; do
     eidolon plugins grant browser_$v file:~/.config/eidolon/browser.token
   done
   ```

   With a person at the console the first call of each verb asks instead, once
   per verb, and the answer is written to the same store. Headless there is
   nobody to answer, so grant them here.

   Coming from the flat install (the commit before this one)? Remove the old
   copies first — `rm -f ~/.config/eidolon/tools/browser_*.rn` — or the plugin's
   verbs collide with the operator's own and the plugin's are refused by name.

3. Start the service. It makes the token file on first run.

   The manifest declares the daemon, so the harness drives it — and the
   operator's yes is recorded against the declaration itself:

   ```bash
   eidolon plugins service approve browser   # the yes, bound to the declaration's hash
   eidolon plugins service start browser     # spawns it detached, waits for /health, reports the pid
   eidolon plugins service status browser    # probes the port; never trusts a record
   ```

   `start` refuses until the declaration is approved, and asks again if the
   `service:` block changes — an update cannot inherit the right to run a
   process. The child outlives the CLI and its output appends to
   `<cache>/eidolon/services/browser.log`; `stop` signals the recorded pid
   only after `/proc` agrees it is that process.

   On a machine that should always have it, a systemd user unit is still the
   better tool for that job:

   ```bash
   mkdir -p ~/.config/systemd/user
   cat > ~/.config/systemd/user/eidolon-browser.service <<'EOF'
   [Unit]
   Description=eidolon browser service

   [Service]
   ExecStart=%h/.nix-profile/bin/eidolon-browser
   Restart=on-failure

   [Install]
   WantedBy=default.target
   EOF
   systemctl --user daemon-reload && systemctl --user enable --now eidolon-browser
   ```

   With a Cargo install, `ExecStart=%h/.cargo/bin/eidolon-browser`.

## Verify

```bash
curl -s http://127.0.0.1:8090/health
# {"chromium_installed":true,"status":"ok"}
eidolon plugins | grep -A8 '^browser '     # the verbs and workflows, and whether each is vouched
eidolon tools | grep -c '"name": "browser_'
# 7
```

`chromium_installed: false` means no Chromium was found: set
`EIDOLON_BROWSER_CHROME` to its path.

## Uninstall

```bash
systemctl --user disable --now eidolon-browser 2>/dev/null
rm -f ~/.config/systemd/user/eidolon-browser.service
rm -rf ~/.config/eidolon/plugins/browser ~/.config/eidolon/browser.token
nix profile remove browser 2>/dev/null || cargo uninstall eidolon-browser
```

The vouch and grant rows stay behind in `~/.config/eidolon/policy.permits.rn`:
`eidolon plugins untrust browser` drops the vouches, and
`eidolon plugins revoke browser_open file:~/.config/eidolon/browser.token` drops
one grant (once per verb). They are inert once the directory is gone, but the
store is the record and it should say what is true.

## Settings

All optional, read by the service at start.

| variable | default | |
|---|---|---|
| `EIDOLON_SERVICE_PORT` | `8090` | change `base_url` in every `tools/browser_*.rn` to match |
| `EIDOLON_BROWSER_TOKEN_FILE` | `~/.config/eidolon/browser.token` | change `token_file` in the tools to match |
| `EIDOLON_BROWSER_CHROME` | `chromium`, then `google-chrome` on PATH | the Nix package sets it |
| `EIDOLON_BROWSER_NO_SANDBOX` | unset | `1` turns Chromium's sandbox off; set it only if Chromium can't start its sandbox (some containers). Any other value keeps it on |

## How it works

`service/` is the Rust crate. `src/main.rs` is the HTTP side (`/health`,
`/call`, auth). `src/browser.rs` is the browser and the six methods.
`src/snapshot.rs` prints Chrome's accessibility tree, `src/filter.rs` does
`roles`/`within`/`section`, `src/proxy.rs` handles `confine`, and
`src/sweep.rs` cleans up profile directories left by a crash.

Tests: `cargo test` runs the unit tests. With `EIDOLON_BROWSER_CHROME` set
(it is in `nix develop`), it also runs `tests/real_browser.rs`, which drives
real Chromium against two local test sites and checks, among other things,
that a confined page never reaches the other site.
