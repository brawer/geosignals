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
//! Each signal goes to its own folder named after it, such as
//! `s3://bucket/prefix/osm-node-density/`, mirroring its output
//! directory. The dated GeoTIFFs and data BOMs go first, and the
//! undated STAC item that points to them goes last, so a client that
//! fetches it never finds a link to a file that is not there yet.

use crate::store::{S3, parse_s3_url};
use anyhow::{Context, Result, ensure};
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

    /// Uploads the release that the STAC item at `item`, such as
    /// `osm-node-density.stac.json`, describes: the files it points to,
    /// then the item itself. Returns where they went.
    pub fn publish(&self, item: &Path) -> Result<Vec<String>> {
        let dir = item.parent().unwrap_or(Path::new("."));
        let item_name = item
            .file_name()
            .context("STAC item has no file name")?
            .to_string_lossy()
            .into_owned();
        let name = item_name
            .strip_suffix(".stac.json")
            .with_context(|| format!("{item_name} is no STAC item"))?;
        ensure!(is_file_name(name), "bad signal name {name:?}");
        let mut files = crate::stac::asset_files(item)?;
        files.push((item_name.clone(), crate::stac::ITEM_MEDIA_TYPE.into()));
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

/// True for a plain file name in the same directory.
fn is_file_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\'])
}
