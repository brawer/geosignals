// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `naturalist-observers` pipeline: how many different people
//! reported wildlife from each place, per km², from the iNaturalist and
//! eBird observations in GBIF's monthly occurrence snapshot.
//!
//! 1. [`snapshot`] lists the Parquet files of the snapshot and reads
//!    their `datasetkey` column; for row groups that hold one of our
//!    datasets, it also reads license, location, uncertainty and
//!    observer.
//! 2. Each record that is precise enough becomes an (observer, zoom-16
//!    pixel) pair in the layers its license allows. Observers get small
//!    numbers per dataset, in whatever order threads meet them; that
//!    order does not change any count.
//! 3. [`distinct`] counts distinct observers per pixel at every zoom.
//!
//! A run writes one GeoTIFF per [`LAYERS`] entry, each with its own
//! data BOM and license, and one `datapackage.json` for them all.

pub mod distinct;
pub mod snapshot;

use crate::bom::{self, License};
use crate::cog::{Crs, Grid, Metadata, Spool};
use crate::histogram::Histogram;
use crate::tile::TILE_SIZE;
use crate::workdir::Workdir;
use anyhow::{Context, Result, ensure};
use distinct::MAX_TILE_ZOOM;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

const PACKAGE: crate::datapackage::Package = crate::datapackage::Package {
    name: "naturalist-observers",
    // UUIDv5(URL, "https://github.com/brawer/osmsignals#naturalist-observers").
    // Frozen: catalogs key on it, so it keeps the repo's old name.
    id: "27f5f48c-05ed-5d0f-bd8f-39f008bf7075",
    title: "Naturalist observers",
    description: "How many different people reported wildlife observations per km² \
        of Earth surface, for every zoom-16 Web Mercator tile (about 611 m at the \
        equator), from the iNaturalist and eBird datasets in GBIF's occurrence \
        snapshot. One Cloud-Optimized GeoTIFF with overviews per dataset and \
        license; each overview pixel counts the distinct observers of its whole \
        area. Pixel values encode log2(observers per km²); 0 means none.",
};

/// A GBIF dataset we read.
#[derive(Debug, PartialEq, Eq)]
pub struct Dataset {
    /// GBIF's dataset key.
    pub key: &'static str,
    /// Short name, used in file names.
    pub name: &'static str,
    pub title: &'static str,
    pub doi: &'static str,
    /// Attribution, as GBIF's citation for the dataset gives it.
    pub attribution: &'static str,
}

pub const INATURALIST: Dataset = Dataset {
    key: "50c9509d-22c7-4a22-a47d-8c48425ef4a7",
    name: "inaturalist",
    title: "iNaturalist Research-grade Observations",
    doi: "10.15468/ab3s5x",
    attribution: "iNaturalist contributors",
};

pub const EBIRD: Dataset = Dataset {
    key: "4fa7b334-ce0d-4e88-aaae-2e0c138d049e",
    name: "ebird",
    title: "EOD – eBird Observation Dataset",
    doi: "10.15468/aomfnb",
    attribution: "Cornell Lab of Ornithology and eBird contributors",
};

/// GBIF's names for the licenses that occurrences can have.
const GBIF_LICENSES: [&str; 3] = ["CC0_1_0", "CC_BY_4_0", "CC_BY_NC_4_0"];

/// One output file: the records of `dataset` whose license is one of
/// `licenses` (indices into [`GBIF_LICENSES`]), published under `license`.
#[derive(Debug)]
pub struct Layer {
    pub dataset: &'static Dataset,
    pub licenses: &'static [usize],
    pub license: &'static License,
}

/// What a run makes. Share-alike, no-derivatives and unlicensed data are
/// never used; GBIF does not take them from iNaturalist anyway.
pub const LAYERS: [Layer; 3] = [
    Layer {
        dataset: &EBIRD,
        licenses: &[0, 1],
        license: &bom::CC_BY_4_0,
    },
    Layer {
        dataset: &INATURALIST,
        licenses: &[0, 1],
        license: &bom::CC_BY_4_0,
    },
    Layer {
        dataset: &INATURALIST,
        licenses: &[0, 1, 2],
        license: &bom::CC_BY_NC_4_0,
    },
];

