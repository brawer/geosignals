#!/usr/bin/env python3

# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

"""Compares image formats for node-density tiles.

Reads the raw tile samples written by `osmsignals node-density --sample-dir`
(zstd-compressed little-endian u32 counts, 256×256, one file per tile named
z-x-y.u32.zst), encodes each tile in several ways, and extrapolates the
total archive size. Tiles at zoom <= 5 are all sampled; deeper tiles are
sampled at 1/RATE, so their sizes are multiplied by RATE.

Requires: numpy, imagecodecs.
"""

import argparse
import math
import multiprocessing
import os
import sys
from collections import defaultdict

import imagecodecs as ic
import numpy as np

R = 6371.0072


def row_areas(z, y):
    """Area in km² of each of the 256 pixel rows of tile z/_/y."""
    pz = z + 8
    rows = np.arange(y * 256, y * 256 + 257, dtype=np.float64)
    lat = np.arctan(np.sinh(np.pi * (1 - 2 * rows / 2**pz)))
    return R * R * (2 * np.pi / 2**pz) * (np.sin(lat[:-1]) - np.sin(lat[1:]))


def log_quantize(density, min_density, steps_per_octave, max_value):
    q = np.zeros(density.shape, dtype=np.float64)
    nz = density > 0
    q[nz] = 1 + np.round(steps_per_octave * np.log2(density[nz] / min_density))
    return np.clip(q, 0, max_value) * nz


def dequantize(q, min_density, steps_per_octave):
    q = q.astype(np.float64)
    return np.where(q > 0, min_density * np.exp2((q - 1) / steps_per_octave), 0.0)


# name -> (bits, steps per octave)
SCALES = {
    "q8": (8, 6.0),        # 12% steps, v=1 at 2^-16 /km²
    "q12": (12, 96.0),     # 0.7% steps
    "q16": (16, 1536.0),   # effectively exact
}
MIN_DENSITY = 2.0**-16


def encoders():
    """name -> (scale, encode(arr) -> bytes, decode(bytes) -> arr or None)."""
    e = {}
    e["png8"] = ("q8", lambda a: ic.png_encode(a.astype(np.uint8), level=9), ic.png_decode)
    rgb = lambda a: np.repeat(a.astype(np.uint8)[:, :, None], 3, axis=2)
    e["webp8-lossless"] = ("q8", lambda a: ic.webp_encode(rgb(a), lossless=True, method=6, level=100), None)
    for q in (90, 75):
        e[f"webp8-lossy-q{q}"] = ("q8", (lambda q: lambda a: ic.webp_encode(rgb(a), level=q, lossless=False, method=6))(q),
                                  lambda b: ic.webp_decode(b)[:, :, 0])
    e["avif8-lossless"] = ("q8", lambda a: ic.avif_encode(a.astype(np.uint8), level=100, speed=6), None)
    e["jxl8-lossless"] = ("q8", lambda a: ic.jpegxl_encode(a.astype(np.uint8), lossless=True, effort=7), None)
    # 12 bits spread over two channels, like Mapbox Terrain-RGB.
    rg = lambda a: np.stack([(a.astype(np.uint16) >> 8).astype(np.uint8), (a.astype(np.uint16) & 255).astype(np.uint8), np.zeros(a.shape, np.uint8)], axis=2)
    e["webp12-rg-lossless"] = ("q12", lambda a: ic.webp_encode(rg(a), lossless=True, method=6, level=100), None)
    e["png16"] = ("q12", lambda a: ic.png_encode(a.astype(np.uint16), level=9), None)
    for q in (90, 75, 60):
        e[f"avif12-lossy-q{q}"] = ("q12", (lambda q: lambda a: ic.avif_encode(a.astype(np.uint16), level=q, speed=6, bitspersample=12))(q),
                                   ic.avif_decode)
    e["avif12-lossless"] = ("q12", lambda a: ic.avif_encode(a.astype(np.uint16), level=100, speed=6, bitspersample=12), None)
    e["jxl12-lossless"] = ("q12", lambda a: ic.jpegxl_encode(a.astype(np.uint16), lossless=True, effort=7, bitspersample=12), None)
    e["jxl16-lossless"] = ("q16", lambda a: ic.jpegxl_encode(a.astype(np.uint16), lossless=True, effort=7), None)
    return e


