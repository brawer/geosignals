// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! End-to-end test of the transit-departures pipeline: a catalog and
//! two small feeds that share a bus, served over HTTP, through the
//! cache, with the server first up, then unchanged, then down.

mod common;

use geosignals::tile;
use geosignals::transit::{self, Licenses, condensed};
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

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A gzipped tarball of a repository, like GitHub serves for a branch:
/// a pax global header with the commit, then files in a top directory.
fn repo_tarball(files: &[(&str, &str)]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut b = tar::Builder::new(gz);
    let pax = format!("comment={COMMIT}\n");
    let record = format!("{} {pax}", pax.len() + 3);
    let mut h = tar::Header::new_ustar();
    h.set_path("pax_global_header").unwrap();
    h.set_entry_type(tar::EntryType::XGlobalHeader);
    h.set_size(record.len() as u64);
    h.set_cksum();
    b.append(&h, record.as_bytes()).unwrap();
    for (path, content) in files {
        let mut h = tar::Header::new_ustar();
        h.set_path(format!("transitous-main/{path}")).unwrap();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append(&h, content.as_bytes()).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

/// Transitous's catalog for Switzerland, pointing at `base`.
fn catalog(base: &str) -> String {
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
    )
}

struct Server {
    base: String,
    down: Arc<AtomicBool>,
    not_modified: Arc<AtomicUsize>,
}

/// Serves `files` with ETags, answering `If-None-Match` with 304, and
/// 503 for everything while `down` is set.
fn serve(mut files: HashMap<String, Vec<u8>>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    files.insert(
        "/transitous.tar.gz".into(),
        repo_tarball(&[("README.md", "hi"), ("feeds/ch.json", &catalog(&base))]),
    );
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
    // The regional publisher's own server is gone; MobilityData's copy works.
    std::fs::create_dir_all(dir).unwrap();
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
        transitous: format!("{base}/transitous.tar.gz"),
        transitland_atlas: None,
        mobility_database: Some(mdb.to_str().unwrap().to_string()),
        cache: dir.join("cache").to_str().unwrap().to_string(),
        output_dir: dir.join("out"),
        workdir: dir.join("work"),
        threads: 3,
        date: geosignals::date::days_from_civil(2026, 6, 15) * 86_400 + 3600,
        licenses: Licenses::Open,
        offline: false,
        only: Vec::new(),
        max_expired_days: 90,
        run_id: "test-run".into(),
        retry_delay: std::time::Duration::from_millis(1),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

fn sha512_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA512, data).as_ref())
}

/// Pixel value at a location in the full-resolution image.
fn value_at(img: &common::Image, (lon, lat): (f64, f64)) -> u16 {
    let (x, y) = tile::pixel_xy(lon, lat).unwrap();
    let t = &img.tiles[&(10, x >> 8, y >> 8)];
    t[(y as usize & 255) * 256 + (x as usize & 255)]
}

