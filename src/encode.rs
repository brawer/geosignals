//! Turns per-pixel node counts into tile images.
//!
//! Each pixel stores node density (nodes per km² of real Earth surface)
//! on a logarithmic scale, so that one 8-bit value covers everything from
//! a lone node in a zoom-8 pixel of tundra to a dense city block.
//! The scale is fixed, not derived from the data, so values from
//! different months are comparable.

use crate::pyramid::Counts;
use crate::tile::{self, TILE_BITS, TILE_SIZE};
use anyhow::Result;

/// Density represented by pixel value 1, in nodes/km².
pub const MIN_DENSITY: f64 = 1.0 / 65536.0;

/// Pixel values per doubling of density.
pub const STEPS_PER_OCTAVE: f64 = 6.0;

/// Maps a density to a pixel value; 0 means "no nodes at all".
pub fn quantize(density: f64) -> u8 {
    if density <= 0.0 {
        return 0;
    }
    let v = 1.0 + (STEPS_PER_OCTAVE * (density / MIN_DENSITY).log2()).round();
    v.clamp(1.0, 255.0) as u8
}

/// Inverse of [`quantize`], returning the center of the value's bucket.
pub fn dequantize(v: u8) -> f64 {
    if v == 0 {
        0.0
    } else {
        MIN_DENSITY * ((f64::from(v) - 1.0) / STEPS_PER_OCTAVE).exp2()
    }
}

/// Converts the counts of tile `z/_/y` into quantized pixel values.
pub fn pixels(z: u8, y: u32, counts: &Counts) -> Vec<u8> {
    let pixel_zoom = z + TILE_BITS;
    let mut out = vec![0u8; TILE_SIZE * TILE_SIZE];
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

/// Encodes 8-bit grayscale pixels as lossless WebP.
pub fn webp(pixels: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out).encode(
        pixels,
        TILE_SIZE as u32,
        TILE_SIZE as u32,
        image_webp::ColorType::L8,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_round_trip() {
        assert_eq!(quantize(0.0), 0);
        assert_eq!(quantize(MIN_DENSITY), 1);
        assert_eq!(quantize(MIN_DENSITY / 100.0), 1, "any node is non-zero");
        assert_eq!(quantize(1e30), 255);
        for v in 1..=255u8 {
            assert_eq!(quantize(dequantize(v)), v);
        }
        // Monotone.
        let mut prev = 0;
        for i in 0..2000 {
            let q = quantize(1e-6 * 1.02f64.powi(i));
            assert!(q >= prev);
            prev = q;
        }
    }

    #[test]
    fn one_node_anywhere_is_visible() {
        // Largest pixel: zoom 8 at the equator.
        let mut counts = vec![0u64; TILE_SIZE * TILE_SIZE];
        counts[128 * TILE_SIZE] = 1;
        assert!(pixels(0, 0, &counts)[128 * TILE_SIZE] >= 1);
    }

    #[test]
    fn webp_decodes() {
        let px: Vec<u8> = (0..TILE_SIZE * TILE_SIZE).map(|i| (i % 7) as u8).collect();
        let data = webp(&px).unwrap();
        let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(&data)).unwrap();
        let mut buf = vec![0u8; dec.output_buffer_size().unwrap()];
        dec.read_image(&mut buf).unwrap();
        // L8 decodes as RGB(A); every channel holds the gray value.
        let channels = buf.len() / px.len();
        let gray: Vec<u8> = buf.chunks(channels).map(|c| c[0]).collect();
        assert_eq!(gray, px);
    }
}
