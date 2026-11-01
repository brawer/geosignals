// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! A small key-value object store for caching inputs between runs:
//! either a local directory or an S3-compatible bucket.
//!
//! Signals that download many inputs, such as the transit feeds, keep
//! the latest copy of each input here, so a run can go on with the
//! cached copy when a source is down, and can skip downloads that have
//! not changed.
//!
//! `--cache s3://bucket/prefix` talks to S3 with path-style URLs and
//! AWS Signature Version 4, configured from environment variables:
//! `CACHE_S3_ACCESS_KEY_ID`, `CACHE_S3_SECRET_ACCESS_KEY`,
//! `CACHE_S3_REGION` (default `auto`, which Cloudflare R2 and others
//! accept), `CACHE_S3_ENDPOINT_URL` (default Amazon S3 for the
//! region) and, for temporary credentials, `CACHE_S3_SESSION_TOKEN`.
//! The standard `AWS_*` variables are ignored. [`crate::publish`] uses
//! the same client with `PUBLIC_S3_*` variables. Payloads are
//! sent unsigned, so uploads stream from disk without being read twice.
//! Files over [`PART_SIZE`] go up as multipart uploads, in parts of
//! that size, so a failed request repeats one part, not the whole file.
//! Anything else is a local directory.

use anyhow::{Context, Result, bail, ensure};
use ring::{digest, hmac};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use ureq::http::StatusCode;

pub trait Store: Send + Sync {
    /// The object at `key`, or `None` if there is none.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Copies the object at `key` to the file `dest`; false if there is none.
    fn download(&self, key: &str, dest: &Path) -> Result<bool>;

    /// Stores `data` at `key`, replacing what was there.
    fn put(&self, key: &str, data: &[u8]) -> Result<()>;

    /// Stores the content of file `src` at `key`.
    fn upload(&self, key: &str, src: &Path) -> Result<()>;

    /// Where the store is, for log messages.
    fn describe(&self) -> String;
}

/// Opens `s3://bucket/prefix` or a local directory.
pub fn open(location: &str) -> Result<Box<dyn Store>> {
    if location.starts_with("s3://") {
        let (bucket, prefix) = parse_s3_url(location)?;
        return Ok(Box::new(S3::from_env("CACHE_S3_", bucket, prefix)?));
    }
    Ok(Box::new(LocalDir::new(location)?))
}

/// Splits `s3://bucket/prefix` into bucket and prefix.
pub fn parse_s3_url(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("s3://")
        .with_context(|| format!("{url} is not an s3:// URL"))?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    ensure!(!bucket.is_empty(), "no bucket in {url}");
    Ok((bucket, prefix))
}

/// Rejects keys that could escape the store's root.
fn check_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && !key.starts_with('/')
            && key
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "bad object key {key:?}"
    );
    Ok(())
}

/// A directory on the local file system; keys are relative paths.
pub struct LocalDir {
    root: PathBuf,
}

