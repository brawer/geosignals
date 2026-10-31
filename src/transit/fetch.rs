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
//! - `condensed-v<N>-<sha256 prefix>.bin`: the condensed feed; see
//!   [`super::condensed`].
//!
//! A CIF feed keeps `feed.cif.gz` instead of `feed.zip`, the NaPTAN
//! file that places its stations under `stations/`, and its condensed
//! form as `condensed-cif-v<N>-<feed sha256 prefix>-<stations sha256
//! prefix>.bin`; see [`super::cif`].
//!
//! A run asks the publisher whether the feed changed, with
//! `If-None-Match` and `If-Modified-Since`. If not, or if the download
//! has the same hash, it takes the condensed copy from the cache. If
//! the publisher cannot be reached, it tries the feed's mirrors, and
//! then falls back to the cached copy, however old. Only a feed that was
//! never fetched successfully is missing from the output.
//!
//! The catalogs that list the feeds are cached the same way, under
//! `catalogs/<name>/`, so a run works even when GitHub is down.

use super::catalog::{Feed, Format};
use super::cif;
use super::condensed;
use super::dedup::Outline;
use super::gtfs;
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
    /// For CIF: the NaPTAN file that placed the stations.
    pub stations: Option<Meta>,
    /// The condensed feed, encoded, in a memory-mapped scratch file so
    /// that thousands of them stay off the heap; see [`Fetched::feed`].
    pub data: memmap2::Mmap,
    pub outline: Outline,
    /// Last day with service, as the feed claims it.
    pub last_day: i64,
    /// Whether any trip runs.
    pub has_trips: bool,
}

impl Fetched {
    /// Decodes the condensed feed.
    pub fn feed(&self) -> Result<condensed::Feed> {
        condensed::Feed::decode(&self.data)
    }
}

/// A condensed feed and what a run needs to know of it up front.
struct Loaded {
    data: memmap2::Mmap,
    outline: Outline,
    last_day: i64,
    has_trips: bool,
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

/// A file kept fresh through the cache: `<prefix>/<file>` and its
/// `<prefix>/meta.json`.
pub struct Source<'a> {
    pub prefix: &'a str,
    pub file: &'a str,
    /// Where to download it, best first.
    pub urls: &'a [String],
    /// What the file starts with, such as `PK` for a zip.
    pub magic: &'a [u8],
    /// What to call it in errors, such as "zip file".
    pub kind: &'a str,
    /// `Authorization` header for the download, if it needs one.
    pub auth: Option<String>,
}

/// The user name and password in `<prefix>_USERNAME` and
/// `<prefix>_PASSWORD`, if both are set and not empty.
pub fn credentials(prefix: &str) -> Option<(String, String)> {
    let var = |k: &str| {
        std::env::var(format!("{prefix}_{k}"))
            .ok()
            .filter(|v| !v.is_empty())
    };
    Some((var("USERNAME")?, var("PASSWORD")?))
}

