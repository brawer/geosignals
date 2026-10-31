// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `transit-departures` pipeline: how often public transport
//! departs, per day and km², from the GTFS feeds that Transitous lists
//! and the feeds in `checked.json`, such as Network Rail's CIF timetable.
//!
//! 1. [`catalog`] reads the feed list and drops feeds whose license we
//!    cannot republish.
//! 2. [`fetch`] downloads each feed through the cache, or takes the
//!    cached copy, and [`gtfs`] or [`cif`] condenses it; see [`condensed`].
//! 3. [`dedup`] drops trips that a larger feed also has.
//! 4. [`history`] adds each feed's departures per pixel and week to
//!    what earlier runs saw, and averages the past 12 months. Each
//!    pixel gets the departures on an average day, summed over feeds.
//!
//! Besides the GeoTIFF, its data BOM and `datapackage.json`, a run
//! writes a report with each feed's status and the duplicates found.

pub mod catalog;
pub mod cif;
pub mod condensed;
pub mod dedup;
pub mod fetch;
pub mod gtfs;
pub mod history;
pub mod repo;

use crate::cog::{Grid, Metadata, Spool};
use crate::histogram::Histogram;
use crate::tile::{MAX_TILE_ZOOM, TILE_SIZE};
use crate::workdir::Workdir;
use anyhow::{Context, Result, ensure};
use catalog::{Catalogs, Feed};
use fetch::{Fetched, Fetcher, Status};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

const PACKAGE: crate::datapackage::Package = crate::datapackage::Package {
    name: "transit-departures",
    // UUIDv5(URL, "https://github.com/brawer/osmsignals#transit-departures").
    // Frozen: catalogs key on it, so it keeps the repo's old name.
    id: "48622a5a-879b-5594-ad49-8776f47d43c4",
    title: "Public transport departures",
    description: "How many public transport departures there are per day and km² \
        of Earth surface, for every zoom-18 Web Mercator tile, as a Cloud-Optimized \
        GeoTIFF with overviews. Counted from GTFS feeds as the average day of the past \
        12 months, with trips that several feeds contain counted once. Pixel values encode \
        log2(departures per day per km²); 0 means no departures.",
    grid: crate::cog::Grid::WEB_MERCATOR,
    nodata: None,
};

/// The Mobility Database's export of its catalog.
pub const MOBILITY_DATABASE_CSV: &str = "https://files.mobilitydatabase.org/feeds_v2.csv";

/// The Transitous repository, whose `feeds/` list the feeds.
pub const TRANSITOUS: &str = "public-transport/transitous";

/// The Transitland Atlas repository.
pub const TRANSITLAND_ATLAS: &str = "transitland/transitland-atlas";

/// Tarballs of the catalogs' main branches, as `repo::github_tarball` makes them.
pub const TRANSITOUS_URL: &str =
    "https://github.com/public-transport/transitous/archive/refs/heads/main.tar.gz";
pub const TRANSITLAND_ATLAS_URL: &str =
    "https://github.com/transitland/transitland-atlas/archive/refs/heads/main.tar.gz";

/// Which feeds to use, by license.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Licenses {
    /// Only feeds under a license on [`catalog::OPEN_LICENSES`].
    Open,
    /// All feeds, including those without a known license. The output
    /// may then not be redistributable.
    Any,
}

pub struct Config {
    /// Transitous: the URL of a tarball of the repository, such as
    /// [`repo::github_tarball`] makes, a local tarball, or a checkout.
    pub transitous: String,
    /// The Transitland Atlas, for sources that point into it; given
    /// like `transitous`.
    pub transitland_atlas: Option<String>,
    /// The Mobility Database export, as a URL or file, for sources
    /// that point into it.
    pub mobility_database: Option<String>,
    /// The cache: a directory or `s3://bucket/prefix`.
    pub cache: String,
    pub output_dir: PathBuf,
    pub workdir: PathBuf,
    pub threads: usize,
    /// The day the output is for, as seconds since the epoch.
    pub date: i64,
    pub licenses: Licenses,
    /// Use only cached feeds.
    pub offline: bool,
    /// Only feeds whose id starts with one of these, such as `ch/`.
    pub only: Vec<String>,
    /// Drop feeds whose service ended more than this many days before `date`.
    pub max_expired_days: i64,
    /// Identifies this run in its scheduler, for the data BOM.
    pub run_id: String,
    /// Pause before retrying a failed download; quadrupled each time.
    pub retry_delay: std::time::Duration,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub output: PathBuf,
    pub bom: PathBuf,
    pub datapackage: PathBuf,
    pub report: PathBuf,
    /// Feeds in the output.
    pub feeds: usize,
    /// Of those, feeds taken from the cache since the publisher failed.
    pub stale: usize,
    /// Feeds without any data, and feeds whose service has ended.
    pub failed: usize,
    pub expired: usize,
    /// Departures on an average day of the past 12 months, without
    /// duplicates, summed over the feeds.
    pub departures: f64,
    /// Duplicates dropped, on an average day of the feeds' current
    /// timetables.
    pub duplicates: f64,
    pub pixels: u64,
}

