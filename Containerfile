# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

# Builds a minimal OCI image that holds nothing but a statically linked
# osmsignals binary.
#
#   podman build -t osmsignals .
#   podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" \
#       -v "$PWD:/data" osmsignals \
#       node-density --input planet.osm.pbf --output node-density.tif
#
# The build is meant to be reproducible, so that a later SLSA release
# workflow can rebuild and attest it: the base image is pinned by digest
# (Dependabot keeps it current), Cargo.lock is enforced with --locked,
# nothing is downloaded from a package manager other than crates.io,
# and build paths are stripped from the binary.

# Alpine's Rust is native musl, which links statically by default; the
# image already ships gcc and musl-dev for the C parts of zstd. Building
# natively for the target platform keeps multi-arch builds
# (podman build --platform linux/amd64,linux/arm64) free of cross-compilers.
FROM docker.io/library/rust:1.97-alpine@sha256:3c38f3f82c2f3d73da3b38e18d279393a04cb43ddded0e35088a8c3324d40900 AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Cache mounts only speed up rebuilds; they don't affect the output.
# The binary is copied out of the cached target directory so the next
# stage can see it.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    RUSTFLAGS="--remap-path-prefix=/src=. --remap-path-prefix=/usr/local/cargo=cargo"
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin osmsignals \
 && cp target/release/osmsignals /osmsignals

# Fail the build unless the binary is static (static-pie is fine:
# it has a dynamic section, but no interpreter and no needed libraries).
RUN ! readelf --program-headers /osmsignals | grep -q INTERP \
 && ! readelf --dynamic /osmsignals | grep -q NEEDED \
 && /osmsignals --version

# scratch has no filesystem at all; give it a /tmp for the sort spill
# files (the default --tmpdir) and a /data working directory to mount.
RUN mkdir -p /rootfs/tmp /rootfs/data \
 && chmod 1777 /rootfs/tmp \
 && chmod 0777 /rootfs/data \
 && cp /osmsignals /rootfs/osmsignals

FROM scratch

LABEL org.opencontainers.image.title="osmsignals" \
      org.opencontainers.image.description="Ranking signals for OpenStreetMap" \
      org.opencontainers.image.source="https://github.com/brawer/osmsignals" \
      org.opencontainers.image.licenses="MIT"

COPY --from=build /rootfs/ /

# Run unprivileged (nobody:nogroup).
USER 65534:65534
WORKDIR /data
ENTRYPOINT ["/osmsignals"]
CMD ["--help"]
