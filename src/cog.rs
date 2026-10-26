// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Collects compressed tiles and writes them as a Cloud-Optimized GeoTIFF.
//!
//! The output is a BigTIFF with one 16-bit band, square and a power of
//! two tiles wide; see [`Grid`]. Overviews halve the resolution down to a
//! single 256×256 tile. Tiles without data are not stored at all (offset
//! and byte count 0), which GDAL reads as all zeros.
//!
//! The layout follows GDAL's COG driver, so that GDAL recognizes the file
//! as cloud-optimized: header, structural metadata, all image file
//! directories, the TileOffsets arrays, then tile data from the smallest
//! overview to the full-resolution image, each tile in row-major order
//! and wrapped in GDAL's leader and trailer. TileByteCounts come last,
//! since readers find each tile's size in its leader; see
//! <https://gdal.org/drivers/raster/cog.html>.
//!
//! Tiles arrive from the pyramid builder in Hilbert order, children
//! before parents, so the zoom levels interleave. We spool each zoom
//! level to its own temporary file and reorder when writing the output.
//! We use BigTIFF because the planet already needs 3 GB, close to the
//! 4 GB limit of classic TIFF.

use crate::tile::{MAX_TILE_ZOOM, TILE_BITS, TILE_SIZE};
use anyhow::{Context, Result, ensure};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

/// Descriptive metadata, stored in the full-resolution image.
pub struct Metadata {
    pub description: String,
    pub software: String,
    pub copyright: String,
    /// Smallest and largest pixel value in the full-resolution image.
    pub min_value: u16,
    pub max_value: u16,
    /// XML for the GDAL_METADATA tag, such as band scale and offset.
    pub gdal_metadata: String,
    /// Pixel value for "no data", written as the GDAL_NODATA tag.
    pub nodata: Option<u16>,
}

/// Coordinate reference system of the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crs {
    /// Web Mercator (EPSG:3857), covering the square world of web maps.
    WebMercator,
    /// Longitude and latitude (EPSG:4326), covering −180° to 180° and
    /// 90° to −90°, so pixels are twice as wide as tall in degrees.
    Wgs84,
}

/// The pixel grid. The full-resolution image is `256 << max_zoom`
/// pixels wide and high and covers the whole world in `crs`. Overview
/// `z` has `2^z × 2^z` tiles, like a web map at zoom `z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub crs: Crs,
    pub max_zoom: u8,
}

impl Grid {
    /// Web Mercator with one pixel per zoom-18 tile.
    pub const WEB_MERCATOR: Grid = Grid {
        crs: Crs::WebMercator,
        max_zoom: MAX_TILE_ZOOM,
    };

    /// Width and height of the full-resolution image in pixels.
    pub fn size(&self) -> u32 {
        (TILE_SIZE as u32) << self.max_zoom
    }
}

pub struct Spool {
    grid: Grid,
    files: Vec<BufWriter<File>>,
    pub tiles_per_zoom: Vec<u64>,
    pub bytes_per_zoom: Vec<u64>,
}

impl Spool {
    pub fn new(tmpdir: &Path, grid: Grid) -> Result<Self> {
        // Named files rather than unlinked ones, so their disk usage is
        // visible to whoever watches the working directory.
        let files = (0..=grid.max_zoom)
            .map(|z| {
                let path = tmpdir.join(format!("spool-{z:02}"));
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                Ok(BufWriter::new(f))
            })
            .collect::<Result<_>>()?;
        let n = grid.max_zoom as usize + 1;
        Ok(Spool {
            grid,
            files,
            tiles_per_zoom: vec![0; n],
            bytes_per_zoom: vec![0; n],
        })
    }

    pub fn add(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        ensure!(data.len() >= 4, "tile {z}/{x}/{y} too short");
        ensure!(
            z <= self.grid.max_zoom && x < 1 << z && y < 1 << z,
            "tile {z}/{x}/{y} outside the grid"
        );
        let f = &mut self.files[z as usize];
        f.write_all(&x.to_le_bytes())?;
        f.write_all(&y.to_le_bytes())?;
        f.write_all(&(data.len() as u32).to_le_bytes())?;
        f.write_all(data)?;
        self.tiles_per_zoom[z as usize] += 1;
        self.bytes_per_zoom[z as usize] += data.len() as u64;
        Ok(())
    }

