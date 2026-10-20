// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Single-pass construction of the tile pyramid from a key-sorted stream.
//!
//! The input is a stream of `(pixel key, count)` pairs, sorted by Hilbert
//! key. We keep only the chain of tiles from the root down to the zoom-10
//! tile that is currently being filled. When the stream moves on to a
//! different tile, the tiles it left can never receive more data, so we
//! emit them and add their counts into their parent, 2×2 pixels into one.
//! Peak memory is 11 tiles of 256×256 counters, independent of input size.

use crate::tile::{self, MAX_TILE_ZOOM, TILE_BITS, TILE_SIZE};
use anyhow::{Result, ensure};

/// Node counts per pixel of one tile, row by row.
pub type Counts = [u64];

struct Level {
    x: u32,
    y: u32,
    counts: Box<[u64]>,
}

pub struct Builder<F: FnMut(u8, u32, u32, &Counts) -> Result<()>> {
    levels: Vec<Option<Level>>,
    spare: Vec<Box<[u64]>>,
    last_key: Option<u64>,
    emit: F,
}

impl<F: FnMut(u8, u32, u32, &Counts) -> Result<()>> Builder<F> {
    /// `emit(z, x, y, counts)` is called once for every non-empty tile,
    /// children before parents.
    pub fn new(emit: F) -> Self {
        Builder {
            levels: (0..=MAX_TILE_ZOOM).map(|_| None).collect(),
            spare: Vec::new(),
            last_key: None,
            emit,
        }
    }

    /// Adds `count` nodes to the pixel with `key`. Keys must be ascending.
    pub fn add(&mut self, key: u64, count: u64) -> Result<()> {
        ensure!(
            self.last_key.is_none_or(|k| k < key),
            "keys not strictly ascending"
        );
        self.last_key = Some(key);
        let (px, py) = tile::key_to_pixel(key);
        let (tx, ty) = (px >> TILE_BITS, py >> TILE_BITS);

        // Find the deepest level whose tile is an ancestor of (tx, ty).
        let mut keep = 0usize;
        for z in 0..=MAX_TILE_ZOOM as usize {
            let shift = MAX_TILE_ZOOM as usize - z;
            match &self.levels[z] {
                Some(l) if l.x == tx >> shift && l.y == ty >> shift => keep = z + 1,
                _ => break,
            }
        }
        self.flush_below(keep)?;
        for z in keep..=MAX_TILE_ZOOM as usize {
            let shift = MAX_TILE_ZOOM as usize - z;
            let counts = self
                .spare
                .pop()
                .unwrap_or_else(|| vec![0u64; TILE_SIZE * TILE_SIZE].into_boxed_slice());
            self.levels[z] = Some(Level {
                x: tx >> shift,
                y: ty >> shift,
                counts,
            });
        }
        let leaf = self.levels[MAX_TILE_ZOOM as usize].as_mut().unwrap();
        let mask = TILE_SIZE as u32 - 1;
        leaf.counts[((py & mask) as usize) * TILE_SIZE + (px & mask) as usize] += count;
        Ok(())
    }

    /// Emits all remaining tiles.
    pub fn finish(mut self) -> Result<()> {
        self.flush_below(0)
    }

