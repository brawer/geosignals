// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Streaming large files over HTTPS, resuming where a dropped
//! connection left off.
//!
//! TLS is rustls with the ring provider and Mozilla's root certificates
//! from webpki-roots, compiled into the binary, so the container image
//! needs no CA bundle.

use anyhow::{Context, Result, bail};
use std::io::{self, Read};
use std::time::Duration;
use ureq::ResponseExt;
use ureq::http::{StatusCode, header};

/// The most recent weekly planet dump. OSM redirects this to the dated
/// file in its cloud bucket, which is what we then read.
pub const PLANET_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf";

/// Consecutive failed attempts before giving up.
const MAX_ATTEMPTS: u32 = 8;

/// Each request may receive body data for this long. A healthy
/// connection then continues with a range request; a stalled one is
/// abandoned. ureq has no idle timeout, so this bounds stalls.
const REQUEST_DURATION: Duration = Duration::from_secs(10 * 60);

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!(
            "osmsignals/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/brawer/osmsignals)"
        ))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(REQUEST_DURATION))
        .build()
        .into()
}

/// Reads the body of a URL, reconnecting with a `Range` request when
/// the connection breaks. Later requests go to the URL the first one
/// was redirected to, and carry `If-Match` with its ETag, so a file
/// replaced mid-download makes the read fail instead of mixing data.
pub struct HttpReader {
    agent: ureq::Agent,
    /// Final URL after redirects.
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    len: Option<u64>,
    pos: u64,
    body: Option<ureq::BodyReader<'static>>,
    /// Whether the current request has received any data.
    got_data: bool,
    /// Pause before the first retry; doubled on each further failure.
    backoff: Duration,
}

impl HttpReader {
    pub fn open(agent: ureq::Agent, url: &str) -> Result<HttpReader> {
        Ok(Self::open_if_changed(agent, url, None, None)?.expect("unconditional request"))
    }

