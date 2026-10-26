// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end test of the nominatim-secondary-importance pipeline, on a
//! small dump in the format that Nominatim publishes.

mod common;

use osmsignals::nominatim;
use std::io::Write;

/// A PostGIS raster tile in hex WKB, `w`×`h` cells with its top left at
/// grid cell (`col`, `row`), like a row of Nominatim's dump.
fn wkb(col: u32, row: u32, w: u16, h: u16, px: &[u16]) -> String {
    let n = 16384.0;
    let mut b = vec![1u8, 0, 0, 1, 0];
    for v in [
        360.0 / n,
        -180.0 / n,
        -180.0 + f64::from(col) * 360.0 / n,
        90.0 - f64::from(row) * 180.0 / n,
        0.0,
        0.0,
    ] {
        b.extend_from_slice(&f64::to_le_bytes(v));
    }
    b.extend_from_slice(&4326i32.to_le_bytes());
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.extend_from_slice(&[0x46, 0, 0]); // 16BUI with NoData value 0
    for v in px {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// Grid cell of a location.
fn cell(lon: f64, lat: f64) -> (u32, u32) {
    (
        ((lon + 180.0) / 360.0 * 16384.0) as u32,
        ((90.0 - lat) / 180.0 * 16384.0) as u32,
    )
}

#[test]
fn republishes_raster() {
    // Zürich gets a 128×128 tile of 44901 with a hole of NoData, and a
    // second tile overlaps it with smaller values.
    let (zx, zy) = cell(8.5417, 47.3769);
    let (x0, y0) = (zx / 128 * 128, zy / 128 * 128);
    let mut px = vec![44901u16; 128 * 128];
    px[0] = 0;
    let overlap = vec![100u16; 4];
    let dump = format!(
        "BEGIN;\nCREATE TABLE \"public\".\"secondary_importance\" (\"rid\" serial PRIMARY KEY,\"rast\" raster);\n\
         COPY \"public\".\"secondary_importance\" (\"rast\") FROM stdin;\n{}\n\\.\n\
         COPY \"public\".\"secondary_importance\" (\"rast\") FROM stdin;\n{}\n{}\n\\.\nEND;\n",
        wkb(x0, y0, 128, 128, &px),
        wkb(x0, y0, 2, 2, &overlap),
        wkb(16383, 16383, 1, 1, &[65535]),
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(dump.as_bytes()).unwrap();
    let gz = gz.finish().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = nominatim::Config {
        input: osmsignals::bom::Input::new(nominatim::URL).from(&osmsignals::bom::NOMINATIM),
        timestamp: 1_714_812_134, // Sat, 04 May 2024 08:42:14 GMT
        run_id: "test-run".into(),
        output_dir: dir.path().join("out"),
        workdir: dir.path().join("work"),
        threads: 3,
    };
    let summary = nominatim::run(gz.as_slice(), &cfg).unwrap();
    assert_eq!(summary.source_tiles, 3);
    assert_eq!(summary.pixels, 128 * 128 + 1);
    assert_eq!(
        summary.output.file_name().unwrap(),
        "nominatim-secondary-importance.2024-05-04.tiff",
        "named after the input's last modification"
    );

    let tiff = std::fs::read(&summary.output).unwrap();
    let image = common::read(&tiff);
    assert_eq!(image.ifds.len(), 7, "full resolution plus 6 overviews");
    let full = &image.ifds[0];
    assert_eq!(full[&256].int(), 16384);
    assert_eq!(full[&257].int(), 16384);
    let keys = full[&34735].ints();
    assert!(
        keys.windows(4).any(|k| k == [2048, 0, 1, 4326]),
        "EPSG:4326"
    );
    assert!(keys.windows(4).any(|k| k == [1024, 0, 1, 2]), "geographic");
    let common::Field::Doubles(scale) = &full[&33550] else {
        panic!("ModelPixelScale")
    };
    assert_eq!(scale[..2], [360.0 / 16384.0, 180.0 / 16384.0]);
    let common::Field::Doubles(tie) = &full[&33922] else {
        panic!("ModelTiepoint")
    };
    assert_eq!(tie[3..5], [-180.0, 90.0]);
    assert_eq!(full[&42113].ascii(), "0", "GDAL_NODATA");
    assert_eq!(full[&340].int(), 100, "SMinSampleValue, without NoData");
    assert_eq!(full[&341].int(), 65535, "SMaxSampleValue");
    assert!(full[&33432].ascii().contains("OpenStreetMap contributors"));
    let gdal = full[&42112].ascii();
    assert!(
        gdal.contains(r#"role="scale">0.0000152587890625<"#),
        "{gdal}"
    );
    assert!(gdal.contains("2024-05-04T08:42:14Z"), "{gdal}");
    assert!(gdal.contains(r#"<Row index="0"><F>0</F><F>1</F><F>268419071</F></Row>"#));
    assert!(gdal.contains(r#"<Row index="1"><F>100</F><F>101</F><F>1</F></Row>"#));
    assert!(gdal.contains(r#"<Row index="2"><F>44901</F><F>44902</F><F>16383</F></Row>"#));
    assert!(gdal.contains(r#"<Row index="3"><F>65535</F><F>65536</F><F>1</F></Row>"#));

    // Pixels are the dump's values; where tiles overlap, the larger wins,
    // and NoData stays 0.
    let at = |z: u8, x: u32, y: u32| -> u16 {
        image
            .tiles
            .get(&(z, x >> 8, y >> 8))
            .map_or(0, |t| t[((y & 255) * 256 + (x & 255)) as usize])
    };
    assert_eq!(at(6, zx, zy), 44901);
    assert_eq!(at(6, x0, y0), 100, "NoData under the overlapping tile");
    assert_eq!(at(6, x0 + 1, y0), 44901);
    assert_eq!(at(6, x0 + 128, y0), 0);
    assert_eq!(at(6, 16383, 16383), 65535);
    // Overviews keep the largest value.
    for z in 0..6u8 {
        let k = 6 - z;
        assert_eq!(at(z, zx >> k, zy >> k), 44901, "zoom {z}");
        assert_eq!(at(z, 16383 >> k, 16383 >> k), 65535, "zoom {z}");
    }
    assert_eq!(image.tiles.keys().filter(|(z, _, _)| *z == 6).count(), 2);

    // The BOM records the input's hashes and date.
    let bom: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&summary.bom).unwrap()).unwrap();
    let input = &bom["components"][0];
    assert_eq!(input["name"], "wikimedia-secondary-importance.sql.gz");
    assert_eq!(input["supplier"]["name"], "Nominatim");
    assert_eq!(input["version"], "2024-05-04T08:42:14Z");
    let sha256: String = ring::digest::digest(&ring::digest::SHA256, &gz)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(input["hashes"][0]["content"], sha256.as_str());
    assert!(
        input["purl"]
            .as_str()
            .unwrap()
            .starts_with("pkg:generic/nominatim/wikimedia-secondary-importance.sql.gz@")
    );

    let pkg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&summary.datapackage).unwrap()).unwrap();
    assert_eq!(pkg["name"], "nominatim-secondary-importance");
    assert_eq!(pkg["version"], "2024-05-04");
    assert_eq!(pkg["sources"][0]["path"], nominatim::URL);
    assert_eq!(
        pkg["resources"][0]["path"],
        "nominatim-secondary-importance.2024-05-04.tiff"
    );

    // Running again gives the same files.
    let again = nominatim::run(gz.as_slice(), &cfg).unwrap();
    assert_eq!(std::fs::read(&again.output).unwrap(), tiff);
}