    /// Emits the tiles at levels `>= keep`, deepest first, and folds each
    /// into its parent.
    fn flush_below(&mut self, keep: usize) -> Result<()> {
        for z in (keep..=MAX_TILE_ZOOM as usize).rev() {
            let Some(mut level) = self.levels[z].take() else {
                continue;
            };
            (self.emit)(z as u8, level.x, level.y, &level.counts)?;
            if z > 0 {
                let parent = self.levels[z - 1].as_mut().expect("parent chain");
                let half = TILE_SIZE / 2;
                let ox = (level.x as usize & 1) * half;
                let oy = (level.y as usize & 1) * half;
                for y in 0..TILE_SIZE {
                    let row = &level.counts[y * TILE_SIZE..(y + 1) * TILE_SIZE];
                    let prow = (oy + y / 2) * TILE_SIZE + ox;
                    for (x, &c) in row.iter().enumerate() {
                        parent.counts[prow + x / 2] += c;
                    }
                }
            }
            level.counts.fill(0);
            self.spare.push(level.counts);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    type Tiles = BTreeMap<(u8, u32, u32), Vec<u64>>;

    fn build(pixels: &[(u32, u32, u64)]) -> (Tiles, Vec<(u8, u32, u32)>) {
        let mut sorted: Vec<(u64, u64)> = pixels
            .iter()
            .map(|&(x, y, c)| (tile::pixel_key(x, y), c))
            .collect();
        sorted.sort();
        let mut tiles = Tiles::new();
        let mut order = Vec::new();
        let mut b = Builder::new(|z, x, y, counts: &Counts| {
            order.push((z, x, y));
            assert!(
                tiles.insert((z, x, y), counts.to_vec()).is_none(),
                "tile emitted twice"
            );
            Ok(())
        });
        for (k, c) in sorted {
            b.add(k, c).unwrap();
        }
        b.finish().unwrap();
        (tiles, order)
    }

    /// Straightforward reference: sum pixels into every zoom level.
    fn reference(pixels: &[(u32, u32, u64)]) -> Tiles {
        let mut tiles = Tiles::new();
        for &(px, py, c) in pixels {
            for z in 0..=MAX_TILE_ZOOM {
                let shift = (MAX_TILE_ZOOM - z) as u32;
                let (x, y) = (px >> shift, py >> shift);
                let t = tiles
                    .entry((z, x >> TILE_BITS, y >> TILE_BITS))
                    .or_insert_with(|| vec![0; TILE_SIZE * TILE_SIZE]);
                t[(y as usize % TILE_SIZE) * TILE_SIZE + x as usize % TILE_SIZE] += c;
            }
        }
        tiles
    }

    #[test]
    fn matches_reference() {
        let mut pixels = Vec::new();
        let mut s = 7u64;
        for i in 0..3000u64 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Cluster most points in a small region, spread the rest.
            let (x, y) = if i % 16 != 0 {
                (
                    137_000 + (s >> 50) as u32 % 2000,
                    91_000 + (s >> 30) as u32 % 2000,
                )
            } else {
                ((s >> 46) as u32, (s >> 20) as u32 & 0x3ffff)
            };
            if !pixels.iter().any(|&(a, b, _)| a == x && b == y) {
                pixels.push((x, y, 1 + s % 5));
            }
        }
        pixels.push((0, 0, 1));
        pixels.push(((1 << 18) - 1, (1 << 18) - 1, 3));
        let (got, order) = build(&pixels);
        assert_eq!(got, reference(&pixels));

        // Children are emitted before their parents.
        let pos: BTreeMap<_, _> = order.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        for &(z, x, y) in &order {
            if z > 0 {
                assert!(pos[&(z, x, y)] < pos[&(z - 1, x / 2, y / 2)]);
            }
        }
        // Within a zoom level, tiles come out in PMTiles (Hilbert) order.
        for z in 0..=MAX_TILE_ZOOM {
            let ids: Vec<u64> = order
                .iter()
                .filter(|t| t.0 == z)
                .map(|&(z, x, y)| fast_hilbert::xy2h(x, y, z))
                .collect();
            assert!(ids.windows(2).all(|w| w[0] < w[1]), "zoom {z}");
        }
    }

    #[test]
    fn empty_input_emits_nothing() {
        assert!(build(&[]).0.is_empty());
    }

    #[test]
    fn rejects_unsorted_keys() {
        let mut b = Builder::new(|_, _, _, _: &Counts| Ok(()));
        b.add(10, 1).unwrap();
        assert!(b.add(10, 1).is_err());
        assert!(b.add(5, 1).is_err());
    }
}
