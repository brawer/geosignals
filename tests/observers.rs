// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end test of the naturalist-observers pipeline: a fake GBIF
//! bucket with two snapshots of small Parquet files laid out like
//! GBIF's, served over HTTP with S3 listings and range requests.

mod common;

use geosignals::observers::{self, EBIRD, INATURALIST};
use geosignals::{encode, tile};
use parquet::data_type::{ByteArray, ByteArrayType, DoubleType};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Zürich main station, and a place 1.5 km east: different zoom-16
/// pixels in the same zoom-8 tile.
const A: (f64, f64) = (8.5402, 47.3782);
const B: (f64, f64) = (8.5602, 47.3782);

const OTHER: &str = "00000000-0000-0000-0000-000000000000";

/// One occurrence: dataset, license, location, uncertainty, observers.
type Row = (
    &'static str,
    &'static str,
    Option<(f64, f64)>,
    Option<f64>,
    &'static [&'static str],
);

const SCHEMA: &str = "message spark_schema {
    optional binary gbifid (UTF8);
    optional binary datasetkey (UTF8);
    optional double decimallatitude;
    optional double decimallongitude;
    optional double coordinateuncertaintyinmeters;
    optional binary license (UTF8);
    optional group recordedby (LIST) {
        repeated group array {
            optional binary array_element (UTF8);
        }
    }
}";

/// A Parquet file like GBIF's, with one row group per slice of rows.
fn parquet_file(row_groups: &[&[Row]]) -> Vec<u8> {
    parquet_file_with_pages(row_groups, 1 << 20)
}

/// Like `parquet_file`, with data pages of about `page_size` bytes.
fn parquet_file_with_pages(row_groups: &[&[Row]], page_size: usize) -> Vec<u8> {
    let schema = Arc::new(parquet::schema::parser::parse_message_type(SCHEMA).unwrap());
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            .set_data_page_size_limit(page_size)
            .set_dictionary_page_size_limit(page_size)
            .set_write_batch_size(1024)
            .build(),
    );
    let mut w = SerializedFileWriter::new(Vec::new(), schema, props).unwrap();
    for rows in row_groups {
        let mut rg = w.next_row_group().unwrap();
        let mut i = 0;
        while let Some(mut col) = rg.next_column().unwrap() {
            let strings = |f: &dyn Fn(&Row) -> Option<String>| {
                let mut def = Vec::new();
                let mut vals = Vec::new();
                for r in rows.iter() {
                    match f(r) {
                        Some(s) => {
                            def.push(1);
                            vals.push(ByteArray::from(s.as_str()));
                        }
                        None => def.push(0),
                    }
                }
                (def, vals)
            };
            let doubles = |f: &dyn Fn(&Row) -> Option<f64>| {
                let def: Vec<i16> = rows.iter().map(|r| i16::from(f(r).is_some())).collect();
                let vals: Vec<f64> = rows.iter().filter_map(f).collect();
                (def, vals)
            };
            match i {
                0 | 1 | 5 => {
                    let (def, vals) = match i {
                        0 => strings(&|_| Some("1".into())),
                        1 => strings(&|r| Some(r.0.into())),
                        _ => strings(&|r| Some(r.1.into())),
                    };
                    col.typed::<ByteArrayType>()
                        .write_batch(&vals, Some(&def), None)
                        .unwrap();
                }
                2..=4 => {
                    let (def, vals) = match i {
                        2 => doubles(&|r| r.2.map(|p| p.1)),
                        3 => doubles(&|r| r.2.map(|p| p.0)),
                        _ => doubles(&|r| r.3),
                    };
                    col.typed::<DoubleType>()
                        .write_batch(&vals, Some(&def), None)
                        .unwrap();
                }
                6 => {
                    let (mut def, mut rep, mut vals) = (Vec::new(), Vec::new(), Vec::new());
                    for r in rows.iter() {
                        if r.4.is_empty() {
                            def.push(1); // empty list
                            rep.push(0);
                        }
                        for (j, who) in r.4.iter().enumerate() {
                            def.push(3);
                            rep.push(i16::from(j > 0));
                            vals.push(ByteArray::from(*who));
                        }
                    }
                    col.typed::<ByteArrayType>()
                        .write_batch(&vals, Some(&def), Some(&rep))
                        .unwrap();
                }
                _ => unreachable!(),
            }
            col.close().unwrap();
            i += 1;
        }
        rg.close().unwrap();
    }
    w.into_inner().unwrap()
}

const BY: &str = "CC_BY_4_0";
const NC: &str = "CC_BY_NC_4_0";
const CC0: &str = "CC0_1_0";

