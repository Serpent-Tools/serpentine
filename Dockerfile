FROM debian:bookworm-slim as download
ARG CONTAINERD_VERSION=2.2.1
ARG RUNC_VERSION=v1.4.0

ENV DEBIAN_FRONTEND=noninteractive

ADD https://github.com/containerd/containerd/releases/download/v${CONTAINERD_VERSION}/containerd-${CONTAINERD_VERSION}-linux-amd64.tar.gz /containerd.tar.gz
RUN mkdir /containerd && cat /containerd.tar.gz | tar -xz -C /containerd

ADD https://github.com/opencontainers/runc/releases/download/${RUNC_VERSION}/runc.amd64 /runc
RUN chmod +x /runc

ADD https://github.com/krallin/tini/releases/latest/download/tini-static /tini
RUN chmod +x /tini

FROM lukemathwalker/cargo-chef:latest-rust-1.92 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ENV RUSTFLAGS="-C target-feature=+crt-static"
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release -p sidecar --recipe-path recipe.json
COPY . .
RUN cargo build --release -p sidecar --frozen

FROM gcr.io/distroless/base-debian13
COPY --from=download /containerd /usr/local
COPY --from=download /runc /usr/local/sbin/runc
COPY --from=download /tini /usr/local/bin/tini
COPY --from=builder /app/target/release/sidecar /usr/local/bin

EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/tini", "--", "/usr/local/bin/sidecar"]
