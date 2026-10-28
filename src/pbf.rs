// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Parallel extraction of node sort keys from an OSM PBF stream.
//!
//! One thread reads raw blobs; worker threads decompress them, turn each
//! node into its Hilbert key, and collect the keys into sorted runs on disk.
//! Keys of nodes with tags go into a second set of runs as well; see
//! [`counts_as_tag`] for the tags that count.

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
    /// Nodes with at least one tag for which [`counts_as_tag`] holds,
    /// including any outside the Web Mercator latitude range.
    pub tagged: u64,
    /// Nodes outside the Web Mercator latitude range, which we skip.
    pub skipped: u64,
    /// When the data was current, in seconds since the Unix epoch,
    /// from the `osmosis_replication_timestamp` of the PBF header.
    pub timestamp: i64,
}

pub struct Options<'a> {
    pub threads: usize,
    /// Number of keys each worker buffers before spilling a sorted run.
    pub run_size: usize,
    pub tmpdir: &'a Path,
    /// Stop after this many data blocks, for smoke tests.
    pub max_blocks: Option<usize>,
}

/// Sorted runs of node keys.
#[derive(Default)]
pub struct Runs {
    /// Keys of all nodes.
    pub all: Vec<Run>,
    /// Keys of the nodes with tags; see [`Stats::tagged`].
    pub tagged: Vec<Run>,
}

/// Whether a tag key is about the edit rather than the feature, so that
/// a node tagged only with such keys counts as untagged. Some of these
/// belong on changesets, but imports sometimes put them on every node.
pub fn is_meta_key(key: &str) -> bool {
    let base = key.split(':').next().unwrap_or(key);
    key.starts_with("source")
        || key == "survey:date"
        || matches!(
            base,
            "created_by" | "note" | "fixme" | "FIXME" | "attribution" | "check_date"
        )
}

/// Whether a node with this tag key counts as tagged: not for
/// [`is_meta_key`] keys, and not for addresses (`addr:*`). A third of
/// all tagged nodes are address points, mostly from imports, and they
/// mirror the buildings that the layer of all nodes already shows.
pub fn counts_as_tag(key: &str) -> bool {
    !is_meta_key(key) && key.split(':').next() != Some("addr")
}

fn has_tags<'a>(mut tags: impl Iterator<Item = (&'a str, &'a str)>) -> bool {
    tags.any(|(k, _)| counts_as_tag(k))
}

