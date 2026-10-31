// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Fetches feeds through the cache, and condenses them.
//!
//! The cache keeps everything under `transit/`, so other signals can
//! share it under their own prefixes. Every object has a fixed key:
//!
//! - `transit/<feed>/condensed.bin`: the condensed feed; see
//!   [`super::condensed`]. Its header records the condenser version
//!   and each file it was made from: URL, `ETag`, `Last-Modified`,
//!   SHA-256 and SHA-512. That header is the cache's record of the feed.
//! - `transit/<feed>/gtfs.zip`, or `cif.gz` for a CIF feed: the last
//!   download, so the feed can be condensed again when the method
//!   changes.
//! - `transit/shared/<name>/<file>` and `<name>.json`: inputs that are
//!   not feeds, such as the catalogs and the NaPTAN file that places
//!   CIF stations, with a small JSON file that records the same as a
//!   header would. Each is fetched once per run.
//! - `transit/index.json`: what each feed's header said at the end of
//!   the last run. A run makes its conditional requests from it, so a
//!   feed that changed never needs its stale `condensed.bin` loaded.
//!   It is only a hint: where it disagrees with a header, the header wins.
//!
//! A run asks the publisher whether the feed changed, with
//! `If-None-Match` and `If-Modified-Since`. If not, or if the download
//! has the same hash, it takes the condensed copy from the cache. If
//! the publisher cannot be reached, it tries the feed's mirrors, and
//! then falls back to the cached copy, however old. Only a feed that was
//! never fetched successfully is missing from the output.

use super::catalog::{Feed, Format};
use super::cif;
use super::condensed;
use super::dedup::Outline;
use super::gtfs;
use crate::http::HttpReader;
use crate::store::Store;
use anyhow::{Context, Result, anyhow, bail, ensure};
use ring::digest;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Key of the index that a run writes for the next one.
pub const INDEX_KEY: &str = "transit/index.json";

/// What the cache knows about a downloaded file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// The URL we asked for, from the feed list.
    pub url: String,
    /// Where that redirected to.
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub sha256: String,
    pub sha512: String,
    pub bytes: u64,
    /// When this content was first downloaded, in seconds since the epoch.
    pub fetched: i64,
    /// When the publisher last confirmed it, in seconds since the epoch.
    /// Condensed headers leave it out, so they need not be rewritten
    /// on every run.
    pub checked: i64,
}

