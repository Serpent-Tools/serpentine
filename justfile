check: test (run "FULL")

run entry_point="DEFAULT": build_container
    cargo run -p serpentine -- run --entry-point {{entry_point}}

test filter="": build_container
    RUST_LOG="serpentine=trace" cargo nextest run --no-fail-fast --jobs num-cpus --features _test_docker {{filter}} 

update_snapshots: build_container
    NEXTEST_PROFILE="snapshots" cargo insta test --features _test_docker --review --unreferenced delete --test-runner nextest

bench: build_container
    cargo run --release -p serpentine --features _test_docker,_bench -- --bench

build_container:
    docker container rm -f serpent-tools.containerd
    docker build -t serpent-tools/containerd:dev . --pull=false

trivy: build_container
    trivy image --disable-telemetry --exit-code 1 --no-progress localhost/serpent-tools/containerd:dev

clean: build_container
    cargo run -p serpentine -- clean || exit 0
    cargo clean
    docker system reset -f

sidecar_logs:
    docker logs serpent-tools.containerd

run_sidecar: build_container
    docker run --rm -it serpent-tools/containerd:dev

pull_images:
    grep -iE '^FROM\s+' Dockerfile | awk '{print $2}' | xargs -n1 docker pull || exit 0

size_benchmark: build_container
    cargo build --release -p serpentine
    docker images --filter reference=containerd
    docker history localhost/serpent-tools/containerd:dev
    ls ./target/release/serpentine --size --human-readable

count_deps:
    cargo tree --depth 999 --prefix none -p serpentine --edges normal | sort -u | wc -l
    cargo tree --depth 999 --prefix none -p sidecar --edges normal | sort -u | wc -l
