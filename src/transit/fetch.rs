// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Fetches feeds through the cache, and condenses them.
//!
//! For each feed, the cache holds three objects under `gtfs/<feed>/`:
//!
//! - `meta.json`: where the feed was last downloaded from, its `ETag`,
//!   `Last-Modified` and SHA-256, and when it was fetched and checked;
//! - `feed.zip`: the feed as downloaded, so it can be condensed again
//!   when the method changes;
//! - `condensed-v<N>-<sha256 prefix>.bin`: its median week; see
//!   [`super::gtfs`].
//!
//! A run asks the publisher whether the feed changed, with
//! `If-None-Match` and `If-Modified-Since`. If not, or if the download
//! has the same hash, it takes the condensed copy from the cache. If
//! the publisher cannot be reached, it tries the feed's mirrors, and
//! then falls back to the cached copy, however old. Only a feed that was
//! never fetched successfully is missing from the output.

use super::catalog::Feed;
use super::gtfs::{self, Condensed};
use crate::http::HttpReader;
use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use ring::digest;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::Path;

/// What the cache knows about a feed's last download.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// The URL we asked for, from the feed list.
    pub url: String,
    /// Where that redirected to.
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256: String,
    pub bytes: u64,
    /// When this content was first downloaded, in seconds since the epoch.
    pub fetched: i64,
    /// When the publisher last confirmed it, in seconds since the epoch.
    pub checked: i64,
}

impl Meta {
    fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "final_url": self.final_url,
            "etag": self.etag,
            "last_modified": self.last_modified,
            "sha256": self.sha256,
            "bytes": self.bytes,
            "fetched": crate::date::rfc3339(self.fetched),
            "checked": crate::date::rfc3339(self.checked),
        })
    }

    fn from_json(v: &Value) -> Option<Meta> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        let t = |k: &str| v[k].as_str().and_then(parse_rfc3339);
        Some(Meta {
            url: s("url")?,
            final_url: s("final_url").unwrap_or_default(),
            etag: s("etag"),
            last_modified: s("last_modified"),
            sha256: s("sha256").filter(|h| h.len() == 64)?,
            bytes: v["bytes"].as_u64()?,
            fetched: t("fetched")?,
            checked: t("checked")?,
        })
    }
}

/// Parses what [`crate::date::rfc3339`] writes.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let days = crate::date::days_from_civil(n(0..4)?, n(5..7)? as u32, n(8..10)? as u32);
    Some(days * 86_400 + n(11..13)? * 3600 + n(14..16)? * 60 + n(17..19)?)
}

/// How a feed was obtained in this run.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Downloaded, and new or changed.
    Changed,
    /// The publisher confirmed that the cached copy is current.
    Unchanged,
    /// The publisher could not be reached; this is the cached copy.
    Stale { error: String },
}

pub struct Fetched {
    pub meta: Meta,
    pub status: Status,
    pub condensed: Condensed,
}

pub struct Fetcher<'a> {
    pub store: &'a dyn Store,
    pub agent: ureq::Agent,
    /// Directory for downloads.
    pub tmp: &'a Path,
    /// Use only the cache; contact no publisher.
    pub offline: bool,
    /// Current time, in seconds since the epoch.
    pub now: i64,
    /// Pause before retrying a failed download; quadrupled each time.
    pub retry_delay: std::time::Duration,
}

/// Object key prefix for a feed, with its id made safe for any store.
pub fn key_prefix(feed_id: &str) -> String {
    let mut s = String::from("gtfs/");
    for part in feed_id.split('/') {
        if !s.ends_with('/') {
            s.push('/');
        }
        for c in part.chars() {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                s.push(c);
            } else {
                for b in c.to_string().bytes() {
                    s.push_str(&format!("%{b:02X}"));
                }
            }
        }
        if part.is_empty() || part.chars().all(|c| c == '.') {
            s.push('_');
        }
    }
    s
}

