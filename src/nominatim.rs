// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! The `nominatim-secondary-importance` pipeline: republishes the
//! secondary importance raster of [Nominatim] as a Cloud-Optimized
//! GeoTIFF.
//!
//! Nominatim publishes the raster as a gzipped PostgreSQL dump of a
//! PostGIS raster table, with one row per 128×128 tile in hex-encoded
//! [WKB]. All tiles belong to one global grid in EPSG:4326 that is
//! 16384 cells wide and high, from −180° to 180° and 90° to −90°, and
//! hold unsigned 16-bit values with 0 for "no data". We keep that grid
//! and the values exactly as they are, so a pixel of our GeoTIFF reads
//! the same as the raster in Nominatim's database. Only tiles with data
//! are in the dump; everything else stays 0.
//!
//! The whole raster takes at most 512 MB, but only about a third of its
//! 256×256 output tiles hold any data, so we keep just those in memory.
//! Overview pixels take the largest of their four children, as
//! Nominatim keeps the largest importance where areas overlap.
//!
//! [Nominatim]: https://nominatim.org/
//! [WKB]: https://github.com/postgis/postgis/blob/master/raster/doc/RFC2-WellKnownBinaryFormat

use crate::bom::{self, Hashes};
use crate::cog::{Crs, Grid, Metadata, Spool};
use crate::histogram::Histogram;
use crate::tile::{TILE_BITS, TILE_SIZE};
use crate::workdir::Workdir;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::time::Instant;

/// Where Nominatim publishes the raster.
pub const URL: &str = "https://nominatim.org/data/wikimedia-secondary-importance.sql.gz";

/// Nominatim's grid: 16384 = 256 << 6 cells wide and high.
pub const GRID: Grid = Grid {
    crs: Crs::Wgs84,
    max_zoom: 6,
};

/// Pixel value v stands for an importance of v / SCALE. Nominatim itself
/// adds v / 655350000 to the importance of places without a Wikipedia
/// article, as a tie-breaker; see the README.
pub const SCALE: f64 = 65536.0;

const PACKAGE: crate::datapackage::Package = crate::datapackage::Package {
    name: "nominatim-secondary-importance",
    // UUIDv5(URL, "https://github.com/brawer/osmsignals#nominatim-secondary-importance").
    // Frozen: catalogs key on it.
    id: "e4f31309-3b17-57f1-beb2-29a90aaab267",
    title: "Nominatim secondary importance",
    description: "Nominatim's secondary importance raster, republished unchanged as a \
        Cloud-Optimized GeoTIFF with overviews, in EPSG:4326 on a grid of \
        16384 × 16384 cells. Each pixel holds the highest importance of the \
        OpenStreetMap areas below county level that cover it, such as \
        municipalities and suburbs, as importance × 65536; 0 means no such area.",
};

const DESCRIPTION: &str = "Nominatim secondary importance";

pub struct Config {
    /// Where the input came from, for the data BOM. Its hashes are
    /// filled in while reading.
    pub input: bom::Input,
    /// When the input was last modified, in seconds since the epoch.
    /// Names the output and dates the BOM.
    pub timestamp: i64,
    /// Identifies this run in its scheduler, for the data BOM.
    pub run_id: String,
    pub output_dir: PathBuf,
    /// Directory for temporary files; see [`crate::workdir`].
    pub workdir: PathBuf,
    pub threads: usize,
}

#[derive(Debug, Default)]
pub struct Summary {
    /// The GeoTIFF file that was written.
    pub output: PathBuf,
    /// Its data bill of materials; see [`crate::bom`].
    pub bom: PathBuf,
    /// The data package pointing to both; see [`crate::datapackage`].
    pub datapackage: PathBuf,
    /// Size and digests of the input.
    pub input_hashes: Option<Hashes>,
    /// PostGIS raster tiles in the input.
    pub source_tiles: u64,
    /// Full-resolution pixels with a value other than 0.
    pub pixels: u64,
    pub tiles_per_zoom: Vec<u64>,
    pub bytes_per_zoom: Vec<u64>,
}