impl LocalDir {
    pub fn new(root: impl Into<PathBuf>) -> Result<LocalDir> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(LocalDir { root })
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(key))
    }

    /// Writes via a temporary file in the same directory, so readers
    /// never see half an object.
    fn write_atomic(&self, key: &str, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let path = self.path(key)?;
        let dir = path.parent().expect("key has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = tempfile::NamedTempFile::new_in(dir)?;
        write(tmp.path())?;
        tmp.persist(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

impl Store for LocalDir {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        match std::fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn download(&self, key: &str, dest: &Path) -> Result<bool> {
        let path = self.path(key)?;
        match std::fs::copy(&path, dest) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("copying {}", path.display())),
        }
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.write_atomic(key, |tmp| Ok(std::fs::write(tmp, data)?))
    }

    fn upload(&self, key: &str, src: &Path) -> Result<()> {
        self.write_atomic(key, |tmp| {
            std::fs::copy(src, tmp).with_context(|| format!("copying {}", src.display()))?;
            Ok(())
        })
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// Credentials and location of an S3 bucket.
pub struct S3 {
    agent: ureq::Agent,
    endpoint: String,
    bucket: String,
    prefix: String,
    region: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    part_size: u64,
}

/// Attempts per request before giving up.
const S3_ATTEMPTS: u32 = 5;

/// Files larger than this are uploaded in parts of this size. S3 wants
/// at least 5 MiB per part except the last, and at most 10,000 parts,
/// so files can be up to 500 GiB.
pub const PART_SIZE: u64 = 50 << 20;

impl S3 {
    /// Reads the credentials from environment variables whose names
    /// start with `env_prefix`, such as `CACHE_S3_ACCESS_KEY_ID`.
    pub fn from_env(env_prefix: &str, bucket: &str, prefix: &str) -> Result<S3> {
        let var = |name: &str| {
            let name = format!("{env_prefix}{name}");
            match std::env::var(&name) {
                Ok(v) if !v.is_empty() => Ok(v),
                _ => Err(name),
            }
        };
        let region = var("REGION").unwrap_or_else(|_| "auto".into());
        let endpoint =
            var("ENDPOINT_URL").unwrap_or_else(|_| format!("https://s3.{region}.amazonaws.com"));
        let access_key = var("ACCESS_KEY_ID").map_err(|n| anyhow::anyhow!("{n} is not set"))?;
        let secret_key = var("SECRET_ACCESS_KEY").map_err(|n| anyhow::anyhow!("{n} is not set"))?;
        let session_token = var("SESSION_TOKEN").ok();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .build()
            .into();
        Ok(S3 {
            agent,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region,
            access_key,
            secret_key,
            session_token,
            part_size: PART_SIZE,
        })
    }

    /// The path part of the URL for `key`, URI-encoded as SigV4 wants it.
    fn object_path(&self, key: &str) -> Result<String> {
        check_key(key)?;
        let full = if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        };
        Ok(format!(
            "/{}/{}",
            uri_encode(&self.bucket, false),
            uri_encode(&full, true)
        ))
    }

    /// Sends a signed request, retrying transient failures. `query` is
    /// the canonical query string, with names sorted and values
    /// URI-encoded, or empty. `send` gets the URL and the headers to
    /// set, and is called again for each attempt.
    fn send(
        &self,
        method: &str,
        key: &str,
        query: &str,
        mut send: impl FnMut(&str, &[(&str, String)]) -> Result<Response, ureq::Error>,
    ) -> Result<Response> {
        let path = self.object_path(key)?;
        let url = if query.is_empty() {
            format!("{}{path}", self.endpoint)
        } else {
            format!("{}{path}?{query}", self.endpoint)
        };
        let host = host_of(&self.endpoint)?;
        let mut backoff = Duration::from_secs(1);
        let mut attempt = 1;
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            let amz_date = amz_date(now);
            let mut signed = vec![
                ("host", host.as_str()),
                ("x-amz-content-sha256", UNSIGNED_PAYLOAD),
                ("x-amz-date", amz_date.as_str()),
            ];
            if let Some(token) = &self.session_token {
                signed.push(("x-amz-security-token", token.as_str()));
            }
            let auth = authorization(
                &self.access_key,
                &self.secret_key,
                &self.region,
                method,
                &path,
                query,
                &signed,
                UNSIGNED_PAYLOAD,
            );
            let mut headers = vec![
                ("x-amz-content-sha256", UNSIGNED_PAYLOAD.to_string()),
                ("x-amz-date", amz_date.clone()),
                ("authorization", auth),
            ];
            if let Some(token) = &self.session_token {
                headers.push(("x-amz-security-token", token.clone()));
            }
            let result = send(&url, &headers);
            let retry = match &result {
                Ok(resp) => {
                    resp.status().is_server_error()
                        || resp.status() == StatusCode::TOO_MANY_REQUESTS
                }
                Err(_) => true,
            };
            if !retry || attempt >= S3_ATTEMPTS {
                return result.with_context(|| format!("{method} {url}"));
            }
            let why = match &result {
                Ok(resp) => resp.status().to_string(),
                Err(e) => e.to_string(),
            };
            eprintln!("{method} {url}: {why}; retrying in {backoff:?}");
            std::thread::sleep(backoff);
            backoff *= 2;
            attempt += 1;
        }
    }

    fn get_response(&self, key: &str) -> Result<Option<Response>> {
        let resp = self.send("GET", key, "", |url, headers| {
            let mut req = self.agent.get(url);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            req.call()
        })?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp)),
            StatusCode::NOT_FOUND => Ok(None),
            s => bail!(
                "GET {key} from {}: {s}: {}",
                self.describe(),
                error_body(resp)
            ),
        }
    }

    fn put_with(
        &self,
        key: &str,
        query: &str,
        mut body: impl FnMut(
            ureq::RequestBuilder<ureq::typestate::WithBody>,
        ) -> Result<Response, ureq::Error>,
    ) -> Result<Response> {
        let resp = self.send("PUT", key, query, |url, headers| {
            let mut req = self.agent.put(url);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            body(req)
        })?;
        if !resp.status().is_success() {
            bail!(
                "PUT {key} to {}: {}: {}",
                self.describe(),
                resp.status(),
                error_body(resp)
            );
        }
        Ok(resp)
    }

    /// Sends a POST with `body`, and returns the response text.
    fn post(
        &self,
        key: &str,
        query: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<String> {
        let resp = self.send("POST", key, query, |url, headers| {
            let mut req = self.agent.post(url);
            for (k, v) in headers {
                req = req.header(*k, v);
            }
            if let Some(t) = content_type {
                req = req.header("content-type", t);
            }
            req.send(body)
        })?;
        let status = resp.status();
        let mut text = String::new();
        resp.into_body()
            .into_reader()
            .read_to_string(&mut text)
            .with_context(|| format!("POST {key} to {}", self.describe()))?;
        // CompleteMultipartUpload can fail after answering 200 OK.
        if !status.is_success() || text.contains("<Error>") {
            bail!("POST {key} to {}: {status}: {text}", self.describe());
        }
        Ok(text)
    }

    /// Uploads `src` in parts of `self.part_size`.
    fn upload_multipart(
        &self,
        key: &str,
        src: &Path,
        size: u64,
        content_type: Option<&str>,
    ) -> Result<()> {
        let created = self.post(key, "uploads=", content_type, &[])?;
        let upload_id = xml_element(&created, "UploadId")
            .with_context(|| format!("no UploadId in answer to creating upload of {key}"))?;
        let id_param = format!("uploadId={}", uri_encode(&upload_id, false));
        let result = (|| -> Result<()> {
            let mut f =
                std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
            let parts = size.div_ceil(self.part_size).max(1);
            ensure!(parts <= 10_000, "{} is too big for S3", src.display());
            let mut etags = Vec::new();
            let mut buf = Vec::new();
            for number in 1..=parts {
                buf.clear();
                (&mut f).take(self.part_size).read_to_end(&mut buf)?;
                let query = format!("partNumber={number}&{id_param}");
                let resp = self.put_with(key, &query, |r| r.send(&buf[..]))?;
                let etag = resp
                    .headers()
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .with_context(|| format!("no ETag for part {number} of {key}"))?
                    .to_string();
                etags.push(etag);
            }
            let mut body = String::from("<CompleteMultipartUpload>");
            for (i, etag) in etags.iter().enumerate() {
                body.push_str(&format!(
                    "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                    i + 1,
                    xml_escape(etag)
                ));
            }
            body.push_str("</CompleteMultipartUpload>");
            self.post(key, &id_param, Some("application/xml"), body.as_bytes())?;
            Ok(())
        })();
        if result.is_err() {
            // Otherwise the bucket keeps the parts, and bills for them.
            let _ = self.send("DELETE", key, &id_param, |url, headers| {
                let mut req = self.agent.delete(url);
                for (k, v) in headers {
                    req = req.header(*k, v);
                }
                req.call()
            });
        }
        result
    }
}

