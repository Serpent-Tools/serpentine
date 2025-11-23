run:
    cargo run -p serpentine -- run

run_ci:
    cargo run -p serpentine -- run --ci --clean-old

snapshot:
    cargo insta test --review --unreferenced delete

# WARNING: If a `DOCKER_HOST` env is set serpentine will run its docker test suite.
# This test suite, like serpentine itself, will spawn containers and create images on your host.
# Similarly, running test suite without this env set might cause important integration tests to be skipped.
# (Stuff like the stdlib requires docker access)
test:
    RUST_LOG="serpentine=trace" cargo nextest run --no-fail-fast

# A failing test from serpentine might leave containers running
# To be clear we mean a failing test in serpentine's test suite,
# serpentine itself will only leave containers hanging in the event of a panic or other unexpected pre-mature shutdown
# (ctrl-c / SIGINT is not considered a pre-mature shutdown, and does cause cleanup paths to run).
# 
# WARNING: This might stop/delete other images on your system
# Its only recommended to be run if you don't have other running important containers
clean:
    cargo run -p serpentine -- clean
    docker stop --all
    docker rm --all
    docker system prune --all