pub fn run<R: Read>(mut input: R, cfg: &Config) -> Result<Summary> {
    let start = Instant::now();
    let workdir = Workdir::open(&cfg.workdir, PACKAGE.name)?;
    std::fs::create_dir_all(&cfg.output_dir)
        .with_context(|| format!("creating {}", cfg.output_dir.display()))?;

    // The file is small (8 MB in 2024), so we read it whole, which lets
    // us hash it for the BOM.
    let mut data = Vec::new();
    input
        .read_to_end(&mut data)
        .with_context(|| format!("reading {}", cfg.input.name))?;
    let hashes = bom::hash_bytes(&data);
    let gz = flate2::read::MultiGzDecoder::new(data.as_slice());
    let mut raster = Raster::default();
    let source_tiles = parse_dump(std::io::BufReader::new(gz), &mut raster)
        .with_context(|| format!("parsing {}", cfg.input.name))?;
    ensure!(source_tiles > 0, "{} has no raster tiles", cfg.input.name);
    eprintln!(
        "read {source_tiles} tiles from {} ({} bytes) in {:.0?}",
        cfg.input.name,
        data.len(),
        start.elapsed()
    );
    drop(data);

    let levels = raster.pyramid();
    let mut hist = Histogram::new(u16::MAX);
    let full = &levels[GRID.max_zoom as usize];
    for px in full.values() {
        hist.add(px);
    }
    let tiles_per_side = 1u64 << GRID.max_zoom;
    let empty = tiles_per_side * tiles_per_side - full.len() as u64;
    hist.add_value(0, empty * (TILE_SIZE * TILE_SIZE) as u64);
    let pixels = hist.counts()[1..].iter().sum();

    let mut spool = Spool::new(workdir.tmp(), GRID)?;
    let jobs: Vec<(u8, u32, u32, &[u16])> = levels
        .iter()
        .enumerate()
        .flat_map(|(z, tiles)| {
            tiles
                .iter()
                .map(move |(&(x, y), px)| (z as u8, x, y, px.as_slice()))
        })
        .collect();
    for (&(z, x, y, _), data) in jobs.iter().zip(encode_all(&jobs, cfg.threads)) {
        spool.add(z, x, y, &data?)?;
    }
    eprintln!("encoded {} tiles in {:.0?}", jobs.len(), start.elapsed());

    let input = bom::Input {
        hashes: Some(hashes.clone()),
        ..cfg.input.clone()
    };
    let output = cfg
        .output_dir
        .join(crate::date::dated_name(PACKAGE.name, cfg.timestamp)?);
    let mut summary = Summary {
        output: output.clone(),
        input_hashes: Some(hashes),
        source_tiles,
        pixels,
        tiles_per_zoom: spool.tiles_per_zoom.clone(),
        bytes_per_zoom: spool.bytes_per_zoom.clone(),
        ..Summary::default()
    };
    let (min_value, max_value) = hist.range();
    let meta = Metadata {
        description: DESCRIPTION.into(),
        software: format!("osmsignals {}", env!("CARGO_PKG_VERSION")),
        copyright: "© OpenStreetMap contributors, ODbL 1.0; \
                    importance by Nominatim from Wikipedia links"
            .into(),
        // Without the zeros, as GDAL does when a NoData value is set.
        min_value: (1..=max_value)
            .find(|&v| hist.counts()[v as usize] > 0)
            .unwrap_or(min_value),
        max_value,
        gdal_metadata: gdal_metadata(&input, cfg.timestamp, &hist),
        nodata: Some(0),
    };
    spool.write_cog(&output, &meta)?;
    let tiff_hashes = bom::hash_file(&output)?;
    summary.bom = bom::write(
        &bom::Run {
            input: &input,
            timestamp: cfg.timestamp,
            output: &output,
            description: DESCRIPTION,
            run_id: &cfg.run_id,
        },
        &tiff_hashes,
    )?;
    // Written last, so it never points to files that are not there yet.
    summary.datapackage = crate::datapackage::write(
        &PACKAGE,
        &crate::datapackage::Files {
            input: &input,
            timestamp: cfg.timestamp,
            tiff: &output,
            tiff_hashes: &tiff_hashes,
            bom: &summary.bom,
        },
    )?;
    eprintln!(
        "wrote {}, {} and {} in {:.0?}",
        output.display(),
        summary.bom.display(),
        summary.datapackage.display(),
        start.elapsed()
    );
    Ok(summary)
}

