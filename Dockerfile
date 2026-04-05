FROM alpine as download
RUN apk add tar curl

RUN curl -fsSL https://github.com/krallin/tini/releases/latest/download/tini-static -o /tini && \
    chmod +x /tini

FROM golang:1.26-bookworm AS cni

ARG CNI_VERSION=v1.9.1

RUN git clone --depth 1 --branch ${CNI_VERSION} \
    https://github.com/containernetworking/plugins.git /src/cni-plugins
WORKDIR /src/cni-plugins

ENV CGO_ENABLED=0
ENV GOFLAGS="-mod=vendor"
ENV LDFLAGS="-w -s -extldflags -static -X github.com/containernetworking/plugins/pkg/utils/buildversion.BuildVersion=${CNI_VERSION}"

RUN go build -o /cni/loopback -ldflags "$LDFLAGS" ./plugins/main/loopback && \
    go build -o /cni/bridge -ldflags "$LDFLAGS" ./plugins/main/bridge && \
    go build -o /cni/host-local -ldflags "$LDFLAGS" ./plugins/ipam/host-local && \
    go build -o /cni/static -ldflags "$LDFLAGS" ./plugins/ipam/static
RUN strip --strip-all /cni/*

FROM golang:1.26-bookworm AS runc
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    libbtrfs-dev \
    && rm -rf /var/lib/apt/lists/*

ARG RUNC_VERSION=v1.4.2

RUN git clone --depth 1 --branch ${RUNC_VERSION} \
    https://github.com/opencontainers/runc.git /src/runc
WORKDIR /src/runc
RUN make BUILDTAGS="" EXTRA_FLAGS="-a" EXTRA_LDFLAGS="-w -s" static
RUN strip --strip-all runc

FROM golang:1.26-bookworm AS containerd
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y gcc libseccomp-dev \
    && rm -rf /var/lib/apt/lists/*

ARG CONTAINERD_VERSION=v2.2.2

RUN git clone --depth 1 --branch ${CONTAINERD_VERSION} \
    https://github.com/containerd/containerd.git /src/containerd

WORKDIR /src/containerd

RUN sed -i 's|google.golang.org/grpc v1.78.0|google.golang.org/grpc v1.79.3|' go.mod && \
    go mod download google.golang.org/grpc@v1.79.3 && \
    go mod tidy && \
    go mod vendor

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

FROM rust as chef
RUN cargo install cargo-chef
WORKDIR /app

FROM chef as planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef as builder
ENV RUSTFLAGS="-C target-feature=+crt-static"
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release -p sidecar --target x86_64-unknown-linux-gnu --recipe-path recipe.json
COPY . .
RUN cargo build --release -p sidecar --target x86_64-unknown-linux-gnu

FROM alpine
RUN apk upgrade zlib --no-cache
RUN apk add --no-cache iptables

COPY --from=containerd /src/containerd/bin /bin
COPY --from=runc /src/runc/runc /bin/runc
COPY --from=download /tini /bin/tini
COPY --from=cni /cni /cni
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/sidecar /bin

EXPOSE 8000
ENTRYPOINT ["/bin/tini", "--", "/bin/sidecar"]
