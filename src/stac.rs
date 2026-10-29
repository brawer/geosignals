// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! A static [STAC 1.1] catalog of each signal, so that tools such as
//! QGIS, pystac and STAC Browser can find and load the GeoTIFFs.
//!
//! Each signal's output directory holds one STAC Collection,
//! `collection.json`, and one STAC Item per release, such as
//! `osm-node-density.2026-10-12.stac.json`. The item lists the
//! release's GeoTIFFs and data BOMs as assets; it is dated and never
//! changes, like the files it points to. The collection has a fixed
//! name and is rewritten by each run; it links to every item in the
//! directory, so it also lists older releases while their files are
//! kept. All links are relative, which makes the catalog
//! self-contained: it can be copied or served from anywhere.
//!
//! Everything is taken from the `datapackage.json` descriptor, which
//! is written just before, so the two always agree. Like it, the STAC
//! files are reproducible: the same inputs give the same files. STAC
//! has no place for license evidence, so the data BOMs stay the
//! authority on sources and licenses.
//!
//! [STAC 1.1]: https://github.com/radiantearth/stac-spec/tree/v1.1.0

use crate::cog::Grid;
use crate::datapackage::Package;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const COLLECTION_FILE_NAME: &str = "collection.json";

const STAC_VERSION: &str = "1.1.0";
const PROJECTION: &str = "https://stac-extensions.github.io/projection/v2.0.0/schema.json";
const FILE: &str = "https://stac-extensions.github.io/file/v2.1.0/schema.json";
const HOMEPAGE: &str = "https://github.com/brawer/geosignals";

/// What a STAC item file is called, for the release of `pkg` on `date`.
pub fn item_file_name(pkg: &Package, date: &str) -> String {
    format!("{}.{date}.stac.json", pkg.name)
}

/// Writes the STAC item for the release described by `datapackage`, and
/// rewrites `collection.json` in `dir`. Returns the item's path.
pub fn write(pkg: &Package, dir: &Path, datapackage: &Value) -> Result<PathBuf> {
    let item = item(pkg, datapackage);
    let version = datapackage["version"].as_str().unwrap_or_default();
    let item_path = dir.join(item_file_name(pkg, version));
    save(&item_path, &item)?;
    let items = scan(pkg, dir)?;
    save(
        &dir.join(COLLECTION_FILE_NAME),
        &collection(pkg, datapackage, &items),
    )?;
    Ok(item_path)
}

/// A STAC item found in the output directory.
struct Found {
    file_name: String,
    title: Option<String>,
    datetime: String,
}

/// Finds this signal's STAC items in `dir`, in date order.
fn scan(pkg: &Package, dir: &Path) -> Result<Vec<Found>> {
    let prefix = format!("{}.", pkg.name);
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let file_name = entry?.file_name().to_string_lossy().into_owned();
        let Some(date) = file_name
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(".stac.json"))
        else {
            continue;
        };
        if !is_date(date) {
            continue;
        }
        let path = dir.join(&file_name);
        let item: Value = serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        let props = &item["properties"];
        let datetime = props["datetime"]
            .as_str()
            .with_context(|| format!("{} has no datetime", path.display()))?
            .to_string();
        found.push(Found {
            file_name,
            title: props["title"].as_str().map(str::to_string),
            datetime,
        });
    }
    found.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(found)
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

fn item(pkg: &Package, dp: &Value) -> Value {
    let version = dp["version"].as_str().unwrap_or_default();
    let [w, s, e, n] = pkg.grid.bbox();
    let mut properties = json!({
        "title": format!("{}, {version}", pkg.title),
        "datetime": dp["created"],
    });
    if let Some(l) = single_license(dp) {
        properties["license"] = json!(l);
    }
    let mut assets = serde_json::Map::new();
    for r in dp["resources"].as_array().into_iter().flatten() {
        let name = r["name"].as_str().unwrap_or_default();
        assets.insert(name.to_string(), asset(&pkg.grid, pkg.nodata, r));
    }
    let up = json!({"href": format!("./{COLLECTION_FILE_NAME}"), "type": "application/json", "title": pkg.title});
    let mut links = vec![
        with_rel("root", &up),
        with_rel("parent", &up),
        with_rel("collection", &up),
    ];
    links.extend(license_links(dp));
    json!({
        "type": "Feature",
        "stac_version": STAC_VERSION,
        "stac_extensions": [PROJECTION, FILE],
        "id": format!("{}-{version}", pkg.name),
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]],
        },
        "bbox": [w, s, e, n],
        "properties": properties,
        "links": links,
        "assets": assets,
        "collection": pkg.name,
    })
}

