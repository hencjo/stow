{
  description = "stow";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          stow = pkgs.rustPlatform.buildRustPackage {
            pname = "stow";
            version = "0.1.0";

            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            CARGO_BUILD_TARGET = pkgs.stdenv.hostPlatform.rust.rustcTargetSpec;
          };

          default = self.packages.${system}.stow;
        }
      );
    };
}