const DATASETS: [&Dataset; 2] = [&EBIRD, &INATURALIST];

impl Layer {
    /// Such as `inaturalist-observers-cc-by-nc-4.0`.
    pub fn name(&self) -> String {
        format!(
            "{}-observers-{}",
            self.dataset.name,
            self.license.id.to_ascii_lowercase()
        )
    }

    /// Such as `inaturalist-observers.2026-10-01.cc-by-nc-4.0.tiff`.
    pub fn file_name(&self, snapshot_date: &str) -> String {
        format!(
            "{}-observers.{snapshot_date}.{}.tiff",
            self.dataset.name,
            self.license.id.to_ascii_lowercase()
        )
    }

    fn title(&self) -> String {
        let what = if self.dataset == &EBIRD {
            "eBird"
        } else {
            "iNaturalist"
        };
        format!("{what} observers ({})", self.license.id)
    }

    fn copyright(&self) -> String {
        format!(
            "{}, {}, via GBIF.org, https://doi.org/{}; licensed under {}",
            self.dataset.attribution, self.dataset.title, self.dataset.doi, self.license.id
        )
    }
}

/// Records decoded at a time from each column.
const BATCH: usize = 1 << 14;

/// Pairs per layer that a reader thread collects before passing them on.
const PIECE: usize = 1 << 17;

/// Records whose stated uncertainty is worse than this are left out.
/// That includes the locations iNaturalist blurs to protect a species or
/// an observer's privacy, which GBIF marks with an uncertainty of 20–30 km.
pub const MAX_UNCERTAINTY_M: f64 = 10_000.0;

pub struct Config {
    /// The bucket, such as [`snapshot::BUCKET_URL`].
    pub bucket: String,
    /// The snapshot date, YYYY-MM-DD; `None` for the latest.
    pub snapshot: Option<String>,
    pub output_dir: PathBuf,
    pub workdir: PathBuf,
    pub threads: usize,
    /// Pairs and keys held in memory before spilling a sorted run.
    pub run_size: usize,
    /// Read only these files of the snapshot, by position in its listing.
    pub files: Option<std::ops::Range<usize>>,
    /// Identifies this run in its scheduler, for the data BOM.
    pub run_id: String,
}

#[derive(Debug, Default)]
pub struct LayerSummary {
    pub output: PathBuf,
    pub bom: PathBuf,
    /// Records that went into the layer.
    pub records: u64,
    /// Distinct observers, and distinct (observer, zoom-16 pixel) pairs.
    pub observers: u64,
    pub pairs: u64,
    /// Zoom-16 pixels with at least one observer.
    pub pixels: u64,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub snapshot: String,
    pub datapackage: PathBuf,
    pub layers: Vec<LayerSummary>,
    /// Snapshot files read, and how many of them hold our datasets.
    pub files: usize,
    pub files_with_data: usize,
    /// Records of our datasets, and why some were left out.
    pub records: u64,
    pub no_location: u64,
    pub imprecise: u64,
    pub no_observer: u64,
    pub other_license: u64,
}

/// Counts of what one file had.
#[derive(Default)]
struct Tally {
    records: u64,
    no_location: u64,
    imprecise: u64,
    no_observer: u64,
    other_license: u64,
    per_layer: [u64; LAYERS.len()],
}