/// Reads all nodes from `input` and returns sorted runs of their keys.
///
/// If the file declares itself sorted by type, reading stops at the first
/// block with ways or relations, so most of the planet need not be decoded.
pub fn sorted_runs<R: Read + Send>(input: R, opts: &Options) -> Result<(Runs, Stats)> {
    let nodes = AtomicU64::new(0);
    let tagged = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::bounded::<osmpbf::Blob>(opts.threads * 4);
    let mut runs = Runs::default();

    let timestamp = std::thread::scope(|s| -> Result<i64> {
        let mut workers = Vec::new();
        for _ in 0..opts.threads.max(1) {
            let rx = rx.clone();
            let (nodes, tagged, skipped, done) = (&nodes, &tagged, &skipped, &done);
            workers.push(s.spawn(move || -> Result<Runs> {
                let mut runs = Runs::default();
                let mut keys: Vec<u64> = Vec::with_capacity(opts.run_size);
                let mut tagged_keys: Vec<u64> = Vec::new();
                for blob in rx {
                    let BlobDecode::OsmData(block) = blob.decode()? else {
                        continue;
                    };
                    let (mut n, mut t, mut skip) = (0u64, 0u64, 0u64);
                    let mut push = |lon: f64, lat: f64, has_tags: bool| {
                        n += 1;
                        t += u64::from(has_tags);
                        match tile::node_key(lon, lat) {
                            Some(k) => {
                                keys.push(k);
                                if has_tags {
                                    tagged_keys.push(k);
                                }
                            }
                            None => skip += 1,
                        }
                    };
                    for group in block.groups() {
                        for node in group.dense_nodes() {
                            push(node.lon(), node.lat(), has_tags(node.tags()));
                        }
                        for node in group.nodes() {
                            push(node.lon(), node.lat(), has_tags(node.tags()));
                        }
                        if group.ways().len() > 0 || group.relations().len() > 0 {
                            done.store(true, Ordering::Relaxed);
                        }
                    }
                    nodes.fetch_add(n, Ordering::Relaxed);
                    tagged.fetch_add(t, Ordering::Relaxed);
                    skipped.fetch_add(skip, Ordering::Relaxed);
                    if keys.len() >= opts.run_size {
                        runs.all.push(extsort::write_run(&mut keys, opts.tmpdir)?);
                        keys.clear();
                    }
                    if tagged_keys.len() >= opts.run_size {
                        runs.tagged
                            .push(extsort::write_run(&mut tagged_keys, opts.tmpdir)?);
                        tagged_keys.clear();
                    }
                }
                if !keys.is_empty() {
                    runs.all.push(extsort::write_run(&mut keys, opts.tmpdir)?);
                }
                if !tagged_keys.is_empty() {
                    runs.tagged
                        .push(extsort::write_run(&mut tagged_keys, opts.tmpdir)?);
                }
                Ok(runs)
            }));
        }
        drop(rx);

        let mut sorted_by_type = false;
        let mut seen_header = false;
        let mut timestamp = None;
        let mut blocks = 0;
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
                        timestamp = header.osmosis_replication_timestamp();
                        if timestamp.is_none() {
                            bail!("PBF header has no osmosis_replication_timestamp");
                        }
                        seen_header = true;
                    }
                    osmpbf::BlobType::OsmData => {
                        if !seen_header {
                            bail!("PBF data before header");
                        }
                        if sorted_by_type && done.load(Ordering::Relaxed) {
                            break;
                        }
                        if opts.max_blocks.is_some_and(|max| blocks >= max) {
                            break;
                        }
                        blocks += 1;
                        if tx.send(blob).is_err() {
                            break; // A worker failed; its error is reported below.
                        }
                    }
                    osmpbf::BlobType::Unknown(_) => {}
                }
            }
            if !seen_header {
                bail!("PBF input has no header");
            }
            Ok(())
        })();
        drop(tx);
        for w in workers {
            let r = w.join().expect("worker panicked")?;
            runs.all.extend(r.all);
            runs.tagged.extend(r.tagged);
        }
        read_result?;
        Ok(timestamp.expect("checked when reading the header"))
    })?;

    let stats = Stats {
        nodes: nodes.into_inner(),
        tagged: tagged.into_inner(),
        skipped: skipped.into_inner(),
        timestamp,
    };
    Ok((runs, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_keys() {
        for k in [
            "source",
            "source:date",
            "source_ref",
            "created_by",
            "note",
            "note:de",
        ] {
            assert!(is_meta_key(k), "{k}");
        }
        for k in [
            "fixme",
            "FIXME",
            "attribution",
            "check_date",
            "check_date:opening_hours",
        ] {
            assert!(is_meta_key(k), "{k}");
        }
        for k in ["amenity", "name", "addr:street", "natural", "notes_count"] {
            assert!(!is_meta_key(k), "{k}");
        }
    }

    #[test]
    fn tags_count_unless_all_meta() {
        assert!(!has_tags(std::iter::empty()));
        assert!(!has_tags([("source", "survey"), ("note", "x")].into_iter()));
        assert!(has_tags(
            [("source", "survey"), ("highway", "crossing")].into_iter()
        ));
        assert!(!has_tags(
            [("addr:street", "A"), ("addr:housenumber", "1")].into_iter()
        ));
        assert!(has_tags(
            [("addr:street", "A"), ("shop", "bakery")].into_iter()
        ));
        assert!(!counts_as_tag("survey:date"));
        assert!(counts_as_tag("address"), "not an addr: key");
    }
}
