// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Parallel extraction of node sort keys from an OSM PBF stream.
//!
//! One thread reads raw blobs; worker threads decompress them, turn each
//! node into its Hilbert key, and collect the keys into sorted runs on disk.
//! Keys of nodes with tags go into a second set of runs as well; see
//! [`counts_as_tag`] for the tags that count.
//!
//! Ways that describe a point of interest ([`is_poi`]) count as one
//! tagged node, at their first node. Since the planet stores nodes before
//! ways, this takes a second pass: [`sorted_runs`] collects the first
//! node IDs of such ways, and [`way_points`] reads the nodes again to
//! look up where they are.

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
    /// Ways with a point-of-interest tag; see [`is_poi`].
    pub poi_ways: u64,
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
    /// Also read the ways, to collect the first nodes of [`is_poi`] ways.
    pub poi_ways: bool,
}

/// Sorted runs of node keys.
#[derive(Default)]
pub struct Runs {
    /// Keys of all nodes.
    pub all: Vec<Run>,
    /// Keys of the nodes with tags; see [`Stats::tagged`].
    pub tagged: Vec<Run>,
    /// IDs of the first nodes of [`is_poi`] ways, for [`way_points`].
    pub way_nodes: Vec<Run>,
}

/// Keys of tags that make a way a point of interest: something people
/// look for, rather than a building, road or patch of land.
const POI_KEYS: &[&str] = &[
    "amenity",
    "shop",
    "tourism",
    "leisure",
    "office",
    "craft",
    "healthcare",
    "historic",
    "man_made",
    "public_transport",
];

