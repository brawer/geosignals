// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! External sort of `u64` keys that counts how often each key occurs.
//!
//! Keys are collected into memory buffers. A full buffer is sorted,
//! run-length encoded into `(key, count)` pairs, and spilled to a
//! zstd-compressed temporary file holding delta-encoded varints.
//! [`merge`] then streams all runs in key order, summing the counts
//! of equal keys.

use anyhow::{Context, Result};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use tempfile::{NamedTempFile, TempPath};

/// A sorted run of `(key, count)` pairs, stored in a temporary file.
pub struct Run {
    /// Deleted when dropped. Not kept open, so runs cost no file
    /// handles while they wait to be merged.
    file: TempPath,
    pub len: u64,
}

/// Sorts `keys`, counts duplicates and writes them as a run.
pub fn write_run(keys: &mut [u64], tmpdir: &Path) -> Result<Run> {
    keys.sort_unstable();
    write_sorted(keys.iter().map(|&k| Ok((k, 1))), tmpdir)
}

/// Writes the keys of several sorted slices as one run, counting
/// duplicates within and across them. Unlike concatenating them for
/// [`write_run`], this needs no memory beyond the slices themselves.
pub fn write_merged(parts: &[Vec<u64>], tmpdir: &Path) -> Result<Run> {
    debug_assert!(parts.iter().all(|p| p.is_sorted()));
    let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> = parts
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.first().map(|&k| Reverse((k, i, 0))))
        .collect();
    let keys = std::iter::from_fn(|| {
        let Reverse((key, i, j)) = heap.pop()?;
        if let Some(&next) = parts[i].get(j + 1) {
            heap.push(Reverse((next, i, j + 1)));
        }
        Some(Ok((key, 1)))
    });
    write_sorted(keys, tmpdir)
}

/// Writes `(key, count)` pairs that come in key order as a run,
/// summing the counts of equal keys.
fn write_sorted(keys: impl Iterator<Item = Result<(u64, u64)>>, tmpdir: &Path) -> Result<Run> {
    let file = NamedTempFile::new_in(tmpdir).context("creating run file")?;
    let mut enc = zstd::Encoder::new(BufWriter::with_capacity(1 << 20, file.reopen()?), 1)?;
    let mut buf = Vec::with_capacity(1 << 16);
    let mut prev = 0u64;
    let mut len = 0u64;
    let mut keys = keys.peekable();
    while let Some(next) = keys.next() {
        let (key, mut count) = next?;
        while let Some(Ok((_, c))) = keys.next_if(|n| matches!(n, Ok((k, _)) if *k == key)) {
            count += c;
        }
        put_varint(&mut buf, key - prev);
        put_varint(&mut buf, count);
        prev = key;
        len += 1;
        if buf.len() >= (1 << 16) - 32 {
            enc.write_all(&buf)?;
            buf.clear();
        }
    }
    enc.write_all(&buf)?;
    enc.finish()?.flush()?;
    Ok(Run {
        file: file.into_temp_path(),
        len,
    })
}

/// Most runs that [`merge`] reads at once. Each costs an open file and
/// about half a megabyte of buffers.
const MAX_FAN_IN: usize = 64;

/// Streams the union of `runs` in key order, summing counts of equal keys.
/// Beyond [`MAX_FAN_IN`] runs, merges groups of them into longer runs
/// first, next to where they are.
pub fn merge(mut runs: Vec<Run>) -> Result<Merger> {
    while runs.len() > MAX_FAN_IN {
        let dir = runs[0]
            .file
            .parent()
            .context("run file has no directory")?
            .to_path_buf();
        let mut merged = Vec::with_capacity(runs.len().div_ceil(MAX_FAN_IN));
        while !runs.is_empty() {
            let rest = runs.split_off(runs.len().min(MAX_FAN_IN));
            let group = std::mem::replace(&mut runs, rest);
            merged.push(write_sorted(merge_runs(group)?, &dir)?);
        }
        runs = merged;
    }
    merge_runs(runs)
}

