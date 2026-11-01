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

- a [STAC](https://stacspec.org/) item for the release, such as
  `osm-node-density.2026-10-12.stac.json`, and `collection.json`, the
  STAC collection of the signal; see [STAC catalog](#stac-catalog).

Give each signal its own output directory, so their `datapackage.json`
and `collection.json` files don't overwrite each other.

The same inputs and geosignals release give the same files, except for
the optional `--run-id` (such as a Kubernetes job name), which is
recorded in the data BOM as the workflow `uid`.

## STAC catalog

Each output directory is also a static, self-contained
[STAC 1.1](https://github.com/radiantearth/stac-spec/tree/v1.1.0)
catalog, so tools such as QGIS, [pystac](https://pystac.readthedocs.io/)
and [STAC Browser](https://radiantearth.github.io/stac-browser/) can
find and load the GeoTIFFs without reading these docs:

- `collection.json` is the STAC collection of the signal, such as
  `osm-node-density`. It has the signal's title, description, license,
  and spatial and temporal extent, links to one item per release, and
  lists `datapackage.json` as an asset. Like `datapackage.json`, it
  keeps its name and is rewritten by each run.
- Each release gets a dated STAC item, such as
  `osm-node-density.2026-10-12.stac.json`. Its assets are the
  release's GeoTIFFs (role `data`) and data BOMs (role `metadata`),
  with size and SHA-256 ([file
  extension](https://github.com/stac-extensions/file)), and for the
  GeoTIFFs the EPSG code, shape and transform of the full-resolution
  image ([projection
  extension](https://github.com/stac-extensions/projection)) and the
  band's data type and nodata value. Like the files it lists, the item
  never changes.

The collection links to every item it finds in the directory, so
releases you keep stay in the catalog. To drop a release, delete its
files including the `.stac.json`; the next run leaves it out. All links
are relative, so the directory can be copied or served from anywhere.
For example, with pystac:

```python
import pystac
collection = pystac.read_file("https://example.org/osm-node-density/collection.json")
item = max(collection.get_items(), key=lambda i: i.datetime)
print(item.assets["osm-node-density"].get_absolute_href())
```

The STAC files hold what `datapackage.json` holds, in another
vocabulary. STAC has no place for license evidence and per-source
attribution, so the data BOM stays the authority on sources and
licenses. The smoke test checks the STAC files with pystac against the
STAC schemas.

## Publishing to Source Cooperative

geosignals can upload each release to public S3-compatible storage,
such as [Source Cooperative](https://source.coop), when the run is
done. The step is optional and is configured with environment
variables, like the transit cache's `CACHE_S3_*`:

| Variable | |
|---|---|
| `PUBLIC_S3_URL` | Where to upload, as `s3://bucket/prefix`. On Source Cooperative, the bucket is the account and the prefix starts with the repository, such as `s3://brawer/geosignals`. |
| `PUBLIC_S3_ACCESS_KEY_ID` | Access key. |
| `PUBLIC_S3_SECRET_ACCESS_KEY` | Secret key. |
| `PUBLIC_S3_ENDPOINT_URL` | The S3 endpoint, such as `https://data.source.coop`. Defaults to Amazon S3 in `PUBLIC_S3_REGION`. |
| `PUBLIC_S3_REGION` | Defaults to `auto`. |
| `PUBLIC_S3_SESSION_TOKEN` | Only for temporary credentials that come with a session token. |

Take the values from the credentials that your storage provider shows
for the repository. The standard `AWS_*` variables are not read, so
the cache and the public storage can live with different providers.

- When no `PUBLIC_S3_*` variable is set, nothing is uploaded, and the
  run says so.
- When some are set but `PUBLIC_S3_URL` or a key is missing, geosignals
  stops before computing anything, rather than failing after hours.
- Each signal goes into a folder named after it, such as
  `s3://brawer/geosignals/osm-node-density/`, holding the same files
  as its output directory. The dated files (GeoTIFFs, data BOMs, STAC
  item) are uploaded first, and `datapackage.json` and `collection.json`
  last, so readers never find a link to a file that isn't there yet.
  Each object gets its media type, such as
  `image/tiff; application=geotiff; profile=cloud-optimized`.
- Partial or restricted outputs are never uploaded: runs with
  `--smoke-test`, `--files`, `--only` or `--licenses any`.
- If an upload fails after retries, the run exits with an error. The
  files stay in `--output-dir`, and a rerun uploads them again.
- Files over 50 MiB go up as multipart uploads in 50 MiB parts, so a
  network error repeats one part rather than the whole file. If the
  upload fails anyway, geosignals aborts it, so the bucket doesn't keep
  the parts. The transit cache uploads large files the same way.

`collection.json` lists the releases whose STAC items are in the local
output directory. To keep older releases in the public catalog, keep
the output directory between runs.

## Scratch space and memory

`--workdir` (default: the system's temporary directory) holds sorted
runs and other scratch files. Each signal uses its own subdirectory and
cleans up leftovers of interrupted runs. Put it on disk, not on a RAM
disk such as a memory-backed Kubernetes `emptyDir`.

`--threads` defaults to the number of CPUs. On Linux, geosignals caps
glibc at two malloc arenas, which saves about a third of memory on
heavily threaded runs; set `MALLOC_ARENA_MAX` to override. The static
musl build in the container image needs no such setting.
