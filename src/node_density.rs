// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `node-density` pipeline: PBF → sorted keys → pyramid → GeoTIFF.

use crate::cog::{Grid, Metadata, Spool};
use crate::histogram::Histogram;
use crate::tile::{MAX_TILE_ZOOM, TILE_SIZE};
use crate::workdir::Workdir;
use crate::{encode, extsort, pbf, pyramid};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

const PACKAGE: crate::datapackage::Package = crate::datapackage::Package {
    name: "osm-node-density",
    // UUIDv5(URL, "https://github.com/brawer/osmsignals#osm-node-density").
    // Frozen: catalogs key on it.
    id: "2c7bffea-cc30-533e-ac1d-dc7b4aefd158",
    title: "OpenStreetMap node density",
    description: "How many OpenStreetMap nodes there are per km² of Earth surface, \
        for every zoom-18 Web Mercator tile, as a Cloud-Optimized GeoTIFF with \
        overviews. Pixel values encode log2(nodes per km²); 0 means no nodes.",
};

pub struct Config {
    /// Where the input came from, for the data BOM.
    pub input: crate::bom::Input,
    /// Identifies this run in its scheduler, for the data BOM.
    pub run_id: String,
    /// Directory for the output, which is named after the date of the
    /// input data; see [`output_name`].
    pub output_dir: PathBuf,
    /// Directory for temporary files; see [`crate::workdir`].
    pub workdir: PathBuf,
    pub threads: usize,
    /// Stop reading after this many PBF data blocks, for smoke tests.
    pub max_blocks: Option<usize>,
    pub run_size: usize,
    /// If set, raw counts of a sample of tiles are written here,
    /// for benchmarking image formats.
    pub sample_dir: Option<PathBuf>,
    /// Keep one in this many tiles at zoom 6 and deeper in the sample.
    pub sample_rate: u64,
}

#[derive(Debug, Default)]
pub struct Summary {
    /// The GeoTIFF file that was written.
    pub output: PathBuf,
    /// Its data bill of materials; see [`crate::bom`].
    pub bom: PathBuf,
    /// The data package pointing to both; see [`crate::datapackage`].
    pub datapackage: PathBuf,
    pub nodes: u64,
    pub skipped_nodes: u64,
    pub pixels: u64,
    pub tiles_per_zoom: Vec<u64>,
    pub bytes_per_zoom: Vec<u64>,
}

pub fn run<R: Read + Send>(input: R, cfg: &Config) -> Result<Summary> {
    let start = Instant::now();
    let workdir = Workdir::open(&cfg.workdir, "node-density")?;
    std::fs::create_dir_all(&cfg.output_dir)
        .with_context(|| format!("creating {}", cfg.output_dir.display()))?;
    let opts = pbf::Options {
        threads: cfg.threads,
        run_size: cfg.run_size,
        tmpdir: workdir.tmp(),
        max_blocks: cfg.max_blocks,
    };
    let (runs, stats) = pbf::sorted_runs(input, &opts)?;
    eprintln!(
        "read {} nodes ({} outside Web Mercator) into {} runs in {:.0?}",
        stats.nodes,
        stats.skipped,
        runs.len(),
        start.elapsed()
    );

    let mut spool = Spool::new(workdir.tmp(), Grid::WEB_MERCATOR)?;
    let mut pixels = 0u64;
    let mut hist = Histogram::default();
    if let Some(dir) = &cfg.sample_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Tiles are encoded in parallel but spooled in the order the pyramid
    // emitted them, so each zoom level stays in Hilbert order.
    std::thread::scope(|s| -> Result<()> {
        type Job = (u64, u8, u32, u32, Vec<u16>);
        let (job_tx, job_rx) = crossbeam_channel::bounded::<Job>(cfg.threads * 8);
        let (res_tx, res_rx) =
            crossbeam_channel::bounded::<(u64, u8, u32, u32, Result<Vec<u8>>)>(cfg.threads * 8);
        let mut workers = Vec::new();
        for _ in 0..cfg.threads.max(1) {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            workers.push(s.spawn(move || {
                let mut hist = Histogram::default();
                for (seq, z, x, y, px) in job_rx {
                    if z == MAX_TILE_ZOOM {
                        hist.add(&px);
                    }
                    if res_tx.send((seq, z, x, y, encode::zstd(&px))).is_err() {
                        break;
                    }
                }
                hist
            }));
        }
        drop((job_rx, res_tx));
        let spool = &mut spool;
        let collector = s.spawn(move || -> Result<()> {
            let mut pending = BTreeMap::new();
            let mut next = 0u64;
            for (seq, z, x, y, data) in res_rx {
                pending.insert(seq, (z, x, y, data));
                while let Some((z, x, y, data)) = pending.remove(&next) {
                    spool.add(z, x, y, &data?)?;
                    next += 1;
                }
            }
            Ok(())
        });

        let mut seq = 0u64;
        let sample_dir = cfg.sample_dir.as_deref();
        let mut builder = pyramid::Builder::new(|z, x, y, counts: &pyramid::Counts| {
            if let Some(dir) = sample_dir
                && (z <= 5 || fast_hilbert::xy2h(x, y, z) % cfg.sample_rate == 0)
            {
                write_sample(dir, z, x, y, counts)?;
            }
            let px = encode::pixels(z, y, counts);
            job_tx.send((seq, z, x, y, px)).context("encoder failed")?;
            seq += 1;
            Ok(())
        });
        for item in extsort::merge(runs)? {
            let (key, count) = item?;
            builder.add(key, count)?;
            pixels += 1;
        }
        let built = builder.finish();
        drop(job_tx);
        let collected = collector.join().expect("collector panicked");
        for w in workers {
            hist.merge(&w.join().expect("encoder panicked"));
        }
        built?;
        collected
    })?;
    eprintln!(
        "built pyramid from {pixels} non-empty pixels in {:.0?}",
        start.elapsed()
    );

    // Tiles without nodes were never emitted; their pixels are all zero.
    let full_tiles = spool.tiles_per_zoom[MAX_TILE_ZOOM as usize];
    let empty_tiles = (1u64 << (2 * MAX_TILE_ZOOM)) - full_tiles;
    hist.add_value(0, empty_tiles * (TILE_SIZE * TILE_SIZE) as u64);

    let output = cfg.output_dir.join(output_name(stats.timestamp)?);
    let mut summary = Summary {
        output: output.clone(),
        nodes: stats.nodes,
        skipped_nodes: stats.skipped,
        pixels,
        tiles_per_zoom: spool.tiles_per_zoom.clone(),
        bytes_per_zoom: spool.bytes_per_zoom.clone(),
        ..Summary::default()
    };
    let meta = Metadata {
        description: "OpenStreetMap node density".into(),
        software: format!("osmsignals {}", env!("CARGO_PKG_VERSION")),
        copyright: "© OpenStreetMap contributors, ODbL 1.0".into(),
        min_value: hist.range().0,
        max_value: hist.range().1,
        gdal_metadata: gdal_metadata(stats.nodes, &hist),
        nodata: None,
    };
    spool.write_cog(&output, &meta)?;
    let hashes = crate::bom::hash_file(&output)?;
    summary.bom = crate::bom::write(
        &crate::bom::Run {
            input: &cfg.input,
            timestamp: stats.timestamp,
            output: &output,
            description: "OpenStreetMap node density",
            run_id: &cfg.run_id,
        },
        &hashes,
    )?;
    // Written last, so it never points to files that are not there yet.
    summary.datapackage = crate::datapackage::write(
        &PACKAGE,
        &crate::datapackage::Files {
            input: &cfg.input,
            timestamp: stats.timestamp,
            tiff: &output,
            tiff_hashes: &hashes,
            bom: &summary.bom,
        },
    )?;
    eprintln!(
        "wrote {}, {} and {} in {:.0?}",
        output.display(),
        summary.bom.display(),
        summary.datapackage.display(),
        start.elapsed()
    );
    Ok(summary)
}

