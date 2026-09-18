{
  lib,
  rustPlatform,
  protobuf,
}:
let
  cargoToml = lib.importTOML ../serpentine/Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  # The flake packages serpentine, not the workspace's other binaries.
  cargoBuildFlags = [
    "--package"
    "serpentine"
  ];

  # The test suite drives a docker daemon.
  doCheck = false;

  nativeBuildInputs = [ protobuf ];

  meta = {
    inherit (cargoToml.package) description homepage;
    license = lib.licenses.mit;
    mainProgram = cargoToml.package.name;
    platforms = lib.platforms.unix ++ lib.platforms.windows;
    maintainers = [
      {
        github = "vivax3794";
        email = "vivax3794@protonmail.com";
        name = "Viv";
      }
    ];
  };
}