impl Meta {
    pub fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "final_url": self.final_url,
            "etag": self.etag,
            "last_modified": self.last_modified,
            "sha256": self.sha256,
            "sha512": self.sha512,
            "bytes": self.bytes,
            "fetched": crate::date::rfc3339(self.fetched),
            "checked": crate::date::rfc3339(self.checked),
        })
    }

    pub fn from_json(v: &Value) -> Option<Meta> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        let t = |k: &str| v[k].as_str().and_then(parse_rfc3339);
        Some(Meta {
            url: s("url")?,
            final_url: s("final_url").unwrap_or_default(),
            etag: s("etag"),
            last_modified: s("last_modified"),
            sha256: s("sha256").filter(|h| h.len() == 64)?,
            sha512: s("sha512").filter(|h| h.len() == 128)?,
            bytes: v["bytes"].as_u64()?,
            fetched: t("fetched")?,
            checked: t("checked")?,
        })
    }

    /// What a condensed header records about this file.
    fn to_input(&self) -> condensed::Input {
        let mut sha256 = [0u8; 32];
        for (b, d) in sha256.iter_mut().zip(unhex(&self.sha256)) {
            *b = d;
        }
        condensed::Input {
            url: self.url.clone(),
            final_url: self.final_url.clone(),
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            sha256,
            sha512: unhex(&self.sha512),
            bytes: self.bytes,
            first_seen: self.fetched,
        }
    }

    /// The inverse of [`Meta::to_input`]. Headers do not record when
    /// the publisher last confirmed a file, so that is taken to be
    /// when it was first downloaded.
    fn from_input(i: &condensed::Input) -> Meta {
        Meta {
            url: i.url.clone(),
            final_url: i.final_url.clone(),
            etag: i.etag.clone(),
            last_modified: i.last_modified.clone(),
            sha256: hex(&i.sha256),
            sha512: hex(&i.sha512),
            bytes: i.bytes,
            fetched: i.first_seen,
            checked: i.first_seen,
        }
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

/// Reads the index the last run wrote, by feed id. A missing or
/// unreadable index gives an empty one, since it is only a hint.
pub fn read_index(store: &dyn Store) -> HashMap<String, Meta> {
    let parsed = store.get(INDEX_KEY).map(|data| {
        data.and_then(|d| serde_json::from_slice::<Value>(&d).ok())
            .and_then(|v| v["feeds"].as_object().cloned())
    });
    match parsed {
        Ok(Some(feeds)) => feeds
            .iter()
            .filter_map(|(id, v)| Some((id.clone(), Meta::from_json(v)?)))
            .collect(),
        Ok(None) => HashMap::new(),
        Err(e) => {
            eprintln!("not using {INDEX_KEY}: {e:#}");
            HashMap::new()
        }
    }
}

/// Writes the index for the next run: `previous`, updated with the
/// feeds of this run. Feeds this run did not fetch keep their entries,
/// so a run on some feeds only leaves the others' hints in place.
pub fn write_index<'f>(
    store: &dyn Store,
    previous: &'f HashMap<String, Meta>,
    feeds: impl IntoIterator<Item = (&'f str, &'f Meta)>,
) -> Result<()> {
    let mut all: BTreeMap<&str, &Meta> = previous.iter().map(|(k, v)| (k.as_str(), v)).collect();
    all.extend(feeds);
    let feeds: serde_json::Map<String, Value> = all
        .into_iter()
        .map(|(id, m)| (id.to_string(), m.to_json()))
        .collect();
    let mut text = serde_json::to_vec_pretty(&json!({ "feeds": feeds }))?;
    text.push(b'\n');
    store
        .put(INDEX_KEY, &text)
        .with_context(|| format!("writing {INDEX_KEY}"))
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

    /// Decodes the condensed feed and its stored weekly totals.
    pub fn feed_with_totals(&self) -> Result<(condensed::Feed, condensed::Totals)> {
        condensed::Feed::decode_with_totals(&self.data)
    }
}

/// A condensed feed and what a run needs to know of it up front.
struct Loaded {
    data: memmap2::Mmap,
    outline: Outline,
    last_day: i64,
    has_trips: bool,
}

/// A shared input, such as NaPTAN, as fetched in this run.
struct Shared {
    meta: Meta,
    status: Status,
    /// Its key in the cache.
    key: String,
    /// Where it goes on disk, when a feed needs it.
    path: PathBuf,
    /// Whether it is at `path` yet.
    on_disk: Mutex<bool>,
}

/// A shared input once fetched, or why that failed.
type SharedCell = Arc<OnceLock<Result<Arc<Shared>, String>>>;

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
    /// What the last run's index said, by feed id; see [`read_index`].
    pub index: HashMap<String, Meta>,
    /// Shared inputs by key prefix, each fetched at most once.
    shared: Mutex<HashMap<String, SharedCell>>,
}

