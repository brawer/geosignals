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
            l.lock().unwrap().push(Request {
                method,
                path,
                headers,
                body,
            });
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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
            ("PUBLIC_S3_SESSION_TOKEN", "token"),
        ],
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "{stderr}");

    let log = log.lock().unwrap();
    let names: Vec<&str> = log
        .iter()
        .map(|r| {
            assert_eq!(r.method, "PUT");
            r.path
                .strip_prefix("/geosignals/signals/osm-node-density/")
                .unwrap_or_else(|| panic!("unexpected path {}", r.path))
        })
        .collect();
    let n = names.len();
    assert!(n >= 5, "{names:?}");
    // The files with fixed names go last, after what they point to.
    assert_eq!(names[n - 2], "datapackage.json");
    assert_eq!(names[n - 1], "collection.json");
    assert!(names[n - 3].ends_with(".stac.json"), "{names:?}");
    assert!(names.iter().any(|f| f.ends_with(".tiff")), "{names:?}");
    assert!(names.iter().any(|f| f.ends_with(".cdx.json")), "{names:?}");

    for (r, name) in log.iter().zip(&names) {
        assert_eq!(r.body, std::fs::read(out.join(name)).unwrap(), "{name}");
        assert_eq!(r.headers["x-amz-security-token"], "token");
        let auth = &r.headers["authorization"];
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
            "{auth}"
        );
        assert!(auth.contains("/auto/s3/aws4_request"), "{auth}");
        assert!(auth.contains("x-amz-security-token"), "{auth}");
        let expected_type = if name.ends_with(".tiff") {
            "image/tiff; application=geotiff; profile=cloud-optimized"
        } else if name.ends_with(".cdx.json") {
            "application/vnd.cyclonedx+json"
        } else if name.ends_with(".stac.json") {
            "application/geo+json"
        } else {
            "application/json"
        };
        assert_eq!(r.headers["content-type"], expected_type, "{name}");
    }
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
