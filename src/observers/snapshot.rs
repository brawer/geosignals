// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! GBIF's monthly occurrence snapshot on Amazon S3, read column by
//! column over HTTPS.
//!
//! GBIF publishes every occurrence it holds as a few thousand Parquet
//! files under `occurrence/YYYY-MM-DD/occurrence.parquet/` in a public
//! bucket, about 300 GB in all. We only need a handful of the 50
//! columns, so we never download whole files: for each file we fetch
//! its footer, then only the column chunks we read, with range
//! requests of at most 1 MiB as the reader walks through their pages,
//! so a column costs about a megabyte of memory. The bucket needs no credentials; we list it with the
//! S3 `ListObjectsV2` API, whose XML answer we pick apart by hand.

use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use parquet::column::reader::{ColumnReader, ColumnReaderImpl};
use parquet::data_type::DataType;
use parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use parquet::file::reader::{ChunkReader, Length, RowGroupReader};
use parquet::file::serialized_reader::SerializedRowGroupReader;
use std::sync::Arc;

/// The bucket in AWS region us-east-1. GBIF mirrors it to other regions.
pub const BUCKET_URL: &str = "https://gbif-open-data-us-east-1.s3.amazonaws.com";

/// One snapshot of all occurrences.
#[derive(Debug)]
pub struct Snapshot {
    /// As in the bucket, such as `2026-10-01`.
    pub date: String,
    /// The date at midnight UTC, in seconds since the epoch.
    pub timestamp: i64,
    /// URL of the directory holding the Parquet files.
    pub url: String,
    pub files: Vec<File>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub url: String,
    pub size: u64,
}

/// Lists the snapshot of `date` (YYYY-MM-DD) in the bucket at
/// `bucket_url`, or the latest one if `date` is `None`.
pub fn open(agent: &ureq::Agent, bucket_url: &str, date: Option<&str>) -> Result<Snapshot> {
    let bucket_url = bucket_url.trim_end_matches('/');
    let date = match date {
        Some(d) => d.to_string(),
        None => {
            let (_, prefixes) = list(agent, bucket_url, "occurrence/", true)?;
            prefixes
                .iter()
                .filter_map(|p| p.strip_prefix("occurrence/")?.strip_suffix('/'))
                .filter(|d| parse_date(d).is_some())
                .max()
                .with_context(|| format!("no snapshot in {bucket_url}/occurrence/"))?
                .to_string()
        }
    };
    let timestamp = parse_date(&date).with_context(|| format!("bad snapshot date {date:?}"))?;
    let prefix = format!("occurrence/{date}/occurrence.parquet/");
    let (objects, _) = list(agent, bucket_url, &prefix, false)?;
    let mut files: Vec<File> = objects
        .into_iter()
        .filter(|(key, size)| {
            let name = key.rsplit('/').next().unwrap_or_default();
            *size > 0 && !name.is_empty() && !name.starts_with(['_', '.'])
        })
        .map(|(key, size)| File {
            url: format!("{bucket_url}/{key}"),
            size,
        })
        .collect();
    files.sort_by(|a, b| a.url.cmp(&b.url));
    ensure!(!files.is_empty(), "no files in {bucket_url}/{prefix}");
    Ok(Snapshot {
        date,
        timestamp,
        url: format!("{bucket_url}/{prefix}"),
        files,
    })
}

/// `YYYY-MM-DD` at midnight UTC, in seconds since the epoch.
fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(crate::date::days_from_civil(y, m, d) * 86_400)
}

/// Objects as (key, size), and common prefixes.
type Listing = (Vec<(String, u64)>, Vec<String>);

/// Lists the objects whose key starts with `prefix`, as (key, size),
/// following continuation tokens. With `dirs`, lists only the common
/// prefixes one level down, such as `occurrence/2026-10-01/`.
fn list(agent: &ureq::Agent, bucket_url: &str, prefix: &str, dirs: bool) -> Result<Listing> {
    let mut objects = Vec::new();
    let mut prefixes = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut url = format!(
            "{bucket_url}/?list-type=2&prefix={}",
            percent_encode(prefix)
        );
        if dirs {
            url.push_str("&delimiter=%2F");
        }
        if let Some(t) = &token {
            url.push_str(&format!("&continuation-token={}", percent_encode(t)));
        }
        let body = crate::http::get(agent, &url, None)?;
        let xml = String::from_utf8(body).with_context(|| format!("listing {url}"))?;
        ensure!(
            xml.contains("<ListBucketResult"),
            "unexpected answer listing {url}"
        );
        for contents in elements(&xml, "Contents") {
            let key = elements(contents, "Key").next().map(unescape);
            let size = elements(contents, "Size")
                .next()
                .and_then(|s| s.parse().ok());
            match (key, size) {
                (Some(k), Some(s)) => objects.push((k, s)),
                _ => bail!("bad <Contents> listing {url}"),
            }
        }
        for p in elements(&xml, "CommonPrefixes") {
            prefixes.extend(elements(p, "Prefix").map(unescape));
        }
        let truncated = elements(&xml, "IsTruncated").next() == Some("true");
        token = elements(&xml, "NextContinuationToken").next().map(unescape);
        if !truncated || token.is_none() {
            return Ok((objects, prefixes));
        }
    }
}

