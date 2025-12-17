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

build_container:
    docker build -t serpentine/containerd:dev -f containerd.Dockerfile
    docker container rm -a -f 
