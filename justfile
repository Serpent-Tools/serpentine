run:
    RUST_LOG=trace cargo run -p serpentine -- --pipeline ci/main.snek

snapshot:
    cargo insta test --review --unreferenced delete

test:
    cargo nextest run
