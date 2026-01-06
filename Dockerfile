FROM debian:bookworm-slim as tini
ENV DEBIAN_FRONTEND=noninteractive

ADD https://github.com/krallin/tini/releases/latest/download/tini-static /tini
RUN chmod +x /tini

FROM rustlang/rust:nightly-slim AS youki
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    git \
    pkg-config \
    libsystemd-dev \
    build-essential \
    libelf-dev \
    libseccomp-dev \
    libclang-dev \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

ARG YOUKI_VERSION=v0.5.7

RUN git clone --depth 1 --branch ${YOUKI_VERSION} \
    https://github.com/youki-dev/youki.git /src/youki
WORKDIR /src/youki
RUN sed -i '/\[profile\. release\]/,/lto = true/ c\
[profile.release]\
opt-level = 3\
codegen-units = 1\
strip = "symbols"\
lto = "fat"\
panic = "abort"\
' Cargo.toml

ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo +nightly build --release -p youki --target=x86_64-unknown-linux-gnu --features v2

FROM golang:1.24-bookworm AS containerd
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y gcc libseccomp-dev \
    && rm -rf /var/lib/apt/lists/*

ARG CONTAINERD_VERSION=v2.2.1

RUN git clone --depth 1 --branch ${CONTAINERD_VERSION} \
    https://github.com/containerd/containerd.git /src/containerd

WORKDIR /src/containerd

RUN sed -i \
    -e '/plugins\/imageverifier/d' \
    -e '/plugins\/nri/d' \
    -e '/plugins\/restart/d' \
    -e '/plugins\/sandbox/d' \
    -e '/plugins\/services\/images/d' \
    -e '/plugins\/services\/introspection/d' \
    -e '/plugins\/services\/sandbox/d' \
    -e '/plugins\/services\/transfer/d' \
    -e '/plugins\/services\/streaming/d' \
    -e '/plugins\/transfer/d' \
    -e '/plugins\/streaming/d' \
    -e '/plugins\/snapshots\/btrfsd/d' \
    -e '/plugins\/snapshots\/native/d' \
    -e '/plugins\/snapshots\/blockfile/d' \
    -e '/plugins\/snapshots\/devmapper/d' \
    -e '/plugins\/snapshots\/erofs/d' \
    -e '/plugins\/diff\/erofs/d' \
    -e '/plugins\/mount\/erofs/d' \
    -e '/plugins\/cri/d' \
    -e '/pkg\/tracing/d' \
    -e '/zfs/d' \
    cmd/containerd/builtins/*.go

ENV BUILDTAGS="no_cri no_btrfs no_devmapper no_zfs no_dynamic_plugins"
RUN make BUILDTAGS="$BUILDTAGS" STATIC=1 bin/containerd
RUN make BUILDTAGS="$BUILDTAGS" STATIC=1 bin/containerd-shim-runc-v2
RUN strip --strip-all bin/containerd
RUN strip --strip-all bin/containerd-shim-runc-v2

FROM rustlang/rust:nightly-slim AS chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ENV RUSTFLAGS="-C target-feature=+crt-static"
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release -p sidecar --recipe-path recipe.json
COPY . .
RUN cargo build --release -p sidecar

# FROM scratch
FROM debian:bookworm-slim
COPY --from=containerd /src/containerd/bin /bin
COPY --from=youki /src/youki/target/x86_64-unknown-linux-gnu/release/youki /bin/runc
COPY --from=tini /tini /bin/tini
COPY --from=builder /app/target/release/sidecar /bin

RUN apt update && apt install -y vim
RUN mkdir -p test/rootfs

EXPOSE 8000
ENTRYPOINT ["/bin/tini",  "--", "/bin/sidecar"]