fn merge_runs(runs: Vec<Run>) -> Result<Merger> {
    let mut readers = Vec::with_capacity(runs.len());
    let mut heap = BinaryHeap::with_capacity(runs.len());
    for run in runs {
        let file = File::open(&run.file)?;
        let mut r = RunReader {
            dec: zstd::Decoder::with_buffer(BufReader::with_capacity(1 << 18, file))?,
            prev: 0,
            remaining: run.len,
            _file: run.file,
        };
        if let Some((key, count)) = r.next()? {
            heap.push(Reverse((key, count, readers.len())));
        }
        readers.push(r);
    }
    Ok(Merger { readers, heap })
}

struct RunReader {
    dec: zstd::Decoder<'static, BufReader<File>>,
    prev: u64,
    remaining: u64,
    _file: TempPath,
}

impl RunReader {
    fn next(&mut self) -> Result<Option<(u64, u64)>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        let delta = get_varint(&mut self.dec)?;
        let count = get_varint(&mut self.dec)?;
        self.prev += delta;
        Ok(Some((self.prev, count)))
    }
}

pub struct Merger {
    readers: Vec<RunReader>,
    heap: BinaryHeap<Reverse<(u64, u64, usize)>>,
}

impl Iterator for Merger {
    type Item = Result<(u64, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((key, mut total, idx)) = self.heap.pop()?;
        let mut advance = Some(idx);
        loop {
            if let Some(i) = advance.take() {
                match self.readers[i].next() {
                    Ok(Some((k, c))) => self.heap.push(Reverse((k, c, i))),
                    Ok(None) => {}
                    Err(e) => return Some(Err(e)),
                }
            }
            match self.heap.peek() {
                Some(Reverse((k, _, _))) if *k == key => {
                    let Reverse((_, c, i)) = self.heap.pop().unwrap();
                    total += c;
                    advance = Some(i);
                }
                _ => return Some(Ok((key, total))),
            }
        }
    }
}

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn get_varint<R: Read>(r: &mut R) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b).context("truncated run file")?;
        v |= u64::from(b[0] & 0x7f) << shift;
        if b[0] < 0x80 {
            return Ok(v);
        }
        shift += 7;
        anyhow::ensure!(shift < 64, "corrupt varint in run file");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn varint_round_trip() {
        for v in [0, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(get_varint(&mut buf.as_slice()).unwrap(), v);
        }
    }

    #[test]
    fn merges_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut expected = BTreeMap::new();
        let mut runs = Vec::new();
        let mut s = 99u64;
        for _ in 0..5 {
            let mut keys = Vec::new();
            for _ in 0..10_000 {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let key = (s >> 40) % 3000 + if s & 1 == 0 { 0 } else { 1 << 35 };
                keys.push(key);
                *expected.entry(key).or_insert(0u64) += 1;
            }
            runs.push(write_run(&mut keys, dir.path()).unwrap());
        }
        runs.push(write_run(&mut [], dir.path()).unwrap());
        let got: Vec<(u64, u64)> = merge(runs).unwrap().map(|r| r.unwrap()).collect();
        let want: Vec<(u64, u64)> = expected.into_iter().collect();
        assert_eq!(got, want);
    }

    #[test]
    fn writes_merged_parts() {
        let dir = tempfile::tempdir().unwrap();
        let parts = vec![vec![1, 3, 3, 9], vec![], vec![0, 3, 10], vec![9]];
        let run = write_merged(&parts, dir.path()).unwrap();
        assert_eq!(run.len, 5);
        let got: Vec<(u64, u64)> = merge(vec![run]).unwrap().map(Result::unwrap).collect();
        assert_eq!(got, [(0, 1), (1, 1), (3, 3), (9, 2), (10, 1)]);
    }

    #[test]
    fn merges_more_runs_than_it_opens_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let n = MAX_FAN_IN * MAX_FAN_IN + 3;
        let runs: Vec<Run> = (0..n as u64)
            .map(|i| write_run(&mut [i % 100, 1000 + i], dir.path()).unwrap())
            .collect();
        let got: Vec<(u64, u64)> = merge(runs).unwrap().map(Result::unwrap).collect();
        assert_eq!(got.len(), 100 + n);
        assert_eq!(got[0], (0, n.div_ceil(100) as u64));
        assert!(got[100..].iter().all(|&(_, c)| c == 1));
        assert!(got.is_sorted());
    }

    #[test]
    fn merge_of_nothing() {
        assert_eq!(merge(Vec::new()).unwrap().count(), 0);
    }
}