    /// Writes the GeoTIFF to `path`.
    pub fn write_cog(self, path: &Path, meta: &Metadata) -> Result<()> {
        let grid = self.grid;
        let mut levels = Vec::new();
        for (z, f) in self.files.into_iter().enumerate() {
            let f = f.into_inner().map_err(|e| e.into_error())?;
            levels.push(Level::index(z as u8, f)?);
        }

        // Every value in the header region has a fixed width, so we can
        // lay it out once to learn its size, then again for real.
        let len = header(&levels, meta, &grid, 0)?.len() as u64;
        let mut pos = len;
        for level in levels.iter_mut() {
            for t in level.tiles.iter_mut().filter(|t| t.len > 0) {
                t.offset = pos + 4; // after the leader
                pos += u64::from(t.len) + 8;
            }
        }
        let data_end = pos;
        let head = header(&levels, meta, &grid, data_end.next_multiple_of(8))?;
        ensure!(head.len() as u64 == len, "header size changed");

        // Write under a temporary name and rename at the end, so that a
        // half-written file never shows up under the real name, even if
        // we get killed.
        let partial = partial_path(path);
        let out =
            File::create(&partial).with_context(|| format!("creating {}", partial.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, out);
        out.write_all(&head)?;
        let mut buf = Vec::new();
        for level in &levels {
            for t in level.tiles.iter().filter(|t| t.len > 0) {
                buf.resize(t.len as usize, 0);
                level.spool.read_exact_at(&mut buf, t.spool_offset)?;
                out.write_all(&t.len.to_le_bytes())?;
                out.write_all(&buf)?;
                out.write_all(&buf[buf.len() - 4..])?;
            }
        }
        // TileByteCounts, in the same order as the directories.
        out.write_all(&vec![0; (data_end.next_multiple_of(8) - data_end) as usize])?;
        for level in levels.iter().rev().filter(|l| l.tiles.len() > 2) {
            for t in &level.tiles {
                out.write_all(&t.len.to_le_bytes())?;
            }
        }
        let out = out.into_inner().map_err(|e| e.into_error())?;
        out.sync_all()?;
        drop(out);
        std::fs::rename(&partial, path)
            .with_context(|| format!("renaming {} to {}", partial.display(), path.display()))
    }
}

/// `planet.tif` → `planet.tif.partial`, in the same directory so that
/// renaming is atomic.
fn partial_path(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

#[derive(Clone, Copy, Default)]
struct Tile {
    spool_offset: u64,
    /// Compressed size; 0 if the tile is empty.
    len: u32,
    /// Position in the output file.
    offset: u64,
}

/// One zoom level, with its tiles in row-major order.
struct Level {
    zoom: u8,
    tiles: Vec<Tile>,
    spool: File,
}

impl Level {
    fn index(zoom: u8, mut spool: File) -> Result<Level> {
        let n = 1usize << zoom;
        let mut tiles = vec![Tile::default(); n * n];
        spool.seek(SeekFrom::Start(0))?;
        let mut r = BufReader::with_capacity(1 << 16, &spool);
        let mut pos = 0u64;
        let mut head = [0u8; 12];
        loop {
            match r.read_exact(&mut head) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let x = u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize;
            let y = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
            let len = u32::from_le_bytes(head[8..12].try_into().unwrap());
            let t = &mut tiles[y * n + x];
            ensure!(t.len == 0, "duplicate tile {zoom}/{x}/{y}");
            t.spool_offset = pos + 12;
            t.len = len;
            r.seek_relative(i64::from(len))?;
            pos += 12 + u64::from(len);
        }
        drop(r);
        Ok(Level { zoom, tiles, spool })
    }
}

// TIFF field types.
const ASCII: u16 = 2;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const DOUBLE: u16 = 12;
const LONG8: u16 = 16;

/// Half the Web Mercator world width, in meters.
const HALF_WORLD: f64 = std::f64::consts::PI * 6_378_137.0;

enum Value {
    /// Bytes stored in the directory itself or right after it.
    Data(Vec<u8>),
    /// Bytes stored elsewhere, at this file offset.
    At(u64),
}

struct Entry {
    tag: u16,
    typ: u16,
    count: u64,
    value: Value,
}

fn shorts(tag: u16, v: &[u16]) -> Entry {
    let data = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    Entry {
        tag,
        typ: SHORT,
        count: v.len() as u64,
        value: Value::Data(data),
    }
}

fn long(tag: u16, v: u32) -> Entry {
    Entry {
        tag,
        typ: LONG,
        count: 1,
        value: Value::Data(v.to_le_bytes().to_vec()),
    }
}

fn doubles(tag: u16, v: &[f64]) -> Entry {
    let data = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    Entry {
        tag,
        typ: DOUBLE,
        count: v.len() as u64,
        value: Value::Data(data),
    }
}

fn ascii(tag: u16, s: &str) -> Entry {
    let mut data = s.as_bytes().to_vec();
    data.push(0);
    Entry {
        tag,
        typ: ASCII,
        count: data.len() as u64,
        value: Value::Data(data),
    }
}

/// Lays out everything before the tile data: file header, GDAL's
/// structural metadata, one directory per zoom level (full resolution
/// first), and the TileOffsets arrays. `byte_counts_pos` is where the
/// TileByteCounts arrays will start, after the tile data.
fn header(levels: &[Level], meta: &Metadata, grid: &Grid, byte_counts_pos: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&43u16.to_le_bytes()); // BigTIFF
    out.extend_from_slice(&8u16.to_le_bytes()); // offset size
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // first IFD, patched below

    // https://gdal.org/drivers/raster/cog.html#header-ghost-area
    // The space after "NO" is required.
    let smd = "LAYOUT=IFDS_BEFORE_DATA\n\
               BLOCK_ORDER=ROW_MAJOR\n\
               BLOCK_LEADER=SIZE_AS_UINT4\n\
               BLOCK_TRAILER=LAST_4_BYTES_REPEATED\n\
               KNOWN_INCOMPATIBLE_EDITION=NO \n";
    write!(
        out,
        "GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n{smd}",
        smd.len()
    )?;
    pad(&mut out);
    let first_ifd = out.len() as u64;
    out[8..16].copy_from_slice(&first_ifd.to_le_bytes());

    // Arrays of TileOffsets and TileByteCounts too big for a directory
    // entry go after all directories, in directory order.
    let full = levels.len() - 1;
    let ifds_len: u64 = levels
        .iter()
        .rev()
        .map(|l| ifd_len(&entries(l, l.zoom as usize == full, meta, grid, 0, 0)))
        .sum();
    let mut offsets_pos = out.len() as u64 + ifds_len;
    let mut counts_pos = byte_counts_pos;

    for (i, level) in levels.iter().enumerate().rev() {
        let n = level.tiles.len();
        let entries = entries(level, i == full, meta, grid, offsets_pos, counts_pos);
        if n * 8 > 8 {
            offsets_pos += 8 * n as u64;
        }
        if n * 4 > 8 {
            counts_pos += 4 * n as u64;
        }
        let next = if i == 0 {
            0
        } else {
            out.len() as u64 + ifd_len(&entries)
        };
        write_ifd(&mut out, &entries, next);
    }
    for level in levels.iter().rev().filter(|l| l.tiles.len() > 1) {
        for t in &level.tiles {
            out.extend_from_slice(&t.offset.to_le_bytes());
        }
    }
    ensure!(out.len() as u64 == offsets_pos, "TileOffsets misplaced");
    Ok(out)
}

fn entries(
    level: &Level,
    full: bool,
    meta: &Metadata,
    grid: &Grid,
    offsets_pos: u64,
    counts_pos: u64,
) -> Vec<Entry> {
    let width = (TILE_SIZE as u32) << level.zoom;
    let n = level.tiles.len();
    let tile_offsets = Entry {
        tag: 324,
        typ: LONG8,
        count: n as u64,
        value: if n == 1 {
            Value::Data(level.tiles[0].offset.to_le_bytes().to_vec())
        } else {
            Value::At(offsets_pos)
        },
    };
    let tile_byte_counts = Entry {
        tag: 325,
        typ: LONG,
        count: n as u64,
        value: if n <= 2 {
            Value::Data(
                level
                    .tiles
                    .iter()
                    .flat_map(|t| t.len.to_le_bytes())
                    .collect(),
            )
        } else {
            Value::At(counts_pos)
        },
    };
    let mut e = Vec::new();
    if !full {
        e.push(long(254, 1)); // NewSubfileType: reduced-resolution image
    }
    e.push(long(256, width)); // ImageWidth
    e.push(long(257, width)); // ImageLength
    e.push(shorts(258, &[16])); // BitsPerSample
    e.push(shorts(259, &[50000])); // Compression: Zstandard
    e.push(shorts(262, &[1])); // PhotometricInterpretation: BlackIsZero
    if full {
        e.push(ascii(270, &meta.description)); // ImageDescription
    }
    e.push(shorts(277, &[1])); // SamplesPerPixel
    e.push(shorts(284, &[1])); // PlanarConfiguration: contiguous
    if full {
        e.push(ascii(305, &meta.software)); // Software
    }
    e.push(shorts(322, &[TILE_SIZE as u16])); // TileWidth
    e.push(shorts(323, &[TILE_SIZE as u16])); // TileLength
    e.push(tile_offsets);
    e.push(tile_byte_counts);
    e.push(shorts(339, &[1])); // SampleFormat: unsigned integer
    if full {
        e.push(shorts(340, &[meta.min_value])); // SMinSampleValue
        e.push(shorts(341, &[meta.max_value])); // SMaxSampleValue
    }
    if full {
        e.push(ascii(33432, &meta.copyright)); // Copyright
        georef(&mut e, grid);
        e.push(ascii(42112, &meta.gdal_metadata)); // GDAL_METADATA
        if let Some(v) = meta.nodata {
            e.push(ascii(42113, &v.to_string())); // GDAL_NODATA
        }
    }
    e
}

/// ModelPixelScale, ModelTiepoint and the GeoKeys, as GDAL writes them.
fn georef(e: &mut Vec<Entry>, grid: &Grid) {
    let pixel_zoom = grid.max_zoom + TILE_BITS;
    let n = f64::from(1u32 << pixel_zoom);
    match grid.crs {
        Crs::WebMercator => {
            let res = 2.0 * HALF_WORLD / n;
            e.push(doubles(33550, &[res, res, 0.0])); // ModelPixelScale
            e.push(doubles(
                33922,
                &[0.0, 0.0, 0.0, -HALF_WORLD, HALF_WORLD, 0.0],
            )); // ModelTiepoint
            e.push(shorts(
                34735,
                &[
                    1, 1, 0, 7, // version 1.1.0, 7 keys
                    1024, 0, 1, 1, // GTModelType: projected
                    1025, 0, 1, 1, // GTRasterType: PixelIsArea
                    1026, 34737, 25, 0, // GTCitation
                    2049, 34737, 7, 25, // GeodeticCitation
                    2054, 0, 1, 9102, // GeogAngularUnits: degree
                    3072, 0, 1, 3857, // ProjectedCRS: Web Mercator
                    3076, 0, 1, 9001, // ProjLinearUnits: meter
                ],
            ));
            e.push(ascii(34737, "WGS 84 / Pseudo-Mercator|WGS 84|")); // GeoAsciiParams
        }
        Crs::Wgs84 => {
            e.push(doubles(33550, &[360.0 / n, 180.0 / n, 0.0])); // ModelPixelScale
            e.push(doubles(33922, &[0.0, 0.0, 0.0, -180.0, 90.0, 0.0])); // ModelTiepoint
            e.push(shorts(
                34735,
                &[
                    1, 1, 0, 5, // version 1.1.0, 5 keys
                    1024, 0, 1, 2, // GTModelType: geographic
                    1025, 0, 1, 1, // GTRasterType: PixelIsArea
                    2048, 0, 1, 4326, // GeodeticCRS: WGS 84
                    2049, 34737, 7, 0, // GeodeticCitation
                    2054, 0, 1, 9102, // GeogAngularUnits: degree
                ],
            ));
            e.push(ascii(34737, "WGS 84|")); // GeoAsciiParams
        }
    }
}

/// Size of a directory plus the out-of-line values stored after it.
fn ifd_len(entries: &[Entry]) -> u64 {
    let mut len = 8 + 20 * entries.len() as u64 + 8;
    for e in entries {
        if let Value::Data(d) = &e.value
            && d.len() > 8
        {
            len += d.len().next_multiple_of(8) as u64;
        }
    }
    len
}

fn write_ifd(out: &mut Vec<u8>, entries: &[Entry], next: u64) {
    let start = out.len() as u64;
    let mut extra_pos = start + 8 + 20 * entries.len() as u64 + 8;
    let mut extra = Vec::new();
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    let mut last_tag = 0;
    for e in entries {
        assert!(e.tag > last_tag, "TIFF tags must be sorted");
        last_tag = e.tag;
        out.extend_from_slice(&e.tag.to_le_bytes());
        out.extend_from_slice(&e.typ.to_le_bytes());
        out.extend_from_slice(&e.count.to_le_bytes());
        match &e.value {
            Value::Data(d) if d.len() <= 8 => {
                let mut v = [0u8; 8];
                v[..d.len()].copy_from_slice(d);
                out.extend_from_slice(&v);
            }
            Value::Data(d) => {
                out.extend_from_slice(&extra_pos.to_le_bytes());
                extra.extend_from_slice(d);
                pad(&mut extra);
                extra_pos = start + 8 + 20 * entries.len() as u64 + 8 + extra.len() as u64;
            }
            Value::At(pos) => out.extend_from_slice(&pos.to_le_bytes()),
        }
    }
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&extra);
}

/// Pads to a multiple of 8 bytes; TIFF wants values at even offsets.
fn pad(buf: &mut Vec<u8>) {
    buf.resize(buf.len().next_multiple_of(8), 0);
}