    /// Like [`HttpReader::open`], but sends `If-None-Match` and
    /// `If-Modified-Since` with what an earlier response said, and
    /// returns `None` if the server answers that nothing has changed.
    pub fn open_if_changed(
        agent: ureq::Agent,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<Option<HttpReader>> {
        let mut req = agent.get(url);
        if let Some(etag) = etag {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        if let Some(lm) = last_modified {
            req = req.header(header::IF_MODIFIED_SINCE, lm);
        }
        let resp = req.call().with_context(|| format!("fetching {url}"))?;
        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let final_url = resp.get_uri().to_string();
        let header = |name| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header(header::ETAG);
        let last_modified = header(header::LAST_MODIFIED);
        let len = header(header::CONTENT_LENGTH).and_then(|v| v.parse().ok());
        let accepts_ranges = header(header::ACCEPT_RANGES).as_deref() == Some("bytes");
        if !accepts_ranges {
            eprintln!("warning: {final_url} does not accept range requests");
        }
        Ok(Some(HttpReader {
            agent,
            url: final_url,
            etag,
            last_modified,
            len,
            pos: 0,
            body: Some(resp.into_body().into_reader()),
            got_data: false,
            backoff: Duration::from_secs(1),
        }))
    }

    /// The URL being read, after redirects.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The `ETag` header of the first response, if any.
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// The `Last-Modified` header of the first response, if any.
    pub fn last_modified(&self) -> Option<&str> {
        self.last_modified.as_deref()
    }

    fn reconnect(&mut self) -> Result<()> {
        let mut req = self
            .agent
            .get(&self.url)
            .header(header::RANGE, format!("bytes={}-", self.pos));
        if let Some(etag) = &self.etag {
            req = req.header(header::IF_MATCH, etag);
        }
        let resp = req.call()?;
        if resp.status() != StatusCode::PARTIAL_CONTENT {
            bail!("expected 206 Partial Content, got {}", resp.status());
        }
        self.body = Some(resp.into_body().into_reader());
        self.got_data = false;
        Ok(())
    }
}

impl Read for HttpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut attempt = 1;
        loop {
            let err = match self.body.as_mut().map(|b| b.read(buf)) {
                Some(Ok(0)) if self.len.is_some_and(|len| self.pos < len) => {
                    io::Error::from(io::ErrorKind::UnexpectedEof)
                }
                Some(Ok(n)) => {
                    self.pos += n as u64;
                    if n > 0 {
                        self.got_data = true;
                        self.backoff = Duration::from_secs(1);
                    }
                    return Ok(n);
                }
                Some(Err(e)) => e,
                None => io::Error::other("not connected"),
            };
            self.body = None;
            // A request that ran out of time after receiving data ends
            // normally; see REQUEST_DURATION. One that got nothing stalled.
            let expired = err.kind() == io::ErrorKind::TimedOut && self.got_data;
            if !expired {
                if attempt >= MAX_ATTEMPTS {
                    return Err(io::Error::other(format!(
                        "reading {} at byte {}: {err}",
                        self.url, self.pos
                    )));
                }
                eprintln!(
                    "reading {} at byte {}: {err}; retrying in {:?}",
                    self.url, self.pos, self.backoff
                );
                std::thread::sleep(self.backoff);
                self.backoff = (self.backoff * 2).min(Duration::from_secs(60));
                attempt += 1;
            }
            if let Err(e) = self.reconnect() {
                let replaced = matches!(
                    e.downcast_ref::<ureq::Error>(),
                    Some(ureq::Error::StatusCode(412))
                );
                if replaced || attempt >= MAX_ATTEMPTS {
                    let why = if replaced {
                        "file was replaced"
                    } else {
                        "giving up"
                    };
                    return Err(io::Error::other(format!(
                        "reconnecting to {} at byte {}: {why}: {e:#}",
                        self.url, self.pos
                    )));
                }
                // The next loop iteration reports "not connected" and
                // waits before trying again.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Serves `data` at http://127.0.0.1:port/file, with a redirect from
    /// /latest. The first `drops` responses close the connection after
    /// sending `cut` bytes of their body. Returns the base URL and the
    /// Range headers of all requests.
    fn serve(data: Vec<u8>, cut: usize, drops: usize) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let seen = ranges.clone();
        std::thread::spawn(move || {
            let mut drops = drops;
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut path, mut start, mut if_match) = (String::new(), 0, None);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(p) = line.strip_prefix("GET ") {
                        path = p.split(' ').next().unwrap().to_string();
                    } else if let Some(r) = lower.strip_prefix("range: bytes=") {
                        start = r.trim().trim_end_matches('-').parse().unwrap();
                        seen.lock().unwrap().push(format!("{start}-"));
                    } else if let Some(m) = lower.strip_prefix("if-match: ") {
                        if_match = Some(m.trim().to_string());
                    }
                }
                if path == "/latest" {
                    let resp = "HTTP/1.1 302 Found\r\nLocation: /file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    stream.write_all(resp.as_bytes()).unwrap();
                    continue;
                }
                if if_match.is_some_and(|m| m != "\"v1\"") {
                    let resp = "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    stream.write_all(resp.as_bytes()).unwrap();
                    continue;
                }
                let status = if start > 0 {
                    "206 Partial Content"
                } else {
                    "200 OK"
                };
                let body = &data[start..];
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\
                     ETag: \"v1\"\r\nLast-Modified: Sat, 04 May 2024 08:42:14 GMT\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                if drops > 0 {
                    drops -= 1;
                    let _ = stream.write_all(&body[..cut.min(body.len())]);
                } else {
                    let _ = stream.write_all(body);
                }
            }
        });
        (base, ranges)
    }

    fn test_agent() -> ureq::Agent {
        ureq::Agent::config_builder().proxy(None).build().into()
    }

    #[test]
    fn follows_redirect_and_resumes() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let (base, ranges) = serve(data.clone(), 30_000, 2);
        let mut r = HttpReader::open(test_agent(), &format!("{base}/latest")).unwrap();
        assert_eq!(r.url(), format!("{base}/file"));
        assert_eq!(r.etag(), Some("\"v1\""));
        assert_eq!(r.last_modified(), Some("Sat, 04 May 2024 08:42:14 GMT"));
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), data.len());
        assert!(got == data);
        assert_eq!(*ranges.lock().unwrap(), ["30000-", "60000-"]);
    }

    #[test]
    fn gives_up_eventually() {
        let data = vec![7u8; 1000];
        let (base, _) = serve(data, 0, usize::MAX);
        let mut r = HttpReader::open(test_agent(), &format!("{base}/file")).unwrap();
        r.backoff = Duration::from_millis(1);
        let mut got = Vec::new();
        let err = r.read_to_end(&mut got).unwrap_err();
        assert!(err.to_string().contains("at byte 0"), "{err}");
    }

    #[test]
    fn fails_if_file_was_replaced() {
        let data = vec![7u8; 1000];
        let (base, _) = serve(data, 100, 1);
        let mut r = HttpReader::open(test_agent(), &format!("{base}/file")).unwrap();
        r.etag = Some("\"v0\"".into()); // as if the server now had a new file
        r.backoff = Duration::from_millis(1);
        let mut got = Vec::new();
        let err = r.read_to_end(&mut got).unwrap_err();
        assert!(err.to_string().contains("replaced"), "{err}");
        assert_eq!(got.len(), 100);
    }
}
