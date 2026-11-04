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
//!
//! After uploading, old releases are deleted from the signal's folder.
//! It keeps the files of the [`KEEP_RELEASES`] newest release dates,
//! plus any release dated within the last [`KEEP_DAYS`] days, plus the
//! release just uploaded. A file's release date is the `YYYY-MM-DD`
//! part of its name, such as in `osm-node-density.2026-10-19.tiff`.
//! Files without a date, such as the STAC item, are kept, except for
//! `datapackage.json` and `collection.json`, which earlier versions of
//! geosignals wrote and which pointed to files that are now deleted.
//! If deleting fails, the run says so but still succeeds, since the
//! release is out; the next run tries again.

use crate::store::{S3, parse_s3_url};
use anyhow::{Context, Result, ensure};
use std::path::Path;

const ENV_PREFIX: &str = "PUBLIC_S3_";

/// How many of the newest releases to keep in public storage.
pub const KEEP_RELEASES: usize = 3;

/// Releases dated within this many days are kept too.
pub const KEEP_DAYS: i64 = 7;

/// Undated files that earlier versions wrote, deleted on sight.
const LEFTOVERS: [&str; 2] = ["datapackage.json", "collection.json"];

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
        let current: Vec<&str> = files.iter().map(|(f, _)| f.as_str()).collect();
        if let Err(e) = self.prune(name, &current) {
            eprintln!("warning: could not delete old releases of {name}: {e:#}");
        }
        Ok(uploaded)
    }

    /// Deletes the old releases in the folder `name`, keeping `current`
    /// and what [`stale`] keeps. Returns the deleted keys.
    fn prune(&self, name: &str, current: &[&str]) -> Result<Vec<String>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        let keys = self.s3.list(name)?;
        let prefix = format!("{name}/");
        let files: Vec<&str> = keys
            .iter()
            .filter_map(|k| k.strip_prefix(&prefix))
            .collect();
        let doomed = stale(&files, current, now);
        for file in &doomed {
            let key = format!("{prefix}{file}");
            self.s3
                .delete(&key)
                .with_context(|| format!("deleting {key}"))?;
            eprintln!("deleted {}", self.s3.describe_key(&key));
        }
        Ok(doomed.into_iter().map(|f| format!("{prefix}{f}")).collect())
    }
}

/// Which of `files`, the names in a signal's folder, to delete at
/// `now` (seconds since the epoch), when `current` was just uploaded.
fn stale(files: &[&str], current: &[&str], now: i64) -> Vec<String> {
    // The listing may lag behind the upload, so the current release
    // counts among the newest even if it is not listed yet.
    let mut dates: Vec<&str> = files
        .iter()
        .chain(current)
        .filter_map(|f| release_date(f))
        .collect();
    dates.sort_unstable();
    dates.dedup();
    let (y, m, d) = crate::date::civil_date(now.div_euclid(86_400) - KEEP_DAYS);
    let cutoff = format!("{y:04}-{m:02}-{d:02}");
    let newest = &dates[dates.len().saturating_sub(KEEP_RELEASES)..];
    let keep = |date: &str| {
        date >= cutoff.as_str()
            || newest.contains(&date)
            || current.iter().any(|c| release_date(c) == Some(date))
    };
    files
        .iter()
        .filter(|f| !current.contains(f))
        .filter(|f| match release_date(f) {
            Some(date) => !keep(date),
            None => LEFTOVERS.contains(f),
        })
        .map(|f| f.to_string())
        .collect()
}

/// The `YYYY-MM-DD` part of a file name such as
/// `ebird-observers.2026-10-01.cc-by-4.0.tiff`, if it has one.
fn release_date(file: &str) -> Option<&str> {
    if file.contains('/') {
        return None;
    }
    file.split('.').find(|part| {
        let b = part.as_bytes();
        b.len() == 10
            && b.iter().enumerate().all(|(i, c)| {
                if i == 4 || i == 7 {
                    *c == b'-'
                } else {
                    c.is_ascii_digit()
                }
            })
    })
}

/// True for a plain file name in the same directory.
fn is_file_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-04T12:00:00Z.
    const NOW: i64 = 1_793_793_600;

    fn release(date: &str) -> Vec<String> {
        vec![
            format!("x.{date}.tiff"),
            format!("x.{date}.cdx.json"),
            format!("x-tagged.{date}.cc-by-4.0.tiff"),
        ]
    }

    #[test]
    fn keeps_three_newest_and_last_week() {
        assert_eq!(crate::date::rfc3339(NOW), "2026-11-04T12:00:00Z");
        let mut owned = Vec::new();
        for d in [
            "2026-09-01",
            "2026-10-01",
            "2026-10-08",
            "2026-10-15",
            "2026-10-22",
            "2026-10-28",
            "2026-11-01",
            "2026-11-04",
        ] {
            owned.extend(release(d));
        }
        owned.extend([
            "x.stac.json".into(),
            "datapackage.json".into(),
            "collection.json".into(),
        ]);
        owned.push("sub/x.2020-01-01.tiff".into());
        let files: Vec<&str> = owned.iter().map(String::as_str).collect();
        let current = ["x.2026-11-04.tiff", "x.2026-11-04.cdx.json", "x.stac.json"];
        let mut got = stale(&files, &current, NOW);
        got.sort();
        let mut want: Vec<String> = [
            "2026-09-01",
            "2026-10-01",
            "2026-10-08",
            "2026-10-15",
            "2026-10-22",
        ]
        .iter()
        .flat_map(|d| release(d))
        .collect();
        want.extend(["collection.json".into(), "datapackage.json".into()]);
        want.sort();
        // 2026-10-28 is exactly a week old, so it stays.
        assert_eq!(got, want);
    }

    #[test]
    fn keeps_three_newest_when_all_are_old() {
        let mut owned = Vec::new();
        for d in ["2024-01-01", "2024-02-01", "2024-03-01", "2024-04-01"] {
            owned.extend(release(d));
        }
        let files: Vec<&str> = owned.iter().map(String::as_str).collect();
        let got = stale(&files, &[], NOW);
        assert_eq!(got, release("2024-01-01"));
    }

    #[test]
    fn never_deletes_the_current_release() {
        // A rerun on an old input, such as Nominatim's 2024 raster,
        // while newer releases exist.
        let mut owned = Vec::new();
        for d in ["2024-05-04", "2025-01-01", "2025-02-01", "2025-03-01"] {
            owned.extend(release(d));
        }
        let files: Vec<&str> = owned.iter().map(String::as_str).collect();
        let got = stale(&files, &["x.2024-05-04.tiff"], NOW);
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn finds_release_dates() {
        assert_eq!(release_date("x.2026-10-01.tiff"), Some("2026-10-01"));
        assert_eq!(
            release_date("ebird-observers.2026-10-01.cc-by-4.0.tiff"),
            Some("2026-10-01")
        );
        assert_eq!(release_date("x.stac.json"), None);
        assert_eq!(release_date("x.2026-1x-01.tiff"), None);
        assert_eq!(release_date("a/x.2026-10-01.tiff"), None);
    }
}
