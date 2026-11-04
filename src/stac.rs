// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! A [STAC 1.1] item for each signal, so that clients and tools such as
//! QGIS, pystac and STAC Browser can find and load the latest GeoTIFFs.
//!
//! Each release of a signal is a dated GeoTIFF and a dated data BOM,
//! such as `osm-node-density.2026-10-12.tiff` and
//! `osm-node-density.2026-10-12.cdx.json`; some signals have several
//! of each. The dated files never change. Next to them, one undated
//! STAC item, such as `osm-node-density.stac.json`, points to the files
//! of the latest release. Each run replaces it, so it is the only file
//! that changes in place; a client fetches it, compares `datetime`, and
//! resolves each asset's relative `href` against the item's own URL.
//!
//! Like the BOM, every field comes from the input or the files, so the
//! same inputs give the same item. STAC has no place for license
//! evidence, so the data BOMs stay the authority on sources and
//! licenses.
//!
//! [STAC 1.1]: https://github.com/radiantearth/stac-spec/tree/v1.1.0

use crate::bom::{self, Hashes, License};
use crate::cog::Grid;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const STAC_VERSION: &str = "1.1.0";
const PROJECTION: &str = "https://stac-extensions.github.io/projection/v2.0.0/schema.json";
const FILE: &str = "https://stac-extensions.github.io/file/v2.1.0/schema.json";
const HOMEPAGE: &str = "https://github.com/brawer/geosignals";

/// Media type of a STAC item.
pub const ITEM_MEDIA_TYPE: &str = "application/geo+json";

/// Media type of Cloud-Optimized GeoTIFF, as registered by the OGC.
const COG_MEDIA_TYPE: &str = "image/tiff; application=geotiff; profile=cloud-optimized";

/// Registered with IANA for CycloneDX JSON.
const CYCLONEDX_MEDIA_TYPE: &str = "application/vnd.cyclonedx+json";

/// Describes one signal.
pub struct Package<'a> {
    /// Name of the signal, such as `osm-node-density`. Also the name of
    /// its output folder and of its STAC item.
    pub name: &'a str,
    /// Stable identity of the dataset, the same for every release.
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    /// Pixel grid of the GeoTIFFs.
    pub grid: Grid,
    /// The GeoTIFFs' GDAL_NODATA value, if they have one.
    pub nodata: Option<u16>,
}

/// One GeoTIFF of a release, with its data BOM.
pub struct Layer<'a> {
    /// Asset key, such as `osm-node-density`; the BOM's key gets
    /// `-bom` appended.
    pub name: &'a str,
    pub title: &'a str,
    pub tiff: &'a Path,
    pub tiff_hashes: &'a Hashes,
    pub bom: &'a Path,
    pub license: &'a License,
}

/// What the STAC item of the signal named `name` is called.
pub fn item_file_name(name: &str) -> String {
    format!("{name}.stac.json")
}

/// Writes the STAC item for a release whose data is as of `timestamp`
/// (seconds since the epoch), next to the first layer's GeoTIFF, and
/// returns its path. Call it after writing the layers, so that the item
/// never points to files that are not there yet.
pub fn write(pkg: &Package, timestamp: i64, layers: &[Layer]) -> Result<PathBuf> {
    ensure!(!layers.is_empty(), "no layers to describe");
    let mut bom_hashes = Vec::new();
    for l in layers {
        bom_hashes.push(bom::hash_file(l.bom)?);
    }
    let doc = item(pkg, timestamp, layers, &bom_hashes);
    let dir = layers[0].tiff.parent().unwrap_or(Path::new("."));
    let path = dir.join(item_file_name(pkg.name));
    save(&path, &doc)?;
    Ok(path)
}