/// The contents of every `<tag>…</tag>` in `xml`, not nested.
fn elements<'a>(xml: &'a str, tag: &str) -> impl Iterator<Item = &'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut rest = xml;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let len = rest[start..].find(&close)?;
        let found = &rest[start..start + len];
        rest = &rest[start + len + close.len()..];
        Some(found)
    })
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Column chunks of a remote file, fetched piece by piece as the
/// Parquet reader walks through their pages. Each chunk keeps only its
/// latest window in memory, so a column costs about one window and one
/// decoded page, however big its chunk is.
struct Chunks {
    agent: ureq::Agent,
    url: String,
    len: u64,
    chunks: Vec<Chunk>,
}

/// The byte range `start..end` of one column chunk, and the window
/// of it we fetched last, by position in the file.
struct Chunk {
    start: u64,
    end: u64,
    window: std::sync::Mutex<(u64, Bytes)>,
}

/// How much of a column chunk we fetch at once. Pages written by
/// parquet-mr are about 1 MiB, so this is about one request per page.
const WINDOW: u64 = 1 << 20;

/// Enough for any page header, which we must hand out in one piece.
const MAX_HEADER: u64 = 64 << 10;

impl Chunks {
    /// Returns bytes `start..start + len`, and the rest of the window
    /// past them.
    fn get(&self, start: u64, len: u64) -> parquet::errors::Result<(Bytes, usize)> {
        let Some(chunk) = self
            .chunks
            .iter()
            .find(|c| c.start <= start && start < c.end)
        else {
            return Err(parquet::errors::ParquetError::General(format!(
                "{}: byte {start} is in no column chunk we read",
                self.url
            )));
        };
        let len = len.min(chunk.end - start);
        let mut window = chunk.window.lock().expect("window poisoned");
        let (pos, data) = &*window;
        if !(*pos <= start && start + len <= pos + data.len() as u64) {
            let n = len.max(WINDOW).min(chunk.end - start);
            let data = crate::http::get(&self.agent, &self.url, Some((start, n)))
                .map_err(|e| parquet::errors::ParquetError::External(e.into()))?;
            *window = (start, Bytes::from(data));
        }
        let (pos, data) = &*window;
        let from = (start - pos) as usize;
        Ok((data.slice(from..), len as usize))
    }
}

impl Length for Chunks {
    fn len(&self) -> u64 {
        self.len
    }
}

impl ChunkReader for Chunks {
    type T = std::io::Cursor<Bytes>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        // The reader parses a page header from here, so it needs at
        // least a header's worth; the rest of the window comes along.
        let (data, _) = self.get(start, MAX_HEADER)?;
        Ok(std::io::Cursor::new(data))
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        let (data, len) = self.get(start, length as u64)?;
        if len < length {
            return Err(parquet::errors::ParquetError::EOF(format!(
                "{}: bytes {start}..{} run past their column chunk",
                self.url,
                start + length as u64
            )));
        }
        Ok(data.slice(..len))
    }
}

/// How much of the end of a file we fetch to find its metadata. GBIF's
/// footers are about 20 kB; a bigger one costs a second request.
const FOOTER_GUESS: u64 = 64 << 10;

/// A Parquet file on a web server, whose columns we read one by one.
pub struct RemoteFile<'a> {
    agent: &'a ureq::Agent,
    url: String,
    len: u64,
    metadata: ParquetMetaData,
}

