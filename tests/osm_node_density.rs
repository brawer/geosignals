// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end tests of the osm-node-density pipeline.

mod common;

use osmsignals::{encode, osm_node_density, tile};
use std::path::Path;

/// Output of a run: its summary, the decoded image, the data BOM, and
/// the SHA-256 of the image file.
struct Output {
    summary: osm_node_density::Summary,
    image: common::Image,
    bom: serde_json::Value,
    datapackage: serde_json::Value,
    sha256: String,
    bom_bytes: usize,
}

fn run(pbf: &Path, run_size: usize) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let cfg = osm_node_density::Config {
        input: osmsignals::bom::Input::new("zugerland.osm.pbf"),
        run_id: "test-run".into(),
        output_dir: dir.path().to_path_buf(),
        workdir: dir.path().to_path_buf(),
        threads: 3,
        run_size,
        max_blocks: None,
        sample_dir: None,
        sample_rate: 64,
    };
    let input = std::fs::File::open(pbf).unwrap();
    let summary = osm_node_density::run(std::io::BufReader::new(input), &cfg).unwrap();
    let tiff = std::fs::read(&summary.output).unwrap();
    let image = common::read(&tiff);
    let digest = ring::digest::digest(&ring::digest::SHA256, &tiff);
    let sha256 = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let bom_json = std::fs::read(&summary.bom).unwrap();
    let bom = serde_json::from_slice(&bom_json).unwrap();
    let datapackage =
        serde_json::from_slice(&std::fs::read(&summary.datapackage).unwrap()).unwrap();
    Output {
        summary,
        image,
        bom,
        datapackage,
        sha256,
        bom_bytes: bom_json.len(),
    }
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
    let Output {
        summary,
        image,
        bom,
        datapackage,
        sha256,
        bom_bytes,
    } = run(Path::new(ZUGERLAND), 1000);
    assert_eq!(summary.nodes, 1790);
    assert_eq!(summary.skipped_nodes, 0);
    assert_eq!(
        summary.output.file_name().unwrap(),
        "osm-node-density.2026-01-27.tiff",
        "named after the replication timestamp in the PBF header"
    );

    // The data BOM sits next to the output and describes both files.
    assert_eq!(
        summary.bom,
        summary
            .output
            .with_file_name("osm-node-density.2026-01-27.cdx.json")
    );
    assert_eq!(bom["specVersion"], "1.7");
    assert_eq!(bom["metadata"]["timestamp"], "2026-01-27T08:11:02Z");
    let output = &bom["metadata"]["component"];
    assert_eq!(output["name"], "osm-node-density.2026-01-27.tiff");
    assert_eq!(output["hashes"][0]["content"], sha256.as_str());
    let input = &bom["components"][0];
    assert_eq!(input["name"], "zugerland.osm.pbf");
    assert!(input.get("hashes").is_none(), "input is not hashed");
    assert_eq!(
        bom["dependencies"][0]["dependsOn"][0], input["bom-ref"],
        "output depends on input"
    );
    assert_eq!(bom["formulation"][0]["workflows"][0]["uid"], "test-run");

    // The data package points to both files, by name relative to itself.
    assert_eq!(
        summary.datapackage,
        summary.output.with_file_name("datapackage.json")
    );
    assert_eq!(datapackage["version"], "2026-01-27");
    let resources = datapackage["resources"].as_array().unwrap();
    assert_eq!(resources[0]["path"], "osm-node-density.2026-01-27.tiff");
    assert_eq!(resources[0]["hash"], format!("sha256:{sha256}"));
    assert_eq!(resources[1]["path"], "osm-node-density.2026-01-27.cdx.json");
    assert_eq!(resources[1]["bytes"], bom_bytes, "BOM size");
    assert_eq!(image.ifds.len(), 11, "full resolution plus 10 overviews");
    let full = &image.ifds[0];
    assert_eq!(full[&256].int(), 1 << 18, "one pixel per zoom-18 tile");
    assert!(full[&33432].ascii().contains("OpenStreetMap contributors"));
    assert!(full[&34735].ints().contains(&3857), "Web Mercator");
    let gdal = full[&42112].ascii();
    assert!(gdal.contains(r#"<Item name="NODES">1790</Item>"#), "{gdal}");
    assert!(gdal.contains(r#"role="scale">0.01<"#), "{gdal}");
    assert!(gdal.contains(r#"role="offset">-15.01<"#), "{gdal}");

    // The embedded histogram matches the full-resolution pixels.
    let mut hist = std::collections::BTreeMap::<u64, u64>::new();
    for row in gdal.split("<Row ").skip(1) {
        let f: Vec<u64> = row
            .split("<F>")
            .skip(1)
            .map(|f| f.split('<').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(f[1], f[0] + 1, "bin size 1");
        hist.insert(f[0], f[2]);
    }
    assert_eq!(hist.values().sum::<u64>(), 1 << 36, "every pixel counted");
    let mut expected = std::collections::BTreeMap::<u64, u64>::new();
    for ((z, _, _), px) in &image.tiles {
        for &v in px.iter().filter(|_| *z == 10) {
            *expected.entry(u64::from(v)).or_default() += 1;
        }
    }
    for (v, n) in &expected {
        if *v > 0 {
            assert_eq!(hist[v], *n, "value {v}");
        }
    }
    let nonzero: u64 = hist.iter().filter(|(v, _)| **v > 0).map(|(_, n)| n).sum();
    assert_eq!(nonzero, summary.pixels);
    assert_eq!(
        *hist.keys().last().unwrap(),
        *expected.keys().last().unwrap()
    );

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
    let (a, b) = (
        run(Path::new(ZUGERLAND), 7),
        run(Path::new(ZUGERLAND), 1 << 20),
    );
    assert_eq!(a.image.tiles, b.image.tiles);
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.bom, b.bom, "data BOM is reproducible");
    assert_eq!(a.datapackage, b.datapackage, "data package is reproducible");
}

/// Kubernetes may restart a killed run with the same working directory.
/// The new run must clean up after the old one and still succeed.
#[test]
fn restart_after_crash() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let output = dir.path().join("osm-node-density.2026-01-27.tiff");
    let partial = dir.path().join("osm-node-density.2026-01-27.tiff.partial");
    // What a run killed halfway through leaves behind.
    let stale = work.join("osm-node-density/tmp");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("spool-10"), vec![7u8; 1000]).unwrap();
    std::fs::write(stale.join(".tmpAbC123"), b"half a run").unwrap();
    std::fs::write(&partial, b"half a file").unwrap();
    std::fs::write(work.join("planet.osm.pbf"), b"not ours").unwrap();

    let cfg = osm_node_density::Config {
        input: osmsignals::bom::Input::new("zugerland.osm.pbf"),
        run_id: "test-run".into(),
        output_dir: dir.path().to_path_buf(),
        workdir: work.clone(),
        threads: 2,
        run_size: 100,
        max_blocks: None,
        sample_dir: None,
        sample_rate: 64,
    };
    let input = std::fs::File::open(ZUGERLAND).unwrap();
    let summary = osm_node_density::run(std::io::BufReader::new(input), &cfg).unwrap();
    assert_eq!(summary.nodes, 1790);
    assert_eq!(summary.output, output);
    common::read(&std::fs::read(&output).unwrap());

    assert!(!partial.exists());
    assert!(!stale.exists(), "temporary files left behind");
    assert_eq!(
        std::fs::read(work.join("planet.osm.pbf")).unwrap(),
        b"not ours"
    );
}

/// Runs on a bigger extract given by `OSMSIGNALS_SMOKE_PBF`, such as
/// Liechtenstein from Geofabrik. CI sets this up; locally it is skipped.
#[test]
fn smoke_test() {
    let Ok(path) = std::env::var("OSMSIGNALS_SMOKE_PBF") else {
        eprintln!("OSMSIGNALS_SMOKE_PBF not set, skipping");
        return;
    };
    let Output { summary, image, .. } = run(Path::new(&path), 1 << 20);
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
