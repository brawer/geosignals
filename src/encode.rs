// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Turns per-pixel node counts into compressed tiles.
//!
//! Each pixel stores node density (nodes per km² of real Earth surface)
//! on a logarithmic scale with 100 steps per doubling:
//!
//! ```text
//! v = 0                      no nodes at all
//! log2(density) = 0.01·v − 15.01   otherwise
//! ```
//!
//! So each step is a 0.7% change in density, and the largest density on
//! the planet (about 2^19.4 nodes/km²) needs 3439, which fits in 12 bits.
//! The scale is fixed, not derived from the data, so values from
//! different months are comparable.

use crate::pyramid::Counts;
use crate::tile::{self, TILE_BITS, TILE_SIZE};
use anyhow::Result;

/// log2 of the density represented by pixel value 1, in nodes/km².
/// The sparsest possible pixel (one node in a zoom-8 pixel at the
/// equator) has 2^-14.6 nodes/km².
pub const LOG2_MIN_DENSITY: f64 = -15.0;

/// Pixel values per doubling of density.
pub const STEPS_PER_OCTAVE: f64 = 100.0;

/// Largest pixel value we write; 12 bits.
pub const MAX_VALUE: u16 = 4095;

/// Zstandard level for tiles. The output is written once a month and
/// downloaded many times, so we spend the CPU.
pub const ZSTD_LEVEL: i32 = 19;

/// Maps a density to a pixel value; 0 means "no nodes at all".
pub fn quantize(density: f64) -> u16 {
    if density <= 0.0 {
        return 0;
    }
    let v = 1.0 + (STEPS_PER_OCTAVE * (density.log2() - LOG2_MIN_DENSITY)).round();
    v.clamp(1.0, f64::from(MAX_VALUE)) as u16
}

/// Inverse of [`quantize`], returning the center of the value's bucket.
pub fn dequantize(v: u16) -> f64 {
    if v == 0 {
        0.0
    } else {
        ((f64::from(v) - 1.0) / STEPS_PER_OCTAVE + LOG2_MIN_DENSITY).exp2()
    }
}

/// Converts the counts of tile `z/_/y` into quantized pixel values.
pub fn pixels(z: u8, y: u32, counts: &Counts) -> Vec<u16> {
    let pixel_zoom = z + TILE_BITS;
    let mut out = vec![0u16; TILE_SIZE * TILE_SIZE];
    for row in 0..TILE_SIZE {
        let area = tile::pixel_area_km2(pixel_zoom, (y << TILE_BITS) + row as u32);
        let src = &counts[row * TILE_SIZE..(row + 1) * TILE_SIZE];
        let dst = &mut out[row * TILE_SIZE..(row + 1) * TILE_SIZE];
        for (d, &c) in dst.iter_mut().zip(src) {
            *d = quantize(c as f64 / area);
        }
    }
    out
}

/// Compresses pixels as a TIFF tile: little-endian u16, Zstandard,
/// no predictor. (On this sparse data, horizontal differencing makes
/// tiles about 25% bigger.)
pub fn zstd(pixels: &[u16]) -> Result<Vec<u8>> {
    let raw: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
    Ok(zstd::bulk::compress(&raw, ZSTD_LEVEL)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_round_trip() {
        assert_eq!(quantize(0.0), 0);
        assert_eq!(quantize(LOG2_MIN_DENSITY.exp2()), 1);
        assert_eq!(quantize(1e-9), 1, "any node is non-zero");
        assert_eq!(quantize(1e30), MAX_VALUE);
        assert_eq!(quantize(1.0), 1501);
        for v in 1..=MAX_VALUE {
            assert_eq!(quantize(dequantize(v)), v);
        }
        // Monotone.
        let mut prev = 0;
        for i in 0..5000 {
            let q = quantize(1e-6 * 1.01f64.powi(i));
            assert!(q >= prev);
            prev = q;
        }
    }

    #[test]
    fn densest_place_fits() {
        // Densest zoom-18 pixel of the 2026 planet: 6.8·10⁵ nodes/km².
        assert!(quantize(6.8e5) < 3500);
    }

    #[test]
    fn one_node_anywhere_is_visible() {
        // Largest pixel: zoom 8 at the equator.
        let mut counts = vec![0u64; TILE_SIZE * TILE_SIZE];
        counts[128 * TILE_SIZE] = 1;
        assert!(pixels(0, 0, &counts)[128 * TILE_SIZE] > 1);
    }

    #[test]
    fn zstd_decodes() {
        let px: Vec<u16> = (0..TILE_SIZE * TILE_SIZE)
            .map(|i| (i % 4001) as u16)
            .collect();
        let raw = zstd::bulk::decompress(&zstd(&px).unwrap(), 1 << 17).unwrap();
        assert_eq!(raw.len(), 2 * px.len());
        assert_eq!(&raw[2..4], &1u16.to_le_bytes());
        assert_eq!(&raw[8000..8002], &4000u16.to_le_bytes());
    }
}
