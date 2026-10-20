// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! A tiny BigTIFF reader for tests, written from the TIFF and COG specs
//! so that it checks our output independently of the writer.

use std::collections::BTreeMap;

pub struct Image {
    /// Tag values of each directory, full resolution first.
    pub ifds: Vec<BTreeMap<u16, Field>>,
    /// Decoded pixels of each non-empty tile, by (zoom, x, y).
    pub tiles: BTreeMap<(u8, u32, u32), Vec<u16>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Ascii(String),
    Ints(Vec<u64>),
    Doubles(Vec<f64>),
}

impl Field {
    pub fn int(&self) -> u64 {
        match self {
            Field::Ints(v) if v.len() == 1 => v[0],
            _ => panic!("not a single integer: {self:?}"),
        }
    }
    pub fn ints(&self) -> &[u64] {
        match self {
            Field::Ints(v) => v,
            _ => panic!("not integers: {self:?}"),
        }
    }
    pub fn ascii(&self) -> &str {
        match self {
            Field::Ascii(s) => s,
            _ => panic!("not ASCII: {self:?}"),
        }
    }
}

fn u16_at(b: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes(b[pos..pos + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(b[pos..pos + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(b[pos..pos + 8].try_into().unwrap())
}

fn field(b: &[u8], entry: usize) -> (u16, Field) {
    let tag = u16_at(b, entry);
    let typ = u16_at(b, entry + 2);
    let count = u64_at(b, entry + 4) as usize;
    let size = match typ {
        2 => 1,
        3 => 2,
        4 => 4,
        12 | 16 => 8,
        _ => panic!("tag {tag}: unexpected type {typ}"),
    };
    let pos = if size * count <= 8 {
        entry + 12
    } else {
        let p = u64_at(b, entry + 12) as usize;
        assert_eq!(p % 2, 0, "tag {tag}: odd offset");
        p
    };
    let value = match typ {
        2 => {
            let s = &b[pos..pos + count];
            assert_eq!(s[count - 1], 0, "tag {tag}: missing NUL");
            Field::Ascii(String::from_utf8(s[..count - 1].to_vec()).unwrap())
        }
        3 => Field::Ints(
            (0..count)
                .map(|i| u64::from(u16_at(b, pos + 2 * i)))
                .collect(),
        ),
        4 => Field::Ints(
            (0..count)
                .map(|i| u64::from(u32_at(b, pos + 4 * i)))
                .collect(),
        ),
        16 => Field::Ints((0..count).map(|i| u64_at(b, pos + 8 * i)).collect()),
        12 => Field::Doubles(
            (0..count)
                .map(|i| f64::from_le_bytes(b[pos + 8 * i..pos + 8 * i + 8].try_into().unwrap()))
                .collect(),
        ),
        _ => unreachable!(),
    };
    (tag, value)
}

pub fn read(b: &[u8]) -> Image {
    assert_eq!(&b[0..4], b"II\x2b\x00", "little-endian BigTIFF");
    assert_eq!(u16_at(b, 4), 8);
    let mut ifds = Vec::new();
    let mut pos = u64_at(b, 8) as usize;
    while pos != 0 {
        let n = u64_at(b, pos) as usize;
        let mut ifd = BTreeMap::new();
        let mut last = 0;
        for i in 0..n {
            let (tag, value) = field(b, pos + 8 + 20 * i);
            assert!(tag > last, "tags out of order");
            last = tag;
            ifd.insert(tag, value);
        }
        ifds.push(ifd);
        pos = u64_at(b, pos + 8 + 20 * n) as usize;
    }

    let levels = ifds.len();
    let mut tiles = BTreeMap::new();
    let mut prev_end = 0;
    // COG: smallest overview's data first, full resolution last.
    for (i, ifd) in ifds.iter().enumerate().rev() {
        let zoom = (levels - 1 - i) as u8;
        assert_eq!(ifd[&256].int(), 256 << zoom, "ImageWidth");
        assert_eq!(ifd[&259].int(), 50000, "Zstandard");
        assert_eq!(ifd[&258].int(), 16, "BitsPerSample");
        let offsets = ifd[&324].ints();
        let counts = ifd[&325].ints();
        let n = 1usize << zoom;
        assert_eq!(offsets.len(), n * n);
        assert_eq!(counts.len(), n * n);
        for (t, (&off, &len)) in offsets.iter().zip(counts).enumerate() {
            if len == 0 {
                assert_eq!(off, 0, "empty tile with an offset");
                continue;
            }
            let (off, len) = (off as usize, len as usize);
            assert!(off > prev_end, "tiles out of order");
            prev_end = off;
            // GDAL's tile leader and trailer.
            assert_eq!(u32_at(b, off - 4) as usize, len, "leader");
            assert_eq!(&b[off + len..off + len + 4], &b[off + len - 4..off + len]);
            let raw = zstd::bulk::decompress(&b[off..off + len], 1 << 17).unwrap();
            assert_eq!(raw.len(), 2 * 256 * 256);
            let px = raw
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let (x, y) = ((t % n) as u32, (t / n) as u32);
            tiles.insert((zoom, x, y), px);
        }
    }
    Image { ifds, tiles }
}
