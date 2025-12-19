run entry_point="DEFAULT": build_container
    cargo run -- run --entry-point {{entry_point}}

test: build_container
    RUST_LOG="serpentine=trace" cargo nextest run --features _test_docker --no-fail-fast containerd

graph:
    cargo run -- graph

clean: build_container
    cargo run -- clean || exit 0
    cargo clean
    docker system reset -f

run_sidecar: build_container
    docker run --rm -it serpent-tools/containerd:dev

build_container:
    docker build -t serpent-tools/containerd:dev -f Dockerfile
    docker container rm -a -f 