/// The `Authorization` header for HTTP basic authentication.
fn basic_auth(user: &str, password: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let data = format!("{user}:{password}");
    let mut out = String::from("Basic ");
    for chunk in data.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Fetcher<'_> {
    /// Fetches and condenses `feed`; fails only if there is neither a
    /// download nor a cached copy.
    pub fn fetch(&self, feed: &Feed) -> Result<Fetched> {
        if feed.format == Format::Cif {
            return self.fetch_cif(feed);
        }
        let prefix = key_prefix(&feed.id);
        let src = Source {
            prefix: &prefix,
            file: "feed.zip",
            urls: &feed.urls,
            magic: b"PK",
            kind: "zip file",
            auth: auth_header(feed),
        };
        let zip_path = self.tmp.join(format!("{}.zip", prefix.replace('/', "~")));
        let _cleanup = Cleanup(&zip_path);
        let (meta, status, have_zip) = self.refresh(&src, &zip_path)?;

        let condensed_key = format!(
            "{prefix}/condensed-v{}-{}.bin",
            gtfs::VERSION,
            &meta.sha256[..16]
        );
        if let Some(data) = self.store.get(&condensed_key)?
            && let Ok(l) = self.load(&data, None)
        {
            return Ok(fetched(meta, status, None, l));
        }
        if !have_zip {
            self.restore_cached(&src, &meta, &zip_path)?;
        }
        let mut full = gtfs::condense_in(&zip_path, Some(self.tmp))
            .with_context(|| format!("condensing {}", feed.id))?;
        full.inputs = vec![input(&meta)];
        let data = full.encode()?;
        self.store.put(&condensed_key, &data)?;
        let l = self.load(&data, Some(&full))?;
        Ok(fetched(meta, status, None, l))
    }

    /// Like [`Fetcher::fetch`], for a CIF feed and its stations. The
    /// condensed copy depends on both, so either changing makes a new one.
    fn fetch_cif(&self, feed: &Feed) -> Result<Fetched> {
        let prefix = key_prefix(&feed.id);
        let src = Source {
            prefix: &prefix,
            file: "feed.cif.gz",
            urls: &feed.urls,
            magic: &[0x1f, 0x8b],
            kind: "gzipped CIF file",
            auth: auth_header(feed),
        };
        let stations = feed
            .stations
            .as_ref()
            .with_context(|| format!("{}: CIF feed without stations", feed.id))?;
        let stations_prefix = format!("{prefix}/stations");
        let stations_urls = [stations.url.clone()];
        let stations_src = Source {
            prefix: &stations_prefix,
            file: "naptan.csv",
            urls: &stations_urls,
            magic: b"",
            kind: "CSV file",
            auth: None,
        };
        let tmp_name = prefix.replace('/', "~");
        let cif_path = self.tmp.join(format!("{tmp_name}.cif.gz"));
        let csv_path = self.tmp.join(format!("{tmp_name}.naptan.csv"));
        let (_c1, _c2) = (Cleanup(&cif_path), Cleanup(&csv_path));
        let (meta, mut status, have_cif) = self.refresh(&src, &cif_path)?;
        let (st_meta, st_status, have_csv) = self
            .refresh(&stations_src, &csv_path)
            .with_context(|| format!("fetching the stations of {}", feed.id))?;
        if let Status::Stale { error } = &st_status
            && !matches!(status, Status::Stale { .. })
        {
            status = Status::Stale {
                error: format!("stations: {error}"),
            };
        }

        let condensed_key = format!(
            "{prefix}/condensed-cif-v{}-{}-{}.bin",
            cif::VERSION,
            &meta.sha256[..16],
            &st_meta.sha256[..16]
        );
        if let Some(data) = self.store.get(&condensed_key)?
            && let Ok(l) = self.load(&data, None)
        {
            return Ok(fetched(meta, status, Some(st_meta), l));
        }
        if !have_cif {
            self.restore_cached(&src, &meta, &cif_path)?;
        }
        if !have_csv {
            self.restore_cached(&stations_src, &st_meta, &csv_path)?;
        }
        let naptan =
            cif::Stations::from_naptan(std::io::BufReader::new(std::fs::File::open(&csv_path)?))
                .with_context(|| format!("reading the stations of {}", feed.id))?;
        ensure!(
            !naptan.is_empty(),
            "{}: no railway stations in {}",
            feed.id,
            stations.url
        );
        let mut full = cif::condense_path(&cif_path, &naptan)
            .with_context(|| format!("condensing {}", feed.id))?;
        full.inputs = vec![input(&meta), input(&st_meta)];
        let data = full.encode()?;
        self.store.put(&condensed_key, &data)?;
        let l = self.load(&data, Some(&full))?;
        Ok(fetched(meta, status, Some(st_meta), l))
    }

    /// Checks an encoded condensed feed, `feed` if already decoded, and
    /// moves it to a scratch file.
    fn load(&self, data: &[u8], feed: Option<&condensed::Feed>) -> Result<Loaded> {
        let decoded;
        let feed = match feed {
            Some(f) => f,
            None => {
                decoded = condensed::Feed::decode(data)?;
                &decoded
            }
        };
        let mut file = tempfile::tempfile_in(self.tmp)?;
        file.write_all(data)?;
        // SAFETY: the file is ours alone and unlinked; nothing changes it.
        let data = unsafe { memmap2::Mmap::map(&file)? };
        Ok(Loaded {
            data,
            outline: Outline::new(feed),
            last_day: feed.last_day,
            has_trips: !feed.trips.is_empty() || !feed.frequencies.is_empty(),
        })
    }

    /// Fetches `src` into the file `dest`, from its publisher if it
    /// changed or else from the cache; fails only if there is neither.
    pub fn fetch_file(&self, src: &Source, dest: &Path) -> Result<(Meta, Status)> {
        let (meta, status, have_file) = self.refresh(src, dest)?;
        if !have_file {
            self.restore_cached(src, &meta, dest)?;
        }
        Ok((meta, status))
    }

    /// Asks the publisher for a newer version of `src`, and stores it in
    /// the cache if there is one. Returns what the cache now knows, how
    /// we got it, and whether the file was downloaded to `dest`.
    fn refresh(&self, src: &Source, dest: &Path) -> Result<(Meta, Status, bool)> {
        let meta_key = format!("{}/meta.json", src.prefix);
        let file_key = format!("{}/{}", src.prefix, src.file);
        let cached: Option<Meta> = self
            .store
            .get(&meta_key)?
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| Meta::from_json(&v));
        let mut downloaded = false;
        let (meta, status) = match self.download(src, cached.as_ref(), dest) {
            Ok(Download::NotModified) => {
                let mut m = cached
                    .clone()
                    .expect("conditional request needs a cached copy");
                m.checked = self.now;
                (m, Status::Unchanged)
            }
            Ok(Download::Fetched(mut m)) => {
                downloaded = true;
                match &cached {
                    Some(old) if old.sha256 == m.sha256 => {
                        m.fetched = old.fetched;
                        (m, Status::Unchanged)
                    }
                    _ => {
                        self.store.upload(&file_key, dest)?;
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
        Ok((meta, status, downloaded))
    }

    /// Copies the cached file of `src` to `dest`, checking its hash.
    fn restore_cached(&self, src: &Source, meta: &Meta, dest: &Path) -> Result<()> {
        let key = format!("{}/{}", src.prefix, src.file);
        ensure!(
            self.store.download(&key, dest)?,
            "{key} is missing from {}",
            self.store.describe()
        );
        ensure!(
            sha256_file(dest)? == meta.sha256,
            "{key} in the cache does not match its meta.json"
        );
        Ok(())
    }

    /// Tries the URLs of `src` in turn.
    fn download(&self, src: &Source, cached: Option<&Meta>, dest: &Path) -> Result<Download> {
        if self.offline {
            bail!("offline");
        }
        let mut errors = Vec::new();
        for url in src.urls {
            let previous = cached.filter(|m| &m.url == url);
            let mut backoff = self.retry_delay;
            for attempt in 1..=DOWNLOAD_ATTEMPTS {
                match self.download_one(src, url, previous, dest) {
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

    fn download_one(
        &self,
        src: &Source,
        url: &str,
        previous: Option<&Meta>,
        dest: &Path,
    ) -> Result<Download> {
        let Some(mut reader) = HttpReader::open_if_changed(
            self.agent.clone(),
            url,
            previous.and_then(|m| m.etag.as_deref()),
            previous.and_then(|m| m.last_modified.as_deref()),
            src.auth.as_deref(),
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
            if bytes == 0 && !buf[..n].starts_with(src.magic) {
                bail!("{url} is not a {}", src.kind);
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

/// The `Authorization` header for `feed`, if it needs credentials and
/// they are set.
fn auth_header(feed: &Feed) -> Option<String> {
    let (user, password) = credentials(feed.credentials.as_deref()?)?;
    Some(basic_auth(&user, &password))
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

fn fetched(meta: Meta, status: Status, stations: Option<Meta>, l: Loaded) -> Fetched {
    Fetched {
        meta,
        status,
        stations,
        data: l.data,
        outline: l.outline,
        last_day: l.last_day,
        has_trips: l.has_trips,
    }
}

/// What a condensed feed records about a file it was made from.
fn input(meta: &Meta) -> condensed::Input {
    let mut sha256 = [0u8; 32];
    for (i, b) in sha256.iter_mut().enumerate() {
        *b = meta
            .sha256
            .get(2 * i..2 * i + 2)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .unwrap_or(0);
    }
    condensed::Input {
        url: meta.url.clone(),
        etag: meta.etag.clone(),
        last_modified: meta.last_modified.clone(),
        sha256,
        sha512: Vec::new(),
        first_seen: meta.fetched,
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
    fn basic_auth_header() {
        // RFC 7617's example.
        assert_eq!(
            basic_auth("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(basic_auth("a", ""), "Basic YTo=");
        assert_eq!(basic_auth("ab", "c"), "Basic YWI6Yw==");
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
