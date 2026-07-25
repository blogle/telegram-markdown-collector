{
  description = "Reproducible Rust environment and OCI image for telegram_collector";

  # Keep the Rust toolchain and native linker in one pinned Nix closure. The
  # current dependency graph includes edition-2024 crates, so 24.11's Cargo is
  # too old even though the collector itself remains edition 2021.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          craneLib = crane.mkLib pkgs;
          commonArgs = {
            pname = "telegram_collector";
            version = "0.1.0";
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.sqlite ];
          };
          # Keep the expensive dependency graph in its own derivation. Normal
          # source edits then rebuild only the collector crate and image layer.
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          collector = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
          });
          ociImage = pkgs.dockerTools.buildLayeredImage {
            name = "telegram_collector";
            tag = "latest";
            contents = [ collector pkgs.cacert ];
            config = {
              User = "1000:1000";
              WorkingDir = "/";
              Entrypoint = [ "${collector}/bin/telegram_collector" ];
            };
          };
        in {
          default = collector;
          inherit ociImage;
        });

      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.rustc
              pkgs.cargo
              pkgs.rustfmt
              pkgs.clippy
              pkgs.stdenv.cc
              pkgs.binutils
              pkgs.pkg-config
              pkgs.sqlite
            ];
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.sqlite ];
          };
        });
    };
}
