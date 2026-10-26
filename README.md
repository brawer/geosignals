<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# OSM Signals

[![Tests](https://github.com/brawer/osmsignals/actions/workflows/test.yml/badge.svg)](https://github.com/brawer/osmsignals/actions/workflows/test.yml)
[![REUSE status](https://api.reuse.software/badge/github.com/brawer/osmsignals)](https://api.reuse.software/info/github.com/brawer/osmsignals)

Ranking signals for [OpenStreetMap](https://www.openstreetmap.org/),
computed from the planet file and published as
[Cloud-Optimized GeoTIFFs](https://cogeo.org/).

Work in progress. The first signal is **node density**: how many OSM
nodes there are per km² of Earth surface, at a resolution of zoom-18
Web Mercator tiles (~150 m at the equator), similar in spirit to
[tyrasd/osm-node-density](https://github.com/tyrasd/osm-node-density).
The second is [Nominatim's secondary importance](#nominatim-secondary-importance),
republished in the same format.

## Usage

```sh
cargo run --release -- node-density --output-dir .
```

By default, this streams the most recent weekly planet dump from
`planet.openstreetmap.org` over HTTPS, without storing it. Since the
planet is sorted, reading stops once all nodes are seen, so only part
of the file gets downloaded. If the connection drops, reading resumes
with a range request. `--input` also takes a local file, another URL,
or `-` for standard input. `--smoke-test` reads only the first few
blocks, to check that everything works without processing the planet.

The output is named after the replication timestamp in the PBF header,
such as `osm-node-density.2026-10-12.tiff` (the date is in UTC). Input
without that timestamp is rejected.

Next to it, `osm-node-density.2026-10-12.cdx.json` is a data bill of
materials in [CycloneDX 1.7](https://cyclonedx.org/docs/1.7/json/)
format. It records the osmsignals version, the planet it read (by name,
replication timestamp and download URL, but without a hash since the
planet is only partly read), and the SHA-256 and SHA-512 of the GeoTIFF.
The same planet and release give the same file, except for the
optional `--run-id` (such as a Kubernetes job name), which is recorded
as the workflow `uid`.

Finally, `datapackage.json` is a [Frictionless Data Package
v2](https://datapackage.org/standard/data-package/) descriptor that
points to both dated files by relative path, with their sizes and
SHA-256 hashes. Unlike them, it keeps its name and is replaced by
each run, so clients can fetch it to find the current release.

### Container

The [`Containerfile`](Containerfile) builds a ~3 MB image that holds
just a statically linked binary. With rootless podman, map your own
user into the container so the output belongs to you:

```sh
podman build -t osmsignals .
podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" -v "$PWD:/data" \
  osmsignals node-density --output-dir .
```

Each release is also published as `ghcr.io/brawer/osmsignals` for
amd64 and arm64, with SLSA Build Level 3 provenance and a CycloneDX 1.7
SBOM per architecture attached as attestations. The single-architecture
images are tagged too, such as `v0.1.0-amd64`. To check that an image
was built by this repository's release workflow, and to read its SBOM:

```sh
gh attestation verify oci://ghcr.io/brawer/osmsignals:v0.1.0 --owner brawer \
  --signer-workflow brawer/osmsignals/.github/workflows/release-build.yml
gh attestation verify oci://ghcr.io/brawer/osmsignals:v0.1.0-amd64 --owner brawer \
  --predicate-type https://cyclonedx.org/bom --format json \
  | jq '.[0].verificationResult.statement.predicate'
```

The binary is built with [cargo-auditable](https://github.com/rust-secure-code/cargo-auditable),
so scanners such as Trivy or Grype can also read its dependencies
straight from the image.

## Output

A Cloud-Optimized GeoTIFF (BigTIFF) in Web Mercator (EPSG:3857) with
one 16-bit band, tiled 256×256 and compressed with Zstandard. The
full-resolution image is 262,144 pixels wide, so one pixel covers one
zoom-18 tile; ten overviews halve the resolution down to a single
256×256 tile. Tiles without any nodes are not stored, and GDAL reads
them as zero. The planet comes to about 3 GB.

Pixels hold node density on a log scale with 100 steps per doubling:

```
v = 0      no nodes
v > 0      log2(nodes per km²) = 0.01 · v − 15.01
```

Each step is a 0.7% change in density, and the densest place on the
planet needs about 3440, so values fit in 12 bits. The file declares
this scale and offset in its GDAL metadata, so QGIS and GDAL show
log2(nodes/km²) directly. Density is measured against true area on the
sphere, not Web Mercator area, so pixels near the poles are not
inflated. Overview pixels sum the counts of their four children before
dividing by area, so every level is a true average density. The scale
is fixed, so values from different months are comparable.

The file also embeds an exact histogram of the full-resolution pixel
values, one bucket per value, as a GDAL Raster Attribute Table. GDAL
3.12+ and QGIS read it without scanning the image, and it lets you turn
any pixel value into a percentile. Bucket 0 counts pixels without
nodes; no NoData value is set, since zero nodes is a measurement.

## How it works

1. Worker threads decode the PBF and map every node to the Hilbert index
   of its zoom-18 tile.
2. Keys are sorted in memory-bounded runs, counted, and spilled to disk.
3. A k-way merge streams `(pixel, count)` in Hilbert order. Because the
   Hilbert curve visits every sub-tree contiguously, only the chain of
   tiles from the root to the current zoom-10 tile is ever in memory;
   finished tiles are encoded and folded into their parent.
4. Tiles are spooled per zoom level, then written into the GeoTIFF in
   the order GDAL expects of a COG: smallest overview first, each level
   in row-major order.

## Nominatim secondary importance

```sh
cargo run --release -- nominatim-secondary-importance --output-dir nominatim
```

This downloads the secondary importance raster that
[Nominatim](https://nominatim.org/) uses to rank search results,
[`wikimedia-secondary-importance.sql.gz`](https://nominatim.org/data/)
(8 MB), and republishes it as a Cloud-Optimized GeoTIFF, without
changing any value. It takes about 20 seconds. `--input` also takes a
local file. The output is named after the file's `Last-Modified` date
(for a local file, its modification time), such as
`nominatim-secondary-importance.2024-05-04.tiff`, with a data BOM and
a `datapackage.json` as for node density. The BOM also records the
SHA-256 and SHA-512 of the input, since it is read whole. Give each
signal its own output directory, so their `datapackage.json` files do
not overwrite each other.

### Output

Nominatim's grid is kept as it is: plain longitude and latitude
(EPSG:4326) from −180° to 180° and 90° to −90°, 16384 cells wide and
high, so a cell is 360/16384° wide and 180/16384° tall (about 2.4 ×
1.2 km at the equator). The full-resolution image has one pixel per
cell; six overviews halve the resolution down to a single 256×256
tile, keeping the largest of each 2×2 block. The pixel holding a
location is

```
x = floor((lon + 180) / 360 · 16384)
y = floor((90 − lat) / 180 · 16384)
```

Pixels are unsigned 16-bit integers, exactly as in Nominatim's
database:

```
v = 0      no area (also declared as the NoData value)
v > 0      importance = v / 65536
```

The file declares this scale in its GDAL metadata, so QGIS and GDAL
show importance between 0 and 1. As for node density, it embeds an
exact histogram of the full-resolution pixels as a Raster Attribute
Table, here with one row per value that occurs.

Each pixel holds the highest importance of the OSM areas covering it
whose rank is below county level, such as municipalities, towns and
suburbs. Countries, states and counties are not painted, so remote
areas without a municipality are 0, such as the Nevada desert,
inland Libya or much of Greenland. An area's importance comes from its Wikipedia article (via
the `wikipedia` or `wikidata` tag) as computed by
[osm-search/wikipedia-wikidata](https://github.com/osm-search/wikipedia-wikidata):
the logarithm of how many Wikipedia articles link to it, across 39
languages, scaled so that the most-linked article gets 1. Areas
without an article get a default for their rank. The raster is made
by [osm-search/secondary-importance](https://github.com/osm-search/secondary-importance).

The published file dates from May 2024. Its defaults follow an older
Nominatim formula, 0.75001 − rank/40 (so the most common value,
26215, is 0.40001), rather than today's 0.40001 − rank/75.

### How Nominatim uses it

The raster is not a full importance score, since Nominatim scores a
place by its tags and type, and only falls back to its location. For
a place with centroid *c* and search rank *r*, Nominatim's
[`compute_importance`](https://github.com/osm-search/Nominatim/blob/master/lib-sql/functions/importance.sql)
does this:

1. If the place's `wikipedia` tag names a known article, its
   importance is that article's importance.
2. Otherwise, if its `wikidata` tag names a known item, its importance
   is that item's importance.
3. Otherwise its importance is 0.40001 − r / 75, plus v / 655350000
   where v is the raster value at *c*, so at most 0.0001. This only
   breaks ties between places of the same rank, such as streets and
   POIs without a Wikipedia article.

Clients that know a place's tags and rank can follow the same steps,
reading v from this GeoTIFF.

## Tests

`cargo test` runs unit tests and end-to-end tests on a small extract
around Zug and on a tiny dump in Nominatim's format. A smoke test on Liechtenstein from Geofabrik runs for
release tags and on demand (see `.github/workflows/smoke-test.yml`);
locally, point `OSMSIGNALS_SMOKE_PBF` at any extract to run it. It also
republishes the real Nominatim raster and checks the result with GDAL.

## Contributing

PR titles follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat: …`, `fix: …`, `docs: …`), since PRs are squash-merged with
the title as commit message. Every file carries SPDX headers; check
with `reuse lint`.

Releases are cut by [release-please](https://github.com/googleapis/release-please):
merging its release PR bumps the version, updates `CHANGELOG.md`, tags
the commit and publishes a GitHub Release.

## License

Code: [MIT](LICENSE). Output data is derived from OpenStreetMap,
© OpenStreetMap contributors, available under the
[Open Database License](https://www.openstreetmap.org/copyright).
The secondary importance raster is made by Nominatim from OpenStreetMap
areas and Wikipedia link counts.
`tests/data/zugerland.osm.pbf` is an OpenStreetMap extract (ODbL),
taken from [brawer/osmdiffs](https://github.com/brawer/osmdiffs).
