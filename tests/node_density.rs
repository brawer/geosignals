// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end tests of the node-density pipeline.

mod common;

use osmsignals::{encode, node_density, tile};
use std::path::Path;

fn run(pbf: &Path, run_size: usize) -> (node_density::Summary, common::Image) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = node_density::Config {
        output: dir.path().join("out.tif"),
        tmpdir: dir.path().to_path_buf(),
        threads: 3,
        run_size,
        sample_dir: None,
        sample_rate: 64,
    };
    let input = std::fs::File::open(pbf).unwrap();
    let summary = node_density::run(std::io::BufReader::new(input), &cfg).unwrap();
    let image = common::read(&std::fs::read(&cfg.output).unwrap());
    (summary, image)
}

/// Sum of node counts implied by the pixels of a tile, up to quantization.
fn approx_nodes(z: u8, y: u32, pixels: &[u16]) -> f64 {
    let mut total = 0.0;
    for row in 0..256 {
        let area = tile::pixel_area_km2(z + 8, y * 256 + row as u32);
        for &v in &pixels[row * 256..(row + 1) * 256] {
            total += encode::dequantize(v) * area;
        }
    }
    total
}

const ZUGERLAND: &str = "tests/data/zugerland.osm.pbf";

#[test]
fn zugerland() {
    let (summary, image) = run(Path::new(ZUGERLAND), 1000);
    assert_eq!(summary.nodes, 1790);
    assert_eq!(summary.skipped_nodes, 0);
    assert_eq!(image.ifds.len(), 11, "full resolution plus 10 overviews");
    let full = &image.ifds[0];
    assert_eq!(full[&256].int(), 1 << 18, "one pixel per zoom-18 tile");
    assert!(full[&33432].ascii().contains("OpenStreetMap contributors"));
    assert!(full[&34735].ints().contains(&3857), "Web Mercator");
    let gdal = full[&42112].ascii();
    assert!(gdal.contains(r#"<Item name="NODES">1790</Item>"#), "{gdal}");
    assert!(gdal.contains(r#"role="scale">0.01<"#), "{gdal}");
    assert!(gdal.contains(r#"role="offset">-15.01<"#), "{gdal}");
    for ovr in &image.ifds[1..] {
        assert_eq!(ovr[&254].int(), 1, "reduced-resolution image");
    }

    // One tile per zoom level, plus where the extract crosses a tile edge.
    let (x, y) = tile::pixel_xy(8.515, 47.166).unwrap();
    for z in 0..=10u8 {
        let shift = 18 - z;
        assert!(
            image.tiles.contains_key(&(z, x >> shift, y >> shift)),
            "zoom {z}"
        );
    }
    assert_eq!(
        image.tiles.len() as u64,
        summary.tiles_per_zoom.iter().sum::<u64>()
    );

    // Every level accounts for all nodes, within quantization error.
    for z in 0..=10u8 {
        let n: f64 = image
            .tiles
            .iter()
            .filter(|((tz, _, _), _)| *tz == z)
            .map(|(&(z, _, y), data)| approx_nodes(z, y, data))
            .sum();
        let err = (n / 1790.0 - 1.0).abs();
        assert!(err < 0.06, "zoom {z}: {n:.0} nodes");
    }
}

#[test]
fn run_size_does_not_change_output() {
    let (_, a) = run(Path::new(ZUGERLAND), 7);
    let (_, b) = run(Path::new(ZUGERLAND), 1 << 20);
    assert_eq!(a.tiles, b.tiles);
}

/// Runs on a bigger extract given by `OSMSIGNALS_SMOKE_PBF`, such as
/// Liechtenstein from Geofabrik. CI sets this up; locally it is skipped.
#[test]
fn smoke_test() {
    let Ok(path) = std::env::var("OSMSIGNALS_SMOKE_PBF") else {
        eprintln!("OSMSIGNALS_SMOKE_PBF not set, skipping");
        return;
    };
    let (summary, image) = run(Path::new(&path), 1 << 20);
    eprintln!("{summary:#?}");
    assert!(summary.nodes > 100_000, "suspiciously few nodes");
    // Vaduz castle should be denser than the Malbun ski slopes nearby.
    let density_at = |lon: f64, lat: f64| -> u16 {
        let (x, y) = tile::pixel_xy(lon, lat).unwrap();
        let data = &image.tiles[&(10, x >> 8, y >> 8)];
        data[((y & 255) * 256 + (x & 255)) as usize]
    };
    let vaduz = density_at(9.5215, 47.1410);
    let alps = density_at(9.6100, 47.1000);
    eprintln!("vaduz={vaduz} alps={alps}");
    assert!(vaduz > alps);
    // All nodes are accounted for at zoom 0.
    let z0 = &image.tiles[&(0, 0, 0)];
    let n = approx_nodes(0, 0, z0);
    assert!(
        (n / summary.nodes as f64 - 1.0).abs() < 0.06,
        "{n} vs {}",
        summary.nodes
    );
}
