// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end test of the transit-departures pipeline: two small feeds
//! that share a bus, served over HTTP, through the cache, with the
//! server first up, then unchanged, then down.

mod common;

use osmsignals::tile;
use osmsignals::transit::{self, Licenses};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const A: (f64, f64) = (8.5402, 47.3782);
const B: (f64, f64) = (8.5502, 47.3882);
const C: (f64, f64) = (8.5602, 47.3982);
const D: (f64, f64) = (8.5702, 47.4082);

const CALENDAR: &str = "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
    wd,1,1,1,1,1,0,0,20260601,20260630\n\
    daily,1,1,1,1,1,1,1,20260601,20260630\n";

fn stops() -> String {
    let mut s = String::from("stop_id,stop_name,stop_lat,stop_lon\n");
    for (id, (lon, lat)) in [("A", A), ("B", B), ("C", C), ("D", D)] {
        s.push_str(&format!("{id},{id},{lat},{lon}\n"));
    }
    s
}

fn zip(files: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, content) in files {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(content.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// The national feed: a train A→B→C on weekdays, a shuttle A→B every
/// 10 minutes all day, and the regional bus B→C→D on weekdays.
fn national() -> Vec<u8> {
    zip(&[
        (
            "agency.txt",
            "agency_id,agency_name\nsbb,National Rail\nrb,Regio Bus\n",
        ),
        (
            "routes.txt",
            "route_id,agency_id,route_type\ntrain,sbb,2\nshuttle,sbb,3\nbus,rb,3\n",
        ),
        ("calendar.txt", CALENDAR),
        ("stops.txt", &stops()),
        (
            "trips.txt",
            "route_id,service_id,trip_id\ntrain,wd,n1\nshuttle,daily,n3\nbus,wd,r1\n",
        ),
        (
            "stop_times.txt",
            "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
             n1,08:00:00,08:00:00,A,1\nn1,08:05:00,08:05:00,B,2\nn1,08:10:00,08:10:00,C,3\n\
             n3,06:00:00,06:00:00,A,1\nn3,06:04:00,06:04:00,B,2\n\
             r1,09:00:00,09:00:00,B,1\nr1,09:03:00,09:03:00,C,2\nr1,09:06:00,09:06:00,D,3\n",
        ),
        (
            "frequencies.txt",
            "trip_id,start_time,end_time,headway_secs\nn3,06:00:00,22:00:00,600\n",
        ),
    ])
}

/// The regional feed: the same bus, but every day.
fn regional() -> Vec<u8> {
    zip(&[
        ("agency.txt", "agency_id,agency_name\nrb,Regio Bus\n"),
        ("routes.txt", "route_id,agency_id,route_type\nbus,rb,3\n"),
        ("calendar.txt", CALENDAR),
        ("stops.txt", &stops()),
        ("trips.txt", "route_id,service_id,trip_id\nbus,daily,r1\n"),
        (
            "stop_times.txt",
            "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
             r1,09:00:00,09:00:00,B,1\nr1,09:03:00,09:03:00,C,2\nr1,09:06:00,09:06:00,D,3\n",
        ),
    ])
}

struct Server {
    base: String,
    down: Arc<AtomicBool>,
    not_modified: Arc<AtomicUsize>,
}

/// Serves `files` with ETags, answering `If-None-Match` with 304, and
/// 503 for everything while `down` is set.
fn serve(files: HashMap<String, Vec<u8>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let down = Arc::new(AtomicBool::new(false));
    let not_modified = Arc::new(AtomicUsize::new(0));
    let files = Arc::new(Mutex::new(files));
    let (d, nm) = (down.clone(), not_modified.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut path, mut if_none_match) = (String::new(), None);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(p) = line.strip_prefix("GET ") {
                    path = p.split(' ').next().unwrap().to_string();
                } else if let Some(v) = line.to_ascii_lowercase().strip_prefix("if-none-match: ") {
                    if_none_match = Some(v.trim().to_string());
                }
            }
            let reply = |status: &str, etag: &str, body: &[u8]| {
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let mut out = head.into_bytes();
                out.extend_from_slice(body);
                out
            };
            let out = if d.load(Ordering::SeqCst) {
                reply("503 Service Unavailable", "\"-\"", b"")
            } else {
                match files.lock().unwrap().get(&path) {
                    None => reply("404 Not Found", "\"-\"", b""),
                    Some(data) => {
                        let etag = format!("\"{}\"", data.len());
                        if if_none_match.as_deref() == Some(etag.as_str()) {
                            nm.fetch_add(1, Ordering::SeqCst);
                            reply("304 Not Modified", &etag, b"")
                        } else {
                            reply("200 OK", &etag, data)
                        }
                    }
                }
            };
            let _ = stream.write_all(&out);
        }
    });
    Server {
        base,
        down,
        not_modified,
    }
}

