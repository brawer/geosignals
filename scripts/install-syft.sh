#!/bin/sh
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Installs a pinned, checksum-verified syft into DIR (default: ./bin).
# Dependabot can't see this pin; bump VERSION and both checksums together,
# taking them from syft_${VERSION}_checksums.txt on the release page.

set -eu

VERSION=1.52.0
DIR="${1:-bin}"

case "$(uname -m)" in
  x86_64)        ARCH=amd64
                 SHA256=caeedb81fb0491615f1ebd1761e4145d41ee86dd2cc7bf80669f9f5ad9d6133d ;;
  aarch64|arm64) ARCH=arm64
                 SHA256=c46d5e4c28e12aa4c5becfaa343ef1c7f89045b6b895f2c21d471c62db09c706 ;;
  *) echo "ERROR: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

TARBALL="syft_${VERSION}_linux_${ARCH}.tar.gz"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
curl --fail --silent --show-error --location --retry 3 --output "$WORK/$TARBALL" \
  "https://github.com/anchore/syft/releases/download/v${VERSION}/${TARBALL}"
echo "${SHA256}  $WORK/$TARBALL" | sha256sum --check --quiet
mkdir -p "$DIR"
tar -xzf "$WORK/$TARBALL" -C "$DIR" syft
"$DIR/syft" version | sed -n 's/^Version: *//p'
