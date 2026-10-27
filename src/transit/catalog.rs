// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The list of GTFS feeds to read, taken from [Transitous].
//!
//! Transitous curates feeds for its worldwide router in one JSON file
//! per region, such as `feeds/ch.json`, under CC0. A source there is
//! either a direct download (`http`, `url`) or a reference into the
//! [Transitland Atlas] (`transitland-atlas`, CC-BY) or the [Mobility
//! Database] (`mobility-database`), which we resolve with those
//! catalogs. We take only static GTFS, skip what Transitous skips, and
//! skip feeds that need credentials, since Transitous's keys are not
//! ours to use.
//!
//! [Transitous]: https://github.com/public-transport/transitous
//! [Transitland Atlas]: https://github.com/transitland/transitland-atlas
//! [Mobility Database]: https://mobilitydatabase.org/

use super::repo::FeedFiles;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;

/// One GTFS feed to fetch.
#[derive(Clone, Debug, PartialEq)]
pub struct Feed {
    /// Unique and stable, such as `ch/opentransportdataswiss26`: the
    /// Transitous region file and the source name.
    pub id: String,
    /// Where to download it, best first. Later URLs are mirrors.
    pub urls: Vec<String>,
    pub license: License,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct License {
    /// SPDX identifier, such as `CC-BY-4.0`, if known.
    pub spdx: Option<String>,
    /// Page with the license or terms of use.
    pub url: Option<String>,
    /// Attribution the publisher asks for.
    pub attribution: Option<String>,
}

/// SPDX licenses whose data we may combine and republish under ODbL
/// with attribution. Share-alike licenses other than ODbL, and
/// non-commercial or no-derivatives ones, are not on the list.
pub const OPEN_LICENSES: &[&str] = &[
    "CC0-1.0",
    "PDDL-1.0",
    "CC-BY-2.0",
    "CC-BY-2.5",
    "CC-BY-3.0",
    "CC-BY-3.0-AT",
    "CC-BY-3.0-DE",
    "CC-BY-3.0-NL",
    "CC-BY-4.0",
    "ODC-By-1.0",
    "ODbL-1.0",
    "etalab-2.0",
    "NLOD-1.0",
    "NLOD-2.0",
    "DL-DE-BY-2.0",
    "DL-DE-ZERO-2.0",
    "OGL-UK-3.0",
    "OGL-Canada-2.0",
    "MIT",
    "Apache-2.0",
];

impl License {
    pub fn is_open(&self) -> bool {
        self.spdx
            .as_deref()
            .is_some_and(|id| OPEN_LICENSES.iter().any(|o| o.eq_ignore_ascii_case(id)))
    }
}

/// A source we do not read, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
}

/// The other catalogs that Transitous sources may point into.
#[derive(Default)]
pub struct Catalogs {
    /// Transitland Atlas feeds by Onestop ID.
    pub atlas: HashMap<String, Value>,
    /// Rows of the Mobility Database export `feeds_v2.csv` by id.
    pub mdb: HashMap<String, HashMap<String, String>>,
}

impl Catalogs {
    /// Reads the feed files of the Transitland Atlas.
    pub fn load_atlas(&mut self, files: &FeedFiles) -> Result<()> {
        for (name, data) in &files.files {
            let doc: Value = serde_json::from_slice(data)
                .with_context(|| format!("parsing Transitland Atlas feeds/{name}.json"))?;
            for feed in doc["feeds"].as_array().into_iter().flatten() {
                if let Some(id) = feed["id"].as_str() {
                    self.atlas.insert(id.to_string(), feed.clone());
                }
            }
        }
        Ok(())
    }

    /// Reads the Mobility Database's CSV export.
    pub fn load_mdb(&mut self, csv: impl std::io::Read) -> Result<()> {
        let mut r = csv::ReaderBuilder::new().flexible(true).from_reader(csv);
        let headers: Vec<String> = r.headers()?.iter().map(str::to_string).collect();
        for rec in r.records() {
            let rec = rec?;
            let row: HashMap<String, String> = headers
                .iter()
                .cloned()
                .zip(rec.iter().map(str::to_string))
                .collect();
            if let Some(id) = row.get("id").filter(|id| !id.is_empty()) {
                self.mdb.insert(id.clone(), row.clone());
            }
        }
        Ok(())
    }
}