fn files() -> Vec<Vec<u8>> {
    let e = EBIRD.key;
    let i = INATURALIST.key;
    vec![
        // Neither of our datasets.
        parquet_file(&[&[(OTHER, BY, Some(A), None, &["x"])]]),
        parquet_file(&[&[
            (OTHER, BY, Some(A), None, &["y"]),
            (e, BY, Some(A), None, &["obsr1"]),
            (e, BY, Some(A), None, &["obsr1"]),
            (e, BY, Some(A), None, &["obsr2"]),
            (e, BY, Some(B), None, &["obsr1"]),
            (i, NC, Some(A), Some(5.0), &["alice"]),
        ]]),
        parquet_file(&[
            &[
                (i, CC0, Some(A), Some(30.0), &["bob"]),
                (i, BY, Some(B), None, &["bob", "carol"]),
                // Blurred, unlocated, share-alike, anonymous.
                (i, BY, Some(A), Some(28_447.0), &["dave"]),
                (i, BY, None, None, &["erin"]),
                (i, "CC_BY_SA_4_0", Some(A), None, &["frank"]),
                (i, BY, Some(A), None, &[]),
            ],
            &[(i, NC, Some(B), None, &["alice"])],
        ]),
    ]
}

/// Serves a bucket with snapshots 2026-09-01 (empty, to check that we
/// take the latest) and 2026-10-01, listing one key per page.
fn serve(files: Vec<Vec<u8>>) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let ranges = Arc::new(AtomicUsize::new(0));
    let r = ranges.clone();
    let mut objects: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    objects.insert(
        "occurrence/2026-09-01/occurrence.parquet/000000".into(),
        files[0].clone(),
    );
    for (i, f) in files.into_iter().enumerate() {
        objects.insert(
            format!("occurrence/2026-10-01/occurrence.parquet/{i:06}"),
            f,
        );
    }
    objects.insert(
        "occurrence/2026-10-01/occurrence.parquet/_SUCCESS".into(),
        Vec::new(),
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut path, mut range) = (String::new(), None);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(p) = line.strip_prefix("GET ") {
                    path = p.split(' ').next().unwrap().to_string();
                } else if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    let (a, b) = v.trim().split_once('-').unwrap();
                    range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                }
            }
            let (status, body) = if let Some(q) = path.strip_prefix("/?") {
                ("200 OK", list(&objects, q).into_bytes())
            } else {
                match (objects.get(&path[1..]), range) {
                    (Some(data), Some((a, b))) => {
                        r.fetch_add(1, Ordering::SeqCst);
                        ("206 Partial Content", data[a..=b].to_vec())
                    }
                    (Some(data), None) => ("200 OK", data.clone()),
                    (None, _) => ("404 Not Found", Vec::new()),
                }
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, ranges)
}

fn decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// An S3 ListObjectsV2 answer, one object or prefix per page.
fn list(objects: &BTreeMap<String, Vec<u8>>, query: &str) -> String {
    let params: BTreeMap<&str, String> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k, decode(v)))
        .collect();
    assert_eq!(params["list-type"], "2");
    let prefix = &params["prefix"];
    let mut entries: Vec<String> = Vec::new();
    for (key, data) in objects.range(prefix.clone()..) {
        let Some(rest) = key.strip_prefix(prefix.as_str()) else {
            break;
        };
        if params.contains_key("delimiter") {
            let dir = format!("{prefix}{}/", rest.split('/').next().unwrap());
            let e = format!("<CommonPrefixes><Prefix>{dir}</Prefix></CommonPrefixes>");
            if !entries.contains(&e) {
                entries.push(e);
            }
        } else {
            entries.push(format!(
                "<Contents><Key>{key}</Key><Size>{}</Size></Contents>",
                data.len()
            ));
        }
    }
    let start: usize = params
        .get("continuation-token")
        .map_or(0, |t| t.strip_prefix("t&").unwrap().parse().unwrap());
    let more = start + 1 < entries.len();
    let mut xml = String::from("<?xml version=\"1.0\"?><ListBucketResult>");
    xml.push_str(entries.get(start).map_or("", String::as_str));
    xml.push_str(&format!("<IsTruncated>{more}</IsTruncated>"));
    if more {
        xml.push_str(&format!(
            "<NextContinuationToken>t&amp;{}</NextContinuationToken>",
            start + 1
        ));
    }
    xml.push_str("</ListBucketResult>");
    xml
}

struct Output {
    summary: observers::Summary,
    tiffs: Vec<Vec<u8>>,
    datapackage: Value,
    boms: Vec<Value>,
    /// The STAC item and collection.
    item: Value,
    collection: Value,
}

