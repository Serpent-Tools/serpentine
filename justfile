run:
    RUST_LOG="serpentine=trace" cargo run -p serpentine -- --pipeline ci/main.snek

snapshot:
    cargo insta test --review --unreferenced delete

test:
    RUST_LOG=serpentine=trace cargo nextest run --no-fail-fast
