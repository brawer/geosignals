// SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
// SPDX-License-Identifier: MIT

//! A tiny PMTiles v3 reader for tests, written from the spec so that
//! it checks our output independently of the writer library.

use std::collections::BTreeMap;
use std::io::Read;

pub struct Archive {
    pub header: Vec<u8>,
    pub metadata: String,
    /// Tile data by (z, x, y).
    pub tiles: BTreeMap<(u8, u32, u32), Vec<u8>>,
}

fn u64_at(b: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(b[pos..pos + 8].try_into().unwrap())
}

fn varint(b: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[*pos];
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return v;
        }
        shift += 7;
    }
}

fn gunzip(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(b)
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn tile_id_to_zxy(id: u64) -> (u8, u32, u32) {
    let mut base = 0u64;
    for z in 0..32u8 {
        let n = 1u64 << (2 * z);
        if id < base + n {
            let (x, y) = if z == 0 {
                (0, 0)
            } else {
                fast_hilbert::h2xy::<u32>(id - base, z)
            };
            return (z, x, y);
        }
        base += n;
    }
    panic!("tile id {id} too large");
}

/// Entries of a directory: (tile_id, offset, length, run_length).
fn directory(b: &[u8]) -> Vec<(u64, u64, u64, u64)> {
    let b = gunzip(b);
    let mut pos = 0;
    let n = varint(&b, &mut pos) as usize;
    let mut ids = Vec::with_capacity(n);
    let mut last = 0;
    for _ in 0..n {
        last += varint(&b, &mut pos);
        ids.push(last);
    }
    let runs: Vec<u64> = (0..n).map(|_| varint(&b, &mut pos)).collect();
    let lens: Vec<u64> = (0..n).map(|_| varint(&b, &mut pos)).collect();
    let mut offsets: Vec<u64> = Vec::with_capacity(n);
    for i in 0..n {
        let v = varint(&b, &mut pos);
        offsets.push(if v == 0 && i > 0 {
            offsets[i - 1] + lens[i - 1]
        } else {
            v - 1
        });
    }
    assert_eq!(pos, b.len(), "trailing bytes in directory");
    (0..n)
        .map(|i| (ids[i], offsets[i], lens[i], runs[i]))
        .collect()
}

pub fn read(data: &[u8]) -> Archive {
    assert_eq!(&data[0..7], b"PMTiles");
    assert_eq!(data[7], 3);
    assert_eq!(data[97], 2, "internal compression should be gzip");
    let header = data[..127].to_vec();
    let (root_off, root_len) = (u64_at(data, 8) as usize, u64_at(data, 16) as usize);
    let (meta_off, meta_len) = (u64_at(data, 24) as usize, u64_at(data, 32) as usize);
    let leaf_off = u64_at(data, 40) as usize;
    let data_off = u64_at(data, 56) as usize;
    let metadata = String::from_utf8(gunzip(&data[meta_off..meta_off + meta_len])).unwrap();
    let mut tiles = BTreeMap::new();
    let mut stack = vec![directory(&data[root_off..root_off + root_len])];
    while let Some(entries) = stack.pop() {
        for (id, off, len, run) in entries {
            let (off, len) = (off as usize, len as usize);
            if run == 0 {
                stack.push(directory(&data[leaf_off + off..leaf_off + off + len]));
                continue;
            }
            for i in 0..run {
                let body = data[data_off + off..data_off + off + len].to_vec();
                assert!(tiles.insert(tile_id_to_zxy(id + i), body).is_none());
            }
        }
    }
    Archive {
        header,
        metadata,
        tiles,
    }
}

/// Decodes a grayscale WebP tile into 256×256 pixel values.
pub fn decode_webp(data: &[u8]) -> Vec<u8> {
    let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(data)).unwrap();
    let (w, h) = dec.dimensions();
    assert_eq!((w, h), (256, 256));
    let mut buf = vec![0u8; dec.output_buffer_size().unwrap()];
    dec.read_image(&mut buf).unwrap();
    let channels = buf.len() / (256 * 256);
    buf.chunks(channels).map(|c| c[0]).collect()
}
