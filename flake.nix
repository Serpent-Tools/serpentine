{
  description = "Powerful simplistic workflow runner.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        pkgs = import nixpkgs {
          inherit system;
        };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "serpentine";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "Powerful simplistic workflow runner.";
            license = pkgs.lib.licenses.mit;
            mainProgram = "serpentine";
            homepage = "https://github.com/Serpent-Tools/serpentine";
            maintainers = [
              {
                github = "vivax3794";
                email = "vivax3794@protonmail.com";
                name = "Viv";
              }
            ];
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            (fenix.packages.${system}.latest.withComponents [
              "cargo"
              "clippy"
              "rustc"
              "rust-src"
              "rustfmt"
            ])
            just
          ];
        };
      }
    );
}
