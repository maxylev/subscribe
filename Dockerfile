# syntax=docker/dockerfile:1
#
# Stages:
#   build    compile from source (musl, static binary)
#   runtime  minimal Alpine image with a non-root user, no binary yet
#   prebuilt runtime + binary from ./dist/<arch>/subscribe (used by the release workflow,
#            so multi-arch images reuse natively built musl binaries instead of compiling under QEMU)
#   final    runtime + binary from the build stage (default target: plain `docker build .`)

ARG RUST_VERSION=1.99

FROM rust:${RUST_VERSION}-alpine3.24 AS build
# rust-toolchain.toml is deliberately NOT copied: the image tag already pins the compiler, and a
# toolchain file naming a different patch release (e.g. 1.99.1 vs the image's 1.99.x) would make
# rustup download another toolchain at build time. Bump RUST_VERSION to change the compiler.
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Cache mounts are not part of the image layer, so the binary is copied to /out inside the same RUN.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && install -D -m 0755 target/release/subscribe /out/subscribe

FROM alpine:3.24 AS runtime
LABEL org.opencontainers.image.title="subscribe" \
      org.opencontainers.image.description="Stateless on-chain subscription service: pay an ERC20 token to rent jobs that report to Telegram." \
      org.opencontainers.image.source="https://github.com/maxylev/subscribe" \
      org.opencontainers.image.url="https://github.com/maxylev/subscribe" \
      org.opencontainers.image.licenses="MIT"
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 subscribe \
    && adduser -S -D -H -u 10001 -G subscribe -h /app subscribe \
    && mkdir -p /app/data \
    && chown -R 10001:10001 /app
WORKDIR /app
USER 10001:10001
ENTRYPOINT ["subscribe"]
CMD ["serve", "-c", "/app/config.toml"]

FROM runtime AS prebuilt
ARG TARGETARCH
COPY --chmod=0755 dist/${TARGETARCH}/subscribe /usr/local/bin/subscribe

FROM runtime AS final
COPY --from=build --chmod=0755 /out/subscribe /usr/local/bin/subscribe
