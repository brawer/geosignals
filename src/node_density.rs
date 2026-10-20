// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `node-density` pipeline: PBF → sorted keys → pyramid → PMTiles.

use crate::{encode, extsort, output::Spool, pbf, pyramid, tile::TILE_SIZE};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub struct Config {
    pub output: PathBuf,
    pub tmpdir: PathBuf,
    pub threads: usize,
    pub run_size: usize,
    /// If set, raw counts of a sample of tiles are written here,
    /// for benchmarking image formats.
    pub sample_dir: Option<PathBuf>,
    /// Keep one in this many tiles at zoom 6 and deeper in the sample.
    pub sample_rate: u64,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub nodes: u64,
    pub skipped_nodes: u64,
    pub pixels: u64,
    pub tiles_per_zoom: Vec<u64>,
    pub bytes_per_zoom: Vec<u64>,
}

pub fn run<R: Read + Send>(input: R, cfg: &Config) -> Result<Summary> {
    let start = Instant::now();
    let opts = pbf::Options {
        threads: cfg.threads,
        run_size: cfg.run_size,
        tmpdir: &cfg.tmpdir,
    };
    let (runs, stats) = pbf::sorted_runs(input, &opts)?;
    eprintln!(
        "read {} nodes ({} outside Web Mercator) into {} runs in {:.0?}",
        stats.nodes,
        stats.skipped,
        runs.len(),
        start.elapsed()
    );

    let mut spool = Spool::new(&cfg.tmpdir)?;
    let mut pixels = 0u64;
    if let Some(dir) = &cfg.sample_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Tiles are encoded in parallel but spooled in the order the pyramid
    // emitted them, so each zoom level stays in Hilbert order.
    std::thread::scope(|s| -> Result<()> {
        type Job = (u64, u8, u32, u32, Vec<u8>);
        let (job_tx, job_rx) = crossbeam_channel::bounded::<Job>(cfg.threads * 8);
        let (res_tx, res_rx) =
            crossbeam_channel::bounded::<(u64, u8, u32, u32, Result<Vec<u8>>)>(cfg.threads * 8);
        for _ in 0..cfg.threads.max(1) {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            s.spawn(move || {
                for (seq, z, x, y, px) in job_rx {
                    if res_tx.send((seq, z, x, y, encode::webp(&px))).is_err() {
                        break;
                    }
                }
            });
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
        built?;
        collected
    })?;
    eprintln!(
        "built pyramid from {pixels} non-empty pixels in {:.0?}",
        start.elapsed()
    );

    let summary = Summary {
        nodes: stats.nodes,
        skipped_nodes: stats.skipped,
        pixels,
        tiles_per_zoom: spool.tiles_per_zoom.clone(),
        bytes_per_zoom: spool.bytes_per_zoom.clone(),
    };
    let metadata = format!(
        concat!(
            r#"{{"name":"OSM node density","#,
            r#""description":"OpenStreetMap nodes per km², log scale: value v>0 means {} * 2^((v-1)/{}) nodes/km²; 0 means no nodes","#,
            r#""attribution":"© OpenStreetMap contributors","#,
            r#""generator":"osmsignals {}","nodes":{}}}"#
        ),
        encode::MIN_DENSITY,
        encode::STEPS_PER_OCTAVE,
        env!("CARGO_PKG_VERSION"),
        stats.nodes,
    );
    spool.write_pmtiles(&cfg.output, &metadata)?;
    eprintln!("wrote {} in {:.0?}", cfg.output.display(), start.elapsed());
    Ok(summary)
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
