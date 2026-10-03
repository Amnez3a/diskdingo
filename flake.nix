{
  description = "Diskdingo - command-line tool for working with disks";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems ( s: f nixpkgs.legacyPackages.${s});
    in {
      packages = forAll ( pkgs: rec {
        diskdingo = pkgs.rustPlatform.buildRustPackage {
          pname = "diskdingo";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta.mainProgram = "diskdingo";
        };
        default = diskdingo;
      });
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      });
    };
}
