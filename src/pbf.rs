// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Parallel extraction of node sort keys from an OSM PBF stream.
//!
//! One thread reads raw blobs; worker threads decompress them, turn each
//! node into its Hilbert key, and collect the keys into sorted runs on disk.

use crate::extsort::{self, Run};
use crate::tile;
use anyhow::{Context, Result, bail};
use osmpbf::{BlobDecode, BlobReader};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Nodes seen in the input.
    pub nodes: u64,
    /// Nodes outside the Web Mercator latitude range, which we skip.
    pub skipped: u64,
}

pub struct Options<'a> {
    pub threads: usize,
    /// Number of keys each worker buffers before spilling a sorted run.
    pub run_size: usize,
    pub tmpdir: &'a Path,
}

/// Reads all nodes from `input` and returns sorted runs of their keys.
///
/// If the file declares itself sorted by type, reading stops at the first
/// block with ways or relations, so most of the planet need not be decoded.
pub fn sorted_runs<R: Read + Send>(input: R, opts: &Options) -> Result<(Vec<Run>, Stats)> {
    let nodes = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::bounded::<osmpbf::Blob>(opts.threads * 4);
    let mut runs = Vec::new();

    std::thread::scope(|s| -> Result<()> {
        let mut workers = Vec::new();
        for _ in 0..opts.threads.max(1) {
            let rx = rx.clone();
            let (nodes, skipped, done) = (&nodes, &skipped, &done);
            workers.push(s.spawn(move || -> Result<Vec<Run>> {
                let mut runs = Vec::new();
                let mut keys: Vec<u64> = Vec::with_capacity(opts.run_size);
                for blob in rx {
                    let BlobDecode::OsmData(block) = blob.decode()? else {
                        continue;
                    };
                    let (mut n, mut skip) = (0u64, 0u64);
                    let mut push = |lon: f64, lat: f64| {
                        n += 1;
                        match tile::node_key(lon, lat) {
                            Some(k) => keys.push(k),
                            None => skip += 1,
                        }
                    };
                    for group in block.groups() {
                        for node in group.dense_nodes() {
                            push(node.lon(), node.lat());
                        }
                        for node in group.nodes() {
                            push(node.lon(), node.lat());
                        }
                        if group.ways().len() > 0 || group.relations().len() > 0 {
                            done.store(true, Ordering::Relaxed);
                        }
                    }
                    nodes.fetch_add(n, Ordering::Relaxed);
                    skipped.fetch_add(skip, Ordering::Relaxed);
                    if keys.len() >= opts.run_size {
                        runs.push(extsort::write_run(&mut keys, opts.tmpdir)?);
                        keys.clear();
                    }
                }
                if !keys.is_empty() {
                    runs.push(extsort::write_run(&mut keys, opts.tmpdir)?);
                }
                Ok(runs)
            }));
        }
        drop(rx);

        let mut sorted_by_type = false;
        let mut seen_header = false;
        let read_result = (|| -> Result<()> {
            for blob in BlobReader::new(input) {
                let blob = blob.context("reading PBF blob")?;
                match blob.get_type() {
                    osmpbf::BlobType::OsmHeader => {
                        let header = blob.to_headerblock()?;
                        for f in header.required_features() {
                            if !matches!(f.as_str(), "OsmSchema-V0.6" | "DenseNodes") {
                                bail!("unsupported PBF feature: {f}");
                            }
                        }
                        sorted_by_type = header
                            .optional_features()
                            .iter()
                            .any(|f| f == "Sort.Type_then_ID");
                        seen_header = true;
                    }
                    osmpbf::BlobType::OsmData => {
                        if !seen_header {
                            bail!("PBF data before header");
                        }
                        if sorted_by_type && done.load(Ordering::Relaxed) {
                            break;
                        }
                        if tx.send(blob).is_err() {
                            break; // A worker failed; its error is reported below.
                        }
                    }
                    osmpbf::BlobType::Unknown(_) => {}
                }
            }
            Ok(())
        })();
        drop(tx);
        for w in workers {
            runs.extend(w.join().expect("worker panicked")?);
        }
        read_result
    })?;

    let stats = Stats {
        nodes: nodes.into_inner(),
        skipped: skipped.into_inner(),
    };
    Ok((runs, stats))
}