fn expected((lon, lat): (f64, f64), per_week: f64) -> u16 {
    let y = tile::pixel_xy(lon, lat).unwrap().1;
    geosignals::encode::quantize(per_week / 7.0 / tile::pixel_area_km2(18, y))
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
    // A week of the national feed: train 2×5, shuttle 96×7, bus 2×5;
    // of the regional one: bus 2×7, of which the weekday runs are
    // duplicates. The three weeks of June up to the run count.
    let close = |a: f64, b: f64| (a - b).abs() < 0.01;
    assert!(
        close(s.departures, (10.0 + 672.0 + 10.0 + 14.0 - 10.0) / 7.0),
        "{}",
        s.departures
    );
    // Duplicates over the current timetable: 22 weekdays in June.
    assert!(close(s.duplicates, 22.0 * 2.0 / 30.0), "{}", s.duplicates);
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
    assert_eq!(report["feeds"][0]["weeks"], 3);
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
    let names: Vec<&str> = components
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "catalogs/transitous",
            "catalogs/mobility-database",
            "gtfs/ch/national",
            "gtfs/ch/regional"
        ]
    );
    assert_eq!(components[0]["licenses"][0]["license"]["id"], "CC0-1.0");
    assert!(components[0]["hashes"][0]["content"].is_string());
    assert!(components[1].get("licenses").is_none(), "unknown license");
    let national_sha512 = sha512_hex(&national());
    assert_eq!(
        components[2]["hashes"],
        serde_json::json!([
            {"alg": "SHA-256", "content": sha256_hex(&national())},
            {"alg": "SHA-512", "content": national_sha512},
        ])
    );
    assert_eq!(components[2]["licenses"][0]["license"]["id"], "CC-BY-4.0");
    assert_eq!(components[2]["copyright"], "National Rail");
    assert_eq!(
        components[3]["licenses"][0]["license"]["url"],
        "https://regional.example/terms"
    );
    assert_eq!(
        bom["dependencies"][0]["dependsOn"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let pkg: Value = serde_json::from_slice(&std::fs::read(&s.datapackage).unwrap()).unwrap();
    assert_eq!(pkg["sources"].as_array().unwrap().len(), 4);
    assert_eq!(report["catalogs"][0]["name"], "transitous");
    assert_eq!(report["catalogs"][0]["commit"], COMMIT);
    assert_eq!(report["catalogs"][0]["status"], "changed");
    assert_eq!(pkg["version"], "2026-06-15");

    // Second run: the server says nothing changed.
    let s2 = transit::run(&cfg).unwrap();
    assert_eq!(server.not_modified.load(Ordering::SeqCst), 3);
    let report: Value = serde_json::from_slice(&std::fs::read(&s2.report).unwrap()).unwrap();
    assert_eq!(
        statuses(&report),
        [
            ("ch/national".into(), "unchanged".into()),
            ("ch/regional".into(), "unchanged".into())
        ]
    );
    assert_eq!(std::fs::read(&s2.output).unwrap(), tiff);

    // The cache holds one object per fixed key, and no meta.json.
    let cache = dir.path().join("cache");
    let mut keys = Vec::new();
    let mut todo = vec![cache.clone()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                todo.push(p);
            } else {
                keys.push(
                    p.strip_prefix(&cache)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                );
            }
        }
    }
    keys.sort();
    assert_eq!(
        keys,
        [
            "transit/ch/national/condensed.bin",
            "transit/ch/national/gtfs.zip",
            "transit/ch/national/history.bin",
            "transit/ch/regional/condensed.bin",
            "transit/ch/regional/gtfs.zip",
            "transit/ch/regional/history.bin",
            "transit/index.json",
            "transit/shared/transitous/archive.tar.gz",
            "transit/shared/transitous/transitous.json",
        ]
    );
    let condensed_path = cache.join("transit/ch/national/condensed.bin");
    let feed = condensed::Feed::decode(&std::fs::read(&condensed_path).unwrap()).unwrap();
    let input = &feed.inputs[0];
    assert_eq!(input.final_url, format!("{}/national.zip", server.base));
    assert_eq!(
        input.etag.as_deref(),
        Some(format!("\"{}\"", national().len()).as_str())
    );
    assert_eq!(input.bytes, national().len() as u64);
    assert_eq!(hex(&input.sha512), national_sha512);
    let index_path = cache.join("transit/index.json");
    let mut index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["feeds"]["ch/national"]["sha512"], national_sha512);
    assert_eq!(
        index["feeds"]["ch/national"]["etag"],
        input.etag.clone().unwrap()
    );

    // The index is only a hint. Where it disagrees with the header, the
    // run asks again with what the header says.
    index["feeds"]["ch/national"]["sha256"] = "00".repeat(32).into();
    std::fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    let s3 = transit::run(&cfg).unwrap();
    assert_eq!(server.not_modified.load(Ordering::SeqCst), 3 + 4);
    assert_eq!((s3.feeds, s3.stale), (2, 0));
    assert_eq!(std::fs::read(&s3.output).unwrap(), tiff);
    let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(
        index["feeds"]["ch/national"]["sha256"],
        sha256_hex(&national())
    );

    // Without an index, the headers say what is cached.
    std::fs::remove_file(&index_path).unwrap();

    // Third run: the server is down, so the cached copies are used.
    server.down.store(true, Ordering::SeqCst);
    let s3 = transit::run(&cfg).unwrap();
    assert_eq!((s3.feeds, s3.stale), (2, 2));
    assert_eq!(std::fs::read(&s3.output).unwrap(), tiff);
    let bom: Value = serde_json::from_slice(&std::fs::read(&s3.bom).unwrap()).unwrap();
    assert_eq!(
        bom["components"][2]["hashes"][1]["content"],
        national_sha512
    );
    let report: Value = serde_json::from_slice(&std::fs::read(&s3.report).unwrap()).unwrap();
    assert_eq!(report["catalogs"][0]["status"], "stale");
    assert_eq!(report["catalogs"][0]["commit"], COMMIT);
    assert!(
        report["feeds"][0]["error"]
            .as_str()
            .unwrap()
            .contains("503")
    );

    // Offline, with an empty cache, there is no catalog to read.
    let mut empty = config(&dir.path().join("other"), &server.base);
    empty.offline = true;
    let err = transit::run(&empty).unwrap_err();
    assert!(err.to_string().contains("transitous catalog"), "{err}");

    // A local checkout works offline, but no feed is cached.
    let checkout = dir.path().join("checkout");
    std::fs::create_dir_all(checkout.join("feeds")).unwrap();
    std::fs::write(checkout.join("feeds/ch.json"), catalog(&server.base)).unwrap();
    empty.transitous = checkout.to_str().unwrap().to_string();
    let err = transit::run(&empty).unwrap_err();
    assert!(err.to_string().contains("no feed"), "{err}");
}

