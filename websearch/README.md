# websearch

Web search through Ollama's hosted search API, for an eidolon on a host with no
browser to search with. One verb, no service: the tool calls
`https://ollama.com/api/web_search` over HTTPS with the key from eidolon's secret store.

| tool | file | does |
|---|---|---|
| `websearch_search` | `tools/search.rn` | search for `query`, return up to `max_results` (1 to 10, default 5) as numbered text: title, url, snippet |

```
eidolon ──tools/search.rn──api_request──▶ https://ollama.com/api/web_search
   (the descriptor: base_url fixed in the file,      (Bearer key, put in the
    token_secret "ollama", no other host)             header by eidolon, never
                                                       seen by the script)
```

Reading one page in full is eidolon's built-in `fetch`, which needs no key. This
plugin ships no fetch verb of its own.

## Prerequisites

- Linux, or any host eidolon runs on, with eidolon's plugin runtime (`api_request`
  and `plugins`).
- An Ollama API key. Create one at ollama.com; it is an account key and the search
  calls count against that account.
- Outbound HTTPS to `ollama.com`. A query leaves the machine when this tool runs.

## Install

```bash
eidolon plugins install dxcently/eidolon-plugins websearch
eidolon plugins trust websearch
```

Then the operator stores the key, unless `ollama` is already set (the built-in Ollama
provider reads the same entry). An agent does not type it: run this yourself and paste
the key when it asks, or pipe it from a file you control:

```bash
eidolon secret set ollama
```

The secret is the Ollama provider's own key, shared on purpose. This plugin names it
rather than keeping a copy, so there is one key to rotate.

Then the grant, which records that `websearch_search` may spend the `ollama` secret.
It is the one permission this plugin needs; nothing else is granted:

```bash
eidolon plugins grant --plugin websearch secret:ollama
```

`plugins trust` vouches the verb, and `plugins grant` records the credential. Both are
the operator's yes; run them as written.

## Verify

```bash
eidolon plugins
```

The listing has a `websearch` block with `claims tools: search`,
`claims grants: secret:ollama`, and the verb `websearch_search` marked as vouched and
not as asking. Then ask a session to run `websearch_search` with a query; it returns
numbered results, or a line naming what is missing (the secret, the grant, or the HTTP
status Ollama answered with).

## Uninstall

```bash
eidolon plugins revoke websearch_search secret:ollama
eidolon plugins uninstall websearch
```

`uninstall` leaves vouches and grants in place, as every plugin's does, so the revoke
line is what takes this plugin's access away. The `ollama` secret itself stays: it is
the Ollama provider's key, and deleting it with the plugin would break the provider.
Remove it with `eidolon secret delete ollama` only if the provider is gone for good.

## Trust

- The endpoint is in the tool file's last lines: `https://ollama.com`, nothing else.
  The key is injected into the `Authorization` header by eidolon and is never a value
  the script or a tool result can see.
- The grant is per verb and per source. It is written for `websearch_search` and
  `secret:ollama` only; a verb added later asks.
- Results are candidates, not verified answers. The snippets are Ollama's text about a
  page, so check the url before relying on one.

## Limits

- At most 10 results per call. Ollama's response is returned in full, so a long
  snippet is long; there is no cap on the text.
- Failures name what failed: a missing `ollama` secret names `eidolon secret set
  ollama`; a missing grant names `secret:ollama`; an HTTP error quotes Ollama's body.
- Not tested against the live Ollama API from the repo: the repo's test
  ([`tests/websearch/`](../tests/websearch/)) runs with no network and no key.