fn run(bucket: &str, threads: usize, run_size: usize) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let cfg = observers::Config {
        bucket: bucket.into(),
        snapshot: None,
        output_dir: dir.path().join("out"),
        workdir: dir.path().join("work"),
        threads,
        run_size,
        files: None,
        run_id: "test-run".into(),
    };
    let summary = observers::run(&cfg).unwrap();
    let tiffs = summary
        .layers
        .iter()
        .map(|l| std::fs::read(&l.output).unwrap())
        .collect();
    let boms = summary
        .layers
        .iter()
        .map(|l| serde_json::from_slice(&std::fs::read(&l.bom).unwrap()).unwrap())
        .collect();
    let datapackage =
        serde_json::from_slice(&std::fs::read(&summary.datapackage).unwrap()).unwrap();
    let json = |name: &str| -> Value {
        serde_json::from_slice(&std::fs::read(cfg.output_dir.join(name)).unwrap()).unwrap()
    };
    let collection = json("collection.json");
    let item = json(
        collection["links"]
            .as_array()
            .unwrap()
            .iter()
            .rfind(|l| l["rel"] == "item")
            .unwrap()["href"]
            .as_str()
            .unwrap(),
    );
    Output {
        summary,
        tiffs,
        datapackage,
        boms,
        item,
        collection,
    }
}

/// The pixel value at a place, at tile zoom `z`.
fn value(img: &common::Image, z: u8, (lon, lat): (f64, f64)) -> u16 {
    let (x, y) = tile::pixel_xy(lon, lat).unwrap();
    let shift = 18 - (z + 8);
    let (x, y) = (x >> shift, y >> shift);
    img.tiles
        .get(&(z, x >> 8, y >> 8))
        .map_or(0, |t| t[(y as usize % 256) * 256 + x as usize % 256])
}

/// The pixel value for `n` observers at a place, at tile zoom `z`.
fn expected(z: u8, (lon, lat): (f64, f64), n: u64) -> u16 {
    let (_, y) = tile::pixel_xy(lon, lat).unwrap();
    let y = y >> (18 - (z + 8));
    encode::quantize(n as f64 / tile::pixel_area_km2(z + 8, y))
}

#[test]
fn counts_distinct_observers() {
    let (bucket, ranges) = serve(files());
    let out = run(&bucket, 3, 1 << 20);
    let s = &out.summary;
    assert_eq!(s.snapshot, "2026-10-01");
    assert_eq!((s.files, s.files_with_data), (3, 2));
    assert_eq!(s.records, 12);
    assert_eq!(
        (s.no_location, s.imprecise, s.no_observer, s.other_license),
        (1, 1, 1, 1)
    );
    // Datasetkey for every row group, five more columns for the three
    // with our data, and three footers.
    assert_eq!(ranges.load(Ordering::SeqCst), 4 + 5 * 3 + 3);

    let names: Vec<String> = s
        .layers
        .iter()
        .map(|l| l.output.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "ebird-observers.2026-10-01.cc-by-4.0.tiff",
            "inaturalist-observers.2026-10-01.cc-by-4.0.tiff",
            "inaturalist-observers.2026-10-01.cc-by-nc-4.0.tiff",
        ]
    );
    let stats: Vec<(u64, u64, u64, u64)> = s
        .layers
        .iter()
        .map(|l| (l.records, l.observers, l.pairs, l.pixels))
        .collect();
    // eBird: obsr1 at A and B, obsr2 at A. iNaturalist CC BY: bob at A,
    // "bob, carol" (one observer) at B. NonCommercial adds alice at A and B.
    assert_eq!(stats, [(4, 2, 3, 2), (2, 2, 2, 2), (4, 3, 4, 2)]);

    let imgs: Vec<common::Image> = out.tiffs.iter().map(|t| common::read(t)).collect();
    for img in &imgs {
        assert_eq!(img.ifds.len(), 9, "zoom 8 and eight overviews");
        assert_eq!(img.ifds[0][&256].int(), 256 << 8);
    }
    let (ebird, by, nc) = (&imgs[0], &imgs[1], &imgs[2]);
    assert_eq!(value(ebird, 8, A), expected(8, A, 2));
    assert_eq!(value(ebird, 8, B), expected(8, B, 1));
    assert_eq!(value(by, 8, A), expected(8, A, 1));
    assert_eq!(value(nc, 8, A), expected(8, A, 2));
    assert_eq!(value(nc, 8, B), expected(8, B, 2));
    // Overviews count each observer once: obsr1 reported from A and B.
    for z in 0..=5 {
        assert_eq!(value(ebird, z, A), expected(z, A, 2), "zoom {z}");
        assert_eq!(value(by, z, A), expected(z, A, 2), "zoom {z}");
        assert_eq!(value(nc, z, A), expected(z, A, 3), "zoom {z}");
    }
    let ifd = &ebird.ifds[0];
    assert!(ifd[&33432].ascii().contains("doi.org/10.15468/aomfnb"));
    assert!(ifd[&42112].ascii().contains("CC-BY-4.0"));

    // Each file and its BOM carry their own license.
    assert_eq!(
        out.boms[2]["metadata"]["component"]["licenses"][0]["license"]["id"],
        "CC-BY-NC-4.0"
    );
    assert_eq!(
        out.boms[0]["metadata"]["component"]["licenses"][0]["license"]["id"],
        "CC-BY-4.0"
    );
    let res = out.datapackage["resources"].as_array().unwrap();
    assert_eq!(res.len(), 6);
    assert_eq!(res[0]["name"], "ebird-observers-cc-by-4.0");
    assert_eq!(res[4]["path"], names[2]);
    assert_eq!(res[4]["licenses"][0]["name"], "CC-BY-NC-4.0");
    assert_eq!(res[5]["name"], "inaturalist-observers-cc-by-nc-4.0-bom");
    assert_eq!(out.datapackage["version"], "2026-10-01");
    assert_eq!(out.datapackage["sources"].as_array().unwrap().len(), 3);

    // The layers differ in license, so the STAC collection says "other"
    // and each asset states its own.
    let item = &out.item;
    assert_eq!(item["id"], "naturalist-observers-2026-10-01");
    assert!(item["properties"].get("license").is_none());
    assert_eq!(
        item["assets"]["inaturalist-observers-cc-by-nc-4.0"]["license"],
        "CC-BY-NC-4.0"
    );
    assert_eq!(out.collection["license"], "other");

    // The same input gives the same files, whatever the threads and
    // memory limit.
    let again = run(&bucket, 1, 1);
    assert_eq!(again.tiffs, out.tiffs);
}

