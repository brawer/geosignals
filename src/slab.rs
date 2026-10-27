// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Large arrays of small records, kept in memory-mapped scratch files
//! instead of the heap.
//!
//! A [`Writer`] appends records to an unnamed temporary file, and
//! [`Writer::finish`] maps that file into memory. The kernel then pages
//! the records in when they are read and drops them again when memory
//! gets tight, so a job can work on more data than fits in its RAM, as
//! long as the scratch directory is on disk (not a RAM disk). The file
//! has no name, so it goes away when the slab is dropped, even if the
//! process crashes.
//!
//! Without a scratch directory, a slab is a plain `Vec`, which is
//! simpler for tests and small data.

use anyhow::{Context, Result};
use std::io::{BufWriter, Write};
use std::path::Path;

/// A record type that can live in a slab: plain data with a fixed
/// layout, where any bit pattern is a valid value.
///
/// # Safety
///
/// The type must be `#[repr(C)]`, have only integer fields, and have a
/// size of `SIZE`. `write` must write all `SIZE` bytes in native byte
/// order at the fields' offsets, so that reading the bytes back as the
/// type gives the same value.
pub unsafe trait Record: Copy {
    const SIZE: usize;
    fn write(&self, out: &mut [u8]);
}

/// An array of records, on the heap or in a mapped file.
pub enum Slab<T> {
    Heap(Vec<T>),
    Mapped { map: memmap2::MmapMut, len: usize },
}

impl<T: Record> Slab<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            Slab::Heap(v) => v,
            // SAFETY: the map holds `len` records written by `Writer`,
            // starts at a page boundary, and `Record` types accept any
            // bit pattern.
            Slab::Mapped { map, len } => unsafe {
                std::slice::from_raw_parts(map.as_ptr().cast::<T>(), *len)
            },
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match self {
            Slab::Heap(v) => v,
            // SAFETY: as in `as_slice`; we hold the only reference.
            Slab::Mapped { map, len } => unsafe {
                std::slice::from_raw_parts_mut(map.as_mut_ptr().cast::<T>(), *len)
            },
        }
    }
}

impl<T: Record> std::ops::Deref for Slab<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<'a, T: Record> IntoIterator for &'a Slab<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Slab::Heap(Vec::new())
    }
}

impl<T> From<Vec<T>> for Slab<T> {
    fn from(v: Vec<T>) -> Self {
        Slab::Heap(v)
    }
}

impl<T: Record> Clone for Slab<T> {
    fn clone(&self) -> Self {
        Slab::Heap(self.as_slice().to_vec())
    }
}

impl<T: Record + PartialEq> PartialEq for Slab<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Record + std::fmt::Debug> std::fmt::Debug for Slab<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_slice().fmt(f)
    }
}

/// Appends records to a new slab.
pub enum Writer<T> {
    Heap(Vec<T>),
    File {
        out: BufWriter<std::fs::File>,
        buf: Vec<u8>,
        len: usize,
    },
}

impl<T: Record> Writer<T> {
    /// A writer into an unnamed file in `dir`, or onto the heap if
    /// `dir` is `None`.
    pub fn new(dir: Option<&Path>) -> Result<Self> {
        const { assert!(std::mem::size_of::<T>() == T::SIZE) };
        Ok(match dir {
            None => Writer::Heap(Vec::new()),
            Some(dir) => Writer::File {
                out: BufWriter::with_capacity(
                    1 << 20,
                    tempfile::tempfile_in(dir)
                        .with_context(|| format!("creating a scratch file in {}", dir.display()))?,
                ),
                buf: vec![0; T::SIZE],
                len: 0,
            },
        })
    }

    pub fn len(&self) -> usize {
        match self {
            Writer::Heap(v) => v.len(),
            Writer::File { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn push(&mut self, record: T) -> Result<()> {
        match self {
            Writer::Heap(v) => v.push(record),
            Writer::File { out, buf, len } => {
                record.write(buf);
                out.write_all(buf)?;
                *len += 1;
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Slab<T>> {
        match self {
            Writer::Heap(v) => Ok(Slab::Heap(v)),
            Writer::File { len: 0, .. } => Ok(Slab::Heap(Vec::new())),
            Writer::File { out, len, .. } => {
                let file = out.into_inner().map_err(|e| e.into_error())?;
                // SAFETY: the file has no name and only we hold it, so
                // nothing else can change it while it is mapped.
                let map = unsafe { memmap2::MmapMut::map_mut(&file) }
                    .context("mapping a scratch file")?;
                anyhow::ensure!(map.len() == len * T::SIZE, "short scratch file");
                Ok(Slab::Mapped { map, len })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Rec {
        a: u32,
        b: u16,
        c: u8,
    }

    unsafe impl Record for Rec {
        const SIZE: usize = 8;
        fn write(&self, out: &mut [u8]) {
            out[0..4].copy_from_slice(&self.a.to_ne_bytes());
            out[4..6].copy_from_slice(&self.b.to_ne_bytes());
            out[6] = self.c;
            out[7] = 0;
        }
    }

    fn records(n: u32) -> Vec<Rec> {
        (0..n)
            .map(|i| Rec {
                a: i.wrapping_mul(2_654_435_761),
                b: i as u16,
                c: (i % 7) as u8,
            })
            .collect()
    }

    #[test]
    fn mapped_and_heap_agree() {
        let dir = tempfile::tempdir().unwrap();
        let want = records(300_000);
        let mut slabs = Vec::new();
        for d in [None, Some(dir.path())] {
            let mut w = Writer::new(d).unwrap();
            for r in &want {
                w.push(*r).unwrap();
            }
            assert_eq!(w.len(), want.len());
            slabs.push(w.finish().unwrap());
        }
        assert!(matches!(slabs[1], Slab::Mapped { .. }));
        assert_eq!(slabs[0].as_slice(), want.as_slice());
        assert_eq!(slabs[1].as_slice(), want.as_slice());
        assert_eq!(slabs[0], slabs[1]);

        // Sorting works in place, in the mapped file too.
        let mut sorted = want.clone();
        sorted.sort_unstable_by_key(|r| r.a);
        slabs[1].as_mut_slice().sort_unstable_by_key(|r| r.a);
        assert_eq!(slabs[1].as_slice(), sorted.as_slice());

        // The scratch file has no name, so nothing is left behind.
        drop(slabs);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn empty() {
        let dir = tempfile::tempdir().unwrap();
        let w: Writer<Rec> = Writer::new(Some(dir.path())).unwrap();
        assert!(w.is_empty());
        assert!(w.finish().unwrap().is_empty());
    }
}
