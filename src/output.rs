// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! Collects encoded tiles and writes them as one PMTiles archive.
//!
//! Tiles arrive children-before-parents, so the zoom levels interleave.
//! We spool each zoom level to its own temporary file and concatenate
//! them at the end, zoom 0 first. Within a level, tiles already arrive in
//! Hilbert order, so the archive comes out clustered (sorted by tile ID),
//! which PMTiles readers like best.

use crate::tile::MAX_TILE_ZOOM;
use anyhow::{Context, Result};
use pmtiles::{PmTilesWriter, TileCoord, TileType};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Spool {
    files: Vec<BufWriter<File>>,
    pub tiles_per_zoom: Vec<u64>,
    pub bytes_per_zoom: Vec<u64>,
}

impl Spool {
    pub fn new(tmpdir: &Path) -> Result<Self> {
        let files = (0..=MAX_TILE_ZOOM)
            .map(|_| Ok(BufWriter::new(tempfile::tempfile_in(tmpdir)?)))
            .collect::<Result<_>>()?;
        let n = MAX_TILE_ZOOM as usize + 1;
        Ok(Spool {
            files,
            tiles_per_zoom: vec![0; n],
            bytes_per_zoom: vec![0; n],
        })
    }

    pub fn add(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> Result<()> {
        let f = &mut self.files[z as usize];
        f.write_all(&x.to_le_bytes())?;
        f.write_all(&y.to_le_bytes())?;
        f.write_all(&(data.len() as u32).to_le_bytes())?;
        f.write_all(data)?;
        self.tiles_per_zoom[z as usize] += 1;
        self.bytes_per_zoom[z as usize] += data.len() as u64;
        Ok(())
    }

    /// Writes the archive to `path`.
    pub fn write_pmtiles(self, path: &Path, metadata: &str) -> Result<()> {
        let out = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let mut w = PmTilesWriter::new(TileType::Webp)
            .min_zoom(0)
            .max_zoom(MAX_TILE_ZOOM)
            .bounds(-180.0, -85.051_129, 180.0, 85.051_129)
            .center(0.0, 0.0)
            .center_zoom(2)
            .metadata(metadata)
            .create(out)?;
        for (z, f) in self.files.into_iter().enumerate() {
            let mut f = f.into_inner().map_err(|e| e.into_error())?;
            f.seek(SeekFrom::Start(0))?;
            let mut r = BufReader::with_capacity(1 << 20, f);
            let mut head = [0u8; 12];
            let mut data = Vec::new();
            loop {
                match r.read_exact(&mut head) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e.into()),
                }
                let x = u32::from_le_bytes(head[0..4].try_into().unwrap());
                let y = u32::from_le_bytes(head[4..8].try_into().unwrap());
                let len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
                data.resize(len, 0);
                r.read_exact(&mut data)?;
                w.add_tile(TileCoord::new(z as u8, x, y)?, &data)?;
            }
        }
        w.finalize()?;
        fix_empty_leaf_offset(path)
    }
}

/// When an archive has no leaf directories, the `pmtiles` crate records
/// their offset as 0, which `pmtiles verify` (go-pmtiles) rejects.
/// We point the empty section at the end of the tile data instead.
fn fix_empty_leaf_offset(path: &Path) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut header = [0u8; 127];
    f.read_exact(&mut header)?;
    let field = |i: usize| u64::from_le_bytes(header[i..i + 8].try_into().unwrap());
    let (leaf_offset, leaf_length) = (field(40), field(48));
    if leaf_offset == 0 && leaf_length == 0 {
        let end_of_data = field(56) + field(64);
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&end_of_data.to_le_bytes())?;
    }
    Ok(())
}