fn config(dir: &Path, base: &str) -> transit::Config {
    let feeds = dir.join("feeds");
    std::fs::create_dir_all(&feeds).unwrap();
    std::fs::write(
        feeds.join("ch.json"),
        format!(
            r#"{{"sources": [
                {{"name": "national", "type": "http", "url": "{base}/national.zip",
                  "license": {{"spdx-identifier": "CC-BY-4.0", "attribution-text": "National Rail"}}}},
                {{"name": "regional", "type": "mobility-database", "mdb-id": "mdb-7",
                  "license": {{"spdx-identifier": "ODbL-1.0"}}}},
                {{"name": "closed", "type": "http", "url": "{base}/closed.zip",
                  "license": {{"spdx-identifier": "CC-BY-NC-4.0"}}}},
                {{"name": "realtime", "type": "url", "spec": "gtfs-rt", "url": "{base}/rt"}}
            ]}}"#
        ),
    )
    .unwrap();
    // The regional publisher's own server is gone; MobilityData's copy works.
    let mdb = dir.join("mdb.csv");
    std::fs::write(
        &mdb,
        format!(
            "id,data_type,urls.direct_download,urls.latest,urls.license\n\
             mdb-7,gtfs,{base}/gone.zip,{base}/mirror/regional.zip,https://regional.example/terms\n"
        ),
    )
    .unwrap();
    transit::Config {
        feeds,
        transitland_atlas: None,
        mobility_database: Some(mdb.to_str().unwrap().to_string()),
        cache: dir.join("cache").to_str().unwrap().to_string(),
        output_dir: dir.join("out"),
        workdir: dir.join("work"),
        threads: 3,
        date: osmsignals::date::days_from_civil(2026, 6, 15) * 86_400 + 3600,
        licenses: Licenses::Open,
        offline: false,
        only: Vec::new(),
        max_expired_days: 90,
        run_id: "test-run".into(),
        retry_delay: std::time::Duration::from_millis(1),
    }
}

/// Pixel value at a location in the full-resolution image.
fn value_at(img: &common::Image, (lon, lat): (f64, f64)) -> u16 {
    let (x, y) = tile::pixel_xy(lon, lat).unwrap();
    let t = &img.tiles[&(10, x >> 8, y >> 8)];
    t[(y as usize & 255) * 256 + (x as usize & 255)]
}

fn expected((lon, lat): (f64, f64), per_week: f64) -> u16 {
    let y = tile::pixel_xy(lon, lat).unwrap().1;
    osmsignals::encode::quantize(per_week / 7.0 / tile::pixel_area_km2(18, y))
}

