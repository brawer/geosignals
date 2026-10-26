// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Turns sorted per-pixel counts into the compressed tiles of a
//! Cloud-Optimized GeoTIFF, encoding tiles on several threads.

use crate::cog::Spool;
use crate::histogram::Histogram;
use crate::pyramid::{self, Counts};
use crate::tile::MAX_TILE_ZOOM;
use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// Builds the pyramid from `(pixel key, count)` pairs in ascending key
/// order, turns each tile's counts into pixel values with
/// `to_pixels(z, y, counts)`, and adds the compressed tiles to `spool`.
/// Returns the histogram of the full-resolution pixels that were
/// emitted; tiles without data are not counted.
pub fn build(
    pixels: impl IntoIterator<Item = (u64, u64)>,
    threads: usize,
    spool: &mut Spool,
    to_pixels: impl Fn(u8, u32, &Counts) -> Vec<u16> + Sync,
) -> Result<Histogram> {
    let mut hist = Histogram::default();
    std::thread::scope(|s| -> Result<()> {
        type Job = (u64, u8, u32, u32, Vec<u16>);
        let (job_tx, job_rx) = crossbeam_channel::bounded::<Job>(threads * 8);
        let (res_tx, res_rx) =
            crossbeam_channel::bounded::<(u64, u8, u32, u32, Result<Vec<u8>>)>(threads * 8);
        let mut workers = Vec::new();
        for _ in 0..threads.max(1) {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            workers.push(s.spawn(move || {
                let mut hist = Histogram::default();
                for (seq, z, x, y, px) in job_rx {
                    if z == MAX_TILE_ZOOM {
                        hist.add(&px);
                    }
                    if res_tx
                        .send((seq, z, x, y, crate::encode::zstd(&px)))
                        .is_err()
                    {
                        break;
                    }
                }
                hist
            }));
        }
        drop((job_rx, res_tx));
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
        let to_pixels = &to_pixels;
        let mut builder = pyramid::Builder::new(|z, x, y, counts: &Counts| {
            job_tx
                .send((seq, z, x, y, to_pixels(z, y, counts)))
                .context("encoder failed")?;
            seq += 1;
            Ok(())
        });
        let built = pixels
            .into_iter()
            .try_for_each(|(key, count)| builder.add(key, count))
            .and_then(|()| builder.finish());
        drop(job_tx);
        let collected = collector.join().expect("collector panicked");
        for w in workers {
            hist.merge(&w.join().expect("encoder panicked"));
        }
        built?;
        collected
    })?;
    Ok(hist)
}