def bench_tile(job):
    path, z, y, weight, names = job
    encs = encoders()
    with open(path, "rb") as fh:
        counts = np.frombuffer(ic.zstd_decode(fh.read()), dtype="<u4").reshape(256, 256)
    density = counts / row_areas(z, y)[:, None]
    quantized = {s: log_quantize(density, MIN_DENSITY, spo, 2**bits - 1)
                 for s, (bits, spo) in SCALES.items()}
    nz = density > 0
    res = {}
    for name in names:
        scale, enc, dec = encs[name]
        data = enc(quantized[scale])
        err, fl = None, None
        if dec is not None:
            got = dequantize(dec(data), MIN_DENSITY, SCALES[scale][1])
            ok = nz & (got > 0)
            err = np.abs(np.log2(got[ok] / density[ok]))
            fl = (int((nz & (got == 0)).sum()), int((~nz & (got > 0)).sum()), int(nz.sum()))
        res[name] = (len(data), err, fl)
    return z, weight, res


def main():
    p = argparse.ArgumentParser()
    p.add_argument("sample_dir")
    p.add_argument("--rate", type=int, default=64)
    p.add_argument("--only", help="comma-separated encoder names")
    p.add_argument("--limit", type=int, default=0, help="max tiles per zoom (for quick runs)")
    args = p.parse_args()

    encs = encoders()
    if args.only:
        encs = {k: v for k, v in encs.items() if k in args.only.split(",")}
    files = sorted(os.listdir(args.sample_dir))
    per_zoom = defaultdict(list)
    for f in files:
        z, x, y = (int(v) for v in f.split(".")[0].split("-"))
        per_zoom[z].append((x, y, f))
    if args.limit:
        rng = np.random.default_rng(1)
        for z in per_zoom:
            if len(per_zoom[z]) > args.limit:
                idx = rng.choice(len(per_zoom[z]), args.limit, replace=False)
                per_zoom[z] = [per_zoom[z][i] for i in sorted(idx)]

    sizes = defaultdict(lambda: defaultdict(float))  # enc -> zoom -> bytes (extrapolated)
    tiles = defaultdict(float)
    errors = defaultdict(list)  # enc -> list of (abs log2 error) over nonzero pixels
    flips = defaultdict(lambda: [0, 0, 0])  # enc -> [nonzero->zero, zero->nonzero, nonzero]
    jobs = []
    for z in sorted(per_zoom):
        n_all = len([f for f in files if f.startswith(f"{z}-")])
        weight = (1 if z <= 5 else args.rate) * n_all / len(per_zoom[z])
        for x, y, f in per_zoom[z]:
            jobs.append((os.path.join(args.sample_dir, f), z, y, weight, list(encs)))
    with multiprocessing.Pool() as pool:
        for z, weight, res in pool.imap_unordered(bench_tile, jobs, chunksize=4):
            tiles[z] += weight
            for name, (size, err, fl) in res.items():
                sizes[name][z] += weight * size
                if err is not None:
                    errors[name].append(err)
                    for i in range(3):
                        flips[name][i] += fl[i]

    print(f"{'encoding':22} {'total GB':>9} {'z10 GB':>8} {'B/tile':>7} {'median err':>10} {'p99 err':>8} {'lost px':>8} {'ghost px':>8}")
    ntiles = sum(tiles.values())
    for name in encs:
        total = sum(sizes[name].values())
        line = f"{name:22} {total/1e9:9.2f} {sizes[name][10]/1e9:8.2f} {total/ntiles:7.0f}"
        if errors[name]:
            e = np.concatenate(errors[name])
            pct = lambda v: 100 * (2**v - 1)
            fl = flips[name]
            line += f" {pct(np.median(e)):9.1f}% {pct(np.quantile(e, 0.99)):7.1f}% {100*fl[0]/fl[2]:7.2f}% {100*fl[1]/fl[2]:7.2f}%"
        print(line)
    print(f"tiles: {ntiles:.0f} ({', '.join(f'z{z}={tiles[z]:.0f}' for z in sorted(tiles))})")


if __name__ == "__main__":
    main()
