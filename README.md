<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# OSM Signals

[![Tests](https://github.com/brawer/osmsignals/actions/workflows/test.yml/badge.svg)](https://github.com/brawer/osmsignals/actions/workflows/test.yml)
[![REUSE status](https://api.reuse.software/badge/github.com/brawer/osmsignals)](https://api.reuse.software/info/github.com/brawer/osmsignals)

Ranking signals for [OpenStreetMap](https://www.openstreetmap.org/),
computed from the planet file and published as
[PMTiles](https://docs.protomaps.com/pmtiles/) raster pyramids.

Work in progress. The first signal is **node density**: how many OSM
nodes there are per km² of Earth surface, at a resolution of zoom-18
Web Mercator tiles (~150 m at the equator), similar in spirit to
[tyrasd/osm-node-density](https://github.com/tyrasd/osm-node-density).

## Usage

```sh
cargo run --release -- node-density --input planet.osm.pbf --output node-density.pmtiles
# or stream the planet without storing it:
curl -s https://osm-pds.s3.amazonaws.com/2026/planet-261012.osm.pbf \
  | cargo run --release -- node-density --input - --output node-density.pmtiles
```

## Output

Tiles are 256×256 grayscale lossless WebP, zoom 0 to 10, so a pixel in
a zoom-10 tile is one zoom-18 tile. A pixel value `v` of 0 means no
nodes; otherwise the density is `2^-16 · 2^((v-1)/6)` nodes per km²,
i.e. six steps per doubling. Overview tiles sum the counts of their
four children before dividing by area, so every zoom level is a true
average density. The scale is fixed, so values from different months
are comparable.

## How it works

1. Worker threads decode the PBF and map every node to the Hilbert index
   of its zoom-18 tile, the same curve PMTiles uses for tile IDs.
2. Keys are sorted in memory-bounded runs, counted, and spilled to disk.
3. A k-way merge streams `(pixel, count)` in Hilbert order. Because the
   Hilbert curve visits every sub-tree contiguously, only the chain of
   tiles from the root to the current zoom-10 tile is ever in memory;
   finished tiles are encoded and folded into their parent.
4. Tiles are spooled per zoom level and concatenated into a clustered
   PMTiles archive.

## Tests

`cargo test` runs unit tests and an end-to-end test on a small extract
around Zug. CI also runs a smoke test on Liechtenstein from Geofabrik;
locally, point `OSMSIGNALS_SMOKE_PBF` at any extract to run it.

## Contributing

PR titles follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat: …`, `fix: …`, `docs: …`), since PRs are squash-merged with
the title as commit message. Every file carries SPDX headers; check
with `reuse lint`.

## License

Code: [MIT](LICENSE). Output data is derived from OpenStreetMap,
© OpenStreetMap contributors, available under the
[Open Database License](https://www.openstreetmap.org/copyright).
`tests/data/zugerland.osm.pbf` is an OpenStreetMap extract (ODbL),
taken from [brawer/osmdiffs](https://github.com/brawer/osmdiffs).
