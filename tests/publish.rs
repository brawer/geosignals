// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Checks the optional upload to public S3 storage, against a fake S3
//! server that records what it receives.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

const ZUGERLAND: &str = "tests/data/zugerland.osm.pbf";

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// Accepts every request with 200 OK and records it.
fn serve() -> (String, Arc<Mutex<Vec<Request>>>) {
    serve_listing(Vec::new())
}

/// Like [`serve`], but answers bucket listings with `pages`, one per
/// request, then with empty listings.
fn serve_listing(pages: Vec<String>) -> (String, Arc<Mutex<Vec<Request>>>) {
    let pages = Arc::new(Mutex::new(std::collections::VecDeque::from(pages)));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut parts = first.split(' ');
            let method = parts.next().unwrap().to_string();
            let path = parts.next().unwrap().to_string();
            let mut headers = BTreeMap::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let (k, v) = line.split_once(':').unwrap();
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
            let mut body = Vec::new();
            if let Some(n) = headers.get("content-length") {
                body.resize(n.parse().unwrap(), 0);
                reader.read_exact(&mut body).unwrap();
            } else if headers.get("transfer-encoding").map(String::as_str) == Some("chunked") {
                loop {
                    let mut size = String::new();
                    reader.read_line(&mut size).unwrap();
                    let n = usize::from_str_radix(size.trim(), 16).unwrap();
                    let mut chunk = vec![0; n + 2];
                    reader.read_exact(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
            }
            let answer = if method == "GET" && path.contains("list-type=2") {
                pages.lock().unwrap().pop_front().unwrap_or_default()
            } else {
                String::new()
            };
            l.lock().unwrap().push(Request {
                method,
                path,
                headers,
                body,
            });
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .as_bytes(),
            );
        }
    });
    (base, log)
}

/// Runs osm-node-density on the test extract with `env`, after
/// removing any PUBLIC_S3_ variables of the caller.
fn run(out: &Path, work: &Path, extra: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_geosignals"));
    cmd.args(["osm-node-density", "--input", ZUGERLAND, "--output-dir"])
        .arg(out)
        .arg("--workdir")
        .arg(work)
        .args(extra);
    for (k, _) in std::env::vars() {
        if k.starts_with("PUBLIC_S3_") {
            cmd.env_remove(k);
        }
    }
    cmd.envs(env.iter().copied());
    cmd.output().unwrap()
}

#[test]
fn uploads_release() {
    let (endpoint, log) = serve();
    let dir = tempfile::tempdir().unwrap();
    let (out, work) = (dir.path().join("out"), dir.path().join("work"));
    let result = run(
        &out,
        &work,
        &[],
        &[
            ("PUBLIC_S3_URL", "s3://geosignals/signals/"),
            ("PUBLIC_S3_ENDPOINT_URL", &endpoint),
            ("PUBLIC_S3_ACCESS_KEY_ID", "AKID"),
            ("PUBLIC_S3_SECRET_ACCESS_KEY", "secret"),
        ],
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");

    let log = log.lock().unwrap();
    let puts: Vec<&Request> = log.iter().filter(|r| r.method == "PUT").collect();
    let names: Vec<&str> = puts
        .iter()
        .map(|r| {
            assert_eq!(r.method, "PUT");
            r.path
                .strip_prefix("/geosignals/signals/osm-node-density/")
                .unwrap_or_else(|| panic!("unexpected path {}", r.path))
        })
        .collect();
    // Two GeoTIFFs and their BOMs, then the undated STAC item last,
    // after what it points to.
    assert_eq!(names.len(), 5, "{names:?}");
    assert_eq!(names[4], "osm-node-density.stac.json");
    assert_eq!(names.iter().filter(|f| f.ends_with(".tiff")).count(), 2);
    assert_eq!(names.iter().filter(|f| f.ends_with(".cdx.json")).count(), 2);

    for (r, name) in puts.iter().zip(&names) {
        assert_eq!(r.body, std::fs::read(out.join(name)).unwrap(), "{name}");
        let auth = &r.headers["authorization"];
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
            "{auth}"
        );
        assert!(auth.contains("/auto/s3/aws4_request"), "{auth}");
        let expected_type = if name.ends_with(".tiff") {
            "image/tiff; application=geotiff; profile=cloud-optimized"
        } else if name.ends_with(".cdx.json") {
            "application/vnd.cyclonedx+json"
        } else {
            assert!(name.ends_with(".stac.json"), "{name}");
            "application/geo+json"
        };
        assert_eq!(r.headers["content-type"], expected_type, "{name}");
    }
}

