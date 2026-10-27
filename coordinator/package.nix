# The coordination monitor, built from the workspace.
#
# It is the process the plugin's `service:` block declares: the one that holds the
# adoption records, watches the workers that were adopted, and sends one notice
# per observed halt. `eidolon plugins service start coordinator` runs it detached,
# and readiness is the `/health` probe below — the same block the tools dial.
{
  lib,
  rustPlatform,
  bash,
}:

rustPlatform.buildRustPackage {
  pname = "eidolon-coordinator";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ./service
      # The rank blocks are not compiled in — the monitor reads them from the
      # installed plugin folder at run time — but `cargo test -p eidolon-coordinator`
      # asserts that the shipped blocks are placeholders, so the check phase needs
      # them in the source it starts from.
      ./ranks
      # The workspace has more than one member; cargo loads every member's
      # manifest, so each package's source has to carry the other one.
      ../browser/service
      ../librewolf/service
      ../claude/service
      ../godot/service
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # One package, one program: without this the workspace's other members build
  # too, and `.$coordinator` would carry eidolon-browser.
  cargoBuildFlags = [ "-p" "eidolon-coordinator" ];

  # The check phase runs this crate's tests and no others: they are the monitor's
  # own — the occurrence dedup, the notice lifecycle, the release rules — and they
  # need no harness, no roster and no send.
  cargoTestFlags = [ "-p" "eidolon-coordinator" ];
  # The real-child placement fixtures generate Bash scripts during the tests.
  nativeCheckInputs = [ bash ];

  meta = {
    description = "The coordination monitor behind eidolon's coordinator_* tools";
    longDescription = ''
      Holds the explicit adoption records, watches the workers they name over the
      harness's structured roster, and sends one notice per observed halt through
      `eidolon send`. It never resumes, steers or cancels anything, and it never
      claims a delivery the transport did not report.
    '';
    license = lib.licenses.mit;
    mainProgram = "eidolon-coordinator";
    platforms = lib.platforms.linux;
  };
}
