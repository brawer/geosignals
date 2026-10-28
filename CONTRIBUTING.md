<!--
SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
SPDX-License-Identifier: MIT
-->

# Contributing

## Tests

`cargo test` runs unit tests and end-to-end tests:

- node density on a small extract around Zug;
- Nominatim's secondary importance on a tiny dump in Nominatim's format;
- public transport departures on two small GTFS feeds served over HTTP,
  through the cache while the server is up, unchanged and down;
- naturalist observers on small Parquet files laid out like GBIF's,
  served from a fake S3 bucket.

A smoke test on Liechtenstein from Geofabrik runs for release tags and
on demand (see [`smoke-test.yml`](.github/workflows/smoke-test.yml));
locally, point `GEOSIGNALS_SMOKE_PBF` at any extract to run it. It also
republishes the real Nominatim raster and checks the result with GDAL.

## Pull requests

PR titles follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat: …`, `fix: …`, `docs: …`), since PRs are squash-merged with
the title as commit message. Every file carries SPDX headers; check
with `reuse lint`.

## Releases

Releases are cut by [release-please](https://github.com/googleapis/release-please):
merging its release PR bumps the version, updates `CHANGELOG.md`, tags
the commit and publishes a GitHub Release.