/// A ListObjectsV2 answer for `files` in the folder `dir`.
fn listing(dir: &str, files: &[&str], next: Option<&str>) -> String {
    let mut xml = String::from("<ListBucketResult>");
    for f in files {
        xml.push_str(&format!(
            "<Contents><Key>signals/{dir}/{f}</Key></Contents>"
        ));
    }
    match next {
        Some(t) => xml.push_str(&format!(
            "<IsTruncated>true</IsTruncated><NextContinuationToken>{t}</NextContinuationToken>"
        )),
        None => xml.push_str("<IsTruncated>false</IsTruncated>"),
    }
    xml.push_str("</ListBucketResult>");
    xml
}

#[test]
fn deletes_old_releases() {
    let dir = "osm-node-density";
    let (endpoint, log) = serve_listing(vec![
        listing(
            dir,
            &[
                "osm-node-density.2024-01-01.tiff",
                "osm-node-density.2024-01-01.cdx.json",
                "osm-node-density.2024-02-01.tiff",
                "datapackage.json",
                "osm-node-density.stac.json",
            ],
            Some("page/2"),
        ),
        listing(
            dir,
            &[
                "osm-node-density.2024-03-01.tiff",
                "osm-node-density.2024-04-01.tiff",
            ],
            None,
        ),
    ]);
    let tmp = tempfile::tempdir().unwrap();
    let (out, work) = (tmp.path().join("out"), tmp.path().join("work"));
    let result = run(
        &out,
        &work,
        &[],
        &[
            ("PUBLIC_S3_URL", "s3://geosignals/signals"),
            ("PUBLIC_S3_ENDPOINT_URL", &endpoint),
            ("PUBLIC_S3_ACCESS_KEY_ID", "AKID"),
            ("PUBLIC_S3_SECRET_ACCESS_KEY", "secret"),
        ],
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(!stderr.contains("warning"), "{stderr}");

    let log = log.lock().unwrap();
    let lists: Vec<&str> = log
        .iter()
        .filter(|r| r.method == "GET")
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(
        lists,
        [
            "/geosignals?list-type=2&prefix=signals%2Fosm-node-density%2F",
            "/geosignals?continuation-token=page%2F2&list-type=2&prefix=signals%2Fosm-node-density%2F",
        ]
    );
    // The upload is the newest release, so 2024-04-01 and 2024-03-01
    // stay, and so does the STAC item.
    let deleted: Vec<&str> = log
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| {
            r.path
                .strip_prefix("/geosignals/signals/osm-node-density/")
                .unwrap()
        })
        .collect();
    assert_eq!(
        deleted,
        [
            "osm-node-density.2024-01-01.tiff",
            "osm-node-density.2024-01-01.cdx.json",
            "osm-node-density.2024-02-01.tiff",
            "datapackage.json",
        ]
    );
    // Deleting comes after uploading.
    let first_delete = log.iter().position(|r| r.method == "DELETE").unwrap();
    let last_put = log.iter().rposition(|r| r.method == "PUT").unwrap();
    assert!(last_put < first_delete);
}

#[test]
fn skips_upload_without_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let (out, work) = (dir.path().join("out"), dir.path().join("work"));
    let result = run(&out, &work, &["--smoke-test"], &[]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(stderr.contains("nothing gets uploaded"), "{stderr}");
}

#[test]
fn skips_upload_of_smoke_test() {
    let (endpoint, log) = serve();
    let dir = tempfile::tempdir().unwrap();
    let (out, work) = (dir.path().join("out"), dir.path().join("work"));
    let result = run(
        &out,
        &work,
        &["--smoke-test"],
        &[
            ("PUBLIC_S3_URL", "s3://geosignals"),
            ("PUBLIC_S3_ENDPOINT_URL", &endpoint),
            ("PUBLIC_S3_ACCESS_KEY_ID", "AKID"),
            ("PUBLIC_S3_SECRET_ACCESS_KEY", "secret"),
        ],
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(stderr.contains("not uploading"), "{stderr}");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn incomplete_configuration_fails_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (out, work) = (dir.path().join("out"), dir.path().join("work"));
    let result = run(&out, &work, &[], &[("PUBLIC_S3_ACCESS_KEY_ID", "AKID")]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(stderr.contains("PUBLIC_S3_URL is not set"), "{stderr}");
    assert!(!out.exists(), "ran before checking the configuration");

    let result = run(&out, &work, &[], &[("PUBLIC_S3_URL", "s3://geosignals")]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(
        stderr.contains("PUBLIC_S3_ACCESS_KEY_ID is not set"),
        "{stderr}"
    );
    assert!(!out.exists(), "ran before checking the configuration");
}