impl<'a> RemoteFile<'a> {
    pub fn open(agent: &'a ureq::Agent, file: &File) -> Result<RemoteFile<'a>> {
        let len = file.size;
        ensure!(len >= 12, "{} is too short for Parquet", file.url);
        let tail_len = len.min(FOOTER_GUESS);
        let mut tail = crate::http::get(agent, &file.url, Some((len - tail_len, tail_len)))?;
        ensure!(
            tail.ends_with(b"PAR1"),
            "{} is not a Parquet file (or it is encrypted)",
            file.url
        );
        let meta_len = u64::from(u32::from_le_bytes(
            tail[tail.len() - 8..tail.len() - 4].try_into()?,
        ));
        ensure!(meta_len + 8 <= len - 4, "{}: bad footer length", file.url);
        if meta_len + 8 > tail_len {
            tail = crate::http::get(agent, &file.url, Some((len - meta_len - 8, meta_len + 8)))?;
        }
        let meta = &tail[tail.len() - 8 - meta_len as usize..tail.len() - 8];
        let metadata = ParquetMetaDataReader::decode_metadata(meta)
            .with_context(|| format!("decoding the footer of {}", file.url))?;
        Ok(RemoteFile {
            agent,
            url: file.url.clone(),
            len,
            metadata,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn row_groups(&self) -> usize {
        self.metadata.num_row_groups()
    }

    pub fn rows(&self, rg: usize) -> usize {
        self.metadata.row_group(rg).num_rows() as usize
    }

    /// Returns readers for `columns` in row group `rg`, in the same
    /// order. They fetch their chunks a window at a time as they read.
    /// Each column is a top-level field such as `license` or
    /// `recordedby`, which must have exactly one leaf column.
    pub fn columns(&self, rg: usize, columns: &[&str]) -> Result<Vec<Column>> {
        let meta = self.metadata.row_group(rg);
        let mut chunks = Vec::new();
        let mut found = Vec::new();
        for &column in columns {
            let mut leaves = meta.columns().iter().enumerate().filter(|(_, c)| {
                c.column_path().parts().first().map(String::as_str) == Some(column)
            });
            let Some((i, chunk)) = leaves.next() else {
                bail!("{}: no column {column}", self.url);
            };
            ensure!(
                leaves.next().is_none(),
                "{}: column {column} has several leaves",
                self.url
            );
            let (start, len) = chunk.byte_range();
            ensure!(
                start + len <= self.len,
                "{}: column {column} out of range",
                self.url
            );
            chunks.push(Chunk {
                start,
                end: start + len,
                window: std::sync::Mutex::new((0, Bytes::new())),
            });
            found.push((column, i, chunk.column_descr_ptr()));
        }
        let fetched = Arc::new(Chunks {
            agent: self.agent.clone(),
            url: self.url.clone(),
            len: self.len,
            chunks,
        });
        let props = Arc::new(parquet::file::properties::ReaderProperties::builder().build());
        let index = parquet::file::metadata::page_index::RowGroupPageIndex::new(rg, None);
        let rgr = SerializedRowGroupReader::new(fetched, meta, index, props)?;
        found
            .into_iter()
            .map(|(name, i, descr)| {
                Ok(Column {
                    reader: rgr.get_column_reader(i)?,
                    max_def: descr.max_def_level(),
                    max_rep: descr.max_rep_level(),
                    name: format!("{}: {name}", self.url),
                    def: Vec::new(),
                    rep: Vec::new(),
                })
            })
            .collect()
    }
}

/// Reads one column of a row group, a batch of records at a time, so
/// that memory does not grow with the size of the row group.
pub struct Column {
    reader: ColumnReader,
    max_def: i16,
    max_rep: i16,
    /// File and column, for error messages.
    name: String,
    def: Vec<i16>,
    rep: Vec<i16>,
}

/// Code for a string column's missing values.
pub const NULL: u8 = u8::MAX;

impl Column {
    /// Reads the next `n` records of a column of strings, such as
    /// `license`, and appends `code(value)` for each to `out`, or
    /// [`NULL`] where a record has none.
    pub fn codes(
        &mut self,
        n: usize,
        out: &mut Vec<u8>,
        mut code: impl FnMut(&[u8]) -> u8,
    ) -> Result<()> {
        ensure!(self.max_rep == 0, "{} is a list", self.name);
        let max_def = self.max_def;
        let ColumnReader::ByteArrayColumnReader(r) = &mut self.reader else {
            bail!("{} is not a string column", self.name);
        };
        batch(r, n, &mut self.def, &mut self.rep, |def, _, values| {
            spread(def, max_def, values, |v| {
                out.push(v.map_or(NULL, |v| code(v.data())));
            })
        })
        .with_context(|| format!("reading {}", self.name))
    }

    /// Reads the next `n` records of a column of numbers, such as
    /// `decimallatitude`, and appends them to `out`, with NaN where a
    /// record has none.
    pub fn doubles(&mut self, n: usize, out: &mut Vec<f64>) -> Result<()> {
        ensure!(self.max_rep == 0, "{} is a list", self.name);
        let max_def = self.max_def;
        let ColumnReader::DoubleColumnReader(r) = &mut self.reader else {
            bail!("{} is not a column of doubles", self.name);
        };
        batch(r, n, &mut self.def, &mut self.rep, |def, _, values| {
            spread(def, max_def, values, |v| {
                out.push(v.copied().unwrap_or(f64::NAN))
            })
        })
        .with_context(|| format!("reading {}", self.name))
    }

    /// Reads the next `n` records of a column of string lists, such as
    /// `recordedby`, and appends `id(list)` for each to `out`, where
    /// `list` holds the elements joined with `sep`, or is `None` for an
    /// empty list.
    pub fn lists<T>(
        &mut self,
        n: usize,
        sep: u8,
        out: &mut Vec<T>,
        mut id: impl FnMut(Option<&[u8]>) -> T,
    ) -> Result<()> {
        ensure!(self.max_rep == 1, "{} is not a flat list", self.name);
        let max_def = self.max_def;
        let ColumnReader::ByteArrayColumnReader(r) = &mut self.reader else {
            bail!("{} is not a list of strings", self.name);
        };
        let start = out.len();
        let mut cur: Vec<u8> = Vec::new();
        batch(r, n, &mut self.def, &mut self.rep, |def, rep, values| {
            ensure!(def.len() == rep.len(), "bad levels");
            let mut values = values.iter();
            // Batches end at record boundaries, so the first level of
            // each starts a record.
            let mut open = false;
            let mut empty = true;
            for (&d, &r) in def.iter().zip(rep) {
                if r == 0 {
                    if open {
                        out.push(id((!empty).then_some(cur.as_slice())));
                    }
                    open = true;
                    empty = true;
                    cur.clear();
                }
                if d == max_def {
                    let v = values.next().context("too few values")?;
                    if !empty {
                        cur.push(sep);
                    }
                    cur.extend_from_slice(v.data());
                    empty = false;
                }
            }
            if open {
                out.push(id((!empty).then_some(cur.as_slice())));
            }
            ensure!(values.next().is_none(), "levels and values disagree");
            Ok(())
        })
        .with_context(|| format!("reading {}", self.name))?;
        ensure!(
            out.len() - start == n,
            "{}: expected {n} records, got {}",
            self.name,
            out.len() - start
        );
        Ok(())
    }
}

/// Reads exactly `n` more records of a column, passing each batch's
/// definition and repetition levels and non-null values to `f`. Each
/// batch holds whole records.
fn batch<T: DataType>(
    r: &mut ColumnReaderImpl<T>,
    n: usize,
    def: &mut Vec<i16>,
    rep: &mut Vec<i16>,
    mut f: impl FnMut(&[i16], &[i16], &[T::T]) -> Result<()>,
) -> Result<()> {
    let mut values = Vec::new();
    let mut total = 0;
    while total < n {
        def.clear();
        rep.clear();
        values.clear();
        let (records, _, _) = r.read_records(n - total, Some(def), Some(rep), &mut values)?;
        if records == 0 {
            break;
        }
        total += records;
        f(def, rep, &values)?;
    }
    ensure!(total == n, "expected {n} records, got {total}");
    Ok(())
}

/// Calls `f` for each row of a batch of a flat column, with its value
/// or `None`.
fn spread<V>(def: &[i16], max_def: i16, values: &[V], mut f: impl FnMut(Option<&V>)) -> Result<()> {
    if max_def == 0 {
        values.iter().for_each(|v| f(Some(v)));
        return Ok(());
    }
    let mut values = values.iter();
    for &d in def {
        if d == max_def {
            f(Some(values.next().context("too few values")?));
        } else {
            f(None);
        }
    }
    ensure!(values.next().is_none(), "more values than levels");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_elements() {
        let xml = "<a><Key>x&amp;y</Key><Size>3</Size></a><a><Key>z</Key></a>";
        let keys: Vec<String> = elements(xml, "Key").map(unescape).collect();
        assert_eq!(keys, ["x&y", "z"]);
        assert_eq!(elements(xml, "Size").collect::<Vec<_>>(), ["3"]);
        assert_eq!(elements(xml, "Nope").count(), 0);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            percent_encode("occurrence/2026-10-01/"),
            "occurrence%2F2026-10-01%2F"
        );
        assert_eq!(percent_encode("a+b=c"), "a%2Bb%3Dc");
    }

    #[test]
    fn parses_snapshot_dates() {
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("2026-10-01"), Some(1_790_812_800));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("latest"), None);
    }
}
