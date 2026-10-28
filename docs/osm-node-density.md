<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# OpenStreetMap node density

This data package tells how many [OpenStreetMap](https://www.openstreetmap.org)
nodes there are at each place on Earth. A node is a point in OSM: a
shop or a bus stop, but much more often a corner of a building or a
vertex along a road, river or coastline. Node density is a signal of
how much has been built and mapped at a place. It doesn't count people
or visits, and it measures the work of mappers as much as the world
they map.

A second raster counts only the nodes that describe something, such as
a shop, a tree or a bench, together with points of interest drawn as
ways. It is a sparser signal of how many features are mapped at a
place; see [the tagged layer](#the-tagged-layer).

This page is for data analysts who consider using the file. It covers
what the files contain, how it is computed, its artifacts and biases,
and its license. The numbers come from the run on the planet of
19 October 2026.

## The files

Each release has two Cloud-Optimized GeoTIFFs, a data bill of
materials (`.cdx.json`) for each, and one `datapackage.json`:

| File | What it holds |
| --- | --- |
| `osm-node-density.2026-10-19.tiff` | The raster, 2.8 GB |
| `osm-node-density.2026-10-19.cdx.json` | The geosignals release that made the raster, the planet it read, and the raster's SHA-256 and SHA-512 |
| `osm-tagged-node-density.2026-10-19.tiff` | The tagged layer, 332 MB |
| `osm-tagged-node-density.2026-10-19.cdx.json` | The same for the tagged layer |
| `datapackage.json` | All of the above as a Frictionless data package, with the license |

The date in each name is the date of the planet's data, taken from the
replication timestamp in its header. A dated file never changes; the
next release gets new files. The same planet and geosignals release
always give the same rasters, byte for byte.

## The source

OpenStreetMap publishes a dump of its whole database every week, the
[planet file](https://planet.openstreetmap.org/pbf/). By default, a run
reads the most recent one. The planet holds the current state of the
map, without history: deleted nodes and old positions of moved nodes
are not in it.

The planet of 19 October 2026 has 10,949,467,954 nodes. They all count
equally in the main raster, whether they carry tags or not. Most nodes
have no tags of their own and only give the shape of a way: the outline
of a building, the course of a road, the edge of a forest.

The planet is sorted by type: nodes, then ways, then relations. A run
streams it over HTTPS and stops reading where the relations begin.
It then reads the nodes a second time, to find where the ways of the
tagged layer are. It never stores the planet and never downloads the
relations.

## What a pixel means

In the main raster, a pixel counts the **nodes** whose coordinates fall into it, divided by
the pixel's area. Ways, relations and tags are not looked at. A
building with 40 corners adds 40 nodes, and a node shared by two ways
counts once.

### Grid and encoding

The file uses the Web Mercator grid (EPSG:3857) and covers the world
up to about ±85.05° latitude.

- **Finest level.** Zoom-10 tiles of 256 × 256 pixels, so each pixel is
  a zoom-18 tile. That is about 153 m wide at the equator, 108 m at
  45° and 76 m at 60°. The full image is 262,144 pixels on a side.
- **Overviews.** Ten overview levels follow, down to one tile for the
  world.
- **Values.** A pixel stores nodes per km² of real Earth surface,
  measured on the sphere, so pixels can be compared across latitudes.
  The values are unsigned 16-bit integers on a log scale, with the
  same encoding as the other geosignals rasters:

```
v = 0                                   no nodes
log2(nodes per km²) = 0.01 · v − 15.01  otherwise
```

Each step of 100 doubles the density; each single step is a 0.7%
change. One node in a pixel at the equator (0.0234 km²) is about 43
nodes per km², or v ≈ 2043. At 45°, where a pixel is 0.0117 km², the
same node gives v ≈ 2143. To get node counts back, multiply the density
by the pixel's area. The scale is fixed, not fitted to the data, so
values from different releases are comparable.

The GeoTIFF carries `SCALE` and `OFFSET` metadata, so GDAL and QGIS
show log2(nodes per km²) directly. A `NODES` item holds the number of
nodes that went in. There is no NoData value: zero nodes is a
measurement, not a gap.

The file also carries a raster attribute table with an exact histogram
of the finest level, one row per pixel value. GDAL 3.12 and later, and
QGIS, read it without scanning the image. Use it to turn a pixel value
into a percentile.

### Overview pixels

Nodes add up. An overview pixel holds the sum of the nodes below it,
divided by its own area. Its density is therefore the area-weighted
average of its children's densities, so you can also build coarser
grids yourself by summing nodes (density × area). The overviews give
the same result.

### The values in October 2026

Of the 68.7 billion pixels at the finest level, 1.31 billion (1.9%)
have at least one node. Among those, the densities are spread like
this:

| Percentile | Value | Nodes per km² |
| ---: | ---: | ---: |
| 10% | 2110 | 68 |
| 25% | 2203 | 130 |
| 50% | 2320 | 292 |
| 75% | 2440 | 671 |
| 90% | 2557 | 1,510 |
| 99% | 2806 | 8,481 |
| 99.9% | 2947 | 22,537 |
| maximum | 3571 | 1,703,417 |

The densest areas at zoom 12, about 10 km across at the equator, are
city centres: Tokyo, São Paulo, Paris, Amsterdam and The Hague, each
with about 23,000 to 27,000 nodes per km². At the finest level, the
densest pixels are single objects mapped in great detail rather than
cities (see below).

## The tagged layer

The second raster, `osm-tagged-node-density`, uses the same grid,
encoding and overviews as the main one. It counts fewer things:

- **Nodes with tags that describe something.** Tags about the mapping
  itself don't count: any `source` key (`source`, `source:geometry`
  and the like), `created_by`, `note`, `fixme`, `FIXME`, `attribution`,
  `check_date` and `survey:date`. Imports often set such tags on every
  node, including the corners of buildings. Address tags (`addr:*`)
  don't count either, so a node that is only an address point is left
  out, while a shop with an address counts.
- **Points of interest drawn as ways.** Many shops, schools and parks
  are mapped as outlines rather than as single nodes. A way counts if
  it has one of the keys `amenity`, `shop`, `tourism`, `leisure`,
  `office`, `craft`, `healthcare`, `historic`, `man_made` or
  `public_transport`. It counts once, at the position of its first
  node.

In October, 210,532,191 nodes counted (1.9% of all nodes) and
35,698,342 ways, so the layer's `NODES` metadata says 246,230,533.
Of the 68.7 billion pixels at the finest level, 90.2 million (0.13%)
are not empty. Among those, the densities are spread like this:

| Percentile | Value | Features per km² |
| ---: | ---: | ---: |
| 10% | 2071 | 52 |
| 25% | 2112 | 69 |
| 50% | 2171 | 104 |
| 75% | 2263 | 197 |
| 90% | 2384 | 455 |
| 99% | 2626 | 2,435 |
| 99.9% | 2783 | 7,231 |
| maximum | 3263 | 201,441 |

Half of the non-empty pixels hold a single feature or two. At zoom 12,
the densest areas are again city centres, but a different set from the
main raster: Paris (about 5,400 features per km²), Berlin, Edinburgh,
Gothenburg, Warsaw, Montréal, Helsinki and Brussels. This reflects
where mappers record shops, trees, benches and street furniture.

**Biases of the tagged layer.** Imports of trees, street lamps or
similar objects still show up, because those nodes carry tags that
count; Gothenburg's mapped trees are a likely example (inferred, not
checked). The densest pixels we located are in Berlin (999 features at
52.5141° N, 13.3793° E, presumably the stelae of the Holocaust
Memorial), Spain, Gothenburg, Vienna, Munich, Paris, Strasbourg and
Ashgabat. Points of interest drawn as multipolygon relations, such as
some large parks or campuses, are missed, since relations aren't read.
A way counts at its first node rather than its centre, which can put
a large site into a neighbouring pixel. Ways with none of the listed
keys, such as plain buildings or roads, don't count at all.

## How it is computed

1. **Reading.** Worker threads decode the planet's node blocks and map
   each node to the Hilbert index of its zoom-18 pixel, once for the
   main raster and once more if it has tags that count. For each
   point-of-interest way, the ID of its first node is kept. Reading
   stops at the first block of relations.
2. **Finding the ways.** The ways' first-node IDs are sorted, and the
   nodes are read again. The planet's nodes are sorted by ID too, so
   both lists can be walked side by side. This pass stops once all the
   wanted nodes are found.
3. **Sorting.** Pixel keys are sorted in memory-bounded runs, counted,
   and spilled to disk, compressed.
4. **Building the pyramid.** A merge of the runs streams
   `(pixel, count)` pairs in Hilbert order. The Hilbert curve visits
   every subtree in one stretch, so only the chain of tiles from the
   world down to the current zoom-10 tile is in memory at any time.
   Finished tiles are encoded and added into their parent.
5. **Writing.** Tiles are written into the GeoTIFF in the order of a
   Cloud-Optimized GeoTIFF: smallest overview first. Tiles without any
   nodes are not stored, and readers see them as zero.

The October run took 2 hours 54 minutes on a machine with 4 CPU
cores: 80 minutes for the first pass over the planet, 28 for the
second, 53 for building the main raster and 12 for the tagged one.

## Artifacts and biases

**Nodes outside Web Mercator.** Web Mercator stops at ±85.05°, so
nodes nearer the poles are left out. In October these were 137,959 of
10.9 billion nodes, or 0.0013%. We don't record where they are. North
of 85° there is only the Arctic Ocean, so presumably most are in the
interior of Antarctica, around the South Pole.

**Mapping effort, not the world.** Node density measures how much
mappers have drawn, and how finely. Two identical villages differ in
density if only one has its buildings mapped. Large imports of
buildings, roads, land cover or address points raise the density of
whole regions at once, and the boundary of an import can show up as a
sharp edge in the raster. Changes between releases are just as often
changes in mapping as changes on the ground.

**Detail, not importance.** Every node counts the same. A curved
coastline, a lake shore, a wiggly river or a detailed forest outline
can have as many nodes as a town centre. Rural areas with many small
fields, hedges or stone walls can outrank suburbs.

**Single objects in great detail.** The densest pixels at the finest
level are not city centres. They probably hold single objects drawn
with very many nodes, such as detailed building parts or monuments. The
densest pixel, at 55.0048° N, 10.6149° E in Denmark, has 13,065 nodes
in a 150 m square. The next ones are in Serbia (43.8519° N,
19.8475° E, 16,662 nodes in a larger pixel at lower latitude), near
the Kremlin in Moscow, near VDNKh in Moscow, and on the coast west of
Bergen in Norway. We haven't checked what exactly is mapped at each
of them. If such outliers matter for your use, look at the coarser
overviews or cap the values, for example at the 99.9th percentile.

**Zero means unmapped.** A pixel without nodes may have nothing in it,
like most of the ocean, or something that nobody has mapped yet. The
file can't tell these apart.

## License and attribution

The file is derived from OpenStreetMap and published under the
[Open Database License (ODbL) 1.0](https://opendatacommons.org/licenses/odbl/1-0/),
like OpenStreetMap itself. The GeoTIFF's `Copyright` tag reads
"© OpenStreetMap contributors, ODbL 1.0".

If you publish something based on the file, credit
"© OpenStreetMap contributors" and say it is available under the ODbL,
as described on OpenStreetMap's
[copyright page](https://www.openstreetmap.org/copyright). A database
that you derive from the file and publish falls under the ODbL's
share-alike terms. Whether bare counts of nodes per place are
protected by database rights at all is a separate legal question; this
page is not legal advice.

## Reproducing the files

```sh
cargo run --release -- osm-node-density --output-dir .
```

By default, this streams the most recent weekly planet dump from
`planet.openstreetmap.org` over HTTPS, without storing it. If the
connection drops, reading resumes with a range request. `--input`
also takes a local file, another URL, or `-` for standard input.
Standard input can't be read twice, so with `--input -` the tagged
layer counts only nodes, not ways. Input without a replication
timestamp in its PBF header is rejected, since the output is named
after it. `--smoke-test` reads only the first few blocks, to check
that everything works without processing the planet.

The GeoTIFFs are BigTIFF, tiled 256 × 256 and compressed with
Zstandard. The data BOM names the planet by name, replication
timestamp and download URL, but has no hash of it, since the planet
is only partly read. See [Running geosignals](running.md) for the
container, the data BOM, `datapackage.json` and scratch space.