pub fn run(cfg: &Config) -> Result<Summary> {
    let start = Instant::now();
    let workdir = Workdir::open(&cfg.workdir, "transit-departures")?;
    std::fs::create_dir_all(&cfg.output_dir)
        .with_context(|| format!("creating {}", cfg.output_dir.display()))?;
    let day = cfg.date.div_euclid(86_400);
    let timestamp = day * 86_400;

    let store = crate::store::open(&cfg.cache)?;
    let fetcher = Fetcher {
        store: store.as_ref(),
        agent: crate::http::agent(),
        tmp: workdir.tmp(),
        offline: cfg.offline,
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64,
        retry_delay: cfg.retry_delay,
    };
    let mut inputs = Vec::new();
    let mut catalogs = Catalogs::default();
    let (transitous, input) = read_catalog(&fetcher, "transitous", &cfg.transitous)?;
    let Data::Repo(transitous) = transitous else {
        anyhow::bail!("{} is not a repository", cfg.transitous);
    };
    inputs.push(input);
    if let Some(location) = &cfg.transitland_atlas {
        let (data, input) = read_catalog(&fetcher, "transitland-atlas", location)?;
        let Data::Repo(files) = data else {
            anyhow::bail!("{location} is not a repository");
        };
        catalogs.load_atlas(&files)?;
        inputs.push(input);
    }
    if let Some(location) = &cfg.mobility_database {
        let (data, input) = read_catalog(&fetcher, "mobility-database", location)?;
        let Data::Csv(path) = data else {
            anyhow::bail!("{location} is not a CSV file");
        };
        let f = std::fs::File::open(&path)?;
        catalogs
            .load_mdb(std::io::BufReader::new(f))
            .with_context(|| format!("reading {location}"))?;
        inputs.push(input);
    }
    let (feeds, mut skipped) = catalog::load(&transitous, &catalogs)?;
    let mut selected: Vec<Feed> = Vec::new();
    for f in feeds {
        if !cfg.only.is_empty() && !cfg.only.iter().any(|p| f.id.starts_with(p.as_str())) {
            continue;
        }
        if cfg.licenses == Licenses::Open && !f.license.is_open() {
            let what = f.license.spdx.clone().unwrap_or_else(|| "unknown".into());
            skipped.push(catalog::Skipped {
                id: f.id,
                reason: format!("license {what} is not on the open list"),
            });
            continue;
        }
        if let Some(prefix) = &f.credentials
            && !cfg.offline
            && fetch::credentials(prefix).is_none()
        {
            skipped.push(catalog::Skipped {
                id: f.id,
                reason: format!("needs {prefix}_USERNAME and {prefix}_PASSWORD"),
            });
            continue;
        }
        selected.push(f);
    }
    eprintln!(
        "{} feeds to read, {} skipped, cache {}",
        selected.len(),
        skipped.len(),
        cfg.cache
    );

    let results = fetch_all(&fetcher, &selected, cfg.threads);
    eprintln!(
        "fetched {} feeds in {:.0?}",
        selected.len(),
        start.elapsed()
    );

    let mut summary = Summary::default();
    let mut report_feeds = Vec::new();
    let mut used: Vec<(&Feed, Fetched)> = Vec::new();
    for (feed, result) in selected.iter().zip(results) {
        match result {
            Err(e) => {
                summary.failed += 1;
                report_feeds
                    .push(json!({"id": feed.id, "status": "failed", "error": format!("{e:#}")}));
            }
            Ok(f) if !f.has_trips || f.last_day < day - cfg.max_expired_days => {
                summary.expired += 1;
                report_feeds.push(json!({
                    "id": feed.id,
                    "status": "expired",
                    "last_day": day_string(f.last_day),
                }));
            }
            Ok(f) => used.push((feed, f)),
        }
    }
    ensure!(!used.is_empty(), "no feed could be read");

    let outlines: Vec<&dedup::Outline> = used.iter().map(|(_, f)| &f.outline).collect();
    let (dropped, overlaps) = dedup::find(
        &outlines,
        |i| {
            used[i]
                .1
                .feed()
                .with_context(|| format!("decoding {}", used[i].0.id))
        },
        cfg.threads,
    )?;
    eprintln!(
        "compared overlapping feeds, found {} with duplicates, in {:.0?}",
        overlaps.len(),
        start.elapsed()
    );

    // Each feed's average day over the past 12 months, from its history
    // and its current calendar without duplicates.
    let next = std::sync::atomic::AtomicUsize::new(0);
    let averaged: Vec<Result<(usize, Averaged)>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..cfg.threads.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((feed, f)) = used.get(i) else {
                            return out;
                        };
                        out.push(
                            average(fetcher.store, feed, f, &dropped[i], day)
                                .with_context(|| format!("averaging {}", feed.id))
                                .map(|a| (i, a)),
                        );
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("averaging worker panicked"))
            .collect()
    });
    let mut averaged: Vec<(usize, Averaged)> = averaged.into_iter().collect::<Result<_>>()?;
    averaged.sort_unstable_by_key(|(i, _)| *i);

    let mut counts: HashMap<u64, f64> = HashMap::new();
    for ((feed, f), (_, a)) in used.iter().zip(&averaged) {
        let mut departures = 0.0;
        for &(key, n) in &a.pixels {
            *counts.entry(key).or_default() += n;
            departures += n;
        }
        summary.departures += departures;
        summary.duplicates += a.duplicates_per_day;
        let mut entry = json!({
            "id": feed.id,
            "status": status_name(&f.status),
            "url": public_url(feed, &f.meta),
            "fetched": crate::date::rfc3339(f.meta.fetched),
            "last_day": day_string(f.last_day),
            "weeks": a.weeks,
            "departures_per_day": round2(departures),
            "duplicate_departures_per_day": round2(a.duplicates_per_day),
        });
        if let Some(spdx) = &feed.license.spdx {
            entry["license"] = json!(spdx);
        } else if let Some(p) = &feed.license.permission {
            entry["permission"] = json!(p);
        }
        if let Some(e) = &feed.license.evidence {
            entry["evidence"] = json!(e);
        }
        if let Status::Stale { error } = &f.status {
            entry["error"] = json!(error);
            summary.stale += 1;
        }
        report_feeds.push(entry);
    }
    drop(averaged);
    summary.feeds = used.len();
    // Thousandths of a departure per day, so seasonal service shows.
    let mut pixels: Vec<(u64, u64)> = counts
        .into_iter()
        .map(|(k, n)| (k, (n * 1000.0).round() as u64))
        .filter(|&(_, n)| n > 0)
        .collect();
    pixels.sort_unstable();
    summary.pixels = pixels.len() as u64;
    eprintln!(
        "{:.0} departures on an average day from {} feeds ({:.0} duplicates dropped) in {} pixels, {:.0?}",
        summary.departures,
        summary.feeds,
        summary.duplicates,
        summary.pixels,
        start.elapsed()
    );

    let mut spool = Spool::new(workdir.tmp(), Grid::WEB_MERCATOR)?;
    let mut hist = crate::raster::build(pixels, cfg.threads, &mut spool, |z, y, counts| {
        crate::encode::pixels_scaled(z, y, counts, 1.0 / 1000.0)
    })?;
    let full_tiles = spool.tiles_per_zoom[MAX_TILE_ZOOM as usize];
    let empty_tiles = (1u64 << (2 * MAX_TILE_ZOOM)) - full_tiles;
    hist.add_value(0, empty_tiles * (TILE_SIZE * TILE_SIZE) as u64);

    let output = cfg.output_dir.join(output_name(timestamp)?);
    let copyright = format!(
        "Public transport data from {} GTFS feeds; see the data BOM for their sources and licenses",
        used.len()
    );
    let meta = Metadata {
        description: "Public transport departures".into(),
        software: format!("geosignals {}", env!("CARGO_PKG_VERSION")),
        copyright: copyright.clone(),
        min_value: hist.range().0,
        max_value: hist.range().1,
        gdal_metadata: gdal_metadata(used.len(), &hist),
        nodata: None,
    };
    spool.write_cog(&output, &meta)?;
    let hashes = crate::bom::hash_file(&output)?;

    let mut sources: Vec<crate::bom::Source> = inputs.iter().map(|i| i.bom_source()).collect();
    for (feed, f) in &used {
        sources.push(crate::bom::Source {
            name: fetch::key_prefix(&feed.id),
            version: f.meta.fetched,
            url: Some(public_url(feed, &f.meta)).filter(|u| !u.is_empty()),
            license_id: feed.license.spdx.clone(),
            license_url: feed.license.url.clone(),
            copyright: feed.license.attribution.clone(),
            sha256: Some(f.meta.sha256.clone()),
            evidence: feed.license.evidence.clone(),
        });
        if let (Some(st), Some(meta)) = (&feed.stations, &f.stations) {
            sources.push(crate::bom::Source {
                name: format!("{}/stations", fetch::key_prefix(&feed.id)),
                version: meta.fetched,
                url: Some(st.url.clone()),
                license_id: st.license.spdx.clone(),
                license_url: st.license.url.clone(),
                copyright: st.license.attribution.clone(),
                sha256: Some(meta.sha256.clone()),
                evidence: None,
            });
        }
    }
    summary.bom = crate::bom::write_sources(
        &crate::bom::SourcesRun {
            sources: &sources,
            timestamp,
            output: &output,
            description: "Public transport departures",
            run_id: &cfg.run_id,
            copyright: &copyright,
            license: &crate::bom::ODBL,
        },
        &hashes,
    )?;

    summary.report = output.with_extension("report.json");
    let report = json!({
        "date": day_string(day),
        "catalogs": inputs.iter().map(|i| i.report()).collect::<Vec<_>>(),
        "feeds": report_feeds,
        "skipped": skipped.iter().map(|s| json!({"id": s.id, "reason": s.reason})).collect::<Vec<_>>(),
        "duplicates": overlaps.iter().map(|o| json!({
            "feed": used[o.smaller].0.id,
            "also_in": used[o.larger].0.id,
            "shared_days": o.days,
            "departures_per_week": o.departures * 7 / u64::from(o.days.max(1)),
            "by_agency": o.by_agency.iter().map(|(a, n)| json!({"agency": a, "departures_per_week": n * 7 / u64::from(o.days.max(1))})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    write_json(&summary.report, &report)?;

    // Written last, so it never points to files that are not there yet.
    summary.datapackage = crate::datapackage::write_many(
        &PACKAGE,
        &crate::datapackage::ManyFiles {
            sources: sources
                .iter()
                .map(|src| {
                    let mut s = json!({
                        "title": src.name.strip_prefix("gtfs/").unwrap_or(&src.name),
                        "version": crate::date::rfc3339(src.version),
                    });
                    if let Some(url) = src.url.as_ref().filter(|u| !u.is_empty()) {
                        s["path"] = json!(url);
                    }
                    s
                })
                .collect(),
            noun: "GTFS feeds",
            timestamp,
            tiff: &output,
            tiff_hashes: &hashes,
            bom: &summary.bom,
        },
    )?;
    summary.output = output;
    eprintln!(
        "wrote {}, {}, {} and {} in {:.0?}",
        summary.output.display(),
        summary.bom.display(),
        summary.report.display(),
        summary.datapackage.display(),
        start.elapsed()
    );
    Ok(summary)
}

/// What a catalog gave, after reading.
enum Data {
    Repo(repo::FeedFiles),
    /// A CSV file in the working directory.
    Csv(PathBuf),
}

/// Which version of a catalog a run read, for the BOM and the report.
struct CatalogInput {
    name: &'static str,
    location: String,
    /// For a download: what the cache knows, and how we got it.
    fetched: Option<(fetch::Meta, Status)>,
    commit: Option<String>,
    /// For a local file: when it was last modified, in seconds since the epoch.
    modified: i64,
}

impl CatalogInput {
    fn bom_source(&self) -> crate::bom::Source {
        let (license_id, license_url, copyright) = match self.name {
            "transitous" => (Some("CC0-1.0"), None, None),
            "transitland-atlas" => (
                Some("CC-BY-4.0"),
                None,
                Some("Transitland Atlas, https://www.transit.land"),
            ),
            _ => (None, None, None),
        };
        let mut url = self.location.clone();
        if let Some(c) = &self.commit
            && let Some(repo) = url
                .strip_prefix("https://github.com/")
                .and_then(|r| r.split("/archive/").next())
                .map(str::to_string)
        {
            url = format!("https://github.com/{repo}/tree/{c}");
        }
        crate::bom::Source {
            name: format!("catalogs/{}", self.name),
            version: self
                .fetched
                .as_ref()
                .map_or(self.modified, |(m, _)| m.fetched),
            url: Some(url),
            license_id: license_id.map(str::to_string),
            license_url,
            copyright: copyright.map(str::to_string),
            sha256: self.fetched.as_ref().map(|(m, _)| m.sha256.clone()),
            evidence: None,
        }
    }

    fn report(&self) -> Value {
        let mut v = json!({"name": self.name, "location": self.location, "commit": self.commit});
        if let Some((meta, status)) = &self.fetched {
            v["fetched"] = json!(crate::date::rfc3339(meta.fetched));
            v["sha256"] = json!(meta.sha256);
            v["status"] = json!(status_name(status));
            if let Status::Stale { error } = status {
                v["error"] = json!(error);
            }
        }
        v
    }
}

/// Where a feed came from, for the BOM and the report. For a feed
/// behind a login, that is the URL we asked for: it redirects to a
/// signed, short-lived URL that is no use to anyone else.
fn public_url(feed: &Feed, meta: &fetch::Meta) -> String {
    if feed.credentials.is_some() {
        meta.url.clone()
    } else {
        meta.final_url.clone()
    }
}

fn status_name(s: &Status) -> &'static str {
    match s {
        Status::Changed => "changed",
        Status::Unchanged => "unchanged",
        Status::Stale { .. } => "stale",
    }
}

/// Reads a catalog from a URL (through the cache), a tarball or CSV
/// file, or a repository checkout.
fn read_catalog(
    fetcher: &Fetcher,
    name: &'static str,
    location: &str,
) -> Result<(Data, CatalogInput)> {
    let csv = name == "mobility-database";
    let mut input = CatalogInput {
        name,
        location: location.to_string(),
        fetched: None,
        commit: None,
        modified: 0,
    };
    let path = if location.starts_with("https://") || location.starts_with("http://") {
        let file = if csv { "feeds.csv" } else { "archive.tar.gz" };
        let path = fetcher.tmp.join(format!("catalog-{name}-{file}"));
        let prefix = format!("catalogs/{name}");
        let urls = [location.to_string()];
        let src = fetch::Source {
            prefix: &prefix,
            file,
            urls: &urls,
            magic: if csv { b"" } else { &[0x1f, 0x8b] },
            kind: if csv { "CSV file" } else { "gzipped tarball" },
            auth: None,
        };
        let (meta, status) = fetcher
            .fetch_file(&src, &path)
            .with_context(|| format!("fetching the {name} catalog"))?;
        if let Status::Stale { error } = &status {
            eprintln!("{name}: using cached copy: {error}");
        }
        input.fetched = Some((meta, status));
        path
    } else {
        let path = PathBuf::from(location);
        input.modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64);
        path
    };
    if csv {
        return Ok((Data::Csv(path), input));
    }
    let files = if path.is_dir() {
        repo::from_dir(&path)?
    } else {
        let f =
            std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        repo::from_tar_gz(std::io::BufReader::new(f))
            .with_context(|| format!("reading {location}"))?
    };
    anyhow::ensure!(!files.files.is_empty(), "no feeds/*.json in {location}");
    input.commit = files.commit.clone();
    eprintln!(
        "{name}: {} feed files{}",
        files.files.len(),
        files
            .commit
            .as_deref()
            .map(|c| format!(" at commit {c}"))
            .unwrap_or_default()
    );
    Ok((Data::Repo(files), input))
}

/// Fetches all feeds on `threads` threads; results are in feed order.
fn fetch_all(fetcher: &Fetcher, feeds: &[Feed], threads: usize) -> Vec<Result<Fetched>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<(usize, Result<Fetched>)> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(feed) = feeds.get(i) else {
                            return out;
                        };
                        let result = fetcher.fetch(feed);
                        match &result {
                            Ok(f) => {
                                if let Status::Stale { error } = &f.status {
                                    eprintln!("{}: using cached copy: {error}", feed.id);
                                }
                            }
                            Err(e) => eprintln!("{}: {e:#}", feed.id),
                        }
                        let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if n.is_multiple_of(100) {
                            eprintln!("fetched {n} of {} feeds", feeds.len());
                        }
                        out.push((i, result));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("fetch worker panicked"))
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

fn day_string(day: i64) -> String {
    crate::date::rfc3339(day * 86_400)[..10].to_string()
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

/// File name for data as of `timestamp`, such as
/// `transit-departures.2026-10-26.tiff`. The date is in UTC.
pub fn output_name(timestamp: i64) -> Result<String> {
    crate::date::dated_name(PACKAGE.name, timestamp)
}

/// Band description and scale for GDAL, so that tools like QGIS show
/// log2(departures per day per km²), plus the value histogram.
fn gdal_metadata(feeds: usize, hist: &Histogram) -> String {
    let scale = 1.0 / crate::encode::STEPS_PER_OCTAVE;
    let offset = crate::encode::LOG2_MIN_DENSITY - scale;
    format!(
        concat!(
            "<GDALMetadata>",
            r#"<Item name="FEEDS">{feeds}</Item>"#,
            r#"<Item name="ENCODING">v = 0: no departures; otherwise log2(departures per day per km²) = {scale} * v - {neg_offset}</Item>"#,
            r#"<Item name="DESCRIPTION" sample="0" role="description">log2(public transport departures per day per km²)</Item>"#,
            r#"<Item name="SCALE" sample="0" role="scale">{scale}</Item>"#,
            r#"<Item name="OFFSET" sample="0" role="offset">{offset}</Item>"#,
            "{rat}",
            "</GDALMetadata>"
        ),
        feeds = feeds,
        scale = scale,
        offset = offset,
        neg_offset = -offset,
        rat = hist.rat_xml(),
    )
}

/// What averaging one feed gives.
struct Averaged {
    /// Departures on an average day of the past 12 months, by pixel key.
    pixels: Vec<(u64, f64)>,
    /// Weeks that counted.
    weeks: usize,
    /// Duplicates dropped, on an average day of the current timetable.
    duplicates_per_day: f64,
}

/// Adds a feed's current timetable, without its duplicates, to its
/// history in the cache, and averages the past 12 months.
fn average(
    store: &dyn crate::store::Store,
    feed: &Feed,
    f: &Fetched,
    dropped: &dedup::Dropped,
    day: i64,
) -> Result<Averaged> {
    let key = format!("{}/history.bin", fetch::key_prefix(&feed.id));
    let mut history = match store.get(&key)? {
        None => history::History::new(day),
        Some(data) => history::History::decode(&data).unwrap_or_else(|e| {
            eprintln!("{}: starting a new history, since {key}: {e:#}", feed.id);
            history::History::new(day)
        }),
    };
    let full = f.feed()?;
    let all = full.totals();
    let without = full.totals_without(dropped);
    let duplicates: u64 = all.values.iter().sum::<u64>() - without.values.iter().sum::<u64>();
    let before = history.clone();
    history.update(&full, &without, day);
    if history != before {
        store.put(&key, &history.encode()?)?;
    }
    let (pixels, weeks) = history.average(day);
    Ok(Averaged {
        pixels,
        weeks,
        duplicates_per_day: duplicates as f64 / f64::from(full.days.max(1)),
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_names() {
        assert_eq!(
            output_name(1_782_000_000).unwrap(),
            "transit-departures.2026-06-21.tiff"
        );
    }

    #[test]
    fn package_id_is_derived_as_documented() {
        let url_ns = [
            0x6b, 0xa7, 0xb8, 0x11, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        assert_eq!(
            crate::bom::uuid_v5(
                &url_ns,
                b"https://github.com/brawer/osmsignals#transit-departures"
            ),
            format!("urn:uuid:{}", PACKAGE.id)
        );
    }
}