/// The STAC asset for a Frictionless resource.
fn asset(grid: &Grid, nodata: Option<u16>, r: &Value) -> Value {
    let path = r["path"].as_str().unwrap_or_default();
    let mut a = json!({
        "href": format!("./{path}"),
        "type": r["mediatype"],
        "title": r["title"],
        "description": r["description"],
    });
    if r["format"] == "tiff" {
        let size = grid.size();
        let mut band = json!({"data_type": "uint16"});
        if let Some(v) = nodata {
            band["nodata"] = json!(v);
        }
        a["roles"] = json!(["data"]);
        a["proj:code"] = json!(format!("EPSG:{}", grid.epsg()));
        a["proj:shape"] = json!([size, size]);
        a["proj:transform"] = json!(grid.transform());
        a["bands"] = json!([band]);
    } else {
        a["roles"] = json!(["metadata"]);
    }
    a["file:size"] = r["bytes"].clone();
    if let Some(hex) = r["hash"].as_str().and_then(|h| h.strip_prefix("sha256:")) {
        // A multihash: 0x12 is SHA2-256, 0x20 its length in bytes.
        a["file:checksum"] = json!(format!("1220{hex}"));
    }
    // Resources with their own license, such as observer layers
    // under different Creative Commons licenses, keep it.
    if let Some(l) = r["licenses"].as_array().and_then(|l| l.first()) {
        a["license"] = l["name"].clone();
    }
    a
}

fn collection(pkg: &Package, dp: &Value, items: &[Found]) -> Value {
    let me = json!({"href": format!("./{COLLECTION_FILE_NAME}"), "type": "application/json", "title": pkg.title});
    let mut links = vec![with_rel("root", &me)];
    for i in items {
        let mut link = json!({
            "rel": "item",
            "href": format!("./{}", i.file_name),
            "type": "application/geo+json",
        });
        if let Some(t) = &i.title {
            link["title"] = json!(t);
        }
        links.push(link);
    }
    links.extend(license_links(dp));
    links.push(json!({
        "rel": "about",
        "href": HOMEPAGE,
        "type": "text/html",
        "title": "geosignals on GitHub",
    }));
    let start = items.first().map(|i| i.datetime.as_str());
    let end = items.last().map(|i| i.datetime.as_str());
    json!({
        "type": "Collection",
        "stac_version": STAC_VERSION,
        "stac_extensions": [],
        "id": pkg.name,
        "title": pkg.title,
        "description": pkg.description,
        "license": single_license(dp).unwrap_or("other"),
        "providers": [{
            "name": "geosignals",
            "description": "Computes ranking signals for places from open data.",
            "roles": ["processor"],
            "url": HOMEPAGE,
        }],
        "extent": {
            "spatial": {"bbox": [pkg.grid.bbox()]},
            "temporal": {"interval": [[start, end]]},
        },
        "links": links,
        "assets": {
            "datapackage": {
                "href": format!("./{}", crate::datapackage::FILE_NAME),
                "type": "application/json",
                "title": "Frictionless Data Package",
                "description": "Frictionless Data Package v2 descriptor of the \
                    latest release, with its sources.",
                "roles": ["metadata"],
            },
        },
    })
}

fn with_rel(rel: &str, link: &Value) -> Value {
    let mut l = json!({"rel": rel});
    for (k, v) in link.as_object().into_iter().flatten() {
        l[k] = v.clone();
    }
    l
}

/// The SPDX identifier, if the whole package has one license.
fn single_license(dp: &Value) -> Option<&str> {
    match dp["licenses"].as_array().map(Vec::as_slice) {
        Some([l]) => l["name"].as_str(),
        _ => None,
    }
}

