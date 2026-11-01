// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Uploads each release to public S3-compatible storage, such as
//! [Source Cooperative](https://source.coop).
//!
//! The step is optional. It runs when `PUBLIC_S3_URL` is set to
//! `s3://bucket/prefix`, with credentials in `PUBLIC_S3_ACCESS_KEY_ID`,
//! `PUBLIC_S3_SECRET_ACCESS_KEY`, `PUBLIC_S3_ENDPOINT_URL` and
//! `PUBLIC_S3_REGION` (default `auto`), like the `CACHE_S3_*` variables
//! of [`crate::store`]. When no `PUBLIC_S3_*` variable is set, nothing is
//! uploaded. When some are set but others are missing, the run fails
//! before it starts, rather than computing for hours and then finding
//! it can't publish.
//!
//! Each signal goes to its own folder named after its package, such as
//! `s3://bucket/prefix/osm-node-density/`, mirroring its output
//! directory. The dated files go first and the files with fixed names,
//! `datapackage.json` and `collection.json`, go last, so a client that
//! fetches them never finds a link to a file that is not there yet.

use crate::store::{S3, parse_s3_url};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;

const ENV_PREFIX: &str = "PUBLIC_S3_";

pub struct Publisher {
    s3: S3,
}

impl Publisher {
    /// Reads the `PUBLIC_S3_*` variables, or returns `None` if none is set.
    pub fn from_env() -> Result<Option<Publisher>> {
        let configured = std::env::vars_os()
            .any(|(k, v)| k.to_str().is_some_and(|k| k.starts_with(ENV_PREFIX)) && !v.is_empty());
        if !configured {
            return Ok(None);
        }
        let url = std::env::var(format!("{ENV_PREFIX}URL"))
            .ok()
            .filter(|u| !u.is_empty())
            .context("PUBLIC_S3_URL is not set, but other PUBLIC_S3_ variables are")?;
        let (bucket, prefix) = parse_s3_url(&url)?;
        let s3 = S3::from_env(ENV_PREFIX, bucket, prefix)?;
        Ok(Some(Publisher { s3 }))
    }

    /// Uploads the release that `datapackage` (a `datapackage.json`)
    /// describes: its resources, its STAC item, the descriptor itself
    /// and the STAC collection. Returns where they went.
    pub fn publish(&self, datapackage: &Path) -> Result<Vec<String>> {
        let dir = datapackage.parent().unwrap_or(Path::new("."));
        let dp: Value = serde_json::from_slice(
            &std::fs::read(datapackage)
                .with_context(|| format!("reading {}", datapackage.display()))?,
        )
        .with_context(|| format!("parsing {}", datapackage.display()))?;
        let files = files(&dp)?;
        let name = dp["name"].as_str().unwrap_or_default();
        let start = std::time::Instant::now();
        let mut uploaded = Vec::new();
        for (file, media_type) in &files {
            let key = format!("{name}/{file}");
            self.s3
                .upload_as(&key, &dir.join(file), Some(media_type))
                .with_context(|| format!("uploading {file}"))?;
            let dest = self.s3.describe_key(&key);
            eprintln!("uploaded {file} to {dest}");
            uploaded.push(dest);
        }
        eprintln!(
            "published {} files in {:.0?}",
            uploaded.len(),
            start.elapsed()
        );
        Ok(uploaded)
    }
}

/// The files of the release described by `dp`, in upload order, with
/// their media types.
fn files(dp: &Value) -> Result<Vec<(String, String)>> {
    let name = dp["name"].as_str().context("datapackage has no name")?;
    let version = dp["version"]
        .as_str()
        .context("datapackage has no version")?;
    ensure!(is_file_name(name), "bad package name {name:?}");
    let mut files = Vec::new();
    for r in dp["resources"].as_array().into_iter().flatten() {
        let path = r["path"].as_str().context("resource without path")?;
        ensure!(
            is_file_name(path),
            "resource path {path:?} is not a file name"
        );
        let media_type = r["mediatype"]
            .as_str()
            .unwrap_or("application/octet-stream");
        files.push((path.to_string(), media_type.to_string()));
    }
    let item = crate::stac::item_file_name(name, version);
    ensure!(is_file_name(&item), "bad STAC item name {item:?}");
    files.push((item, "application/geo+json".into()));
    files.push((
        crate::datapackage::FILE_NAME.into(),
        "application/json".into(),
    ));
    files.push((
        crate::stac::COLLECTION_FILE_NAME.into(),
        "application/json".into(),
    ));
    Ok(files)
}

/// True for a plain file name in the same directory.
fn is_file_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lists_dated_files_first() {
        let dp = json!({
            "name": "osm-node-density",
            "version": "2026-10-12",
            "resources": [
                {"path": "osm-node-density.2026-10-12.tiff", "mediatype": "image/tiff"},
                {"path": "osm-node-density.2026-10-12.cdx.json", "mediatype": "application/vnd.cyclonedx+json"},
            ],
        });
        let names: Vec<String> = files(&dp).unwrap().into_iter().map(|(f, _)| f).collect();
        assert_eq!(
            names,
            [
                "osm-node-density.2026-10-12.tiff",
                "osm-node-density.2026-10-12.cdx.json",
                "osm-node-density.2026-10-12.stac.json",
                "datapackage.json",
                "collection.json",
            ]
        );
    }

    #[test]
    fn rejects_paths_outside_the_directory() {
        let dp = json!({
            "name": "x",
            "version": "2026-10-12",
            "resources": [{"path": "../secret"}],
        });
        assert!(files(&dp).is_err());
    }
}
