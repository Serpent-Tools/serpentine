FROM docker.io/library/alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 as download
RUN apk add tar curl

ARG TINI_VERSION=v0.19.0
RUN curl -fsSL "https://github.com/krallin/tini/releases/download/${TINI_VERSION}/tini-static" -o /tini && \
    echo "c5b0666b4cb676901f90dfcb37106783c5fe2077b04590973b885950611b30ee  /tini" | sha256sum -c - && \
    chmod +x /tini
RUN curl -fsSL "https://raw.githubusercontent.com/krallin/tini/${TINI_VERSION}/LICENSE" -o /tini.LICENSE

FROM docker.io/library/golang:1.27.1-bookworm@sha256:69a7b9788769bec032d238959b61854e9ae87f57be9029ec04e9885fabf99195 AS go_base

FROM go_base AS cni

# renovate: datasource=github-tags depName=containernetworking/plugins
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

FROM go_base AS runc
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y \
    libbtrfs-dev \
    && rm -rf /var/lib/apt/lists/*

# renovate: datasource=github-tags depName=opencontainers/runc
ARG RUNC_VERSION=v1.5.1
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

# renovate: datasource=github-tags depName=containerd/containerd
ARG CONTAINERD_VERSION=v2.4.0
ARG CONTAINERD_COMMIT=a7fe631d96c08fb14cf8eff0afdc280e99c30a94

RUN git clone https://github.com/containerd/containerd.git /src/containerd && \
    git -C /src/containerd checkout ${CONTAINERD_COMMIT}

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

FROM docker.io/library/rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e as rust_base
RUN cargo install cargo-chef@=0.1.78 --locked
# cargo-about puts its binary behind `cli`; without it the install is a no-op that still exits 0.
RUN cargo install cargo-about@=0.9.2 --locked --features cli
WORKDIR /app

FROM rust_base as planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM rust_base as builder
ENV RUSTFLAGS="-C target-feature=+crt-static"

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release -p sidecar --target x86_64-unknown-linux-gnu --recipe-path recipe.json

COPY about.toml about.hbs .
RUN cargo about generate -c about.toml -m sidecar/Cargo.toml \
    --target x86_64-unknown-linux-gnu about.hbs -o /THIRD-PARTY.md

COPY . .
RUN cargo build --release -p sidecar --target x86_64-unknown-linux-gnu

FROM docker.io/library/alpine:3.24.2@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6
RUN apk add --no-cache iptables
# Alpine ships no license files of its own, so record what is installed and where its source is.
RUN mkdir -p  /usr/share/licenses/ && \
    apk info -v > /usr/share/licenses/alpine-packages.txt && \
    echo "Source: https://gitlab.alpinelinux.org/alpine/aports" >> /usr/share/licenses/alpine-packages.txt

COPY --from=containerd /src/containerd/bin /bin
COPY --from=runc /src/runc/runc /bin/runc
COPY --from=download /tini /bin/tini
COPY --from=cni /cni /cni

COPY --from=containerd /src/containerd/LICENSE /src/containerd/NOTICE /usr/share/licenses/containerd/
COPY --from=runc /src/runc/LICENSE /src/runc/NOTICE /usr/share/licenses/runc/
COPY --from=cni /src/cni-plugins/LICENSE /usr/share/licenses/cni-plugins/
COPY --from=download /tini.LICENSE /usr/share/licenses/tini/LICENSE

COPY --from=builder /THIRD-PARTY.md /usr/share/licenses/rust-crates/
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/sidecar /bin

EXPOSE 8000
ENTRYPOINT ["/bin/tini", "--", "/bin/sidecar"]