/// File name for data as of `timestamp` (seconds since the Unix epoch),
/// such as `osm-node-density.2026-10-19.tiff`. The date is in UTC.
pub fn output_name(timestamp: i64) -> Result<String> {
    crate::date::dated_name(PACKAGE.name, timestamp)
}

fn write_sample(dir: &Path, z: u8, x: u32, y: u32, counts: &pyramid::Counts) -> Result<()> {
    let mut raw = Vec::with_capacity(TILE_SIZE * TILE_SIZE * 4);
    for &c in counts {
        raw.extend_from_slice(&(c.min(u32::MAX as u64) as u32).to_le_bytes());
    }
    let data = zstd::encode_all(raw.as_slice(), 3)?;
    std::fs::write(dir.join(format!("{z}-{x}-{y}.u32.zst")), data)?;
    Ok(())
}

/// Band description and scale for GDAL, so that tools like QGIS show
/// log2(nodes/km²) instead of raw pixel values, plus the value histogram.
fn gdal_metadata(nodes: u64, hist: &Histogram) -> String {
    let scale = 1.0 / encode::STEPS_PER_OCTAVE;
    let offset = encode::LOG2_MIN_DENSITY - scale;
    format!(
        concat!(
            "<GDALMetadata>",
            r#"<Item name="NODES">{nodes}</Item>"#,
            r#"<Item name="ENCODING">v = 0: no nodes; otherwise log2(nodes per km²) = {scale} * v - {neg_offset}</Item>"#,
            r#"<Item name="DESCRIPTION" sample="0" role="description">log2(OSM nodes per km²)</Item>"#,
            r#"<Item name="SCALE" sample="0" role="scale">{scale}</Item>"#,
            r#"<Item name="OFFSET" sample="0" role="offset">{offset}</Item>"#,
            "{rat}",
            "</GDALMetadata>"
        ),
        nodes = nodes,
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
    fn output_names() {
        let name = |t| output_name(t).unwrap();
        assert_eq!(name(0), "osm-node-density.1970-01-01.tiff");
        assert_eq!(name(951_782_400), "osm-node-density.2000-02-29.tiff");
        assert_eq!(name(1_760_918_399), "osm-node-density.2025-10-19.tiff");
        assert_eq!(name(1_760_918_400), "osm-node-density.2025-10-20.tiff");
        assert!(output_name(-1).is_err());
        assert!(output_name(i64::MAX / 2).is_err());
    }
}
