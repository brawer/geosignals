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
    e["webp8-lossless"] = ("q8", lambda a: ic.webp_encode(a.astype(np.uint8), lossless=True, method=6, level=100), None)
    for q in (90, 75):
        e[f"webp8-lossy-q{q}"] = ("q8", (lambda q: lambda a: ic.webp_encode(np.repeat(a.astype(np.uint8)[:, :, None], 3, axis=2), level=q, method=6))(q),
                                  lambda b: ic.webp_decode(b)[:, :, 0])
    e["avif8-lossless"] = ("q8", lambda a: ic.avif_encode(a.astype(np.uint8), level=100, speed=6), None)
    e["jxl8-lossless"] = ("q8", lambda a: ic.jpegxl_encode(a.astype(np.uint8), lossless=True, effort=7), None)
    e["png16"] = ("q12", lambda a: ic.png_encode(a.astype(np.uint16), level=9), None)
    for q in (90, 75, 60):
        e[f"avif12-lossy-q{q}"] = ("q12", (lambda q: lambda a: ic.avif_encode(a.astype(np.uint16), level=q, speed=6, bitspersample=12))(q),
                                   ic.avif_decode)
    e["avif12-lossless"] = ("q12", lambda a: ic.avif_encode(a.astype(np.uint16), level=100, speed=6, bitspersample=12), None)
    e["jxl12-lossless"] = ("q12", lambda a: ic.jpegxl_encode(a.astype(np.uint16), lossless=True, effort=7, bitspersample=12), None)
    e["jxl16-lossless"] = ("q16", lambda a: ic.jpegxl_encode(a.astype(np.uint16), lossless=True, effort=7), None)
    return e


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
    for z in sorted(per_zoom):
        n_all = len([f for f in files if f.startswith(f"{z}-")])
        weight = (1 if z <= 5 else args.rate) * n_all / len(per_zoom[z])
        for x, y, f in per_zoom[z]:
            with open(os.path.join(args.sample_dir, f), "rb") as fh:
                counts = np.frombuffer(ic.zstd_decode(fh.read()), dtype="<u4").reshape(256, 256)
            density = counts / row_areas(z, y)[:, None]
            tiles[z] += weight
            quantized = {s: log_quantize(density, MIN_DENSITY, spo, 2**bits - 1)
                         for s, (bits, spo) in SCALES.items()}
            nz = density > 0
            for name, (scale, enc, dec) in encs.items():
                data = enc(quantized[scale])
                sizes[name][z] += weight * len(data)
                if dec is not None:
                    got = dequantize(dec(data), MIN_DENSITY, SCALES[scale][1])
                    ok = nz & (got > 0)
                    errors[name].append(np.abs(np.log2(got[ok] / density[ok])))
                    fl = flips[name]
                    fl[0] += int((nz & (got == 0)).sum())
                    fl[1] += int((~nz & (got > 0)).sum())
                    fl[2] += int(nz.sum())
        print(f"zoom {z}: {len(per_zoom[z])} sampled, ~{tiles[z]:.0f} tiles", file=sys.stderr)

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
