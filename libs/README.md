# Libraries

Rune libraries shared **at source, self-contained at runtime**: a plugin's
`plugin.rn` may declare a library dependency, and `eidolon plugins install`
vendors a pinned copy into the plugin's own `lib/` at install time. Nothing
at runtime knows the difference — a vendored file is the plugin's own
`lib/*.rn`, hashed into every workflow run's pin, so the bytes that ran are
journaled and replay-verified by machinery that was already there.

Trust is unchanged: a shared lib is Rune under the same fail-closed compile,
and source-sharing cannot widen what a plugin may do.

## Layout

```
libs/<name>/<name>.rn     the library: one file, self-contained — it may call
                          Rune built-ins and nothing outside itself
libs/<name>/README.md     what it is, where it came from
```

## Declaring one

```
pub fn manifest() {
    #{
        name: "myplugin",
        libraries: [
            #{
                repo: "dxcently/eidolon-plugins",
                path: "libs/regex/regex.rn",
                pin: "sha256:…",
            },
        ],
    }
}
```

The pin is the **content's sha256** — the house spelling, the way `jev`'s
graphs pin. The install verifies it and refuses, naming both hashes, when
the bytes at the source are not the bytes the manifest reviewed. A library
that moved and a manifest that still pins the old bytes install nothing;
change the library and re-pin in the same commit, the discipline
`tests/jev/pin.sh` keeps for graphs and `tests/libsfixture/pin.sh` keeps
for this tier.

A plugin cannot vendor over a file its own tree carries, and two declared
libraries cannot vendor under one name: two sources for one file is a
manifest bug, refused at install.
