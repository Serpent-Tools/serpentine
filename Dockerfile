FROM alpine:3.24.1@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b as download
RUN apk add tar=1.35-r5 curl=8.21.0-r0

ARG TINI_VERSION=v0.19.0
RUN curl -fsSL "https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini-static" -o /tini && \
    echo "c5b0666b4cb676901f90dfcb37106783c5fe2077b04590973b885950611b30ee  /tini" | sha256sum -c - && \
    chmod +x /tini

FROM golang:1.26.5-bookworm@sha256:1ecb7edf62a0408027bd5729dfd6b1b8766e578e8df93995b225dfd0944eb651 AS go_base

FROM go_base AS cni

ARG CNI_VERSION=v1.9.1
ARG CNI_COMMIT=adc3e6b5b581638afbd194cf2e9319ecbb0151a1

RUN git clone https://github.com/containernetworking/plugins.git /src/cni-plugins && \
    git -C /src/cni-plugins checkout ${CNI_COMMIT}
WORKDIR /src/cni-plugins

# Bump golang.org/x/sys to a release that fixes CVE-2026-39824 (>= v0.44.0);
# no tagged cni-plugins release pins it yet.
RUN GOFLAGS=-mod=mod go get golang.org/x/sys@v0.44.0 && \
    GOFLAGS=-mod=mod go mod tidy && \
    go mod vendor

ENV CGO_ENABLED=0
ENV GOFLAGS="-mod=vendor"
ENV LDFLAGS="-w -s -extldflags -static -X github.com/containernetworking/plugins/pkg/utils/buildversion.BuildVersion=${CNI_VERSION}"

RUN go build -o /cni/loopback -ldflags "$LDFLAGS" ./plugins/main/loopback && \
    go build -o /cni/bridge -ldflags "$LDFLAGS" ./plugins/main/bridge && \
    go build -o /cni/host-local -ldflags "$LDFLAGS" ./plugins/ipam/host-local && \
    go build -o /cni/static -ldflags "$LDFLAGS" ./plugins/ipam/static
RUN strip --strip-all /cni/*

FROM go_base AS runc
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    libbtrfs-dev \
    && rm -rf /var/lib/apt/lists/*

# runc v1.5.1
ARG RUNC_COMMIT=8f2685a471d3347a686ad3909783d8aafc6bb208

RUN git clone https://github.com/opencontainers/runc.git /src/runc && \
    git -C /src/runc checkout ${RUNC_COMMIT}
WORKDIR /src/runc
RUN make BUILDTAGS="" EXTRA_FLAGS="-a" EXTRA_LDFLAGS="-w -s" static
RUN strip --strip-all runc

FROM go_base AS containerd
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y gcc libseccomp-dev \
    && rm -rf /var/lib/apt/lists/*

# containerd v2.3.3
ARG CONTAINERD_COMMIT=aad11006b869517fcd3009450b6f82da282e1a9b

RUN git clone https://github.com/containerd/containerd.git /src/containerd && \
    git -C /src/containerd checkout ${CONTAINERD_COMMIT}

WORKDIR /src/containerd

# Bump golang.org/x/net and golang.org/x/text to releases that fix
# CVE-2026-46600 and CVE-2026-56852; no tagged containerd release pins them yet.
RUN GOFLAGS=-mod=mod go get golang.org/x/net@v0.56.0 golang.org/x/text@v0.39.0 && \
    GOFLAGS=-mod=mod go mod tidy && \
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

FROM rust:1.97.1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa as chef
RUN cargo install cargo-chef@0.1.77 --locked
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

FROM alpine:3.24.1@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b
RUN apk add --no-cache zlib=1.3.2-r0
RUN apk add --no-cache iptables=1.8.13-r0
RUN apk add --no-cache libcrypto3=3.5.7-r0 libssl3=3.5.7-r0 musl=1.2.6-r2 musl-utils=1.2.6-r2

COPY --from=containerd /src/containerd/bin /bin
COPY --from=runc /src/runc/runc /bin/runc
COPY --from=download /tini /bin/tini
COPY --from=cni /cni /cni
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/sidecar /bin

EXPOSE 8000
ENTRYPOINT ["/bin/tini", "--", "/bin/sidecar"]