/// Reads the feeds of Transitous's region files.
pub fn load(transitous: &FeedFiles, catalogs: &Catalogs) -> Result<(Vec<Feed>, Vec<Skipped>)> {
    let mut feeds = Vec::new();
    let mut skipped = Vec::new();
    for (region, data) in &transitous.files {
        let doc: Value = serde_json::from_slice(data)
            .with_context(|| format!("parsing Transitous feeds/{region}.json"))?;
        for source in doc["sources"].as_array().into_iter().flatten() {
            let name = source["name"].as_str().unwrap_or("");
            let id = format!("{region}/{name}");
            match resolve(source, catalogs) {
                Ok(Some(mut feed)) => {
                    feed.id = id;
                    feeds.push(feed);
                }
                Ok(None) => {} // not static GTFS
                Err(reason) => skipped.push(Skipped { id, reason }),
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    feeds.retain(|f| {
        let new = seen.insert(f.id.clone());
        if !new {
            skipped.push(Skipped {
                id: f.id.clone(),
                reason: "duplicate name in region".into(),
            });
        }
        new
    });
    Ok((feeds, skipped))
}

const ENCRYPTED: &str = "AGE-ENCRYPTED:";

/// Headers that carry no credentials, which we may send.
const HARMLESS_HEADERS: &[&str] = &["accept-encoding", "accept"];

/// Turns a Transitous source into a feed; `Ok(None)` for sources that
/// are not static GTFS, `Err` with the reason for those we skip.
fn resolve(source: &Value, catalogs: &Catalogs) -> Result<Option<Feed>, String> {
    let spec = source["spec"].as_str().unwrap_or("gtfs");
    if spec != "gtfs" {
        return Ok(None);
    }
    if source["skip"].as_bool() == Some(true) {
        let why = source["skip-reason"].as_str().unwrap_or("no reason given");
        return Err(format!("skipped by Transitous: {why}"));
    }
    let options = &source["http-options"];
    if options["method"].as_str().is_some_and(|m| m != "GET") {
        return Err("needs a POST request".into());
    }
    for (name, _) in options["headers"].as_object().into_iter().flatten() {
        if !HARMLESS_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(format!("needs credentials in header {name}"));
        }
    }
    if source["api-key"].is_string() {
        return Err("needs an API key".into());
    }
    let mut license = license_of(&source["license"], "spdx-identifier", "attribution-text");
    let url_override = source["url-override"]
        .as_str()
        .filter(|u| !u.starts_with(ENCRYPTED));
    if source["url-override"]
        .as_str()
        .is_some_and(|u| u.starts_with(ENCRYPTED))
    {
        return Err("URL is encrypted".into());
    }
    let urls = match source["type"].as_str().unwrap_or("") {
        "http" | "url" => {
            let url = url_override.or(source["url"].as_str()).ok_or("no url")?;
            vec![url.to_string()]
        }
        "transitland-atlas" => {
            let id = source["transitland-atlas-id"]
                .as_str()
                .ok_or("no atlas id")?;
            let feed = catalogs
                .atlas
                .get(id)
                .ok_or_else(|| format!("{id} not in Transitland Atlas"))?;
            if !feed["authorization"].is_null() {
                return Err("needs authorization".into());
            }
            let urls = &feed["urls"];
            let url = url_override
                .or(urls["static_current"].as_str())
                .or(urls["static_historic"][0].as_str())
                .ok_or_else(|| format!("{id} has no static GTFS URL"))?;
            let atlas = license_of(&feed["license"], "spdx_identifier", "attribution_text");
            license = merge(license, atlas);
            vec![url.to_string()]
        }
        "mobility-database" => {
            let id = source["mdb-id"].as_str().ok_or("no mdb id")?;
            let row = catalogs
                .mdb
                .get(id)
                .ok_or_else(|| format!("{id} not in Mobility Database"))?;
            let get = |k: &str| row.get(k).map(String::as_str).filter(|v| !v.is_empty());
            if get("data_type").is_some_and(|t| t != "gtfs") {
                return Ok(None);
            }
            if get("urls.authentication_type").is_some_and(|t| t != "0") {
                return Err("needs authentication".into());
            }
            let mut urls: Vec<String> = Vec::new();
            if let Some(u) = url_override.or(get("urls.direct_download")) {
                urls.push(u.to_string());
            }
            // MobilityData's daily copy, for when the publisher is down.
            if let Some(u) = get("urls.latest") {
                urls.push(u.to_string());
            }
            if urls.is_empty() {
                return Err(format!("{id} has no download URL"));
            }
            license = merge(
                license,
                License {
                    spdx: get("license.spdx_identifier").map(str::to_string),
                    url: get("urls.license").map(str::to_string),
                    attribution: None,
                },
            );
            urls
        }
        "ftp" => return Err("FTP is not supported".into()),
        other => return Err(format!("unknown source type {other:?}")),
    };
    Ok(Some(Feed {
        id: String::new(),
        urls,
        license,
    }))
}

fn license_of(v: &Value, spdx_key: &str, attribution_key: &str) -> License {
    let s = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
    License {
        spdx: s(spdx_key),
        url: s("url"),
        attribution: s(attribution_key),
    }
}

/// What Transitous says wins; the catalog fills the gaps.
fn merge(a: License, b: License) -> License {
    License {
        spdx: a.spdx.or(b.spdx),
        url: a.url.or(b.url),
        attribution: a.attribution.or(b.attribution),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalogs() -> Catalogs {
        let mut c = Catalogs::default();
        c.atlas.insert(
            "f-u0-x".into(),
            json!({"id": "f-u0-x", "spec": "gtfs",
                   "urls": {"static_current": "https://x.example/gtfs.zip"},
                   "license": {"spdx_identifier": "CC-BY-4.0", "attribution_text": "X AG"}}),
        );
        c.atlas.insert(
            "f-u0-locked".into(),
            json!({"id": "f-u0-locked", "urls": {"static_current": "https://l.example/"},
                   "authorization": {"type": "basic_auth"}}),
        );
        let csv = "id,data_type,urls.direct_download,urls.latest,urls.license,urls.authentication_type\n\
                   mdb-1,gtfs,https://y.example/g.zip,https://mirror.example/1.zip,https://y.example/terms,0\n\
                   mdb-2,gtfs_rt,https://y.example/rt,,,0\n";
        c.load_mdb(csv.as_bytes()).unwrap();
        c
    }

    fn resolve_one(source: Value) -> Result<Option<Feed>, String> {
        resolve(&source, &catalogs())
    }

    #[test]
    fn direct_download() {
        let f = resolve_one(
            json!({"name": "a", "type": "http", "url": "https://a.example/g.zip",
            "license": {"spdx-identifier": "ODbL-1.0"}}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(f.urls, ["https://a.example/g.zip"]);
        assert!(f.license.is_open());
        // Transitous's "url" type with no spec is static GTFS too.
        let f = resolve_one(json!({"name": "b", "type": "url", "url": "https://b.example/"}));
        assert!(f.unwrap().is_some());
    }

    #[test]
    fn resolves_catalogs() {
        let f = resolve_one(
            json!({"name": "x", "type": "transitland-atlas", "transitland-atlas-id": "f-u0-x"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(f.urls, ["https://x.example/gtfs.zip"]);
        assert_eq!(f.license.spdx.as_deref(), Some("CC-BY-4.0"));
        assert_eq!(f.license.attribution.as_deref(), Some("X AG"));

        let f = resolve_one(json!({"name": "y", "type": "mobility-database", "mdb-id": "mdb-1"}))
            .unwrap()
            .unwrap();
        assert_eq!(
            f.urls,
            ["https://y.example/g.zip", "https://mirror.example/1.zip"]
        );
        assert_eq!(f.license.url.as_deref(), Some("https://y.example/terms"));
        assert!(!f.license.is_open(), "no SPDX id");

        let rt = resolve_one(json!({"name": "rt", "type": "mobility-database", "mdb-id": "mdb-2"}));
        assert_eq!(rt, Ok(None));
    }

    #[test]
    fn ignores_and_skips() {
        assert_eq!(
            resolve_one(json!({"type": "url", "spec": "gtfs-rt", "url": "https://rt/"})),
            Ok(None)
        );
        let skip = |s: Value| resolve_one(s).unwrap_err();
        assert!(
            skip(json!({"type": "http", "url": "u", "skip": true, "skip-reason": "broken"}))
                .contains("broken")
        );
        assert!(
            skip(json!({"type": "http", "url": "u", "http-options": {"headers": {"ApiKey": "k"}}}))
                .contains("credentials")
        );
        assert!(
            skip(json!({"type": "transitland-atlas", "transitland-atlas-id": "f-u0-locked"}))
                .contains("authorization")
        );
        assert!(
            skip(json!({"type": "transitland-atlas", "transitland-atlas-id": "f-gone"}))
                .contains("not in")
        );
        assert!(
            skip(json!({"type": "http", "url": "u", "url-override": "AGE-ENCRYPTED:abc"}))
                .contains("encrypted")
        );
        assert!(skip(json!({"type": "ftp", "url": "ftp://x"})).contains("FTP"));
        // Harmless headers are fine.
        let ok = resolve_one(
            json!({"type": "http", "url": "u", "http-options": {"headers": {"accept-encoding": "gzip"}}}),
        );
        assert!(ok.unwrap().is_some());
    }

    #[test]
    fn loads_region_files() {
        let ch = json!({"sources": [
            {"name": "sbb", "type": "http", "url": "https://sbb/"},
            {"name": "sbb", "type": "http", "url": "https://sbb2/"},
            {"name": "rt", "type": "url", "spec": "gtfs-rt", "url": "https://rt/"},
            {"name": "old", "type": "http", "url": "https://old/", "skip": true},
        ]});
        let files = FeedFiles {
            files: vec![("ch".into(), ch.to_string().into_bytes())],
            commit: None,
        };
        let (feeds, skipped) = load(&files, &Catalogs::default()).unwrap();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].id, "ch/sbb");
        let reasons: Vec<_> = skipped
            .iter()
            .map(|s| (s.id.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            reasons,
            [
                ("ch/old", "skipped by Transitous: no reason given"),
                ("ch/sbb", "duplicate name in region")
            ]
        );
    }
}
