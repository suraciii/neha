# syntax=docker/dockerfile:1.7
FROM rust:1.99.0-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN cargo build --locked --release -p neha-app --bin neha

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates coreutils \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 neha \
    && useradd --uid 10001 --gid neha --no-create-home --shell /usr/sbin/nologin neha \
    && mkdir -p /workspace \
    && chown neha:neha /workspace
COPY --from=rust-build /src/target/release/neha /usr/local/bin/neha
USER 10001:10001
WORKDIR /workspace
ENTRYPOINT ["neha", "computer", "worker"]
