FROM alpine:3.21@sha256:22e0ec13c0db6b3e1ba3280e831fc50ba7bffe58e81f31670a64b1afede247bc as download
RUN apk add tar=1.35-r2 curl=8.14.1-r2

ARG TINI_VERSION=v0.19.0
RUN curl -fsSL "https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini-static" -o /tini && \
    echo "c5b0666b4cb676901f90dfcb37106783c5fe2077b04590973b885950611b30ee  /tini" | sha256sum -c - && \
    chmod +x /tini

FROM golang:1.26-bookworm@sha256:4f4ab2c90005e7e63cb631f0b4427f05422f241622ee3ec4727cc5febbf83e34 AS cni

ARG CNI_VERSION=v1.9.1
ARG CNI_COMMIT=adc3e6b5b581638afbd194cf2e9319ecbb0151a1

RUN git clone https://github.com/containernetworking/plugins.git /src/cni-plugins && \
    git -C /src/cni-plugins checkout ${CNI_COMMIT}
WORKDIR /src/cni-plugins

ENV CGO_ENABLED=0
ENV GOFLAGS="-mod=vendor"
ENV LDFLAGS="-w -s -extldflags -static -X github.com/containernetworking/plugins/pkg/utils/buildversion.BuildVersion=${CNI_VERSION}"

RUN go build -o /cni/loopback -ldflags "$LDFLAGS" ./plugins/main/loopback && \
    go build -o /cni/bridge -ldflags "$LDFLAGS" ./plugins/main/bridge && \
    go build -o /cni/host-local -ldflags "$LDFLAGS" ./plugins/ipam/host-local && \
    go build -o /cni/static -ldflags "$LDFLAGS" ./plugins/ipam/static
RUN strip --strip-all /cni/*

FROM golang:1.26-bookworm@sha256:4f4ab2c90005e7e63cb631f0b4427f05422f241622ee3ec4727cc5febbf83e34 AS runc
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    libbtrfs-dev=6.2-1+deb12u2 \
    && rm -rf /var/lib/apt/lists/*

# runc v1.4.2
ARG RUNC_COMMIT=c241c0bb5e60a8e8c1b2e53d4eca8d0068d8d57e

RUN git clone https://github.com/opencontainers/runc.git /src/runc && \
    git -C /src/runc checkout ${RUNC_COMMIT}
WORKDIR /src/runc
RUN make BUILDTAGS="" EXTRA_FLAGS="-a" EXTRA_LDFLAGS="-w -s" static
RUN strip --strip-all runc

FROM golang:1.26-bookworm@sha256:4f4ab2c90005e7e63cb631f0b4427f05422f241622ee3ec4727cc5febbf83e34 AS containerd
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y gcc=4:12.2.0-3 libseccomp-dev=2.5.4-1+deb12u1 \
    && rm -rf /var/lib/apt/lists/*

# containerd v2.2.2
ARG CONTAINERD_COMMIT=301b2dac98f15c27117da5c8af12118a041a31d9

RUN git clone https://github.com/containerd/containerd.git /src/containerd && \
    git -C /src/containerd checkout ${CONTAINERD_COMMIT}

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

FROM rust:1.94.1-bookworm@sha256:2ab796040c03a34d0f090f0d4da18f6ac0503124167c6898ed70a434f108e4ef as chef
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

FROM alpine:3.21@sha256:22e0ec13c0db6b3e1ba3280e831fc50ba7bffe58e81f31670a64b1afede247bc
RUN apk add --no-cache zlib=1.3.1-r2
RUN apk add --no-cache iptables=1.8.11-r1

COPY --from=containerd /src/containerd/bin /bin
COPY --from=runc /src/runc/runc /bin/runc
COPY --from=download /tini /bin/tini
COPY --from=cni /cni /cni
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/sidecar /bin

EXPOSE 8000
ENTRYPOINT ["/bin/tini", "--", "/bin/sidecar"]
