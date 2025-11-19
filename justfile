run:
    RUST_LOG="serpentine=trace" cargo run -p serpentine -- --pipeline ci/main.snek

run_ci:
    cargo run -p serpentine -- --pipeline ci/main.snek --ci

snapshot:
    cargo insta test --review --unreferenced delete

test:
    RUST_LOG=serpentine=trace cargo nextest run --no-fail-fast
