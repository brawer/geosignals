# Changelog

## [0.3.1](https://github.com/brawer/geosignals/compare/v0.3.0...v0.3.1) (2026-10-31)


### 🆕 Enhancements

* add format 2 of condensed transit feeds ([f3f31cc](https://github.com/brawer/geosignals/commit/f3f31ccf0ad5ea7d559c4433edf332f20db874b0))
* compare feeds for duplicates on the days both cover ([c27d521](https://github.com/brawer/geosignals/commit/c27d5216259585055f16249c6835f70cb6a472fe))
* condense GTFS and CIF feeds to format 2 ([d6fcca3](https://github.com/brawer/geosignals/commit/d6fcca3940f0b15b27eef814552b3b83ed00c072))
* credit gtfs-data.jp feeds by name ([b10be0a](https://github.com/brawer/geosignals/commit/b10be0a695d191053518882aee2cc6909a9ef936))
* read gtfs-data.jp as a catalog for Japan ([3a7d11a](https://github.com/brawer/geosignals/commit/3a7d11a8b204a55487423c531511fa44e28e40f5))
* read Network Rail's CIF timetable for trains in Great Britain ([2c04804](https://github.com/brawer/geosignals/commit/2c048044e38235ec6d70f665de78e276797b8bb7))
* show the average day of the past 12 months ([498b4ea](https://github.com/brawer/geosignals/commit/498b4ea9568d663b40a0c564cd4fcdf8634b90f8))
* use Network Rail's feed under the Open Government Licence ([d3b5b35](https://github.com/brawer/geosignals/commit/d3b5b351ca3a5aafd6004c20d9609ff29a1abc9e))
* use Swiss transit feeds by permission of opentransportdata.swiss ([#42](https://github.com/brawer/geosignals/issues/42)) ([f7b2873](https://github.com/brawer/geosignals/commit/f7b2873f9365636b58d97bb7d7c3209c19869cfc))
* write a static STAC catalog next to each signal's outputs ([#44](https://github.com/brawer/geosignals/issues/44)) ([d48aa87](https://github.com/brawer/geosignals/commit/d48aa8757eec76488044d3396656ce3a81eccbab))


### 📚 Documentation

* add a technical design for transit departures ([dcd3986](https://github.com/brawer/geosignals/commit/dcd398677af639beca569bd743bb80b44500220e))
* shorten the transit user documentation ([a604227](https://github.com/brawer/geosignals/commit/a6042275fa5046b3076c313a893ebe2e78fc9600))


### 🚧 Maintenance

* import pystac.validation explicitly in the smoke test ([#45](https://github.com/brawer/geosignals/issues/45)) ([3b859df](https://github.com/brawer/geosignals/commit/3b859df2e0394f2f97afc24c447c4c6b968bf887))

## [0.3.0](https://github.com/brawer/geosignals/compare/v0.2.0...v0.3.0) (2026-10-28)


### ⚠ BREAKING CHANGES

* the node-density subcommand is now osm-node-density.

### 🆕 Enhancements

* add naturalist-observers signal from GBIF's iNaturalist and eBird data ([d2c187e](https://github.com/brawer/geosignals/commit/d2c187e8981e53b5182e0f9a81344993ea6f7863))
* add transit-departures signal from GTFS feeds ([#23](https://github.com/brawer/geosignals/issues/23)) ([4446de3](https://github.com/brawer/geosignals/commit/4446de38b79d17a40e7cbbb9174aa6e4860b6152))
* also write a raster of nodes with tags ([#36](https://github.com/brawer/geosignals/issues/36)) ([a0184b9](https://github.com/brawer/geosignals/commit/a0184b99e621f19a9cc0438f2a76a862525883e1))
* count point-of-interest ways in the tagged layer ([#40](https://github.com/brawer/geosignals/issues/40)) ([2d2d192](https://github.com/brawer/geosignals/commit/2d2d1929bd32becd9e38cabf38d6bd675e3d00b7))
* describe the transit dataset and widen its open-license coverage ([#32](https://github.com/brawer/geosignals/issues/32)) ([0eee63c](https://github.com/brawer/geosignals/commit/0eee63c8f875c38e0a2e08305953dc53f12210aa))
* fetch Transitous and Transitland Atlas over HTTP ([#28](https://github.com/brawer/geosignals/issues/28)) ([2ec8e64](https://github.com/brawer/geosignals/commit/2ec8e642287df84f645e39a25e55a09225c9f12e))
* rename node-density to osm-node-density ([#33](https://github.com/brawer/geosignals/issues/33)) ([93426fc](https://github.com/brawer/geosignals/commit/93426fc8c846fcdce07962ca6feabc37f7312a43))
* republish Nominatim's secondary importance raster as a COG ([#21](https://github.com/brawer/geosignals/issues/21)) ([f04ddbc](https://github.com/brawer/geosignals/commit/f04ddbcb9ac9ba72fd87d7198dec37d0f8ae1dec))


### 🐞 Bug Fixes

* satisfy newer clippy and cargo-deny license check ([0dd4f11](https://github.com/brawer/geosignals/commit/0dd4f1185b36fcd8210e6a1cab51518867aa8072))


### ⚡ Performance

* keep condensed transit feeds in memory-mapped scratch files ([#29](https://github.com/brawer/geosignals/issues/29)) ([4e01349](https://github.com/brawer/geosignals/commit/4e01349e06919a8d81f6354cf3731b16e20467a4))


### 📚 Documentation

* describe the naturalist-observers data for analysts ([380a53a](https://github.com/brawer/geosignals/commit/380a53ac569c08b469f74a9ca46f3647fc37c391))
* describe the Nominatim importance dataset and how Nominatim ranks results ([#34](https://github.com/brawer/geosignals/issues/34)) ([ad9e51c](https://github.com/brawer/geosignals/commit/ad9e51c010d0667a687c19e533735a41a09676bf))
* describe the osm-node-density data for analysts ([#35](https://github.com/brawer/geosignals/issues/35)) ([5503d06](https://github.com/brawer/geosignals/commit/5503d06708a38b07c3da86cafdb188ccd51bfdf6))
* explain the 0.85 peak in the Nominatim importance raster ([#38](https://github.com/brawer/geosignals/issues/38)) ([93f785d](https://github.com/brawer/geosignals/commit/93f785d3c9b7bddf1de33c22f233a1d119b37743))
* record how the static musl build fares on memory ([#31](https://github.com/brawer/geosignals/issues/31)) ([2d8c328](https://github.com/brawer/geosignals/commit/2d8c32819627dc685e895c4cf2a6180ebcd6666b))
* shorten README and move details into docs ([#41](https://github.com/brawer/geosignals/issues/41)) ([0605f0f](https://github.com/brawer/geosignals/commit/0605f0feac1f5dde71263326e10773ce8c1aca97))
* state iNaturalist freshness as measured ([e3ee575](https://github.com/brawer/geosignals/commit/e3ee575e927a9103528ba5cb090d5c083e1f6535))
* verify and source the eBird claims, fix review nits ([3dd2d21](https://github.com/brawer/geosignals/commit/3dd2d2132b3554d227ca3a93cf30d6d01a756ca9))


### 🚧 Maintenance

* allow CC0-1.0 dependencies in cargo-deny ([d8be3ca](https://github.com/brawer/geosignals/commit/d8be3cabe8abd27ebc61bec0e4f36100f970e448))
* **ci:** Bump actions/download-artifact from 8.0.1 to 8.0.2 ([#24](https://github.com/brawer/geosignals/issues/24)) ([88fffe7](https://github.com/brawer/geosignals/commit/88fffe7e3a6ef70c49c7c57a0ad009d38f05280a))
* **ci:** Bump actions/upload-artifact from 7.0.1 to 7.0.2 ([#27](https://github.com/brawer/geosignals/issues/27)) ([a0f8729](https://github.com/brawer/geosignals/commit/a0f8729fed5ed07e294d68a983b51453b056c6e2))
* **ci:** Bump docker/login-action from 4.6.0 to 4.7.0 ([#26](https://github.com/brawer/geosignals/issues/26)) ([25b0feb](https://github.com/brawer/geosignals/commit/25b0feb7d9648b58fc30bfddc63a69f45f9142b3))
* **ci:** Bump Swatinem/rust-cache from 2.9.2 to 2.9.3 ([#25](https://github.com/brawer/geosignals/issues/25)) ([9c2fb47](https://github.com/brawer/geosignals/commit/9c2fb4775f862f643a45e6df62eb5b47bbcdd089))
* rename osmsignals to geosignals ([#37](https://github.com/brawer/geosignals/issues/37)) ([2b2735e](https://github.com/brawer/geosignals/commit/2b2735e925659d5a3366177157c919e36ad48d2e))

## [0.2.0](https://github.com/brawer/geosignals/compare/v0.1.0...v0.2.0) (2026-10-21)


### ⚠ BREAKING CHANGES

* --output FILE is replaced by --output-dir DIR.

### 🆕 Enhancements

* add --workdir for temporary files ([#14](https://github.com/brawer/geosignals/issues/14)) ([22989a2](https://github.com/brawer/geosignals/commit/22989a2c10911f0cc5fe138c10d388acb690e5b8))
* name output after the PBF replication timestamp ([#17](https://github.com/brawer/geosignals/issues/17)) ([047bb10](https://github.com/brawer/geosignals/commit/047bb10600cb81790d1ef1db4697514f2b57c941))
* stream the latest planet over HTTPS by default ([#18](https://github.com/brawer/geosignals/issues/18)) ([36f0be6](https://github.com/brawer/geosignals/commit/36f0be6aeda34744364f7f7434efcd5066b11f28))
* write a CycloneDX 1.7 data BOM next to the output ([#19](https://github.com/brawer/geosignals/issues/19)) ([a112e97](https://github.com/brawer/geosignals/commit/a112e97bc3e38e79e3b04591de00fa6175bc84b2))
* write a Frictionless datapackage.json next to the output ([#20](https://github.com/brawer/geosignals/issues/20)) ([631cf9d](https://github.com/brawer/geosignals/commit/631cf9d4e37b2b12c8ed16bf2b8ee8b77d54705c))


### 🚧 Maintenance

* let release-please pick versions after 0.1.0 ([#16](https://github.com/brawer/geosignals/issues/16)) ([62a7bd1](https://github.com/brawer/geosignals/commit/62a7bd102db0f52f44d48c380c7b7fa31b6f2ce3))

## 0.1.0 (2026-10-20)


### ⚠ BREAKING CHANGES

* write node density as Cloud-Optimized GeoTIFF ([#10](https://github.com/brawer/geosignals/issues/10))

### 🆕 Enhancements

* add node-density prototype ([#1](https://github.com/brawer/geosignals/issues/1)) ([1bb3fdb](https://github.com/brawer/geosignals/commit/1bb3fdbccc30aa093dabe07d32f819296b42a583))
* embed histogram of pixel values ([#11](https://github.com/brawer/geosignals/issues/11)) ([079db7f](https://github.com/brawer/geosignals/commit/079db7f4430d935060c0cdc402aed537a18a0348))
* write node density as Cloud-Optimized GeoTIFF ([#10](https://github.com/brawer/geosignals/issues/10)) ([4792f93](https://github.com/brawer/geosignals/commit/4792f93bebbc131d6a504fae9926a0bd28d05993))


### 🚧 Maintenance

* add Containerfile and podman CI workflow ([#8](https://github.com/brawer/geosignals/issues/8)) ([4de6a1e](https://github.com/brawer/geosignals/commit/4de6a1e3777ec58673c49dfa2fb7709b3f7f60b5))
* add dependabot and cargo-deny ([#5](https://github.com/brawer/geosignals/issues/5)) ([2a17a4b](https://github.com/brawer/geosignals/commit/2a17a4bcf498b94aa41d004c27756f4d7fab9300))
* **ci:** Bump actions/create-github-app-token from 3.2.0 to 3.3.0 ([#7](https://github.com/brawer/geosignals/issues/7)) ([79b52a5](https://github.com/brawer/geosignals/commit/79b52a55829a27ca96f1e677f96e6f6fa2541029))
* **container:** Bump library/rust from 1.97-alpine to 1.99-alpine ([#9](https://github.com/brawer/geosignals/issues/9)) ([e631e4b](https://github.com/brawer/geosignals/commit/e631e4b3033e766bf07a49303db2b72a4862bfae))
* cover generated CHANGELOG.md in REUSE.toml ([#12](https://github.com/brawer/geosignals/issues/12)) ([c12276e](https://github.com/brawer/geosignals/commit/c12276e74424e41d445d092882d1bcd361ad49bc))
* lint PR titles, check REUSE compliance, cut releases with release-please ([#4](https://github.com/brawer/geosignals/issues/4)) ([cade29c](https://github.com/brawer/geosignals/commit/cade29c5df179be143db3e112b643aecbaa37dd0))
* publish attested multi-arch container image on release ([#13](https://github.com/brawer/geosignals/issues/13)) ([06b602f](https://github.com/brawer/geosignals/commit/06b602f0da35aeb0d7b8b299b2fc058a542bbeda))
