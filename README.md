<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Geo Signals

[![Tests](https://github.com/brawer/geosignals/actions/workflows/test.yml/badge.svg)](https://github.com/brawer/geosignals/actions/workflows/test.yml)
[![REUSE status](https://api.reuse.software/badge/github.com/brawer/geosignals)](https://api.reuse.software/info/github.com/brawer/geosignals)

Ranking signals for places on Earth, computed from open data and
published as [Cloud-Optimized GeoTIFFs](https://cogeo.org/), each with
a data bill of materials and a Frictionless `datapackage.json`. Work in
progress.

| Signal | What a pixel holds | Source |
| --- | --- | --- |
| [OSM node density](docs/osm-node-density.md) | OpenStreetMap nodes per km², and a second layer of tagged features | OSM planet |
| [Nominatim secondary importance](docs/nominatim-secondary-importance.md) | Nominatim's location-based importance, republished unchanged | nominatim.org |
| [Public transport departures](docs/transit-departures.md) | Departures per day and km² | GTFS feeds listed by Transitous |
| [Naturalist observers](docs/naturalist-observers.md) | Distinct iNaturalist and eBird observers per km² | GBIF occurrence snapshot |

Each page explains what the files mean, how they are made, their
biases and their licenses.

## Quick start

```sh
cargo run --release -- osm-node-density --output-dir .
cargo run --release -- nominatim-secondary-importance --output-dir nominatim
cargo run --release -- transit-departures --cache cache --output-dir transit
cargo run --release -- naturalist-observers --output-dir observers
```

Or use the container image `ghcr.io/brawer/geosignals`. See
[Running geosignals](docs/running.md) for the container, the output
files and resource needs, and `--help` for all options.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for tests, PR conventions and
releases.

## License

Code: [MIT](LICENSE). Each output file states its own license and
attribution; see the signal's page. `tests/data/zugerland.osm.pbf` is an
OpenStreetMap extract (ODbL), taken from
[brawer/osmdiffs](https://github.com/brawer/osmdiffs).
