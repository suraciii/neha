# syntax=docker/dockerfile:1.7
FROM rust:1.99.0-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY apps ./apps
RUN cargo build --locked --release -p neha-app --bin neha

FROM node:24-bookworm-slim AS ui-build
WORKDIR /src/ui
RUN corepack enable && corepack prepare pnpm@11.21.0 --activate
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui ./
RUN pnpm run build

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 neha \
    && useradd --uid 10001 --gid neha --no-create-home --shell /usr/sbin/nologin neha
COPY --from=rust-build /src/target/release/neha /usr/local/bin/neha
COPY --from=ui-build /src/ui/dist /app/ui
USER 10001:10001
ENV NEHA_HOME=/data \
    HOME=/data/home \
    NEHA_BIND=0.0.0.0 \
    NEHA_UI_DIR=/app/ui
EXPOSE 7337
ENTRYPOINT ["neha", "serve", "--port", "7337", "--ui", "/app/ui"]