fn item(pkg: &Package, timestamp: i64, layers: &[Layer], bom_hashes: &[Hashes]) -> Value {
    let created = crate::date::rfc3339(timestamp);
    let [w, s, e, n] = pkg.grid.bbox();
    let mut properties = json!({
        "title": pkg.title,
        "description": pkg.description,
        "datetime": created,
        "providers": [{
            "name": "geosignals",
            "description": "Computes ranking signals for places from open data.",
            "roles": ["processor"],
            "url": HOMEPAGE,
        }],
    });
    // When all layers share a license, the item states it too.
    if layers.iter().all(|l| l.license.id == layers[0].license.id) {
        properties["license"] = json!(layers[0].license.id);
    }
    let mut assets = serde_json::Map::new();
    let mut licenses: Vec<&License> = Vec::new();
    for (l, bh) in layers.iter().zip(bom_hashes) {
        assets.insert(l.name.to_string(), tiff_asset(pkg, l));
        assets.insert(format!("{}-bom", l.name), bom_asset(l, bh));
        if !licenses.iter().any(|k| k.id == l.license.id) {
            licenses.push(l.license);
        }
    }
    let mut links: Vec<Value> = licenses
        .iter()
        .map(|l| {
            json!({
                "rel": "license",
                "href": l.url,
                "type": "text/html",
                "title": l.title,
            })
        })
        .collect();
    links.push(json!({
        "rel": "about",
        "href": HOMEPAGE,
        "type": "text/html",
        "title": "geosignals on GitHub",
    }));
    json!({
        "type": "Feature",
        "stac_version": STAC_VERSION,
        "stac_extensions": [PROJECTION, FILE],
        "id": pkg.name,
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
        },
        "bbox": [w, s, e, n],
        "properties": properties,
        "links": links,
        "assets": assets,
    })
}

fn tiff_asset(pkg: &Package, l: &Layer) -> Value {
    let size = pkg.grid.size();
    let mut band = json!({"data_type": "uint16"});
    if let Some(v) = pkg.nodata {
        band["nodata"] = json!(v);
    }
    let mut a = file_asset(
        l.tiff,
        l.title,
        "Cloud-Optimized GeoTIFF. This dated file never changes; \
         the next release gets a new one.",
        COG_MEDIA_TYPE,
        "data",
        l.tiff_hashes,
        l.license,
    );
    a["proj:code"] = json!(format!("EPSG:{}", pkg.grid.epsg()));
    a["proj:shape"] = json!([size, size]);
    a["proj:transform"] = json!(pkg.grid.transform());
    a["bands"] = json!([band]);
    a
}

fn bom_asset(l: &Layer, hashes: &Hashes) -> Value {
    file_asset(
        l.bom,
        "Data bill of materials (CycloneDX 1.7)",
        &format!(
            "Which geosignals release made {}, from which sources.",
            l.title
        ),
        CYCLONEDX_MEDIA_TYPE,
        "metadata",
        hashes,
        l.license,
    )
}

fn file_asset(
    path: &Path,
    title: &str,
    description: &str,
    media_type: &str,
    role: &str,
    hashes: &Hashes,
    license: &License,
) -> Value {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "href": format!("./{file_name}"),
        "type": media_type,
        "title": title,
        "description": description,
        "roles": [role],
        "license": license.id,
        "file:size": hashes.bytes,
        // A multihash: 0x12 is SHA2-256, 0x20 its length in bytes.
        "file:checksum": format!("1220{}", hashes.sha256),
    })
}

/// The files that the STAC item at `path` points to, as file names in
/// its directory, with their media types.
pub fn asset_files(path: &Path) -> Result<Vec<(String, String)>> {
    let item: Value = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    let mut files = Vec::new();
    for (key, a) in item["assets"].as_object().into_iter().flatten() {
        let href = a["href"]
            .as_str()
            .with_context(|| format!("asset {key} has no href"))?;
        let name = href.strip_prefix("./").unwrap_or(href);
        ensure!(
            is_file_name(name),
            "asset {key} points to {href:?}, which is not in the same directory"
        );
        let media_type = a["type"].as_str().unwrap_or("application/octet-stream");
        files.push((name.to_string(), media_type.to_string()));
    }
    Ok(files)
}

/// True for a plain file name in the same directory.
fn is_file_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '?', '#'])
}

