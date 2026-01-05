run entry_point="DEFAULT": build_container
    cargo run -p serpentine -- run --entry-point {{entry_point}}

test: build_container
    RUST_LOG="serpentine=trace" cargo nextest run --features _test_docker --no-fail-fast 

graph:
    cargo run -- graph

clean: build_container
    cargo run -p serpentine -- clean || exit 0
    cargo clean
    docker system reset -f

run_sidecar: build_container
    docker run --rm -it serpent-tools/containerd:dev

build_container:
    docker container rm -f serpent-tools.containerd
    docker build -t serpent-tools/containerd:dev -f Dockerfile

size_benchmark: build_container
    cargo build --release -p serpentine
    docker images --filter reference=containerd
    docker history localhost/serpent-tools/containerd:dev
    ls ./target/release/serpentine --size --human-readable