#[test]
fn reads_a_given_snapshot() {
    let (bucket, _) = serve(files());
    let dir = tempfile::tempdir().unwrap();
    let cfg = observers::Config {
        bucket,
        snapshot: Some("2026-09-01".into()),
        output_dir: dir.path().join("out"),
        workdir: dir.path().join("work"),
        threads: 2,
        run_size: 1000,
        files: None,
        run_id: String::new(),
    };
    let s = observers::run(&cfg).unwrap();
    assert_eq!((s.files, s.records), (1, 0));
    // Empty, but valid.
    let img = common::read(&std::fs::read(&s.layers[0].output).unwrap());
    assert!(img.tiles.is_empty());
}

/// Columns much bigger than what the reader fetches at once, so it has
/// to fetch them piece by piece, with pages that straddle its windows.
#[test]
fn reads_columns_in_pieces() {
    const ROWS: usize = 200_000;
    let names: Vec<&'static str> = (0..1000)
        .map(|i| &*Box::leak(format!("obsr{i}").into_boxed_str()))
        .collect();
    let who: Vec<&'static [&'static str]> = names
        .iter()
        .map(|n| &*Box::leak(vec![*n].into_boxed_slice()))
        .collect();
    let mut rows: Vec<Row> = Vec::new();
    let mut want = std::collections::HashSet::new();
    for i in 0..ROWS {
        // Places that hardly repeat, so the columns barely compress.
        let at = (A.0 + i as f64 * 1e-7, A.1 + i as f64 * 3e-8);
        rows.push((EBIRD.key, BY, Some(at), None, who[i % 1000]));
        want.insert((
            i % 1000,
            observers::distinct::pixel_key(at.0, at.1).unwrap(),
        ));
    }
    let (bucket, ranges) = serve(vec![parquet_file_with_pages(&[&rows], 100 << 10)]);
    let out = run(&bucket, 2, 1 << 20);
    let s = &out.summary;
    assert_eq!(s.records, ROWS as u64);
    let ebird = &s.layers[0];
    assert_eq!((ebird.records, ebird.observers), (ROWS as u64, 1000));
    assert_eq!(ebird.pairs, want.len() as u64);
    // Latitudes and longitudes take over a megabyte each, so at least
    // one more request each, on top of the footer and the six columns.
    let n = ranges.load(Ordering::SeqCst);
    assert!(n >= 1 + 6 + 2, "{n} range requests");
}
