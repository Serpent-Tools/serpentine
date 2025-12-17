# tiny reproducible image for running containerd in a container
FROM debian:bookworm-slim
ARG CONTAINERD_VERSION=2.2.0
ARG RUNC_VERSION=v1.4.0

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl iproute2 gnupg \
    && rm -rf /var/lib/apt/lists/*

# containerd binaries
RUN curl -fsSL "https://github.com/containerd/containerd/releases/download/v${CONTAINERD_VERSION}/containerd-${CONTAINERD_VERSION}-linux-amd64.tar.gz" \
    | tar -xz -C /usr/local

# runc binary
RUN curl -fsSL -Lo /usr/local/sbin/runc \
    "https://github.com/opencontainers/runc/releases/download/${RUNC_VERSION}/runc.amd64" && \
    chmod +x /usr/local/sbin/runc

RUN mkdir -p /etc/cni/net.d /var/lib/containerd
