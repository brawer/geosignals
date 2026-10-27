// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Counts distinct observers per pixel, at every zoom level.
//!
//! Distinct counts do not add up: someone who reported from two
//! neighboring pixels is one observer of their parent, not two. So
//! unlike node density, overviews cannot sum their children. Instead
//! we start from the set of (observer, zoom-16 pixel) pairs, sorted by
//! observer and then by the pixel's Hilbert key. Shifting the key right
//! by two bits gives the parent pixel, and since that keeps the pairs
//! sorted, dropping repeats of the previous pair gives the distinct
//! (observer, pixel) pairs of each coarser level in one pass. Each
//! level's pixel keys then go through an external sort that counts
//! them, which yields the number of observers per pixel in Hilbert
//! order, and so tile by tile.

use crate::extsort::{self, Run};
use anyhow::Result;
use std::path::Path;

/// Zoom level of the finest pixels, about 611 m wide at the equator.
pub const PIXEL_ZOOM: u8 = 16;

/// Zoom level of the finest tiles, 256 pixels wide.
pub const MAX_TILE_ZOOM: u8 = PIXEL_ZOOM - crate::tile::TILE_BITS;

/// Hilbert key of a zoom-16 pixel, for a location in degrees.
pub fn pixel_key(lon: f64, lat: f64) -> Option<u32> {
    let (x, y) = crate::tile::pixel_xy(lon, lat)?;
    let shift = crate::tile::PIXEL_ZOOM - PIXEL_ZOOM;
    let h: u64 = fast_hilbert::xy2h(x >> shift, y >> shift, PIXEL_ZOOM);
    Some(h as u32)
}

/// A pair as the external sort keeps it: observer in the high half,
/// pixel key in the low one.
#[inline]
pub fn pair(observer: u32, pixel: u32) -> u64 {
    (u64::from(observer) << 32) | u64::from(pixel)
}

/// Reads distinct pairs in ascending order and returns, for each tile
/// zoom level `z` from 0 to [`MAX_TILE_ZOOM`], sorted runs of the
/// Hilbert keys of the zoom `z + 8` pixels, one key per distinct
/// observer of that pixel. At most `run_size` keys are held in memory.
pub fn level_runs(
    pairs: impl Iterator<Item = Result<u64>>,
    tmpdir: &Path,
    run_size: usize,
) -> Result<Vec<Vec<Run>>> {
    let levels = MAX_TILE_ZOOM as usize + 1;
    let mut bufs: Vec<Vec<u64>> = vec![Vec::new(); levels];
    let mut runs: Vec<Vec<Run>> = (0..levels).map(|_| Vec::new()).collect();
    let mut last: Vec<Option<u64>> = vec![None; levels];
    let mut held = 0usize;
    for p in pairs {
        let p = p?;
        let (observer, pixel) = (p >> 32, p & 0xffff_ffff);
        for z in 0..levels {
            let shift = 2 * (MAX_TILE_ZOOM as usize - z);
            let key = (observer << 32) | (pixel >> shift);
            if last[z] != Some(key) {
                last[z] = Some(key);
                bufs[z].push(pixel >> shift);
                held += 1;
            }
        }
        if held >= run_size.max(levels) {
            spill(&mut bufs, &mut runs, tmpdir)?;
            held = 0;
        }
    }
    spill(&mut bufs, &mut runs, tmpdir)?;
    Ok(runs)
}

fn spill(bufs: &mut [Vec<u64>], runs: &mut [Vec<Run>], tmpdir: &Path) -> Result<()> {
    for (buf, runs) in bufs.iter_mut().zip(runs.iter_mut()) {
        if !buf.is_empty() {
            runs.push(extsort::write_run(buf, tmpdir)?);
            buf.clear();
        }
    }
    Ok(())
}

/// Streams the distinct values of runs, in ascending order.
pub fn distinct(runs: Vec<Run>) -> Result<impl Iterator<Item = Result<u64>>> {
    Ok(extsort::merge(runs)?.map(|r| r.map(|(key, _count)| key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn counts_distinct_observers_per_level() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = 5u64;
        let mut pairs = BTreeSet::new();
        for _ in 0..5000 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let observer = (s >> 60) as u32;
            // Most in one small area, so pixels share observers.
            let pixel = if s & 3 == 0 {
                (s >> 20) as u32
            } else {
                0x1234_0000 + ((s >> 40) as u32 & 0xfff)
            };
            pairs.insert(pair(observer, pixel));
        }
        let runs = level_runs(pairs.iter().map(|&p| Ok(p)), dir.path(), 777).unwrap();
        for (z, runs) in runs.into_iter().enumerate() {
            let shift = 2 * (MAX_TILE_ZOOM as usize - z);
            let mut want: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
            for &p in &pairs {
                want.entry((p & 0xffff_ffff) >> shift)
                    .or_default()
                    .insert(p >> 32);
            }
            let want: Vec<(u64, u64)> =
                want.into_iter().map(|(k, o)| (k, o.len() as u64)).collect();
            let got: Vec<(u64, u64)> = extsort::merge(runs).unwrap().map(|r| r.unwrap()).collect();
            assert_eq!(got, want, "zoom {z}");
        }
    }

    #[test]
    fn pixel_keys_nest() {
        // Zürich main station.
        let h = pixel_key(8.5402, 47.3782).unwrap();
        let (x, y): (u32, u32) = fast_hilbert::h2xy(u64::from(h), PIXEL_ZOOM);
        assert_eq!((x >> 8, y >> 8), (134, 89)); // zoom-8 tile
        assert_eq!(pixel_key(0.0, 89.0), None);
    }
}
