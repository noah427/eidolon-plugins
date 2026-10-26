{
  description = "Extensions for eidolon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # One package per extension that has something to run.
      packages = forAll (pkgs: rec {
        browser = pkgs.callPackage ./browser/package.nix { };
        coordinator = pkgs.callPackage ./coordinator/package.nix { };
        default = browser;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.chromium
          ];
          # Lets `cargo test` run the real-browser tests too.
          EIDOLON_BROWSER_CHROME = pkgs.lib.getExe pkgs.chromium;
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt-rfc-style);
    };
}