impl S3 {
    /// Like [`Store::upload`], but also sets the object's media type,
    /// which the store returns as `Content-Type` when serving it.
    pub fn upload_as(&self, key: &str, src: &Path, content_type: Option<&str>) -> Result<()> {
        let size = std::fs::metadata(src)
            .with_context(|| format!("reading {}", src.display()))?
            .len();
        if size > self.part_size {
            return self.upload_multipart(key, src, size, content_type);
        }
        self.put_with(key, "", |mut r| {
            if let Some(t) = content_type {
                r = r.header("content-type", t);
            }
            let f = std::fs::File::open(src).map_err(ureq::Error::Io)?;
            r.send(&f)
        })?;
        Ok(())
    }

    /// Where `key` goes, for log messages.
    pub fn describe_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            format!("s3://{}/{key}", self.bucket)
        } else {
            format!("s3://{}/{}/{key}", self.bucket, self.prefix)
        }
    }
}

type Response = ureq::http::Response<ureq::Body>;

fn error_body(resp: Response) -> String {
    let mut s = String::new();
    let _ = resp
        .into_body()
        .into_reader()
        .take(2000)
        .read_to_string(&mut s);
    s
}

impl Store for S3 {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(resp) = self.get_response(key)? else {
            return Ok(None);
        };
        let mut data = Vec::new();
        resp.into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader()
            .read_to_end(&mut data)?;
        Ok(Some(data))
    }

    fn download(&self, key: &str, dest: &Path) -> Result<bool> {
        let Some(resp) = self.get_response(key)? else {
            return Ok(false);
        };
        let mut f = std::fs::File::create(dest)?;
        std::io::copy(
            &mut resp.into_body().into_with_config().limit(u64::MAX).reader(),
            &mut f,
        )
        .with_context(|| format!("downloading {key} from {}", self.describe()))?;
        Ok(true)
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.put_with(key, "", |r| r.send(data))?;
        Ok(())
    }

    fn upload(&self, key: &str, src: &Path) -> Result<()> {
        self.upload_as(key, src, None)
    }

    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.prefix)
    }
}

