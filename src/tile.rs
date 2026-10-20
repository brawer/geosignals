// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Web Mercator tile math and the Hilbert sort key.
//!
//! Every OSM node is mapped to a pixel at zoom 18 (a 256×256 tile at zoom 10
//! has one pixel per zoom-18 tile). The sort key of a pixel is its position
//! on the Hilbert curve of order 18, as used by PMTiles for tile IDs. Because
//! the Hilbert curve visits every quadrant of every level in one contiguous
//! run, `key >> (2 * k)` is the Hilbert index of the enclosing tile at zoom
//! `18 - k`. Sorting by key therefore visits the tile tree in depth-first
//! order, which is what lets us build the overview pyramid in one pass.

/// Zoom level of the finest pixels.
pub const PIXEL_ZOOM: u8 = 18;

/// Zoom level of the finest tiles in the output.
pub const MAX_TILE_ZOOM: u8 = 10;

/// log2 of the tile width in pixels.
pub const TILE_BITS: u8 = PIXEL_ZOOM - MAX_TILE_ZOOM;

/// Tile width and height in pixels.
pub const TILE_SIZE: usize = 1 << TILE_BITS;

/// Largest latitude that Web Mercator can represent.
pub const MAX_LAT: f64 = 85.051_128_779_806_59;

/// Mean radius of the Earth in km (IUGG authalic radius).
pub const EARTH_RADIUS_KM: f64 = 6371.0072;

/// Projects a location to the zoom-18 pixel that contains it, or `None`
/// if the latitude lies outside the Web Mercator range.
pub fn pixel_xy(lon: f64, lat: f64) -> Option<(u32, u32)> {
    if !(-MAX_LAT..=MAX_LAT).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let n = (1u32 << PIXEL_ZOOM) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.to_radians();
    let y = (1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    let max = (1u32 << PIXEL_ZOOM) - 1;
    Some(((x as u32).min(max), (y.max(0.0) as u32).min(max)))
}

/// Hilbert key of a zoom-18 pixel.
#[inline]
pub fn pixel_key(x: u32, y: u32) -> u64 {
    fast_hilbert::xy2h(x, y, PIXEL_ZOOM)
}

/// Inverse of [`pixel_key`].
#[inline]
pub fn key_to_pixel(key: u64) -> (u32, u32) {
    fast_hilbert::h2xy(key, PIXEL_ZOOM)
}

/// Sort key for a node location, or `None` if outside Web Mercator.
#[inline]
pub fn node_key(lon: f64, lat: f64) -> Option<u64> {
    pixel_xy(lon, lat).map(|(x, y)| pixel_key(x, y))
}

/// Latitude in degrees of the northern edge of row `y` at zoom `z`.
pub fn row_lat(z: u8, y: u32) -> f64 {
    let n = (1u64 << z) as f64;
    let t = std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n);
    t.sinh().atan().to_degrees()
}

/// Area on Earth, in km², of one pixel in row `y` at zoom `z`.
/// All pixels in a row have the same area.
pub fn pixel_area_km2(z: u8, y: u32) -> f64 {
    let north = row_lat(z, y).to_radians();
    let south = row_lat(z, y + 1).to_radians();
    let dlon = 2.0 * std::f64::consts::PI / (1u64 << z) as f64;
    EARTH_RADIUS_KM * EARTH_RADIUS_KM * dlon * (north.sin() - south.sin())
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn projects_known_places() {
        // Zürich main station, checked against tile.openstreetmap.org.
        let (x, y) = pixel_xy(8.5402, 47.3782).unwrap();
        assert_eq!((x >> 8, y >> 8), (536, 358));
        assert_eq!(pixel_xy(-180.0, MAX_LAT), Some((0, 0)));
        assert_eq!(
            pixel_xy(180.0, -MAX_LAT),
            Some(((1 << 18) - 1, (1 << 18) - 1))
        );
        assert_eq!(pixel_xy(0.0, 0.0), Some((1 << 17, 1 << 17)));
        assert_eq!(pixel_xy(0.0, 86.0), None);
        assert_eq!(pixel_xy(0.0, -89.9), None);
        assert_eq!(pixel_xy(f64::NAN, 0.0), None);
    }

    #[test]
    fn key_round_trip() {
        for &(x, y) in &[(0, 0), (1, 0), (12345, 67890), ((1 << 18) - 1, 7)] {
            assert_eq!(key_to_pixel(pixel_key(x, y)), (x, y));
        }
    }

    #[test]
    fn key_prefix_is_ancestor_key() {
        // The pyramid builder relies on this, and so does PMTiles clustering.
        let mut s = 0x1234_5678_u64;
        for _ in 0..20_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = (s & 0x3ffff) as u32;
            let y = ((s >> 20) & 0x3ffff) as u32;
            let key = pixel_key(x, y);
            for k in 0..PIXEL_ZOOM {
                let z = PIXEL_ZOOM - k;
                let ancestor: u64 = fast_hilbert::xy2h(x >> k, y >> k, z);
                assert_eq!(key >> (2 * k), ancestor, "x={x} y={y} z={z}");
            }
        }
    }

    #[test]
    fn pixel_area() {
        // At the equator, a zoom-18 pixel is about 152.7 m wide on our sphere.
        let a = pixel_area_km2(18, 1 << 17);
        let w = 2.0 * std::f64::consts::PI * EARTH_RADIUS_KM / (1 << 18) as f64;
        assert_relative_eq!(w, 0.152_70, max_relative = 1e-4);
        assert_relative_eq!(a, w * w, max_relative = 1e-4);
        // Shrinks with cos²(lat).
        let y = pixel_xy(0.0, 60.0).unwrap().1;
        assert_relative_eq!(pixel_area_km2(18, y) / a, 0.25, max_relative = 1e-3);
        // Rows sum up to the area of the Mercator band.
        let total: f64 = (0..256).map(|y| pixel_area_km2(8, y) * 256.0).sum();
        let band =
            4.0 * std::f64::consts::PI * EARTH_RADIUS_KM.powi(2) * MAX_LAT.to_radians().sin();
        assert_relative_eq!(total, band, max_relative = 1e-9);
    }
}