/// A record of a CIF file: 80 characters with fields at their offsets.
fn cif_record(fields: &[(usize, &str)]) -> String {
    let mut b = vec![b' '; 80];
    for (at, s) in fields {
        b[*at..*at + s.len()].copy_from_slice(s.as_bytes());
    }
    String::from_utf8(b).unwrap()
}

/// A gzipped CIF timetable: one train A→B→C on weekdays in June 2026.
fn cif_timetable() -> Vec<u8> {
    let lines = [
        cif_record(&[(0, "HD")]),
        cif_record(&[
            (0, "BSN"),
            (3, "C12345"),
            (9, "260601"),
            (15, "260630"),
            (21, "1111100"),
            (29, "P"),
            (79, "P"),
        ]),
        cif_record(&[(0, "BX"), (11, "GW")]),
        cif_record(&[
            (0, "LO"),
            (2, "AAAA"),
            (10, "0700 "),
            (15, "0700"),
            (29, "TB"),
        ]),
        cif_record(&[
            (0, "LI"),
            (2, "BBBB"),
            (10, "0710 "),
            (15, "0711 "),
            (25, "0710"),
            (29, "0711"),
            (42, "T "),
        ]),
        cif_record(&[
            (0, "LT"),
            (2, "CCCC"),
            (10, "0720 "),
            (15, "0720"),
            (25, "TF"),
        ]),
        cif_record(&[(0, "ZZ")]),
    ];
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(lines.join("\n").as_bytes()).unwrap();
    gz.finish().unwrap()
}

fn naptan() -> String {
    let mut s = String::from("ATCOCode,CommonName,Longitude,Latitude,StopType\n");
    for (t, (lon, lat)) in [("AAAA", A), ("BBBB", B), ("CCCC", C)] {
        s.push_str(&format!("9100{t},{t},{lon},{lat},RLY\n"));
    }
    s
}

