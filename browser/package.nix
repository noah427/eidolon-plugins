# The browser service, built from the workspace, with Chromium from nixpkgs.
# `nix run .#browser` starts it.
{
  lib,
  rustPlatform,
  makeWrapper,
  chromium,
}:

rustPlatform.buildRustPackage {
  pname = "eidolon-browser";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ./service
      # The workspace has more than one member; cargo loads every member's
      # manifest, so each package's source has to carry all of them.
      ../librewolf/service
      ../claude/service
      ../godot/service
      ../coordinator/service
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # The workspace has more than one member. Without this, `cargo build` builds every
  # member and `$out/bin` ships both binaries: `nix build .#librewolf` would carry
  # eidolon-browser, and `.#browser` would carry this one. One package, one program.
  cargoBuildFlags = [ "-p" "eidolon-browser" ];

  # The same scope for the check phase, and it has to be said out loud: without it
  # `cargo test` runs every workspace member's tests — claude's and librewolf's
  # included — so this package fails on tests that are not its own and reports their
  # verdict as its build's. The claude tests in particular drive a fake CLI through a
  # Python shebang, an input this package does not declare. `cargo test
  # -p eidolon-browser` is what runs here, and what a person runs by hand.
  cargoTestFlags = [ "-p" "eidolon-browser" ];

  nativeBuildInputs = [ makeWrapper ];

  # The real-browser tests skip themselves without EIDOLON_BROWSER_CHROME,
  # so this runs the unit tests only.
  postInstall = ''
    wrapProgram $out/bin/eidolon-browser \
      --set-default EIDOLON_BROWSER_CHROME ${lib.getExe chromium}
  '';

  meta = {
    description = "One shared headless Chromium behind eidolon's browser_* tools";
    license = lib.licenses.mit;
    mainProgram = "eidolon-browser";
    platforms = lib.platforms.linux;
  };
}
