#!/bin/sh
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Writes a CycloneDX 1.7 SBOM for one single-architecture container image.
#
#   scripts/sbom.sh IMAGE_ARCHIVE IMAGE_NAME IMAGE_DIGEST OUTPUT
#
#   IMAGE_ARCHIVE  the image as OCI archive (podman save --format oci-archive)
#   IMAGE_NAME     its registry name, such as ghcr.io/brawer/osmsignals
#   IMAGE_DIGEST   its manifest digest in the registry, sha256:...
#   OUTPUT         where to write the SBOM
#
# The component list comes from the image itself: the binary is built with
# cargo-auditable, which embeds its exact dependency graph, and syft reads
# that back. Scanning the artifact rather than the source tree means the
# SBOM lists what was linked for this architecture, nothing more. Two
# things are not embedded, so we add them here:
#
#   * licenses, from `cargo metadata` on the same Cargo.lock;
#   * musl, the C library statically linked into the binary, whose version
#     we read from the build's base image (pinned by digest in Containerfile).
#
# Needs syft, jq and podman on PATH. Cargo runs inside the build's base
# image, which podman has in local storage right after building the image.

set -eu

if [ $# -ne 4 ]; then
  sed -n '6,12p' "$0" >&2
  exit 2
fi
ARCHIVE="$1"
IMAGE_NAME="$2"
IMAGE_DIGEST="$3"
OUTPUT="$4"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SYFT_CHECK_FOR_APP_UPDATE=false syft scan --quiet \
  --source-name "$IMAGE_NAME" --source-version "$IMAGE_DIGEST" \
  --output "cyclonedx-json@1.7=$WORK/syft.json" \
  "oci-archive:$ARCHIVE"

# Run cargo in the same base image the binary was built in, so we need
# no Rust toolchain on the host and use the same cargo.
BASE_IMAGE="$(sed -n 's/^FROM \([^ ]*\) AS build$/\1/p' "$ROOT/Containerfile")"
podman run --rm --volume "$ROOT:/src:ro" --workdir /src "$BASE_IMAGE" \
  cargo metadata --quiet --locked --format-version 1 > "$WORK/cargo.json"

ALPINE="$(podman run --rm --network=none "$BASE_IMAGE" cat /etc/alpine-release)"
# Read apk's database directly; `apk info` wants network access.
MUSL="$(podman run --rm --network=none "$BASE_IMAGE" awk \
  '/^P:/ { p = $0 } /^V:/ && p == "P:musl" { print substr($0, 3) }' \
  /lib/apk/db/installed)"
ARCH="$(podman run --rm --network=none "$BASE_IMAGE" apk --print-arch)"
if [ -z "$ALPINE" ] || [ -z "$MUSL" ] || [ -z "$ARCH" ]; then
  echo "ERROR: cannot determine musl version from $BASE_IMAGE" >&2
  exit 1
fi

jq \
  --slurpfile cargo "$WORK/cargo.json" \
  --arg image "$IMAGE_NAME" --arg digest "$IMAGE_DIGEST" \
  --arg alpine "$ALPINE" --arg musl "$MUSL" --arg arch "$ARCH" \
  '
  # "MIT/Apache-2.0" is old Cargo syntax for "MIT OR Apache-2.0".
  ($cargo[0].packages
    | map(select(.license != null)
          | {key: "\(.name)@\(.version)", value: (.license | gsub("/"; " OR "))})
    | from_entries) as $licenses
  | ("pkg:apk/alpine/musl@\($musl)?arch=\($arch)&distro=alpine-\($alpine)") as $musl_purl
  | .metadata.component.purl =
      "pkg:oci/\($image | split("/") | last)@\($digest | sub(":"; "%3A"))?repository_url=\($image)"
  | .components |= map(
      if (.purl // "" | startswith("pkg:cargo/")) and $licenses["\(.name)@\(.version)"]
      then .licenses = [{expression: $licenses["\(.name)@\(.version)"]}]
      else . end)
  | .components += [{
      "bom-ref": $musl_purl,
      type: "library",
      name: "musl",
      version: $musl,
      description: "C standard library, statically linked into /osmsignals",
      licenses: [{expression: "MIT"}],
      purl: $musl_purl
    }]
  | .dependencies |= map(
      if (.ref | startswith("pkg:cargo/osmsignals@"))
      then .dependsOn += [$musl_purl] else . end)
  ' "$WORK/syft.json" > "$WORK/sbom.json"

# Fail rather than publish an SBOM with holes in it.
jq -e '
  .specVersion == "1.7"
  and ([.components[] | select(.purl // "" | startswith("pkg:cargo/osmsignals@"))] | length == 1)
  and ([.components[] | select(.purl // "" | startswith("pkg:cargo/")) | select(.licenses == null)]
       | length == 0)
  and ([.dependencies[] | select(.dependsOn | index($m))] | length == 1)
  ' --arg m "$(jq -r '.components[] | select(.name == "musl") | .purl' "$WORK/sbom.json")" \
  "$WORK/sbom.json" > /dev/null || {
  echo "ERROR: incomplete SBOM; components without licenses:" >&2
  jq -r '.components[] | select(.purl // "" | startswith("pkg:cargo/"))
         | select(.licenses == null) | .purl' "$WORK/sbom.json" >&2
  exit 1
}

mkdir -p "$(dirname "$OUTPUT")"
mv "$WORK/sbom.json" "$OUTPUT"
echo "wrote $OUTPUT: $(jq '.components | length' "$OUTPUT") components" >&2