const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// The text of the first `<name>` element in `xml`, unescaped.
fn xml_element(xml: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{name}>"))? + name.len() + 2;
    let len = xml[start..].find(&format!("</{name}>"))?;
    Some(
        xml[start..start + len]
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn host_of(endpoint: &str) -> Result<String> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .with_context(|| format!("endpoint {endpoint} is not an http(s) URL"))?;
    Ok(rest.split('/').next().unwrap_or(rest).to_string())
}

/// `20130524T000000Z` for a Unix timestamp.
fn amz_date(timestamp: i64) -> String {
    let s = crate::date::rfc3339(timestamp);
    s.replace(['-', ':'], "")
}

/// URI-encodes as SigV4 requires: everything but `A-Za-z0-9-._~`, and
/// also `/` unless `keep_slash`.
fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Authorization` header of AWS Signature Version 4. `query` is
/// the canonical query string. `headers` must be lowercase and sorted,
/// and include `host` and `x-amz-date`.
#[allow(clippy::too_many_arguments)] // the inputs of the SigV4 spec
fn authorization(
    access_key: &str,
    secret_key: &str,
    region: &str,
    method: &str,
    path: &str,
    query: &str,
    headers: &[(&str, &str)],
    payload_hash: &str,
) -> String {
    let amz_date = headers
        .iter()
        .find(|(k, _)| *k == "x-amz-date")
        .map(|(_, v)| *v)
        .expect("x-amz-date header");
    let date = &amz_date[..8];
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request =
        format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(digest::digest(&digest::SHA256, canonical_request.as_bytes()).as_ref())
    );
    let sign = |key: &[u8], msg: &str| -> Vec<u8> {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), msg.as_bytes())
            .as_ref()
            .to_vec()
    };
    let k = sign(format!("AWS4{secret_key}").as_bytes(), date);
    let k = sign(&k, region);
    let k = sign(&k, "s3");
    let k = sign(&k, "aws4_request");
    let signature = hex(&sign(&k, &string_to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigv4_matches_aws_example() {
        // "Example: GET Object" from the AWS docs on signing S3 requests
        // with the Authorization header.
        let auth = authorization(
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "GET",
            "/test.txt",
            "",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                (
                    "x-amz-content-sha256",
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                ),
                ("x-amz-date", "20130524T000000Z"),
            ],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn sigv4_signs_query() {
        // "Example: GET Bucket Lifecycle" and "Example: Get Bucket (List
        // Objects)" from the same AWS docs.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sign = |query| {
            authorization(
                "AKIAIOSFODNN7EXAMPLE",
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "us-east-1",
                "GET",
                "/",
                query,
                &[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", empty),
                    ("x-amz-date", "20130524T000000Z"),
                ],
                empty,
            )
        };
        assert!(sign("lifecycle=").ends_with(
            "Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
        ));
        assert!(sign("max-keys=2&prefix=J").ends_with(
            "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
        ));
    }

    #[test]
    fn encodes_keys() {
        assert_eq!(
            uri_encode("gtfs/ch/a b%2Fé", true),
            "gtfs/ch/a%20b%252F%C3%A9"
        );
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        assert_eq!(amz_date(1_369_353_600), "20130524T000000Z");
        assert_eq!(
            host_of("https://abc.r2.cloudflarestorage.com/").unwrap(),
            "abc.r2.cloudflarestorage.com"
        );
    }

    /// One request that the fake S3 server got.
    #[derive(Debug)]
    struct Req {
        method: String,
        target: String,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    /// A fake S3 server for multipart uploads; fails part `fail_part`.
    fn fake_s3(fail_part: Option<u32>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Req>>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let l = log.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut parts = first.split(' ');
                let method = parts.next().unwrap().to_string();
                let target = parts.next().unwrap().to_string();
                let (mut len, mut content_type) = (0, None);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let (k, v) = line.split_once(':').unwrap();
                    match k.to_ascii_lowercase().as_str() {
                        "content-length" => len = v.trim().parse().unwrap(),
                        "content-type" => content_type = Some(v.trim().to_string()),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let part = target
                    .split_once("partNumber=")
                    .map(|(_, n)| n.split('&').next().unwrap().parse::<u32>().unwrap());
                let (status, extra, answer) = match method.as_str() {
                    "POST" if target.ends_with("?uploads=") => (
                        "200 OK",
                        String::new(),
                        "<InitiateMultipartUploadResult><UploadId>a/b&amp;c</UploadId>\
                         </InitiateMultipartUploadResult>"
                            .to_string(),
                    ),
                    "PUT" if part.is_some() && part == fail_part => {
                        ("403 Forbidden", String::new(), String::new())
                    }
                    "PUT" => (
                        "200 OK",
                        format!("ETag: \"etag{}\"\r\n", part.unwrap_or(0)),
                        String::new(),
                    ),
                    _ => ("200 OK", String::new(), "<Done/>".to_string()),
                };
                l.lock().unwrap().push(Req {
                    method,
                    target,
                    content_type,
                    body,
                });
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                        answer.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (base, log)
    }

    fn fake_bucket(endpoint: &str) -> S3 {
        S3 {
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into(),
            endpoint: endpoint.to_string(),
            bucket: "bucket".into(),
            prefix: "pre".into(),
            region: "auto".into(),
            access_key: "AKID".into(),
            secret_key: "secret".into(),
            session_token: None,
            part_size: 4,
        }
    }

    #[test]
    fn uploads_in_parts() {
        let (endpoint, log) = fake_s3(None);
        let s3 = fake_bucket(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"0123456789").unwrap();
        s3.upload_as("a.tiff", &src, Some("image/tiff")).unwrap();
        let log = log.lock().unwrap();
        let got: Vec<(&str, &str)> = log
            .iter()
            .map(|r| (r.method.as_str(), r.target.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("POST", "/bucket/pre/a.tiff?uploads="),
                ("PUT", "/bucket/pre/a.tiff?partNumber=1&uploadId=a%2Fb%26c"),
                ("PUT", "/bucket/pre/a.tiff?partNumber=2&uploadId=a%2Fb%26c"),
                ("PUT", "/bucket/pre/a.tiff?partNumber=3&uploadId=a%2Fb%26c"),
                ("POST", "/bucket/pre/a.tiff?uploadId=a%2Fb%26c"),
            ]
        );
        assert_eq!(log[0].content_type.as_deref(), Some("image/tiff"));
        let parts: Vec<&[u8]> = log[1..4].iter().map(|r| &r.body[..]).collect();
        assert_eq!(parts, [&b"0123"[..], b"4567", b"89"]);
        assert_eq!(
            String::from_utf8_lossy(&log[4].body),
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>&quot;etag1&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&quot;etag2&quot;</ETag></Part>\
             <Part><PartNumber>3</PartNumber><ETag>&quot;etag3&quot;</ETag></Part>\
             </CompleteMultipartUpload>"
        );
    }

    #[test]
    fn aborts_failed_multipart_upload() {
        let (endpoint, log) = fake_s3(Some(2));
        let s3 = fake_bucket(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"0123456789").unwrap();
        assert!(s3.upload_as("a.tiff", &src, None).is_err());
        let log = log.lock().unwrap();
        let last = log.last().unwrap();
        assert_eq!(last.method, "DELETE");
        assert_eq!(last.target, "/bucket/pre/a.tiff?uploadId=a%2Fb%26c");
        assert!(!log.iter().any(|r| r.target.contains("partNumber=3")));
    }

    #[test]
    fn small_files_go_in_one_request() {
        let (endpoint, log) = fake_s3(None);
        let s3 = fake_bucket(&endpoint);
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"0123").unwrap();
        s3.upload_as("a.json", &src, None).unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(
            (log[0].method.as_str(), log[0].target.as_str()),
            ("PUT", "/bucket/pre/a.json")
        );
        assert_eq!(log[0].body, b"0123");
    }

    #[test]
    fn local_dir_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(store.get("a/b.json").unwrap(), None);
        store.put("a/b.json", b"{}").unwrap();
        assert_eq!(store.get("a/b.json").unwrap().as_deref(), Some(&b"{}"[..]));
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"zip").unwrap();
        store.upload("a/c.zip", &src).unwrap();
        let dest = dir.path().join("dest.bin");
        assert!(store.download("a/c.zip", &dest).unwrap());
        assert_eq!(std::fs::read(&dest).unwrap(), b"zip");
        assert!(!store.download("a/missing.zip", &dest).unwrap());
        for bad in ["", "/etc/passwd", "a/../b", "a//b", "./a"] {
            assert!(store.get(bad).is_err(), "{bad:?}");
        }
    }
}
