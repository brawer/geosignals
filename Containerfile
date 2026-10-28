# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

# Builds a minimal OCI image that holds nothing but a statically linked
# geosignals binary.
#
#   podman build -t geosignals .
#   podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" \
#       -v "$PWD:/data" geosignals \
#       osm-node-density --output-dir .
#
# Releases are built and attested by .github/workflows/release-build.yml.
# The build is meant to be reproducible, so that anyone can rebuild a
# release and compare it with the attested image: the base image is
# pinned by digest (Dependabot keeps it current), Cargo.lock is enforced
# with --locked, nothing is downloaded from a package manager other than
# crates.io, and build paths are stripped from the binary.

# Alpine's Rust is native musl, which links statically by default; the
# image already ships gcc and musl-dev for the C parts of zstd and ring.
# Building natively for the target platform keeps multi-arch builds
# (podman build --platform linux/amd64,linux/arm64) free of cross-compilers.
FROM docker.io/library/rust:1.99-alpine@sha256:557a94c82b485a14b569120e00867267108c734efe33f10cc31ca0cd5dd1eca9 AS build

# cargo-auditable embeds the exact dependency list into the binary, so
# SBOM generators (syft) and vulnerability scanners (Trivy, Grype) can
# read what is inside the image without access to the source tree.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked cargo-auditable@0.7.7

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
    cargo auditable build --release --locked --bin geosignals \
 && cp target/release/geosignals /geosignals

# Fail the build unless the binary is static (static-pie is fine:
# it has a dynamic section, but no interpreter and no needed libraries).
RUN ! readelf --program-headers /geosignals | grep -q INTERP \
 && ! readelf --dynamic /geosignals | grep -q NEEDED \
 && /geosignals --version

# scratch has no filesystem at all; give it a /tmp for the sort spill
# files (the default --workdir) and a /data directory to mount.
RUN mkdir -p /rootfs/tmp /rootfs/data \
 && chmod 1777 /rootfs/tmp \
 && chmod 0777 /rootfs/data \
 && cp /geosignals /rootfs/geosignals

FROM scratch

LABEL org.opencontainers.image.title="geosignals" \
      org.opencontainers.image.description="Ranking signals for OpenStreetMap" \
      org.opencontainers.image.source="https://github.com/brawer/geosignals" \
      org.opencontainers.image.licenses="MIT"

COPY --from=build /rootfs/ /

# Run unprivileged (nobody:nogroup).
USER 65534:65534
WORKDIR /data
ENTRYPOINT ["/geosignals"]
CMD ["--help"]
