# regex

A small backtracking regex engine for workflow scope, which has no regex.
Supported: literals; `.`; `\s \S \d \D \w \W \n \t` and escaped punctuation;
`[...]` and `[^...]` with ranges; groups `(...)`, `(?:...)`, `(?P<name>...)`;
`|`; greedy `* + ?`; `^` and `$` (always at line boundaries). Anything else
is refused by name at parse, never guessed at. Positions are in characters.

The surface is `regex_compile`, `regex_search`, `regex_search_in`, and the
call a workflow wants is `regex_search(pattern, text)`: `Ok(Some(#{
start, end, groups }))` on a hit, `Ok(None)` on a miss, `Err` for a pattern
the engine refuses.

Copied from `jev/lib/regex.rn` on 2026-10-01, content-identical — the pin
in any manifest declaring this file is that copy's sha256. The engine is
Khoa's, built for `jev`'s graphs; `jev/` carries the original and its
migration is his call. This copy exists so the libraries system's first
consumer does not wait on it.