/// A `license` link to each distinct license in the package.
fn license_links(dp: &Value) -> Vec<Value> {
    let all = dp["licenses"].as_array().into_iter().flatten().chain(
        dp["resources"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|r| r["licenses"].as_array().into_iter().flatten()),
    );
    let mut links: Vec<Value> = Vec::new();
    for l in all {
        if links.iter().any(|k| k["href"] == l["path"]) {
            continue;
        }
        links.push(json!({
            "rel": "license",
            "href": l["path"],
            "type": "text/html",
            "title": l["title"],
        }));
    }
    links
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

    fn datapackage(date: &str) -> Value {
        json!({
            "version": date,
            "created": format!("{date}T00:00:00Z"),
            "licenses": [{"name": "ODbL-1.0", "path": "https://opendatacommons.org/licenses/odbl/1-0/", "title": "ODbL"}],
            "resources": [
                {"name": "osm-node-density", "path": format!("osm-node-density.{date}.tiff"),
                 "title": "All nodes", "description": "COG", "format": "tiff",
                 "mediatype": "image/tiff; application=geotiff; profile=cloud-optimized",
                 "bytes": 1234, "hash": format!("sha256:{}", "a".repeat(64)),
                 "licenses": [{"name": "ODbL-1.0", "path": "https://opendatacommons.org/licenses/odbl/1-0/", "title": "ODbL"}]},
                {"name": "osm-node-density-bom", "path": format!("osm-node-density.{date}.cdx.json"),
                 "title": "BOM", "description": "BOM", "format": "json",
                 "mediatype": "application/vnd.cyclonedx+json",
                 "bytes": 56, "hash": format!("sha256:{}", "b".repeat(64))},
            ],
        })
    }

    #[test]
    fn writes_item_and_collection() {
        let dir = tempfile::tempdir().unwrap();
        // Files that look alike but are no items of this signal.
        std::fs::write(dir.path().join("osm-node-density.latest.stac.json"), "x").unwrap();
        std::fs::write(dir.path().join("other.2026-01-01.stac.json"), "x").unwrap();
        write(&PKG, dir.path(), &datapackage("2026-10-12")).unwrap();
        let path = write(&PKG, dir.path(), &datapackage("2026-10-19")).unwrap();
        assert_eq!(
            path,
            dir.path().join("osm-node-density.2026-10-19.stac.json")
        );

        let item: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(item["id"], "osm-node-density-2026-10-19");
        assert_eq!(item["properties"]["datetime"], "2026-10-19T00:00:00Z");
        assert_eq!(item["properties"]["license"], "ODbL-1.0");
        let tiff = &item["assets"]["osm-node-density"];
        assert_eq!(tiff["href"], "./osm-node-density.2026-10-19.tiff");
        assert_eq!(tiff["roles"], json!(["data"]));
        assert_eq!(tiff["proj:code"], "EPSG:3857");
        assert_eq!(tiff["proj:shape"], json!([1 << 18, 1 << 18]));
        assert_eq!(tiff["file:checksum"], format!("1220{}", "a".repeat(64)));
        assert!(tiff["bands"][0].get("nodata").is_none());
        let bom = &item["assets"]["osm-node-density-bom"];
        assert_eq!(bom["roles"], json!(["metadata"]));
        assert_eq!(bom["file:size"], 56);

        let c: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(COLLECTION_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(c["license"], "ODbL-1.0");
        let items: Vec<&Value> = c["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "item")
            .map(|l| &l["href"])
            .collect();
        assert_eq!(
            items,
            [
                "./osm-node-density.2026-10-12.stac.json",
                "./osm-node-density.2026-10-19.stac.json"
            ]
        );
        assert_eq!(
            c["extent"]["temporal"]["interval"],
            json!([["2026-10-12T00:00:00Z", "2026-10-19T00:00:00Z"]])
        );
        let licenses = c["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "license");
        assert_eq!(licenses.count(), 1);
    }

    #[test]
    fn several_licenses() {
        let mut dp = datapackage("2026-10-12");
        dp.as_object_mut().unwrap().remove("licenses");
        dp["resources"][1]["licenses"] = json!([{"name": "CC-BY-NC-4.0", "path": "https://creativecommons.org/licenses/by-nc/4.0/", "title": "CC BY-NC"}]);
        let pkg = Package {
            grid: Grid {
                crs: Crs::Wgs84,
                max_zoom: 6,
            },
            nodata: Some(0),
            ..PKG
        };
        let item = item(&pkg, &dp);
        assert!(item["properties"].get("license").is_none());
        assert_eq!(item["assets"]["osm-node-density"]["license"], "ODbL-1.0");
        assert_eq!(
            item["assets"]["osm-node-density-bom"]["license"],
            "CC-BY-NC-4.0"
        );
        assert_eq!(item["bbox"], json!([-180.0, -90.0, 180.0, 90.0]));
        assert_eq!(item["assets"]["osm-node-density"]["bands"][0]["nodata"], 0);
        let c = collection(&pkg, &dp, &[]);
        assert_eq!(c["license"], "other");
        let licenses = c["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["rel"] == "license");
        assert_eq!(licenses.count(), 2);
    }

    #[test]
    fn dates() {
        assert!(is_date("2026-10-12"));
        assert!(!is_date("2026-10-1x"));
        assert!(!is_date("latest"));
    }
}
