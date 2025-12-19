FROM debian:bookworm-slim as install
ARG CONTAINERD_VERSION=2.2.1
ARG RUNC_VERSION=v1.4.0

ENV DEBIAN_FRONTEND=noninteractive

# We do everything in one layer to reduce image sizes
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl iproute2
RUN mkdir /containerd && curl -fsSL "https://github.com/containerd/containerd/releases/download/v${CONTAINERD_VERSION}/containerd-${CONTAINERD_VERSION}-linux-amd64.tar.gz" | tar -xz -C /containerd
RUN curl -fsSL -Lo /runc "https://github.com/opencontainers/runc/releases/download/${RUNC_VERSION}/runc.amd64" && \
    chmod +x /runc

FROM gcr.io/distroless/base-debian13
COPY --from=install /containerd /usr/local
COPY --from=install /runc /usr/local/sbin/runc
