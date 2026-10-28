# Changelog

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