/// Whether a way with these tags counts as a point of interest.
pub fn is_poi<'a>(mut tags: impl Iterator<Item = (&'a str, &'a str)>) -> bool {
    tags.any(|(k, _)| POI_KEYS.contains(&k))
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
/// With [`Options::poi_ways`], it stops at the first block of relations.
pub fn sorted_runs<R: Read + Send>(input: R, opts: &Options) -> Result<(Runs, Stats)> {
    let nodes = AtomicU64::new(0);
    let tagged = AtomicU64::new(0);
    let poi_ways = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::bounded::<osmpbf::Blob>(opts.threads * 4);
    let mut runs = Runs::default();

    let timestamp = std::thread::scope(|s| -> Result<i64> {
        let mut workers = Vec::new();
        for _ in 0..opts.threads.max(1) {
            let rx = rx.clone();
            let (nodes, tagged, skipped, done) = (&nodes, &tagged, &skipped, &done);
            let poi_ways = &poi_ways;
            workers.push(s.spawn(move || -> Result<Runs> {
                let mut runs = Runs::default();
                let mut keys: Vec<u64> = Vec::with_capacity(opts.run_size);
                let mut tagged_keys: Vec<u64> = Vec::new();
                let mut way_nodes: Vec<u64> = Vec::new();
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
                        if opts.poi_ways {
                            for way in group.ways() {
                                if is_poi(way.tags())
                                    && let Some(id) = way.refs().next()
                                {
                                    way_nodes.push(id as u64);
                                }
                            }
                        }
                        let past_nodes = group.relations().len() > 0
                            || (group.ways().len() > 0 && !opts.poi_ways);
                        if past_nodes {
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
                    if way_nodes.len() >= opts.run_size {
                        poi_ways.fetch_add(way_nodes.len() as u64, Ordering::Relaxed);
                        runs.way_nodes
                            .push(extsort::write_run(&mut way_nodes, opts.tmpdir)?);
                        way_nodes.clear();
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
                if !way_nodes.is_empty() {
                    poi_ways.fetch_add(way_nodes.len() as u64, Ordering::Relaxed);
                    runs.way_nodes
                        .push(extsort::write_run(&mut way_nodes, opts.tmpdir)?);
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
            runs.way_nodes.extend(r.way_nodes);
        }
        read_result?;
        Ok(timestamp.expect("checked when reading the header"))
    })?;

    let stats = Stats {
        nodes: nodes.into_inner(),
        tagged: tagged.into_inner(),
        poi_ways: poi_ways.into_inner(),
        skipped: skipped.into_inner(),
        timestamp,
    };
    Ok((runs, stats))
}

/// Reads the nodes of `input` again and returns sorted runs with the key
/// of every node in `way_nodes`, as often as it occurs there, and how
/// many of them were found inside the Web Mercator range.
///
/// Blocks are decoded in parallel but joined in file order, which must
/// be by node ID, against the sorted IDs in `way_nodes`. So memory stays
/// bounded however many ways there are.
pub fn way_points<R: Read + Send>(
    input: R,
    opts: &Options,
    way_nodes: Vec<Run>,
) -> Result<(Vec<Run>, u64)> {
    if way_nodes.is_empty() {
        return Ok((Vec::new(), 0));
    }
    type Decoded = (u64, Result<Vec<(u64, Option<u64>)>>);
    let done = AtomicBool::new(false);
    let (tx, rx) = crossbeam_channel::bounded::<(u64, osmpbf::Blob)>(opts.threads * 4);
    let (res_tx, res_rx) = crossbeam_channel::bounded::<Decoded>(opts.threads * 4);
    let mut wanted = extsort::merge(way_nodes)?.peekable();
    let mut runs = Vec::new();
    let mut keys: Vec<u64> = Vec::new();
    let mut found = 0u64;

    std::thread::scope(|s| -> Result<()> {
        for _ in 0..opts.threads.max(1) {
            let (rx, res_tx, done) = (rx.clone(), res_tx.clone(), &done);
            s.spawn(move || {
                for (seq, blob) in rx {
                    let decoded = (|| {
                        let mut out = Vec::new();
                        let BlobDecode::OsmData(block) = blob.decode()? else {
                            return Ok(out);
                        };
                        for group in block.groups() {
                            for n in group.dense_nodes() {
                                out.push((n.id() as u64, tile::node_key(n.lon(), n.lat())));
                            }
                            for n in group.nodes() {
                                out.push((n.id() as u64, tile::node_key(n.lon(), n.lat())));
                            }
                            if group.ways().len() > 0 || group.relations().len() > 0 {
                                done.store(true, Ordering::Relaxed);
                            }
                        }
                        Ok(out)
                    })();
                    if res_tx.send((seq, decoded)).is_err() {
                        break;
                    }
                }
            });
        }
        drop((rx, res_tx));
        let done = &done;
        let reader = s.spawn(move || -> Result<()> {
            let mut seq = 0;
            for blob in BlobReader::new(input) {
                let blob = blob.context("reading PBF blob")?;
                if blob.get_type() != osmpbf::BlobType::OsmData {
                    continue;
                }
                if done.load(Ordering::Relaxed) {
                    break;
                }
                if tx.send((seq, blob)).is_err() {
                    break; // The join failed; its error is reported below.
                }
                seq += 1;
            }
            Ok(())
        });

        let joined = (|| -> Result<()> {
            let mut pending = std::collections::BTreeMap::new();
            let (mut next, mut last_id) = (0u64, 0u64);
            for (seq, decoded) in res_rx.iter() {
                pending.insert(seq, decoded);
                while let Some(decoded) = pending.remove(&next) {
                    next += 1;
                    for (id, key) in decoded? {
                        if id <= last_id && last_id != 0 {
                            bail!("nodes are not sorted by ID: {id} after {last_id}");
                        }
                        last_id = id;
                        // Skip ways whose first node is missing.
                        loop {
                            match wanted.peek() {
                                Some(Ok((w, _))) if *w < id => {
                                    wanted.next();
                                }
                                Some(Err(_)) => {
                                    wanted.next().expect("peeked")?;
                                }
                                _ => break,
                            }
                        }
                        let count = match wanted.peek() {
                            Some(Ok((w, c))) if *w == id => {
                                let c = *c;
                                wanted.next();
                                c
                            }
                            None => {
                                // Every way found; no need to read further.
                                done.store(true, Ordering::Relaxed);
                                continue;
                            }
                            _ => continue,
                        };
                        if let Some(k) = key {
                            found += count;
                            keys.extend(std::iter::repeat_n(k, count as usize));
                            if keys.len() >= opts.run_size {
                                runs.push(extsort::write_run(&mut keys, opts.tmpdir)?);
                                keys.clear();
                            }
                        }
                    }
                }
            }
            Ok(())
        })();
        drop(res_rx);
        let read = reader.join().expect("reader panicked");
        joined?;
        read
    })?;
    if !keys.is_empty() {
        runs.push(extsort::write_run(&mut keys, opts.tmpdir)?);
    }
    Ok((runs, found))
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
    fn poi_ways() {
        assert!(is_poi(
            [("building", "yes"), ("shop", "bakery")].into_iter()
        ));
        assert!(is_poi([("leisure", "park")].into_iter()));
        assert!(!is_poi([("building", "yes"), ("name", "X")].into_iter()));
        assert!(!is_poi([("highway", "residential")].into_iter()));
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
