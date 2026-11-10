# syntax=docker/dockerfile:1
#
# Multi-platform image (linux/amd64, linux/arm64, linux/arm/v7). cargo-zigbuild
# cross-compiles a static musl binary on the build machine's own platform (no
# emulation), then it is copied into a distroless image that runs as non-root.
#
#   docker buildx build --platform linux/arm64 --load -t rust-albums .
#
# Base images are pinned by digest, written literally (no ARG), so Dependabot
# can open a pull request whenever one is rebuilt.

FROM --platform=$BUILDPLATFORM rust:1.98-slim@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS build
# Zig provides the C cross-compiler that ring needs for each target.
ARG ZIG_VERSION=0.17.1
ARG CARGO_ZIGBUILD_VERSION=0.24.1
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && pip3 install --no-cache-dir --break-system-packages \
      "ziglang==${ZIG_VERSION}" "cargo-zigbuild==${CARGO_ZIGBUILD_VERSION}"

# Download crates once, before the per-platform steps: every platform then
# shares this layer read-only instead of unpacking into one cache in parallel.
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo fetch --locked \
 && rm -r src

ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      linux/amd64)  echo x86_64-unknown-linux-musl ;; \
      linux/arm64)  echo aarch64-unknown-linux-musl ;; \
      linux/arm/v7) echo armv7-unknown-linux-musleabihf ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac > /rust-target \
 && rustup target add "$(cat /rust-target)"

# The empty state folder, copied into the image with the non-root owner.
RUN mkdir /state

COPY albums.default.toml ./
COPY src ./src
RUN --mount=type=cache,target=/src/target,id=rust-albums-target-${TARGETPLATFORM} \
    cargo zigbuild --release --frozen --target "$(cat /rust-target)" \
 && cp "target/$(cat /rust-target)/release/rust-albums" /rust-albums

FROM gcr.io/distroless/static-debian13:nonroot@sha256:25ac8f0c3b2334a9eb05452266bd4697864a8c8ef0ef71e5e6480d7f92868329
COPY --from=build /rust-albums /usr/local/bin/rust-albums
# Writable state (config, certificates, link key) lives in /state; mount a volume
# there. Owned by the image's non-root user so a new named volume inherits it.
COPY --from=build --chown=65532:65532 /state /state
ENV ALBUMS_CONFIG=/state/albums.toml \
    ALBUMS_BIND_ADDR=0.0.0.0:8443 \
    ALBUMS_STORAGE_ROOT=/photos
USER 65532:65532
EXPOSE 8443
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD ["/usr/local/bin/rust-albums", "--healthcheck"]
# No init process (tini): the binary handles SIGTERM itself and starts no children.
ENTRYPOINT ["/usr/local/bin/rust-albums"]