/// A feed's id made safe as part of a key in any store, such as
/// `ch/opentransportdataswiss26`.
pub fn safe_id(feed_id: &str) -> String {
    let mut s = String::new();
    for (i, part) in feed_id.split('/').enumerate() {
        if i > 0 {
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

/// Object key prefix for a feed.
pub fn key_prefix(feed_id: &str) -> String {
    format!("transit/{}", safe_id(feed_id))
}

/// Object key prefix for a shared input, such as a catalog.
pub fn shared_prefix(name: &str) -> String {
    format!("transit/shared/{name}")
}

/// A file kept fresh through the cache, as `<prefix>/<file>`.
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

impl Source<'_> {
    fn key(&self) -> String {
        format!("{}/{}", self.prefix, self.file)
    }

    /// For a shared input: `<prefix>/<name>.json`, with `<name>` the
    /// last part of the prefix.
    fn sidecar_key(&self) -> String {
        let name = self.prefix.rsplit('/').next().unwrap_or(self.prefix);
        format!("{}/{name}.json", self.prefix)
    }
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

impl<'a> Fetcher<'a> {
    pub fn new(store: &'a dyn Store, tmp: &'a Path) -> Self {
        Fetcher {
            store,
            agent: crate::http::agent(),
            tmp,
            offline: false,
            now: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
            retry_delay: std::time::Duration::from_secs(2),
            index: HashMap::new(),
            shared: Mutex::default(),
        }
    }

    /// Fetches and condenses `feed`; fails only if there is neither a
    /// download nor a cached copy.
    pub fn fetch(&self, feed: &Feed) -> Result<Fetched> {
        if let Some(hint) = self.index.get(&feed.id)
            && let Some(f) = self.fetch_from(feed, Some(hint))?
        {
            return Ok(f);
        }
        self.fetch_from(feed, None)?
            .context("the cached feed changed during the run")
    }

    /// Like [`Fetcher::fetch`], asking the publisher with what `hint`
    /// says, or else with what the cached header says. Returns `None`
    /// if the hint turns out not to match the cache.
    fn fetch_from(&self, feed: &Feed, hint: Option<&Meta>) -> Result<Option<Fetched>> {
        let prefix = key_prefix(&feed.id);
        let (file, magic, kind, version): (&str, &[u8], &str, u8) = match feed.format {
            Format::Gtfs => ("gtfs.zip", b"PK", "zip file", gtfs::VERSION),
            Format::Cif => ("cif.gz", &[0x1f, 0x8b], "gzipped CIF file", cif::VERSION),
        };
        let src = Source {
            prefix: &prefix,
            file,
            urls: &feed.urls,
            magic,
            kind,
            auth: auth_header(feed),
        };
        let stations = match feed.format {
            Format::Gtfs => None,
            Format::Cif => Some(
                self.stations(feed)
                    .with_context(|| format!("fetching the stations of {}", feed.id))?,
            ),
        };
        let key = format!("{prefix}/condensed.bin");
        let mut stored = match hint {
            Some(_) => None,
            None => self.store.get(&key)?,
        };
        let cached = match hint {
            Some(h) => Some(h.clone()),
            None => stored
                .as_deref()
                .and_then(|d| condensed::decode_inputs(d).ok())
                .and_then(|(_, inputs)| inputs.first().map(Meta::from_input)),
        };
        let raw_path = self
            .tmp
            .join(format!("{}.{file}", prefix.replace('/', "~")));
        let _cleanup = Cleanup(&raw_path);
        let (meta, mut status, have_raw) = self.refresh(&src, cached.as_ref(), &raw_path)?;
        if let Some(st) = &stations
            && let Status::Stale { error } = &st.status
            && !matches!(status, Status::Stale { .. })
        {
            status = Status::Stale {
                error: format!("stations: {error}"),
            };
        }
        let mut inputs = vec![meta.to_input()];
        inputs.extend(stations.iter().map(|st| st.meta.to_input()));

        // If the hint says the feed changed, the cached copy is stale:
        // no need to load it.
        let changed = hint.is_some_and(|h| h.sha256 != meta.sha256);
        if hint.is_some() && !changed {
            stored = self.store.get(&key)?;
            let header = stored
                .as_deref()
                .and_then(|d| condensed::decode_inputs(d).ok());
            if let Some((_, h)) = header
                && h.first().map(|i| i.sha256) != Some(inputs[0].sha256)
            {
                return Ok(None);
            }
        }
        let stations_meta = stations.as_ref().map(|st| st.meta.clone());
        if let Some(data) = stored.filter(|_| !changed)
            && let Some(l) = self.reuse(&key, &data, version, &inputs)?
        {
            return Ok(Some(fetched(meta, status, stations_meta, l)));
        }

        if !have_raw {
            self.restore(&src.key(), &meta.sha256, &raw_path)?;
        }
        let mut full = match &stations {
            None => gtfs::condense_in(&raw_path, Some(self.tmp)),
            Some(st) => {
                self.shared_on_disk(st)?;
                let naptan = cif::Stations::from_naptan(std::io::BufReader::new(
                    std::fs::File::open(&st.path)?,
                ))
                .with_context(|| format!("reading the stations of {}", feed.id))?;
                ensure!(
                    !naptan.is_empty(),
                    "{}: no railway stations in {}",
                    feed.id,
                    st.meta.url
                );
                cif::condense_path(&raw_path, &naptan)
            }
        }
        .with_context(|| format!("condensing {}", feed.id))?;
        full.inputs = inputs;
        let data = full.encode()?;
        self.store.put(&key, &data)?;
        let l = self.load(&data, Some(&full))?;
        Ok(Some(fetched(meta, status, stations_meta, l)))
    }

    /// The cached condensed feed `data`, if made by condenser `version`
    /// from the files in `inputs`. If only their metadata changed, such
    /// as the `ETag`, it is stored again with the new one.
    fn reuse(
        &self,
        key: &str,
        data: &[u8],
        version: u8,
        inputs: &[condensed::Input],
    ) -> Result<Option<Loaded>> {
        let Ok((condenser, cached)) = condensed::decode_inputs(data) else {
            return Ok(None);
        };
        if condenser != u32::from(version)
            || cached.len() != inputs.len()
            || cached.iter().zip(inputs).any(|(c, i)| c.sha256 != i.sha256)
        {
            return Ok(None);
        }
        let Ok(mut feed) = condensed::Feed::decode(data) else {
            return Ok(None);
        };
        if cached == inputs {
            return Ok(Some(self.load(data, Some(&feed))?));
        }
        feed.inputs = inputs.to_vec();
        let data = feed.encode()?;
        self.store.put(key, &data)?;
        Ok(Some(self.load(&data, Some(&feed))?))
    }

    /// The NaPTAN file for a CIF feed, fetched once per run.
    fn stations(&self, feed: &Feed) -> Result<Arc<Shared>> {
        let stations = feed
            .stations
            .as_ref()
            .with_context(|| format!("{}: CIF feed without stations", feed.id))?;
        // All CIF feeds take their stations from NaPTAN, so there is
        // one cached copy.
        let prefix = shared_prefix("naptan");
        let cell = self
            .shared
            .lock()
            .unwrap()
            .entry(prefix.clone())
            .or_default()
            .clone();
        let result = cell.get_or_init(|| {
            let urls = [stations.url.clone()];
            let src = Source {
                prefix: &prefix,
                file: "naptan.csv",
                urls: &urls,
                magic: b"",
                kind: "CSV file",
                auth: None,
            };
            let path = self.tmp.join("shared~naptan.csv");
            self.refresh_shared(&src, &path)
                .map(|(meta, status, on_disk)| {
                    Arc::new(Shared {
                        meta,
                        status,
                        key: src.key(),
                        path,
                        on_disk: Mutex::new(on_disk),
                    })
                })
                .map_err(|e| format!("{e:#}"))
        });
        result.clone().map_err(|e| anyhow!(e))
    }

    /// Puts a shared input on disk, if no feed has needed it yet.
    fn shared_on_disk(&self, shared: &Shared) -> Result<()> {
        let mut on_disk = shared.on_disk.lock().unwrap();
        if !*on_disk {
            self.restore(&shared.key, &shared.meta.sha256, &shared.path)?;
            *on_disk = true;
        }
        Ok(())
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

    /// Fetches the shared input `src` into the file `dest`, from its
    /// publisher if it changed or else from the cache; fails only if
    /// there is neither.
    pub fn fetch_file(&self, src: &Source, dest: &Path) -> Result<(Meta, Status)> {
        let (meta, status, have_file) = self.refresh_shared(src, dest)?;
        if !have_file {
            self.restore(&src.key(), &meta.sha256, dest)?;
        }
        Ok((meta, status))
    }

    /// Like [`Fetcher::refresh`], for a shared input, whose metadata
    /// is in a JSON file next to it.
    fn refresh_shared(&self, src: &Source, dest: &Path) -> Result<(Meta, Status, bool)> {
        let sidecar = src.sidecar_key();
        let cached = self
            .store
            .get(&sidecar)?
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| Meta::from_json(&v));
        let (meta, status, have_file) = self.refresh(src, cached.as_ref(), dest)?;
        if !matches!(status, Status::Stale { .. }) {
            let mut text = serde_json::to_vec_pretty(&meta.to_json())?;
            text.push(b'\n');
            self.store.put(&sidecar, &text)?;
        }
        Ok((meta, status, have_file))
    }

    /// Asks the publisher for a newer version of `src` than `cached`,
    /// and stores the file in the cache if there is one. Returns what
    /// we now know of it, how we got it, and whether the file was
    /// downloaded to `dest`.
    fn refresh(
        &self,
        src: &Source,
        cached: Option<&Meta>,
        dest: &Path,
    ) -> Result<(Meta, Status, bool)> {
        match self.download(src, cached, dest) {
            Ok(Download::NotModified) => {
                let mut m = cached
                    .cloned()
                    .expect("conditional request needs a cached copy");
                m.checked = self.now;
                Ok((m, Status::Unchanged, false))
            }
            Ok(Download::Fetched(mut m)) => match cached {
                Some(old) if old.sha256 == m.sha256 => {
                    m.fetched = old.fetched;
                    Ok((m, Status::Unchanged, true))
                }
                _ => {
                    self.store.upload(&src.key(), dest)?;
                    Ok((m, Status::Changed, true))
                }
            },
            Err(e) => match cached {
                Some(m) => Ok((
                    m.clone(),
                    Status::Stale {
                        error: format!("{e:#}"),
                    },
                    false,
                )),
                None => Err(e),
            },
        }
    }

    /// Copies `key` from the cache to `dest`, checking its hash.
    fn restore(&self, key: &str, sha256: &str, dest: &Path) -> Result<()> {
        ensure!(
            self.store.download(key, dest)?,
            "{key} is missing from {}",
            self.store.describe()
        );
        let hashes = crate::bom::hash_file(dest)?;
        ensure!(
            hashes.sha256 == sha256,
            "{key} in the cache does not match the hash we recorded"
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
        let mut sha256 = digest::Context::new(&digest::SHA256);
        let mut sha512 = digest::Context::new(&digest::SHA512);
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
            sha256.update(&buf[..n]);
            sha512.update(&buf[..n]);
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
            sha256: hex(sha256.finish().as_ref()),
            sha512: hex(sha512.finish().as_ref()),
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

/// Decodes lowercase hex; bad digits become zero.
fn unhex(h: &str) -> Vec<u8> {
    (0..h.len() / 2)
        .map(|i| {
            h.get(2 * i..2 * i + 2)
                .and_then(|d| u8::from_str_radix(d, 16).ok())
                .unwrap_or(0)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefixes() {
        assert_eq!(
            key_prefix("ch/opentransportdataswiss26"),
            "transit/ch/opentransportdataswiss26"
        );
        assert_eq!(
            key_prefix("jp/京王 バス"),
            "transit/jp/%E4%BA%AC%E7%8E%8B%20%E3%83%90%E3%82%B9"
        );
        assert_eq!(key_prefix("x/.."), "transit/x/.._");
        assert_eq!(key_prefix("x/"), "transit/x/_");
        assert_eq!(key_prefix("/x"), "transit/_/x");
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
            sha512: "cd".repeat(64),
            bytes: 42,
            fetched: 1_780_000_000,
            checked: 1_780_086_400,
        };
        assert_eq!(Meta::from_json(&m.to_json()), Some(m.clone()));
        let input = Meta::from_input(&m.to_input());
        assert_eq!(input.checked, m.fetched, "headers leave out checked");
        assert_eq!(
            Meta {
                checked: m.checked,
                ..input
            },
            m
        );
        assert_eq!(parse_rfc3339("2026-06-09T12:34:56Z"), Some(1_781_008_496));
        assert_eq!(parse_rfc3339("2026-06-09"), None);
    }
}