/// Compresses tiles on `threads` threads, returning results in order.
fn encode_all(jobs: &[(u8, u32, u32, &[u16])], threads: usize) -> Vec<Result<Vec<u8>>> {
    let threads = threads.max(1);
    let mut results: Vec<Option<Result<Vec<u8>>>> = (0..jobs.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || {
                    jobs.iter()
                        .enumerate()
                        .skip(t)
                        .step_by(threads)
                        .map(|(i, job)| (i, crate::encode::zstd(job.3)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for w in workers {
            for (i, r) in w.join().expect("encoder panicked") {
                results[i] = Some(r);
            }
        }
    });
    results.into_iter().map(Option::unwrap).collect()
}

/// The full-resolution image, as 256×256 tiles of pixels in row-major
/// order. Missing tiles are all zero.
#[derive(Default)]
pub struct Raster {
    pub tiles: BTreeMap<(u32, u32), Vec<u16>>,
}

impl Raster {
    /// Sets pixel (x, y) to `v` unless it already holds more.
    fn paint(&mut self, x: u32, y: u32, v: u16) {
        let tile = self
            .tiles
            .entry((x >> TILE_BITS, y >> TILE_BITS))
            .or_insert_with(|| vec![0; TILE_SIZE * TILE_SIZE]);
        let mask = TILE_SIZE as u32 - 1;
        let p = &mut tile[((y & mask) as usize) * TILE_SIZE + (x & mask) as usize];
        *p = (*p).max(v);
    }

    /// The tiles of each zoom level, from 0 to [`GRID`]'s `max_zoom`.
    /// Each overview pixel is the largest of its four children.
    fn pyramid(self) -> Vec<BTreeMap<(u32, u32), Vec<u16>>> {
        let mut levels = vec![self.tiles];
        while levels.len() <= GRID.max_zoom as usize {
            let mut parents = BTreeMap::new();
            for (&(x, y), child) in levels.last().unwrap() {
                let parent = parents
                    .entry((x / 2, y / 2))
                    .or_insert_with(|| vec![0u16; TILE_SIZE * TILE_SIZE]);
                let half = TILE_SIZE / 2;
                let (ox, oy) = ((x as usize % 2) * half, (y as usize % 2) * half);
                for row in 0..half {
                    for col in 0..half {
                        let i = 2 * row * TILE_SIZE + 2 * col;
                        let v = child[i]
                            .max(child[i + 1])
                            .max(child[i + TILE_SIZE])
                            .max(child[i + TILE_SIZE + 1]);
                        parent[(oy + row) * TILE_SIZE + ox + col] = v;
                    }
                }
            }
            levels.push(parents);
        }
        levels.reverse();
        levels
    }
}

/// Reads the rows of every `COPY … FROM stdin;` block in a PostgreSQL
/// dump, paints each raster tile into `raster`, and returns the number
/// of tiles.
pub fn parse_dump<R: BufRead>(input: R, raster: &mut Raster) -> Result<u64> {
    let mut in_copy = false;
    let mut tiles = 0;
    let mut buf = Vec::new();
    for (i, line) in input.lines().enumerate() {
        let line = line?;
        if !in_copy {
            in_copy = line.starts_with("COPY ") && line.ends_with("FROM stdin;");
            continue;
        }
        if line == "\\." {
            in_copy = false;
            continue;
        }
        let row = || format!("line {}", i + 1);
        buf.clear();
        unhex(line.as_bytes(), &mut buf).with_context(row)?;
        paint_wkb(&buf, raster).with_context(row)?;
        tiles += 1;
    }
    ensure!(!in_copy, "dump ends inside a COPY block");
    Ok(tiles)
}

fn unhex(hex: &[u8], out: &mut Vec<u8>) -> Result<()> {
    ensure!(hex.len().is_multiple_of(2), "odd number of hex digits");
    let digit = |c: u8| -> Result<u8> {
        Ok(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => bail!("not a hex digit: {:?}", c as char),
        })
    };
    for pair in hex.as_chunks::<2>().0 {
        out.push(digit(pair[0])? << 4 | digit(pair[1])?);
    }
    Ok(())
}

/// PostGIS pixel type of unsigned 16-bit integers.
const PT_16BUI: u8 = 6;

/// Paints one PostGIS raster in little-endian WKB into `raster`, after
/// checking that it lies on Nominatim's grid.
fn paint_wkb(b: &[u8], raster: &mut Raster) -> Result<()> {
    ensure!(b.len() >= 64, "raster too short");
    ensure!(b[0] == 1, "raster is not little-endian");
    let u16_at = |p: usize| u16::from_le_bytes([b[p], b[p + 1]]);
    let f64_at = |p: usize| f64::from_le_bytes(b[p..p + 8].try_into().unwrap());
    ensure!(u16_at(1) == 0, "raster WKB version {}", u16_at(1));
    ensure!(u16_at(3) == 1, "raster has {} bands, not 1", u16_at(3));
    let (scale_x, scale_y) = (f64_at(5), f64_at(13));
    let (left, top) = (f64_at(21), f64_at(29));
    let (skew_x, skew_y) = (f64_at(37), f64_at(45));
    let srid = i32::from_le_bytes(b[53..57].try_into().unwrap());
    let (width, height) = (u32::from(u16_at(57)), u32::from(u16_at(59)));
    let n = f64::from(GRID.size());
    ensure!(
        scale_x == 360.0 / n && scale_y == -180.0 / n && skew_x == 0.0 && skew_y == 0.0,
        "raster cells of {scale_x}° × {scale_y}° (skew {skew_x}, {skew_y}) \
         are not on the grid of 360/{n}° × 180/{n}°"
    );
    ensure!(srid == 4326, "raster in SRID {srid}, not 4326");
    let band = b[61];
    ensure!(
        band & 0x80 == 0,
        "raster band is stored outside the database"
    );
    ensure!(
        band & 0x0f == PT_16BUI,
        "raster has pixel type {}, not 16BUI",
        band & 0x0f
    );
    // Pixels that equal the NoData value have no data, like our zeros.
    let nodata = (band & 0x40 != 0).then(|| u16_at(62));
    let pixels = &b[64..];
    ensure!(
        pixels.len() == 2 * (width * height) as usize,
        "raster of {width}×{height} pixels has {} bytes of pixel data",
        pixels.len()
    );
    let col = (left + 180.0) / scale_x;
    let row = (90.0 - top) / -scale_y;
    ensure!(
        col.fract() == 0.0 && row.fract() == 0.0,
        "raster at {left}, {top} is not aligned with the grid"
    );
    ensure!(
        col >= 0.0 && row >= 0.0 && col + f64::from(width) <= n && row + f64::from(height) <= n,
        "raster at {left}, {top} extends beyond the grid"
    );
    let (col, row) = (col as u32, row as u32);
    for (i, px) in pixels.as_chunks::<2>().0.iter().enumerate() {
        let v = u16::from_le_bytes([px[0], px[1]]);
        if v == 0 || Some(v) == nodata {
            continue;
        }
        let i = i as u32;
        raster.paint(col + i % width, row + i / width, v);
    }
    Ok(())
}

/// Band description and scale for GDAL, so that tools like QGIS show
/// importance instead of raw pixel values, plus the value histogram.
fn gdal_metadata(input: &bom::Input, timestamp: i64, hist: &Histogram) -> String {
    let source = input.url.as_deref().unwrap_or(&input.name);
    format!(
        concat!(
            "<GDALMetadata>",
            r#"<Item name="SOURCE">{source}</Item>"#,
            r#"<Item name="SOURCE_LAST_MODIFIED">{modified}</Item>"#,
            r#"<Item name="ENCODING">v = 0: no area; otherwise importance = v / {scale}</Item>"#,
            r#"<Item name="DESCRIPTION" sample="0" role="description">{description}</Item>"#,
            r#"<Item name="SCALE" sample="0" role="scale">{inv_scale}</Item>"#,
            r#"<Item name="OFFSET" sample="0" role="offset">0</Item>"#,
            "{rat}",
            "</GDALMetadata>"
        ),
        source = xml_escape(source),
        modified = crate::date::rfc3339(timestamp),
        scale = SCALE,
        description = DESCRIPTION,
        inv_scale = 1.0 / SCALE,
        rat = hist.sparse_rat_xml(),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PostGIS raster tile in hex WKB, as in Nominatim's dump.
    pub(crate) fn wkb(col: u32, row: u32, w: u16, h: u16, px: &[u16]) -> String {
        let n = f64::from(GRID.size());
        let mut b = vec![1u8];
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
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
        b.push(0x40 | PT_16BUI);
        b.extend_from_slice(&0u16.to_le_bytes());
        for v in px {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    fn parse(dump: &str) -> Result<Raster> {
        let mut r = Raster::default();
        parse_dump(dump.as_bytes(), &mut r)?;
        Ok(r)
    }

    fn at(r: &Raster, x: u32, y: u32) -> u16 {
        r.tiles
            .get(&(x >> 8, y >> 8))
            .map_or(0, |t| t[((y & 255) * 256 + (x & 255)) as usize])
    }

    #[test]
    fn paints_tiles_and_keeps_maximum() {
        let a = wkb(255, 600, 2, 2, &[1, 2, 3, 4]);
        let b = wkb(256, 601, 1, 1, &[1]);
        let c = wkb(255, 601, 1, 1, &[2]);
        let dump = format!(
            "BEGIN;\nCOPY \"t\" (\"rast\") FROM stdin;\n{a}\n\\.\n\
             COPY \"t\" (\"rast\") FROM stdin;\n{b}\n{c}\n\\.\nEND;\n"
        );
        let r = parse(&dump).unwrap();
        assert_eq!(r.tiles.len(), 2, "crosses a tile edge");
        assert_eq!(at(&r, 255, 600), 1);
        assert_eq!(at(&r, 256, 600), 2);
        assert_eq!(at(&r, 255, 601), 3);
        assert_eq!(at(&r, 256, 601), 4, "max(4, 1)");
        assert_eq!(at(&r, 257, 601), 0);
    }

    #[test]
    fn rejects_bad_input() {
        let copy = |row: &str| format!("COPY t (rast) FROM stdin;\n{row}\n\\.\n");
        assert!(parse(&copy("0G")).is_err(), "not hex");
        assert!(parse(&copy("010")).is_err(), "odd length");
        assert!(
            parse(&copy(&wkb(16383, 0, 2, 1, &[1, 1]))).is_err(),
            "beyond the grid"
        );
        assert!(
            parse(&copy(&wkb(0, 0, 2, 1, &[1]))).is_err(),
            "too few pixels"
        );
        let mut big_endian = wkb(0, 0, 1, 1, &[1]);
        big_endian.replace_range(0..2, "00");
        assert!(parse(&copy(&big_endian)).is_err());
        assert!(
            parse("COPY t (rast) FROM stdin;\n").is_err(),
            "unterminated"
        );
        assert_eq!(parse("SELECT 1;\n").unwrap().tiles.len(), 0);
    }

    #[test]
    fn overviews_keep_maximum() {
        let mut r = Raster::default();
        r.paint(0, 0, 5);
        r.paint(1, 1, 7);
        r.paint(16383, 16383, 9);
        let levels = r.pyramid();
        assert_eq!(levels.len(), 7);
        assert_eq!(levels[6].len(), 2);
        assert_eq!(levels[5][&(0, 0)][0], 7);
        assert_eq!(levels[5][&(31, 31)][255 * 256 + 255], 9);
        assert_eq!(levels[0].len(), 1);
        let top = &levels[0][&(0, 0)];
        assert_eq!((top[0], top[65535]), (7, 9));
        assert_eq!(top.iter().filter(|&&v| v > 0).count(), 2);
    }
}
