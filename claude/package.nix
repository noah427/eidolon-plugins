# The claude driver host, built from the workspace.
#
# This is the program the driver plugin's service declaration names, and the one
# `eidolon plugins service start claude` runs.
#
# What it is *not*: an external-CLI wrapper does not live in eidolon, and this is
# the side that holds the CLI. Nothing here reads a credential.
{
  lib,
  python3,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "eidolon-claude";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ./service
      # The workspace has more than one member; cargo loads every member's
      # manifest, so each package's source has to carry all of them.
      ../browser/service
      ../godot/service
      ../coordinator/service
      ../librewolf/service
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # The workspace has more than one member. Without this, `cargo build` builds every
  # member and `$out/bin` ships every binary. One package, one program.
  cargoBuildFlags = [ "-p" "eidolon-claude" ];

  # The same scope for the check phase: without it `cargo test` runs every workspace
  # member's tests, and this package's verdict would carry theirs. `cargo test
  # -p eidolon-claude` runs the tests below and nothing else — which is also why the
  # interpreter and the shebang rewrite are declared *here* and nowhere else.
  cargoTestFlags = [ "-p" "eidolon-claude" ];

  # The tests spawn a fake CLI, and the fake CLI is a Python script (`#!/usr/bin/env
  # python3`, the shape a real adapter's stream has to be driven through). Without an
  # interpreter in the sandbox every real-process test fails to spawn it and the
  # package does not build at all — which is how this was found, rather than by
  # reading. Declared rather than skipped: a check that quietly does not run is worse
  # than one that does not exist.
  nativeCheckInputs = [ python3 ];

  # And the fixture's own shebang has to point at that interpreter. `#!/usr/bin/env
  # python3` is the right line for a script that runs anywhere a person might run it,
  # and it is the wrong line in a sandbox that has no `/usr/bin/env` — the spawn fails
  # with `ENOENT`, which reads exactly like "the CLI is missing" and is not. Rewriting
  # the shebang is what makes the interpreter this build declared the one the fixture
  # actually runs under.
  preCheck = ''
    patchShebangs claude/service/tests/fixtures
  '';

  meta = {
    description = "The driver host a session dials to run a turn on an external CLI";
    longDescription = ''
      The half of the driver that runs beside the session and owns the external
      CLI. It binds a private unix socket at $XDG_RUNTIME_DIR/eidolon-claude.sock
      and speaks one length-prefixed JSON frame at a time: hello, run_turn,
      cancel, and the events, session mark, usage and settle that come back.

      `eidolon-claude --real --cli <path>` spawns that CLI, translates its
      stream-json into those frames, and serves the two doors it needs to reach
      the harness's gate: a pre-tool hook in own-tools mode, and an MCP server in
      registry mode. Without `--real` it runs a fake backend that scripts a turn,
      which is what the session half is tested against. It reads no credential
      and reaches no network of its own.
    '';
    license = lib.licenses.mit;
    mainProgram = "eidolon-claude";
    platforms = lib.platforms.linux;
  };
}
