{
  description = "Powerful simplistic workflow runner.";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems =
        function: nixpkgs.lib.genAttrs systems (system: function nixpkgs.legacyPackages.${system});
    in
    {
      overlays.default = final: _prev: {
        serpentine = final.callPackage ./nix/package.nix { };
      };

      packages = forAllSystems (pkgs: rec {
        serpentine = pkgs.callPackage ./nix/package.nix { };
        default = serpentine;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/shell.nix { };
      });
    };
}