impl Fetcher<'_> {
    /// Fetches and condenses `feed`; fails only if there is neither a
    /// download nor a cached copy.
    pub fn fetch(&self, feed: &Feed) -> Result<Fetched> {
        let prefix = key_prefix(&feed.id);
        let meta_key = format!("{prefix}/meta.json");
        let zip_key = format!("{prefix}/feed.zip");
        let cached: Option<Meta> = self
            .store
            .get(&meta_key)?
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| Meta::from_json(&v));

        let zip_path = self.tmp.join(format!("{}.zip", prefix.replace('/', "~")));
        let _cleanup = Cleanup(&zip_path);
        let mut have_zip = false;
        let (meta, status) = match self.download(feed, cached.as_ref(), &zip_path) {
            Ok(Download::NotModified) => {
                let mut m = cached
                    .clone()
                    .expect("conditional request needs a cached copy");
                m.checked = self.now;
                (m, Status::Unchanged)
            }
            Ok(Download::Fetched(mut m)) => {
                have_zip = true;
                match &cached {
                    Some(old) if old.sha256 == m.sha256 => {
                        m.fetched = old.fetched;
                        (m, Status::Unchanged)
                    }
                    _ => {
                        self.store.upload(&zip_key, &zip_path)?;
                        (m, Status::Changed)
                    }
                }
            }
            Err(e) => match cached {
                Some(m) => (
                    m,
                    Status::Stale {
                        error: format!("{e:#}"),
                    },
                ),
                None => return Err(e),
            },
        };
        if !matches!(status, Status::Stale { .. }) {
            let mut text = serde_json::to_vec_pretty(&meta.to_json())?;
            text.push(b'\n');
            self.store.put(&meta_key, &text)?;
        }

        let condensed_key = format!(
            "{prefix}/condensed-v{}-{}.bin",
            gtfs::VERSION,
            &meta.sha256[..16]
        );
        if let Some(data) = self.store.get(&condensed_key)?
            && let Ok(c) = Condensed::decode(&data)
        {
            return Ok(Fetched {
                meta,
                status,
                condensed: c,
            });
        }
        if !have_zip {
            ensure!(
                self.store.download(&zip_key, &zip_path)?,
                "{zip_key} is missing from {}",
                self.store.describe()
            );
            let sha = sha256_file(&zip_path)?;
            ensure!(
                sha == meta.sha256,
                "{zip_key} in the cache does not match its meta.json"
            );
        }
        let condensed =
            gtfs::condense(&zip_path).with_context(|| format!("condensing {}", feed.id))?;
        self.store.put(&condensed_key, &condensed.encode()?)?;
        Ok(Fetched {
            meta,
            status,
            condensed,
        })
    }

    /// Tries the feed's URLs in turn.
    fn download(&self, feed: &Feed, cached: Option<&Meta>, dest: &Path) -> Result<Download> {
        if self.offline {
            bail!("offline");
        }
        let mut errors = Vec::new();
        for url in &feed.urls {
            let previous = cached.filter(|m| &m.url == url);
            let mut backoff = self.retry_delay;
            for attempt in 1..=DOWNLOAD_ATTEMPTS {
                match self.download_one(url, previous, dest) {
                    Ok(d) => return Ok(d),
                    Err(e) if attempt < DOWNLOAD_ATTEMPTS && is_transient(&e) => {
                        std::thread::sleep(backoff);
                        backoff *= 4;
                    }
                    Err(e) => {
                        errors.push(format!("{e:#}"));
                        break;
                    }
                }
            }
        }
        bail!("{}", errors.join("; "))
    }

    fn download_one(&self, url: &str, previous: Option<&Meta>, dest: &Path) -> Result<Download> {
        let Some(mut reader) = HttpReader::open_if_changed(
            self.agent.clone(),
            url,
            previous.and_then(|m| m.etag.as_deref()),
            previous.and_then(|m| m.last_modified.as_deref()),
        )?
        else {
            ensure!(
                previous.is_some(),
                "{url}: 304 Not Modified without a cached copy"
            );
            return Ok(Download::NotModified);
        };
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?,
        );
        let mut sha = digest::Context::new(&digest::SHA256);
        let mut buf = vec![0u8; 1 << 16];
        let mut bytes = 0u64;
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| format!("reading {url}"))?;
            if n == 0 {
                break;
            }
            if bytes == 0 && !buf[..n].starts_with(b"PK") {
                bail!("{url} is not a zip file");
            }
            sha.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            bytes += n as u64;
        }
        out.flush()?;
        ensure!(bytes > 0, "{url} is empty");
        Ok(Download::Fetched(Meta {
            url: url.to_string(),
            final_url: reader.url().to_string(),
            etag: reader.etag().map(str::to_string),
            last_modified: reader.last_modified().map(str::to_string),
            sha256: hex(sha.finish().as_ref()),
            bytes,
            fetched: self.now,
            checked: self.now,
        }))
    }
}

/// Tries per URL, for errors that may go away.
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// Whether retrying might help: network trouble and server errors,
/// but not "not found" or a file that is not a zip.
fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(code)) => *code >= 500 || *code == 429 || *code == 408,
        Some(_) => true,
        None => c.downcast_ref::<std::io::Error>().is_some(),
    })
}

enum Download {
    NotModified,
    Fetched(Meta),
}

/// Removes a file when dropped.
struct Cleanup<'a>(&'a Path);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut sha = digest::Context::new(&digest::SHA256);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(sha.finish().as_ref()));
        }
        sha.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefixes() {
        assert_eq!(
            key_prefix("ch/opentransportdataswiss26"),
            "gtfs/ch/opentransportdataswiss26"
        );
        assert_eq!(
            key_prefix("jp/京王 バス"),
            "gtfs/jp/%E4%BA%AC%E7%8E%8B%20%E3%83%90%E3%82%B9"
        );
        assert_eq!(key_prefix("x/.."), "gtfs/x/.._");
        assert_eq!(key_prefix("x/"), "gtfs/x/_");
    }

    #[test]
    fn meta_round_trip() {
        let m = Meta {
            url: "https://a/".into(),
            final_url: "https://b/".into(),
            etag: Some("\"v1\"".into()),
            last_modified: None,
            sha256: "ab".repeat(32),
            bytes: 42,
            fetched: 1_780_000_000,
            checked: 1_780_086_400,
        };
        assert_eq!(Meta::from_json(&m.to_json()), Some(m));
        assert_eq!(parse_rfc3339("2026-06-09T12:34:56Z"), Some(1_781_008_496));
        assert_eq!(parse_rfc3339("2026-06-09"), None);
    }
}