fn statuses(report: &Value) -> Vec<(String, String)> {
    report["feeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["id"].as_str().unwrap().to_string(),
                f["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn counts_departures_through_cache() {
    let files = HashMap::from([
        ("/national.zip".to_string(), national()),
        ("/mirror/regional.zip".to_string(), regional()),
    ]);
    let server = serve(files);
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &server.base);

    // First run: everything is downloaded.
    let s = transit::run(&cfg).unwrap();
    assert_eq!(
        s.output.file_name().unwrap(),
        "transit-departures.2026-06-15.tiff"
    );
    assert_eq!((s.feeds, s.stale, s.failed, s.expired), (2, 0, 0, 0));
    // National: train 2×5, shuttle 96×7, bus 2×5; regional: bus 2×7.
    assert_eq!(s.departures, 10 + 672 + 10 + 14);
    assert_eq!(s.duplicates, 10, "the regional bus on weekdays");
    let tiff = std::fs::read(&s.output).unwrap();
    let img = common::read(&tiff);
    let copyright = img.ifds[0][&33432].ascii();
    assert!(
        copyright.starts_with("Public transport data from 2 GTFS feeds"),
        "{copyright}"
    );
    assert_eq!(value_at(&img, A), expected(A, 5.0 + 672.0));
    assert_eq!(value_at(&img, B), expected(B, 5.0 + 5.0 + 2.0));
    assert_eq!(value_at(&img, C), expected(C, 5.0 + 2.0));
    assert_eq!(value_at(&img, D), 0, "where trips end, nothing departs");

    let report: Value = serde_json::from_slice(&std::fs::read(&s.report).unwrap()).unwrap();
    assert_eq!(
        statuses(&report),
        [
            ("ch/national".into(), "changed".into()),
            ("ch/regional".into(), "changed".into())
        ]
    );
    assert_eq!(
        report["feeds"][1]["url"],
        format!("{}/mirror/regional.zip", server.base)
    );
    assert_eq!(report["feeds"][0]["median_week"], "2026-06-08");
    assert_eq!(report["duplicates"][0]["feed"], "ch/regional");
    assert_eq!(report["duplicates"][0]["also_in"], "ch/national");
    assert_eq!(
        report["duplicates"][0]["by_agency"][0]["agency"],
        "Regio Bus"
    );
    let skipped = report["skipped"].to_string();
    assert!(skipped.contains("CC-BY-NC-4.0"), "{skipped}");

    let bom: Value = serde_json::from_slice(&std::fs::read(&s.bom).unwrap()).unwrap();
    let components = bom["components"].as_array().unwrap();
    assert_eq!(components.len(), 2);
    assert_eq!(components[0]["name"], "gtfs/ch/national");
    assert_eq!(components[0]["licenses"][0]["license"]["id"], "CC-BY-4.0");
    assert_eq!(components[0]["copyright"], "National Rail");
    assert_eq!(
        components[1]["licenses"][0]["license"]["url"],
        "https://regional.example/terms"
    );
    assert_eq!(
        bom["dependencies"][0]["dependsOn"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let pkg: Value = serde_json::from_slice(&std::fs::read(&s.datapackage).unwrap()).unwrap();
    assert_eq!(pkg["sources"].as_array().unwrap().len(), 2);
    assert_eq!(pkg["version"], "2026-06-15");

    // Second run: the server says nothing changed.
    let s2 = transit::run(&cfg).unwrap();
    assert_eq!(server.not_modified.load(Ordering::SeqCst), 2);
    let report: Value = serde_json::from_slice(&std::fs::read(&s2.report).unwrap()).unwrap();
    assert_eq!(
        statuses(&report),
        [
            ("ch/national".into(), "unchanged".into()),
            ("ch/regional".into(), "unchanged".into())
        ]
    );
    assert_eq!(std::fs::read(&s2.output).unwrap(), tiff);

    // Third run: the server is down, so the cached copies are used.
    server.down.store(true, Ordering::SeqCst);
    let s3 = transit::run(&cfg).unwrap();
    assert_eq!((s3.feeds, s3.stale), (2, 2));
    assert_eq!(std::fs::read(&s3.output).unwrap(), tiff);
    let report: Value = serde_json::from_slice(&std::fs::read(&s3.report).unwrap()).unwrap();
    assert!(
        report["feeds"][0]["error"]
            .as_str()
            .unwrap()
            .contains("503")
    );

    // Offline, with an empty cache, there is nothing to read.
    let mut empty = config(&dir.path().join("other"), &server.base);
    empty.offline = true;
    let err = transit::run(&empty).unwrap_err();
    assert!(err.to_string().contains("no feed"), "{err}");
}
