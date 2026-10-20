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
use tempfile::NamedTempFile;

/// A sorted run of `(key, count)` pairs, stored in a temporary file.
pub struct Run {
    file: NamedTempFile,
    pub len: u64,
}

/// Sorts `keys`, counts duplicates and writes them as a run.
pub fn write_run(keys: &mut [u64], tmpdir: &Path) -> Result<Run> {
    keys.sort_unstable();
    let file = NamedTempFile::new_in(tmpdir).context("creating run file")?;
    let mut enc = zstd::Encoder::new(BufWriter::with_capacity(1 << 20, file.reopen()?), 1)?;
    let mut buf = Vec::with_capacity(1 << 16);
    let mut prev = 0u64;
    let mut len = 0u64;
    let mut i = 0;
    while i < keys.len() {
        let key = keys[i];
        let mut j = i + 1;
        while j < keys.len() && keys[j] == key {
            j += 1;
        }
        put_varint(&mut buf, key - prev);
        put_varint(&mut buf, (j - i) as u64);
        prev = key;
        len += 1;
        i = j;
        if buf.len() >= (1 << 16) - 32 {
            enc.write_all(&buf)?;
            buf.clear();
        }
    }
    enc.write_all(&buf)?;
    enc.finish()?.flush()?;
    Ok(Run { file, len })
}

/// Streams the union of `runs` in key order, summing counts of equal keys.
pub fn merge(runs: Vec<Run>) -> Result<Merger> {
    let mut readers = Vec::with_capacity(runs.len());
    let mut heap = BinaryHeap::with_capacity(runs.len());
    for run in runs {
        let file = File::open(run.file.path())?;
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
    _file: NamedTempFile,
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
    fn merge_of_nothing() {
        assert_eq!(merge(Vec::new()).unwrap().count(), 0);
    }
}