pub fn run(cfg: &Config) -> Result<Summary> {
    let start = Instant::now();
    let workdir = Workdir::open(&cfg.workdir, PACKAGE.name)?;
    std::fs::create_dir_all(&cfg.output_dir)
        .with_context(|| format!("creating {}", cfg.output_dir.display()))?;
    let agent = crate::http::agent();
    let snap = snapshot::open(&agent, &cfg.bucket, cfg.snapshot.as_deref())?;
    let files = match &cfg.files {
        Some(r) => &snap.files[r.start.min(snap.files.len())..r.end.min(snap.files.len())],
        None => &snap.files[..],
    };
    eprintln!(
        "snapshot {}: reading {} of {} files",
        snap.date,
        files.len(),
        snap.files.len()
    );

    let observers: Vec<Observers> = DATASETS
        .iter()
        .map(|_| Mutex::new(HashMap::new()))
        .collect();
    let mut runs: Vec<Vec<crate::extsort::Run>> = LAYERS.iter().map(|_| Vec::new()).collect();
    let mut summary = Summary {
        snapshot: snap.date.clone(),
        files: files.len(),
        ..Default::default()
    };
    let mut tally = Tally::default();
    let tmp = workdir.tmp();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| -> Result<()> {
        let (tx, rx) = crossbeam_channel::bounded::<Result<Part>>(2);
        for _ in 0..cfg.threads.max(1) {
            let tx = tx.clone();
            let (next, agent, observers) = (&next, &agent, &observers);
            s.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { return };
                    let mut send = |pairs| {
                        tx.send(Ok(Part::Pairs(pairs)))
                            .map_err(|_| anyhow::anyhow!("pairs not taken"))
                    };
                    let msg = read_file(agent, file, observers, &mut send).map(Part::File);
                    let failed = msg.is_err();
                    if tx.send(msg).is_err() || failed {
                        // On error, stop handing out files.
                        next.store(files.len(), Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
        drop(tx);
        // Each layer's sorted pairs, as they came from the files. We
        // merge them when spilling, so they never get copied together.
        let mut bufs: Vec<Vec<Vec<u64>>> = LAYERS.iter().map(|_| Vec::new()).collect();
        let mut held = 0usize;
        let mut done = 0usize;
        for msg in rx {
            let pairs = match msg? {
                Part::Pairs(pairs) => pairs,
                Part::File(t) => {
                    done += 1;
                    if t.records > 0 {
                        summary.files_with_data += 1;
                    }
                    tally.add(&t);
                    if done.is_multiple_of(500) {
                        eprintln!(
                            "read {done} of {} files, {} records, {:.0?}",
                            files.len(),
                            tally.records,
                            start.elapsed()
                        );
                    }
                    continue;
                }
            };
            for (buf, pairs) in bufs.iter_mut().zip(pairs) {
                if !pairs.is_empty() {
                    held += pairs.len();
                    buf.push(pairs);
                }
            }
            // All layers share one budget; spill the biggest first.
            while held >= cfg.run_size.max(1) {
                let (l, buf) = bufs
                    .iter_mut()
                    .enumerate()
                    .max_by_key(|(_, b)| b.iter().map(Vec::len).sum::<usize>())
                    .unwrap();
                held -= buf.iter().map(Vec::len).sum::<usize>();
                runs[l].push(crate::extsort::write_merged(buf, tmp)?);
                *buf = Vec::new();
            }
        }
        for (buf, runs) in bufs.iter().zip(runs.iter_mut()) {
            if !buf.is_empty() {
                runs.push(crate::extsort::write_merged(buf, tmp)?);
            }
        }
        drop(bufs);
        ensure!(
            done == files.len(),
            "read only {done} of {} files",
            files.len()
        );
        Ok(())
    })?;
    summary.records = tally.records;
    summary.no_location = tally.no_location;
    summary.imprecise = tally.imprecise;
    summary.no_observer = tally.no_observer;
    summary.other_license = tally.other_license;
    eprintln!(
        "read {} files ({} with data), {} records, {:.0?}",
        summary.files,
        summary.files_with_data,
        summary.records,
        start.elapsed()
    );

    let mut layers = Vec::new();
    for ((layer, runs), records) in LAYERS.iter().zip(runs).zip(tally.per_layer) {
        let mut ls = write_layer(cfg, &snap, layer, runs, tmp)?;
        ls.records = records;
        eprintln!(
            "wrote {}: {} records, {} pairs, {} pixels, {:.0?}",
            ls.output.display(),
            ls.records,
            ls.pairs,
            ls.pixels,
            start.elapsed()
        );
        layers.push((layer, ls));
    }

    // Written last, so it never points to files that are not there yet.
    let sources = std::iter::once(serde_json::json!({
        "title": format!("GBIF occurrence snapshot {}", snap.date),
        "path": snap.url,
        "version": crate::date::rfc3339(snap.timestamp),
    }))
    .chain(DATASETS.iter().map(|d| {
        serde_json::json!({
            "title": format!("{}, {}", d.attribution, d.title),
            "path": format!("https://doi.org/{}", d.doi),
            "version": crate::date::rfc3339(snap.timestamp),
        })
    }))
    .collect();
    let mut hashes = Vec::new();
    for (_, ls) in &layers {
        hashes.push(bom::hash_file(&ls.output)?);
    }
    let names: Vec<(String, String)> = layers.iter().map(|(l, _)| (l.name(), l.title())).collect();
    let dp_layers: Vec<crate::datapackage::Layer> = layers
        .iter()
        .zip(&hashes)
        .zip(&names)
        .map(|(((l, ls), h), (name, title))| crate::datapackage::Layer {
            name,
            title,
            tiff: &ls.output,
            tiff_hashes: h,
            bom: &ls.bom,
            license: l.license,
        })
        .collect();
    summary.datapackage =
        crate::datapackage::write_layers(&PACKAGE, sources, snap.timestamp, &dp_layers)?;
    summary.layers = layers.into_iter().map(|(_, ls)| ls).collect();
    eprintln!(
        "wrote {} in {:.0?}",
        summary.datapackage.display(),
        start.elapsed()
    );
    Ok(summary)
}

impl Tally {
    fn add(&mut self, t: &Tally) {
        self.records += t.records;
        self.no_location += t.no_location;
        self.imprecise += t.imprecise;
        self.no_observer += t.no_observer;
        self.other_license += t.other_license;
        for (a, b) in self.per_layer.iter_mut().zip(t.per_layer) {
            *a += b;
        }
    }
}

/// Observers by the 64-bit hash of their name, numbered per dataset.
/// Two names sharing a hash would merge, but among a few million
/// observers the odds are below one in a million.
type Observers = Mutex<HashMap<u64, u32>>;

fn hash(name: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::hash::DefaultHasher::new();
    h.write(name);
    h.finish()
}

/// What a reader thread sends: sorted, distinct pairs of each layer
/// from part of a file, or what a file had once it is done.
enum Part {
    Pairs(Vec<Vec<u64>>),
    File(Tally),
}

/// Reads one snapshot file, passes its pairs to `send` in sorted and
/// deduplicated pieces of about [`PIECE`] per layer, and returns what
/// it had. Pieces may share pairs; the external sort merges them.
fn read_file(
    agent: &ureq::Agent,
    file: &snapshot::File,
    observers: &[Observers],
    send: &mut dyn FnMut(Vec<Vec<u64>>) -> Result<()>,
) -> Result<Tally> {
    let mut tally = Tally::default();
    let mut pairs: Vec<Vec<u64>> = LAYERS.iter().map(|_| Vec::new()).collect();
    let f = snapshot::RemoteFile::open(agent, file)?;
    // Reused for every batch.
    let (mut license, mut lat, mut lon, mut uncertainty, mut who) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for rg in 0..f.row_groups() {
        let rows = f.rows(rg);
        let mut datasets = Vec::with_capacity(rows);
        let mut keys = f.columns(rg, &["datasetkey"])?.remove(0);
        for start in (0..rows).step_by(BATCH) {
            keys.codes(BATCH.min(rows - start), &mut datasets, |k| {
                DATASETS
                    .iter()
                    .position(|d| d.key.as_bytes() == k)
                    .map_or(snapshot::NULL, |i| i as u8)
            })?;
        }
        drop(keys);
        if datasets.iter().all(|&d| d == snapshot::NULL) {
            continue;
        }
        let mut cols = f.columns(
            rg,
            &[
                "license",
                "decimallatitude",
                "decimallongitude",
                "coordinateuncertaintyinmeters",
                "recordedby",
            ],
        )?;

        // Observers get numbers local to this row group, mapped to the
        // dataset's numbers with one lock per dataset and batch.
        let mut local: Vec<HashMap<u64, u32>> = DATASETS.iter().map(|_| HashMap::new()).collect();
        let mut global: Vec<Vec<u32>> = DATASETS.iter().map(|_| Vec::new()).collect();
        let mut new: Vec<Vec<u64>> = DATASETS.iter().map(|_| Vec::new()).collect();
        for start in (0..rows).step_by(BATCH) {
            let n = BATCH.min(rows - start);
            let datasets = &datasets[start..start + n];
            license.clear();
            lat.clear();
            lon.clear();
            uncertainty.clear();
            who.clear();
            cols[0].codes(n, &mut license, |l| {
                GBIF_LICENSES
                    .iter()
                    .position(|g| g.as_bytes() == l)
                    .map_or(snapshot::NULL, |i| i as u8)
            })?;
            cols[1].doubles(n, &mut lat)?;
            cols[2].doubles(n, &mut lon)?;
            cols[3].doubles(n, &mut uncertainty)?;
            let mut row = 0;
            cols[4].lists(n, 0x1f, &mut who, |w| {
                let d = datasets[row];
                row += 1;
                match w {
                    Some(w) if d != snapshot::NULL => {
                        let (local, new) = (&mut local[d as usize], &mut new[d as usize]);
                        let (h, id) = (hash(w), local.len() as u32);
                        *local.entry(h).or_insert_with(|| {
                            new.push(h);
                            id
                        })
                    }
                    _ => u32::MAX,
                }
            })?;
            for ((new, global), all) in new.iter_mut().zip(&mut global).zip(observers) {
                if new.is_empty() {
                    continue;
                }
                let mut all = all.lock().expect("observer table poisoned");
                for h in new.drain(..) {
                    let n = all.len();
                    ensure!(n < u32::MAX as usize, "too many observers");
                    global.push(*all.entry(h).or_insert(n as u32));
                }
            }
            for (row, &d) in datasets.iter().enumerate() {
                if d == snapshot::NULL {
                    continue;
                }
                tally.records += 1;
                let Some(pixel) = distinct::pixel_key(lon[row], lat[row]) else {
                    tally.no_location += 1;
                    continue;
                };
                // NaN, for no uncertainty given, compares false.
                if uncertainty[row] > MAX_UNCERTAINTY_M {
                    tally.imprecise += 1;
                    continue;
                }
                if license[row] == snapshot::NULL {
                    tally.other_license += 1;
                    continue;
                }
                if who[row] == u32::MAX {
                    tally.no_observer += 1;
                    continue;
                }
                let (d, lic) = (d as usize, license[row] as usize);
                let pair = distinct::pair(global[d][who[row] as usize], pixel);
                for (l, layer) in LAYERS.iter().enumerate() {
                    if layer.dataset == DATASETS[d] && layer.licenses.contains(&lic) {
                        pairs[l].push(pair);
                        tally.per_layer[l] += 1;
                    }
                }
            }
            for l in 0..pairs.len() {
                let p = &mut pairs[l];
                if p.len() < PIECE {
                    continue;
                }
                p.sort_unstable();
                p.dedup();
                // Mostly repeats? Then keep collecting.
                if p.len() >= PIECE / 2 {
                    let mut piece: Vec<Vec<u64>> = LAYERS.iter().map(|_| Vec::new()).collect();
                    piece[l] = std::mem::take(p);
                    piece[l].shrink_to_fit();
                    send(piece)?;
                }
            }
        }
    }
    if pairs.iter().any(|p| !p.is_empty()) {
        for p in &mut pairs {
            p.sort_unstable();
            p.dedup();
            // The caller holds on to these, counting their length.
            p.shrink_to_fit();
        }
        send(pairs)?;
    }
    Ok(tally)
}

/// Turns a layer's pair runs into its GeoTIFF and data BOM.
fn write_layer(
    cfg: &Config,
    snap: &snapshot::Snapshot,
    layer: &Layer,
    runs: Vec<crate::extsort::Run>,
    tmp: &Path,
) -> Result<LayerSummary> {
    let mut ls = LayerSummary::default();
    let grid = Grid {
        crs: Crs::WebMercator,
        max_zoom: MAX_TILE_ZOOM,
    };
    let dir = tmp.join(layer.name());
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Count pairs and observers while passing the pairs on.
    let pairs = std::cell::Cell::new(0u64);
    let observers = std::cell::Cell::new(0u64);
    let last_observer = std::cell::Cell::new(u64::MAX);
    let stream = distinct::distinct(runs)?.inspect(|p| {
        if let Ok(p) = p {
            pairs.set(pairs.get() + 1);
            if p >> 32 != last_observer.get() {
                last_observer.set(p >> 32);
                observers.set(observers.get() + 1);
            }
        }
    });
    let level_runs = distinct::level_runs(stream, &dir, cfg.run_size)?;
    ls.pairs = pairs.get();
    ls.observers = observers.get();

    let mut spool = Spool::new(&dir, grid)?;
    let mut hist = Histogram::default();
    for (z, runs) in level_runs.into_iter().enumerate() {
        let z = z as u8;
        let pixels = crate::extsort::merge(runs)?;
        let (h, n) = encode_level(z, pixels, cfg.threads, &mut spool)?;
        if z == MAX_TILE_ZOOM {
            hist = h;
            ls.pixels = n;
        }
    }
    let full_tiles = spool.tiles_per_zoom[MAX_TILE_ZOOM as usize];
    let empty_tiles = (1u64 << (2 * MAX_TILE_ZOOM)) - full_tiles;
    hist.add_value(0, empty_tiles * (TILE_SIZE * TILE_SIZE) as u64);

    ls.output = cfg.output_dir.join(layer.file_name(&snap.date));
    let copyright = layer.copyright();
    let title = layer.title();
    let meta = Metadata {
        description: title.clone(),
        software: format!("geosignals {}", env!("CARGO_PKG_VERSION")),
        copyright: copyright.clone(),
        min_value: hist.range().0,
        max_value: hist.range().1,
        gdal_metadata: gdal_metadata(layer, &snap.date, &hist),
        nodata: None,
    };
    spool.write_cog(&ls.output, &meta)?;
    let hashes = bom::hash_file(&ls.output)?;
    let sources = [
        bom::Source {
            name: "gbif/occurrence-snapshot".into(),
            version: snap.timestamp,
            url: Some(snap.url.clone()),
            copyright: Some("GBIF.org".into()),
            ..Default::default()
        },
        bom::Source {
            name: format!("gbif/{}", layer.dataset.name),
            version: snap.timestamp,
            url: Some(format!("https://doi.org/{}", layer.dataset.doi)),
            license_id: Some(layer.license.id.into()),
            license_url: Some(layer.license.url.into()),
            copyright: Some(layer.dataset.attribution.into()),
            ..Default::default()
        },
    ];
    ls.bom = bom::write_sources(
        &bom::SourcesRun {
            sources: &sources,
            timestamp: snap.timestamp,
            output: &ls.output,
            description: &title,
            run_id: &cfg.run_id,
            copyright: &copyright,
            license: layer.license,
        },
        &hashes,
    )?;
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(ls)
}

/// Assembles the (pixel key, observers) pairs of tile zoom `z`, which
/// come in Hilbert order, into tiles, and encodes and spools them on
/// `threads` threads. Returns the histogram of the encoded pixels and
/// how many pixels had observers.
fn encode_level(
    z: u8,
    pixels: impl Iterator<Item = Result<(u64, u64)>>,
    threads: usize,
    spool: &mut Spool,
) -> Result<(Histogram, u64)> {
    let pixel_zoom = z + crate::tile::TILE_BITS;
    let mut hist = Histogram::default();
    let mut occupied = 0u64;
    std::thread::scope(|s| -> Result<()> {
        type Job = (u32, u32, Vec<u64>);
        let (job_tx, job_rx) = crossbeam_channel::bounded::<Job>(threads * 4);
        let (res_tx, res_rx) =
            crossbeam_channel::bounded::<(u32, u32, Result<Vec<u8>>)>(threads * 4);
        let mut workers = Vec::new();
        for _ in 0..threads.max(1) {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            workers.push(s.spawn(move || {
                let mut hist = Histogram::default();
                for (x, y, counts) in job_rx {
                    let px = crate::encode::pixels(z, y, &counts);
                    hist.add(&px);
                    if res_tx.send((x, y, crate::encode::zstd(&px))).is_err() {
                        break;
                    }
                }
                hist
            }));
        }
        drop((job_rx, res_tx));
        let collector = s.spawn(move || -> Result<()> {
            for (x, y, data) in res_rx {
                spool.add(z, x, y, &data?)?;
            }
            Ok(())
        });
        let send = || -> Result<()> {
            let mut tile: Option<(u64, u32, u32, Vec<u64>)> = None;
            for p in pixels {
                let (key, count) = p?;
                occupied += 1;
                let (px, py): (u32, u32) = fast_hilbert::h2xy(key, pixel_zoom);
                let tile_key = key >> (2 * crate::tile::TILE_BITS);
                if tile.as_ref().is_some_and(|t| t.0 != tile_key) {
                    let (_, x, y, counts) = tile.take().unwrap();
                    job_tx.send((x, y, counts)).context("encoder failed")?;
                }
                let t = tile.get_or_insert_with(|| {
                    let bits = crate::tile::TILE_BITS;
                    (
                        tile_key,
                        px >> bits,
                        py >> bits,
                        vec![0; TILE_SIZE * TILE_SIZE],
                    )
                });
                let mask = TILE_SIZE as u32 - 1;
                t.3[((py & mask) as usize) * TILE_SIZE + (px & mask) as usize] = count;
            }
            if let Some((_, x, y, counts)) = tile {
                job_tx.send((x, y, counts)).context("encoder failed")?;
            }
            Ok(())
        };
        let sent = send();
        drop(job_tx);
        let collected = collector.join().expect("collector panicked");
        for w in workers {
            hist.merge(&w.join().expect("encoder panicked"));
        }
        sent?;
        collected
    })?;
    Ok((hist, occupied))
}

/// Band description and scale for GDAL, so that tools like QGIS show
/// log2(observers per km²), plus the value histogram.
fn gdal_metadata(layer: &Layer, snapshot: &str, hist: &Histogram) -> String {
    let scale = 1.0 / crate::encode::STEPS_PER_OCTAVE;
    let offset = crate::encode::LOG2_MIN_DENSITY - scale;
    format!(
        concat!(
            "<GDALMetadata>",
            r#"<Item name="DATASET">https://doi.org/{doi}</Item>"#,
            r#"<Item name="SNAPSHOT">{snapshot}</Item>"#,
            r#"<Item name="LICENSE">{license}</Item>"#,
            r#"<Item name="ENCODING">v = 0: no observers; otherwise log2(distinct observers per km²) = {scale} * v - {neg_offset}</Item>"#,
            r#"<Item name="DESCRIPTION" sample="0" role="description">log2(distinct observers per km²)</Item>"#,
            r#"<Item name="SCALE" sample="0" role="scale">{scale}</Item>"#,
            r#"<Item name="OFFSET" sample="0" role="offset">{offset}</Item>"#,
            "{rat}",
            "</GDALMetadata>"
        ),
        doi = layer.dataset.doi,
        snapshot = snapshot,
        license = layer.license.id,
        scale = scale,
        offset = offset,
        neg_offset = -offset,
        rat = hist.rat_xml(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(LAYERS[0].name(), "ebird-observers-cc-by-4.0");
        assert_eq!(
            LAYERS[2].file_name("2026-10-01"),
            "inaturalist-observers.2026-10-01.cc-by-nc-4.0.tiff"
        );
        assert_eq!(
            bom::bom_path(Path::new(&LAYERS[1].file_name("2026-10-01"))),
            Path::new("inaturalist-observers.2026-10-01.cc-by-4.0.cdx.json")
        );
    }

    #[test]
    fn layer_licenses() {
        for l in &LAYERS {
            // Only the NonCommercial layer takes NonCommercial records.
            assert_eq!(l.licenses.contains(&2), l.license == &bom::CC_BY_NC_4_0);
        }
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
                b"https://github.com/brawer/osmsignals#naturalist-observers"
            ),
            format!("urn:uuid:{}", PACKAGE.id)
        );
    }
}
