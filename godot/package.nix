# The godot bridge host, built from the workspace. `eidolon plugins service start
# godot` runs it, because the plugin declares it in `plugin.rn`'s `service:` block;
# the other half of the bridge is the addon in `../addon/`, which the operator
# enables in the project they want to lend, and which dials this process.
{
  lib,
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "eidolon-godot";
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
      ../librewolf/service
      ../claude/service
      ../coordinator/service
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # The workspace has more than one member. Without this, `cargo build` builds every
  # member and `$out/bin` ships every binary: `nix build .#godot` would carry
  # eidolon-browser, and `.#browser` would carry this one. One package, one program.
  cargoBuildFlags = [ "-p" "eidolon-godot" ];

  # The same scope for the check phase, and it has to be said out loud: without it
  # `cargo test` runs every workspace member's tests — claude's and the browser's
  # included — and this package fails on tests that are not its own (and would report
  # their verdict as its build's). `cargo test -p eidolon-godot` is what runs here, and
  # what a person runs by hand.
  cargoTestFlags = [ "-p" "eidolon-godot" ];

  meta = {
    description = "The bridge host behind eidolon's godot_* tools";
    longDescription = ''
      The endpoint the godot_* tools dial on 127.0.0.1:8092, in front of the editor
      socket the Godot addon dials on 127.0.0.1:8094. Nothing here starts Godot: the
      engine belongs to the operator, who enables the addon in the project they want
      an agent to work on.
    '';
    license = lib.licenses.mit;
    mainProgram = "eidolon-godot";
    platforms = lib.platforms.linux;
  };
}
