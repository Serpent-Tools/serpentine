{
  mkShell,
  rustPlatform,
  cargo,
  cargo-edit,
  cargo-insta,
  cargo-nextest,
  clippy,
  just,
  mdbook,
  mdbook-mermaid,
  protobuf,
  rustc,
  rustfmt,
}:
mkShell {
  packages = [
    cargo
    clippy
    rustc
    # `rustfmt.toml` sets options that are still nightly-only.
    (rustfmt.override { asNightly = true; })
    just
    protobuf
    cargo-nextest
    cargo-insta
    cargo-edit

    mdbook
    mdbook-mermaid
  ];

  env.RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
}
