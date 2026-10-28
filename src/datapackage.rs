// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! `datapackage.json`, a [Frictionless Data Package v2] descriptor that
//! points to the dated GeoTIFF and its data BOM.
//!
//! Unlike the dated files, it has a fixed name and is replaced by each
//! run, so a client can fetch it, compare `version`, and resolve each
//! `resources[].path` (a bare file name) against the descriptor's own
//! URL. Like the BOM, every field comes from the input or the files, so
//! rebuilding from the same planet with the same release gives the same
//! descriptor. The conventions (frozen `id`, `version` as the data date,
//! relative paths) follow brawer/osmdiffs and brawer/osmviews.
//!
//! [Frictionless Data Package v2]: https://datapackage.org/standard/data-package/

use crate::bom::{self, Hashes, License};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "datapackage.json";

const SCHEMA: &str = "https://datapackage.org/profiles/2.0/datapackage.json";
const HOMEPAGE: &str = "https://github.com/brawer/geosignals";

/// Media type of Cloud-Optimized GeoTIFF, as registered by the OGC.
const COG_MEDIA_TYPE: &str = "image/tiff; application=geotiff; profile=cloud-optimized";

/// Registered with IANA for CycloneDX JSON.
const CYCLONEDX_MEDIA_TYPE: &str = "application/vnd.cyclonedx+json";

/// Describes one signal's package.
pub struct Package<'a> {
    /// Package name, such as `osm-node-density`. Also the name of the
    /// GeoTIFF resource; the BOM resource gets `-bom` appended.
    pub name: &'a str,
    /// Stable identity of the dataset, the same for every release.
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
}

/// What one run produced.
pub struct Files<'a> {
    pub input: &'a bom::Input,
    /// Replication timestamp of the input, in seconds since the epoch.
    pub timestamp: i64,
    pub tiff: &'a Path,
    pub tiff_hashes: &'a Hashes,
    pub bom: &'a Path,
}

/// Writes `datapackage.json` next to the files and returns its path.
pub fn write(pkg: &Package, files: &Files) -> Result<PathBuf> {
    let bom_hashes = bom::hash_file(files.bom)?;
    save(files.tiff, &build(pkg, files, &bom_hashes))
}

/// What one run made from many sources produced.
pub struct ManyFiles<'a> {
    /// Each source as a Frictionless source object, with `title`,
    /// `path` and `version`.
    pub sources: Vec<Value>,
    /// What to call one source in descriptions, such as "GTFS feed".
    pub noun: &'a str,
    /// Date of the data, in seconds since the epoch.
    pub timestamp: i64,
    pub tiff: &'a Path,
    pub tiff_hashes: &'a Hashes,
    pub bom: &'a Path,
}

/// Like [`write`], for an output made from many sources.
pub fn write_many(pkg: &Package, files: &ManyFiles) -> Result<PathBuf> {
    let bom_hashes = bom::hash_file(files.bom)?;
    let doc = document(
        pkg,
        files.sources.clone(),
        files.noun,
        files.timestamp,
        files.tiff,
        files.tiff_hashes,
        files.bom,
        &bom_hashes,
        &bom::ODBL,
    );
    save(files.tiff, &doc)
}

/// One of several GeoTIFFs in a package, each under its own license.
pub struct Layer<'a> {
    /// Resource name, such as `inaturalist-observers-cc-by-4.0`; the
    /// BOM resource gets `-bom` appended.
    pub name: &'a str,
    pub title: &'a str,
    pub tiff: &'a Path,
    pub tiff_hashes: &'a Hashes,
    pub bom: &'a Path,
    pub license: &'a License,
}

/// Writes `datapackage.json` for a run that made several GeoTIFFs,
/// next to the first one. Each resource states its own license, since
/// the files differ in what they may be used for.
pub fn write_layers(
    pkg: &Package,
    sources: Vec<Value>,
    timestamp: i64,
    layers: &[Layer],
) -> Result<PathBuf> {
    anyhow::ensure!(!layers.is_empty(), "no layers to describe");
    let created = crate::date::rfc3339(timestamp);
    let mut resources = Vec::new();
    for l in layers {
        let bom_hashes = bom::hash_file(l.bom)?;
        let licenses = json!([license_json(l.license)]);
        let mut tiff = resource(
            l.name,
            l.tiff,
            l.title,
            "Cloud-Optimized GeoTIFF. This dated file never changes; \
             the next release gets a new one.",
            "tiff",
            COG_MEDIA_TYPE,
            l.tiff_hashes,
        );
        tiff["licenses"] = licenses.clone();
        let mut bom = resource(
            &format!("{}-bom", l.name),
            l.bom,
            "Data bill of materials (CycloneDX 1.7)",
            &format!(
                "Which geosignals release made {}, from which sources.",
                l.title
            ),
            "json",
            CYCLONEDX_MEDIA_TYPE,
            &bom_hashes,
        );
        bom["licenses"] = licenses;
        resources.push(tiff);
        resources.push(bom);
    }
    let mut doc = json!({
        "$schema": SCHEMA,
        "name": pkg.name,
        "id": pkg.id,
        "title": pkg.title,
        "description": pkg.description,
        "version": &created[..10],
        "created": created,
        "homepage": HOMEPAGE,
        "sources": sources,
        "resources": resources,
    });
    // When all layers share a license, the package states it too.
    if layers.iter().all(|l| l.license.id == layers[0].license.id) {
        doc["licenses"] = json!([license_json(layers[0].license)]);
    }
    save(layers[0].tiff, &doc)
}