/// Like Network Rail's feed server: `/cif` needs basic authentication
/// and redirects to a signed URL, which must come without it.
/// Path and `Authorization` header of each request.
type RequestLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn serve_cif() -> (String, RequestLog) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let (mut path, mut auth) = (String::new(), None);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(p) = line.strip_prefix("GET ") {
                    path = p.split(' ').next().unwrap().to_string();
                } else if line.to_ascii_lowercase().starts_with("authorization: ") {
                    auth = Some(line[15..].trim().to_string());
                }
            }
            log.lock().unwrap().push((path.clone(), auth.clone()));
            let reply = |status: &str, extra: &str, body: &[u8]| {
                let mut out = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(body);
                out
            };
            let out = match (path.as_str(), auth.as_deref()) {
                // "user:secret"
                ("/cif", Some("Basic dXNlcjpzZWNyZXQ=")) => {
                    reply("302 Found", "Location: /signed/cif.gz?sig=x\r\n", b"")
                }
                ("/cif", _) => reply("401 Unauthorized", "", b""),
                ("/signed/cif.gz?sig=x", None) => {
                    reply("200 OK", "ETag: \"cif1\"\r\n", &cif_timetable())
                }
                ("/naptan.csv", None) => reply("200 OK", "ETag: \"n1\"\r\n", naptan().as_bytes()),
                _ => reply("400 Bad Request", "", b""),
            };
            let _ = stream.write_all(&out);
        }
    });
    (base, requests)
}

#[test]
fn fetches_cif_with_login() {
    use transit::catalog::{Feed, Format, Stations};
    use transit::fetch::{Fetcher, Status};
    let (base, requests) = serve_cif();
    let dir = tempfile::tempdir().unwrap();
    let store = geosignals::store::open(dir.path().join("cache").to_str().unwrap()).unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut fetcher = Fetcher::new(store.as_ref(), &tmp);
    fetcher.now = geosignals::date::days_from_civil(2026, 6, 15) * 86_400;
    fetcher.retry_delay = std::time::Duration::from_millis(1);
    // SAFETY: no other test reads or writes this variable.
    unsafe {
        std::env::set_var("GEOSIGNALS_TEST_CIF_USERNAME", "user");
        std::env::set_var("GEOSIGNALS_TEST_CIF_PASSWORD", "secret");
    }
    let feed = Feed {
        id: "gb/networkrail".into(),
        urls: vec![format!("{base}/cif")],
        format: Format::Cif,
        stations: Some(Stations {
            url: format!("{base}/naptan.csv"),
            ..Default::default()
        }),
        credentials: Some("GEOSIGNALS_TEST_CIF".into()),
        ..Default::default()
    };
    let f = fetcher.fetch(&feed).unwrap();
    assert_eq!(f.status, Status::Changed);
    assert_eq!(f.meta.url, format!("{base}/cif"));
    assert!(f.stations.is_some());
    let cache = dir.path().join("cache/transit");
    for key in [
        "gb/networkrail/condensed.bin",
        "gb/networkrail/cif.gz",
        "shared/naptan/naptan.csv",
        "shared/naptan/naptan.json",
    ] {
        assert!(cache.join(key).is_file(), "{key} is not cached");
    }
    let c = &f.feed().unwrap().median_week(None).unwrap();
    assert_eq!(c.week_start, geosignals::date::days_from_civil(2026, 6, 8));
    assert_eq!(c.departures_per_week(), 2 * 5);
    let stops: Vec<_> = c
        .trip_stops(&c.trips[0])
        .iter()
        .map(|s| (s.x, s.y, s.minute))
        .collect();
    let (a, b) = (
        tile::pixel_xy(A.0, A.1).unwrap(),
        tile::pixel_xy(B.0, B.1).unwrap(),
    );
    assert_eq!(stops, [(a.0, a.1, 7 * 60), (b.0, b.1, 7 * 60 + 11)]);
    let log = requests.lock().unwrap().clone();
    assert!(
        log.contains(&("/signed/cif.gz?sig=x".to_string(), None)),
        "the password must not follow the redirect: {log:?}"
    );

    // Without credentials, the publisher refuses, and the cache serves.
    unsafe {
        std::env::remove_var("GEOSIGNALS_TEST_CIF_PASSWORD");
    }
    let f2 = fetcher.fetch(&feed).unwrap();
    assert!(matches!(f2.status, Status::Stale { .. }), "{:?}", f2.status);
    assert_eq!(f2.data[..], f.data[..]);
}
