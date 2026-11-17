{
  lib,
  rustPlatform,
  protobuf,
  zstd,
  aws-lc,
  pkg-config,
}:
let
  cargoToml = lib.importTOML ../serpentine/Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  cargoBuildFlags = [
    "--package"
    "serpentine"
  ];

  doCheck = false;

  nativeBuildInputs = [
    protobuf
    pkg-config
  ];
  buildInputs = [
    zstd
    aws-lc
  ];

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