fn license_json(l: &License) -> Value {
    json!({"name": l.id, "path": l.url, "title": l.title})
}

fn save(tiff: &Path, doc: &Value) -> Result<PathBuf> {
    let dir = tiff.parent().unwrap_or(Path::new("."));
    let path = dir.join(FILE_NAME);
    let partial = dir.join(format!("{FILE_NAME}.partial"));
    let mut text = serde_json::to_string_pretty(doc)?;
    text.push('\n');
    std::fs::write(&partial, text).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// The Frictionless source object for `input`, whose data is as of
/// `timestamp` (seconds since the epoch).
pub fn input_source(input: &bom::Input, timestamp: i64) -> Value {
    let mut source = json!({
        "title": format!("{} {}", input.supplier.title, input.name),
        "version": crate::date::rfc3339(timestamp),
    });
    if let Some(url) = &input.url {
        source["path"] = json!(url);
    }
    source
}

fn build(pkg: &Package, files: &Files, bom_hashes: &Hashes) -> Value {
    document(
        pkg,
        vec![input_source(files.input, files.timestamp)],
        files.input.supplier.noun,
        files.timestamp,
        files.tiff,
        files.tiff_hashes,
        files.bom,
        bom_hashes,
        &bom::ODBL,
    )
}

#[allow(clippy::too_many_arguments)]
fn document(
    pkg: &Package,
    sources: Vec<Value>,
    noun: &str,
    timestamp: i64,
    tiff: &Path,
    tiff_hashes: &Hashes,
    bom: &Path,
    bom_hashes: &Hashes,
    license: &License,
) -> Value {
    let created = crate::date::rfc3339(timestamp);
    let date = &created[..10];
    json!({
        "$schema": SCHEMA,
        "name": pkg.name,
        "id": pkg.id,
        "title": pkg.title,
        "description": pkg.description,
        "version": date,
        "created": created,
        "homepage": HOMEPAGE,
        "licenses": [license_json(license)],
        "sources": sources,
        "resources": [
            resource(
                pkg.name,
                tiff,
                pkg.title,
                "Cloud-Optimized GeoTIFF. This dated file never changes; \
                 the next release gets a new one.",
                "tiff",
                COG_MEDIA_TYPE,
                tiff_hashes,
            ),
            resource(
                &format!("{}-bom", pkg.name),
                bom,
                "Data bill of materials (CycloneDX 1.7)",
                &format!("Which geosignals release made the GeoTIFF, from which {noun}."),
                "json",
                CYCLONEDX_MEDIA_TYPE,
                bom_hashes,
            ),
        ],
    })
}

fn resource(
    name: &str,
    path: &Path,
    title: &str,
    description: &str,
    format: &str,
    mediatype: &str,
    hashes: &Hashes,
) -> Value {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "name": name,
        "path": file_name,
        "title": title,
        "description": description,
        "format": format,
        "mediatype": mediatype,
        "bytes": hashes.bytes,
        "hash": format!("sha256:{}", hashes.sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_descriptor() {
        let input = bom::Input::new("https://example.org/planet-261012.osm.pbf");
        let hashes = |c: &str| Hashes {
            bytes: 1234,
            sha256: c.repeat(64),
            sha512: c.repeat(128),
        };
        let pkg = Package {
            name: "osm-node-density",
            id: "2c7bffea-cc30-533e-ac1d-dc7b4aefd158",
            title: "OpenStreetMap node density",
            description: "Nodes per km².",
        };
        let files = Files {
            input: &input,
            timestamp: 1_760_918_400,
            tiff: Path::new("/out/osm-node-density.2025-10-20.tiff"),
            tiff_hashes: &hashes("a"),
            bom: Path::new("/out/osm-node-density.2025-10-20.cdx.json"),
        };
        let doc = build(&pkg, &files, &hashes("b"));
        assert_eq!(doc["version"], "2025-10-20");
        assert_eq!(doc["created"], "2025-10-20T00:00:00Z");
        assert_eq!(
            doc["sources"][0]["path"],
            "https://example.org/planet-261012.osm.pbf"
        );
        let r = &doc["resources"];
        assert_eq!(r[0]["name"], "osm-node-density");
        assert_eq!(r[0]["path"], "osm-node-density.2025-10-20.tiff");
        assert_eq!(r[0]["hash"], format!("sha256:{}", "a".repeat(64)));
        assert_eq!(r[1]["name"], "osm-node-density-bom");
        assert_eq!(r[1]["path"], "osm-node-density.2025-10-20.cdx.json");
        assert_eq!(r[1]["bytes"], 1234);
        // Frictionless v2 only allows "table" as a resource type.
        assert!(r[0].get("type").is_none());
    }
}
