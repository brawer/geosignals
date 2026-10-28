<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Running geosignals

This page is for people who compute the signals themselves. Each
signal's own page ends with a section on reproducing its files, with
the options and resource needs specific to it. `geosignals --help` and
`geosignals <command> --help` list every option.

## Building

```sh
cargo build --release
```

The binary is `target/release/geosignals`. In the examples,
`cargo run --release --` does the same in one step.

## Container

The [`Containerfile`](../Containerfile) builds a ~3 MB image that holds
just a statically linked binary. With rootless podman, map your own
user into the container so the output belongs to you:

```sh
podman build -t geosignals .
podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" -v "$PWD:/data" \
  geosignals osm-node-density --output-dir .
```

Each release is also published as `ghcr.io/brawer/geosignals` for
amd64 and arm64, with SLSA Build Level 3 provenance and a CycloneDX 1.7
SBOM per architecture attached as attestations. The single-architecture
images are tagged too, such as `v0.1.0-amd64`. To check that an image
was built by this repository's release workflow, and to read its SBOM:

```sh
gh attestation verify oci://ghcr.io/brawer/geosignals:v0.1.0 --owner brawer \
  --signer-workflow brawer/geosignals/.github/workflows/release-build.yml
gh attestation verify oci://ghcr.io/brawer/geosignals:v0.1.0-amd64 --owner brawer \
  --predicate-type https://cyclonedx.org/bom --format json \
  | jq '.[0].verificationResult.statement.predicate'
```

The binary is built with [cargo-auditable](https://github.com/rust-secure-code/cargo-auditable),
so scanners such as Trivy or Grype can also read its dependencies
straight from the image.

## What every run writes

Each signal writes into `--output-dir`:

- one or more Cloud-Optimized GeoTIFFs, named after the date of their
  input data, such as `osm-node-density.2026-10-12.tiff`;
- next to each, a data bill of materials such as
  `osm-node-density.2026-10-12.cdx.json`, in
  [CycloneDX 1.7](https://cyclonedx.org/docs/1.7/json/) format. It
  records the geosignals version, the inputs it read, and the SHA-256
  and SHA-512 of the GeoTIFF;
- `datapackage.json`, a [Frictionless Data Package
  v2](https://datapackage.org/standard/data-package/) descriptor that
  points to the dated files by relative path, with their sizes,
  SHA-256 hashes and licenses. Unlike them, it keeps its name and is
  replaced by each run, so clients can fetch it to find the current
  release.

Give each signal its own output directory, so their `datapackage.json`
files don't overwrite each other.

The same inputs and geosignals release give the same files, except for
the optional `--run-id` (such as a Kubernetes job name), which is
recorded in the data BOM as the workflow `uid`.

## Scratch space and memory

`--workdir` (default: the system's temporary directory) holds sorted
runs and other scratch files. Each signal uses its own subdirectory and
cleans up leftovers of interrupted runs. Put it on disk, not on a RAM
disk such as a memory-backed Kubernetes `emptyDir`.

`--threads` defaults to the number of CPUs. On Linux, geosignals caps
glibc at two malloc arenas, which saves about a third of memory on
heavily threaded runs; set `MALLOC_ARENA_MAX` to override. The static
musl build in the container image needs no such setting.
