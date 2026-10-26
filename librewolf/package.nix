# The librewolf bridge host, built from the workspace.
#
# This is **not** a service eidolon starts. LibreWolf starts it, because the
# extension's native messaging host manifest names this binary as its `path`;
# `eidolon plugins service start` cannot start a browser. So the plugin declares
# no `service:` block, and this package exists to put one executable where an
# operator (or `--print-host-manifest`) can point that manifest at.
{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "eidolon-librewolf";
  version = "0.3.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ./service
      # The workspace has more than one member; cargo loads every member's
      # manifest, so each package's source has to carry all of them.
      ../browser/service
      ../claude/service
      ../godot/service
      ../coordinator/service
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # The workspace has more than one member. Without this, `cargo build` builds every
  # member and `$out/bin` ships both binaries: `nix build .#librewolf` would carry
  # eidolon-browser, and `.#browser` would carry this one. One package, one program.
  cargoBuildFlags = [ "-p" "eidolon-librewolf" ];

  # The same scope for the check phase, and it has to be said out loud: without
  # it `cargo test` runs every workspace member's tests — claude's and browser's
  # included — and this package fails on tests that are not its own (and would
  # report their verdict as its build's). `cargo test -p eidolon-librewolf` is
  # what runs here, and what a person runs by hand. `doCheck = false` would have
  # hidden the problem instead of scoping it.

  # The tests that need a browser are the ones in tests/librewolf/, not these:
  # everything cargo test runs here is a fake extension on the other end of a
  # pipe. `cargo test -p eidolon-librewolf` builds and runs them.
  cargoTestFlags = [ "-p" "eidolon-librewolf" ];

  meta = {
    description = "The native messaging host behind eidolon's librewolf_* tools";
    longDescription = ''
      Run by LibreWolf as the native messaging host its bridge extension names,
      and reachable at the same time on 127.0.0.1:8091 as the endpoint the
      librewolf_* tools dial. `--print-host-manifest` prints the manifest to
      install for this binary.
    '';
    license = lib.licenses.mit;
    mainProgram = "eidolon-librewolf";
    platforms = lib.platforms.linux;
  };
}
