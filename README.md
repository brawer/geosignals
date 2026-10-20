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

## Usage

```sh
cargo run --release -- node-density --input planet.osm.pbf --output node-density.tif
# or stream the planet without storing it:
curl -s https://osm-pds.s3.amazonaws.com/2026/planet-261012.osm.pbf \
  | cargo run --release -- node-density --input - --output node-density.tif
```

### Container

The [`Containerfile`](Containerfile) builds a ~3 MB image that holds
just a statically linked binary. With rootless podman, map your own
user into the container so the output belongs to you:

```sh
podman build -t osmsignals .
podman run --rm --userns=keep-id --user "$(id -u):$(id -g)" -v "$PWD:/data" \
  osmsignals node-density --input planet.osm.pbf --output node-density.tif
```

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

## Tests

`cargo test` runs unit tests and an end-to-end test on a small extract
around Zug. A smoke test on Liechtenstein from Geofabrik runs for
release tags and on demand (see `.github/workflows/smoke-test.yml`);
locally, point `OSMSIGNALS_SMOKE_PBF` at any extract to run it.

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
`tests/data/zugerland.osm.pbf` is an OpenStreetMap extract (ODbL),
taken from [brawer/osmdiffs](https://github.com/brawer/osmdiffs).
