check: test (run "FULL")

run entry_point="DEFAULT": build_container
    cargo run -p serpentine -- run --entry-point {{entry_point}}

test filter="": build_container
    RUST_LOG="serpentine=trace" cargo nextest run --no-fail-fast --jobs num-cpus --features _test_docker {{filter}} 

clean: build_container
    cargo run -p serpentine -- clean || exit 0
    cargo clean
    docker system reset -f

sidecar_logs:
    docker logs serpent-tools.containerd

run_sidecar: build_container
    docker run --rm -it serpent-tools/containerd:dev

build_container:
    docker container rm -f serpent-tools.containerd
    docker build -t serpent-tools/containerd:dev . --pull=false

pull_images:
    grep -iE '^FROM\s+' Dockerfile | awk '{print $2}' | xargs -n1 docker pull || exit 0

size_benchmark: build_container
    cargo build --release -p serpentine
    docker images --filter reference=containerd
    docker history localhost/serpent-tools/containerd:dev
    ls ./target/release/serpentine --size --human-readable
