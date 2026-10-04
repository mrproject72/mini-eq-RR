#!/usr/bin/env python3
"""Regenerate the hicolor icon set from the master artwork.

Source of truth: `mini_eq_rr_icon_final.png` (1600x1600 RGBA).

Produces, for each size in SIZES:
  assets/icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr.png
  assets/icons/<N>x<N>/apps/io.github.mrproject72.mini_eq_rr-symbolic.png

Run from the repository root after replacing the master artwork:

    python3 assets/generate-icons.py

Why this exists rather than being done once by hand: the icons are binaries, so
without a generator there is no way to tell they were derived, or to regenerate
them when the artwork changes. Committing the script keeps the set reproducible.

## Downscaling uses premultiplied alpha

Averaging straight RGBA pixels bleeds the RGB of fully transparent pixels into
the edges. Those pixels carry whatever colour the source stored there -- for this
artwork, near-black -- and the result is a dark halo around the badge. Colour is
therefore multiplied by alpha before averaging and divided out again afterwards:

    a   = alpha / 255
    pm  = rgb * a
    out = mean(pm) / mean(a)

Verified: partial-alpha edge pixels stay magenta (rgb(235, 2, 163) at 16px from
the earlier artwork) instead of darkening.

## Symbolic variants are alpha silhouettes

A freedesktop *symbolic* icon is a single-colour shape that the toolkit
recolours to match the active theme, so it must not carry the artwork's own
colours. Keeping only the alpha channel and discarding RGB is the correct
derivation, not a lossy approximation.

## Note on the source alpha

The master artwork has no partial-alpha pixels: its edges are hard, not
antialiased. Area-averaging still produces correct intermediate coverage at small
sizes (the badge spans many source pixels per output pixel), so the small icons
are fine. At 512px the inherited hard edge is visible. Re-exporting the master
with antialiased edges would improve that, if it is ever worth doing.

No third-party dependencies: PNG decode/encode is done with zlib and numpy, so
this runs anywhere the test suite runs.
"""

import os
import struct
import sys
import zlib

try:
    import numpy as np
except ImportError:
    sys.exit("numpy is required: pip install numpy")

HERE = os.path.dirname(os.path.abspath(__file__))
NAME = "io.github.mrproject72.mini_eq_rr"
SIZES = [16, 24, 32, 48, 64, 128, 256, 512]
SOURCE = os.path.join(HERE, "mini_eq_rr_icon_final.png")


def load_png(path):
    """Minimal RGBA8 PNG decoder (no interlacing)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path} is not a PNG")
    pos, idat, ihdr = 8, b"", None
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        chunk = data[pos + 8 : pos + 8 + length]
        if kind == b"IHDR":
            ihdr = struct.unpack(">IIBBBBB", chunk)
        elif kind == b"IDAT":
            idat += chunk
        pos += 12 + length
    width, height, depth, color, _, _, interlace = ihdr
    if depth != 8 or color != 6 or interlace != 0:
        raise ValueError("expected 8-bit non-interlaced RGBA")
    channels, stride = 4, width * 4
    raw = zlib.decompress(idat)
    out = np.zeros((height, stride), dtype=np.uint8)
    prev = np.zeros(stride, dtype=np.uint8)
    p = 0
    for y in range(height):
        filt = raw[p]
        p += 1
        line = np.frombuffer(raw[p : p + stride], dtype=np.uint8).astype(np.int32).copy()
        p += stride
        if filt == 1:
            for i in range(channels, stride):
                line[i] = (line[i] + line[i - channels]) & 0xFF
        elif filt == 2:
            line = (line + prev) & 0xFF
        elif filt == 3:
            for i in range(stride):
                a = line[i - channels] if i >= channels else 0
                line[i] = (line[i] + ((a + int(prev[i])) >> 1)) & 0xFF
        elif filt == 4:
            for i in range(stride):
                a = int(line[i - channels]) if i >= channels else 0
                b = int(prev[i])
                c = int(prev[i - channels]) if i >= channels else 0
                q = a + b - c
                pa, pb, pc = abs(q - a), abs(q - b), abs(q - c)
                pred = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pred) & 0xFF
        prev = line.astype(np.uint8)
        out[y] = prev
    return out.reshape(height, width, 4)


def write_png(path, arr):
    height, width, _ = arr.shape
    raw = b"".join(b"\x00" + arr[y].tobytes() for y in range(height))

    def chunk(kind, payload):
        head = struct.pack(">I", len(payload)) + kind + payload
        return head + struct.pack(">I", zlib.crc32(kind + payload) & 0xFFFFFFFF)

    blob = b"\x89PNG\r\n\x1a\n"
    blob += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    blob += chunk(b"IDAT", zlib.compress(raw, 9))
    blob += chunk(b"IEND", b"")
    open(path, "wb").write(blob)


def resize_premultiplied(src, size):
    """Area-average downscale in premultiplied-alpha space. See module docstring."""
    height, width, _ = src.shape
    alpha = src[..., 3].astype(np.float64) / 255.0
    pm = src[..., :3].astype(np.float64) * alpha[..., None]
    out = np.empty((size, size, 4), dtype=np.uint8)
    ys = np.linspace(0, height, size + 1)
    xs = np.linspace(0, width, size + 1)
    for j in range(size):
        y0, y1 = min(int(ys[j]), height), min(max(int(ys[j]) + 1, int(ys[j + 1])), height)
        for i in range(size):
            x0, x1 = min(int(xs[j]), width), min(max(int(xs[j]) + 1, int(xs[j + 1])), width)
            if y1 <= y0 or x1 <= x0:
                out[j, i] = 0
                continue
            coverage = alpha[y0:y1, x0:x1].mean()
            if coverage <= 1e-9:
                out[j, i] = 0
                continue
            mean_pm = pm[y0:y1, x0:x1].reshape(-1, 3).mean(axis=0)
            out[j, i, :3] = np.clip(mean_pm / coverage + 0.5, 0, 255).astype(np.uint8)
            out[j, i, 3] = int(coverage * 255 + 0.5)
    return out


def silhouette(src):
    """Alpha-only copy: a symbolic icon is recoloured by the toolkit."""
    height, width, _ = src.shape
    out = np.zeros((height, width, 4), dtype=np.uint8)
    out[..., 3] = src[..., 3]
    return out


def main():
    src = load_png(SOURCE)
    for size in SIZES:
        target = os.path.join(HERE, "icons", f"{size}x{size}", "apps")
        os.makedirs(target, exist_ok=True)
        write_png(os.path.join(target, f"{NAME}.png"), resize_premultiplied(src, size))
        write_png(
            os.path.join(target, f"{NAME}-symbolic.png"), silhouette(resize_premultiplied(src, size))
        )
        print(f"wrote {size}x{size}")


if __name__ == "__main__":
    main()