fn save(path: &Path, doc: &Value) -> Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let mut text = serde_json::to_string_pretty(doc)?;
    text.push('\n');
    std::fs::write(&partial, text).with_context(|| format!("writing {}", path.display()))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog::Crs;

    const PKG: Package = Package {
        name: "osm-node-density",
        id: "2c7bffea-cc30-533e-ac1d-dc7b4aefd158",
        title: "OpenStreetMap node density",
        description: "Nodes per km².",
        grid: Grid::WEB_MERCATOR,
        nodata: None,
    };

    fn hashes(c: &str) -> Hashes {
        Hashes {
            bytes: 1234,
            sha256: c.repeat(64),
            sha512: c.repeat(128),
        }
    }

    #[test]
    fn writes_item() {
        let dir = tempfile::tempdir().unwrap();
        let tiff = dir.path().join("osm-node-density.2026-10-19.tiff");
        let bom = dir.path().join("osm-node-density.2026-10-19.cdx.json");
        std::fs::write(&tiff, "tiff").unwrap();
        std::fs::write(&bom, "bom").unwrap();
        let h = hashes("a");
        let layers = [Layer {
            name: "osm-node-density",
            title: "All nodes",
            tiff: &tiff,
            tiff_hashes: &h,
            bom: &bom,
            license: &bom::ODBL,
        }];
        let path = write(&PKG, 1_760_832_000, &layers).unwrap();
        assert_eq!(path, dir.path().join("osm-node-density.stac.json"));

        let item: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(item["id"], "osm-node-density");
        assert_eq!(item["properties"]["datetime"], "2025-10-19T00:00:00Z");
        assert_eq!(item["properties"]["license"], "ODbL-1.0");
        let t = &item["assets"]["osm-node-density"];
        assert_eq!(t["href"], "./osm-node-density.2026-10-19.tiff");
        assert_eq!(t["roles"], json!(["data"]));
        assert_eq!(t["proj:code"], "EPSG:3857");
        assert_eq!(t["proj:shape"], json!([1 << 18, 1 << 18]));
        assert_eq!(t["file:checksum"], format!("1220{}", "a".repeat(64)));
        assert!(t["bands"][0].get("nodata").is_none());
        let b = &item["assets"]["osm-node-density-bom"];
        assert_eq!(b["href"], "./osm-node-density.2026-10-19.cdx.json");
        assert_eq!(b["roles"], json!(["metadata"]));
        assert_eq!(b["file:size"], 3);
        let licenses = item["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "license");
        assert_eq!(licenses.count(), 1);

        let files = asset_files(&path).unwrap();
        let names: Vec<&str> = files.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(
            names,
            [
                "osm-node-density.2026-10-19.tiff",
                "osm-node-density.2026-10-19.cdx.json"
            ]
        );
        assert_eq!(files[0].1, COG_MEDIA_TYPE);
        assert_eq!(files[1].1, CYCLONEDX_MEDIA_TYPE);
    }

    #[test]
    fn several_licenses() {
        let (h, b) = (hashes("a"), hashes("b"));
        let layers = [
            Layer {
                name: "x-cc-by-4.0",
                title: "CC BY",
                tiff: Path::new("/out/x-cc-by-4.0.2026-10-01.tiff"),
                tiff_hashes: &h,
                bom: Path::new("/out/x-cc-by-4.0.2026-10-01.cdx.json"),
                license: &bom::CC_BY_4_0,
            },
            Layer {
                name: "x-cc-by-nc-4.0",
                title: "CC BY-NC",
                tiff: Path::new("/out/x-cc-by-nc-4.0.2026-10-01.tiff"),
                tiff_hashes: &h,
                bom: Path::new("/out/x-cc-by-nc-4.0.2026-10-01.cdx.json"),
                license: &bom::CC_BY_NC_4_0,
            },
        ];
        let pkg = Package {
            grid: Grid {
                crs: Crs::Wgs84,
                max_zoom: 6,
            },
            nodata: Some(0),
            ..PKG
        };
        let item = item(&pkg, 0, &layers, &[b.clone(), b]);
        assert!(item["properties"].get("license").is_none());
        assert_eq!(item["assets"]["x-cc-by-4.0"]["license"], "CC-BY-4.0");
        assert_eq!(
            item["assets"]["x-cc-by-nc-4.0-bom"]["license"],
            "CC-BY-NC-4.0"
        );
        assert_eq!(item["bbox"], json!([-180.0, -90.0, 180.0, 90.0]));
        assert_eq!(item["assets"]["x-cc-by-4.0"]["bands"][0]["nodata"], 0);
        let licenses = item["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "license");
        assert_eq!(licenses.count(), 2);
    }

    #[test]
    fn rejects_assets_outside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.stac.json");
        std::fs::write(&path, r#"{"assets": {"a": {"href": "../secret"}}}"#).unwrap();
        assert!(asset_files(&path).is_err());
    }
}
